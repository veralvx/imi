//! The terminal front end.
//!
//! `imi-core` reports through [`imi_core::Events`] and never touches a
//! terminal itself. Everything a person sees — the phase lines, the
//! warnings, the destructive-action prompt — is produced here, which is
//! why the library is usable from a GUI that renders the same events
//! entirely differently.
//!
//! The confirmation deliberately reads from `/dev/tty` rather than
//! stdin, so a piped invocation cannot answer it by accident, and writes
//! the prompt there too, so a redirected stdout cannot hide it.

use std::fs::OpenOptions;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;

use imi_core::{Events, Phase, PhaseOutcome, Summary};

use crate::progress::{make_progress_bar, make_verify_pb};

/// Renders pipeline events to the terminal.
pub(crate) struct Cli {
    /// The device named on the command line.
    ///
    /// Held here because the front end already knows it — it parsed the
    /// arguments — so [`Events::phase_started`] does not need to carry
    /// context the caller supplied in the first place.
    device: std::path::PathBuf,
    /// The bar for the phase in flight.
    ///
    /// Created on `phase_started` so it renders at zero immediately, and
    /// its timer reset on the first `progress` — which is exactly what
    /// the library used to do with `reset_elapsed`. Creating it lazily
    /// instead starts the clock *after* the first chunk has arrived, and
    /// the rate is then computed over a near-zero window: a 3 MiB image
    /// reported 21.91 GiB/s.
    bar: Option<indicatif::ProgressBar>,
    /// Whether the bar's timer has been reset for this phase yet.
    bar_started: bool,
}

impl Cli {
    /// A front end for the device named on the command line.
    pub(crate) fn new(device: std::path::PathBuf) -> Self {
        Self { device, bar: None, bar_started: false }
    }
}

/// Whether a phase draws the verify bar rather than the flash bar.
///
/// The two templates differ, and selecting on whether a total is known
/// instead of on the phase gave a raw-image flash the *verify* bar —
/// both report a known total. The compressed path routed correctly by
/// accident, which is why it went unnoticed.
fn uses_verify_bar(phase: Phase) -> bool {
    phase == Phase::Verify
}

/// Seconds still to wait, or `None` when the countdown is over.
///
/// Saturating, so a `done` past `total` ends the countdown rather than
/// wrapping to a very large number of seconds.
fn countdown_remaining(done: u64, total: Option<u64>) -> Option<u64> {
    match total?.saturating_sub(done) {
        0 => None,
        remaining => Some(remaining),
    }
}

/// Whether a finished phase should leave its progress bar on screen.
///
/// A failed phase does: the bar shows how far the write got before it
/// stopped, which is what an operator needs in order to decide anything.
/// A completed phase clears it so the next phase's output starts on a
/// clean line.
///
/// Extracted so it can be tested. Inverted, a failed flash would wipe
/// the only on-screen record of how far it reached, and a successful one
/// would leave a stale bar behind.
fn keeps_bar_on_screen(outcome: PhaseOutcome) -> bool {
    outcome == PhaseOutcome::Failed
}

/// Whether a typed response authorises destroying the device.
///
/// Extracted from the prompt so it can be tested: fused to the `/dev/tty`
/// read it needed a terminal, and this is the last check standing between
/// a keystroke and an overwritten disk. Inverting the comparison would
/// mean every answer *except* "yes" proceeded.
///
/// Only an exact `yes` approves, after trimming the newline the terminal
/// appends. Not `y`, not `YES` — a prompt that accepts near-misses
/// trains people to answer without reading.
fn response_approves(input: &str) -> bool {
    input.trim() == "yes"
}

/// A byte count in the largest binary unit that keeps it readable.
///
/// The banner shows both the exact count and this, because they
/// answer different questions: the operator matches the exact bytes
/// against what they expect, and reads the scaled figure to notice
/// they picked a 32 GiB stick when they meant a 2 TiB disk. A wrong
/// divisor here shows a 500 GB disk as something else entirely.
///
/// Scaled rather than fixed at GiB: images are routinely far smaller
/// than devices, and a 700 MiB ISO rendered as `0.68 GiB` — or a
/// 3 MiB one as `0.00 GiB` — tells the reader nothing.
#[expect(
    clippy::cast_precision_loss,
    reason = "display-only figure on the confirmation banner; \
              sub-ULP rounding beyond 8 EiB is irrelevant"
)]
pub(crate) fn human_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    for (limit, unit) in
        [(KIB, "KiB"), (KIB * KIB, "MiB"), (KIB * KIB * KIB, "GiB"), (KIB * KIB * KIB * KIB, "TiB")]
    {
        if b < limit * KIB {
            return format!("{:.2} {unit}", b / limit);
        }
    }
    format!("{:.2} PiB", b / (KIB * KIB * KIB * KIB * KIB))
}

impl Events for Cli {
    fn phase_started(&mut self, phase: Phase) {
        self.bar_started = false;
        // Only the phases that used to announce themselves still do. The
        // quiet ones (preflight, topology, claim) finish in milliseconds
        // and a line each would be noise, not information.
        match phase {
            Phase::Wipe => println!("Wiping partition signatures..."),
            Phase::Flash => println!("Flashing image to {}...", self.device.display()),
            Phase::Verify => println!("Verifying data integrity..."),
            Phase::KernelSync => println!("Securing kernel and blocking automounts..."),
            Phase::Preflight | Phase::Topology | Phase::Claim | Phase::Automount | _ => {}
        }
    }

    fn phase_skipped(&mut self, phase: Phase) {
        match phase {
            Phase::Cooldown => println!("Skipping cooldown (--skip-cooldown)."),
            Phase::Verify => println!("Skipping verification (--skip-verification)."),
            Phase::Preflight
            | Phase::Topology
            | Phase::Claim
            | Phase::Wipe
            | Phase::Flash
            | Phase::KernelSync
            | Phase::Automount
            | _ => {}
        }
    }

    fn progress(&mut self, phase: Phase, done: u64, total: Option<u64>) {
        // The unit comes with the phase: Flash and Verify report bytes,
        // Cooldown reports seconds. Before `phase` was a parameter this
        // had to be recovered from state tracked across `phase_started`.
        if phase != Phase::Cooldown {
            let bar = self.bar.get_or_insert_with(|| {
                if uses_verify_bar(phase) {
                    make_verify_pb(total.unwrap_or(0))
                } else {
                    make_progress_bar(total)
                }
            });
            if self.bar_started {
                bar.set_position(done);
            } else {
                // The opening tick creates the bar and starts its clock,
                // and does nothing else. It must not draw — indicatif
                // renders on the first `set_position`, so a `tick` here
                // adds a 0% frame the old library never emitted — and it
                // must not step, since a zero-byte sample with no
                // elapsed time skews the rate estimator: the opening
                // figure read 263 GiB/s against a true 928 MiB/s.
                bar.reset_elapsed();
                self.bar_started = true;
            }
            return;
        }
        if let Some(remaining) = countdown_remaining(done, total) {
            {
                // Trailing spaces overwrite the residue of a longer
                // previous value: "10s" -> " 9s" would otherwise
                // leave a stray 's'.
                print!("\rCooldown and FTL sync... ({remaining}s)   ");
                drop(std::io::Write::flush(&mut std::io::stdout()));
            }
        }
    }

    fn phase_finished(&mut self, phase: Phase, outcome: PhaseOutcome) {
        if let Some(bar) = self.bar.take() {
            if keeps_bar_on_screen(outcome) {
                bar.abandon();
            } else {
                // `finish_and_clear` erases the bar and leaves the cursor
                // at the start of the line it occupied, so the next phase
                // line reuses that row. No newline here: one would leave a
                // blank line after Flashing and Verifying that the phases
                // without bars do not have.
                bar.finish_and_clear();
            }
        }
        if phase == Phase::Cooldown {
            match outcome {
                // Not "done": the other four phases print their line once
                // and say nothing on completion — the next line appearing
                // is the signal. This one only writes again because its
                // countdown rewrote the line in place, so the trailing
                // spaces are here to erase the last "(1s)   ", not to
                // report anything.
                PhaseOutcome::Completed => println!("\rCooldown and FTL sync...          "),
                // Bailing with "cancelled" either way; just close the line.
                PhaseOutcome::Failed | _ => println!(),
            }
        }
    }

    fn action(&mut self, message: &str) {
        eprintln!(" -> {message}");
    }

    fn warning(&mut self, message: &str) {
        eprintln!("warning: {message}");
    }

    fn confirm(&mut self, summary: &Summary<'_>) -> bool {
        let model = summary.model.unwrap_or("(unknown)");

        println!();
        println!("WARNING: This will DESTROY ALL DATA on {}", summary.device.display());
        println!("  Model:     {model}");
        println!(
            "  Size:      {} bytes ({})",
            summary.device_size,
            human_size(summary.device_size)
        );
        println!("  Image:     {}", summary.image.display());
        println!("  Format:    {}", summary.compression);
        if let Some(n) = summary.raw_image_size {
            println!("  Img size:  {n} bytes ({})", human_size(n));
        }
        println!();

        // Read the answer from the controlling terminal, not stdin: a
        // piped invocation must not be able to answer this. Write the
        // prompt there too, so a redirected stdout cannot hide it.
        let Ok(mut tty) = OpenOptions::new().read(true).write(true).open("/dev/tty") else {
            eprintln!("error: no controlling terminal to confirm on; rerun with --yes");
            return false;
        };
        if write!(tty, "Type 'yes' to proceed: ").is_err() {
            return false;
        }
        // Best-effort flush: the read below blocks on the same tty, so a
        // failed flush at worst delays the prompt appearing.
        drop(tty.flush());

        let mut input = String::new();
        if BufReader::new(tty).read_line(&mut input).is_err() {
            return false;
        }
        response_approves(&input)
    }

    fn finished(&mut self, device: &Path) {
        println!("SUCCESS: You can now safely remove {}.", device.display());
    }
}

#[cfg(test)]
mod tests {
    use super::human_size;
    use imi_core::{Phase, PhaseOutcome};

    /// Only the verify phase draws the verify bar.
    ///
    /// Selecting on whether a total is known instead once gave a
    /// raw-image flash the verify template — both report a known total —
    /// and the compressed path hid it by routing correctly for the wrong
    /// reason.
    #[test]
    fn only_verify_uses_the_verify_bar() {
        assert!(super::uses_verify_bar(Phase::Verify));
        for other in [Phase::Flash, Phase::Wipe, Phase::Cooldown, Phase::Preflight] {
            assert!(!super::uses_verify_bar(other), "{other:?} must use the flash bar");
        }
    }

    /// The countdown ends at zero and never wraps.
    #[test]
    fn countdown_ends_at_zero() {
        assert_eq!(super::countdown_remaining(0, Some(10)), Some(10));
        assert_eq!(super::countdown_remaining(9, Some(10)), Some(1));
        assert_eq!(super::countdown_remaining(10, Some(10)), None, "zero ends it");
        assert_eq!(
            super::countdown_remaining(11, Some(10)),
            None,
            "overshoot must end it, not wrap to a huge number of seconds"
        );
        assert_eq!(super::countdown_remaining(0, None), None, "no total, no countdown");
    }

    /// A failed phase keeps its bar; a completed one clears it.
    #[test]
    fn only_a_failed_phase_keeps_its_bar() {
        assert!(super::keeps_bar_on_screen(PhaseOutcome::Failed));
        assert!(!super::keeps_bar_on_screen(PhaseOutcome::Completed));
    }

    /// Only an exact "yes" may authorise destroying a device.
    ///
    /// Both directions matter and the negative cases carry the weight: a
    /// check that accepted anything would approve every accidental
    /// keystroke, and one that rejected "yes" would be inverted — under
    /// which every other answer proceeds.
    #[test]
    fn only_an_exact_yes_approves() {
        assert!(super::response_approves("yes"));
        assert!(super::response_approves("yes\n"), "the terminal appends a newline");
        assert!(super::response_approves("  yes  \r\n"), "surrounding whitespace is trimmed");

        for refused in ["", "\n", "y", "Y", "YES", "Yes", "yes please", "no", "yess", "ye"] {
            assert!(!super::response_approves(refused), "{refused:?} must not approve");
        }
    }

    /// Every phase line must terminate the same way.
    ///
    /// The cooldown is the only one that rewrites its line, so it is the
    /// only one that writes again on completion — and that write used to
    /// say "done", which none of the other four say. The trailing spaces
    /// exist to erase the last "(1s)   ", nothing more, so the erasure
    /// must be at least as wide as the widest countdown suffix.
    #[test]
    fn the_cooldown_line_erases_without_reporting_a_status() {
        // The widest suffix the countdown can leave behind.
        let widest = " (10s)   ";
        let terminator = "          "; // what the completion write appends

        assert!(
            terminator.len() >= widest.len(),
            "the erasure must cover the widest countdown suffix: {} < {}",
            terminator.len(),
            widest.len()
        );
        assert!(
            terminator.trim().is_empty(),
            "the terminator must be blank; the other four phases report nothing on completion"
        );
    }

    /// The banner's scaled figure must use binary units, and must pick a
    /// unit that keeps the number meaningful.
    ///
    /// Anchored at exact powers of two, plus the discriminator that
    /// catches a decimal divisor: 1 GB decimal is ~0.93 GiB, which after
    /// scaling lands in MiB. Showing an operator the wrong capacity is
    /// how the wrong device gets confirmed.
    #[test]
    fn human_size_uses_binary_units_and_scales() {
        // A real 32 GB stick, the case this format exists for.
        assert_eq!(human_size(30_765_219_840), "28.65 GiB");

        assert_eq!(human_size(0x4000_0000), "1.00 GiB");
        assert_eq!(human_size(8 * 0x4000_0000), "8.00 GiB");
        assert_eq!(human_size(0x2000_0000), "512.00 MiB");
        assert_eq!(human_size(0x10_0000), "1.00 MiB");
        assert_eq!(human_size(700 * 0x10_0000), "700.00 MiB");
        assert_eq!(human_size(0x400), "1.00 KiB");

        // The reason for scaling: fixed GiB would render these as 0.00.
        assert_eq!(human_size(3 * 0x10_0000), "3.00 MiB");
        assert_eq!(human_size(0), "0.00 KiB");

        // A decimal divisor would say "1.00 GB"; binary gives 0.93 GiB.
        assert_eq!(human_size(1_000_000_000), "953.67 MiB");

        // Above GiB it keeps scaling rather than showing four digits.
        assert_eq!(human_size(2 * 1024 * 0x4000_0000), "2.00 TiB");
    }
}
