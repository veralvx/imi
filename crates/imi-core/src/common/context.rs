//! Values produced by one phase and consumed by later ones.
//!
//! [`Target`] is what Phase 0 establishes; [`FlashOutcome`] is what
//! Phase 4 reports. Both live here rather than in the phase that
//! produces them, for the same reason `TargetDevts` lives in
//! `common::mount`: a type named in two phases' signatures is shared,
//! and the crate's rule is that shared things live in `common` so
//! phases never import from each other. Passing these structs also
//! keeps the per-phase signatures short and makes the data flow
//! explicit.

use std::path::{Path, PathBuf};

use crate::common::identity::DeviceIdentity;
use crate::common::image::Compression;

/// Everything Phase 0 established about the image and the target.
///
/// Produced by `phases::phase_0::run` and threaded through the later
/// phases. Fields are private and read through getters; the type is
/// additionally `#[non_exhaustive]`.
///
/// Both restrictions are load-bearing rather than cosmetic, and they
/// close different halves of the same hole. `dev_size` is the bound
/// every Phase 4 write is checked against, and `dev_canon`/`identity`
/// describe the device the guard is holding. A forged `Target` could
/// pair a real `FlashGuard` for one device with a `dev_size` for
/// another, defeating the capacity check that keeps the write inside
/// the device.
///
/// `#[non_exhaustive]` stops a caller *constructing* one with a struct
/// literal. Private fields stop them rewriting `dev_size` on the value
/// `phase_0::run` returned before handing it to Phase 4 — the same
/// forgery in a different shape, which `#[non_exhaustive]` alone does
/// not prevent. Together they make the combination unrepresentable
/// rather than merely discouraged.
///
/// # Examples
///
/// ```no_run
/// # use std::path::PathBuf;
/// # let config = imi_core::Config::new(PathBuf::from("a.iso"), PathBuf::from("/dev/sdc"));
/// // A `Target` can only come from Phase 0, so holding one means the
/// // image and device were validated against each other.
/// let target = imi_core::phases::phase_0::run(&config, &mut ())?;
/// println!("{} -> {}", target.image_path().display(), target.device_path().display());
/// println!("{} bytes, format {}", target.device_size(), target.compression());
/// if let Some(raw) = target.raw_image_size() {
///     println!("decompresses to {raw} bytes");
/// }
/// # Ok::<(), imi_core::Error>(())
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Target {
    /// Canonicalised path to the source image.
    pub(crate) img_canon: PathBuf,
    /// Canonicalised path to the target block device.
    pub(crate) dev_canon: PathBuf,
    /// Kernel name of the target whole-disk device (e.g. `sdc`).
    pub(crate) dev_kname: String,
    /// Compression detected from the image's magic bytes.
    pub(crate) comp: Compression,
    /// Size of a raw image in bytes; `None` for compressed input.
    pub(crate) raw_size: Option<u64>,
    /// Size of the target device in bytes, from `BLKGETSIZE64`.
    pub(crate) dev_size: u64,
    /// Identity snapshot re-verified under the `O_EXCL` claim.
    pub(crate) identity: DeviceIdentity,
}

impl Target {
    /// Build a minimal target naming `dev`, for tests in other modules.
    ///
    /// Only the device path is meaningful; the rest is filler. Not
    /// available outside test builds — in production a `Target` may only
    /// come from Phase 0, so it always describes a device that was
    /// actually probed.
    #[cfg(test)]
    pub(crate) fn for_test(dev: PathBuf) -> Self {
        Self {
            img_canon: PathBuf::from("/tmp/test.img"),
            dev_canon: dev,
            dev_kname: "test0".to_owned(),
            comp: Compression::Raw,
            raw_size: None,
            dev_size: 0,
            identity: DeviceIdentity::for_test(0, None, 0),
        }
    }

    /// Canonicalised path to the source image.
    #[must_use]
    pub fn image_path(&self) -> &Path {
        &self.img_canon
    }

    /// Canonicalised path to the target block device.
    #[must_use]
    pub fn device_path(&self) -> &Path {
        &self.dev_canon
    }

    /// Kernel name of the target whole-disk device, e.g. `sdc`.
    #[must_use]
    pub fn device_kname(&self) -> &str {
        &self.dev_kname
    }

    /// Compression detected from the image's magic bytes.
    #[must_use]
    pub fn compression(&self) -> Compression {
        self.comp
    }

    /// Size of a raw image in bytes; `None` for compressed input, whose
    /// decompressed length is not known before the write.
    #[must_use]
    pub fn raw_image_size(&self) -> Option<u64> {
        self.raw_size
    }

    /// Size of the target device in bytes, from `BLKGETSIZE64`.
    ///
    /// This is the bound every Phase 4 write is checked against.
    #[must_use]
    pub fn device_size(&self) -> u64 {
        self.dev_size
    }

    /// Identity snapshot Phase 2 re-verifies under the `O_EXCL` claim.
    ///
    /// [`DeviceIdentity`] is opaque — it exposes no accessors — but it
    /// derives `PartialEq` and `Clone`, so a caller can snapshot one and
    /// compare it later to ask "is this still the same device?" without
    /// needing to know which attributes the answer rests on. That set has
    /// already grown once, from rdev/model/size to include the sysfs
    /// serial and WWID, without breaking any caller.
    #[must_use]
    pub fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }
}

/// Result of a completed write pass.
///
/// Marked `#[non_exhaustive]`: this is a return-only value, so
/// blocking external construction costs callers nothing and lets
/// later releases report more about a completed write.
///
/// # Examples
///
/// ```no_run
/// # use std::sync::atomic::AtomicBool;
/// # use std::path::PathBuf;
/// # let config = imi_core::Config::new(PathBuf::from("a.iso"), PathBuf::from("/dev/sdc"));
/// # let target = imi_core::phases::phase_0::run(&config, &mut ())?;
/// # let devts = imi_core::phases::phase_1::run(&target, &mut ())?;
/// # let session = imi_core::phases::phase_2::run(target, &devts, &mut ())?;
/// # let cancel = AtomicBool::new(false);
/// # let mut session = imi_core::phases::phase_3::run(session, &cancel, &mut ())?;
/// let outcome = imi_core::phases::phase_4::run(&mut session, None, &cancel, &mut ())?;
/// println!("wrote {} bytes", outcome.bytes_written());
/// # Ok::<(), imi_core::Error>(())
/// ```
///
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct FlashOutcome {
    /// Exact number of bytes written to the device.
    pub(crate) bytes_written: u64,
}

impl FlashOutcome {
    /// Exact number of bytes written to the device.
    ///
    /// Phase 5b verifies precisely this many bytes. The field is private
    /// so the figure can only come from a completed flash: a smaller
    /// value passed by hand would verify part of the device and still
    /// report success, which is the failure a verification step exists
    /// to prevent.
    #[must_use]
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{FlashOutcome, Target};
    use crate::common::identity::DeviceIdentity;
    use crate::common::image::Compression;

    /// A target whose every field holds a distinct, recognisable value,
    /// so a getter wired to the wrong field cannot pass.
    fn sample() -> Target {
        Target {
            img_canon: PathBuf::from("/tmp/source.iso"),
            dev_canon: PathBuf::from("/dev/loop9"),
            dev_kname: "loop9".to_owned(),
            comp: Compression::Zstd,
            raw_size: Some(4096),
            dev_size: 0x0080_0000,
            identity: DeviceIdentity::for_test(0x0700_0009, Some("SanDisk"), 0x0080_0000),
        }
    }

    /// Each getter must return its own field.
    ///
    /// These are one-line accessors, which is exactly why they need
    /// executing rather than merely type-checking: a getter returning a
    /// neighbouring field, or a constant, compiles perfectly and would
    /// hand a later phase the wrong device size.
    #[test]
    fn every_getter_returns_its_own_field() {
        let t = sample();
        assert_eq!(t.image_path(), PathBuf::from("/tmp/source.iso"));
        assert_eq!(t.device_path(), PathBuf::from("/dev/loop9"));
        assert_eq!(t.device_kname(), "loop9");
        assert_eq!(t.compression(), Compression::Zstd);
        assert_eq!(t.raw_image_size(), Some(4096));
        assert_eq!(t.device_size(), 0x0080_0000);
        assert_eq!(
            *t.identity(),
            DeviceIdentity::for_test(0x0700_0009, Some("SanDisk"), 0x0080_0000)
        );
    }

    /// The image and device paths must not be confused with each other:
    /// both are `PathBuf`, so a swap type-checks silently.
    #[test]
    fn image_and_device_paths_are_not_swapped() {
        let t = sample();
        assert_ne!(t.image_path(), t.device_path());
        assert!(t.image_path().to_string_lossy().contains("source.iso"));
        assert!(t.device_path().to_string_lossy().contains("loop9"));
    }

    /// Compressed input reports no raw size; the two fields are both
    /// size-shaped and must not be crossed.
    #[test]
    fn raw_size_is_distinct_from_device_size() {
        let t = sample();
        assert_ne!(t.raw_image_size(), Some(t.device_size()));

        let compressed = Target { raw_size: None, ..sample() };
        assert_eq!(compressed.raw_image_size(), None);
        assert_eq!(compressed.device_size(), 0x0080_0000, "device size is unaffected");
    }
    /// `FlashOutcome::bytes_written` must report the count it holds.
    ///
    /// A one-line getter, which is why it needs executing rather than
    /// merely compiling: Phase 5b verifies exactly this many bytes, so a
    /// getter returning a constant would silently shorten the read-back
    /// and still report success.
    #[test]
    fn flash_outcome_reports_the_byte_count_it_holds() {
        assert_eq!(FlashOutcome { bytes_written: 0 }.bytes_written(), 0);
        assert_eq!(FlashOutcome { bytes_written: 1 }.bytes_written(), 1);
        assert_eq!(FlashOutcome { bytes_written: 0x0080_0000 }.bytes_written(), 0x0080_0000);
        assert_eq!(FlashOutcome { bytes_written: u64::MAX }.bytes_written(), u64::MAX);
    }
}
