# 03 — Phase 2: Exclusive Claim and TOCTOU Closure

**Source:** `src/main.rs::open_exclusive`, `src/main.rs::run` (Phase 2
block), `src/guard.rs`.

**Purpose:** Acquire kernel-enforced exclusive ownership of the target
block device, then re-verify topology under the lock to close the
between-phases time-of-check / time-of-use window.

## The `O_EXCL` claim — what it actually does

`open(path, O_RDWR | O_EXCL | O_CLOEXEC)` on a block device invokes the
kernel's `bd_prepare_to_claim()` (block/bdev.c). The kernel maintains a
per-block-device claim list. The open succeeds only if:

1. No other open file description currently has an `O_EXCL` claim on
   this device or any of its partitions.
2. No filesystem is mounted from this device or any of its partitions.
3. No swap is active on any of its partitions.

If any of those is false, the open returns `EBUSY`. **This is stronger
than `flock(2)`** — `flock` is advisory; `O_EXCL` on a block device is
kernel-enforced and applies to mounts and swap, not just other
processes that opt into the claim.

```rust
OpenOptions::new()
    .read(true)
    .write(true)
    .custom_flags(libc::O_EXCL | libc::O_CLOEXEC)
    .open(path)
```

`O_CLOEXEC` ensures the FD is not inherited by any future child process
we might unintentionally fork (we do not, but defence in depth).

## Why Phase 1 had to run first

If any partition of the target device is mounted at the moment of
`open`, the call fails with `EBUSY`. The error message offers no
information about _which_ partition is mounted; you have to re-parse
mountinfo to find out. The bash original learned this and started
unmounting before opening; we inherit that ordering.

## TOCTOU re-check

Between the Phase 1 unmount and the Phase 2 `O_EXCL` open, a racing
`udisks2` could in principle:

- Auto-mount one of the partitions we just unmounted (its uevent for
  the unmount triggers a re-evaluation of the device).
- Activate a holder via udev rules.

`O_EXCL` succeeding is _strong_ evidence the race did not happen, but
not absolute proof — there's a multi-syscall window between the
mountinfo re-scan at the end of Phase 1 and the open syscall. So we
re-run both checks under the lock:

```rust
mount::reject_active_stacked_volumes(&dev_kname)?;
let mounts2 = mount::mounts_on_target(&devts)?;
if !mounts2.is_empty() {
    bail!("target acquired a new mount after O_EXCL claim ...");
}
```

If a re-mount slipped in, the re-check catches it. The mount re-scan
deliberately reuses the _Phase 1_ devt set rather than rebuilding it:
between Phase 1 and this point nothing can change the partition
inventory — `BLKRRPART` doesn't run until Phase 6, and a physical
replug is caught by the identity re-check below regardless of what
devts it brings. (Phase 7 is the opposite case, and rebuilds; see
`08-phase-7-automount.md`.) The `O_EXCL` claim
itself blocks any _future_ mount attempt for as long as we hold the FD,
so the check is monotonic from this point: anything we don't see here
cannot appear before we explicitly drop the FD in Phase 6.

The re-check also verifies **device identity** against the Phase 0
snapshot (`DeviceIdentity`): `fstat` of the _claimed FD_ must report the
same `st_rdev`, sysfs must report the same model string, and
`BLKGETSIZE64` on the claimed FD must report the same size. This closes
the replug TOCTOU: between the operator confirming the prompt and the
`O_EXCL` open, a stick can be yanked and a different one can land on the
same `/dev/sdX` name (frequently with the same recycled devt — which is
why the model and size comparisons back up the rdev check). A mismatch
aborts before the guard ever arms.

## Building the FlashGuard immediately

```rust
let dev_file = open_exclusive(&dev_canon)?;
let mut guard = FlashGuard::new(dev_file, dev_canon.clone());
```

The guard wraps the `File` _immediately_ on a successful open. From this
moment, any panic, `?`-propagated error, or signal-driven cancel that
unwinds past this point will release the FD via the guard's `Drop`. The
guard is constructed in disarmed state; it does not yet print the
"device inconsistent" warning, because nothing destructive has happened
yet.

The guard arms in Phase 3 (immediately before the first `pwrite` of the
signature wipe) and disarms in Phase 5b (after verification passes).
See `09-flashguard.md` for the full lifecycle.

## Why `O_RDWR` and not `O_WRONLY`

Phase 5 reads back from the device for verification while the same FD
is still held. Splitting into a write FD then a read FD would leak the
exclusive claim during the FD swap. `O_RDWR` lets us keep one FD across
both phases.

## Signal-handler installation

`ctrlc::set_handler` is installed back in Phase 0 (before any
destructive action). The handler closure captures an
`Arc<AtomicBool>` and only flips the flag — it never calls
`std::process::exit`. That is critical: an `exit()` from a signal
handler bypasses all `Drop` impls, including `FlashGuard::drop`. The
flag-flipping pattern lets the destructive loops in Phase 4 / 5
return `Err`, which drives normal stack unwinding through the guard.

## What can go wrong here

- `EBUSY` despite Phase 1 succeeding → almost always udisks2 racing in.
  The error message points to that directly. Operator can disable
  udisks2 (`systemctl stop udisks2`) and retry.
- `EACCES` → not running as root, or `/dev/<name>` has been chmod'd
  weirdly. Phase 0's root check should have caught the first.
- `ENOENT` → device hot-unplugged between Phase 0 canonicalize and now.
  Refuse and retry from scratch.

## Manual test

```sh
# Setup: have udisks2 running and an unmounted USB stick at /dev/sdb.

# Test the exclusive claim works:
sudo ./target/release/imi -i img.iso -d /dev/sdb -y
# Phase 2 should be silent on success.

# Test the EBUSY path: hold the device manually with another process.
exec 3<> /dev/sdb     # opens read-write but not O_EXCL, won't conflict
# Confirm no conflict.

# Properly conflicting — hold a real O_EXCL claim (flock would NOT
# conflict; it is advisory and never registers a block-device claim):
sudo python3 -c 'import os,time; os.open("/dev/sdb", os.O_RDWR|os.O_EXCL); time.sleep(60)' &
sudo ./target/release/imi -i img.iso -d /dev/sdb -y
# Should fail Phase 2 with the EBUSY message.
```
