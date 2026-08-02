//! Phase 1 — topology audit, swap removal and unmounting.
//!
//! Must complete before Phase 2: the kernel refuses an `O_EXCL` open
//! while any partition of the device is still mounted. [`run`] clears
//! the target and returns the devt set later phases re-scan against.

use crate::Result;
use crate::common::context::Target;
use crate::common::mount::{self, TargetDevts};
use crate::error::Context as _;
use crate::events::{Events, Phase as UiPhase, PhaseOutcome};

/// Clear the target of mounts and swaps, returning the devt set that
/// later phases re-scan against.
///
/// Phases are ordered: this one assumes every earlier phase succeeded.
///
/// # Errors
///
/// Returns an error if the target carries a stacked volume (LVM,
/// dm-crypt, MD, zram), if a filesystem is mounted outside the
/// auto-mount whitelist, or if anything remains mounted after the
/// unmount attempts.
pub fn run<E: Events + ?Sized>(target: &Target, events: &mut E) -> Result<TargetDevts> {
    let outcome = run_inner(target, events).map_err(|e| e.at(crate::DeviceState::Untouched));
    events.phase_finished(
        UiPhase::Topology,
        if outcome.is_ok() { PhaseOutcome::Completed } else { PhaseOutcome::Failed },
    );
    outcome
}

/// The phase proper: where `?` and `.context()` chain.
///
/// Split from [`run`] so that every exit path passes through one
/// place that maps the device state and emits `phase_finished`.
fn run_inner<E: Events + ?Sized>(target: &Target, events: &mut E) -> Result<TargetDevts> {
    events.phase_started(UiPhase::Topology);
    mount::reject_active_stacked_volumes(&target.dev_kname)
        .context("Phase 1: rejecting active stacked volumes")?;

    let devts =
        TargetDevts::from_disk(&target.dev_kname).context("Phase 1: building target devt set")?;
    let mounts =
        mount::mounts_on_target(&devts).context("Phase 1: enumerating mounts on target")?;
    mount::enforce_whitelist(&mounts, &target.dev_canon).context("Phase 1: whitelist check")?;

    mount::disable_swaps_on_target(&devts, events).context("Phase 1: disabling swaps on target")?;

    // No emptiness guard: `unmount_all` iterates the slice, so an empty
    // one is already a no-op that emits nothing. The guard it replaces
    // was a branch whose negation — `if mounts.is_empty()` — skipped
    // unmounting exactly when there was something to unmount, leaving
    // auto-unmount silently broken and caught only by the residual
    // re-scan refusing afterwards. Nothing reachable without root could
    // tell the two apart, so the branch is gone instead.
    mount::unmount_all(&mounts, events).context("Phase 1: unmounting partitions of target")?;

    let residual =
        mount::mounts_on_target(&devts).context("Phase 1: re-scanning mounts after unmount")?;
    mount::ensure_unmounted(&residual, "target still mounted after unmount attempts")
        .context("Phase 1")?;

    Ok(devts)
}
