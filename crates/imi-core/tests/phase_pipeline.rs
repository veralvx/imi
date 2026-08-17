//! Drives `imi-core`'s per-phase API directly, as a downstream caller
//! would.
//!
//! `public_api.rs` proves the eight phases *type-check* as a chain; this
//! proves they actually *compose* — that sequencing them by hand
//! produces the same flashed device as `imi_core::run`, and that the
//! documented "nothing is destroyed before Phase 3" boundary holds.
//!
//! These are `#[ignore]`d by default: they need root and a free loop
//! device. Run them explicitly:
//!
//! ```sh
//! sudo -E cargo test -p imi-core --test phase_pipeline -- --ignored --test-threads=1
//! ```
//!
//! `--test-threads=1` matters: the tests attach loop devices and must
//! not race each other for `losetup -f`.
//!
//! # Two tests that can no longer be written
//!
//! `phase_4_refuses_a_guard_phase_3_never_armed` and its Phase 5 twin
//! each built a disarmed guard, handed it to a later phase, and asserted
//! the runtime refusal. Neither can be written since the typestate
//! split: Phases 4 and 5 take `&mut ArmedGuard`, which only Phase 3
//! produces, so the call does not compile. Verified against an external
//! consumer crate — skipping Phase 3 gives `E0308: expected
//! &mut ArmedGuard, found &mut FlashGuard`.
//!
//! A test that cannot be expressed is the strongest possible outcome for
//! the property it was testing, but it leaves a gap in the record, which
//! this note fills.
//!
//! It fills it here, at module level, rather than as a free-standing
//! `///` block where it first lived. That form is a doc comment attached
//! to nothing, and a sweep for orphaned doc comments deleted it — which
//! is how the gap it was written to prevent opened anyway.
//!
//! The same has now happened for *pairing*. `crossed_guard_and_target_
//! are_refused` here, and its unit-level counterparts in `guard.rs`,
//! `phase_3.rs`, `phase_4.rs` and `phase_5.rs`, each built a guard on one
//! device and a target describing another, then asserted the runtime
//! refusal. None can be written now: Phases 3 to 6 take a `Session` or an
//! `ArmedSession`, which pairs the two at construction, so there is no
//! second parameter a mismatched target could occupy. Verified against an
//! external consumer crate — the crossed call fails with `E0061`, wrong
//! number of arguments, rather than compiling and being refused.
//!
//! `FlashGuard::ensure_device_is` and its `ArmedGuard` delegate went with
//! them. They had no callers left, and `FlashGuard::new` is `pub(crate)`,
//! so no consumer-reachable path can produce an unpaired guard for them
//! to check.

// Scaffolding sits outside `#[test]` bodies, so clippy's
// `allow-*-in-tests` knobs do not reach it; an integration-test crate
// also has no `#[cfg(test)]` module by construction.
#![expect(
    clippy::expect_used,
    clippy::tests_outside_test_module,
    reason = "integration-test crate: scaffolding fns sit outside #[test] \
              bodies, so the in-tests exemptions do not reach them; the \
              crate has no cfg(test) module by design"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;

// `unused_crate_dependencies` is evaluated per compilation unit and an
// integration-test target links every one of the package's
// dependencies. This test names only `imi_core`, so acknowledge the
// rest. The list must mirror `crates/imi-core/Cargo.toml`.
use bzip2 as _;
use flate2 as _;
use libc as _;
use nix as _;
use xz2 as _;
use zstd as _;

/// Backing-file size: above the `2 * WIPE_REGION` floor, small enough
/// to flash in well under a second.
const BACKING_LEN: usize = 8 * 1024 * 1024;

/// A loop device attached to a scratch backing file, detached on drop.
struct Loop {
    /// Path of the `/dev/loopN` node.
    node: String,
    /// Path of the backing file behind it.
    backing: PathBuf,
}

impl Loop {
    /// Attach a fresh loop device over a backing file filled with `fill`.
    fn attach(tag: &str, fill: u8) -> Self {
        let backing = std::env::temp_dir().join(format!("imi-phase-{tag}-{}", std::process::id()));
        std::fs::write(&backing, vec![fill; BACKING_LEN]).expect("write backing file");
        let out = Command::new("losetup")
            .arg("--find")
            .arg("--show")
            .arg(&backing)
            .output()
            .expect("losetup must be runnable (these tests require root + util-linux)");
        assert!(
            out.status.success(),
            "losetup failed (are we root, with loop devices available?): {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let node = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        Self { node, backing }
    }

    /// Read the backing file back — evidence independent of the pipeline.
    fn read_backing(&self) -> Vec<u8> {
        std::fs::read(&self.backing).expect("read backing file")
    }
}

/// A mountpoint that unmounts itself, however the test leaves.
///
/// The mount tests used trailing `umount` statements, which a failed
/// assertion skips — and a mounted loop device cannot be detached, so
/// `Loop::drop` then fails too and leaks both. Verified: with the mount
/// in place `losetup -d` reports "detach failed: No such device or
/// address" and the device stays attached.
struct Mounted {
    /// Where the filesystem is mounted.
    dir: PathBuf,
}

impl Mounted {
    /// Mount `device` at `dir`, creating the directory.
    ///
    /// Returns `None` when the mount fails, so a caller can skip rather
    /// than fail on a kernel without the filesystem.
    fn at(device: &str, dir: PathBuf) -> Option<Self> {
        std::fs::create_dir_all(&dir).expect("mountpoint");
        let ok = Command::new("mount").arg(device).arg(&dir).status().is_ok_and(|s| s.success());
        if ok {
            Some(Self { dir })
        } else {
            let _rm = std::fs::remove_dir(&dir);
            None
        }
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        // Only unmount if it is still mounted. These fixtures wrap tests
        // whose subject is a phase that unmounts the target, so the
        // expected case is that the phase already did it — and an
        // unconditional `umount` then prints "not mounted" to stderr in
        // a passing run, which reads like a failure. Checking first
        // keeps a real failure ("target is busy") visible.
        // Field 5 (index 4) is the mount point, matching the crate's own
        // parser. mountinfo escapes space, tab, newline and backslash as
        // octal, and this compares the raw path — safe only because these
        // fixtures build their directory from a tag and a pid. A path
        // with any of those characters would silently fail to match, the
        // guard would skip the umount, and the mount and its loop device
        // would both leak. Assert it rather than rely on it.
        let dir = self.dir.display().to_string();
        assert!(
            !dir.contains([' ', '\t', '\n', '\\']),
            "fixture mountpoint {dir:?} contains a character mountinfo escapes; \
             the liveness check below would not match it"
        );
        let still = std::fs::read_to_string("/proc/self/mountinfo")
            .unwrap_or_default()
            .lines()
            .any(|l| l.split(' ').nth(4) == Some(dir.as_str()));
        if still {
            let _umount = Command::new("umount").arg(&self.dir).status();
        }
        let _rmdir = std::fs::remove_dir(&self.dir);
    }
}

impl Drop for Loop {
    fn drop(&mut self) {
        let _detach = Command::new("losetup").arg("-d").arg(&self.node).status();
        let _rm = std::fs::remove_file(&self.backing);
    }
}

/// Fail, because the environment cannot create the state under test.
///
/// These suites cover what AGENTS.md calls the recurring blind spot —
/// branches needing a mount, a swap area, or a device that changes under
/// the kernel. When the tool that builds that state is missing, the test
/// used to return early, and cargo reported `ok`: indistinguishable from
/// having tested it, so the coverage was imaginary and nothing said so.
///
/// Printing a notice instead does not work. Cargo captures output from
/// passing tests, so the line is invisible without `--nocapture` — which
/// is exactly the run where nobody is looking for it.
///
/// So it fails. These tests are `#[ignore]`d and their ignore strings
/// already name what they require; running them deliberately and lacking
/// the tooling is an environment fault worth surfacing, not a pass. The
/// message says which tool and what went unverified, so it cannot be
/// mistaken for a defect in the code.
#[expect(
    clippy::panic,
    reason = "a test whose fixture cannot be built has not \
                                  passed; the panic is the only signal cargo \
                                  does not swallow"
)]
fn missing_tooling(tool: &str, unverified: &str) -> ! {
    panic!(
        "environment lacks {tool}, so {unverified} was NOT verified. \
         This is an environment limit, not a code defect: install {tool} \
         and re-run, or filter this test out deliberately."
    );
}

/// Write a raw source image of `len` bytes with a recognisable pattern.
fn raw_image(tag: &str, len: usize) -> PathBuf {
    let path = std::env::temp_dir().join(format!("imi-phase-img-{tag}-{}", std::process::id()));
    let body: Vec<u8> = (0..len).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
    std::fs::write(&path, body).expect("write source image");
    path
}

/// A config that never blocks on a TTY and skips the 10 s cooldown, so
/// the test measures the pipeline rather than the wait.
fn config_for(image: &Path, device: &str) -> imi_core::Config {
    let mut config = imi_core::Config::new(image.to_path_buf(), PathBuf::from(device));
    config.yes = true;
    config.skip_cooldown = true;
    config
}

/// Driving the eight phases by hand must flash the device byte-exactly.
///
/// This is the contract the per-phase API exists to support: each
/// phase's return value is the next phase's input, in the documented
/// order, with no hidden state passed behind the caller's back.
#[test]
#[ignore = "requires root and a free loop device"]
fn per_phase_drive_flashes_byte_exact() {
    let dev = Loop::attach("drive", 0x00);
    let image = raw_image("drive", 1_000_000);
    let config = config_for(&image, &dev.node);
    let cancel = AtomicBool::new(false);

    // The same sequence `imi_core::run` performs, spelled out.
    let target = imi_core::phases::phase_0::run(&config, &mut ()).expect("phase 0");
    let devts = imi_core::phases::phase_1::run(&target, &mut ()).expect("phase 1");
    let guard = imi_core::phases::phase_2::run(target, &devts, &mut ()).expect("phase 2");
    let mut guard = imi_core::phases::phase_3::run(guard, &cancel, &mut ()).expect("phase 3");
    let outcome =
        imi_core::phases::phase_4::run(&mut guard, None, &cancel, &mut ()).expect("phase 4");
    imi_core::phases::phase_5::run(&mut guard, &config, outcome, &cancel, &mut ())
        .expect("phase 5");
    // Phase 6 hands the target back, which Phase 7 needs.
    let flashed = imi_core::phases::phase_6::run(guard, &mut ());
    imi_core::phases::phase_7::run(&flashed, &cancel, &mut ()).expect("phase 7");

    let expected = std::fs::read(&image).expect("read source image");
    assert_eq!(
        outcome.bytes_written(),
        u64::try_from(expected.len()).unwrap(),
        "phase 4 must report exactly the image length"
    );

    let written = dev.read_backing();
    assert_eq!(
        written.get(..expected.len()),
        Some(expected.as_slice()),
        "hand-driven phases must flash the image byte-exactly"
    );

    let _rm = std::fs::remove_file(&image);
}

/// Hand-driving must agree with `imi_core::run` on the same input.
///
/// If the two ever diverge, one of them is doing something the other
/// is not, and the per-phase API would be quietly lying about being the
/// same pipeline.
#[test]
#[ignore = "requires root and a free loop device"]
fn per_phase_drive_matches_whole_pipeline_run() {
    let image = raw_image("parity", 512 * 1024);
    let expected = std::fs::read(&image).expect("read source image");
    let cancel = AtomicBool::new(false);

    let by_hand = {
        let dev = Loop::attach("parity-hand", 0xEE);
        let config = config_for(&image, &dev.node);
        let target = imi_core::phases::phase_0::run(&config, &mut ()).expect("phase 0");
        let devts = imi_core::phases::phase_1::run(&target, &mut ()).expect("phase 1");
        let guard = imi_core::phases::phase_2::run(target, &devts, &mut ()).expect("phase 2");
        let mut guard = imi_core::phases::phase_3::run(guard, &cancel, &mut ()).expect("phase 3");
        let outcome =
            imi_core::phases::phase_4::run(&mut guard, None, &cancel, &mut ()).expect("phase 4");
        imi_core::phases::phase_5::run(&mut guard, &config, outcome, &cancel, &mut ())
            .expect("phase 5");
        let flashed = imi_core::phases::phase_6::run(guard, &mut ());
        imi_core::phases::phase_7::run(&flashed, &cancel, &mut ()).expect("phase 7");
        dev.read_backing()
    };

    let by_run = {
        let dev = Loop::attach("parity-run", 0xEE);
        let config = config_for(&image, &dev.node);
        imi_core::run_with_cancel(&config, &cancel).expect("whole-pipeline run");
        dev.read_backing()
    };

    assert_eq!(by_hand.len(), by_run.len());
    assert_eq!(
        by_hand.get(..expected.len()),
        by_run.get(..expected.len()),
        "hand-driven phases and imi_core::run must produce identical devices"
    );

    let _rm = std::fs::remove_file(&image);
}

/// Phases 0-2 are documented as non-destructive: they validate, unmount
/// and take the exclusive claim, but arm nothing and write nothing. The
/// guard is armed in Phase 3, and that boundary is what lets an aborted
/// run before Phase 3 promise the device is untouched.
///
/// Stopping the sequence after Phase 2 is only expressible because the
/// phases are public — this property cannot be tested through the
/// binary at all.
#[test]
#[ignore = "requires root and a free loop device"]
fn phases_0_to_2_leave_the_device_untouched() {
    let dev = Loop::attach("readonly", 0xA5);
    let image = raw_image("readonly", 64 * 1024);
    let config = config_for(&image, &dev.node);

    let before = dev.read_backing();

    let target = imi_core::phases::phase_0::run(&config, &mut ()).expect("phase 0");
    let devts = imi_core::phases::phase_1::run(&target, &mut ()).expect("phase 1");
    let guard = imi_core::phases::phase_2::run(target, &devts, &mut ()).expect("phase 2");

    // Releasing an unarmed guard must be silent and non-destructive.
    drop(guard);

    let after = dev.read_backing();
    assert_eq!(before, after, "phases 0-2 must not modify a single byte");
    assert!(after.iter().all(|&b| b == 0xA5), "backing pattern must survive intact");

    let _rm = std::fs::remove_file(&image);
}

/// A cancel flag that is already set must abort the write rather than
/// flash, and must do so as an `Err` — not a panic and not a partial
/// success reported as success.
///
/// This test deliberately ends with the guard still armed (Phase 3 ran,
/// Phase 4 refused), so it prints the guard's FATAL notice on drop.
/// That output is the expected behaviour being exercised, not a
/// failure: the device really was wiped and never written.
#[test]
#[ignore = "requires root and a free loop device"]
fn a_pre_set_cancel_flag_aborts_the_write() {
    let dev = Loop::attach("cancel", 0x11);
    let image = raw_image("cancel", 256 * 1024);
    let config = config_for(&image, &dev.node);
    // Phase 3 refuses a flag that is already set — that is its own test.
    // Here we reach Phase 4 with a clear flag, then set it.
    let clear = AtomicBool::new(false);
    let cancel = AtomicBool::new(true);

    let target = imi_core::phases::phase_0::run(&config, &mut ()).expect("phase 0");
    let devts = imi_core::phases::phase_1::run(&target, &mut ()).expect("phase 1");
    let guard = imi_core::phases::phase_2::run(target, &devts, &mut ()).expect("phase 2");
    let mut guard = imi_core::phases::phase_3::run(guard, &clear, &mut ()).expect("phase 3");

    let result = imi_core::phases::phase_4::run(&mut guard, None, &cancel, &mut ());
    assert!(result.is_err(), "a pre-set cancel flag must abort phase 4");

    let _rm = std::fs::remove_file(&image);
}

/// A zero throttle must be refused, not honoured.
///
/// `Config::throttle` is a public field and every phase is a public
/// entry point, so a library consumer can set a rate the CLI would have
/// rejected. Before this was validated, `Some(0)` produced a per-chunk
/// target of `u64::MAX` nanoseconds — roughly 584 years — so the flash
/// wrote its first chunk and then appeared to hang forever. The bound
/// on this test is the real assertion: it must fail in seconds.
#[test]
#[ignore = "requires root and a free loop device"]
fn a_zero_throttle_is_refused_rather_than_stalling() {
    let dev = Loop::attach("zerothrottle", 0x33);
    let image = raw_image("zerothrottle", 64 * 1024);
    let mut config = config_for(&image, &dev.node);
    config.throttle = Some(0);
    let cancel = AtomicBool::new(false);

    let target = imi_core::phases::phase_0::run(&config, &mut ()).expect("phase 0");
    let devts = imi_core::phases::phase_1::run(&target, &mut ()).expect("phase 1");
    let guard = imi_core::phases::phase_2::run(target, &devts, &mut ()).expect("phase 2");
    let mut guard = imi_core::phases::phase_3::run(guard, &cancel, &mut ()).expect("phase 3");

    let started = std::time::Instant::now();
    let err = imi_core::phases::phase_4::run(&mut guard, config.throttle, &cancel, &mut ())
        .expect_err("a zero throttle must be refused");
    assert!(started.elapsed() < std::time::Duration::from_secs(10), "must fail fast, not stall");
    assert!(format!("{err:#}").contains("at least 1 byte per second"), "{err:#}");

    let _rm = std::fs::remove_file(&image);
}

/// `run` and `run_with_cancel` must produce identical devices.
///
/// `run` is documented as the same pipeline with nothing to cancel from,
/// and it is implemented by delegating with a never-set flag. That is an
/// implementation detail a refactor could break — someone could give the
/// two entry points genuinely different bodies and no existing test
/// would notice, because every other test drives only one of them.
#[test]
#[ignore = "requires root and two free loop devices"]
fn run_and_run_with_cancel_produce_identical_devices() {
    let image = raw_image("entrypoints", 512 * 1024);
    let expected = std::fs::read(&image).expect("read source image");

    let plain = {
        let dev = Loop::attach("entry-plain", 0xC3);
        let config = config_for(&image, &dev.node);
        imi_core::run(&config).expect("run");
        dev.read_backing()
    };

    let with_cancel = {
        let dev = Loop::attach("entry-cancel", 0xC3);
        let config = config_for(&image, &dev.node);
        imi_core::run_with_cancel(&config, &AtomicBool::new(false)).expect("run_with_cancel");
        dev.read_backing()
    };

    assert_eq!(plain.len(), with_cancel.len());
    assert_eq!(
        plain.get(..expected.len()),
        with_cancel.get(..expected.len()),
        "the two entry points must flash identically"
    );
    assert_eq!(
        plain.get(..expected.len()),
        Some(expected.as_slice()),
        "and both must match the source image"
    );

    let _rm = std::fs::remove_file(&image);
}

/// Classification must be right against a real device, not just in unit
/// tests.
///
/// A cancellation set before the write leaves the device untouched; the
/// same flag observed during the write does not. The unit tests
/// construct both by hand — this proves the phases actually tag them
/// that way when driven against a loop device.
#[test]
#[ignore = "requires root and a free loop device"]
fn a_cancelled_write_is_classified_as_indeterminate() {
    use imi_core::{DeviceState, ErrorKind};

    let image = raw_image("classify", 8 * 1024 * 1024);
    let dev = Loop::attach("classify", 0x5A);
    let config = config_for(&image, &dev.node);

    // Cancel before anything is written: Phase 0 refuses early.
    let pre_set = AtomicBool::new(true);
    let err = imi_core::run_with_cancel(&config, &pre_set).expect_err("must abort");
    assert_eq!(err.kind(), ErrorKind::Cancelled, "{err:#}");
    assert_eq!(
        err.device_state(),
        DeviceState::Untouched,
        "a flag already set must abort before Phase 3 wipes anything: {err:#}"
    );

    // And a phase past the guard's arm reports the opposite.
    let target = imi_core::phases::phase_0::run(&config, &mut ()).expect("phase 0");
    let devts = imi_core::phases::phase_1::run(&target, &mut ()).expect("phase 1");
    let guard = imi_core::phases::phase_2::run(target, &devts, &mut ()).expect("phase 2");
    let clear = AtomicBool::new(false);
    let mut guard = imi_core::phases::phase_3::run(guard, &clear, &mut ()).expect("phase 3");
    let mid = AtomicBool::new(true);
    let mid_err = imi_core::phases::phase_4::run(&mut guard, None, &mid, &mut ())
        .expect_err("a pre-set flag must abort the write");
    assert_eq!(mid_err.kind(), ErrorKind::Cancelled, "{mid_err:#}");
    assert_eq!(
        mid_err.device_state(),
        DeviceState::Indeterminate,
        "the signatures are already wiped, so the device is not safe: {mid_err:#}"
    );

    // Phase 6 consumes and disarms the guard; without it the drop would
    // print the FATAL notice, which is correct behaviour but noisy here.
    imi_core::phases::phase_6::run(guard, &mut ());
    let _rm = std::fs::remove_file(&image);
}

/// The confirmation gate must hold in both directions.
///
/// Two negations guard it: `!config.yes` decides whether to ask at all,
/// and `!events.confirm(..)` decides what the answer means. Dropping
/// either is catastrophic — the first skips confirmation on an
/// unattended run, the second destroys the device when the operator
/// says *no* and aborts when they say *yes*.
///
/// Testable only because confirmation goes through `Events`; while it
/// read `/dev/tty` directly, nothing could exercise it.
#[test]
#[ignore = "requires root and a free loop device"]
fn the_confirmation_gate_holds_in_both_directions() {
    /// Answers as told, and remembers whether it was asked.
    struct Sink {
        answer: bool,
        asked: std::cell::Cell<u32>,
    }
    impl imi_core::Events for Sink {
        fn confirm(&mut self, _s: &imi_core::Summary<'_>) -> bool {
            self.asked.set(self.asked.get() + 1);
            self.answer
        }
    }

    let image = raw_image("confirm", 512 * 1024);
    let dev = Loop::attach("confirm", 0x11);
    let mut config = config_for(&image, &dev.node);
    config.yes = false;

    // Refused: the run must stop, and it must have asked.
    let mut no = Sink { answer: false, asked: std::cell::Cell::new(0) };
    let err = imi_core::phases::phase_0::run(&config, &mut no)
        .expect_err("a refused confirmation must abort");
    assert!(format!("{err:#}").contains("aborted"), "{err:#}");
    assert_eq!(no.asked.get(), 1, "it must actually ask");

    // Approved: the run must proceed.
    let mut yes = Sink { answer: true, asked: std::cell::Cell::new(0) };
    imi_core::phases::phase_0::run(&config, &mut yes)
        .expect("an approved confirmation must proceed");
    assert_eq!(yes.asked.get(), 1);

    // `--yes` must skip the question entirely, not answer it.
    config.yes = true;
    let mut never = Sink { answer: false, asked: std::cell::Cell::new(0) };
    imi_core::phases::phase_0::run(&config, &mut never).expect("--yes must not consult the sink");
    assert_eq!(never.asked.get(), 0, "--yes must not ask; a refusing sink would abort");

    let _rm = std::fs::remove_file(&image);
}

/// Verification must actually detect a device that does not match.
///
/// `full_pipeline_flashes_byte_exact` would pass unchanged if Phase 5
/// were a no-op: it only proves a good flash looks good. Nothing proved
/// the pipeline's verification step reads the device back and compares,
/// which is the entire reason Phase 5 exists — bad media that writes
/// without complaint and returns different bytes.
///
/// The image is altered after the write, so the device is intact and the
/// comparison must fail on the difference. Corrupting the device instead
/// is not possible from here: the guard holds it `O_EXCL`.
#[test]
#[ignore = "requires root and a free loop device"]
fn verification_detects_a_device_that_does_not_match_the_image() {
    use std::io::{Seek, SeekFrom, Write};

    use imi_core::{DeviceState, ErrorKind};

    let image = raw_image("verifycatch", 2 * 1024 * 1024);
    let dev = Loop::attach("verifycatch", 0x3C);
    let config = config_for(&image, &dev.node);
    let cancel = AtomicBool::new(false);

    let target = imi_core::phases::phase_0::run(&config, &mut ()).expect("phase 0");
    let devts = imi_core::phases::phase_1::run(&target, &mut ()).expect("phase 1");
    let guard = imi_core::phases::phase_2::run(target, &devts, &mut ()).expect("phase 2");
    let mut guard = imi_core::phases::phase_3::run(guard, &cancel, &mut ()).expect("phase 3");
    let outcome =
        imi_core::phases::phase_4::run(&mut guard, None, &cancel, &mut ()).expect("phase 4");

    // Alter the image so it no longer matches what was written. Offset
    // chosen past the first chunk so the pipelined and serial arms both
    // have to reach it rather than failing on the opening comparison.
    let corrupt_at = 0x0010_0000 + 4_242_u64;
    {
        let mut f = std::fs::OpenOptions::new().write(true).open(&image).expect("open image");
        f.seek(SeekFrom::Start(corrupt_at)).expect("seek");
        f.write_all(&[0xAA]).expect("write");
        f.sync_all().expect("sync");
    }

    let err = imi_core::phases::phase_5::run(&mut guard, &config, outcome, &cancel, &mut ())
        .expect_err("a device that differs from the image must fail verification");

    let text = format!("{err:#}");
    assert_eq!(err.kind(), ErrorKind::VerificationFailed, "{text}");
    assert_eq!(err.device_state(), DeviceState::Indeterminate, "{text}");
    assert!(
        text.contains(&format!("mismatch at byte offset {corrupt_at}")),
        "the diagnostic must name the exact offset: {text}"
    );

    imi_core::phases::phase_6::run(guard, &mut ());
    let _rm = std::fs::remove_file(&image);
}

/// The Phase 2 claim must actually exclude other openers.
///
/// `CLAIM_FLAGS` pins that `O_EXCL` is in the flag word, but a correct
/// flag word proves nothing about the kernel honouring it. This is the
/// mechanism every downstream mount defence rests on: if the claim does
/// not exclude, udisks2 can open the device mid-flash and remount it.
#[test]
#[ignore = "requires root and a free loop device"]
fn the_phase_2_claim_excludes_other_openers() {
    use std::os::unix::fs::OpenOptionsExt;

    let image = raw_image("exclusive", 512 * 1024);
    let dev = Loop::attach("exclusive", 0x2B);
    let config = config_for(&image, &dev.node);

    let target = imi_core::phases::phase_0::run(&config, &mut ()).expect("phase 0");
    let devts = imi_core::phases::phase_1::run(&target, &mut ()).expect("phase 1");

    // Before the claim, an exclusive open succeeds.
    // Retried for the same reason as the release check below. The device
    // was attached moments ago, and the uevent that followed makes udev
    // probe it; a probe holding the device briefly would fail an
    // instantaneous claim here exactly as it did after the release.
    let claim_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let before = loop {
        let attempt = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_EXCL)
            .open(&dev.node);
        if attempt.is_ok() || std::time::Instant::now() >= claim_deadline {
            break attempt;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(
        before.is_ok(),
        "the device should be claimable before Phase 2; still refused after 5s: {:?}",
        before.err()
    );
    drop(before);

    let guard = imi_core::phases::phase_2::run(target, &devts, &mut ()).expect("phase 2");

    // While the guard holds it, a second exclusive open must be refused.
    let during = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_EXCL)
        .open(&dev.node)
        .expect_err("a claimed device must refuse a second O_EXCL open");
    assert_eq!(
        during.raw_os_error(),
        Some(libc::EBUSY),
        "the kernel must refuse with EBUSY, got {during}"
    );

    // Releasing the guard releases *our* claim. Whether the device is
    // instantly claimable by anyone else is a different question, and on
    // a machine running udev the answer is often no for a moment:
    // dropping the FD emits a `change` uevent, and the probe that
    // follows — or the kernel's own rescan, which takes a transient
    // claim when its caller is not already holding one — can hold the
    // device briefly. Phase 7 exists because of exactly that window.
    //
    // So retry rather than asserting instantaneous availability. This
    // failed intermittently on a developer's machine and never in the
    // container it was written in, which has no udevd at all.
    // Phase 6 takes an ArmedGuard, and this test never armed one — it
    // only ever needed the claim released, which dropping does.
    drop(guard);

    let release_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut last: Option<std::io::Error> = None;
    let after = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_EXCL)
            .open(&dev.node)
        {
            Ok(f) => break Ok(f),
            Err(e) if std::time::Instant::now() < release_deadline => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => break Err(e),
        }
    };
    assert!(
        after.is_ok(),
        "releasing the guard must release the claim; still refused after 5s{}",
        last.map(|e| format!(" (last error: {e})")).unwrap_or_default()
    );

    let _rm = std::fs::remove_file(&image);
}

/// A target that does not exist must be refused as a *target*.
///
/// Nothing tested this. The shared fixture points both paths at
/// `/nonexistent/`, and Phase 0 canonicalises the image first — so the
/// device path was never reached and the two tests named for it were
/// really testing the image check, or, for a non-root caller, the root
/// check. A regression that made Phase 0 accept a missing device would
/// have passed all of them.
///
/// Needs a real image to get past the earlier gate. Root-gated only
/// because the root check comes before both.
#[test]
#[ignore = "requires root: Phase 0 checks for it before either path"]
fn a_nonexistent_target_is_refused_as_a_target() {
    // This suite's own fixtures: raw_image writes a real file, so Phase 0
    // gets past the image gate and reaches the device path.
    let image = raw_image("no-target", 4096);
    let config = config_for(&image, "/nonexistent/imi-no-device");

    let err = imi_core::phases::phase_0::run(&config, &mut ())
        .expect_err("a device path that does not exist must be refused");
    let text = format!("{err:#}");

    let _rm = std::fs::remove_file(&image);

    assert!(
        text.contains("canonicalize device path"),
        "the refusal must name the device, not something earlier: {text}"
    );
    assert_eq!(err.kind(), imi_core::ErrorKind::Refused);
    assert_eq!(err.device_state(), imi_core::DeviceState::Untouched);
}

/// Phase 1 must actually unmount a mounted target.
///
/// Mutation testing leaves `delete !` on the unmount trigger alive: with
/// the negation gone, Phase 1 unmounts only when nothing is mounted, and
/// the residual check then refuses the run. The refusal keeps the device
/// safe, so every other test still passes — but the auto-unmount that
/// makes `imi` usable on a plugged-in stick has silently stopped
/// working.
///
/// This is the only test that puts a real filesystem on the device and
/// mounts it.
#[test]
#[ignore = "requires root, a free loop device and mkfs.ext4"]
fn phase_1_unmounts_a_mounted_target() {
    use std::process::Command;

    let image = raw_image("unmounting", 256 * 1024);
    let dev = Loop::attach("unmounting", 0x6D);

    let mkfs = Command::new("mkfs.ext4").args(["-q", "-F", &dev.node]).status();
    match mkfs {
        Ok(s) if s.success() => {}
        _ => {
            let _rm = std::fs::remove_file(&image);
            missing_tooling("mkfs.ext4", "Phase 1's unmount of a mounted target");
        }
    }

    // Must be inside the auto-unmount whitelist. Anywhere else, Phase 1
    // refuses rather than unmounting — which is the behaviour the
    // whitelist exists for, and is asserted separately below.
    let mnt = PathBuf::from(format!("/run/media/imi-mnt-{}", std::process::id()));
    // Held for the rest of the test: its `Drop` unmounts however this
    // exits, so a failed assertion below cannot leave a mounted device
    // that `Loop::drop` would then fail to detach.
    let _mount = Mounted::at(&dev.node, mnt.clone()).expect("could not mount the target");

    let still_mounted = || {
        std::fs::read_to_string("/proc/self/mountinfo")
            .unwrap_or_default()
            .lines()
            .any(|l| l.contains(&mnt.display().to_string()))
    };
    assert!(still_mounted(), "precondition: the target must be mounted");

    let config = config_for(&image, &dev.node);
    let target = imi_core::phases::phase_0::run(&config, &mut ()).expect("phase 0");
    imi_core::phases::phase_1::run(&target, &mut ()).expect("phase 1 must unmount, not refuse");

    assert!(!still_mounted(), "Phase 1 must have unmounted the target");

    // The other half of the contract: outside the whitelist, Phase 1
    // must refuse rather than unmount someone's system volume.
    let outside = std::env::temp_dir().join(format!("imi-mnt-out-{}", std::process::id()));
    if let Some(_outside_mount) = Mounted::at(&dev.node, outside) {
        let err = imi_core::phases::phase_1::run(&target, &mut ())
            .expect_err("a mount outside the whitelist must be refused, not unmounted");
        let text = format!("{err:#}");
        assert!(text.contains("Refusing to auto-unmount"), "{text}");
    }

    let _rm = std::fs::remove_file(&image);
}

/// A device that changes underneath the pipeline must be caught.
///
/// `DeviceIdentity` is captured in Phase 0 and re-verified in Phase 2,
/// against the scenario where a stick is pulled and a different one
/// plugged in between the probe and the claim. Everything downstream —
/// the wipe, the write, the verify — trusts that the device it holds is
/// the device that was checked.
///
/// The only end-to-end coverage was `size_of_val(identity()) > 0`, which
/// asserts nothing about the check working. Here the loop device's
/// backing file is truncated and `losetup -c` makes the driver re-read
/// it, so the device really does change size under a live probe — the
/// `BLKGETSIZE64` arm of the re-check has to notice.
#[test]
#[ignore = "requires root, a free loop device and losetup -c"]
fn a_device_that_changes_size_after_probing_is_refused() {
    use std::process::Command;

    let image = raw_image("swapped", 256 * 1024);
    let dev = Loop::attach("swapped", 0x4F);
    let config = config_for(&image, &dev.node);

    // Phase 0 snapshots rdev, model and size.
    let target = imi_core::phases::phase_0::run(&config, &mut ()).expect("phase 0");
    let devts = imi_core::phases::phase_1::run(&target, &mut ()).expect("phase 1");
    let probed = target.device_size();

    // Shrink the device under the pipeline's feet.
    let halved = probed / 2;
    let truncated = Command::new("truncate")
        .args(["-s", &halved.to_string(), &dev.backing.display().to_string()])
        .status();
    if !truncated.is_ok_and(|s| s.success()) {
        let _rm = std::fs::remove_file(&image);
        missing_tooling("truncate", "the size-change refusal");
    }
    let refreshed = Command::new("losetup").args(["-c", &dev.node]).status();
    if !refreshed.is_ok_and(|s| s.success()) {
        let _rm = std::fs::remove_file(&image);
        missing_tooling("losetup -c", "the size-change refusal");
    }

    // Phase 2 claims the node, then re-verifies what it claimed.
    let err = imi_core::phases::phase_2::run(target, &devts, &mut ())
        .expect_err("a device whose size changed after probing must be refused");
    let text = format!("{err:#}");
    // Specifically the size arm: a looser match would also accept the
    // rdev arm firing, which is a different failure with a different
    // cause and would leave this test passing for the wrong reason.
    assert!(
        text.contains("size"),
        "the size arm of the identity re-check must be the one that fires, got: {text}"
    );
    assert!(
        text.contains(&probed.to_string()) || text.contains(&halved.to_string()),
        "the diagnostic must name a concrete size, got: {text}"
    );
    assert_eq!(
        err.device_state(),
        imi_core::DeviceState::Untouched,
        "nothing may be written when the identity check fails: {text}"
    );

    let _rm = std::fs::remove_file(&image);
}

/// Phase 1 must disable swap on the target before anything is written.
///
/// An active swap area means the kernel is writing to the device right
/// now. Flashing over it corrupts both: the image lands under the
/// kernel's feet, and the kernel keeps paging into blocks the write has
/// already replaced.
///
/// The `swapoff` path was reachable by no test at all — every fixture
/// flashes a device with no swap on it, so the branch and its `unsafe`
/// call never executed. `mkswap` and `swapon` make it reachable.
#[test]
#[ignore = "requires root, a free loop device, mkswap and swapon"]
fn phase_1_disables_swap_on_the_target() {
    use std::process::Command;

    let image = raw_image("swapoff", 256 * 1024);
    let dev = Loop::attach("swapoff", 0x8E);

    if !Command::new("mkswap").args(["-f", &dev.node]).status().is_ok_and(|s| s.success()) {
        let _rm = std::fs::remove_file(&image);
        missing_tooling("mkswap", "Phase 1's swapoff on the target");
    }
    if !Command::new("swapon").arg(&dev.node).status().is_ok_and(|s| s.success()) {
        let _rm = std::fs::remove_file(&image);
        missing_tooling("a usable swapon", "Phase 1's swapoff on the target");
    }

    let active = || {
        std::fs::read_to_string("/proc/swaps")
            .unwrap_or_default()
            .lines()
            .any(|l| l.split_whitespace().next() == Some(dev.node.as_str()))
    };
    assert!(active(), "precondition: swap must be active on the target");

    let config = config_for(&image, &dev.node);
    let outcome = imi_core::phases::phase_0::run(&config, &mut ())
        .and_then(|target| imi_core::phases::phase_1::run(&target, &mut ()).map(|_| ()));

    let still_on = active();
    if still_on {
        // Never leave a swap attached to a loop device we are about to detach.
        drop(Command::new("swapoff").arg(&dev.node).status());
    }
    let _rm = std::fs::remove_file(&image);

    outcome.expect("phase 1 must disable swap, not refuse");
    assert!(!still_on, "Phase 1 must have called swapoff on the target");
}
