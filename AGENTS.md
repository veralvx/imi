# Agent Directives — `imi`

> **Read this file completely before making any change to this repository.**
> If you (a future LLM) are contributing to `imi` (formerly `flashrs`), the rules below are
> not stylistic preferences. They are load-bearing safety invariants. The
> Bash predecessor of this tool destroyed real users' boot drives several
> times before reaching its current shape; the Rust port preserves every
> defence the bash version learned the hard way.

## What this project is

`imi` is a defensive ISO/IMG flashing utility for Linux. It writes
operating-system images to USB block devices with extensive checks against
flashing the wrong device, racing with `udisks2`, leaving the device in an
inconsistent state on Ctrl+C, or browning out cheap USB-NAND bridge
controllers during read-back verification. It is intentionally pure Rust
with no shell-outs.

## Hard rules — non-negotiable

1. **Zero external binaries.** No `std::process::Command`, no `dd`, no
   `umount`, no `partprobe`, no `wipefs`, no `lsblk`, no `udevadm`, no
   `file`. Every operation goes through Rust syscalls (`nix`, `libc`) and
   sysfs/procfs parsing. Spawning a binary at any point in the destructive
   pipeline is grounds for immediate review rejection.

   This binds the shipped crates (`crates/*/src/`), which contain zero
   uses. It does **not** bind test scaffolding: the gated loop-device
   suites shell out to `losetup` to build their fixtures, which is
   deliberate — the fixture must be created by something other than the
   code under test, so the verification is independent evidence.

2. **The phase pipeline is canonical.** Phases run in order
   0 → 1 → 2 → 3 → 4 → 5a → 5b → 6 → 7. Reordering them is almost certainly
   wrong. In particular:
   - Phase 1 unmounting **must** precede the Phase 2 `O_EXCL` open. Linux
     `O_EXCL` on a block device is kernel-enforced and is rejected with
     `EBUSY` while any partition is mounted.
   - Phase 5a (cooldown) **must** run unless the operator explicitly
     passed `--skip-cooldown`; `--skip-verification` must never skip it. Pulling
     power before USB-NAND controllers finish FTL drain corrupts the flash
     even on a successful write.
   - Phase 6 (`BLKRRPART`) **must** happen before the FD is dropped. Phase
     7 (automount defense) **must** happen after.

   Each phase is a **public** entry point of `imi-core`
   (`phases::phase_N::run`), so this ordering is a contract with callers
   outside this repository, not merely an internal convention. Two
   defenses keep a misordering from becoming a wrong-device write rather
   than a clean error: `Target` and `FlashOutcome` are `#[non_exhaustive]`
   (a caller cannot forge the values a later phase trusts), and phases 3,
   4 and 5 call `FlashGuard::ensure_device_is` to refuse a guard that is
   not holding the device the `Target` describes. Anything that weakens
   either defense needs the same scrutiny as a change to the phase order
   itself.

3. **`O_DIRECT` invariants.** The aligned write path requires:
   - 4 KiB-aligned buffer (currently 4 MiB at 4 KiB alignment via
     `AlignedBuf`).
   - Sector-aligned offset (we always write at multiples of 4 MiB).
   - Sector-aligned length (the tail chunk is the only sub-sector write,
     and we explicitly disable `O_DIRECT` before issuing it).
     If you change the chunk size, alignment, or buffer type, re-prove all
     three invariants.

4. **Never set `O_SYNC` via `fcntl(F_SETFL)`.** It is not in Linux's mutable
   flags set; the call silently no-ops on some kernels and `EINVAL`s on
   others. Durability comes from `fdatasync()` after the write loop and
   `BLKFLSBUF` before verify. **Do** use `F_SETFL` to toggle `O_DIRECT`,
   which **is** in the mutable set.

5. **`FlashGuard` lifecycle is contract.** Construct disarmed →
   `arm(ArmedPhase::WipingSignatures)` before the first destructive
   write (Phase 3) → `set_phase(...)` at the start of every subsequent
   destructive phase (4 / 5a / 5b) → `disarm()` at the start of Phase 6,
   never before the last destructive phase has completed
   passes (or after Phase 5a with `--skip-verification`, or directly
   after Phase 4 when both skip flags are set). The guard's `Drop`
   is what tells the operator the device is in an inconsistent state
   if anything unwinds while armed, and the active phase is what
   makes the warning's verb honest. Do not bypass it,
   do not turn it into `Box<dyn Drop>`, do not make it `Send + Sync` for
   "convenience."

6. **The `ctrlc` handler must never `exit()`.** It only flips an
   `AtomicBool`. Every long-running loop (flash, verify, cooldown,
   automount sweep) checks the flag at iteration boundaries and returns
   `Err`. This drives normal unwind, which runs `FlashGuard::drop`.
   Calling `std::process::exit` from the signal handler skips all
   destructors and undoes the entire safety story.

7. **Verification runs while `O_EXCL` is still held.** This prevents
   `udisks2` / GNOME / KDE from auto-mounting the new filesystem and
   mutating on-disk bytes (mount-time superblock fields, journal replay,
   `.Trash-NNN`) between write and read-back. Releasing the lock before
   verify defeats the entire phase.

8. **Whitelist for auto-unmount.** Only `/media`, `/run/media`, and
   `/var/run/media` (plus strict descendants, with trailing-slash sentinel
   so `/media-user/...` does not match `/media`). Any other mountpoint
   gets a refusal, not an automatic teardown. If the user has the device
   mounted on `/mnt/work`, they may be editing it; we don't tear that down
   for them.

9. **Unmounting is plain `umount2(target, 0)` — never `MNT_DETACH`, never
   `MNT_FORCE`.** A lazy detach removes the mount from `/proc/self/mountinfo`
   _immediately_ while the filesystem stays alive (and writable) through any
   open fds and the kernel claim persists until the last fd closes — turning
   every later mountinfo scan into a false oracle (Phase 1's residual re-scan
   passes vacuously; Phase 7 can print SUCCESS over a detached-but-active
   mount). A plain umount's `EBUSY` is the honest, actionable signal.

10. **All device correlation is by `(major, minor)`, not by string —
    with a source-stat fallback.**
    `/proc/self/mountinfo` line 10, `/proc/swaps` paths, and user-supplied
    device arguments may all be symlinks (`/dev/disk/by-uuid/...`,
    `/dev/mapper/...`). The kernel writes the _original_ path the user gave
    at mount/swapon time, so string matching on `/dev/<kname>` is fragile.
    Stat the path; compare `st_rdev` against the target devt set.
    Additionally: mountinfo field 3 alone is NOT sufficient — btrfs (and any
    anonymous-devt filesystem) reports a synthetic `0:NN` there that never
    appears in a sysfs-built devt set. The mountinfo filter therefore also
    decodes field 10 (source), stats it, and matches by `st_rdev`.

11. **Every `unsafe` block carries a `// SAFETY:` comment** stating the
    precise invariants (FD validity, pointer provenance, alignment, ioctl
    argument shape). If you add an `unsafe` block without that comment,
    the review will block.

## Threading — Phase 4 and Phase 5b pipelined arms only

Threading is permitted in Phase 4's pipelined arm only. Raw images
route through `flash_serial`, which is single-threaded; compressed
images route through `flash_pipelined`, which spawns one worker thread
that owns the `ImageReader` (decompression) by-move. The device FD is
held under `O_EXCL`; **only the main (writer) thread may ever call
`write_all_at` on it**, and the pipelined arm enforces this
structurally: the worker fills `AlignedBuf`s and hands them over
`mpsc` channels; it never receives `&FlashGuard` or any handle to the
FD. The shared helper `process_chunk` is called from the main thread
in both arms; any change to flash-loop semantics (the capacity
pre-check, O_DIRECT toggling for the tail, bytes accounting) belongs
there, fixed once for both arms. ENOSPC mapping continues to live in
`write_direct`/`write_tail`. Loop discipline inside `flash_pipelined`
(reviewers must enforce): no `return` inside `'write_loop` — every
exit breaks to the single cleanup block, which drops BOTH main-side
channel halves, joins the worker, and only then `resume_unwind`s a
captured worker panic so `FlashGuard::drop`'s FATAL fires. Phase 5b's verify mirrors this shape: `verify_serial` for raw,
`verify_pipelined` for compressed, with the worker owning the reopened
`ImageReader` and pacing itself by `bytes_written`; **only the main
thread may ever call `read_exact_at` on the FD**, the shared
`compare_chunk` holds the comparison/diagnostic invariant, and both
arms' loop discipline (break-only, two-drop cleanup, join, then
`resume_unwind`) is identical to Phase 4's. No other phase may spawn
threads without a design document equivalent to `.agents/docs/11-threading.md`. (Thread census during a compressed Phase 4, for
anyone counting `clone`s under strace: main + the `ctrlc` handler
thread + indicatif's steady-tick spinner thread + the one worker —
the ticker predates the pipeline and belongs to the progress UI, not
to this directive.)

## Repository layout

A two-crate Cargo workspace. `imi-core` is the library that holds the
whole pipeline; `imi` is the command-line binary. The split exists so
the pipeline can be driven by something other than this CLI (a GUI, a
test harness, another tool) without dragging `clap` and the terminal
UX along with it.

```
Cargo.toml              workspace: shared metadata, deps, lints, profiles
crates/imi-core/        the library
  src/lib.rs            module tree, `Config`, `run`, public re-exports
  src/config.rs         `Config` — the core's own input type
  src/common.rs         declares src/common/
  src/common/           infrastructure used by two or more dependents:
                        aligned, cancel, context, direct_io, geometry,
                        guard, identity, image, ioctl, mount, sysfs,
                        testing (cfg(test)), throttle
  src/phases.rs         declares src/phases/
  src/phases/phase_N.rs one file per phase, each exposing `pub fn run`
crates/imi/             the binary
  src/main.rs           parse args, install signal handler, exit code
  src/cli.rs            `clap` surface + `Cli::to_config`
  src/signals.rs        SIGINT/SIGTERM -> the shared cancel flag
  tests/                root-gated loop-device integration tests
```

`imi-core` exposes two entry points. `run` executes the whole pipeline;
the eight `phases::phase_N::run` functions are public so a caller can
drive the sequence itself, threading each phase's return value into the
next. The phase order is load-bearing — phase _N_ assumes phase _N-1_
succeeded — and every phase's doc comment says so.

Every module under `common/` is `pub(crate)`; none is `pub`. The types
that appear in a phase's public signature reach a caller by re-export
from the crate root instead — `Target`, `FlashOutcome`, `FlashGuard`,
`GuardPhase`, `DeviceIdentity`, `Compression`, `TargetDevts` — so the
module tree is free to move without breaking a consumer. A module belongs in `common/` when more than one thing depends on
it — a phase or another `common/` module — and in a phase file
otherwise. Applying that rule mechanically is what stops phases from
reaching into each other for helpers, and **no phase imports another**:
the last such reference was `phase_0` reading `phase_3::WIPE_REGION` for
the minimum-size floor, which now lives in `common/geometry.rs`.

The second clause is not pedantry. `sysfs` is named directly by only one
phase but also by `mount` and `identity`; a phases-only reading of the
rule would move it into that phase and force two `common/` modules to
import from a phase, which is a worse version of the coupling the rule
exists to prevent.

### Publishing order

The split has one operational consequence worth writing down, because it
looks like a broken build if you meet it without warning.

`crates/imi/Cargo.toml` depends on `imi-core = { version = "0.2.0",
path = "crates/imi-core" }`. When packaging, cargo drops the path and
requires that exact version from the registry. So:

```sh
cargo publish -p imi-core     # must go first
cargo publish -p imi          # only resolves once the above is on the index
```

Until `imi-core` is published, `cargo package -p imi` fails with
"no matching package named `imi-core` found" — which is correct
behaviour, not a defect in the manifest.

This matters right now: `imi` is on crates.io at 0.1.7, the
pre-workspace monolith, and `imi-core` has never been published at all.
Both README instructions that name it — `cargo add imi-core` and the
link to `crates.io/crates/imi-core` — become true only after the first
command above. The release workflow does not do this; `cargo-dist`
builds binaries for GitHub Releases and never touches the registry.

Signal handling lives in the binary, not the library: taking over
process-wide signal disposition is the application's business, so
`imi_core::run_with_cancel` takes a `&AtomicBool` the caller owns
(`imi_core::run` is the same pipeline with nothing to cancel from).

## How to navigate this codebase

Per-phase explanatory documentation lives under `.agents/docs/`. **Read
the relevant phase doc before modifying that phase's source file.** The
docs explain not just what the code does but why the design choices are
made — including failure modes that motivated each defence.

| You want to change…             | Read first                               |
| ------------------------------- | ---------------------------------------- |
| Argument parsing, throttle, UX  | `.agents/docs/00-cli-and-ux.md`          |
| Pre-flight validation           | `.agents/docs/01-phase-0-preflight.md`   |
| Mount/swap/topology checks      | `.agents/docs/02-phase-1-topology.md`    |
| `O_EXCL` claim, TOCTOU re-check | `.agents/docs/03-phase-2-exclusive.md`   |
| Signature wipe (head/tail)      | `.agents/docs/04-phase-3-wipe.md`        |
| Flash write loop, `O_DIRECT`    | `.agents/docs/05-phase-4-flash.md`       |
| Cooldown + verify               | `.agents/docs/06-phase-5-verify.md`      |
| `BLKRRPART`, lock release       | `.agents/docs/07-phase-6-kernel-sync.md` |
| Automount defense               | `.agents/docs/08-phase-7-automount.md`   |
| `FlashGuard` lifecycle          | `.agents/docs/09-flashguard.md`          |
| Aligned buffer, syscall layer   | `.agents/docs/10-aligned-and-ioctls.md`  |

## Style and idioms

- Edition 2024; toolchain pinned to 1.97 in `rust-toolchain.toml`, matching
  `rust-version` in `Cargo.toml`. If you find yourself
  needing a newer feature, raise the MSRV in a separate commit with
  justification — don't sneak it in.
- Lint policy: clippy's `cargo`, `pedantic`, and `restriction` groups are
  enabled wholesale in `Cargo.toml`, with a documented allow-list carving
  out style-pair lints, no_std-portability lints, and churn-only lints.
  Correctness-signal lints (`arithmetic_side_effects`, `indexing_slicing`,
  `unwrap_used`/`expect_used`, `cast_*`, …) stay enabled and are satisfied
  per-site — by a real fix where possible, or by `#[expect(..., reason)]`
  stating the invariant that makes the operation infallible. `just clippy`
  is expected to pass clean on the pinned stable toolchain; do not add a
  global allow to silence a new warning without a written justification
  next to it.
- **The crate owns its error type.** `imi_core::Error` carries the
  diagnostic, the chain of phase context above it, and two
  classifications: `kind()` and `device_state()`. It implements
  `std::error::Error`, so a consumer can embed it as a `#[source]`, box
  it, or convert it into `anyhow::Error`.
- **Build errors with `bail!`, `err!` and `.context()`**, from
  `crate::error`. These deliberately mirror the `anyhow` API they
  replaced, so a phase reads as it always did — put context at every `?`
  boundary that would otherwise be uninformative. The user reading the
  final error must be able to reconstruct _what_ operation, _on which
  path_, _with what underlying cause_.
- **Classify at the boundary, not by message text.** `Cancelled` and
  `VerificationMismatch` are types; `Error::at(DeviceState)` is called
  once per phase wrapper. Rewording a diagnostic must never reclassify a
  failure. A layer added by `.context()` inherits the kind and device
  state beneath it — losing that silently downgraded `Cancelled` to
  `Failed` once already.
- **`imi-core` depends on `libc`, `nix` and the four decompressors.**
  Nothing else. `anyhow` belongs to the binary, where an application
  error type is the right tool; adding it back to the library would put
  it in every downstream build.
- `nix` wrappers preferred over raw `libc` where they exist. Drop to
  `libc` only for things `nix` doesn't cover — currently `swapoff`
  alone. `fcntl(F_SETFL, O_DIRECT)` was on this list until `nix`'s
  `fcntl` turned out to wrap it over `AsFd`, which also made the
  descriptor's validity structural rather than a comment.
- No `unwrap()` outside test code, with a small handful of audited
  exceptions for static `expect`-with-explanation in `ProgressStyle`
  template construction.
- No `println!`/`eprintln!` from inside long loops. Use `indicatif`
  progress bars, and finish them with `finish_and_clear()` alone — it
  leaves the cursor at the start of the row the bar occupied, so the
  next phase line reuses it. A `println!()` after it leaves a blank line
  that the phases without bars do not have. (An earlier revision of this
  rule prescribed that `println!()`; `ui.rs::phase_finished` never made
  the call, and doc-00 carried the same wrong prescription.)
- For any `thread::sleep` longer than ~100 ms inside a phase that the
  operator might want to interrupt, use `common::cancel::cancellable_sleep`
  (or, for the cooldown's UI-driven case, a per-second polling loop
  with explicit cancel-flag checks). A naïve `thread::sleep(Duration::
  from_secs(N))` will make Ctrl+C wait up to N seconds before the
  program responds, which is indistinguishable from a hang to the
  operator. The throttle paths in Phases 4 and 5b, and the udev-settle
  sleeps in Phase 7, all use `cancellable_sleep` for this reason.
- Progress bars in Phases 4 (raw) and 5b (verify) use the shared
  `UNIFIED_BAR_TEMPLATE` constant in `crates/imi/src/progress.rs`. It is
  private to that module: the bars live in the binary, because the
  library reports through `Events` and renders nothing. Do not introduce a parallel
  template string — the operator-facing format is part of the
  observability contract, and two source-of-truth templates can
  silently drift apart. The compressed-image branch of Phase 4 keeps
  its spinner template (no percent/total available), which is the only
  intentional exception.

## Testing additions

CLI parser changes ship with unit tests. Anything that touches the
destructive pipeline (Phases 3–7) is exercised manually before merge —
against scratch USB sticks for hardware-specific behavior (real FTL
timing, `BLKRRPART` uevents, udisks2 races), and/or against **loop
devices in a privileged container**, which safely exercise the entire
pipeline end-to-end: loop nodes are the sanctioned test target, the
wipe/flash/verify content can be inspected byte-for-byte through the
backing file, and refusal paths (mounts, swap, `O_EXCL` contention,
read-only via `losetup -r`, undersized devices) are all reproducible.
**On `--test-threads=1`.** The commands above pass it, and it is a
readability choice rather than a correctness requirement — worth stating
because a flag with no stated reason gets copied into places it does not
belong, or dropped in places it does.

Measured: `losetup --find --show` is atomic (eight concurrent attaches
gave eight distinct devices), the kernel allocates loop devices past
`max_loop` on demand (`--find` returned `/dev/loop8` with all eight in
use), no two tests share a mountpoint, backing-file name or fixture tag,
and the suite passes repeatedly at `--test-threads=8` including with six
of eight loop devices already occupied.

What serial execution buys is legible failure. These suites mutate
global state — mounts, swap, loop devices — and print FATAL notices from
`FlashGuard::drop` as part of passing tests. In parallel those lines
interleave with test names from other threads, and working out which
test produced which notice costs more than the run saves. Use `-1` when
something is failing; drop it when you just want the answer.

Add a doc note in the relevant `.agents/docs/*.md` file describing the
manual test you ran. The core loop-device scenarios are additionally
encoded as gated integration tests — `sudo -E cargo test -p imi --test
loop_pipeline -- --ignored --test-threads=1`, plus `sudo -E cargo test -p
imi-core --test phase_pipeline -- --ignored --test-threads=1` for the
per-phase API — which any root+loop
machine (including CI runners with privileged containers) can execute.

## Why `Events` is generic with a `?Sized` bound

Every function taking a sink is `fn ...<E: Events + ?Sized>(..., events: &mut E)`.
That one bound serves two callers who want opposite things.

**A CLI or script** passes a concrete sink and gets static dispatch: the
calls inline, and `()` — the silent sink — optimises away to nothing.

**A GUI** passes `&mut *boxed` from a `Box<dyn Events>` and `E` binds to
`dyn Events` itself. The `?Sized` is what allows that, and it matters
far more than the dispatch cost. A GUI holds its sink in application
state across frames; a generic parameter there would spread through
every type that touches the app — the event loop, the window handler,
anything embedding it. It also wants runtime selection (progress dialog
against log pane against headless), heterogeneous fan-out
(`Vec<Box<dyn Events>>`, which generics cannot express at all), and
plugin or FFI sinks, which can only ever be trait objects.

Rejected: a plain `E: Events` bound plus a blanket
`impl<T: Events + ?Sized> Events for &mut T`. It reaches the same place
through thirty lines of forwarding that must stay in step with the trait
and an extra blanket impl in the coherence space. `?Sized` on the bound
does the same work with nothing to maintain.

Monomorphisation costs nothing here, which is worth recording because
the opposite is the usual objection. The `imi` binary has exactly one
call site and one sink type, so the phases are instantiated once —
`()` appears only in tests. Measured at the 0.2.0 tag: the release binary was **1,928,976 bytes
against 0.1.7's 1,942,000**, i.e. slightly smaller. It has drifted since
with unrelated changes — 1,932,232 at the time of writing — so treat the
figure as dated evidence for the argument rather than a current
measurement; the point is the order of magnitude, not the digits. A consumer
using several concrete sinks would pay per type; one passing
`&mut dyn Events` pays nothing extra at all.

For scale: `progress` fires once per 4 MiB chunk, so a 64 GiB image
makes 16,384 calls in the flash and as many again in the verify —
about 32,800 across the run. Dynamic dispatch across all of them measured
51.8 µs against generics' 1.29 µs — a 40x ratio and 50 µs absolute, on a
flash that runs eleven minutes and issues 16,384 `pwrite` syscalls. The
generic form is free, so take it; but the reason for `?Sized` is the
GUI's ergonomics, not the microseconds.

## The events sink is a test seam

Routing output through [`imi_core::Events`] made code testable that was
not before. `cooldown` used to sleep and write a countdown to stdout —
nothing could observe it, and mutation testing showed the whole function
could be replaced with `Ok(())` undetected. It now emits
`phase_started` / `progress` / `phase_finished`, and a recording sink in
its unit test asserts all three, including that a cancelled cooldown
still closes its phase (otherwise a front end is left holding a
countdown that never ends).

Use the same pattern for anything that reports: implement `Events` on a
struct that pushes into `Vec`s and assert on what arrived. Use
`common::testing::Recorder`, which is that sink — three phase test
modules had each grown their own copy, two of them byte-identical,
before it was shared.

## Keeping safety decisions testable

Most of this tool's safety lives in small decisions embedded in code
that needs root and real hardware to run: is this a block device, does
this device still match what the operator confirmed, is the image larger
than the target. Written inline, those decisions cannot be reached by
any test, and mutation testing showed the consequence plainly — the
same-path refusal, the minimum-size floor, the `S_IFBLK` gates, the
replug-identity comparisons and the guard's arm/disarm state machine
could each be inverted or deleted with the suite still green.

The convention that fixes this: **separate the decision from the I/O**.
The I/O function does its reads and then calls a pure helper that
decides; the helper is unit-tested directly. Existing examples to copy:

- `common/identity.rs` — `check_rdev` / `check_model` / `check_size` /
  `check_serial` / `check_wwid`, with `verify_claimed` doing the
  `fstat`/sysfs/ioctl reads around them.
  Note it keeps the _order_ of those reads: checking `rdev` first means
  a replugged device reports "device replugged" rather than a bare
  `ENODEV` from a later ioctl.
- `phases/phase_0.rs` — `ensure_distinct_paths`,
  `ensure_device_large_enough`, `ensure_image_fits`,
  `ensure_physical_or_loop`, `confirm_destruction`, `is_same_file`.
- `common/sysfs.rs` — `_in(root, ...)` variants taking the sysfs root.
- `common/throttle.rs` — the rate-to-nanoseconds conversion, which also
  rejects a zero rate for every caller rather than trusting the CLI.

When adding a check to a phase, add it as a pure function with its own
test. Two things are worth asserting every time: both directions (a
guard that accepts everything and a guard that rejects everything are
both broken), and the exact boundary where one exists.

**A root-gated test cannot substitute for a unit test here.**
`cargo mutants` runs the suite without `--ignored`, so anything behind
that flag is invisible to it — a mutant covered only by a root-gated
test still reports as MISSED, correctly, because nothing in a normal
`cargo test` would catch the regression. `phase_0::confirm_destruction`
was extracted for exactly this reason.

**Run it with `--in-place`.** By default `cargo mutants` copies the tree
to a temp directory per mutant, which discards the build cache and costs
about fifty seconds each — enough that a single file can exceed any
reasonable time budget. `--in-place` mutates the working tree and reuses
the incremental build, cutting that to a few seconds:

```sh
cargo mutants -p imi-core --in-place --file crates/imi-core/src/error.rs
```

### Branches gated on device state

The recurring blind spot in this project is not untested _code_, it is
untested _device state_. Every fixture flashes a clean loop device, so
any branch that needs a mount, a swap area, a stacked volume or a
reappearing automount never executes — and a change to it passes every
gate. That is how the mount-path wording changed unnoticed, and how the
`swapoff` path went from 0.1.7 to here with no test ever entering it.

Instrumented coverage would find these directly, but `llvm-tools` is not
installed here, so the substitute is to enumerate the states the code
branches on and ask which fixture produces each:

| state                      | how a test creates it                    | covered by                                                    |
| -------------------------- | ---------------------------------------- | ------------------------------------------------------------- |
| mounted target             | `mkfs.ext4` + `mount` under `/run/media` | `phase_1_unmounts_a_mounted_target`                           |
| active swap                | `mkswap` + `swapon`                      | `phase_1_disables_swap_on_the_target`                         |
| claimed device             | Phase 2, then a second `O_EXCL` open     | `the_phase_2_claim_excludes_other_openers`                    |
| device changed after probe | `truncate` + `losetup -c`                | `a_device_that_changes_size_after_probing_is_refused`         |
| device ≠ image             | flash, then alter the image              | `verification_detects_a_device_that_does_not_match_the_image` |
| interrupted mid-write      | `--throttle` + `SIGINT` to a child       | `an_interrupted_flash_warns_that_the_device_is_unsafe`        |

Still unreachable here: stacked volumes (LVM/md/dm holders) and an
automount that reappears during Phase 7's retry loop. Both need
device-mapper or a running udisks2, neither of which this container has.

Any test that creates such a state must clean it up **before** its
assertions, not after: a failed assertion otherwise leaves a mount or a
swap attached to a loop device the harness is about to detach.

### Integrity checks

Scripted edits damage files in ways the compiler sometimes accepts.
This project has, at various points, carried a swallowed closing brace,
a duplicated comment block, orphaned doc comments, and a live
`cargo-mutants` mutation left in `phase_0.rs`. Sweep for the class, not
the instance:

```sh
grep -rl 'changed by cargo-mutants' crates/   # mutation residue
git status --short                            # anything unexpected
```

Beyond that, worth checking after a scripted pass: duplicated
consecutive comment lines, a `///` followed by a blank line (an orphaned
doc), leftover `TODO`/`dbg!`/`todo!`, trailing whitespace, tabs, and
files not ending in exactly one newline. `cargo fmt --check` catches the
last three; the first two it does not.

Verify the shipped artefact too, not just the tree it came from: extract
the archive somewhere else and run the gates there. It carries `.git`,
so `git fsck` and `git status` work on the extract and will show whether
what shipped matches what was committed.

### Coverage baseline

Every file swept with `--in-place`, integrity-checked after each run.
Use this to spot a regression: a new survivor outside the three
categories below is a real gap.

| file                                  | caught | missed | survivors are                                         |
| ------------------------------------- | ------ | ------ | ----------------------------------------------------- |
| `error.rs`                            | 8      | 0      | —                                                     |
| `events.rs` `config.rs` `context.rs`  | 11     | 0      | —                                                     |
| `mount.rs`                            | 27     | 7      | 4 I/O wrappers, 2 equivalent, 1 wrapper               |
| `guard.rs` `identity.rs` `aligned.rs` | 30     | 3      | 2 `Drop`, 1 hardware                                  |
| `image.rs` `sysfs.rs`                 | 18     | 5      | 1 equivalent, 4 need device nodes                     |
| `phase_0.rs`                          | 22     | 2      | `phase0_root_check`, both directions                  |
| `phase_1.rs`                          | 0      | 0      | — (a no-op branch was removed rather than covered)    |
| `phase_2.rs`                          | 1      | 1      | 1 equivalent (`O_EXCL`/`O_CLOEXEC` bits are disjoint) |
| `phase_3.rs`                          | 6      | 2      | whole-function, need a device                         |
| `phase_4.rs`                          | 14     | 1      | 1 equivalent (`<` vs `<=` before an empty read)       |
| `phase_5.rs`                          | 11     | 3      | whole-function, need a device                         |
| `phase_6.rs`                          | 1      | 0      | —                                                     |
| `phase_7.rs`                          | 3      | 0      | —                                                     |
| `imi` (binary)                        | 28     | 12     | methods that only print; `confirm` needs a tty        |

Phases 1 through 7 were measured during the file-by-file review; the
earlier rows predate it. Four of those rows moved because the review
added coverage rather than because the code changed: `phase_4` gained a
unit test for the phase-ordering refusal, which was asserted only behind
`--ignored` and therefore invisible here; `phase_6` and `phase_7` gained
tests for the events they emit and the verdict they reach; and
`phase_1`'s single survivor was a no-op branch deleted rather than
covered. Measured per file with `--file`, which is why a total across
rows will not match a whole-crate run.

Three categories, and only a fourth would be a defect:

1. **Equivalent mutants.** `|` against `^` over disjoint bit fields
   (`O_EXCL`/`O_CLOEXEC`, the octal decoder), `<` against `<=` on a loop
   that then reads zero bytes. Unkillable by construction.
2. **I/O wrappers.** A function that reads a fixed path and delegates to
   a tested pure core. Make the core testable, not the wrapper.
3. **Environment-bound branches.** `phase0_root_check` cannot be
   exercised in both directions from a process already running as root;
   `confirm` needs a terminal; `FlashGuard::drop` writes to stderr during
   an unwind.

Timeouts count as caught: the mutant hangs a retry loop, which is
detection, not escape.

**Never wrap it in a shell `timeout`, and check `git status` after.**
`--in-place` edits the real source and restores it when it finishes; a
killed run leaves the mutation applied. This happened here — a
`timeout 175` cut a run mid-mutant and left

```rust
if dev_size < 2 + /* ~ changed by cargo-mutants ~ */ WIPE_REGION {
```

in `phase_0.rs`, where the size floor should be `2 * WIPE_REGION`. It was
one commit away from shipping a check that accepts a device two bytes
larger than one wipe region, which Phase 3 would then fail to wipe with
the guard already armed.

It also poisons the results: any run started against corrupted source
measures the wrong baseline, and a mutant reported MISSED may only be
the interrupted one. If a survivor looks impossible — as that one did,
since the floor test computes `2 * WIPE_REGION` independently and would
have caught it — suspect residue before believing the report.

`git checkout -- crates/` recovers, but it discards uncommitted work
alongside the mutation, so commit before running.

## Testing `common/sysfs.rs`

Every lookup in this module resolves against the machine's real `/sys`,
which for a long time made the whole module unreachable from a unit test:
mutation testing reported 24 of 24 mutants surviving, including
`is_partition -> false`, which would let a flash proceed onto a partition.

Each function now has an `_in(root, ...)` variant taking the sysfs root,
with the original as a one-line wrapper passing the real constant. No
caller changed. Tests build a synthetic tree in a tempdir and exercise
the logic directly — mirroring the kernel's actual layout, where a
partition appears **both** at top level (`/sys/class/block/sda1`) and as
a child of its disk (`/sys/class/block/sda/sda1`); `is_partition`
consults the former and `partitions_of` scans the latter, so a fixture
carrying only one of them tests something sysfs never looks like.

When adding a function here, add its `_in` variant at the same time and
test that. Keep the `_in` variant private: its only callers are the
public wrapper beside it and the tests in the same file, so anything
wider advertises a crate-internal API that does not exist. The surviving mutants are now only the production wrappers,
which cannot be unit-tested by construction — `kname_for_path` and
`kname_for_devt` additionally need real device nodes and remain covered
only by the root-gated suites.

## What to do when something looks wrong

If you spot a real bug, fix it. If you spot a defence that looks redundant
or paranoid: **the burden of proof is on removal, not retention.** Read
the phase doc, read the comment, search the git history for the rationale
before deleting it. Almost every "redundant" check in this codebase
exists because someone bricked their boot drive.

## Verifying progress-bar output

`indicatif` draws to stderr and suppresses itself when stderr is not a
terminal. Every "stderr identical" comparison in this project's history
is therefore **silent about bar behaviour** — redirecting output hides
exactly the thing under test.

`.agents/ptycap.py` runs a command under a pseudo-terminal and captures
what a real terminal would receive, spinner glyphs and cursor control
included:

```sh
python3 .agents/ptycap.py ./target/release/imi -i img.xz -d /dev/loop0 --yes --skip-cooldown > out.bin
```

Capture a baseline from a known-good binary before touching the flash or
verify loops, and diff against it afterwards. Without this, a change that
breaks the bar passes every other gate in the repository.

### Verified against 0.1.7

The refactors between 0.1.7 and now — workspace split, public API pass,
typed errors, sans-I/O — changed a great deal of structure and no
observable behaviour. Confirmed by running the 0.1.7 binary against the
current one:

- **Device contents byte-identical** on all five formats (raw, gzip,
  bzip2, xz, zstd) and on a 32 MiB multi-chunk image.
- **CLI output identical** on `--help`, `--version`, and the
  nonexistent-image, non-block-device, same-path and zero-throttle
  refusals — stdout, stderr and exit codes.
- **Progress rendering identical**: 3395 bytes, 17 frames, the same
  `12→25→…→100→100 | 12→25→…→100` sequence.

Keep a 0.1.7 build around for this. Structural refactors are cheap to
verify against a reference binary and nearly impossible to verify by
reading, and every behavioural regression found in this project was
found by comparing outputs rather than by inspecting code.

## The library reports; it does not print

`imi-core` writes nothing to a terminal. Phases emit through
[`imi_core::Events`] and the `imi` binary renders — that is what makes
the crate usable from a GUI, which can neither intercept `println!` nor
answer a prompt read from `/dev/tty`.

The vocabulary is `phase_started(Phase)` / `phase_skipped(Phase)` /
`phase_finished(Phase, PhaseOutcome)` — typed, so a front end labels
steps in its own words — plus `progress(Phase, done, total)`, `action`,
`warning`, `confirm(&Summary) -> bool` and `finished`.

`progress` carries its phase for a reason worth not losing: it used to
be `progress(done, total)`, and `done` was bytes for Flash and Verify
but **seconds** for Cooldown, under a doc comment that said "bytes
written". A front end could only tell the two apart through state it
kept on the side. The phase parameter is what makes the pair
self-describing, and a change that drops it reintroduces the ambiguity. Every method defaults to a no-op **except `confirm`,
which defaults to refusing**: a sink that has not considered the question
must not be able to authorise wiping a disk. `()` implements the trait,
so `&mut ()` is a silent sink.

The one exception is `FlashGuard::drop`, which still writes its FATAL
notice to stderr. It is a backstop for the unwind path, where no error
value survives to carry the news; the normal path reports through
[`imi_core::Error::device_state`] instead.

When adding output to a phase, add an event — never a `print!`. Note
that a grep for `println!`/`eprintln!` will not find `write!` on a
locked handle, which is how the cooldown countdown survived the original
refactor.

#### Progress-bar rendering after the bars moved

Resolved. A pty capture of a 32 MiB flash is **3395 bytes and 17 frames
both before and after**, with the identical sequence
`12→25→38→50→62→75→88→100→100 | 12→25→38→50→62→75→88→100` and identical
cursor control. The only difference left is the rate figure's unit, and
that number varies elevenfold between runs of the _same_ binary, so it
is not a property of the code.

Three defects were found getting here, each by comparing frame
sequences rather than single numbers:

1. Creating the bar lazily on the first `progress` started its clock
   after the data had arrived (a 3 MiB image read 21.91 GiB/s).
2. The opening `progress(0, total)` tick called `set_position(0)`,
   handing the estimator a sample with no elapsed time.
3. **The serial flash arm had no opening tick at all** — the insertion
   was lost when `phase_4` was redone after a rollback, leaving only its
   orphaned comment. The bar was therefore born at 4 MiB and its 12%
   frame never drawn. Verify and the pipelined arm were unaffected,
   which is why the fault looked like a rate problem rather than a
   missing event.

The compressed (pipelined) path differs by one frame, and in the current
binary's favour: it shows `0 B → 32.00 MiB`, where 0.2.0-alpha showed
only `0 B` and never reflected the final position. The spinner has no
total to render a percentage against, so the closing byte count is the
only completion signal a compressed flash gives. Left as is.

Method that found it: extract the percentage from every frame in both
captures and diff the _sequences_. Comparing the first or final rate
tells you nothing — run the old binary three times and watch it
disagree with itself.
