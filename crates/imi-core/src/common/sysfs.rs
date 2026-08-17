//! Sysfs helpers.
//!
//! Everything here is pure filesystem lookup under `/sys/class/block/<name>/`
//! and `/sys/dev/block/<major>:<minor>`. No ioctls, no child processes.
//!
//! Functions use `String` for kernel names rather than `OsString`/`PathBuf`
//! because kernel names are ASCII by construction (`sda`, `sdb1`, `dm-0`,
//! `nvme0n1p2`, `loop0`, …). If we ever see a non-UTF-8 name, something else
//! is already very wrong and returning an error is the right move.
//!
//! That applies to the enumerations a safety gate depends on —
//! [`partitions_of`] and [`holders_recursive`] — where skipping an
//! unreadable name means the caller is never told about a partition or a
//! stacked volume, and proceeds to destroy the device. The one
//! deliberate exception is [`all_block_knames`], a peripheral widening
//! scan whose own callers document that an individual unreadable or
//! vanished node must not block an otherwise valid flash.

use crate::Result;
use crate::error::{Context as _, err};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Base of sysfs block class. Exposed to keep call-sites greppable.
const SYS_CLASS_BLOCK: &str = "/sys/class/block";

/// Base of sysfs dev-major:minor lookup.
const SYS_DEV_BLOCK: &str = "/sys/dev/block";

/// Resolve a device node path (e.g. `/dev/sdb`, `/dev/mapper/foo`, a symlink
/// under `/dev/disk/by-*`) to its kernel name (e.g. `sdb`, `dm-0`).
///
/// Works by stat'ing the node and consulting `/sys/dev/block/<major>:<minor>`.
pub(crate) fn kname_for_path(path: &Path) -> Result<String> {
    let st = nix::sys::stat::stat(path).with_context(|| format!("stat {}", path.display()))?;
    // `st_rdev` is the device number that *this device node refers to*.
    // `st_dev` is the FS-device the *inode itself* lives on. We want `st_rdev`
    // for a device node.
    let rdev = st.st_rdev;
    let maj = nix::sys::stat::major(rdev);
    let min = nix::sys::stat::minor(rdev);
    kname_for_devt(maj, min)
}

/// Resolve a `(major, minor)` pair to its kernel name.
///
/// `/sys/dev/block/<major>:<minor>` is a symlink into `/sys/devices/...`
/// whose last component is the kernel name.
pub(crate) fn kname_for_devt(major: u64, minor: u64) -> Result<String> {
    kname_for_devt_in(SYS_DEV_BLOCK, major, minor)
}

/// [`kname_for_devt`] against an arbitrary sysfs root.
///
/// This is the entry point every other lookup in the module depends on:
/// it is what turns the device path an operator typed into the kernel
/// name that `is_partition`, `holders_recursive` and `partitions_of` all
/// key on. It was also the only function here without an `_in` variant,
/// so the one link in the chain that parses operator input was the one
/// link no unit test could reach.
fn kname_for_devt_in(root: &str, major: u64, minor: u64) -> Result<String> {
    let link = format!("{root}/{major}:{minor}");
    let target = fs::read_link(&link).with_context(|| format!("readlink {link}"))?;
    target
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .ok_or_else(|| err!("no basename in sysfs link target: {}", target.display()))
}

/// True if the kernel name refers to a partition (has a `partition` file
/// under `/sys/class/block/<kname>/`).
///
/// Returns `false` when the marker file is absent **or** cannot be
/// read: `Path::exists` collapses both. A caller using this as a gate
/// must therefore also confirm the device exists — [`exists`] fails in
/// the safe direction, so checking it after this one turns a
/// can't-tell into a refusal. `ensure_whole_disk` relies on exactly
/// that ordering.
pub(crate) fn is_partition(kname: &str) -> bool {
    is_partition_in(SYS_CLASS_BLOCK, kname)
}

/// [`is_partition`] against an arbitrary sysfs root.
///
/// The `*_in` variants exist purely for testability: every lookup in
/// this module resolves against the machine's real `/sys`, which makes
/// the functions unreachable from a unit test and leaves the refusals
/// they gate unverified. Taking the root as a parameter lets tests build
/// a synthetic tree in a tempdir. Production callers use the wrappers.
fn is_partition_in(root: &str, kname: &str) -> bool {
    Path::new(&format!("{root}/{kname}/partition")).exists()
}

/// True if the kernel name refers to a block device known to sysfs.
pub(crate) fn exists(kname: &str) -> bool {
    exists_in(SYS_CLASS_BLOCK, kname)
}

/// [`exists`] against an arbitrary sysfs root.
fn exists_in(root: &str, kname: &str) -> bool {
    Path::new(&format!("{root}/{kname}")).exists()
}

/// List the partitions of a whole disk, returned as kernel names.
/// Scans `/sys/class/block/<disk>/` for child directories containing a
/// `partition` file.
pub(crate) fn partitions_of(disk_kname: &str) -> Result<Vec<String>> {
    partitions_of_in(SYS_CLASS_BLOCK, disk_kname)
}

/// [`partitions_of`] against an arbitrary sysfs root.
///
/// # Errors
///
/// Returns an error if the disk's sysfs directory cannot be read.
fn partitions_of_in(root: &str, disk_kname: &str) -> Result<Vec<String>> {
    let dir = format!("{root}/{disk_kname}");
    let mut out = Vec::new();
    let read = fs::read_dir(&dir).with_context(|| format!("read_dir {dir}"))?;
    for entry in read {
        let entry = entry.with_context(|| format!("read_dir entry under {dir}"))?;
        // Not `continue`, for the same reason as `walk_holders`: a
        // partition this cannot name is one Phase 1 will never unmount.
        let name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| {
                err!("partition of {disk_kname} has a non-UTF-8 name: {:?}", entry.file_name())
            })?
            .to_owned();
        let part_marker = entry.path().join("partition");
        if part_marker.exists() {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// Every whole-disk kernel name the host exposes.
///
/// Read from `/sys/block`, which lists disks only — partitions live one
/// level down and never appear here — so no `is_partition` filtering is
/// needed on the result. Virtual devices (`loop0`, `zram0`, ...) do
/// appear and are the caller's problem: this function reports what the
/// kernel exposes, and the policy of which disks are *plausible flash
/// targets* belongs to `devices::candidate_devices`, in one place.
///
/// Names that are not UTF-8 are skipped rather than failing the whole
/// enumeration: such a name cannot be a candidate the picker could
/// display or the user could type, and one hostile udev rule should not
/// blind the listing to every other disk.
pub(crate) fn all_disk_knames() -> Result<Vec<String>> {
    all_disk_knames_in("/sys/block")
}

/// Injectable body of [`all_disk_knames`].
fn all_disk_knames_in(root: &str) -> Result<Vec<String>> {
    let rd = fs::read_dir(root).with_context(|| format!("read {root}"))?;
    let mut out = Vec::new();
    for entry in rd {
        let entry = entry.with_context(|| format!("read an entry of {root}"))?;
        if let Ok(name) = entry.file_name().into_string() {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// The disk's capacity in bytes, from `/sys/class/block/<kname>/size`.
///
/// The sysfs attribute counts 512-byte sectors regardless of the disk's
/// logical block size — that constant is part of the sysfs ABI, not a
/// property of the device — so the byte figure is `sectors * 512`.
///
/// `None` for a missing or unparseable attribute *and* for a present
/// zero: a card reader with no card inserted reports 0, and "no medium"
/// and "no attribute" call for the same treatment from a picker.
pub(crate) fn size_bytes(kname: &str) -> Option<u64> {
    size_bytes_in(SYS_CLASS_BLOCK, kname)
}

/// Injectable body of [`size_bytes`].
fn size_bytes_in(root: &str, kname: &str) -> Option<u64> {
    let raw = fs::read_to_string(format!("{root}/{kname}/size")).ok()?;
    let sectors: u64 = raw.trim().parse().ok()?;
    sectors.checked_mul(512).filter(|&b| b > 0)
}

/// Whether the device reports itself removable.
///
/// `false` for a missing attribute: `NVMe`-in-USB enclosures and some virtio
/// disks report 0 despite being exactly what a user wants to flash, so
/// this is display metadata for sorting a picker — never an exclusion
/// gate. Treating "unknown" and "fixed" the same is therefore harmless
/// here, where it would be a policy bug in a filter.
pub(crate) fn removable(kname: &str) -> bool {
    removable_in(SYS_CLASS_BLOCK, kname)
}

/// Injectable body of [`removable`].
fn removable_in(root: &str, kname: &str) -> bool {
    fs::read_to_string(format!("{root}/{kname}/removable")).is_ok_and(|s| s.trim() == "1")
}

/// Read `/sys/class/block/<kname>/dm/uuid` if present. DM devices embed a
/// prefix identifying the target type: `LVM-`, `CRYPT-`, `mpath-`, `DMRAID-`,
/// `part-`, etc. Returns `None` for non-DM devices.
pub(crate) fn dm_uuid(kname: &str) -> Option<String> {
    dm_uuid_in(SYS_CLASS_BLOCK, kname)
}

/// [`dm_uuid`] against an arbitrary sysfs root.
fn dm_uuid_in(root: &str, kname: &str) -> Option<String> {
    let p = format!("{root}/{kname}/dm/uuid");
    fs::read_to_string(p).ok().map(|s| s.trim().to_owned())
}

/// Recursively collect the transitive closure of `holders/` for a given
/// device. Each entry in `holders/` is a symlink to another `/sys/class/block`
/// node; we follow its basename.
///
/// Does *not* include `root_kname` itself — add separately if you want it.
pub(crate) fn holders_recursive(root_kname: &str) -> Result<HashSet<String>> {
    holders_recursive_in(SYS_CLASS_BLOCK, root_kname)
}

/// [`holders_recursive`] against an arbitrary sysfs root.
///
/// # Errors
///
/// Returns an error if a `holders/` directory exists but cannot be read.
fn holders_recursive_in(root: &str, root_kname: &str) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    walk_holders(root, root_kname, &mut out)?;
    Ok(out)
}

/// DFS over `holders/` links, accumulating every transitive holder.
fn walk_holders(root: &str, kname: &str, acc: &mut HashSet<String>) -> Result<()> {
    let dir = format!("{root}/{kname}/holders");
    let read = match fs::read_dir(&dir) {
        Ok(r) => r,
        // `holders/` may not exist on some synthetic/removed devices; treat
        // as "no holders" rather than propagating.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(err!("read_dir {dir}: {e}")),
    };
    for entry in read {
        let entry = entry.with_context(|| format!("read_dir entry under {dir}"))?;
        // Not `continue`. A holder whose name this cannot read is a
        // stacked volume the caller will never be told about, and the
        // caller uses this to decide whether the device is safe to
        // destroy. Kernel block names are ASCII by construction, so this
        // should be unreachable — which is exactly why erroring costs
        // nothing and skipping costs a flash over live LVM.
        let child = entry
            .file_name()
            .to_str()
            .ok_or_else(|| err!("holder of {kname} has a non-UTF-8 name: {:?}", entry.file_name()))?
            .to_owned();
        if acc.insert(child.clone()) {
            // Only recurse if we haven't already visited this node. Prevents
            // infinite loops on pathological sysfs layouts (shouldn't happen
            // in practice, but cheap insurance).
            walk_holders(root, &child, acc)?;
        }
    }
    Ok(())
}

/// Read `/sys/class/block/<kname>/device/model` if present, trimmed.
///
/// Shown to the operator in the confirmation prompt and in the device
/// picker's list — the two places a human decides by it — which is why
/// [`sanitise_model`] runs here rather than at either display site:
/// every future reader of this value gets the defanged form.
pub(crate) fn device_model(kname: &str) -> Option<String> {
    device_model_in(SYS_CLASS_BLOCK, kname)
}

/// [`device_model`] against an arbitrary sysfs root.
fn device_model_in(root: &str, kname: &str) -> Option<String> {
    let p = format!("{root}/{kname}/device/model");
    fs::read_to_string(p).ok().map(|s| sanitise_model(&s))
}

/// Device serial from sysfs, if the device exposes one.
///
/// `None` for anything without a `device/serial` attribute — loop
/// devices, device-mapper nodes, and plenty of real hardware. Callers
/// must treat `None` as a value to match, not as a wildcard.
///
/// Sanitised identically to [`device_model`]: it is the same class of
/// firmware-supplied string and reaches the same terminal.
pub(crate) fn device_serial(kname: &str) -> Option<String> {
    device_serial_in(SYS_CLASS_BLOCK, kname)
}

/// Device WWID from sysfs, if the device exposes one.
///
/// Served from VPD page 0x83 (Device Identification), which is unique by
/// design where the page 0x80 serial is vendor-defined. Same absence
/// semantics as [`device_serial`].
pub(crate) fn device_wwid(kname: &str) -> Option<String> {
    device_wwid_in(SYS_CLASS_BLOCK, kname)
}

/// [`device_wwid`] against an arbitrary sysfs root.
fn device_wwid_in(root: &str, kname: &str) -> Option<String> {
    device_attr_in(root, kname, "wwid")
}

/// Read one `device/<attr>` string, sanitised, with empty meaning absent.
///
/// Shared by [`device_serial`] and [`device_wwid`]: both are
/// firmware-supplied strings read the same way, and an empty attribute
/// is absence rather than an empty-string identity — a device that
/// reports a blank serial must not be treated as matching another that
/// also reports blank.
fn device_attr_in(root: &str, kname: &str, attr: &str) -> Option<String> {
    let p = format!("{root}/{kname}/device/{attr}");
    fs::read_to_string(p).ok().map(|s| sanitise_model(&s)).filter(|s| !s.is_empty())
}

/// [`device_serial`] against an arbitrary sysfs root.
fn device_serial_in(root: &str, kname: &str) -> Option<String> {
    device_attr_in(root, kname, "serial")
}

/// Strip anything from a device-reported string that could rewrite the
/// terminal it is printed on.
///
/// The model comes from the device's own firmware. The kernel emits it
/// unfiltered — `drivers/scsi/scsi_sysfs.c` has
/// `sdev_rd_attr(model, "%.16s\n")` over the raw INQUIRY bytes, with no
/// `isprint` check anywhere in the file — so every byte the device chose
/// reaches us.
///
/// This string is printed in the confirmation banner, which is the whole
/// of this tool's safety model: show the operator which disk is about to
/// be destroyed and make them type `yes`. A model containing
/// `\x1b[2K\x1b[1A` can erase the lines above it, so a device could
/// present itself under the path of another one and collect a
/// confirmation meant for that other device. USB sticks are the primary
/// target here, and a USB device chooses its own descriptors.
///
/// Control characters are replaced rather than dropped, so a doctored
/// model looks visibly wrong instead of looking like a shorter name.
fn sanitise_model(raw: &str) -> String {
    let cleaned: String =
        raw.trim().chars().map(|c| if c.is_control() { '\u{FFFD}' } else { c }).collect();
    // The kernel field is 16 bytes; anything longer is not a model.
    cleaned.chars().take(64).collect()
}

/// True if `/sys/class/block/<kname>/device` exists — i.e. the kernel name
/// is backed by a physical device (`sd*`, `nvme*n*`, `mmcblk*`, `vd*`, …).
///
/// Virtual and stacked nodes — `dm-*`, `md*`, `zram*`, `ram*` — expose no
/// `device` link. Loop devices also lack it and are allow-listed separately
/// by the caller (they are the sanctioned test target).
pub(crate) fn has_backing_device(kname: &str) -> bool {
    has_backing_device_in(SYS_CLASS_BLOCK, kname)
}

/// [`has_backing_device`] against an arbitrary sysfs root.
fn has_backing_device_in(root: &str, kname: &str) -> bool {
    Path::new(&format!("{root}/{kname}/device")).exists()
}

/// List every kernel block name currently known to sysfs
/// (`/sys/class/block/*`). Non-UTF-8 entries are skipped, matching the
/// module-wide policy on kernel names.
pub(crate) fn all_block_knames() -> Result<Vec<String>> {
    let read =
        fs::read_dir(SYS_CLASS_BLOCK).with_context(|| format!("read_dir {SYS_CLASS_BLOCK}"))?;
    let mut out = Vec::new();
    for entry in read {
        let entry = entry.with_context(|| format!("read_dir entry under {SYS_CLASS_BLOCK}"))?;
        if let Some(name) = entry.file_name().to_str() {
            out.push(name.to_owned());
        }
    }
    out.sort();
    Ok(out)
}

/// Read `/sys/class/block/<kname>/loop/backing_file` for a loop device.
///
/// Returns the backing file's path, or `None` when the node is not a loop
/// device, has no file attached, or the sysfs read fails. The kernel
/// appends ` (deleted)` when the backing file has been unlinked while
/// still attached; we strip that marker so the path can be stat'ed (the
/// stat will then fail cleanly for a genuinely-deleted file).
pub(crate) fn loop_backing_file(kname: &str) -> Option<PathBuf> {
    loop_backing_file_in(SYS_CLASS_BLOCK, kname)
}

/// [`loop_backing_file`] against an arbitrary sysfs root.
fn loop_backing_file_in(root: &str, kname: &str) -> Option<PathBuf> {
    let p = format!("{root}/{kname}/loop/backing_file");
    let raw = fs::read_to_string(p).ok()?;
    let trimmed = strip_deleted_suffix(raw.trim_end_matches('\n'));
    if trimmed.is_empty() { None } else { Some(PathBuf::from(trimmed)) }
}

/// Strip the kernel's ` (deleted)` suffix from a sysfs-reported path.
/// Pure; unit-tested below.
fn strip_deleted_suffix(s: &str) -> &str {
    s.strip_suffix(" (deleted)").unwrap_or(s)
}

#[cfg(test)]
mod tests {
    /// The serial read must find the attribute, sanitise it, and treat
    /// An attribute that exists but cannot be read is absent, not present.
    ///
    /// Field data: one tested USB stick has no `serial` file at all, and a
    /// `wwid` file that returns `ENXIO` — SCSI registers the attribute
    /// for every device, and the read fails when there is no VPD page
    /// 0x83 to fill it from. `read_to_string(..).ok()` maps that to
    /// `None`, which is the behaviour the identity check needs: a device
    /// that reports nothing must compare equal to itself later.
    ///
    /// Simulated with a directory where the file belongs, which fails
    /// the read with `EISDIR`. The errno differs from the hardware's;
    /// what is pinned is that a failed read is not mistaken for data,
    /// and that a caller testing `Path::exists` first would get the
    /// opposite answer.
    #[test]
    fn an_unreadable_attribute_reads_as_absent() {
        let root = fake_sysfs("unreadable-attr");
        let r = root.path();
        let dev = format!("{r}/sdz/device");
        fs::create_dir_all(&dev).unwrap();
        // Present as a path, unreadable as a file.
        fs::create_dir_all(format!("{dev}/wwid")).unwrap();
        fs::write(format!("{dev}/serial"), "REAL-SERIAL\n").unwrap();

        assert!(Path::new(&format!("{dev}/wwid")).exists(), "precondition: the path is there");
        assert_eq!(
            device_wwid_in(r, "sdz"),
            None,
            "an unreadable attribute must not be reported as present"
        );
        assert_eq!(
            device_serial_in(r, "sdz"),
            Some("REAL-SERIAL".to_owned()),
            "a readable sibling must still be read"
        );
    }

    /// an empty file as absent.
    #[test]
    fn device_serial_reads_sanitises_and_normalises_empty() {
        let root = fake_sysfs("serial");
        let r = root.path();

        fs::create_dir_all(format!("{r}/sdb/device")).unwrap();
        fs::write(format!("{r}/sdb/device/serial"), "  4C5300012605  \n").unwrap();
        assert_eq!(device_serial_in(r, "sdb").as_deref(), Some("4C5300012605"));

        // Same firmware-supplied string class as the model, same risk.
        fs::create_dir_all(format!("{r}/sdc/device")).unwrap();
        fs::write(format!("{r}/sdc/device/serial"), "\u{1b}[2KAAAA").unwrap();
        let cleaned = device_serial_in(r, "sdc").expect("present");
        assert!(!cleaned.chars().any(char::is_control), "{cleaned:?}");

        // An empty attribute is absence, not an empty-string identity.
        fs::create_dir_all(format!("{r}/sdd/device")).unwrap();
        fs::write(format!("{r}/sdd/device/serial"), "\n").unwrap();
        assert_eq!(device_serial_in(r, "sdd"), None);

        // No attribute at all.
        assert_eq!(device_serial_in(r, "sda"), None);
    }

    /// The serial and WWID readers must read *different* attributes.
    ///
    /// Both delegate to `device_attr_in` with a name, and nothing else
    /// checks the name is right. A `"wwid"` typed as `"serial"` would
    /// make both return the same string, `check_wwid` would then compare
    /// a serial against itself, and the WWID protection would be
    /// silently vacuous — present in the code, absent in effect. Which
    /// is the whole reason it was added: page 0x80 serials are
    /// duplicated across cheap sticks, page 0x83 WWIDs are not.
    #[test]
    fn serial_and_wwid_read_distinct_attributes() {
        let root = fake_sysfs("attrs");
        let r = root.path();

        fs::create_dir_all(format!("{r}/sde/device")).unwrap();
        fs::write(format!("{r}/sde/device/serial"), "SERIAL-0080\n").unwrap();
        fs::write(format!("{r}/sde/device/wwid"), "naa.WWID-0083\n").unwrap();

        assert_eq!(device_serial_in(r, "sde").as_deref(), Some("SERIAL-0080"));
        assert_eq!(device_wwid_in(r, "sde").as_deref(), Some("naa.WWID-0083"));
        assert_ne!(
            device_serial_in(r, "sde"),
            device_wwid_in(r, "sde"),
            "the two readers must not be reading the same file"
        );

        // Each is independently absent.
        fs::create_dir_all(format!("{r}/sdf/device")).unwrap();
        fs::write(format!("{r}/sdf/device/serial"), "ONLY-SERIAL\n").unwrap();
        assert_eq!(device_serial_in(r, "sdf").as_deref(), Some("ONLY-SERIAL"));
        assert_eq!(device_wwid_in(r, "sdf"), None, "no wwid attribute means None");
    }

    /// A device-supplied model must not be able to rewrite the banner.
    ///
    /// The kernel passes INQUIRY bytes through untouched, and the banner
    /// is what an operator reads before agreeing to destroy a disk. A
    /// model carrying `ESC [ 2K` and `ESC [ 1A` could erase the device
    /// path printed above it.
    #[test]
    fn a_hostile_model_string_cannot_rewrite_the_terminal() {
        // Erase-line, cursor-up, carriage return, and a bare NUL.
        let hostile = "\u{1b}[2K\u{1b}[1A\rSanDisk Cruzer\u{0}";
        let clean = sanitise_model(hostile);

        assert!(!clean.contains('\u{1b}'), "escape survived: {clean:?}");
        assert!(!clean.contains('\r'), "carriage return survived: {clean:?}");
        assert!(!clean.contains('\u{0}'), "NUL survived: {clean:?}");
        assert!(!clean.chars().any(char::is_control), "a control char survived: {clean:?}");

        // The legible part is kept, so the operator still sees a name.
        assert!(clean.contains("SanDisk Cruzer"), "{clean:?}");

        // Replaced, not dropped: a doctored model must look wrong rather
        // than merely look short.
        assert!(clean.contains('\u{FFFD}'), "control chars must leave a mark: {clean:?}");

        // An ordinary model is untouched apart from trimming.
        assert_eq!(sanitise_model("  Ultra Fit  \n"), "Ultra Fit");

        // A model longer than any real one is capped.
        assert_eq!(sanitise_model(&"A".repeat(500)).chars().count(), 64);
    }

    /// The public wrappers must consult the real sysfs, at the right
    /// root.
    ///
    /// Each is a one-line delegation to an `_in` variant with a root
    /// constant, and the `_in` variants are thoroughly covered against
    /// synthetic trees — but nothing checked that the wrapper passes
    /// `SYS_CLASS_BLOCK` rather than `SYS_DEV_BLOCK`, or that it
    /// delegates at all. Mutation testing could replace any of them
    /// wholesale with a constant and no test noticed.
    ///
    /// Reads the machine's own `/sys`, which every Linux host has and
    /// which needs no privilege — so this runs in the ordinary suite
    /// rather than behind `--ignored`, where `cargo mutants` would not
    /// see it.
    /// The `pub(crate)` wrappers must read the real root, checked against
    /// the filesystem rather than against themselves.
    ///
    /// Each wrapper is one line: `f(k)` calls `f_in(SYS_CLASS_BLOCK, k)`.
    /// The only thing that can be wrong is the root it passes, so the
    /// obvious test is that the wrapper agrees with its `_in` variant —
    /// and that test is worthless. A pure delegation agrees with itself
    /// whatever root it used; both sides move together.
    ///
    /// The independent oracle is `/sys/class/block` itself: probe it
    /// directly and compare. That kills three mutants the existing
    /// real-sysfs test missed, because that one asserts
    /// `partitions_of(..).is_ok()` without looking at the contents and
    /// `dm_uuid(..).is_none()` in the negative direction only.
    ///
    /// What it does *not* do here is catch a wrapper reading the wrong
    /// tree. Tried: pointing `is_partition` at `/sys/dev/block`, which is
    /// keyed by `MAJ:MIN` rather than by name, and this test still
    /// passed. On a host with no partitions the wrong root and the right
    /// one both answer "not a partition" for every device, so nothing
    /// discriminates. The oracle is still the right shape — comparing a
    /// delegation against itself would be worthless — but its reach
    /// depends on the tree having something to disagree about.
    #[test]
    fn public_wrappers_match_a_direct_probe_of_the_real_sysfs() {
        let names = all_block_knames().expect("/sys/class/block must be readable");
        assert!(!names.is_empty(), "no block devices; this test cannot run");

        // Devices actually probed, as distinct from devices enumerated.
        // The `continue` below skips one that vanished, and a skip that
        // fired for every device would leave this test green having
        // asserted nothing — checked by forcing exactly that, which it
        // passed until this counter existed.
        let mut probed = 0_usize;

        for k in &names {
            let base = PathBuf::from(SYS_CLASS_BLOCK).join(k);

            // Read the directory first, and treat "gone" as "skip".
            //
            // `all_block_knames` enumerated a moment ago; cargo runs test
            // binaries concurrently, and the integration suites attach and
            // detach loop devices. A device that vanished between the
            // enumeration and this probe is a benign race, not a defect,
            // and every other error still fails the test. Reasoned rather
            // than observed — this host has one CPU, so the binaries
            // serialise and the window never opens locally.
            let entries = match fs::read_dir(&base) {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => panic!("read_dir {}: {e}", base.display()),
            };

            assert_eq!(
                is_partition(k),
                base.join("partition").exists(),
                "{k}: is_partition must match the presence of the partition marker"
            );

            // The oracle must be independent in implementation and
            // identical in contract. `partitions_of_in` propagates a bad
            // entry rather than skipping it — its own comment says why: a
            // partition it cannot name is one Phase 1 will never unmount
            // — and rejects a non-UTF-8 name outright. An oracle that
            // `flatten()`s errors away and `to_string_lossy()`s names
            // would agree with a function that had lost both behaviours.
            let mut expected: Vec<String> = entries
                .map(|e| e.expect("a readable dir entry"))
                .filter(|e| e.path().join("partition").exists())
                .map(|e| e.file_name().to_str().expect("a UTF-8 partition name").to_owned())
                .collect();
            expected.sort();
            let mut got = partitions_of(k).expect("partitions_of must read the real root");
            got.sort();
            assert_eq!(got, expected, "{k}: partitions_of must match a direct readdir");

            // `dm_uuid_in` trims but does not discard an empty result, so
            // an empty `dm/uuid` file yields `Some("")`. An oracle that
            // filtered empties would report `None` and disagree.
            //
            // Not demonstrable on a host with no device-mapper device:
            // adding the filter to `dm_uuid_in` here leaves the whole
            // suite green, because both sides stay `None` whatever the
            // rule is. The alignment is a contract argument, not a
            // measured one, and this note exists so the next reader does
            // not "simplify" one side back out of step with the other.
            let expected_uuid =
                fs::read_to_string(base.join("dm").join("uuid")).ok().map(|u| u.trim().to_owned());
            assert_eq!(dm_uuid(k), expected_uuid, "{k}: dm_uuid must match the real file");

            probed += 1;
        }

        assert!(
            probed > 0,
            "every one of the {} enumerated devices was skipped; this test asserted nothing",
            names.len()
        );
    }

    #[test]
    fn public_wrappers_resolve_against_the_real_sysfs() {
        // The emptiness check stays an assertion rather than an early
        // return: everything below is vacuous without at least one
        // device, and a test that silently verifies nothing is worse
        // than one that says why it cannot.
        let names = all_block_knames().expect("/sys/class/block must be readable");
        assert!(
            !names.is_empty(),
            "no block devices in /sys/class/block; the wrapper checks below cannot run"
        );

        // Every name sysfs just gave us must exist, and a made-up one
        // must not. Kills `exists -> true` and `exists -> false`.
        for n in &names {
            assert!(exists(n), "{n} came from /sys/class/block but exists() denies it");
        }
        assert!(!exists("imi-no-such-device"), "a fabricated name must not exist");

        // `partitions_of` must succeed for every listed device, even one
        // with no partitions. This discriminates the root without
        // needing a partitioned disk: `/sys/dev/block` is keyed by
        // `MAJ:MIN`, so a wrapper passing it would fail `read_dir` on
        // every kernel name.
        for n in &names {
            assert!(
                partitions_of(n).is_ok(),
                "partitions_of({n}) must read /sys/class/block, not a MAJ:MIN tree"
            );
        }

        // `is_partition` is the one wrapper whose root cannot be checked
        // this way: neither `/sys/class/block/loop0/partition` nor
        // `/sys/dev/block/loop0/partition` exists, so both roots answer
        // "false" on a host with no partitioned disk. The block below
        // covers it where the host provides one.
        //
        // If the host has a partition *and* lists its parent disk, the
        // two must disagree about `is_partition`, and the disk must
        // enumerate the partition. Both conditions are host facts, not
        // invariants: a container can expose a partition whose disk is
        // not in its `/sys` view. Finding neither is not a failure —
        // asserting it would fail this suite on someone's machine for a
        // reason that has nothing to do with the code.
        let pair = names.iter().find_map(|part| {
            is_partition(part)
                .then(|| names.iter().find(|d| *d != part && part.starts_with(d.as_str())))
                .flatten()
                .map(|disk| (part, disk))
        });
        if let Some((part, disk)) = pair {
            assert!(!is_partition(disk), "{disk} is a whole disk, {part} is its partition");
            assert!(
                partitions_of(disk).expect("partitions_of").contains(part),
                "{disk} must enumerate {part}"
            );
        }

        // `dm_uuid` must be None for something that is not a dm device.
        let non_dm = names.iter().find(|n| !n.starts_with("dm-"));
        if let Some(n) = non_dm {
            assert!(dm_uuid(n).is_none(), "{n} is not device-mapper");
        }

        // kname_for_path must round-trip a real node back to its name.
        //
        // Skipped under Miri, which does not model `st_rdev`: its `stat`
        // shim returns zero for it, so this resolves to `0:0` and fails
        // on `readlink /sys/dev/block/0:0`. Major 0 is reserved for
        // anonymous devices and cannot name a real block node, so the
        // zero is the shim rather than the host. Everything above runs
        // under Miri unchanged — only this branch needs a device number.
        if !cfg!(miri)
            && let Some(n) = names.first()
        {
            let node = PathBuf::from(format!("/dev/{n}"));
            if node.exists() {
                assert_eq!(&kname_for_path(&node).expect("kname_for_path"), n);
            }
        }
    }

    /// `kname_for_devt` must take the link's last component, and must
    /// fail rather than invent one.
    ///
    /// Every refusal in Phase 0 keys on the name this returns, so a
    /// wrong answer here misdirects `is_partition`, `holders_recursive`
    /// and `partitions_of` at once. Mutation testing could replace the
    /// whole function with `Ok("xyzzy".into())` and nothing noticed,
    /// because it was the only lookup in the module with no `_in`
    /// variant to test against a synthetic tree.
    #[test]
    fn kname_for_devt_reads_the_link_target() {
        use std::os::unix::fs::symlink;

        let root = fake_sysfs("devt");
        let r = root.path();
        fs::create_dir_all(format!("{r}/devices/pci0000:00/nvme0n1")).unwrap();
        symlink(format!("{r}/devices/pci0000:00/nvme0n1"), format!("{r}/259:0")).unwrap();

        assert_eq!(kname_for_devt_in(r, 259, 0).unwrap(), "nvme0n1");

        // A device number with no link must be an error, not a guess.
        let err = kname_for_devt_in(r, 8, 16).unwrap_err();
        assert!(format!("{err:#}").contains("readlink"), "{err:#}");

        // Major and minor must not be interchangeable.
        assert!(kname_for_devt_in(r, 0, 259).is_err(), "0:259 is not 259:0");
    }

    /// A name this cannot read must fail the enumeration, not vanish
    /// from it.
    ///
    /// Both of these feed a refusal: `partitions_of` decides what
    /// Phase 1 unmounts, `holders_recursive` decides whether the device
    /// has a stacked volume on it. Skipping an unreadable entry means
    /// the caller is told the device is clean and destroys it.
    #[test]
    fn a_non_utf8_name_fails_the_enumeration() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let root = fake_sysfs("nonutf8");
        let r = root.path();

        // A holder and a partition whose names are not valid UTF-8.
        let bad = OsStr::from_bytes(b"dm-\xff\xfe");
        fs::create_dir_all(format!("{r}/sda/holders")).unwrap();
        fs::create_dir_all(Path::new(&format!("{r}/sda/holders")).join(bad)).unwrap();
        let part = Path::new(&format!("{r}/sda")).join(bad);
        fs::create_dir_all(&part).unwrap();
        fs::write(part.join("partition"), "1").unwrap();

        let holders = holders_recursive_in(r, "sda");
        assert!(holders.is_err(), "an unreadable holder must not be skipped");
        assert!(format!("{:#}", holders.unwrap_err()).contains("non-UTF-8"));

        let parts = partitions_of_in(r, "sda");
        assert!(parts.is_err(), "an unreadable partition must not be skipped");
        assert!(format!("{:#}", parts.unwrap_err()).contains("non-UTF-8"));
    }

    use super::*;

    #[test]
    fn strip_deleted_suffix_removes_marker() {
        assert_eq!(strip_deleted_suffix("/tmp/img.iso (deleted)"), "/tmp/img.iso");
    }

    #[test]
    fn strip_deleted_suffix_leaves_plain_paths() {
        assert_eq!(strip_deleted_suffix("/tmp/img.iso"), "/tmp/img.iso");
        assert_eq!(strip_deleted_suffix(""), "");
    }

    /// Only a trailing marker is stripped; a file whose *name* contains
    /// the string mid-path must survive intact.
    #[test]
    fn strip_deleted_suffix_only_strips_trailing_marker() {
        assert_eq!(strip_deleted_suffix("/tmp/x (deleted)/img.iso"), "/tmp/x (deleted)/img.iso");
    }

    /// Build a synthetic `/sys/class/block` tree in a tempdir.
    ///
    /// Layout mirrors what the kernel exposes:
    /// ```text
    /// sda/                    whole disk, has device/model
    /// sda1/partition          a partition of it
    /// sda2/partition
    /// sdb/holders/dm-0        sdb is claimed by dm-0
    /// dm-0/dm/uuid            dm-0 is a LUKS volume
    /// dm-0/holders/dm-1       ...which is itself claimed by dm-1
    /// dm-1/dm/uuid
    /// loop0/loop/backing_file
    /// ```
    /// A synthetic sysfs tree that removes itself, including on panic.
    ///
    /// The cleanup used to be a trailing statement in each test, which
    /// runs only when the test passes. Every failing assertion therefore
    /// leaked a populated tree, and `cargo mutants` — which fails the
    /// suite deliberately, hundreds of times, each run under a new pid —
    /// left 466 of them in `/tmp` on this machine. That is not tidiness:
    /// a container with a small `/tmp` runs out of space and the next
    /// build fails with an error that looks nothing like its cause.
    struct TempTree {
        /// Root of the tree, removed when this drops.
        root: PathBuf,
    }

    impl TempTree {
        /// The root as a `&str`, for the `_in` variants.
        fn path(&self) -> &str {
            self.root.to_str().expect("temp path is UTF-8")
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            drop(fs::remove_dir_all(&self.root));
        }
    }

    fn fake_sysfs(tag: &str) -> TempTree {
        let root = std::env::temp_dir().join(format!(
            "imi-sysfs-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let mk = |p: &str| fs::create_dir_all(root.join(p)).unwrap();
        let wr = |p: &str, c: &str| fs::write(root.join(p), c).unwrap();

        // The kernel exposes a partition twice: as a top-level entry
        // (`/sys/class/block/sda1`) and as a child of its disk
        // (`/sys/class/block/sda/sda1`). `is_partition` consults the
        // former, `partitions_of` scans the latter, so the fixture has
        // to carry both or it tests something sysfs never looks like.
        mk("sda/device");
        wr("sda/device/model", "Ultra Fit  \n");
        for part in ["sda1", "sda2"] {
            mk(part);
            wr(&format!("{part}/partition"), "1\n");
            mk(&format!("sda/{part}"));
            wr(&format!("sda/{part}/partition"), "1\n");
        }
        // A non-partition child of the disk must be ignored by
        // `partitions_of` — this is the discriminator the scan exists for.
        mk("sda/queue");

        mk("sdb/holders/dm-0");
        mk("dm-0/dm");
        wr("dm-0/dm/uuid", "CRYPT-LUKS2-abc\n");
        mk("dm-0/holders/dm-1");
        mk("dm-1/dm");
        wr("dm-1/dm/uuid", "LVM-xyz\n");

        mk("loop0/loop");
        wr("loop0/loop/backing_file", "/tmp/disk.img\n");
        TempTree { root }
    }

    /// Partition detection gates Phase 0's refusal of `/dev/sda1`. Both
    /// directions matter: a disk must not look like a partition, and a
    /// partition must not look like a disk.
    /// Build a minimal, isolated tree for one test.
    ///
    /// The canonical `fake_sysfs` fixture models `/sys/class/block`,
    /// where partitions appear as top-level entries; `all_disk_knames`
    /// reads `/sys/block`, where they do not. Testing it against the
    /// canonical root would assert the wrong universe, so these tests
    /// build their own — through the same `TempTree` so cleanup and
    /// uniqueness come from one place.
    fn mini_tree(tag: &str, files: &[(&str, &str)]) -> TempTree {
        let root = std::env::temp_dir().join(format!(
            "imi-sysfs-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        for (rel, content) in files {
            let full = root.join(rel);
            fs::create_dir_all(full.parent().expect("relative path has a parent")).unwrap();
            fs::write(full, content).unwrap();
        }
        TempTree { root }
    }

    /// `/sys/block` enumeration returns every entry, sorted.
    #[test]
    fn all_disk_knames_lists_and_sorts_the_root() {
        let t =
            mini_tree("knames", &[("sdb/size", "1\n"), ("sda/size", "1\n"), ("vda/size", "1\n")]);
        let got = all_disk_knames_in(t.path()).unwrap();
        assert_eq!(got, ["sda", "sdb", "vda"], "sorted, complete");
    }

    /// Size is sectors-times-512, and both "absent" and "zero" are `None`.
    ///
    /// The zero case is a card reader with no card: a picker must treat
    /// "no medium" exactly like "no such attribute", so the reader folds
    /// them before policy ever sees a number.
    #[test]
    fn size_bytes_scales_sectors_and_folds_zero_to_none() {
        let t = mini_tree(
            "size",
            &[("sda/size", "7814037168\n"), ("empty/size", "0\n"), ("junk/size", "many\n")],
        );
        assert_eq!(size_bytes_in(t.path(), "sda"), Some(7_814_037_168 * 512));
        assert_eq!(size_bytes_in(t.path(), "empty"), None, "no medium reads as absent");
        assert_eq!(size_bytes_in(t.path(), "junk"), None, "unparseable reads as absent");
        assert_eq!(size_bytes_in(t.path(), "missing"), None);
    }

    /// Removable is display metadata: strict about "1", quiet about the rest.
    #[test]
    fn removable_is_true_only_for_a_literal_one() {
        let t = mini_tree(
            "removable",
            &[("usb/removable", "1\n"), ("nvme/removable", "0\n"), ("odd/removable", "yes\n")],
        );
        assert!(removable_in(t.path(), "usb"));
        assert!(!removable_in(t.path(), "nvme"));
        assert!(!removable_in(t.path(), "odd"), "anything but 1 is not removable");
        assert!(!removable_in(t.path(), "missing"), "absent attribute is not removable");
    }

    #[test]
    fn is_partition_distinguishes_disks_from_partitions() {
        let root = fake_sysfs("ispart");
        let r = root.path();
        assert!(is_partition_in(r, "sda1"), "sda1 has a partition file");
        assert!(is_partition_in(r, "sda2"));
        assert!(!is_partition_in(r, "sda"), "a whole disk must not be a partition");
        assert!(!is_partition_in(r, "nonexistent"));
    }

    /// Presence check used to validate knames before further lookups.
    #[test]
    fn exists_reports_known_block_devices() {
        let root = fake_sysfs("exists");
        let r = root.path();
        assert!(exists_in(r, "sda"));
        assert!(exists_in(r, "dm-0"));
        assert!(!exists_in(r, "sdz"));
    }

    /// `partitions_of` builds the devt set Phases 1, 2 and 7 scan
    /// against. It must return only children carrying a `partition`
    /// marker, sorted, and must not include the disk itself.
    #[test]
    fn partitions_of_lists_only_real_partitions() {
        let root = fake_sysfs("parts");
        let r = root.path();
        let parts = partitions_of_in(r, "sda").unwrap();
        assert_eq!(parts, vec!["sda1".to_owned(), "sda2".to_owned()]);

        // A disk with no partitions yields an empty list, not an error.
        assert!(partitions_of_in(r, "dm-0").unwrap().is_empty());
        // A device that does not exist is an error, not silence.
        partitions_of_in(r, "sdz").unwrap_err();
    }

    /// The transitive holder walk is what refuses LVM/dm-crypt/MD
    /// stacks. It must follow holders of holders, and must not include
    /// the starting device.
    #[test]
    fn holders_recursive_follows_the_whole_chain() {
        let root = fake_sysfs("holders");
        let r = root.path();
        let hs = holders_recursive_in(r, "sdb").unwrap();
        assert!(hs.contains("dm-0"), "direct holder must be found");
        assert!(hs.contains("dm-1"), "transitive holder must be found");
        assert!(!hs.contains("sdb"), "the starting device must not be included");
        assert_eq!(hs.len(), 2);

        // A device with no holders/ directory yields an empty set rather
        // than an error — synthetic and removed devices lack it.
        assert!(holders_recursive_in(r, "sda").unwrap().is_empty());
    }

    /// dm UUID classification distinguishes LUKS from LVM from non-dm.
    #[test]
    fn dm_uuid_reads_and_trims() {
        let root = fake_sysfs("dmuuid");
        let r = root.path();
        assert_eq!(dm_uuid_in(r, "dm-0").as_deref(), Some("CRYPT-LUKS2-abc"));
        assert_eq!(dm_uuid_in(r, "dm-1").as_deref(), Some("LVM-xyz"));
        assert_eq!(dm_uuid_in(r, "sda"), None, "a plain disk has no dm uuid");
    }

    /// The model string feeds the confirmation prompt and the replug
    /// identity check, and must be trimmed.
    #[test]
    fn device_model_is_trimmed_and_optional() {
        let root = fake_sysfs("model");
        let r = root.path();
        assert_eq!(device_model_in(r, "sda").as_deref(), Some("Ultra Fit"));
        assert_eq!(device_model_in(r, "dm-0"), None, "dm devices expose no model");
        assert!(has_backing_device_in(r, "sda"));
        assert!(!has_backing_device_in(r, "dm-0"));
    }

    /// Loop backing-file resolution feeds Phase 0's refusal to flash a
    /// loop device from its own backing file.
    #[test]
    fn loop_backing_file_resolves_and_trims() {
        let root = fake_sysfs("loopback");
        let r = root.path();
        assert_eq!(loop_backing_file_in(r, "loop0"), Some(PathBuf::from("/tmp/disk.img")));
        assert_eq!(loop_backing_file_in(r, "sda"), None);
    }
}
