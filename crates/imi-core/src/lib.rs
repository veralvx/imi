//! `imi-core` — the safety pipeline behind the [`imi`] flasher.
//!
//! The crate writes a disk image to a Linux block device through a fixed
//! sequence of phases. Each phase establishes a specific invariant for
//! the next, and a [`FlashGuard`] tracks how far the run got so an
//! interrupted flash can tell the operator what state the device is in.
//!
//! | Phase | Purpose                                             |
//! |-------|-----------------------------------------------------|
//! | 0     | Pre-flight validation                               |
//! | 1     | Topology audit + swap/unmount (before `O_EXCL`)     |
//! | 2     | `O_EXCL` claim + TOCTOU re-check                    |
//! | 3     | Signature wipe (head + tail); guard armed here      |
//! | 4     | Flash write                                         |
//! | 5a    | Hardware cooldown (skippable)                       |
//! | 5b    | Byte-for-byte verify (skippable)                    |
//! | 6     | `BLKRRPART` under lock, then drop FD                |
//! | 7     | Automount defense after lock release                |
//!
//! # Two ways to drive it
//!
//! [`run`] is the whole pipeline in one call:
//!
//! ```no_run
//! # use std::path::PathBuf;
//! let mut config = imi_core::Config::new(PathBuf::from("a.iso"), PathBuf::from("/dev/sdc"));
//! config.yes = true;
//! imi_core::run(&config).unwrap();
//! ```
//!
//! [`run_with_cancel`] is the same pipeline with a caller-owned
//! cancellation flag, polled by every waiting or looping phase. That is
//! what the `imi` binary calls, because it owns the signal handler; the
//! library never installs one. Reach for [`run`] when there is nothing
//! to cancel from.
//!
//! [`run_with`] adds an [`Events`] sink. The pipeline writes nothing to
//! a terminal — progress, phase changes, warnings and the
//! destructive-action confirmation all arrive through it, so a graphical
//! or remote caller renders them however it likes:
//!
//! ```no_run
//! # use std::path::PathBuf;
//! # use std::sync::atomic::AtomicBool;
//! use imi_core::{Events, Phase};
//!
//! struct Ui;
//! impl Events for Ui {
//!     fn progress(&mut self, _phase: Phase, done: u64, total: Option<u64>) {
//!         let _ = (done, total);
//!     }
//!     fn confirm(&mut self, _s: &imi_core::Summary<'_>) -> bool {
//!         true // ask the user; the default is to refuse
//!     }
//! }
//!
//! # let config = imi_core::Config::new(PathBuf::from("a.iso"), PathBuf::from("/dev/sdc"));
//! imi_core::run_with(&config, &AtomicBool::new(false), &mut Ui).unwrap();
//! ```
//!
//! The sink is taken by generic with a `?Sized` bound, so a concrete
//! type dispatches statically while a `Box<dyn Events>` — what a GUI
//! holds in its application state — still works via `&mut *boxed`.
//! [`Events::confirm`] defaults to **refusing**, so an unattended caller
//! sets [`Config::yes`].
//!
//! Each phase is also public in [`phases`], so a caller can drive the
//! sequence itself — pausing between phases, reporting progress, or
//! substituting a step. [`run_with`] is the one to read while doing so:
//! [`run`] and [`run_with_cancel`] are two-line wrappers that supply a
//! silent sink and a throwaway flag before delegating to it.
//!
//! The order is load-bearing: phase *N* assumes phase *N-1* succeeded,
//! and the handoffs enforce it. Phase 0 produces a [`Target`]; Phase 1
//! the devt set Phase 2 needs; Phase 2 consumes the `Target` and returns
//! a [`Session`] — the guard and that target, paired for good; Phase 3
//! consumes the `Session` and returns an [`ArmedSession`]; Phases 4 and
//! 5 borrow it, Phase 4 additionally producing the [`FlashOutcome`]
//! Phase 5 verifies against; Phase 6 consumes the `ArmedSession` and
//! hands the `Target` back for Phase 7. Phases 5 and 7 return
//! `Result<()>`; Phase 6 is the only one that cannot fail.
//!
//! One step is not a phase and is easy to miss.  [`run_with`] ends with
//! `events.finished(&target.dev_canon)`, which is what emits the "you
//! can now safely remove" line. Drive the phases by hand and nothing
//! emits it unless you do — and that line is the only positive signal
//! this crate gives that a device is safe to unplug, so its absence
//! looks exactly like a run that stopped without saying so.
//!
//! # Failures
//!
//! Everything fallible returns [`Result<T>`](Result), whose error is
//! [`Error`]. It implements [`std::error::Error`], so it can be a
//! `#[source]` in a caller's own error enum, boxed as `dyn Error`, or
//! converted with `crate::error::Error::new`. `Display` shows the phase
//! context and `{:#}` prints the full chain.
//!
//! Before showing a failure to anyone, check [`Error::device_state`]:
//!
//! ```no_run
//! # use std::path::PathBuf;
//! use imi_core::DeviceState;
//!
//! # let config = imi_core::Config::new(PathBuf::from("a.iso"), PathBuf::from("/dev/sdc"));
//! if let Err(e) = imi_core::run(&config) {
//!     match e.device_state() {
//!         DeviceState::Untouched => println!("{e:#} — the device is unchanged"),
//!         DeviceState::Indeterminate => println!("{e:#} — DO NOT REMOVE; re-flash required"),
//!         DeviceState::Written => println!("{e:#} — the image is written; unmount first"),
//!         _ => println!("{e:#}"),
//!     }
//! }
//! ```
//!
//! [`Error::kind`] answers the separate question of *how* to report it —
//! a [`ErrorKind::Cancelled`] run is not an error to show in red. The
//! two are independent: a cancellation can leave the device either
//! untouched or indeterminate depending on when it landed.
//!
//! # Values the phases hand you
//!
//! [`Config`] is yours to build: [`Config::new`] takes the two paths and
//! the optional fields are assigned afterwards.
//!
//! [`Target`] and [`FlashOutcome`] are not. Their fields are private and
//! read through accessors — [`Target::device_size`],
//! [`FlashOutcome::bytes_written`] and friends — so the only way to
//! obtain one is from the phase that produces it.
//!
//! That is a safety property, not style. `Target::device_size` is the
//! bound every Phase 4 write is checked against, and
//! `FlashOutcome::bytes_written` decides how much of the device Phase 5b
//! reads back; a smaller figure verifies part of it and still reports
//! success. Both must therefore mean what Phase 0 and Phase 4 measured.
//!
//! `#[non_exhaustive]` alone would not achieve this. It prevents a
//! caller *constructing* the struct, but says nothing about *mutating*
//! one it already owns — a consumer could take the value `phase_0::run`
//! returned and rewrite the size before Phase 4 checked against it.
//! Private fields are what close that; the attribute remains for its own
//! purpose, which is letting these types gain fields without a breaking
//! release.
//!
//! For the same reason the guard and the `Target` travel as one value
//! from Phase 2 onward. Two legitimately obtained values from different
//! devices used to be crossable by a caller driving several at once,
//! and a runtime check in each phase refused the mismatch; a [`Session`]
//! has no second `Target` parameter to cross, so the mistake now fails
//! to compile instead.
//!
//! # Choosing a device
//!
//! [`candidate_devices`] enumerates the disks an interactive consumer —
//! the `imi` binary's picker, or a GUI — may safely *offer* as flash
//! targets, as a list of [`CandidateDevice`] values (path, size, model,
//! removable). The filter reuses the pipeline's own rules, so a listed
//! disk is one the pipeline could actually proceed against; the system
//! disk never appears. It is an offering, not a decision: nothing in
//! this crate selects a device, and every candidate still passes
//! through the full Phase 0–2 gauntlet after a human picks it.
//!
//! # Privileges and side effects
//!
//! Every phase from 1 onward requires `root`. Phases 3 onward are
//! **destructive**.
//!
//! The library writes to a terminal in exactly one place, and it is not
//! a phase: [`FlashGuard`]'s `Drop` prints the FATAL notice to stderr
//! when an interrupted run leaves the device mid-flash. Everything else
//! goes through [`Events`], including progress, warnings and the
//! confirmation. The exception exists because a `Drop` running during
//! unwind has no sink to reach — the sink belongs to the caller, whose
//! stack is being torn down — and staying silent there would mean an
//! operator unplugging a half-written device with no warning at all.
//!
//! [`imi`]: https://github.com/veralvx/imi

#![cfg(target_os = "linux")]

// `common` and `config` are private: every type a caller needs is
// re-exported below, so there is exactly one path to each. Keeping the
// modules private also means the internal layout — which module a type
// lives in — is not part of the public API and can be refactored
// without a breaking release. `phases` is public because the per-phase
// entry points *are* the API.
mod common;
mod config;
mod error;
mod events;

pub mod phases;

use std::sync::atomic::AtomicBool;

pub use crate::common::context::{FlashOutcome, Target};
pub use crate::common::devices::{CandidateDevice, candidate_devices};
pub use crate::common::guard::{ArmedGuard, FlashGuard, GuardPhase};
pub use crate::common::identity::DeviceIdentity;
pub use crate::common::image::Compression;
pub use crate::common::mount::TargetDevts;
pub use crate::common::session::{ArmedSession, Session};
pub use crate::config::Config;
pub use crate::error::{DeviceState, Error, ErrorKind};
pub use crate::events::{Events, Phase, PhaseOutcome, Summary};

/// The result of any fallible operation in this crate.
///
/// Aliased so callers write `imi_core::Result<T>` rather than repeating
/// the error type; [`Error`] implements [`std::error::Error`], so it
/// also slots directly into a caller's own error enum.
pub type Result<T> = std::result::Result<T, Error>;

/// Run the full pipeline against the image and device named in `config`.
///
/// The run cannot be cancelled cooperatively; see [`run_with_cancel`]
/// to supply a flag.
///
/// That distinction matters for more than convenience. With no flag,
/// a `SIGINT` reaching a process that has not installed a handler is
/// fatal by default: the process dies without unwinding, so
/// [`FlashGuard`]'s `Drop` never runs and the operator is never told the
/// device is half-written. If the caller's process can receive signals
/// during a flash, install a handler and use [`run_with_cancel`] — that
/// is exactly why the `imi` binary does.
///
/// # Errors
///
/// Returns the first phase failure, wrapped with a `Phase N: ...`
/// context string. A failure before Phase 3 leaves the device untouched;
/// from Phase 3 onward the armed [`FlashGuard`] additionally prints a
/// FATAL notice naming the interrupted phase.
///
/// # Panics
///
/// Propagates a panic from the decompression worker thread, re-raised on
/// the calling thread by `resume_unwind` after the channels are closed
/// and the worker joined. Phases 4 and 5b spawn that worker for
/// compressed images; no other path here panics — the crate's few
/// production `expect`s all take a value out of an `Option` that only a
/// consuming method empties, so each is unreachable by construction.
///
/// The guard's FATAL notice is printed before the panic escapes, so an
/// operator is told the device state even on this path.
/// # Examples
///
/// ```no_run
/// use std::path::PathBuf;
///
/// let mut config = imi_core::Config::new(
///     PathBuf::from("ubuntu.iso"),
///     PathBuf::from("/dev/sdc"),
/// );
/// // `run` uses a silent sink, whose `confirm` refuses by default, so an
/// // unattended caller must say so explicitly.
/// config.yes = true;
/// imi_core::run(&config)?;
/// # Ok::<(), imi_core::Error>(())
/// ```
pub fn run(config: &Config) -> Result<()> {
    run_with_cancel(config, &AtomicBool::new(false))
}

/// Run the full pipeline, honouring a caller-owned cancellation flag.
///
/// `cancel` is polled by every waiting or looping phase; set it from a
/// signal handler to abort at the next check. Cancellation inside the
/// destructive window still leaves the guard's FATAL notice on stderr.
///
/// Use [`run`] when there is nothing to cancel from.
///
/// # Errors
///
/// Returns the first phase failure, wrapped with a `Phase N: ...`
/// context string. A failure before Phase 3 leaves the device untouched;
/// from Phase 3 onward the armed [`FlashGuard`] additionally prints a
/// FATAL notice naming the interrupted phase.
///
/// # Panics
///
/// Propagates a panic from the decompression worker thread, re-raised on
/// the calling thread by `resume_unwind` after the channels are closed
/// and the worker joined. Phases 4 and 5b spawn that worker for
/// compressed images; no other path here panics — the crate's few
/// production `expect`s all take a value out of an `Option` that only a
/// consuming method empties, so each is unreachable by construction.
///
/// The guard's FATAL notice is printed before the panic escapes, so an
/// operator is told the device state even on this path.
/// # Examples
///
/// ```no_run
/// use std::path::PathBuf;
/// use std::sync::Arc;
/// use std::sync::atomic::{AtomicBool, Ordering};
/// let cancel = Arc::new(AtomicBool::new(false));
/// let flag = Arc::clone(&cancel);
/// // Set the flag from a signal handler, another thread, or a UI button.
/// std::thread::spawn(move || flag.store(true, Ordering::SeqCst));
/// let mut config = imi_core::Config::new(
///     PathBuf::from("ubuntu.iso"),
///     PathBuf::from("/dev/sdc"),
/// );
/// config.yes = true;
/// imi_core::run_with_cancel(&config, &cancel)?;
/// # Ok::<(), imi_core::Error>(())
/// ```
pub fn run_with_cancel(config: &Config, cancel: &AtomicBool) -> Result<()> {
    run_with(config, cancel, &mut ())
}

/// Run the full pipeline, reporting to `events`.
///
/// This is the complete form: [`run`] and [`run_with_cancel`] are it
/// with a silent sink and, for `run`, a flag nothing ever sets.
///
/// The pipeline writes nothing to a terminal. Progress, phase changes,
/// warnings and the destructive-action confirmation all arrive through
/// `events`, so a graphical or remote caller can render them however it
/// likes. Note that [`Events::confirm`] defaults to **refusing**: an
/// unattended caller wanting no prompt sets [`Config::yes`].
///
/// # Examples
///
/// ```no_run
/// use std::path::PathBuf;
/// use std::sync::atomic::AtomicBool;
/// struct Ui;
/// impl imi_core::Events for Ui {
///     fn progress(&mut self, _phase: imi_core::Phase, done: u64, total: Option<u64>) {
///         match total {
///             Some(t) => println!("{done}/{t}"),
///             None => println!("{done} bytes"),
///         }
///     }
///     fn confirm(&mut self, s: &imi_core::Summary<'_>) -> bool {
///         println!("about to erase {}", s.device.display());
///         true // ask a human here; the default is to refuse
///     }
/// }
/// let config = imi_core::Config::new(
///     PathBuf::from("ubuntu.iso"),
///     PathBuf::from("/dev/sdc"),
/// );
/// imi_core::run_with(&config, &AtomicBool::new(false), &mut Ui)?;
/// # Ok::<(), imi_core::Error>(())
/// ```
///
/// # Errors
///
/// Returns the first phase failure. Check [`Error::device_state`] before
/// reporting it — a [`DeviceState::Indeterminate`] device is holding a
/// partial image and must be re-flashed.
///
/// # Panics
///
/// Propagates a panic from the decompression worker thread, re-raised on
/// the calling thread by `resume_unwind` after the channels are closed
/// and the worker joined. Phases 4 and 5b spawn that worker for
/// compressed images; no other path here panics — the crate's few
/// production `expect`s all take a value out of an `Option` that only a
/// consuming method empties, so each is unreachable by construction.
///
/// The guard's FATAL notice is printed before the panic escapes, so an
/// operator is told the device state even on this path.
pub fn run_with<E: Events + ?Sized>(
    config: &Config,
    cancel: &AtomicBool,
    events: &mut E,
) -> Result<()> {
    let target = phases::phase_0::run(config, events)?;
    let devts = phases::phase_1::run(&target, events)?;
    let session = phases::phase_2::run(target, &devts, events)?;
    let mut session = phases::phase_3::run(session, cancel, events)?;
    let outcome = phases::phase_4::run(&mut session, config.throttle, cancel, events)?;
    phases::phase_5::run(&mut session, config, outcome, cancel, events)?;
    // Phase 6 hands the target back: it outlives the session, and Phase 7
    // needs it. Named distinctly from the pre-Phase-2 binding, which was
    // moved into the session and is gone.
    let flashed = phases::phase_6::run(session, events);
    phases::phase_7::run(&flashed, cancel, events)?;

    events.finished(&flashed.dev_canon);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    /// Pins the crate's public surface.
    ///
    /// Every re-exported type and every phase entry point is named with
    /// its exact signature, so dropping a `pub use`, renaming a phase, or
    /// changing what a phase takes or returns fails to compile here
    /// rather than silently breaking downstream callers. The function
    /// pointers are never invoked — the phases need root and a real
    /// device — so this is a compile-time contract check that happens to
    /// run.
    #[test]
    fn public_api_surface_is_pinned() {
        /// Phases 4 and 5 take five and six parameters; naming the
        /// shapes keeps the pins readable.
        type Phase4 = fn(
            &mut crate::ArmedSession,
            Option<u64>,
            &AtomicBool,
            &mut (),
        ) -> crate::Result<crate::FlashOutcome>;
        type Phase5 = fn(
            &mut crate::ArmedSession,
            &crate::Config,
            crate::FlashOutcome,
            &AtomicBool,
            &mut (),
        ) -> crate::Result<()>;

        let _: fn(&crate::Config) -> crate::Result<()> = crate::run;
        let _: fn(&crate::Config, &AtomicBool) -> crate::Result<()> = crate::run_with_cancel;
        let _: fn(&crate::Config, &AtomicBool, &mut ()) -> crate::Result<()> =
            crate::run_with::<()>;

        let _: fn(&crate::Config, &mut ()) -> crate::Result<crate::Target> =
            crate::phases::phase_0::run::<()>;
        let _: fn(&crate::Target, &mut ()) -> crate::Result<crate::TargetDevts> =
            crate::phases::phase_1::run::<()>;
        let _: fn(crate::Target, &crate::TargetDevts, &mut ()) -> crate::Result<crate::Session> =
            crate::phases::phase_2::run::<()>;
        // Phase 3 is the arming boundary: it consumes the disarmed session
        // and yields the armed one. Phases 4 and 5 take `&mut ArmedSession`,
        // so skipping it does not compile — and neither type has a second
        // slot a mismatched target could occupy.
        let _: fn(crate::Session, &AtomicBool, &mut ()) -> crate::Result<crate::ArmedSession> =
            crate::phases::phase_3::run::<()>;
        let _: Phase4 = crate::phases::phase_4::run::<()>;
        let _: Phase5 = crate::phases::phase_5::run::<()>;
        let _: fn(crate::ArmedSession, &mut ()) -> crate::Target =
            crate::phases::phase_6::run::<()>;
        let _: fn(&crate::Target, &AtomicBool, &mut ()) -> crate::Result<()> =
            crate::phases::phase_7::run::<()>;
    }
}
