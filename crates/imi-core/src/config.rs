//! Runtime configuration for the pipeline.
//!
//! This is the core's own input type: it carries exactly what the phases
//! need and nothing about how a caller obtained it. The `imi` binary
//! builds one from its `clap` arguments, but any caller can construct
//! one directly.

use std::path::PathBuf;

use crate::Result;
use crate::error::bail;

/// Everything the pipeline needs from its caller.
///
/// Marked `#[non_exhaustive]`: construct with [`Config::new`] and set the
/// optional fields afterwards, so that later releases can add options
/// without breaking callers.
///
/// ```no_run
/// # use std::path::PathBuf;
/// let mut config = imi_core::Config::new(
///     PathBuf::from("/tmp/image.iso.zst"),
///     PathBuf::from("/dev/sdc"),
/// );
/// config.yes = true;
/// config.throttle = Some(8 * 1024 * 1024);
/// imi_core::run(&config).unwrap();
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Config {
    /// Source image; may be gzip/xz/bzip2/zstd compressed.
    pub img: PathBuf,
    /// Target whole-disk block device (not a partition).
    pub dev: PathBuf,
    /// The caller has already confirmed; do not ask again.
    ///
    /// When this is `false`, Phase 0 asks [`crate::Events::confirm`] —
    /// whose default implementation **refuses**. An unattended caller
    /// passing a silent sink therefore needs this set, or the run aborts
    /// with `aborted by user` before anything is touched. The `imi`
    /// binary sets it from `--yes`; a front end that puts the question
    /// to a human implements `confirm` instead and leaves this `false`.
    pub yes: bool,
    /// Write/read rate cap in bytes per second; `None` is unthrottled.
    ///
    /// `Some(0)` is refused at runtime rather than made unrepresentable
    /// by `Option<NonZeroU64>`: this is a field a caller sets by hand,
    /// and `NonZeroU64::new(8 * 1024 * 1024).unwrap()` is a poor trade
    /// for an invariant that Phase 4 already checks and a test already
    /// pins. A zero cap would stall rather than fail, which is why it is
    /// refused rather than clamped.
    pub throttle: Option<u64>,
    /// Skip the Phase 5a hardware cooldown.
    ///
    /// The cooldown lets cheap USB-NAND bridge controllers drain their
    /// write cache after `fdatasync`; skipping it risks corruption on
    /// unplug for such devices.
    pub skip_cooldown: bool,
    /// Skip the Phase 5b byte-for-byte verification.
    pub skip_verification: bool,
    /// Seconds for the Phase 5a hardware cooldown; `None` uses
    /// [`DEFAULT_COOLDOWN_SECS`].
    ///
    /// Validated in Phase 0 and again in Phase 5 rather than made
    /// unrepresentable by a newtype: an out-of-range value here only
    /// sleeps, so there is no allocation or undefined behaviour to
    /// protect against, and a field callers set by hand is worth
    /// keeping ergonomic. See [`validate_cooldown_secs`] for the
    /// bounds, and why zero is refused in favour of `skip_cooldown`
    /// rather than accepted as a synonym for it.
    pub cooldown_secs: Option<u64>,
}

impl Config {
    /// A configuration that flashes `img` to `dev` with every safety
    /// step enabled and no rate cap.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::path::PathBuf;
    /// let mut config = imi_core::Config::new(
    ///     PathBuf::from("ubuntu.iso"),
    ///     PathBuf::from("/dev/sdc"),
    /// );
    /// config.yes = true;                    // skip the confirmation
    /// config.throttle = Some(8 * 1024 * 1024); // 8 MiB/s ceiling
    /// assert!(config.yes);
    /// ```
    #[must_use]
    pub fn new(img: PathBuf, dev: PathBuf) -> Self {
        Self {
            img,
            dev,
            yes: false,
            throttle: None,
            skip_cooldown: false,
            skip_verification: false,
            cooldown_secs: None,
        }
    }

    /// The cooldown this configuration resolves to, in seconds.
    ///
    /// One function is the only place the fallback exists, so the phases
    /// and the tests cannot disagree about it. Written inline at each
    /// call site, a test asserting `cooldown_secs.unwrap_or(DEFAULT)`
    /// would be exercising its own copy of the expression — a mutation
    /// run proved exactly that, surviving `unwrap_or(10)` inside Phase 5
    /// with every test green.
    ///
    /// The result is still unvalidated; every caller pairs this with
    /// [`validate_cooldown_secs`].
    pub(crate) fn resolved_cooldown_secs(&self) -> u64 {
        self.cooldown_secs.unwrap_or(DEFAULT_COOLDOWN_SECS)
    }
}

/// The Phase 5a cooldown used when [`Config::cooldown_secs`] is `None`.
///
/// Twenty seconds. The bash original this pipeline descends from used
/// ten, tuned empirically against roughly thirty USB sticks; that
/// measurement is a floor this default respects rather than contradicts,
/// since a longer wait cannot fail where a shorter one succeeded. The
/// extra ten seconds buy the tail of the drain distribution, which the
/// original sample cannot speak to, and the cost of being short is
/// asymmetric: a truncated cooldown risks corruption on unplug, or a
/// verification failure on a device that is actually fine. The cost of
/// being long is ten seconds on an operation that already took minutes.
pub const DEFAULT_COOLDOWN_SECS: u64 = 20;

/// The largest cooldown a caller may request.
///
/// Ten minutes is far past any controller drain; the bound exists to
/// catch the typo class — someone assuming milliseconds and writing
/// `10000` — rather than to stop a considered choice.
pub const MAX_COOLDOWN_SECS: u64 = 600;

// The default must satisfy the bounds this crate enforces. Both are
// edited by hand and live in different paragraphs; without this, a
// future change to either could silently disagree with the other.
const _: () = assert!(
    DEFAULT_COOLDOWN_SECS > 0 && DEFAULT_COOLDOWN_SECS <= MAX_COOLDOWN_SECS,
    "the default cooldown must satisfy the bounds validate_cooldown_secs enforces"
);

/// Bounds-check a requested cooldown, returning it unchanged.
///
/// Called from Phase 0, so a bad value is refused before anything is
/// armed, and again from Phase 5, where it is consumed — a library
/// caller can assign `Config::cooldown_secs` directly, or mutate it
/// after Phase 0, and neither should be able to reach an unbounded
/// sleep. The function is total and does no I/O, so calling it twice
/// costs nothing and keeps one source of truth for the rule.
///
/// # Errors
///
/// Refuses zero: a zero cooldown is [`Config::skip_cooldown`], which
/// carries the corruption warning an innocuous-looking `0` would not.
/// Refuses anything above [`MAX_COOLDOWN_SECS`].
pub fn validate_cooldown_secs(secs: u64) -> Result<u64> {
    if secs == 0 {
        bail!(
            "a zero cooldown is spelled --skip-cooldown, which carries the \
             corruption warning this setting does not"
        );
    }
    if secs > MAX_COOLDOWN_SECS {
        bail!("cooldown of {secs}s exceeds the maximum of {MAX_COOLDOWN_SECS}s");
    }
    Ok(secs)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Config, MAX_COOLDOWN_SECS, validate_cooldown_secs};

    /// `Config::new` must default to the safe end of every choice: the
    /// operator is asked to confirm, nothing is skipped, nothing capped.
    /// A regression here would silently disable a safety step for every
    /// caller that does not set the field.
    #[test]
    fn new_defaults_to_every_safety_step_enabled() {
        let config = Config::new(PathBuf::from("/tmp/a.iso"), PathBuf::from("/dev/loop9"));
        assert_eq!(config.img, PathBuf::from("/tmp/a.iso"));
        assert_eq!(config.dev, PathBuf::from("/dev/loop9"));
        assert!(!config.yes);
        assert!(config.throttle.is_none());
        assert!(!config.skip_cooldown);
        assert!(!config.skip_verification);
        assert!(config.cooldown_secs.is_none(), "the cooldown must default to the library's own");
    }

    /// The bounds, at and either side of every edge.
    #[test]
    fn validate_cooldown_secs_bounds() {
        let zero = validate_cooldown_secs(0).unwrap_err().to_string();
        assert!(zero.contains("--skip-cooldown"), "{zero}");

        assert_eq!(validate_cooldown_secs(1).unwrap(), 1, "one second is legal, if unwise");
        assert_eq!(
            validate_cooldown_secs(MAX_COOLDOWN_SECS).unwrap(),
            MAX_COOLDOWN_SECS,
            "the bound is inclusive"
        );

        let over = validate_cooldown_secs(MAX_COOLDOWN_SECS + 1).unwrap_err().to_string();
        assert!(
            over.contains(&format!("maximum of {MAX_COOLDOWN_SECS}s")),
            "the bound must be named exactly — a wrong bound like 6000 would \
             satisfy a bare substring check: {over}"
        );

        // No overflow, no panic — the typo class reaches here from a
        // library caller with no parser in the path.
        validate_cooldown_secs(u64::MAX).unwrap_err();
    }

    /// An unset field resolves to twenty seconds, and that resolution is
    /// itself valid.
    ///
    /// Asserting the literal rather than the constant is deliberate: written
    /// as `DEFAULT_COOLDOWN_SECS` this test would follow the constant
    /// wherever it went and pin nothing. It is the only place in the tree
    /// that states the number independently.
    #[test]
    fn an_unset_cooldown_resolves_to_twenty_seconds() {
        let config = Config::new(PathBuf::from("/tmp/a.iso"), PathBuf::from("/dev/loop9"));
        let resolved = config.resolved_cooldown_secs();
        assert_eq!(resolved, 20);
        assert_eq!(validate_cooldown_secs(resolved).unwrap(), 20, "the default must be in bounds");
    }

    /// An explicit value wins over the default.
    #[test]
    fn an_explicit_cooldown_is_used_verbatim() {
        let mut config = Config::new(PathBuf::from("/tmp/a.iso"), PathBuf::from("/dev/loop9"));
        config.cooldown_secs = Some(45);
        assert_eq!(config.resolved_cooldown_secs(), 45);
    }
}
