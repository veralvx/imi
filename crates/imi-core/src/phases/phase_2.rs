//! Phase 2 — the exclusive claim and the TOCTOU re-check.
//!
//! [`run`] takes the `O_EXCL` claim first, then re-checks under it. The
//! order is the whole point: re-checking and then claiming leaves the
//! window open between the two, which is the race this phase exists to
//! close.
//!
//! Two of the three re-checks confirm what Phase 1 established — no
//! stacked volumes, nothing mounted. The third confirms Phase 0's
//! [`DeviceIdentity`] snapshot, taken before the operator was asked to
//! confirm, and it is the one that catches a stick pulled and replaced
//! between the question and the answer.
//!
//! Why the mount re-check runs even though the claim succeeded is worth
//! being careful about, because the obvious answer is wrong.
//!
//! It is *not* that a partition can be mounted under a claimed disk.
//! `bd_may_claim` in `block/bdev.c` marks the whole device's
//! `bd_holder` when a partition is claimed, so a later whole-disk
//! `O_EXCL` open fails on the first branch — a mounted `/dev/sdb1`
//! blocks a claim of `/dev/sdb`. A mount of the claimed node itself
//! likewise returns `EBUSY`, which is verified: opening a mounted loop
//! device `O_RDWR | O_EXCL` gives errno 16.
//!
//! What the re-check does catch is a mount the *enumeration* missed.
//! `mounts_on_target` matches against the `TargetDevts` set Phase 1
//! captured, so a partition that appeared since — a table re-read, a
//! device-mapper node activated over the disk — carries a devt that set
//! does not contain. The claim would still have refused it; the point of
//! re-enumerating is that a disagreement between the two means the
//! topology moved under us, and this phase would rather stop than
//! reconcile it.
//!
//! [`DeviceIdentity`]: crate::common::identity::DeviceIdentity

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use crate::Result;
use crate::common::context::Target;
use crate::common::guard::FlashGuard;
use crate::common::mount::{self, TargetDevts};
use crate::error::Context as _;
use crate::events::{Events, Phase as UiPhase, PhaseOutcome};

/// Claim the device and hand back the armed-capable guard that owns it.
///
/// Phases are ordered: this one assumes every earlier phase succeeded.
///
/// # Errors
///
/// Returns an error if the `O_EXCL` open fails (`EBUSY` means another
/// process holds the device), if a mount or stacked volume appeared
/// since Phase 1, or if the device identity no longer matches the one
/// the operator confirmed — the replug TOCTOU check.
pub fn run<E: Events + ?Sized>(
    target: &Target,
    devts: &TargetDevts,
    events: &mut E,
) -> Result<FlashGuard> {
    let outcome = run_inner(target, devts, events).map_err(|e| e.at(crate::DeviceState::Untouched));
    events.phase_finished(
        UiPhase::Claim,
        if outcome.is_ok() { PhaseOutcome::Completed } else { PhaseOutcome::Failed },
    );
    outcome
}

/// The phase proper: where `?` and `.context()` chain.
///
/// Split from [`run`] so that every exit path passes through one
/// place that maps the device state and emits `phase_finished`.
fn run_inner<E: Events + ?Sized>(
    target: &Target,
    devts: &TargetDevts,
    events: &mut E,
) -> Result<FlashGuard> {
    events.phase_started(UiPhase::Claim);
    let dev_file =
        open_exclusive(&target.dev_canon).context("Phase 2: opening target device with O_EXCL")?;
    let guard = FlashGuard::new(dev_file, target.dev_canon.clone());

    mount::reject_active_stacked_volumes(&target.dev_kname)
        .context("Phase 2: re-checking stacked volumes under lock")?;
    let mounts =
        mount::mounts_on_target(devts).context("Phase 2: re-enumerating mounts under lock")?;
    mount::ensure_unmounted(
        &mounts,
        "target acquired a new mount after O_EXCL claim (racing udisks2?)",
    )
    .context("Phase 2")?;
    target
        .identity
        .verify_claimed(&guard, &target.dev_kname)
        .context("Phase 2: re-verifying device identity under lock")?;

    Ok(guard)
}

/// The flags the claim is made with, beside `O_RDWR` from
/// `OpenOptions::read`/`write`.
///
/// A named constant so the composition is testable. `O_EXCL` is the
/// whole point of this phase — without it the kernel lets udisks2 open
/// the device concurrently and every mount defence downstream is
/// worthless — and `|` mutated to `&` yields `0o200 & 0o2000000 == 0`,
/// which silently drops both flags while still opening the device.
pub(crate) const CLAIM_FLAGS: i32 = libc::O_EXCL | libc::O_CLOEXEC;

/// Phase 2's exclusive claim: `O_RDWR | O_EXCL | O_CLOEXEC` on the node.
fn open_exclusive(path: &Path) -> Result<File> {
    OpenOptions::new().read(true).write(true).custom_flags(CLAIM_FLAGS).open(path).with_context(
        || {
            format!(
                "opening {} with O_RDWR|O_EXCL|O_CLOEXEC (EBUSY => someone else holds the device)",
                path.display()
            )
        },
    )
}

#[cfg(test)]
mod tests {

    use super::CLAIM_FLAGS;

    /// The claim must actually request exclusivity.
    ///
    /// `O_EXCL` is the whole of Phase 2. Composed with `&` instead of
    /// `|` the value is `0o200 & 0o2000000 == 0`, which opens the device
    /// perfectly well and silently drops the claim — every mount defence
    /// downstream then guards nothing.
    #[test]
    fn claim_flags_request_exclusive_and_cloexec() {
        assert_ne!(CLAIM_FLAGS, 0, "a zeroed flag word opens without claiming");
        assert_eq!(CLAIM_FLAGS & libc::O_EXCL, libc::O_EXCL, "O_EXCL must be set");
        assert_eq!(CLAIM_FLAGS & libc::O_CLOEXEC, libc::O_CLOEXEC, "O_CLOEXEC must be set");
        assert_eq!(CLAIM_FLAGS, libc::O_EXCL | libc::O_CLOEXEC);
    }
}
