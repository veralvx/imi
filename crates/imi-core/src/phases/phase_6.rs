//! Phase 6 — kernel partition-table sync, then release of the claim.
//!
//! [`run`] disarms the guard, asks the kernel to re-read the new
//! partition table, and releases the exclusive claim.

use crate::common::context::Target;
use crate::common::ioctl;
use crate::common::session::ArmedSession;
use crate::events::{Events, Phase as UiPhase, PhaseOutcome};

/// Close the destructive window and hand the device back to userspace.
///
/// Consumes the guard: releasing the `O_EXCL` claim is exactly the act
/// of dropping the file it owns, so the type system enforces that no
/// later phase can still be holding it.
///
/// Phases are ordered: this one assumes every earlier phase succeeded.
pub fn run<E: Events + ?Sized>(session: ArmedSession, events: &mut E) -> Target {
    // Successfully past the destructive window.
    // The target comes back out because Phase 7 runs after this and
    // still needs it; the session is consumed here.
    let (guard, target) = session.disarm();

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
    target
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use super::run;
    use crate::common::context::Target;
    use crate::common::guard::ArmedPhase;
    use crate::common::guard::FlashGuard;
    use crate::common::session::Session;
    use crate::common::testing::Recorder;
    use crate::common::testing::TempPath;
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
    #[cfg_attr(
        miri,
        ignore = "run issues a real BLKRRPART ioctl, a foreign function Miri cannot execute"
    )]
    fn kernel_sync_disarms_and_closes_its_phase() {
        let path = TempPath::new("p6");
        let file = File::create(&*path).expect("temp file");
        let guard = FlashGuard::new(file, path.to_path_buf());
        let session =
            Session::new(guard, Target::for_test(path.to_path_buf())).arm(ArmedPhase::Verifying);
        // "Armed on entry" is no longer an assertion: `run` takes an
        // `ArmedGuard`, so holding one to pass in *is* the precondition.
        //
        // Nor is the disarm asserted here, and it cannot be — `run`
        // consumes the guard, so there is nothing left to inspect. It is
        // enforced instead: `disarm` is the only route from `ArmedGuard`
        // to a `FlashGuard`, and only a `FlashGuard` has `into_file`, so
        // a `run` that skipped it could not release the claim without
        // dropping an armed guard and printing a spurious FATAL. That
        // last case is what `full_pipeline_flashes_byte_exact` catches,
        // by asserting no FATAL appears on a clean run.
        //
        // What is left for this test is the pair of events, which nothing
        // else pins at unit level.

        let mut rec = Recorder::default();
        let _target = run(session, &mut rec);

        assert_eq!(rec.started, vec![UiPhase::KernelSync], "the phase must open");
        assert_eq!(
            rec.finished,
            vec![(UiPhase::KernelSync, PhaseOutcome::Completed)],
            "the phase must close, and as Completed — BLKRRPART is best-effort"
        );
    }
}
