//! `imi` — entry point and orchestration of the seven-phase flash pipeline.
//!
//! Phase boundaries are load-bearing: each phase has a specific invariant
//! it establishes for the next, and the `FlashGuard`'s armed/disarmed state
//! is synchronised with those invariants.
//!
//! | Phase | Purpose                                             |
//! |-------|-----------------------------------------------------|
//! | 0     | Pre-flight validation                               |
//! | 1     | Topology audit + swap/unmount (before `O_EXCL`)     |
//! | 2     | `O_EXCL` claim + TOCTOU re-check                    |
//! | 3     | Signature wipe (head + tail); guard armed here      |
//! | 4     | Flash write                                         |
//! | 5a    | Hardware cooldown (skipped with --skip-cooldown)    |
//! | 5b    | Byte-for-byte verify (--skip-verification skips)    |
//! | 6     | `BLKRRPART` under lock, then drop FD                |
//! | 7     | Automount defense after lock release                |
//!
//! UX contract: phase 0–2 emit nothing on success; phase 3 onward announces
//! itself with a single line. Progress bars use `finish_and_clear` + a
//! trailing newline so no residue bleeds into subsequent output.

#![cfg(target_os = "linux")]
#[cfg(not(target_os = "linux"))]
compile_error!("imi is Linux-only");

mod aligned;
mod cli;
mod flash;
mod gpt;
mod guard;
mod image;
mod ioctl;
mod mount;
mod preflight;
mod progress;
mod sysfs;
mod verify;

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::cli::Cli;
use crate::guard::FlashGuard;
use crate::image::Compression;
use crate::mount::TargetDevts;
use crate::preflight::{
    DeviceIdentity, ensure_block_device, ensure_image_is_regular_file, ensure_image_not_on_target,
    ensure_whole_disk, phase0_root_check, query_dev_geometry_readonly,
};

fn main() {
    let exit = match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    };
    std::process::exit(exit);
}

/// The seven-phase pipeline, in order. Reads top-to-bottom; see the
/// module doc's phase table and `.agents/docs/` for per-phase rationale.
fn run() -> Result<()> {
    let cli = Cli::parse();
    let cancel = Arc::new(AtomicBool::new(false));
    install_signal_handler(Arc::clone(&cancel))?;

    // ------------------------------------------------------------------
    // Phase 0 — pre-flight validation (silent on success).
    // ------------------------------------------------------------------
    phase0_root_check()?;

    let img_canon = std::fs::canonicalize(&cli.img)
        .with_context(|| format!("canonicalize image path {}", cli.img.display()))?;
    let dev_canon = std::fs::canonicalize(&cli.dev)
        .with_context(|| format!("canonicalize device path {}", cli.dev.display()))?;

    if img_canon == dev_canon {
        bail!(
            "image and target resolve to the same path ({}); refusing to flash",
            dev_canon.display()
        );
    }

    ensure_image_is_regular_file(&img_canon)?;
    ensure_block_device(&dev_canon)?;
    ensure_whole_disk(&dev_canon)?;

    let dev_kname = sysfs::kname_for_path(&dev_canon)
        .with_context(|| format!("resolving kernel name for {}", dev_canon.display()))?;

    ensure_image_not_on_target(&img_canon, &dev_kname)?;

    let comp = image::detect_compression(&img_canon)
        .with_context(|| format!("detect compression format of {}", img_canon.display()))?;

    let raw_size: Option<u64> = if comp.is_compressed() {
        None
    } else {
        let meta = std::fs::metadata(&img_canon)
            .with_context(|| format!("stat {}", img_canon.display()))?;
        Some(meta.len())
    };

    let (dev_size, dev_read_only) = query_dev_geometry_readonly(&dev_canon)
        .with_context(|| format!("query size/RO state of {}", dev_canon.display()))?;

    if dev_read_only {
        bail!(
            "{} reports itself write-protected (BLKROGET). Check the physical \
             RO switch or `blockdev --setro` state; refusing before any \
             destructive step.",
            dev_canon.display()
        );
    }

    // Minimum-size floor, checked here for the same reason as BLKROGET
    // above: Phase 3's wipe re-checks this bound (defense in depth), but
    // by then the guard is armed, so a too-small device would earn the
    // "device is in an inconsistent state" FATAL warning despite never
    // having been touched. Refuse cleanly before the prompt instead.
    if dev_size < 2 * gpt::WIPE_REGION {
        bail!(
            "device is too small ({dev_size} bytes) for a head+tail signature \
             wipe (minimum {} bytes); refusing before any destructive step.",
            2 * gpt::WIPE_REGION
        );
    }

    if let Some(image_size) = raw_size
        && image_size > dev_size
    {
        bail!(
            "raw image ({image_size} bytes) is larger than target device \
                 ({dev_size} bytes); aborting"
        );
    }

    // Snapshot the device identity the operator is about to confirm.
    // Phase 2 re-verifies this against the O_EXCL-claimed FD, closing the
    // replug TOCTOU: if the stick is yanked and a different one lands on
    // the same /dev name between the prompt and the claim, the mismatch
    // aborts before anything destructive.
    let expected_identity = DeviceIdentity::capture(&dev_canon, &dev_kname, dev_size)?;

    if !cli.yes {
        tty_confirm(&dev_canon, &dev_kname, dev_size, &img_canon, comp, raw_size)?;
    }

    // ------------------------------------------------------------------
    // Phase 1 — topology audit + unmount (silent on success).
    //
    // Must precede O_EXCL: the kernel's exclusive claim is rejected while
    // any partition of the device is mounted.
    // ------------------------------------------------------------------
    mount::reject_active_stacked_volumes(&dev_kname)
        .context("Phase 1: rejecting active stacked volumes")?;

    let devts = TargetDevts::from_disk(&dev_kname).context("Phase 1: building target devt set")?;
    let mounts =
        mount::mounts_on_target(&devts).context("Phase 1: enumerating mounts on target")?;
    mount::enforce_whitelist(&mounts, &dev_canon).context("Phase 1: whitelist check")?;

    mount::disable_swaps_on_target(&devts).context("Phase 1: disabling swaps on target")?;

    if !mounts.is_empty() {
        mount::unmount_all(&mounts).context("Phase 1: unmounting partitions of target")?;
    }

    let residual =
        mount::mounts_on_target(&devts).context("Phase 1: re-scanning mounts after unmount")?;
    if !residual.is_empty() {
        bail!("target still has {} mount(s) after unmount attempts; aborting", residual.len());
    }

    // ------------------------------------------------------------------
    // Phase 2 — exclusive claim + TOCTOU re-check (silent on success).
    // ------------------------------------------------------------------
    let dev_file =
        open_exclusive(&dev_canon).context("Phase 2: opening target device with O_EXCL")?;
    let mut guard = FlashGuard::new(dev_file, dev_canon.clone());

    mount::reject_active_stacked_volumes(&dev_kname)
        .context("Phase 2: re-checking stacked volumes under lock")?;
    let mounts2 =
        mount::mounts_on_target(&devts).context("Phase 2: re-enumerating mounts under lock")?;
    if !mounts2.is_empty() {
        bail!("target acquired a new mount after O_EXCL claim (racing udisks2?); aborting");
    }
    expected_identity
        .verify_claimed(&guard, &dev_kname)
        .context("Phase 2: re-verifying device identity under lock")?;

    // ------------------------------------------------------------------
    // Phase 3 — signature wipe. Arm the guard here.
    // ------------------------------------------------------------------
    guard.arm(guard::GuardPhase::WipingSignatures);
    println!("Wiping partition signatures...");
    gpt::wipe_ends(&guard, dev_size).context("Phase 3: wiping device signatures")?;

    // ------------------------------------------------------------------
    // Phase 4 — flash.
    // ------------------------------------------------------------------
    guard.set_phase(guard::GuardPhase::Writing);
    println!("Flashing image to {}...", dev_canon.display());
    let reader = image::ImageReader::open(&img_canon, comp).context("Phase 4: opening image")?;
    let outcome = flash::flash(&mut guard, reader, comp, raw_size, dev_size, cli.throttle, &cancel)
        .context("Phase 4: flash write loop")?;

    // ------------------------------------------------------------------
    // Phase 5a — hardware cooldown. Runs unless --skip-cooldown.
    //
    // This is not ritual: cheap USB-NAND bridges drain writes from DRAM
    // into NAND while doing TLC/QLC garbage collection and FTL housekeeping
    // after `sync`/`fdatasync` return. Pulling power during that window
    // corrupts the flash regardless of whether we ran a read-back compare.
    // The skip flag exists for loop devices, automated tests, and media
    // whose controllers honor cache flushes; cli.rs carries the risk
    // statement the operator sees.
    // ------------------------------------------------------------------
    if cli.skip_cooldown {
        println!("Skipping cooldown (--skip-cooldown).");
    } else {
        guard.set_phase(guard::GuardPhase::Cooldown);
        verify::cooldown(10, &cancel).context("Phase 5a: hardware cooldown")?;
    }

    // ------------------------------------------------------------------
    // Phase 5b — byte-for-byte verification. Skipped on --skip-verification.
    // ------------------------------------------------------------------
    if cli.skip_verification {
        println!("Skipping verification (--skip-verification).");
    } else {
        guard.set_phase(guard::GuardPhase::Verifying);
        println!("Verifying data integrity...");
        verify::verify(&mut guard, &img_canon, comp, outcome.bytes_written, cli.throttle, &cancel)
            .context("Phase 5b: verification")?;
    }

    // Successfully past the destructive window.
    guard.disarm();

    // ------------------------------------------------------------------
    // Phase 6 + 7 — kernel sync, lock release, automount defense.
    // ------------------------------------------------------------------
    println!("Securing kernel and blocking automounts...");

    // Phase 6: BLKRRPART under the O_EXCL claim. Non-fatal on failure.
    //
    // SAFETY: `guard` still owns a valid, O_EXCL-claimed FD for the target
    // block device. BLKRRPART takes no argument and does not touch user
    // memory; failure surfaces as `Err` from the nix wrapper.
    unsafe {
        if let Err(e) = ioctl::blkrrpart(guard.as_raw_fd()) {
            eprintln!("warning: BLKRRPART failed ({e}); proceeding anyway");
        }
    }

    // Release the O_EXCL claim so udisks2/systemd-udevd can see the device
    // and present the new partition table to userspace. Phase 7 then
    // defends against any race-condition re-mounts.
    drop(guard.into_file());

    // Phase 7: multi-pass automount defense. The devt-set rebuild lives
    // inside this function (after its initial settle sleep) so it covers
    // both the BLKRRPART-success path (where sysfs is already updated
    // synchronously) and the BLKRRPART-failure fallback path (where
    // udev's processing of the FD-release change uevent eventually
    // populates sysfs). See `08-phase-7-automount.md`.
    phase7_automount_defense(&dev_kname, &cancel)?;

    println!("SUCCESS: You can now safely remove {}.", dev_canon.display());
    Ok(())
}

/// TTY confirmation. Opens `/dev/tty` explicitly (read **and** write) so:
/// (1) a piped stdin cannot bypass the prompt — the response must come from
/// the controlling terminal, and (2) the prompt itself is visible even when
/// stdout is redirected to a log file. Both halves matter for a destructive
/// tool: a pipeline like `imi ... > log.txt 2>&1` would otherwise appear
/// to hang because the operator never sees the prompt.
fn tty_confirm(
    dev: &Path,
    dev_kname: &str,
    dev_size: u64,
    img: &Path,
    comp: Compression,
    raw_size: Option<u64>,
) -> Result<()> {
    let model = sysfs::device_model(dev_kname).unwrap_or_else(|| "(unknown)".into());
    #[expect(
        clippy::cast_precision_loss,
        reason = "display-only GiB figure on the confirmation banner; \
                  sub-ULP rounding on a >8 EiB device is irrelevant"
    )]
    let gib = dev_size as f64 / (1024.0 * 1024.0 * 1024.0);

    println!("\n");
    println!("WARNING: This will DESTROY ALL DATA on {}", dev.display());
    println!("  Model:     {model}");
    println!("  Size:      {dev_size} bytes ({gib:.2} GiB)");
    println!("  Image:     {}", img.display());
    println!("  Format:    {}", comp.label());
    if let Some(n) = raw_size {
        println!("  Img size:  {n} bytes");
    }
    println!("\n");

    // Open /dev/tty for read+write. Read for the response (stdin would let a
    // piped invocation bypass the prompt); write for the prompt itself
    // (stdout would be invisible if redirected). `ENXIO` here means no
    // controlling terminal — the operator should be using --yes anyway.
    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("opening /dev/tty; rerun with --yes for automation")?;

    write!(tty, "Type 'yes' to proceed: ").context("writing prompt to /dev/tty")?;
    // Best-effort flush: the read_line below blocks on the same tty, so a
    // failed flush at worst delays prompt visibility.
    drop(tty.flush());

    let mut reader = BufReader::new(tty);
    let mut input = String::new();
    reader.read_line(&mut input).context("reading confirmation from /dev/tty")?;

    if input.trim() != "yes" {
        bail!("aborted by user");
    }
    Ok(())
}

// =======================================================================
// Phase 2 helper
// =======================================================================

/// Phase 2's exclusive claim: `O_RDWR | O_EXCL | O_CLOEXEC` on the node.
fn open_exclusive(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_EXCL | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| {
            format!(
                "opening {} with O_RDWR|O_EXCL|O_CLOEXEC (EBUSY => someone else holds the device)",
                path.display()
            )
        })
}

// =======================================================================
// Phase 7 helper
// =======================================================================

/// Phase 7: settle, then up-to-three plain-umount sweeps against daemon
/// automounts, ending in an honest final mountinfo verdict.
fn phase7_automount_defense(dev_kname: &str, cancel: &AtomicBool) -> Result<()> {
    // Let udev process the uevents from BLKRRPART + FD release. Two
    // things happen in this 2-second window:
    //   1. The kernel's `change` uevent (emitted on FD release) triggers
    //      udev to repopulate `/dev/disk/by-*` and any rules-driven
    //      symlinks for the new partition layout.
    //   2. udisks2 reacts to udev's settled state and may issue a mount
    //      for the new filesystem.
    // We then rebuild `TargetDevts` from sysfs *after* the sleep so it
    // covers the BLKRRPART-failure path: when BLKRRPART returns Err we
    // still get the new partition entries, but only via udev's
    // processing of the FD-release change uevent, which is asynchronous.
    // Doing the rebuild inside Phase 7 covers both paths uniformly.
    flash::cancellable_sleep(Duration::from_secs(2), cancel);
    if cancel.load(Ordering::SeqCst) {
        bail!("cancelled by user during automount defense settle");
    }

    let devts = TargetDevts::from_disk(dev_kname)
        .context("Phase 7: rebuilding target devt set after udev settle")?;

    for attempt in 1..=3_u32 {
        if cancel.load(Ordering::SeqCst) {
            bail!("cancelled by user during automount defense");
        }

        let mounts = mount::mounts_on_target(&devts)
            .with_context(|| format!("Phase 7 attempt {attempt}: scanning mountinfo"))?;
        if mounts.is_empty() {
            return Ok(());
        }

        eprintln!(" -> pass {attempt}: found {} new mount(s)", mounts.len());
        for m in &mounts {
            eprintln!("    unmounting {}", m.target.display());
            // Plain umount, NOT MNT_DETACH. A fresh daemon automount
            // normally unmounts cleanly; if something already has files
            // open on it (tracker-miner, a thumbnailer), a lazy detach
            // would erase it from mountinfo while it stays alive through
            // those fds — and the final scan below would then bless a
            // still-active mount with SUCCESS. Plain umount keeps the
            // final verdict honest: a stubborn mount stays visible and
            // trips the "Do NOT remove" abort instead. Per-mount errors
            // are tolerated here; the next pass (and the final scan)
            // re-evaluate.
            if let Err(e) = nix::mount::umount2(&m.target, nix::mount::MntFlags::empty()) {
                eprintln!("    (unmount failed: {e}; re-checking on the next pass)");
            }
        }

        flash::cancellable_sleep(Duration::from_secs(2), cancel);
    }

    let still = mount::mounts_on_target(&devts).context("Phase 7: final mountinfo scan")?;
    if still.is_empty() {
        Ok(())
    } else {
        bail!(
            "device still has {} persistent mount(s) after 3 automount-defense passes. \
             Do NOT remove the device. Unmount manually before unplugging.",
            still.len()
        );
    }
}

// =======================================================================
// Signal handling
// =======================================================================

/// Install the SIGINT/SIGTERM handler that flips the shared cancel flag.
fn install_signal_handler(cancel: Arc<AtomicBool>) -> Result<()> {
    ctrlc::set_handler(move || {
        cancel.store(true, Ordering::SeqCst);
        // Deliberately do NOT exit() from the handler: setting the flag
        // lets the main thread's chunk loops return Err, driving normal
        // unwind so FlashGuard::drop runs.
    })
    .context("installing SIGINT/SIGTERM handler")?;
    Ok(())
}
