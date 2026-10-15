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

use imi_core::Config;

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
    /// Omit to pick interactively: candidate devices are listed (path,
    /// size, model; removable first) for arrow-key selection. Only whole
    /// disks with media, no block-layer holders, and no mounts outside
    /// /media, /run/media, /var/run/media are offered, so the system
    /// disk never appears. Esc aborts; without a terminal the candidates
    /// are printed and the run refuses. Nothing is ever auto-selected.
    #[arg(short = 'd', long = "dev", value_name = "DEVICE")]
    pub(crate) dev: Option<PathBuf>,

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

    /// Skip the Phase 5a hardware cooldown entirely (20 s by default; see
    /// --cooldown-secs to shorten rather than skip it). The cooldown lets cheap
    /// USB-NAND bridge controllers drain their DRAM write cache to flash after
    /// fdatasync returns; skipping it risks silent corruption on unplug for such
    /// devices. Intended for loop devices, automated tests, and high-quality
    /// media whose controllers honor cache flushes.
    #[arg(long = "skip-cooldown")]
    pub(crate) skip_cooldown: bool,

    /// Seconds to wait after the flash for the device controller to drain its write
    /// cache (Phase 5a). Default 20. Lowering it risks two things: corruption if the
    /// device is unplugged while its controller is still writing, and — seen more
    /// often — verification reporting a false failure, because reading at full speed
    /// during the drain window can brown out a bus-powered bridge. This matters more,
    /// not less, with --skip-verification: without the verify pass the cooldown is the
    /// only settling time before unplug. Use --skip-cooldown to omit the wait entirely.
    #[arg(
        long = "cooldown-secs",
        value_name = "SECONDS",
        conflicts_with = "skip_cooldown",
        value_parser = parse_cooldown_secs
    )]
    pub(crate) cooldown_secs: Option<u64>,
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

/// Parse and bounds-check `--cooldown-secs`.
///
/// The bounds themselves live in [`imi_core::validate_cooldown_secs`], so the
/// library holds them for every caller; mirroring them here means a typo fails
/// at argument-parse time, before any device is opened.
fn parse_cooldown_secs(s: &str) -> Result<u64, String> {
    let s = s.trim();
    // `30s` is the predictable mistake, because every other numeric flag here
    // takes a suffix. Left to `u64::from_str` it answers "invalid digit found
    // in string", which names neither the problem nor the fix.
    //
    // The let-chain is not a style choice: as a nested `if` this trips clippy's
    // `collapsible_if`, which is default-warn and therefore fatal under the
    // workspace's `-D warnings`.
    if let Some(stem) = s.strip_suffix(['s', 'S'])
        && !stem.is_empty()
        && stem.chars().all(|c| c.is_ascii_digit())
    {
        return Err(format!("this flag takes plain seconds — write '{stem}', not '{s}'"));
    }
    let n: u64 = s.parse().map_err(|e| format!("invalid cooldown seconds '{s}': {e}"))?;
    imi_core::validate_cooldown_secs(n).map_err(|e| e.to_string())?;
    Ok(n)
}

impl Cli {
    /// Project the parsed arguments onto the core's own input type.
    ///
    /// The core deliberately knows nothing about `clap`; this is the one
    /// place the two vocabularies meet.
    pub(crate) fn to_config(&self, dev: PathBuf) -> Config {
        let mut config = Config::new(self.img.clone(), dev);
        config.yes = self.yes;
        config.throttle = self.throttle;
        config.skip_cooldown = self.skip_cooldown;
        config.skip_verification = self.skip_verification;
        config.cooldown_secs = self.cooldown_secs;
        config
    }
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
    /// Every CLI field must land on the matching `Config` field.
    ///
    /// Distinct values are used per field so a transposed assignment in
    /// `to_config` fails here rather than silently changing behaviour.
    #[test]
    fn to_config_maps_every_field() {
        let config = Cli::try_parse_from([
            "imi",
            "-i",
            "/tmp/src.iso",
            "-d",
            "/dev/loop9",
            "--yes",
            "-t",
            "1024",
            "--skip-cooldown",
            "--skip-verification",
        ])
        .unwrap()
        .to_config(std::path::PathBuf::from("/dev/test-resolved"));

        assert_eq!(config.img, std::path::PathBuf::from("/tmp/src.iso"));
        // `-d` no longer reaches `to_config`; resolution happens in `picker`
        // and the resolved path is passed in. The parse itself is pinned by
        // `dev_flag_parses_into_the_option` below.
        assert_eq!(config.dev, std::path::PathBuf::from("/dev/test-resolved"));
        assert!(config.yes);
        assert_eq!(config.throttle, Some(1024));
        assert!(config.skip_cooldown);
        assert!(config.skip_verification);
        assert_eq!(
            config.cooldown_secs, None,
            "an unset --cooldown-secs must map to None, not to the default"
        );

        // `--cooldown-secs` cannot join the argv above: it conflicts with
        // `--skip-cooldown`, which that invocation needs in order to pin
        // the skip flag's mapping. A second parse covers the other arm —
        // without it, deleting the `config.cooldown_secs = ...` line in
        // `to_config` would pass every test in this file.
        let with_cooldown = Cli::try_parse_from([
            "imi",
            "-i",
            "/tmp/src.iso",
            "-d",
            "/dev/loop9",
            "--yes",
            "--cooldown-secs",
            "30",
        ])
        .unwrap()
        .to_config(std::path::PathBuf::from("/dev/test-resolved"));
        assert_eq!(with_cooldown.cooldown_secs, Some(30));
        assert!(!with_cooldown.skip_cooldown, "the two must stay independent");
    }

    /// `-d` parses into `Some`, and omitting it parses into `None`.
    ///
    /// `None` is what routes `main` through the picker, so this is the
    /// contract between the CLI and `picker::resolve_device`: an omitted
    /// flag must be representable, not an error.
    #[test]
    fn dev_flag_parses_into_the_option() {
        let with = Cli::try_parse_from(["imi", "-i", "/i", "-d", "/dev/sdz"]).unwrap();
        assert_eq!(with.dev.as_deref(), Some(std::path::Path::new("/dev/sdz")));

        let without = Cli::try_parse_from(["imi", "-i", "/i"]).unwrap();
        assert_eq!(without.dev, None, "omitting --dev must parse, not error");
    }

    /// `--cooldown-secs` accepts a plain integer and rejects everything else.
    ///
    /// The `-1` case is not a value error: clap reads a leading dash as an
    /// unknown flag, so `parse_cooldown_secs` never runs. Asserting a parser
    /// message there would fail for a reason unrelated to this flag.
    #[test]
    fn cooldown_secs_parses_and_bounds_check() {
        for good in ["1", "20", "30", "600"] {
            let cli = Cli::try_parse_from(["imi", "-i", "/i", "--cooldown-secs", good]).unwrap();
            assert_eq!(cli.cooldown_secs, Some(good.parse().unwrap()), "{good}");
        }

        let zero = Cli::try_parse_from(["imi", "-i", "/i", "--cooldown-secs", "0"]).unwrap_err();
        assert!(
            zero.to_string().contains("--skip-cooldown"),
            "refusing zero must name the flag that carries the warning: {zero}"
        );

        for over in ["601", "10000"] {
            let err =
                Cli::try_parse_from(["imi", "-i", "/i", "--cooldown-secs", over]).unwrap_err();
            // The full phrase, not a bare "600": a message naming a wrong
            // bound like "6000" would satisfy a substring check.
            assert!(
                err.to_string().contains(&format!("maximum of {}s", imi_core::MAX_COOLDOWN_SECS)),
                "the bound must be named exactly: {err}"
            );
        }

        let suffixed =
            Cli::try_parse_from(["imi", "-i", "/i", "--cooldown-secs", "30s"]).unwrap_err();
        assert!(
            suffixed.to_string().contains("plain seconds") && suffixed.to_string().contains("'30'"),
            "the suffix message must name the fix: {suffixed}"
        );

        for bad in ["abc", "1.5", ""] {
            assert!(
                Cli::try_parse_from(["imi", "-i", "/i", "--cooldown-secs", bad]).is_err(),
                "{bad:?} must not parse"
            );
        }

        let negative =
            Cli::try_parse_from(["imi", "-i", "/i", "--cooldown-secs", "-1"]).unwrap_err();
        assert!(
            negative.to_string().contains("unexpected argument"),
            "a leading dash is an unknown flag to clap, not a bad value: {negative}"
        );

        let omitted = Cli::try_parse_from(["imi", "-i", "/i"]).unwrap();
        assert_eq!(omitted.cooldown_secs, None, "omitting the flag must parse, not error");
    }

    /// Setting a duration and skipping the wait are contradictory, and clap
    /// refuses the pair rather than picking a winner.
    #[test]
    fn cooldown_secs_conflicts_with_skip_cooldown() {
        let err =
            Cli::try_parse_from(["imi", "-i", "/i", "--skip-cooldown", "--cooldown-secs", "30"])
                .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("--skip-cooldown"), "{text}");
        assert!(text.contains("--cooldown-secs"), "{text}");
    }

    /// The help text states the same default the library actually uses.
    ///
    /// The number lives in `DEFAULT_COOLDOWN_SECS` and, unavoidably, in a
    /// doc comment that cannot interpolate it. This is the gate that keeps
    /// the two from drifting when the default next moves.
    #[test]
    fn help_states_the_real_default() {
        let help = <Cli as clap::CommandFactory>::command().render_long_help().to_string();
        assert!(
            help.contains(&format!("Default {}", imi_core::DEFAULT_COOLDOWN_SECS)),
            "--help must name the real default ({}): {help}",
            imi_core::DEFAULT_COOLDOWN_SECS
        );
    }

    /// The two skip flags are independent and must not be swapped: each
    /// disables a different safety step, so a transposition would be a
    /// silent safety regression.
    #[test]
    fn to_config_keeps_the_skip_flags_independent() {
        let cooldown_only = Cli::try_parse_from(["imi", "-i", "/i", "-d", "/d", "--skip-cooldown"])
            .unwrap()
            .to_config(std::path::PathBuf::from("/dev/test-resolved"));
        assert!(cooldown_only.skip_cooldown);
        assert!(!cooldown_only.skip_verification, "--skip-cooldown must not skip verification");

        let verify_only =
            Cli::try_parse_from(["imi", "-i", "/i", "-d", "/d", "--skip-verification"])
                .unwrap()
                .to_config(std::path::PathBuf::from("/dev/test-resolved"));
        assert!(verify_only.skip_verification);
        assert!(!verify_only.skip_cooldown, "--skip-verification must not skip the cooldown");
    }

    /// With no flags, every safety step must remain enabled.
    #[test]
    fn to_config_defaults_keep_every_safety_step() {
        let config = Cli::try_parse_from(["imi", "-i", "/i", "-d", "/d"])
            .unwrap()
            .to_config(std::path::PathBuf::from("/dev/test-resolved"));
        assert!(!config.yes, "confirmation must be required by default");
        assert!(config.throttle.is_none(), "throttle must default to unlimited");
        assert!(!config.skip_cooldown, "cooldown must run by default");
        assert!(!config.skip_verification, "verification must run by default");
        assert_eq!(
            config.cooldown_secs, None,
            "an omitted --cooldown-secs must defer to the library default"
        );
    }
}
