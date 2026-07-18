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
pub(crate) enum GuardPhase {
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
pub(crate) struct FlashGuard {
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

impl FlashGuard {
    /// Construct a new guard around a freshly-opened, `O_EXCL`-claimed device.
    /// The guard starts disarmed; no destructive operation has happened yet.
    pub(crate) fn new(file: File, dev_path: PathBuf) -> Self {
        Self { file: Some(file), dev_path, phase: AtomicU8::new(GuardPhase::Disarmed as u8) }
    }

    /// Arm the guard for the given phase. From this point, any early drop
    /// prints the warning with a phase-appropriate verb.
    pub(crate) fn arm(&self, phase: GuardPhase) {
        debug_assert_ne!(phase, GuardPhase::Disarmed, "use disarm() to disarm");
        self.phase.store(phase as u8, Ordering::SeqCst);
    }

    /// Update which phase the guard is currently in. The guard must already
    /// be armed (i.e. you cannot use `set_phase` to arm an initially-disarmed
    /// guard — call `arm()` for that, to make the intent explicit at the
    /// arming point).
    pub(crate) fn set_phase(&self, phase: GuardPhase) {
        debug_assert_ne!(phase, GuardPhase::Disarmed, "use disarm() to disarm");
        debug_assert_ne!(
            self.phase.load(Ordering::SeqCst),
            GuardPhase::Disarmed as u8,
            "set_phase called on a disarmed guard"
        );
        self.phase.store(phase as u8, Ordering::SeqCst);
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

    /// Raw fd of the held device.
    pub(crate) fn as_raw_fd(&self) -> RawFd {
        self.file().as_raw_fd()
    }

    /// Consume the guard and return the inner `File`. The caller is then
    /// responsible for dropping it (which releases the `O_EXCL` claim).
    ///
    /// Disarms automatically: by the time the caller wants the `File` back,
    /// the destructive region is over.
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
        let phase = GuardPhase::from_u8(self.phase.load(Ordering::SeqCst));
        if !matches!(phase, GuardPhase::Disarmed) {
            // `eprintln!` may allocate, but Rust's panic machinery keeps
            // stderr functional even during OOM situations, and a failed
            // allocation just means the user sees a shorter line — still
            // better than silent failure.
            eprintln!(
                "\n\u{26A0}  FATAL: flash interrupted while {} was {}. \
                 The device is in an inconsistent state. DO NOT REMOVE IT. \
                 Re-run imi to recover.",
                self.dev_path.display(),
                phase.interrupted_verb()
            );
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
}
