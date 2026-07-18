# 06 — Phase 5: Cooldown and Verification

**Source:** `src/verify.rs::cooldown`, `src/verify.rs::verify`,
`src/main.rs::run` (Phase 5 block).

**Purpose:** Allow the device to finalize NAND/FTL operations before
any read-back, then optionally compare the device contents byte-for-byte
against the source image.

This phase splits in two:

- **5a — Cooldown.** Runs unless `--skip-cooldown` (loop devices, tests,
  flush-honoring controllers; cli.rs carries the operator-facing risk).
- **5b — Verification.** Skipped with `--skip-verification`.

## Phase 5a — the 10-second cooldown

```rust
verify::cooldown(10, &cancel)?;
```

This is **not** ritual. Cheap USB-NAND bridge controllers (Phison, SMI,
Realtek, JMicron) implement an internal write cache in DRAM. When the
host issues a write:

```
host → SCSI WRITE(10/16) → bridge controller → DRAM write buffer
                                                  │
                                          (returns success here)
                                                  │
                                                  ▼
                                         FTL log + NAND program
                                         + GC + L2P table update
```

The bridge ACKs the write the moment bytes hit DRAM. The actual NAND
program, garbage collection (TLC/QLC blocks need erase-then-program
cycles), and FTL logical-to-physical table maintenance happen
afterward, on the controller's own schedule, often for 5–30 seconds
after the host stops issuing writes.

`fdatasync()` on the host side issues a SCSI `SYNCHRONIZE CACHE`
command. **Most USB mass-storage bridges do not translate
`SYNCHRONIZE CACHE` meaningfully**: they ACK it without flushing
because they don't expose a meaningful host-cache distinction (the host
has been writing through to controller-DRAM all along, in the
controller's view). `hdparm -f` is therefore not a substitute for the
wall-clock wait.

**What goes wrong without the cooldown:**

- Operator unplugs immediately on success → controller's pending
  GC/program operations are interrupted mid-write → resulting NAND
  state is undefined → device is silently corrupted (boot fails on
  next plug).
- Verification reads at full speed during the drain window → bridge
  controller browns out (controller is doing GC, FTL writes, _and_
  serving reads concurrently on a 1 W power budget) → verification
  reports false-positive failures.

A 10-second wall-clock wait reliably avoids both. We borrow this
constant from the bash original where it was tuned empirically against
~30 different USB sticks.

### Cooldown implementation

```rust
for remaining in (1..=seconds).rev() {
    if cancel.load(Ordering::SeqCst) {
        bail!("cancelled by user during cooldown");
    }
    write!(stdout, "\rCooldown and FTL sync... ({remaining}s)   ")?;
    sleep(Duration::from_secs(1));
}
writeln!(stdout, "\rCooldown and FTL sync... done       ")?;
```

In-place countdown via `\r` so we don't pollute the log. Trailing
spaces overwrite any residual from a longer previous value (e.g.
"10s" → " 9s" without padding would leave a stray 's'). The cancel
flag is checked every second so Ctrl+C is responsive. The final state
overwrites the countdown with `done`.

## Phase 5b — verification

```rust
if cli.skip_verification {
    println!("Skipping verification (--skip-verification).");
} else {
    println!("Verifying data integrity...");
    verify::verify(&mut guard, &img_canon, comp,
                   outcome.bytes_written, cli.throttle, &cancel)?;
}
```

### Critical: verification runs while `O_EXCL` is still held

If we released the FD before verifying, `udisks2` / GNOME / KDE would
auto-mount the new filesystem within milliseconds. Mount-time mutations
include:

- ext4: superblock fields update (mount count, last-mount-time).
- xfs / btrfs: log/journal replay if any pending transactions exist.
- exFAT/FAT: dirty-bit clear, last-access fields update.
- All: `.Trash-NNN` directories created by GNOME automatic-cleanup.

Any of these changes flips bytes that no longer match the source image.
Holding `O_EXCL` through the verify pass — until Phase 6 — is therefore
load-bearing. The lock is released only after verification passes.

### Pre-verify ioctls

```rust
unsafe { ioctl::blkflsbuf(fd)?; }       // flush kernel buffer cache
set_direct(fd, false)?;                 // we're using page cache for reads
```

`BLKFLSBUF` invalidates the kernel's buffer cache pages for this device.
Without it, reads might return cached pages from the _write_ path
(post-`fdatasync` the kernel may still have read-after-write cached
pages from the page-cache tail in Phase 4) rather than going to the
NAND. We need to actually round-trip through hardware to detect bad
blocks, which is the whole point of verification.

`O_DIRECT` is disabled because the read pattern includes the trailing
chunk which is not sector-aligned in length. Page-cache reads handle
the unalignment transparently.

### Read-and-compare loop (the serial arm's; the pipelined arm splits

### the same work per the sections above)

```rust
while remaining > 0 {
    let chunk64 = remaining.min(BUF_SIZE as u64);
    let this_chunk = usize::try_from(chunk64)?;
    let dev_chunk = dev_buf.as_mut_slice().get_mut(..this_chunk)?;
    guard.file().read_exact_at(dev_chunk, offset)?;
    let img_chunk = img_buf.get_mut(..this_chunk)?;
    fill_exact(&mut img, img_chunk)?;
    if dev_chunk != img_chunk {
        // find first differing byte for the error message
        bail!("verification mismatch at byte offset {abs}. ...");
    }
    remaining -= chunk64;
    offset += chunk64;
}
```

Two buffers:

- `dev_buf` — a fresh `AlignedBuf` (Phase 4's was dropped when the
  flash returned; the alignment is unnecessary for page-cache reads,
  but reusing the same buffer type keeps the two loops symmetric).
- `img_buf` — a fresh `Vec<u8>` for decompressor output. Kept separate
  so a corrupt comparison can never accidentally compare the buffer
  to itself.

We stop exactly at `bytes_written` (the count Phase 4 returned). Reading
beyond that would hit the trailing zeros from Phase 3's tail wipe and
falsely indict a correct flash.

### Decompressors are single-use

The image-side reader is rebuilt fresh:

```rust
let mut img = ImageReader::open(image_path, comp)?;
```

`flate2::GzDecoder`, `xz2::XzDecoder`, etc. don't implement `Seek`. We
can't rewind the Phase 4 reader; we open the file again and start a new
decompression pass from offset 0.

### Throttling and cancellation

Same pattern as Phase 4: per-chunk start time, sleep the residual at
the end if we beat the throttle target; check the cancel flag at the
top of each iteration. Verification at the throttled rate matches the
flash rate, so total wall time is roughly 2× the flash time (write +
read pass).

The throttle sleep uses `flash::cancellable_sleep`, the same helper
Phase 4 uses, so Ctrl+C remains responsive (~100ms latency) at any
throttle rate.

### Progress UI

Identical template to Phase 4 (raw image case):

```
[==================>                     ]  47% 476.84 MiB / 1.00 GiB (1.35 GiB/s)
```

Five fixed components: bar, percent (right-aligned to 3 columns),
`{bytes}` read so far, `{total_bytes}` (= `bytes_written` from Phase 4),
and the current read rate. The semantic shift from "writing" to
"reading-back" is implied by the surrounding `Verifying data
integrity...` headline; the bar layout itself is identical so the
operator's eye doesn't have to recalibrate.

`pb.reset_elapsed()` is called before the loop, same reason as Phase 4
— to exclude `BLKFLSBUF` and decompressor-init time from the rate
calculation. See `00-cli-and-ux.md` for the unified-template rationale.

### Mismatch reporting

On comparison failure, we find the first differing byte within the
chunk and report the absolute device offset:

```
verification mismatch at byte offset 1234567. The device may be
faulty, failing, or counterfeit. Image: /path/to/image.iso
```

A counterfeit USB stick (advertised 64 GB, actually 8 GB with wrap-around
firmware) hits this every time — read-back from the wrap-around region
returns either zeros or stale data, both of which mismatch the image.
This is why `--skip-verification` is opt-in, not default.

## Dispatch and shared helpers (threading plan, phase 5b)

`verify::verify` is a thin runtime dispatcher on
`comp.is_compressed()`, with the once-only setup — `BLKFLSBUF`,
`set_direct(false)`, reopening the image — hoisted into it for both
arms. Raw images take **`verify_serial`** (the pre-threading loop,
verbatim); compressed images take **`verify_pipelined`** (below).
Both arms compare through **`compare_chunk`** — pure, no I/O: byte
equality plus the first-diff absolute-offset diagnostic live there,
fixed once — and finish through **`verify_finalize`** (bar teardown +
newline; no `fdatasync`, verify is read-only). The mismatch message
is byte-identical across arms, pinned by
`verify_arms_report_identical_mismatch`.

## Pipelined verify arm

One worker thread owns the reopened `ImageReader` by-move and paces
itself by `remaining` (from `bytes_written`), filling image-side
`AlignedBuf`s from a two-buffer pool; the main thread keeps its own
`dev_buf` — which never crosses threads — and issues each device
`read_exact_at` BEFORE receiving the matching image chunk, so the USB
read overlaps the decompression (this is where the wall-clock win
lives). Cancel mirror, unbounded-`mpsc`/pool-backpressure, the
two-drop cleanup with `drop(free_tx)` as the join-deadlock preventer,
and `resume_unwind`-after-cleanup (so an armed guard prints "being
verified") all mirror the flash arm. Two fail-loud protocol checks
harden past the plan's sketch: a worker chunk whose length differs
from the independently derived `min(remaining, BUF_SIZE)`, or a clean
worker disconnect while `remaining > 0`, is an error — a short verify
must never pass silently. Pinned by the `verify_worker_*` and
`pipelined_verify_*` tests.

## Why `--skip-verification` (and `--skip-cooldown`) exist

Real-world reasons operators ask for it:

1. **Mass production.** Flashing 50 known-good USB sticks for an event
   — verification doubles the time for a defect class that was already
   ruled out in QA.
2. **Verified-elsewhere workflows.** A pipeline that hashes the device
   externally afterward (against the image's SHA256) doesn't need our
   byte-by-byte read-back.
3. **Failing devices.** A flaky stick may fail verification due to a
   read brownout that wouldn't actually affect boot. Skipping verify
   lets the operator try anyway.

In all three cases, the **cooldown still runs** because skipping it
silently corrupts the device.

## Manual test

```sh
# Normal verify path:
sudo ./target/release/imi -i debian-live.iso -d /dev/sdb -y
# Should display "Cooldown and FTL sync... done", then "Verifying...",
# then succeed.

# --skip-verification path:
sudo ./target/release/imi -i debian-live.iso -d /dev/sdb -y -n
# Should display "Cooldown and FTL sync... done", then
# "Skipping verification (--skip-verification).", then proceed.

# Counterfeit-stick simulation: flash an image larger than the stick's
# real capacity (set up loopback or use a known fake), expect a verify
# mismatch.
```
