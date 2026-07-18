//! Mount-topology inspection, swap handling, and unmount orchestration.
//!
//! All mount discovery goes through `/proc/self/mountinfo` (parsed by field,
//! not regex). Device correlation is belt-and-suspenders:
//!
//! 1. Field 3's `major:minor` — the filesystem's device number. For most
//!    filesystems (ext4, xfs, vfat, iso9660, …) this equals the backing
//!    block device's devt and is the canonical match.
//! 2. Field 10's source path, decoded and stat'ed, compared by `st_rdev`.
//!    This catches filesystems with *anonymous* device numbers — btrfs
//!    (and bcachefs, and any multi-device filesystem) reports a synthetic
//!    `0:NN` in field 3 that never appears in a sysfs-built devt set, so a
//!    btrfs partition mounted from the target would otherwise be invisible
//!    to every phase that scans mountinfo (including the Phase 7 sweep
//!    whose final verdict backs the SUCCESS line). The kernel writes the
//!    *original* mount-time source path into field 10 (possibly a
//!    `/dev/disk/by-uuid/...` symlink), which is exactly why we stat it
//!    and compare devts instead of comparing strings.
//!
//! Whitelist: we only auto-unmount mounts under `/media`, `/run/media`, or
//! `/var/run/media` (canonical udisks2 / systemd-logind removable-media
//! locations). Anything else we refuse to touch — if the operator has mounted
//! the USB on, say, `/mnt/work`, we'd rather abort than risk tearing down
//! something they might be editing.
//!
//! Unmounting is plain `umount2(target, 0)`, never `MNT_DETACH` and never
//! `MNT_FORCE`. A lazy detach removes the mount from the namespace — and
//! from `/proc/self/mountinfo` — *immediately*, while the filesystem stays
//! fully alive (and writable) through any open file descriptors, and the
//! kernel's block-device claim persists until the last fd closes. That
//! makes every subsequent mountinfo scan a false oracle: the Phase 1
//! residual re-scan would pass vacuously and the failure would resurface
//! as a misattributed `O_EXCL` `EBUSY`, and a Phase 7 sweep could declare
//! SUCCESS over a detached-but-still-writing mount. Plain umount gives the
//! honest signal: `EBUSY` means "something has files open — stop and tell
//! the operator", which is exactly what a destructive tool should do.
//! (`MNT_FORCE` is additionally useless here: on local filesystems it is
//! mostly a no-op; it exists for hung network filesystems.)

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use nix::mount::{MntFlags, umount2};

use crate::sysfs;

/// One parsed entry from `/proc/self/mountinfo`.
///
/// Correlation with the target is primarily via `(major, minor)` from
/// field 3 (the filesystem's device number). The decoded field-10 source
/// path is *also* retained so the filter can stat it and compare
/// `st_rdev` — the fallback that catches anonymous-devt filesystems
/// (btrfs et al.) whose field 3 is a synthetic `0:NN`. See the module
/// doc for the full rationale.
#[derive(Debug, Clone)]
pub(crate) struct MountInfo {
    /// Device number (major, minor) of the mounted filesystem.
    pub(crate) devt: (u64, u64),
    /// Mount-point target path (field 5), with octal escapes decoded.
    pub(crate) target: PathBuf,
    /// Mount source path (field 10), with octal escapes decoded. `None`
    /// when the line was truncated before field 10. May be a non-path
    /// pseudo-source (`tmpfs`, `sysfs`, …) — the stat-based comparison
    /// naturally ignores those.
    pub(crate) source: Option<PathBuf>,
}

/// Set of `(major, minor)` pairs that belong to the target disk — the disk
/// itself plus every partition currently visible under `/sys/class/block/`.
///
/// Built once per phase and reused for mountinfo / swaps filtering. The
/// inner set is private; the only supported access is via [`TargetDevts::contains`],
/// which keeps the invariant "this set was built from sysfs at construction
/// time and has not been mutated since" enforceable from the outside.
pub(crate) struct TargetDevts {
    /// The devt set, built once from sysfs at construction.
    set: HashSet<(u64, u64)>,
}

impl TargetDevts {
    /// Populate from a disk kernel name.
    pub(crate) fn from_disk(disk_kname: &str) -> Result<Self> {
        let mut set = HashSet::new();
        let disk_devt = read_devt_of(disk_kname)?;
        set.insert(disk_devt);
        for part in sysfs::partitions_of(disk_kname)? {
            let devt = read_devt_of(&part)
                .with_context(|| format!("reading devt for partition {part}"))?;
            set.insert(devt);
        }
        Ok(Self { set })
    }

    /// Membership test against the construction-time devt set.
    pub(crate) fn contains(&self, devt: (u64, u64)) -> bool {
        self.set.contains(&devt)
    }

    /// Test-only constructor that bypasses sysfs. Lets unit tests
    /// exercise `mounts_on_target` and the `contains` predicate without
    /// requiring a real block device on the test host.
    #[cfg(test)]
    fn from_set_for_test(set: HashSet<(u64, u64)>) -> Self {
        Self { set }
    }
}

/// Read `/sys/class/block/<kname>/dev` and parse the "major:minor" line.
fn read_devt_of(kname: &str) -> Result<(u64, u64)> {
    let p = format!("/sys/class/block/{kname}/dev");
    let s = fs::read_to_string(&p).with_context(|| format!("read {p}"))?;
    let s = s.trim();
    let (maj, min) = s.split_once(':').ok_or_else(|| anyhow!("malformed {p}: {s:?}"))?;
    let maj: u64 = maj.parse().with_context(|| format!("parse major in {p}"))?;
    let min: u64 = min.parse().with_context(|| format!("parse minor in {p}"))?;
    Ok((maj, min))
}

/// Decode `/proc`-style octal escapes in-place: `\040` = space, `\011` = tab,
/// `\012` = newline, `\134` = backslash. Used by both `/proc/self/mountinfo`
/// (field 5 — mount target) and `/proc/swaps` (column 1 — swap path), since
/// both use the same `seq_file_path()` encoding in the kernel.
///
/// The kernel only emits the four canonical escapes, but the generic decoder
/// (`\` + 3 octal digits → byte) handles any kernel that adds more.
///
/// Output is a `String` because both call-sites need string semantics for
/// downstream use (path comparison, prefix matching, `Path::new`-then-`stat`).
/// A non-UTF-8 byte sequence after decoding is converted lossily — invalid
/// bytes become `U+FFFD`, which then cannot match any whitelist root or
/// resolve to a real path. The lossy fallback is defence in depth; mountinfo
/// targets and swap paths under `/media`, `/dev/`, `/var/` etc. are
/// invariably valid UTF-8 in practice.
fn unescape_proc_octal(s: &str) -> String {
    /// Octal digit value of `b`, or `None` when `b` is not `'0'..='7'`.
    /// `u16` so the three-digit combination below cannot truncate.
    fn octal_val(b: u8) -> Option<u16> {
        u16::from(b).checked_sub(u16::from(b'0')).filter(|&v| v < 8)
    }

    let mut out = Vec::with_capacity(s.len());
    let mut rest = s.as_bytes();
    while let Some((&b, tail)) = rest.split_first() {
        if b == b'\\'
            && let [d0, d1, d2, after @ ..] = tail
            && let (Some(h), Some(m), Some(l)) = (octal_val(*d0), octal_val(*d1), octal_val(*d2))
        {
            // h, m, l are octal digits (<= 7), so the combined value is
            // at most 0o777 = 511 — bitwise ops on u16 cannot overflow.
            let v = (h << 6) | (m << 3) | l;
            // The kernel escapes single *bytes*, so only \000-\377
            // can legitimately appear. \400-\777 is corrupt or
            // adversarial input; passing it through literally is
            // more faithful than wrapping it into some other byte.
            if let Ok(byte) = u8::try_from(v) {
                out.push(byte);
                rest = after;
                continue;
            }
        }
        out.push(b);
        rest = tail;
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Parse `/proc/self/mountinfo` and filter to entries on the target disk.
pub(crate) fn mounts_on_target(target: &TargetDevts) -> Result<Vec<MountInfo>> {
    let text = fs::read_to_string("/proc/self/mountinfo").context("read /proc/self/mountinfo")?;
    Ok(filter_mountinfo(&text, target, block_rdev_of))
}

/// Stat `path` and return its `(major, minor)` iff it is a block-device
/// node. Regular files, directories, pseudo-sources (`tmpfs`, `sysfs`),
/// and vanished paths all yield `None` — a regular file's `st_rdev` is 0
/// and must never be compared against a real devt set.
pub(crate) fn block_rdev_of(path: &Path) -> Option<(u64, u64)> {
    let st = nix::sys::stat::stat(path).ok()?;
    if st.st_mode & libc::S_IFMT != libc::S_IFBLK {
        return None;
    }
    Some((nix::sys::stat::major(st.st_rdev), nix::sys::stat::minor(st.st_rdev)))
}

/// Parse-and-filter the body of `/proc/self/mountinfo`. Extracted from
/// [`mounts_on_target`] so the parsing pipeline is testable without
/// reading from `/proc`. Lines that fail to parse are silently skipped
/// (defensive — a malformed line in `/proc/self/mountinfo` is more
/// likely a kernel bug than an imi bug, and we'd rather lose one
/// mount entry than abort the flash).
///
/// A line matches when *either* its field-3 devt is in the target set
/// (the common case) *or* its field-10 source resolves — via the
/// injected `source_devt` resolver, which is stat-based in production
/// and a table in tests — to a devt in the set (the anonymous-devt
/// fallback for btrfs and friends).
fn filter_mountinfo<F>(text: &str, target: &TargetDevts, source_devt: F) -> Vec<MountInfo>
where
    F: Fn(&Path) -> Option<(u64, u64)>,
{
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(mi) = parse_mountinfo_line(line) {
            let by_devt = target.contains(mi.devt);
            let by_source =
                mi.source.as_deref().and_then(&source_devt).is_some_and(|d| target.contains(d));
            if by_devt || by_source {
                out.push(mi);
            }
        }
    }
    out
}

/// Find the mount whose target is the deepest path-prefix of `path`, and
/// return that mount's decoded source. This is the anonymous-devt fallback
/// used by the image-on-target check: when a file's `st_dev` does not
/// resolve to any block device (btrfs subvolumes report a synthetic
/// anonymous devt), the mount table still records which block device
/// backs the filesystem containing the file.
///
/// Prefix matching is component-wise (`Path::starts_with`), so
/// `/media-user/img.iso` does not match a mount at `/media`.
pub(crate) fn backing_source_for_path(path: &Path) -> Result<Option<PathBuf>> {
    let text = fs::read_to_string("/proc/self/mountinfo").context("read /proc/self/mountinfo")?;
    Ok(source_of_longest_mount_prefix(&text, path))
}

/// Pure core of [`backing_source_for_path`]: scan mountinfo text for the
/// deepest mount whose target is a component-wise prefix of `path`, and
/// return its source. Entries without a source (truncated lines) are
/// skipped — a deeper source-less entry must not shadow a shallower one
/// that does carry a source, so depth is only considered for candidates
/// that have one. At equal depth the *later* line wins: mountinfo lists
/// mounts in mount order, so for an overmounted target (same directory
/// mounted twice) the last entry is the currently-visible filesystem.
fn source_of_longest_mount_prefix(text: &str, path: &Path) -> Option<PathBuf> {
    let mut best: Option<(usize, PathBuf)> = None;
    for line in text.lines() {
        let Some(mi) = parse_mountinfo_line(line) else { continue };
        let Some(src) = mi.source else { continue };
        if !path.starts_with(&mi.target) {
            continue;
        }
        let depth = mi.target.components().count();
        if best.as_ref().is_none_or(|(d, _)| depth >= *d) {
            best = Some((depth, src));
        }
    }
    best.map(|(_, src)| src)
}

/// Parse one line of `/proc/self/mountinfo`.
///
/// Layout (see `man 5 proc`):
/// ```text
/// 36 35 98:0 /mnt1 /mnt/parent rw,noatime master:1 - ext3 /dev/root rw,errors=continue
/// (1)(2)(3)  (4)   (5)         (6)        (7..)    (-) (9) (10)     (11)
/// ```
/// We need field 3 (devt), field 5 (target), and field 10 (source, for
/// the anonymous-devt fallback). Optional fields end at `-`. A line
/// truncated after the fstype still parses, with `source: None`.
fn parse_mountinfo_line(line: &str) -> Option<MountInfo> {
    let mut it = line.split(' ');
    let _id = it.next()?;
    let _parent = it.next()?;
    let devt_str = it.next()?;
    let _root = it.next()?;
    let target_raw = it.next()?;
    let _mopts = it.next()?;
    // Skip zero-or-more optional fields until we hit "-".
    for tok in it.by_ref() {
        if tok == "-" {
            break;
        }
    }
    let _fstype = it.next()?;
    // Field 10 (source): decoded and retained for the stat-based fallback
    // match. Both field 5 and field 10 use the kernel's seq_file_path()
    // octal escaping.
    let source = it.next().map(|s| PathBuf::from(unescape_proc_octal(s)));
    // Parse devt.
    let (maj, min) = devt_str.split_once(':')?;
    let maj: u64 = maj.parse().ok()?;
    let min: u64 = min.parse().ok()?;
    Some(MountInfo {
        devt: (maj, min),
        target: PathBuf::from(unescape_proc_octal(target_raw)),
        source,
    })
}

/// Whitelist check. A mount is eligible for auto-unmount iff its target
/// is exactly one of the whitelisted roots or a strict descendant. The
/// trailing-slash sentinel distinguishes `/media/alice` (legitimate
/// descendant of `/media`) from `/media-user` (string-prefix lookalike
/// that is *not* a descendant).
///
/// Visibility is `pub(crate)` rather than private only so the unit-test
/// suite can cover a long table of accepted/rejected paths. No caller
/// outside `mount.rs` should reach for this — they should call
/// [`enforce_whitelist`], which produces the actionable error message.
pub(crate) fn is_whitelisted(target: &Path) -> bool {
    const ROOTS: &[&str] = &["/media", "/run/media", "/var/run/media"];
    let Some(s) = target.to_str() else { return false };
    for root in ROOTS {
        if s == *root {
            return true;
        }
        // Trailing-slash sentinel so `/media-user/...` does not match `/media`.
        let mut pfx = String::with_capacity(root.len().saturating_add(1));
        pfx.push_str(root);
        pfx.push('/');
        if s.starts_with(&pfx) {
            return true;
        }
    }
    false
}

/// Validate that every mount on the target lives under a whitelisted root.
/// Returns an error naming the first offender if any do not.
pub(crate) fn enforce_whitelist(mounts: &[MountInfo], dev_display: &Path) -> Result<()> {
    for m in mounts {
        if !is_whitelisted(&m.target) {
            bail!(
                "target '{}' is mounted at '{}' (not under /media, /run/media, /var/run/media). \
                 Refusing to auto-unmount potential system data. Unmount manually and retry.",
                dev_display.display(),
                m.target.display()
            );
        }
    }
    Ok(())
}

/// Inspect `/proc/swaps` for any entry whose source is a device node belonging
/// to the target, and call `swapoff(2)` on it.
///
/// Uses stat-based `(major, minor)` comparison rather than path-string matching
/// so that swaps activated through `/dev/disk/by-uuid/...`, `/dev/mapper/...`,
/// or any other symlink are caught.
pub(crate) fn disable_swaps_on_target(targets: &TargetDevts) -> Result<()> {
    let text = match fs::read_to_string("/proc/swaps") {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(anyhow!("read /proc/swaps: {e}")),
    };

    let mut lines = text.lines();
    let _header = lines.next();
    for line in lines {
        let Some(first) = line.split_whitespace().next() else { continue };
        // The kernel emits `/proc/swaps` paths through `seq_file_path()`,
        // the same encoder used for `/proc/self/mountinfo`. Spaces, tabs,
        // newlines, and backslashes are octal-escaped (`\040` etc.). We
        // must decode before passing to `stat`, otherwise a swap on a
        // path with a space (`/var/swap file.bin` → `/var/swap\040file.bin`
        // in /proc/swaps) is silently skipped, and Phase 2's O_EXCL open
        // then fails with a misleading "EBUSY (racing udisks2?)" message.
        let unescaped = unescape_proc_octal(first);
        // `/proc/swaps` lists file-backed swaps too (file `Type`), whose stat
        // would resolve to a filesystem, not a block device. Stat'ing and
        // comparing `st_rdev` safely handles both cases — a file-backed swap
        // will have `st_rdev == 0` and never match.
        // A swap entry pointing at a vanished path is skipped.
        let Ok(st) = nix::sys::stat::stat(Path::new(&unescaped)) else { continue };
        let swap_devt = (nix::sys::stat::major(st.st_rdev), nix::sys::stat::minor(st.st_rdev));
        if !targets.contains(swap_devt) {
            continue;
        }
        eprintln!(" -> Disabling swap on {unescaped}…");
        let cpath = std::ffi::CString::new(unescaped.as_bytes())
            .context("swap path contained interior NUL")?;
        // SAFETY: `swapoff` takes a NUL-terminated C string and is safe to
        // call with any valid path. `CString::new` ensures NUL-termination.
        // Errors surface via the returned `errno` and do not touch our memory.
        let rc = unsafe { libc::swapoff(cpath.as_ptr()) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            bail!("swapoff({unescaped}) failed: {err}");
        }
    }
    Ok(())
}

/// Unmount every whitelisted mount on the target with plain
/// `umount2(target, 0)`, deepest-first. Hard-fails on the first refusal.
///
/// Deliberately **no** `MNT_DETACH` and **no** `MNT_FORCE` (see module
/// doc): a lazy detach clears mountinfo instantly while the filesystem
/// stays alive through open fds and the kernel claim persists — turning
/// the Phase 1 residual re-scan into a false oracle and converting an
/// actionable "target is busy" into a misleading `O_EXCL` `EBUSY` later.
/// `EBUSY` here means a process has files open on the mount; that is a
/// pre-destruction refusal the operator can act on.
pub(crate) fn unmount_all(mounts: &[MountInfo]) -> Result<()> {
    // Sort by target-path length descending so nested mounts go first.
    let mut sorted: Vec<&MountInfo> = mounts.iter().collect();
    sorted.sort_by_key(|m| std::cmp::Reverse(m.target.as_os_str().len()));

    for m in &sorted {
        eprintln!(" -> Unmounting {}…", m.target.display());
        umount2(&m.target, MntFlags::empty()).with_context(|| {
            format!(
                "unmounting {} failed — a process likely has files open on it \
                 (file manager, indexer, shell cwd). Close them and retry. \
                 imi refuses to lazy-detach: a detached mount vanishes from \
                 mount tables while staying active through open files.",
                m.target.display()
            )
        })?;
    }
    Ok(())
}

/// Classification of a device-mapper holder, derived from its
/// `/sys/class/block/<kname>/dm/uuid` string. The DM subsystem
/// embeds a prefix identifying the target type — `LVM-`, `CRYPT-`,
/// `mpath-`, `DMRAID-` for known live-data cases, and `part-`
/// (kpartx partition mappings) as the *only* known-synthetic case.
///
/// The classifier is **default-deny**: any prefix not in the table —
/// including future DM target types (`VDO-`, integrity/writecache
/// setups) and plain `dmsetup create` devices with an empty UUID —
/// classifies as [`DmHolderKind::Unknown`], which is treated as live
/// data. Enumerating *bad* types is how new stack types slip through;
/// enumerating the one known-harmless type cannot rot the same way.
///
/// Used internally by [`reject_active_stacked_volumes`]; exposed at
/// `pub(crate)` only so the unit tests can exercise the prefix table
/// without spinning up a real DM stack on the test host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DmHolderKind {
    /// LVM logical volume — destroying the underlying disk would lose
    /// data the operator has no other copy of.
    Lvm,
    /// dm-crypt / LUKS volume — destroying the underlying disk would
    /// lose the only intelligible copy of the encrypted data.
    Crypt,
    /// Device-mapper multipath aggregator — destroying any one path
    /// breaks the active multipath group.
    Mpath,
    /// dm-raid array — destroying a member breaks the array.
    DmRaid,
    /// `part<N>-*` kpartx partition mappings — purely synthetic views
    /// that carry no data of their own.
    Synthetic,
    /// Anything else, including an empty UUID. Fail closed: an
    /// unrecognized DM target sitting on the disk is assumed to carry
    /// live data until the operator tears it down themselves.
    Unknown,
}

impl DmHolderKind {
    /// True if this holder type carries (or must be presumed to carry)
    /// live data we must not destroy without explicit operator action.
    /// Everything except the known-synthetic `part-*` mapping.
    fn is_live_data(self) -> bool {
        !matches!(self, Self::Synthetic)
    }

    /// Operator-readable label for the FATAL error message.
    fn label(self) -> &'static str {
        match self {
            Self::Lvm => "LVM",
            Self::Crypt => "CRYPT",
            Self::Mpath => "mpath",
            Self::DmRaid => "DMRAID",
            Self::Synthetic => "synthetic (kpartx)",
            Self::Unknown => "unrecognized device-mapper",
        }
    }
}

/// Classify a DM-UUID string by its prefix. The prefix is everything
/// before the first `-`; if there is no `-`, the entire string is
/// treated as the prefix. Unlisted prefixes — and the empty string —
/// classify as [`DmHolderKind::Unknown`] (live data, default-deny).
///
/// kpartx partition mappings are the one synthetic case, and their UUID
/// shape is `part<N>-<parent-uuid>` (kpartx formats it as `part%d-%s`),
/// so the pre-dash prefix is `part1`, `part2`, … — never bare `part`.
/// The matcher therefore requires `part` followed by one or more ASCII
/// digits; anything else (including bare `part`) stays default-deny.
pub(crate) fn classify_dm_uuid(uuid: &str) -> DmHolderKind {
    let prefix = uuid.split_once('-').map_or(uuid, |(p, _)| p);
    if let Some(digits) = prefix.strip_prefix("part")
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
    {
        return DmHolderKind::Synthetic;
    }
    match prefix {
        "LVM" => DmHolderKind::Lvm,
        "CRYPT" => DmHolderKind::Crypt,
        "mpath" => DmHolderKind::Mpath,
        "DMRAID" => DmHolderKind::DmRaid,
        _ => DmHolderKind::Unknown,
    }
}

/// Reject the flash if the target has any holder that carries — or must
/// be presumed to carry — live data: LVM, dm-crypt, multipath, dm-raid,
/// *any unrecognized DM target*, MD RAID, or any non-DM stacking driver
/// we cannot classify (bcache, drbd, …). The only holder accepted
/// silently is the known-synthetic kpartx `part<N>-*` mapping.
///
/// Walks holders recursively so that a two-level stack (LVM-on-LUKS) is
/// rejected the same way as a direct LUKS-on-disk.
pub(crate) fn reject_active_stacked_volumes(disk_kname: &str) -> Result<()> {
    let mut roots = vec![disk_kname.to_owned()];
    roots.extend(sysfs::partitions_of(disk_kname)?);

    for root in &roots {
        // Sorted so the "first offender" named in the error is
        // deterministic run-to-run (HashSet iteration order is not).
        let mut holders: Vec<String> = sysfs::holders_recursive(root)?.into_iter().collect();
        holders.sort();
        for holder in &holders {
            let Some(uuid) = sysfs::dm_uuid(holder) else {
                // Non-DM holder. Name MD explicitly (the common case);
                // everything else is an unclassifiable stacking driver
                // (bcache, drbd, a future subsystem) — fail closed and
                // tell the operator to detach it themselves.
                if holder.starts_with("md") {
                    bail!(
                        "target disk '{disk_kname}' has an active MD raid holder ({holder}); aborting"
                    );
                }
                bail!(
                    "target disk '{disk_kname}' has an active block-layer holder '{holder}' \
                     that imi cannot classify (bcache/drbd/other stacking driver?). \
                     Detach it manually before flashing; refusing to guess."
                );
            };
            let kind = classify_dm_uuid(&uuid);
            if kind.is_live_data() {
                bail!(
                    "target disk '{disk_kname}' has an active {} holder ({holder}); \
                     aborting to avoid destroying stacked storage",
                    kind.label()
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- unescape_proc_octal --------------------------------------------

    #[test]
    fn unescape_passes_through_plain_ascii() {
        assert_eq!(unescape_proc_octal("/media/usb"), "/media/usb");
        assert_eq!(unescape_proc_octal(""), "");
        assert_eq!(unescape_proc_octal("a"), "a");
    }

    #[test]
    fn unescape_decodes_kernel_canonical_escapes() {
        assert_eq!(unescape_proc_octal("\\040"), " "); // space
        assert_eq!(unescape_proc_octal("\\011"), "\t"); // tab
        assert_eq!(unescape_proc_octal("\\012"), "\n"); // newline
        assert_eq!(unescape_proc_octal("\\134"), "\\"); // backslash
    }

    #[test]
    fn unescape_handles_escapes_at_string_boundaries() {
        // Escape at exact end-of-string — the decoder's 3-digit window
        // (`[d0, d1, d2, after @ ..]` against the tail) must match when
        // the escape's last digit is the final byte; this guards against
        // a regression that requires trailing bytes after the escape.
        assert_eq!(unescape_proc_octal("\\040"), " ");
        assert_eq!(unescape_proc_octal("/var/swap\\040file"), "/var/swap file");
        assert_eq!(unescape_proc_octal("\\040prefix"), " prefix");
        assert_eq!(unescape_proc_octal("suffix\\012"), "suffix\n");
    }

    #[test]
    fn unescape_passes_through_malformed_escapes() {
        // Non-octal digits after backslash: leave the backslash literal,
        // resume scanning from the next byte.
        assert_eq!(unescape_proc_octal("\\xyz"), "\\xyz");
        assert_eq!(unescape_proc_octal("\\9"), "\\9"); // 9 is not in 0..=7
        assert_eq!(unescape_proc_octal("\\08"), "\\08"); // 8 is not in 0..=7
        // Truncated escape at end-of-string (fewer than 3 digits
        // available): the backslash stays literal.
        assert_eq!(unescape_proc_octal("\\"), "\\");
        assert_eq!(unescape_proc_octal("\\0"), "\\0");
        assert_eq!(unescape_proc_octal("\\01"), "\\01");
    }

    /// `8` and `9` are decimal digits but not octal ones: a backslash
    /// followed by three characters that include them is not an escape
    /// and must pass through literally. (Kills the mutant that widens
    /// `octal_val`'s range check from `< 8` to `< 10`.)
    #[test]
    fn unescape_rejects_non_octal_digits_in_window() {
        assert_eq!(unescape_proc_octal("\\091"), "\\091");
        assert_eq!(unescape_proc_octal("\\190"), "\\190");
        assert_eq!(unescape_proc_octal("\\009"), "\\009");
    }

    /// Escapes above `\377` cannot encode a byte; the kernel never emits
    /// them (it escapes single bytes). Corrupt or adversarial `\4xx`-`\7xx`
    /// sequences must pass through literally rather than wrap into some
    /// unrelated byte value.
    #[test]
    fn unescape_passes_through_out_of_range_octal() {
        assert_eq!(unescape_proc_octal("\\740"), "\\740");
        assert_eq!(unescape_proc_octal("\\777"), "\\777");
        // Boundary: \377 (= 0xFF) is the largest legitimate escape. The
        // decoded byte is not valid UTF-8 on its own, so the lossy
        // fallback maps it to U+FFFD — the important property is that it
        // *was* decoded (not passed through as literal text).
        assert_eq!(unescape_proc_octal("\\377"), "\u{FFFD}");
    }

    // -- is_whitelisted -------------------------------------------------

    #[test]
    fn whitelist_accepts_canonical_roots() {
        for r in ["/media", "/run/media", "/var/run/media"] {
            assert!(is_whitelisted(Path::new(r)), "{r} should be allowed");
        }
    }

    #[test]
    fn whitelist_accepts_descendants() {
        assert!(is_whitelisted(Path::new("/media/alice/Ubuntu")));
        assert!(is_whitelisted(Path::new("/run/media/bob/USB1")));
        assert!(is_whitelisted(Path::new("/var/run/media/c/x/y/z")));
    }

    #[test]
    fn whitelist_rejects_lookalike_prefixes() {
        // The trailing-slash sentinel is the whole reason `is_whitelisted`
        // builds an explicit `<root>/` prefix instead of using
        // `starts_with(root)`. Without the sentinel, "/media-user" would
        // pass — and that mountpoint has nothing to do with /media.
        assert!(!is_whitelisted(Path::new("/media-user")));
        assert!(!is_whitelisted(Path::new("/media-foo/bar")));
        assert!(!is_whitelisted(Path::new("/run/media-x")));
        assert!(!is_whitelisted(Path::new("/var/run/media-y")));
    }

    #[test]
    fn whitelist_rejects_unrelated_paths() {
        assert!(!is_whitelisted(Path::new("/")));
        assert!(!is_whitelisted(Path::new("/mnt")));
        assert!(!is_whitelisted(Path::new("/mnt/work")));
        assert!(!is_whitelisted(Path::new("/home/alice")));
        assert!(!is_whitelisted(Path::new("/tmp")));
    }

    // -- parse_mountinfo_line -------------------------------------------

    #[test]
    fn parse_mountinfo_basic() {
        // Real example from `/proc/self/mountinfo`, no optional fields.
        let line = "36 35 98:0 / /mnt/parent rw,noatime - ext3 /dev/root rw,errors=continue";
        let mi = parse_mountinfo_line(line).expect("parse should succeed");
        assert_eq!(mi.devt, (98, 0));
        assert_eq!(mi.target, PathBuf::from("/mnt/parent"));
    }

    #[test]
    fn parse_mountinfo_with_optional_fields() {
        // Optional fields between mount-options and the `-` separator.
        let line = "36 35 8:1 / /media/usb rw shared:1 master:2 - ext4 /dev/sdb1 rw";
        let mi = parse_mountinfo_line(line).expect("parse should succeed");
        assert_eq!(mi.devt, (8, 1));
        assert_eq!(mi.target, PathBuf::from("/media/usb"));
    }

    #[test]
    fn parse_mountinfo_decodes_target_escapes() {
        // Mount target containing a space (encoded by the kernel as \040).
        let line = "36 35 8:1 / /media/My\\040USB rw - ext4 /dev/sdb1 rw";
        let mi = parse_mountinfo_line(line).expect("parse should succeed");
        assert_eq!(mi.target, PathBuf::from("/media/My USB"));
    }

    #[test]
    fn parse_mountinfo_rejects_truncated() {
        // Fewer than the required leading fields — should return None
        // rather than panic.
        assert!(parse_mountinfo_line("").is_none());
        assert!(parse_mountinfo_line("36").is_none());
        assert!(parse_mountinfo_line("36 35 8:1").is_none());
        assert!(parse_mountinfo_line("36 35 8:1 / /mnt").is_none());
    }

    #[test]
    fn parse_mountinfo_rejects_malformed_devt() {
        // Field 3 must parse as `<u64>:<u64>`.
        let bad_devt = "36 35 not-a-devt / /mnt rw - ext4 /dev/sdb1 rw";
        assert!(parse_mountinfo_line(bad_devt).is_none());
        let bad_minor = "36 35 8:notnum / /mnt rw - ext4 /dev/sdb1 rw";
        assert!(parse_mountinfo_line(bad_minor).is_none());
    }

    // -- TargetDevts::contains -------------------------------------------

    #[test]
    fn target_devts_contains_recognises_members() {
        let mut set = HashSet::new();
        set.insert((8, 32)); // sdc disk
        set.insert((8, 33)); // sdc1
        set.insert((8, 34)); // sdc2
        let devts = TargetDevts::from_set_for_test(set);
        assert!(devts.contains((8, 32)));
        assert!(devts.contains((8, 33)));
        assert!(devts.contains((8, 34)));
    }

    #[test]
    fn target_devts_contains_rejects_non_members() {
        let mut set = HashSet::new();
        set.insert((8, 32));
        let devts = TargetDevts::from_set_for_test(set);
        // Different disk on the same major (e.g. sdb is (8, 16)).
        assert!(!devts.contains((8, 16)));
        // Different major (NVMe is 259).
        assert!(!devts.contains((259, 0)));
        // Virtual filesystems (devt = (0, N)).
        assert!(!devts.contains((0, 22)));
    }

    #[test]
    fn target_devts_contains_handles_empty_set() {
        let devts = TargetDevts::from_set_for_test(HashSet::new());
        assert!(!devts.contains((8, 32)));
        assert!(!devts.contains((0, 0)));
    }

    // -- enforce_whitelist -----------------------------------------------

    fn mi(devt: (u64, u64), target: &str) -> MountInfo {
        MountInfo { devt, target: PathBuf::from(target), source: None }
    }

    #[test]
    fn enforce_whitelist_passes_when_all_under_media() {
        let mounts = vec![mi((8, 33), "/media/user/OS"), mi((8, 34), "/run/media/alice/USB")];
        let dev = PathBuf::from("/dev/sdc");
        enforce_whitelist(&mounts, &dev).unwrap();
    }

    #[test]
    fn enforce_whitelist_passes_on_empty_input() {
        let mounts: Vec<MountInfo> = vec![];
        let dev = PathBuf::from("/dev/sdc");
        enforce_whitelist(&mounts, &dev).unwrap();
    }

    #[test]
    fn enforce_whitelist_rejects_first_offender() {
        // /mnt/work is not whitelisted; we must refuse.
        let mounts = vec![mi((8, 33), "/media/user/OS"), mi((8, 34), "/mnt/work")];
        let dev = PathBuf::from("/dev/sdc");
        let err = enforce_whitelist(&mounts, &dev).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("/mnt/work"), "error must name the offending mount, got: {msg}");
        assert!(msg.contains("/dev/sdc"), "error must name the target device, got: {msg}");
    }

    #[test]
    fn enforce_whitelist_rejects_lookalike_prefix() {
        // /media-user is not under /media/ — the trailing-slash sentinel
        // logic in is_whitelisted prevents this from matching, and
        // enforce_whitelist must surface that as an error.
        let mounts = vec![mi((8, 33), "/media-user/sneaky")];
        let dev = PathBuf::from("/dev/sdc");
        assert!(enforce_whitelist(&mounts, &dev).is_err());
    }

    // -- filter_mountinfo (integration of parse + filter) ----------------

    /// Realistic /proc/self/mountinfo — virtual filesystems plus the
    /// real `KitOS` USB stick mounted at /media/user/OS. Only the
    /// (8, 33) entry should survive filtering when the target set is
    /// the /dev/sdc devts.
    #[test]
    fn filter_mountinfo_picks_matching_partition() {
        let text = "\
24 29 0:22 / /sys rw,nosuid,nodev,noexec,relatime shared:6 - sysfs sysfs rw
25 29 0:23 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw
26 29 0:6 / /dev rw,nosuid,relatime shared:2 - devtmpfs udev rw,size=8093912k,nr_inodes=2023478,mode=755,inode64
63 29 8:33 / /media/user/OS ro,nosuid,nodev,relatime shared:105 - iso9660 /dev/sdc1 ro,nojoliet,check=s,map=n
203 28 0:61 / /run/user/1000 rw,nosuid,nodev,relatime shared:484 - tmpfs tmpfs rw,size=1631288k
";
        let mut set = HashSet::new();
        set.insert((8, 32)); // /dev/sdc
        set.insert((8, 33)); // /dev/sdc1
        let target = TargetDevts::from_set_for_test(set);

        let mounts = filter_mountinfo(text, &target, |_| None);

        // Exactly the KitOS partition should match.
        assert_eq!(mounts.len(), 1, "expected exactly one match, got {mounts:?}");
        assert_eq!(mounts[0].devt, (8, 33));
        assert_eq!(mounts[0].target, PathBuf::from("/media/user/OS"));
    }

    /// Empty input → empty output. Trivially correct, but worth a
    /// regression guard against someone "optimising" the loop in a way
    /// that panics on the no-data case.
    #[test]
    fn filter_mountinfo_empty_input_yields_empty_output() {
        let target = TargetDevts::from_set_for_test(HashSet::new());
        assert!(filter_mountinfo("", &target, |_| None).is_empty());
    }

    /// Lines that fail to parse are silently dropped, not errors. A
    /// malformed entry in /proc/self/mountinfo is more likely a kernel
    /// bug than something we can recover from; skipping it is the
    /// least-bad behavior because aborting the flash on a kernel
    /// quirk would be worse than missing one mount entry.
    #[test]
    fn filter_mountinfo_skips_unparseable_lines() {
        let text = "\
this is not a mountinfo line at all
36 35 8:33 / /media/usb rw - ext4 /dev/sdb1 rw
also garbage
";
        let mut set = HashSet::new();
        set.insert((8, 33));
        let target = TargetDevts::from_set_for_test(set);
        let mounts = filter_mountinfo(text, &target, |_| None);
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].target, PathBuf::from("/media/usb"));
    }

    /// Multiple matches preserved in order of appearance. The `unmount_all`
    /// caller resorts internally by target-path length, so this property
    /// isn't strictly required *today* — but a future caller iterating
    /// the result directly would want stable, source-order semantics.
    /// Documenting and testing it locks the contract.
    #[test]
    fn filter_mountinfo_preserves_multiple_matches() {
        let text = "\
36 35 8:33 / /media/usb1 rw - ext4 /dev/sdc1 rw
37 35 8:34 / /media/usb2 rw - ext4 /dev/sdc2 rw
38 35 0:6 / /dev rw - devtmpfs udev rw
39 35 8:35 / /media/usb3 rw - ext4 /dev/sdc3 rw
";
        let mut set = HashSet::new();
        set.insert((8, 33));
        set.insert((8, 34));
        set.insert((8, 35));
        let target = TargetDevts::from_set_for_test(set);
        let mounts = filter_mountinfo(text, &target, |_| None);
        assert_eq!(mounts.len(), 3);
        assert_eq!(mounts[0].devt, (8, 33));
        assert_eq!(mounts[1].devt, (8, 34));
        assert_eq!(mounts[2].devt, (8, 35));
    }

    // -- classify_dm_uuid -----------------------------------------------

    /// Each canonical live-data prefix maps to its holder kind. These are
    /// the exact strings the DM subsystem emits in `/sys/.../dm/uuid`;
    /// a typo in the prefix table here would mean we silently allow
    /// flashing onto an active LVM/LUKS disk — exactly the failure
    /// mode the function exists to prevent.
    #[test]
    fn dm_uuid_classifies_live_data_prefixes() {
        // Real-world UUID shapes (real DM uses a hex/uuid suffix after
        // the dash; the helper only inspects the prefix).
        assert_eq!(classify_dm_uuid("LVM-abcd1234efab5678"), DmHolderKind::Lvm);
        assert_eq!(classify_dm_uuid("CRYPT-LUKS2-9f8a-disk-of-truth"), DmHolderKind::Crypt);
        assert_eq!(classify_dm_uuid("mpath-360a98000aabbccddeeff"), DmHolderKind::Mpath);
        assert_eq!(classify_dm_uuid("DMRAID-isw_xxyyzz-array0"), DmHolderKind::DmRaid);
    }

    /// The one known-synthetic shape — kpartx partition mappings,
    /// `part<N>-<parent-uuid>` — classifies as Synthetic. This table is
    /// the contract with kpartx's `part%d-%s` UUID format: the digits
    /// are part of the pre-dash prefix, so a bare `part-` (which kpartx
    /// never emits) does NOT qualify.
    #[test]
    fn dm_uuid_classifies_kpartx_partition_mappings() {
        assert_eq!(classify_dm_uuid("part1-mpath-3600a098000aabbcc"), DmHolderKind::Synthetic);
        assert_eq!(
            classify_dm_uuid("part12-CRYPT-LUKS2-9f8a-disk-of-truth"),
            DmHolderKind::Synthetic
        );
        // Bare `part` / `part-…` is not a shape kpartx emits; default-deny.
        assert_eq!(classify_dm_uuid("part-1-on-loop0"), DmHolderKind::Unknown);
        assert_eq!(classify_dm_uuid("part"), DmHolderKind::Unknown);
        // Non-digit after `part` is some other target type; default-deny.
        assert_eq!(classify_dm_uuid("partition-x"), DmHolderKind::Unknown);
        assert_eq!(classify_dm_uuid("partx1-y"), DmHolderKind::Unknown);
    }

    /// Everything unrecognized — unknown prefixes, the empty string,
    /// dash-less strings — falls to Unknown, which `is_live_data`
    /// treats as live. This is the default-deny contract: a DM target
    /// type we have never heard of must be rejected, not waved through.
    #[test]
    fn dm_uuid_default_denies_unknown_prefixes() {
        assert_eq!(classify_dm_uuid("FOOBAR-anything"), DmHolderKind::Unknown);
        assert_eq!(classify_dm_uuid("VDO-abcdef"), DmHolderKind::Unknown);
        // Plain `dmsetup create` devices carry an empty UUID.
        assert_eq!(classify_dm_uuid(""), DmHolderKind::Unknown);
        // No dash at all → the whole string is the prefix → Unknown.
        assert_eq!(classify_dm_uuid("standalone"), DmHolderKind::Unknown);
    }

    /// The matcher is case-sensitive on purpose. The kernel/userland
    /// emit specific cases for these prefixes (LVM and CRYPT uppercase,
    /// mpath lowercase, DMRAID uppercase). Under default-deny this is
    /// belt-and-suspenders rather than load-bearing: a wrong-case
    /// prefix no longer slips through as benign — it classifies as
    /// Unknown and is rejected with the "unrecognized" label instead
    /// of the specific one. The assertions below pin that behaviour.
    #[test]
    fn dm_uuid_classification_is_case_sensitive() {
        assert_eq!(classify_dm_uuid("lvm-lower"), DmHolderKind::Unknown);
        assert_eq!(classify_dm_uuid("crypt-lower"), DmHolderKind::Unknown);
        assert_eq!(classify_dm_uuid("MPATH-upper"), DmHolderKind::Unknown);
        assert_eq!(classify_dm_uuid("dmraid-lower"), DmHolderKind::Unknown);
        assert_eq!(classify_dm_uuid("PART1-upper"), DmHolderKind::Unknown);
    }

    /// The `is_live_data` predicate must agree with the default-deny
    /// classification: every kind except the known-synthetic kpartx
    /// mapping is live data — including Unknown.
    #[test]
    fn dm_holder_kind_is_live_data_predicate() {
        assert!(DmHolderKind::Lvm.is_live_data());
        assert!(DmHolderKind::Crypt.is_live_data());
        assert!(DmHolderKind::Mpath.is_live_data());
        assert!(DmHolderKind::DmRaid.is_live_data());
        assert!(DmHolderKind::Unknown.is_live_data());
        assert!(!DmHolderKind::Synthetic.is_live_data());
    }

    /// Each kind has a distinct, non-empty operator-readable label.
    /// Catches a regression where someone accidentally collapses two
    /// kinds to the same string in the FATAL message.
    #[test]
    fn dm_holder_kind_labels_are_distinct_and_nonempty() {
        let labels = [
            DmHolderKind::Lvm.label(),
            DmHolderKind::Crypt.label(),
            DmHolderKind::Mpath.label(),
            DmHolderKind::DmRaid.label(),
            DmHolderKind::Synthetic.label(),
            DmHolderKind::Unknown.label(),
        ];
        let unique: HashSet<&&str> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "labels must be distinct: {labels:?}");
        assert!(labels.iter().all(|l| !l.is_empty()), "no label may be empty");
    }

    // -- field-10 source capture and the anonymous-devt fallback ---------

    /// `parse_mountinfo_line` captures the decoded field-10 source. The
    /// source uses the same `seq_file_path()` octal escaping as the target.
    #[test]
    fn parse_mountinfo_captures_decoded_source() {
        let plain = "63 29 8:33 / /media/user/OS ro shared:105 - iso9660 /dev/sdc1 ro";
        let mi = parse_mountinfo_line(plain).expect("parse should succeed");
        assert_eq!(mi.source, Some(PathBuf::from("/dev/sdc1")));

        let escaped = "63 29 0:38 / /media/u/BTR rw - btrfs /dev/My\\040Disk rw,subvol=/";
        let mi_escaped = parse_mountinfo_line(escaped).expect("parse should succeed");
        assert_eq!(mi_escaped.source, Some(PathBuf::from("/dev/My Disk")));
    }

    /// A line truncated right after the fstype still parses (devt/target
    /// intact) with `source: None` — degraded, not dropped.
    #[test]
    fn parse_mountinfo_tolerates_missing_source() {
        let line = "63 29 8:33 / /media/user/OS ro - iso9660";
        let mi = parse_mountinfo_line(line).expect("parse should succeed");
        assert_eq!(mi.devt, (8, 33));
        assert_eq!(mi.source, None);
    }

    /// The btrfs case that motivated the fallback: field 3 carries an
    /// anonymous `0:NN` that is never in a sysfs-built devt set, but the
    /// field-10 source stat-resolves to the target partition. With a
    /// resolver that models the stat, the mount is matched; with a
    /// resolver that cannot resolve it (device vanished, pseudo-source),
    /// it is not.
    #[test]
    fn filter_mountinfo_matches_anonymous_devt_via_source() {
        let text = "\
24 29 0:22 / /sys rw shared:6 - sysfs sysfs rw
70 29 0:38 / /media/user/BTRFS rw shared:120 - btrfs /dev/sdc1 rw,subvol=/
";
        let mut set = HashSet::new();
        set.insert((8, 32)); // /dev/sdc
        set.insert((8, 33)); // /dev/sdc1
        let target = TargetDevts::from_set_for_test(set);

        // Stat-model: /dev/sdc1 is a block node at (8, 33); the sysfs
        // pseudo-source resolves to nothing.
        let resolver = |p: &Path| (p == Path::new("/dev/sdc1")).then_some((8_u64, 33_u64));
        let mounts = filter_mountinfo(text, &target, resolver);
        assert_eq!(mounts.len(), 1, "btrfs mount must match via source: {mounts:?}");
        assert_eq!(mounts[0].devt, (0, 38));
        assert_eq!(mounts[0].target, PathBuf::from("/media/user/BTRFS"));

        // Without source resolution the anonymous devt stays invisible —
        // this pins the *reason* the fallback exists.
        let unresolved = filter_mountinfo(text, &target, |_| None);
        assert!(unresolved.is_empty(), "0:38 must not match by devt alone");
    }

    /// A source that resolves to a devt *outside* the set must not match
    /// (another disk's partition mounted nearby is not our business).
    #[test]
    fn filter_mountinfo_ignores_foreign_sources() {
        let text = "70 29 0:38 / /media/user/OTHER rw - btrfs /dev/sdd1 rw\n";
        let mut set = HashSet::new();
        set.insert((8, 33));
        let target = TargetDevts::from_set_for_test(set);
        let mounts = filter_mountinfo(text, &target, |_| Some((8, 49)));
        assert!(mounts.is_empty());
    }

    // -- source_of_longest_mount_prefix ------------------------------------

    /// The deepest mount whose target prefixes the path wins — a file on
    /// a nested mount is backed by the nested mount's source, not the
    /// parent's.
    #[test]
    fn longest_prefix_picks_deepest_mount() {
        let text = "\
29 1 8:2 / / rw - ext4 /dev/sda2 rw
63 29 0:38 / /media/user rw - btrfs /dev/sdc1 rw,subvol=/
70 63 0:44 / /media/user/inner rw - btrfs /dev/sdd1 rw,subvol=/
";
        let src_inner =
            source_of_longest_mount_prefix(text, Path::new("/media/user/inner/img.iso"));
        assert_eq!(src_inner, Some(PathBuf::from("/dev/sdd1")));

        let src_user = source_of_longest_mount_prefix(text, Path::new("/media/user/img.iso"));
        assert_eq!(src_user, Some(PathBuf::from("/dev/sdc1")));

        // Anything else falls through to the root mount.
        let src_root = source_of_longest_mount_prefix(text, Path::new("/home/user/img.iso"));
        assert_eq!(src_root, Some(PathBuf::from("/dev/sda2")));
    }

    /// Prefix matching is component-wise: `/media-user/...` must not
    /// match a mount at `/media` (same sentinel property the whitelist
    /// enforces, inherited here from `Path::starts_with`).
    #[test]
    fn longest_prefix_is_component_wise() {
        let text = "63 29 0:38 / /media rw - btrfs /dev/sdc1 rw\n";
        let lookalike = source_of_longest_mount_prefix(text, Path::new("/media-user/img.iso"));
        assert_eq!(lookalike, None);
        let descendant = source_of_longest_mount_prefix(text, Path::new("/media/img.iso"));
        assert_eq!(descendant, Some(PathBuf::from("/dev/sdc1")));
    }

    /// Mounts without a source (truncated lines) are skipped rather than
    /// shadowing a shallower mount that does carry one.
    #[test]
    fn longest_prefix_skips_sourceless_entries() {
        let text = "\
29 1 8:2 / / rw - ext4 /dev/sda2 rw
63 29 0:38 / /media rw - btrfs
";
        let src = source_of_longest_mount_prefix(text, Path::new("/media/img.iso"));
        assert_eq!(src, Some(PathBuf::from("/dev/sda2")));
    }

    /// Overmount tie-break: the same target mounted twice appears as two
    /// mountinfo lines; the *later* one is the currently-visible
    /// filesystem, so its source must win at equal depth.
    #[test]
    fn longest_prefix_prefers_later_line_on_overmount() {
        let text = "\
63 29 8:33 / /media/x rw - ext4 /dev/sdc1 rw
71 29 0:44 / /media/x rw - btrfs /dev/sdd1 rw,subvol=/
";
        let src = source_of_longest_mount_prefix(text, Path::new("/media/x/img.iso"));
        assert_eq!(src, Some(PathBuf::from("/dev/sdd1")));
    }
}
