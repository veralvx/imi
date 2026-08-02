//! Throttle rate handling, shared by Phase 4 and Phase 5b.
//!
//! Both phases pace themselves the same way: convert an optional
//! bytes-per-second cap into a per-chunk wall-clock target, then sleep
//! out whatever is left of that target after the chunk's real work.
//! The conversion lived in four places before this module — once per
//! arm, per phase — which is three places too many for arithmetic whose
//! correctness depends on rejecting a zero divisor.

use crate::Result;
use crate::common::aligned::BUF_SIZE;
use crate::error::bail;

// The conversion below relies on `BUF_SIZE * 1e9` fitting in a u64
// after the u128 intermediate. That holds with a factor of ~4400 to
// spare at 4 MiB, but it is an assumption about a constant declared in
// another module, so it is checked rather than trusted: the alternative
// failure is `u64::try_from(..).unwrap_or(u64::MAX)` quietly yielding a
// 584-year chunk target.
const _: () = assert!(
    (BUF_SIZE as u128).saturating_mul(1_000_000_000) <= u64::MAX as u128,
    "BUF_SIZE * 1e9 must fit in u64 or the throttle target clamps silently"
);

/// Convert a throttle rate in bytes per second into the wall-clock
/// nanoseconds one `BUF_SIZE` chunk should take.
///
/// `None` in means unthrottled, and `None` out.
///
/// # Errors
///
/// Returns an error for a rate of zero. A zero rate is not a slow flash,
/// it is an impossible one: the per-chunk target would be unbounded and
/// the phase would sleep effectively forever on its first chunk.
///
/// The binary's argument parser also rejects `-t 0`, but that is not
/// where this belongs. `Config::throttle` is a public, freely-assignable
/// field and every phase is a public entry point, so a library consumer
/// reaches this code without passing through any CLI parsing at all.
/// Validating here makes the invariant hold for every caller instead of
/// only the one that happens to come through `clap`.
pub(crate) fn chunk_target_nanos(throttle: Option<u64>) -> Result<Option<u64>> {
    let Some(rate_bps) = throttle else {
        return Ok(None);
    };
    if rate_bps == 0 {
        bail!(
            "throttle rate must be at least 1 byte per second (got 0); a zero \
             rate would stall the transfer indefinitely rather than slow it"
        );
    }

    // None of the three fallbacks below can fire, and it is worth being
    // explicit about that rather than leaving them to look load-bearing:
    //
    //   saturating_mul  BUF_SIZE * 1e9 is 4.19e15 against a u128 ceiling
    //                   of 3.4e38 — twenty-three orders of magnitude of
    //                   headroom.
    //   checked_div     only None for a zero divisor, rejected above.
    //   try_from        4.19e15 against u64::MAX 1.8e19, a factor of
    //                   ~4400. The slowest legal rate, 1 byte/s, gives a
    //                   48.5-day chunk target, which fits comfortably.
    //
    // They are written this way because the crate denies
    // `clippy::arithmetic_side_effects`; plain `*` and `/` do not
    // compile here. The headroom is pinned by the assertion above, so a
    // change to BUF_SIZE that broke it would fail the build rather than
    // clamp silently to u64::MAX — a target of 584 years.
    let ideal = (BUF_SIZE as u128)
        .saturating_mul(1_000_000_000)
        .checked_div(u128::from(rate_bps))
        .unwrap_or(u128::MAX);
    Ok(Some(u64::try_from(ideal).unwrap_or(u64::MAX)))
}

#[cfg(test)]
mod tests {
    use super::chunk_target_nanos;
    use crate::common::aligned::BUF_SIZE;

    /// Unthrottled stays unthrottled.
    #[test]
    fn none_passes_through() {
        assert_eq!(chunk_target_nanos(None).unwrap(), None);
    }

    /// A zero rate is refused rather than silently becoming an
    /// unbounded sleep. This is the case the CLI catches for its own
    /// callers and that a library consumer would otherwise hit.
    #[test]
    fn zero_rate_is_rejected() {
        let err = chunk_target_nanos(Some(0)).unwrap_err();
        assert!(err.to_string().contains("at least 1 byte per second"), "{err}");
    }

    /// A rate of exactly one chunk per second must yield a one-second
    /// target — the arithmetic's anchor point.
    #[test]
    fn one_chunk_per_second_is_one_second() {
        let nanos = chunk_target_nanos(Some(BUF_SIZE as u64)).unwrap().unwrap();
        assert_eq!(nanos, 1_000_000_000);
    }

    /// Halving the rate doubles the per-chunk target.
    #[test]
    fn halving_the_rate_doubles_the_target() {
        let full = chunk_target_nanos(Some(BUF_SIZE as u64)).unwrap().unwrap();
        let half = chunk_target_nanos(Some(BUF_SIZE as u64 / 2)).unwrap().unwrap();
        assert_eq!(half, full * 2);
    }

    /// The slowest representable rate must still produce a finite target
    /// rather than overflowing or saturating to `u64::MAX`.
    #[test]
    fn one_byte_per_second_is_finite_and_large() {
        let nanos = chunk_target_nanos(Some(1)).unwrap().unwrap();
        assert_eq!(nanos, BUF_SIZE as u64 * 1_000_000_000);
        assert!(nanos < u64::MAX);
    }

    /// A rate far above any real device clamps the target toward zero
    /// without underflowing.
    #[test]
    fn absurdly_fast_rate_yields_no_meaningful_delay() {
        assert_eq!(chunk_target_nanos(Some(u64::MAX)).unwrap().unwrap(), 0);
    }
}
