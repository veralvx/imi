//! Phase 3 — signature wipe. The guard is armed here.
//!
//! [`run`] arms the guard and destroys any pre-existing partition
//! signatures. This is where the destructive window opens: from the
//! `arm` call onwards an interrupted run prints the guard's FATAL
//! notice.
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

use std::sync::atomic::{AtomicBool, Ordering};

use crate::Result;
use crate::common::context::Target;
use crate::common::geometry::WIPE_REGION;
use crate::common::guard::{ArmedPhase, FlashGuard};
use crate::error::Cancelled;
use crate::error::{Context as _, bail};
use crate::events::{Events, Phase as UiPhase, PhaseOutcome};

/// Arm the guard and destroy any pre-existing partition signatures.
///
/// Phases are ordered: this one assumes every earlier phase succeeded.
///
/// # Errors
///
/// Returns an error if either signature-region write fails. The guard
/// is already armed when that happens, so the failure is reported with
/// the FATAL notice.
pub fn run<E: Events + ?Sized>(
    guard: &mut FlashGuard,
    target: &Target,
    cancel: &AtomicBool,
    events: &mut E,
) -> Result<()> {
    let outcome = {
        // Split deliberately. Everything before `arm` leaves the device
        // exactly as it was, so those failures report `Untouched`; from the
        // wipe onward they cannot, and report `Indeterminate`. Tagging the
        // whole phase `Indeterminate` would be safe but would make a caller
        // re-flash a device that was never touched.
        preflight(guard, target, cancel).map_err(|e| e.at(crate::DeviceState::Untouched))?;
        wipe(guard, target, events).map_err(|e| e.at(crate::DeviceState::Indeterminate))
    };
    events.phase_finished(
        UiPhase::Wipe,
        if outcome.is_ok() { PhaseOutcome::Completed } else { PhaseOutcome::Failed },
    );
    outcome
}

/// Checks that run before anything is written.
///
/// The cancellation check belongs here rather than in Phase 4: this is
/// the first destructive phase, and a flag already set when the caller
/// invoked us must not cost the operator their partition table. Phase 4
/// polling alone would wipe the signatures first and only then notice.
fn preflight(guard: &FlashGuard, target: &Target, cancel: &AtomicBool) -> Result<()> {
    guard.ensure_device_is(&target.dev_canon).context("Phase 3")?;
    if cancel.load(Ordering::SeqCst) {
        bail!(Cancelled { during: None });
    }
    Ok(())
}

/// The phase proper: where `?` and `.context()` chain.
///
/// Split from [`run`] so that every exit path passes through one
/// place that maps the device state and emits `phase_finished`.
/// The destructive part: from here the device is being modified.
fn wipe<E: Events + ?Sized>(guard: &mut FlashGuard, target: &Target, events: &mut E) -> Result<()> {
    guard.arm(ArmedPhase::WipingSignatures);
    events.phase_started(UiPhase::Wipe);
    wipe_ends(guard, target.dev_size).context("Phase 3: wiping device signatures")
}

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
fn wipe_ends(guard: &FlashGuard, dev_size: u64) -> Result<()> {
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
    use super::{FlashGuard, WIPE_REGION, preflight, tail_wipe_offset, wipe_ends};

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

    /// The wipe must zero exactly the head and tail regions and leave
    /// everything between them untouched.
    ///
    /// This is the pipeline's first destructive act, and until now it had
    /// no coverage outside the root-gated suites: `wipe_ends` could be
    /// replaced with `Ok(())` and the default test run stayed green.
    #[test]
    fn wipe_ends_zeroes_head_and_tail_only() {
        use std::io::Write as _;

        let dev_size = 4 * WIPE_REGION;
        let len = usize::try_from(dev_size).unwrap();
        let path =
            std::env::temp_dir().join(format!("imi-wipe-{}-{}", std::process::id(), line!()));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&vec![0xFF_u8; len]).unwrap();
        drop(f);

        let file = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let guard = FlashGuard::new(file, path.clone());
        wipe_ends(&guard, dev_size).expect("wipe must succeed on a writable file");
        guard.disarm();
        drop(guard);

        let body = std::fs::read(&path).unwrap();
        assert_eq!(body.len(), len, "wipe must not resize the device");

        let region = usize::try_from(WIPE_REGION).unwrap();
        assert!(body[..region].iter().all(|&b| b == 0), "head region must be zeroed");
        assert!(body[len - region..].iter().all(|&b| b == 0), "tail region must be zeroed");
        assert!(
            body[region..len - region].iter().all(|&b| b == 0xFF),
            "the middle must be left untouched"
        );

        std::fs::remove_file(&path).unwrap();
    }

    /// A device below the head+tail floor must be refused, not partially
    /// wiped.
    #[test]
    fn wipe_ends_refuses_an_undersized_device() {
        let path =
            std::env::temp_dir().join(format!("imi-wipe-{}-{}", std::process::id(), line!()));
        let file = std::fs::File::create(&path).unwrap();
        let guard = FlashGuard::new(file, path.clone());

        let err = wipe_ends(&guard, WIPE_REGION).expect_err("undersized device must be refused");
        assert!(err.to_string().contains("too small"), "{err}");

        guard.disarm();
        drop(guard);
        std::fs::remove_file(&path).unwrap();
    }

    /// Phase 3's pre-flight is the cancellation gate for the whole
    /// destructive half of the pipeline.
    ///
    /// It must refuse a flag that is already set — before `arm`, before
    /// the wipe — and it must refuse a guard holding a different device
    /// than the target names. If it silently returned `Ok`, a caller who
    /// cancelled during the confirmation prompt would still lose their
    /// partition table.
    #[test]
    fn preflight_refuses_a_set_flag_and_a_crossed_pairing() {
        use std::sync::atomic::AtomicBool;

        use crate::common::context::Target;

        let path = std::env::temp_dir().join(format!("imi-pre-{}-{}", std::process::id(), line!()));
        std::fs::write(&path, [0_u8; 64]).unwrap();
        let file = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let guard = FlashGuard::new(file, path.clone());
        let target = Target::for_test(path.clone());

        // Clear flag, matching device: proceed.
        preflight(&guard, &target, &AtomicBool::new(false))
            .expect("a clear flag and a matching device must pass");

        // Flag already set: refuse before anything is written.
        let err = preflight(&guard, &target, &AtomicBool::new(true))
            .expect_err("a set flag must stop Phase 3");
        // Classification first: a string `bail!` would still say
        // "cancelled" while classifying as `Failed`.
        assert_eq!(err.kind(), crate::ErrorKind::Cancelled, "{err}");
        assert!(err.to_string().contains("cancelled"), "{err}");

        // Guard and target naming different devices: refuse.
        let other = Target::for_test(path.with_extension("other"));
        preflight(&guard, &other, &AtomicBool::new(false))
            .expect_err("a crossed guard/target pairing must be refused");

        guard.disarm();
        drop(guard);
        let _rm = std::fs::remove_file(&path);
    }
}
