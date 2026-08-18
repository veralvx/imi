//! `O_DIRECT` toggling on the claimed device FD.
//!
//! Phase 4 enables `O_DIRECT` for the aligned write loop and clears it
//! for the unaligned tail; Phase 5b clears it once before reading back.
//! Both phases drive the same FD, so the toggle lives here rather than
//! in either phase.

use std::os::fd::AsFd;

use nix::fcntl::{FcntlArg, OFlag, fcntl};

use crate::Result;
use crate::error::Context as _;

/// Toggle `O_DIRECT` on an already-open FD via `fcntl(F_SETFL, …)`.
///
/// Reads current flags, flips the bit, writes them back. Only `O_DIRECT`
/// changes; `O_APPEND`, `O_NONBLOCK` and anything else are preserved —
/// `from_bits_retain` keeps bits `OFlag` does not name, which
/// `from_bits_truncate` would silently drop on the way back out.
///
/// Takes `impl AsFd` rather than a `RawFd`: the borrow proves the
/// descriptor is open for the duration of the call, so there is no
/// precondition left for a caller to violate and no `unsafe` here at
/// all. `nix` wraps both `fcntl` calls.
///
/// # Errors
///
/// Returns the `fcntl` failure with the operation named.
pub(crate) fn set_direct<F: AsFd>(fd: F, enable: bool) -> Result<()> {
    let bits = fcntl(&fd, FcntlArg::F_GETFL).context("fcntl(F_GETFL)")?;
    let flags = OFlag::from_bits_retain(bits);

    let wanted = if enable { flags | OFlag::O_DIRECT } else { flags & !OFlag::O_DIRECT };
    if wanted == flags {
        return Ok(());
    }

    fcntl(&fd, FcntlArg::F_SETFL(wanted))
        .with_context(|| format!("fcntl(F_SETFL, O_DIRECT={enable})"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::common::testing::TempPath;

    use super::set_direct;

    /// Setting and clearing must move exactly the `O_DIRECT` bit.
    ///
    /// The function had no test of its own — it was exercised only as a
    /// side effect of a Phase 4 test — so an inverted branch would have
    /// left the write loop running without `O_DIRECT`, which is slower
    /// and silently defeats the point of the aligned buffer rather than
    /// failing.
    #[test]
    #[cfg_attr(miri, ignore = "opens with O_DIRECT, an open flag Miri's shim does not support")]
    fn toggling_moves_only_the_o_direct_bit() {
        let path = TempPath::new("odirect");
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&*path)
            .expect("temp file");

        let get = || nix::fcntl::fcntl(&f, nix::fcntl::FcntlArg::F_GETFL).expect("F_GETFL");
        let before = get();
        assert_eq!(before & libc::O_DIRECT, 0, "precondition: not direct yet");

        set_direct(&f, true).expect("enable");
        let on = get();
        assert_ne!(on & libc::O_DIRECT, 0, "O_DIRECT must be set");
        assert_eq!(on & !libc::O_DIRECT, before & !libc::O_DIRECT, "no other flag may change");

        // Idempotent: the early return must not disturb anything.
        set_direct(&f, true).expect("enable again");
        assert_eq!(get(), on, "re-enabling must be a no-op");

        set_direct(&f, false).expect("disable");
        assert_eq!(get(), before, "clearing must restore the original flags exactly");

        drop(f);
    }

    // No test for the `fcntl` failure path.
    //
    // Reaching it needs an invalid descriptor, and the only way to hold
    // one is `BorrowedFd::borrow_raw`, whose contract is "The resource
    // pointed to by `fd` must remain open for the duration of the
    // returned `BorrowedFd`". Closing a file and borrowing its number
    // violates that by construction — and fd numbers are reused, so with
    // the harness running tests in parallel another thread could be
    // handed that number between the close and the call, at which point
    // this would set `O_DIRECT` on an unrelated test's file.
    //
    // The path itself is two lines of `.context()` over nix's error. Not
    // worth a deliberate contract violation and a cross-test race to
    // assert.
}
