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

2. **The seven-phase pipeline is canonical.** Phases run in order
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
   `arm(GuardPhase::WipingSignatures)` before the first destructive
   write (Phase 3) → `set_phase(...)` at the start of every subsequent
   destructive phase (4 / 5a / 5b) → `disarm()` only after Phase 5b
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
threads without a design document equivalent to `.agents/docs/threading-plan/`. (Thread census during a compressed Phase 4, for
anyone counting `clone`s under strace: main + the `ctrlc` handler
thread + indicatif's steady-tick spinner thread + the one worker —
the ticker predates the pipeline and belongs to the progress UI, not
to this directive.)

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
- `anyhow` for application errors with `.context()` / `.with_context()` at
  every `?` boundary that would otherwise be uninformative. The user
  reading the final error must be able to reconstruct _what_ operation,
  _on which path_, _with what underlying cause_.
- `nix` wrappers preferred over raw `libc` where they exist. Drop to
  `libc` only for things `nix` doesn't cover (currently: `swapoff`,
  `fcntl(F_SETFL, O_DIRECT)`).
- No `unwrap()` outside test code, with a small handful of audited
  exceptions for static `expect`-with-explanation in `ProgressStyle`
  template construction.
- No `println!`/`eprintln!` from inside long loops. Use `indicatif`
  progress bars; finish them with `finish_and_clear()` followed by a
  `println!()` so output doesn't bleed into the next phase.
- For any `thread::sleep` longer than ~100 ms inside a phase that the
  operator might want to interrupt, use `flash::cancellable_sleep`
  (or, for the cooldown's UI-driven case, a per-second polling loop
  with explicit cancel-flag checks). A naïve `thread::sleep(Duration::
  from_secs(N))` will make Ctrl+C wait up to N seconds before the
  program responds, which is indistinguishable from a hang to the
  operator. The throttle paths in Phases 4 and 5b, and the udev-settle
  sleeps in Phase 7, all use `cancellable_sleep` for this reason.
- Progress bars in Phases 4 (raw) and 5b (verify) use the shared
  `flash::UNIFIED_BAR_TEMPLATE` constant. Do not introduce a parallel
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
Add a doc note in the relevant `.agents/docs/*.md` file describing the
manual test you ran. The core loop-device scenarios are additionally
encoded as gated integration tests — `sudo -E cargo test --test
loop_pipeline -- --ignored --test-threads=1` — which any root+loop
machine (including CI runners with privileged containers) can execute.

## What to do when something looks wrong

If you spot a real bug, fix it. If you spot a defence that looks redundant
or paranoid: **the burden of proof is on removal, not retention.** Read
the phase doc, read the comment, search the git history for the rationale
before deleting it. Almost every "redundant" check in this codebase
exists because someone bricked their boot drive.
