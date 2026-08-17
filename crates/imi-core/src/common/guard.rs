//! `FlashGuard` — RAII wrapper around the exclusively-locked block-device FD.
//!
//! The guard is constructed after the `O_EXCL` open succeeds. It exists to
//! guarantee that if the program unwinds (panic, `?`-propagated error, Ctrl+C)
//! *while armed*, a loud, specific warning is printed to `stderr` telling the
//! operator that the device is partially written and must not be removed.
//!
//! The guard tracks which phase is currently active so that the warning
//! describes the operation that was in flight honestly. A fault during
//! verification reads is *not* "while writing" — the device is still
//! inconsistent (Phase 5b runs before the kernel partition-table sync of
//! Phase 6, and the FD is still held under `O_EXCL`), but the verb in the
//! warning matters for operator trust.
//!
//! Lifecycle. Note which steps *consume* their receiver — the transitions
//! between the two types do, so the value must be rebound:
//!
//! 1. `let guard = FlashGuard::new(file, dev_path)` right after the
//!    `O_EXCL` open. Disarmed.
//! 2. `let armed = guard.arm(ArmedPhase::WipingSignatures)` immediately
//!    before Phase 3. Consumes the `FlashGuard`; the result is
//!    `#[must_use]`, because an `ArmedGuard` dropped on the spot prints
//!    the FATAL notice over a device nothing has touched.
//! 3. `armed.set_phase(ArmedPhase::Writing)` at the start of Phase 4.
//! 4. `armed.set_phase(ArmedPhase::Cooldown)` at the start of Phase 5a.
//! 5. `armed.set_phase(ArmedPhase::Verifying)` at the start of Phase 5b.
//! 6. `let guard = armed.disarm()` after verification (Phase 5) passes.
//!    Consumes the `ArmedGuard`.
//! 7. `guard.into_file()` when the caller wants the `File` back to drop it
//!    before Phase 7 (releasing the `O_EXCL` claim so udisks2 can see it).
//!
//! Note: Rust does not run `Drop` on `SIGINT` by default. The caller installs
//! a `ctrlc` handler elsewhere that sets a cancel flag; long-running loops
//! check the flag and return `Err`, which drives normal unwind and our `Drop`.

use std::fs::File;
use std::io::{self, Write as _};
use std::os::unix::io::{AsRawFd, RawFd};

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};

/// Which phase of the destructive pipeline the guard is currently armed for.
///
/// Encoded as a `u8` so the whole guard state can live behind a single
/// `AtomicU8` — no `Mutex`, no allocation, no signal-handler interaction
/// concerns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[non_exhaustive]
pub enum GuardPhase {
    /// Guard not armed. Drop is silent.
    Disarmed = 0,
    /// Phase 3 — head/tail signature wipe in progress.
    WipingSignatures = 1,
    /// Phase 4 — image content being streamed to the device.
    Writing = 2,
    /// Phase 5a — hardware cooldown (no I/O issued, but device is still
    /// in mid-flash state from the operator's perspective).
    Cooldown = 3,
    /// Phase 5b — byte-for-byte readback compare.
    Verifying = 4,
}

impl GuardPhase {
    /// Decode the atomic's `u8` back into a phase. Every store goes
    /// through `phase as u8` of a real variant, so the wildcard arm is
    /// unreachable in practice — and it maps to `Writing`, not
    /// `Disarmed`, on purpose: if the state were ever unrepresentable,
    /// a spurious FATAL warning (fail-loud) is the acceptable failure
    /// mode for this tool; a silent drop (fail-open) is not.
    fn from_u8(v: u8) -> Self {
        match v {
            0 => GuardPhase::Disarmed,
            1 => GuardPhase::WipingSignatures,
            3 => GuardPhase::Cooldown,
            4 => GuardPhase::Verifying,
            _ => GuardPhase::Writing,
        }
    }

    /// Human-readable verb describing the active operation, for the
    /// "FATAL: flash interrupted while X was being …" warning.
    fn interrupted_verb(self) -> &'static str {
        match self {
            GuardPhase::Disarmed => "in an unknown state",
            GuardPhase::WipingSignatures => "having its partition signatures wiped",
            GuardPhase::Writing => "being written",
            // Cooldown does no I/O; the danger is solely "we already wrote
            // bytes; the controller hasn't finished draining DRAM to NAND".
            GuardPhase::Cooldown => "settling its NAND/FTL state after the write",
            GuardPhase::Verifying => "being read back for verification",
        }
    }
}

/// RAII guard over a block-device `File` held with `O_EXCL`.
#[derive(Debug)]
pub struct FlashGuard {
    /// Stored as `Option` so `into_file` can take ownership without triggering
    /// the warning, while still leaving the guard valid for its own `Drop` to
    /// run harmlessly.
    file: Option<File>,
    /// Device path, used only for the FATAL warning text.
    dev_path: PathBuf,
    /// Active phase encoded as a `u8` (see `GuardPhase`). `Disarmed` means
    /// drop is silent.
    phase: AtomicU8,
}

/// A phase the guard can be *armed* in.
///
/// Deliberately not [`GuardPhase`]: that enum includes `Disarmed`, and
/// arming a guard into `Disarmed` would make
/// [`FlashGuard::would_warn_on_drop`] return `false` — suppressing the
/// FATAL notice on a device that is halfway through being written.
///
/// That case used to be a `debug_assert`, which is compiled out of the
/// release builds a consumer ships. Making it unrepresentable removes
/// the possibility instead of reporting it in the one build where it
/// cannot happen anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArmedPhase {
    /// Phase 3 — wiping partition signatures.
    WipingSignatures,
    /// Phase 4 — writing the image.
    Writing,
    /// Phase 5a — waiting out the device's cache.
    Cooldown,
    /// Phase 5b — reading the device back.
    Verifying,
}

impl From<ArmedPhase> for GuardPhase {
    fn from(p: ArmedPhase) -> Self {
        match p {
            ArmedPhase::WipingSignatures => Self::WipingSignatures,
            ArmedPhase::Writing => Self::Writing,
            ArmedPhase::Cooldown => Self::Cooldown,
            ArmedPhase::Verifying => Self::Verifying,
        }
    }
}

impl FlashGuard {
    /// Construct a new guard around a freshly-opened, `O_EXCL`-claimed device.
    /// The guard starts disarmed; no destructive operation has happened yet.
    pub(crate) fn new(file: File, dev_path: PathBuf) -> Self {
        Self { file: Some(file), dev_path, phase: AtomicU8::new(GuardPhase::Disarmed as u8) }
    }

    /// Arm the guard for the given phase. From this point, any early drop
    /// prints the warning with a phase-appropriate verb.
    pub(crate) fn arm(self, phase: ArmedPhase) -> ArmedGuard {
        self.phase.store(GuardPhase::from(phase) as u8, Ordering::SeqCst);
        ArmedGuard { inner: Some(self) }
    }

    /// Update which phase the guard is currently in. The guard must already
    /// be armed (i.e. you cannot use `set_phase` to arm an initially-disarmed
    /// guard — call `arm()` for that, to make the intent explicit at the
    /// arming point).
    fn advance_phase(&self, phase: ArmedPhase) {
        // A `debug_assert`, and therefore absent from the builds a
        // consumer ships. That is tolerable only because arming a
        // disarmed guard fails *loud*: the guard starts warning on drop
        // rather than staying silent, so the worst outcome is a FATAL
        // notice over a device that was never written.
        //
        // Total by construction: this advances an *already armed* guard
        // and cannot create an armed one.
        //
        // It used to `store` unconditionally under a `debug_assert`,
        // which is absent from the builds a consumer ships — so in
        // release, calling it on a disarmed guard silently armed it, and
        // the guard then warned on drop about a device the run never
        // wrote. A false FATAL, and false ones are what teach an operator
        // to ignore the real ones.
        //
        // `try_update` rather than load-then-store: the read and the
        // write are one operation, so the answer cannot change between
        // them. Nothing shares a guard across threads today — only the
        // main thread touches the descriptor, see `11-threading.md` — but
        // a total operation that is also atomic costs nothing and removes
        // the question from a future reader.
        //
        // This does not replace `ensure_armed`, which answers a different
        // question: whether Phase 3 wiped the device. A caller reaching
        // Phase 4 or 5 without that needs an error, not a silently
        // skipped state change.
        let _unchanged_if_disarmed =
            self.phase.try_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                (current != GuardPhase::Disarmed as u8).then_some(GuardPhase::from(phase) as u8)
            });
    }

    /// Disarm the guard. Subsequent drops are silent.
    fn clear_phase(&self) {
        self.phase.store(GuardPhase::Disarmed as u8, Ordering::SeqCst);
    }

    /// Borrow the inner `File` (non-consumingly).
    #[expect(
        clippy::expect_used,
        reason = "unreachable: into_file consumes the guard by value, so no \
                  &self method can run after it; the Option exists solely so \
                  Drop can run harmlessly on the moved-out shell"
    )]
    pub(crate) fn file(&self) -> &File {
        self.file.as_ref().expect("FlashGuard::file called after into_file")
    }

    /// The phase this guard would report if dropped right now.
    ///
    /// Exists so the `arm`/`set_phase`/`disarm` state machine can be
    /// asserted directly. Without it every one of those methods could be
    /// replaced by a no-op and the suite would stay green, because the
    /// FATAL notice they drive is written to stderr from `Drop`, which
    /// no in-process test observes.
    ///
    /// Crate-internal, and the narrowing is the typestate split's doing.
    ///
    /// This was `pub` so a caller driving [`crate::phases`] by hand could
    /// tell whether it was inside the destructive window. It cannot
    /// usefully answer that any more: a consumer holding a `FlashGuard`
    /// is outside the window *by construction*, so the answer is always
    /// `Disarmed`. The question moved to the type, and the informative
    /// version is [`ArmedGuard::current_phase`], which is `pub` and is
    /// what keeps [`GuardPhase`] a produced type rather than an exported
    /// name with no producer.
    pub(crate) fn current_phase(&self) -> GuardPhase {
        GuardPhase::from_u8(self.phase.load(Ordering::SeqCst))
    }

    /// Whether dropping now would emit the FATAL notice.
    ///
    /// Equivalent to "the device is mid-flash". `Drop` consults exactly
    /// this, so a caller can ask the same question the guard will ask,
    /// and a test on it pins the polarity of the decision. Inverting the
    /// check would mean staying silent on a half-written device and
    /// crying wolf on a clean one — the single worst failure this type
    /// can have.
    /// Crate-internal since the typestate split: for every `FlashGuard` a
    /// consumer can hold this is `false`. `arm` consumes the guard into an
    /// `ArmedGuard`, and `disarm` clears the phase on the way back, so the
    /// only guard for which it is `true` lives inside an `ArmedGuard` and
    /// is not publicly reachable. A public method that always answers the
    /// same thing tells a caller nothing and invites them to build on it.
    /// Private for the same reason as [`Self::current_phase`]: a consumer
    /// holding a `FlashGuard` is outside the destructive window, so this
    /// is constantly `false` for them. It survives as the predicate
    /// `fatal_notice` consults.
    fn would_warn_on_drop(&self) -> bool {
        !matches!(self.current_phase(), GuardPhase::Disarmed)
    }

    /// The notice this guard would print if dropped now, if any.
    ///
    /// Extracted from `Drop` so the decision and the wording are testable
    /// without a process that can observe stderr. `Drop` is then a thin
    /// I/O wrapper — the one mutant category AGENTS.md records as an
    /// acceptable survivor — and everything that can be wrong about
    /// *what* it says is reachable from a unit test.
    ///
    /// Before this split `replace drop with ()` survived: no non-root
    /// test observed the notice, and the root-gated one that does runs
    /// behind `--ignored`, which `cargo mutants` does not execute.
    fn fatal_notice(&self) -> Option<String> {
        if !self.would_warn_on_drop() {
            return None;
        }
        Some(format!(
            "\n\u{26A0}  FATAL: flash interrupted while {} was {}. \
             The device is in an inconsistent state. DO NOT REMOVE IT. \
             Re-run imi to recover.",
            self.dev_path.display(),
            self.current_phase().interrupted_verb()
        ))
    }

    /// Raw fd of the held device.
    pub(crate) fn as_raw_fd(&self) -> RawFd {
        self.file().as_raw_fd()
    }

    /// Consume the guard and return the inner `File`. The caller is then
    /// responsible for dropping it (which releases the `O_EXCL` claim).
    ///
    /// Disarms automatically: by the time the caller wants the `File`
    /// back, the destructive region is over.
    ///
    /// This is the second disarm on the success path — Phase 6 calls
    /// [`FlashGuard::disarm`] before handing the guard over — and the
    /// redundancy is deliberate. Either one alone keeps a successful
    /// flash silent, so losing one is invisible; both must go before
    /// the FATAL notice appears over a device that is perfectly fine,
    /// which `full_pipeline_flashes_byte_exact` now asserts against.
    /// Do not remove one on the grounds that the other covers it.
    #[expect(
        clippy::expect_used,
        reason = "unreachable: into_file takes self by value, so it cannot \
                  be called twice on the same guard"
    )]
    pub(crate) fn into_file(mut self) -> File {
        self.clear_phase();
        self.file.take().expect("FlashGuard::into_file called twice")
    }
}

/// A [`FlashGuard`] whose destructive window is open.
///
/// Phase 3 produces one by consuming a disarmed guard, and only this type
/// exposes the operations that belong inside that window. Phases 4 and 5
/// take `&mut ArmedGuard`, so reaching them without Phase 3 is a compile
/// error rather than a check someone has to remember to write.
///
/// That is not hypothetical. Phase 4 carried a runtime check for exactly
/// this and Phase 5 did not, for 116 commits, through a file-by-file
/// review that read both files. Nothing caught it — not clippy, not the
/// tests, not `cargo mutants`, because there was nothing to mutate.
///
/// The wrapper holds an `Option` rather than the guard directly so
/// `disarm` can take the inner value out without
/// `unsafe`. `ManuallyDrop::take` is the alternative and needs an unsafe
/// block; adding one to a crate that audits every unsafe site, in order
/// to delete a `debug_assert`, is the wrong trade. `FlashGuard::into_file`
/// already uses the same `Option` for the same reason.
///
/// There is deliberately no `Drop` impl here. Dropping an `ArmedGuard`
/// drops the `FlashGuard` inside it, whose own `Drop` prints the FATAL
/// notice — so the warning behaviour is defined in exactly one place.
#[derive(Debug)]
#[must_use = "dropping an ArmedGuard prints the FATAL notice — a guard armed and \
              immediately discarded reports an untouched device as inconsistent. \
              Bind it, or call `disarm()` if the window is over."]
pub struct ArmedGuard {
    /// `None` only after `disarm` has taken it, at which point the shell
    /// is about to be dropped and has nothing left to warn about.
    inner: Option<FlashGuard>,
}

impl ArmedGuard {
    /// The guard inside.
    #[expect(
        clippy::expect_used,
        reason = "unreachable: disarm takes self by value, so no &self method \
                  can run after the Option is emptied"
    )]
    fn guard(&self) -> &FlashGuard {
        self.inner.as_ref().expect("ArmedGuard used after disarm")
    }

    /// Advance to a later armed phase, changing the verb in the notice.
    ///
    /// No check is needed and none is possible: holding this type *is*
    /// the proof that the guard is armed.
    pub(crate) fn set_phase(&self, phase: ArmedPhase) {
        self.guard().advance_phase(phase);
    }

    /// Close the destructive window, yielding the disarmed guard.
    ///
    /// Consuming `self` is what makes this a one-way door: there is no
    /// second call, and no way to keep using the armed operations
    /// afterwards.
    #[expect(
        clippy::expect_used,
        reason = "unreachable: takes self by value, so the Option is full"
    )]
    pub(crate) fn disarm(mut self) -> FlashGuard {
        let guard = self.inner.take().expect("ArmedGuard::disarm called twice");
        guard.clear_phase();
        guard
    }

    /// The claimed device's open file, for the phases that write to it.
    pub(crate) fn file(&self) -> &File {
        self.guard().file()
    }

    /// The claimed descriptor, for the ioctl wrappers.
    pub(crate) fn as_raw_fd(&self) -> RawFd {
        self.guard().as_raw_fd()
    }

    /// The notice this guard would print if dropped now.
    ///
    /// Always `Some` — holding an `ArmedGuard` is what makes it so. Here
    /// for the tests that assert the wording, which would otherwise have
    /// to reach through the private inner accessor and around the type
    /// boundary this split exists to draw.
    #[cfg(test)]
    /// The notice the guard inside would print if dropped now.
    ///
    /// Exists for `guard.rs`'s own tests: `Drop` lives on `FlashGuard`
    /// and calls its `fatal_notice` directly, so this delegation has no
    /// production caller. Kept private for that reason — it is a window
    /// into the wrapper for the test that asserts the notice tracks the
    /// armed phase, not part of the crate's surface.
    fn fatal_notice(&self) -> Option<String> {
        self.guard().fatal_notice()
    }

    /// Which armed phase the guard is in.
    #[must_use]
    pub fn current_phase(&self) -> GuardPhase {
        self.guard().current_phase()
    }
}

impl Drop for FlashGuard {
    fn drop(&mut self) {
        if let Some(notice) = self.fatal_notice() {
            // `writeln!` rather than `eprintln!`, and the result
            // deliberately discarded.
            //
            // `eprintln!` panics if the write fails — a closed or full
            // stderr, an EPIPE from a dead `less`. This runs from `Drop`,
            // and the case it exists for is unwinding, where a second
            // panic aborts the process immediately. That abort would skip
            // this very notice and every remaining destructor, turning
            // "your device is half-written" into a bare `SIGABRT`.
            // Reproduced at exit 134 during this file's code review, by
            // piping stderr to a reader that exits.
            //
            // If stderr is gone the operator cannot be told regardless;
            // what matters is that the attempt cannot make things worse.
            let mut err = io::stderr().lock();
            let _notice = writeln!(err, "{notice}");
            let _flushed = err.flush();
        }
        // `self.file` (if still `Some`) drops here, releasing the claim.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testing::{ArmedForTest, TempPath};

    /// Each phase must map to a distinct, descriptive verb so the FATAL
    /// warning honestly describes what was in flight at unwind time. A
    /// regression here (e.g. all phases collapsing to "being written")
    /// is exactly the bug we are protecting against.
    #[test]
    fn each_phase_has_a_distinct_verb() {
        let verbs = [
            GuardPhase::WipingSignatures.interrupted_verb(),
            GuardPhase::Writing.interrupted_verb(),
            GuardPhase::Cooldown.interrupted_verb(),
            GuardPhase::Verifying.interrupted_verb(),
        ];
        let unique: std::collections::HashSet<&&str> = verbs.iter().collect();
        assert_eq!(unique.len(), verbs.len(), "phase verbs must be distinct: {verbs:?}");
    }

    #[test]
    fn verifying_phase_does_not_say_written() {
        let verb = GuardPhase::Verifying.interrupted_verb();
        assert!(
            !verb.contains("written") && !verb.contains("writing"),
            "Verifying phase verb should not mention writing, got: {verb:?}"
        );
        assert!(
            verb.contains("read") || verb.contains("verif"),
            "Verifying phase verb should mention reading or verification, got: {verb:?}"
        );
    }

    #[test]
    fn from_u8_round_trips_known_phases() {
        for p in [
            GuardPhase::Disarmed,
            GuardPhase::WipingSignatures,
            GuardPhase::Writing,
            GuardPhase::Cooldown,
            GuardPhase::Verifying,
        ] {
            assert_eq!(GuardPhase::from_u8(p as u8), p);
        }
        // Unknown values fail LOUD (armed as Writing), never silent:
        // an unrepresentable phase byte must not suppress the warning.
        assert_eq!(GuardPhase::from_u8(99), GuardPhase::Writing);
        assert_eq!(GuardPhase::from_u8(255), GuardPhase::Writing);
    }

    /// The FATAL notice: whether it fires, and what it says.
    ///
    /// `Drop` cannot be tested directly without a process that can
    /// observe stderr, so the decision and the wording live in
    /// `fatal_notice` and are asserted here. Both directions, because a
    /// `fatal_notice` that always returned `None` would pass a
    /// silence-only test while leaving a half-written device unannounced.
    #[test]
    fn fatal_notice_fires_only_when_armed_and_names_the_device_and_phase() {
        let path = TempPath::new("guard-notice");
        let file = File::create(&*path).unwrap();
        let guard = FlashGuard::new(file, path.to_path_buf());

        assert!(guard.fatal_notice().is_none(), "a disarmed guard must say nothing");

        let armed = guard.arm(ArmedPhase::Writing);
        let notice = armed.fatal_notice().expect("an armed guard must announce itself");
        assert!(notice.contains("FATAL"), "{notice}");
        assert!(notice.contains("DO NOT REMOVE IT"), "the operator instruction: {notice}");
        assert!(
            notice.contains(&path.display().to_string()),
            "must name the device, or an operator cannot tell which: {notice}"
        );
        assert!(
            notice.contains(GuardPhase::Writing.interrupted_verb()),
            "must name what was interrupted: {notice}"
        );

        // The verb tracks the phase, so the notice is not a fixed string.
        armed.set_phase(ArmedPhase::Verifying);
        let later = armed.fatal_notice().expect("still armed");
        assert!(later.contains(GuardPhase::Verifying.interrupted_verb()), "{later}");
        assert_ne!(notice, later, "the verb must change with the phase");

        // And silence returns on disarm.
        let disarmed = armed.disarm();
        assert!(disarmed.fatal_notice().is_none(), "a disarmed guard must fall silent");
    }

    /// Both guards must hand out the descriptor they were built from.
    ///
    /// `as_raw_fd` is one line on each type and looked untestable without
    /// hardware: the way a wrong descriptor *manifests* is an ioctl
    /// failing, and every ioctl here needs a real device. But the way it
    /// is *wrong* is simpler than that — the function promises "this
    /// guard's descriptor", and that is checkable against the `File` the
    /// guard was constructed from, with no ioctl at all.
    ///
    /// Both mutants — `replace as_raw_fd with Default::default()`, which
    /// yields fd 0, stdin — survived until this existed. Phase 6 would
    /// have issued `BLKRRPART` against stdin.
    #[test]
    fn as_raw_fd_returns_the_guards_own_descriptor() {
        let path = TempPath::new("rawfd");
        let file = File::create(&*path).unwrap();
        let expected = file.as_raw_fd();
        assert!(expected > 2, "precondition: a real file, not a std stream: {expected}");

        let guard = FlashGuard::new(file, path.to_path_buf());
        assert_eq!(guard.as_raw_fd(), expected, "FlashGuard must hand out its own fd");

        // And the wrapper must delegate rather than invent one.
        let armed = ArmedForTest::new(guard.arm(ArmedPhase::Writing));
        assert_eq!(armed.as_raw_fd(), expected, "ArmedGuard must hand out the same fd");
    }

    #[test]
    fn guard_state_machine_tracks_every_transition() {
        let path = TempPath::new("sm");
        let file = File::create(&*path).unwrap();
        let guard = FlashGuard::new(file, path.to_path_buf());

        assert_eq!(guard.current_phase(), GuardPhase::Disarmed);
        assert!(!guard.would_warn_on_drop(), "a fresh guard must be silent on drop");

        let guard = guard.arm(ArmedPhase::WipingSignatures);
        assert_eq!(guard.current_phase(), GuardPhase::WipingSignatures);
        // Armed: holding an `ArmedGuard` is that assertion.

        for phase in [ArmedPhase::Writing, ArmedPhase::Cooldown, ArmedPhase::Verifying] {
            guard.set_phase(phase);
            assert_eq!(guard.current_phase(), GuardPhase::from(phase));
        }

        // Every armed phase maps to a distinct, non-Disarmed GuardPhase:
        // the conversion must not collapse two phases together, and must
        // never yield Disarmed — that is the whole point of the type.
        for phase in [
            ArmedPhase::WipingSignatures,
            ArmedPhase::Writing,
            ArmedPhase::Cooldown,
            ArmedPhase::Verifying,
        ] {
            assert_ne!(GuardPhase::from(phase), GuardPhase::Disarmed, "{phase:?}");
        }

        let guard = guard.disarm();
        assert_eq!(guard.current_phase(), GuardPhase::Disarmed);
        assert!(!guard.would_warn_on_drop(), "a disarmed guard must be silent again");
    }
}
