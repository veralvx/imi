# 01 — Phase 0: Pre-Flight Validation

**Source:** `src/preflight.rs` (the `ensure_*` family, geometry
capture, `DeviceIdentity`), `src/main.rs::run` (Phase 0 call order),
`src/sysfs.rs`, `src/image.rs::detect_compression`.

**Purpose:** Reject flatly impossible or obviously-wrong inputs before
acquiring any locks or touching the destructive pipeline. Every check
here can fail fast with no recovery cost.

## Phase 0 is silent on success

Pre-flight prints nothing when everything is well-formed. The first
visible output the user sees on a successful run is either the TTY
confirmation prompt or `Wiping partition signatures...` (Phase 3). This
matches the bash original's contract and keeps the operator-facing log
focused on actions that actually mutated the device.

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
std::fs::canonicalize(&cli.img)?;
std::fs::canonicalize(&cli.dev)?;
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
ioctl_read!(blkgetsize64, 0x12, 114, u64);
```

Opens the device read-only (no `O_EXCL` yet — we don't have it locked
until Phase 2) and reads the size in bytes via the ioctl. `_IOR` request
encoding via `ioctl_read!` macro. Used both for the upfront capacity
check and for Phase 3's tail-wipe offset.

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

Phase 0 finally snapshots the device identity — `st_rdev`, sysfs model,
and the `BLKGETSIZE64` size — as a `DeviceIdentity`. Phase 2 re-verifies
all three against the _claimed FD_ after the `O_EXCL` open, closing the
replug TOCTOU (same `/dev` name, different physical stick between the
prompt and the claim). See `03-phase-2-exclusive.md`.

### 9. TTY confirmation

If `--yes` was not passed:

- Print device summary (model from `/sys/class/block/<kname>/device/model`,
  size, compression format, image path) to stdout.
- Open `/dev/tty` _explicitly_ for read **and** write — not stdin, not
  stdout. The read side prevents `echo yes | imi ...` from bypassing
  the prompt (the exact failure mode that turns a typo into a destroyed
  root drive). The write side keeps the `Type 'yes' to proceed:` prompt
  visible even when the rest of the program's output has been redirected
  to a log file.
- Require the literal string `yes\n`. Anything else aborts.

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

# Ought to fail at whole-disk check.
sudo ./target/release/imi -i /tmp/img.iso -d /dev/sda1

# Ought to fail at image-on-target check (image on the disk being flashed).
sudo ./target/release/imi -i /mnt/sdb/img.iso -d /dev/sdb
```
