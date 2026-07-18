//! Sysfs helpers.
//!
//! Everything here is pure filesystem lookup under `/sys/class/block/<name>/`
//! and `/sys/dev/block/<major>:<minor>`. No ioctls, no child processes.
//!
//! Functions use `String` for kernel names rather than `OsString`/`PathBuf`
//! because kernel names are ASCII by construction (`sda`, `sdb1`, `dm-0`,
//! `nvme0n1p2`, `loop0`, …). If we ever see a non-UTF-8 name, something else
//! is already very wrong and returning an error is the right move.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

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
    let link = format!("{SYS_DEV_BLOCK}/{major}:{minor}");
    let target = fs::read_link(&link).with_context(|| format!("readlink {link}"))?;
    target
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("no basename in sysfs link target: {}", target.display()))
}

/// True if the kernel name refers to a partition (has a `partition` file
/// under `/sys/class/block/<kname>/`).
pub(crate) fn is_partition(kname: &str) -> bool {
    Path::new(&format!("{SYS_CLASS_BLOCK}/{kname}/partition")).exists()
}

/// True if the kernel name refers to a block device known to sysfs.
pub(crate) fn exists(kname: &str) -> bool {
    Path::new(&format!("{SYS_CLASS_BLOCK}/{kname}")).exists()
}

/// List the partitions of a whole disk, returned as kernel names.
/// Scans `/sys/class/block/<disk>/` for child directories containing a
/// `partition` file.
pub(crate) fn partitions_of(disk_kname: &str) -> Result<Vec<String>> {
    let dir = format!("{SYS_CLASS_BLOCK}/{disk_kname}");
    let mut out = Vec::new();
    let read = fs::read_dir(&dir).with_context(|| format!("read_dir {dir}"))?;
    for entry in read {
        let entry = entry.with_context(|| format!("read_dir entry under {dir}"))?;
        let name = match entry.file_name().to_str() {
            Some(s) => s.to_owned(),
            None => continue,
        };
        let part_marker = entry.path().join("partition");
        if part_marker.exists() {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// Read `/sys/class/block/<kname>/dm/uuid` if present. DM devices embed a
/// prefix identifying the target type: `LVM-`, `CRYPT-`, `mpath-`, `DMRAID-`,
/// `part-`, etc. Returns `None` for non-DM devices.
pub(crate) fn dm_uuid(kname: &str) -> Option<String> {
    let p = format!("{SYS_CLASS_BLOCK}/{kname}/dm/uuid");
    fs::read_to_string(p).ok().map(|s| s.trim().to_owned())
}

/// Recursively collect the transitive closure of `holders/` for a given
/// device. Each entry in `holders/` is a symlink to another `/sys/class/block`
/// node; we follow its basename.
///
/// Does *not* include `root_kname` itself — add separately if you want it.
pub(crate) fn holders_recursive(root_kname: &str) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    walk_holders(root_kname, &mut out)?;
    Ok(out)
}

/// DFS over `holders/` links, accumulating every transitive holder.
fn walk_holders(kname: &str, acc: &mut HashSet<String>) -> Result<()> {
    let dir = format!("{SYS_CLASS_BLOCK}/{kname}/holders");
    let read = match fs::read_dir(&dir) {
        Ok(r) => r,
        // `holders/` may not exist on some synthetic/removed devices; treat
        // as "no holders" rather than propagating.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(anyhow!("read_dir {dir}: {e}")),
    };
    for entry in read {
        let entry = entry.with_context(|| format!("read_dir entry under {dir}"))?;
        let child = match entry.file_name().to_str() {
            Some(s) => s.to_owned(),
            None => continue,
        };
        if acc.insert(child.clone()) {
            // Only recurse if we haven't already visited this node. Prevents
            // infinite loops on pathological sysfs layouts (shouldn't happen
            // in practice, but cheap insurance).
            walk_holders(&child, acc)?;
        }
    }
    Ok(())
}

/// Read `/sys/class/block/<kname>/device/model` if present, trimmed.
/// Used only for the confirmation prompt.
pub(crate) fn device_model(kname: &str) -> Option<String> {
    let p = format!("{SYS_CLASS_BLOCK}/{kname}/device/model");
    fs::read_to_string(p).ok().map(|s| s.trim().to_owned())
}

/// True if `/sys/class/block/<kname>/device` exists — i.e. the kernel name
/// is backed by a physical device (`sd*`, `nvme*n*`, `mmcblk*`, `vd*`, …).
///
/// Virtual and stacked nodes — `dm-*`, `md*`, `zram*`, `ram*` — expose no
/// `device` link. Loop devices also lack it and are allow-listed separately
/// by the caller (they are the sanctioned test target).
pub(crate) fn has_backing_device(kname: &str) -> bool {
    Path::new(&format!("{SYS_CLASS_BLOCK}/{kname}/device")).exists()
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
    let p = format!("{SYS_CLASS_BLOCK}/{kname}/loop/backing_file");
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
    use super::strip_deleted_suffix;

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
}
