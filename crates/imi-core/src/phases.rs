//! The pipeline's phases, one module per phase.
//!
//! Each module exposes a `run` that executes that phase end to end, plus
//! the helpers only that phase uses. Anything two or more phases need
//! lives in the crate-internal `common` module instead.
//!
//! [`crate::run_with`] is what calls the eight in order; [`crate::run`]
//! and [`crate::run_with_cancel`] are thin wrappers that supply a silent
//! sink and a throwaway cancellation flag.
//!
//! # Driving the phases yourself
//!
//! All eight `run` functions are public, so a caller that wants to stop
//! between phases or substitute its own step can call them directly.
//! Not every phase returns something, and not every value goes
//! everywhere. Phase 0 yields the [`Target`], which every later phase
//! takes except Phase 6 — that one needs only the guard, because
//! releasing the claim is the whole of its job. Phase 1 yields the devt
//! set Phase 2 needs; Phase 2 the [`FlashGuard`] that Phases 3 to 5
//! borrow and Phase 6 consumes; Phase 4 the [`FlashOutcome`] Phase 5
//! verifies against.
//!
//! Phases 3, 5 and 7 return `Result<()>` — nothing to thread onward, but
//! still a failure to handle. Phase 6 returns no type at all, and is the
//! only phase that cannot fail: releasing the claim is a drop, and a
//! `BLKRRPART` that does not take is a warning rather than an error. A
//! caller sequencing by hand writes `?` on the first three and not on
//! Phase 6.
//!
//! One step is easy to miss because it is not a phase.
//! [`crate::run_with`] ends with `events.finished(&target.dev_canon)`,
//! which is what emits the "you can now safely remove" line. A caller
//! sequencing the phases by hand gets no such line unless it makes that
//! call itself — and on this tool, the absence of a success message is
//! not a cosmetic difference: it is the only positive signal that the
//! device is safe to unplug.
//!
//! [`Target`]: crate::Target
//! [`FlashGuard`]: crate::FlashGuard
//! [`FlashOutcome`]: crate::FlashOutcome

pub mod phase_0;
pub mod phase_1;
pub mod phase_2;
pub mod phase_3;
pub mod phase_4;
pub mod phase_5;
pub mod phase_6;
pub mod phase_7;
