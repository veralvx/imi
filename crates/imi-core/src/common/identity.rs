//! The device-identity snapshot that closes the replug TOCTOU window.
//!
//! Phase 0 captures the identity the operator confirms; Phase 2
//! re-verifies it against the `O_EXCL`-claimed FD. Because the two
//! halves live in different phases, the type lives here.

use std::path::Path;

use crate::Result;
use crate::common::guard::FlashGuard;
use crate::common::ioctl;
use crate::common::sysfs;
use crate::error::{Context as _, bail};

/// Device identity captured at Phase 0, re-verified under the Phase 2
/// `O_EXCL` claim. Closes the replug TOCTOU window between the operator
/// confirming the prompt and the exclusive open: same `/dev` name, but
/// a different physical device.
///
/// # Examples
///
/// The type exposes no accessors on purpose — which attributes make up
/// an identity is an implementation detail, and the set has already
/// grown once. What it offers instead is equality: snapshot one and
/// compare it later to ask whether the device is still the one you
/// checked.
///
/// ```no_run
/// # use std::path::PathBuf;
/// # use std::sync::atomic::AtomicBool;
/// # let mut config = imi_core::Config::new(PathBuf::from("a.iso"), PathBuf::from("/dev/sdc"));
/// # config.yes = true;
/// let before = imi_core::phases::phase_0::run(&config, &mut ())?.identity().clone();
///
/// // ... time passes, and the operator may have swapped the stick ...
///
/// let now = imi_core::phases::phase_0::run(&config, &mut ())?;
/// if &before != now.identity() {
///     eprintln!("that is a different device");
/// }
/// # Ok::<(), imi_core::Error>(())
/// ```
///
/// Phase 2 does exactly this internally, against the snapshot Phase 0
/// took before the operator was asked to confirm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIdentity {
    /// `st_rdev` of the device node at capture time.
    rdev: libc::dev_t,
    /// Sysfs model string at capture time (None when the device exposes
    /// no model — the re-check then requires it to still be None).
    model: Option<String>,
    /// `BLKGETSIZE64` at capture time.
    size: u64,
    /// Sysfs WWID at capture time, when the device exposes one.
    ///
    /// The kernel serves this from VPD page 0x83, the Device
    /// Identification page (`sdev_show_wwid` -> `scsi_vpd_lun_id` in
    /// `drivers/scsi/scsi_sysfs.c`), which is designed to be globally
    /// unique. `serial` comes from page 0x80, which is vendor-defined —
    /// and cheap USB sticks are widely shipped with blank or duplicated
    /// unit serials, so page 0x80 alone would not reliably close the
    /// identical-stick gap it was added for.
    ///
    /// Either field differing means a different device. Both absent
    /// means no strengthening and no weakening.
    wwid: Option<String>,
    /// Sysfs serial at capture time, when the device exposes one.
    ///
    /// The other three fields cannot tell two identical devices apart:
    /// `model` and `size` are equal for the same make and capacity, and
    /// `rdev` is whatever minor the kernel happened to reassign. Swap one
    /// 32 GB stick for another of the same make and capacity that lands
    /// on the same minor and the gate opens. A serial closes that, where
    /// the device offers one.
    ///
    /// `None` when it does not — loop devices, device-mapper, and much
    /// real hardware — and `None` must then still be `None` at re-check,
    /// exactly like `model`. A device that gains or loses a serial has
    /// changed.
    serial: Option<String>,
}

impl DeviceIdentity {
    /// Build an identity directly, for tests in other modules that need
    /// a complete [`crate::Target`]. Not available outside test builds:
    /// in production an identity may only come from [`Self::capture`],
    /// so it always reflects a device that was actually inspected.
    #[cfg(test)]
    pub(crate) fn for_test(rdev: libc::dev_t, model: Option<&str>, size: u64) -> Self {
        Self { rdev, model: model.map(str::to_owned), size, serial: None, wwid: None }
    }

    /// [`Self::for_test`] with a serial, for the serial comparison.
    #[cfg(test)]
    pub(crate) fn for_test_with_serial(
        rdev: libc::dev_t,
        model: Option<&str>,
        size: u64,
        serial: Option<&str>,
    ) -> Self {
        Self {
            rdev,
            model: model.map(str::to_owned),
            size,
            serial: serial.map(str::to_owned),
            wwid: None,
        }
    }

    /// Snapshot rdev + model + size for the device at `dev` (Phase 0).
    pub(crate) fn capture(dev: &Path, dev_kname: &str, size: u64) -> Result<Self> {
        let st = nix::sys::stat::stat(dev)
            .with_context(|| format!("stat {} for identity snapshot", dev.display()))?;
        Ok(Self {
            rdev: st.st_rdev,
            model: sysfs::device_model(dev_kname),
            size,
            serial: sysfs::device_serial(dev_kname),
            wwid: sysfs::device_wwid(dev_kname),
        })
    }

    /// Verify the *claimed FD* (not the path — the path could have been
    /// re-bound to a new device) still refers to the confirmed device:
    /// same `st_rdev`, same sysfs model, same `BLKGETSIZE64`.
    pub(crate) fn verify_claimed(&self, guard: &FlashGuard, dev_kname: &str) -> Result<()> {
        // The three reads stay interleaved with their checks, and the
        // order is load-bearing: if the device was replugged, the fd may
        // refer to something gone, and BLKGETSIZE64 would then fail with
        // a bare ENODEV. Checking `rdev` first means the operator gets
        // "device replugged" rather than an ioctl error in exactly the
        // scenario this function exists to catch.
        let st = nix::sys::stat::fstat(guard.file()).context("fstat of claimed device FD")?;
        self.check_rdev(st.st_rdev)?;

        self.check_model(sysfs::device_model(dev_kname).as_deref())?;
        self.check_serial(sysfs::device_serial(dev_kname).as_deref())?;
        self.check_wwid(sysfs::device_wwid(dev_kname).as_deref())?;

        let mut size_now: u64 = 0;
        // Kept as an ioctl rather than `File::seek(SeekFrom::End(0))`,
        // which would give the same number without `unsafe`: this must
        // read the size of the device *the guard is holding*, through
        // the claimed descriptor. Seeking would move that descriptor's
        // offset, which Phase 4 and Phase 5 then write and read from.
        // See `phase_0::query_dev_geometry_readonly` for why the size
        // and write-protect ioctls are not interchangeable with their
        // sysfs-looking alternatives.
        //
        // SAFETY: `guard` owns a valid, currently-open O_EXCL FD for the
        // block device — the `&FlashGuard` borrow is what enforces that,
        // not merely what asserts it: the guard owns the `File`, so it
        // cannot have been closed while this reference exists.
        // BLKGETSIZE64 writes a `u64` through the pointer, and
        // `&raw mut size_now` is a valid, aligned, non-null pointer to a
        // live local that outlives the call.
        //
        // The block covers the call and nothing else. It used to enclose
        // the `.context()?` as well, which put an early return inside
        // `unsafe` and meant any line added after the ioctl would land
        // there unnoticed.
        let rc = unsafe { ioctl::blkgetsize64(guard.as_raw_fd(), &raw mut size_now) };
        rc.context("BLKGETSIZE64 (identity re-check)")?;

        self.check_size(size_now)
    }

    /// Compare the device number against the snapshot.
    ///
    /// Split out from [`Self::verify_claimed`] so the comparison is
    /// testable without a block device. It is the primary replug
    /// detector, and an inverted condition here would accept exactly the
    /// swapped device it exists to reject.
    ///
    /// # Errors
    ///
    /// Returns an error when the device number differs.
    fn check_rdev(&self, rdev_now: libc::dev_t) -> Result<()> {
        if rdev_now != self.rdev {
            bail!(
                "device number changed between confirmation and O_EXCL claim \
                 (was {}:{}, now {}:{}). Device replugged? Re-run from scratch.",
                nix::sys::stat::major(self.rdev),
                nix::sys::stat::minor(self.rdev),
                nix::sys::stat::major(rdev_now),
                nix::sys::stat::minor(rdev_now)
            );
        }
        Ok(())
    }

    /// Compare the sysfs model string against the snapshot.
    ///
    /// A device exposing no model must still expose none: `None` is a
    /// value to match, not a wildcard.
    ///
    /// # Errors
    ///
    /// Returns an error when the model differs.
    fn check_model(&self, model_now: Option<&str>) -> Result<()> {
        if model_now != self.model.as_deref() {
            bail!(
                "device model changed between confirmation and O_EXCL claim \
                 (was {:?}, now {:?}). A different device was plugged in under \
                 the same name. Re-run from scratch.",
                self.model,
                model_now
            );
        }
        Ok(())
    }

    /// Compare the sysfs serial against the snapshot.
    ///
    /// This is the only field that distinguishes two devices of the same
    /// make, model and capacity, so it is the one that closes the gap
    /// the other three leave. Like `model`, `None` is a value: a device
    /// that gained or lost a serial is not the device that was
    /// confirmed.
    ///
    /// # Errors
    ///
    /// Returns an error when the serial differs.
    fn check_serial(&self, serial_now: Option<&str>) -> Result<()> {
        if serial_now != self.serial.as_deref() {
            bail!(
                "device serial changed between confirmation and O_EXCL claim \
                 (was {:?}, now {:?}). A different device was plugged in under \
                 the same name. Re-run from scratch.",
                self.serial,
                serial_now
            );
        }
        Ok(())
    }

    /// Compare the sysfs WWID against the snapshot.
    ///
    /// Stronger than `serial` where both exist: VPD 0x83 is unique by
    /// design, VPD 0x80 is whatever the vendor wrote. Same
    /// `None`-is-a-value rule.
    ///
    /// # Errors
    ///
    /// Returns an error when the WWID differs.
    fn check_wwid(&self, wwid_now: Option<&str>) -> Result<()> {
        if wwid_now != self.wwid.as_deref() {
            bail!(
                "device WWID changed between confirmation and O_EXCL claim \
                 (was {:?}, now {:?}). A different device was plugged in under \
                 the same name. Re-run from scratch.",
                self.wwid,
                wwid_now
            );
        }
        Ok(())
    }

    /// Compare the device size against the snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the size differs.
    fn check_size(&self, size_now: u64) -> Result<()> {
        if size_now != self.size {
            bail!(
                "device size changed between confirmation and O_EXCL claim \
                 (was {} bytes, now {size_now} bytes). A different device was \
                 plugged in under the same name. Re-run from scratch.",
                self.size
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::DeviceIdentity;

    /// The replug detector must accept its own device and reject any
    /// other. An inverted condition here would accept exactly the
    /// swapped device the check exists to reject, so both directions are
    /// asserted.
    #[test]
    fn check_rdev_accepts_same_and_rejects_different() {
        let id = DeviceIdentity::for_test(0x0800_0010, Some("SanDisk"), 1024);
        id.check_rdev(0x0800_0010).expect("the same device number must pass");

        let err = id.check_rdev(0x0800_0020).unwrap_err();
        assert!(err.to_string().contains("device number changed"), "{err}");
        assert!(err.to_string().contains("replugged"), "{err}");
    }

    /// Model comparison, including the case where a device exposes no
    /// model: `None` is a value that must match, not a wildcard.
    #[test]
    fn check_model_treats_none_as_a_value() {
        let named = DeviceIdentity::for_test(1, Some("SanDisk"), 1024);
        named.check_model(Some("SanDisk")).expect("identical model must pass");
        assert!(named.check_model(Some("Kingston")).is_err(), "a different model must fail");
        assert!(named.check_model(None).is_err(), "losing the model must fail");

        let anonymous = DeviceIdentity::for_test(1, None, 1024);
        anonymous.check_model(None).expect("no-model must still match no-model");
        assert!(anonymous.check_model(Some("SanDisk")).is_err(), "gaining a model must fail");
    }

    /// The serial is what tells two identical devices apart.
    ///
    /// `rdev`, `model` and `size` all match when one 32 GB stick is
    /// swapped for another of the same make and capacity on the same minor —
    /// which is the realistic version of the attack this whole type
    /// exists to stop, since an operator with two identical sticks is a
    /// far commoner situation than one with two different ones.
    #[test]
    fn check_serial_distinguishes_otherwise_identical_devices() {
        let id = DeviceIdentity::for_test_with_serial(
            0x0800_0010,
            Some("SanDisk Ultra"),
            32 * 1024 * 1024 * 1024,
            Some("4C530001260501117433"),
        );

        // Everything else about the impostor matches.
        id.check_rdev(0x0800_0010).expect("same minor");
        id.check_model(Some("SanDisk Ultra")).expect("same model");
        id.check_size(32 * 1024 * 1024 * 1024).expect("same size");

        // Only the serial catches it.
        id.check_serial(Some("4C530001260501117433")).expect("its own serial must pass");
        let err = id.check_serial(Some("4C530001260501199999")).unwrap_err();
        assert!(err.to_string().contains("device serial changed"), "{err}");

        // WWID is the stronger of the two where both exist: VPD 0x83 is
        // unique by design, VPD 0x80 is whatever the vendor wrote, and
        // cheap sticks ship duplicated unit serials. A device matching
        // on serial must still be rejected on WWID.
        let dual = DeviceIdentity {
            rdev: 0x0800_0010,
            model: Some("SanDisk Ultra".to_owned()),
            size: 32 * 1024 * 1024 * 1024,
            serial: Some("DUPLICATED".to_owned()),
            wwid: Some("naa.5001b448b94f9b21".to_owned()),
        };
        dual.check_serial(Some("DUPLICATED")).expect("a duplicated serial still matches");
        let wwid_err = dual.check_wwid(Some("naa.5001b448b94f9b99")).unwrap_err();
        assert!(wwid_err.to_string().contains("device WWID changed"), "{wwid_err}");
        dual.check_wwid(Some("naa.5001b448b94f9b21")).expect("its own WWID must pass");
        assert!(dual.check_wwid(None).is_err(), "losing the WWID must fail");

        // `None` is a value, exactly as for `model`.
        assert!(id.check_serial(None).is_err(), "losing the serial must fail");
        let anonymous = DeviceIdentity::for_test(1, Some("Generic"), 1024);
        anonymous.check_serial(None).expect("no-serial must still match no-serial");
        assert!(anonymous.check_serial(Some("X")).is_err(), "gaining a serial must fail");
    }

    /// Size comparison catches a same-name device of a different
    /// capacity — the case a model string alone would miss.
    #[test]
    fn check_size_accepts_same_and_rejects_different() {
        let id = DeviceIdentity::for_test(1, Some("SanDisk"), 8 * 1024 * 1024 * 1024);
        id.check_size(8 * 1024 * 1024 * 1024).expect("identical size must pass");

        let err = id.check_size(16 * 1024 * 1024 * 1024).unwrap_err();
        assert!(err.to_string().contains("device size changed"), "{err}");
    }
}
