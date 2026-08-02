//! Phase 6 — kernel partition-table sync, then release of the claim.
//!
//! [`run`] disarms the guard, asks the kernel to re-read the new
//! partition table, and releases the exclusive claim.

use crate::common::guard::FlashGuard;
use crate::common::ioctl;
use crate::events::{Events, Phase as UiPhase, PhaseOutcome};

/// Close the destructive window and hand the device back to userspace.
///
/// Consumes the guard: releasing the `O_EXCL` claim is exactly the act
/// of dropping the file it owns, so the type system enforces that no
/// later phase can still be holding it.
///
/// Phases are ordered: this one assumes every earlier phase succeeded.
pub fn run<E: Events + ?Sized>(guard: FlashGuard, events: &mut E) {
    // Successfully past the destructive window.
    guard.disarm();

    events.phase_started(UiPhase::KernelSync);

    // BLKRRPART under the O_EXCL claim. Non-fatal on failure.
    //
    // Necessary as an ioctl: there is no safe wrapper for BLKRRPART in
    // `nix`, and the userspace alternatives — `partprobe`, `partx -u` —
    // are external programs a library cannot depend on. Unlike the size
    // read in Phase 0, there is no attribute in sysfs that asks the
    // kernel to re-read a partition table; sysfs reports state, it does
    // not trigger this.
    //
    // SAFETY: `guard` still owns a valid, O_EXCL-claimed FD for the
    // target block device — ownership of the `FlashGuard` this function
    // consumes is what enforces that. BLKRRPART takes no argument and
    // writes nothing through a pointer, so there is no user memory for
    // it to touch; failure surfaces as `Err` from the nix wrapper.
    //
    // The block covers the call alone. It used to enclose the `if let`
    // and the `events.warning` call as well, which put an arbitrary
    // front-end callback inside `unsafe` — a trait method a consumer
    // implements, running in a scope that claims to have checked its
    // preconditions.
    let rrpart = unsafe { ioctl::blkrrpart(guard.as_raw_fd()) };
    if let Err(e) = rrpart {
        events.warning(&format!("BLKRRPART failed ({e}); proceeding anyway"));
    }

    // Release the O_EXCL claim so udisks2/systemd-udevd can see the device
    // and present the new partition table to userspace. Phase 7 then
    // defends against any race-condition re-mounts.
    drop(guard.into_file());
    events.phase_finished(UiPhase::KernelSync, PhaseOutcome::Completed);
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use super::{FlashGuard, run};
    use crate::common::guard::ArmedPhase;
    use crate::common::testing::Recorder;
    use crate::events::{Phase as UiPhase, PhaseOutcome};

    /// The phase must disarm the guard and report itself to the front
    /// end.
    ///
    /// Replacing this function wholesale with a no-op leaves the guard
    /// armed, so a successful flash ends by telling the operator the
    /// device is inconsistent — and closes no phase, so a front end
    /// leaves the last bar on screen forever. The integration suite
    /// catches the first half, but only behind `--ignored`; the events
    /// are observable here.
    #[test]
    #[cfg_attr(miri, ignore)] // unsupported operation 
    fn kernel_sync_disarms_and_closes_its_phase() {
        let path = std::env::temp_dir().join(format!("imi-p6-{}", std::process::id()));
        let file = File::create(&path).expect("temp file");
        let guard = FlashGuard::new(file, path.clone());
        guard.arm(ArmedPhase::Verifying);
        assert!(guard.would_warn_on_drop(), "precondition: armed on entry");

        let mut rec = Recorder::default();
        run(guard, &mut rec);

        assert_eq!(rec.started, vec![UiPhase::KernelSync], "the phase must open");
        assert_eq!(
            rec.finished,
            vec![(UiPhase::KernelSync, PhaseOutcome::Completed)],
            "the phase must close, and as Completed — BLKRRPART is best-effort"
        );

        let _rm = std::fs::remove_file(&path);
    }
}
