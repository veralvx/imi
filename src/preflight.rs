//! Phase 0 — pre-flight validation of the image and the target device.
//!
//! Everything here runs BEFORE any lock is taken and before any byte is
//! written: these are the refusals that keep a mistyped `--dev` from
//! becoming a destroyed disk. The family covers root privileges, image
//! regular-file-ness, block-device-ness, whole-disk-ness (partitions and
//! dm/md/zram nodes refused), the image-on-target self-reference trap
//! (flashing a loop device from its own backing file), read-only
//! geometry capture, and the [`DeviceIdentity`] snapshot re-verified
//! after the `O_EXCL` open closes the replug TOCTOU window.
//!
//! Design rationale per check: `.agents/docs/01-phase-0-preflight.md`.
//! `run()` in `main.rs` is the arbiter for call order.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::guard::FlashGuard;
use crate::ioctl;
use crate::mount;
use crate::sysfs;

/// Refuse to run without root: every later phase needs raw block access.
pub(crate) fn phase0_root_check() -> Result<()> {
    if !nix::unistd::Uid::effective().is_root() {
        bail!("imi must be run as root (try: sudo imi ...)");
    }
    Ok(())
}

/// Abort unless the image is a regular file.
///
/// This is the image-side counterpart of the device-identity chain: the
/// `-i` argument is opened and streamed wholesale in Phase 4, so a typo
/// that lands on a device node (`-i /dev/zero` streams zeros until the
/// capacity check trips — after the target has been overwritten), a
/// FIFO (blocks forever *before* the confirmation prompt), or a
/// directory must be refused here, with a clear message, before any
/// prompt or destructive step.
pub(crate) fn ensure_image_is_regular_file(path: &Path) -> Result<()> {
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if !meta.is_file() {
        bail!(
            "image {} is not a regular file (found {}). imi streams the image \
             from a plain file; if you meant to clone a device or pipe data, \
             copy the content to a file first.",
            path.display(),
            file_type_name(meta.file_type())
        );
    }
    Ok(())
}

/// Human-readable name for a file type, for the image-validation error.
fn file_type_name(ft: std::fs::FileType) -> &'static str {
    use std::os::unix::fs::FileTypeExt;
    if ft.is_dir() {
        "a directory"
    } else if ft.is_symlink() {
        "a symlink"
    } else if ft.is_block_device() {
        "a block device"
    } else if ft.is_char_device() {
        "a character device"
    } else if ft.is_fifo() {
        "a FIFO/pipe"
    } else if ft.is_socket() {
        "a socket"
    } else {
        "a non-regular file"
    }
}

/// Abort unless `path` is an actual block device node.
pub(crate) fn ensure_block_device(path: &Path) -> Result<()> {
    let st = nix::sys::stat::stat(path).with_context(|| format!("stat {}", path.display()))?;
    let file_type = st.st_mode & libc::S_IFMT;
    if file_type != libc::S_IFBLK {
        bail!("{} is not a block device (st_mode=0o{:o}); aborting", path.display(), st.st_mode);
    }
    Ok(())
}

/// Reject partitions. A partition exposes `/sys/class/block/<kname>/partition`;
/// a whole disk does not.
///
/// Additionally reject *virtual* whole-node block devices — `dm-*`
/// (an open LUKS mapping!), `md*`, `zram*`, `ram*` — which are "not a
/// partition" yet are exactly the wrong thing to point a USB flasher at:
/// an unmounted-but-open dm-crypt view carries no kernel claim, so
/// `O_EXCL` would succeed and the flash would destroy the encrypted
/// volume's contents. Physical disks expose a `device` link in sysfs;
/// loop devices (the sanctioned test target) are allow-listed by name.
pub(crate) fn ensure_whole_disk(path: &Path) -> Result<()> {
    let kname = sysfs::kname_for_path(path)
        .with_context(|| format!("resolving kernel name for {}", path.display()))?;
    if sysfs::is_partition(&kname) {
        bail!(
            "{} is a partition ({}), not a whole disk. \
             Pass the base device (e.g. /dev/sdb, not /dev/sdb1).",
            path.display(),
            kname
        );
    }
    if !sysfs::exists(&kname) {
        bail!(
            "{} does not appear in /sys/class/block/{}; refusing to flash an unknown device",
            path.display(),
            kname
        );
    }
    if !(kname.starts_with("loop") || sysfs::has_backing_device(&kname)) {
        bail!(
            "{} ({kname}) is a virtual/stacked block device (device-mapper, md, \
             zram, …), not a physical disk. imi targets physical disks and loop \
             devices; flash the underlying disk, or tear the stack down first.",
            path.display()
        );
    }
    Ok(())
}

/// Image-on-target ancestry check.
///
/// Two independent hazards are refused here:
///
/// 1. **Image stored on the target's block stack** — resolved via the
///    kernel name of the block device backing the image's filesystem,
///    checked against the target's block subtree.
/// 2. **Image *is* a loop member's backing file** — a loop target reads
///    its content from its backing file, so flashing a loop device from
///    its own backing file is self-referential: the Phase 3 wipe zeroes
///    the head of the very stream Phase 4 is about to read, and Phase 5
///    then compares the mutated file against itself — printing SUCCESS
///    over corrupted content. Matched by `(st_dev, st_ino)` identity so
///    hardlinks and alternate paths to the same file are also caught.
///    This check runs for *every* loop member of the subtree and does
///    not depend on the image's own filesystem being block-backed —
///    which hazard 1's resolution does.
pub(crate) fn ensure_image_not_on_target(img: &Path, dev_kname: &str) -> Result<()> {
    let img_meta = std::fs::metadata(img).with_context(|| format!("stat {}", img.display()))?;
    let img_id = (img_meta.dev(), img_meta.ino());

    let subtree = target_block_subtree(dev_kname)?;

    for member in &subtree {
        let Some(backing) = sysfs::loop_backing_file(member) else { continue };
        // A vanished or unreadable backing file cannot be the image we
        // just stat'ed successfully; skip rather than abort.
        let Ok(back_meta) = std::fs::metadata(&backing) else { continue };
        if (back_meta.dev(), back_meta.ino()) == img_id {
            bail!(
                "image {} is the backing file of loop device '{member}', which is \
                 within the target's block stack ({dev_kname}). Flashing a loop \
                 device from its own backing file corrupts the stream while it is \
                 being read; copy the image to a different file first.",
                img.display()
            );
        }
    }

    let Some(img_kname) = block_kname_backing_path(img)
        .with_context(|| format!("resolving block device backing {}", img.display()))?
    else {
        // Image lives on tmpfs / FUSE / NFS / another non-block backend
        // that neither st_dev nor the mount table can tie to a block
        // device; hazard 1 has nothing to protect against (hazard 2 was
        // already checked above, independent of this resolution).
        return Ok(());
    };

    if subtree.contains(&img_kname) {
        bail!(
            "image {} is on block device '{img_kname}', which is within the target's \
             block stack ({dev_kname}). Copy the image to a different filesystem first.",
            img.display()
        );
    }
    Ok(())
}

/// Resolve the kernel name of the block device backing the filesystem
/// that contains `path`.
///
/// Primary route: `st_dev` of the file → `/sys/dev/block/<maj>:<min>`.
/// That fails for filesystems with *anonymous* device numbers — btrfs
/// reports a synthetic per-subvolume `st_dev` that has no sysfs entry —
/// so the fallback walks `/proc/self/mountinfo` for the deepest mount
/// containing `path` and stats its source. Only when both routes come
/// up empty do we conclude "non-block backend" (`None`). Without the
/// fallback, an image on a btrfs partition *of the target* would skip
/// this check silently, get auto-unmounted in Phase 1, and Phase 4
/// would fail to open it *after* the signature wipe.
fn block_kname_backing_path(path: &Path) -> Result<Option<String>> {
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let dev_num = meta.dev();
    let maj = nix::sys::stat::major(dev_num);
    let min = nix::sys::stat::minor(dev_num);

    if let Ok(kname) = sysfs::kname_for_devt(maj, min) {
        return Ok(Some(kname));
    }

    let Some(source) = mount::backing_source_for_path(path)? else {
        return Ok(None);
    };
    let Some((smaj, smin)) = mount::block_rdev_of(&source) else {
        return Ok(None); // pseudo-source (tmpfs, overlay, network fs)
    };
    Ok(sysfs::kname_for_devt(smaj, smin).ok())
}

/// Build the target's block subtree: the disk, its partitions, every
/// recursive holder of any member, and — iterated to a fixpoint — every
/// loop device whose backing file lives on a member (plus that loop's
/// own partitions and holders). Loop-over-target matters because the
/// loop's content *is* target content: an image stored inside a
/// loop-mounted container on the stick is still on the stick.
fn target_block_subtree(dev_kname: &str) -> Result<BTreeSet<String>> {
    // BTreeSet: ordered iteration makes the fixpoint scan and any error
    // it surfaces deterministic run-to-run (HashSet order is not), and
    // membership stays O(log n) over a set of at most a few dozen knames.
    let mut subtree: BTreeSet<String> = BTreeSet::new();
    subtree.insert(dev_kname.to_owned());
    for p in sysfs::partitions_of(dev_kname)? {
        subtree.insert(p);
    }

    loop {
        let mut grew = false;

        let snapshot: Vec<String> = subtree.iter().cloned().collect();
        for member in &snapshot {
            let mut hs: Vec<String> = sysfs::holders_recursive(member)?.into_iter().collect();
            hs.sort_unstable();
            for h in hs {
                grew |= subtree.insert(h);
            }
        }

        // Loop devices backed by files on any subtree member. Failures
        // on individual loop nodes (detached mid-scan, backing file
        // vanished) skip that node rather than aborting: this scan is a
        // peripheral widening pass, and a vanished unrelated loop must
        // not block a valid flash.
        for lk in sysfs::all_block_knames()? {
            if !lk.starts_with("loop") || subtree.contains(&lk) {
                continue;
            }
            let Some(backing) = sysfs::loop_backing_file(&lk) else { continue };
            let Ok(Some(backing_kname)) = block_kname_backing_path(&backing) else {
                continue;
            };
            if subtree.contains(&backing_kname) {
                grew |= subtree.insert(lk.clone());
                if let Ok(parts) = sysfs::partitions_of(&lk) {
                    for p in parts {
                        grew |= subtree.insert(p);
                    }
                }
            }
        }

        if !grew {
            break;
        }
    }
    Ok(subtree)
}

/// Query device size (`BLKGETSIZE64`) and write-protect state (`BLKROGET`)
/// via a read-only FD (no `O_EXCL`, no destructive side effects).
///
/// The RO check exists so a hardware write-protect switch (or
/// `blockdev --setro`) is refused *here*, in Phase 0 — not discovered as
/// an `EPERM` at the Phase 3 wipe with the guard already armed, which
/// would print the "device is in an inconsistent state" FATAL warning
/// for a device that was never touched.
pub(crate) fn query_dev_geometry_readonly(path: &Path) -> Result<(u64, bool)> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("open {} (read-only)", path.display()))?;
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&f);
    let mut size: u64 = 0;
    // SAFETY: `f` is a valid, open FD. BLKGETSIZE64 writes a u64 through
    // the pointer; `&raw mut size` is a valid, aligned, non-null pointer
    // to a live local that outlives the call.
    unsafe {
        ioctl::blkgetsize64(fd, &raw mut size).context("BLKGETSIZE64")?;
    }
    let mut ro: libc::c_int = 0;
    // SAFETY: same FD validity as above. BLKROGET writes a c_int through
    // the pointer; `&raw mut ro` is a valid, aligned, non-null pointer
    // to a live local that outlives the call.
    unsafe {
        ioctl::blkroget(fd, &raw mut ro).context("BLKROGET (write-protect state)")?;
    }
    Ok((size, ro != 0))
}

/// Device identity captured at Phase 0, re-verified under the Phase 2
/// `O_EXCL` claim. Closes the replug TOCTOU window between the operator
/// confirming the prompt and the exclusive open: same `/dev` name, but
/// a different physical device.
pub(crate) struct DeviceIdentity {
    /// `st_rdev` of the device node at capture time.
    rdev: libc::dev_t,
    /// Sysfs model string at capture time (None when the device exposes
    /// no model — the re-check then requires it to still be None).
    model: Option<String>,
    /// `BLKGETSIZE64` at capture time.
    size: u64,
}

impl DeviceIdentity {
    /// Snapshot rdev + model + size for the device at `dev` (Phase 0).
    pub(crate) fn capture(dev: &Path, dev_kname: &str, size: u64) -> Result<Self> {
        let st = nix::sys::stat::stat(dev)
            .with_context(|| format!("stat {} for identity snapshot", dev.display()))?;
        Ok(Self { rdev: st.st_rdev, model: sysfs::device_model(dev_kname), size })
    }

    /// Verify the *claimed FD* (not the path — the path could have been
    /// re-bound to a new device) still refers to the confirmed device:
    /// same `st_rdev`, same sysfs model, same `BLKGETSIZE64`.
    pub(crate) fn verify_claimed(&self, guard: &FlashGuard, dev_kname: &str) -> Result<()> {
        let st = nix::sys::stat::fstat(guard.file()).context("fstat of claimed device FD")?;
        if st.st_rdev != self.rdev {
            bail!(
                "device number changed between confirmation and O_EXCL claim \
                 (was {}:{}, now {}:{}). Device replugged? Re-run from scratch.",
                nix::sys::stat::major(self.rdev),
                nix::sys::stat::minor(self.rdev),
                nix::sys::stat::major(st.st_rdev),
                nix::sys::stat::minor(st.st_rdev)
            );
        }
        let model_now = sysfs::device_model(dev_kname);
        if model_now != self.model {
            bail!(
                "device model changed between confirmation and O_EXCL claim \
                 (was {:?}, now {:?}). A different device was plugged in under \
                 the same name. Re-run from scratch.",
                self.model,
                model_now
            );
        }
        let mut size_now: u64 = 0;
        // SAFETY: `guard` owns a valid, currently-open O_EXCL FD for the
        // block device. BLKGETSIZE64 writes a u64 through the pointer;
        // `&raw mut size_now` is a valid, aligned, non-null pointer to a
        // live local that outlives the call.
        unsafe {
            ioctl::blkgetsize64(guard.as_raw_fd(), &raw mut size_now)
                .context("BLKGETSIZE64 (identity re-check)")?;
        }
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
    use super::{ensure_image_is_regular_file, file_type_name};
    use std::path::PathBuf;

    /// Regular files pass — the overwhelmingly common case.
    #[test]
    fn image_check_accepts_regular_file() {
        let p = std::env::temp_dir().join(format!("imi-img-check-{}", std::process::id()));
        std::fs::write(&p, b"not really an iso").unwrap();
        ensure_image_is_regular_file(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
    }

    /// Directories are refused with a message naming the actual type
    /// and the path — the operator can act on it immediately.
    #[test]
    fn image_check_rejects_directory() {
        let d = std::env::temp_dir();
        let err = ensure_image_is_regular_file(&d).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not a regular file"), "got: {msg}");
        assert!(msg.contains("a directory"), "got: {msg}");
    }

    /// A vanished path surfaces the stat context (which path failed),
    /// not a bare ENOENT.
    #[test]
    fn image_check_names_path_on_missing_file() {
        let p = PathBuf::from("/nonexistent/imi-no-such-image.iso");
        let err = ensure_image_is_regular_file(&p).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("imi-no-such-image.iso"), "got: {msg}");
    }

    /// The type-namer covers the common misfires distinctly; `metadata`
    /// follows symlinks, so the symlink arm is unreachable from
    /// `ensure_image_is_regular_file` but kept for completeness.
    #[test]
    fn file_type_name_labels_dir_and_file_types() {
        let d = std::fs::metadata(std::env::temp_dir()).unwrap();
        assert_eq!(file_type_name(d.file_type()), "a directory");
    }
}
