//! The library's only channel to a user interface.
//!
//! A flasher has things to say while it works — which phase it is in,
//! how far the write has got, that it just unmounted something — and one
//! question it must ask before doing anything destructive. Writing that
//! to stdout would make this crate unusable from anything but a
//! terminal: a GUI cannot intercept `println!`, cannot answer a prompt
//! read from `/dev/tty`, and cannot draw its own progress bar from
//! output it never sees.
//!
//! So the phases emit through [`Events`] and never touch a terminal. The
//! `imi` binary implements the trait with `indicatif` and a tty prompt;
//! a GUI implements it with whatever it uses; a script passes `()` and
//! hears nothing.
//!
//! Every method has a default, so an implementor writes only what it
//! cares about — except [`Events::confirm`], whose default **refuses**.
//! A caller that has not thought about confirmation should not be able
//! to destroy a disk by accident; see [`crate::Config::yes`] for the
//! explicit opt-out.

use std::path::Path;

use crate::common::image::Compression;

/// Which step of the pipeline is being reported.
///
/// A typed value rather than a message so an interface can label the
/// step in its own words, in its own language, without parsing prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Phase {
    /// Phase 0 — validating the image and device before anything else.
    Preflight,
    /// Phase 1 — enumerating partitions, mounts and stacked volumes.
    Topology,
    /// Phase 2 — taking the exclusive kernel claim on the device.
    Claim,
    /// Phase 3 — wiping partition signatures. **Destructive.**
    Wipe,
    /// Phase 4 — writing the image. **Destructive.**
    Flash,
    /// Phase 5a — waiting for the device's own cache to settle.
    Cooldown,
    /// Phase 5b — reading the device back and comparing it.
    Verify,
    /// Phase 6 — re-reading the partition table and releasing the claim.
    KernelSync,
    /// Phase 7 — keeping the host from remounting the device.
    Automount,
}

/// How a phase ended.
///
/// A front end drawing a progress bar needs the distinction: a completed
/// phase clears its bar, a failed one leaves it on screen showing how
/// far the write got before it stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PhaseOutcome {
    /// The phase did what it set out to do.
    Completed,
    /// The phase stopped early. The error is returned separately;
    /// consult [`crate::Error::device_state`] for what it means.
    Failed,
}

/// What is about to be destroyed, for [`Events::confirm`].
///
/// Everything an operator needs to recognise the device and notice they
/// picked the wrong one.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Summary<'a> {
    /// The block device that will be overwritten.
    pub device: &'a Path,
    /// Model string from sysfs, when the device reports one.
    pub model: Option<&'a str>,
    /// Capacity in bytes, from `BLKGETSIZE64`.
    pub device_size: u64,
    /// The image that will be written.
    pub image: &'a Path,
    /// Compression detected from the image's magic bytes.
    pub compression: Compression,
    /// Size of the image on disk, when it is raw. `None` for a
    /// compressed image, whose decompressed length is not known until
    /// the write finishes.
    pub raw_image_size: Option<u64>,
}

/// Where the pipeline reports to.
///
/// Implement the methods you need; the rest do nothing. `()` implements
/// this trait, so `&mut ()` is a silent sink.
///
/// # Stability
///
/// The trait is deliberately not sealed — implementing it is the whole
/// point, since a GUI or a remote front end is expected to. That makes
/// its shape a semver commitment worth stating: **every method here has
/// a default body, and any method added later will too.** A consumer's
/// existing `impl` therefore keeps compiling across releases.
///
/// The corollary binds this crate rather than the caller: a method
/// added without a default would break every implementor at once, so
/// there is no such thing as a minor release that adds a required
/// method. If a future phase genuinely needs something a default cannot
/// supply, it takes a new trait or a major version.
///
/// # Examples
///
/// ```
/// use imi_core::{Events, Phase, Summary};
///
/// #[derive(Default)]
/// struct Ui {
///     written: u64,
/// }
/// impl Events for Ui {
///     fn phase_started(&mut self, phase: Phase) {
///         if phase == Phase::Flash {
///             println!("writing…");
///         }
///     }
///     fn progress(&mut self, _phase: Phase, done: u64, _total: Option<u64>) {
///         self.written = done;
///     }
///     fn confirm(&mut self, summary: &Summary<'_>) -> bool {
///         // Ask a human. The default refuses, which is why an
///         // unattended caller sets `Config::yes` instead.
///         println!("erase {}?", summary.device.display());
///         false
///     }
/// }
/// let mut ui = Ui::default();
/// ui.progress(Phase::Flash, 4096, None);
/// assert_eq!(ui.written, 4096);
/// ```
pub trait Events {
    /// A phase has started doing its work.
    fn phase_started(&mut self, _phase: Phase) {}

    /// A phase was skipped because the caller asked for it.
    fn phase_skipped(&mut self, _phase: Phase) {}

    /// A phase stopped, either way.
    ///
    /// Always paired with a [`Events::phase_started`], including when
    /// the phase failed — a front end holding a progress bar needs to be
    /// told to put it down whichever way the phase ended.
    fn phase_finished(&mut self, _phase: Phase, _outcome: PhaseOutcome) {}

    /// How far `phase` has got, in that phase's own unit.
    ///
    /// The unit is **not** always bytes, which is why the phase is
    /// passed rather than left for the caller to track: [`Phase::Flash`]
    /// and [`Phase::Verify`] report bytes, [`Phase::Cooldown`] reports
    /// seconds. Rendering a countdown as a byte count is the mistake
    /// this parameter exists to prevent.
    ///
    /// `total` is `None` when the end is not known — a compressed image
    /// has no decompressed length until the write finishes.
    ///
    /// Called roughly once per 4 MiB during a write, so an
    /// implementation should be cheap.
    fn progress(&mut self, _phase: Phase, _done: u64, _total: Option<u64>) {}

    /// Something was done on the operator's behalf — a filesystem
    /// unmounted, a swap area disabled.
    fn action(&mut self, _message: &str) {}

    /// Something went wrong that did not stop the run.
    fn warning(&mut self, _message: &str) {}

    /// Approve destroying the device described by `summary`.
    ///
    /// **Returning `false` aborts the flash**, and that is the default:
    /// a sink that has not considered the question must not be able to
    /// authorise wiping a disk. Set [`crate::Config::yes`] to skip the
    /// question entirely when the caller has already confirmed.
    fn confirm(&mut self, _summary: &Summary<'_>) -> bool {
        false
    }

    /// The device was written and verified successfully.
    ///
    /// Emitted by [`crate::run_with`] after Phase 7 returns — it is not
    /// a phase, so a caller sequencing [`crate::phases`] by hand must
    /// make this call itself. On this tool that omission is not
    /// cosmetic: this is the only positive signal that the device is
    /// safe to unplug, and its absence looks exactly like a run that
    /// stopped somewhere without saying so.
    fn finished(&mut self, _device: &Path) {}
}

/// The silent sink.
///
/// `run(&config)` uses this, and so does any caller passing `&mut ()`.
/// Note that it declines [`Events::confirm`] by inheriting the default,
/// so an unattended run needs [`crate::Config::yes`].
impl Events for () {}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Events, Phase, PhaseOutcome, Summary};
    use crate::common::image::Compression;

    /// The silent sink must refuse to confirm.
    ///
    /// This is the safety default: a caller who passes `()` has not
    /// thought about confirmation, and the answer to "may I destroy this
    /// disk" when nobody is listening is no.
    #[test]
    fn the_silent_sink_refuses_to_confirm() {
        let summary = Summary {
            device: Path::new("/dev/sdz"),
            model: Some("Test"),
            device_size: 1024,
            image: Path::new("/tmp/a.iso"),
            compression: Compression::Raw,
            raw_image_size: Some(512),
        };
        assert!(!().confirm(&summary), "the default must refuse");
    }

    /// Every other method must be a no-op that an empty impl inherits.
    #[test]
    fn the_silent_sink_accepts_every_other_event() {
        // An implementor that overrides nothing, over state the defaults
        // must leave alone. Calling the methods on `()` would only prove
        // they do not panic; this proves they do nothing, which is the
        // contract a partial implementor relies on.
        #[derive(Default, PartialEq, Debug)]
        struct Untouched {
            calls: u32,
        }
        impl Events for Untouched {}

        let mut sink = Untouched::default();
        sink.phase_started(Phase::Flash);
        sink.phase_skipped(Phase::Verify);
        sink.phase_finished(Phase::Flash, PhaseOutcome::Completed);
        sink.phase_finished(Phase::Verify, PhaseOutcome::Failed);
        sink.progress(Phase::Flash, 1024, Some(4096));
        sink.progress(Phase::Flash, 1024, None);
        sink.action("unmounting /dev/sdz1");
        sink.warning("BLKRRPART failed");
        sink.finished(Path::new("/dev/sdz"));

        assert_eq!(sink, Untouched { calls: 0 }, "every default must be a no-op");
    }

    /// An implementor overriding one method inherits the rest, including
    /// the refusing `confirm`.
    #[test]
    fn partial_implementations_inherit_the_safe_default() {
        struct OnlyProgress {
            last: u64,
        }
        impl Events for OnlyProgress {
            fn progress(&mut self, _phase: Phase, done: u64, _total: Option<u64>) {
                self.last = done;
            }
        }

        let mut sink = OnlyProgress { last: 0 };
        sink.progress(Phase::Flash, 4096, None);
        assert_eq!(sink.last, 4096);

        let summary = Summary {
            device: Path::new("/dev/sdz"),
            model: None,
            device_size: 0,
            image: Path::new("/tmp/a.iso"),
            compression: Compression::Gzip,
            raw_image_size: None,
        };
        assert!(!sink.confirm(&summary), "an unconsidered confirm must still refuse");
    }
}
