//! Phase 5 — post-flash hardware cooldown and byte-for-byte verification.
//!
//! The module exposes two public entry points that the orchestrator calls
//! separately:
//!
//! 1. [`cooldown`] — the 10-second wait that lets cheap USB-NAND bridge
//!    controllers drain their DRAM write cache to flash and finish FTL
//!    housekeeping (TLC/QLC garbage collection, L2P table updates). This
//!    runs on every flash unless the operator passed `--skip-cooldown`
//!    (intended for loop devices, tests, and controllers that honor cache
//!    flushes — cli.rs carries the risk statement). Cutting it on a cheap
//!    bridge risks a device that reports "ready" over the bus while still
//!    staging metadata, leading to silent corruption on unplug.
//!
//! 2. [`verify`] — the byte-for-byte readback compare. Runs while the
//!    `O_EXCL` lock is still held, so that `udisks2` / GNOME / KDE cannot
//!    mount the new filesystem between write and readback and mutate
//!    on-disk bytes (mount-time superblock fields, journal replay, trash
//!    folder creation).
//!
//! Why 10 seconds? `sync`/`fdatasync` only guarantee the kernel has handed
//! bytes to the device. USB mass-storage bridges typically do not translate
//! `SYNCHRONIZE CACHE` (SCSI opcode 0x35) meaningfully, so the ioctl
//! returns success without flushing anything. A wall-clock wait is the
//! most reliable path we have; 10 s matches the bash original and clears
//! the controller drain for every device we have tested.

use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use indicatif::ProgressBar;

use crate::aligned::{AlignedBuf, BUF_SIZE};
use crate::flash::{cancellable_sleep, set_direct};
use crate::guard::FlashGuard;
use crate::image::{Compression, ImageReader};
use crate::ioctl;
use crate::progress::make_verify_pb;

/// Hardware cooldown. Sleeps `seconds` seconds while displaying an in-place
/// countdown. Checks the cancel flag every second so Ctrl+C is responsive.
///
/// Runs unless `--skip-cooldown` — an independent phase, not part of verify.
pub(crate) fn cooldown(seconds: u64, cancel: &AtomicBool) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    for remaining in (1..=seconds).rev() {
        if cancel.load(Ordering::SeqCst) {
            // Best-effort newline: we are bailing with "cancelled" either way.
            drop(writeln!(stdout));
            bail!("cancelled by user during cooldown");
        }
        // Trailing spaces overwrite any residual from a longer previous value
        // (e.g. "10s" → " 9s" without them would leave a stray 's').
        write!(stdout, "\rCooldown and FTL sync... ({remaining}s)   ")
            .context("writing cooldown status to stdout")?;
        // Best-effort flush: a failed flush only delays the countdown echo.
        drop(stdout.flush());
        thread::sleep(Duration::from_secs(1));
    }
    // Overwrite the final countdown with a terminal "done" state and newline.
    writeln!(stdout, "\rCooldown and FTL sync... done       ")
        .context("writing cooldown completion to stdout")?;
    Ok(())
}

/// Byte-for-byte verification. Assumes [`cooldown`] has already run.
///
/// A thin runtime dispatcher (threading plan, phase 5b): the once-only
/// setup — buffer-cache invalidation, `O_DIRECT` off, reopening the
/// image — happens here for both arms; then raw images take
/// [`verify_serial`] (the pre-threading loop) and compressed images
/// take [`verify_pipelined`], which overlaps decompression with the
/// device read on a worker thread. The public contract (signature,
/// mismatch diagnostic, error chains, progress output) is unchanged.
pub(crate) fn verify(
    guard: &mut FlashGuard,
    image_path: &Path,
    comp: Compression,
    bytes_written: u64,
    throttle: Option<u64>,
    cancel: &AtomicBool,
) -> Result<()> {
    // Invalidate the kernel's buffer cache for the device so subsequent
    // reads come from NAND, not from cached write-side pages.
    //
    // SAFETY: `guard` owns a valid, currently-open file descriptor for the
    // block device. `BLKFLSBUF` takes no argument and has no side effects
    // on memory allocated to this process.
    unsafe {
        ioctl::blkflsbuf(guard.as_raw_fd()).context("BLKFLSBUF (flush kernel buffer cache)")?;
    }

    // Ensure O_DIRECT is off — verification reads use the page cache so we
    // don't have to align the trailing-chunk read to a sector boundary.
    // Both arms rely on this; neither toggles it afterwards.
    set_direct(guard.as_raw_fd(), false).context("disabling O_DIRECT for verify")?;

    // Fresh reader — decompressors cannot be rewound.
    let img = ImageReader::open(image_path, comp).context("reopening image for verify")?;

    if comp.is_compressed() {
        verify_pipelined(guard, img, image_path, bytes_written, throttle, cancel)
    } else {
        verify_serial(guard, img, image_path, bytes_written, throttle, cancel)
    }
}

/// Serial verify arm — routes raw images (per the dispatcher). This is
/// the pre-threading `verify::verify` loop verbatim; comparison lives
/// in [`compare_chunk`] and post-loop work in [`verify_finalize`].
#[expect(
    clippy::arithmetic_side_effects,
    reason = "loop invariants: chunk64 = remaining.min(BUF_SIZE) so \
              remaining - chunk64 cannot underflow; offset + chunk64 is \
              bounded by bytes_written (a real device size); rate_bps >= 1 \
              is enforced by cli::parse_rate, so the throttle division \
              cannot divide by zero"
)]
fn verify_serial(
    guard: &mut FlashGuard,
    mut img: ImageReader,
    image_path: &Path,
    bytes_written: u64,
    throttle: Option<u64>,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut dev_buf = AlignedBuf::new()?;
    // try_reserve_exact + resize: OOM unwinds (guard FATAL fires) rather
    // than aborting past Drop via handle_alloc_error.
    let mut img_buf = Vec::new();
    img_buf
        .try_reserve_exact(BUF_SIZE)
        .context("allocating the 4 MiB verify image buffer (out of memory)")?;
    img_buf.resize(BUF_SIZE, 0_u8);

    let pb = make_verify_pb(bytes_written);
    pb.reset_elapsed();

    let mut remaining: u64 = bytes_written;
    let mut offset: u64 = 0;

    let chunk_target_nanos = throttle.map(|rate_bps| {
        // rate_bps >= 1 is enforced by cli::parse_rate; checked_div keeps
        // that invariant local instead of relying on it at a distance.
        let ideal = (BUF_SIZE as u128)
            .saturating_mul(1_000_000_000)
            .checked_div(u128::from(rate_bps))
            .unwrap_or(u128::MAX);
        u64::try_from(ideal).unwrap_or(u64::MAX)
    });

    while remaining > 0 {
        if cancel.load(Ordering::SeqCst) {
            pb.abandon();
            bail!("cancelled by user");
        }

        let start = chunk_target_nanos.map(|_| Instant::now());

        let chunk64 = remaining.min(BUF_SIZE as u64);
        let this_chunk = usize::try_from(chunk64)
            .context("verify chunk exceeds usize (impossible: bounded by BUF_SIZE)")?;

        let dev_chunk = dev_buf
            .as_mut_slice()
            .get_mut(..this_chunk)
            .context("device buffer smaller than verify chunk")?;
        guard.file().read_exact_at(dev_chunk, offset).with_context(|| {
            format!("reading {this_chunk} bytes from device at offset {offset}")
        })?;

        let img_chunk =
            img_buf.get_mut(..this_chunk).context("image buffer smaller than verify chunk")?;
        fill_exact(&mut img, img_chunk)
            .with_context(|| format!("reading {this_chunk} bytes from image stream"))?;

        // Shared comparison invariant (threading plan, phase 5b);
        // abandon the bar at the call site so its last rendered state
        // stays on screen as the mismatch record, exactly as before.
        compare_chunk(dev_chunk, img_chunk, offset, image_path).inspect_err(|_| pb.abandon())?;

        remaining -= chunk64;
        offset += chunk64;
        pb.set_position(offset);

        if let (Some(target_ns), Some(t0)) = (chunk_target_nanos, start) {
            let elapsed_ns = u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX);
            if let Some(residual) = target_ns.checked_sub(elapsed_ns) {
                cancellable_sleep(Duration::from_nanos(residual), cancel);
            }
        }
    }

    verify_finalize(&pb);
    Ok(())
}

/// Items the verify worker sends to the reader thread: a filled
/// image-side buffer plus its valid byte count, or a fatal worker-side
/// error (a `fill_exact` failure carrying the serial arm's exact
/// context string, so error chains are arm-independent).
type FilledItem = Result<(AlignedBuf, usize)>;

/// Decompression worker for the pipelined verify arm. Owns the reader
/// by-move and paces itself by `remaining` (initialized from
/// `bytes_written` at spawn): each iteration fills exactly
/// `min(remaining, BUF_SIZE)` bytes — the same chunk sequence the main
/// thread derives independently — and exits after producing the last
/// chunk. Never sees the device FD or the guard. Generic over the
/// reader so tests can drive erroring/panicking streams; production
/// monomorphizes with [`ImageReader`].
#[expect(
    clippy::arithmetic_side_effects,
    reason = "remaining -= chunk cannot underflow: chunk = \
              remaining.min(BUF_SIZE), the same invariant the serial \
              arm's expect documents"
)]
fn verify_worker_loop<R: Read>(
    mut reader: R,
    bytes_written: u64,
    worker_cancel: &Arc<AtomicBool>,
    filled_tx: &mpsc::Sender<FilledItem>,
    free_rx: &mpsc::Receiver<AlignedBuf>,
) {
    let mut remaining = bytes_written;
    loop {
        // 1. Done: the last chunk has been produced and sent.
        if remaining == 0 {
            return;
        }

        // 2. Acquire an image-side buffer from the pool.
        let Ok(mut buf) = free_rx.recv() else {
            return; // main shut down (dropped free_tx)
        };

        // 3. Cancel mirror — skip a wasted decompress on shutdown.
        if worker_cancel.load(Ordering::SeqCst) {
            return;
        }

        // 4. Fill exactly this chunk from the decompressor.
        let chunk64 = remaining.min(BUF_SIZE as u64);
        let Ok(chunk) = usize::try_from(chunk64) else {
            // Unreachable (bounded by BUF_SIZE); fail loud if broken.
            // Send-failure means main is already gone; either way, exit.
            drop(filled_tx.send(Err(anyhow!("verify chunk exceeds usize"))));
            return;
        };
        let Some(dst) = buf.as_mut_slice().get_mut(..chunk) else {
            // Send-failure means main is already gone; either way, exit.
            drop(filled_tx.send(Err(anyhow!("image buffer smaller than verify chunk"))));
            return;
        };
        if let Err(e) = fill_exact(&mut reader, dst) {
            let wrapped =
                anyhow::Error::from(e).context(format!("reading {chunk} bytes from image stream"));
            if filled_tx.send(Err(wrapped)).is_err() { /* main exited first */ }
            return;
        }

        // 5. Hand it to the reader thread.
        if filled_tx.send(Ok((buf, chunk))).is_err() {
            return; // main dropped filled_rx (error/cancel shutdown)
        }
        remaining -= chunk64;
    }
}

/// Pipelined verify arm — routes compressed images (per the
/// dispatcher). The worker decompresses the image into a two-buffer
/// pool while this (main) thread reads the device into its own
/// `dev_buf` — which never crosses threads — then compares via the
/// shared [`compare_chunk`]. Device reads are issued BEFORE receiving
/// the image chunk, so the USB read overlaps the decompression. Only
/// this thread ever touches the FD. Loop discipline mirrors
/// `flash_pipelined`: no `return` inside `'verify_loop`; every exit
/// breaks to one cleanup block (drop both main-side channel halves —
/// `drop(free_tx)` is the join-deadlock preventer, `mpsc` sends never
/// block — join, then `resume_unwind` any captured worker panic so an
/// armed guard's "being verified" FATAL fires with channels closed).
/// Fail-loud protocol checks (hardened past the plan's sketch): a
/// worker chunk whose length differs from the independently derived
/// expectation, or a clean worker disconnect while `remaining > 0`,
/// is an error — never a silent short verify.
#[expect(
    clippy::arithmetic_side_effects,
    reason = "loop invariants: chunk64 = remaining.min(BUF_SIZE) so \
              remaining - chunk64 cannot underflow; offset + chunk64 is \
              bounded by bytes_written (a real device size); rate_bps >= 1 \
              is enforced by cli::parse_rate, so the throttle division \
              cannot divide by zero"
)]
fn verify_pipelined<R: Read + Send + 'static>(
    guard: &mut FlashGuard,
    img: R,
    image_path: &Path,
    bytes_written: u64,
    throttle: Option<u64>,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut dev_buf = AlignedBuf::new()?;

    let pb = make_verify_pb(bytes_written);
    pb.reset_elapsed();

    let chunk_target_nanos = throttle.map(|rate_bps| {
        // rate_bps >= 1 is enforced by cli::parse_rate; checked_div keeps
        // that invariant local instead of relying on it at a distance.
        let ideal = (BUF_SIZE as u128)
            .saturating_mul(1_000_000_000)
            .checked_div(u128::from(rate_bps))
            .unwrap_or(u128::MAX);
        u64::try_from(ideal).unwrap_or(u64::MAX)
    });

    // Cancel mirror (the worker cannot borrow the non-'static parent flag).
    let local_cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&local_cancel);

    let (filled_tx, filled_rx) = mpsc::channel::<FilledItem>();
    let (free_tx, free_rx) = mpsc::channel::<AlignedBuf>();
    // Seed the image-side two-buffer pool before spawning (the pool, not
    // the unbounded channel, bounds pipeline depth).
    for _ in 0..2_u8 {
        if free_tx.send(AlignedBuf::new()?).is_err() {
            bail!("verify buffer-pool receiver closed before worker spawn (unreachable)");
        }
    }

    let mut worker_handle: Option<thread::JoinHandle<()>> = Some(thread::spawn(move || {
        verify_worker_loop(img, bytes_written, &worker_cancel, &filled_tx, &free_rx);
    }));
    let mut worker_panic: Option<Box<dyn std::any::Any + Send>> = None;

    let mut remaining: u64 = bytes_written;
    let mut offset: u64 = 0;
    let mut outcome: Result<()> = Ok(());

    'verify_loop: loop {
        // 0. Success: every byte compared.
        if remaining == 0 {
            break 'verify_loop;
        }

        // 1. Cancel check (parent flag), mirrored to the worker.
        if cancel.load(Ordering::SeqCst) {
            local_cancel.store(true, Ordering::SeqCst);
            outcome = Err(anyhow!("cancelled by user"));
            break 'verify_loop;
        }

        let start = chunk_target_nanos.map(|_| Instant::now());
        let chunk64 = remaining.min(BUF_SIZE as u64);
        let Ok(this_chunk) = usize::try_from(chunk64) else {
            outcome = Err(anyhow!("verify chunk exceeds usize (impossible: bounded by BUF_SIZE)"));
            break 'verify_loop;
        };

        // 2. Read the device chunk FIRST — this is the overlap: the USB
        //    read proceeds while the worker decompresses the matching
        //    image chunk.
        let Some(dev_chunk) = dev_buf.as_mut_slice().get_mut(..this_chunk) else {
            outcome = Err(anyhow!("device buffer smaller than verify chunk"));
            break 'verify_loop;
        };
        if let Err(e) = guard
            .file()
            .read_exact_at(dev_chunk, offset)
            .with_context(|| format!("reading {this_chunk} bytes from device at offset {offset}"))
        {
            outcome = Err(e);
            break 'verify_loop;
        }

        // 3. Receive the matching image chunk.
        let (img_buf, n) = match filled_rx.recv() {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                outcome = Err(e);
                break 'verify_loop;
            }
            Err(_) => {
                // Disconnected. A panic is captured for post-cleanup
                // re-raise; a CLEAN exit while remaining > 0 is a
                // protocol violation (the worker paces itself by the
                // same bytes_written) and must fail loud, never pass
                // as a short-but-successful verify.
                match worker_handle.take().map(thread::JoinHandle::join) {
                    Some(Err(panic)) => worker_panic = Some(panic),
                    Some(Ok(())) | None => {
                        outcome = Err(anyhow!(
                            "image worker exited {remaining} bytes before the \
                             end of verification (pipeline protocol violation)"
                        ));
                    }
                }
                break 'verify_loop;
            }
        };

        // 4. Protocol check: both sides derive the chunk length from the
        //    same bytes_written sequence; a divergence means a bug, and
        //    comparing mismatched lengths would mis-diagnose it as a
        //    device fault.
        if n != this_chunk {
            outcome = Err(anyhow!(
                "image worker produced a {n}-byte chunk where {this_chunk} \
                 bytes were expected (pipeline protocol violation)"
            ));
            break 'verify_loop;
        }

        // 5. Compare (shared helper), then return the buffer to the pool.
        let Some(img_chunk) = img_buf.as_slice().get(..n) else {
            outcome = Err(anyhow!("image buffer smaller than verify chunk"));
            break 'verify_loop;
        };
        if let Err(e) = compare_chunk(dev_chunk, img_chunk, offset, image_path) {
            outcome = Err(e);
            break 'verify_loop;
        }
        if free_tx.send(img_buf).is_err() { /* worker done (last chunk) */ }

        remaining -= chunk64;
        offset += chunk64;
        pb.set_position(offset);

        // 6. Throttle, then re-check cancel (blocking in the next recv on
        //    a chunk a mirrored-cancelled worker will never send would
        //    hang).
        if let (Some(target_ns), Some(t0)) = (chunk_target_nanos, start) {
            let elapsed_ns = u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX);
            if let Some(residual) = target_ns.checked_sub(elapsed_ns) {
                cancellable_sleep(Duration::from_nanos(residual), cancel);
                if cancel.load(Ordering::SeqCst) {
                    local_cancel.store(true, Ordering::SeqCst);
                    outcome = Err(anyhow!("cancelled by user"));
                    break 'verify_loop;
                }
            }
        }
    }

    // Single cleanup block — every 'verify_loop exit lands here. Same
    // rationale as flash_pipelined: drop(filled_rx) for prompt worker
    // exit and to free a parked buffer; drop(free_tx) as the actual
    // unblocker for a pool-parked worker (MUST precede the join).
    drop(filled_rx);
    drop(free_tx);
    if let Some(handle) = worker_handle.take()
        && let Err(panic) = handle.join()
    {
        worker_panic = Some(panic);
    }
    if let Some(panic) = worker_panic {
        // Channels closed, worker joined; unwinding now runs
        // FlashGuard::drop with the correct "being verified" FATAL.
        std::panic::resume_unwind(panic);
    }

    if let Err(e) = outcome {
        pb.abandon();
        return Err(e);
    }

    verify_finalize(&pb);
    Ok(())
}

/// Compare one chunk of device bytes against image bytes; on mismatch,
/// produce the operator diagnostic naming the absolute offset of the
/// FIRST differing byte.
///
/// This is the verify-loop invariant shared by both verify arms
/// (threading plan, phase 5b): comparison semantics and the mismatch
/// message live here, fixed once. Pure — no I/O, no FD, no progress
/// bar; the caller reads both buffers and handles `pb.abandon()`.
/// Slice lengths are a caller precondition (both derive from the same
/// `min(remaining, BUF_SIZE)`); Rust slice equality compares lengths
/// first, so a caller bug surfaces as a mismatch, never as UB.
#[expect(
    clippy::arithmetic_side_effects,
    reason = "offset + first_diff is bounded by bytes_written (a real \
              device size), the same bound the serial loop's expect \
              documents"
)]
fn compare_chunk(dev_bytes: &[u8], img_bytes: &[u8], offset: u64, image_path: &Path) -> Result<()> {
    if dev_bytes == img_bytes {
        return Ok(());
    }
    let first_diff = dev_bytes.iter().zip(img_bytes.iter()).position(|(a, b)| a != b).unwrap_or(0);
    let abs = offset + first_diff as u64;
    bail!(
        "verification mismatch at byte offset {abs}. The device may \
         be faulty, failing, or counterfeit. Image: {}",
        image_path.display()
    );
}

/// Shared post-loop work for the verify arms: finish the bar and emit
/// the clean newline. No `fdatasync` (verify is read-only) and no
/// `O_DIRECT` toggle (the dispatcher cleared it once at entry).
fn verify_finalize(pb: &ProgressBar) {
    pb.finish_and_clear();
    println!();
}

/// Read exactly `dst.len()` bytes from `r`, retrying on short reads and
/// `Interrupted` errors. Returns `Err` on early EOF.
#[expect(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "loop invariant: filled < dst.len() at the index site, and \
              n <= dst.len() - filled per the Read contract, so filled + n \
              cannot overflow dst.len(), let alone usize"
)]
fn fill_exact<R: Read>(r: &mut R, dst: &mut [u8]) -> std::io::Result<()> {
    let mut filled = 0;
    while filled < dst.len() {
        match r.read(&mut dst[filled..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "image stream ended before expected verification length",
                ));
            }
            Ok(n) => filled += n,
            // Retry: EINTR is transparent to the caller.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, ErrorKind};

    /// Happy path: reader has at least `dst.len()` bytes; we read exactly
    /// that many.
    #[test]
    fn fill_exact_succeeds_when_data_abundant() {
        let data: Vec<u8> = (0..200).cycle().take(1000).collect();
        let mut r = Cursor::new(data.clone());
        let mut dst = vec![0_u8; 256];
        fill_exact(&mut r, &mut dst).unwrap();
        assert_eq!(&*dst, &data[..256]);
    }

    /// Early EOF: dst is bigger than the reader's content. `fill_exact`
    /// MUST return Err(UnexpectedEof). Verify-phase relies on this:
    /// "image stream ended before we expected to compare bytes" is a
    /// genuine integrity failure (the image got truncated, or the
    /// caller passed the wrong byte count).
    #[test]
    fn fill_exact_errors_on_early_eof() {
        let mut r = Cursor::new(vec![0_u8; 100]);
        let mut dst = vec![0_u8; 200];
        let err = fill_exact(&mut r, &mut dst).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
    }

    /// Empty reader against any non-empty dst is the simplest EOF case
    /// — must error, not silently succeed with zeroed output.
    #[test]
    fn fill_exact_errors_on_empty_reader() {
        let mut r = Cursor::new(Vec::<u8>::new());
        let mut dst = vec![0_u8; 1];
        let err = fill_exact(&mut r, &mut dst).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
    }

    /// Empty dst is a no-op — succeeds without touching the reader.
    /// Avoids spurious "EOF" on a zero-length verify (defensive against
    /// `bytes_written` = 0).
    #[test]
    fn fill_exact_zero_length_dst_is_noop() {
        let mut r = Cursor::new(vec![1_u8, 2, 3]);
        let mut dst: [u8; 0] = [];
        fill_exact(&mut r, &mut dst).unwrap();
    }

    /// Short reads must be re-driven. A reader that returns 1 byte at a
    /// time is a stress-test for the loop; the verify path exercises
    /// real decompressors which can do this for real.
    #[test]
    fn fill_exact_redrives_short_reads() {
        struct OneByteAtATime<'a> {
            data: &'a [u8],
            pos: usize,
        }
        impl Read for OneByteAtATime<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.pos >= self.data.len() || buf.is_empty() {
                    return Ok(0);
                }
                buf[0] = self.data[self.pos];
                self.pos += 1;
                Ok(1)
            }
        }
        let data: Vec<u8> = (0..200).collect();
        let mut r = OneByteAtATime { data: &data, pos: 0 };
        let mut dst = vec![0_u8; 100];
        fill_exact(&mut r, &mut dst).unwrap();
        assert_eq!(&*dst, &data[..100]);
    }

    /// `Interrupted` errors must be retried, not propagated.
    #[test]
    fn fill_exact_retries_on_interrupted() {
        struct InterruptThenSucceed {
            interrupts_left: u32,
            data: Vec<u8>,
            pos: usize,
        }
        impl Read for InterruptThenSucceed {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.interrupts_left > 0 {
                    self.interrupts_left -= 1;
                    return Err(std::io::Error::from(ErrorKind::Interrupted));
                }
                let avail = self.data.len() - self.pos;
                let n = avail.min(buf.len());
                buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
                self.pos += n;
                Ok(n)
            }
        }
        let mut r = InterruptThenSucceed { interrupts_left: 3, data: vec![0xAB; 64], pos: 0 };
        let mut dst = vec![0_u8; 32];
        fill_exact(&mut r, &mut dst).unwrap();
        assert!(dst.iter().all(|&b| b == 0xAB));
    }

    /// Non-Interrupted IO errors propagate. Verify-phase must surface
    /// real read failures (the device returning EIO mid-verify is a
    /// genuine fault and the operator needs the error).
    #[test]
    fn fill_exact_propagates_other_errors() {
        struct FailingReader;
        impl Read for FailingReader {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("synthetic IO failure"))
            }
        }
        let mut r = FailingReader;
        let mut dst = vec![0_u8; 100];
        let err = fill_exact(&mut r, &mut dst).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Other);
    }
    // ------------------------------------------------------------------
    // compare_chunk — 5b Step 1 of the threading plan.
    // ------------------------------------------------------------------

    /// Equal slices (including empty) compare clean.
    #[test]
    fn compare_chunk_accepts_equal_slices() {
        let p = Path::new("/tmp/img.gz");
        compare_chunk(&[1, 2, 3], &[1, 2, 3], 0, p).unwrap();
        compare_chunk(&[], &[], 4096, p).unwrap();
    }

    /// The diagnostic names the ABSOLUTE offset of the first differing
    /// byte (chunk offset + index) and carries the image path.
    #[test]
    fn compare_chunk_reports_first_diff_absolute_offset() {
        let mut dev = vec![0xAA_u8; 64];
        dev[5] = 0x00;
        dev[9] = 0x00; // later diff must NOT win
        let img = vec![0xAA_u8; 64];
        let err = compare_chunk(&dev, &img, 4096, Path::new("/tmp/x.img")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mismatch at byte offset 4101"), "{msg}");
        assert!(msg.contains("/tmp/x.img"), "{msg}");

        // Last-position diff: the scan must reach the slice's end.
        let mut dev2 = vec![0_u8; 64];
        dev2[63] = 1;
        let e2 = compare_chunk(&dev2, &[0_u8; 64], 0, Path::new("/i")).unwrap_err();
        assert!(e2.to_string().contains("offset 63"), "{e2}");

        // Multi-GiB base offset: abs = offset + idx with no overflow or
        // truncation in the reported number.
        let mut dev3 = vec![0_u8; 256];
        dev3[100] = 1;
        let e3 = compare_chunk(&dev3, &[0_u8; 256], 0x4000_0000, Path::new("/i")).unwrap_err();
        assert!(e3.to_string().contains("offset 1073741924"), "{e3}");
    }

    /// A diff in the very first byte reports the chunk offset itself.
    #[test]
    fn compare_chunk_reports_diff_at_chunk_start() {
        let err = compare_chunk(&[0_u8], &[1_u8], 777, Path::new("/i")).unwrap_err();
        assert!(err.to_string().contains("offset 777"), "{err}");
    }
    // ------------------------------------------------------------------
    // Pipelined verify arm — 5b Step 2 protocol tests (re-derived).
    // ------------------------------------------------------------------

    /// Real guard over a fresh tempfile whose contents ARE the device.
    fn tempfile_guard(tag: &str, contents: &[u8]) -> (FlashGuard, std::path::PathBuf) {
        let p = std::env::temp_dir().join(format!("imi-vfy-{tag}-{}", std::process::id()));
        std::fs::write(&p, contents).unwrap();
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&p).unwrap();
        (FlashGuard::new(f, p.clone()), p)
    }

    /// Gzip `payload` to a temp file and open it as an `ImageReader`.
    fn gzip_reader(tag: &str, payload: &[u8]) -> (ImageReader, std::path::PathBuf) {
        use std::io::Write as _;
        let p = std::env::temp_dir().join(format!("imi-vfy-{tag}-{}.gz", std::process::id()));
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(payload).unwrap();
        std::fs::write(&p, enc.finish().unwrap()).unwrap();
        (ImageReader::open(&p, Compression::Gzip).unwrap(), p)
    }

    /// The worker produces exactly the `min(remaining, BUF_SIZE)` chunk
    /// sequence derived from `bytes_written`, then exits.
    #[test]
    fn verify_worker_paces_chunks_by_bytes_written() {
        let total = BUF_SIZE as u64 + 137;
        let payload = vec![0x6D_u8; BUF_SIZE + 137];
        let (reader, img_p) = gzip_reader("pace", &payload);

        let (filled_tx, filled_rx) = mpsc::channel::<FilledItem>();
        let (free_tx, free_rx) = mpsc::channel::<AlignedBuf>();
        for _ in 0..2_u8 {
            free_tx.send(AlignedBuf::new().unwrap()).unwrap();
        }
        let mirror = Arc::new(AtomicBool::new(false));
        let m = Arc::clone(&mirror);
        let h = thread::spawn(move || verify_worker_loop(reader, total, &m, &filled_tx, &free_rx));

        let (_b1, n1) = filled_rx.recv().unwrap().unwrap();
        assert_eq!(n1, BUF_SIZE);
        let (_b2, n2) = filled_rx.recv().unwrap().unwrap();
        assert_eq!(n2, 137);
        assert!(filled_rx.recv().is_err(), "worker must exit after the last chunk");
        h.join().unwrap();
        std::fs::remove_file(&img_p).unwrap();
    }

    /// Happy path end to end: device contents equal the decompressed
    /// image (one full chunk + tail) — pipelined verify returns Ok.
    #[test]
    #[cfg_attr(miri, ignore)] // MIRI ICE
    fn pipelined_verify_accepts_matching_device() {
        let mut payload = vec![0x2E_u8; BUF_SIZE];
        payload.extend_from_slice(&[0xE2; 137]);
        let (mut guard, dev_p) = tempfile_guard("ok", &payload);
        let (reader, img_p) = gzip_reader("ok", &payload);
        let cancel = AtomicBool::new(false);
        verify_pipelined(&mut guard, reader, &img_p, payload.len() as u64, None, &cancel).unwrap();
        std::fs::remove_file(&dev_p).unwrap();
        std::fs::remove_file(&img_p).unwrap();
    }

    /// A single corrupted device byte in the SECOND chunk is reported at
    /// its absolute offset by the pipelined arm.
    #[test]
    #[cfg_attr(miri, ignore)] // MIRI ICE
    fn pipelined_verify_reports_mismatch_offset() {
        let mut payload = vec![0x55_u8; BUF_SIZE + 4096];
        let (reader, img_p) = gzip_reader("mm", &payload);
        let corrupt_at = BUF_SIZE + 1000;
        payload[corrupt_at] ^= 0xFF;
        let (mut guard, dev_p) = tempfile_guard("mm", &payload);
        let cancel = AtomicBool::new(false);
        let err = verify_pipelined(&mut guard, reader, &img_p, payload.len() as u64, None, &cancel)
            .unwrap_err();
        assert!(
            err.to_string().contains(&format!("mismatch at byte offset {corrupt_at}")),
            "{err}"
        );
        std::fs::remove_file(&dev_p).unwrap();
        std::fs::remove_file(&img_p).unwrap();
    }

    /// Arm parity on the failure path: serial and pipelined report the
    /// SAME mismatch diagnostic for the same corrupted device.
    #[test]
    #[cfg_attr(miri, ignore)] // MIRI ICE
    fn verify_arms_report_identical_mismatch() {
        let mut payload = vec![0x77_u8; 2 * BUF_SIZE];
        let (reader_p, img_p) = gzip_reader("par", &payload);
        payload[123_456] ^= 0x01;
        let cancel = AtomicBool::new(false);

        let (mut g1, d1) = tempfile_guard("par-s", &payload);
        // Serial arm normally routes raw; drive it with the gzip reader
        // directly — the arm is reader-agnostic by construction.
        let reader_s = ImageReader::open(&img_p, Compression::Gzip).unwrap();
        let e_serial =
            verify_serial(&mut g1, reader_s, &img_p, payload.len() as u64, None, &cancel)
                .unwrap_err();

        let (mut g2, d2) = tempfile_guard("par-p", &payload);
        let e_pipe =
            verify_pipelined(&mut g2, reader_p, &img_p, payload.len() as u64, None, &cancel)
                .unwrap_err();

        assert_eq!(e_serial.to_string(), e_pipe.to_string(), "arms diverged");
        assert!(e_serial.to_string().contains("offset 123456"), "{e_serial}");
        for p in [d1, d2, img_p] {
            std::fs::remove_file(p).unwrap();
        }
    }

    /// A worker panic resumes on the main thread through the real
    /// disconnect-join-capture path; the guard would FATAL during this
    /// unwind if armed.
    struct PanickingReader;
    impl Read for PanickingReader {
        #[expect(
            clippy::panic_in_result_fn,
            reason = "panicking is this stub's entire purpose: it drives \
                      the verify worker-panic propagation test through \
                      the real resume_unwind path"
        )]
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            panic!("injected verify worker panic (test)");
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)] // MIRI ICE
    fn pipelined_verify_resumes_worker_panic_on_main_thread() {
        let (mut guard, dev_p) = tempfile_guard("panic", &vec![0_u8; 8192]);
        let cancel = AtomicBool::new(false);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            verify_pipelined(&mut guard, PanickingReader, Path::new("/img"), 8192, None, &cancel)
        }));
        assert!(caught.is_err(), "worker panic must resume on main");
        std::fs::remove_file(&dev_p).unwrap();
    }

    /// A truncated image stream surfaces the worker's fill error with
    /// the serial arm's context string.
    #[test]
    #[cfg_attr(miri, ignore)] // MIRI ICE
    fn pipelined_verify_propagates_worker_error_with_context() {
        let payload = vec![0x11_u8; 200 * 1024];
        let (_full_reader, img_p) = gzip_reader("werr", &payload);
        // Truncate the gzip mid-stream, then reopen.
        let full = std::fs::read(&img_p).unwrap();
        std::fs::write(&img_p, &full[..40]).unwrap();
        let reader = ImageReader::open(&img_p, Compression::Gzip).unwrap();

        let (mut guard, dev_p) = tempfile_guard("werr", &payload);
        let cancel = AtomicBool::new(false);
        let err = verify_pipelined(&mut guard, reader, &img_p, payload.len() as u64, None, &cancel)
            .unwrap_err();
        assert!(format!("{err:#}").contains("from image stream"), "{err:#}");
        std::fs::remove_file(&dev_p).unwrap();
        std::fs::remove_file(&img_p).unwrap();
    }
    /// `verify_finalize` finishes the bar (the newline is a println we
    /// trust rather than capture, per the plan's own allowance).
    #[test]
    #[cfg_attr(miri, ignore)] // MIRI ICE
    fn verify_finalize_finishes_bar() {
        let pb = ProgressBar::new(8);
        pb.set_position(8);
        verify_finalize(&pb);
        assert!(pb.is_finished());
    }

    /// V5 (verify flavor): a verify worker parked on the pool exits
    /// when main drops `free_tx` — even with bytes remaining.
    #[test]
    fn verify_worker_exits_when_pool_sender_drops() {
        let (filled_tx, _filled_rx) = mpsc::channel::<FilledItem>();
        let (free_tx, free_rx) = mpsc::channel::<AlignedBuf>();
        let mirror = Arc::new(AtomicBool::new(false));
        let m = Arc::clone(&mirror);
        let h = thread::spawn(move || {
            verify_worker_loop(std::io::repeat(0), 10 * BUF_SIZE as u64, &m, &filled_tx, &free_rx);
        });
        drop(free_tx); // no seed: worker is pool-parked; unblock it
        h.join().unwrap();
    }

    /// V6 (verify flavor): a pre-set mirror stops the worker after it
    /// takes a buffer, before any fill or send.
    #[test]
    fn verify_cancel_mirror_stops_worker_before_fill() {
        let (filled_tx, filled_rx) = mpsc::channel::<FilledItem>();
        let (free_tx, free_rx) = mpsc::channel::<AlignedBuf>();
        let mirror = Arc::new(AtomicBool::new(true));
        let m = Arc::clone(&mirror);
        free_tx.send(AlignedBuf::new().unwrap()).unwrap();
        let h = thread::spawn(move || {
            verify_worker_loop(std::io::repeat(0), BUF_SIZE as u64, &m, &filled_tx, &free_rx);
        });
        assert!(filled_rx.recv().is_err(), "worker must not send after cancel");
        h.join().unwrap();
    }

    /// V8: an image that ends BEFORE `bytes_written` is classified as a
    /// truncation (`fill_exact`'s `UnexpectedEof`, with the stream context)
    /// — never as a content mismatch.
    #[test]
    #[cfg_attr(miri, ignore)] // MIRI ICE
    fn pipelined_verify_classifies_truncation_not_mismatch() {
        let payload = vec![0x44_u8; 100 * 1024];
        let (reader, img_p) = gzip_reader("short", &payload);
        // Device claims twice the bytes the image actually decodes to.
        let claimed = 2 * payload.len();
        let (mut guard, dev_p) = tempfile_guard("short", &vec![0x44_u8; claimed]);
        let cancel = AtomicBool::new(false);
        let err = verify_pipelined(&mut guard, reader, &img_p, claimed as u64, None, &cancel)
            .unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("ended before expected"), "{chain}");
        assert!(!chain.contains("mismatch"), "must classify as truncation: {chain}");
        std::fs::remove_file(&dev_p).unwrap();
        std::fs::remove_file(&img_p).unwrap();
    }
}
