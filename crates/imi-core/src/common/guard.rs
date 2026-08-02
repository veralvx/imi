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
//! Lifecycle:
//! 1. `FlashGuard::new(file, dev_path)` right after the `O_EXCL` open. Disarmed.
//! 2. `guard.arm(GuardPhase::WipingSignatures)` immediately before Phase 3.
//! 3. `guard.set_phase(GuardPhase::Writing)` at the start of Phase 4.
//! 4. `guard.set_phase(GuardPhase::Cooldown)` at the start of Phase 5a.
//! 5. `guard.set_phase(GuardPhase::Verifying)` at the start of Phase 5b.
//! 6. `guard.disarm()` after verification (Phase 5) passes.
//! 7. `guard.into_file()` when the caller wants the `File` back to drop it
//!    before Phase 7 (releasing the `O_EXCL` claim so udisks2 can see it).
//!
//! Note: Rust does not run `Drop` on `SIGINT` by default. The caller installs
//! a `ctrlc` handler elsewhere that sets a cancel flag; long-running loops
//! check the flag and return `Err`, which drives normal unwind and our `Drop`.

use std::fs::File;
use std::io::{self, Write as _};
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::Path;

use crate::Result;
use crate::error::bail;
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
    pub(crate) fn arm(&self, phase: ArmedPhase) {
        self.phase.store(GuardPhase::from(phase) as u8, Ordering::SeqCst);
    }

    /// Update which phase the guard is currently in. The guard must already
    /// be armed (i.e. you cannot use `set_phase` to arm an initially-disarmed
    /// guard — call `arm()` for that, to make the intent explicit at the
    /// arming point).
    pub(crate) fn set_phase(&self, phase: ArmedPhase) {
        // A `debug_assert`, and therefore absent from the builds a
        // consumer ships. That is tolerable only because arming a
        // disarmed guard fails *loud*: the guard starts warning on drop
        // rather than staying silent, so the worst outcome is a FATAL
        // notice over a device that was never written.
        //
        // Phase 4 additionally refuses a disarmed guard at runtime, so
        // its call is covered in release too. Phase 5's two calls are
        // not — nothing there checks the guard was armed, and a caller
        // driving the phases by hand could reach Phase 5 without Phases
        // 3 and 4. Verification then compares an unwritten device
        // against the image and fails, which catches it, but by
        // consequence rather than by design.
        debug_assert_ne!(
            self.phase.load(Ordering::SeqCst),
            GuardPhase::Disarmed as u8,
            "set_phase called on a disarmed guard"
        );
        self.phase.store(GuardPhase::from(phase) as u8, Ordering::SeqCst);
    }

    /// Disarm the guard. Subsequent drops are silent.
    pub(crate) fn disarm(&self) {
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
    /// Public so a caller driving [`crate::phases`] by hand can tell
    /// whether it is inside the destructive window — everything from
    /// Phase 3's arm to Phase 6's disarm — and report accordingly. It is
    /// also what makes [`GuardPhase`] a usable type rather than an
    /// exported name with no producer.
    #[must_use]
    pub fn current_phase(&self) -> GuardPhase {
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
    #[must_use]
    pub fn would_warn_on_drop(&self) -> bool {
        !matches!(self.current_phase(), GuardPhase::Disarmed)
    }

    /// Refuse to act if this guard is not holding `expected`.
    ///
    /// Phases 3, 4 and 5 each receive a [`FlashGuard`] and a `Target`
    /// separately, and nothing in the type system ties the two together.
    /// Inside `crate::run` they always agree, but the phases are public:
    /// a caller driving two devices concurrently could cross the values
    /// and write one image onto the other device — silently, since the
    /// guard would accept the writes. This turns that into a clean
    /// refusal before anything destructive happens.
    ///
    /// # Errors
    ///
    /// Returns an error when the paths differ.
    pub(crate) fn ensure_device_is(&self, expected: &Path) -> Result<()> {
        if self.dev_path != expected {
            bail!(
                "internal consistency check failed: the exclusive claim is held on {} \
                 but the phase was given a target describing {}. Refusing to act on a \
                 mismatched device.",
                self.dev_path.display(),
                expected.display()
            );
        }
        Ok(())
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
        self.disarm();
        self.file.take().expect("FlashGuard::into_file called twice")
    }
}

impl Drop for FlashGuard {
    fn drop(&mut self) {
        let phase = self.current_phase();
        if self.would_warn_on_drop() {
            // Written through `writeln!` rather than `eprintln!`, and
            // the result deliberately discarded.
            //
            // `eprintln!` panics if the write fails — a closed or full
            // stderr, an EPIPE from a dead `less`. This runs from `Drop`,
            // and the case it exists for is unwinding, where a second
            // panic aborts the process immediately. The abort would skip
            // this very notice and every remaining destructor, turning
            // "your device is half-written" into a bare `SIGABRT`.
            //
            // If stderr is gone the operator cannot be told regardless;
            // what matters is that the attempt cannot make things worse.
            let mut err = io::stderr().lock();
            let _notice = writeln!(
                err,
                "\n\u{26A0}  FATAL: flash interrupted while {} was {}. \
                 The device is in an inconsistent state. DO NOT REMOVE IT. \
                 Re-run imi to recover.",
                self.dev_path.display(),
                phase.interrupted_verb()
            );
            let _flushed = err.flush();
        }
        // `self.file` (if still `Some`) drops here, releasing the O_EXCL claim.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The guard must accept its own device and refuse any other. This
    /// is what stops a caller of the public per-phase API from pairing a
    /// claim on one device with a `Target` describing another.
    #[test]
    fn ensure_device_is_accepts_own_path_and_rejects_others() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("imi-guard-match-{}", std::process::id()));
        let file = File::create(&path).unwrap();
        let guard = FlashGuard::new(file, path.clone());

        guard.ensure_device_is(&path).expect("its own path must be accepted");

        let other = dir.join("imi-some-other-device");
        let err = guard.ensure_device_is(&other).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mismatched device"), "{msg}");
        assert!(msg.contains(&path.display().to_string()), "{msg}");

        guard.disarm();
        std::fs::remove_file(&path).unwrap();
    }

    /// The `arm`/`set_phase`/`disarm` state machine is what makes the FATAL
    /// notice correct. Each transition is asserted directly because the
    /// notice itself goes to stderr from `Drop`, which no in-process
    /// test can observe — without these, all three methods could be
    /// no-ops and the suite would not notice.
    #[test]
    fn guard_state_machine_tracks_every_transition() {
        let path = std::env::temp_dir().join(format!("imi-sm-{}", std::process::id()));
        let file = File::create(&path).unwrap();
        let guard = FlashGuard::new(file, path.clone());

        assert_eq!(guard.current_phase(), GuardPhase::Disarmed);
        assert!(!guard.would_warn_on_drop(), "a fresh guard must be silent on drop");

        guard.arm(ArmedPhase::WipingSignatures);
        assert_eq!(guard.current_phase(), GuardPhase::WipingSignatures);
        assert!(guard.would_warn_on_drop(), "an armed guard must warn on drop");

        for phase in [ArmedPhase::Writing, ArmedPhase::Cooldown, ArmedPhase::Verifying] {
            guard.set_phase(phase);
            assert_eq!(guard.current_phase(), GuardPhase::from(phase));
            assert!(guard.would_warn_on_drop(), "{phase:?} must still warn on drop");
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

        guard.disarm();
        assert_eq!(guard.current_phase(), GuardPhase::Disarmed);
        assert!(!guard.would_warn_on_drop(), "a disarmed guard must be silent again");

        std::fs::remove_file(&path).unwrap();
    }
}
