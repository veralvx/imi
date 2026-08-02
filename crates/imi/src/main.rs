//! `imi` — binary entry point.
//!
//! Everything of substance lives in `imi-core`; this crate owns the
//! command-line surface, the signal handler, and the exit code.
//!
//! # Exit codes
//!
//! | code | meaning |
//! | --- | --- |
//! | 0 | the device was written and verified |
//! | 1 | the run failed or was cancelled; `error:` on stderr says why |
//! | 2 | the arguments were wrong; `clap` printed usage |
//!
//! A script should treat 2 as its own bug and 1 as the device's
//! business — and on 1 it must not assume the device is untouched.
//! Whether it is appears in the message, and the `FATAL` notice on
//! stderr is the machine-visible marker for "written, state unknown".
//!
//! # Why `exit()` is safe here
//!
//! `std::process::exit` does not unwind, so anything still alive with a
//! `Drop` is skipped — which would be a serious bug in this particular
//! program, since [`imi_core::FlashGuard`]'s `Drop` is what warns an
//! operator that a device is half-written.
//!
//! It is safe because the guard is owned inside `imi_core::run_with` and
//! dropped before that call returns. Verified rather than assumed: on an
//! interrupted flash the FATAL notice appears on stderr *above* the
//! `error:` line printed below, so the destructor has already run by the
//! time this function chooses an exit code.
//!
//! Nothing else alive at that point has an observable `Drop`: the parsed
//! arguments are plain data, and the cancel flag is an `Arc` whose only
//! effect is freeing memory the kernel reclaims anyway.

#![cfg(target_os = "linux")]

mod cli;
mod progress;
mod signals;
mod ui;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use clap::Parser as _;

/// Parse the command line, run the pipeline, and exit with its status.
fn main() {
    let args = cli::Cli::parse();

    // The cancel flag is owned here rather than in the library: taking
    // over process-wide signal disposition is the binary's business, and
    // a library embedding `imi-core` may already have its own handler.
    let cancel = Arc::new(AtomicBool::new(false));

    let outcome = signals::install_signal_handler(Arc::clone(&cancel))
        // `imi_core::Error` converts into `anyhow::Error` because it
        // implements `std::error::Error + Send + Sync + 'static`. That
        // interop is the reason the library returns its own type rather
        // than `anyhow::Error`, which cannot implement `Error` at all.
        .and_then(|()| {
            imi_core::run_with(&args.to_config(), &cancel, &mut ui::Cli::new(args.dev.clone()))
                .map_err(anyhow::Error::new)
        });

    let code = match outcome {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    };
    std::process::exit(code);
}
