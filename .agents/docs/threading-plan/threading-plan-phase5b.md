# Threading Plan — Phase 5b Verify (Dispatch with Shared Helper)

**Status:** Plan only. No code changes in this document.
**Scope:** Phase 5b (`verify::verify`). Compressed images get a producer-consumer pipeline; raw images stay on a single-threaded loop. Both call into a shared chunk-comparison helper.
**Prerequisite:** This plan **assumes Phase 4's threading plan has landed and soaked** (steps 1–3 of `threading-plan.md`). Specifically, it requires `unsafe impl Send for AlignedBuf {}` to be in place from Phase 4's Step 2. Implementers must not land this plan before Phase 4 has been deployed in production for at least one release tag without threading-related regressions.
**Author:** see commit history.
**Last updated:** 2026-05.

---

## TL;DR

Refactor `verify::verify` into:

1. A thin dispatcher that branches on `comp.is_compressed()`.
2. **`verify_serial`** — the existing serial loop, kept structurally as it is today. Routes raw images.
3. **`verify_pipelined`** — a new producer-consumer pipeline. Routes compressed images.
4. **`compare_chunk`** — a shared helper that both arms call, holding the byte-comparison and mismatch-diagnostic logic. Does **no** FD access — that stays on the main thread inside each arm.
5. **`verify_finalize`** — a shared post-loop helper: `progress.finish_and_clear()` + `println!()`. (No `fdatasync` because verify is read-only.)

The benefit is concentrated on compressed images. Verify reads are typically 1.5–2× faster than verify writes on the same USB device, so the saving from pipelining is somewhat smaller than for Phase 4: ~15–25% wall-clock improvement on USB 3.0 for xz/bzip2; <5% for gzip/zstd; near-zero for raw.

The shared helper structure mirrors Phase 4's plan exactly — same dispatch shape, same channel pattern, same cancel-mirror discipline, same drop-both-channel-halves cleanup, same panic propagation. **The architectural decisions made for Phase 4 carry over directly; this plan does not re-litigate them.**

What's _different_ from Phase 4:

- **The bottleneck flips.** In Phase 4, the writer (main thread) was the slow side (~30–50 ms USB write per chunk). In Phase 5b, the _worker_ (decompressor) is typically the slow side, because the device read is faster than the device write (~20–30 ms USB read per chunk vs ~50 ms decompress for xz, ~80 ms for bzip2). This affects performance estimates but NOT the architecture.
- **The helper does not touch the FD.** `compare_chunk` is a pure compute function: it takes two byte slices and an offset, compares, produces a diagnostic on mismatch. The device read happens in the caller (each arm), structured to overlap with worker decompression in the pipelined case.
- **No O_DIRECT toggling.** Verify runs with `O_DIRECT` cleared throughout (set once by the dispatcher before branching). Neither arm changes it during the loop. This is _simpler_ than Phase 4's tail-write O_DIRECT toggle.
- **No `fdatasync`.** Verify is read-only; nothing to flush.
- **Mismatch is a hard error.** Where Phase 4 propagates errors and lets `FlashGuard::Drop` print FATAL, verify's mismatch case is its OWN diagnostic (`bail!("verification mismatch at byte offset {abs}...")`). The helper produces this message; both arms surface it identically.

The change introduces:

- One spawned worker thread per `verify::verify` call when the input is compressed.
- Two `std::sync::mpsc` channels in the pipelined arm.
- Two `AlignedBuf` instances on the image-side buffer pool in the pipelined arm.
- One `AlignedBuf` for the device-read buffer (unchanged from today's serial verify).
- Local-cancel `Arc<AtomicBool>` plumbing in the pipelined arm only.

The change does **not** introduce:

- Any new `unsafe`. The `unsafe impl Send for AlignedBuf {}` was added in Phase 4's Step 2; this plan reuses it.
- Any threading on the device-read path. Only the main thread ever calls `read_exact_at` on the FD.
- Any threading on the comparison step. Comparison is on the main thread, between recv'ing the image buffer and returning it to the pool.
- Any threading in the `cooldown` function. Phase 5a is unchanged; it remains a single-threaded countdown sleep.
- Any compile-time feature flag. The dispatch is a runtime `if`.

---

## Why this plan exists separately from Phase 4's

A single combined plan was rejected for three reasons:

1. **Soak time matters.** Phase 4's threading lands first. Operators run it for a release tag; bugs surface (or don't); confidence builds. Only THEN does the same architectural pattern get applied to verify. Bundling them would mean shipping two threading-related changes simultaneously, doubling the risk surface.

2. **Verify's architecture has its own subtleties.** The bottleneck flips (worker is slow side, not main). The helper signature differs (no FD access). The mismatch diagnostic is verify-specific. These are small differences but warrant their own walkthrough.

3. **Verify can be threaded independently of Phase 4.** Even if a future architectural change rolls back Phase 4's pipelining (e.g., revert to serial-everything), verify could remain threaded — the `Send` impl, the channel pattern, and the helpers would all stand alone. The decoupling is real.

---

## Why threading helps for verify

### The bottleneck table (verify)

For one 4 MiB chunk (`BUF_SIZE`), per-stage time on typical hardware. Numbers are approximate; the _shape_ is what matters.

| Stage                                       | Time (NVMe source / USB 3.0 target) | CPU-bound?                                                                            |
| ------------------------------------------- | ----------------------------------- | ------------------------------------------------------------------------------------- |
| `read_exact_at` (USB 3.0, O_DIRECT off)     | ~15–25 ms                           | No (USB controller; reads typically 1.5–2× faster than writes on the same controller) |
| `read_exact_at` (USB 2.0)                   | ~70–100 ms                          | No                                                                                    |
| `fill_exact` (raw image — direct file read) | <1 ms                               | No                                                                                    |
| `fill_exact` (gzip)                         | ~2–5 ms                             | Yes (modest)                                                                          |
| `fill_exact` (zstd)                         | ~2–5 ms                             | Yes (modest)                                                                          |
| `fill_exact` (xz)                           | ~15–40 ms                           | **Yes — comparable to or exceeding the USB read**                                     |
| `fill_exact` (bzip2)                        | ~30–80 ms                           | **Yes — typically exceeds the USB read**                                              |
| Byte comparison (4 MiB memcmp)              | ~1–3 ms                             | Yes (cache-friendly)                                                                  |

Serial verify takes `read_dev + read_image + compare` per chunk. Pipelined verify takes `max(read_dev + compare, read_image)` per chunk. Savings are proportional to `min(read_dev + compare, read_image)`.

Concrete worked examples for a 1 GiB image (256 chunks of 4 MiB):

| Image type / target    | Serial estimate | Pipelined estimate | Saved    |
| ---------------------- | --------------- | ------------------ | -------- |
| xz on USB 3.0          | ~9–17 s         | ~6–10 s            | ~3–7 s   |
| bzip2 on USB 3.0       | ~12–27 s        | ~8–20 s            | ~4–7 s   |
| gzip / zstd on USB 3.0 | ~5–8 s          | ~5–7 s             | ~0.5–1 s |
| xz on USB 2.0          | ~22–35 s        | ~18–28 s           | ~4–7 s   |
| bzip2 on USB 2.0       | ~25–50 s        | ~18–28 s           | ~7–22 s  |
| gzip / zstd on USB 2.0 | ~18–28 s        | ~18–28 s           | ~0.5–1 s |

The savings are smaller than for Phase 4 because the device read is faster than the device write (so the "fixed" side of `max(read_dev, decompress)` is smaller, leaving less headroom for the decompression to absorb).

Raw images stay on the serial path and see no change.

### Why no other verify operation benefits

- `cooldown` is a wall-clock wait; threading defeats the purpose.
- `BLKFLSBUF` is a single blocking ioctl that takes from sub-millisecond up to tens of milliseconds (depending on how much page-cache state the kernel must invalidate). It runs once per verify call, can't be parallelized with anything else (the verify reads MUST come AFTER the cache invalidation, otherwise they'd return write-side cached pages). Threading does not help.
- `set_direct(fd, false)` is one fcntl. Same.
- Reopening the image is one open syscall + decompressor construction. Sub-millisecond. Same.

### Why the device FD must stay single-threaded (same as Phase 4)

`O_EXCL` on a Linux block device is held for the duration of Phase 5b (verify happens before `guard.into_file()` releases it). Concurrent `read_exact_at` calls from two threads would not be a kernel-level soundness violation (the kernel serializes them), but they would be an application-level correctness failure: the verify loop's offset bookkeeping assumes sequential reads.

The dispatch design enforces single-threaded FD access structurally: the worker thread in `verify_pipelined` holds an `&mut ImageReader` (for decompression) and never receives `&FlashGuard`. Only the main thread calls `read_exact_at`. The serial path has only one thread; the question doesn't arise.

---

## Architecture

### Component diagram

```
      ┌─────────────────────────────┐
      │       verify::verify        │
      │   (thin runtime dispatch)   │
      │   + once-only setup:        │
      │   BLKFLSBUF, O_DIRECT off,  │
      │   reopen ImageReader        │
      └──────────────┬──────────────┘
                     │
         comp.is_compressed()?
              │           │
            no│           │yes
              ▼           ▼
┌──────────────────┐  ┌──────────────────┐
│   verify_serial  │  │ verify_pipelined │
│  (current shape) │  │   (new threaded) │
└────────┬─────────┘  └────────┬─────────┘
         │                     │
         └──────────┬──────────┘
                    ▼
         ┌──────────────────────┐
         │    compare_chunk     │  ← shared compare-and-diagnose
         │  (byte-by-byte cmp,  │     (no FD access; takes two
         │   mismatch error)    │      slices and an offset; both
         └──────────────────────┘      arms call it the same way)
                    ▲
                    │ (called once per chunk
                    │  by either arm, after
                    │  filling both buffers)
         ┌──────────┴──────────┐
         │  verify_finalize    │  ← shared post-loop work
         │ (finish progress,   │     (called by both arms after
         │  println newline)   │      the verify loop succeeds)
         └─────────────────────┘
```

### Pipelined data-flow diagram

```
                   free_tx                          free_rx
   ┌──────────────────────────────────────────────────────┐
   │                                                      │
   ▼                                                      │
┌─────────────────────────┐                  ┌────────────┴──────────────┐
│  Worker thread          │                  │  Main thread (reader)     │
│                         │                  │                           │
│  loop:                  │                  │  loop:                    │
│    if remaining == 0:   │                  │    if remaining == 0:     │
│      return             │                  │      break (success)      │
│    img_buf ← free_rx    │                  │    if cancel: shutdown    │
│      .recv()            │                  │    read_exact_at(         │
│    if local_cancel:     │                  │      &mut dev_buf, off)   │
│      return             │                  │    (img_buf, n) ←         │
│    fill_exact(reader,   │                  │      filled_rx.recv()     │
│      &mut img_buf[..n]) │   filled_tx      │    compare_chunk(...)     │
│    filled_tx.send(      │ ───────────────▶ │      → Ok: bookkeeping,   │
│      Ok((img_buf, n)))  │                  │           pb.set_position │
│    remaining -= n       │                  │           return img_buf  │
│  on error:              │                  │      → Err: mismatch,     │
│    filled_tx.send(Err)  │                  │           bail            │
│    return               │                  │    throttle               │
└─────────────────────────┘                  └───────────────────────────┘

  (worker reads from ImageReader by-move. Reader contains a decompressor;
   ImageReader is `Read + Send`. Raw images do NOT go through this path.
   Main thread holds the FlashGuard and the dev_buf; worker has no FD
   access. Coordination by chunk-count: worker tracks remaining bytes
   from the bytes_written total passed in at spawn time, returns when
   it has produced the last chunk.)
```

### The shared helper: `compare_chunk`

This is the structural anchor of the dispatch design — but it is much smaller than Phase 4's `process_chunk` because it has no I/O work, only comparison.

```rust
// In src/verify.rs — new, private to the module.

/// Compare a single chunk of device bytes against image bytes. Returns
/// Err with a precise mismatch diagnostic if the slices differ.
///
/// This is the shared verify-loop invariant between `verify_serial` and
/// `verify_pipelined`. Any change to mismatch reporting, byte-by-byte
/// comparison, or first-diff-offset computation belongs here, fixed
/// once and benefiting both arms.
///
/// Does NOT do any I/O — the caller has already filled both buffers.
/// Does NOT update the progress bar — the caller does that after a
/// successful return.
///
/// Preconditions:
///   - `dev_bytes.len() == img_bytes.len() == this_chunk`.
///   - `dev_bytes` was just read from the device at `offset`.
///   - `img_bytes` was just read from the image at the corresponding
///     decompressed-stream offset.
///   - `offset` is the absolute byte offset into the device (used for
///     the mismatch diagnostic only).
///
/// Postconditions:
///   - On `Ok(())`: the bytes match.
///   - On `Err(_)`: the bytes differ; the error message contains the
///     absolute byte offset of the FIRST differing byte and the image
///     path (for operator diagnostics).
fn compare_chunk(
    dev_bytes: &[u8],
    img_bytes: &[u8],
    offset: u64,
    image_path: &Path,
) -> Result<()>;
```

Implementation sketch (preserves today's diagnostic message verbatim):

```rust
fn compare_chunk(
    dev_bytes: &[u8],
    img_bytes: &[u8],
    offset: u64,
    image_path: &Path,
) -> Result<()> {
    debug_assert_eq!(dev_bytes.len(), img_bytes.len(),
        "compare_chunk: caller must pre-equalize slice lengths");

    if dev_bytes == img_bytes {
        return Ok(());
    }

    // Mismatch — find the first differing byte for the diagnostic.
    let first_diff = dev_bytes
        .iter()
        .zip(img_bytes)
        .position(|(a, b)| a != b)
        .unwrap_or(0); // unreachable in practice — slices differ but
                       // position can return None only if zip is empty,
                       // and an empty slice can't differ. unwrap_or
                       // for defense-in-depth.
    let abs = offset + first_diff as u64;
    bail!(
        "verification mismatch at byte offset {abs}. The device may \
         be faulty, failing, or counterfeit. Image: {}",
        image_path.display()
    );
}
```

Key properties of this helper:

- **No I/O.** Pure compute. Easier to test (no mock FlashGuard needed for these unit tests).
- **No threading awareness.** Takes two slices; doesn't know whether they came from sync reads or channel transfers.
- **No cancellation handling.** Cancellation is the caller's job, checked between chunks.
- **No throttle.** Same as `process_chunk`: throttle sandwiches the helper.
- **No progress-bar updates.** The caller calls `pb.set_position(offset)` after a successful return.
- **Mismatch is fatal.** Returns `Err`; the caller propagates immediately (no recovery is meaningful — the device's contents differ from the image, the flash is invalid).
- **Length equality is a precondition, not an invariant the helper checks.** A `debug_assert_eq!` guards against caller bugs in test/debug builds; in release, lengths are trusted (Rust slice equality compares lengths first anyway, so a length mismatch would still surface as a comparison failure rather than UB).

### The shared post-loop helper: `verify_finalize`

```rust
/// Run the post-loop work that both arms perform identically:
///   1. progress.finish_and_clear() — clean up the bar.
///   2. println!() — emit a clean newline so the bar's last rendered
///      position doesn't blend with the next phase's text.
///
/// Note: no fdatasync (verify is read-only) and no set_direct toggle
/// (the dispatcher cleared O_DIRECT once at entry; nothing else changed
/// it).
fn verify_finalize(progress: &ProgressBar);
```

Implementation:

```rust
fn verify_finalize(progress: &ProgressBar) {
    progress.finish_and_clear();
    println!();
}
```

This helper is trivially small but its existence is structurally important: it gives the dispatch arms a single shared post-loop call site, so a future change (e.g., adding a "verification passed" log line) lives in one place.

### The serial arm: `verify_serial`

```rust
fn verify_serial(
    guard: &mut FlashGuard,
    mut reader: ImageReader,
    image_path: &Path,
    bytes_written: u64,
    throttle: Option<u64>,
    cancel: &AtomicBool,
) -> Result<()> {
    let pb = make_verify_pb(bytes_written);
    pb.reset_elapsed();

    let mut dev_buf = AlignedBuf::new();
    let mut img_buf = AlignedBuf::new();
    // Note: today's serial verify uses Vec<u8> for img_buf. This plan
    // switches to AlignedBuf for parity with the pipelined arm (the
    // pipelined arm uses AlignedBuf because the channel-passed buffers
    // already have the Send impl from Phase 4). Functionally identical
    // for verify (O_DIRECT is off so alignment is irrelevant); using
    // AlignedBuf lets compare_chunk's signature accept &[u8] from both
    // buffer types without conversion.

    let chunk_target_nanos = throttle.map(|rate_bps| {
        let ideal = (BUF_SIZE as u128).saturating_mul(1_000_000_000)
            / rate_bps as u128;
        u64::try_from(ideal).unwrap_or(u64::MAX)
    });

    let mut remaining: u64 = bytes_written;
    let mut offset: u64 = 0;

    while remaining > 0 {
        if cancel.load(Ordering::SeqCst) {
            pb.abandon();
            bail!("cancelled by user");
        }

        let start = chunk_target_nanos.map(|_| Instant::now());

        let this_chunk = remaining.min(BUF_SIZE as u64) as usize;

        // 1. Read from device.
        guard
            .file()
            .read_exact_at(&mut dev_buf.as_mut_slice()[..this_chunk], offset)
            .with_context(|| format!(
                "reading {this_chunk} bytes from device at offset {offset}"
            ))?;

        // 2. Read from image.
        fill_exact(&mut reader, &mut img_buf.as_mut_slice()[..this_chunk])
            .with_context(|| format!(
                "reading {this_chunk} bytes from image stream"
            ))?;

        // 3. Compare (shared helper).
        compare_chunk(
            &dev_buf.as_slice()[..this_chunk],
            &img_buf.as_slice()[..this_chunk],
            offset,
            image_path,
        ).map_err(|e| { pb.abandon(); e })?;

        remaining -= this_chunk as u64;
        offset += this_chunk as u64;
        pb.set_position(offset);

        // 4. Throttle.
        if let (Some(target_ns), Some(t0)) = (chunk_target_nanos, start) {
            let elapsed_ns = u64::try_from(t0.elapsed().as_nanos())
                .unwrap_or(u64::MAX);
            if let Some(residual) = target_ns.checked_sub(elapsed_ns) {
                cancellable_sleep(Duration::from_nanos(residual), cancel);
            }
        }
    }

    verify_finalize(&pb);
    Ok(())
}
```

This is structurally identical to today's verify body, with three changes:

1. The byte-comparison and mismatch-diagnostic block is replaced by a single call to `compare_chunk`.
2. The post-loop work is replaced by a single call to `verify_finalize`.
3. `img_buf` is now an `AlignedBuf` (was `Vec<u8>`). Functionally identical for the serial arm; this is a cosmetic change that lets `compare_chunk` take `&[u8]` from both `AlignedBuf::as_slice()` call sites.

Everything else — `BLKFLSBUF`, `set_direct(fd, false)`, image reopen — is now in the dispatcher (called once for both arms), not duplicated here.

The `.map_err(|e| { pb.abandon(); e })?` on the `compare_chunk` call ensures that on mismatch, the bar is abandoned before the error propagates — preserving today's "leave the bar on screen as a record" behavior. (Today's code does `pb.abandon()` then `bail!(...)`; the helper-extraction means the bail happens inside `compare_chunk`, so we have to do the abandon at the call site.)

### The pipelined arm: `verify_pipelined`

```rust
fn verify_pipelined(
    guard: &mut FlashGuard,
    reader: ImageReader,
    image_path: &Path,
    bytes_written: u64,
    throttle: Option<u64>,
    cancel: &AtomicBool,
) -> Result<()> {
    let pb = make_verify_pb(bytes_written);
    pb.reset_elapsed();

    let chunk_target_nanos = throttle.map(|rate_bps| {
        let ideal = (BUF_SIZE as u128).saturating_mul(1_000_000_000)
            / rate_bps as u128;
        u64::try_from(ideal).unwrap_or(u64::MAX)
    });

    // Construct cancel mirror.
    let local_cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = local_cancel.clone();

    // Construct channels and seed the buffer pool with two AlignedBufs
    // for the image-side. The worker fills them; main consumes and
    // returns them. The dev_buf stays on the main thread (no channel
    // for it — it's reused in place).
    let (filled_tx, filled_rx) = mpsc::channel::<FilledItem>();
    let (free_tx, free_rx) = mpsc::channel::<AlignedBuf>();
    free_tx.send(AlignedBuf::new())
        .expect("seed buffer 0: free_rx still in scope");
    free_tx.send(AlignedBuf::new())
        .expect("seed buffer 1: free_rx still in scope");

    // Spawn the worker. `reader`, `worker_cancel`, `filled_tx`, `free_rx`,
    // and `bytes_written` are all moved into the closure. The worker
    // tracks how many bytes it has produced so it knows when to exit
    // (last chunk reached).
    let worker_handle: thread::JoinHandle<()> = thread::spawn(move || {
        verify_worker_loop(reader, bytes_written, worker_cancel,
                           filled_tx, free_rx);
    });
    let mut worker_handle_taken: Option<thread::JoinHandle<()>> =
        Some(worker_handle);

    let mut dev_buf = AlignedBuf::new();
    let mut remaining: u64 = bytes_written;
    let mut offset: u64 = 0;
    let mut outcome: Result<()> = Ok(());
    let mut worker_panic: Option<Box<dyn std::any::Any + Send>> = None;

    'verify_loop: loop {
        if remaining == 0 {
            // All bytes verified. Exit cleanly.
            break 'verify_loop;
        }

        // 1. Cancel check.
        if cancel.load(Ordering::SeqCst) {
            local_cancel.store(true, Ordering::SeqCst);
            outcome = Err(anyhow!("cancelled by user"));
            break 'verify_loop;
        }

        let start = chunk_target_nanos.map(|_| Instant::now());

        let this_chunk = remaining.min(BUF_SIZE as u64) as usize;

        // 2. Read from device. This runs in parallel with the worker's
        //    decompression of the next chunk — that's where the
        //    pipelining benefit comes from.
        if let Err(e) = guard.file().read_exact_at(
            &mut dev_buf.as_mut_slice()[..this_chunk],
            offset,
        ) {
            outcome = Err(anyhow::Error::from(e).context(format!(
                "reading {this_chunk} bytes from device at offset {offset}"
            )));
            break 'verify_loop;
        }

        // 3. Receive next image-side filled buffer.
        let (img_buf, img_filled) = match filled_rx.recv() {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => { outcome = Err(e); break 'verify_loop; }
            Err(_) => {
                // Channel disconnected. Distinguish clean EOF from panic.
                match worker_handle_taken.take().unwrap().join() {
                    Ok(()) => {
                        // Worker exited cleanly but main still expected
                        // more bytes — this is a contract violation
                        // (worker should have produced exactly bytes_written
                        // bytes before exiting). Treat as truncation.
                        outcome = Err(anyhow!(
                            "image stream ended before {remaining} expected \
                             bytes (worker exited prematurely)"
                        ));
                        break 'verify_loop;
                    }
                    Err(panic) => {
                        worker_panic = Some(panic);
                        break 'verify_loop;
                    }
                }
            }
        };

        // Sanity: worker must have filled exactly this_chunk bytes.
        // If it filled fewer, something is wrong with the worker's
        // chunk-size accounting.
        if img_filled != this_chunk {
            outcome = Err(anyhow!(
                "worker produced {} bytes for chunk at offset {}, expected {}",
                img_filled, offset, this_chunk
            ));
            drop(img_buf);
            break 'verify_loop;
        }

        // 4. Compare (shared helper).
        if let Err(e) = compare_chunk(
            &dev_buf.as_slice()[..this_chunk],
            &img_buf.as_slice()[..this_chunk],
            offset,
            image_path,
        ) {
            outcome = Err(e);
            drop(img_buf);
            break 'verify_loop;
        }

        // 5. Bookkeeping; return img_buf to the pool.
        remaining -= this_chunk as u64;
        offset += this_chunk as u64;
        pb.set_position(offset);
        // Send is best-effort; if worker has exited (last chunk just
        // received), free_rx is dropped and send returns Err harmlessly.
        let _ = free_tx.send(img_buf);

        // 6. Throttle. Same pattern as Phase 4: sleep for the residual,
        //    re-check cancel after sleep so we don't block on the next
        //    recv if cancel fired during sleep.
        if let (Some(target_ns), Some(t0)) = (chunk_target_nanos, start) {
            let elapsed_ns = u64::try_from(t0.elapsed().as_nanos())
                .unwrap_or(u64::MAX);
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

    // Single cleanup block — runs on EVERY exit path. Same discipline
    // as Phase 4's flash_pipelined: drop BOTH channel halves before
    // joining, because the worker may be blocked on either send or recv.
    //
    // 1. Drop filled_rx so any pending worker send fails.
    drop(filled_rx);
    // 2. Drop free_tx so a worker blocked on free_rx.recv() also exits.
    drop(free_tx);
    // 3. Join the worker (if not already joined in the disconnect branch).
    if let Some(handle) = worker_handle_taken.take() {
        let _ = handle.join();
    }
    // 4. If the worker panicked, re-raise now (after cleanup is complete).
    //    No FlashGuard FATAL message here — verify is read-only, the
    //    device contents are unchanged. A panic in the verify pipeline
    //    means we couldn't verify, not that we corrupted the disk.
    if let Some(panic) = worker_panic {
        std::panic::resume_unwind(panic);
    }

    // 5. If the loop set an error outcome (cancel, mismatch, read error,
    //    worker error), abandon the progress bar and propagate.
    if outcome.is_err() {
        pb.abandon();
        outcome?;
    }

    // 6. Success path — finalize.
    verify_finalize(&pb);
    Ok(())
}
```

### The worker loop: `verify_worker_loop`

```rust
fn verify_worker_loop(
    mut reader: ImageReader,
    mut remaining: u64,
    worker_cancel: Arc<AtomicBool>,
    filled_tx: mpsc::Sender<FilledItem>,
    free_rx: mpsc::Receiver<AlignedBuf>,
) {
    while remaining > 0 {
        // 1. Acquire a fresh buffer from the pool.
        let mut buf = match free_rx.recv() {
            Ok(b) => b,
            Err(_) => return,  // main has shut down
        };

        // 2. Cancel check (mirror set by main thread on parent-flag).
        if worker_cancel.load(Ordering::SeqCst) {
            return;
        }

        // 3. Determine this chunk's size.
        let this_chunk = remaining.min(BUF_SIZE as u64) as usize;

        // 4. Fill exactly this_chunk bytes from the ImageReader.
        //    fill_exact retries Interrupted, propagates other errors,
        //    and returns UnexpectedEof if the stream truncates.
        if let Err(e) = fill_exact(&mut reader, &mut buf.as_mut_slice()[..this_chunk]) {
            let wrapped = anyhow::Error::from(e)
                .context("reading from compressed image stream during verify");
            let _ = filled_tx.send(Err(wrapped));
            return;
        }

        // 5. Send the filled buffer (with its byte count) to main.
        if filled_tx.send(Ok((buf, this_chunk))).is_err() {
            return;  // main has dropped filled_rx
        }

        // 6. Update remaining; exit cleanly when last chunk delivered.
        remaining -= this_chunk as u64;
    }
    // remaining == 0 — last chunk has been delivered. Worker exits.
}
```

The verify worker has one structural difference from Phase 4's worker: it **knows how many bytes it owes** (received as `remaining` at spawn time). It exits cleanly when that count reaches zero, rather than sensing EOF via "filled < BUF_SIZE." This matches verify's contract — `bytes_written` was computed in Phase 4 as the exact number of bytes that were flashed, and the verify worker must produce exactly that many.

If the image stream truncates before `bytes_written` (e.g., the operator's image file got corrupted or replaced between flash and verify), `fill_exact` returns `UnexpectedEof` from inside the worker, which gets wrapped and sent on `filled_tx` as Err. Main propagates with the appropriate context. ✓

### The dispatcher: `verify::verify`

```rust
pub fn verify(
    guard: &mut FlashGuard,
    image_path: &Path,
    comp: Compression,
    bytes_written: u64,
    throttle: Option<u64>,
    cancel: &AtomicBool,
) -> Result<()> {
    // Edge case: zero bytes written. Nothing to verify; return Ok
    // without touching the FD or spawning a worker.
    //
    // Behaviour change vs today: the current verify::verify calls
    // BLKFLSBUF, set_direct, and ImageReader::open even for
    // bytes_written == 0 (the loop simply doesn't enter). These are
    // wasted on a zero-byte verify and the early-return is cleaner;
    // for an operator-visible difference, the zero-byte verify path
    // now skips a sub-millisecond ioctl and an unnecessary image-file
    // open. No semantic change to a non-zero verify.
    if bytes_written == 0 {
        return Ok(());
    }

    // Setup that runs ONCE for both arms:
    // 1. Invalidate the kernel's buffer cache so reads come from NAND.
    //    SAFETY: guard owns a valid, currently-open file descriptor for
    //    the block device. BLKFLSBUF takes no argument and has no
    //    side effects on memory allocated to this process.
    unsafe {
        ioctl::blkflsbuf(guard.as_raw_fd())
            .context("BLKFLSBUF (flush kernel buffer cache)")?;
    }

    // 2. Ensure O_DIRECT is off — verification reads use the page cache
    //    so we don't have to align the trailing-chunk read to a sector
    //    boundary.
    set_direct(guard.as_raw_fd(), false)
        .context("disabling O_DIRECT for verify")?;

    // 3. Reopen the image — decompressors cannot be rewound from where
    //    Phase 4 left them.
    let reader = ImageReader::open(image_path, comp)
        .context("reopening image for verify")?;

    // 4. Dispatch.
    if comp.is_compressed() {
        verify_pipelined(guard, reader, image_path, bytes_written, throttle, cancel)
    } else {
        verify_serial(guard, reader, image_path, bytes_written, throttle, cancel)
    }
}
```

The dispatcher does the once-only setup that both arms need (BLKFLSBUF, O_DIRECT clear, image reopen), then branches. Its public signature is unchanged from today; callers in `main.rs` need no edits.

### Send-bounds story

Same as Phase 4: `AlignedBuf: Send` (added in Phase 4's Step 2), `ImageReader: Send` automatically (each variant contains `Send` types). No new `unsafe impl` is needed for verify.

### Channels and ownership

Same shape as Phase 4:

- **`filled_tx: Sender<FilledItem>` / `filled_rx: Receiver<FilledItem>`** — worker → main. Carries filled image-side buffers and worker errors. `FilledItem` is `anyhow::Result<(AlignedBuf, usize)>`.
- **`free_tx: Sender<AlignedBuf>` / `free_rx: Receiver<AlignedBuf>`** — main → worker. Returns drained image-side buffers for refill.

`FilledItem` is defined as a private `type` alias inside `verify.rs`, separate from Phase 4's. They happen to have the same definition — `anyhow::Result<(AlignedBuf, usize)>` — but defining them separately keeps each module self-contained. (Alternative: hoist `FilledItem` to a shared crate-level module; not worth the indirection for one type alias used in two places.)

The pool is seeded with two `AlignedBuf::new()` values before spawning the worker. Same 2-buffer rationale as Phase 4: 2 enables pipelining with a single in-flight chunk plus one being processed.

The `dev_buf` stays on the main thread and does NOT go through any channel. There's only one of it; main reuses it in place. Verify reads from the device into this single buffer; the buffer is consumed by `compare_chunk` in the same iteration; it's reused next iteration.

### Cancellation propagation

Same two-mechanism design as Phase 4:

1. **Channel disconnects (primary):** Drop both `filled_rx` and `free_tx` in cleanup → worker's pending recv/send returns Err → worker exits. Sufficient on its own for correctness.

2. **Cancel mirror (latency optimization):** `Arc<AtomicBool>` cloned to worker; main mirrors parent flag into it before breaking. Worker checks mirror after a successful `free_rx.recv()`, before `fill_exact`. Lets the worker bail out of an iteration before doing a wasted ~80 ms `fill_exact` on bzip2.

The serial arm just checks the parent `cancel: &AtomicBool` directly at the top of each iteration, same as Phase 4's serial arm.

**Memory ordering:** `SeqCst` for clarity over performance. Same trade-off as Phase 4; same reasoning.

### Error propagation

Four error-source paths in the pipelined arm:

- **Worker error** (decompressor I/O fault, including truncation as `UnexpectedEof`): worker sends `Err(e)` on `filled_tx` with `.context("reading from compressed image stream during verify")` and returns. Main receives `Ok(Err(e))`, breaks loop, propagates after cleanup.

- **Device-read error** (USB I/O fault, EIO on flaky device): main's `read_exact_at` returns Err. Main sets `outcome` and breaks. Cleanup drops both channels; worker's next send/recv fails; worker exits. Main joins, propagates the error.

- **Mismatch**: `compare_chunk` returns `Err`. Main sets `outcome`, drops `img_buf`, breaks. Same cleanup. Main joins, propagates the mismatch diagnostic.

- **Cancellation**: same as Phase 4 — main mirrors to local_cancel, drops channels, worker exits, main propagates `Err("cancelled by user")`.

The serial arm has three error paths (no worker channel disconnect): device-read error, image-read error, mismatch. Each propagated immediately via `?`.

The order of channel drops at main shutdown is precise: drop both main-side endpoints (`filled_rx` and `free_tx`) before joining. The first unblocks any worker that's pending in `filled_tx.send(...)`; the second unblocks any worker that's blocked in `free_rx.recv()`. Without both drops, `join()` can deadlock. Same critical invariant as Phase 4.

### Joining the worker thread

Same discipline as Phase 4: the cleanup block joins on every exit path; the channel-disconnect branch may have already taken the handle (capturing a panic into `worker_panic`); the post-cleanup `resume_unwind` rethrows any captured panic.

**Difference from Phase 4 in panic handling**: Phase 4's cleanup mentions that during a worker panic, `FlashGuard::Drop` runs and prints FATAL because the destructive pipeline panicked mid-flash. **For verify, there is no destructive window.** A worker panic during verify means we couldn't verify, not that we corrupted the disk. The panic propagates up through `verify::verify`, then up through `main.rs`, where it surfaces as a normal Rust panic. The operator sees a stack trace, the disk's data is unchanged from when verify started.

This is the right behavior: verify is non-destructive, so a verify failure (whether by panic or by mismatch) does not warrant a FATAL warning. The operator should retry — perhaps with a different image file, or a different USB port, or a different stick.

### Writer-loop and worker-loop discipline

Same two structural rules as Phase 4 (re-stated for verify):

1. **No `return` inside `'verify_loop`.** Every exit goes through `break 'verify_loop` to the cleanup block. A bare `return` skips channel cleanup and the worker join.

2. **`drop(filled_rx)` AND `drop(free_tx)` before joining.** Same deadlock-avoidance reasoning as Phase 4: a worker blocked on `filled_tx.send(...)` is unblocked only by dropping `filled_rx`; a worker blocked on `free_rx.recv()` is unblocked only by dropping `free_tx`. Both states are reachable in practice (the second occurs on error paths where main drops `img_buf` without returning it to the pool — the mismatch path is one example).

The serial arm has no analogous disciplines.

### O_DIRECT invariants

**Verify does NOT use O_DIRECT.** The dispatcher clears it once at entry (matching today's behavior). Neither arm changes it during the loop. The `AlignedBuf`s are still aligned, but the alignment is unused — reads go through the page cache.

This is _simpler_ than Phase 4, where `process_chunk` had to toggle O_DIRECT for the tail write. Verify has no equivalent toggle. The plan does NOT require any O_DIRECT-related logic in `compare_chunk` or in either arm.

### `AlignedBuf: Send` — already in place

This plan REUSES the `unsafe impl Send for AlignedBuf {}` added in Phase 4's Step 2. No new unsafe is introduced. The compile-time `aligned_buf_is_send` test from Phase 4 covers verify's use of cross-thread `AlignedBuf` ownership transfer — the test asserts the Send impl exists; both Phase 4 and Phase 5b rely on the same impl.

### Throttle behaviour

Same wall-clock-rate enforcement as Phase 4. The throttle measures per-iteration wall-clock time, which includes any time main spends blocked on `filled_rx.recv()` waiting for the worker:

- **Serial arm**: `elapsed = read_exact_at + fill_exact + compare ≈ 20 + 50 + 1 = 71 ms` per chunk on bzip2/USB-3.0. ~21 ms on gzip/USB-3.0.
- **Pipelined arm (worker bottleneck case, xz/bzip2)**: `elapsed = max(read_exact_at, worker_fill) + compare ≈ max(20, 50) + 1 = 51 ms` per chunk (main blocks on recv while worker finishes its chunk). The 20 ms read time is hidden inside the worker's 50 ms.
- **Pipelined arm (main bottleneck case, gzip/zstd)**: `elapsed = read_exact_at + compare ≈ 20 + 1 = 21 ms` per chunk (worker finishes well before main is ready to recv).

The throttle subtracts `elapsed` from `target_ns` and sleeps the residual. As long as `elapsed < target_ns`, the throttle enforces a stable wall-clock rate regardless of codec. If `elapsed > target_ns` (operator set throttle below the natural rate), the `checked_sub` returns `None` and no sleep happens — the verify runs as fast as it can, which is appropriate.

A subtle observation: at the same throttle rate, the pipelined gzip arm sleeps LONGER per chunk than the pipelined bzip2 arm (because elapsed is smaller for gzip). This is correct — both end up pacing at the same wall-clock rate.

### Progress bar

Same structure as Phase 4: `make_verify_pb(bytes_written)` constructs the bar (already a project helper); `pb.reset_elapsed()` immediately before the loop; `pb.set_position(offset)` after each successful chunk; `pb.abandon()` on cancel/mismatch/error; `verify_finalize` does `finish_and_clear` + `println!()` on success.

### Performance characteristics

The same per-chunk and per-flash threading-overhead numbers from Phase 4's plan apply (channel ops, atomics, context switches, thread spawn). Repeating the table for verify:

**Per-chunk overhead (pipelined arm only):** total well under 1 µs against ~20–80 ms of useful work, ~0.001%.

**Per-flash (per-verify) overhead:** dominated by buffer allocation (~1–10 ms for `AlignedBuf::new()` × 2 for the serial arm, or × 3 for the pipelined arm; each call invokes `std::alloc::alloc_zeroed` against an aligned `Layout`, which is the page-zeroing cost for 4 MiB plus the alignment overhead); actual _threading_ overhead under 1 ms.

**Memory:**

- Serial arm: 1× `AlignedBuf` for `dev_buf` (4 MiB) + 1× `AlignedBuf` for `img_buf` (4 MiB) + the existing `BufReader<File>` (project uses `IMG_BUFREAD_CAP = 2 MiB`). Total: ~10 MiB. (Note: today's serial verify uses Vec<u8> for `img_buf`, which is also 4 MiB. No size change; just changing from heap-Vec to heap-`AlignedBuf`.)
- Pipelined arm: 1× `dev_buf` AlignedBuf (4 MiB) + 2× image-side `AlignedBuf` (8 MiB) + worker thread stack (~2 MiB Rust default) + `BufReader<File>` inside worker (2 MiB). Total: ~16 MiB.

Both fit comfortably. The pipelined arm uses 6 MiB more than the serial arm — large enough to spill out of typical L2 caches (256 KiB–1 MiB) but well within L3 (4–32 MiB) on modern CPUs, and trivial against the operator-class machine's main memory. The cache effect on the per-chunk byte comparison is minor: each 4 MiB compare touches both buffers sequentially, so even with L2 misses on every cache-line, the memcmp throughput stays in the multi-GiB/s range — comparison is not the verify bottleneck.

### Single-core and constrained-CPU systems

Same compatibility story as Phase 4: POSIX threads work on any kernel `imi` already requires. On single-core hardware, the pipelined arm still works but the speedup is roughly halved (the worker and main can't run in parallel; they time-slice).

| Image type / target    | Multi-core saving | Single-core saving |
| ---------------------- | ----------------- | ------------------ |
| xz on USB 3.0          | ~15–25%           | ~10–15%            |
| bzip2 on USB 3.0       | ~15–30%           | ~10–20%            |
| gzip / zstd on USB 3.0 | <5%               | <3%                |

Raw images go through `verify_serial` regardless and see no change on any hardware.

---

## Files to change

### Required source changes

**`src/aligned.rs`** — no changes. The `Send` impl from Phase 4's Step 2 is already in place; the compile-time `aligned_buf_is_send` test from Phase 4 already guards the impl.

**`src/verify.rs`** — the substantive change. Add five new functions and rewrite the existing one as a dispatcher:

1. **`compare_chunk(...) -> Result<()>`** — new shared helper, ~20 lines. Pure compute; no FD access.
2. **`verify_finalize(...)`** — new shared post-loop helper, ~5 lines. No `Result` (cannot fail).
3. **`verify_serial(...) -> Result<()>`** — new function, ~60 lines. Body is structurally identical to today's `verify::verify` post-setup loop, with the comparison block replaced by `compare_chunk` and post-loop work replaced by `verify_finalize`.
4. **`verify_pipelined(...) -> Result<()>`** — new function, ~130 lines including channel setup, worker spawn, the labelled-break main loop, and cleanup. Slightly longer than `flash_pipelined` because verify's loop has both a device-read AND a recv per iteration (vs Phase 4's single recv).
5. **`verify_worker_loop(...)`** — new function, ~35 lines. Called from `thread::spawn` in `verify_pipelined`. Tracks remaining bytes itself (does not sense EOF the way Phase 4's worker does).
6. **`verify::verify`** — rewritten as a dispatcher (~25 lines including the once-only setup: `bytes_written == 0` early return, `BLKFLSBUF`, `set_direct(fd, false)`, `ImageReader::open`, branch on `comp.is_compressed()`).

Helper types added: `type FilledItem = anyhow::Result<(AlignedBuf, usize)>` (private to `verify` module).

Helper functions preserved unchanged: `fill_exact`, `make_verify_pb`. Functions imported from `flash` unchanged: `cancellable_sleep`, `set_direct`, `UNIFIED_BAR_TEMPLATE` (already used).

New imports required at the top of `src/verify.rs` (current imports cover most of what's needed):

```rust
use std::sync::{Arc, mpsc};
use std::thread;
// std::panic::resume_unwind is referenced via its full path in the
// pipelined arm; no `use` needed.
// anyhow! macro is needed for the pipelined arm's error construction;
// add it to the existing `use anyhow::{bail, Context, Result};` line:
use anyhow::{anyhow, bail, Context, Result};
```

Approximate net change in `src/verify.rs`: +280 lines, -75 lines (the existing verify body that's now split between dispatcher setup, `verify_serial`, and `compare_chunk`).

### Documentation changes

**`AGENTS.md`** — extend the threading directive added by Phase 4 to mention Phase 5b:

> Threading is permitted in Phase 4's pipelined arm AND Phase 5b's pipelined arm. Both follow the same architecture: dispatch on `comp.is_compressed()`; raw images route through the serial arm; compressed images route through a pipelined arm that spawns a worker thread for decompression. The device FD is held under `O_EXCL` throughout both phases; only the main thread must ever call `pwrite` (Phase 4) or `read_exact_at` (Phase 5b) on it. Workers receive only `&mut [u8]` slices and never receive `&FlashGuard`. Shared helpers (`process_chunk` for Phase 4, `compare_chunk` for Phase 5b) are called from the main thread in both arms; per-phase loop-semantic changes belong there.

**`.agents/docs/06-phase-5-verify.md`** — extend with two new subsections (paralleling the additions made for Phase 4):

- **"Dispatch and shared helper"** — describe the dispatch on `comp.is_compressed()` and the role of `compare_chunk` / `verify_finalize`. Include the component diagram from this plan.
- **"Pipelined arm"** — describe the channel design, the cancel-mirror pattern, the join-on-every-exit-path discipline, the worker-panic propagation. Include the data-flow diagram from this plan. Note explicitly that verify panics do NOT trigger `FlashGuard::Drop` FATAL (verify is read-only).

The existing "Why 10 seconds cooldown" section is preserved verbatim; cooldown is unchanged.

**`README.md`** — extend the "Compressed images" subsection added by Phase 4 with one sentence:

> The verify phase (post-flash byte-for-byte readback) uses the same pipelining strategy: decompression runs on a worker thread while the device read runs on the main thread. Verify saves ~15–25% wall-clock for xz/bzip2 on USB 3.0; gzip/zstd see ~1 second on a 1 GiB image. Raw images verify on the serial path.

### Cargo.toml

No changes. The decompressor dep comments added by Phase 4 already cover verify (same decoders, same constraints).

### Out of scope for this commit

- **Threading the device read against the comparison (a "reader" thread).** This is worth a careful walkthrough because the intuition "we could save time by overlapping read N+1 with compare N" is appealing.

  Consider the steady-state per-iteration timing in the pipelined arm. Three thread-bound operations are in flight:
  - Main: `read_exact_at` (~20 ms USB 3.0), then `recv` (~free if worker has chunk ready), then `compare` (~1-3 ms), then `free_tx.send` (~free).
  - Worker: `fill_exact` (~5 ms gzip, ~30 ms xz, ~50 ms bzip2).

  Main's per-iteration wall time: ~21-23 ms (read + compare; recv and send are free).
  Worker's per-iteration wall time: ~5 ms (gzip), ~30 ms (xz), ~50 ms (bzip2).

  **Per-chunk critical path in the pipelined arm is `max(main_work, worker_work)`.** For xz and bzip2, worker > main, so worker is the bottleneck. For gzip and zstd, main > worker.

  Now consider adding a third thread that reads the next chunk while main compares:
  - With third thread: per-iteration = `max(read_alone, recv + compare + send)` ≈ `max(20, 1)` = 20 ms (for the main thread's responsibilities, with read moved to its own thread).
  - But the OVERALL per-iteration is STILL `max(read_thread, main_thread, worker_thread)`. For xz: `max(20, 1, 30) = 30 ms` — no change vs the current `max(21, 30) = 30 ms`. The saving is zero on the worker-bottleneck cases.
  - For gzip: `max(20, 1, 5) = 20 ms` vs current `max(21, 5) = 21 ms`. Saving: 1 ms/chunk.

  **Per-image savings:**
  - 1 GiB gzip: 256 × 1 ms = ~256 ms saved.
  - 4 GiB gzip: 1024 × 1 ms ≈ 1 s saved.
  - 16 GiB gzip: 4096 × 1 ms ≈ 4 s saved.
  - 1 GiB xz/bzip2: zero saved (worker is bottleneck).
  - 16 GiB xz/bzip2: zero saved (worker is bottleneck).

  The benefit exists only when the worker decompressor is faster than the device read. That's the gzip/zstd case. For xz/bzip2 — the formats most likely to be used on large OS images — the optimization saves nothing because the worker thread is already on the critical path.

  **Cost of a reader thread:**
  - Another spawned thread with its own panic/cleanup discipline.
  - A second pair of mpsc channels (free/filled for dev_bufs).
  - A second cancel-mirror Arc clone.
  - Two additional `drop` calls in the cleanup block.
  - The cleanup block's complexity roughly doubles (must now coordinate two workers, distinguishing panic in reader vs panic in decompressor).
  - ~80 additional lines of code and a noticeable expansion of the test surface (need to test reader-thread shutdown, reader-thread panic, reader-error vs decompressor-error disambiguation).

  **A cheaper alternative: kernel read-ahead via `posix_fadvise`.** Linux exposes `posix_fadvise(fd, offset, length, POSIX_FADV_WILLNEED)` to hint to the kernel that the application will need a specific page range soon. The kernel readahead daemon then issues asynchronous read requests, populating the page cache before the application's `read_exact_at` syscall. The result is similar to a userspace reader thread but with the I/O work running on a kernel thread, no userspace thread management, no channel discipline. Main thread call sequence: `posix_fadvise(WILLNEED)` for chunk N+1 (returns immediately, ~100 ns) → `recv` chunk N from worker → `compare` chunk N → `read_exact_at` chunk N+1 (now likely served from page cache, ~5 ms instead of ~20 ms).

  Even `posix_fadvise` is a sub-percent optimization on the cases where it would matter, and on USB devices the kernel readahead window may not be honored aggressively (block-device-readahead is queue-depth-limited). The complexity-to-saving ratio still doesn't favor adding it now.

  **Decision: out of scope for this commit, with the option preserved.** If after Phase 5b lands a benchmark shows that gzip/zstd verify on USB 3.0 with multi-GiB images is operator-visibly slow (e.g., >30 seconds on a 10 GiB raw-equivalent gzip), the `posix_fadvise` approach is the first thing to try. The reader-thread approach is the second option, and should only be pursued if `posix_fadvise` proves insufficient.

- **Async/await with Tokio.** Same rejection rationale as Phase 4 — the work is synchronous CPU and synchronous I/O, both halves end up on a Tokio blocking pool, no benefit.

- **Multi-worker decompression for verify.** Same as Phase 4 — if the worker is the bottleneck (which it often is for verify), in principle parallel decompression could help. But each decompressor is single-stream by design (gzip/xz/bzip2 are not chunkable without re-design), and parallel decoder libraries (e.g. `rust-zstd`'s parallel feature) would conflict with the single-worker model and require re-doing the threading analysis. Explicitly out of scope.

- **Routing raw images through the pipelined arm.** Same rejection as Phase 4 — raw `fill_exact` is sub-1 ms per chunk; pipelining recovers ~1 ms/chunk while adding thread-spawn overhead. Net negative. Explicitly out of scope.

---

## Implementation order

A merge-able sequence with intermediate checkpoints. Each step is independently committable; do not collapse them.

### Step 1: Extract `compare_chunk` and `verify_finalize` from current serial code

**Pure refactor.** Replace the byte-comparison block inside today's `verify::verify` (lines 124–137 of `src/verify.rs` in the delivery state) with a call to a new private `compare_chunk` function that contains the same logic. Replace the post-loop `pb.finish_and_clear()` + `println!()` calls with a call to `verify_finalize`. Switch `img_buf` from `Vec<u8>` to `AlignedBuf` so `compare_chunk` can take `&[u8]` from both buffers symmetrically. The verify body otherwise stays exactly as it is — single-threaded, single image-side buffer. No behavioural change.

This step lands a cleaner factoring of `verify::verify` even if Step 2 is later abandoned.

Acceptance: tests V1–V4 from the _Tests_ section land in this commit and pass. `cargo test` passes 101 tests total (97 from after Phase 4 + 4 new). `cargo clippy` produces no new lints; the `imi-review` skill produces no new findings.

### Step 2: Add `verify_pipelined`, `verify_worker_loop`, and dispatch

**The substantive commit.** Add `verify_pipelined`, `verify_worker_loop`, and the local `FilledItem` type alias. Move the once-only setup block (`BLKFLSBUF`, `set_direct(fd, false)`, image reopen) from inside the verify body into the new dispatcher. Move the post-setup loop body into a new private function `verify_serial` with the appropriate signature. Then rewrite `verify::verify` as a dispatcher that calls either `verify_serial` or `verify_pipelined` based on `comp.is_compressed()`. The body of `verify_serial` should be a pure code-motion of what `verify::verify` was at end-of-Step-1, with the once-only setup pulled into the dispatcher. Add the `bytes_written == 0` early return at the top of the dispatcher.

Add the unit tests V5–V11 described in _Tests_. Update `AGENTS.md`, the Phase-5 doc, and `README.md` in the same commit.

Run `cargo test`. Run `cargo +1.85 build --locked` (MSRV check). **Hardware-test before merge** (see _Required hardware tests_).

Acceptance: tests V5–V11 land in this commit. `cargo test` passes ≥108 tests (101 from after Step 1 + 7 new items in Step 2; the actual `#[test]` count may be slightly higher because some items have sub-tests). The dispatcher routes correctly; both arms exercise `compare_chunk` with parity (test V11).

### No "Send impl" step

Phase 4's Step 2 (`unsafe impl Send for AlignedBuf`) is already in place. This plan skips that step.

---

## Pitfalls (and how to avoid each)

Most pitfalls are inherited verbatim from Phase 4 — the same channel-shutdown deadlock, the same cancel-flag lifetime confusion, the same mid-flash worker panic, the same `Send` impl regression. The list below highlights the _verify-specific_ pitfalls; for the others, refer to Phase 4's plan.

### [Verify-specific] Worker truncates the image stream

**Pitfall:** the image file is shorter than `bytes_written` (operator replaced the file between flash and verify; or the original file was somehow corrupted). The worker's `fill_exact` returns `UnexpectedEof` when it can't fill the requested chunk size. The worker sends `Err(io::Error of UnexpectedEof)` on `filled_tx`. Main propagates with the operator-readable message.

**Fix:** `fill_exact`'s existing `UnexpectedEof` semantics are sufficient — the worker just wraps the error with `.context("reading from compressed image stream during verify")` and forwards it. No new logic required. The existing `fill_exact` tests cover the `UnexpectedEof` case (test `fill_exact_errors_on_early_eof` in `src/verify.rs`).

### [Verify-specific] Worker exits without producing the last chunk

**Pitfall:** the worker exits cleanly (e.g., `free_rx.recv()` returns Err because main dropped `free_tx` early) before delivering all `bytes_written` bytes. Main's next `filled_rx.recv()` returns Err (channel disconnected, worker gone). If main treats this as "worker is done normally" (Phase 4's clean-EOF path), it would return `Ok(())` from verify — but bytes weren't actually compared.

**Fix:** in the channel-disconnect branch of main's `filled_rx.recv()`, check whether `remaining > 0` after joining. If so, the worker exited prematurely — surface as `Err(anyhow!("image stream ended before {remaining} expected bytes (worker exited prematurely)"))`. The pipelined-arm code shown above does exactly this: the `Ok(())` arm of `worker_handle.join()` always sets a truncation error, because the verify worker is _expected_ to deliver exactly `bytes_written` bytes; an "early exit" is only consistent with a contract violation.

This differs from Phase 4's clean-EOF semantics. Phase 4's worker exits when it produces a `filled<BUF_SIZE` chunk (natural EOF signal); Phase 5b's worker exits when it has produced exactly `remaining=0` worth of bytes (counted exit). The verify arm therefore treats "worker exited but main expected more" as an error, where Phase 4 treats the same condition as success.

### [Verify-specific] Mismatch on the last chunk

**Pitfall:** the last chunk is sub-`BUF_SIZE` (because `bytes_written` is not a multiple of BUF_SIZE). The compare runs over `..this_chunk` rather than the full buffer. If `compare_chunk`'s caller passes the wrong `this_chunk` value, comparison could include uninitialized buffer bytes (from previous-iteration buffer contents in `dev_buf`, or from the `AlignedBuf::new()` zeros if it's the first chunk).

**Fix:** `compare_chunk`'s preconditions explicitly require `dev_bytes.len() == img_bytes.len() == this_chunk`. The callers slice the buffers with `..this_chunk` before passing. The `debug_assert_eq!` inside `compare_chunk` catches caller bugs in test/debug builds. In release, slice-equality already compares lengths first, so a length mismatch surfaces as a slice-inequality (the bytes can't differ if one slice is shorter — Rust slice equality returns false on length mismatch without comparing bytes).

### [Verify-specific] Cancel during a long device read

**Pitfall:** USB 2.0 device reads can take ~100 ms per chunk. The main thread is inside `read_exact_at` when cancel fires. The cancel won't be observed until `read_exact_at` returns.

**Fix:** accept the latency. There is no clean way to interrupt an in-flight `read_exact_at`. The next iteration's cancel check (top of loop) catches it. Worst-case latency in the pipelined arm: `device_read + recv_wait + compare + bookkeeping`, where `recv_wait` is bounded by the worker's `fill_exact` time (since main may be waiting for the worker to finish its current chunk before recv unblocks). For USB 2.0 with bzip2: ~100 ms (in-flight device read) + ~80 ms (worker's in-flight fill_exact) + ~1 ms (compare + bookkeeping) ≈ ~180 ms. For USB 3.0 with xz: ~25 + ~40 + ~1 ≈ 66 ms. All well under "sub-second responsive."

The serial arm's worst-case is simpler: `device_read + image_read + compare + next-iteration-observation ≈ 100 + 80 + 1 + (one more sleep/loop tick) ≈ ~200 ms`. Same order of magnitude.

### [Verify-specific] Worker panic produces no FATAL warning (correct behavior, easy to misread)

**Pitfall:** during code review, someone notices that `verify_pipelined`'s cleanup does `resume_unwind` on a captured panic but doesn't call any FATAL-equivalent. Compared to Phase 4's `flash_pipelined`, where the panic propagates through `FlashGuard::Drop` which fires FATAL, this looks like an omission.

**Fix:** it's _intentional_. Verify is read-only. A panic during verify means we couldn't _verify_, not that we _corrupted_ the disk. The disk's contents are unchanged from when verify started — they're whatever the flash phase wrote. The operator should retry verify (perhaps with `--skip-verification` if the verify phase is failing for non-disk reasons), not treat it as a destructive event.

The plan's _Pitfalls_ section flags this so reviewers don't accidentally "fix" the missing FATAL. The `.agents/docs/06-phase-5-verify.md` extension also documents this.

---

## Tests

### Existing tests that must continue to pass

All 97 tests passing after Phase 4 is fully landed (85 baseline + 12 from Phase 4). After Step 1 (extract helpers), every test that exercises `verify::verify` indirectly exercises `compare_chunk` and `verify_finalize`. After Step 2, those same tests exercise `verify_serial`. No test changes are needed to the existing surface.

The existing `fill_exact_*` tests (7 of them) continue to pass unchanged — `fill_exact` is preserved verbatim and used by both the serial arm and the worker.

### New tests — `src/verify.rs::tests`

**Mock `FlashGuard` infrastructure (prerequisite).** Same situation as Phase 4: tests that exercise `verify_serial` or `verify_pipelined` end-to-end need a way to observe `read_exact_at` calls without touching real hardware. The same two approaches apply — (a) trait abstraction, or (b) tempfile + observation wrapper. Phase 4's plan recommended (b); this plan inherits that recommendation. If Phase 4 chose (a) and modified the helper signatures, this plan must adapt accordingly.

For tests V1–V4 (`compare_chunk` and `verify_finalize`), no mock FlashGuard is needed — these helpers do no FD work. They're trivially testable.

For tests V5–V11 (verify-arm-specific), a mock FlashGuard with a configurable backing-store is needed (same scaffolding as Phase 4's plan).

**V1. `compare_chunk_succeeds_when_bytes_match`**
A trivial test: two identical 4 MiB byte vectors, call `compare_chunk(&dev, &img, 0, image_path)`. Verifies `Ok(())`.

Scope: the happy path. Catches a regression where the comparison function spuriously returns Err.

**V2. `compare_chunk_reports_first_diff_offset`**
Two 4 MiB vectors, identical except for one byte at offset 1234. Verifies:

- Returns `Err`.
- The error message contains "byte offset 1234" (or `offset + 1234` if `offset` is non-zero).
- The error message contains the image_path's display string.

A second sub-test sets the differing byte at the very last position (offset BUF_SIZE-1) to verify the loop reaches the end of the slice.

A third sub-test uses `offset = 1_073_741_824` (1 GiB) and a differing byte at slice-position 100, verifying that `abs = offset + first_diff = 1_073_741_924` is reported correctly (no integer overflow at multi-GB offsets).

Scope: mismatch diagnostic precision. Critical for operator-facing error quality.

**V3. `compare_chunk_handles_empty_slices`**
Two zero-length slices. Verifies `Ok(())` (vacuously, since there are no bytes to compare). Catches a regression where an empty-slice edge case panics.

Scope: defensive-coding guard. Empty slices are an unlikely-but-possible input on the path where `bytes_written == 0` somehow reaches `compare_chunk` (it doesn't today, because the dispatcher early-returns; but this test guards the helper independently).

**V4. `verify_finalize_runs_steps_in_order`**
Test that constructs a real `ProgressBar`, sets some position, then calls `verify_finalize`. Verifies:

- `pb.is_finished()` returns true after the call.
- (Conceptually: `println!()` was called. This is hard to assert directly without redirecting stdout; the test can use the `gag` crate or just trust the call.) The Step 1 acceptance criteria mention this test is allowed to be light on the println assertion.

Scope: shared post-loop helper.

**V5. Channel-driven shutdown ordering (pipelined arm)**
Same fixture pattern as Phase 4's test 6. Verifies both directions of the shutdown protocol:

- main dropping `filled_rx` causes the stub worker's `filled_tx.send(...)` to fail.
- main dropping `free_tx` causes the stub worker's `free_rx.recv()` to fail.

Scope: cleanup-block correctness in `verify_pipelined`.

**V6. Cancel-mirror semantics (pipelined arm)**
Same as Phase 4's test 7, but for verify_pipelined. Two related tests:

- `cancel_mirror_propagates_within_one_main_iteration_verify`
- `cancel_mirror_causes_verify_worker_exit_within_one_fill_iteration`

Scope: verify-arm cancellation propagation.

**V7. Worker-error propagation (pipelined arm)**
A test fixture using a `Mock` ImageReader variant (or a tempfile of malformed compressed data wrapped in the matching `Gzip`/`Xz`/`Bz2`/`Zstd` variant) that errors on the second `read` call. Runs `verify_pipelined`. Verifies:

- The error reaches the main thread with `"reading from compressed image stream during verify"` context attached.
- The mock FlashGuard recorded exactly one `read_exact_at` (the first chunk's device read happened before the worker error surfaced).
- The worker thread joined cleanly; no panic.

Scope: worker-error path of the pipelined verify arm.

**V8. Worker truncation propagation (pipelined arm)**
A test fixture where the image stream produces fewer bytes than `bytes_written` claims. Verifies:

- `verify_pipelined` returns `Err`.
- The error message indicates a truncation (either "image stream ended before expected bytes" from `fill_exact`'s `UnexpectedEof`, or "worker exited prematurely" from the channel-disconnect branch — depending on whether `fill_exact` errors or the worker reaches `remaining=0`).
- No mismatch is reported; the failure is correctly classified as truncation, not as a content mismatch.

Scope: contract violation when image and `bytes_written` disagree. Critical because today's serial verify catches this via `fill_exact`'s `UnexpectedEof`; the pipelined arm must catch it too.

**V9. Worker-panic propagation (pipelined arm)**
A test fixture that panics inside the worker (using a `Mock` ImageReader with a `read` that panics, or `panic!()` from a custom Read impl). Verifies:

- `verify_pipelined` propagates the panic via `resume_unwind`.
- A surrounding `std::panic::catch_unwind` in the test detects it.
- **No `FlashGuard::Drop` FATAL message** is produced (because verify is non-destructive; the disk is unchanged). This is a verify-specific assertion that does NOT apply to Phase 4's panic test.

Scope: panic-propagation path. Critical: this is the test that distinguishes verify's panic semantics from flash's.

**V10. Mismatch propagation through pipelined arm**
A test where the device's bytes (from the mock FlashGuard's backing store) differ from the image's bytes at a known offset. Runs `verify_pipelined`. Verifies:

- Returns `Err` with `"verification mismatch at byte offset"` and the correct absolute offset in the message.
- The error is the SAME diagnostic that `verify_serial` would produce for the same input (i.e., `compare_chunk` is the single source of truth, called from both arms).

Scope: end-to-end mismatch path through the pipelined arm.

**V11. Dispatch parity test**
A test that verifies both arms produce identical observable behavior for matched inputs. Setup:

- A known byte pattern of size `N` (e.g., 12 MiB across 3 chunks).
- Two image fixtures backed by the same decompressed bytes: a raw file (the pattern verbatim) wrapped in `ImageReader::Raw`, and a compressed file (e.g., gzip-encoded) wrapped in `ImageReader::Gzip`.
- Two fresh mock `FlashGuard` instances, both pre-populated with the same byte pattern in their backing store.

Then:

- Run `verify_serial(guard_a, Raw(...), ...)`. Capture mock_a's `read_exact_at` log.
- Run `verify_pipelined(guard_b, Gzip(...), ...)`. Capture mock_b's `read_exact_at` log.

Verifies:

- Both return `Ok(())`.
- Both logs are identical: same number of reads, same (offset, length) pairs in order.
- A second sub-test corrupts the mock guards' backing stores at the same offset (introducing a mismatch). Both arms return `Err`, and the two error messages contain the same byte offset and image path (modulo path differences for the two image fixtures).

Scope: catches dispatch drift between `verify_serial` and `verify_pipelined`. Same role as Phase 4's parity test.

### New tests — total count

| File                   | New tests                                 |
| ---------------------- | ----------------------------------------- |
| `src/verify.rs::tests` | 11 (items V1–V11; some contain sub-tests) |
| **Total new**          | **11**                                    |

Total project tests after Step 2: 97 + 11 = **108** (or higher, accounting for sub-tests).

### Hardware tests (manual, but blocking on merge)

**Required minimal hardware test** before merging Step 2:

1. Flash + verify a known-good xz-compressed Linux ISO (Arch, Alpine) to a scratch USB stick. Verify completes with the pipelined arm (compressed → pipelined verify); operator-visible "verification passed" or no error.
2. Flash + verify a known-good _raw_ `.img` (e.g. raw Raspberry Pi OS). Verify completes via the serial arm.
3. Re-run with `--throttle 4M` for both compressed and raw inputs; verify throttling works on both arms (wall-clock time approximately matches expected).
4. Cancel mid-verify with Ctrl+C in both arms; verify the exit is responsive (within ~100 ms on USB 3.0 with gzip/zstd; up to ~200 ms on USB 2.0 with bzip2 in the worst case — see the _Pitfalls_ section for the per-codec breakdown). **No FATAL warning** should fire — verify is non-destructive, so the cancel surfaces as a clean `bail!("cancelled by user")`.
5. **Mismatch test (counterfeit-stick simulation).** imi has no "verify-only" flag, so a mismatch is hard to trigger on a good stick. Three options:
   - Use a real counterfeit USB stick (advertised X GB, actually <X with wrap-around firmware). flash an image larger than the stick's actual capacity and watch verify report a mismatch in the wrap-around region. This is the test the existing `06-phase-5-verify.md` doc recommends.
   - In a tempfile loopback test: create a tempfile of size N, flash an image of size N to the loopback device, then manually modify the tempfile mid-stream (in the gap between flash completing and verify reading), then re-run with the same image. Requires kernel-level intervention to make verify see post-flash modification; hard to set up.
   - Unit-test the mismatch path (test V10 in _Tests_) instead, and rely on hardware tests 1–4 to cover the rest. The mismatch path is fundamentally a `compare_chunk` test; running it through real hardware adds little confidence beyond the unit tests.

**Optional but recommended:**

- Run the same suite on a USB 2.0 device (verify reads will be slower; less benefit from pipelining but should still work correctly).
- Run on a single-core or single-vCPU VM to confirm correctness on constrained-CPU systems.
- Stress test: verify a large bzip2 image (≥4 GiB decompressed). Watch with `strace -e trace=clone,close` to confirm exactly three threads are alive during Phase 5b: main + worker + the `ctrlc` library's signal-handler thread.

### Tests that should _not_ be added

Same negative list as Phase 4: no wall-clock comparison tests (flaky), no "doesn't deadlock" tests (architectural argument), no race tests (type system handles it).

---

## Safety analysis

### `unsafe` surface introduced

**Zero new `unsafe` sites.** The `unsafe impl Send for AlignedBuf {}` is already in place from Phase 4's Step 2. The `BLKFLSBUF` ioctl call in the dispatcher (line: `unsafe { ioctl::blkflsbuf(...) }`) is preserved unchanged from today's serial verify; the SAFETY comment is preserved.

### `unsafe` surface NOT introduced

The plan does **not** require:

- `unsafe impl Sync` for any type. No shared-reference cross-thread access.
- Any new `unsafe` block or `unsafe impl` in `verify.rs`. The pipelined arm uses safe channel operations and safe ownership transfers.
- Any new ioctl, signal handler, or kernel-interface code.

### Non-`unsafe` correctness invariants

Same structural enforcement as Phase 4:

- **Single-threaded FD access.** Only the main thread calls `guard.file()` or `read_exact_at`. The worker is statically prevented because it never receives `&FlashGuard`.
- **Single-threaded ImageReader access.** The worker owns the `ImageReader` by-move; main no longer references it after spawning.
- **Cancel parent flag access.** Only main reads parent `cancel: &AtomicBool`. Worker reads only the local mirror.
- **AlignedBuf allocation/deallocation.** Same as Phase 4: allocations on main; deallocation may happen on either thread; safe because the global allocator is thread-safe.

### AGENTS.md hard-rule compliance

Walking through each hard rule:

1. **Zero external binaries.** Unchanged.
2. **Phase ordering canonical.** Unchanged. Phase 5b still runs between Phase 5a (cooldown) and Phase 6 (BLKRRPART).
3. **`O_DIRECT` invariants.** Verify continues to run with `O_DIRECT` cleared. The dispatcher's `set_direct(fd, false)` happens before either arm runs; neither arm changes it.
4. **Never `O_SYNC` via `F_SETFL`.** Unchanged.
5. **`FlashGuard` lifecycle.** Unchanged. Verify still runs with the guard in `GuardPhase::Verifying`; `disarm` happens after verify succeeds, before Phase 6.
6. **`ctrlc` handler must never `exit()`.** Unchanged. Cancel-mirror is _additional_ propagation, not a replacement.
7. **Verification under `O_EXCL`.** Unchanged. Both arms run before `guard.into_file()` releases the FD. The worker thread does not touch the FD; main retains exclusive access throughout.
8. **`/media` whitelist with sentinel.** Unchanged. Phase 1 logic is untouched.
9. **Devt-not-string correlation.** Unchanged.
10. **`SAFETY` comments mandatory.** Preserved. The existing `unsafe { ioctl::blkflsbuf(...) }` SAFETY comment is preserved verbatim. No new `unsafe` is added.

No hard rule is relaxed. Phase 5b's contract is preserved; only its internal threading model changes.

### Forward-compatibility for further threading

If a future change wants something more aggressive (multiple decompressor workers; async/await; parallel decoder libraries), the analysis is different and this plan is _not_ sufficient. That kind of change requires its own threading-plan-equivalent document and its own architectural review.

---

## Rollback plan

Same shape as Phase 4's:

1. **Per-commit rollback.** Each step independently revertable.
   - Step 1 (extract helpers): reverting returns to today's inline verify body. Helpers go away.
   - Step 2 (pipelined arm + dispatch): reverting collapses the dispatcher back to the Step-1 form (single function, helpers still present).

2. **Targeted rollback of dispatch only.** If Step 1 lands fine but Step 2's pipelined arm has issues, change the dispatcher's branch from `if comp.is_compressed()` to `if false` — routes all images through `verify_serial`. Works because `verify_serial`'s `fill_exact` accepts any `Read` (decompressors are transparent), the byte-count math is identical, and the comparison logic is the same `compare_chunk`. The verified bytes are identical between the two arms; only the wall-clock timing differs.

3. **Bug-fix forward.** Most likely scenario. Verify's bug surface is smaller than Phase 4's (no destructive writes; pure compute + I/O), so most issues will be diagnosable from the operator-visible error message and fixable in a follow-up commit.

---

## Acceptance criteria

The dispatch path is ready to merge when ALL of:

- [ ] Phase 4's threading plan has soaked in production for at least one release tag without threading-related regressions reported.
- [ ] `cargo build --release` succeeds with zero warnings.
- [ ] `cargo test` passes all existing 97 tests + 11 new tests = ≥108 total.
- [ ] `cargo +1.85 build --locked` succeeds (MSRV check).
- [ ] `cargo clippy -- -D warnings` produces no new lints.
- [ ] At least one xz-compressed real Linux ISO has been flashed AND verified via the pipelined verify arm; the verify produced no error.
- [ ] At least one raw `.img` has been flashed AND verified via the serial verify arm.
- [ ] `--throttle` paths have been individually exercised on both raw and compressed verify.
- [ ] Mid-verify Ctrl+C produces a clean `cancelled by user` error with no FATAL warning printed (verify is non-destructive).
- [ ] Mismatch diagnostic verified: either via the unit tests (V10 for pipelined arm, V11 dispatch parity) or via a counterfeit-stick hardware test if one is available. Unit-test coverage is acceptable as the primary signal; the hardware test path is hard to set up without a counterfeit stick on hand (see _Hardware tests_ item 5 for the three options).
- [ ] The `imi-review` skill produces no new findings against the Phase-5b additions.
- [ ] `AGENTS.md`, the Phase-5 doc, and `README.md` are updated in the same commit as Step 2.

The dispatch path is **not** ready to merge if any of the following are true:

- Any of the above checklist items are unchecked.
- A reviewer cannot trace, from this plan, what changes in the code on a verify mismatch (`compare_chunk` produces the diagnostic; both arms surface it identically; pb is abandoned at the call site).
- A reviewer cannot trace the cleanup discipline for `verify_pipelined` (drop both channels before joining, capture panic from join handle, resume_unwind after cleanup).
- A reviewer cannot trace why verify panics produce no FATAL warning (verify is read-only; disk is unchanged; FATAL would mislead the operator).

---

## Appendix: relationship to Phase 4

Phase 4's `threading-plan.md` is the canonical reference for the dispatch-with-shared-helper architecture. This plan applies the same pattern to verify with these specific adaptations:

| Aspect                  | Phase 4 (flash)                                                     | Phase 5b (verify)                                                       |
| ----------------------- | ------------------------------------------------------------------- | ----------------------------------------------------------------------- |
| Shared helper signature | `process_chunk(guard, buf, filled, offset) -> Result<ChunkOutcome>` | `compare_chunk(dev_bytes, img_bytes, offset, image_path) -> Result<()>` |
| Helper does FD I/O?     | Yes (writes via `write_direct`/`write_tail`)                        | No (pure compute)                                                       |
| Helper returns enum?    | Yes (`ChunkOutcome::{Continue,Done}`)                               | No (just `Result<()>`)                                                  |
| Worker EOF mechanism    | Filled < BUF_SIZE signals last chunk                                | Counted: worker exits when `remaining == 0`                             |
| Per-chunk main work     | Write to device + bookkeeping                                       | Read from device + recv + compare + bookkeeping                         |
| Per-chunk worker work   | Read from ImageReader (decompress)                                  | Same                                                                    |
| Bottleneck side         | Usually main (USB write)                                            | Usually worker (decompress > USB read)                                  |
| Panic propagation       | Resume_unwind after cleanup; FlashGuard FATAL fires                 | Resume_unwind after cleanup; NO FATAL (read-only)                       |
| Mismatch concept        | N/A                                                                 | First-diff-offset reported with image path                              |
| Rollback escape         | `if false` routes all through serial                                | Same                                                                    |
| Buffer pool size        | 2 AlignedBufs (image-side)                                          | 2 AlignedBufs (image-side) + 1 standalone (dev_buf, main-thread only)   |

The architectural skeleton is identical; only the per-arm work and a few terminal-state decisions differ.

---

## Implementation addendum (as-built, 2026-07)

Implemented in the same engagement as Phase 4's Steps 1–3, **under an
explicit project-owner override of this plan's production-soak
prerequisite** (no production release occurs inside the engagement;
the owner directed proceeding, and the override is recorded here
rather than silently ignored). Divergences from the sketches, all
deliberate and re-derived against the current codebase:

1. The serial arm keeps its `Vec` + `try_reserve_exact` image buffer
   (the OOM-unwind hardening) instead of switching to `AlignedBuf`;
   `compare_chunk` takes `&[u8]`, so the plan's parity rationale for
   the swap was moot and the swap would have been a needless behavior
   surface.
2. Both arms and the worker carry fail-loud protocol checks the
   sketch lacked: a chunk-length divergence or a clean worker
   disconnect while `remaining > 0` errors out instead of passing as
   a short verify (mirrors the Phase-4 self-review hardening).
3. `verify_worker_loop` / `verify_pipelined` are generic over
   `R: Read` (+ `Send + 'static` for the arm) so the panic and error
   propagation tests drive the real paths; production monomorphizes
   with `ImageReader`.
4. Worker fill errors reuse the serial arm's exact context string
   ("reading {n} bytes from image stream").
5. `std::sync::mpsc` is unbounded (sends never block); the pool
   provides backpressure and `drop(free_tx)` is the join-deadlock
   preventer — same correction as recorded in the Phase-4 addendum.
6. Setup (BLKFLSBUF, `set_direct(false)`, image reopen) lives in the
   dispatcher exactly as this plan's diagram shows; `guard` stays
   `&mut FlashGuard` end-to-end matching the pre-change signature, so
   `main.rs` is untouched.

7. Plan test V9 and hardware-test item 4 assert that NO FATAL warning
   should fire on a mid-verify panic or Ctrl+C ("verify is
   non-destructive"). This contradicts the codebase's long-established
   guard contract: the guard stays armed through Phase 5b
   (`GuardPhase::Verifying`, verb "being read back for verification",
   doc-09's table), because an interrupted verify means the device was
   _written but never verified_ — precisely the state the operator
   must be warned not to trust. As-built keeps that contract; the
   FATAL fires (live-proven for both Ctrl+C and injected mismatch),
   and the plan's assertion is recorded here as an inaccuracy rather
   than implemented.
