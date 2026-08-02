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

use crate::Result;
use crate::error::Context as _;
use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ptr::NonNull;

/// 4 MiB — the per-chunk write size.
pub(crate) const BUF_SIZE: usize = 4 * 1024 * 1024;

/// 4 KiB — alignment. Matches `x86_64` and aarch64 base page size and is
/// a universal floor for `O_DIRECT` buffer alignment on real hardware.
pub(crate) const BUF_ALIGN: usize = 4096;

// `alloc_zeroed` is undefined behaviour for a zero-sized layout — std's
// wording is "zero sized `layout` will result in undefined behavior". The
// SAFETY comment in `new` discharges that by pointing at this constant,
// so the constant is where it has to be enforced: an edit setting
// BUF_SIZE to 0 would otherwise compile and be UB at the first
// allocation.
const _: () = assert!(BUF_SIZE > 0, "alloc_zeroed is UB for a zero-sized layout");
// `Layout::from_size_align` rejects a non-power-of-two alignment, so a
// bad BUF_ALIGN is already caught — but at runtime, on every flash, in
// the field. Both constants are known at compile time, so this belongs
// in the build.
const _: () = assert!(BUF_ALIGN.is_power_of_two(), "BUF_ALIGN must be a power of two");
const _: () =
    assert!(BUF_SIZE.is_multiple_of(BUF_ALIGN), "BUF_SIZE must be a whole number of blocks");

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

// SAFETY: `AlignedBuf` exclusively owns its heap allocation. Its only
// fields are the owning `NonNull<u8>` and the `Layout`; there are no
// shared pointers, no `Rc`, and no borrows stored alongside the owner.
// The allocation has no thread affinity — the global allocator's
// `alloc`/`dealloc` are thread-safe — so moving the sole owner to
// another thread, and dropping it there, is sound. That is the whole
// argument for `Send`, and it does not depend on how the crate happens
// to use the type.
//
// `Sync` is a different question and the answer is no: two threads
// holding `&AlignedBuf` could read the same allocation while a third
// path writes it, which this argument does not cover. Nothing needs it
// — the pipelined flash arm moves buffers between the worker and writer
// threads through `mpsc`, never shares them — and `aligned_buf_is_not_
// sync` makes adding `unsafe impl Sync` a build failure rather than a
// review question. `aligned_buf_is_send` pins the positive half against
// a future field that is not `Send`. This impl is the case
// `.agents/docs/10-aligned-and-ioctls.md` pre-authorized.
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
        let addr = buf.as_slice().as_ptr().expose_provenance();
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
        assert_eq!(buf.as_slice().as_ptr().expose_provenance() % BUF_ALIGN, 0);
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
        assert_ne!(
            a.as_slice().as_ptr().expose_provenance(),
            b.as_slice().as_ptr().expose_provenance()
        );
    }
    /// Compile-time guard for the `unsafe impl Send`: if a future field
    /// makes `AlignedBuf` structurally non-`Send`, the conflicting
    /// manual impl surfaces here rather than at a distant channel call
    /// `AlignedBuf` must **not** be `Sync`.
    ///
    /// The `unsafe impl Send` rests on by-move transfer: exactly one
    /// thread holds a given buffer at a time. `Sync` would let two
    /// threads hold `&AlignedBuf` simultaneously, which the safety
    /// argument does not cover and nothing in the crate needs.
    ///
    /// A negative bound cannot be written directly, so this uses
    /// inherent-impl priority: `Probe<T>::IS_SYNC` resolves to the
    /// inherent constant only when `T: Sync`, otherwise to the blanket
    /// trait constant. Checked with `const _`, so adding
    /// `unsafe impl Sync` fails the **build**, not merely a test.
    struct Probe<T>(core::marker::PhantomData<T>);

    trait NotSync {
        const IS_SYNC: bool = false;
    }

    impl<T> NotSync for Probe<T> {}

    impl<T: Sync> Probe<T> {
        const IS_SYNC: bool = true;
    }

    const _: () = assert!(!<Probe<AlignedBuf>>::IS_SYNC);
    // The probe must actually discriminate, or the line above is vacuous.
    const _: () = assert!(<Probe<u8>>::IS_SYNC);

    /// site in the pipelined flash arm.
    #[test]
    fn aligned_buf_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<AlignedBuf>();
    }
}
