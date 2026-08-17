# IMI - IMage Inoculator

Flash bootable ISO and IMG images to USB drives and SD cards on Linux, with
exclusive device locking, aligned direct I/O, and byte-for-byte verification.

`imi` writes disk images to block devices: USB sticks, SD cards, NVMe drives.
It refuses dangerous targets before writing anything, holds the device under an
exclusive kernel lock for the entire operation, and reads every byte back to
confirm the flash landed correctly. Compressed images (gzip, xz, bzip2, zstd)
are decompressed on the fly, including multi-member/multi-stream archives
produced by `pigz`, `pbzip2`, and `xz --threads`.

## What it checks

Every one of these happens before or around the write, and any of them will
stop the run:

- Refuses partitions (`/dev/sda1`), device-mapper/LVM/dm-crypt/MD RAID stacks,
  loopbacks backed by the image file itself, and write-protected devices.
- Evicts desktop auto-mounts (`/media`, `/run/media`, `/var/run/media`) and swap before
  writing; re-checks after acquiring the lock to close the TOCTOU window.
- Holds a kernel `O_EXCL` claim from before the first destructive write through
  verification, blocking `udisks2` and other userspace openers.
- Wipes stale GPT/MBR signatures (first and last 1 MiB) so the new partition
  table is the only one the kernel sees.
- Writes through `O_DIRECT` in 4 MiB aligned chunks, bypassing the page cache.
- Waits 10 seconds after `fdatasync` for cheap USB-NAND bridge controllers to
  drain their write cache to flash (skippable with `--skip-cooldown`).
- Reads back exactly the bytes written under the same lock, comparing against a
  fresh decompress of the source image, and reports the absolute byte offset of
  the first mismatch (skippable with `--skip-verification`).
- Prints a `FATAL` warning naming the interrupted phase if the process is killed
  or panics while the device is in a partially-written state.

For compressed images, decompression runs on a worker thread overlapping the
device I/O, so the wall-clock cost of decompression is mostly hidden behind the
USB write. Raw images stay single-threaded (their read cost is negligible).

## Requirements

- Linux (kernel 2.6.26 or later — needs `/proc/self/mountinfo`).
- Root privileges (`sudo`).
- Rust 1.95+ to build from source.

## Install

```
cargo install imi
```

Or with [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall):

```
cargo binstall imi
```

## Build from source

```
git clone https://github.com/veralvx/imi.git
cd imi
cargo build --release
```

## Usage

```
sudo imi --img image.iso --dev /dev/sdc
```

If the command fails with 'command not found', add your `PATH`:

```
sudo env PATH="$PATH" imi --img image.iso --dev /dev/sdc
```

The tool prints a confirmation prompt showing the device model, size, and the
image path. Type `yes` to proceed (or pass `--yes` for scripted use).

A compressed image works identically — `imi` detects the format from the file's
magic bytes, not the extension:

```
sudo imi --img image.iso.zst --dev /dev/sdc
```

To throttle writes on a cheap drive that thermally throttles at sustained rates:

```
sudo imi --img image.iso --dev /dev/sdc --throttle 8M
```

### Options

```
Safely flash an ISO/IMG (optionally compressed) to a USB block device.

Usage: imi [OPTIONS] --img <PATH>

Options:
  -i, --img <PATH>         Path to the source .iso/.img file (may be gzip/xz/bzip2/zstd compressed)
  -d, --dev <DEVICE>       Target block device, e.g. /dev/sdc. Must be a whole disk, not a partition. Omit to pick interactively: candidate devices are listed (path, size, model; removable first) for arrow-key selection. Only whole disks with media, no block-layer holders, and no mounts outside /media, /run/media, /var/run/media are offered, so the system disk never appears. Esc aborts; without a terminal the candidates are printed and the run refuses. Nothing is ever auto-selected
  -t, --throttle [<RATE>]  Write/read rate cap (e.g. 500K, 8M, 1G). Omit flag entirely for unthrottled; pass `-t` with no value to default to 8M
  -y, --yes                Skip the interactive TTY confirmation. Intended for automation; use with care
      --skip-verification  Skip Phase 5b byte-for-byte verification. The hardware cooldown (Phase 5a, unless --skip-cooldown), kernel partition-table sync (Phase 6), and automount defense (Phase 7) all still run — only the readback compare is omitted. Use this when you trust the device and the throughput gain is worth the loss of the defect-detection pass
      --skip-cooldown      Skip the 10-second hardware cooldown (Phase 5a). The cooldown lets cheap USB-NAND bridge controllers drain their DRAM write cache to flash after fdatasync returns; skipping it risks silent corruption on unplug for such devices. Intended for loop devices, automated tests, and high-quality media whose controllers honor cache flushes
  -h, --help               Print help
  -V, --version            Print version
```

## Execution phases

`imi` runs a fixed sequence. If any phase fails, the process aborts and either
leaves the device untouched (phases 0–2) or prints a `FATAL` warning describing
the interrupted state (phases 3–5b).

| Phase | What happens                                                                                                                                                                                        |
| ----- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 0     | Validates the image (regular file, detected compression) and the target (whole disk, not a partition, not write-protected, no LVM/dm-crypt/MD/zram stack).                                          |
| 1     | Parses `/proc/self/mountinfo` and `/proc/swaps`; unmounts auto-mounted filesystems under `/media`, `/run/media` or `/var/run/media`; disables swap on the device. Refuses filesystems mounted outside the whitelist. |
| 2     | Opens the device with `O_EXCL` (kernel-level exclusive claim), then re-reads mounts to catch anything that raced the open.                                                                          |
| 3     | Arms the interrupt guard. Wipes the first and last 1 MiB to destroy stale GPT/MBR/PMBR signatures.                                                                                                  |
| 4     | Writes the image in 4 MiB `O_DIRECT` chunks. Compressed images decompress on a worker thread; raw images write single-threaded.                                                                     |
| 5a    | 10-second cooldown for USB-NAND FTL cache drain (`--skip-cooldown` skips).                                                                                                                          |
| 5b    | Reads back every written byte under the same lock and compares against a fresh decompress of the source image (`--skip-verification` skips).                                                        |
| 6     | Issues `BLKRRPART` so the kernel re-reads the new partition table, then drops the `O_EXCL` lock.                                                                                                    |
| 7     | Sweeps for desktop auto-mounts that fired between lock release and process exit.                                                                                                                    |

## Testing

Unit tests cover the policy parsers (`/proc/self/mountinfo`, `/proc/swaps`,
sysfs device topology, mount classification), the `O_DIRECT` write and verify
helpers, the pipelined flash and verify protocols (shutdown, cancellation, error
propagation, worker panic forwarding, arm parity), multi-member decompression
for all four formats, and the CLI flag surface:

```
cargo test
```

Two further suites drive real loop devices end to end and require root plus a
free `/dev/loopN` — one exercising the `imi` binary as a black box, one driving
`imi-core`'s per-phase API directly:

```
sudo -E cargo test -p imi --test loop_pipeline -- --ignored --test-threads=1
sudo -E cargo test -p imi-core --test phase_pipeline -- --ignored --test-threads=1
```

## Using it as a library

The pipeline lives in [`imi-core`](https://crates.io/crates/imi-core); the
`imi` binary is a thin CLI over it. Depend on the library to drive a flash from
your own code:

```console
cargo add imi-core
```

```rust,no_run
use std::path::PathBuf;

let mut config = imi_core::Config::new(
    PathBuf::from("image.iso.zst"),
    PathBuf::from("/dev/sdc"),
);
config.yes = true;                       // skip the interactive confirmation
imi_core::run(&config).expect("flash failed");
```

To make a run interruptible, use `imi_core::run_with_cancel(&config, &flag)`
and set the `AtomicBool` from your own signal handler. The library never
installs one: taking over process-wide signal disposition is the
application's decision, which is why the `imi` binary owns it.

The library writes nothing to a terminal. Progress, phase changes,
warnings and the destructive-action confirmation all arrive through
`imi_core::Events`, which you implement — so a GUI renders them its own
way. `imi_core::run(&config)` uses a silent sink; `run_with(&config,
&cancel, &mut ui)` is the full form. Note that `Events::confirm`
defaults to **refusing**, so an unattended caller sets `Config::yes`.

The sink is a generic parameter with a `?Sized` bound. A concrete type
dispatches statically and the silent sink compiles away; a
`Box<dyn Events>` — what a GUI keeps in its application state — is
passed as `&mut *boxed` and works unchanged.

Failures come back as `imi_core::Error`, which implements
`std::error::Error`, `Display`, `Debug`, `Send` and `Sync` — so it drops
into `thiserror` enums as a `#[source]`, boxes as `dyn Error`, or
converts into `anyhow::Error` with `anyhow::Error::new` (the library
itself does not depend on it). `Display`
renders the phase context, and `{:#}` prints the whole chain:
`Phase 4: flash write loop: cancelled by user`.

Two classifications come with it, and they answer different questions.
`Error::device_state()` says whether the hardware is safe — `Untouched`,
`Indeterminate` (a partial image; must be re-flashed), or `Written` (the
image is intact, the failure came afterwards). `Error::kind()` says how
to report it — `Refused`, `Cancelled`, `VerificationFailed`, `Failed`.
They are independent, because a cancellation can land either before or
during the write.

Each phase is also public as `imi_core::phases::phase_N::run`, so a caller that
wants to stop between phases, report progress, or substitute a step can
sequence them itself. `imi_core::run_with` is the function to read while doing
so — `run` and `run_with_cancel` are two-line wrappers that supply a silent
sink and a throwaway cancel flag before delegating to it.

One step there is not a phase and is easy to miss: `run_with` ends with
`events.finished(...)`, which is the only positive signal this crate gives
that a device is safe to unplug. A caller sequencing the phases by hand emits
nothing unless it makes that call itself.

Everything from Phase 1 onward requires root, and Phases 3-7 are destructive.

## Design documentation

Per-phase design rationale — locking semantics, `O_DIRECT` alignment invariants,
the `FlashGuard` FATAL contract, the threading pipeline's protocol and
shutdown proofs — lives in `.agents/docs/`. Threading in particular is
`.agents/docs/11-threading.md`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))
