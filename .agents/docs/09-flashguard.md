# 09 — `FlashGuard`: RAII Lifecycle and Drop Semantics

**Source:** `crates/imi-core/src/common/guard.rs`, `crates/imi-core/src/phases/phase_3.rs`, `crates/imi-core/src/phases/phase_4.rs`, `crates/imi-core/src/phases/phase_5.rs`, `crates/imi-core/src/phases/phase_6.rs` (arm/set_phase/disarm calls).

**Purpose:** Guarantee that any unwind path through the destructive
section of the pipeline emits a loud, _phase-honest_ warning to the
operator that the device is partially flashed. Hold the `O_EXCL` claim
for the entire flash + verify window.

## Two types, not one

Since the typestate split there are two: `FlashGuard`, which holds the
`O_EXCL` claim and is not armed, and `ArmedGuard`, which is what Phase 3
returns and what Phases 4, 5 and 6 accept. Everything below about
arming, phases and the FATAL notice describes an `ArmedGuard`; a bare
`FlashGuard` has none of it and drops silently.

That is the whole enforcement. Phases 4 and 5 take `&mut ArmedGuard`,
which only `FlashGuard::arm` produces, so reaching them without Phase 3
does not compile — verified against an external consumer crate, where
skipping Phase 3 gives `E0308: expected &mut ArmedGuard, found &mut
FlashGuard`. Two tests used to assert that refusal at runtime; neither
can be written now.

`ArmedGuard` wraps `Option<FlashGuard>` rather than owning the fields.
That keeps `FlashGuard`'s `Drop` as the single definition of the notice,
lets `ArmedGuard` have no `Drop` of its own, and lets `disarm` take the
inner value out without `unsafe` — `ManuallyDrop::take` being the
alternative, and an unsafe block a poor price for deleting a
`debug_assert`.

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

## The helpers that exist for testability

The FATAL notice is written to stderr from `Drop`, which no in-process
test can observe. Three helpers make the decision behind it assertable.

All three are crate-internal. `FlashGuard::current_phase` and
`would_warn_on_drop` were public until the typestate split made them
constants: `arm` consumes the guard and `disarm` clears the phase, so
every `FlashGuard` a consumer can hold reports `Disarmed` and would not
warn. What a caller actually wants to know — is this run inside the
destructive window — is now answered by which type they are holding, and
`ArmedGuard::current_phase` is the public accessor for _which_ armed
phase.

- `current_phase()` — the phase the guard would report if dropped now,
  so the arm/set_phase/disarm transitions can be checked directly.
- `would_warn_on_drop()` — what `fatal_notice` consults, and through it
  what `Drop` does. Inverting it would mean staying silent on a
  half-written device and warning on a clean one; a test pins the
  polarity.
- `fatal_notice()` — the notice `Drop` would print, or `None`. Split out
  so the decision and the wording are testable without a process that
  can observe stderr: before the split, `replace drop with ()` survived
  mutation testing, because the only test that watched the notice is
  root-gated and `cargo mutants` does not run `--ignored`.

`ensure_device_is(expected)` used to be a third, and is gone. Phases 3,
4 and 5 each received a guard and a `Target` separately, nothing in the
types tied them together, and a caller of the public per-phase API
driving two devices could cross them and write one image onto the other.
Those phases called it first and refused.

The pairing is now structural: Phase 2 returns a `Session`, which holds
the guard and the target it claimed, and Phases 3 to 6 take that. There
is no second `Target` parameter for a mismatched one to occupy, so the
check had nothing left to check — verified against an external consumer
crate, where the crossed call fails with `E0061`, wrong number of
arguments, rather than compiling and being refused at runtime.

Both definitions were deleted rather than kept as a backstop.
`FlashGuard::new` is `pub(crate)`, so a consumer's only guard comes from
Phase 2, which now returns it already paired: the method was unreachable
rather than merely unused.

## Where each transition fires (in the phase pipelines)

| Transition              | Source location                                                                                                                                                                                                                                                                                                                               |
| ----------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `new(file, dev_path)`   | end of Phase 2, just acquired `O_EXCL`                                                                                                                                                                                                                                                                                                        |
| `arm(WipingSignatures)` | start of Phase 3                                                                                                                                                                                                                                                                                                                              |
| `set_phase(Writing)`    | start of Phase 4                                                                                                                                                                                                                                                                                                                              |
| `set_phase(Cooldown)`   | start of Phase 5a                                                                                                                                                                                                                                                                                                                             |
| `set_phase(Verifying)`  | start of Phase 5b                                                                                                                                                                                                                                                                                                                             |
| `disarm()`              | **start of Phase 6** (`phase_6.rs`), which consumes the `ArmedGuard` and yields a `FlashGuard`. It is the only caller, and there is no second one to drift from: the method exists on `ArmedGuard` alone. Which phase last ran before it depends on the skip flags: Phase 5b normally, Phase 5a with `--skip-verification`, Phase 4 with both |
| `into_file()`           | end of Phase 6                                                                                                                                                                                                                                                                                                                                |

`set_phase` requires the guard to already be armed (debug-asserted).
That makes "I forgot to call `arm()` first and the warning never fires"
a panic in debug builds, not a silent safety hole.

## The Drop implementation

```rust
fn fatal_notice(&self) -> Option<String> {
    if !self.would_warn_on_drop() {
        return None;
    }
    Some(format!(
        "\n⚠  FATAL: flash interrupted while {} was {}. \
         The device is in an inconsistent state. DO NOT REMOVE IT. \
         Re-run imi to recover.",
        self.dev_path.display(),
        self.current_phase().interrupted_verb()
    ))
}

impl Drop for FlashGuard {
    fn drop(&mut self) {
        if let Some(notice) = self.fatal_notice() {
            let mut err = io::stderr().lock();
            let _notice = writeln!(err, "{notice}");
            let _flushed = err.flush();
        }
        // `self.file` (if still `Some`) drops here, releasing the claim.
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
- **`writeln!` with the result discarded, never `eprintln!`.** This is
  not a style preference. `eprintln!` _panics_ if the write fails — a
  closed or full stderr, an `EPIPE` from a dead pager. This code runs
  from `Drop`, and the case it exists for is unwinding, where a second
  panic aborts the process immediately. That abort would skip this very
  notice and every remaining destructor, turning "your device is
  half-written" into a bare `SIGABRT`.

  Reproduced during the code review at exit 134, by piping stderr to a
  reader that exits: `imi ... 2>&1 | head`. If stderr is gone the
  operator cannot be told regardless; what matters is that the attempt
  cannot make things worse.
- The atomic load uses `SeqCst`. `Acquire` would suffice (we're
  synchronising with the `Release` store in `set_phase`), but the
  cost difference is invisible against a `writeln!` to a locked stderr,
  and `SeqCst` is easier to reason about for future contributors.

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
    bail!(Cancelled { during: None });
}
```

`Cancelled` is a type, not a message. A string `bail!` would classify as
`ErrorKind::Failed`, and a consumer branching on `kind()` would report a
Ctrl+C as a failure — a downgrade this project has made once already.

`during` labels the wait when naming it helps. The flash and verify
loops pass `None`, because "cancelled by user" is already unambiguous
there; Phase 5a passes `Some("cooldown")` and Phase 7 passes
`Some("automount defense settle")` or `Some("automount defense")`, where
an operator would otherwise wonder which of several sleeps they
interrupted.

Inside the pipelined arms the same value is built with `err!` and
assigned rather than `bail!`ed, because those loops must break to their
single cleanup block instead of returning — see `05-phase-4-flash.md`.

The `bail!` produces an `imi_core::Error` that propagates via `?`,
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
let armed = guard.arm(ArmedPhase::WipingSignatures);    // ← here, before
println!("Wiping partition signatures...");
wipe_ends(guard, target.dev_size)?;   // phase_3
```

If the wipe itself fails (`EIO` mid-write), the device is _already_
inconsistent — the head may have been wiped before the tail write
erred. We want the warning. Arming-after-wipe would miss exactly this
case.

The cost of arming earlier than necessary is zero: there's no
realistic path where the arming is observable but no destructive
side effect has occurred.

## Why disarm only once the last destructive phase has completed

```rust
verify(...)?;                    // Phase 5b (phase_5)
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
Cooldown and FTL sync...
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
