//! Progress-bar construction for Phase 4 (flash) and Phase 5b (verify).
//!
//! The rendered lines these bars produce are part of the terminal
//! **output contract** (`.agents/docs/00-cli-and-ux.md`): operators pipe
//! `imi` into log aggregators and parse these lines, so any change to a
//! template here is a breaking change to that contract. Centralising
//! both constructors and the shared template in one module means a
//! future format change touches one file — and cannot silently desync
//! the two phases' rendering.

use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

/// Template for the compressed-image spinner.
///
/// Compressed input has no known decompressed total, so the unified
/// percent/total layout is unavailable and this is the fallback. Named
/// rather than inlined so it can be parsed by a unit test: `indicatif`
/// validates a template only at `with_template()` time, which for this
/// branch happens inside Phase 4 with the guard already armed.
const SPINNER_TEMPLATE: &str = "   {spinner} {bytes} written ({bytes_per_sec})";

/// Unified progress-bar template shared by Phase 4 (flash, raw image)
/// and Phase 5b (verify). Five fixed components in fixed positions:
/// a 40-cell bar in brackets, percent right-aligned to 3 columns (so
/// the "%" stays in the same column from "  0%" through "100%"),
/// `{bytes} / {total_bytes}` with binary suffixes, and the current
/// rate in parentheses.
///
/// Kept in one place so a future format change touches one constant
/// rather than two parallel templates that could drift apart.
///
/// Sample rendered output:
/// ```text
///    [==================>                     ]  47%  476.84 MiB / 1.00 GiB (1.35 GiB/s)
/// ```
const UNIFIED_BAR_TEMPLATE: &str =
    "   [{bar:40}] {percent:>3}%  {bytes} / {total_bytes} ({bytes_per_sec})";

/// Build the Phase 4 progress bar: a percent/total bar for raw images
/// (known size), a byte-count spinner for compressed streams.
#[expect(
    clippy::expect_used,
    reason = "both templates are named constants whose parseability is \
              pinned by unit tests that call this very function, so a \
              malformed template fails the suite rather than panicking \
              inside Phase 4 with the guard armed"
)]
pub(crate) fn make_progress_bar(total: Option<u64>) -> ProgressBar {
    if let Some(n) = total {
        let pb = ProgressBar::new(n);
        pb.set_style(
            // Unified template — see `UNIFIED_BAR_TEMPLATE` for the
            // layout rationale.
            //
            // `{bytes_per_sec}` (inside the constant) uses
            // indicatif's built-in double-smoothed EWMA estimator
            // (see indicatif's `state.rs::Estimator`), which is
            // what we want for the throttle case — combined with
            // `reset_elapsed()` before the loop, this prevents
            // the initial-spike artefact without any extra plumbing.
            ProgressStyle::with_template(UNIFIED_BAR_TEMPLATE)
                .expect("valid progress template")
                .progress_chars("=> "),
        );
        pb
    } else {
        // Compressed input: we don't know the final uncompressed size,
        // so the unified percent/total format isn't available — fall
        // back to a spinner with bytes-written and rate. Operators
        // running on compressed images already accept that they
        // can't see "X% complete" anywhere; this is the same
        // limitation `dd` and similar tools have.
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::with_template(SPINNER_TEMPLATE).expect("valid spinner template"),
        );
        pb.enable_steady_tick(Duration::from_millis(100));
        pb
    }
}

/// Build the verify-phase progress bar with the shared unified template.
#[expect(
    clippy::expect_used,
    reason = "UNIFIED_BAR_TEMPLATE is a named constant whose parseability \
              is pinned by a unit test that calls this very function, so a \
              malformed template fails the suite rather than panicking \
              inside Phase 5b with the guard armed"
)]
pub(crate) fn make_verify_pb(total: u64) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        // Unified template shared with Phase 4 (flash, raw image).
        // See `UNIFIED_BAR_TEMPLATE` above for the layout rationale.
        //
        // Both phases now render identically so the operator's eye
        // doesn't have to recalibrate when the pipeline transitions
        // from writing to verification.
        //
        // The `{bytes_per_sec}` rate uses the same double-smoothed EWMA
        // as the flash phase; verification reads from the device under
        // O_DIRECT-cleared mode so the rate is a meaningful "this is
        // how fast we're reading back" metric, not just an artefact of
        // the page cache.
        ProgressStyle::with_template(UNIFIED_BAR_TEMPLATE)
            .expect("valid verify template")
            .progress_chars("=> "),
    );
    pb
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- UNIFIED_BAR_TEMPLATE ------------------------------------------

    /// The shared constant must parse as a valid indicatif template.
    /// indicatif validates at `with_template()` time; this guards
    /// against a future edit that introduces a typo (mismatched braces,
    /// invalid token name) and would only otherwise surface at the
    /// first call site to construct the bar.
    #[test]
    fn unified_template_is_valid_indicatif_syntax() {
        let result = ProgressStyle::with_template(UNIFIED_BAR_TEMPLATE);
        assert!(result.is_ok(), "UNIFIED_BAR_TEMPLATE failed to parse: {:?}", result.err());
    }

    /// The template must reference all five contractual tokens by
    /// name. Catches a regression where someone "simplifies" the
    /// template and silently drops one of them — indicatif renders
    /// unknown tokens as empty strings, so a missing token is *not*
    /// a parse error (see the historical `{smoothed_bytes_per_sec}`
    /// bug for what that costs).
    ///
    /// We check token names, not exact width specs (`{bar:40}` rather
    /// than `{bar}`, `{percent:>3}` rather than `{percent}`), because
    /// the alignment and width are UX choices that may legitimately
    /// evolve — but a missing token name is always a regression.
    #[test]
    fn unified_template_references_required_tokens() {
        for tok in &["{bar", "{percent", "{bytes}", "{total_bytes}", "{bytes_per_sec}"] {
            assert!(
                UNIFIED_BAR_TEMPLATE.contains(tok),
                "UNIFIED_BAR_TEMPLATE missing required token name {tok}"
            );
        }
    }
    // -- constructors ---------------------------------------------------
    //
    // How lenient `ProgressStyle::with_template` is decides what these
    // tests have to cover, so it was measured rather than assumed: it
    // rejects malformed *style specifiers* — `{bar:notanumber}`,
    // `{bar:40:.red/blue}` — and accepts everything else, including
    // unknown keys (`{bogus}`), unbalanced braces (`{bar:40`) and a bare
    // `{`.
    //
    // That is why the two kinds of test are complementary rather than
    // redundant. The parse tests guard the `.expect()` panic path; the
    // token tests guard the silent-wrong-rendering path, where a dropped
    // `{bytes_per_sec}` parses cleanly and renders nothing.
    //
    // Each `.expect()`ed template is exercised through its real
    // constructor, not just parsed here. `indicatif` validates only at
    // `with_template()` time, and all three call sites sit inside the
    // destructive window with the guard armed — a typo that surfaces
    // only in a root-gated run is not caught early enough.

    /// Raw input takes the percent/total bar.
    #[test]
    #[cfg_attr(miri, ignore)] // MIRI ICE
    fn make_progress_bar_parses_the_unified_template() {
        let pb = make_progress_bar(Some(1024));
        pb.finish_and_clear();
    }

    /// An unknown total takes the spinner — the branch whose template
    /// was previously an untested inline literal. A compressed image is
    /// the case that reaches it: its decompressed length is not known
    /// until the write finishes.
    #[test]
    #[cfg_attr(miri, ignore)] // MIRI ICE
    fn make_progress_bar_parses_the_spinner_template() {
        let pb = make_progress_bar(None);
        assert_eq!(pb.length(), None, "an unknown total must leave the bar unbounded");
        pb.finish_and_clear();
    }

    /// Phase 5b's bar shares the unified template but builds it in its
    /// own constructor, so it gets its own call.
    #[test]
    #[cfg_attr(miri, ignore)] // MIRI ICE
    fn make_verify_pb_parses_its_template() {
        let pb = make_verify_pb(4096);
        assert_eq!(pb.length(), Some(4096), "the verify bar is always bounded");
        pb.finish_and_clear();
    }

    /// The spinner template must carry the tokens operators rely on.
    #[test]
    fn spinner_template_references_required_tokens() {
        for tok in ["{spinner}", "{bytes}", "{bytes_per_sec}"] {
            assert!(
                SPINNER_TEMPLATE.contains(tok),
                "SPINNER_TEMPLATE missing required token {tok}"
            );
        }
    }

    /// Directly parseable too, independent of the constructors.
    #[test]
    fn spinner_template_is_valid_indicatif_syntax() {
        let result = ProgressStyle::with_template(SPINNER_TEMPLATE);
        assert!(result.is_ok(), "SPINNER_TEMPLATE failed to parse: {:?}", result.err());
    }
}
