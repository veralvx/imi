//! Compiles the pipeline a consumer would write from the docs alone.
//!
//! Not run — it would flash a device. The point is that it *compiles*:
//! every type the sequence needs must be reachable from the crate root,
//! every signature must compose, and the values must thread as the crate
//! doc says they do. A missing re-export or a changed signature fails
//! here, in the shape a consumer would meet it.
#![expect(
    clippy::tests_outside_test_module,
    reason = "structural: an integration test target has no cfg(test) module to sit inside"
)]

use bzip2 as _;
use flate2 as _;
use libc as _;
use nix as _;
use xz2 as _;
use zstd as _;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use imi_core::{Config, DeviceState, ErrorKind, Events, Phase, PhaseOutcome, Summary};

/// The front end a GUI would supply.
#[derive(Default)]
struct MyUi {
    written: u64,
}

impl Events for MyUi {
    fn phase_started(&mut self, _phase: Phase) {}
    fn phase_finished(&mut self, _phase: Phase, _outcome: PhaseOutcome) {}
    fn progress(&mut self, _phase: Phase, done: u64, _total: Option<u64>) {
        self.written = done;
    }
    fn confirm(&mut self, summary: &Summary<'_>) -> bool {
        // Every field the banner needs must be readable, and each at the
        // type a front end would render it as. Written as a returned
        // tuple so the annotations are load-bearing rather than discarded
        // bindings.
        fn readable<'a>(
            s: &Summary<'a>,
        ) -> (
            &'a std::path::Path,
            Option<&'a str>,
            &'a std::path::Path,
            imi_core::Compression,
            Option<u64>,
            u64,
        ) {
            (s.device, s.model, s.image, s.compression, s.raw_image_size, s.device_size)
        }
        let (device, _model, _image, _comp, _raw, size) = readable(summary);
        !device.as_os_str().is_empty() && size > 0
    }
    fn finished(&mut self, _device: &std::path::Path) {}
}

/// Drive the eight phases by hand, exactly as the crate doc describes.
#[expect(dead_code, reason = "compiled, never run: it would flash a device")]
fn drive_by_hand(config: &Config, cancel: &AtomicBool, ui: &mut MyUi) -> imi_core::Result<()> {
    let target = imi_core::phases::phase_0::run(config, ui)?;
    let devts = imi_core::phases::phase_1::run(&target, ui)?;
    // Phase 2 takes the target by value and returns it paired with the
    // claim; Phase 6 hands it back for Phase 7.
    let session = imi_core::phases::phase_2::run(target, &devts, ui)?;
    let mut session = imi_core::phases::phase_3::run(session, cancel, ui)?;
    let outcome = imi_core::phases::phase_4::run(&mut session, config.throttle, cancel, ui)?;
    imi_core::phases::phase_5::run(&mut session, config, outcome, cancel, ui)?;
    // No `?`: Phase 6 cannot fail.
    let flashed = imi_core::phases::phase_6::run(session, ui);
    imi_core::phases::phase_7::run(&flashed, cancel, ui)?;
    // The step that is not a phase, which the crate doc warns about.
    ui.finished(flashed.device_path());
    Ok(())
}

/// Read everything the accessors expose, as a caller reporting a result
/// would.
#[expect(dead_code, reason = "compiled for its signature, not run")]
fn inspect(target: &imi_core::Target, outcome: imi_core::FlashOutcome) -> u64 {
    // Every accessor used for its value, so removing one is a compile
    // error here rather than an unnoticed narrowing of the surface.
    let id = target.identity().clone();
    assert_eq!(&id, target.identity(), "DeviceIdentity must compare equal to its clone");

    let named = !target.image_path().as_os_str().is_empty()
        && !target.device_path().as_os_str().is_empty()
        && !target.device_kname().is_empty()
        && !target.compression().label().is_empty()
        && target.raw_image_size().is_none_or(|n| n > 0);

    if named { outcome.bytes_written().min(target.device_size()) } else { 0 }
}

/// Classify a failure the way the crate doc's example does.
#[expect(dead_code, reason = "compiled for its signature, not run")]
fn report(e: &imi_core::Error) -> &'static str {
    match (e.device_state(), e.kind()) {
        (DeviceState::Untouched, _) => "unchanged",
        (DeviceState::Indeterminate, ErrorKind::Cancelled) => "cancelled mid-write",
        (DeviceState::Indeterminate, _) => "DO NOT REMOVE",
        (DeviceState::Written, _) => "written; unmount first",
        _ => "unknown",
    }
}

/// The README's library example, verbatim.
///
/// Nothing else compiles it. The crate does not pull the README in with
/// `#![doc = include_str!(..)]` — it would duplicate the crate doc and
/// drag install instructions into the API reference — and no CI job
/// touches it, so the first person to find a signature change would be
/// a reader whose copy-paste does not build.
///
/// Compiled, never run: it flashes a device. Kept in step with the
/// README by [`the_readme_example_is_the_one_compiled_above`], because a
/// copy that compiles is worthless if the README has since said
/// something else.
#[expect(dead_code, reason = "compiled for its signature, not run")]
#[expect(
    unused_qualifications,
    clippy::expect_used,
    reason = "verbatim from the README, which has no `use` in scope and \
              legitimately uses expect() as README examples do — rewriting \
              either would break the property this test exists for"
)]
#[rustfmt::skip]
fn readme_library_example() {
let mut config = imi_core::Config::new(
    PathBuf::from("image.iso.zst"),
    PathBuf::from("/dev/sdc"),
);
config.yes = true;                       // skip the interactive confirmation
imi_core::run(&config).expect("flash failed");
}

/// The compiled copy must still be what the README shows.
///
/// The function above gates the example against signature changes. It
/// does not, on its own, gate the README against being edited — the
/// copy would go on compiling while the published text drifted away
/// from it, which is the failure the copy exists to prevent, one level
/// up.
///
/// Compares token streams rather than lines: `cargo fmt` reflows the
/// copy and will not reflow the README, so a line-for-line match is not
/// something this can ask for.
#[test]
fn the_readme_example_is_the_one_compiled_above() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root");
    let readme = std::fs::read_to_string(root.join("README.md")).expect("read README.md");
    let test_src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/api_consumer.rs"),
    )
    .expect("read this file");

    let between = |hay: &str, start: &str, end: &str| -> String {
        let (_, rest) = hay.split_once(start).unwrap_or_else(|| panic!("missing {start:?}"));
        let (body, _) = rest.split_once(end).unwrap_or_else(|| panic!("missing {end:?}"));
        body.to_owned()
    };

    let doc = between(&readme, "```rust,no_run\n", "```");
    let copy = between(&test_src, "fn readme_library_example() {\n", "\n}");

    // Line for line, because `#[rustfmt::skip]` on the copy keeps the
    // README's own layout — including the trailing comma fmt would
    // otherwise drop when it joined the call onto one line. Two earlier
    // versions of this comparison normalised whitespace instead, and
    // both failed on differences that were fmt's, not the author's.
    let squash = |src: &str| -> String {
        src.lines()
            .map(|l| l.split("//").next().unwrap_or("").trim())
            .filter(|l| !l.is_empty() && !l.starts_with("use "))
            .collect::<Vec<_>>()
            .join("")
            .replace(' ', "")
    };

    assert_eq!(
        squash(&doc),
        squash(&copy),
        "README's example and the compiled copy have diverged.\n\
         README:\n{doc}\ncopy:\n{copy}"
    );
}

/// A boxed sink, the shape the `?Sized` bound exists for.
#[test]
fn a_boxed_sink_satisfies_the_bound() {
    let mut boxed: Box<dyn Events> = Box::new(MyUi::default());
    let config = Config::new(PathBuf::from("/nonexistent.img"), PathBuf::from("/dev/null"));
    let cancel = AtomicBool::new(false);
    // Fails at Phase 0 — the point is that it type-checks and runs.
    let err = imi_core::run_with(&config, &cancel, &mut *boxed).unwrap_err();
    assert_eq!(err.device_state(), DeviceState::Untouched, "{err:#}");
}
