# 08 — Phase 7: Automount Defense

**Source:** `src/main.rs::phase7_automount_defense`, `src/mount.rs`.

**Purpose:** After the `O_EXCL` lock is released, prevent the
just-flashed device from being auto-mounted by `udisks2`,
`systemd-logind`, or desktop environments before the operator has a
chance to physically remove it.

## The race we're fighting

The moment we drop the FD at the end of Phase 6:

1. The kernel emits a `change` uevent for `/dev/sdb`.
2. `BLKRRPART` (issued in Phase 6) emitted `add` uevents for each new
   partition `/dev/sdb1`, `/dev/sdb2`, ….
3. `udev` rules under `/usr/lib/udev/rules.d/` fire. Notably:
   - `60-block.rules` and `60-persistent-storage.rules` populate
     `/dev/disk/by-*/`.
   - `udisks2` and `systemd-logind` subscribe to these events and
     consult their auto-mount policies.
4. If the new image contains a recognised filesystem (which it almost
   always does — Live ISOs are usually ext4 or iso9660), `udisks2`
   queues a mount request.
5. Within ~500 ms of FD release, `/media/<user>/<volume-label>` may
   appear with the freshly-flashed filesystem mounted on it.

This auto-mount is harmful because:

- It pollutes the freshly-flashed filesystem with mount-time mutations
  (see `06-phase-5-verify.md`).
- The operator typically just wants to unplug the stick and walk away;
  unmounting again is friction.
- On systems with stricter mount policies, the auto-mount may _fail_
  (e.g. `nosuid,nodev` mismatch with what's expected for booting),
  leaving the user with a confusing error notification.

The bash original tried to suppress udisks2 with `udisksctl
power-off`, which is a binary call and has its own edge cases. We
defend differently: poll `/proc/self/mountinfo` after FD release and
unmount anything that appears.

## The defense pattern

```rust
fn phase7_automount_defense(dev_kname: &str, cancel: &AtomicBool)
    -> Result<()>
{
    sleep(Duration::from_secs(2));            // let udev process events
    let devts = TargetDevts::from_disk(dev_kname)?;  // rebuild post-settle

    for attempt in 1..=3 {
        if cancel.load(SeqCst) {
            bail!("cancelled by user during automount defense");
        }
        let mounts = mounts_on_target(&devts)?;
        if mounts.is_empty() {
            return Ok(());                    // clean
        }
        for m in &mounts {
            // Plain umount; per-pass failures are logged to stderr
            // and tolerated (the next pass / final scan re-evaluate).
            if let Err(e) = umount2(&m.target, MntFlags::empty()) {
                eprintln!("    (unmount failed: {e}; re-checking on the next pass)");
            }
        }
        sleep(Duration::from_secs(2));
    }

    let still = mounts_on_target(&devts)?;
    if still.is_empty() { Ok(()) }
    else { bail!("device still has {} persistent mount(s) ...") }
}
```

### Initial 2-second sleep

After dropping the FD we wait 2 seconds before our first scan. Reasons:

- udev needs time to process the `change` uevent (typically ~100–500 ms
  depending on system load and the number of rule files).
- udisks2 reacts to udev's settled state — it usually issues its mount
  ~1 second after udev finishes.
- Polling immediately would catch nothing and give a false all-clear,
  followed by an auto-mount appearing ~1 second later.

### 3 passes with 2-second gaps

Some auto-mount tools retry on failure. A single unmount sweep can
race with a reconnect-and-retry from the daemon. Three passes spaced
2 seconds apart cover the realistic worst case:

- Pass 1 unmounts the auto-mount.
- udisks2 (or equivalent) sees the mount disappear, may retry.
- Pass 2 unmounts the retry.
- udisks2 backs off after two failures (per its default policy).
- Pass 3 confirms the stable state.

### Plain umount, never `MNT_DETACH`, never `MNT_FORCE`

We use `umount2(target, 0)`. The mounts appearing here are by
definition just-created auto-mounts and _usually_ have nothing holding
them — a plain umount succeeds. But "usually" is the operative word: on
a default GNOME/KDE desktop, tracker-miner and thumbnailers open files
on a fresh automount within seconds. A `MNT_DETACH` against such a
mount "succeeds" by erasing it from `/proc/self/mountinfo` while the
filesystem stays alive — and keeps writing — through those fds until
they close. The final scan below reads mountinfo; feeding it a lazily
detached mount means it can bless a still-active mount with a clean
verdict, and the operator unplugs a device that is still being written.

Plain umount keeps the oracle honest: a mount that something holds open
stays _visible_, survives the three passes, and trips the
"Do NOT remove the device" abort — the correct outcome. Per-pass umount
errors are tolerated (the next pass and the final scan re-evaluate);
only the final scan's verdict matters.

Phase 1 and Phase 7 still differ in _policy_ — Phase 1 refuses
non-whitelisted mounts (operator agency) while Phase 7 unmounts
unconditionally (daemon-owned) — but both use the same plain-umount
_mechanism_ for the same oracle-honesty reason.

### Cancellation responsiveness

The 2-second sleeps (initial settle + between-pass) use
`flash::cancellable_sleep`, which polls the cancel flag at 100ms
granularity. On Ctrl+C, the worst-case latency from key-press to
"cancelled by user" message is ~100ms — well below the
human-perceptible threshold for "the program is responding."

The same helper is used by the throttle paths in Phase 4 (flash) and
Phase 5b (verify) to avoid the same class of unresponsiveness at low
throttle rates. Phase 5a (cooldown) has its own per-second polling
loop because it drives an in-place UI countdown ("`Cooldown and FTL
sync... (Ns)`"); the per-second granularity matches the visual update
cadence.

The cancel-flag check is at the top of each pass — also at the
post-settle entry point — so an operator hitting Ctrl+C during a
pass-in-progress may complete that pass before bailing. That's fine:
a plain-umount sweep is fast and non-destructive, and we're not
protecting against malicious cancellation but against perceived hangs.

### Final scan + abort

After the 3 passes we re-scan once more. If anything is still mounted,
we bail with an actionable message:

```
device still has 1 persistent mount(s) after 3 automount-defense passes.
Do NOT remove the device. Unmount manually before unplugging.
```

This is rare in practice — typically only happens if the operator
manually re-mounts the device during Phase 7, or if a runaway daemon
keeps re-mounting. Either way, the right move is to stop and let the
operator deal with it.

## Why `TargetDevts` is rebuilt between Phase 6 and Phase 7

`TargetDevts::from_disk` is called twice in the pipeline:

1. Once in Phase 1, before the destructive section, to filter the
   pre-existing mountinfo and to identify swap-on-target.
2. Again at the start of Phase 7, **after** Phase 6's `BLKRRPART` has
   reorganised the partition table.

The second build is load-bearing. Consider flashing a 4-partition GPT
image onto a previously-empty disk:

- **Before flash:** `/sys/class/block/sdc/` has only `sdc/dev`. The
  Phase 1 `TargetDevts` set is `{ (8, 32) }`.
- **After flash + BLKRRPART:** The kernel reads the new GPT and creates
  `sdc1`, `sdc2`, `sdc3`, `sdc4`, with devts `(8, 33)…(8, 36)`.
- **udisks2 races to mount sdc1** at `/media/<user>/<label>`. The mount
  shows up in `/proc/self/mountinfo` with devt `(8, 33)`.

If Phase 7 used the _Phase 1_ devt set `{ (8, 32) }`, the mountinfo
filter `target.contains(mi.devt)` would silently drop the new mount —
`(8, 33)` is not in the set. Phase 7 would report "no mounts found",
return `Ok`, and we'd print
`SUCCESS: You can now safely remove /dev/sdc.` while udisks2 was
holding the device. Operator unplugs → "device busy" notification →
confused operator → potentially corrupted filesystem if they yank the
USB stick anyway.

Rebuilding `TargetDevts` re-stats `/sys/class/block/` and captures
the freshly-created partition entries, so Phase 7's mountinfo filter
sees them. The cost is one extra round of sysfs reads (microseconds);
the benefit is the "SUCCESS" message is honest.

### Why the rebuild lives _inside_ Phase 7, after the settle sleep

The rebuild is intentionally placed inside `phase7_automount_defense`,
_after_ the initial 2-second sleep, rather than immediately after the
BLKRRPART call. This positioning covers two distinct paths uniformly:

1. **BLKRRPART succeeds** — sysfs is already updated synchronously (see
   below). The rebuild reads up-to-date sysfs; the 2-second sleep was
   needed for udisks2 timing anyway, so positioning the rebuild after
   it costs nothing.
2. **BLKRRPART fails** — sysfs has _not_ been updated by the kernel.
   The new partition entries appear only via udev's processing of the
   `change` uevent emitted on FD release, which is asynchronous. The
   2-second sleep gives udev time to settle. Without the sleep-then-
   rebuild ordering, the BLKRRPART-failure path would race: rebuild
   would read stale sysfs, the new partitions would be missing from
   the devt set, and the failure mode would be the same silent-success
   bug we are fixing — but only when BLKRRPART itself happened to fail.

The previous design rebuilt immediately after BLKRRPART. That design
was correct for the success path but exposed the failure path to a
race. Moving the rebuild past the settle sleep eliminates the race
without making the success path slower (the 2-second sleep was already
on its critical path).

### Why BLKRRPART-success populates sysfs synchronously

A natural worry: if `BLKRRPART` emits uevents asynchronously, could the
sysfs entries for the new partitions still be unpopulated when we read
sysfs in the success path?

The answer is no, and it's worth recording why so future contributors
don't reintroduce a defensive sleep that isn't needed.

`BLKRRPART` invokes `disk_scan_partitions()` in `block/ioctl.c`, which
calls `add_partition()` for each newly-discovered partition.
`add_partition()` calls `device_add()` from `drivers/base/core.c`,
which is the kernel's canonical kobject-registration entry point and
**synchronously creates the sysfs directory** (via `kobject_add()`)
before returning. Only _after_ the sysfs entry exists does
`device_add()` call `kobject_uevent(KOBJ_ADD)` to enqueue the userspace
notification. The official kernel block ABI documents this directionally:
`GENHD_FL_HIDDEN`, the flag for hidden devices, is described as making
the device "not appear in sysfs" _and_ "not produce events" together —
sysfs presence and uevent emission are coupled, not separable.

So by the time the `BLKRRPART` ioctl returns from kernel space, the
partition entries are already present under `/sys/class/block/<disk>/`.
Our rebuild reads them directly from sysfs (via
`/sys/class/block/<disk>/<part>/dev`); it does _not_ depend on udev
having processed the uevents yet, because we don't read
`/dev/disk/by-*` or any other udev-managed path.

### What the 2-second sleep actually buys

What _is_ asynchronous and motivates the 2-second sleep:

- **udevd** processes the uevents to populate `/dev/disk/by-uuid/`,
  `/dev/disk/by-label/`, etc.
- **udisks2** reacts to udev's settled state and decides whether to
  auto-mount.
- These two steps together typically take ~500 ms to ~2 s on a
  loaded system.

After the sleep, sysfs is up-to-date in both the BLKRRPART-success and
BLKRRPART-failure cases (success: kernel did it synchronously; failure:
udev caught up via the change uevent). Then the rebuild → mountinfo
scan → unmount loop runs against a current view of the world.

The two phases handle different threat models:

| Phase 1                                                                              | Phase 7                                                     |
| ------------------------------------------------------------------------------------ | ----------------------------------------------------------- |
| Pre-existing mounts on operator's filesystem; could be `/mnt/work` with unsaved work | Auto-mounts created by daemons within seconds of FD release |
| Refuse outside whitelist (operator agency)                                           | Unmount unconditionally — daemon doesn't have files open    |

Phase 7 _cannot_ refuse mounts it sees, because if it didn't unmount
them the operator would unplug a mounted device and get filesystem
corruption. Phase 1 _cannot_ unmount unconditionally, because if it
did it could trash the operator's work.

## What Phase 7 cannot prevent

- Network-mounted automounts (NFS, SMB, etc.) targeting the new device.
  These are rare.
- A daemon that re-mounts faster than 2-second polling. None we know of
  do this; they all have backoff policies.
- A process that grabs files on the automount within the sweep window
  (tracker-miner, thumbnailers). Plain umount then fails, the mount
  stays visible, and the run ends with the "Do NOT remove" abort — by
  design. The operator waits for the indexer to finish and unmounts
  manually; a lazy detach here would instead have _hidden_ the active
  mount behind a SUCCESS line.
- A mounter whose _first_ engagement lands only after the final clean
  scan. Phase 7's window is bounded by design (settle + up-to-three
  passes, ~6-8 s); a mount arriving after the clean verdict is
  indistinguishable from a mount a minute later, and guarding forever
  is not a coherent goal for a program that must exit. (Live-probed:
  a scripted mounter firing 0.5 s after a first-scan-clean verdict
  produced SUCCESS with the late mount present — correct per this
  contract, and why the SUCCESS line says "you can now safely
  remove", present tense, not a promise about the future.)
- A future kernel feature that makes mounts un-detachable. None
  currently exist.

If a clever attack scenario emerges, the next defence layer would be
`mount(2)` namespace isolation (running imi in a private mount
namespace). That's an order of magnitude more complex and not currently
justified.

## Manual test

```sh
# Setup: ensure udisks2 is running.
sudo systemctl status udisks2

# Run imi:
sudo ./target/release/imi -i debian-live.iso -d /dev/sdb -y

# Right after the success message, check:
mount | grep /dev/sdb
# Should be empty.

ls /media/$USER/  /run/media/$USER/  2>/dev/null
# Should not contain a fresh entry for the new image's volume label.
```
