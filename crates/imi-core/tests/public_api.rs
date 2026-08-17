//! Consumer-side validation of `imi-core`'s public API.
//!
//! This is an integration-test crate, so it sees exactly what a
//! downstream dependent sees: `pub(crate)` items are invisible here.
//! That is the point — the unit tests inside the library can reach
//! internals and therefore cannot prove the published surface is
//! actually usable.
//!
//! Everything here runs **without root** and touches no device: the
//! phases are only named, never driven. The root-gated companion that
//! actually drives them is `phase_pipeline.rs`.

#![expect(
    clippy::tests_outside_test_module,
    reason = "integration-test crate: it has no cfg(test) module by design"
)]

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

// `unused_crate_dependencies` is evaluated per compilation unit and an
// integration-test target links every one of the package's
// dependencies. This test names only `imi_core`, so acknowledge the
// rest — including `anyhow`, which it no longer needs to name now that
// the public API returns `imi_core::Error` instead. The list must
// mirror `crates/imi-core/Cargo.toml`.
use bzip2 as _;
use flate2 as _;
use libc as _;
use nix as _;
use xz2 as _;
use zstd as _;

/// A `Config` for a target that cannot exist, so every phase refuses
/// long before touching hardware.
fn nonexistent_config() -> imi_core::Config {
    let mut config = imi_core::Config::new(
        PathBuf::from("/nonexistent/imi-test-image.iso"),
        PathBuf::from("/nonexistent/imi-test-device"),
    );
    config.yes = true; // never block on a TTY prompt in CI
    config
}

/// Reads every public accessor on the types the phases hand back.
///
/// Referenced as a function pointer below; it exists to make that
/// readability a compile-time contract. The fields themselves are
/// private — `target.dev_size` is `E0616` from outside the crate — and
/// the types are `#[non_exhaustive]` on top of that, so a consumer can
/// neither build one nor reach into it. The getters are the whole of
/// the readable surface, which is exactly why it needs pinning: remove
/// one and nothing else here would fail.
fn reads_every_public_accessor(target: &imi_core::Target, outcome: imi_core::FlashOutcome) -> bool {
    !target.image_path().as_os_str().is_empty()
        && !target.device_path().as_os_str().is_empty()
        && !target.device_kname().is_empty()
        && !target.compression().is_compressed()
        && target.raw_image_size().is_none()
        && target.device_size() > 0
        && outcome.bytes_written() > 0
        && size_of_val(target.identity()) > 0
}

/// The documented construction path has to work from *outside* the
/// crate: `Config` is `#[non_exhaustive]`, which forbids struct-literal
/// construction downstream, so `Config::new` plus field assignment is
/// the only way a consumer can build one. Inside the defining crate the
/// attribute has no effect at all, so no unit test can prove this.
#[test]
fn config_is_constructible_and_mutable_downstream() {
    let mut config =
        imi_core::Config::new(PathBuf::from("/tmp/img.iso"), PathBuf::from("/dev/loop9"));

    assert_eq!(config.img, PathBuf::from("/tmp/img.iso"));
    assert_eq!(config.dev, PathBuf::from("/dev/loop9"));
    assert!(!config.yes);
    assert!(config.throttle.is_none());
    assert!(!config.skip_cooldown);
    assert!(!config.skip_verification);

    config.yes = true;
    config.throttle = Some(8 * 1024 * 1024);
    config.skip_cooldown = true;
    config.skip_verification = true;

    assert!(config.yes);
    assert_eq!(config.throttle, Some(8 * 1024 * 1024));
    assert!(config.skip_cooldown);
    assert!(config.skip_verification);
}

/// Every re-exported type must be nameable through the crate root, not
/// only through its defining module. A dropped `pub use` would compile
/// fine inside the library and break only here.
#[test]
fn re_exports_are_reachable_from_the_crate_root() {
    assert!(size_of::<imi_core::Config>() > 0);
    assert!(size_of::<imi_core::Target>() > 0);
    assert!(size_of::<imi_core::TargetDevts>() > 0);
    assert!(size_of::<imi_core::FlashGuard>() > 0);
    assert!(size_of::<imi_core::GuardPhase>() > 0);
    assert!(size_of::<imi_core::DeviceIdentity>() > 0);
    assert!(size_of::<imi_core::FlashOutcome>() > 0);
    assert!(size_of::<imi_core::Compression>() > 0);
}

/// Every `CandidateDevice` accessor a picker depends on, by signature.
///
/// A downstream picker — the `imi` binary, or a GUI — is built entirely
/// on these five reads plus the enumeration entry point below. Renaming
/// one, or narrowing a return type, breaks a consumer this crate cannot
/// see; this function makes it break here first.
fn reads_every_candidate_accessor(c: &imi_core::CandidateDevice) -> bool {
    !c.path().as_os_str().is_empty()
        && !c.kname().is_empty()
        && c.size_bytes() > 0
        && c.model().is_none_or(|m| !m.is_empty())
        && (c.removable() || !c.removable())
}

/// The enumeration entry point keeps its shape.
#[test]
fn candidate_enumeration_is_pinned() {
    let _: fn() -> imi_core::Result<Vec<imi_core::CandidateDevice>> = imi_core::candidate_devices;
    let _: fn(&imi_core::CandidateDevice) -> bool = reads_every_candidate_accessor;
}

/// `Target`'s fields are private and readable only through getters.
///
/// `#[non_exhaustive]` alone was not enough: it blocks constructing a
/// `Target` but not mutating one, so a consumer could take the value
/// `phase_0::run` returned and rewrite `dev_size` before Phase 4 checked
/// its writes against it. Private fields close that. What consumers *do*
/// need is to read what a phase returned, and that must not regress.
#[test]
fn returned_types_stay_readable_downstream() {
    let _: fn(&imi_core::Target, imi_core::FlashOutcome) -> bool = reads_every_public_accessor;
}

/// The per-phase API is the crate's second entry point, and its whole
/// value is that a caller can sequence the phases itself. That is only
/// true if each phase's output type is the next phase's input type.
/// Binding all eight as function pointers proves the chain type-checks
/// from outside the crate.
#[test]
fn phase_entry_points_chain_from_outside_the_crate() {
    /// Phases 4 and 5 take five and six parameters; naming the shapes
    /// keeps the pins readable and satisfies the complexity lint.
    type Phase4 =
        fn(&mut ArmedSession, Option<u64>, &AtomicBool, &mut ()) -> imi_core::Result<FlashOutcome>;

    type Phase5 =
        fn(&mut ArmedSession, &Config, FlashOutcome, &AtomicBool, &mut ()) -> imi_core::Result<()>;

    use imi_core::{ArmedSession, Config, FlashOutcome, Session, Target, TargetDevts};

    let _: fn(&Config, &mut ()) -> imi_core::Result<Target> = imi_core::phases::phase_0::run::<()>;
    let _: fn(&Target, &mut ()) -> imi_core::Result<TargetDevts> =
        imi_core::phases::phase_1::run::<()>;
    // Phase 2 takes the target by value and pairs it with the claim; from
    // here to Phase 6 the two travel as one value, so no later signature
    // has anywhere to put a mismatched target.
    let _: fn(Target, &TargetDevts, &mut ()) -> imi_core::Result<Session> =
        imi_core::phases::phase_2::run::<()>;
    let _: fn(Session, &AtomicBool, &mut ()) -> imi_core::Result<ArmedSession> =
        imi_core::phases::phase_3::run::<()>;
    let _: Phase4 = imi_core::phases::phase_4::run::<()>;
    let _: Phase5 = imi_core::phases::phase_5::run::<()>;
    // Phase 6 hands the target back out, for Phase 7.
    let _: fn(ArmedSession, &mut ()) -> Target = imi_core::phases::phase_6::run::<()>;
    let _: fn(&Target, &AtomicBool, &mut ()) -> imi_core::Result<()> =
        imi_core::phases::phase_7::run::<()>;

    let _: fn(&Config) -> imi_core::Result<()> = imi_core::run;
    let _: fn(&Config, &AtomicBool) -> imi_core::Result<()> = imi_core::run_with_cancel;
}

/// `Compression` is part of the surface because it is a `Target` field;
/// its query methods must be callable downstream.
#[test]
fn compression_queries_are_public() {
    assert!(!imi_core::Compression::Raw.is_compressed());
    assert!(!imi_core::Compression::Raw.label().is_empty());
}

/// Phase 0 must *return* an error for an unusable target, never panic
/// and never abort the process. This holds whether or not the test runs
/// as root: unprivileged it fails the root check, privileged it fails to
/// canonicalize the path. Either way the contract is the same, which is
/// what makes the assertion safe to run in CI.
#[test]
fn phase_0_refuses_a_bad_config_without_panicking() {
    let result = imi_core::phases::phase_0::run(&nonexistent_config(), &mut ());
    let err = result.expect_err("phase 0 must refuse this config");

    // Which refusal fires depends on the uid, and neither is the target:
    // Phase 0 checks for root, then canonicalises the *image* before the
    // device. This test is about not panicking and about classifying, so
    // both are acceptable — but it must not silently become a test of
    // something else again, which is what happened when it was named for
    // the target and asserted only `is_err()`.
    let text = format!("{err:#}");
    assert!(
        text.contains("canonicalize image path") || text.contains("must be run as root"),
        "expected the image or root refusal, got: {text}"
    );
    assert_eq!(err.kind(), imi_core::ErrorKind::Refused);
    assert_eq!(err.device_state(), imi_core::DeviceState::Untouched);
}

/// The same contract through the whole-pipeline entry point. Nothing is
/// mounted, opened, or written: the run dies in Phase 0.
#[test]
fn run_refuses_a_bad_config_without_panicking() {
    let result = imi_core::run(&nonexistent_config());
    let err = result.expect_err("run must refuse this config");
    assert_eq!(err.kind(), imi_core::ErrorKind::Refused);
    assert_eq!(err.device_state(), imi_core::DeviceState::Untouched);
}

/// An error from the pipeline has to explain the refusal — that string
/// is what an operator reads.
#[test]
fn pipeline_errors_explain_the_refusal() {
    let err = imi_core::run(&nonexistent_config()).expect_err("a nonexistent target must fail");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("root") || chain.contains("canonicalize"),
        "error chain should explain the refusal, got: {chain}"
    );
}

/// Every public type must implement `Debug`.
///
/// Rust API Guidelines C-DEBUG. Without it a consumer cannot log these,
/// cannot `dbg!` them, and cannot `#[derive(Debug)]` on any struct that
/// holds one — which quietly makes the whole crate awkward to embed.
/// Asserted by requiring the bound, so a dropped derive fails here.
#[test]
fn every_public_type_implements_debug() {
    fn assert_debug<T: std::fmt::Debug>() {}

    assert_debug::<imi_core::Config>();
    assert_debug::<imi_core::Target>();
    assert_debug::<imi_core::TargetDevts>();
    assert_debug::<imi_core::FlashGuard>();
    assert_debug::<imi_core::ArmedGuard>();
    assert_debug::<imi_core::GuardPhase>();
    assert_debug::<imi_core::DeviceIdentity>();
    assert_debug::<imi_core::Compression>();
    assert_debug::<imi_core::FlashOutcome>();

    // The five the test's name promised and the body omitted. These are
    // the ones a consumer is likeliest to format: `Error` in a log line,
    // and the three classification enums in whatever it prints when a
    // run fails.
    assert_debug::<imi_core::Error>();
    assert_debug::<imi_core::ErrorKind>();
    assert_debug::<imi_core::DeviceState>();
    assert_debug::<imi_core::Phase>();
    assert_debug::<imi_core::PhaseOutcome>();
}

/// `GuardPhase` must be obtainable, not merely exported.
///
/// It previously had no public producer or consumer: the type was named
/// in the crate root and nothing downstream could ever hold one. These
/// two accessors are what make it meaningful, and they let a caller
/// driving the phases by hand ask whether it is inside the destructive
/// window.
#[test]
fn guard_state_is_observable_downstream() {
    // Since the typestate split, the observable one is `ArmedGuard`.
    // `FlashGuard`'s equivalents went crate-internal because they became
    // constants: `arm` consumes the guard and `disarm` clears the phase,
    // so every `FlashGuard` a consumer can hold reports `Disarmed` and
    // would not warn. Whether the run is inside the destructive window is
    // now answered by which type you are holding.
    let _: fn(&imi_core::ArmedGuard) -> imi_core::GuardPhase = imi_core::ArmedGuard::current_phase;

    // The variants a caller may need to compare against.
    assert_ne!(imi_core::GuardPhase::Disarmed, imi_core::GuardPhase::Writing);
}

/// Cancellation is opt-in.
///
/// `run` takes a config and nothing else; a caller with nothing to
/// cancel from should not have to conjure an `AtomicBool`. The
/// cancel-aware form remains available and is what the `imi` binary
/// uses, since it owns the signal handler.
#[test]
fn cancellation_is_opt_in() {
    use imi_core::Config;

    let _: fn(&Config) -> imi_core::Result<()> = imi_core::run;
    let _: fn(&Config, &AtomicBool) -> imi_core::Result<()> = imi_core::run_with_cancel;
}

/// Every public type must stay `Send` and `Sync`.
///
/// Rust API Guidelines C-SEND-SYNC, pinned with the pattern the
/// guidelines themselves prescribe. These are auto traits: they are
/// granted by the compiler based on field types, so adding an `Rc`, a
/// `Cell`, or a raw pointer to any of these structs silently revokes
/// them. That breaks every downstream caller who moves a value across a
/// thread — a `FlashGuard` handed to a worker, a `Target` stored in a
/// GUI's shared state — and it breaks at *their* compile, not ours.
#[test]
fn public_types_stay_send_and_sync() {
    fn assert_send<T: Send>() {}
    fn assert_eq_impl<T: PartialEq>() {}
    fn assert_clone<T: Clone>() {}
    fn assert_copy<T: Copy>() {}
    fn assert_sync<T: Sync>() {}

    assert_send::<imi_core::Config>();

    // The error type every consumer handles, and the only one here whose
    // auto-traits are not obvious from its shape: `Error` boxes its
    // source as `dyn StdError + Send + Sync`, and drops both if that
    // bound is ever relaxed. A caller returning `imi_core::Result` from
    // a worker thread, or storing an error in shared state, depends on
    // it — and would find out at their call site, not here.
    assert_send::<imi_core::Error>();
    assert_sync::<imi_core::Error>();

    // The four plain enums are trivially both, but pinning them costs a
    // line each and makes the set complete rather than partial.
    assert_send::<imi_core::DeviceState>();
    assert_sync::<imi_core::DeviceState>();
    assert_send::<imi_core::ErrorKind>();
    assert_sync::<imi_core::ErrorKind>();
    assert_send::<imi_core::Phase>();
    assert_sync::<imi_core::Phase>();
    assert_send::<imi_core::PhaseOutcome>();
    assert_sync::<imi_core::PhaseOutcome>();

    // Equality, which the documented examples depend on rather than
    // merely benefit from. `Err(e) if e.kind() == ErrorKind::Cancelled`
    // in error.rs and the `match e.device_state()` in the crate doc both
    // stop compiling for every consumer if these derives are dropped,
    // and neither is covered by the Send/Sync pins above.
    assert_eq_impl::<imi_core::DeviceState>();
    assert_eq_impl::<imi_core::ErrorKind>();
    assert_eq_impl::<imi_core::Phase>();
    assert_eq_impl::<imi_core::PhaseOutcome>();
    assert_eq_impl::<imi_core::Compression>();
    assert_eq_impl::<imi_core::GuardPhase>();

    // DeviceIdentity is the one where equality *is* the interface: it
    // exposes no accessors, so snapshot-and-compare is the only thing a
    // consumer can do with it.
    assert_eq_impl::<imi_core::DeviceIdentity>();
    assert_clone::<imi_core::DeviceIdentity>();

    // Copy on the small enums, which a front end passes by value
    // repeatedly — Events hands `Phase` to four different callbacks.
    assert_copy::<imi_core::Phase>();
    assert_copy::<imi_core::PhaseOutcome>();
    assert_copy::<imi_core::DeviceState>();
    assert_copy::<imi_core::ErrorKind>();
    assert_copy::<imi_core::Compression>();
    assert_sync::<imi_core::Config>();
    assert_send::<imi_core::Target>();
    assert_sync::<imi_core::Target>();
    assert_send::<imi_core::TargetDevts>();
    assert_sync::<imi_core::TargetDevts>();
    assert_send::<imi_core::FlashGuard>();
    assert_sync::<imi_core::FlashGuard>();
    // The typestate split added a second public guard type. Pinned
    // alongside the first, because a future field that is not `Send`
    // would otherwise pass every gate and break a consumer moving the
    // guard between the phases that produce and consume it.
    assert_send::<imi_core::ArmedGuard>();
    assert_sync::<imi_core::ArmedGuard>();
    assert_send::<imi_core::GuardPhase>();
    assert_sync::<imi_core::GuardPhase>();
    assert_send::<imi_core::DeviceIdentity>();
    assert_sync::<imi_core::DeviceIdentity>();
    assert_send::<imi_core::Compression>();
    assert_sync::<imi_core::Compression>();
    assert_send::<imi_core::FlashOutcome>();
    assert_sync::<imi_core::FlashOutcome>();
}

/// `Compression` renders itself, so callers can format it directly.
#[test]
fn compression_displays_downstream() {
    assert_eq!(imi_core::Compression::Raw.to_string(), "raw");
    assert_eq!(format!("{}", imi_core::Compression::Gzip), "gzip");
}

/// The public API must not expose `anyhow`.
///
/// The pipeline still builds its phase-context chain with `anyhow`
/// internally — that is what `.context("Phase N: ...")` is for — but
/// none of it reaches a caller. This test would not compile if `Error`
/// grew a public `From<anyhow::Error>` or if a signature leaked the
/// type, because it names the error only through this crate's own
/// aliases.
#[test]
fn the_error_type_is_a_first_class_std_error() {
    fn assert_std_error<T: std::error::Error + Send + Sync + 'static>() {}
    assert_std_error::<imi_core::Error>();

    // `Result` is the crate's own alias, not a re-export of anyhow's.
    let _: fn(&imi_core::Config) -> imi_core::Result<()> = imi_core::run;

    // And it boxes into the standard trait object, which is how most
    // error-handling crates absorb a foreign error.
    let boxed: Box<dyn std::error::Error + Send + Sync> =
        Box::new(imi_core::run(&nonexistent_config()).unwrap_err());
    assert!(!boxed.to_string().is_empty());
}

/// A consumer must be able to answer "is the device safe to unplug?"
/// without reading the message.
///
/// The error chain must be walkable from outside the crate.
///
/// `Error` implements `std::error::Error`, and the reason that matters
/// is `source()`: a consumer can walk to the underlying cause without
/// this crate exporting the types it wraps. `Cancelled` and
/// `VerificationMismatch` are `pub(crate)` and stay that way — what a
/// caller gets is their `Display` text through `&dyn Error`, which is
/// the part that is stable.
///
/// Without this, integrating with `anyhow`, `eyre` or a logger that
/// renders causes silently produces one line where there were three.
#[test]
fn the_error_chain_is_walkable_downstream() {
    let err = imi_core::run(&nonexistent_config()).expect_err("must refuse");

    assert!(!err.to_string().is_empty(), "an error must say something");

    // Walk the chain as a logger would.
    let mut depth = 0;
    let mut cause: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&err);
    while let Some(c) = cause {
        assert!(!c.to_string().is_empty(), "every link must render");
        depth += 1;
        assert!(depth < 16, "chain is implausibly deep, or cyclic");
        cause = c.source();
    }

    let flat = format!("{err}");
    let full = format!("{err:#}");

    // Which error a caller can even obtain here depends on the uid, so
    // both branches assert rather than one of them skipping.
    //
    // Phase 0 checks for root before it canonicalises anything. As root
    // the run gets past that and fails on the missing image, wrapping an
    // io::Error — so the chain must be walkable, which is the property
    // this test exists for. As anyone else it stops at the root refusal,
    // which wraps nothing and correctly has no source.
    //
    // The first version of this test asserted `depth >= 1`
    // unconditionally. It passed in a root container and failed for
    // everyone else, which is the wrong way round for a test whose
    // subject is what a consumer sees.
    // SAFETY: `geteuid` reads a process property and cannot fail.
    let is_root = unsafe { libc::geteuid() } == 0;
    if is_root {
        assert!(depth >= 1, "a wrapped io::Error must be reachable through source(); got none");
        assert!(full.len() > flat.len(), "the alternate form must add the cause:\n{full}\n{flat}");
        assert!(!flat.contains("No such file"), "the plain form must not inline the cause: {flat}");
        assert!(full.contains("No such file"), "the alternate form must include it: {full}");
    } else {
        assert_eq!(depth, 0, "the root refusal wraps nothing, so it must have no source");
        assert_eq!(full, flat, "with no chain, the two forms must render the same");
        assert!(flat.contains("must be run as root"), "expected the root refusal, got: {flat}");
    }

    // True either way: the classification does not depend on the uid.
    assert_eq!(err.kind(), imi_core::ErrorKind::Refused);
    assert_eq!(err.device_state(), imi_core::DeviceState::Untouched);
}

/// This is the whole point of the classification. Before it existed the
/// only way to tell a refusal from a half-written disk was to search the
/// text for "Phase 3", which breaks the moment anyone rewords a
/// diagnostic — and the consequence of getting it wrong is an operator
/// walking off with an unbootable drive.
#[test]
fn failures_classify_without_reading_the_message() {
    use imi_core::{DeviceState, ErrorKind};

    let err = imi_core::run(&nonexistent_config()).unwrap_err();

    // Nothing was written, so this is a refusal and the device is safe.
    assert_eq!(err.kind(), ErrorKind::Refused);
    assert_eq!(err.device_state(), DeviceState::Untouched);

    // Both are plain values a UI can match on.
    let advice = match err.device_state() {
        DeviceState::Untouched => "safe to retry",
        DeviceState::Indeterminate => "DO NOT REMOVE — re-flash required",
        DeviceState::Written => "written; unmount before removing",
        _ => "unknown",
    };
    assert_eq!(advice, "safe to retry");

    // Only the safe classification is reachable from here, and that is
    // structural rather than an omission: `Indeterminate` is set from
    // Phase 3 onward, Phase 3 needs a `FlashGuard`, and the only
    // producer of one is Phase 2's `O_EXCL` open of a block device —
    // which needs root. A test that could assert the dangerous state
    // without root would mean the guard could be obtained without one.
    //
    // The other two states are asserted in `phase_pipeline.rs`, behind
    // `--ignored`: `Indeterminate` after an interrupted write, `Written`
    // after a cancelled Phase 7. What matters here is that the three are
    // distinguishable at all, which the `assert_ne!` below pins.
    assert_ne!(
        DeviceState::Untouched,
        DeviceState::Indeterminate,
        "the states a consumer branches on must not compare equal"
    );
    assert_ne!(DeviceState::Untouched, DeviceState::Written);
    assert_ne!(DeviceState::Indeterminate, DeviceState::Written);
}

/// The classification types must be usable in a caller's own match.
#[test]
fn classification_types_are_ergonomic() {
    fn assert_traits<T: std::fmt::Debug + Clone + Copy + PartialEq + Send + Sync>() {}
    assert_traits::<imi_core::ErrorKind>();
    assert_traits::<imi_core::DeviceState>();
    assert_ne!(imi_core::DeviceState::Untouched, imi_core::DeviceState::Indeterminate);
}

/// A boxed sink must still work.
///
/// The API is generic, so a concrete sink monomorphises and the silent
/// sink optimises away entirely. A GUI cannot use that — it holds its
/// sink in application state across frames, and a generic parameter
/// there would infect every type that touches the app. The `?Sized`
/// bound is what keeps both callers served: `E` may be `dyn Events`.
#[test]
fn a_boxed_sink_still_satisfies_the_api() {
    use imi_core::{DeviceState, Events};

    #[derive(Default)]
    struct Recorder {
        confirms: u32,
    }
    impl Events for Recorder {
        fn confirm(&mut self, _s: &imi_core::Summary<'_>) -> bool {
            self.confirms += 1;
            false
        }
    }

    // Erased behind a box, as a GUI would hold it. The `?Sized` bound
    // is what lets `E` be `dyn Events` itself, so this needs no
    // adapter — the trait object is passed straight through.
    let mut boxed: Box<dyn Events> = Box::new(Recorder::default());
    let err = imi_core::run_with(&nonexistent_config(), &AtomicBool::new(false), &mut *boxed)
        .expect_err("a nonexistent target must fail");
    assert_eq!(err.device_state(), DeviceState::Untouched);

    // And so does a concrete sink, monomorphised.
    let mut concrete = Recorder::default();
    imi_core::run_with(&nonexistent_config(), &AtomicBool::new(false), &mut concrete)
        .expect_err("a nonexistent target must fail");
}
