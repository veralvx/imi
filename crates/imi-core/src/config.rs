//! Runtime configuration for the pipeline.
//!
//! This is the core's own input type: it carries exactly what the phases
//! need and nothing about how a caller obtained it. The `imi` binary
//! builds one from its `clap` arguments, but any caller can construct
//! one directly.

use std::path::PathBuf;

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
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::Config;

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
    }
}
