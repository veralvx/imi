//! Phase 4 — the flash write loop.
//!
//! Consumes bytes from an [`ImageReader`], writes them to the locked device FD
//! in 4 MiB page-aligned chunks via `O_DIRECT`, and returns the exact number of
//! bytes written (which equals the decompressed image size).
//!
//! Design notes:
//!
//! - `O_DIRECT` is toggled on the already-opened device FD via
//!   `fcntl(F_SETFL, flags | O_DIRECT)`. Unlike `O_SYNC` (which Linux will
//!   not let you add via `F_SETFL`), `O_DIRECT` is in the mutable set.
//!
//! - Decompressors return short reads unpredictably; we loop `Read::read`
//!   into the aligned buffer until it is full (or until EOF) before
//!   dispatching a single `pwrite` to the device. 4 MiB per syscall is
//!   the sweet spot — small enough to keep the kernel writeback queue
//!   happy, large enough to dwarf syscall overhead.
//!
//! - The tail chunk is almost never sector-aligned in length. For the
//!   tail we clear `O_DIRECT` and write through the page cache;
//!   `fdatasync` at the end makes it durable.
//!
//! - `ENOSPC` from a compressed input — where we have no pre-flight size —
//!   is caught and surfaced as a typed, actionable error.
//!
//! - Cancellation: each iteration inspects an `AtomicBool` set by the
//!   `ctrlc` handler; if true, we return `Err` promptly. The guard is
//!   still armed, so its `Drop` runs during unwind.
//!
//! - UX: the progress bar uses indicatif's built-in
//!   `{bytes_per_sec}` token, which internally feeds a double-smoothed
//!   exponentially weighted estimator. We pair that with
//!   `pb.reset_elapsed()` immediately before the first write so setup
//!   cost (decompressor init, buffer allocation, first decoder fill)
//!   does not contaminate the initial rate estimate. Together these
//!   eliminate the throttle "spike-then-decay" artefact that the
//!   default-untouched configuration shows. At the end we
//!   `finish_and_clear` the bar and emit a newline so the next phase's
//!   message starts clean.

use std::io::Read;
use std::os::unix::fs::FileExt;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use indicatif::ProgressBar;

use crate::aligned::{AlignedBuf, BUF_SIZE};
use crate::guard::FlashGuard;
use crate::image::{Compression, ImageReader};
use crate::progress::make_progress_bar;

/// Result of a completed write pass.
#[derive(Debug)]
pub(crate) struct FlashOutcome {
    /// Exact number of bytes written to the device. Used by Phase 5
    /// verification to know where to stop reading.
    pub(crate) bytes_written: u64,
}

/// Serial flash arm — routes raw images (per the dispatcher in
/// [`flash`]). This is the pre-threading `flash::flash` body verbatim,
/// moved here by Step 3 of the threading plan; its loop delegates
/// per-chunk work to [`process_chunk`] and post-loop work to
/// [`flash_finalize`] (Step 1's extraction).
fn flash_serial(
    guard: &mut FlashGuard,
    mut reader: ImageReader,
    comp: Compression,
    raw_size: Option<u64>,
    dev_size: u64,
    throttle: Option<u64>,
    cancel: &AtomicBool,
) -> Result<FlashOutcome> {
    let fd = guard.as_raw_fd();

    set_direct(fd, true).context("enabling O_DIRECT for flash phase")?;

    let pb = make_progress_bar(comp, raw_size);
    // Critical: reset the bar's elapsed timer immediately before the write
    // loop starts. Without this, bar construction + first-chunk fill time
    // count toward the rate calculation and produce a large initial spike
    // (especially visible with --throttle, where the first chunk writes
    // at unthrottled speed before any sleep has fired).
    pb.reset_elapsed();

    let mut buf = AlignedBuf::new()?;
    // Deliberately uninitialized: every path that reaches a read of
    // `total` first assigns it from a `ChunkOutcome` (the compiler's
    // definite-assignment analysis enforces this), and an `= 0`
    // initializer would be dead — flagged by `unused_assignments`.
    let mut total: u64;
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

    loop {
        if cancel.load(Ordering::SeqCst) {
            pb.abandon();
            bail!("cancelled by user");
        }

        let start = chunk_target_nanos.map(|_| Instant::now());

        let filled =
            fill_buffer(&mut reader, buf.as_mut_slice()).context("reading from image stream")?;

        // Loop-body invariants (capacity check, O_DIRECT-vs-tail
        // dispatch, accounting) live in the shared helper per the
        // threading plan; abandon the bar on any helper error so the
        // last rendered state stays visible as the abort record.
        match process_chunk(guard, &buf, filled, offset, dev_size) {
            Ok(ChunkOutcome::Continue { end }) => {
                offset = end;
                total = end;
            }
            Ok(ChunkOutcome::Done { end }) => {
                total = end;
                break;
            }
            Err(e) => {
                pb.abandon();
                return Err(e);
            }
        }

        pb.set_position(total);

        // Thermal / throttle mitigation — sleep the residual of the chunk's
        // ideal duration at the configured rate.
        //
        // The sleep is cancel-responsive: at very low rates (e.g.
        // `--throttle 100K`, ~40s/chunk), a naïve `thread::sleep` would
        // make Ctrl+C wait the full residual before noticing. We poll in
        // 100ms increments, which is well below any human-perceptible
        // unresponsiveness threshold and adds negligible cost (a single
        // atomic load per tick).
        if let (Some(target_ns), Some(t0)) = (chunk_target_nanos, start) {
            let elapsed_ns = u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX);
            if let Some(residual) = target_ns.checked_sub(elapsed_ns) {
                cancellable_sleep(Duration::from_nanos(residual), cancel);
            }
        }
    }

    pb.set_position(total);
    flash_finalize(guard, &pb)?;

    Ok(FlashOutcome { bytes_written: total })
}

/// Outcome of processing one filled buffer. Drives the caller's loop.
///
/// Divergence from the threading plan's sketch (which carried
/// `bytes_written` per chunk): each variant carries `end` — the checked
/// new total, which is also the next write offset for `Continue`. This
/// fits the post-plan capacity invariant: `chunk_end_within` is the one
/// place offset arithmetic happens, and both the bound and the advance
/// come from the same checked expression (see `process_chunk`).
#[derive(Debug)]
enum ChunkOutcome {
    /// A full `BUF_SIZE` chunk was written under `O_DIRECT`. The caller
    /// should set `offset = total = end` and continue looping.
    Continue {
        /// Checked new total = next write offset (`offset + BUF_SIZE`).
        end: u64,
    },
    /// The final write happened (tail, or empty input with
    /// `end == offset`). The caller must set `total = end` and break.
    Done {
        /// Checked new total (`offset + filled`; equals `offset` when
        /// the input was empty).
        end: u64,
    },
}

/// Process one filled buffer: capacity-check it, write it to the device,
/// and report the checked new total.
///
/// This is the loop-body invariant holder shared by the flash arms (per
/// the threading plan): O_DIRECT-vs-tail dispatch, the capacity
/// pre-check, and bytes accounting live here, fixed once. ENOSPC
/// mapping lives inside `write_direct`/`write_tail`; this helper
/// propagates the typed error unchanged. Progress-bar updates are the
/// caller's job (call `pb.set_position` after a successful return, and
/// `pb.abandon()` before propagating an `Err`).
///
/// Preconditions: `filled <= BUF_SIZE`; for the full-chunk arm `offset`
/// is a `BUF_SIZE` multiple (guaranteed by the caller advancing only to
/// `Continue::end`); the guard is armed in `GuardPhase::Writing`; the
/// FD has `O_DIRECT` set on entry (it is cleared here on the tail path,
/// exactly as the pre-extraction loop did).
fn process_chunk(
    guard: &FlashGuard,
    buf: &AlignedBuf,
    filled: usize,
    offset: u64,
    dev_size: u64,
) -> Result<ChunkOutcome> {
    if filled == 0 {
        return Ok(ChunkOutcome::Done { end: offset });
    }

    // Capacity pre-check doubles as the offset advance: `end` is the one
    // place offset arithmetic happens, checked, and both branches below
    // reuse it — no separate `offset + len` can drift from the bound
    // that was just verified.
    let Some(end) = chunk_end_within(offset, filled, dev_size) else {
        bail!(
            "image data exceeds device capacity: the next {filled} bytes at \
             offset {offset} would pass the device's end ({dev_size} bytes). \
             The (decompressed) image is larger than the target device. Aborting."
        );
    };

    if filled == BUF_SIZE {
        write_direct(guard, buf.as_slice(), offset)?;
        return Ok(ChunkOutcome::Continue { end });
    }

    // Tail: disable O_DIRECT and write the exact residual through the
    // page cache. This is always the final chunk.
    set_direct(guard.as_raw_fd(), false).context("disabling O_DIRECT for tail write")?;
    let tail = buf
        .as_slice()
        .get(..filled)
        .context("tail slice exceeds buffer (impossible: filled <= BUF_SIZE)")?;
    write_tail(guard, tail, offset)?;
    Ok(ChunkOutcome::Done { end })
}

/// Shared post-loop work for the flash arms: finish the progress bar,
/// emit the clean newline, clear `O_DIRECT` (hardening), and issue the
/// durable `fdatasync`.
///
/// Ordering note — deliberate divergence from the threading plan's
/// sketch, which ordered `fdatasync` first: this preserves the
/// pre-extraction code's order (UI teardown, then FD hardening, then
/// sync) byte-for-byte, because Step 1 of the plan is chartered as a
/// pure refactor. Durability is still established before this function
/// returns, i.e. before Phase 5 reads anything back; reordering would
/// be a behavior change and needs its own justification.
fn flash_finalize(guard: &FlashGuard, pb: &ProgressBar) -> Result<()> {
    pb.finish_and_clear();
    // Force a clean newline so the bar's last rendered position doesn't
    // mix with the next phase's text.
    println!();

    // Hardening — restore O_DIRECT to its pre-Phase-4 state (off) before
    // returning. The next phase (verify) currently calls `set_direct(fd,
    // false)` on entry anyway, so this is not strictly required today; but
    // leaving O_DIRECT set on exit makes the contract "the next phase
    // cleans up after us" rather than "each phase leaves the FD in a known
    // state". The latter is what we want — a future phase inserted between
    // 4 and 5, or a future call from a different context, would otherwise
    // inherit O_DIRECT and trip EINVAL on the first misaligned read or
    // write. The cost is one extra fcntl on the success path.
    set_direct(guard.as_raw_fd(), false).context("clearing O_DIRECT after flash write loop")?;

    // Durable commit before Phase 5 reads the device back.
    //
    // We pass `guard.file()` directly (a `&File`, which implements `AsFd`)
    // rather than `guard.as_raw_fd()`. nix ≥ 0.30 requires `AsFd` here,
    // and `BorrowedFd<'_>` (what AsFd produces) ties the borrow to the
    // open file's lifetime — preventing any use-after-close race.
    nix::unistd::fdatasync(guard.file()).context("fdatasync after flash write loop")?;
    Ok(())
}

/// Items the worker sends to the writer: a filled buffer plus its valid
/// byte count, or a fatal worker-side error (a `fill_buffer` failure,
/// already wrapped with the same context string the serial arm uses so
/// operator-visible error chains are identical across arms).
type FilledItem = Result<(AlignedBuf, usize)>;

/// Phase 4 entry point — a thin runtime dispatcher (threading plan,
/// Step 3). Raw images take [`flash_serial`], structurally the
/// pre-threading code with zero threading overhead; compressed images
/// take [`flash_pipelined`], which overlaps decompression with the
/// device write on a worker thread. The public contract (signature,
/// FATAL-on-unwind guard behavior, progress output, error text) is
/// unchanged from the pre-dispatch `flash`.
///
/// `dev_size` (from `BLKGETSIZE64`) bounds every write in both arms:
/// each chunk is pre-checked against device capacity *before* the
/// `pwrite` (inside [`process_chunk`]), so a compressed image that
/// decompresses past the device's end produces the friendly capacity
/// diagnostic deterministically. Without the pre-check, a chunk
/// straddling the device end can partially complete, and
/// `write_all_at`'s retry of the remainder — now at a misaligned offset
/// with a misaligned length under `O_DIRECT` — surfaces as a bare
/// `EINVAL` that `is_capacity_error` cannot recognise.
pub(crate) fn flash(
    guard: &mut FlashGuard,
    reader: ImageReader,
    comp: Compression,
    raw_size: Option<u64>,
    dev_size: u64,
    throttle: Option<u64>,
    cancel: &AtomicBool,
) -> Result<FlashOutcome> {
    if comp.is_compressed() {
        // raw_size is meaningless for compressed inputs (decompressed
        // total unknown); the pipelined arm always uses the spinner.
        flash_pipelined(guard, reader, comp, dev_size, throttle, cancel)
    } else {
        flash_serial(guard, reader, comp, raw_size, dev_size, throttle, cancel)
    }
}

/// Decompression worker for the pipelined arm. Owns the reader by-move;
/// never sees the device FD or the guard (single-threaded-FD access is
/// structural, not conventional). Exits on: pool disconnect (main
/// dropped `free_tx`), cancel mirror, fill error (sent to main first),
/// send failure (main dropped `filled_rx`), or EOF (a fill shorter than
/// `BUF_SIZE`, which main processes as the tail).
///
/// Generic over the reader so tests can drive the protocol with
/// erroring or panicking readers; production instantiates it with
/// [`ImageReader`] only.
fn worker_loop<R: Read>(
    mut reader: R,
    worker_cancel: &Arc<AtomicBool>,
    filled_tx: &mpsc::Sender<FilledItem>,
    free_rx: &mpsc::Receiver<AlignedBuf>,
) {
    loop {
        // 1. Acquire a buffer from the pool (blocks while both are in
        //    flight on the writer side).
        let Ok(mut buf) = free_rx.recv() else {
            return; // main shut down (dropped free_tx)
        };

        // 2. Cancel mirror — lets a cancel skip a wasted fill (up to
        //    ~80 ms on bzip2) that main would only discard.
        if worker_cancel.load(Ordering::SeqCst) {
            return;
        }

        // 3. Fill from the decompressor. Same context string as the
        //    serial arm so error chains are arm-independent.
        let filled = match fill_buffer(&mut reader, buf.as_mut_slice()) {
            Ok(n) => n,
            Err(e) => {
                let wrapped = Err(anyhow::Error::from(e).context("reading from image stream"));
                // Send failure means main is already gone; nothing to do.
                if filled_tx.send(wrapped).is_err() { /* main exited first */ }
                return;
            }
        };

        // 4. Hand the filled buffer to the writer.
        if filled_tx.send(Ok((buf, filled))).is_err() {
            return; // main dropped filled_rx (error/cancel shutdown)
        }

        // 5. EOF: a short fill is always the final chunk; main will
        //    process it as the tail (or the empty-input Done) and break.
        if filled < BUF_SIZE {
            return;
        }
    }
}

/// Pipelined flash arm — routes compressed images (per the dispatcher).
///
/// Producer-consumer over two `mpsc` channels with a two-buffer pool:
/// the worker decompresses into one `AlignedBuf` while this (main)
/// thread writes the other. Only this thread ever touches the device
/// FD; the worker never receives `&FlashGuard`. Loop discipline (from
/// the threading plan, enforced by review): no `return` inside
/// `'write_loop` — every exit `break`s to the single cleanup block,
/// which drops BOTH main-side channel halves (either drop alone can
/// leave the worker blocked on the other operation), joins the worker,
/// and only then re-raises a captured worker panic so
/// `FlashGuard::drop`'s FATAL fires during a fully-cleaned-up unwind.
fn flash_pipelined<R: Read + Send + 'static>(
    guard: &mut FlashGuard,
    reader: R,
    comp: Compression,
    dev_size: u64,
    throttle: Option<u64>,
    cancel: &AtomicBool,
) -> Result<FlashOutcome> {
    set_direct(guard.as_raw_fd(), true).context("enabling O_DIRECT for flash phase")?;

    // Compressed inputs: decompressed total unknown -> spinner variant.
    let pb = make_progress_bar(comp, None);
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

    // Cancel mirror: the worker cannot borrow `cancel` (non-'static);
    // main observes the parent flag and mirrors it into this Arc.
    let local_cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&local_cancel);

    let (filled_tx, filled_rx) = mpsc::channel::<FilledItem>();
    let (free_tx, free_rx) = mpsc::channel::<AlignedBuf>();
    // Seed the two-buffer pool before spawning. The receiver is alive on
    // our stack, so send cannot fail; bail (not expect) keeps the
    // restriction-lint contract if that impossibility ever breaks.
    for _ in 0..2_u8 {
        if free_tx.send(AlignedBuf::new()?).is_err() {
            bail!("buffer-pool receiver closed before worker spawn (unreachable)");
        }
    }

    let mut worker_handle: Option<thread::JoinHandle<()>> = Some(thread::spawn(move || {
        worker_loop(reader, &worker_cancel, &filled_tx, &free_rx);
    }));
    let mut worker_panic: Option<Box<dyn std::any::Any + Send>> = None;

    // Initialized (unlike flash_serial's declared-uninit `total`): on
    // the error/cancel exits `total` is never read (they return before
    // the post-loop read), but flow analysis cannot correlate `outcome`
    // with `total`, so an initializer is required for the code to
    // compile — and 0 is the honest "nothing written yet" value.
    let mut total: u64 = 0;
    let mut offset: u64 = 0;
    let mut outcome: Result<()> = Ok(());

    'write_loop: loop {
        // 1. Cancel check (parent flag), mirrored to the worker.
        if cancel.load(Ordering::SeqCst) {
            local_cancel.store(true, Ordering::SeqCst);
            outcome = Err(anyhow!("cancelled by user"));
            break 'write_loop;
        }

        let start = chunk_target_nanos.map(|_| Instant::now());

        // 2. Receive the next filled buffer. `t0` deliberately starts
        //    before this recv: the recv wait is the decompression cost
        //    that is actually serialized from the writer's perspective,
        //    so the throttle keeps measuring real chunk wall-time.
        let (buf, filled) = match filled_rx.recv() {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                outcome = Err(e);
                break 'write_loop;
            }
            Err(_) => {
                // Disconnected. Distinguish worker panic (join Err,
                // re-raised after the cleanup block with channels
                // closed) from a clean exit — which, HERE, is a
                // protocol violation: every voluntary worker exit path
                // sends a final item first (tail, `(buf, 0)`, or a
                // fill error), so a silent disappearance must fail
                // loud rather than let a partial flash return SUCCESS.
                // (The plan's sketch broke with Ok here; hardened
                // during self-review.)
                match worker_handle.take().map(thread::JoinHandle::join) {
                    Some(Err(panic)) => worker_panic = Some(panic),
                    Some(Ok(())) | None => {
                        outcome = Err(anyhow!(
                            "image worker exited without sending a final \
                             chunk (pipeline protocol violation)"
                        ));
                    }
                }
                break 'write_loop;
            }
        };

        // 3. Shared loop-body invariants (capacity, O_DIRECT-vs-tail,
        //    accounting) — identical helper call to flash_serial.
        match process_chunk(guard, &buf, filled, offset, dev_size) {
            Ok(ChunkOutcome::Continue { end }) => {
                offset = end;
                total = end;
                pb.set_position(total);
                // Return the drained buffer to the pool. If the worker
                // already exited (EOF), the send fails harmlessly; the
                // next recv sees the disconnect.
                if free_tx.send(buf).is_err() { /* worker done */ }
            }
            Ok(ChunkOutcome::Done { end }) => {
                total = end;
                break 'write_loop;
            }
            Err(e) => {
                outcome = Err(e);
                break 'write_loop;
            }
        }

        // 4. Throttle, then re-check cancel: cancellable_sleep returns
        //    early on cancel, and blocking in the next recv on a chunk
        //    the (mirrored-cancelled) worker will never send would hang.
        if let (Some(target_ns), Some(t0)) = (chunk_target_nanos, start) {
            let elapsed_ns = u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX);
            if let Some(residual) = target_ns.checked_sub(elapsed_ns) {
                cancellable_sleep(Duration::from_nanos(residual), cancel);
                if cancel.load(Ordering::SeqCst) {
                    local_cancel.store(true, Ordering::SeqCst);
                    outcome = Err(anyhow!("cancelled by user"));
                    break 'write_loop;
                }
            }
        }
    }

    // Single cleanup block — every 'write_loop exit lands here.
    //
    // Divergence from the threading plan's rationale, which assumed a
    // send could block: `std::sync::mpsc::channel` is UNBOUNDED, so
    // `filled_tx.send` never blocks — backpressure comes from the
    // two-buffer pool (the worker cannot fill without a buffer), which
    // is what bounds pipeline depth. The only operation the worker can
    // actually block on is `free_rx.recv()`, and `drop(free_tx)` is the
    // unblocker; it MUST precede the join or the join deadlocks.
    // `drop(filled_rx)` is still done first, for two real (if softer)
    // reasons: a worker mid-iteration gets a send `Err` and exits
    // promptly instead of looping into one more fill, and any filled
    // 4 MiB buffer parked in the channel is freed now rather than at
    // function end.
    drop(filled_rx);
    drop(free_tx);
    if let Some(handle) = worker_handle.take() {
        // A panic here (without a prior disconnect) is re-raised below;
        // a clean Err cannot occur for a () worker.
        if let Err(panic) = handle.join() {
            worker_panic = Some(panic);
        }
    }
    if let Some(panic) = worker_panic {
        // Channels are closed and the worker is joined; unwinding now
        // runs FlashGuard::drop with the correct "being written" FATAL.
        std::panic::resume_unwind(panic);
    }

    if let Err(e) = outcome {
        pb.abandon();
        return Err(e);
    }

    pb.set_position(total);
    flash_finalize(guard, &pb)?;

    Ok(FlashOutcome { bytes_written: total })
}

/// Fill `dst` with bytes from `reader`. Returns the number of bytes actually
/// placed in `dst`. Short reads are re-driven until the buffer is full or
/// the reader hits EOF.
#[expect(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "loop invariant: filled < dst.len() at the index site, and \
              n <= dst.len() - filled per the Read contract, so filled + n \
              cannot overflow dst.len(), let alone usize"
)]
fn fill_buffer<R: Read>(reader: &mut R, dst: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < dst.len() {
        match reader.read(&mut dst[filled..]) {
            Ok(0) => break, // EOF
            Ok(n) => filled += n,
            // Retry: EINTR is transparent to the caller.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// If writing `len` bytes at `offset` stays within a device of `dev_size`
/// bytes, return the end offset (`offset + len`); otherwise `None`.
/// Overflow-safe: an offset+len that overflows `u64` trivially exceeds any
/// real device. Pure; unit-tested below.
fn chunk_end_within(offset: u64, len: usize, dev_size: u64) -> Option<u64> {
    offset.checked_add(len as u64).filter(|&end| end <= dev_size)
}

/// Write a full aligned chunk via `pwrite`. Maps `ENOSPC` / short-write to a
/// typed error.
fn write_direct(guard: &FlashGuard, data: &[u8], offset: u64) -> Result<()> {
    match guard.file().write_all_at(data, offset) {
        Ok(()) => Ok(()),
        Err(e) if is_capacity_error(&e) => Err(anyhow!(
            "device ran out of space at offset {offset}. The decompressed \
             image is larger than the target device. Aborting."
        )),
        Err(e) => Err(anyhow::Error::from(e)
            .context(format!("writing 4 MiB chunk at device offset {offset}"))),
    }
}

/// Write the final, sub-chunk residual via buffered `pwrite` (`O_DIRECT`
/// already cleared by the caller). Maps `ENOSPC` / short-write to a
/// typed error, like [`write_direct`].
fn write_tail(guard: &FlashGuard, data: &[u8], offset: u64) -> Result<()> {
    match guard.file().write_all_at(data, offset) {
        Ok(()) => Ok(()),
        Err(e) if is_capacity_error(&e) => Err(anyhow!(
            "device ran out of space writing tail ({} bytes at offset {offset}). \
             The decompressed image is larger than the target device.",
            data.len()
        )),
        Err(e) => Err(anyhow::Error::from(e)
            .context(format!("writing tail ({} bytes) at offset {offset}", data.len()))),
    }
}

/// Both `ENOSPC` and `WriteZero` mean "the device is full". The former is
/// what the kernel returns on a full block device; the latter is what
/// `write_all_at` synthesizes when partial writes stall. `ErrorKind::StorageFull`
/// was stabilised in Rust 1.83.
fn is_capacity_error(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::StorageFull | std::io::ErrorKind::WriteZero)
}

/// Sleep for `total`, polling `cancel` between fixed-size sub-sleeps so
/// Ctrl+C is responsive even at very low throttle rates.
///
/// Used by both Phase 4 (flash) and Phase 5b (verify) for the
/// post-chunk throttle wait. At `--throttle 100K`, one 4 MiB chunk
/// translates to a ~40-second sleep; without polling, Ctrl+C would
/// wait the full residual before the loop noticed. 100ms granularity
/// is well below human-perceptible unresponsiveness and adds one
/// atomic load per tick — invisible cost in any realistic profile.
///
/// Sub-100ms sleeps are issued in one shot since the polling overhead
/// would dominate.
///
/// On cancel, the function returns from the sleep early but does *not*
/// itself signal the cancellation: it returns `()` rather than `Result`,
/// so the caller's outer loop is responsible for re-checking the flag
/// at its next iteration boundary and bailing with a context-wrapped
/// error. Returning `Result` from here would force every call site to
/// `?`-propagate, adding error-path noise without any new information.
pub(crate) fn cancellable_sleep(total: Duration, cancel: &AtomicBool) {
    const TICK: Duration = Duration::from_millis(100);
    if total <= TICK {
        if !total.is_zero() {
            thread::sleep(total);
        }
        return;
    }
    // `Instant + Duration` panics on overflow. The realistic ceiling for
    // `total` is the throttle calculation's worst case (~48 days at
    // rate=1), well under any monotonic-clock representation limit, so
    // the panic is unreachable in practice — but we'd rather degrade
    // to a single non-cancellable sleep than panic deep inside the
    // destructive pipeline.
    let Some(deadline) = Instant::now().checked_add(total) else {
        thread::sleep(total);
        return;
    };
    loop {
        if cancel.load(Ordering::SeqCst) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        thread::sleep(remaining.min(TICK));
    }
}

/// Toggle `O_DIRECT` on an already-open FD via `fcntl(F_SETFL, …)`.
///
/// Reads current flags, flips the bit, writes them back. Only `O_DIRECT`
/// changes; `O_APPEND`, `O_NONBLOCK`, etc. are preserved.
pub(crate) fn set_direct(fd: RawFd, enable: bool) -> Result<()> {
    // SAFETY: `fd` is a valid file descriptor owned by the FlashGuard;
    // `F_GETFL` has no memory preconditions beyond a valid fd. Return is
    // either `-1` on error (checked via errno) or the flags as a c_int.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error()).context("fcntl(F_GETFL)");
    }
    let new_flags = if enable { flags | libc::O_DIRECT } else { flags & !libc::O_DIRECT };
    if new_flags == flags {
        return Ok(());
    }
    // SAFETY: `fd` is valid (see above). `F_SETFL` with an int argument is
    // well-defined; the kernel validates the flag bits itself.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, new_flags) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error())
            .context(format!("fcntl(F_SETFL, O_DIRECT={enable})"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, ErrorKind};

    // -- is_capacity_error ----------------------------------------------

    /// `ENOSPC` from the kernel (the canonical "device full" errno) must
    /// be classified as a capacity error. We construct the error via
    /// `from_raw_os_error(ENOSPC)` rather than naming `ErrorKind::StorageFull`
    /// directly so this test compiles on toolchains predating Rust 1.83
    /// (which stabilised `StorageFull`). On 1.83+ the resulting error's
    /// `.kind()` is `StorageFull`; the production matcher recognises both
    /// paths.
    #[test]
    fn capacity_error_recognises_enospc() {
        let e = std::io::Error::from_raw_os_error(libc::ENOSPC);
        assert!(is_capacity_error(&e));
    }

    /// `write_all_at` synthesises `ErrorKind::WriteZero` when the
    /// underlying write returns 0 bytes (i.e. the device couldn't accept
    /// any more). Also a capacity signal.
    #[test]
    fn capacity_error_recognises_write_zero() {
        let e = std::io::Error::from(ErrorKind::WriteZero);
        assert!(is_capacity_error(&e));
    }

    /// The per-chunk capacity pre-check: exact fit passes (and yields the
    /// end offset the loop advances to), one byte past the end fails, and
    /// an offset+len that overflows u64 fails rather than wrapping around
    /// into a "fits" verdict.
    #[test]
    fn chunk_end_within_boundaries() {
        // Exact fit to device end — allowed, end == dev_size.
        assert_eq!(chunk_end_within(0, 4096, 4096), Some(4096));
        assert_eq!(chunk_end_within(4096, 4096, 8192), Some(8192));
        // One byte past the end — rejected.
        assert_eq!(chunk_end_within(1, 4096, 4096), None);
        assert_eq!(chunk_end_within(4097, 4096, 8192), None);
        // Zero-length write never exceeds (loop never issues one, but the
        // predicate must not misfire on it).
        assert_eq!(chunk_end_within(4096, 0, 4096), Some(4096));
        // Overflow safety: u64::MAX offset plus any length must reject.
        assert_eq!(chunk_end_within(u64::MAX, 1, u64::MAX), None);
        assert_eq!(chunk_end_within(u64::MAX - 1, 4096, u64::MAX), None);
    }

    /// Non-capacity errors (EIO, EPERM, etc.) must NOT be classified as
    /// capacity. A misclassification here would mean a hardware fault
    /// during write surfaces as "image too large" — misleading the
    /// operator into trying a smaller image when their device is dying.
    #[test]
    fn capacity_error_rejects_unrelated_errors() {
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::NotFound,
            ErrorKind::Interrupted,
            ErrorKind::TimedOut,
            ErrorKind::BrokenPipe,
            ErrorKind::InvalidInput,
            ErrorKind::Other,
            ErrorKind::UnexpectedEof,
        ] {
            let e = std::io::Error::from(kind);
            assert!(
                !is_capacity_error(&e),
                "ErrorKind::{kind:?} should not be classified as capacity"
            );
        }
    }

    // -- fill_buffer ----------------------------------------------------

    /// Happy path: reader has more data than the destination; we fill
    /// the buffer exactly and return.
    #[test]
    fn fill_buffer_fills_when_data_abundant() {
        let data: Vec<u8> = (0..100).cycle().take(8192).collect();
        let mut r = Cursor::new(data.clone());
        let mut dst = vec![0_u8; 4096];
        let n = fill_buffer(&mut r, &mut dst).unwrap();
        assert_eq!(n, 4096);
        assert_eq!(&*dst, &data[..4096]);
    }

    /// EOF before the buffer is full: returns the partial fill count.
    /// This is the *signal* that we've reached the end of the image —
    /// downstream code uses `filled < BUF_SIZE` to identify the tail
    /// chunk.
    #[test]
    fn fill_buffer_returns_partial_on_eof() {
        let data = vec![0xCD_u8; 1500];
        let mut r = Cursor::new(data);
        let mut dst = vec![0_u8; 4096];
        let n = fill_buffer(&mut r, &mut dst).unwrap();
        assert_eq!(n, 1500);
        assert!(dst[..1500].iter().all(|&b| b == 0xCD));
        assert!(dst[1500..].iter().all(|&b| b == 0)); // unfilled remains 0
    }

    /// Empty input: must return Ok(0), not an error. Downstream uses
    /// this to detect EOF on the first iteration.
    #[test]
    fn fill_buffer_returns_zero_on_empty_input() {
        let mut r = Cursor::new(Vec::<u8>::new());
        let mut dst = vec![0_u8; 4096];
        let n = fill_buffer(&mut r, &mut dst).unwrap();
        assert_eq!(n, 0);
    }

    /// Short reads: a reader that returns small chunks must be
    /// re-driven until the destination is full. `flate2`'s `Read` impl
    /// can return arbitrarily small chunks; our caller wires them
    /// straight into 4 MiB-aligned writes, so re-driving short reads
    /// is non-optional.
    #[test]
    fn fill_buffer_redrives_short_reads() {
        struct Trickle<'a> {
            data: &'a [u8],
            pos: usize,
            chunk: usize,
        }
        impl Read for Trickle<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let avail = self.data.len() - self.pos;
                let n = self.chunk.min(buf.len()).min(avail);
                buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
                self.pos += n;
                Ok(n)
            }
        }
        let data: Vec<u8> = (0..255_u8).cycle().take(5000).collect();
        let mut r = Trickle { data: &data, pos: 0, chunk: 17 }; // tiny chunks
        let mut dst = vec![0_u8; 4096];
        let n = fill_buffer(&mut r, &mut dst).unwrap();
        assert_eq!(n, 4096);
        assert_eq!(&*dst, &data[..4096]);
    }

    /// `Interrupted` errors (EINTR from a signal) must be retried, not
    /// propagated. Without this, a SIGCHLD or any spurious signal during
    /// a slow read would abort the flash.
    #[test]
    fn fill_buffer_retries_on_interrupted() {
        struct ThenSucceed {
            interrupts_left: u32,
            data: Vec<u8>,
            pos: usize,
        }
        impl Read for ThenSucceed {
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
        let mut r = ThenSucceed { interrupts_left: 5, data: vec![0x42; 100], pos: 0 };
        let mut dst = vec![0_u8; 50];
        let n = fill_buffer(&mut r, &mut dst).unwrap();
        assert_eq!(n, 50);
        assert!(dst.iter().all(|&b| b == 0x42));
    }

    /// Non-interrupted IO errors propagate.
    #[test]
    fn fill_buffer_propagates_other_errors() {
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("synthetic failure"))
            }
        }
        let mut r = Failing;
        let mut dst = vec![0_u8; 100];
        let err = fill_buffer(&mut r, &mut dst).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Other);
    }

    /// Empty destination buffer is a no-op that returns 0 cleanly,
    /// regardless of what the reader has available.
    #[test]
    fn fill_buffer_zero_length_dst_is_noop() {
        let mut r = Cursor::new(vec![1_u8, 2, 3, 4]);
        let mut dst: [u8; 0] = [];
        let n = fill_buffer(&mut r, &mut dst).unwrap();
        assert_eq!(n, 0);
    }

    // -- cancellable_sleep ----------------------------------------------

    /// Sub-tick sleep is single-shot. Must complete in approximately the
    /// requested duration (not faster, not 100ms-rounded).
    #[test]
    fn cancellable_sleep_short_duration_completes() {
        let cancel = AtomicBool::new(false);
        let t0 = Instant::now();
        cancellable_sleep(Duration::from_millis(20), &cancel);
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_millis(15),
            "should sleep at least ~20ms, slept {elapsed:?}"
        );
        // Generous upper bound to absorb test-runner scheduling jitter.
        assert!(
            elapsed < Duration::from_millis(500),
            "should not significantly overshoot, slept {elapsed:?}"
        );
    }

    /// Zero-duration sleep is a no-op (no sleep call at all).
    #[test]
    fn cancellable_sleep_zero_is_noop() {
        let cancel = AtomicBool::new(false);
        let t0 = Instant::now();
        cancellable_sleep(Duration::from_secs(0), &cancel);
        assert!(
            t0.elapsed() < Duration::from_millis(50),
            "zero-duration sleep should not actually sleep"
        );
    }

    /// Setting cancel before the sleep starts: the function returns
    /// promptly (within the granularity of one tick). This is the
    /// common case when Ctrl+C fires just before the throttle sleep.
    #[test]
    fn cancellable_sleep_returns_promptly_when_pre_cancelled() {
        let cancel = AtomicBool::new(true);
        let t0 = Instant::now();
        cancellable_sleep(Duration::from_secs(10), &cancel);
        let elapsed = t0.elapsed();
        // The function checks the flag at the top of the loop, so for a
        // long sleep it returns essentially immediately.
        assert!(
            elapsed < Duration::from_millis(100),
            "pre-cancelled long sleep should return promptly, slept {elapsed:?}"
        );
    }

    /// Setting cancel mid-sleep: the function returns within one tick
    /// (~100ms) of the flag being set. This is the property that
    /// makes Ctrl+C feel responsive even at very low throttle rates.
    ///
    /// We deliberately do NOT assert a lower bound on elapsed time.
    /// The setter thread's `sleep(150ms)` and the main thread's `t0`
    /// are not strictly synchronized — under heavy CI load the setter
    /// can start before `t0` is recorded, making a `>= 150ms` assertion
    /// flaky for non-bug reasons. The substantive property is "returns
    /// within ~one tick of cancel being set"; the
    /// `cancellable_sleep_returns_promptly_when_pre_cancelled` test
    /// covers the "doesn't return immediately when cancel is *not*
    /// set" half of the contract.
    #[test]
    fn cancellable_sleep_returns_within_one_tick_after_cancel() {
        use std::sync::Arc;
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_setter = Arc::clone(&cancel);
        // Trigger cancel after ~150ms.
        let setter = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            cancel_setter.store(true, Ordering::SeqCst);
        });
        let t0 = Instant::now();
        // Sleep target: 5 seconds. With cancel firing after ~150ms and
        // 100ms tick granularity, we should return well before the full
        // 5 seconds elapse — the upper bound proves cancel-responsiveness.
        cancellable_sleep(Duration::from_secs(5), &cancel);
        let elapsed = t0.elapsed();
        setter.join().unwrap();
        assert!(
            elapsed < Duration::from_millis(500),
            "should return within ~one tick after cancel was set, slept {elapsed:?}"
        );
    }

    // ------------------------------------------------------------------
    // process_chunk / flash_finalize — Step 1 of the threading plan.
    // Approach (b) from the plan: real FlashGuard over a tempfile;
    // outcomes verified through the file's contents. Re-derived for the
    // post-plan signature (dev_size + chunk_end_within capacity check).
    // ------------------------------------------------------------------

    /// Real guard over a fresh tempfile, plus the path for cleanup.
    fn tempfile_guard(tag: &str) -> (FlashGuard, std::path::PathBuf) {
        let p = std::env::temp_dir().join(format!("imi-chunk-{tag}-{}", std::process::id()));
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&p)
            .unwrap();
        (FlashGuard::new(f, p.clone()), p)
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn process_chunk_full_chunk_writes_and_continues() {
        let (guard, p) = tempfile_guard("full");
        let mut buf = AlignedBuf::new().unwrap();
        buf.as_mut_slice().fill(0xA7);
        let out = process_chunk(&guard, &buf, BUF_SIZE, 0, BUF_SIZE as u64).unwrap();
        assert!(matches!(out, ChunkOutcome::Continue { end } if end == BUF_SIZE as u64));
        let written = std::fs::read(&p).unwrap();
        assert_eq!(written.len(), BUF_SIZE);
        assert!(written.iter().all(|&b| b == 0xA7));
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn process_chunk_tail_writes_partial_and_finishes() {
        let (guard, p) = tempfile_guard("tail");
        let mut buf = AlignedBuf::new().unwrap();
        buf.as_mut_slice().fill(0x5C);
        let out = process_chunk(&guard, &buf, 1234, 0, BUF_SIZE as u64).unwrap();
        assert!(matches!(out, ChunkOutcome::Done { end } if end == 1234));
        let written = std::fs::read(&p).unwrap();
        assert_eq!(written.len(), 1234);
        assert!(written.iter().all(|&b| b == 0x5C));
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn process_chunk_empty_is_done_without_write() {
        let (guard, p) = tempfile_guard("empty");
        let buf = AlignedBuf::new().unwrap();
        let out = process_chunk(&guard, &buf, 0, 4096, BUF_SIZE as u64).unwrap();
        // `end == offset`: empty input advances nothing.
        assert!(matches!(out, ChunkOutcome::Done { end } if end == 4096));
        assert_eq!(std::fs::read(&p).unwrap().len(), 0, "no write may happen");
        std::fs::remove_file(&p).unwrap();
    }

    /// The capacity invariant lives in the helper (post-plan divergence):
    /// a chunk that would pass the device's end is refused before any
    /// byte is written — full-chunk and tail shapes both.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn process_chunk_refuses_chunk_beyond_dev_size() {
        let (guard, p) = tempfile_guard("cap");
        let buf = AlignedBuf::new().unwrap();

        let err = process_chunk(&guard, &buf, BUF_SIZE, 0, 4096).unwrap_err();
        assert!(err.to_string().contains("exceeds device capacity"), "{err}");

        let tail_err = process_chunk(&guard, &buf, 100, 4090, 4096).unwrap_err();
        assert!(tail_err.to_string().contains("exceeds device capacity"), "{tail_err}");

        assert_eq!(std::fs::read(&p).unwrap().len(), 0, "refusal must not write");
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // miri ICE
    fn flash_finalize_finishes_bar_and_returns_ok() {
        let (guard, p) = tempfile_guard("fin");
        let pb = ProgressBar::new(4);
        pb.set_position(4);
        flash_finalize(&guard, &pb).unwrap();
        assert!(pb.is_finished());
        // The guard's FD stays usable after finalize (hardening cleared
        // O_DIRECT; fdatasync succeeded on a regular file).
        guard.file().sync_data().unwrap();
        std::fs::remove_file(&p).unwrap();
    }
    // ------------------------------------------------------------------
    // Pipelined-arm protocol tests — Step 3 of the threading plan
    // (tests 6-11, re-derived). Real threads, real channels, tempfile
    // guards; readers are std::io stubs via the generic worker/arm.
    // ------------------------------------------------------------------

    /// Test 6a (shutdown protocol): a worker blocked in
    /// `free_rx.recv()` — the one operation that can actually block on
    /// an unbounded mpsc — exits when main drops `free_tx`.
    #[test]
    fn worker_exits_when_pool_sender_drops() {
        let (filled_tx, _filled_rx) = mpsc::channel::<FilledItem>();
        let (free_tx, free_rx) = mpsc::channel::<AlignedBuf>();
        let mirror = Arc::new(AtomicBool::new(false));
        let m = Arc::clone(&mirror);
        let h = thread::spawn(move || worker_loop(std::io::empty(), &m, &filled_tx, &free_rx));
        // No seed: the worker is parked in free_rx.recv(). Unblock it.
        drop(free_tx);
        h.join().unwrap();
    }

    /// Test 6b (shutdown protocol): after main drops `filled_rx`, the
    /// worker's next send fails and it exits instead of looping.
    #[test]
    fn worker_exits_when_filled_receiver_drops() {
        let (filled_tx, filled_rx) = mpsc::channel::<FilledItem>();
        let (free_tx, free_rx) = mpsc::channel::<AlignedBuf>();
        let mirror = Arc::new(AtomicBool::new(false));
        let m = Arc::clone(&mirror);
        drop(filled_rx); // main gone before the first handoff
        free_tx.send(AlignedBuf::new().unwrap()).unwrap();
        let h = thread::spawn(move || worker_loop(std::io::repeat(0), &m, &filled_tx, &free_rx));
        h.join().unwrap();
    }

    /// Test 7 (cancel mirror): a pre-set mirror makes the worker exit
    /// after taking a buffer but before filling or sending anything —
    /// observed as a disconnect with no item ever delivered.
    #[test]
    fn cancel_mirror_stops_worker_before_fill() {
        let (filled_tx, filled_rx) = mpsc::channel::<FilledItem>();
        let (free_tx, free_rx) = mpsc::channel::<AlignedBuf>();
        let mirror = Arc::new(AtomicBool::new(true));
        let m = Arc::clone(&mirror);
        free_tx.send(AlignedBuf::new().unwrap()).unwrap();
        let h = thread::spawn(move || worker_loop(std::io::repeat(0), &m, &filled_tx, &free_rx));
        assert!(filled_rx.recv().is_err(), "worker must not send after cancel");
        h.join().unwrap();
    }

    /// Test 8 (worker-error propagation, end to end): a truncated gzip
    /// stream makes the worker's fill fail; `flash_pipelined` surfaces
    /// the error with the serial arm's context string and writes
    /// nothing to the device.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn pipelined_propagates_worker_error_with_context() {
        // 100 KiB of zeros, gzipped, then truncated mid-deflate-stream.
        let img_p = std::env::temp_dir().join(format!("imi-trunc-{}.gz", std::process::id()));
        let full = {
            use std::io::Write as _;
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            enc.write_all(&vec![0_u8; 100 * 1024]).unwrap();
            enc.finish().unwrap()
        };
        std::fs::write(&img_p, &full[..40]).unwrap();
        let reader = ImageReader::open(&img_p, Compression::Gzip).unwrap();

        let (mut guard, dev_p) = tempfile_guard("werr");
        let cancel = AtomicBool::new(false);
        let err =
            flash_pipelined(&mut guard, reader, Compression::Gzip, BUF_SIZE as u64, None, &cancel)
                .unwrap_err();
        assert!(format!("{err:#}").contains("reading from image stream"), "{err:#}");
        assert_eq!(std::fs::read(&dev_p).unwrap().len(), 0, "no bytes may land");
        std::fs::remove_file(&dev_p).unwrap();
        std::fs::remove_file(&img_p).unwrap();
    }

    /// A reader that panics on first read — drives test 9 through the
    /// real disconnect-join-capture-resume_unwind path.
    struct PanickingReader;
    impl Read for PanickingReader {
        #[expect(
            clippy::panic_in_result_fn,
            reason = "panicking is this stub's entire purpose: it drives \
                      the worker-panic propagation test through the real \
                      resume_unwind path"
        )]
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            panic!("injected worker panic (test)");
        }
    }

    /// Test 9 (worker-panic propagation): the panic crosses the join
    /// and re-raises on the main thread AFTER channel cleanup, so an
    /// armed guard's Drop would fire during this unwind. Nothing may
    /// have been written.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn pipelined_resumes_worker_panic_on_main_thread() {
        let (mut guard, dev_p) = tempfile_guard("wpanic");
        let cancel = AtomicBool::new(false);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            flash_pipelined(
                &mut guard,
                PanickingReader,
                Compression::Gzip,
                BUF_SIZE as u64,
                None,
                &cancel,
            )
        }));
        assert!(caught.is_err(), "worker panic must resume on main");
        assert_eq!(std::fs::read(&dev_p).unwrap().len(), 0);
        std::fs::remove_file(&dev_p).unwrap();
    }

    /// Test 10 (throttle survives pipelining): 8 MiB of zeros (two full
    /// chunks) at 4 MiB/s must take at least ~2 chunk-periods; assert a
    /// loose lower bound (no upper bound — CI boxes stall).
    #[test]
    #[cfg_attr(miri, ignore)]
    fn pipelined_throttle_enforces_rate_floor() {
        let img_p = std::env::temp_dir().join(format!("imi-thr-{}.gz", std::process::id()));
        let gz = {
            use std::io::Write as _;
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            enc.write_all(&vec![0_u8; 2 * BUF_SIZE]).unwrap();
            enc.finish().unwrap()
        };
        std::fs::write(&img_p, gz).unwrap();
        let reader = ImageReader::open(&img_p, Compression::Gzip).unwrap();

        let (mut guard, dev_p) = tempfile_guard("thr");
        let cancel = AtomicBool::new(false);
        let t0 = Instant::now();
        let out = flash_pipelined(
            &mut guard,
            reader,
            Compression::Gzip,
            (2 * BUF_SIZE) as u64,
            Some(4 * 1024 * 1024), // 4 MiB/s -> 1 s per chunk target
            &cancel,
        )
        .unwrap();
        assert_eq!(out.bytes_written, (2 * BUF_SIZE) as u64);
        assert!(
            t0.elapsed() >= Duration::from_millis(1200),
            "throttle floor violated: {:?}",
            t0.elapsed()
        );
        std::fs::remove_file(&dev_p).unwrap();
        std::fs::remove_file(&img_p).unwrap();
    }

    /// Test 11 (dispatch parity): the same 5 MiB + 137 B pattern, sent
    /// raw through `flash_serial` and gzipped through
    /// `flash_pipelined`, must produce identical outcomes and identical
    /// device bytes — the structural guard against arm drift.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn arms_produce_identical_bytes_for_identical_input() {
        let mut pattern = vec![0xC3_u8; 5 * 1024 * 1024];
        pattern.extend_from_slice(&[0x3C; 137]);
        let pid = std::process::id();
        let dir = std::env::temp_dir();

        let raw_p = dir.join(format!("imi-par-raw-{pid}.img"));
        std::fs::write(&raw_p, &pattern).unwrap();
        let gz_p = dir.join(format!("imi-par-{pid}.gz"));
        let gz = {
            use std::io::Write as _;
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            enc.write_all(&pattern).unwrap();
            enc.finish().unwrap()
        };
        std::fs::write(&gz_p, gz).unwrap();

        let cancel = AtomicBool::new(false);
        let dev_size = 64 * 1024 * 1024;

        let (mut g1, d1) = tempfile_guard("par-serial");
        let r1 = ImageReader::open(&raw_p, Compression::Raw).unwrap();
        let o1 = flash_serial(
            &mut g1,
            r1,
            Compression::Raw,
            Some(pattern.len() as u64),
            dev_size,
            None,
            &cancel,
        )
        .unwrap();

        let (mut g2, d2) = tempfile_guard("par-pipe");
        let r2 = ImageReader::open(&gz_p, Compression::Gzip).unwrap();
        let o2 = flash_pipelined(&mut g2, r2, Compression::Gzip, dev_size, None, &cancel).unwrap();

        assert_eq!(o1.bytes_written, o2.bytes_written);
        assert_eq!(o1.bytes_written, pattern.len() as u64);
        let b1 = std::fs::read(&d1).unwrap();
        let b2 = std::fs::read(&d2).unwrap();
        assert_eq!(b1, pattern, "serial arm bytes differ from input");
        assert_eq!(b1, b2, "arms diverged");

        for p in [d1, d2, raw_p, gz_p] {
            std::fs::remove_file(p).unwrap();
        }
    }
    /// Plan test 4, real form: the ENOSPC diagnostic mapped inside
    /// `write_direct`/`write_tail` must survive propagation through
    /// `process_chunk` untouched. `/dev/full` returns ENOSPC on every
    /// write, giving the real error without mocks (approach (b)).
    #[test]
    #[cfg_attr(miri, ignore)]
    fn process_chunk_surfaces_enospc_diagnostic() {
        let f = std::fs::OpenOptions::new().write(true).read(true).open("/dev/full").unwrap();
        let guard = FlashGuard::new(f, std::path::PathBuf::from("/dev/full"));
        let buf = AlignedBuf::new().unwrap();
        let huge = u64::MAX / 2; // capacity pre-check must pass

        let e_full = process_chunk(&guard, &buf, BUF_SIZE, 0, huge).unwrap_err();
        assert!(e_full.to_string().contains("ran out of space at offset 0"), "{e_full}");

        let e_tail = process_chunk(&guard, &buf, 1234, 0, huge).unwrap_err();
        assert!(e_tail.to_string().contains("ran out of space writing tail"), "{e_tail}");
    }
}
