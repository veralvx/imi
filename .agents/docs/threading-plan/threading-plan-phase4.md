# Threading Plan — Dispatch with Shared Helper

**Status:** Plan only. No code changes in this document.
**Scope:** Phase 4 (`flash::flash`). Compressed images get a producer-consumer pipeline; raw images stay on a single-threaded loop. Both call into a shared chunk-processing helper that holds the loop-semantics invariants. **Phase 5b verify continues to run on every flash exactly as today — single-threaded, with the same byte-for-byte comparison logic.** Threading the verify loop (overlapping decompression with the device read, analogous to the Phase 4 change) is a follow-up engineering effort, separate from this plan and deferred to a later PR. Verification itself is and remains a non-negotiable part of the pipeline; only its internal threading model is out of scope here.
**Author:** see commit history.
**Last updated:** 2026-05.

---

## TL;DR

Refactor `flash::flash` into:

1. A thin dispatcher that branches on `comp.is_compressed()`.
2. **`flash_serial`** — the existing serial loop, kept structurally as it is today. Routes raw images.
3. **`flash_pipelined`** — a new producer-consumer pipeline. Routes compressed images.
4. **`process_chunk`** — a shared helper that both arms call, holding the invariants common to both: O_DIRECT-vs-tail dispatch and bytes-written accounting. (ENOSPC-to-diagnostic-message mapping already lives inside the existing `write_direct`/`write_tail` helpers; `process_chunk` propagates the typed error.) The caller updates the progress bar after `process_chunk` returns successfully.
5. **`flash_finalize`** — a shared post-loop helper: `fdatasync`, the hardening `set_direct(fd, false)`, `progress.finish_and_clear()`.

The benefit is concentrated on compressed images: xz and bzip2 inputs see ~25–40% wall-clock improvement on USB 3.0 because decompression overlaps with the device write. gzip and zstd see <10%. Raw images stay on the existing serial code, paying zero overhead and keeping their simpler audit surface.

The shared helper is the critical structural choice. It means:

- Future correctness fixes to the loop body live in _one_ place (`process_chunk`) and benefit both arms automatically.
- The dispatch arms are thin shells over the helpers — `flash_serial` is ~50 lines (mostly the existing serial loop), `flash_pipelined` is ~110 lines (the extra weight is unavoidable channel/worker plumbing); both arms' chunk-processing logic is a single call to `process_chunk`.
- The "two diffs for every fix" cost normally associated with dispatch dissolves.

The change introduces:

- One spawned worker thread per `flash::flash` call when the input is compressed.
- One `unsafe impl Send for AlignedBuf {}` with a SAFETY comment.
- Two `std::sync::mpsc` channels in the pipelined arm.
- Two `AlignedBuf` instances (the double-buffer pair) in the pipelined arm; one in the serial arm (unchanged).
- Local-cancel `Arc<AtomicBool>` plumbing in the pipelined arm only.

The change does **not** introduce:

- Any threading on the device-write path. Only the main (writer) thread ever calls `pwrite` on the kernel-`O_EXCL`-claimed FD.
- Any `Mutex`, `RwLock`, or shared mutable buffer state. Channel transfers are the only synchronisation.
- Any threading in raw-image flash, signature-wipe, mount-scan, swap-disable, BLKRRPART, automount-defense, or cooldown paths.
- Any threading-related code on the raw-image hot path. Raw images run through code that is structurally identical to today's `flash::flash` body.
- Any compile-time feature flag. The dispatch is a runtime `if`.

---

## Why dispatch with shared helper — rationale and rejected alternatives

### Why not full unification

A previous draft of this plan threaded all images through one pipelined function. The argument for that was "one code path is simpler to maintain than two." On reflection, this argument has two weaknesses:

1. **Single function ≠ single mental model.** A reviewer auditing a unified pipelined `flash::flash` has to hold flash-loop semantics _plus_ threading semantics (channel ordering, cancel-mirror, join-on-every-exit, worker-panic propagation) — every time, including when reviewing a change that only affects raw flashing. Dispatch lets the threading model stay out of the reviewer's head when they're working on the serial path.

2. **Blast-radius asymmetry matters in a destructive tool.** A bug that surfaces only on compressed flashes hits roughly half of operator workflows. The same bug under unification hits _all_ of them, including the most-common case (operators flashing raw `.iso` distros). For a tool whose failure mode is "destroyed boot drive," exposing the common case to the new bug-class is the wrong direction.

The "two diffs for every fix" cost of dispatch is real but largely solved by the shared helper. Loop-body invariants live in `process_chunk`; both arms call it. A correctness fix to (say) the O_DIRECT toggling for tail writes lives in one place, fixed once. The dispatch arms themselves are thin — the kind of code you read once, audit once, and rarely modify.

### Why not pure dispatch (no shared helper)

A naïve dispatch would have `flash_serial` and `flash_pipelined` each contain their own copy of the loop body — each with their own `process the filled buffer` logic. This is the configuration that genuinely doubles future-refactor cost, because every flash-loop semantic change has to be made twice and kept in sync.

The shared helper resolves this. Both arms become "drive a chunk source; for each chunk, call `process_chunk`; finally call `flash_finalize`." The arms differ only in _how chunks are produced_ (synchronous fill vs channel recv) and _how cancellation propagates_ (direct flag check vs cancel-mirror).

### Why dispatch with shared helper

It captures the safety profile of dispatch (raw images don't run threading code) and most of the maintenance profile of unification (one place to fix the loop body). The two-arms structure is a thin shell over a shared core; the cognitive cost is "one helper plus two small adapters" rather than "two parallel implementations" or "one complex unified path."

The arms are different enough that forcing them into one function would mean injecting `if threaded { ... } else { ... }` branches into the loop body, which is just dispatch with worse syntax. They are similar enough that the loop body itself can be shared. Dispatch-with-shared-helper is the natural shape.

---

## Why threading helps in this project at all

### The bottleneck table

For one 4 MiB chunk (`BUF_SIZE`), per-stage time on typical hardware. Numbers are approximate and content-dependent — the _shape_ (read vs write dominance) is what matters.

| Stage                    | Time (NVMe source / USB 3.0 target) | CPU-bound?                                         |
| ------------------------ | ----------------------------------- | -------------------------------------------------- |
| `fill_buffer` (raw)      | <1 ms                               | No (disk read)                                     |
| `fill_buffer` (gzip)     | ~2–5 ms                             | Yes (modest)                                       |
| `fill_buffer` (zstd)     | ~2–5 ms                             | Yes (modest)                                       |
| `fill_buffer` (xz)       | ~15–40 ms                           | **Yes — comparable to or exceeding the USB write** |
| `fill_buffer` (bzip2)    | ~30–80 ms                           | **Yes — typically exceeds the USB write**          |
| `write_direct` (USB 3.0) | ~30–50 ms                           | No (USB controller)                                |
| `write_direct` (USB 2.0) | ~120–150 ms                         | No (USB controller)                                |

Serial pipeline takes `read + write` per chunk; pipelined pipeline takes `max(read, write)` per chunk. Savings are proportional to the _shorter_ of {read, write}. On USB 3.0 with xz or bzip2, read and write are comparable, so savings are large. On USB 3.0 with gzip/zstd, read is tiny so savings are small. On USB 2.0, the write dominates: gzip/zstd save little (small read), but xz/bzip2 still save substantial absolute time (their read cost is recovered from the critical path). Raw on any USB has near-zero read cost so threading is pointless.

Concrete worked examples for a 1 GiB image (256 chunks of 4 MiB). Each row's "Saved" column is the difference between serial (`read + write`) and pipelined (`max(read, write)`):

| Image type / target    | Serial estimate | Pipelined estimate | Saved   |
| ---------------------- | --------------- | ------------------ | ------- |
| xz on USB 3.0          | ~14–23 s        | ~10–13 s           | ~3–10 s |
| bzip2 on USB 3.0       | ~17–33 s        | ~10–20 s           | ~7–13 s |
| gzip / zstd on USB 3.0 | ~9–14 s         | ~8–13 s            | ~1 s    |
| xz on USB 2.0          | ~37–48 s        | ~32–38 s           | ~5–10 s |
| bzip2 on USB 2.0       | ~40–60 s        | ~32–38 s           | ~8–22 s |
| gzip / zstd on USB 2.0 | ~33–40 s        | ~32–38 s           | ~1 s    |

The general pattern: pipelining saves time proportional to the _shorter_ of {decompression cost, device-write cost}, capped by the longer one. On USB 2.0, the USB write is so slow that pipelining recovers most of the decompression cost; on USB 3.0, the savings depend more on the specific compression format.

Raw images stay on the serial path and see no change.

### Why no other phase benefits

- **Phase 0 (preflight), Phase 1 (topology audit), Phase 2 (`O_EXCL` claim), Phase 6 (BLKRRPART):** sub-millisecond syscalls. No CPU/IO overlap to exploit.
- **Phase 3 (wipe):** two `pwrite`s of 1 MiB zeros each. No second computation to pipeline.
- **Phase 5a (cooldown):** an explicit 10-second wall-clock wait. Threading defeats the purpose.
- **Phase 7 (automount defense):** sleeping on udev events and scanning `/proc/self/mountinfo`. I/O-bound on procfs; no CPU work.

### Why the device FD must stay single-threaded

`O_EXCL` on a Linux block device is a kernel-enforced exclusive claim, _not_ a thread-local lock. The kernel allows only one `pwrite` call at a time on the FD; concurrent `pwrite`s from two threads would interleave at the kernel-internal locking layer.

The `O_DIRECT` invariants (4 KiB-aligned buffer, sector-aligned offset, sector-aligned length) are checked per-`pwrite` syscall. Two threads issuing `pwrite` to the same FD with overlapping offsets would not be a soundness failure at the kernel level (the kernel would serialise), but it would be an application-level correctness failure (the project's offset bookkeeping assumes sequential writes).

The dispatch design enforces single-threaded FD access structurally: the worker thread in `flash_pipelined` holds an `&mut` view of an `AlignedBuf` (for filling) and never receives `&FlashGuard`. Only the main thread calls `process_chunk`, which is the only function that ever touches the FD. The serial path has only one thread; the question doesn't arise.

---

## Architecture

### Component diagram

```
      ┌─────────────────────────────┐
      │         flash::flash         │
      │   (thin runtime dispatch)   │
      └──────────────┬──────────────┘
                     │
         comp.is_compressed()?
              │           │
            no│           │yes
              ▼           ▼
┌──────────────────┐  ┌──────────────────┐
│   flash_serial   │  │  flash_pipelined │
│  (current shape) │  │   (new threaded) │
└────────┬─────────┘  └────────┬─────────┘
         │                     │
         └──────────┬──────────┘
                    ▼
         ┌──────────────────────┐
         │    process_chunk     │  ← shared loop-body invariants
         │  (full / tail / EOF) │     (O_DIRECT toggling for tail
         └──────────────────────┘      and bytes-accounting return
                    ▲                  value; ENOSPC handling lives
                    │                  inside write_direct/write_tail
                    │                  which it calls. Called once
                    │                  per chunk from either arm.)
         ┌──────────┴──────────┐
         │   flash_finalize    │  ← shared post-loop work
         │ (fdatasync, harden, │     (called by both arms after
         │  finish progress,   │      the chunk loop succeeds)
         │  println newline)   │
         └─────────────────────┘
```

### Pipelined data-flow diagram

```
                   free_tx                          free_rx
   ┌──────────────────────────────────────────────────────┐
   │                                                      │
   ▼                                                      │
┌─────────────────────────┐                  ┌────────────┴──────────────┐
│  Worker thread          │                  │  Main thread (writer)     │
│                         │                  │                           │
│  loop:                  │                  │  loop:                    │
│    buf ← free_rx.recv() │                  │    if cancel: shutdown    │
│    if local_cancel:     │                  │    (buf, n) ← filled_rx   │
│      return             │                  │             .recv()       │
│    fill buf from        │   filled_tx      │    process_chunk(...)     │
│      ImageReader        │ ───────────────▶ │      → Continue:          │
│    filled_tx.send(      │                  │          free_tx.send(buf)│
│      Ok((buf, n)))      │                  │      → Done:              │
│    if n < BUF_SIZE:     │                  │          drop(buf); break │
│      return (EOF)       │                  │    throttle               │
│  on error:              │                  │                           │
│    filled_tx.send(Err)  │                  │                           │
│    return               │                  │                           │
└─────────────────────────┘                  └───────────────────────────┘

  (worker reads from ImageReader, which the worker owns by-move.
   The reader contains a decompressor; ImageReader is `Read + Send`
   to the worker. Raw images do NOT go through this path.)
```

### The shared helper: `process_chunk`

This is the new function and the structural anchor of the dispatch design. Its contract:

```rust
// In src/flash.rs — new, private to the module.

/// Outcome of processing a single chunk. Drives the caller's loop control.
enum ChunkOutcome {
    /// Wrote a full BUF_SIZE chunk. Caller should continue looping.
    Continue { bytes_written: u64 },
    /// Wrote a tail (or detected empty input — `bytes_written == 0`).
    /// Caller must break its loop; no further chunks will be processed.
    Done { bytes_written: u64 },
}

/// Items the worker thread sends to the main thread via filled_tx.
/// The Ok variant carries a filled buffer and how many bytes are valid;
/// the Err variant carries a fatal worker-side error (typically a
/// fill_buffer failure wrapped with `.context("...")`).
type FilledItem = anyhow::Result<(AlignedBuf, usize)>;

/// Process one filled buffer: write it to the device, return how many
/// bytes hit the device and whether more chunks are expected.
///
/// This is the loop-body invariant shared between `flash_serial` and
/// `flash_pipelined`. Any change to O_DIRECT toggling for the tail or
/// bytes-written accounting belongs here, fixed once and benefiting
/// both arms. ENOSPC mapping itself lives inside `write_direct` and
/// `write_tail` (they map ENOSPC to a diagnostic anyhow message); this
/// helper just propagates the typed error via `?`.
///
/// Progress bar updates are NOT done here — the caller calls
/// `pb.set_position(total)` after each successful return, matching the
/// existing serial code's pattern of separating I/O work from UI updates.
///
/// Preconditions:
///   - `buf.as_slice()[..filled]` contains the bytes to write.
///   - `filled <= BUF_SIZE`.
///   - `offset` is a multiple of `BUF_SIZE` (required for the full-chunk
///     O_DIRECT write; tail writes happen with O_DIRECT off, so any
///     offset is acceptable, but in practice `offset` is always a
///     BUF_SIZE multiple because we only get here after a sequence of
///     full-chunk writes).
///   - `guard` is armed in `GuardPhase::Writing`.
///   - The FD's O_DIRECT state is whatever the caller set it to before
///     entering the loop (typically: O_DIRECT on at the start of the
///     flash phase).
///
/// Postconditions:
///   - On `Ok(Continue { bytes_written: BUF_SIZE })`: a full BUF_SIZE
///     write happened at `offset`; the FD remains in `O_DIRECT` mode.
///   - On `Ok(Done { bytes_written })`: either a tail of `bytes_written`
///     bytes was written (FD now in non-O_DIRECT mode), or the input
///     was empty (`bytes_written == 0`, FD state unchanged from caller).
///   - On `Err(_)`: a write failed; FD state is undefined; caller must
///     propagate the error and let `FlashGuard::Drop` handle the
///     destructive-window FATAL warning.
fn process_chunk(
    guard: &FlashGuard,
    buf: &AlignedBuf,
    filled: usize,
    offset: u64,
) -> Result<ChunkOutcome>;
```

Implementation sketch (uses the project's existing idioms — `write_direct`/`write_tail` already return `anyhow::Result<()>` with ENOSPC-to-message mapping baked in; `process_chunk` does not need to re-do that mapping):

```rust
fn process_chunk(
    guard: &FlashGuard,
    buf: &AlignedBuf,
    filled: usize,
    offset: u64,
) -> Result<ChunkOutcome> {
    if filled == 0 {
        // Empty input. No write; signal done.
        return Ok(ChunkOutcome::Done { bytes_written: 0 });
    }

    if filled == BUF_SIZE {
        // Full chunk — write under O_DIRECT. write_direct already maps
        // ENOSPC to the diagnostic message; just propagate.
        write_direct(guard, buf.as_slice(), offset)?;
        return Ok(ChunkOutcome::Continue { bytes_written: BUF_SIZE as u64 });
    }

    // Tail chunk (0 < filled < BUF_SIZE) — disable O_DIRECT, write through
    // page cache. `buf` stays alive for the borrow's full extent because
    // the slice borrow is bounded by this function scope. write_tail
    // already maps ENOSPC to the diagnostic message; just propagate.
    let fd = guard.as_raw_fd();
    set_direct(fd, false).context("disabling O_DIRECT for tail write")?;
    write_tail(guard, &buf.as_slice()[..filled], offset)?;
    Ok(ChunkOutcome::Done { bytes_written: filled as u64 })
}
```

Note the signature uses `guard: &FlashGuard` (immutable) and does **not** take a `&ProgressBar`. The progress bar is updated by the caller after `process_chunk` returns successfully — this matches the existing serial code's pattern (`pb.set_position(total)` outside the chunk-processing block) and keeps `process_chunk` purely about writing. Both arms call `pb.set_position(total)` after a successful `Continue` and after the final `Done`.

`&mut FlashGuard` is only required for `arm`/`disarm`/`set_phase` — lifecycle methods that this helper does not call. The project's existing `write_direct` and `write_tail` take `&FlashGuard`, matching this contract.

Key properties of this helper:

- **No threading awareness.** It takes a buffer and writes it; it doesn't know whether the buffer came from a synchronous `fill_buffer` call or a `mpsc::Receiver`.
- **No cancellation handling.** Cancellation is the caller's job, checked between chunks, not during them. (A chunk write is fast enough — tens of milliseconds — that interrupting mid-write would not improve responsiveness.)
- **No throttle.** Throttling is per-iteration timing logic that sandwiches the helper.
- **No progress-bar updates.** The caller calls `pb.set_position(total)` after a successful return; the helper stays focused on I/O work. This matches the existing serial code's separation of I/O and UI.
- **Returns a discriminated outcome.** The caller breaks its loop on `Done`, continues on `Continue`. There is no third state.
- **`bytes_written` is `u64` from the start** to match `FlashOutcome::bytes_written` and avoid `as` casts at the call sites.

### The shared post-loop helper: `flash_finalize`

```rust
/// Run the post-loop work that both arms perform identically:
///   1. fdatasync to flush kernel buffers to the device.
///   2. set_direct(fd, false) — hardening: leave the FD in a known state
///      so the next phase doesn't inherit O_DIRECT it didn't expect.
///   3. progress.finish_and_clear() — finishes the bar (process_chunk
///      does not touch the bar; the caller updates it via set_position
///      between chunks; the bar is only finished here, on the success
///      path). On error/cancel paths, the caller calls pb.abandon()
///      directly and skips this finalize.
///   4. println!() — emit a clean newline so the bar's last rendered
///      position doesn't blend with the next phase's text.
fn flash_finalize(guard: &FlashGuard, progress: &ProgressBar) -> Result<()>;
```

Implementation:

```rust
fn flash_finalize(guard: &FlashGuard, progress: &ProgressBar) -> Result<()> {
    // fdatasync takes &File; nix::unistd::fdatasync handles that directly.
    nix::unistd::fdatasync(guard.file())
        .context("fdatasync after flash loop")?;

    // Hardening: leave the FD in a known-clean state for subsequent phases.
    // Currently the verify phase explicitly clears O_DIRECT on entry, so
    // this line is "load-bearing only against future changes." Do not
    // remove without auditing every caller of guard.file() in subsequent
    // phases. This preserves the intent of today's post-loop set_direct
    // call; see AGENTS.md style notes on inert-but-future-proof code.
    let fd = guard.as_raw_fd();
    set_direct(fd, false)
        .context("post-flash O_DIRECT clear (hardening)")?;

    progress.finish_and_clear();
    // Force a clean newline so the bar's last rendered position doesn't
    // mix with the next phase's text. Matches the existing serial code's
    // post-loop output ordering.
    println!();
    Ok(())
}
```

The hardening comment is verbatim guidance to preserve the intent of today's post-loop `set_direct(fd, false)` call. That intent should survive into the helper.

### The serial arm: `flash_serial`

```rust
fn flash_serial(
    guard: &mut FlashGuard,
    mut reader: ImageReader,
    raw_size: Option<u64>,
    throttle: Option<u64>,
    cancel: &AtomicBool,
) -> Result<FlashOutcome> {
    let fd = guard.as_raw_fd();
    set_direct(fd, true).context("enabling O_DIRECT for flash phase")?;

    // flash_serial only routes raw images (per the dispatcher), so the
    // progress bar is always the percent-bar variant.
    let pb = make_progress_bar(Compression::Raw, raw_size);
    // Critical: reset the bar's elapsed timer immediately before the write
    // loop starts. Without this, bar construction + first-chunk fill time
    // count toward the rate calculation and produce a large initial spike.
    pb.reset_elapsed();

    let mut buf = AlignedBuf::new();
    let mut total: u64 = 0;
    let mut offset: u64 = 0;
    let chunk_target_nanos = throttle.map(|rate_bps| {
        // Use u128 to avoid overflow on very low rates.
        let ideal = (BUF_SIZE as u128).saturating_mul(1_000_000_000)
            / rate_bps as u128;
        u64::try_from(ideal).unwrap_or(u64::MAX)
    });

    loop {
        if cancel.load(Ordering::SeqCst) {
            pb.abandon();
            bail!("cancelled by user");
        }

        let start = chunk_target_nanos.map(|_| Instant::now());

        let filled = fill_buffer(&mut reader, buf.as_mut_slice())
            .context("reading from image stream")?;

        match process_chunk(guard, &buf, filled, offset)? {
            ChunkOutcome::Done { bytes_written } => {
                total += bytes_written;
                pb.set_position(total);
                break;
            }
            ChunkOutcome::Continue { bytes_written } => {
                total += bytes_written;
                offset += bytes_written;
                pb.set_position(total);
            }
        }

        // Throttle: cancellable_sleep observes the parent cancel flag and
        // returns early if cancellation fires mid-sleep.
        if let (Some(target_ns), Some(t0)) = (chunk_target_nanos, start) {
            let elapsed_ns = u64::try_from(t0.elapsed().as_nanos())
                .unwrap_or(u64::MAX);
            if let Some(residual) = target_ns.checked_sub(elapsed_ns) {
                cancellable_sleep(Duration::from_nanos(residual), cancel);
            }
        }
    }

    flash_finalize(guard, &pb)?;
    Ok(FlashOutcome { bytes_written: total })
}
```

This is structurally identical to today's `flash::flash` body, with two changes:

1. The body of the chunk-processing block is a single call to `process_chunk` followed by a `pb.set_position(total)` update — instead of inline `write_direct` / `write_tail` logic with the same `pb.set_position(total)` afterward.
2. The post-loop work is a single call to `flash_finalize` instead of inline `fdatasync` / `set_direct` / `finish_and_clear` / `println!()` calls.

Everything else — the `AlignedBuf` allocation, `fill_buffer` call, throttle timing, cancel check at top of loop, `pb.abandon()` on cancel, `pb.reset_elapsed()` after construction — is unchanged from today's serial code.

### The pipelined arm: `flash_pipelined`

```rust
fn flash_pipelined(
    guard: &mut FlashGuard,
    reader: ImageReader,
    comp: Compression,
    throttle: Option<u64>,
    cancel: &AtomicBool,
) -> Result<FlashOutcome> {
    let fd = guard.as_raw_fd();
    set_direct(fd, true).context("enabling O_DIRECT for flash phase")?;

    // Spinner template (compressed images: total decompressed size unknown).
    // The `comp` argument lets make_progress_bar pick the spinner-with-rate
    // variant; raw_size is None because we don't know the decompressed total.
    let pb = make_progress_bar(comp, None);
    pb.reset_elapsed();

    let chunk_target_nanos = throttle.map(|rate_bps| {
        // u128 to avoid overflow on very low rates.
        let ideal = (BUF_SIZE as u128).saturating_mul(1_000_000_000)
            / rate_bps as u128;
        u64::try_from(ideal).unwrap_or(u64::MAX)
    });

    // Construct cancel mirror.
    let local_cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = local_cancel.clone();

    // Construct channels and seed the buffer pool with two AlignedBufs.
    // AlignedBuf::new() returns AlignedBuf directly (not Result), so no
    // .context() is needed at allocation. The mpsc::Sender::send Err
    // variant only fires if the receiver is closed, which cannot happen
    // here (we still hold free_rx in the local stack), so the unwrap on
    // send is sound for seeding.
    let (filled_tx, filled_rx) = mpsc::channel::<FilledItem>();
    let (free_tx, free_rx) = mpsc::channel::<AlignedBuf>();
    free_tx.send(AlignedBuf::new())
        .expect("seed buffer 0: free_rx still in scope");
    free_tx.send(AlignedBuf::new())
        .expect("seed buffer 1: free_rx still in scope");

    // Spawn the worker. `reader`, `worker_cancel`, `filled_tx`, `free_rx`
    // are all moved into the closure.
    let worker_handle: thread::JoinHandle<()> = thread::spawn(move || {
        worker_loop(reader, worker_cancel, filled_tx, free_rx);
    });
    let mut worker_handle_taken: Option<thread::JoinHandle<()>> =
        Some(worker_handle);

    let mut total: u64 = 0;
    let mut offset: u64 = 0;
    let mut outcome: Result<()> = Ok(());
    let mut worker_panic: Option<Box<dyn std::any::Any + Send>> = None;

    'write_loop: loop {
        // 1. Cancel check.
        if cancel.load(Ordering::SeqCst) {
            local_cancel.store(true, Ordering::SeqCst);
            outcome = Err(anyhow!("cancelled by user"));
            break 'write_loop;
        }

        let start = chunk_target_nanos.map(|_| Instant::now());

        // 2. Receive next filled buffer.
        let (buf, filled) = match filled_rx.recv() {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => { outcome = Err(e); break 'write_loop; }
            Err(_) => {
                // Channel disconnected. Distinguish clean EOF from panic
                // by joining the worker now; capture any panic for
                // re-raise after the cleanup block runs (so channels
                // close before unwinding through FlashGuard::Drop).
                match worker_handle_taken.take().unwrap().join() {
                    Ok(()) => break 'write_loop,
                    Err(panic) => {
                        worker_panic = Some(panic);
                        break 'write_loop;
                    }
                }
            }
        };

        // 3. Process the chunk (shared helper, identical to flash_serial).
        match process_chunk(guard, &buf, filled, offset) {
            Ok(ChunkOutcome::Continue { bytes_written }) => {
                total += bytes_written;
                offset += bytes_written;
                pb.set_position(total);
                // Return drained buffer to the worker pool. If the worker
                // has already exited (e.g., EOF reached and free_rx
                // dropped), the send fails harmlessly — the next
                // filled_rx.recv() will detect disconnect.
                let _ = free_tx.send(buf);
            }
            Ok(ChunkOutcome::Done { bytes_written }) => {
                total += bytes_written;
                pb.set_position(total);
                drop(buf);
                break 'write_loop;
            }
            Err(e) => {
                drop(buf);
                outcome = Err(e);
                break 'write_loop;
            }
        }

        // 4. Throttle. cancellable_sleep observes the parent cancel flag
        // and returns early if cancellation fires mid-sleep; we still
        // re-check the flag and break, otherwise the next filled_rx.recv()
        // would block waiting for a chunk the worker won't send.
        if let (Some(target_ns), Some(t0)) = (chunk_target_nanos, start) {
            let elapsed_ns = u64::try_from(t0.elapsed().as_nanos())
                .unwrap_or(u64::MAX);
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

    // Single cleanup block — runs on EVERY exit path.
    //
    // We must drop BOTH channel halves the main thread holds before
    // joining, because the worker can be blocked on either of two
    // recv/send operations and we don't know which:
    //   - blocked on `filled_tx.send(...)` if it just produced a chunk
    //     and the main loop never recv'd it (e.g. ENOSPC mid-flash);
    //   - blocked on `free_rx.recv()` if the worker just returned to
    //     the top of its loop and the pool is empty (the main loop
    //     consumed both buffers without returning them — e.g. the
    //     ENOSPC error path that does drop(buf) on Err).
    // Dropping `filled_rx` unblocks the first case; dropping `free_tx`
    // unblocks the second. Without both, `join()` can deadlock.
    //
    // 1. Drop filled_rx FIRST so any pending worker send fails.
    drop(filled_rx);
    // 2. Drop free_tx so a worker blocked on free_rx.recv() also exits.
    drop(free_tx);
    // 3. Join the worker (if not already joined in the disconnect branch).
    //    Discard any error/panic here — if the channel-disconnect branch
    //    captured a panic into worker_panic, we'll resume_unwind below.
    if let Some(handle) = worker_handle_taken.take() {
        let _ = handle.join();
    }
    // 4. If the worker panicked, re-raise now (after cleanup is complete).
    //    FlashGuard::Drop will run during the unwind and print FATAL.
    if let Some(panic) = worker_panic {
        std::panic::resume_unwind(panic);
    }

    // 5. If the loop set an error outcome (cancel or write error),
    //    abandon the progress bar (leaves it on screen as the last
    //    operator-visible state) and propagate the error. Skip
    //    flash_finalize — fdatasync is unnecessary on the error path
    //    (the data is already lost or undefined), and calling
    //    finish_and_clear on an already-abandoned bar is wasted work.
    if outcome.is_err() {
        pb.abandon();
        outcome?;
    }

    // 6. Success path — finalize cleanly. flash_finalize calls
    //    fdatasync, the hardening set_direct(false), and finish_and_clear.
    flash_finalize(guard, &pb)?;
    Ok(FlashOutcome { bytes_written: total })
}
```

### The worker loop: `worker_loop`

```rust
fn worker_loop(
    mut reader: ImageReader,
    worker_cancel: Arc<AtomicBool>,
    filled_tx: mpsc::Sender<FilledItem>,
    free_rx: mpsc::Receiver<AlignedBuf>,
) {
    loop {
        // 1. Acquire a fresh buffer from the pool.
        let mut buf = match free_rx.recv() {
            Ok(b) => b,
            Err(_) => return,  // main has shut down
        };

        // 2. Cancel check (mirror set by main thread on parent-flag).
        if worker_cancel.load(Ordering::SeqCst) {
            return;
        }

        // 3. Fill the buffer from the ImageReader.
        let filled = match fill_buffer(&mut reader, buf.as_mut_slice()) {
            Ok(n) => n,
            Err(e) => {
                let wrapped = anyhow::Error::from(e)
                    .context("reading from compressed image stream");
                let _ = filled_tx.send(Err(wrapped));
                return;
            }
        };

        // 4. Send the filled buffer to the main thread.
        if filled_tx.send(Ok((buf, filled))).is_err() {
            return;  // main has dropped filled_rx
        }

        // 5. EOF — main will process this as the tail and break.
        if filled < BUF_SIZE {
            return;
        }
    }
}
```

### The dispatcher: `flash::flash`

```rust
pub fn flash(
    guard: &mut FlashGuard,
    reader: ImageReader,
    comp: Compression,
    raw_size: Option<u64>,
    throttle: Option<u64>,
    cancel: &AtomicBool,
) -> Result<FlashOutcome> {
    if comp.is_compressed() {
        // raw_size is unused for compressed inputs (decompressed total
        // unknown — the spinner template doesn't take a length).
        flash_pipelined(guard, reader, comp, throttle, cancel)
    } else {
        // comp is Compression::Raw here; flash_serial hardcodes the
        // percent-bar variant of the progress bar.
        flash_serial(guard, reader, raw_size, throttle, cancel)
    }
}
```

The dispatcher is six lines. Its public signature is unchanged from today; callers in `main.rs` need no edits.

### Send-bounds story

`AlignedBuf` becomes `Send` (see _`AlignedBuf: Send` impl_ below). `ImageReader` is `Send` automatically because every variant contains `Send` types: `BufReader<File>` is `Send` (since `File: Send`), and each decompressor (`flate2::bufread::GzDecoder<R>`, `xz2::bufread::XzDecoder<R>`, `bzip2::bufread::BzDecoder<R>`, `zstd::Decoder<'static, R>`) is `Send` when its inner `R: Send`. The plan does not need any new `Send` impl on `ImageReader` or its decoder variants.

The worker thread takes ownership of the `ImageReader` by-move (closure with `move ||` capture, or in this plan via `worker_loop(reader, ...)` taking it by value). The main thread no longer references it after spawning. There is no shared `&mut ImageReader` — ownership transfer is total.

### Channels and ownership

Two `std::sync::mpsc` channels in the pipelined arm:

- **`filled_tx: Sender<FilledItem>` / `filled_rx: Receiver<FilledItem>`** — worker → main. Carries filled buffers and worker errors. `FilledItem` is `Result<(AlignedBuf, usize)>`.
- **`free_tx: Sender<AlignedBuf>` / `free_rx: Receiver<AlignedBuf>`** — main → worker. Returns drained buffers for refill.

The pipeline is seeded by sending two fresh `AlignedBuf::new()` values into `free_tx` _before_ spawning the worker. Each `AlignedBuf` is owned by exactly one of the two threads at any moment; channel transfers are the synchronisation primitive. There is no `Mutex` and no shared mutable state.

Because the worker only fills a buffer it just received from `free_rx`, and the main thread only reads a buffer it just received from `filled_rx`, the maximum in-flight count is bounded by the pool size (2 buffers). No queue can grow unboundedly.

**Why exactly 2 buffers, not 3 or N?** Two is the minimum that enables pipelining: while the writer is processing buffer A, the worker can fill buffer B. A third buffer would let the worker run two chunks ahead — but that gains nothing, because the writer is the bottleneck and cannot consume faster. The third buffer would just sit filled, adding 4 MiB of resident memory for zero throughput improvement. Two buffers is therefore the throughput-optimal pool size; smaller wouldn't pipeline; larger would waste memory.

**Channel choice cost.** `std::sync::mpsc` is `Mutex`-based, paying ~50–100 ns per `send`/`recv`. With 4 channel ops per chunk and ~25 chunks/second at 100 MiB/s flash throughput, this is ~10 µs/sec total — under 0.001% of wall-clock. `crossbeam::channel::bounded(2)` would be lock-free and marginally faster, but the absolute saving is in the noise. The plan's appendix discusses this trade-off explicitly under _Alternative 6_.

### Cancellation propagation

The pipelined arm uses two complementary signaling mechanisms:

1. **Channel disconnects (primary):** When main observes the parent cancel flag and breaks the loop, the cleanup block drops both `filled_rx` and `free_tx`. These drops cause the worker's pending `recv` or `send` to return `Err`, which the worker treats as a shutdown signal. This mechanism alone is sufficient for correctness — the worker WILL exit eventually.

2. **Cancel mirror (latency optimization):** A local `Arc<AtomicBool>` cloned into the worker. When main observes the parent flag, it stores `true` into this mirror _before_ breaking. The worker checks the mirror on entry to each iteration (after a successful `free_rx.recv()`, before calling `fill_buffer`). The mirror lets the worker bail out of a _successful_ recv without doing a wasted `fill_buffer` + `filled_tx.send` cycle — important because `fill_buffer` on bzip2 input can take ~80 ms, and the mirror cuts that off at the start of the iteration.

Without the mirror, cancellation would still work correctly but with up to ~80 ms of extra latency in the worst case (worker mid-`fill_buffer`). With the mirror, the worker exits as soon as it returns to the top of its loop.

The serial arm does not need a cancel mirror; it has direct access to `cancel: &AtomicBool` and checks it at the top of each iteration. There's no second thread, no channel, no fill_buffer-in-flight to interrupt.

This split — parent flag in the serial arm, mirror in the pipelined arm — is a feature of the dispatch design. Each arm uses the simpler primitive that fits its threading model.

**Memory ordering.** The plan uses `Ordering::SeqCst` for all atomic loads and stores (parent flag, local mirror). This is stronger than strictly necessary — `Relaxed` would suffice for the cancel flag because no data synchronization rides on it (we're signaling "stop", not publishing data through the flag). On x86 the difference is one `mfence` per `SeqCst` load (~5–20 ns); on ARM it's a `dmb` (~10–50 ns). Per-chunk cost: a few hundred nanoseconds across both threads. Per-flash: low microseconds.

`SeqCst` is chosen for **clarity over performance**: it makes the cancel signal visibly synchronous to a reader, and it avoids the trap of someone later adding a non-cancel atomic that _does_ need ordering and conflating it with the cancel flag. The cost is invisible against per-chunk write times of tens of milliseconds. If a future change makes atomics show up in a profile (they currently don't), `Relaxed` for the cancel flag is a single-line optimization with no correctness risk.

### Error propagation

Three error-source paths in the pipelined arm:

- **Worker error** (decompressor I/O fault): worker sends `Err(e)` on `filled_tx` and returns. Main, on receiving `Err`, breaks its loop and propagates after joining.
- **Writer error** (`process_chunk` returns `Err`, e.g. ENOSPC): main breaks its loop and drops both `filled_rx` and `free_tx`. The worker is most likely blocked on `free_rx.recv()` (because the main consumed and dropped a buffer without returning it); dropping `free_tx` unblocks it. The worker returns; main joins; main propagates the error.
- **Cancellation**: main detects parent flag, mirrors to `local_cancel`, drops `filled_rx` and `free_tx`. Worker observes either the cancel mirror (on entry to next iteration) or the channel disconnects (on its current `recv`/`send`), and returns. Main joins, returns `Err("cancelled by user")`.

The order of channel drops at main shutdown is precise: drop both main-side endpoints (`filled_rx` and `free_tx`) before joining. The first unblocks any worker that's pending in `filled_tx.send(...)`; the second unblocks any worker that's blocked in `free_rx.recv()` (which can happen when the main thread consumed the last pool buffer without returning it — e.g., on the ENOSPC error path that does `drop(buf)`). Without both drops, `join()` can deadlock.

The serial arm has only one error path: `process_chunk` or `fill_buffer` returns `Err`, which is propagated immediately.

### Joining the worker thread

The main thread always joins on **every** exit path (success, cancel, writer error, worker error). The cleanup block in `flash_pipelined`'s structure makes this discipline structural — `if let Some(handle) = worker_handle_taken.take() { let _ = handle.join(); }` after the labelled-break loop covers all exits.

If the worker panicked, the channel-disconnect branch of `filled_rx.recv()` already consumed the handle and captured the panic in `worker_panic`. The cleanup block's `take()` returns `None` in that case, and the post-cleanup `resume_unwind` rethrows the panic. `FlashGuard::Drop` runs during the unwind and prints the FATAL warning.

### Writer-loop and worker-loop discipline

The pipelined arm follows two structural disciplines that code review must enforce:

1. **No `return` inside `'write_loop`.** Every exit goes through `break 'write_loop` to the cleanup block. A bare `return` skips channel cleanup and the worker join, leaking the worker thread.

2. **`drop(filled_rx)` AND `drop(free_tx)` before joining.** A worker blocked on `filled_tx.send(...)` is unblocked only by dropping `filled_rx`; a worker blocked on `free_rx.recv()` is unblocked only by dropping `free_tx`. Both states are reachable in practice (the second occurs on error paths where the main thread consumed a buffer without returning it to the pool), so both drops are required before `join()`.

The serial arm has no analogous disciplines — bare `return` is fine because there are no spawned threads or channels to clean up.

### O_DIRECT invariants

Both arms preserve all three `O_DIRECT` invariants from `AGENTS.md`, identical to today's serial code:

1. **Buffer alignment**: each `AlignedBuf` is allocated by `AlignedBuf::new()` with the canonical layout. The worker fills it via `as_mut_slice()` but does not allocate. Alignment is preserved across channel transfer because we move the same heap allocation, not its contents.
2. **Offset alignment**: full-chunk writes happen at `offset` values that are multiples of `BUF_SIZE`. The dispatch arms increment `offset` only inside `ChunkOutcome::Continue` (where `bytes_written == BUF_SIZE`).
3. **Length alignment**: `process_chunk` calls `write_direct` (full BUF_SIZE) only when `filled == BUF_SIZE`. The tail (sub-`BUF_SIZE`) clears `O_DIRECT` first, identical to today.

The worker never touches the device FD, so there is no path by which the worker could violate any of these.

### `AlignedBuf: Send` impl

Currently `AlignedBuf` is `!Send + !Sync` because `NonNull<u8>` is `!Send + !Sync`. To transfer ownership across the channel in the pipelined arm, we must add:

```rust
// SAFETY: AlignedBuf exclusively owns its heap allocation. The pipelined
// arm in flash.rs uses mpsc channels to transfer ownership: at any moment
// exactly one thread holds a given AlignedBuf, with no concurrent access.
// Send across thread boundaries is therefore sound. Sync remains not
// implemented — two threads must not hold a shared reference to the
// same buffer simultaneously.
unsafe impl Send for AlignedBuf {}
```

This impl is already foreshadowed in `.agents/docs/10-aligned-and-ioctls.md`:

> "If a future change crosses the buffer between threads, add `unsafe impl Send for AlignedBuf {}` with a SAFETY comment explaining why exclusive ownership transfer is sound, and audit the call sites."

The doc was written in advance for exactly this case.

The serial arm uses `AlignedBuf` on a single thread and does not exercise the `Send` impl. Adding the impl does not introduce any cost to the serial arm (Send is a marker trait; it produces no runtime code).

### Throttle behaviour

The serial arm's throttle is unchanged from today: per-chunk wall-clock measurement, sleep for the residual.

The pipelined arm's throttle is structurally similar but the timing reference (`t0`) starts _before_ `filled_rx.recv()` rather than before `fill_buffer` — which is correct, because the read happened in parallel and the chunk's wall-clock cost from the main thread's perspective is `recv() + write_direct()`. The throttle thus correctly measures only the costs that are actually serialised.

On throttled pipelined runs, the worker pre-fills the next buffer entirely during the main thread's sleep — decompression is fully absorbed by the throttle. Pipelined throttled flashes therefore see zero CPU-cost from the worker on the wall clock.

### Progress bar

`make_progress_bar(comp, raw_size)` is called once at the top of each arm:

- `flash_serial` calls with `(Compression::Raw, raw_size)` — gets a percent-bar.
- `flash_pipelined` calls with `(<any compressed>, None)` — gets a spinner.

The progress bar's per-chunk update is `pb.set_position(total)`, called by each arm after a successful `process_chunk` return — matching the existing serial code's pattern of separating I/O from UI. `process_chunk` itself does not touch the bar. Bar finalisation (`pb.finish_and_clear()` followed by `println!()` for a clean newline before the next phase) lives in `flash_finalize`. On the cancel or write-error paths in `flash_pipelined`, `pb.abandon()` is called explicitly — leaving the bar's last rendered state on screen as a visible record of the abort.

### Performance characteristics

The dominant performance dimension is throughput, addressed in _Why threading helps_ above. This section catalogs the second-order costs — the sub-millisecond, sub-megabyte concerns that don't move the bottleneck table but accumulate enough to be worth being explicit about.

**Per-chunk overhead introduced by threading (pipelined arm only):**

| Source                                    | Per-chunk cost | Per-second @ 100 MiB/s | Notes                                                                                  |
| ----------------------------------------- | -------------- | ---------------------- | -------------------------------------------------------------------------------------- |
| 4× `mpsc::channel` ops                    | ~200–400 ns    | ~10 µs                 | `Mutex`-based; `crossbeam` would be lock-free for ~free, but absolute saving is noise. |
| 2× `AtomicBool::load(SeqCst)`             | ~50–100 ns     | ~2.5 µs                | Stronger than necessary; `Relaxed` would suffice (see _Cancellation propagation_).     |
| Context switches (worker ↔ main)          | ~1–5 µs (rare) | depends on scheduler   | Mostly overlapped with USB I/O wait; rarely visible on the critical path.              |
| Cache footprint (2× `AlignedBuf` = 8 MiB) | n/a            | n/a                    | Larger than serial's 4 MiB; well within L2 on any system that runs `imi`.              |

Total per-chunk threading overhead: well under 1 µs against ~30–80 ms of useful work per chunk — roughly **0.001%**.

**Per-flash overhead:**

| Source                                              | Cost          |
| --------------------------------------------------- | ------------- |
| `thread::spawn`                                     | ~100 µs once  |
| `Arc::clone` for cancel mirror                      | ~10 ns once   |
| Two `AlignedBuf::new()` (each 4 MiB `alloc_zeroed`) | ~1–10 ms once |

Per-flash overhead is dominated by the buffer allocation, which happens whether or not threading is used (the serial arm allocates one buffer; the pipelined arm allocates two). The actual _threading_ overhead per flash is well under 1 ms.

**Memory:**

- Serial arm: 1× `AlignedBuf` (4 MiB) + the existing `BufReader<File>` (2 MiB). Total: ~6 MiB.
- Pipelined arm: 2× `AlignedBuf` (8 MiB) + worker thread stack (Rust default ~2 MiB) + `BufReader<File>` inside the worker (2 MiB). Total: ~12 MiB.

Both fit comfortably in any operator-class machine. The 8 MiB buffer pool is below L2 on most modern CPUs; the working set of one chunk being read while another is being written stays cache-friendly.

**Architecture choices that prioritize efficiency:**

- **Concrete enum dispatch on `ImageReader`** rather than `Box<dyn Read>` — zero vtable indirection on the hot path of `fill_buffer`.
- **`std::thread::spawn`** rather than an async runtime — no Tokio worker pool overhead, no `spawn_blocking` round-trip on synchronous I/O.
- **By-move ownership transfer through channels** rather than `Arc<Mutex<AlignedBuf>>` — no lock acquisition per chunk, no reference counting on the hot path.
- **Single worker thread** rather than a worker pool — the writer is the bottleneck; additional workers would be idle.
- **2-buffer pool** rather than N-buffer queue — minimum count that pipelines; larger would just add resident memory with zero throughput gain.
- **Helper functions called directly** (no `dyn Trait` objects) — function calls inline in release builds; no vtable lookup.

The only performance compromise the plan makes deliberately is `SeqCst` over `Relaxed` for atomics, traded for clarity. That cost is microseconds-per-flash and is documented above so a future reader can change it if it ever shows up in a profile.

### Single-core and constrained-CPU systems

Threading does not introduce a compatibility floor for `imi`. The minimum kernel `imi` already requires (Linux 2.6.26 for `/proc/self/mountinfo` and `/sys/dev/block/`) is well past the introduction of SMP and POSIX threading in Linux. Any system that meets `imi`'s existing baseline has full `pthread` semantics, futexes, and the rest of the userspace threading machinery.

On single-core hardware (older laptops, low-end embedded boards, single-vCPU VMs), the pipelined arm still works — POSIX threads are a userspace scheduling construct that the kernel time-slices onto whatever CPU it has. There is no `--no-thread` flag, no compile-time feature gate.

What single-core changes is the _magnitude_ of the speedup on compressed images:

| Image type / target    | Multi-core saving | Single-core saving |
| ---------------------- | ----------------- | ------------------ |
| xz on USB 3.0          | ~25–40%           | ~15–25%            |
| bzip2 on USB 3.0       | ~25–50%           | ~15–30%            |
| gzip / zstd on USB 3.0 | <10%              | <5%                |

Raw images go through `flash_serial` regardless and see no change on any hardware.

Two related cases:

- **Heavily-contended multi-core systems** (operator running CPU-heavy work alongside the flash) are closer to the single-core profile. Not a correctness concern.
- **Container or chroot environments where `clone(2)` is restricted** (seccomp filters that block thread creation) could in principle reject `std::thread::spawn` in the pipelined arm. Such environments cannot run `imi` at all because Phase 0 requires root, and root in a container with seccomp blocking `clone` is unusual. Flagging for completeness.

---

## Files to change

### Required source changes

**`src/aligned.rs`** — add `unsafe impl Send for AlignedBuf {}` with the SAFETY comment shown above. The aligned.rs unit test module gets one new test:

```rust
#[test]
fn aligned_buf_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<AlignedBuf>();
}
```

This compile-time assertion guards the `Send` impl: if a future edit accidentally adds a non-`Send` field, the test refuses to compile and surfaces the regression.

**`src/flash.rs`** — the substantive change. Add five new functions and rewrite the existing one as a dispatcher:

1. **`process_chunk(...) -> Result<ChunkOutcome>`** — new shared helper, ~25 lines (the body is small because ENOSPC mapping already lives in `write_direct`/`write_tail`; the helper just dispatches on `filled` and propagates).
2. **`flash_finalize(...) -> Result<()>`** — new shared post-loop helper, ~10 lines.
3. **`flash_serial(...) -> Result<FlashOutcome>`** — new function, ~50 lines. Body is structurally identical to today's `flash::flash`, with the chunk-processing block replaced by `process_chunk` and post-loop work replaced by `flash_finalize`.
4. **`flash_pipelined(...) -> Result<FlashOutcome>`** — new function, ~110 lines including channel setup, worker spawn, the labelled-break main loop, and cleanup.
5. **`worker_loop(...)`** — new function, ~40 lines. Called from `thread::spawn` in `flash_pipelined`.
6. **`flash::flash`** — rewritten as a six-line dispatcher.

Helper types added: `ChunkOutcome` enum, `type FilledItem = Result<(AlignedBuf, usize)>`.

Helper functions preserved unchanged: `fill_buffer`, `write_direct`, `write_tail`, `set_direct`, `make_progress_bar`, `cancellable_sleep`, `is_capacity_error`.

New imports required at the top of `src/flash.rs` (current imports cover `anyhow!`, `bail!`, `Context`, `Result`, `Duration`, `Instant`, `AtomicBool`, `Ordering`, `Read`, `RawFd`, etc. — but the threading additions need):

```rust
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
// std::panic::resume_unwind is referenced via its full path in the
// pipelined arm; no `use` needed for it.
```

Approximate net change in `src/flash.rs`: +220 lines, -65 lines (the current serial body's chunk-processing block, now in the helper).

### Documentation changes

**`AGENTS.md`** — add a threading directive to the style section:

> Threading is permitted in Phase 4's pipelined arm only. Raw images route through `flash_serial`, which is single-threaded; compressed images route through `flash_pipelined`, which spawns a worker thread for decompression. The device FD is held under `O_EXCL`; only the main (writer) thread must ever call `write_all_at` on it. The pipelined arm enforces this structurally: the worker thread receives only `&mut [u8]` slices via `AlignedBuf::as_mut_slice()` and never receives `&FlashGuard` or any handle to the FD. The shared helper `process_chunk` is called from the main thread in both arms; any change to flash-loop semantics (O_DIRECT toggling for the tail, bytes-written accounting) belongs there, fixed once and benefiting both arms. ENOSPC mapping continues to live in `write_direct`/`write_tail` (not in `process_chunk`); the helper just propagates the typed error.

**`.agents/docs/05-phase-4-flash.md`** — extend with two new subsections:

- **"Dispatch and shared helper"** — describe the dispatch on `comp.is_compressed()` and the role of `process_chunk` / `flash_finalize`. Include the component diagram from this plan.
- **"Pipelined arm"** — describe the channel design, the cancel-mirror pattern, the join-on-every-exit-path discipline, the worker-panic propagation. Include the data-flow diagram from this plan.

The existing `O_DIRECT` invariants section is preserved verbatim — those invariants apply to `process_chunk` and are unchanged.

**`.agents/docs/10-aligned-and-ioctls.md`** — update the "Send/Sync deliberately not implemented" subsection to reflect that `Send` is now implemented, and explain the exclusivity contract (channel-mediated ownership transfer in `flash_pipelined`). `Sync` remains unimplemented; that is unchanged. The forward-compatibility note rewrites to describe the _current_ design.

**`README.md`** — one paragraph in a new "Compressed images" subsection:

> For compressed images (gzip, xz, bzip2, zstd), `imi` overlaps decompression with the device write on a worker thread. Pipelining shrinks total flash time when decompression is slow relative to the device write. Expected wall-clock improvement on USB 3.0 is ~25–40% for xz, ~25–50% for bzip2, and under 10% for gzip and zstd. On USB 2.0 the write dominates so percentage savings are smaller, but xz and bzip2 still recover meaningful absolute time (~5–22 seconds per GiB depending on input). Raw images do not go through this path — the disk read is sub-1% of chunk time, and adding a thread would add overhead with no measurable gain. The exact speedup depends on CPU, image content, and USB controller.

### Cargo.toml

Add a comment at the decompressor dep lines:

```toml
# DO NOT enable parallel features without re-doing the threading analysis
# in threading-plan.md — the pipelined arm assumes single-threaded decoders.
flate2 = "1.0"
xz2 = "0.1"
bzip2 = "0.4"
zstd = "0.13"
```

### Out of scope for this commit

- **Threading optimization of Phase 5b verify.** Verify itself runs on every flash exactly as today, single-threaded, byte-for-byte comparing the device against the image. The threading-plan change for _its internal loop_ — overlapping decompression with the device read, mirroring this plan's Phase 4 architecture — is a separate diff, landed after Phase 4 threading has soaked in production. The Phase 5b benefit from threading is somewhat smaller because the verify's USB-read path is typically 1.5–2× faster than the flash's USB-write path, so the read-vs-decompress overlap recovers less wall-clock time (~15–25% for xz/bzip2 verify, vs ~25–40% for xz/bzip2 flash).
- **Multi-worker decompression.** A single worker suffices because the writer is the bottleneck. Adding workers above one does nothing.
- **Async/await.** `std::thread` is the right primitive for this work shape (synchronous-CPU on worker, synchronous-IO on main).
- **Routing raw images through the pipeline.** Explicitly rejected in _Why dispatch with shared helper_. Do not reconsider without re-running the analysis.

---

## Implementation order

A merge-able sequence with intermediate checkpoints. Each step is independently committable; do not collapse them. The order is chosen so each step is behaviour-preserving on its own (Steps 1 and 2) or behaviour-changing in a contained way (Step 3).

### Step 1: Extract `process_chunk` and `flash_finalize` from current serial code

**Pure refactor.** Replace the chunk-processing block inside today's `flash::flash` with a call to a new private `process_chunk` function that contains the same logic. Replace the post-loop `fdatasync` / `set_direct(fd, false)` / `pb.finish_and_clear()` / `println!()` calls with a single call to `flash_finalize`. The per-iteration `pb.set_position(total)` stays in the loop body (now after the `process_chunk` call instead of after the inline write logic). The `flash::flash` body still has its current loop structure and threading model — single-threaded, single buffer. No behavioural change. All 85 existing tests pass.

This step is the safety net. It introduces the shared helpers in a context where they are exercised by the existing test surface, and it lands a cleaner factoring of `flash::flash` even if Steps 2 and 3 are later abandoned.

Acceptance: tests 1–5 from the _Tests_ section (process_chunk full/tail/empty/enospc and flash_finalize order) land in this commit and pass. `cargo test` passes 90 tests total (85 existing + 5 new); `cargo clippy` produces no new lints; the `flashrs-review` skill produces no new findings.

### Step 2: Add `unsafe impl Send for AlignedBuf` and the compile-time test

**Pure additive.** Add the impl and the `aligned_buf_is_send` test. Update `.agents/docs/10-aligned-and-ioctls.md` to reflect the change. Run `cargo test`.

This is the smallest possible change that exposes the new capability. It is independently committable because it does not yet have any caller. If anything later proves problematic, this step can stay landed without harm — the impl just becomes unused.

Acceptance: `cargo test` passes 91 tests (90 from after Step 1 + 1 new); the `Send` test compiles only because of the impl.

### Step 3: Add `flash_pipelined`, `worker_loop`, and dispatch

**The substantive commit.** Add `flash_pipelined`, `worker_loop`, and the `FilledItem` type alias. The `ChunkOutcome` enum already exists (added in Step 1 alongside `process_chunk`), so it is just _used_ here, not defined. Move today's `flash::flash` body (the Step-1-refactored loop that calls `process_chunk` and `flash_finalize`) verbatim into a new private function `flash_serial` with the appropriate signature. Then rewrite `flash::flash` itself as a six-line dispatcher that calls either `flash_serial` or `flash_pipelined` based on `comp.is_compressed()`. The body of `flash_serial` should require zero logic changes from what `flash::flash` was at end-of-Step-1 — it's a pure code-motion operation.

Add the unit tests described in _Tests_. Update AGENTS.md, the Phase-4 doc, and README in the same commit.

Run `cargo test` (existing tests now exercise `flash_serial` for raw and `flash_pipelined` for compressed; both must pass). Run `cargo +1.85 build --locked` (MSRV check). **Hardware-test before merge** (see _Required hardware tests_).

Acceptance: tests 6–11 from the _Tests_ section land in this commit. `cargo test` passes ≥97 tests (91 from after Step 2 + 6 new items in Step 3; the actual `#[test]` count may be slightly higher because items 4 and 7 are written as multiple sub-tests in the test module); the dispatcher routes correctly; both arms exercise `process_chunk` with parity (test 11).

### Step 4 (deferred to a separate PR, scheduled after Step 3 soaks): Thread the Phase 5b verify loop

Verify continues running on every flash regardless of whether this step ever lands; the verify phase itself is non-negotiable. What this step adds is the threading optimization of verify's internal read loop — overlapping decompression with the device read, mirroring Step 3's architecture for the flash loop.

The same `Send` impl, channel pattern, and cancel-mirror discipline from Step 3 apply directly. A `verify_chunk` helper paralleling `process_chunk` would hold the per-chunk comparison logic; both the serial and pipelined verify arms would call it. The estimated wall-clock saving is somewhat smaller than for Phase 4 (~15–25% for xz/bzip2 verify on USB 3.0) because USB read is faster than USB write on most devices.

Land this step only after Step 3 has soaked in production for at least one release tag without threading-related regressions reported. The "soak" is what earns the trust to apply the same pattern a second time.

---

## Pitfalls (and how to avoid each)

The earlier exploration in this codebase hit several of these. Listing them so the implementer doesn't repeat history.

Pitfalls fall into three groups: **shared** (apply to both arms because they live in `process_chunk` or in shared infrastructure), **pipelined-only** (apply only inside `flash_pipelined`), and **dispatch-specific** (apply at the architectural boundary).

### [Shared] Tail-buffer use-after-drop

**Pitfall:** the tail-write path inside `process_chunk` drops `O_DIRECT`, then writes a sub-`BUF_SIZE` slice. If the helper drops or moves `buf` before calling `write_tail`, the slice points to freed memory. Earlier exploration had this exact bug.

**Fix:** `process_chunk` takes `buf: &AlignedBuf` (borrow, not owned), so the borrow checker enforces that `buf` is alive across the `write_tail` call automatically. The slice `&buf.as_slice()[..filled]` is bounded by the function scope. The helper cannot drop `buf` because it doesn't own it. This pitfall is structurally prevented by the helper's signature.

### [Shared] ENOSPC mapping must propagate cleanly

**Pitfall:** today's `write_direct` and `write_tail` helpers map `ErrorKind::StorageFull | ErrorKind::WriteZero` (i.e. ENOSPC) to a specific anyhow message ("device ran out of space..."). If a refactor of either helper broke this mapping — or if `process_chunk` accidentally re-wrapped the error with a generic `.context()` that buried the diagnostic — operators would see "input/output error at offset 12345" instead of "device too small for image."

**Fix:** keep the ENOSPC mapping in `write_direct` and `write_tail` exactly as today; `process_chunk` invokes them directly with `?` propagation and adds **no** extra `.context()` wrapping. Anyhow's chain preserves the diagnostic message at the root of the chain. Cross-cutting tests (test 4 in _Tests_) verify the diagnostic propagates through `process_chunk` to its caller without being lost.

### [Pipelined] Cancel-flag lifetime confusion

**Pitfall:** trying to share `cancel: &AtomicBool` directly with the worker thread. The borrow checker rejects this (non-`'static` reference); attempts to wrap with `unsafe { transmute }` are unsound — the parent stack frame may unwind before the thread does.

**Fix:** the cancel-mirror pattern. Local `Arc<AtomicBool>` for the worker; main thread mirrors the parent flag into it. Do **not** make `flash::flash` (or `flash_pipelined`) take `Arc<AtomicBool>` directly — that ripples up the call stack and forces other phases to also accept Arc, which is gratuitous.

### [Pipelined] Channel-drop ordering at shutdown

**Pitfall:** the main thread detects an error or cancel, returns immediately without dropping its channel halves. The worker, blocked in `free_rx.recv()` _or_ in `filled_tx.send(...)`, never wakes (because the corresponding endpoint is still held by the main's local stack frame). `join()` deadlocks.

The two blocking states are _both_ reachable in practice:

- **Worker blocked on `filled_tx.send(...)`**: the worker has just filled a buffer and is trying to hand it to the main thread, but the main thread has stopped recv'ing.
- **Worker blocked on `free_rx.recv()`**: the worker has just sent a buffer and is asking for the next free one, but the pool is empty (the main thread has consumed both buffers — e.g., one in flight to filled_rx, one being processed by `process_chunk` — and broken the loop without returning either via `free_tx.send(buf)`). This happens on the ENOSPC path (drop(buf) on Err) and possibly on the cancel path.

**Fix:** before joining, drop **both** main-side channel halves: `drop(filled_rx)` _and_ `drop(free_tx)`. The first unblocks workers stuck on send; the second unblocks workers stuck on recv. The labelled `'write_loop` with single post-loop cleanup block makes this discipline structural — every exit goes through the same drops in the same order.

Code review on diffs touching this cleanup must enforce: any change that adds a channel must also extend the cleanup block to drop the main's endpoint of that channel before joining. The Rust compiler will not catch a missed drop.

### [Pipelined] Worker-thread error wrapping

**Pitfall:** the worker forwards `io::Error` from `fill_buffer` bare. The main thread receives an error with no context; the operator's chain reads "early EOF" with no clue where it happened.

**Fix:** before `filled_tx.send(Err(...))`, wrap with `.context("reading from compressed image stream")`. The standard pattern matches the rest of the codebase. The main thread's `?` then surfaces the full chain.

### [Pipelined] Spurious throttle re-check after cancel

**Pitfall:** the throttle's `cancellable_sleep` returns when the parent cancel is set. The main thread must then re-check the parent flag and break the loop, not attempt another `filled_rx.recv()` (which would block waiting for a chunk the worker will never send).

**Fix:** after every `cancellable_sleep`, re-check `cancel.load(...)` and break if set. The pipelined-arm skeleton above shows the pattern; preserve it in the implementation.

### [Pipelined] Worker thread accidentally outlives `flash_pipelined`

**Pitfall:** the main thread returns `Ok(...)` without calling `join()` on the worker handle. The worker continues running; the `Arc<AtomicBool>` keeps it alive past the function boundary; weird intermittent failures in subsequent phases.

**Fix (normal exit paths):** the cleanup block makes join structural — `if let Some(handle) = worker_handle_taken.take() { let _ = handle.join(); }` after the labelled-break loop covers all exits. Code review must verify no diff adds a `return` inside `'write_loop`.

**Edge case (panic mid-loop):** if `process_chunk` itself panics (or any code between the worker spawn and the cleanup block), the function unwinds and the cleanup block does not execute. `worker_handle_taken` drops as a local — and `JoinHandle::drop` _detaches_ the thread rather than joining it. The worker continues running until it observes the channel halves drop (which happens during unwind as `filled_rx`/`free_tx` go out of scope) and exits naturally.

This detached-worker case is not a correctness violation: the worker only holds `Arc<AtomicBool>` and (potentially) a single `AlignedBuf`; it cannot touch the FD. By the time `FlashGuard::Drop` prints FATAL, the worker may still be running for a few more milliseconds, but its work is harmless. The Arc keeps the AtomicBool allocation alive until the worker exits, then everything deallocates cleanly. Acceptable behaviour for a panic-during-destructive-pipeline scenario.

A `JoinGuard` wrapper that joins on `Drop` was considered and rejected: it would block unwinding (potentially indefinitely if the worker is itself unwinding through a panic) and could turn a single panic into a deadlock. Detach-on-panic is the safer choice for this codebase.

### [Pipelined] Mid-flash panic in the worker

**Pitfall:** the worker panics (e.g. arithmetic overflow in a decompressor library on malformed input). The main thread's `filled_rx.recv()` returns `Err(_)` because the channel disconnected. Without explicit handling, the main treats this as "worker is done normally" and returns `Ok(FlashOutcome { bytes_written })` — but the device has only a partial image.

**Fix:** distinguish "worker exited cleanly" from "worker panicked." On `filled_rx.recv() → Err(RecvError)`, call `worker_handle.join()` and capture any panic into `worker_panic: Option<Box<dyn Any + Send>>`. After the cleanup block runs, `resume_unwind` the captured panic. `FlashGuard::Drop` runs during the unwind and prints FATAL — which is the correct behaviour for a destructive pipeline panicking mid-flash.

### [Shared] Decompressor crate's internal threading

**Pitfall:** `xz2` or `zstd` may internally spawn worker threads for parallel decompression. If the project later enables those features, the worker count multiplies and the bottleneck shifts unexpectedly.

**Fix:** add a comment in `Cargo.toml` at the relevant dep lines: `# DO NOT enable parallel features without re-doing the threading analysis in threading-plan.md`. This concern applies regardless of pipeline architecture.

### [Shared] `Send` impl regression on `AlignedBuf`

**Pitfall:** a future change adds a non-`Send` field to `AlignedBuf` (e.g. `Rc<...>`). The `unsafe impl Send` becomes unsound. The compiler does not warn — `unsafe impl` is a developer's promise.

**Fix:** the compile-time test `aligned_buf_is_send` is the guard. `fn assert_send<T: Send>() {} assert_send::<AlignedBuf>();` enforces structurally — if `AlignedBuf` becomes non-`Send` despite the `unsafe impl`, the trait bound conflict surfaces.

### [Dispatch] Drift between dispatch arms

**Pitfall:** a future change adds a step to `flash_serial` (e.g. a metrics counter, a new validation check) and forgets to add it to `flash_pipelined`. The arms diverge silently. Over time, behaviour differs between raw and compressed flashes in ways nobody notices until an operator hits the difference.

**Fix:** three layers of mitigation, in increasing strength:

1. **Code review discipline.** Any diff touching `flash_serial` or `flash_pipelined` must also touch the other, or explicitly justify why not. The reviewer should ask: "could this logic live in `process_chunk` or `flash_finalize` instead?" If yes, push it down.

2. **Push new logic into shared helpers when possible.** `process_chunk` and `flash_finalize` are the primary structural mitigation. Any per-chunk concern (validation, accounting, hashing) should live in `process_chunk`; any pre-flash or post-flash concern should live in a sibling helper alongside `flash_finalize`.

3. **Parity test.** A unit test that runs the same input through both arms (against a mock `FlashGuard` and a stub `ImageReader`) and verifies the resulting `FlashOutcome` and side effects (mock write log) are identical. See _Tests, item 11_. This catches drift at PR time even when the code-review discipline misses.

### [Dispatch] Dispatch decision based on stale state

**Pitfall:** the dispatcher branches on `comp.is_compressed()`, where `comp` is computed at Phase 0 from magic-byte detection. If a future change re-detects compression mid-stream (e.g. a multi-stream archive format), or if the dispatcher is called with `Compression::Raw` for a file that's actually compressed, raw bytes get pushed through the serial arm and written to the device verbatim — visibly broken on first boot.

**Fix:** the dispatch decision is made once at function entry and is final for the duration of the flash. There is no mid-flash re-detection. The Phase 0 compression detection is the single source of truth, and `flash::flash` trusts its caller to pass an `ImageReader` consistent with the `Compression` value. This is already the contract today; the dispatch change does not weaken it.

If a future change introduces multi-stream archives or content-aware re-detection, the dispatch design must be revisited — but that is a separate change with its own threading-plan-equivalent document.

---

## Tests

### Existing tests that must continue to pass

All 85 existing tests, unchanged.

After Step 1 (extract helpers), every test that exercises `flash::flash` for raw images now exercises `process_chunk` and `flash_finalize` indirectly. After Step 3, those same tests exercise `flash_serial`. No test changes are needed.

If any existing test fails after Step 1 or Step 3, that is a real regression in the refactor or pipeline implementation, not a test that needs updating.

### New tests — `src/aligned.rs`

```rust
#[test]
fn aligned_buf_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<AlignedBuf>();
}
```

Trivial but load-bearing. Catches the "future field accidentally makes the type !Send" regression.

### New tests — `src/flash.rs::tests`

The tests below are the complete proposed list. Each is described with its scope (what it verifies) and its boundary (what it does not verify).

**Mock `FlashGuard` infrastructure (prerequisite).** Tests 1–11 below all assume a way to observe `pwrite`/`set_direct`/`fdatasync` calls without touching real hardware. **This infrastructure does not exist in the project today** — existing tests use `tempfile`-backed real `File`s. Building it is part of Step 1's work, not separately scoped, and is non-trivial.

Two approaches:

- **(a) Trait abstraction.** Define a trait `FlashGuardOps` that both the real `FlashGuard` and a mock implement; change `process_chunk` and `flash_finalize` signatures from `&FlashGuard` to `&dyn FlashGuardOps` (or a generic `&G: FlashGuardOps` to stay zero-cost). This cleanly separates production from test code but requires changing the helper signatures shown elsewhere in this plan. Pure dispatch-cost: ~1 ns per `dyn` call (negligible).

- **(b) Tempfile + observation wrapper.** Keep `&FlashGuard` signatures unchanged. Tests construct a real `FlashGuard` over a tempfile, call the helpers, then read the tempfile's contents to verify what was written. ENOSPC tests use a tempfile on a small tmpfs or an ftruncate-shrunk file. set_direct tests check the FD's flags via fcntl. This couples tests to real-FS behaviour but doesn't change the production signatures.

**The plan's code sketches (signatures, helpers, test descriptions) assume approach (b)** — `process_chunk` takes `&FlashGuard`, not `&dyn FlashGuardOps`. If the implementer chooses approach (a), they must update the helper signatures consistently across the plan and the implementation. Approach (b) is recommended because it requires fewer changes and matches the project's existing tempfile-based test style.

The +220 LoC source-change estimate in _Files to change_ does **not** include this test infrastructure; expect another ~150–250 lines of test scaffolding under approach (b), or ~100 lines plus signature edits under approach (a).

**1. `process_chunk_full_chunk_writes_buf_size_at_offset`**
A test that constructs a mock `FlashGuard` (capturing `pwrite` calls), allocates an `AlignedBuf`, fills it with a known pattern, and calls `process_chunk` with `filled = BUF_SIZE`. Verifies:

- Returns `Ok(ChunkOutcome::Continue { bytes_written: BUF_SIZE as u64 })`.
- The mock recorded exactly one `pwrite` of `BUF_SIZE` bytes at the given offset.
- The FD remains in `O_DIRECT` mode (no `set_direct` calls were issued).

Scope: full-chunk path of the helper. Does not test threading, dispatch, or progress-bar updates (those live in callers).

**2. `process_chunk_tail_writes_partial_with_o_direct_off`**
Same setup but `filled = 1234` (sub-`BUF_SIZE`). Verifies:

- Returns `Ok(ChunkOutcome::Done { bytes_written: 1234 })`.
- The mock recorded exactly one `set_direct(fd, false)` call followed by one `pwrite` of 1234 bytes.
- The bytes written exactly match `buf.as_slice()[..1234]` (catches the use-after-drop pitfall — if the helper's lifetime contract were wrong, the slice would be reading freed memory and the bytes would be undefined).

Scope: tail path of the helper. Critical for the use-after-drop pitfall.

**3. `process_chunk_empty_signals_done_without_write`**
`filled = 0`. Verifies:

- Returns `Ok(ChunkOutcome::Done { bytes_written: 0 })`.
- The mock recorded no `pwrite` calls and no `set_direct` calls — the helper short-circuited cleanly.

Scope: empty-input path. Catches a regression where empty input triggers a spurious zero-byte write.

**4. `process_chunk_propagates_enospc_diagnostic`**
`filled = BUF_SIZE`, mock `pwrite` configured to return `ENOSPC`. Verifies:

- The returned error's chain contains "ran out of space" and the offset.
- A second sub-test with `filled = 1234` and ENOSPC on the tail write verifies the tail path also surfaces the diagnostic.

Scope: error propagation through the helper. Note that the ENOSPC-to-message mapping itself lives inside `write_direct`/`write_tail` (existing project code, already tested separately); this test verifies that `process_chunk` propagates the typed error without losing context.

**5. `flash_finalize_runs_steps_in_order`**
Test that constructs a mock `FlashGuard` and verifies `flash_finalize` issues, in order: `fdatasync` on the file → `set_direct(fd, false)` → `pb.finish_and_clear()` → `println!()` (a clean newline so the bar's last position doesn't blend with the next phase's text).

Scope: shared post-loop helper. The order matters because `fdatasync` flushes the device; `set_direct` resets FD state for subsequent phases; the bar finalisation and newline are operator-visible UI cleanup.

**6. Channel-driven shutdown ordering (pipelined arm)**
A test fixture simulating the main-thread state machine using two `mpsc::channel`s and a stub "worker" closure. Verifies both directions of the shutdown protocol:

- main dropping `filled_rx` causes the stub worker's `filled_tx.send(...)` to fail (worker exits within one iteration on the send-blocked path).
- main dropping `free_tx` causes the stub worker's `free_rx.recv()` to fail (worker exits within one iteration on the recv-blocked path).

Both paths are required because the worker can be blocked on either operation depending on timing; the cleanup block must drop both endpoints to avoid deadlock.

**7. Cancel-mirror semantics (pipelined arm)**
Two related tests:

- `cancel_mirror_propagates_within_one_main_iteration`: parent flag set → main observes → main stores into local mirror, all within one iteration.
- `cancel_mirror_causes_worker_exit_within_one_fill_iteration`: local mirror set → worker observes on next iteration entry → returns.

Scope: the cancellation propagation discipline of `flash_pipelined`. Tests the protocol via stub closures, not via real threads.

**8. Worker-error propagation (pipelined arm)**
A test fixture injecting a `Read` impl that errors on its second `read` call. Since `ImageReader` is a concrete enum with file-backed variants, the implementer has two options for the injection: either (a) wrap a tempfile of malformed compressed data in the matching `ImageReader::Gzip` / `Xz` / `Bz2` / `Zstd` variant so the decoder's `read` returns an error after consuming a partial header (most decoders fail fast on malformed framing), or (b) add a `#[cfg(test)] Mock(Box<dyn Read + Send>)` variant to `ImageReader` purely for tests. Option (a) matches the project's existing tempfile-style tests; option (b) is more flexible but expands the enum's surface. Pick whichever is less invasive.

Runs `flash_pipelined` against a mock `FlashGuard`. Verifies:

- The error reaches the main thread with `"reading from compressed image stream"` context attached.
- The mock `FlashGuard` recorded exactly one full-chunk `pwrite` (the first chunk succeeded before the error).
- The worker thread joined cleanly; no panic, no orphan.

Scope: worker-error path of the pipelined arm.

**9. Worker-panic propagation (pipelined arm)**
A test fixture injecting a `Read` impl that panics on its second `read` call. Same construction options as test 8 — but option (b) (the `Mock` variant) is more straightforward here because triggering a _panic_ (not just an error) from inside a real decoder is harder to engineer reliably from malformed data alone. Verifies:

- `flash_pipelined` propagates the panic via `resume_unwind`.
- A surrounding `std::panic::catch_unwind` in the test detects it.
- The mock `FlashGuard`'s `pwrite` log shows partial state (first chunk written, no further).

Scope: panic-propagation path. Critical: this is the test that prevents the "main treats panic as clean EOF" pitfall.

**10. Round-trip under throttle (pipelined arm)**
Integration-level: runs `flash_pipelined` against an in-memory writable buffer (mock `FlashGuard`), with a small image and a slow throttle (`Some(1024 * 1024)` for 1 MiB/s). Verifies:

- Bytes-written count equals the image length.
- Wall-clock time is approximately `image_size / rate` (with reasonable CI-tolerable slack).

Scope: throttle semantics survive the pipelined refactor.

**11. Dispatch parity test**
Runs the same input (a known byte pattern, both as raw `ImageReader::Raw(...)` and as a compressed wrapper) through `flash_serial` and `flash_pipelined` separately, against fresh mock `FlashGuard` instances. Verifies:

- Both produce `FlashOutcome` with identical `bytes_written`.
- Both mock guards recorded `pwrite` calls with identical (offset, length, payload) sequences.
- Both ended with `fdatasync` followed by `set_direct(fd, false)` followed by a finished progress bar.

Scope: the dispatch-drift pitfall. This is the structural test that catches divergence between arms.

### New tests — total count

| File                  | New tests                                                                                  |
| --------------------- | ------------------------------------------------------------------------------------------ |
| `src/aligned.rs`      | 1                                                                                          |
| `src/flash.rs::tests` | 11 (items 1–11 above; some of those items contain sub-tests, total assert sites is higher) |
| **Total new**         | **12**                                                                                     |

Total project tests after Step 3: 85 + 12 = **97**.

### Hardware tests (manual, but blocking on merge)

Unit tests verify the _protocol_ and the _helper invariants_. Hardware tests verify the _system_. Both required.

**Required minimal hardware test** before merging Step 3:

1. Flash a known-good xz-compressed Linux ISO (Arch, Alpine) to a scratch USB stick using the pipelined arm. Verify completes; boot the stick; OS comes up.
2. Flash a known-good _raw_ `.img` (e.g. raw Raspberry Pi OS) to a scratch USB stick using the serial arm. Verify completes; partition layout matches expectations post-BLKRRPART.
3. Re-run with `--skip-verification` for both compressed and raw inputs; verify the cooldown still runs and the FATAL warning still fires on Ctrl+C mid-flash.
4. Re-run with `--throttle 4M` for both; verify throttling produces accurate rate-limiting in both arms. Time it; expected wall-clock is `image_size / 4 MiB/s` plus cooldown and verify.
5. Cancel mid-flash with Ctrl+C in both arms; verify the FATAL message names the correct phase verb and the exit is responsive (within ~1 second of Ctrl+C).

**Optional but recommended:**

- Run the same suite on a USB 2.0 device.
- Run on a single-core or single-vCPU VM to confirm correctness on constrained-CPU systems.
- Stress test: flash a large bzip2 image (≥4 GiB decompressed) to verify no resource leaks (file descriptors, memory, threads) over a long pipeline. Watch with `strace -e trace=clone,close` to confirm exactly three threads are alive during Phase 4: main + worker + the `ctrlc` library's signal-handler thread (which exists since startup, not specific to Phase 4).

### Tests that should _not_ be added

- **"Verify the pipeline is faster than the serial path."** Wall-clock comparison tests are flaky and depend on CI hardware. The benefit is documented in the architecture; the test surface should verify _correctness_, not _performance_.
- **"Verify the pipeline does not deadlock."** Hard to test definitively. The architectural review (channel-drop ordering, join-on-every-exit) is the correctness argument.
- **"Verify the worker and main threads do not race."** Rust's type system already enforces this for buffer ownership transfer (channel sends move). For the cancel flag, `AtomicBool` is `Sync` by definition.

---

## Safety analysis

### `unsafe` surface introduced

Exactly one new `unsafe` site: `unsafe impl Send for AlignedBuf {}` in `src/aligned.rs`. (This is an `unsafe impl` rather than an `unsafe { ... }` block, but AGENTS.md rule 10's intent — every use of the `unsafe` keyword carries a `// SAFETY:` comment — applies the same way; the comment shown above satisfies it.)

The SAFETY comment names two invariants:

1. `AlignedBuf` exclusively owns its heap allocation (no shared pointers, no `Rc`, no `&'a` borrows held alongside the owner).
2. The pipelined arm transfers ownership via `mpsc::channel`, which moves the value rather than copying it; at any moment exactly one thread holds a given `AlignedBuf`.

Both invariants are observable in code:

- The first by inspecting `AlignedBuf`'s fields (currently `NonNull<u8>` + `Layout`; both are `Copy`/`Sized` types with no aliasing concerns).
- The second by inspecting the channel call sites in `flash_pipelined`: every `send` is preceded by the sender no longer using the buffer, every `recv` returns ownership.

### `unsafe` surface NOT introduced

The plan does **not** require:

- `unsafe impl Sync for AlignedBuf` — and we should NOT add it. Sync would mean two threads can hold a `&AlignedBuf` simultaneously. The pipeline never does this; channel transfer is by-move, not by-share.
- Any new `unsafe` block or `unsafe impl` in `flash.rs`. The dispatch arms, helpers, and worker loop use safe channel operations and safe ownership transfers throughout.
- Raw pointer arithmetic, FFI, or transmutes beyond the existing `AlignedBuf` allocation/dealloc.
- Any new ioctl, signal handler, or kernel-interface code.

### Non-`unsafe` correctness invariants

These are upheld by the type system, helper signatures, and code review — not by any `unsafe` block:

- **Single-threaded FD access.** Only the main thread calls `guard.file()`, `process_chunk` (which calls `write_direct`/`write_tail`/`set_direct`), or `flash_finalize` (which calls `fdatasync`/`set_direct`). The worker is statically prevented from these because it never receives `&FlashGuard` (the channels carry `AlignedBuf`, not FD handles).
- **Single-threaded ImageReader access.** The worker owns the `ImageReader` by-move; the main thread no longer references it after spawning. The serial arm has only one thread.
- **Cancel parent flag access.** Only the main thread reads the parent `cancel: &AtomicBool`. The worker only reads the local mirror.
- **AlignedBuf allocation-and-deallocation.** `AlignedBuf::new()` is called on the main thread (during seeding in the pipelined arm; once at start in the serial arm). `Drop::drop` may run on either thread depending on which holds the buffer at function end — fine because `dealloc` is thread-safe per the global allocator's contract.

### AGENTS.md hard-rule compliance

Walking through each hard rule:

1. **Zero external binaries.** Unchanged. Neither arm calls a subprocess.
2. **Phase ordering canonical.** Unchanged. Phase 4 still runs between Phase 3 (wipe) and Phase 5a (cooldown). The internal restructuring of Phase 4 doesn't reorder phases.
3. **`O_DIRECT` invariants.** Preserved (see _O_DIRECT invariants_ in Architecture). Both arms call `process_chunk`, which enforces all three invariants identically.
4. **Never `O_SYNC` via `F_SETFL`.** Unchanged. The helpers use the same `set_direct` helper.
5. **`FlashGuard` lifecycle.** Unchanged. The guard is armed/transitioned/disarmed exactly as today; only the main thread interacts with the guard. The worker never touches it.
6. **`ctrlc` handler must never `exit()`.** Unchanged. The handler still flips `cancel: AtomicBool`; the main thread's loop-iteration check still drives normal unwind. The cancel-mirror to `local_cancel` is an _additional_ propagation in the pipelined arm, not a replacement.
7. **Verification under `O_EXCL`.** Unchanged. Phase 5b still happens before `guard.into_file()` releases the FD.
8. **`/media` whitelist with sentinel.** Unchanged. Phase 1 logic is untouched.
9. **Devt-not-string correlation.** Unchanged. `TargetDevts` is unaffected.
10. **`SAFETY` comments mandatory.** Preserved. The new `unsafe impl Send` has its SAFETY comment.

No hard rule is relaxed. The internal structure of Phase 4 changes; the contract Phase 4 satisfies does not.

### Forward-compatibility for further threading

If a future change wants to thread the Phase 5b verify loop (the planned follow-up — verify itself continues running unchanged either way; only its internal threading model would change), the same `Send` impl, the same channel pattern, and the same cancel-mirror discipline apply. A shared `verify_chunk` helper paralleling `process_chunk` would hold the per-chunk comparison logic. No additional `unsafe` is needed.

If a future change wants something more aggressive — e.g. multiple decompressor workers feeding a single writer — the analysis is different and this plan is _not_ sufficient. That kind of change requires its own threading-plan.md document and its own architectural review.

---

## Rollback plan

If hardware testing reveals an issue with the pipelined arm:

1. **Per-commit rollback.** Each step in _Implementation order_ is independently revertable.
   - Step 1 (extract helpers): reverting returns to today's inline serial loop. Helpers go away.
   - Step 2 (Send impl): reverting just removes the impl and test. Step 3 must be reverted first if it has been merged.
   - Step 3 (pipelined arm + dispatch): reverting collapses the dispatcher back to a single function, leaves `flash_serial` unused or renamed back to `flash`, and removes the pipelined arm. Helpers from Step 1 stay landed.

2. **Targeted rollback of dispatch only.** If Steps 1 and 2 prove fine but Step 3's pipelined arm has issues, an intermediate revert is possible: keep `flash_serial` as the only callable arm and route compressed images through it too. This is a one-line change in the dispatcher (`if false {` instead of `if comp.is_compressed() {`). It works because `flash_serial`'s `fill_buffer` call accepts any `Read` (the decompressor handles on-the-fly decompression transparently), and `make_progress_bar(Compression::Raw, None)` falls through to the spinner variant — exactly what the pipelined arm would have shown. The flashed bytes are bit-identical between the two arms; only the timing differs (no decompress/write overlap). Effectively a feature-flag-equivalent escape that costs one line and doesn't require maintaining a parallel implementation. _This is a strictly better rollback profile than full unification, which had no escape._

3. **Bug-fix forward.** Most likely scenario. If hardware testing reveals a specific bug (channel-ordering, throttle interaction, decompressor-edge-case, parity divergence between arms), fix it in a follow-up commit and re-test. The bug-fix is smaller than rollback.

---

## Acceptance criteria

The dispatch path is ready to merge when ALL of:

- [ ] `cargo build --release` succeeds with zero warnings.
- [ ] `cargo test` passes all existing 85 tests + 12 new tests = ≥97 total.
- [ ] `cargo +1.85 build --locked` succeeds (MSRV check, per AGENTS.md).
- [ ] `cargo clippy -- -D warnings` produces no new lints.
- [ ] At least one xz-compressed real Linux ISO has been flashed via the pipelined arm to a scratch USB stick, and the resulting USB stick boots.
- [ ] At least one raw `.img` (e.g. Raspberry Pi OS) has been flashed via the serial arm and boots correctly.
- [ ] `--skip-verification` and `--throttle` paths have been individually exercised on both raw and compressed inputs.
- [ ] A mid-flash Ctrl+C produces the FATAL warning with the correct phase verb on both raw and compressed inputs, with sub-second responsiveness.
- [ ] The dispatch parity test (test 11) passes — both arms produce identical `FlashOutcome` and side effects for the same input.
- [ ] AGENTS.md, the Phase-4 doc, the aligned-and-ioctls doc, README, and Cargo.toml comments have been updated in the same commit as the code change.
- [ ] The `flashrs-review` skill has been run on the diff and produced no `[CRITICAL]` / `[HIGH]` findings; any `[MEDIUM]` findings have been addressed.
- [ ] At least one human reviewer (other than the author) has signed off, having read this plan and the diff together.

If any acceptance criterion is not met, the change is not ready.

---

## Appendix: alternatives considered

### Alternative 1: Don't thread at all

Keep today's serial flash loop. xz flashes ~25–40% slower than they could on USB 3.0; bzip2 ~25–50% slower.

The case against: substantial speedup on the formats distros use most, achievable with one new `unsafe impl Send` and a contained set of new code. Operator-visible.

### Alternative 2: Full unification — thread all images through one pipelined function

The previous plan iteration. Single function handles raw and compressed; raw images pay a small overhead (thread spawn, channel ops) for ~zero gain.

The case against:

- Single function ≠ single mental model. A reviewer auditing a unified pipelined `flash::flash` carries threading semantics into every audit, even when reviewing changes that affect only raw flashing.
- Blast-radius asymmetry. A bug in the threading code under unification hits all flashes, including the most-common case (raw). Under dispatch, the same bug only hits compressed flashes.
- The "two diffs for every fix" cost that motivates unification is largely solved by the shared helper. `process_chunk` and `flash_finalize` cover the loop body and post-loop work; the dispatch arms are thin shells.

### Alternative 3: Pure dispatch — two parallel implementations, no shared helper

Each arm contains its own copy of the loop body. This is the configuration that genuinely doubles future-refactor cost.

The case against: the shared helper resolves the maintenance concern at the cost of one extra function. Pure dispatch is strictly worse than dispatch-with-helper.

### Alternative 4: Multiple decompressor workers

Two or more worker threads feeding a shared filled-buffer queue.

The case against: the writer is the bottleneck, not the worker. Adding workers above one does nothing; the writer can still consume only one chunk per write-time. The added complexity (multiple `unsafe impl Send`, work-stealing, ordering-of-writes if chunks complete out-of-order) is real, the benefit is zero.

### Alternative 5: Async/await with Tokio

Spawn a Tokio runtime, model the worker and main thread as `async fn`s, use `tokio::sync::mpsc`.

The case against: the work is genuinely synchronous-CPU on the worker (decompressor crates have no async API and would need `spawn_blocking`) and synchronous-IO on the main thread (`pwrite` is synchronous). Both halves end up running on a Tokio blocking-pool thread, which is exactly what `std::thread::spawn` already provides — but with extra runtime overhead and a substantial dependency.

### Alternative 6: `crossbeam` channels

`crossbeam::channel::bounded(2)` instead of `std::sync::mpsc`.

The case for: cleaner API, slightly faster (lock-free vs Mutex-based), supports `select!`. The bounded variant gives explicit backpressure.

The case against: a new prod dependency for marginal benefit. `std::sync::mpsc` works correctly here; the seeding-with-two-buffers idiom expresses the bound implicitly. Migration is straightforward later if profiling reveals the channel as a bottleneck.

**Decision:** stay with `std::sync::mpsc` for this commit.

---

## Implementation addendum (as-built, 2026-07)

Steps 1–3 are implemented; Step 4 remains deferred per this plan's own
soak gate. Divergences from the sketches above, all deliberate,
re-derived against the post-plan codebase:

1. `process_chunk` gained `dev_size` and the `chunk_end_within`
   capacity pre-check (a post-plan safety invariant; per this plan's
   own doctrine, loop-body invariants live in the shared helper).
   `ChunkOutcome` variants therefore carry `end` (checked new total)
   instead of `bytes_written`.
2. `AlignedBuf::new()` is fallible (`Result`) since the OOM-unwind
   hardening; seeding uses `?` + a `bail!` guard instead of `expect`
   (restriction-lint regime).
3. `std::sync::mpsc` is unbounded: a send never blocks. The two-buffer
   pool, not channel capacity, bounds pipeline depth; the cleanup's
   `drop(filled_rx)` is for prompt worker exit and buffer release,
   while `drop(free_tx)` is the actual join-deadlock preventer.
4. `flash_finalize` preserves the pre-extraction order (bar teardown →
   newline → `set_direct(false)` → `fdatasync`), not the sketch's
   sync-first order; durability is still established before return.
5. `worker_loop` and `flash_pipelined` are generic over `R: Read`
   (+`Send + 'static` for the arm), so the panic/error-propagation
   tests drive the real code paths; production monomorphizes with
   `ImageReader` only.
6. Worker fill errors reuse the serial arm's exact context string
   ("reading from image stream") for arm-independent error chains.
7. Test counts and MSRV references in this plan predate ten review
   passes (now 124 unit + 3 gated integration tests; MSRV 1.95).
8. Throttle math uses the checked `saturating_mul`/`checked_div`
   idiom from the post-plan hardening.

9. Step 4 (phase-5b verify threading) was subsequently
   implemented in the same engagement under an explicit
   project-owner override of the soak gate; see the addendum
   in `threading-plan-phase5b.md`.
