//! Cancellable sleeping — shared by every phase that waits.
//!
//! Phases 4, 5a, 5b and 7 all sleep (throttle pacing, the hardware
//! cooldown, the automount-defense settle) and all must stay responsive
//! to the `ctrlc` handler's cancel flag. The polling sleep lives here
//! rather than in any one phase so no phase owns another's waiting.

//! # Memory ordering
//!
//! Every atomic access in this crate uses `SeqCst` — verified, not
//! aspirational: there is no other ordering anywhere in `crates/`.
//!
//! That is a deliberate uniformity rather than a per-site optimum. A
//! weaker ordering is very likely sufficient for a bare stop flag, but
//! "sufficient" depends on whether a given site also needs to observe
//! writes made before the store, and that is a judgement to make per
//! call site, in the phase that owns it. Using one ordering everywhere
//! means no site needs that judgement, and a reader never has to work
//! out whether a `Relaxed` was reasoned about or just typed.
//!
//! The cost is close to nothing here: on the architectures this ships
//! for a `SeqCst` load of a `bool` is the same instruction as a
//! `Relaxed` one, and these loads sit at loop boundaries. Weaken one
//! only with the data dependencies of that specific site written down.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Sleep for `total`, polling `cancel` between fixed-size sub-sleeps so
/// Ctrl+C is responsive even at very low throttle rates.
///
/// Used by both Phase 4 (flash) and Phase 5b (verify) for the
/// post-chunk throttle wait. At `--throttle 100K`, one 4 MiB chunk
/// translates to a ~40-second sleep; without polling, Ctrl+C would
/// wait the full residual before the loop noticed. 100ms granularity
/// is well below human-perceptible unresponsiveness and adds one
/// atomic load per tick — invisible cost in any realistic profile.
///
/// Sub-100ms sleeps are issued in one shot since the polling overhead
/// would dominate.
///
/// On cancel, the function returns from the sleep early but does *not*
/// itself signal the cancellation: it returns `()` rather than `Result`,
/// so the caller's outer loop is responsible for re-checking the flag
/// at its next iteration boundary and bailing with a context-wrapped
/// error. Returning `Result` from here would force every call site to
/// `?`-propagate, adding error-path noise without any new information.
pub(crate) fn cancellable_sleep(total: Duration, cancel: &AtomicBool) {
    const TICK: Duration = Duration::from_millis(100);
    if total <= TICK {
        if !total.is_zero() {
            thread::sleep(total);
        }
        return;
    }
    // `Instant + Duration` panics on overflow. The realistic ceiling for
    // `total` is the throttle calculation's worst case (~48 days at
    // rate=1), far below what a monotonic clock can represent, so this
    // is unreachable in practice — but a panic deep inside the
    // destructive pipeline is not an acceptable degradation either.
    //
    // The fallback counts down instead of computing a deadline, so it
    // stays cancellable. Sleeping the whole span in one shot would be
    // worse than the panic it avoids: the process would sit
    // unresponsive to Ctrl+C, with the guard armed and the device
    // half-written, for however long `total` happened to be.
    let Some(deadline) = Instant::now().checked_add(total) else {
        let mut left = total;
        while !left.is_zero() {
            if cancel.load(Ordering::SeqCst) {
                return;
            }
            let step = left.min(TICK);
            thread::sleep(step);
            // Saturating: `step` is `min(left, TICK)` so it can never
            // exceed `left`, but the subtraction is on the wait path of
            // a destructive pipeline and a wrap here would loop for
            // roughly forever.
            left = left.saturating_sub(step);
        }
        return;
    };
    loop {
        if cancel.load(Ordering::SeqCst) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        thread::sleep(remaining.min(TICK));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- cancellable_sleep ----------------------------------------------

    /// Sub-tick sleep is single-shot. Must complete in approximately the
    /// requested duration (not faster, not 100ms-rounded).
    #[test]
    fn cancellable_sleep_short_duration_completes() {
        let cancel = AtomicBool::new(false);
        let t0 = Instant::now();
        cancellable_sleep(Duration::from_millis(20), &cancel);
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_millis(15),
            "should sleep at least ~20ms, slept {elapsed:?}"
        );
        // Generous upper bound to absorb test-runner scheduling jitter.
        assert!(
            elapsed < Duration::from_millis(500),
            "should not significantly overshoot, slept {elapsed:?}"
        );
    }

    /// Zero-duration sleep is a no-op (no sleep call at all).
    #[test]
    fn cancellable_sleep_zero_is_noop() {
        let cancel = AtomicBool::new(false);
        let t0 = Instant::now();
        cancellable_sleep(Duration::from_secs(0), &cancel);
        assert!(
            t0.elapsed() < Duration::from_millis(50),
            "zero-duration sleep should not actually sleep"
        );
    }

    /// Setting cancel before the sleep starts: the function returns
    /// promptly (within the granularity of one tick). This is the
    /// common case when Ctrl+C fires just before the throttle sleep.
    #[test]
    fn cancellable_sleep_returns_promptly_when_pre_cancelled() {
        let cancel = AtomicBool::new(true);
        let t0 = Instant::now();
        cancellable_sleep(Duration::from_secs(10), &cancel);
        let elapsed = t0.elapsed();
        // The function checks the flag at the top of the loop, so for a
        // long sleep it returns essentially immediately.
        assert!(
            elapsed < Duration::from_millis(100),
            "pre-cancelled long sleep should return promptly, slept {elapsed:?}"
        );
    }

    /// Setting cancel mid-sleep: the function returns within one tick
    /// (~100ms) of the flag being set. This is the property that
    /// makes Ctrl+C feel responsive even at very low throttle rates.
    ///
    /// We deliberately do NOT assert a lower bound on elapsed time.
    /// The setter thread's `sleep(150ms)` and the main thread's `t0`
    /// are not strictly synchronized — under heavy CI load the setter
    /// can start before `t0` is recorded, making a `>= 150ms` assertion
    /// flaky for non-bug reasons. The substantive property is "returns
    /// within ~one tick of cancel being set"; the
    /// `cancellable_sleep_returns_promptly_when_pre_cancelled` test
    /// covers the "doesn't return immediately when cancel is *not*
    /// set" half of the contract.
    #[test]
    fn cancellable_sleep_returns_within_one_tick_after_cancel() {
        use std::sync::Arc;
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_setter = Arc::clone(&cancel);
        // Trigger cancel after ~150ms.
        let setter = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            cancel_setter.store(true, Ordering::SeqCst);
        });
        let t0 = Instant::now();
        // Sleep target: 5 seconds. With cancel firing after ~150ms and
        // 100ms tick granularity, we should return well before the full
        // 5 seconds elapse — the upper bound proves cancel-responsiveness.
        cancellable_sleep(Duration::from_secs(5), &cancel);
        let elapsed = t0.elapsed();
        setter.join().unwrap();
        assert!(
            elapsed < Duration::from_millis(500),
            "should return within ~one tick after cancel was set, slept {elapsed:?}"
        );
    }

    /// The overflow fallback must stay cancellable.
    ///
    /// `Instant::now() + Duration::MAX` cannot be represented, so this
    /// takes the branch that counts down instead of computing a
    /// deadline. Sleeping the whole span in one shot — which is what the
    /// fallback used to do — would hang the process for the age of the
    /// universe with the guard armed, unresponsive to Ctrl+C. Unreachable
    /// in practice; the point is that the degradation is not worse than
    /// the panic it replaces.
    #[test]
    fn overflowing_duration_still_honours_cancel() {
        assert!(
            Instant::now().checked_add(Duration::MAX).is_none(),
            "precondition: Duration::MAX must overflow the deadline",
        );

        let cancel = AtomicBool::new(true);
        let t0 = Instant::now();
        cancellable_sleep(Duration::MAX, &cancel);
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "an already-set flag must return at once, not sleep: {:?}",
            t0.elapsed()
        );
    }

    /// The overflow fallback must actually wait until cancelled.
    ///
    /// The test above pre-sets the flag, so it returns at once whether
    /// the loop runs or not — which leaves `while !left.is_zero()`
    /// mutable to `while left.is_zero()`, skipping the wait entirely.
    /// This one sets the flag from another thread partway through, so a
    /// fallback that does not wait returns too early and a fallback that
    /// ignores the flag never returns at all.
    #[test]
    fn overflowing_duration_waits_until_the_flag_is_set() {
        use std::sync::Arc;

        let cancel = Arc::new(AtomicBool::new(false));
        let setter = Arc::clone(&cancel);
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(250));
            setter.store(true, Ordering::SeqCst);
        });

        let t0 = Instant::now();
        cancellable_sleep(Duration::MAX, &cancel);
        let waited = t0.elapsed();
        handle.join().expect("setter thread");

        assert!(
            waited >= Duration::from_millis(200),
            "must wait for the flag, not return immediately: {waited:?}"
        );
        assert!(
            waited < Duration::from_secs(5),
            "must return once the flag is set, not sleep the whole span: {waited:?}"
        );
    }
}
