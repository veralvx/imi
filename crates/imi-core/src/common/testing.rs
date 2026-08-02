//! Test-only helpers shared by the phase test modules.
//!
//! Compiled only under `cfg(test)`, so nothing here reaches a consumer.
//!
//! Exists for the same reason `common` itself does: three phase modules
//! had grown their own copy of the same recording [`Events`] sink, two of
//! them identical to the byte. The crate's rule is that something more
//! than one phase depends on belongs here, and a test fixture is not an
//! exception — a fourth copy is how the third one's drift goes unnoticed.

use crate::events::{Events, Phase, PhaseOutcome};

/// An [`Events`] sink that remembers what it was told.
///
/// Records the three callbacks a phase test needs to assert on. The
/// remaining trait methods keep their defaults, which is the point of
/// them having defaults: a sink is not obliged to care about `action`
/// or `warning` to be a valid front end.
#[derive(Default)]
pub(crate) struct Recorder {
    /// Every `phase_started`, in order.
    pub(crate) started: Vec<Phase>,
    /// Every `phase_finished`, with its outcome, in order.
    pub(crate) finished: Vec<(Phase, PhaseOutcome)>,
    /// Every `progress`, as `(done, total)`.
    ///
    /// The phase is deliberately not recorded: the tests that use this
    /// drive a single phase, so it would be a constant column.
    pub(crate) progress: Vec<(u64, Option<u64>)>,
}

impl Events for Recorder {
    fn phase_started(&mut self, phase: Phase) {
        self.started.push(phase);
    }

    fn phase_finished(&mut self, phase: Phase, outcome: PhaseOutcome) {
        self.finished.push((phase, outcome));
    }

    fn progress(&mut self, _phase: Phase, done: u64, total: Option<u64>) {
        self.progress.push((done, total));
    }
}

/// A temp-file path that deletes itself, however the test leaves.
///
/// The phase tests built a `FlashGuard` over a tempfile and removed the
/// file with a trailing statement, which a failed assertion or a panic
/// skips. Each path carries the process id, so every run that failed
/// left a fresh file behind — 2767 of them had accumulated in `/tmp`
/// before this existed.
///
/// Deref gives `&Path`, so a caller passes it wherever the bare path
/// went.
///
/// # Ordering
///
/// `Drop` unlinks the name, not the inode: a [`FlashGuard`] holding an
/// open descriptor keeps reading and writing the same file afterwards,
/// but `std::fs::read` on the path fails with `ENOENT`. Verified both
/// halves rather than assumed.
///
/// Rust drops locals in reverse declaration order, so
/// `let (guard, path) = tempfile_guard(..)` drops `path` first. Any
/// assertion that reads the file *by path* must therefore run before the
/// end of the test, which is where every current caller has it. A
/// helper that returned the path and outlived it would fail with a
/// confusing "no such file" rather than a wrong-bytes mismatch.
///
/// [`FlashGuard`]: crate::common::guard::FlashGuard
pub(crate) struct TempPath {
    /// The path to unlink on drop.
    path: std::path::PathBuf,
}

impl TempPath {
    /// A unique path under the temp directory, not yet created.
    pub(crate) fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        /// Distinguishes paths within one process, since tests share a pid.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        Self { path: std::env::temp_dir().join(format!("imi-{tag}-{}-{n}", std::process::id())) }
    }
}

impl std::ops::Deref for TempPath {
    type Target = std::path::Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl AsRef<std::path::Path> for TempPath {
    fn as_ref(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _rm = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::TempPath;

    /// Two paths from one process must differ.
    ///
    /// The pid alone is not enough: the whole unit-test binary is one
    /// process, and `cargo test` runs its tests on parallel threads. Two
    /// live `TempPath`s sharing a name would have one test's `Drop`
    /// delete the other's file mid-assertion, which surfaces as a
    /// flake rather than a failure.
    #[test]
    fn paths_are_unique_within_one_process() {
        let a = TempPath::new("dup");
        let b = TempPath::new("dup");
        assert_ne!(&*a, &*b, "two paths with the same tag must still differ");

        // And across threads, which is how the suite actually runs.
        let handles: Vec<_> =
            std::iter::repeat_with(|| std::thread::spawn(|| TempPath::new("race").to_path_buf()))
                .take(8)
                .collect();
        let mut seen: Vec<_> = handles.into_iter().map(|h| h.join().expect("thread")).collect();
        seen.sort();
        let before = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), before, "concurrent TempPaths collided");
    }
}
