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

// `unused_crate_dependencies` is evaluated per compilation target, and an
// integration-test crate compiles against every [dependencies] entry.
// Anchor them explicitly; the binary is what actually uses them.
use anyhow as _;
use bzip2 as _;
use clap as _;
use ctrlc as _;
use flate2 as _;
use indicatif as _;
use libc as _;
use nix as _;
use xz2 as _;
use zstd as _;

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

impl Drop for Loop {
    fn drop(&mut self) {
        // Best-effort teardown; a leaked attachment only affects the
        // test host and is visible in `losetup -l`.
        let _detach = Command::new("losetup").arg("-d").arg(&self.node).status();
        let _rm = std::fs::remove_file(&self.backing);
    }
}

/// Run the real `imi` binary with `--yes` against `(image, device)`,
/// returning `(exit_success, combined_output)`.
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

/// Full happy path: flash a raw image with an unaligned tail, expect
/// SUCCESS, and verify the device content byte-for-byte through the
/// backing file — including that the wiped tail region stayed zeroed.
#[test]
#[ignore = "requires root and kernel loop devices"]
fn full_pipeline_flashes_byte_exact() {
    let lo = Loop::attach("happy", 0xEE);
    // 2 MiB + 137 bytes: exercises full chunks plus the buffered tail.
    let img_path = std::env::temp_dir().join(format!("imi-it-img-{}.bin", std::process::id()));
    let mut payload = vec![0xA5_u8; 2 * 1024 * 1024];
    payload.extend_from_slice(&[0x5A; 137]);
    std::fs::write(&img_path, &payload).unwrap();

    let (ok, out) = run_imi(&img_path, &lo.node);
    assert!(ok, "pipeline failed:\n{out}");
    assert!(out.contains("SUCCESS"), "missing SUCCESS line:\n{out}");

    let device = lo.read_backing();
    assert_eq!(&device[..payload.len()], payload.as_slice(), "device != image");
    // The last 1 MiB was wiped in Phase 3 and never rewritten.
    let tail = &device[BACKING_LEN - 1024 * 1024..];
    assert!(tail.iter().all(|&b| b == 0), "tail wipe did not persist");

    std::fs::remove_file(&img_path).unwrap();
}

/// Self-reference refusal: flashing a loop device from its own backing
/// file must be refused in Phase 0, with the backing file untouched.
/// (Pre-fix, this silently zeroed the head and printed SUCCESS.)
#[test]
#[ignore = "requires root and kernel loop devices"]
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
#[ignore = "requires root and kernel loop devices"]
fn multi_member_gzip_flashes_all_members() {
    let lo = Loop::attach("multigz", 0x00);
    // Two members with distinct fill bytes, built via the same crate
    // family the binary uses is unnecessary — any two valid gzip
    // members concatenated exercise the contract. Use `gzip` from the
    // test host if present, else skip-fail with a clear message.
    let a = vec![0x11_u8; 300_000];
    let b = vec![0x22_u8; 300_000];
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let (pa, pb) = (dir.join(format!("imi-it-a-{pid}")), dir.join(format!("imi-it-b-{pid}")));
    std::fs::write(&pa, &a).unwrap();
    std::fs::write(&pb, &b).unwrap();
    let gz = |p: &std::path::Path| {
        let st = Command::new("gzip").arg("-kf").arg(p).status().expect("gzip runnable");
        assert!(st.success());
        std::fs::read(p.with_extension("gz")).unwrap()
    };
    let mut multi = gz(&pa);
    multi.extend_from_slice(&gz(&pb));
    let img = dir.join(format!("imi-it-multi-{pid}.gz"));
    std::fs::write(&img, &multi).unwrap();

    let (ok, out) = run_imi(&img, &lo.node);
    assert!(ok, "multi-member flash failed:\n{out}");
    let device = lo.read_backing();
    assert_eq!(&device[..a.len()], a.as_slice(), "first member mismatch");
    assert_eq!(
        &device[a.len()..a.len() + b.len()],
        b.as_slice(),
        "second member missing/mismatched"
    );

    for p in [pa.clone(), pb.clone(), pa.with_extension("gz"), pb.with_extension("gz"), img] {
        let _cleanup = std::fs::remove_file(p);
    }
}
