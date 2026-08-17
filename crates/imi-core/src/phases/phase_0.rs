//! Phase 0 — pre-flight validation.
//!
//! Validates the image and the target, then asks the operator to
//! confirm. Silent on success; every failure here leaves the device
//! completely untouched. [`run`] is the arbiter for call order.
//!
//! Everything here runs BEFORE any lock is taken and before any byte is
//! written: these are the refusals that keep a mistyped `--dev` from
//! becoming a destroyed disk. The family covers root privileges, image
//! regular-file-ness, block-device-ness, whole-disk-ness (partitions and
//! dm/md/zram nodes refused), the image-on-target self-reference trap
//! (flashing a loop device from its own backing file), write-protected
//! media, and the read-only geometry capture the later phases rely on.
//!
//! The [`DeviceIdentity`] snapshot that closes
//! the replug TOCTOU window is captured from this phase but lives in
//! `common/`, because Phase 2 is what re-verifies it under the `O_EXCL`
//! claim.
//!
//! Design rationale per check: `.agents/docs/01-phase-0-preflight.md`.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use crate::Result;
use crate::common::context::Target;
use crate::common::geometry::WIPE_REGION;
use crate::common::identity::DeviceIdentity;
use crate::common::{image, ioctl, mount, sysfs};
use crate::config::Config;
use crate::error::{Context as _, bail};
use crate::events::{Events, Phase as UiPhase, PhaseOutcome, Summary};

/// Run every Phase 0 check in order and return the validated [`Target`].
///
/// Phases are ordered: this one assumes every earlier phase succeeded.
///
/// # Errors
///
/// Returns an error if the process is not root, the image or device
/// paths do not resolve, the target is not a whole disk, the image is
/// the target's own backing file, the device is write-protected or too
/// small, a raw image is larger than the device, or the operator
/// declines at the prompt. On any error the device is untouched.
pub fn run<E: Events + ?Sized>(config: &Config, events: &mut E) -> Result<Target> {
    let outcome = run_inner(config, events).map_err(|e| e.at(crate::DeviceState::Untouched));
    events.phase_finished(
        UiPhase::Preflight,
        if outcome.is_ok() { PhaseOutcome::Completed } else { PhaseOutcome::Failed },
    );
    outcome
}

/// The phase proper: where `?` and `.context("Phase 0: ...")` chain to
/// build the message the operator reads.
///
/// Split from [`run`] so that every exit path passes through one place
/// that maps the device state and emits `phase_finished`.
fn run_inner<E: Events + ?Sized>(config: &Config, events: &mut E) -> Result<Target> {
    events.phase_started(UiPhase::Preflight);
    phase0_root_check()?;

    let img_canon = std::fs::canonicalize(&config.img)
        .with_context(|| format!("canonicalize image path {}", config.img.display()))?;
    let dev_canon = std::fs::canonicalize(&config.dev)
        .with_context(|| format!("canonicalize device path {}", config.dev.display()))?;

    ensure_distinct_paths(&img_canon, &dev_canon)?;

    ensure_image_is_regular_file(&img_canon)?;
    ensure_block_device(&dev_canon)?;
    ensure_whole_disk(&dev_canon)?;

    let dev_kname = sysfs::kname_for_path(&dev_canon)
        .with_context(|| format!("resolving kernel name for {}", dev_canon.display()))?;

    ensure_image_not_on_target(&img_canon, &dev_kname)?;

    let comp = image::detect_compression(&img_canon)
        .with_context(|| format!("detect compression format of {}", img_canon.display()))?;

    let raw_size: Option<u64> = if comp.is_compressed() {
        None
    } else {
        let meta = std::fs::metadata(&img_canon)
            .with_context(|| format!("stat {}", img_canon.display()))?;
        Some(meta.len())
    };

    let (dev_size, dev_read_only) = query_dev_geometry_readonly(&dev_canon)
        .with_context(|| format!("query size/RO state of {}", dev_canon.display()))?;

    if dev_read_only {
        bail!(
            "{} reports itself write-protected (BLKROGET). Check the physical \
             RO switch or `blockdev --setro` state; refusing before any \
             destructive step.",
            dev_canon.display()
        );
    }

    // Minimum-size floor, checked here for the same reason as BLKROGET
    // above: Phase 3's wipe re-checks this bound (defense in depth), but
    // by then the guard is armed, so a too-small device would earn the
    // "device is in an inconsistent state" FATAL warning despite never
    // having been touched. Refuse cleanly before the prompt instead.
    ensure_device_large_enough(dev_size)?;

    ensure_image_fits(raw_size, dev_size)?;

    // Snapshot the device identity the operator is about to confirm.
    // Phase 2 re-verifies this against the O_EXCL-claimed FD, closing the
    // replug TOCTOU: if the stick is yanked and a different one lands on
    // the same /dev name between the prompt and the claim, the mismatch
    // aborts before anything destructive.
    let identity = DeviceIdentity::capture(&dev_canon, &dev_kname, dev_size)?;

    let model = sysfs::device_model(&dev_kname);
    confirm_destruction(
        config,
        &Summary {
            device: &dev_canon,
            model: model.as_deref(),
            device_size: dev_size,
            image: &img_canon,
            compression: comp,
            raw_image_size: raw_size,
        },
        events,
    )?;

    Ok(Target { img_canon, dev_canon, dev_kname, comp, raw_size, dev_size, identity })
}
/// Obtain permission to destroy the device, or refuse.
///
/// Extracted from [`run`] because [`run`] needs a real block device to
/// reach, and this is the last gate before anything destructive. Two
/// negations live here and mutation testing kills both only if the
/// decision is reachable without hardware: dropping the first skips the
/// question on an unattended run, dropping the second destroys the
/// device when the operator answers *no* and aborts when they answer
/// *yes*.
///
/// Note that a root-gated test cannot substitute — `cargo mutants` runs
/// the suite without `--ignored`, so anything behind that flag is
/// invisible to it.
///
/// # Errors
///
/// Returns an error when the caller has not pre-approved via
/// [`Config::yes`] and the sink declines.
fn confirm_destruction<E: Events + ?Sized>(
    config: &Config,
    summary: &Summary<'_>,
    events: &mut E,
) -> Result<()> {
    if config.yes {
        return Ok(());
    }
    if events.confirm(summary) {
        return Ok(());
    }
    bail!("aborted by user")
}

/// Whether two `(device, inode)` pairs name the same file.
///
/// Identity by devt and inode rather than by path, so a symlink, a bind
/// mount or a `..` in the middle cannot disguise the image as a
/// different file from the loop device's backing store.
///
/// Extracted because the comparison is one `==` inside a loop over
/// sysfs entries, unreachable without a real block stack. Inverted, the
/// check fires on every file *except* the image — refusing unrelated
/// flashes while permitting the one that corrupts the stream as it is
/// read.
fn is_same_file(a: (u64, u64), b: (u64, u64)) -> bool {
    a == b
}

/// Refuse a virtual or stacked block device.
///
/// `imi` targets physical disks and loop devices. Device-mapper, md and
/// zram nodes expose no backing device in sysfs, and flashing one writes
/// into a mapping rather than onto a disk.
///
/// The loop test is a prefix, not `loop` followed by digits, and that is
/// only safe because of where it is called from. `losetup -P` creates
/// partition nodes named `loop0p1`, which this would accept — but
/// [`ensure_whole_disk`] runs `sysfs::is_partition` before reaching
/// here, so a partition is already refused by the time this is asked.
/// Moving this call earlier, or reusing it elsewhere, reintroduces that
/// hole. Tighten the test to `loop` + digits before doing either.
///
/// Extracted because both halves of the decision are mutable and neither
/// is reachable without sysfs: dropping the `!` accepts exactly the
/// stacked devices this exists to refuse, and `||` mutated to `&&`
/// refuses every loop device — a loop node has no backing entry, which
/// is precisely why its name is checked separately.
///
/// # Errors
///
/// Returns an error when `kname` is neither a loop device nor backed by
/// a physical device.
fn ensure_physical_or_loop(kname: &str, has_backing: bool, path: &Path) -> Result<()> {
    if kname.starts_with("loop") || has_backing {
        return Ok(());
    }
    bail!(
        "{} ({kname}) is a virtual/stacked block device (device-mapper, md, \
         zram, …), not a physical disk. imi targets physical disks and loop \
         devices; flash the underlying disk, or tear the stack down first.",
        path.display()
    )
}

/// Refuse when the image and the target resolve to the same path.
///
/// Extracted from [`run`] so the decision is unit-testable: `run` needs
/// a real block device to reach, which left these refusals — the ones
/// that keep a mistyped invocation from destroying data — covered only
/// by root-gated tests.
///
/// # Errors
///
/// Returns an error when both paths are equal after canonicalisation.
fn ensure_distinct_paths(img_canon: &Path, dev_canon: &Path) -> Result<()> {
    if img_canon == dev_canon {
        bail!(
            "image and target resolve to the same path ({}); refusing to flash",
            dev_canon.display()
        );
    }
    Ok(())
}

/// Refuse a device too small to hold a head+tail signature wipe.
///
/// Phase 3's wipe re-checks this bound (defense in depth), but by then
/// the guard is armed, so a too-small device would earn the "device is
/// in an inconsistent state" FATAL warning despite never having been
/// touched. Refusing here keeps that failure clean.
///
/// # Errors
///
/// Returns an error when `dev_size` is below `2 * WIPE_REGION`.
fn ensure_device_large_enough(dev_size: u64) -> Result<()> {
    if dev_size < 2 * WIPE_REGION {
        bail!(
            "device is too small ({dev_size} bytes) for a head+tail signature \
             wipe (minimum {} bytes); refusing before any destructive step.",
            2 * WIPE_REGION
        );
    }
    Ok(())
}

/// Refuse a raw image larger than the target device.
///
/// Only raw images have a known size up front; compressed input passes
/// `None` and is bounded per-chunk during Phase 4 instead.
///
/// # Errors
///
/// Returns an error when a known image size exceeds `dev_size`.
fn ensure_image_fits(raw_size: Option<u64>, dev_size: u64) -> Result<()> {
    if let Some(image_size) = raw_size
        && image_size > dev_size
    {
        bail!(
            "raw image ({image_size} bytes) is larger than target device \
                 ({dev_size} bytes); aborting"
        );
    }
    Ok(())
}

/// Refuse to run without root: every later phase needs raw block access.
fn phase0_root_check() -> Result<()> {
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
fn ensure_image_is_regular_file(path: &Path) -> Result<()> {
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
    // The same `S_IFMT`/`S_IFBLK` test appears in
    // `common::mount::block_rdev_of`. The two are deliberately separate:
    // this one refuses the operator's target with a diagnostic naming
    // `st_mode`, that one classifies a mountinfo source and returns
    // `None`. Merging them would cost the diagnostic. If you change the
    // predicate here, change it there too — both have tests asserting
    // acceptance *and* rejection, because inverting the comparison makes
    // a regular file look like a valid device.
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
fn ensure_whole_disk(path: &Path) -> Result<()> {
    let kname = sysfs::kname_for_path(path)
        .with_context(|| format!("resolving kernel name for {}", path.display()))?;
    // Order is load-bearing: `ensure_physical_or_loop` below accepts any
    // kname starting with "loop", which includes the `loop0p1` nodes
    // `losetup -P` creates. This refusal is what stops one reaching it.
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
    ensure_physical_or_loop(&kname, sysfs::has_backing_device(&kname), path)?;
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
fn ensure_image_not_on_target(img: &Path, dev_kname: &str) -> Result<()> {
    let img_meta = std::fs::metadata(img).with_context(|| format!("stat {}", img.display()))?;
    let img_id = (img_meta.dev(), img_meta.ino());

    let subtree = target_block_subtree(dev_kname)?;

    for member in &subtree {
        let Some(backing) = sysfs::loop_backing_file(member) else { continue };
        // A vanished or unreadable backing file cannot be the image we
        // just stat'ed successfully; skip rather than abort.
        let Ok(back_meta) = std::fs::metadata(&backing) else { continue };
        if is_same_file((back_meta.dev(), back_meta.ino()), img_id) {
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
/// # Why these are ioctls, and therefore `unsafe`
///
/// Both have safe-looking alternatives. Only one of them is equivalent,
/// and the checks below are what the kernel source says, not what the
/// names suggest.
///
/// **Size.** `File::seek(SeekFrom::End(0))` returns the same number:
/// `blkdev_llseek` reads `i_size`, `BLKGETSIZE64` reads
/// `bdev_nr_bytes(bdev)`, and both track capacity — verified on a loop
/// device across a `losetup -c` resize, where seek, the ioctl and
/// `/sys/class/block/<kname>/size × 512` all reported 16 MiB and then
/// 8 MiB. The ioctl is kept because it is the interface that *means*
/// "how large is this block device", where seeking to the end is a
/// consequence that currently agrees.
///
/// **Write protection.** Here the safe alternative is not equivalent and
/// must not be substituted. `BLKROGET` returns `bdev_read_only(bdev)`,
/// which `include/linux/blkdev.h` defines as
/// `bdev_test_flag(bdev, BD_READ_ONLY) || get_disk_ro(bdev->bd_disk)`.
/// The sysfs `ro` attribute is `disk_ro_show`, which returns
/// `get_disk_ro(disk)` — the right operand alone. A device carrying
/// `BD_READ_ONLY` without the gendisk flag reads as writable in sysfs
/// and read-only through the ioctl. Reading sysfs here would narrow a
/// write-protect refusal, which is the wrong direction for a gate whose
/// whole job is to refuse.
fn query_dev_geometry_readonly(path: &Path) -> Result<(u64, bool)> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("open {} (read-only)", path.display()))?;
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&f);
    let mut size: u64 = 0;
    // SAFETY: `fd` is valid for the whole of this function because `f`
    // owns it and outlives the last use — that ownership is the
    // enforcement, since nix's ioctl macros generate
    // `unsafe fn(fd: c_int)` and cannot take a `BorrowedFd` that would
    // carry the guarantee in the type. BLKGETSIZE64 writes a `u64`
    // through the pointer; `&raw mut size` is a valid, aligned,
    // non-null pointer to a live local that outlives the call.
    //
    // The block covers the call alone. It used to enclose the
    // `.context()?` as well, putting an early return inside `unsafe`.
    let size_rc = unsafe { ioctl::blkgetsize64(fd, &raw mut size) };
    size_rc.context("BLKGETSIZE64")?;

    let mut ro: libc::c_int = 0;
    // SAFETY: same FD validity as above, same ownership argument.
    // BLKROGET writes a `c_int` through the pointer; `&raw mut ro` is a
    // valid, aligned, non-null pointer to a live local that outlives
    // the call.
    let ro_rc = unsafe { ioctl::blkroget(fd, &raw mut ro) };
    ro_rc.context("BLKROGET (write-protect state)")?;

    // Keeps `f` alive past the last raw use of `fd` above, which is
    // what the SAFETY comments rest on.
    drop(f);
    Ok((size, ro != 0))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::common::testing::TempPath;

    use super::{
        Config, WIPE_REGION, confirm_destruction, ensure_block_device, ensure_device_large_enough,
        ensure_distinct_paths, ensure_image_fits, ensure_image_is_regular_file,
        ensure_physical_or_loop, file_type_name, is_same_file,
    };

    /// Regular files pass — the overwhelmingly common case.
    #[test]
    fn image_check_accepts_regular_file() {
        let p = TempPath::new("img-check");
        std::fs::write(&p, b"not really an iso").unwrap();
        ensure_image_is_regular_file(&p).unwrap();
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

    /// Flashing a device from itself would read and write the same
    /// bytes; refuse before anything else happens.
    #[test]
    fn distinct_paths_guard_catches_self_reference() {
        let a = Path::new("/dev/sdc");
        let b = Path::new("/tmp/image.iso");
        ensure_distinct_paths(b, a).expect("different paths must pass");

        let err = ensure_distinct_paths(a, a).unwrap_err();
        assert!(err.to_string().contains("same path"), "{err}");
        assert!(err.to_string().contains("/dev/sdc"), "{err}");
    }

    /// The floor is exactly `2 * WIPE_REGION`: one region at the head,
    /// one at the tail. The boundary is asserted on both sides because
    /// an off-by-one here either rejects a usable device or lets Phase 3
    /// arm the guard on one it cannot wipe.
    #[test]
    fn size_floor_guard_is_exactly_two_wipe_regions() {
        let floor = 2 * WIPE_REGION;
        ensure_device_large_enough(floor).expect("exactly the floor must pass");
        ensure_device_large_enough(floor + 1).expect("above the floor must pass");

        let err = ensure_device_large_enough(floor - 1).unwrap_err();
        assert!(err.to_string().contains("too small"), "{err}");
        ensure_device_large_enough(0).unwrap_err();
    }

    /// A raw image may equal the device size but never exceed it.
    /// Compressed input has no known size and must always be allowed
    /// through — Phase 4 bounds it per chunk instead.
    #[test]
    fn image_fits_guard_allows_exact_fit_and_unknown_sizes() {
        ensure_image_fits(Some(1024), 1024).expect("an exact fit must pass");
        ensure_image_fits(Some(1023), 1024).expect("a smaller image must pass");
        ensure_image_fits(None, 1024).expect("compressed input has no known size");
        ensure_image_fits(None, 0).expect("unknown size is never rejected here");

        let err = ensure_image_fits(Some(1025), 1024).unwrap_err();
        assert!(err.to_string().contains("larger than target device"), "{err}");
    }

    /// Only a block device may be a flash target.
    ///
    /// This is Phase 0's `S_IFBLK` gate. Every rejected kind is asserted
    /// individually because the mask (`&`) and the comparison (`!=`) fail
    /// differently: turning `&` into `|` makes everything look wrong,
    /// while inverting `!=` makes everything look right — and the second
    /// mistake would accept a regular file as a device.
    #[test]
    fn block_device_guard_accepts_only_block_devices() {
        let dir = std::env::temp_dir();
        let file = TempPath::new("notblk");
        std::fs::write(&file, b"x").unwrap();

        let err = ensure_block_device(&file).unwrap_err();
        assert!(err.to_string().contains("is not a block device"), "{err}");
        ensure_block_device(&dir).unwrap_err();
        ensure_block_device(Path::new("/dev/null")).unwrap_err();
        // A missing path fails at stat, with the path named.
        let stat_err = ensure_block_device(Path::new("/nonexistent/imi-probe")).unwrap_err();
        assert!(format!("{stat_err:#}").contains("stat"), "{stat_err:#}");

        // Positive direction where the environment provides one.
        let loop0 = Path::new("/dev/loop0");
        if loop0.exists() {
            ensure_block_device(loop0).expect("/dev/loop0 is a block device");
        }
    }

    /// The confirmation gate, in both directions and both modes.
    ///
    /// `--yes` must skip the question outright rather than answer it: a
    /// sink that refuses everything must still let a pre-approved run
    /// through, which is what distinguishes "skip" from "ask and accept".
    #[test]
    fn confirmation_gate_asks_only_when_needed_and_obeys_the_answer() {
        use std::cell::Cell;
        use std::path::Path;

        use crate::common::image::Compression;
        use crate::events::Summary;

        struct Sink {
            answer: bool,
            asked: Cell<u32>,
        }
        impl crate::events::Events for Sink {
            fn confirm(&mut self, _s: &Summary<'_>) -> bool {
                self.asked.set(self.asked.get() + 1);
                self.answer
            }
        }

        let summary = Summary {
            device: Path::new("/dev/sdz"),
            model: Some("Test"),
            device_size: 1024,
            image: Path::new("/tmp/a.iso"),
            compression: Compression::Raw,
            raw_image_size: Some(512),
        };
        let cfg = |yes: bool| {
            let mut c = Config::new(PathBuf::from("/tmp/a.iso"), PathBuf::from("/dev/sdz"));
            c.yes = yes;
            c
        };

        // Not pre-approved, sink refuses: abort, having asked.
        let mut no = Sink { answer: false, asked: Cell::new(0) };
        let err = confirm_destruction(&cfg(false), &summary, &mut no).unwrap_err();
        assert!(err.to_string().contains("aborted by user"), "{err}");
        assert_eq!(no.asked.get(), 1, "it must actually ask");

        // Not pre-approved, sink accepts: proceed.
        let mut yes = Sink { answer: true, asked: Cell::new(0) };
        confirm_destruction(&cfg(false), &summary, &mut yes).expect("an accepted answer proceeds");
        assert_eq!(yes.asked.get(), 1);

        // Pre-approved: never ask. A refusing sink proves it was skipped.
        let mut never = Sink { answer: false, asked: Cell::new(0) };
        confirm_destruction(&cfg(true), &summary, &mut never).expect("--yes must not consult it");
        assert_eq!(never.asked.get(), 0, "--yes must skip the question, not answer it");
    }

    /// Loop devices and physically-backed disks pass; stacked ones do
    /// not.
    ///
    /// The `||` matters as much as the `!`: mutated to `&&` it refuses
    /// every loop device, since a loop node has no backing entry in
    /// sysfs — which is exactly why the name is checked separately.
    #[test]
    fn physical_or_loop_accepts_disks_and_loops_only() {
        let p = Path::new("/dev/x");

        ensure_physical_or_loop("loop0", false, p).expect("a loop device is a supported target");
        ensure_physical_or_loop("loop12", false, p).expect("any loopN");
        ensure_physical_or_loop("sda", true, p).expect("a backed device is physical");
        ensure_physical_or_loop("nvme0n1", true, p).expect("nvme too");

        for kname in ["dm-0", "md0", "zram0", "vg-root"] {
            let err = ensure_physical_or_loop(kname, false, p)
                .expect_err("a stacked device must be refused");
            assert!(err.to_string().contains("virtual/stacked"), "{err}");
        }
    }

    /// File identity is `(device, inode)`, both halves significant.
    ///
    /// Two files can share an inode number on different devices, and a
    /// device holds many inodes — so a comparison that ignored either
    /// half would mistake unrelated files for the image.
    #[test]
    fn same_file_compares_both_device_and_inode() {
        assert!(is_same_file((8, 42), (8, 42)));
        assert!(!is_same_file((8, 42), (8, 43)), "same device, different inode");
        assert!(!is_same_file((8, 42), (9, 42)), "same inode, different device");
        assert!(!is_same_file((8, 42), (9, 43)));
        assert!(is_same_file((0, 0), (0, 0)), "zeroes still compare equal");
    }
}
