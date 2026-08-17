# 01 — Phase 0: Pre-Flight Validation

**Source:** `crates/imi-core/src/phases/phase_0.rs` (the `ensure_*` family, geometry
capture, `DeviceIdentity`), `crates/imi-core/src/phases/phase_0.rs::run`,
`crates/imi-core/src/common/sysfs.rs`, `crates/imi-core/src/common/image.rs::detect_compression`.

**Purpose:** Reject flatly impossible or obviously-wrong inputs before
acquiring any locks or touching the destructive pipeline. Every check
here can fail fast with no recovery cost.

## Phase 0 is silent on success

Pre-flight prints nothing when everything is well-formed. The first
visible output the user sees on a successful run is either the TTY
confirmation prompt or `Wiping partition signatures...` (Phase 3). This
matches the bash original's contract and keeps the operator-facing log
focused on actions that actually mutated the device.

## Where each refusal lives

Most of Phase 0 needs a real device, so the decisions that can be pure
are kept pure and unit-tested; `run` does the I/O and calls them. When
adding a check, follow the same split (see _Keeping safety decisions
testable_ in `AGENTS.md`).

| decision                                   | function                     |
| ------------------------------------------ | ---------------------------- |
| image and target are the same path         | `ensure_distinct_paths`      |
| device below the head+tail wipe floor      | `ensure_device_large_enough` |
| raw image larger than the device           | `ensure_image_fits`          |
| target is a block device                   | `ensure_block_device`        |
| image is a regular file                    | `ensure_image_is_regular_file` |
| target is physical or a loop device        | `ensure_physical_or_loop`    |
| image is the target's own backing file     | `is_same_file`               |
| may we destroy this device                 | `confirm_destruction`        |

The size figure on the banner moved to the binary with the banner
itself (`crates/imi/src/ui.rs::human_size`, which scales to the largest
binary unit that stays readable rather than always using GiB), and its
test went with it.

What remains device-bound is the *plumbing*, not the decisions:
`ensure_whole_disk` and `ensure_image_not_on_target` read sysfs and then
call the pure checks above; `query_dev_geometry_readonly` and
`target_block_subtree` need a live fd. Those wrappers are covered by the
root-gated suites, and their surviving mutants are catalogued in
`AGENTS.md` under the coverage baseline.

Note that a root-gated test does **not** kill a mutant: `cargo mutants`
runs the suite without `--ignored`. If a check matters, it has to be
reachable from a plain `cargo test` — which is why each of these was
extracted.

## Checks performed (in order)

### 1. Effective UID is 0

```rust
nix::unistd::Uid::effective().is_root()
```

We need root for `BLKGETSIZE64`, `O_EXCL` on a block device, `umount2`,
`swapoff`, and `BLKRRPART`. Failing fast prevents partial state where
some checks pass on a normal user before the first privileged syscall
errors out.

### 2. Canonicalize both paths

```rust
std::fs::canonicalize(&config.img)?;
std::fs::canonicalize(&config.dev)?;
```

`canonicalize` resolves symlinks and normalizes `..` / `.`. Crucially it
follows symlinks under `/dev/disk/by-id/`, `/dev/disk/by-label/`, and
`/dev/disk/by-uuid/` — the `--dev` argument the operator typed may not
look like the same path the kernel knows internally.

This is also where we catch nonexistent paths: `canonicalize` returns
`ENOENT`, which surfaces with the contextual message
`canonicalize image path /tmp/foo`.

Right after the same-path check below, the image must be a **regular
file** (`ensure_image_is_regular_file`) — same-path runs first because
"image and device are the same path" is the sharper diagnosis for that
particular typo. The `-i` argument is opened
and streamed wholesale in Phase 4, so this is the image-side
counterpart of the device-identity chain: a typo that lands on a
device node (`-i /dev/zero` would stream zeros until the capacity
check trips — after the target has been overwritten), a FIFO (blocks
before the confirmation prompt ever appears), or a directory is
refused here with a message naming the actual file type.

### 3. Same-path check

```rust
if img_canon == dev_canon { bail!(...) }
```

After canonicalization, identical paths mean the operator passed the same
device or symlink for both `--img` and `--dev`. Almost always a typo,
always destructive — refuse.

### 4. Block-device check

```rust
let st = nix::sys::stat::stat(path)?;
if st.st_mode & libc::S_IFMT != libc::S_IFBLK { bail!(...) }
```

The `S_IFMT` mask isolates the file-type bits. We require `S_IFBLK`
exactly. `/dev/null` has `st_mode = 0o020666` (`S_IFCHR`); the bash
original failed open here because shell `[ -b "$DEV" ]` only checks the
block bit. We do the same.

### 5. Whole-disk check

```rust
sysfs::is_partition(&kname)
  // → /sys/class/block/<kname>/partition exists?
```

Partitions expose a `partition` file (containing the partition number);
whole disks do not. Refusing partitions prevents the operator from
accidentally writing only to `/dev/sdb1` and leaving the rest of the
disk's partition table intact — which would produce a half-written,
unbootable result.

Loop devices are accepted (matching the bash original); they expose
neither `partition` nor a parent disk and are explicitly used in
testing.

Beyond partitions, **virtual whole-node devices are rejected**: `dm-*`
(an open LUKS mapping!), `md*`, `zram*`, `ram*` are "not a partition"
yet are exactly the wrong target for a USB flasher — an
unmounted-but-open dm-crypt view carries no kernel claim, so `O_EXCL`
would succeed and the flash would destroy the encrypted volume's
contents through the mapping. The gate: a physical disk exposes a
`device` link under `/sys/class/block/<kname>/`; loop devices (which
lack it) are allow-listed by name.

### 6. Image-on-target ancestry check

This is the load-bearing "are you flashing the disk you booted from?"
guard.

```text
1. Stat the image file → st_dev (a dev_t).
2. Resolve st_dev to a kernel name via /sys/dev/block/<maj>:<min>.
   FALLBACK: btrfs (and any anonymous-devt filesystem) reports a
   synthetic st_dev with no sysfs entry. When step 2 fails, walk
   /proc/self/mountinfo for the deepest mount containing the image
   path, decode its field-10 source, stat it, and use its st_rdev.
   Only if both routes fail do we conclude "non-block backend".
3. Build the target's "block subtree", iterated to a fixpoint:
     - target disk kname
     - every partition of target (from /sys/class/block/<disk>/*)
     - every recursive holder of any subtree member
       (from /sys/class/block/<member>/holders/)
     - every loop device whose loop/backing_file resolves (via the
       same step-1/2 logic) to a subtree member — loop content IS
       target content — plus that loop's partitions and holders
4. If the image's backing kname appears anywhere in the subtree, abort.
```

The recursive holder walk is what catches the LVM-on-LUKS-on-`/dev/sdb`
case: the image lives on `/dev/mapper/vg-home`, which sits on
`/dev/mapper/cryptroot`, which sits on `/dev/sdb2`. A single `PKNAME` hop
would miss it. The walk follows `holders/` symlinks transitively with a
visited-set guard against pathological sysfs layouts.

### 7. Compression detection

`image::detect_compression` reads the first 8 bytes of `--img` and
matches against magic signatures:

| Magic               | Compression |
| ------------------- | ----------- |
| `1F 8B`             | gzip        |
| `FD 37 7A 58 5A 00` | xz          |
| `42 5A 68`          | bzip2       |
| `28 B5 2F FD`       | zstd        |
| (otherwise)         | raw         |

Before falling through to `raw`, the prefix is checked against archive
containers this crate cannot decode — zip (`50 4B 03 04`, `05 06`,
`07 08`), 7-zip, lz4, rar, lzip, lzop — and those are **refused**.

That refusal is not tidiness. `raw` means "write these bytes verbatim",
and Phase 5 reopens the image with the same `Compression` the `Target`
carries, so it re-reads the archive through the same raw path, the
comparison passes, and the operator is told SUCCESS over a device
holding a `.zip`. The failure is silent in both directions.

Only unambiguous magics are listed. A disk image starts with a boot
sector — `EB`, `FA` or `33` — so none can collide with one. `.lzma`
(`5D 00 00`) is deliberately absent: three bytes, two of them zero, is
not distinctive enough to refuse a flash over.

All four decompressed formats accept **multi-member / multi-stream
input** (two or more independently-compressed streams concatenated):
`MultiGzDecoder`, `MultiBzDecoder`, `XzDecoder::new_multi_decoder`,
and zstd's native multi-frame handling. This is not an edge case —
**pbzip2 output is always multi-stream**, and pigz / `cat a.gz b.gz`
produce multi-member gzip. The single-member decoders stop silently at
the first member boundary, flashing a truncated image that Phase 5b
then _blesses_ (it re-reads through the same truncating decoder).
Regression-pinned by the `multi_member_*_decodes_in_full` tests in
`image.rs`; execution-verified against loop devices. One consequence:
trailing non-stream garbage after the last member is now an error
rather than silently ignored — fail-closed and honest.

Detection here is load-bearing for the next step: if the image is
compressed we _cannot_ perform a capacity check upfront because we don't
know the decompressed size. We rely on `ENOSPC` handling in Phase 4
instead. If the image is raw, we compare its `len()` against
`BLKGETSIZE64` and fail fast.

### 8. Device size query (`BLKGETSIZE64`)

```rust
ioctl_read_bad!(blkgetsize64, request_code_read!(0x12, 114, size_of::<libc::size_t>()), u64);
```

Opens the device read-only (no `O_EXCL` yet — we don't have it locked
until Phase 2) and reads the size in bytes via the ioctl. The `_IOR`
request code is computed from the target's `size_t`, since the kernel
encodes that width into the number and handles both — see
`10-aligned-and-ioctls.md`. Used both for the upfront capacity check and
for Phase 3's tail-wipe offset.

> **Manual test log.** The Phase 0 defenses were exercised live against
> loop devices (privileged container, kernel loop driver): regular-file
> image check (`-i` pointed at a directory / missing path → clean
> refusal naming the type), backing-file identity check (loop target
> flashed from its own backing file and from a hardlink to it → refusal;
> pre-fix this silently corrupted the head and printed SUCCESS),
> undersized-device floor (1 MiB loop → clean refusal, no FATAL), and
> `BLKROGET` (read-only `losetup -r` attachment → refusal). Full
> happy-path runs verified byte-exact content, wipe persistence, and
> every FATAL verb (wipe / write / cooldown / verify interruptions).

The size is also checked against the wipe's `2 * WIPE_REGION` floor
here rather than only in Phase 3: by Phase 3 the guard is armed, so a
too-small device would earn the "inconsistent state" FATAL warning
despite never having been touched. Phase 3 keeps its own bound as
defense in depth.

The same read-only FD also runs `BLKROGET`: a write-protected device
(hardware RO switch, `blockdev --setro`) is refused _here_, not
discovered as `EPERM` at the Phase 3 wipe with the guard already armed
— which would print the "device inconsistent" FATAL warning for a
device that was never touched.

Phase 0 finally snapshots the device identity as a `DeviceIdentity`:
`st_rdev`, the sysfs model, the `BLKGETSIZE64` size, and the sysfs
`wwid` and `serial`. Phase 2 re-verifies every field against the
_claimed FD_ after the `O_EXCL` open, closing the replug TOCTOU (same
`/dev` name, different physical stick between the prompt and the claim).

The last two were added because the first three do not discriminate in
the case that actually happens. Two sticks of the same make and capacity
landing on the same minor are equal in all of `rdev`, `model` and
`size` — and an operator is far likelier to own two identical sticks
than two different ones. `serial` comes from SCSI VPD page 0x80, which
is vendor-defined and widely duplicated on cheap media; `wwid` comes
from page 0x83, which is unique by design. Either differing means a
different device, and `None` must still be `None` at re-check. See
`03-phase-2-exclusive.md`.

### 9. Confirmation

If `Config::yes` was not set, Phase 0 assembles an `imi_core::Summary`
— device path, model from `/sys/class/block/<kname>/device/model`, size,
image path, compression, raw size — and calls `Events::confirm` with it.
A `false` answer aborts with `aborted by user` before anything is
touched.

**Phase 0 does not prompt.** It has no terminal: the default `confirm`
**refuses**, so an unattended caller passing a silent sink must set
`Config::yes` or the run stops here. That default is deliberate — a
caller that has not considered the question must not be able to destroy
a disk by omission.

The prompt itself lives in the binary, in `crates/imi/src/ui.rs`, and
keeps the guarantees this section used to describe:

- `/dev/tty` is opened _explicitly_ for read **and** write — not stdin,
  not stdout. The read side prevents `echo yes | imi ...` from bypassing
  the prompt (the exact failure mode that turns a typo into a destroyed
  root drive). The write side keeps `Type 'yes' to proceed:` visible even
  when the rest of the output is redirected to a log file.
- Only the literal string `yes` is accepted, after trimming. Not `y`,
  not `YES` — a prompt that accepts near-misses trains people to answer
  without reading. Pinned by `response_approves`.

## What Phase 0 does not check

- File-system writability of the image file. We open it read-only later;
  permission errors at Phase 4 surface as `Permission denied`.
- Whether the device is "really" a USB stick vs. an internal SSD. The
  whole-disk + TTY-confirmation gate is the only line of defence here.
  We do not parse `/sys/class/block/<kname>/removable` because some USB
  enclosures lie about it and some legitimately removable internal
  drives are reported as non-removable.
- Whether the image is bootable / has a valid partition table. That is
  the operator's responsibility.

## Manual test

```sh
# Ought to fail at the same-path check.
sudo ./target/release/imi -i /dev/sda -d /dev/sda

# Ought to fail at the block-device check.
sudo ./target/release/imi -i /tmp/img.iso -d /dev/null

# Ought to fail at whole-disk check. Confirmed on hardware 2026-08-02:
#   error: /dev/sdc1 is a partition (sdc1), not a whole disk.
#          Pass the base device (e.g. /dev/sdb, not /dev/sdb1).
# This one cannot be reproduced against a plain loop device — the loop
# driver creates no partition nodes without `max_part` — so it went
# unexercised until a real partitioned stick was available.
sudo ./target/release/imi -i /tmp/img.iso -d /dev/sda1

# Ought to fail at image-on-target check (image on the disk being flashed).
sudo ./target/release/imi -i /mnt/sdb/img.iso -d /dev/sdb
```
