//! Phase 3 — signature wiping.
//!
//! Zeroes the first and last 1 MiB of the target device. This reliably
//! destroys MBR, primary GPT, backup GPT, and almost every filesystem
//! superblock placed near the start of a volume. It is *not* a full
//! `wipefs` replacement (e.g., btrfs places secondary superblocks at
//! 64 MiB / 256 GiB / 1 PiB), but the ISO content Phase 4 writes will
//! overwrite any surviving legacy signatures within its footprint; the
//! 1 MiB tail wipe kills the backup GPT header at LBA-1..-33.
//!
//! Deliberately uses buffered `pwrite` (via `FileExt::write_all_at`),
//! **not** `O_DIRECT`. `O_DIRECT` demands sector-aligned offsets, lengths,
//! and buffers; feeding it 1 MiB writes at `device_size - 1 MiB` would
//! pass alignment today, but a future refactor that changes the wipe size
//! could hit `EINVAL` silently. Not worth the fragility for a one-shot
//! pre-flash step.

use std::os::unix::fs::FileExt;

use anyhow::{Context, Result, bail};

use crate::guard::FlashGuard;

/// Size of the region to zero at head and tail.
pub(crate) const WIPE_REGION: u64 = 1024 * 1024;

/// Tail-wipe offset for a device of `dev_size` bytes, or `None` when the
/// device cannot hold non-overlapping head and tail regions
/// (`dev_size < 2 * WIPE_REGION`): `checked_sub` refuses a device smaller
/// than one region, and the `t >= WIPE_REGION` filter refuses head/tail
/// overlap — together exactly the `2 * WIPE_REGION` bound, with the offset
/// arithmetic and the bound provably the same expression. Pure;
/// unit-tested below.
fn tail_wipe_offset(dev_size: u64) -> Option<u64> {
    dev_size.checked_sub(WIPE_REGION).filter(|&t| t >= WIPE_REGION)
}

/// Wipe 1 MiB at offset 0 and 1 MiB at `dev_size - 1 MiB`.
///
/// The guard must already be armed — this is the first destructive
/// operation in the pipeline, so if it aborts mid-write the `FlashGuard`'s
/// warning is exactly what the operator needs to see.
pub(crate) fn wipe_ends(guard: &FlashGuard, dev_size: u64) -> Result<()> {
    // Phase 0 refuses undersized devices before the guard is armed (see
    // `run()`), so this bail is defense in depth: it fires only if a
    // future caller reaches the wipe without that preflight, and then a
    // FATAL-warning-plus-refusal is the correct fail-loud outcome.
    let Some(tail_offset) = tail_wipe_offset(dev_size) else {
        bail!(
            "device is too small ({dev_size} bytes) for a head+tail signature \
             wipe; refusing to flash"
        )
    };

    // `WIPE_REGION` is a `u64`. On 64-bit Linux (the only target we
    // support) the conversion to `usize` is lossless, but using `try_from`
    // makes a future bump of `WIPE_REGION` past `usize::MAX` (or a future
    // 32-bit cross-compile) surface as a clean pre-write error rather
    // than a silently-truncated zero buffer that produces a bogus partial
    // wipe. The cost is one bounds check on a 1 MiB allocation.
    let wipe_len =
        usize::try_from(WIPE_REGION).context("WIPE_REGION does not fit in usize on this target")?;
    // try_reserve_exact + resize instead of vec![]: on OOM this returns
    // Err (unwinds through the armed guard, FATAL warning fires) rather
    // than aborting via handle_alloc_error (which would skip Drop).
    let mut zeros = Vec::new();
    zeros
        .try_reserve_exact(wipe_len)
        .context("allocating the 1 MiB wipe buffer (out of memory)")?;
    zeros.resize(wipe_len, 0_u8);

    guard.file().write_all_at(&zeros, 0).context("zeroing first 1 MiB of device")?;

    guard.file().write_all_at(&zeros, tail_offset).context("zeroing last 1 MiB of device")?;

    // Durable commit before Phase 4 starts — if anything fatal happens
    // during the flash, the signature wipe should have stuck.
    //
    // `guard.file()` returns `&File`, which implements `AsFd`. nix ≥ 0.30
    // requires that here.
    nix::unistd::fdatasync(guard.file()).context("fdatasync after signature wipe")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{WIPE_REGION, tail_wipe_offset};

    /// The overlap bound: below two regions there is no valid layout —
    /// head [0, R) and tail [dev-R, dev) would intersect. (Kills the
    /// mutant that drops the `t >= WIPE_REGION` filter.)
    #[test]
    fn tail_offset_refuses_undersized_devices() {
        assert_eq!(tail_wipe_offset(0), None);
        assert_eq!(tail_wipe_offset(WIPE_REGION), None);
        assert_eq!(tail_wipe_offset(2 * WIPE_REGION - 1), None);
    }

    /// Exactly two regions is the smallest legal device: head and tail
    /// abut with zero overlap, tail starting at `WIPE_REGION`.
    #[test]
    fn tail_offset_accepts_exact_minimum() {
        assert_eq!(tail_wipe_offset(2 * WIPE_REGION), Some(WIPE_REGION));
    }

    /// Ordinary devices: tail sits exactly one region before the end.
    #[test]
    fn tail_offset_is_one_region_before_end() {
        let dev = 64 * 1024 * 1024;
        assert_eq!(tail_wipe_offset(dev), Some(dev - WIPE_REGION));
    }
}
