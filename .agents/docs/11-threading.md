# 11 — Threading: the Phase 4 and Phase 5b Pipelines

**Source:** `crates/imi-core/src/phases/phase_4.rs` (`flash`, `flash_serial`,
`flash_pipelined`, `worker_loop`, `process_chunk`, `flash_finalize`),
`crates/imi-core/src/phases/phase_5.rs` (`verify`, `verify_serial`,
`verify_pipelined`, `verify_worker_loop`, `compare_chunk`),
`crates/imi-core/src/common/aligned.rs` (`AlignedBuf: Send`).

**Purpose:** Describe the two threaded pipelines as they are, and the
invariants that make them safe to change. This supersedes the two
planning documents that preceded the implementation; where the code
departed from those plans, the code is what is described here and the
departure is noted.

Threading exists in exactly two places: Phase 4's compressed-image arm
and Phase 5b's compressed-image arm. Everything else in this crate is
single-threaded, and **no other phase may spawn a thread** without a
design that answers the same questions this document answers.

## Why only here

For one 4 MiB chunk, the two costs that could overlap are decompression
(CPU) and device I/O (not CPU). Serial cost per chunk is
`read + write`; pipelined cost is `max(read, write)`. The saving is
therefore bounded by the *shorter* of the two, which is what decides
where threading is worth its complexity.

The figures below are inherited from the pre-implementation design notes
and were measured on real USB hardware, which this repository's test
environment does not have. Treat the *shape* — which column dominates —
as the durable part; the absolute numbers are a decade-old hardware
generation away from being re-checkable here.

| Stage | Time, NVMe source → USB 3.0 target | CPU-bound? |
| --- | --- | --- |
| `fill_buffer`, raw | <1 ms | no — disk read |
| `fill_buffer`, gzip / zstd | ~2–5 ms | modestly |
| `fill_buffer`, xz | ~15–40 ms | **yes, comparable to the write** |
| `fill_buffer`, bzip2 | ~30–80 ms | **yes, typically exceeds it** |
| `write_direct`, USB 3.0 | ~30–50 ms | no — USB controller |
| `write_direct`, USB 2.0 | ~120–150 ms | no — USB controller |

Raw images have no meaningful read cost, so they stay serial and see no
change. That is why dispatch is on `comp.is_compressed()` and not on a
flag: there is nothing to overlap for a raw image, and a pipeline that
saves nothing still costs a thread, two channels and a shutdown
protocol.

No other phase has two overlappable costs. Phases 0, 1, 2 and 6 are
sub-millisecond syscalls. Phase 3 is two 1 MiB `pwrite`s with no second
computation. Phase 5a is a deliberate wall-clock wait — threading it
would defeat its purpose. Phase 7 sleeps on udev and scans procfs.

## The shape both pipelines share

```
main thread                                  worker thread
───────────                                  ─────────────
                    free: AlignedBuf  ──────▶ recv a buffer
                                              check the cancel mirror
                                              fill it (decompress)
    recv ◀────── filled: Result<(buf, n)> ──── send it back
    use the buffer (write / compare)
    send it back on `free`  ───────────────▶
```

Two `mpsc` channels carry ownership in a loop, seeded with **two**
buffers before the worker is spawned:

```rust
let (filled_tx, filled_rx) = mpsc::channel::<FilledItem>();
let (free_tx, free_rx) = mpsc::channel::<AlignedBuf>();
for _ in 0..2_u8 {
    if free_tx.send(AlignedBuf::new()?).is_err() {
        bail!("buffer-pool receiver closed before worker spawn (unreachable)");
    }
}
```

**The pool bounds the pipeline depth, not the channel.**
`std::sync::mpsc` is unbounded and a send never blocks, so
backpressure comes from the worker having to wait for a buffer to come
back. Two buffers is one in flight and one being filled — enough to
overlap, and a hard bound on how much can be outstanding at once.

The bound is 8 MiB in flash and 12 MiB in verify: the verify pipeline
allocates a third `AlignedBuf` for the device side, which the main
thread keeps and never sends anywhere. That is the one place the two
pipelines allocate differently, and it is not a pool member — the pool
carries image-side buffers only.

### Only the main thread touches the device

`O_EXCL` is a kernel-enforced claim on the device, not a thread-local
lock; the kernel would happily serialise two `pwrite`s from two threads
and leave the offset bookkeeping wrong. So the rule is that only the
main thread ever calls `write_all_at` or `read_exact_at` on the guard's
descriptor.

This is structural rather than observed. The worker is moved an
`ImageReader` and receives `AlignedBuf`s; it is never given
`&FlashGuard`, nor a `RawFd`, nor anything from which one can be
obtained. There is no rule to remember because there is no expressible
way to break it — which is the property to preserve when editing.

It is also kernel-attested rather than argued. Under
`strace -f -e trace=pwrite64`, a compressed flash issues every
`pwrite64` from a single tid — measured here, not inherited from the
design notes.

### `AlignedBuf: Send`, and deliberately not `Sync`

Buffers cross a thread boundary by move, which needs `Send`. The
argument is ownership: the buffer exclusively owns its allocation, that
allocation has no thread affinity, and the global allocator is
thread-safe, so moving the sole owner and dropping it elsewhere is
sound.

`Sync` is a different question and the answer is no — two threads
holding `&AlignedBuf` could read while a third path writes. Nothing
needs it, and the absence is enforced rather than conventional: a
`Probe<T>` type in `aligned.rs`'s tests carries `IS_SYNC = false` from a
blanket trait impl and `IS_SYNC = true` from an inherent impl gated on
`T: Sync`, with inherent-impl priority selecting between them. Two
`const _` assertions then fix the answer at compile time:

```rust
const _: () = assert!(!<Probe<AlignedBuf>>::IS_SYNC);
// The probe must actually discriminate, or the line above is vacuous.
const _: () = assert!(<Probe<u8>>::IS_SYNC);
```

Adding `unsafe impl Sync for AlignedBuf` fails the **build**, not a
review. The second assertion is what stops the first from passing
vacuously — a probe that answered `false` for everything would be no
guard at all. `aligned_buf_is_send` pins the positive half against a
future field that would make the type structurally non-`Send`. See
`10-aligned-and-ioctls.md`.

## Shutdown: one cleanup block, in one order

This is the part that repays care. Every exit from the labelled loop —
`'write_loop` in flash, `'verify_loop` in verify — **breaks** to a
single cleanup block. There is no `return` inside either: six break
sites in flash, eleven in verify, and zero returns between the loop head
and the cleanup.

```rust
// 1. prompt worker exit; also frees a buffer parked in the queue
drop(filled_rx);
// 2. the actual unblocker for a pool-parked worker
drop(free_tx);

// 3. join, capturing any panic rather than re-raising it here
if let Some(handle) = worker_handle.take() {
    if let Err(panic) = handle.join() {
        worker_panic = Some(panic);
    }
}

// 4. re-raise on the main thread, after the channels are closed
if let Some(panic) = worker_panic {
    std::panic::resume_unwind(panic);
}
outcome?;   // the loop's own error, only if there was no panic
```

The join **captures** rather than re-raising in place. Re-raising inside
the `if let` would unwind with `free_tx` already dropped but the
`outcome` unexamined, and — more importantly — the capture is what lets
step 4 sit after every channel is closed, so `FlashGuard::drop` runs
against a fully torn-down pipeline.

The order is load-bearing:

1. **`drop(filled_rx)` first.** A worker blocked in `filled_tx.send`
   returns `Err` immediately and can exit. This also drops any buffer
   sitting in the queue, releasing 4 MiB.
2. **`drop(free_tx)` second, and this is the one that matters.** A
   worker parked in `free_rx.recv()` waiting for a buffer will wait
   forever unless the sender is gone. Skipping this deadlocks the join.
3. **Join before reporting.** The worker owns the `ImageReader`; the
   error the main thread is about to return may be *caused* by
   something the worker saw.
4. **`resume_unwind` last, after the channels are closed.** A worker
   panic is captured and re-raised on the main thread so that
   `FlashGuard::drop` runs during a normal unwind and the FATAL notice
   reaches the operator. Aborting instead would skip it entirely.

A captured panic outranks a captured error: `resume_unwind` runs before
the loop's own `outcome?` is propagated, so a worker that panicked is
reported as a panic rather than as whatever the writer noticed second.

`worker_exits_when_filled_receiver_drops`, `worker_exits_when_pool_sender_drops`
and `verify_worker_exits_when_pool_sender_drops` pin steps 1 and 2;
`pipelined_resumes_worker_panic_on_main_thread` and its verify twin pin
step 4.

### Why deadlock is unreachable

The main thread blocks in `filled_rx.recv()` only when it is holding
zero pool buffers — it returns each buffer before the next receive. So
whenever main is parked, the pool has at least one buffer and the worker
can always make progress. Both-parked is not a reachable state.

## Cancellation

The parent cancel flag is a `&AtomicBool` the caller owns. The worker
gets a **mirror**: an `Arc<AtomicBool>` the main thread sets, checked by
the worker after each pool receive so a cancelled run skips one wasted
fill rather than decompressing a chunk nobody will use.

The mirror is an optimisation. The correctness backstop is the channel
disconnect — a worker whose channels close exits regardless of what any
flag says. `cancel_mirror_stops_worker_before_fill` and
`verify_cancel_mirror_stops_worker_before_fill` pin the optimisation;
the exit tests above pin the backstop.

Inside the loops, cancellation is built with `err!(Cancelled { .. })`
and assigned to the outcome, then `break`, rather than `bail!`ed —
because the single-cleanup-block rule forbids returning from inside the
loop. Same value, different control flow. See `09-flashguard.md` for
why `Cancelled` must be a type rather than a message.

## Protocol violations are errors, not EOF

Both pipelines treat an unexpected channel state as a hard failure. This
is where the implementation departed from the plans, which sketched
returning `Ok` on a disconnect.

**Phase 4.** EOF is in-band: a short fill. An image that is an exact
`BUF_SIZE` multiple ends with a `(buf, 0)` handoff. Every voluntary
worker exit therefore sends a final item first, so a disconnect *with a
clean join* means a chunk went missing — reported as a pipeline protocol
violation rather than treated as end-of-image. Treating it as EOF would
return SUCCESS over a partially flashed device.

**Phase 5b.** Two checks, because verify knows exactly how much it
expects. A worker chunk whose length differs from the independently
derived `min(remaining, BUF_SIZE)` is an error, and so is a clean worker
disconnect while `remaining > 0`. A short verify must never pass
silently: `pipelined_verify_classifies_truncation_not_mismatch` pins
that the two are distinguishable, because "the image ended early" and
"the bytes differ" call for different operator responses.

## What differs between the two pipelines

They are deliberately the same shape, and the differences are only where
the work differs.

| | Phase 4 flash | Phase 5b verify |
| --- | --- | --- |
| worker owns | the `ImageReader` | a **reopened** `ImageReader` |
| worker paces by | image EOF | `bytes_written` from Phase 4 |
| main thread holds | nothing extra | its own `dev_buf`, never shared |
| per-chunk helper | `process_chunk` | `compare_chunk` |
| device call | `write_all_at` | `read_exact_at` |
| finaliser | `flash_finalize` | `verify_finalize` |

Verify reopens the image because decompressors do not implement `Seek` —
there is no rewinding the Phase 4 reader. And the main thread issues its
device read **before** receiving the matching image chunk, so the USB
read overlaps the decompression rather than following it. That ordering
is where the wall-clock saving actually lives.

`flash_finalize` runs bar teardown, the hardening `set_direct(false)`
and `fdatasync` in that order — deliberately not the plan's sync-first
sketch, since durability is still established before the function
returns. `verify_finalize` has no `fdatasync`: verify is read-only.

## Both arms must agree

The two arms of each phase exist to be interchangeable, and that is
asserted rather than assumed:

- `arms_produce_identical_bytes_for_identical_input` — the serial and
  pipelined flash arms write byte-identical devices.
- `verify_arms_report_identical_mismatch` — both verify arms produce
  the same mismatch message, so a diagnostic does not depend on which
  arm ran.

Per-chunk decisions live in the shared helpers (`process_chunk`,
`compare_chunk`) for this reason: fixed once, both arms inherit it.

## The thread census

During a compressed Phase 4, `/proc/<pid>/task` holds exactly four
entries: main, the `ctrlc` handler, indicatif's steady-tick spinner, and
the one worker. Measured, not assumed — the handler thread shows up
under the name `ctrl-c`.

The ticker predates the pipeline and belongs to the progress UI, not to
this design. Anyone counting threads to check this pipeline should
expect four and account for two of them elsewhere.

## The decompressor crates must stay single-threaded

`Cargo.toml` carries a standing instruction not to enable
parallel/multithreaded features on `bzip2`, `flate2`, `xz2` or `zstd`,
and it points here for the reason.

Both pipelines assume **one worker owns the whole decode**. That is what
makes the buffer pool a real bound on in-flight memory, what makes the
thread census predictable, and what makes the cancel mirror a single
flag rather than a broadcast. A decoder that spawns its own pool would
break none of the safety invariants — the device descriptor is still
untouchable from those threads — but it would silently invalidate the
capacity reasoning and the census, and it would compete for cores with
the writer on exactly the constrained machines where the pipeline
matters least.

If a future change wants more parallelism — several decompressor workers
feeding one writer, or parallel decoder features — the analysis here is
not sufficient for it. That is a different design, and it needs its own
document before it needs code.

## If you change this

- Keep every loop exit a `break` to the single cleanup block.
- Keep `drop(free_tx)` before the join.
- Keep the worker unable to name the descriptor.
- Keep protocol violations loud.
- Add the change to both arms, or explain in the commit why one arm
  differs.

The tests that would catch a mistake in each of these are named above.
If a change makes one of them awkward to keep, that is the signal to
re-read this document rather than to relax the test.
