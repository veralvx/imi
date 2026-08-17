//! A guard and the target it claimed, paired at construction.
//!
//! Phases 3, 4 and 5 each used to take a `&mut ArmedGuard` and a
//! `&Target` as separate parameters. The guard holds an `O_EXCL` claim on
//! one device; the target describes a device. Nothing in the types
//! required them to be the same one, so every phase opened with
//!
//! ```text
//! guard.ensure_device_is(&target.dev_canon).context("Phase N")?;
//! ```
//!
//! and a caller who paired device A's guard with device B's target was
//! refused at runtime, by a check each phase had to remember to make.
//!
//! A [`Session`] is the pair. [`Session::new`] is the only way to build
//! one and is where the correspondence is established; after that there
//! is nothing to check, because there is no way to express the mismatch.
//!
//! Two types rather than one, mirroring the guard's own split: the guard
//! is a `FlashGuard` before Phase 3 and an `ArmedGuard` after, so the
//! pair is a [`Session`] before and an [`ArmedSession`] after.
//!
//! Putting the pairing here rather than on the guard itself is
//! deliberate. `guard.rs`'s production half imports nothing from
//! `common` — verifiably: `grep 'use crate::common' guard.rs` finds one
//! line, inside `#[cfg(test)]` — so it is a leaf, and it stays one.
//! Teaching it about `Target` would reverse that, making the module that
//! holds the destructive claim depend on the module that describes what
//! is being claimed. This module depends on both instead and sits above
//! them.
//!
//! (An earlier draft of this paragraph cited "the crate's layer table",
//! with `guard` at layer 0 and `context` at layer 2. No such table
//! exists in this repository. The dependency ordering it described is
//! real and is stated above in checkable terms; the table was not.)

use crate::common::context::Target;
use crate::common::guard::{ArmedGuard, ArmedPhase, FlashGuard};

/// A disarmed guard and the target it claimed.
///
/// Produced by Phase 2, consumed by Phase 3. Dropping one is silent: the
/// guard inside is disarmed, and a disarmed guard has nothing to warn
/// about.
#[derive(Debug)]
#[must_use = "dropping a Session releases the exclusive claim Phase 2 just \
              acquired, silently — pass it to Phase 3 or bind it deliberately"]
pub struct Session {
    /// The exclusive claim.
    guard: FlashGuard,
    /// The device that claim is on.
    target: Target,
}

impl Session {
    /// Pair a guard with the target it claimed.
    ///
    /// The only constructor, and the only place the correspondence
    /// between the two is established. Phase 2 calls it once, after the
    /// claim succeeds and the identity re-check has passed.
    pub(crate) fn new(guard: FlashGuard, target: Target) -> Self {
        Self { guard, target }
    }

    /// The target this guard claimed.
    pub(crate) fn target(&self) -> &Target {
        &self.target
    }

    /// Open the destructive window, keeping the pair together.
    ///
    /// Consumes the session, because [`FlashGuard::arm`] consumes the
    /// guard: there is no moment where an armed guard and a disarmed
    /// session both exist.
    pub(crate) fn arm(self, phase: ArmedPhase) -> ArmedSession {
        ArmedSession { guard: self.guard.arm(phase), target: self.target }
    }
}

/// An armed guard and the target it claimed.
///
/// Produced by Phase 3, threaded through Phases 4 and 5, consumed by
/// Phase 6.
#[derive(Debug)]
#[must_use = "dropping an ArmedSession prints the FATAL notice, which would \
              report an untouched device as inconsistent"]
pub struct ArmedSession {
    /// The exclusive claim, armed.
    guard: ArmedGuard,
    /// The device that claim is on.
    target: Target,
}

impl ArmedSession {
    /// The armed guard.
    pub(crate) fn guard(&self) -> &ArmedGuard {
        &self.guard
    }

    /// The armed guard, mutably.
    ///
    /// Needed because the flash and verify entry points take
    /// `&mut ArmedGuard`; a session that only lent the guard immutably
    /// could not reach them.
    pub(crate) fn guard_mut(&mut self) -> &mut ArmedGuard {
        &mut self.guard
    }

    /// The target this guard claimed.
    pub(crate) fn target(&self) -> &Target {
        &self.target
    }

    /// Advance to a later armed phase.
    pub(crate) fn set_phase(&self, phase: ArmedPhase) {
        self.guard.set_phase(phase);
    }

    /// Close the destructive window, yielding the pair apart.
    ///
    /// The target comes back because Phase 7 needs it after Phase 6 has
    /// consumed the session.
    pub(crate) fn disarm(self) -> (FlashGuard, Target) {
        (self.guard.disarm(), self.target)
    }
}

/// Both session types must stay usable from a consumer's own threads.
///
/// The guard types are `Send` and the target is plain data, so this holds
/// structurally today. It is asserted because a future private field
/// could quietly break it, and the failure would surface at a consumer's
/// `thread::spawn` rather than here.
///
/// Written as `const _` rather than a function body. A function whose
/// body *is* the assertion can be replaced with `()` by a mutation
/// tester without changing anything observable, so it shows up as a
/// permanent survivor; a `const` item has no body to gut and the check
/// happens at compile time either way.
const _: () = {
    const fn is_send<T: Send>() {}
    is_send::<Session>();
    is_send::<ArmedSession>();
};

#[cfg(test)]
mod tests {
    use super::{ArmedPhase, Session};
    use crate::common::guard::{FlashGuard, GuardPhase};
    use crate::common::testing::TempPath;

    /// Dropping an `ArmedSession` warns, and disarming it does not.
    ///
    /// The `#[must_use]` on `ArmedSession` promises that dropping one
    /// prints the FATAL notice. That is true only through two levels of
    /// indirection — neither `ArmedSession` nor `ArmedGuard` has a `Drop`
    /// impl, and the notice comes from the `FlashGuard` at the bottom —
    /// so it is asserted here rather than traced by a reader.
    ///
    /// Checked through `current_phase`, because `Drop` warns for exactly
    /// the phases that are not `Disarmed` — so the phase is the predicate,
    /// and no process that observes stderr is needed. `guard.rs` owns the
    /// test that the notice itself fires and what it says.
    #[test]
    fn an_armed_session_would_warn_on_drop_and_a_disarmed_one_would_not() {
        let path = TempPath::new("session-warn");
        let file = std::fs::File::create(&*path).unwrap();
        let target = crate::common::context::Target::for_test(path.to_path_buf());

        let session = Session::new(FlashGuard::new(file, path.to_path_buf()), target);
        let armed = session.arm(ArmedPhase::Writing);
        assert_ne!(
            armed.guard().current_phase(),
            GuardPhase::Disarmed,
            "an ArmedSession must warn if dropped: that is what its #[must_use] promises, \
             and `Drop` warns for exactly the non-Disarmed phases"
        );

        let (guard, _target) = armed.disarm();
        assert_eq!(
            guard.current_phase(),
            GuardPhase::Disarmed,
            "and disarming must silence it, or every clean run ends with a FATAL notice"
        );
    }

    /// The pairing survives arming and comes back out of `disarm`.
    ///
    /// The point of this module is that a guard and its target cannot
    /// drift apart, so the test follows one pair through the whole
    /// lifecycle and checks the target is the same at each end.
    #[test]
    fn a_session_keeps_its_pair_through_arm_and_disarm() {
        let path = TempPath::new("session");
        let file = std::fs::File::create(&*path).unwrap();
        let target = crate::common::context::Target::for_test(path.to_path_buf());
        let expected = target.dev_canon.clone();

        let session = Session::new(FlashGuard::new(file, path.to_path_buf()), target);
        assert_eq!(session.target().dev_canon, expected);

        let armed = session.arm(ArmedPhase::WipingSignatures);
        assert_eq!(armed.target().dev_canon, expected, "arming must not disturb the pairing");
        assert_eq!(
            armed.guard().current_phase(),
            GuardPhase::WipingSignatures,
            "arm must put the guard in the phase it was given"
        );

        // The delegation, not merely its existence: a `set_phase` that did
        // nothing would leave the guard in WipingSignatures, and an
        // interrupted flash would then tell the operator the device was
        // having its signatures wiped when it was being written.
        armed.set_phase(ArmedPhase::Writing);
        assert_eq!(
            armed.guard().current_phase(),
            GuardPhase::Writing,
            "set_phase must reach the guard inside"
        );

        let (guard, returned) = armed.disarm();
        assert_eq!(returned.dev_canon, expected, "disarm must return the same target");
        // The guard is dropped here rather than inspected: with the pair
        // structural there is no accessor that could disagree with the
        // target above, which is the property this module exists for.
        drop(guard);
    }
}
