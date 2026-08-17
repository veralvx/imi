//! Resolve the target device when `--dev` was omitted.
//!
//! The policy of *which* disks may be offered lives in
//! [`imi_core::candidate_devices`], beside the pipeline checks it
//! mirrors; this module contributes only the terminal interaction. The
//! division is deliberate: a GUI wanting the same list should not have
//! to reimplement the safety filter, and the binary should not be able
//! to loosen it.
//!
//! # Three refusals, one selection
//!
//! [`resolve_device`] returns the explicit `--dev` untouched, and
//! otherwise either the user's arrow-key selection or an error. It never
//! guesses:
//!
//! - **No candidates**: error naming the situation, because an empty
//!   picker teaches nothing — the message says what was filtered and
//!   points at `--dev`.
//! - **No terminal**: error *listing the candidates inline*, so a script
//!   author who forgot `--dev` sees exactly what to paste. A pipe must
//!   never be able to drive a selection, for the same reason
//!   `ui::confirm` reads `/dev/tty`: destructive choices come from a
//!   human at a terminal.
//! - **Escape / `q`**: error, "no device selected". Backing out of the
//!   menu is a decision to not flash, and it must not fall through to
//!   anything.
//!
//! A single candidate still renders the menu. Auto-accepting the only
//! plugged-in disk would make "plug in a stick, run imi" destroy that
//! stick with one fewer human decision than today; the whole pipeline is
//! built on adding decisions before destruction, not removing them.
//!
//! # Why the list renders on stderr
//!
//! `dialoguer` draws on stderr by default and this module keeps that:
//! stdout stays reserved for the pipeline's own output, so
//! `imi ... > log` captures a run's record without a menu's escape
//! sequences embedded in it.

use std::io::IsTerminal as _;
use std::path::PathBuf;

use anyhow::{Context as _, bail};
use imi_core::CandidateDevice;

use crate::ui;

/// The `--dev` value if given, otherwise an interactive selection.
///
/// See the module doc for the three refusals. `Err` from this function
/// means no device was chosen; nothing destructive has happened and the
/// caller simply reports it.
pub(crate) fn resolve_device(explicit: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(dev) = explicit {
        return Ok(dev);
    }

    let candidates = imi_core::candidate_devices()
        .context("no --dev given, and enumerating candidate devices failed")?;

    if candidates.is_empty() {
        bail!(
            "no --dev given and no candidate device found. Only whole disks \
             with media, no block-layer holders, and no system mounts are \
             offered; plug the target in, or name it explicitly with --dev."
        );
    }

    // A pipe must not drive a destructive selection — same rule as the
    // confirmation prompt. The candidates are listed so the fix is a
    // copy-paste away.
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        bail!(
            "no --dev given and no terminal to pick one on. Candidates:\n{}\
             \nRe-run with --dev <path>.",
            render_lines(&candidates).join("\n")
        );
    }

    let labels = render_lines(&candidates);
    let picked = dialoguer::Select::new()
        .with_prompt("Select the device to flash (Esc aborts)")
        .items(&labels)
        .default(0)
        .interact_opt()
        .context("reading the device selection")?;

    match picked {
        // In-range by construction — the index came from the menu built
        // over these labels — but this is the line that turns a selection
        // into a destruction target, so it gets the fail-closed form
        // anyway rather than an indexing panic's word for it.
        Some(i) => match candidates.get(i) {
            Some(c) => Ok(c.path().to_path_buf()),
            None => bail!("selection index {i} out of range (menu desynchronised?)"),
        },
        None => bail!("no device selected"),
    }
}

/// One display line per candidate: path, size, model, removability.
///
/// The order matches what the confirmation summary will show again
/// later, so the two reads of "what am I about to erase" line up.
fn render_lines(candidates: &[CandidateDevice]) -> Vec<String> {
    candidates
        .iter()
        .map(|c| {
            format!(
                "{:<12} {:>10}  {}{}",
                c.path().display(),
                ui::human_size(c.size_bytes()),
                c.model().unwrap_or("(unknown model)"),
                if c.removable() { "  [removable]" } else { "" },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {

    use imi_core::CandidateDevice;

    /// Every field a selection depends on appears in its line.
    ///
    /// `CandidateDevice::fixture` exists precisely because CI hosts have
    /// no real candidates — every disk carries a system mount, so the
    /// live enumeration is correctly empty and a test over it would be
    /// vacuous.
    #[test]
    fn rendered_lines_carry_path_size_model_and_removability() {
        let list = vec![
            CandidateDevice::fixture("sdb", 15_931_539_456, Some("SanDisk Ultra"), true),
            CandidateDevice::fixture("nvme1n1", 512_110_190_592, None, false),
        ];
        let lines = super::render_lines(&list);
        assert_eq!(lines.len(), 2, "one line per candidate");

        assert!(lines[0].contains("/dev/sdb"), "{:?}", lines[0]);
        assert!(lines[0].contains("14.84 GiB"), "size in human units: {:?}", lines[0]);
        assert!(lines[0].contains("SanDisk Ultra"), "{:?}", lines[0]);
        assert!(lines[0].contains("[removable]"), "{:?}", lines[0]);

        assert!(lines[1].contains("/dev/nvme1n1"), "{:?}", lines[1]);
        assert!(
            lines[1].contains("(unknown model)"),
            "a missing model must say so: {:?}",
            lines[1]
        );
        assert!(
            !lines[1].contains("[removable]"),
            "a fixed disk must not be dressed as removable: {:?}",
            lines[1]
        );
    }

    /// The live list renders one line per candidate, whatever the host has.
    #[test]
    fn live_list_renders_one_line_each() {
        let list = imi_core::candidate_devices().expect("enumeration succeeds on Linux");
        assert_eq!(super::render_lines(&list).len(), list.len());
    }
}
