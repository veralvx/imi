//! Loop-device integration tests for the destructive pipeline.
//!
//! These are `#[ignore]`d by default because they require root and a
//! kernel loop driver. Run them explicitly on a capable machine:
//!
//! ```sh
//! sudo -E cargo test --test loop_pipeline -- --ignored --test-threads=1
//! ```
//!
//! `--test-threads=1` matters: the tests attach loop devices and must
//! not race each other for `losetup -f`.
//!
//! The test harness shells out to `losetup` (AGENTS.md hard rule 1
//! binds `imi` itself, not its test scaffolding). Each test creates its
//! own backing file, attaches a loop device, runs the real binary via
//! `CARGO_BIN_EXE_imi`, and verifies the outcome through the backing
//! file — the same independent-evidence pattern as the manual test log
//! in `.agents/docs/01-phase-0-preflight.md`.

// The `allow-*-in-tests` knobs in clippy.toml key off the `#[test]`
// attribute and therefore DO cover the test bodies below, even in an
// integration crate. What they do not cover is the shared scaffolding
// (`Loop`, `run_imi`), which is not itself a `#[test]` fn — hence the
// expect_used grant. `tests_outside_test_module` is structural: an
// integration-test crate conventionally has no `#[cfg(test)]` module.
#![expect(
    clippy::expect_used,
    clippy::tests_outside_test_module,
    reason = "integration-test crate: scaffolding fns sit outside #[test] \
              bodies, so the in-tests exemptions do not reach them; the \
              crate has no cfg(test) module by design"
)]

use std::path::PathBuf;
use std::process::Command;

// This test drives the pipeline through `CARGO_BIN_EXE_imi` as a black
// box, so it names none of the `imi` package's dependencies. They are
// still linked into the test target, and `unused_crate_dependencies` is
// evaluated per compilation unit, so acknowledge them here rather than
// weakening the lint. This list must mirror `crates/imi/Cargo.toml`:
// the decompressors and `nix`/`libc`/`indicatif` are `imi-core`'s
// dependencies, not this package's.
use anyhow as _;
use clap as _;
use ctrlc as _;
use imi_core as _;
use indicatif as _;

/// Size of every loop backing file: comfortably above the
/// `2 * WIPE_REGION` floor and small enough to flash in well under a
/// second (the 10-second cooldown dominates each test's wall time).
const BACKING_LEN: usize = 8 * 1024 * 1024;

/// A loop device attached for the duration of one test; detached on drop.
struct Loop {
    /// Device node path, e.g. `/dev/loop0`.
    node: String,
    /// The regular file the loop device reads from and writes to.
    backing: PathBuf,
}

impl Loop {
    /// Create a fresh backing file of `BACKING_LEN` bytes filled with
    /// `fill`, and attach it to the first free loop device.
    fn attach(tag: &str, fill: u8) -> Self {
        let backing = std::env::temp_dir().join(format!("imi-it-{tag}-{}.img", std::process::id()));
        std::fs::write(&backing, vec![fill; BACKING_LEN]).expect("write backing file");
        let out = Command::new("losetup")
            .arg("-f")
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

    fn read_backing(&self) -> Vec<u8> {
        std::fs::read(&self.backing).expect("read backing file")
    }
}

/// A temp file that deletes itself, however the test leaves.
///
/// The images here were removed with trailing statements, two of them
/// `unwrap`ped — so a failed assertion above skipped the removal and
/// left a multi-megabyte file behind. `imi-core` has the same fixture as
/// `common::testing::TempPath`; that module is `pub(crate)` and cannot
/// be reached from another crate's integration test.
struct TempImage {
    /// The file to unlink on drop.
    path: PathBuf,
}

impl TempImage {
    /// Write `body` to a uniquely named temp file.
    fn write(tag: &str, body: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!("imi-{tag}-{}.bin", std::process::id()));
        std::fs::write(&path, body).expect("write source image");
        Self { path }
    }

    /// Take ownership of a file another tool created.
    ///
    /// `gzip -kf` writes its output beside the input; nothing else would
    /// unlink it.
    fn adopt(path: PathBuf) -> Self {
        Self { path }
    }
}

impl std::ops::Deref for TempImage {
    type Target = std::path::Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl AsRef<std::ffi::OsStr> for TempImage {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.path.as_os_str()
    }
}

impl Drop for TempImage {
    fn drop(&mut self) {
        let _rm = std::fs::remove_file(&self.path);
    }
}

/// A mountpoint that unmounts itself, however the test leaves.
///
/// The manual form this replaces cleaned up before asserting, which
/// handles a failed assertion but not a panic — and three `expect`s and
/// `unwrap`s sit between the mount and the cleanup. A surviving mount
/// makes `Loop::drop`'s `losetup -d` fail too, so both leak.
///
/// `imi-core` has the same fixture in `common::testing`, but that module
/// is `pub(crate)`: an integration test in this crate cannot reach it,
/// and exporting it would put a test helper in the public API.
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
        if Command::new("mount").arg(device).arg(&dir).status().is_ok_and(|s| s.success()) {
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
        // Best-effort teardown; a leaked attachment only affects the
        // test host and is visible in `losetup -l`.
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

/// Run the real `imi` binary with `--yes` against `(image, device)`,
/// returning `(exit_success, combined_output)`.
/// Run the binary and return its success flag with stdout then stderr,
/// concatenated in that order.
///
/// The two streams are *not* interleaved chronologically: everything
/// stdout produced precedes everything stderr did, whatever the real
/// timing was. Assertions on the relative order of two lines are
/// therefore only meaningful when both come from the same stream. The
/// phase sequence checked below is all `println!`, and
/// [`stream_split_is_what_the_ordering_assertions_assume`] pins that.
fn run_imi(image: &std::path::Path, device: &str) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_imi"))
        .args(["-i"])
        .arg(image)
        .args(["-d", device, "--yes"])
        .output()
        .expect("spawn imi");
    let text =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

/// The exit codes a script branches on.
///
/// 0 for success, 1 for a run that failed, 2 for arguments `clap`
/// rejected. The distinction between 1 and 2 is the one that matters: a
/// wrapper treating every non-zero code alike would retry a typo'd
/// device path forever, and one treating 2 as a device failure would
/// report hardware trouble for its own bug.
///
/// None of these needs root — they all fail before the pipeline starts —
/// so this runs in the ordinary suite rather than behind `--ignored`.
#[test]
#[cfg_attr(miri, ignore)] // unsupported operation
fn exit_codes_distinguish_usage_errors_from_run_failures() {
    let code = |args: &[&str]| -> i32 {
        Command::new(env!("CARGO_BIN_EXE_imi"))
            .args(args)
            .output()
            .expect("spawn imi")
            .status
            .code()
            .expect("exited normally")
    };

    assert_eq!(code(&["--help"]), 0, "--help is a successful invocation");
    assert_eq!(code(&["--version"]), 0, "--version is too");

    assert_eq!(code(&[]), 2, "missing required arguments is a usage error");
    assert_eq!(
        code(&["-i", "/nonexistent.img", "-d", "/dev/null", "--throttle", "0"]),
        2,
        "a throttle that cannot be parsed is a usage error, not a run failure"
    );

    assert_eq!(
        code(&["-i", "/nonexistent-image.img", "-d", "/dev/null", "--yes"]),
        1,
        "arguments that parse but do not work are a run failure"
    );
}

/// Exit 1 alone does not mean the device was touched.
///
/// `main`'s doc tells a script to read the `FATAL` notice, not the exit
/// code, to tell "refused before writing" from "written, state
/// unknown". This pins the half that needs no root: a refusal must not
/// print it. The other half — an interrupted flash that must print it —
/// is `an_interrupted_flash_warns_that_the_device_is_unsafe` below.
///
/// A false FATAL is not a harmless extra line. It tells an operator a
/// device may be corrupt when the tool never opened it, and the habit it
/// teaches is to ignore the notice.
///
/// Both cases here fail in Phase 0, before Phase 2 constructs a
/// `FlashGuard` at all — so what this proves is that the early refusal
/// path stays silent, not that an existing guard would. Forcing
/// `would_warn_on_drop` to `true` does not fail this test, because there
/// is no guard to ask.
///
/// The case where a guard exists and must stay quiet is
/// [`full_pipeline_flashes_byte_exact`], which asserts `FATAL` is absent
/// after a successful flash — a run that built a guard, armed it through
/// four phases and disarmed it. Between the two, both directions are
/// covered: no guard, and a guard that was armed and correctly stood
/// down.
#[test]
#[cfg_attr(miri, ignore)] // unsupported operation
fn a_refusal_before_writing_prints_no_fatal_notice() {
    for args in [
        vec!["-i", "/nonexistent-image.img", "-d", "/dev/null", "--yes"],
        vec!["-i", "/etc/hostname", "-d", "/etc/hostname", "--yes"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_imi")).args(&args).output().expect("spawn imi");
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "expected a run failure for {args:?}:\n{err}");
        assert!(
            !err.contains("FATAL"),
            "a refusal that never opened the device must not warn about it; {args:?} printed:\n{err}"
        );
        assert!(err.contains("error:"), "a failure must say why; {args:?} printed:\n{err}");
    }
}

/// The phase lines must all be on stdout, or the ordering check lies.
///
/// `run_imi` concatenates stdout then stderr, so a phase line moved to
/// stderr would appear after every other one regardless of when it was
/// printed — and the sequence assertion in the happy-path test would
/// fail reporting "out of order" for a run whose order was fine. This
/// pins the assumption at the source rather than leaving it implicit in
/// a test that would misdiagnose its own failure.
#[test]
fn stream_split_is_what_the_ordering_assertions_assume() {
    let ui =
        std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui.rs"))
            .expect("read ui.rs");

    for line in [
        "Wiping partition signatures",
        "Flashing image to",
        "Verifying data integrity",
        "Securing kernel and blocking automounts",
        "SUCCESS: You can now safely remove",
    ] {
        // Every occurrence, not the first: a second one printing to
        // stderr would be invisible to `find`, and it is the one that
        // would break the ordering.
        let sites: Vec<&str> = ui.lines().filter(|l| l.contains(line)).collect();
        assert!(!sites.is_empty(), "phase line {line:?} is no longer in ui.rs");
        for emitted in sites {
            // `contains("println!")` is true for `eprintln!` — the very
            // case this exists to catch. Test for the stderr macro
            // instead, which has no such overlap.
            assert!(
                !emitted.contains("eprintln!"),
                "{line:?} must go to stdout — the ordering assertion in \
                 full_pipeline_flashes_byte_exact compares it against other \
                 stdout lines, and run_imi does not interleave the streams. \
                 Found: {emitted}"
            );
        }
    }
}

/// Full happy path: flash a raw image with an unaligned tail, expect
/// SUCCESS, and verify the device content byte-for-byte through the
/// backing file — including that the wiped tail region stayed zeroed.
#[test]
#[ignore = "requires root and kernel loop devices"] // miri unsupported operation
fn full_pipeline_flashes_byte_exact() {
    let lo = Loop::attach("happy", 0xEE);
    // 2 MiB + 137 bytes: exercises full chunks plus the buffered tail.
    let mut payload = vec![0xA5_u8; 2 * 1024 * 1024];
    payload.extend_from_slice(&[0x5A; 137]);
    let img_path = TempImage::write("it-img", &payload);

    let (ok, out) = run_imi(&img_path, &lo.node);
    assert!(ok, "pipeline failed:\n{out}");

    // Every phase line, in order, and the closing SUCCESS.
    //
    // The front end's Events methods are only observable through output,
    // so nothing else in the suite can tell whether they fired: replacing
    // `phase_started` with an empty body removes every indication a flash
    // is happening and leaves the unit tests green. Asserting the
    // sequence rather than the presence also catches a phase emitted out
    // of order, which is what a mis-threaded pipeline would look like
    // from here.
    let mut rest = out.as_str();
    for line in [
        "Wiping partition signatures",
        "Flashing image to",
        "Verifying data integrity",
        "Securing kernel and blocking automounts",
        "SUCCESS: You can now safely remove",
    ] {
        let (_, after) = rest
            .split_once(line)
            .unwrap_or_else(|| panic!("missing or out-of-order phase line {line:?} in:\n{out}"));
        rest = after;
    }
    assert!(rest.contains(&lo.node), "the SUCCESS line must name the device:\n{out}");

    // The FATAL notice must be absent, not merely outnumbered.
    //
    // Nothing asserted this. Every disarm in the pipeline could have
    // been lost — FlashGuard::into_file's, or Phase 5's — and each
    // successful flash would have printed "the device is in an
    // inconsistent state, DO NOT REMOVE IT" immediately above the
    // SUCCESS line, while this test stayed green because it only checked
    // that SUCCESS appeared. An operator who sees that on a good flash
    // learns to ignore it, which costs the notice its only purpose.
    assert!(
        !out.contains("FATAL"),
        "a successful flash must not warn that the device is inconsistent:\n{out}"
    );
    assert!(
        !out.contains("DO NOT REMOVE"),
        "a successful flash must not tell the operator to leave the device alone:\n{out}"
    );

    let device = lo.read_backing();
    assert_eq!(&device[..payload.len()], payload.as_slice(), "device != image");
    // The last 1 MiB was wiped in Phase 3 and never rewritten.
    let tail = &device[BACKING_LEN - 1024 * 1024..];
    assert!(tail.iter().all(|&b| b == 0), "tail wipe did not persist");
}

/// Self-reference refusal: flashing a loop device from its own backing
/// file must be refused in Phase 0, with the backing file untouched.
/// (Pre-fix, this silently zeroed the head and printed SUCCESS.)
#[test]
#[ignore = "requires root and kernel loop devices"] // miri unsupported operation
fn refuses_backing_file_as_image() {
    let lo = Loop::attach("selfref", 0xAB);
    let (ok, out) = run_imi(&lo.backing, &lo.node);
    assert!(!ok, "self-referential flash must fail:\n{out}");
    assert!(out.contains("backing file of loop device"), "wrong refusal:\n{out}");
    assert!(
        lo.read_backing().iter().all(|&b| b == 0xAB),
        "backing file was modified by a refused run"
    );
}

/// Multi-member gzip must flash the full concatenation, not just the
/// first member. (Pre-fix, the single-member decoder truncated at the
/// member boundary and verification blessed it.)
#[test]
#[ignore = "requires root and kernel loop devices"] // miri unsupported operation
fn multi_member_gzip_flashes_all_members() {
    let lo = Loop::attach("multigz", 0x00);
    // Two members with distinct fill bytes, built via the same crate
    // family the binary uses is unnecessary — any two valid gzip
    // members concatenated exercise the contract. Use `gzip` from the
    // test host if present, else skip-fail with a clear message.
    let a = vec![0x11_u8; 300_000];
    let b = vec![0x22_u8; 300_000];
    // Five files, all self-deleting: the two members, the two `.gz`
    // files `gzip -kf` writes beside them, and the concatenation. The
    // trailing cleanup loop this replaces leaked all five on a failed
    // assertion — measured, not assumed.
    let pa = TempImage::write("it-a", &a);
    let pb = TempImage::write("it-b", &b);
    // `gzip -kf foo.bin` writes `foo.bin.gz` — appended, not substituted,
    // which `with_extension` would get wrong.
    let gz_of = |p: &std::path::Path| {
        let mut n = p.as_os_str().to_owned();
        n.push(".gz");
        PathBuf::from(n)
    };
    let _ga = TempImage::adopt(gz_of(&pa));
    let _gb = TempImage::adopt(gz_of(&pb));
    let gz = |p: &std::path::Path| {
        let st = Command::new("gzip").arg("-kf").arg(p).status().expect("gzip runnable");
        assert!(st.success());
        std::fs::read(gz_of(p)).unwrap()
    };
    let mut multi = gz(&pa);
    multi.extend_from_slice(&gz(&pb));
    let img = TempImage::write("it-multi", &multi);

    let (ok, out) = run_imi(&img, &lo.node);
    assert!(ok, "multi-member flash failed:\n{out}");
    let device = lo.read_backing();
    assert_eq!(&device[..a.len()], a.as_slice(), "first member mismatch");
    assert_eq!(
        &device[a.len()..a.len() + b.len()],
        b.as_slice(),
        "second member missing/mismatched"
    );
}

/// Interrupting a flash must tell the operator the device is unsafe.
///
/// `FlashGuard::drop` prints the FATAL notice, and it is the single most
/// important line this tool emits: it is the only thing standing between
/// an interrupted flash and someone unplugging a half-written disk.
/// Nothing asserted it — mutation testing leaves `drop -> ()` alive,
/// because stderr from a `Drop` during unwind cannot be observed from
/// inside the process.
///
/// Driving the binary as a subprocess and interrupting it mid-write is
/// the only way to see it, and it is also exactly the real scenario:
/// SIGINT sets the cancel flag, Phase 4 aborts with the guard still
/// armed, and the guard speaks on the way out.
#[test]
#[ignore = "requires root and a free loop device"]
fn an_interrupted_flash_warns_that_the_device_is_unsafe() {
    use std::io::Read as _;
    use std::process::Stdio;

    let lo = Loop::attach("fatal", 0xC7);
    let img_path = TempImage::write("fatal", &vec![0x31_u8; 6 * 1024 * 1024]);

    // Throttled hard so the write is still running when the signal lands.
    let mut child = Command::new(env!("CARGO_BIN_EXE_imi"))
        .args(["-i"])
        .arg(&img_path)
        .args(["-d", &lo.node, "--yes", "--throttle", "1M"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn imi");

    std::thread::sleep(std::time::Duration::from_millis(1500));
    // SIGINT is what an operator's Ctrl-C sends.
    let killed =
        Command::new("kill").args(["-INT", &child.id().to_string()]).status().expect("signal");
    assert!(killed.success(), "could not signal the child");

    let status = child.wait().expect("wait");
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        drop(e.read_to_string(&mut err));
    }

    assert!(!status.success(), "an interrupted flash must not report success");
    assert!(
        err.contains("FATAL"),
        "the operator must be told the flash was interrupted; stderr was:\n{err}"
    );
    assert!(
        err.contains("DO NOT REMOVE"),
        "the notice must say not to unplug the device; stderr was:\n{err}"
    );
    assert!(err.contains(&lo.node), "the notice must name the device; stderr was:\n{err}");
}

/// The operator must see what the tool did on their behalf.
///
/// Phase 1 unmounts a mounted target and disables swap on it. Those are
/// the only lines that tell someone their filesystem was taken away, and
/// nothing asserted how they render — the whole class sat outside the
/// harness, because the other tests flash devices with no mounts and no
/// swap, so the branches never execute.
///
/// This is also the path whose wording changed in the sans-I/O refactor:
/// 0.1.7 printed `" -> Unmounting X…"`, the event API renders
/// `" -> unmounting X"`. Pinning it means the next change to that format
/// is a deliberate one.
#[test]
#[ignore = "requires root, a free loop device and mkfs.ext4"]
fn the_operator_is_told_when_a_mount_is_taken_away() {
    let lo = Loop::attach("told", 0x9A);

    if !Command::new("mkfs.ext4").args(["-q", "-F", &lo.node]).status().is_ok_and(|s| s.success()) {
        missing_tooling("mkfs.ext4", "the unmount notice the operator sees");
    }

    // Inside the auto-unmount whitelist, or Phase 1 refuses by design.
    let mnt = PathBuf::from(format!("/run/media/imi-told-{}", std::process::id()));
    // Held for the rest of the test: its `Drop` unmounts however this
    // exits, including on a panic between here and the assertions.
    let _mount = Mounted::at(&lo.node, mnt.clone()).expect("could not mount the target");

    let img_path = TempImage::write("told", &vec![0x77_u8; 1024 * 1024]);

    let (ok, out) = run_imi(&img_path, &lo.node);

    assert!(ok, "the flash should succeed after unmounting:\n{out}");
    assert!(
        out.contains(&format!(" -> unmounting {}", mnt.display())),
        "the operator must be told which mount was removed, and how it renders:\n{out}"
    );
}
