# 09 — `FlashGuard`: RAII Lifecycle and Drop Semantics

**Source:** `src/guard.rs`, `src/main.rs::run` (arm/set_phase/disarm calls).

**Purpose:** Guarantee that any unwind path through the destructive
section of the pipeline emits a loud, _phase-honest_ warning to the
operator that the device is partially flashed. Hold the `O_EXCL` claim
for the entire flash + verify window.

## The three responsibilities

`FlashGuard` does three distinct things:

1. **Lock holder.** Wraps the `File` returned by `O_EXCL` open. Drop
   closes the FD, releasing the kernel's exclusive claim.
2. **Inconsistency annunciator.** When _armed_ and dropped, prints a
   warning explaining the device is mid-flash and must not be removed.
3. **Phase tracker.** Stores which destructive phase is currently in
   flight, so the warning describes the operation honestly. A fault
   during verification reads gets a verb like "being read back", not
   "being written" — the device is still inconsistent (Phase 5b runs
   under the O_EXCL claim, before the kernel partition-table sync of
   Phase 6), but operator trust depends on the message matching reality.

Combining all three is deliberate. The `O_EXCL` claim, the destructive
window, and the phase-tracking are all coterminous; splitting them
into separate types would just create more invariants for callers to
maintain in lockstep.

## Phase enumeration

```rust
pub enum GuardPhase {
    Disarmed         = 0,   // Drop is silent
    WipingSignatures = 1,   // Phase 3 active
    Writing          = 2,   // Phase 4 active
    Cooldown         = 3,   // Phase 5a active
    Verifying        = 4,   // Phase 5b active
}
```

Encoded as a `u8` so the entire guard state lives behind a single
`AtomicU8` — no `Mutex`, no allocation, no signal-handler-vs-mainline
contention. The signal handler (`ctrlc::set_handler`) only flips an
unrelated `AtomicBool`; the guard's phase is updated from the main
thread only and is read from the `Drop` impl (which also runs on the
main thread during unwind).

Each phase maps to a verb via `GuardPhase::interrupted_verb()`:

| Phase            | Verb in the FATAL message                     |
| ---------------- | --------------------------------------------- |
| WipingSignatures | "having its partition signatures wiped"       |
| Writing          | "being written"                               |
| Cooldown         | "settling its NAND/FTL state after the write" |
| Verifying        | "being read back for verification"            |

A unit test in `guard.rs` (`each_phase_has_a_distinct_verb` and
`verifying_phase_does_not_say_written`) protects against a regression
where all phases collapse to a single verb.

## State machine

```
   new(file, dev_path)
            │
            ▼
       ┌──────────┐
       │ Disarmed │ ────────────────────────────┐
       └────┬─────┘                             │
arm(WipingSignatures)                           │
            │                                   │
            ▼                                   │
┌─────────────────────┐                         │
│  WipingSignatures   │                         │
└──────────┬──────────┘                         │
set_phase(Writing)                              │
            │                                   │
            ▼                                   │
     ┌──────────┐                               │
     │ Writing  │                               │
     └────┬─────┘                               │
set_phase(Cooldown)                             │
            │                                   │
            ▼                                   │
     ┌──────────┐                               │
     │ Cooldown │                               │
     └────┬─────┘                          disarm()
set_phase(Verifying)                            │
            │                                   │
            ▼                                   │
     ┌───────────┐                              │
     │ Verifying │ ─────────────────────────────┤
     └───────────┘                              │
                                                ▼
                                     ┌──────────────────┐
                                     │ Disarmed (final) │
                                     │  → into_file()   │
                                     └──────────────────┘
```

Drop fires the FATAL warning iff the phase at drop time is anything
other than `Disarmed`. The arrows from intermediate states to "Disarmed
(final)" are the success path: each is taken when verification
completes (or when the operator passed `--skip-verification` and only the
cooldown ran).

## Where each transition fires (in `main.rs`)

| Transition              | Source location                                                                                                                              |
| ----------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| `new(file, dev_path)`   | end of Phase 2, just acquired `O_EXCL`                                                                                                       |
| `arm(WipingSignatures)` | start of Phase 3                                                                                                                             |
| `set_phase(Writing)`    | start of Phase 4                                                                                                                             |
| `set_phase(Cooldown)`   | start of Phase 5a                                                                                                                            |
| `set_phase(Verifying)`  | start of Phase 5b                                                                                                                            |
| `disarm()`              | end of Phase 5b (verify pass), **or** end of Phase 5a with `--skip-verification`, **or** directly after Phase 4 when both skip flags are set |
| `into_file()`           | end of Phase 6                                                                                                                               |

`set_phase` requires the guard to already be armed (debug-asserted).
That makes "I forgot to call `arm()` first and the warning never fires"
a panic in debug builds, not a silent safety hole.

## The Drop implementation

```rust
impl Drop for FlashGuard {
    fn drop(&mut self) {
        let phase = GuardPhase::from_u8(self.phase.load(Ordering::SeqCst));
        if !matches!(phase, GuardPhase::Disarmed) {
            eprintln!(
                "\n⚠  FATAL: flash interrupted while {} was {}. \
                 The device is in an inconsistent state. DO NOT REMOVE IT. \
                 Re-run imi to recover.",
                self.dev_path.display(),
                phase.interrupted_verb()
            );
        }
    }
}
```

Notes:

- `from_u8` maps unknown values to `Writing`, not `Disarmed`. Every
  store goes through `phase as u8` of a real variant, so the wildcard
  arm is unreachable in practice — but if the state were ever
  unrepresentable, a spurious FATAL warning (fail-loud, with the
  still-accurate "being written" verb) is the acceptable failure mode
  for this tool; a silently suppressed warning (fail-open) is not.
  The `from_u8_round_trips_known_phases` test pins both the round-trip
  and the fail-loud fallback.
- `eprintln!` may allocate. Acceptable on the unwind path because
  Rust's panic machinery keeps stderr functional even under memory
  pressure; worst case we get a truncated line.
- The atomic load uses `SeqCst`. `Acquire` would suffice (we're
  synchronising with the `Release` store in `set_phase`), but the
  cost difference is invisible against an `eprintln!` and `SeqCst`
  is easier to reason about for future contributors.

## Why an `Option<File>`, not just `File`

```rust
pub struct FlashGuard {
    file: Option<File>,
    dev_path: PathBuf,
    phase: AtomicU8,
}
```

`into_file()` needs to extract the `File` for explicit dropping in
Phase 6 _before_ Phase 7 runs (Phase 7 needs the lock released). With
plain `File`, there's no way to extract it without either fighting
with `ManuallyDrop` or running `FlashGuard::drop` and `File::drop`
together at end of function — too late, Phase 7 would already be
running with the lock still held.

After `into_file`, the guard's `Drop` still runs but `self.file` is
`None` (no double-close) and the phase has been disarmed (no spurious
warning).

## Interaction with the signal handler

Rust does **not** run `Drop` on signals like SIGINT by default. A
signal interrupts the current syscall, the libc default handler runs,
and the process exits — `Drop` impls are skipped.

To make `FlashGuard::drop` actually fire on Ctrl+C, we install a
`ctrlc` handler that flips an `Arc<AtomicBool>`. The destructive loops
(Phase 4 flash, Phase 5b verify, Phase 5a cooldown countdown, Phase 7
defense passes) check the flag at iteration boundaries and return
`Err`:

```rust
if cancel.load(Ordering::SeqCst) {
    bail!("cancelled by user");
}
```

The `bail!` produces an `anyhow::Error` that propagates via `?`,
unwinding the stack normally — and _that_ runs `FlashGuard::drop`
with whatever phase was active at the moment of cancel, triggering
the appropriate warning.

**The signal handler must never call `std::process::exit`.** Doing so
bypasses Drop, and the operator gets no warning that their device is
half-flashed. This is the single most important contract in the
codebase.

The same contract shapes how allocation failure is handled inside the
armed window. `handle_alloc_error` and `vec![]`-style infallible
allocation **abort** on OOM — an abort skips Drop exactly like
`exit()` would. Every multi-megabyte allocation between `arm()` and
`disarm()` therefore fails as `Err` instead: `AlignedBuf::new` returns
`Result` (null from `alloc_zeroed` is surfaced, never routed to
`handle_alloc_error`), and the wipe/verify buffers use
`try_reserve_exact` + `resize`. An OOM at any of those sites unwinds
normally and the FATAL warning fires. Sub-kilobyte allocations
(format strings, progress-bar state) still abort on OOM — accepted:
if those fail, the 4 MiB buffers failed first.

## Why arm comes before the wipe call, not after

```rust
guard.arm(GuardPhase::WipingSignatures);    // ← here, before
println!("Wiping partition signatures...");
gpt::wipe_ends(&guard, dev_size)?;
```

If the wipe itself fails (`EIO` mid-write), the device is _already_
inconsistent — the head may have been wiped before the tail write
erred. We want the warning. Arming-after-wipe would miss exactly this
case.

The cost of arming earlier than necessary is zero: there's no
realistic path where the arming is observable but no destructive
side effect has occurred.

## Why disarm only at the end of Phase 5b (or earlier only via the skip flags)

```rust
verify::verify(...)?;            // Phase 5b
guard.disarm();                  // ← only after verify passes
```

The device is "consistent" only once we've read it back and confirmed
the bytes match the source. A successful Phase 4 + cooldown is _not_
enough: a counterfeit USB stick would happily ACK every write and then
return zeros on read, and a flaky stick would have intermittent bad
blocks. The verify pass is the consistency proof.

When the operator passes `--skip-verification`, the disarm happens after the
cooldown (no verify ran). This is a deliberate trade-off: the
operator has explicitly accepted the risk that a defective device
wrote bad data we never checked. The guard cannot warn about
something we chose not to check.

## Real example: the verify-phase IO error that motivated this design

A USB stick reported `EIO (Input/output error)` mid-verify. The
warning printed:

```
Cooldown and FTL sync... done
Verifying data integrity...
⚠  FATAL: flash interrupted while /dev/sdc was being written.
error: Phase 5b: verification: reading 4194304 bytes from device at offset 3166699520: Input/output error
```

The error context (`Phase 5b: verification: reading ...`) was correct.
The FATAL warning verb (`being written`) was a lie — the fault was a
read fault during verify. With the phase-tracking design, the same
scenario now prints:

```
⚠  FATAL: flash interrupted while /dev/sdc was being read back for verification.
```

Honest, and points the operator at the right diagnosis (likely a bad
sector or a failing controller) instead of suggesting a write-side
problem.

## Manual test

Triggering the unwind path on real hardware is the gold-standard test.
Cheap reproductions that don't require a doomed flash drive:

```sh
# Force a write fault with an oversize *compressed* image (a raw image
# of known size is refused by Phase 0's capacity pre-check and never
# reaches Phase 4):
dd if=/dev/zero bs=1M count=200 | gzip > big.gz
sudo ./target/release/imi -i big.gz -d /dev/loop0 -y
# Where /dev/loop0 is a 100 MB loop device — Phase 4's per-chunk
# capacity pre-check aborts mid-write. Expected verb: "being written".

# Force a verify fault by manually corrupting the device after Phase 4:
# (requires a debugger or a deliberately-flaky sparse loop image —
# easier to verify the test suite via `cargo test guard::tests`.)
```

The unit tests `each_phase_has_a_distinct_verb` and
`verifying_phase_does_not_say_written` guard against the specific
regression that motivated this design.
