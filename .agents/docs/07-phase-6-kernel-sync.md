# 07 — Phase 6: Kernel Partition-Table Sync and Lock Release

**Source:** `crates/imi-core/src/phases/phase_6.rs::run`, `crates/imi-core/src/common/ioctl.rs::blkrrpart`.

**Purpose:** Tell the kernel about the new partition table written in
Phase 4, then release the `O_EXCL` claim so userspace can see and
auto-mount the device normally.

## Disarming comes first

`run` opens with `guard.disarm()`, before the ioctl and before the FD is
released. Phases 3 to 5 only ever `set_phase`, so this is the only
phase that disarms.

It is not the only _call_ site, though: `into_file()` disarms again on
its way to taking the `File` out, a few lines below. The two are
redundant, and that is the point — **either one alone suppresses the
notice**, so both would have to be lost before a finished device started
being reported as inconsistent. Verified by deleting the call above and
flashing: still no FATAL, because `into_file()` covered it.

`full_pipeline_flashes_byte_exact` asserts the notice is absent after a
successful flash, which is what would catch losing both.

Placing it here is what makes the guard's contract exact: everything
from Phase 3's `arm` to this line is the destructive window, and an
unwind anywhere inside it prints the FATAL notice. Which phase ran last
before this point depends on the skip flags — Phase 5b normally, Phase
5a with `--skip-verification`, Phase 4 with both — but in every case
the device has been fully written and, unless verification was skipped,
read back. Disarming any earlier would suppress the warning while the
device was still mid-flight; any later would keep warning about a
device that is already correct.

## What `BLKRRPART` does

```rust
ioctl_none!(blkrrpart, 0x12, 95);     // _IO(0x12, 95)
```

`BLKRRPART` ("re-read partition table") is a Linux block-device ioctl
defined in `include/uapi/linux/fs.h`. It tells the kernel to:

1. Drop its in-memory cached view of the device's partition table.
2. Re-scan the device for a new partition table at LBA 0 (or LBA 1 for
   GPT).
3. Create new `/dev/sd<X><N>` partition device nodes for any partitions
   found.
4. Remove device nodes for any partitions that disappeared.
5. Emit `udev` `change` and `add`/`remove` uevents.

Without it, the kernel still believes the _old_ partition table is the
truth: the new image's partition layout is on disk but invisible to
userspace.

## Why we issue it under the lock

`BLKRRPART` is only allowed when the kernel believes the device has no
active partitions in use:

```c
// block/genhd.c::disk_scan_partitions, which BLKRRPART now routes to
if (!disk_has_partscan(disk))
        return -EINVAL;
if (disk->open_partitions)
        return -EBUSY;
if (!(mode & BLK_OPEN_EXCL)) {
        ret = bd_prepare_to_claim(disk->part0, disk_scan_partitions, NULL);
        ...
}
```

`disk->open_partitions` is the count of currently-open partitions — the
field older kernels called `bd_part_count`. Holding `O_EXCL` on the
_whole disk_ prevents any partition open from succeeding (the kernel
rejects with `EBUSY` while we hold the claim), so during the entire
flash + verify window it should be 0.

The third branch is worth noticing: a caller that is _not_ already
holding an exclusive claim makes the kernel take one on its behalf for
the duration of the scan. We pass `BLK_OPEN_EXCL`, so that is skipped —
we are already the claimant.
That makes Phase 6 the cleanest moment to issue `BLKRRPART`: any
cooperative daemons that would otherwise have re-opened a partition
have been blocked since Phase 2.

If you released the FD _before_ `BLKRRPART`, `udisks2` would race in,
auto-mount the new filesystem (incrementing `open_partitions`), and your
`BLKRRPART` would `EBUSY`. The bash original hit exactly this race for
months before fixing the ordering.

## Why failure is non-fatal

```rust
// The block covers the call alone. It used to enclose the `if let` and
// the reporting call too, which put a consumer-implemented trait method
// inside a scope claiming to have checked its preconditions.
let rrpart = unsafe { ioctl::blkrrpart(guard.as_raw_fd()) };
if let Err(e) = rrpart {
    events.warning(&format!("BLKRRPART failed ({e}); proceeding anyway"));
}
```

The library reports through `Events`; the `warning:` prefix and the
stderr routing are the binary's.

`BLKRRPART` can still fail in edge cases:

- Loop and device-mapper nodes without partition scanning fail the
  `disk_has_partscan(disk)` test above and return `EINVAL`. This is not
  a quirk — it is the first branch of the function, and it is why every
  flash to a plain `losetup` device in a test container logs the
  BLKRRPART warning.
- Some firmware-emulated USB sticks have quirks where the ioctl
  returns `EINVAL` even though the partition scan would succeed.
- On a brand-new flash with no partition table, the scan returns empty
  but the ioctl can return `ENOTTY` on some older kernels.

In all of these, `udev` will eventually pick up the change via the
`change` uevent emitted when we drop the FD, and partition nodes will
appear within ~1 second. We log the warning and continue. The Phase 7
sleep gives udev time to settle.

## Releasing the FD

```rust
drop(guard.into_file());
```

`guard.into_file()` consumes the `FlashGuard`, takes the `File` out, and
returns it. The `FlashGuard::Drop` impl runs (silently, because the
guard was disarmed at the top of this phase), and then the `File` is
dropped on the next line.

Dropping the `File`:

1. Closes the FD via `close(2)`.
2. Releases the kernel's `O_EXCL` claim list entry.
3. Triggers a `change` uevent to udev for the now-unclaimed device.
4. Allows future `open()` calls (from `udisks2`, `systemd-mount`, etc.)
   to succeed.

This is the moment where userspace regains "normal" access. Anything
that was waiting on the device (e.g. a `udisks2` mount request that
returned `EBUSY` in Phase 1) will now proceed. Phase 7 exists to defend
against exactly that.

After this drop, control passes to Phase 7. The orchestrator hands it
the whole `Target`; `phase_7::run` takes the disk's kernel name out of
that and passes it — not a `TargetDevts` — to
`phase7_automount_defense`. The rebuild now lives _inside_ Phase 7,
after its initial settle sleep. The pre-flash devt set was built when
the disk may have had no partitions at all; the new image likely
creates several. Phase 7's mountinfo filter is keyed on devt, so a
stale set would silently miss mounts on the new partitions. See
`08-phase-7-automount.md` for the full rationale, including why the
post-settle ordering matters for the BLKRRPART-failure path.

## Why `into_file().drop()` and not just letting `guard` fall out of scope

Two reasons we drop the FD explicitly here rather than letting the
function-level scope handle it:

1. **Ordering.** Phase 7 needs the FD released _before_ it starts its
   automount-defense sweep. If `guard` lived to the end of `run()` and
   was dropped after Phase 7, the `O_EXCL` claim would still be held
   during Phase 7, and `umount2` calls in Phase 7 would race against
   the still-locked device in confusing ways.
2. **Explicit handoff.** The `into_file()` consumes the guard, which is
   a compile-time guarantee that no later code in the function tries
   to use it. The borrow checker rejects subsequent `guard.<anything>`
   calls.

## Manual test

```sh
sudo ./target/release/imi -i bootable.iso -d /dev/sdb -y

# After Phase 6 the new partitions should appear:
ls /dev/sdb*
# Expect: /dev/sdb /dev/sdb1 /dev/sdb2 (or whatever the image has)

# Without imi's BLKRRPART, you'd need to wait for udev or run
# `partprobe` manually. With it, partitions are visible immediately.
```
