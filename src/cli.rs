//! Command-line interface for `imi`.
//!
//! Defines the `Cli` struct (parsed by `clap`) and the custom value parser for
//! `--throttle`, which accepts byte-rate values with binary (K/M/G) suffixes.
//!
//! The `--throttle` flag has three distinct states that the bash original also
//! supported and that call-sites downstream rely on:
//! 1. Flag absent entirely → `None` → run at maximum unthrottled speed.
//! 2. Flag present with no value (`-t`) → `Some(8 MiB/s)` via `default_missing_value`.
//! 3. Flag present with a value (`-t 20M`) → `Some(parsed)`.

use std::path::PathBuf;

use clap::Parser;

/// Parsed command-line arguments.
#[derive(Debug, Parser)]
#[command(
    name = "imi",
    version,
    about = "Safely flash an ISO/IMG (optionally compressed) to a USB block device."
)]
pub(crate) struct Cli {
    /// Path to the source .iso/.img file (may be gzip/xz/bzip2/zstd compressed).
    #[arg(short = 'i', long = "img", value_name = "PATH")]
    pub(crate) img: PathBuf,

    /// Target block device, e.g. /dev/sdc. Must be a whole disk, not a partition.
    #[arg(short = 'd', long = "dev", value_name = "DEVICE")]
    pub(crate) dev: PathBuf,

    /// Write/read rate cap (e.g. 500K, 8M, 1G). Omit flag entirely for
    /// unthrottled; pass `-t` with no value to default to 8M.
    #[arg(
        short = 't',
        long = "throttle",
        value_name = "RATE",
        num_args = 0..=1,
        default_missing_value = "8M",
        value_parser = parse_rate,
    )]
    pub(crate) throttle: Option<u64>,

    /// Skip the interactive TTY confirmation. Intended for automation; use with care.
    #[arg(short = 'y', long = "yes")]
    pub(crate) yes: bool,

    /// Skip Phase 5b byte-for-byte verification. The hardware cooldown
    /// (Phase 5a, unless --skip-cooldown), kernel partition-table sync (Phase 6),
    /// and automount defense (Phase 7) all still run — only the readback compare
    /// is omitted. Use this when you trust the device and the throughput gain is
    /// worth the loss of the defect-detection pass.
    #[arg(long = "skip-verification")]
    pub(crate) skip_verification: bool,

    /// Skip the 10-second hardware cooldown (Phase 5a). The cooldown lets cheap
    /// USB-NAND bridge controllers drain their DRAM write cache to flash after
    /// fdatasync returns; skipping it risks silent corruption on unplug for such
    /// devices. Intended for loop devices, automated tests, and high-quality
    /// media whose controllers honor cache flushes.
    #[arg(long = "skip-cooldown")]
    pub(crate) skip_cooldown: bool,
}

/// Custom `clap` value parser that turns a rate like `500K`, `8M`, `1G`
/// into a byte-rate (`u64`). Suffixes are case-insensitive binary multiples
/// (K = 1024, M = 1024², G = 1024³). Bare integers are accepted as bytes/sec.
///
/// Rejects zero, negative, and malformed inputs with a user-facing error.
pub(crate) fn parse_rate(s: &str) -> Result<u64, String> {
    /// Binary-suffix multipliers, const so the products are compile-time.
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;

    let s = s.trim();
    if s.is_empty() {
        return Err("throttle rate must not be empty".into());
    }

    // `strip_suffix` is UTF-8-boundary-exact by construction, so a
    // multibyte final character simply fails all three suffix matches
    // and falls through to the digit / error arms below.
    let (num_part, mult) = if let Some(n) = s.strip_suffix(['k', 'K']) {
        (n, KIB)
    } else if let Some(n) = s.strip_suffix(['m', 'M']) {
        (n, MIB)
    } else if let Some(n) = s.strip_suffix(['g', 'G']) {
        (n, GIB)
    } else if s.as_bytes().last().copied().is_some_and(|b| b.is_ascii_digit()) {
        (s, 1)
    } else {
        // Display the actual final character, not the trailing UTF-8
        // byte interpreted as a code point. `s.chars().last()` yields
        // `None` only when `s` is empty, which we already checked above.
        let bad = s.chars().last().unwrap_or('?');
        return Err(format!("invalid throttle suffix '{bad}' in '{s}'; expected K, M, G, or none"));
    };

    if num_part.is_empty() {
        return Err(format!("throttle rate missing numeric component in '{s}'"));
    }

    let n: u64 =
        num_part.parse().map_err(|e| format!("invalid throttle numeric value in '{s}': {e}"))?;

    if n == 0 {
        return Err("throttle rate must be greater than zero".into());
    }

    n.checked_mul(mult).ok_or_else(|| format!("throttle rate '{s}' overflows u64"))
}

#[cfg(test)]
mod tests {
    use super::{Cli, parse_rate};
    use clap::Parser as _;

    #[test]
    fn parses_plain_bytes() {
        assert_eq!(parse_rate("1024").unwrap(), 1024);
    }

    #[test]
    fn parses_suffixes() {
        assert_eq!(parse_rate("1K").unwrap(), 1024);
        assert_eq!(parse_rate("1k").unwrap(), 1024);
        assert_eq!(parse_rate("8M").unwrap(), 8 * 1024 * 1024);
        assert_eq!(parse_rate("2G").unwrap(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn rejects_bad() {
        parse_rate("").unwrap_err();
        parse_rate("0").unwrap_err();
        parse_rate("0M").unwrap_err();
        parse_rate("M").unwrap_err();
        parse_rate("12X").unwrap_err();
        parse_rate("abc").unwrap_err();
    }

    /// Whitespace tolerance: trim leading/trailing whitespace per the
    /// `s.trim()` at the top of `parse_rate`.
    #[test]
    fn tolerates_surrounding_whitespace() {
        assert_eq!(parse_rate("  1024  ").unwrap(), 1024);
        assert_eq!(parse_rate("\t8M\n").unwrap(), 8 * 1024 * 1024);
    }

    /// Both lowercase and uppercase suffixes are accepted; this is the
    /// documented behaviour and existing scripts may rely on it.
    #[test]
    fn case_insensitive_suffixes() {
        assert_eq!(parse_rate("1m").unwrap(), 1024 * 1024);
        assert_eq!(parse_rate("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_rate("1g").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_rate("1G").unwrap(), 1024 * 1024 * 1024);
    }

    /// `checked_mul` guards against overflow when multiplying by
    /// 1024^3. A value just over `u64::MAX / GiB` must be rejected
    /// rather than silently wrapping.
    #[test]
    fn rejects_overflow_on_large_g() {
        // u64::MAX / GiB ≈ 17_179_869_184; 17_179_869_185G overflows.
        parse_rate("17179869185G").unwrap_err();
        // u64::MAX itself with a G suffix overflows trivially.
        let huge = format!("{}G", u64::MAX);
        parse_rate(&huge).unwrap_err();
    }

    /// Boundary: largest accepted G value should parse.
    #[test]
    fn accepts_largest_safe_g_value() {
        // Largest n such that n * 1024^3 <= u64::MAX.
        let limit = u64::MAX / (1024 * 1024 * 1024);
        let s = format!("{limit}G");
        let parsed = parse_rate(&s).unwrap();
        assert_eq!(parsed, limit * 1024 * 1024 * 1024);
    }

    /// A suffix-only input ("M", "k") has an empty numeric component
    /// after stripping the suffix and must be rejected with the
    /// "missing numeric component" message rather than panicking on
    /// the empty `parse::<u64>`.
    #[test]
    fn rejects_suffix_only_inputs() {
        for s in ["K", "k", "M", "m", "G", "g"] {
            assert!(parse_rate(s).is_err(), "{s:?} should be rejected");
        }
    }

    /// Negative sign is not in the suffix set and not a digit; must
    /// be rejected.
    #[test]
    fn rejects_negative_sign() {
        parse_rate("-100").unwrap_err();
        parse_rate("-1M").unwrap_err();
    }

    /// Leading-zero numerics are valid u64 (`u64::from_str` accepts them);
    /// document that we don't reject them.
    #[test]
    fn accepts_leading_zeros() {
        assert_eq!(parse_rate("0001024").unwrap(), 1024);
        assert_eq!(parse_rate("01M").unwrap(), 1024 * 1024);
    }

    /// Decimal points are not handled — we only accept integer rates.
    #[test]
    fn rejects_decimal_values() {
        parse_rate("1.5M").unwrap_err();
        parse_rate("0.5G").unwrap_err();
    }

    /// Multibyte trailing characters in the error path: the displayed
    /// character must be the actual final code point, not the trailing
    /// UTF-8 byte reinterpreted. This catches a regression where someone
    /// "simplifies" the unwrap-or by going back to `as char` casting on
    /// the trailing byte — which displays the wrong glyph for any
    /// 3-byte-or-longer UTF-8 character.
    #[test]
    fn error_message_displays_actual_trailing_char() {
        let err = parse_rate("1💩").unwrap_err();
        // The actual trailing character is 💩 (U+1F4A9), encoded in UTF-8
        // as `F0 9F 92 A9`. The trailing byte 0xA9 reinterpreted as a
        // code point would give `©` (U+00A9) — which would be wrong.
        assert!(err.contains('💩'), "error should contain the real glyph, got: {err}");
        assert!(
            !err.contains('©'),
            "error must not contain the misinterpreted byte glyph, got: {err}"
        );
    }

    /// Pure-ASCII garbage trailing character also produces a coherent
    /// error message. The pre-existing match arm already handled this;
    /// this test prevents a regression where the new path breaks the
    /// ASCII case.
    #[test]
    fn error_message_displays_ascii_trailing_char() {
        let err = parse_rate("1Z").unwrap_err();
        assert!(err.contains('Z'), "error should name the bad suffix, got: {err}");
    }
    /// Both skip flags parse as long-only booleans.
    #[test]
    fn skip_flags_parse() {
        let c = Cli::try_parse_from([
            "imi",
            "-i",
            "/i",
            "-d",
            "/d",
            "--skip-verification",
            "--skip-cooldown",
        ])
        .unwrap();
        assert!(c.skip_verification);
        assert!(c.skip_cooldown);
        let bare = Cli::try_parse_from(["imi", "-i", "/i", "-d", "/d"]).unwrap();
        assert!(!bare.skip_verification);
        assert!(!bare.skip_cooldown);
    }

    /// The old spellings are gone: `-n` and `--no-verify` must be
    /// rejected by clap, not silently accepted.
    #[test]
    fn removed_no_verify_spellings_are_rejected() {
        Cli::try_parse_from(["imi", "-i", "/i", "-d", "/d", "-n"]).unwrap_err();
        Cli::try_parse_from(["imi", "-i", "/i", "-d", "/d", "--no-verify"]).unwrap_err();
    }
}
