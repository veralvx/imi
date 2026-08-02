//! SIGINT/SIGTERM handling for the whole pipeline.
//!
//! The handler only flips a shared flag; it never exits. Every phase's
//! loops poll that flag and return `Err`, so the process leaves through
//! a normal unwind and `FlashGuard::drop` still prints its FATAL notice.
//! Both signals are handled — `ctrlc` needs its `termination` feature
//! for SIGTERM, which the workspace manifest enables.
//!
//! # Why a second Ctrl+C does nothing
//!
//! There is no escalation to `exit()`, deliberately, and the cost is
//! worth stating. Cancellation is only noticed where a loop polls, so
//! anything blocking between polls — a 4 MiB `pwrite` to slow media, or
//! an `fdatasync` that a cheap controller takes seconds to acknowledge —
//! leaves the flag set and the process apparently unresponsive.
//!
//! Escalating would end the process without unwinding, which is exactly
//! the outcome `FlashGuard` exists to prevent: no FATAL notice, and an
//! operator with no way to know the device is half-written. Waiting is
//! the correct behaviour, and an operator who reaches for the USB stick
//! instead is the failure this trade avoids.
//!
//! `SIGKILL` remains available to anyone who genuinely needs the process
//! gone, and skips the notice for the same structural reason.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};

/// Install the SIGINT/SIGTERM handler that flips the shared cancel flag.
pub(crate) fn install_signal_handler(cancel: Arc<AtomicBool>) -> Result<()> {
    ctrlc::set_handler(move || {
        cancel.store(true, Ordering::SeqCst);
        // Deliberately do NOT exit() from the handler: setting the flag
        // lets the main thread's chunk loops return Err, driving normal
        // unwind so FlashGuard::drop runs.
    })
    .context("installing SIGINT/SIGTERM handler")?;
    Ok(())
}
