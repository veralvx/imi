# 05 — Phase 4: Flash Write Loop

**Source:** `src/flash.rs`, `src/aligned.rs`, `src/image.rs`,
`src/main.rs::run` (Phase 4 block).

**Purpose:** Stream the (possibly compressed) image into the locked
device FD via aligned `O_DIRECT` writes, with throttling, ENOSPC
handling, signal responsiveness, and a smooth progress display.

## High-level flow (serial arm — `flash_serial`; the pipelined arm's

## chunk _production_ differs per the sections above, its chunk

## _processing_ is this same `process_chunk` call)

```
ImageReader (Raw / Gzip / Xz / Bzip2 / Zstd)
    │
    ▼
fill_buffer(reader, &mut aligned_buf)        ← short-read tolerant
    │   loops Read::read until aligned_buf
    │   is full or EOF
    ▼
if filled == 0:
    break                                     ← EOF on aligned boundary
end = chunk_end_within(offset, filled, dev_size)  ← capacity pre-check;
    │   None ⇒ deterministic "exceeds device capacity" abort
if filled == BUF_SIZE:
    write_all_at(aligned_buf, offset)         ← O_DIRECT
    offset = total = end
else:
    set_direct(fd, false)                     ← clear O_DIRECT
    write_all_at(&aligned_buf[..filled], …)   ← page-cache tail
    total = end; break
set_direct(fd, false)                         ← always clear on exit
fdatasync(guard.file())                       ← durable commit
```

The unconditional `set_direct(fd, false)` after the loop is hardening:
the EOF-on-aligned-boundary path (no tail chunk) would otherwise leave
`O_DIRECT` on. Today's verify phase clears it on entry, so nothing is
broken — but a future phase inserted between Phase 4 and Phase 5, or
a future call from a different context, would inherit `O_DIRECT` and
trip `EINVAL` on the first misaligned read or write. Cheaper to clear
on exit than to litigate the invariant elsewhere.

## Dispatch and shared helpers (threading plan, Steps 1–3)

`flash::flash` is a thin runtime dispatcher on `comp.is_compressed()`:
raw images take **`flash_serial`** (the pre-threading loop, verbatim);
compressed images take **`flash_pipelined`** (below). Both arms
delegate every per-chunk decision to **`process_chunk`** — the
loop-body invariant holder: the `chunk_end_within` capacity pre-check,
full-chunk-vs-tail dispatch (with the tail's O_DIRECT clear), and the
checked `end` accounting all live there, fixed once for both arms —
and both finish through **`flash_finalize`** (bar teardown, newline,
hardening `set_direct(false)`, `fdatasync`; the order preserves the
pre-extraction code, deliberately differing from the plan sketch's
sync-first order, and durability is still established before the
function returns). `ChunkOutcome` variants carry `end` (the checked
new total) rather than the plan's per-chunk `bytes_written`, so the
bound and the advance come from one checked expression.

## Pipelined arm

One worker thread owns the `ImageReader` by-move and fills
`AlignedBuf`s; the main thread writes them. Two `mpsc` channels carry
ownership (`filled`: worker→main with `Result<(AlignedBuf, usize)>`;
`free`: main→worker), seeded with a two-buffer pool before spawn —
the pool, not the channel, bounds pipeline depth, because
`std::sync::mpsc` is unbounded and a send never blocks. Cancellation:
main mirrors the parent flag into an `Arc<AtomicBool>` the worker
checks after each pool recv (skipping a wasted fill); channel
disconnects are the correctness backstop. Every `'write_loop` exit
lands in one cleanup block: `drop(filled_rx)` (prompt worker exit +
frees a parked 4 MiB buffer), `drop(free_tx)` (the true unblocker for
a pool-parked worker — required before the join), join, then
`resume_unwind` of any captured worker panic so the armed guard's
FATAL fires with channels already closed. EOF is signalled in-band by
a short fill; an image that is an exact `BUF_SIZE` multiple ends with
a `(buf, 0)` handoff that `process_chunk` answers with
`Done {{ end: offset }}`. A channel disconnect with a _clean_ worker
join is therefore a protocol violation — every voluntary worker exit
sends a final item first — and the writer fails loud with a
"pipeline protocol violation" error instead of treating it as EOF
(the plan's sketch broke with Ok there; a silent partial flash must
never return SUCCESS). Only the main thread ever touches the FD —
the worker cannot, structurally (and kernel-attested: an strace of a
pipelined flash shows every `pwrite64` on the claimed fd issued by a
single tid). Liveness invariant worth recording: a both-parked
deadlock state is unreachable — the main thread blocks in
`filled_rx.recv()` only when it holds zero pool buffers (it returns
each one before the next recv), so whenever main is parked the pool
has at least one buffer and the worker can always make progress. Errors from the worker's fill carry
the same "reading from image stream" context as the serial arm, so
operator-visible chains are arm-independent. Protocol pinned by the
`worker_exits_*`, `cancel_mirror_*`, `pipelined_*`, and
`arms_produce_identical_bytes_for_identical_input` tests.

## `O_DIRECT` invariants — the three commandments

`O_DIRECT` writes bypass the kernel page cache, which is what we want
(faster steady-state throughput, no double-buffering, no kernel memory
pressure during a flash). The kernel rejects writes that don't satisfy
all three:

1. **Buffer alignment.** `AlignedBuf` allocates a 4 MiB region at 4 KiB
   alignment via `std::alloc::alloc_zeroed` with a precisely-sized
   `Layout`. 4 KiB matches the base page size on x86_64 and aarch64 and
   is a universal floor for USB devices. See `10-aligned-and-ioctls.md`.

2. **Offset alignment.** We only ever issue `O_DIRECT` writes at
   `offset = 0`, `BUF_SIZE`, `2 * BUF_SIZE`, …. All multiples of
   `BUF_SIZE = 4 MiB`, which is a multiple of 4 KiB. Always aligned.

3. **Length alignment.** Every `O_DIRECT` write is exactly `BUF_SIZE`
   bytes. The tail (which is by definition a partial chunk) is the only
   sub-`BUF_SIZE` write, and we explicitly disable `O_DIRECT` for it.

If you change any of these — chunk size, alignment, or buffer type —
you must re-prove all three. Failures appear as `EINVAL` from the
kernel.

## Why a fill-then-write loop, not direct read-into-pwrite

Decompressors (`flate2`, `xz2`, `bzip2`, `zstd::Decoder`) return short
reads unpredictably — their internal block structure does not match
the device's preferred I/O size. A naïve `read_into_pwrite` produces
wildly variable write sizes, breaking the `O_DIRECT` length invariant.

```rust
fn fill_buffer<R: Read>(r: &mut R, dst: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < dst.len() {
        match r.read(&mut dst[filled..]) {
            Ok(0) => break,                     // EOF
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {} // retry
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}
```

The fill loop coalesces decoder output into exactly-`BUF_SIZE` chunks.
A short read at the end means we hit EOF; the residual is treated as
the tail. `Interrupted` is retried (signal during `read` doesn't
abort us; only the cancel flag does).

## Tail handling

```rust
if filled == BUF_SIZE {
    write_direct(...)?;
} else {
    set_direct(fd, false)?;   // disable O_DIRECT for residual
    write_tail(...)?;
    break;
}
```

The tail is by definition shorter than `BUF_SIZE` and almost certainly
not sector-aligned in length. We disable `O_DIRECT` via
`fcntl(F_SETFL, flags & !O_DIRECT)` — `O_DIRECT` _is_ in Linux's mutable
flags set, unlike `O_SYNC` — and write through the page cache. The
final `fdatasync` makes it durable.

A 4 MiB+1-byte image would write 4 MiB direct, then 1 byte buffered,
then sync. Correct.

## Why not `O_SYNC`?

`O_SYNC` forces every write to wait for completion before returning.
Useful for durability, but:

1. It is **not** in Linux's `F_SETFL` mutable set. Setting it via
   `fcntl(F_SETFL, flags | O_SYNC)` silently no-ops on some kernels and
   `EINVAL`s on others. It must be set at `open()` or not at all.
2. We get equivalent durability from `fdatasync()` after the loop.

We `fdatasync` once at the end of Phase 4 rather than per-chunk because
the kernel can batch and reorder writes more efficiently across a long
sync window. The penalty is that a power-loss mid-flash leaves the
device inconsistent — but that's exactly what `FlashGuard` exists to
warn about.

## Throttling — preventing thermal brownout

Cheap USB-NAND bridge controllers (the Phison / SMI / Realtek chips in
$5 sticks) thermally throttle aggressively. Sustained 30+ MB/s writes
will cause the controller to drop to 1–2 MB/s after 30–60 seconds, then
recover, then throttle again, with periodic write errors during
transitions.

Throttling at 8 MiB/s by default (with `-t`) keeps the controller in
steady state. The mechanism:

```rust
let chunk_target_nanos = throttle.map(|rate_bps| {
    (BUF_SIZE as u128)
        .saturating_mul(1_000_000_000)
        .checked_div(u128::from(rate_bps)) // rate >= 1 per parse_rate
        .unwrap_or(u128::MAX)
});
// per chunk:
let start = Instant::now();
write_chunk();
let elapsed_ns = start.elapsed().as_nanos();
if let Some(residual) = target_ns.checked_sub(elapsed_ns) {
    cancellable_sleep(Duration::from_nanos(residual), &cancel);
}
```

The sleep is post-write: we let the kernel issue the write at full
speed, then sleep the remainder of the chunk's "ideal" duration. Net
average matches the cap. Trying to throttle by writing slower (smaller
chunks) would defeat the `O_DIRECT` length alignment.

`cancellable_sleep` polls the cancel flag at 100ms granularity. This
matters at low throttle rates: at `--throttle 100K` the residual sleep
is ~40 seconds per chunk, and a naïve `thread::sleep` would make Ctrl+C
wait the full residual before noticing. The 100ms tick adds one atomic
load per tick — invisible cost in any realistic profile. See
`flash::cancellable_sleep` for the implementation.

## Cancellation

Top of every chunk loop:

```rust
if cancel.load(Ordering::SeqCst) {
    pb.abandon();
    bail!("cancelled by user");
}
```

The `Arc<AtomicBool>` is set by the `ctrlc` handler. Returning `Err`
unwinds through `FlashGuard::drop`, which prints the "device
inconsistent" warning. **The signal handler must not call exit()** —
that would skip `Drop` and leave the operator unwarned.

Practical worst-case latency from Ctrl+C to abort: one chunk write
(4 MiB) plus one decompressor fill, typically < 1 second on real
hardware.

## ENOSPC — capacity overrun on compressed input

For raw images we know the size from `metadata().len()` and check
against `BLKGETSIZE64` in Phase 0. For compressed images we don't, so
the decompressor may produce more bytes than the device can hold. The
write returns `ENOSPC` (or `WriteZero` from `write_all_at`'s loop):

```rust
fn is_capacity_error(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::StorageFull | std::io::ErrorKind::WriteZero
    )
}
```

`ErrorKind::StorageFull` is the typed form of `ENOSPC`, stabilised in
Rust 1.83. We surface it as a typed error explaining that the
_decompressed_ image is larger than the device — the operator may have
just downloaded the wrong target architecture.

Belt-and-suspenders on top of the errno mapping: `flash()` receives
`dev_size` (from Phase 0's `BLKGETSIZE64`) and pre-checks every chunk —
`chunk_end_within(offset, len, dev_size)`, which returns the checked end
offset the loop then advances to — _before_ the `pwrite`. A
chunk straddling the device's end can otherwise partially complete, and
`write_all_at`'s retry of the remainder (now at a misaligned offset with
a misaligned length under `O_DIRECT`) surfaces as a bare `EINVAL` that
the errno mapping cannot recognise. The pre-check makes the capacity
diagnostic deterministic for every overrun, including devices whose
size is not a 4 KiB multiple.

## Progress UI

For raw images (known total) — a unified percent bar shared with
Phase 5b verification:

```
[==================>                     ]  47% 476.84 MiB / 1.00 GiB (1.35 GiB/s)
```

Five fixed components: bar, percent (right-aligned to 3 columns for
stable layout), `{bytes}` written so far, `{total_bytes}` target, and
the current rate. Phase 5b uses the identical template — the operator's
eye doesn't have to recalibrate at the phase transition.

For compressed images (unknown decompressed size) — a spinner with byte
count, since `{percent}` and `{total_bytes}` aren't meaningful:

```
⠋ 200.00 MiB written (4.00 MiB/s)
```

Both use `{bytes_per_sec}` (the standard token, which already routes
through indicatif's double-smoothed EWMA estimator). `pb.reset_elapsed()`
is called immediately before the loop so setup time doesn't contaminate
the rate calculation. See `00-cli-and-ux.md` for the longer discussion
of why this is sufficient and why the `with_smoothing()` /
`{smoothed_bytes_per_sec}` ideas don't correspond to real APIs.

On completion: `pb.finish_and_clear(); println!();` to leave a clean
line for the next phase.

## Manual test

```sh
# Raw image, with throttle to verify rate UX:
time sudo ./target/release/imi -i debian-live.iso -d /dev/sdb -t 4M -y

# Compressed image:
time sudo ./target/release/imi -i debian-live.iso.xz -d /dev/sdb -t 4M -y

# Capacity overrun (ought to fail at write time, not at flush):
sudo ./target/release/imi -i 16gb-image.iso -d /dev/sdb-which-is-8gb -y
```
