//! Phase 7 — automount defense after the claim is released.
//!
//! [`run`] sweeps away any automount that appeared once userspace
//! could see the device again.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::Result;
use crate::common::cancel::cancellable_sleep;
use crate::common::context::Target;
use crate::common::mount::{self, TargetDevts};
use crate::error::Cancelled;
use crate::error::{Context as _, bail};
use crate::events::{Events, Phase as UiPhase, PhaseOutcome};

/// Sweep away any automount that appeared after the claim was dropped.
///
/// Phases are ordered: this one assumes every earlier phase succeeded.
///
/// # Errors
///
/// Returns an error if the post-release sweep cannot enumerate mounts,
/// or if an automount could not be cleared after repeated attempts.
pub fn run<E: Events + ?Sized>(target: &Target, cancel: &AtomicBool, events: &mut E) -> Result<()> {
    let outcome = phase7_automount_defense(&target.dev_kname, cancel, events)
        .map_err(|e| e.at(crate::DeviceState::Written));
    events.phase_finished(
        UiPhase::Automount,
        if outcome.is_ok() { PhaseOutcome::Completed } else { PhaseOutcome::Failed },
    );
    outcome
}

/// Phase 7: settle, then up-to-three plain-umount sweeps against daemon
/// automounts, ending in an honest final mountinfo verdict.
fn phase7_automount_defense<E: Events + ?Sized>(
    dev_kname: &str,
    cancel: &AtomicBool,
    events: &mut E,
) -> Result<()> {
    events.phase_started(UiPhase::Automount);
    // Let udev process the uevents from BLKRRPART + FD release. Two
    // things happen in this 2-second window:
    //   1. The kernel's `change` uevent (emitted on FD release) triggers
    //      udev to repopulate `/dev/disk/by-*` and any rules-driven
    //      symlinks for the new partition layout.
    //   2. udisks2 reacts to udev's settled state and may issue a mount
    //      for the new filesystem.
    // We then rebuild `TargetDevts` from sysfs *after* the sleep so it
    // covers the BLKRRPART-failure path: when BLKRRPART returns Err we
    // still get the new partition entries, but only via udev's
    // processing of the FD-release change uevent, which is asynchronous.
    // Doing the rebuild inside Phase 7 covers both paths uniformly.
    cancellable_sleep(Duration::from_secs(2), cancel);
    if cancel.load(Ordering::SeqCst) {
        bail!(Cancelled { during: Some("automount defense settle") });
    }

    let devts = TargetDevts::from_disk(dev_kname)
        .context("Phase 7: rebuilding target devt set after udev settle")?;

    for attempt in 1..=3_u32 {
        if cancel.load(Ordering::SeqCst) {
            bail!(Cancelled { during: Some("automount defense") });
        }

        let mounts = mount::mounts_on_target(&devts)
            .with_context(|| format!("Phase 7 attempt {attempt}: scanning mountinfo"))?;
        if mounts.is_empty() {
            return Ok(());
        }

        events.warning(&format!("pass {attempt}: found {} new mount(s)", mounts.len()));
        for m in &mounts {
            events.action(&format!("unmounting {}", m.target.display()));
            // Plain umount, NOT MNT_DETACH. A fresh daemon automount
            // normally unmounts cleanly; if something already has files
            // open on it (tracker-miner, a thumbnailer), a lazy detach
            // would erase it from mountinfo while it stays alive through
            // those fds — and the final scan below would then bless a
            // still-active mount with SUCCESS. Plain umount keeps the
            // final verdict honest: a stubborn mount stays visible and
            // trips the "Do NOT remove" abort instead. Per-mount errors
            // are tolerated here; the next pass (and the final scan)
            // re-evaluate.
            if let Err(e) = nix::mount::umount2(&m.target, nix::mount::MntFlags::empty()) {
                events.warning(&format!("unmount failed: {e}; re-checking on the next pass"));
            }
        }

        cancellable_sleep(Duration::from_secs(2), cancel);
    }

    let still = mount::mounts_on_target(&devts).context("Phase 7: final mountinfo scan")?;
    final_verdict(&still)
}

/// The last word on whether the device is safe to unplug.
///
/// Extracted because it is the decision the whole phase exists to reach,
/// and the only one here that does not need a real device to evaluate.
/// Inverted, it blesses a device a daemon still holds with SUCCESS —
/// which is precisely the outcome the plain-`umount`-not-`MNT_DETACH`
/// choice above is designed to keep visible.
///
/// # Errors
///
/// Returns an error naming the count when any mount remains.
fn final_verdict(still: &[mount::MountInfo]) -> Result<()> {
    if still.is_empty() {
        return Ok(());
    }
    bail!(
        "device still has {} persistent mount(s) after 3 automount-defense passes. \
         Do NOT remove the device. Unmount manually before unplugging.",
        still.len()
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    use super::{final_verdict, run};
    use crate::common::context::Target;
    use crate::common::mount::MountInfo;
    use crate::common::testing::Recorder;
    use crate::events::{Phase as UiPhase, PhaseOutcome};

    /// The verdict must refuse while anything is still mounted.
    ///
    /// This is the sentence that decides whether an operator is told to
    /// unplug. Inverted, it blesses a device a daemon still holds — the
    /// outcome the plain-`umount` choice exists to keep visible, undone
    /// at the last step.
    #[test]
    fn final_verdict_refuses_only_when_mounts_remain() {
        let mk = |t: &str| MountInfo {
            devt: (8, 17),
            target: PathBuf::from(t),
            source: Some(PathBuf::from("/dev/sdb1")),
        };

        final_verdict(&[]).expect("a clean device must pass");

        let one = [mk("/run/media/user/UBUNTU")];
        let err = final_verdict(&one).expect_err("a remaining mount must refuse");
        let text = format!("{err:#}");
        assert!(text.contains("Do NOT remove"), "{text}");
        assert!(text.contains("1 persistent mount"), "{text}");

        let two = [mk("/run/media/user/A"), mk("/run/media/user/B")];
        assert!(
            format!("{:#}", final_verdict(&two).unwrap_err()).contains("2 persistent mount"),
            "the count must be honest"
        );
    }

    /// A cancelled run must still open and close its phase.
    ///
    /// Replacing the whole phase with `Ok(())` leaves a device a daemon
    /// may have mounted, and reports SUCCESS over it. The device paths
    /// need real sysfs, but the cancellation branch runs before any of
    /// that — so the phase boundaries, and the refusal itself, are
    /// observable here.
    #[test]
    fn a_cancelled_defense_closes_its_phase_as_failed() {
        let target = Target::for_test(PathBuf::from("/dev/imi-no-such-device"));
        let cancel = AtomicBool::new(true);
        let mut rec = Recorder::default();

        let err = run(&target, &cancel, &mut rec).expect_err("a set flag must abort");
        assert_eq!(err.kind(), crate::ErrorKind::Cancelled, "{err:#}");
        assert_eq!(
            err.device_state(),
            crate::DeviceState::Written,
            "the flash succeeded; only the cleanup was cancelled"
        );

        assert_eq!(rec.started, vec![UiPhase::Automount], "the phase must open");
        assert_eq!(
            rec.finished,
            vec![(UiPhase::Automount, PhaseOutcome::Failed)],
            "a cancelled phase must close as Failed so a front end keeps its last frame"
        );
    }
}
