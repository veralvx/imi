//! The crate's error type, and the machinery the pipeline builds it with.
//!
//! Every fallible operation in this crate returns [`Error`]. It carries
//! the diagnostic an operator reads, the chain of phase context above
//! it, and two classifications a program can act on: [`Error::kind`] and
//! [`Error::device_state`].
//!
//! # Why not `anyhow`
//!
//! The pipeline used `anyhow` internally and wrapped it here. That was
//! the wrong shape for a library, on its author's own advice: *"Use
//! thiserror if you are a library that wants to design your own
//! dedicated error type(s) so that on failures the caller gets exactly
//! the information that you choose."* It also put `anyhow` in every
//! downstream build, and decided backtrace-capture policy on a
//! consumer's behalf.
//!
//! The tell was structural rather than stylistic: [`Cancelled`] and
//! [`VerificationMismatch`] existed as types, were erased into a
//! trait-object chain, and then recovered by downcasting. Three steps to
//! arrive where the first one started.
//!
//! # Why not `thiserror` either
//!
//! `thiserror` earns its keep deriving `Display` for a large enum. This
//! is one struct whose `Display` is custom — it reconstructs the phase
//! chain, and honours `{:#}` for the whole thing against `{}` for the
//! outermost message. The derive would save nothing and add a
//! proc-macro dependency, which is what removing `anyhow` was meant to
//! avoid.
//!
//! # `Error` is returned, never constructed by a caller
//!
//! Every constructor is `pub(crate)`, and there are deliberately no
//! `From<io::Error>` / `From<nix::Error>` impls. Those existed to serve
//! a single bare `?`, and were also the only way a consumer could build
//! one of these. Removing them makes the type return-only, and forces
//! every foreign error inside the crate through `Context` — so a cause
//! is always preserved as a source rather than flattened into text,
//! which is what the two paths used to disagree about.
//!
//! # Building errors (crate-internal)
//!
//! `bail!` and `err!` construct; the `Context` trait adds a layer. All
//! three are `pub(crate)` — a consumer receives [`Error`] but does not
//! build one — which is also why the sketch below is `ignore`d rather
//! than compiled: a doctest runs as an external crate and cannot reach
//! them. The shape is covered by this module's unit tests instead.
//!
//! The signatures deliberately mirror the ones they replaced, so a phase
//! reads the same as it did before:
//!
//! ```ignore
//! std::fs::metadata(path).context("stat target device")?;
//! bail!("device is too small ({size} bytes)");
//! ```

use std::error::Error as StdError;
use std::fmt;

/// What went wrong, coarsely enough to be stable.
///
/// Deliberately small. The crate produces well over a hundred distinct
/// diagnostics, and enumerating them would freeze prose into the API;
/// these four are dictated by the pipeline's shape rather than by any
/// message's wording, so they survive rewording. `#[non_exhaustive]`
/// leaves room to split a case later.
///
/// This answers "how should I report it". For "is the hardware safe",
/// see [`DeviceState`] — the two are independent, because a cancellation
/// can happen either before or during the write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// A safety check refused the request. Nothing was written.
    Refused,
    /// The caller's cancellation flag was observed.
    Cancelled,
    /// The write completed, but reading it back found differing bytes.
    /// The device may be faulty, failing, or counterfeit.
    VerificationFailed,
    /// Anything else — an I/O error, a missing privilege, a device that
    /// disappeared mid-flash.
    Failed,
}

/// What the failure means for the hardware.
///
/// This is the question a user interface must answer before it can say
/// anything useful: may the operator unplug the device, or is it holding
/// a half-written image? Reading it from the error message is not an
/// option — messages get reworded, and this decides whether someone
/// walks away with an unbootable disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeviceState {
    /// Nothing was written. The device still holds its original
    /// contents and is safe to remove or retry against.
    Untouched,
    /// Writing began and did not finish successfully. The contents are
    /// indeterminate: partly the new image, partly whatever was there,
    /// possibly with its partition signatures wiped. It must be
    /// re-flashed before it can be trusted or booted.
    Indeterminate,
    /// The image was written and verified. The failure came afterwards,
    /// while settling the kernel's view of the device, so the data is
    /// intact — but the host may have remounted it.
    Written,
}

/// Marker for a run stopped by the caller's cancellation flag.
///
/// A type rather than a string so [`Error::kind`] can recognise it.
/// `Display` renders exactly what the message used to say, so the
/// operator-visible output is unchanged.
#[derive(Debug)]
pub(crate) struct Cancelled {
    /// What was interrupted, when naming it helps: `"cooldown"`.
    pub(crate) during: Option<&'static str>,
}

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.during {
            Some(what) => write!(f, "cancelled by user during {what}"),
            None => f.write_str("cancelled by user"),
        }
    }
}

impl StdError for Cancelled {}

/// Marker for a read-back that found differing bytes.
#[derive(Debug)]
pub(crate) struct VerificationMismatch {
    /// Absolute byte offset of the first difference.
    pub(crate) offset: u64,
    /// The image the device was compared against.
    pub(crate) image: std::path::PathBuf,
}

impl fmt::Display for VerificationMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "verification mismatch at byte offset {}. The device may be faulty, failing, or \
             counterfeit. Image: {}",
            self.offset,
            self.image.display()
        )
    }
}

impl StdError for VerificationMismatch {}

/// An error from any [`crate::run`], [`crate::run_with_cancel`],
/// [`crate::run_with`] or [`crate::phases`] operation.
///
/// `Display` renders the outermost message; the alternate form `{:#}`
/// renders the whole chain joined by `: ` — `Phase 4: flash write loop:
/// cancelled by user`. [`StdError::source`] walks the same chain
/// programmatically.
///
/// Two classifications sit beside the prose: [`Error::kind`] for how to
/// report the failure and [`Error::device_state`] for what it means for
/// the hardware. Both are `#[non_exhaustive]`, and message text is free
/// to change without reclassifying anything — the kinds come from typed
/// markers, not from words.
///
/// # Examples
///
/// ```no_run
/// # use std::path::PathBuf;
/// # let config = imi_core::Config::new(PathBuf::from("a.iso"), PathBuf::from("/dev/sdc"));
/// if let Err(e) = imi_core::run(&config) {
///     // `{}` is the outermost message; `{:#}` is the whole chain.
///     eprintln!("{e}");    // Phase 4: flash write loop
///     eprintln!("{e:#}");  // Phase 4: flash write loop: cancelled by user
///
///     // It implements `std::error::Error`, so it walks like one and
///     // drops into a caller's own error type as a `#[source]`.
///     let mut cause: Option<&(dyn std::error::Error + 'static)> =
///         std::error::Error::source(&e);
///     while let Some(c) = cause {
///         eprintln!("  caused by: {c}");
///         cause = c.source();
///     }
/// }
/// ```
pub struct Error {
    /// The message for this layer. Always present: every constructor
    /// supplies one, and [`Error::from_source`] takes the cause's own
    /// text rather than leaving this empty and rendering the cause
    /// twice.
    detail: Box<str>,
    /// The layer beneath, if any.
    source: Option<Box<dyn StdError + Send + Sync + 'static>>,
    /// How to report it; see [`Error::kind`].
    kind: ErrorKind,
    /// What it means for the hardware; see [`Error::device_state`].
    device: DeviceState,
}

impl Error {
    /// An error carrying only a message.
    pub(crate) fn msg(detail: impl Into<Box<str>>) -> Self {
        Self {
            detail: detail.into(),
            source: None,
            kind: ErrorKind::Failed,
            device: DeviceState::Indeterminate,
        }
    }

    /// An error wrapping a cause, taking its `Display` as the message.
    ///
    /// Used by [`bail!`] and [`err!`] when handed a value rather than a
    /// format string — the sentinel markers arrive this way, which is
    /// how their kind survives.
    #[expect(
        trivial_casts,
        reason = "the unsize coercion to `dyn StdError` is what enables `is::<T>()`"
    )]
    pub(crate) fn from_source(source: impl StdError + Send + Sync + 'static) -> Self {
        let kind = kind_of(&source as &dyn StdError);
        // The cause's text becomes this layer's message, and the cause
        // is not also kept as a source. Keeping both renders it twice
        // under `{:#}` — `bail!(Cancelled)` wrapped in a phase context
        // printed "cancelled by user: cancelled by user", once as this
        // layer and again as the layer beneath. The classification is
        // already captured in `kind`, so nothing downstream needs the
        // concrete type to remain reachable.
        Self {
            detail: source.to_string().into_boxed_str(),
            source: None,
            kind,
            device: DeviceState::Indeterminate,
        }
    }

    /// One layer: a message directly above a cause.
    ///
    /// Inherits the cause's device state when the cause is itself an
    /// [`Error`], so a boundary that has already recorded one is not
    /// overwritten by a later wrap.
    ///
    /// Deliberately not `from_source(..).context(..)`. That would build
    /// two layers, the inner one carrying no message of its own, and
    /// `{:#}` would then render the cause twice — once as the inner
    /// layer's own text and again as its source.
    #[expect(
        trivial_casts,
        reason = "the unsize coercion to `dyn StdError` is what enables `is::<T>()`"
    )]
    pub(crate) fn layer(
        detail: impl Into<Box<str>>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        let as_dyn = &source as &dyn StdError;
        let kind = kind_of(as_dyn);
        let device =
            as_dyn.downcast_ref::<Error>().map_or(DeviceState::Indeterminate, |inner| inner.device);
        Self { detail: detail.into(), source: Some(Box::new(source)), kind, device }
    }

    /// Add a layer of context above this error.
    ///
    /// The new layer inherits the classification: wrapping
    /// `cancelled by user` in `Phase 4: flash write loop` must not turn
    /// a cancellation into a generic failure.
    pub(crate) fn context(self, detail: impl Into<Box<str>>) -> Self {
        Self {
            detail: detail.into(),
            kind: self.kind,
            device: self.device,
            source: Some(Box::new(self)),
        }
    }

    /// Record what this failure means for the device.
    ///
    /// Called once, at each phase boundary, which is the only place that
    /// knows whether the device has been written to yet. Also settles
    /// the kind: an unrecognised failure before the first write is a
    /// refusal, one after it is a plain failure.
    pub(crate) fn at(mut self, device: DeviceState) -> Self {
        self.device = device;
        if self.kind == ErrorKind::Failed && device == DeviceState::Untouched {
            self.kind = ErrorKind::Refused;
        }
        self
    }

    /// How to report this failure.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::path::PathBuf;
    /// use imi_core::ErrorKind;
    ///
    /// # let config = imi_core::Config::new(PathBuf::from("a.iso"), PathBuf::from("/dev/sdc"));
    /// match imi_core::run(&config) {
    ///     Ok(()) => println!("done"),
    ///     // A user-initiated stop is not an error to show in red.
    ///     Err(e) if e.kind() == ErrorKind::Cancelled => println!("cancelled"),
    ///     Err(e) => eprintln!("error: {e:#}"),
    /// }
    /// ```
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// What the failure means for the device.
    ///
    /// Check this before telling an operator anything. A
    /// [`DeviceState::Indeterminate`] device is holding a partial image
    /// and must be re-flashed; saying "flash failed, try again" without
    /// saying that is how someone ends up booting a corrupted disk.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::path::PathBuf;
    /// use imi_core::DeviceState;
    ///
    /// # let config = imi_core::Config::new(PathBuf::from("a.iso"), PathBuf::from("/dev/sdc"));
    /// if let Err(e) = imi_core::run(&config) {
    ///     let advice = match e.device_state() {
    ///         DeviceState::Untouched => "safe to retry or unplug",
    ///         DeviceState::Indeterminate => "DO NOT REMOVE — re-flash required",
    ///         DeviceState::Written => "image is intact; unmount before removing",
    ///         _ => "unknown state",
    ///     };
    ///     eprintln!("{e:#} — {advice}");
    /// }
    /// ```
    #[must_use]
    pub fn device_state(&self) -> DeviceState {
        self.device
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // This layer's own message, then under `{:#}` the chain beneath
        // it joined by `: `.
        f.write_str(&self.detail)?;
        if f.alternate() {
            let mut cur = self.source();
            while let Some(e) = cur {
                write!(f, ": {e}")?;
                cur = e.source();
            }
        }
        Ok(())
    }
}

impl fmt::Debug for Error {
    /// Renders the whole chain, which is what `.unwrap()` prints and
    /// what `{:?}` puts in a caller's logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:#}")
    }
}

impl StdError for Error {
    #[expect(
        trivial_casts,
        reason = "widening `dyn StdError + Send + Sync` to the plain `dyn StdError` the \
                  trait requires"
    )]
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source.as_deref().map(|b| b as &(dyn StdError + 'static))
    }
}

/// Classify a cause by its concrete type.
///
/// An `Error` beneath a new layer keeps its own classification: wrapping
/// `cancelled by user` in `Phase 4: flash write loop` must not turn a
/// cancellation into a generic failure. Without this the trait impl of
/// [`Context`] silently downgrades every kind it wraps.
fn kind_of(e: &(dyn StdError + 'static)) -> ErrorKind {
    if let Some(inner) = e.downcast_ref::<Error>() {
        inner.kind
    } else if e.is::<Cancelled>() {
        ErrorKind::Cancelled
    } else if e.is::<VerificationMismatch>() {
        ErrorKind::VerificationFailed
    } else {
        ErrorKind::Failed
    }
}

/// Add context to a `Result`, mirroring the trait it replaced.
pub(crate) trait Context<T> {
    /// Wrap an error with a message.
    ///
    /// # Errors
    ///
    /// Returns the original error with one more layer above it.
    fn context(self, detail: impl Into<Box<str>>) -> Result<T, Error>;

    /// Wrap an error with a lazily-built message.
    ///
    /// # Errors
    ///
    /// Returns the original error with one more layer above it.
    fn with_context<S: Into<Box<str>>, F: FnOnce() -> S>(self, f: F) -> Result<T, Error>;
}

impl<T, E: StdError + Send + Sync + 'static> Context<T> for Result<T, E> {
    fn context(self, detail: impl Into<Box<str>>) -> Result<T, Error> {
        self.map_err(|e| Error::layer(detail, e))
    }

    fn with_context<S: Into<Box<str>>, F: FnOnce() -> S>(self, f: F) -> Result<T, Error> {
        self.map_err(|e| Error::layer(f(), e))
    }
}

impl<T> Context<T> for Option<T> {
    fn context(self, detail: impl Into<Box<str>>) -> Result<T, Error> {
        self.ok_or_else(|| Error::msg(detail))
    }

    fn with_context<S: Into<Box<str>>, F: FnOnce() -> S>(self, f: F) -> Result<T, Error> {
        self.ok_or_else(|| Error::msg(f()))
    }
}

/// Return early with an error.
///
/// Accepts a format string, or a value implementing `Error` — the
/// sentinel markers arrive by the second form.
macro_rules! bail {
    ($fmt:literal $($rest:tt)*) => {
        return ::core::result::Result::Err($crate::error::Error::msg(format!($fmt $($rest)*)))
    };
    ($e:expr $(,)?) => {
        return ::core::result::Result::Err($crate::error::Error::from_source($e))
    };
}

/// Construct an error without returning it.
macro_rules! err {
    ($fmt:literal $($rest:tt)*) => {
        $crate::error::Error::msg(format!($fmt $($rest)*))
    };
    ($e:expr $(,)?) => {
        $crate::error::Error::from_source($e)
    };
}

pub(crate) use {bail, err};

#[cfg(test)]
mod tests {
    use std::error::Error as StdError;
    use std::path::PathBuf;

    use super::{Cancelled, Context as _, DeviceState, Error, ErrorKind, VerificationMismatch};

    /// `{}` shows the outermost message; `{:#}` shows the whole chain.
    ///
    /// The alternate form is what an operator reads — the phase context
    /// is the difference between "cancelled by user" and knowing which
    /// phase was interrupted.
    #[test]
    fn display_forwards_the_alternate_flag() {
        let err = Error::msg("cancelled by user").context("Phase 4: flash write loop");
        assert_eq!(format!("{err}"), "Phase 4: flash write loop");
        assert_eq!(format!("{err:#}"), "Phase 4: flash write loop: cancelled by user");
    }

    /// `Debug` renders the chain too: it is what `.unwrap()` prints when
    /// a consumer's flash fails, and what `{:?}` puts in their logs.
    #[test]
    fn debug_renders_the_underlying_diagnostic() {
        let err = Error::msg("inner cause").context("Phase 4: flash write loop");
        let debug = format!("{err:?}");
        assert!(debug.contains("Phase 4: flash write loop"), "{debug}");
        assert!(debug.contains("inner cause"), "{debug}");
    }

    /// `source()` walks the same chain programmatically, which is what
    /// lets a caller embed this in their own error enum.
    #[test]
    fn source_walks_the_chain() {
        let err = Error::msg("innermost").context("middle").context("outermost");
        assert_eq!(err.to_string(), "outermost");

        let mut seen = Vec::new();
        let mut cur: Option<&(dyn StdError + 'static)> = err.source();
        while let Some(e) = cur {
            seen.push(e.to_string());
            cur = e.source();
        }
        assert_eq!(seen, vec!["middle".to_owned(), "innermost".to_owned()]);
    }

    /// An error with no cause has no source.
    #[test]
    fn a_standalone_error_has_no_source() {
        assert!(Error::msg("standalone").source().is_none());
    }

    /// The two axes are independent, and must stay so. A cancellation
    /// before any write leaves the device untouched; the same
    /// cancellation during the write does not. Collapsing them into one
    /// enum would make `Cancelled` unable to answer the only question
    /// that matters to an operator.
    #[test]
    fn kind_and_device_state_are_independent() {
        let early = Error::from_source(Cancelled { during: None }).at(DeviceState::Untouched);
        assert_eq!(early.kind(), ErrorKind::Cancelled);
        assert_eq!(early.device_state(), DeviceState::Untouched);

        let late = Error::from_source(Cancelled { during: None }).at(DeviceState::Indeterminate);
        assert_eq!(late.kind(), ErrorKind::Cancelled, "same kind");
        assert_eq!(late.device_state(), DeviceState::Indeterminate, "different consequence");
    }

    /// A sentinel must survive being wrapped in context — every real
    /// cancellation is wrapped in at least one `Phase N: ...` layer
    /// before it reaches the boundary.
    #[test]
    fn sentinels_survive_context_layers() {
        let deep = Error::from_source(Cancelled { during: Some("cooldown") })
            .context("Phase 5a: cooldown")
            .context("something outer")
            .at(DeviceState::Indeterminate);
        assert_eq!(deep.kind(), ErrorKind::Cancelled);
        assert!(format!("{deep:#}").contains("cancelled by user during cooldown"));

        let mismatch = Error::from_source(VerificationMismatch {
            offset: 4101,
            image: PathBuf::from("/tmp/x.img"),
        })
        .context("Phase 5b: verification")
        .at(DeviceState::Indeterminate);
        assert_eq!(mismatch.kind(), ErrorKind::VerificationFailed);
    }

    /// Only `Untouched` promotes an unclassified failure to `Refused`.
    ///
    /// A `Written` failure is not a refusal — the image is on the device
    /// and the run got as far as settling the kernel's view of it. And
    /// `at` must never downgrade a kind it did not assign: a cancelled
    /// run stays cancelled whatever the device state turns out to be.
    #[test]
    fn at_promotes_only_the_untouched_case() {
        assert_eq!(Error::msg("x").at(DeviceState::Written).kind(), ErrorKind::Failed);
        assert_eq!(Error::msg("x").at(DeviceState::Indeterminate).kind(), ErrorKind::Failed);
        assert_eq!(Error::msg("x").at(DeviceState::Untouched).kind(), ErrorKind::Refused);

        // A classified failure keeps its kind through any device state.
        for state in [DeviceState::Untouched, DeviceState::Indeterminate, DeviceState::Written] {
            let e = Error::from_source(Cancelled { during: None }).at(state);
            assert_eq!(e.kind(), ErrorKind::Cancelled, "{state:?} must not reclassify");
            assert_eq!(e.device_state(), state);
        }
    }

    /// A marker wrapped in phase context must render once, not twice.
    ///
    /// `bail!(Cancelled { .. })` inside a phase produces exactly this
    /// shape, and it reached an operator as
    /// `Phase 4: flash write loop: cancelled by user: cancelled by user`
    /// — the marker rendered as its own layer and again as that layer's
    /// source.
    #[test]
    fn a_marker_under_context_renders_once() {
        let err = Error::from_source(Cancelled { during: None })
            .context("Phase 4: flash write loop")
            .at(DeviceState::Indeterminate);

        let full = format!("{err:#}");
        assert_eq!(full, "Phase 4: flash write loop: cancelled by user", "{full}");
        assert_eq!(full.matches("cancelled by user").count(), 1, "{full}");

        // Two layers of context still read once each.
        let deeper = Error::from_source(Cancelled { during: Some("cooldown") })
            .context("Phase 5a: cooldown")
            .context("outer");
        let text = format!("{deeper:#}");
        assert_eq!(text.matches("cancelled by user").count(), 1, "{text}");
        assert_eq!(text, "outer: Phase 5a: cooldown: cancelled by user during cooldown", "{text}");
    }

    /// Unrecognised failures fall back to the phase's own position.
    #[test]
    fn unclassified_failures_fall_back_to_position() {
        let before = Error::msg("device is not a block device").at(DeviceState::Untouched);
        assert_eq!(before.kind(), ErrorKind::Refused);

        let during = Error::msg("write failed").at(DeviceState::Indeterminate);
        assert_eq!(during.kind(), ErrorKind::Failed);
    }

    /// The sentinels must render exactly the text they replaced, so the
    /// operator-visible output is unchanged by their existence.
    #[test]
    fn sentinels_render_the_original_wording() {
        assert_eq!(Cancelled { during: None }.to_string(), "cancelled by user");
        assert_eq!(
            Cancelled { during: Some("cooldown") }.to_string(),
            "cancelled by user during cooldown"
        );
        let m = VerificationMismatch { offset: 4101, image: PathBuf::from("/tmp/x.img") };
        let text = m.to_string();
        assert!(text.starts_with("verification mismatch at byte offset 4101."), "{text}");
        assert!(text.contains("faulty, failing, or counterfeit"), "{text}");
        assert!(text.ends_with("Image: /tmp/x.img"), "{text}");
    }

    /// `Context` must work on both `Result` and `Option`, and must not
    /// fire on the success path.
    #[test]
    fn context_wraps_results_and_options() {
        let io: Result<(), std::io::Error> =
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"));
        let err = io.context("stat target device").unwrap_err();
        assert_eq!(format!("{err:#}"), "stat target device: no such file");

        let none: Option<u32> = None;
        let none_err = none.context("allocating the buffer").unwrap_err();
        assert_eq!(format!("{none_err:#}"), "allocating the buffer");

        let ok: Result<u32, std::io::Error> = Ok(7);
        assert_eq!(ok.context("unused").unwrap(), 7);
        assert_eq!(Some(7).context("unused").unwrap(), 7);
    }
    /// Every shape the crate can produce must render correctly.
    ///
    /// A cancellation once reached an operator as
    /// `Phase 4: flash write loop: cancelled by user: cancelled by user`,
    /// and the shape that caused it was covered by no test — the
    /// duplicate lived in `from_source`, while the fix at the time went
    /// into the `Context` trait. Enumerating the shapes closes that:
    /// a message, a marker, a foreign error, each bare and under one or
    /// two layers of context.
    #[test]
    fn every_error_shape_renders_correctly() {
        use std::io;
        let io_err = || io::Error::new(io::ErrorKind::PermissionDenied, "Permission denied");

        // (label, error, `{}`, `{:#}`)
        let cases: Vec<(&str, Error, &str, &str)> = vec![
            ("plain", Error::msg("not a block device"), "not a block device", "not a block device"),
            (
                "msg + ctx",
                Error::msg("inner").context("Phase 1: topology"),
                "Phase 1: topology",
                "Phase 1: topology: inner",
            ),
            (
                "msg + two ctx",
                Error::msg("inner").context("mid").context("outer"),
                "outer",
                "outer: mid: inner",
            ),
            (
                "marker",
                Error::from_source(Cancelled { during: None }),
                "cancelled by user",
                "cancelled by user",
            ),
            (
                "marker + ctx",
                Error::from_source(Cancelled { during: None }).context("Phase 4: flash write loop"),
                "Phase 4: flash write loop",
                "Phase 4: flash write loop: cancelled by user",
            ),
            (
                "foreign",
                Error::layer("open /dev/sdc", io_err()),
                "open /dev/sdc",
                "open /dev/sdc: Permission denied",
            ),
            (
                "foreign + ctx",
                Error::layer("open /dev/sdc", io_err()).context("Phase 2: claim"),
                "Phase 2: claim",
                "Phase 2: claim: open /dev/sdc: Permission denied",
            ),
        ];

        for (label, err, plain, full) in cases {
            assert_eq!(format!("{err}"), plain, "{label}: `{{}}`");
            assert_eq!(format!("{err:#}"), full, "{label}: `{{:#}}`");
            // No fragment may appear twice: that is the failure mode.
            for part in full.split(": ") {
                assert_eq!(full.matches(part).count(), 1, "{label}: `{part}` repeats in `{full}`");
            }
        }
    }
}
