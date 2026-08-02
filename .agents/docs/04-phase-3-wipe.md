# 04 — Phase 3: Signature Wipe

**Source:** `crates/imi-core/src/phases/phase_3.rs::wipe_ends`,
`crates/imi-core/src/phases/phase_3.rs::run`,
`crates/imi-core/src/common/geometry.rs` (`WIPE_REGION`).

**Purpose:** Destroy the existing partition table and filesystem
superblocks at the head and tail of the device before writing the new
image. This prevents the kernel and userspace tools from picking up
stale metadata that the image's contents do not overlap.

## What gets wiped

```rust
// common/geometry.rs — Phase 0 reads it for the size floor too, which
// is why it is not in this phase.
pub(crate) const WIPE_REGION: u64 = 1024 * 1024; // 1 MiB

write_all_at(&zeros, 0)?;                        // head
write_all_at(&zeros, dev_size - WIPE_REGION)?;   // tail
fdatasync(guard.file())?;                        // AsFd-based since nix 0.30
```

- 1 MiB at offset 0: kills the MBR (LBA 0), the protective MBR + primary
  GPT (LBAs 0–33), and almost every filesystem superblock that places
  itself at or near the start of the volume (ext2/3/4, xfs, btrfs
  primary, F2FS, NTFS, FAT32 BPB, exFAT, ISO 9660 PVD at LBA 16).
- 1 MiB at `dev_size - 1 MiB`: kills the backup GPT header (LBA-1) and
  backup partition entries (LBA-2..-33).

## What does _not_ get wiped

A 1 MiB head wipe is not equivalent to `wipefs -a`. Several filesystems
place secondary or auxiliary superblocks deeper into the volume:

- **btrfs** keeps three superblocks, at 64 KiB, 64 MiB and 256 GiB
  (`btrfs_sb_offset` in `fs/btrfs/disk-io.h`, with
  `BTRFS_SUPER_MIRROR_MAX` of 3). Only the first is inside the head
  wipe; the other two survive it.
- **ZFS** has labels at the start, end, and quarter-points.
- **bcache** has a superblock at 8 KiB into each cached/backing device.

The Phase 4 image write will overwrite anything within its own
footprint, so these residual signatures are only a concern for
filesystems whose format-specific tooling scans for them outside the
ISO's data range. In practice we accept this gap: a 754 MB ISO written
to a 32 GB stick leaves ~31 GB of "unmapped" space that may contain old
btrfs secondary superblocks, but `blkid` and the kernel's
filesystem-detection paths read the _primary_ superblock at the head
and find the new image's data, so the device boots correctly.

If a future requirement demands a full `wipefs` equivalent, it should be
implemented as a pure-Rust scan of the device's known-secondary-block
locations, not by shelling out (see directive 1 in `AGENTS.md`).

## Buffered writes, deliberately

```rust
guard.file().write_all_at(&zeros, 0)?;          // NOT O_DIRECT
guard.file().write_all_at(&zeros, tail_offset)?;
```

`O_DIRECT` is **off** for this phase. Two reasons:

1. The 1 MiB writes happen at offsets 0 and `dev_size - 1 MiB`. Both
   are sector-aligned today (because `dev_size` from `BLKGETSIZE64` is
   always a multiple of the logical sector size). A future change to
   `WIPE_REGION` that breaks alignment would silently start hitting
   `EINVAL` from the kernel — fragile.
2. The wipe is one-shot, low-bandwidth (2 MiB total), and is followed
   by `fdatasync`. Skipping the page cache here gains nothing
   measurable and forfeits the `EINVAL` safety net.

`fdatasync` after the writes ensures the wipe is durable before Phase 4
starts. If anything fatal happens during the flash, the user reboots
into a device with no partition table at all rather than a confusing
half-wiped layout.

## When the guard arms

```rust
guard.ensure_device_is(&target.dev_canon)?;   // refuse a crossed pairing
guard.arm(ArmedPhase::WipingSignatures);      // ← here
events.phase_started(UiPhase::Wipe);          // the binary renders it
wipe_ends(guard, target.dev_size)?;
```

`arm` takes `ArmedPhase`, not `GuardPhase`: `GuardPhase` is the public
enum that includes `Disarmed`, and a guard cannot be armed _into_ the
disarmed state. The library emits `phase_started` rather than printing —
the "Wiping partition signatures..." line is the binary's rendering of
that event.

The guard arms _immediately before_ the first destructive operation in
the entire pipeline. From this point until the `disarm()` at the start
of Phase 6, any
unwind path (panic, `?`, Ctrl+C) prints the "device in inconsistent
state, do not remove" warning to stderr.

The arming **must** happen before the wipe call, not after. If
`wipe_ends` fails (e.g., `EIO` on a failing USB stick) the device is
already in an inconsistent state — the head may have been wiped before
the tail write erred — and we want the warning. Arming-after would miss
exactly this case.

## Failure modes

- **Device too small.** `wipe_ends` checks `dev_size >= 2 * WIPE_REGION`
  (via the pure, unit-tested `tail_wipe_offset` helper)
  before writing. A 1 MiB device or smaller would have head and tail
  overlap, which we treat as nonsensical. Phase 0 refuses undersized
  devices before the guard is ever armed (clean error, no FATAL); this
  in-phase bound is defense in depth for any future caller that reaches
  the wipe without that preflight.
- **`EIO`.** The USB stick is failing. The guard prints its warning, the
  operator gets an actionable error.
- **`ENOSPC`.** Should be impossible — we already verified `dev_size` via
  `BLKGETSIZE64` and the writes are well within bounds. If it does
  happen, treat as `EIO`.

## Manual test

```sh
# Use a scratch USB stick. Verify a partition table exists first:
sudo sgdisk -p /dev/sdb

# Run imi.
sudo ./target/release/imi -i any.iso -d /dev/sdb -y

# Verify the head was wiped:
sudo dd if=/dev/sdb bs=1M count=1 status=none | xxd | head
# Should be all zeros (or, after Phase 4, the new image's bytes).
```
