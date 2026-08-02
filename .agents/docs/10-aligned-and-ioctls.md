# 10 — `AlignedBuf` and the Block-Device ioctl Layer

**Source:** `crates/imi-core/src/common/aligned.rs`, `crates/imi-core/src/common/ioctl.rs`.

**Purpose:** Reference for the low-level pieces that
`phases/phase_4.rs`, `phases/phase_5.rs` and the phase orchestration in
`lib.rs` build on. If you change either source file, this doc is the
audit checklist.

## `AlignedBuf` — page-aligned heap buffer for `O_DIRECT`

### What it is

A 4 MiB heap region aligned to 4 KiB, allocated with
`std::alloc::alloc_zeroed` and freed via `std::alloc::dealloc` in a
`Drop` impl. Used as the user-side buffer for every `O_DIRECT` write
in Phase 4 and (less critically) every read in Phase 5.

```rust
pub(crate) const BUF_SIZE: usize  = 4 * 1024 * 1024;
pub(crate) const BUF_ALIGN: usize = 4096;
```

### Why those numbers

**4 MiB chunk size.** Big enough to amortise per-syscall overhead
(`pwrite` returning ~5 µs of kernel time per call, regardless of size,
becomes negligible at 4 MiB chunks). Small enough to keep the kernel's
writeback queue from hitting backpressure and to keep `--throttle`
sleep granularity reasonable (one chunk = ~0.5 s at 8 MiB/s, the
default throttled rate).

**4 KiB alignment.** This is the _minimum_ alignment required by Linux
`O_DIRECT` for any realistic block device:

- x86_64 base page size: 4 KiB.
- aarch64 base page size: 4 KiB (also supports 16 KiB and 64 KiB, but
  4 KiB is universally accepted).
- USB/SD device logical sector size: 512 B or 4096 B (rarely larger).
- Filesystem-on-block constraints: irrelevant, we're writing raw to the
  block device, not through a filesystem.

We do not query `BLKSSZGET` to detect a finer alignment requirement.
4 KiB is a conservative safe floor, and querying-and-conforming would
add code without practical benefit on any device we have tested.

### The unsafe surface

Four `unsafe` blocks, all in `aligned.rs`:

```rust
// 1. Allocation
let raw = unsafe { alloc_zeroed(layout) };
// SAFETY: layout has non-zero size; alloc_zeroed contract returns
// either a pointer to layout.size() bytes that we own exclusively,
// or null on OOM. Null is surfaced as Err (never handle_alloc_error,
// which aborts past FlashGuard::drop — see 09-flashguard.md).

// 2. Slice view (immutable)
unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), BUF_SIZE) }
// SAFETY: self.ptr is unique, valid for BUF_SIZE bytes, fully
// initialised (alloc_zeroed). Lifetime tied to &self.

// 3. Slice view (mutable)
unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), BUF_SIZE) }
// SAFETY: as above; &mut self guarantees no aliasing.

// 4. Deallocation
unsafe { dealloc(self.ptr.as_ptr(), self.layout); }
// SAFETY: same Layout as alloc_zeroed used; pointer not freed before.
```

Each comes with a `// SAFETY:` comment in the source. If you alter
`AlignedBuf`'s API, re-prove all four invariants.

### `Send` implemented; `Sync` deliberately not

`AlignedBuf` contains a `NonNull<u8>`, which is neither `Send` nor
`Sync` by default. `Send` is implemented manually (SAFETY comment in
`aligned.rs`).

The argument for `Send` is ownership, not usage: the buffer exclusively
owns its allocation, that allocation has no thread affinity, and the
global allocator's `alloc`/`dealloc` are thread-safe — so moving the
sole owner to another thread and dropping it there is sound. It would
remain sound if the crate used the type somewhere else entirely. The
`aligned_buf_is_send` test pins the impl against a future field that
would make the type structurally non-`Send`.

`Sync` is a separate question and the answer is no: two threads holding
`&AlignedBuf` could read the same allocation while a third path writes
it, which the ownership argument does not cover. Nothing needs it — the
pipelined flash arm moves buffers between the worker and writer threads
through `mpsc`, never shares them.

That is no longer a convention to be observed. `aligned_buf_is_not_sync`
uses inherent-impl priority under `const _`, so adding
`unsafe impl Sync for AlignedBuf` fails the **build**, not a review.

Two more invariants are enforced the same way rather than trusted:
`BUF_SIZE > 0`, because `alloc_zeroed` on a zero-sized layout is
undefined behaviour and the SAFETY comment discharges that by pointing
at the constant; and `BUF_ALIGN.is_power_of_two()` with
`BUF_SIZE % BUF_ALIGN == 0`, which `Layout::from_size_align` would
otherwise catch at runtime, on every flash, in the field.

### Zero-init, not uninit

We use `alloc_zeroed`, not `alloc`. Reading from a slice covering an
`alloc()` region (uninitialised bytes) is undefined behaviour — even
if you only intend to _write_ before reading, the pattern is fragile.
`alloc_zeroed` defines every byte; `as_slice()` returning a slice over
defined bytes is sound. The cost (one CPU-vectorised zero of 4 MiB) is
~100 µs on modern hardware, negligible against any flash operation.

## Block-device ioctls

`crates/imi-core/src/common/ioctl.rs` defines its wrappers using `nix`'s ioctl macros. Each
corresponds to a kernel `_IO` / `_IOR` declaration in
`include/uapi/linux/fs.h`.

### `BLKGETSIZE64` — device size in bytes

```rust
ioctl_read_bad!(blkgetsize64, request_code_read!(0x12, 114, size_of::<libc::size_t>()), u64);
```

Kernel: `_IOR(0x12, 114, size_t)`. Writes a `u64` (size in bytes)
through the argp pointer.

Note `size_t`, not `u64`: the encoded size travels in the request
number, so a 32-bit userspace must send a different number than a
64-bit one. The kernel supports both — `block/ioctl.c` defines
`BLKGETSIZE64_32 _IOR(0x12, 114, int)` and handles it in
`compat_blkdev_ioctl` beside the native case, with the comment "The
data is compatible, but the command number is different". Both call
`put_u64`, so only the request code varies.

`ioctl_read!` derives the encoded size _and_ the out-parameter type
from one argument, which cannot express that split — using it forces a
hardcoded width. `ioctl_read_bad!` with an explicit
`request_code_read!` computes the code from the target's own `size_t`
while keeping the `*mut u64` the kernel actually writes, giving
`unsafe fn(fd, *mut u64) -> Result<i32>` on every width.

The request codes are pinned by `request_codes_match_the_kernel_headers`
in `common/ioctl.rs`, which asserts both encodings and that ours tracks
`size_t` rather than a fixed width.

Issued from two places:

- Phase 0's read-only query before the lock is acquired, which feeds the
  capacity check against the raw image size and the `2 * WIPE_REGION`
  floor.
- Phase 2's identity re-check, where `BLKGETSIZE64` on the _claimed_ FD
  must match the Phase 0 snapshot (replug-TOCTOU closure).

Phase 3 needs the size for its tail-wipe offset (`dev_size - 1 MiB`) but
does not issue the ioctl: it reads the value Phase 0 captured into the
`Target`. Re-querying there would reopen the window the identity
re-check exists to close.

### `BLKSSZGET` — logical sector size (intentionally not wrapped)

We do **not** generate a wrapper for `BLKSSZGET`. The reason: we
hardcode 4 KiB alignment in `aligned.rs::BUF_ALIGN`, which is the
universal safe ceiling for every USB block device in the field (most
report 512 B logical, advanced-format devices report 4 KiB). A runtime
sector-size check would only be useful if we wanted to _relax_ the
alignment to 512 B on legacy devices — which we have no reason to do.
4 KiB writes work everywhere; the small over-alignment cost is
invisible against the per-chunk syscall overhead.

For future contributors who might want to add adaptive alignment: the
ioctl is a quirky one. `BLKSSZGET` is encoded as `_IO(0x12, 104)`
(no direction/size bits) even though it writes an `int` through the
argp pointer. `ioctl_read!` would build the wrong request number
(`_IOR(0x12, 104, c_int)`) that the kernel rejects with `EINVAL`. The
correct wrapper is via `nix::ioctl_read_bad!` with
`request_code_none!(0x12, 104)`. Don't add the wrapper without a real
caller — keeping unused public ioctls in `ioctl.rs` makes future
readers wonder which paths use which ioctls.

### `BLKFLSBUF` — flush kernel buffer cache for a device

```rust
ioctl_none!(blkflsbuf, 0x12, 97);
```

Kernel: `_IO(0x12, 97)`. No argument. Invalidates the kernel's page
cache for the device. Used at the start of Phase 5 verify to ensure
read-back goes to NAND, not to cached write-side pages.

`ioctl_none!` generates `unsafe fn(fd) -> Result<i32>`.

### `BLKROGET` — device write-protect flag

```rust
ioctl_read_bad!(blkroget, request_code_none!(0x12, 94), libc::c_int);
```

Kernel: `_IO(0x12, 94)` — the same **mismatched encoding** described for
`BLKSSZGET` above: declared `_IO` (no direction/size bits) yet the
handler _writes_ an `int` (from `bdev_read_only()`) through the argp
pointer. `ioctl_read!` would build `_IOR(0x12, 94, c_int)`, a request
number the kernel rejects; `ioctl_read_bad!` with `request_code_none!`
reproduces the exact `_IO` code while keeping the out-param signature.

Used in Phase 0 (`query_dev_geometry_readonly`) and nowhere else: a
write-protected device (hardware RO switch, `blockdev --setro`) is
refused before the confirmation prompt instead of surfacing as `EPERM`
at the Phase 3 wipe with the guard already armed — which would print
the "device inconsistent" FATAL warning for an untouched device.

### `BLKRRPART` — re-read partition table

```rust
ioctl_none!(blkrrpart, 0x12, 95);
```

Kernel: `_IO(0x12, 95)`. No argument. Tells the kernel to re-scan the
device for a partition table. Used at the start of Phase 6.

See `07-phase-6-kernel-sync.md` for the full semantic discussion.

## ioctl call-site pattern

Every ioctl call is in an `unsafe` block with a `// SAFETY:` comment:

```rust
// SAFETY: `guard` owns a valid, currently-open file descriptor for the
// block device — that ownership is the enforcement, since nix's ioctl
// macros generate `unsafe fn(fd: c_int)` and cannot carry it in the
// type. BLKFLSBUF takes no argument and writes nothing through a
// pointer, so there is no memory for it to touch.
let rc = unsafe { ioctl::blkflsbuf(guard.as_raw_fd()) };
rc.context("BLKFLSBUF (flush kernel buffer cache)")?;
```

**The block covers the call and nothing else.** An earlier revision of
this pattern put the `.context()?` inside, which places an early return
in a scope that claims to have checked its preconditions — and silently
adopts whatever a later edit adds after the call. All four ioctl sites
were narrowed to this shape.

### Why ioctls take `RawFd` but `fdatasync` takes `&File`

`nix` ≥ 0.30 split its FD-handling APIs in two:

- **High-level syscall wrappers** (`fdatasync`, `fsync`, `read`, `write`,
  …) require `AsFd`, which means callers pass `&File` or
  `BorrowedFd<'_>`. The borrow's lifetime ties the FD to the open file,
  preventing use-after-close races.
- **The `ioctl_*!` macros** still generate `unsafe fn(fd: RawFd, …)`
  signatures. The macros are a low-level FFI layer; the safety contract
  is delegated to the SAFETY comment at each call-site, which has to
  reason about FD validity anyway.

This split is why `phase_3.rs` calls `fdatasync(guard.file())` (passing
`&File` directly) but `phase_5.rs` calls `ioctl::blkflsbuf(guard.as_raw_fd())`
(passing the raw fd). Both are correct for their respective layers.

The SAFETY comment on every ioctl call must address:

- **FD validity.** The fd must be valid for the duration of the call.
  In `imi` this is always satisfied because we hold the FD via
  `FlashGuard` or a freshly-opened `File` — neither can be invalidated
  during the call.
- **Argument shape.** For ioctls with arguments (BLKGETSIZE64,
  BLKSSZGET), the SAFETY comment should note that the pointer is
  valid, aligned, and writeable for the size the kernel will write.

## Adding a new ioctl

Pattern:

1. Read `include/uapi/linux/fs.h` to find the request code definition.
   Note whether it's `_IO`, `_IOR`, `_IOW`, or `_IOWR`.
2. Use the matching `nix` macro:
   - `_IO(group, num)` → `ioctl_none!`
   - `_IOR(group, num, T)` → `ioctl_read!`
   - `_IOW(group, num, T)` → `ioctl_write_ptr!` or `_int!`
   - `_IOWR(group, num, T)` → `ioctl_readwrite!`
   - **Mismatched encoding (`_IO` but writes through argp)** →
     `ioctl_*_bad!` with `request_code_none!`.
3. Add a doc comment quoting the kernel header.
4. Place the call in an `unsafe { … }` block with a SAFETY comment.

## fcntl: `O_DIRECT` toggling

Not strictly an ioctl, but in the same low-level family. Declared in
`common/direct_io.rs::set_direct`:

```rust
pub(crate) fn set_direct<F: AsFd>(fd: F, enable: bool) -> Result<()> {
    let bits = fcntl(&fd, FcntlArg::F_GETFL).context("fcntl(F_GETFL)")?;
    let flags = OFlag::from_bits_retain(bits);

    let wanted = if enable { flags | OFlag::O_DIRECT } else { flags & !OFlag::O_DIRECT };
    if wanted == flags {
        return Ok(()); // already in the requested state; skip the syscall
    }

    fcntl(&fd, FcntlArg::F_SETFL(wanted))
        .with_context(|| format!("fcntl(F_SETFL, O_DIRECT={enable})"))?;
    Ok(())
}
```

This used to be two raw `libc::fcntl` calls in `unsafe` blocks. `nix` —
already a dependency — wraps `fcntl` over `AsFd`, which removes the
`unsafe` entirely and makes the descriptor's validity structural rather
than a comment: the borrow keeps the owner alive for the call. That is
the opposite conclusion from the ioctls above, where no safe wrapper
exists, and the difference is worth noticing before adding another raw
call.

**`O_DIRECT` is in Linux's `F_SETFL` mutable set; `O_SYNC` is not.**
`fs/fcntl.c` defines `SETFL_MASK` as exactly
`(O_APPEND | O_NONBLOCK | O_NDELAY | O_DIRECT | O_NOATIME)` and applies
it as `f_flags = (arg & SETFL_MASK) | (f_flags & ~SETFL_MASK)`.
`O_ASYNC` is settable too but by a different route — `setfl` calls the
file operation's `->fasync()` handler, which is responsible for the
`FASYNC` bit, and only for file types that implement it. Anything
outside both paths is silently masked out — meaning a `F_SETFL |
O_SYNC` returns success without actually setting the flag. We do not
attempt to set `O_SYNC`; durability comes from `fdatasync()` at the
end of Phase 4.

`F_SETFL` can be called multiple times safely; each call replaces the
mutable flags wholesale (we read-modify-write to preserve other flag
bits).
