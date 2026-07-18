//! `AlignedBuf` — a page-aligned, owned, heap buffer for `O_DIRECT` I/O.
//!
//! Linux `O_DIRECT` requires the user buffer to be aligned to the logical
//! sector size of the underlying block device (the kernel actually rounds up
//! to the filesystem's page-alignment requirement, but 4 KiB is the universal
//! safe floor for any USB device in the field).
//!
//! This module provides a 4 MiB buffer aligned to 4 KiB, allocated via
//! `std::alloc::alloc_zeroed` with an explicit `Layout`, and freed via
//! `std::alloc::dealloc` on drop. The `Drop` impl guarantees we don't leak on
//! panic.
//!
//! The buffer is initialised to zero on construction — partly so reads into
//! uninit bytes are not UB (reading uninit `u8` is defined behavior via
//! `MaybeUninit` only; initialising eagerly sidesteps the distinction), and
//! partly so a partial final chunk that gets tail-padded with zeros is
//! deterministic rather than reading stale allocator content.

use anyhow::{Context as _, Result};
use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ptr::NonNull;

/// 4 MiB — the per-chunk write size.
pub(crate) const BUF_SIZE: usize = 4 * 1024 * 1024;

/// 4 KiB — alignment. Matches `x86_64` and aarch64 base page size and is
/// a universal floor for `O_DIRECT` buffer alignment on real hardware.
pub(crate) const BUF_ALIGN: usize = 4096;

/// RAII-managed, page-aligned byte buffer.
///
/// `Send` (manual impl below, with SAFETY comment): the pipelined flash
/// arm moves buffers between the worker and writer threads through
/// `mpsc` channels. Not `Sync`: no two threads may hold references to
/// the same buffer simultaneously, and nothing needs them to.
pub(crate) struct AlignedBuf {
    /// Non-null for the lifetime of the struct.
    ptr: NonNull<u8>,
    /// The exact `Layout` used at allocation time; `Drop` must free with
    /// the identical layout.
    layout: Layout,
}

impl AlignedBuf {
    /// Allocate a new zero-initialised buffer of `BUF_SIZE` bytes,
    /// aligned to `BUF_ALIGN`.
    ///
    /// Allocation failure is returned as `Err`, **not** routed through
    /// `handle_alloc_error`: the latter aborts the process, which would
    /// skip `FlashGuard::drop` and suppress the "device inconsistent"
    /// FATAL warning if the OOM struck while the guard was armed (both
    /// call sites — flash and verify — run inside the armed window).
    /// An `Err` unwinds normally and the warning fires.
    pub(crate) fn new() -> Result<Self> {
        // Infallible for these compile-time constants (power-of-two
        // alignment, size far below isize::MAX), but propagating keeps
        // this function panic- and abort-free by construction.
        let layout = Layout::from_size_align(BUF_SIZE, BUF_ALIGN)
            .context("BUF_SIZE/BUF_ALIGN produce a valid Layout")?;

        // SAFETY: `layout` has non-zero size (BUF_SIZE > 0). `alloc_zeroed`
        // is safe to call with any valid `Layout` of non-zero size; it
        // returns either a pointer to at least `layout.size()` bytes that we
        // own exclusively, or null on OOM. Null is surfaced as `Err`.
        let raw = unsafe { alloc_zeroed(layout) };
        let ptr =
            NonNull::new(raw).context("allocating the 4 MiB aligned I/O buffer (out of memory)")?;

        Ok(Self { ptr, layout })
    }

    /// View the whole buffer as an immutable byte slice.
    pub(crate) fn as_slice(&self) -> &[u8] {
        // SAFETY: `self.ptr` is a unique, valid pointer to `BUF_SIZE` bytes
        // that we own for the lifetime of `self`. The allocation is zero-
        // initialised (from `alloc_zeroed`), so every byte is initialised.
        // The returned slice's lifetime is tied to `&self`, so no aliasing
        // with any concurrent `&mut` view is possible.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), BUF_SIZE) }
    }

    /// View the whole buffer as a mutable byte slice.
    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `self.ptr` is unique and valid for `BUF_SIZE` bytes (see
        // `new`). `&mut self` guarantees we are the sole reference; the
        // returned `&mut [u8]` inherits that uniqueness.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), BUF_SIZE) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: We free via the identical `Layout` we allocated with, and
        // `self.ptr` was returned by `alloc_zeroed(self.layout)` in `new`
        // and has not been freed or reallocated since.
        unsafe {
            dealloc(self.ptr.as_ptr(), self.layout);
        }
    }
}

// SAFETY: `AlignedBuf` exclusively owns its heap allocation — its only
// fields are the owning `NonNull<u8>` and the `Layout`; there are no
// shared pointers, no `Rc`, and no borrows stored alongside the owner.
// The pipelined flash arm transfers buffers between threads by-move
// through `mpsc` channels, so at any moment exactly one thread holds a
// given `AlignedBuf`; `Drop` may consequently run on either thread,
// which is fine because the global allocator's `dealloc` is
// thread-safe. `Sync` is deliberately NOT implemented: no two threads
// may hold references to the same buffer simultaneously, and nothing
// in the codebase needs them to. (This impl is the case
// `.agents/docs/10-aligned-and-ioctls.md` pre-authorized; the
// `aligned_buf_is_send` test pins it against a future non-`Send`
// field.)
unsafe impl Send for AlignedBuf {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_size_matches_buf_size_constant() {
        let mut buf = AlignedBuf::new().unwrap();
        assert_eq!(buf.as_slice().len(), BUF_SIZE);
        assert_eq!(buf.as_mut_slice().len(), BUF_SIZE);
    }

    /// `O_DIRECT` alignment requirement: the buffer's start address must be
    /// a multiple of `BUF_ALIGN`. A regression here would cause `EINVAL`
    /// on every aligned `pwrite` in Phase 4.
    #[test]
    fn buffer_pointer_is_4_kib_aligned() {
        let buf = AlignedBuf::new().unwrap();
        let addr = buf.as_slice().as_ptr() as usize;
        assert_eq!(addr % BUF_ALIGN, 0, "AlignedBuf at {addr:#x} is not aligned to {BUF_ALIGN}");
    }

    /// `alloc_zeroed` is contractually required to zero the allocation;
    /// we depend on this so that tail-padded chunks read zero rather than
    /// stale heap content. Spot-check a handful of offsets — sweeping
    /// every byte would test the allocator, not us.
    #[test]
    fn buffer_starts_zero_initialized() {
        let buf = AlignedBuf::new().unwrap();
        assert_eq!(buf.as_slice()[0], 0);
        assert_eq!(buf.as_slice()[BUF_SIZE - 1], 0);
        assert_eq!(buf.as_slice()[BUF_SIZE / 2], 0);
        assert_eq!(buf.as_slice()[BUF_ALIGN - 1], 0);
        assert_eq!(buf.as_slice()[BUF_ALIGN], 0);
    }

    /// `as_mut_slice` round-trips: writing then reading via `as_slice`
    /// observes the written bytes.
    #[test]
    fn mutable_slice_round_trips_writes() {
        let mut buf = AlignedBuf::new().unwrap();
        buf.as_mut_slice()[0] = 0xAA;
        buf.as_mut_slice()[BUF_SIZE - 1] = 0xBB;
        buf.as_mut_slice()[1024] = 0x42;
        assert_eq!(buf.as_slice()[0], 0xAA);
        assert_eq!(buf.as_slice()[BUF_SIZE - 1], 0xBB);
        assert_eq!(buf.as_slice()[1024], 0x42);
    }

    /// The constructor produces a buffer of exactly `BUF_SIZE` bytes at
    /// `BUF_ALIGN` alignment. (There is deliberately no `Default` impl:
    /// `new` is fallible so OOM unwinds through the guard instead of
    /// aborting, and a panicking `default()` would defeat that.)
    #[test]
    fn new_constructs_a_valid_buffer() {
        let buf = AlignedBuf::new().unwrap();
        assert_eq!(buf.as_slice().len(), BUF_SIZE);
        assert_eq!(buf.as_slice().as_ptr() as usize % BUF_ALIGN, 0);
    }

    /// Two independently-allocated buffers must have distinct backing
    /// storage. Catches a hypothetical bug where the constructor
    /// accidentally returned a shared `static` buffer.
    #[test]
    fn distinct_buffers_have_distinct_storage() {
        let mut a = AlignedBuf::new().unwrap();
        let mut b = AlignedBuf::new().unwrap();
        a.as_mut_slice()[0] = 0x11;
        b.as_mut_slice()[0] = 0x22;
        // If a and b shared storage, the second write would have
        // overwritten the first.
        assert_eq!(a.as_slice()[0], 0x11);
        assert_eq!(b.as_slice()[0], 0x22);
        // Also: distinct base pointers.
        assert_ne!(a.as_slice().as_ptr() as usize, b.as_slice().as_ptr() as usize);
    }
    /// Compile-time guard for the `unsafe impl Send`: if a future field
    /// makes `AlignedBuf` structurally non-`Send`, the conflicting
    /// manual impl surfaces here rather than at a distant channel call
    /// site in the pipelined flash arm.
    #[test]
    fn aligned_buf_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<AlignedBuf>();
    }
}
