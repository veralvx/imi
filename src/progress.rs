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

use crate::image::Compression;

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
pub(crate) const UNIFIED_BAR_TEMPLATE: &str =
    "   [{bar:40}] {percent:>3}%  {bytes} / {total_bytes} ({bytes_per_sec})";

/// Build the Phase 4 progress bar: a percent/total bar for raw images
/// (known size), a byte-count spinner for compressed streams.
#[expect(
    clippy::expect_used,
    reason = "both templates are compile-time constants; \
              UNIFIED_BAR_TEMPLATE is additionally pinned by unit tests. \
              Template parsing cannot fail at runtime"
)]
pub(crate) fn make_progress_bar(comp: Compression, raw_size: Option<u64>) -> ProgressBar {
    if let (Compression::Raw, Some(n)) = (comp, raw_size) {
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
            ProgressStyle::with_template("   {spinner} {bytes} written ({bytes_per_sec})")
                .expect("valid spinner template"),
        );
        pb.enable_steady_tick(Duration::from_millis(100));
        pb
    }
}

/// Build the verify-phase progress bar with the shared unified template.
#[expect(
    clippy::expect_used,
    reason = "UNIFIED_BAR_TEMPLATE is a compile-time constant validated by \
              the flash phase's tests; template parsing cannot fail at runtime"
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
}
