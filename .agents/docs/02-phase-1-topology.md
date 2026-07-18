# 02 — Phase 1: Topology Audit, Swap Disable, Unmounting

**Source:** `src/mount.rs`, `src/sysfs.rs::holders_recursive`,
`src/main.rs::run` (Phase 1 block).

**Purpose:** Bring the device into a state where the Phase 2 `O_EXCL`
claim will succeed without disturbing user data, and where no stacked
storage subsystem is going to react badly to the device disappearing
underneath it.

## Why Phase 1 must precede the `O_EXCL` open

Linux's `O_EXCL` on a block device is a kernel-enforced exclusive claim
implemented in `bd_prepare_to_claim()` (block/bdev.c). It rejects the
open with `EBUSY` if the block device or any of its partitions already
has any claim — including a mounted filesystem. So the Phase 2 open
_cannot_ succeed if Phase 1 hasn't already cleared the way:

1. Stacked-volume holders rejected (active LVM/LUKS/MD/DMRAID).
2. Active swaps disabled.
3. Whitelisted mounts unmounted.
4. Mountinfo re-scanned to confirm no residuals.

The bash original called `umount` _after_ opening — and routinely got
spurious failures because the open had already failed silently.

## Step 1 — Stacked-volume rejection

`mount::reject_active_stacked_volumes(disk_kname)` walks
`/sys/class/block/<disk>/holders/` and `/sys/class/block/<disk><N>/holders/`
recursively. Each holder is inspected:

```rust
let uuid = fs::read_to_string("/sys/class/block/<holder>/dm/uuid")?;
let kind = classify_dm_uuid(&uuid);   // pure prefix-classifier
if kind.is_live_data() { abort }
```

The DM UUID prefix identifies the holder type. The prefix table is
encoded in `mount::classify_dm_uuid` (a pure function unit-tested
in `mount::tests`), so changes to the table can be regression-tested
without spinning up real DM stacks:

| Prefix         | Type                   | Action                    |
| -------------- | ---------------------- | ------------------------- |
| `LVM-`         | LVM logical volume     | abort                     |
| `CRYPT-`       | dm-crypt / LUKS        | abort                     |
| `mpath-`       | multipath              | abort                     |
| `DMRAID-`      | dmraid                 | abort                     |
| `part<N>-`     | kpartx synthetic       | OK (the only benign case) |
| anything else  | unknown DM target      | abort (default-deny)      |
| (empty UUID)   | plain `dmsetup create` | abort (default-deny)      |
| (no `dm/uuid`) | not a DM device        | `md*` → abort as MD RAID; |
|                |                        | otherwise abort as an     |
|                |                        | unclassifiable holder     |
|                |                        | (bcache / drbd / …)       |

The classifier is **default-deny**: only the known-synthetic kpartx
shape is accepted, and that shape is `part<N>-<parent-uuid>` (kpartx
formats the UUID as `part%d-%s`, so the digits are part of the
pre-dash prefix — a bare `part-` never occurs and is rejected).
Enumerating _bad_ types is how new stack types
(dm-vdo, dm-integrity, a future subsystem) slip through; enumerating
the one known-harmless type cannot rot the same way. Non-DM holders
that aren't MD are likewise rejected by name — the operator detaches
them; we don't guess. (The bash original missed MD because MD doesn't
expose `dm/uuid`; naming it separately keeps that lesson visible.)

The recursion is essential. A two-level stack (LVM-on-LUKS-on-disk) has
the LVM holder as a child of the LUKS holder, not a direct child of the
disk. The walk uses a visited-set to prevent loops.

### Why we abort instead of tearing down

Tearing down LVM means `lvchange -an`, which means executing a binary
(forbidden by directive 1 — see `AGENTS.md`). Tearing down LUKS means
`cryptsetup luksClose`, same problem. Tearing down MD means `mdadm
--stop`, same problem. We could in principle issue the equivalent
ioctls/dm-control commands directly, but DM teardown of an active
volume _with mounted filesystems on top_ is a footgun even when done
correctly. Refusing forces the operator to explicitly tear down the
stack themselves before flashing. That preserves their agency over
their own LVM volumes.

## Step 2 — Mount inventory

`mount::mounts_on_target(&devts)` parses `/proc/self/mountinfo`
field-by-field per `man 5 proc`:

```
36 35 98:0 /mnt1 /mnt/parent rw,noatime master:1 - ext3 /dev/root rw,errors=continue
(1)(2)(3)  (4)   (5)         (6)        (7..)    (-) (9) (10)     (11)
```

We extract field 3 (`devt` as `major:minor`) and field 5 (mount target).
Optional fields between 6 and `-` are skipped. Field 5 has octal escape
sequences (`\040`, `\011`, `\012`, `\134`) which we decode in
`unescape_proc_octal`. The same decoder is used for `/proc/swaps`
paths in Step 4 — both files use the kernel's `seq_file_path()`
encoding, so a single helper covers both call-sites.

### Critical: correlation by `(major, minor)` — belt _and_ suspenders

Mountinfo line 10 (the source) is _the path the user gave at mount
time_. It may be `/dev/disk/by-uuid/01234567-...`. Matching string
prefixes against `/dev/sdb` would silently miss those mounts. So the
primary match is by devt: we stat each subtree member, populate a
`HashSet<(u64,u64)>` once, and compare against field 3.

But field 3 alone is **not sufficient**. btrfs — and any filesystem
with anonymous device numbers — reports a synthetic `0:NN` in field 3
that never appears in a sysfs-built devt set, making a btrfs partition
mounted from the target invisible to every mountinfo scan (including
the Phase 7 sweep whose final verdict backs the SUCCESS line). The
filter therefore _also_ decodes field 10, stats it, and matches by
`st_rdev` — never by string. Both routes feed the same set-membership
test; a mount matches if either does.

## Step 3 — Whitelist enforcement

```rust
const ROOTS: &[&str] = &["/media", "/run/media", "/var/run/media"];
```

A mount target is acceptable iff it equals one of those roots exactly,
or starts with `<root>/` (note the trailing slash — without it,
`/media-user/foo` would match `/media`). These are the canonical
removable-media locations for udisks2, GNOME, KDE, and systemd-logind.

Anything outside the whitelist gets a refusal. If the user has the
device mounted on `/mnt/work` they may have unsaved changes; we do not
unmount it for them. They get a message naming the offending mount and
instructions to unmount manually.

## Step 4 — Swap disable

For each line of `/proc/swaps`, we decode kernel-octal escapes in the
path, stat it, and compare `st_rdev` against the target devt set:

```rust
let unescaped = unescape_proc_octal(first);  // \040 → space, etc.
let st = nix::sys::stat::stat(Path::new(&unescaped))?;
let swap_devt = (
    nix::sys::stat::major(st.st_rdev),
    nix::sys::stat::minor(st.st_rdev),
);
if !targets.contains(swap_devt) { continue; }
libc::swapoff(cpath.as_ptr());
```

The decode step is load-bearing. The kernel emits `/proc/swaps` paths
through `seq_file_path()`, the same encoder that handles
`/proc/self/mountinfo`. A swap file at `/var/swap file.bin` appears
in `/proc/swaps` as `/var/swap\040file.bin`; calling `stat` on the
literal escaped string returns `ENOENT`, the swap is silently skipped,
and Phase 2's `O_EXCL` open then fails with a misleading "EBUSY (racing
udisks2?)" message. We use the same `unescape_proc_octal` helper as
the mountinfo parser.

Same devt-comparison reason as mounts: swap activated via
`/dev/disk/by-uuid/...` would be missed by string matching. File-backed
swap has `st_rdev = 0` and never matches a block-device target.

`swapoff` is via raw `libc`; `nix` does not currently wrap it.

## Step 5 — Plain unmount, hard-fail

Whitelisted mounts are sorted by target-path length descending so nested
mounts are unmounted first (e.g. `/media/foo/bar` before `/media/foo`),
then unmounted with plain `umount2(target, 0)`. The first refusal aborts
the run with the mountpoint and errno.

Deliberately **no `MNT_DETACH` and no `MNT_FORCE`**. A lazy detach
"succeeds" by removing the mount from the namespace — and from
`/proc/self/mountinfo` — immediately, while the filesystem stays fully
alive (and writable) through any open file descriptors, and the kernel's
block-device claim persists until the last fd closes. That makes Step 6's
re-scan a false oracle: it passes vacuously, and the real failure
resurfaces later as a misattributed `O_EXCL` `EBUSY` ("racing
udisks2?") while the operator's mount has been yanked out of the mount
table where they could have found it. Plain umount's `EBUSY` is the
honest signal — "a process has files open on this mount" — delivered
before anything destructive, with the mount still visible and fixable.
(`MNT_FORCE` is not an escalation for local filesystems at all; it
exists for hung network filesystems, and the bash original never used
it — that used plain `umount` too.)

## Step 6 — Re-scan

After unmount we call `mounts_on_target` again. If anything is still
present, we abort _before_ attempting `O_EXCL` — better to surface
"unmount didn't take" as a clean error than as a confusing `EBUSY`
later.

## Manual test

```sh
# Set up:
sudo mkfs.ext4 /dev/sdb1
sudo mount /dev/sdb1 /media/test            # whitelisted location
sudo swapon /dev/sdb2                       # if available

# Run:
sudo ./target/release/imi -i img.iso -d /dev/sdb -y

# Should produce a successful Phase 1 with:
#   -> Disabling swap on /dev/sdb2…
#   -> Unmounting /media/test…
# and proceed to Phase 2.
```

```sh
# Negative test — non-whitelisted mount:
sudo mount /dev/sdb1 /mnt/danger
sudo ./target/release/imi -i img.iso -d /dev/sdb -y
# Should refuse with:
#   target '/dev/sdb' is mounted at '/mnt/danger' (not under /media, ...)
```
