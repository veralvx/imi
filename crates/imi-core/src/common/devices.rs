//! Enumerate the disks a picker may offer as flash targets.
//!
//! The `imi` binary shows this list when `--dev` is omitted; a GUI
//! consumer would want the same list, which is why the policy lives here
//! rather than in the binary: "which devices are plausible targets" is
//! safety knowledge, and two copies of safety knowledge drift.
//!
//! # The filter is the pipeline's own policy, applied early
//!
//! A disk appears in the list only if the pipeline could actually
//! proceed against it, and the exclusions reuse the pipeline's code
//! rather than restating it:
//!
//! - **Virtual and optical devices** (`loop*`, `zram*`, `dm-*`, `md*`,
//!   `ram*`, `sr*`, `fd*`, `nbd*`) are dropped by name. Flashing a loop
//!   device is a test-suite affair driven by explicit `--dev`; offering
//!   one to a human browsing for their USB stick is noise at best.
//! - **Media-less readers** are dropped by `sysfs::size_bytes` returning
//!   `None` for a zero size: an empty card reader cannot be flashed.
//! - **Stacked disks** — anything with a block-layer holder — are
//!   dropped via [`sysfs::holders_recursive`]. This is *stricter* than
//!   Phase 1, which tolerates kpartx-synthetic mappings: a fresh stick
//!   has no holders at all, so anything stacked is not the shape this
//!   list exists to offer. The explicit `--dev` path keeps Phase 1's
//!   more permissive judgement.
//! - **Disks with a system mount** are dropped by running every mount
//!   against [`mount::is_whitelisted`] — the same predicate Phase 1
//!   enforces. This one exclusion is what keeps the root disk, a
//!   LUKS-rooted disk (also caught as stacked), `/boot`, and mounted
//!   data disks out of the list, while a stick auto-mounted under
//!   `/run/media` stays in it, exactly as Phase 1 would rule.
//!
//! What the filter deliberately does **not** use is the `removable`
//! attribute: `NVMe`-in-USB enclosures and virtio disks report `0` while
//! being exactly what a user wants to flash. It sorts the list instead —
//! removable first — so the common case is the first arrow-key stop.
//!
//! # Never a decision, always a menu
//!
//! Nothing here selects a device. Even a one-element list goes back to
//! the caller for a human to confirm: auto-picking a destructive target
//! because it happened to be the only one plugged in is a decision this
//! crate refuses to make, in the same spirit as `Events::confirm`
//! defaulting to *no*.
//!
//! # What is trusted, what is sanitised
//!
//! A candidate carries two operator-facing strings with different
//! provenance. The kernel name comes from the kernel's own `/sys/block`
//! directory listing — kernel-trusted, so it is used raw in the
//! `/dev/{kname}` path and in display. The model string comes from the
//! device hardware itself — pluggable, therefore attacker-suppliable —
//! and is defanged at the read in `sysfs::device_model` (control
//! characters folded to U+FFFD), so every consumer of a
//! [`CandidateDevice`], present or future, sees only the sanitised
//! form. The boundary is provenance, not paranoia: sanitising what the
//! kernel names would add nothing, and trusting what the device claims
//! would let a hostile stick write escape sequences into a terminal.

use std::path::PathBuf;

use crate::Result;
use crate::common::{mount, sysfs};
use crate::error::Context as _;

/// A whole disk the picker may offer as a flash target.
///
/// Produced only by [`candidate_devices`]; the fields are private for
/// the same reason [`crate::Target`]'s are — the values mean "what the
/// enumeration measured", and a constructed or edited one would not.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CandidateDevice {
    /// `/dev/<kname>` — the path to hand to [`crate::Config::new`].
    path: PathBuf,
    /// Kernel name, e.g. `sdb`.
    kname: String,
    /// Capacity in bytes.
    size_bytes: u64,
    /// Model string from sysfs, if the device reports one.
    model: Option<String>,
    /// Whether the device reports itself removable. Display metadata:
    /// the module doc explains why it never gates the list.
    removable: bool,
}

impl CandidateDevice {
    /// A fixture for tests that need a candidate without a host that has
    /// one — CI containers mount every disk into the system, so the real
    /// enumeration is legitimately empty there. Mirrors
    /// `Target::for_test`; `cfg(test)` alone would hide it from the
    /// binary's unit tests, which sit in a different crate.
    #[doc(hidden)]
    #[must_use]
    pub fn fixture(kname: &str, size_bytes: u64, model: Option<&str>, removable: bool) -> Self {
        Self {
            path: PathBuf::from(format!("/dev/{kname}")),
            kname: kname.to_owned(),
            size_bytes,
            model: model.map(str::to_owned),
            removable,
        }
    }

    /// The device path, e.g. `/dev/sdb`.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// The kernel name, e.g. `sdb`.
    #[must_use]
    pub fn kname(&self) -> &str {
        &self.kname
    }

    /// Capacity in bytes. Never zero: media-less devices are filtered.
    #[must_use]
    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    /// The model string, if the device reports one.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Whether the device reports itself removable.
    #[must_use]
    pub fn removable(&self) -> bool {
        self.removable
    }
}

/// Prefixes of kernel names that are never plausible flash targets.
///
/// Matched with [`str::starts_with`], so `dm-` catches every mapper
/// device and `sr` catches optical drives. `sd` prefixes none of these,
/// which a test pins — the day a new entry shadows real disks, the
/// filter would silently empty the list.
const VIRTUAL_PREFIXES: &[&str] = &["loop", "ram", "zram", "dm-", "md", "sr", "fd", "nbd"];

/// Every disk the picker may offer, removable devices first.
///
/// See the module doc for what is excluded and why. The list can be
/// empty — a host with only its system disk plugged in has nothing to
/// offer — and an empty list is the caller's cue to ask for an explicit
/// `--dev`, never to relax the filter.
///
/// # Errors
///
/// Fails only if `/sys/block` or `/proc/self/mountinfo` cannot be read
/// at all; a single unreadable disk is skipped, not fatal, because one
/// odd device should not blind the picker to every other.
pub fn candidate_devices() -> Result<Vec<CandidateDevice>> {
    let knames = sysfs::all_disk_knames().context("enumerating /sys/block")?;
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .context("read /proc/self/mountinfo for the candidate filter")?;
    Ok(candidates_from_parts(&knames, &mountinfo, &DiskProbes::real()))
}

/// The five per-disk reads the filter consumes, as injectable probes.
///
/// Same idea as sysfs's `_in(root, ...)` pattern, one level up: the
/// filter's *decision table* — every keep and every skip — is what the
/// unit tests must drive, and three of its arms (holders, devt
/// resolution, and the keep path itself) are reachable only on a host
/// that has a qualifying disk, which CI hosts do not. A mutation run
/// proved it: deleting the holders check, or turning the devt-failure
/// `continue` into a `break`, left every test green. Probes make each
/// arm drivable from a fixture; [`DiskProbes::real`] is the production
/// wiring and its only other construction site.
pub(crate) struct DiskProbes<'a> {
    /// Capacity in bytes; `None` skips (no medium, or no such device).
    pub(crate) size_bytes: &'a dyn Fn(&str) -> Option<u64>,
    /// Recursive block-layer holders; non-empty or `Err` skips.
    pub(crate) holders: &'a dyn Fn(&str) -> Result<std::collections::HashSet<String>>,
    /// The disk's devt set, for the mount check; `Err` skips.
    pub(crate) devts: &'a dyn Fn(&str) -> Result<mount::TargetDevts>,
    /// Model string, display metadata.
    pub(crate) model: &'a dyn Fn(&str) -> Option<String>,
    /// Removable flag, display metadata — sorts, never gates.
    pub(crate) removable: &'a dyn Fn(&str) -> bool,
}

impl DiskProbes<'_> {
    /// The production wiring: every probe is the real reader.
    fn real() -> DiskProbes<'static> {
        DiskProbes {
            size_bytes: &sysfs::size_bytes,
            holders: &sysfs::holders_recursive,
            devts: &mount::TargetDevts::from_disk,
            model: &sysfs::device_model,
            removable: &sysfs::removable,
        }
    }
}

/// The pure policy: which of `knames` survive, in what order.
///
/// Split from [`candidate_devices`] so the filter is testable against a
/// synthetic mount table and device set; the wrapper contributes only
/// the two reads. Per-device sysfs probes still hit the real tree, which
/// the unit tests arrange to be irrelevant (their synthetic knames exist
/// nowhere, so the probes return the "skip" answer being tested).
fn candidates_from_parts(
    knames: &[String],
    mountinfo: &str,
    probes: &DiskProbes<'_>,
) -> Vec<CandidateDevice> {
    let mut out: Vec<CandidateDevice> = Vec::new();
    for kname in knames {
        if VIRTUAL_PREFIXES.iter().any(|p| kname.starts_with(p)) {
            continue;
        }
        // No medium (or no such device): skip, don't fail — see # Errors.
        let Some(size_bytes) = (probes.size_bytes)(kname) else { continue };
        // Anything stacked is not the fresh-stick shape this list offers.
        match (probes.holders)(kname) {
            Ok(h) if h.is_empty() => {}
            _ => continue,
        }
        // The pipeline's own mount policy, as a pre-filter: a disk
        // Phase 1 would refuse is a disk the picker does not offer.
        let Ok(devts) = (probes.devts)(kname) else { continue };
        let mounts = mount::mounts_in_text(mountinfo, &devts);
        if mounts.iter().any(|m| !mount::is_whitelisted(&m.target)) {
            continue;
        }
        out.push(CandidateDevice {
            path: PathBuf::from(format!("/dev/{kname}")),
            kname: kname.clone(),
            size_bytes,
            model: (probes.model)(kname),
            removable: (probes.removable)(kname),
        });
    }
    order_for_display(&mut out);
    out
}

/// Removable first — the sort, not the filter; the module doc has the
/// argument. Within a group, `all_disk_knames`'s ordering is preserved.
///
/// One function is the only place the ordering exists, so test and
/// production cannot diverge: an earlier sort test duplicated the
/// expression and stayed green under an inverted production sort.
fn order_for_display(candidates: &mut [CandidateDevice]) {
    candidates.sort_by_key(|c| !c.removable);
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::{CandidateDevice, DiskProbes, VIRTUAL_PREFIXES, candidates_from_parts};
    use crate::Result;
    use crate::common::mount::TargetDevts;
    use crate::error::err;

    /// Per-kname fixture rows behind the probes.
    ///
    /// Missing rows behave like the real tree does for an absent device:
    /// no size, no holders, an `Err` devt set — so a test only states
    /// what it is about.
    struct Fixture {
        /// One row per disk the test wants the enumeration to see.
        rows: HashMap<&'static str, Row>,
    }

    /// Everything the five probes can be asked about one disk.
    struct Row {
        /// What `size_bytes` answers.
        size: Option<u64>,
        /// What `holders` answers.
        holders: Result<HashSet<String>>,
        /// The single devt `devts` builds a set from, if any.
        devt: Option<(u64, u64)>,
        /// What `model` answers.
        model: Option<&'static str>,
        /// What `removable` answers.
        removable: bool,
    }

    impl Row {
        /// A disk every filter arm accepts.
        fn keeper(devt: (u64, u64), removable: bool) -> Self {
            Row {
                size: Some(1024),
                holders: Ok(HashSet::new()),
                devt: Some(devt),
                model: Some("Fixture Stick"),
                removable,
            }
        }
    }

    /// The kept knames, in order.
    fn names(v: &[CandidateDevice]) -> Vec<&str> {
        v.iter().map(CandidateDevice::kname).collect()
    }

    /// Drive the real function with probes over the fixture rows.
    ///
    /// The closures are locals so their borrows of `fixture` outlive
    /// the call — a method returning `DiskProbes` of `&|k| ...`
    /// temporaries does not compile, and rightly.
    fn run(fixture: &Fixture, knames: &[&str], mountinfo: &str) -> Vec<CandidateDevice> {
        let size_bytes = |k: &str| fixture.rows.get(k)?.size;
        let holders = |k: &str| match fixture.rows.get(k) {
            Some(r) => match &r.holders {
                Ok(h) => Ok(h.clone()),
                Err(e) => Err(err!("{e}")),
            },
            None => Err(err!("no such disk {k}")),
        };
        let devts = |k: &str| {
            fixture
                .rows
                .get(k)
                .and_then(|r| r.devt)
                .map(|d| TargetDevts::from_set_for_test(HashSet::from([d])))
                .ok_or_else(|| err!("no devt for {k}"))
        };
        let model = |k: &str| Some(fixture.rows.get(k)?.model?.to_owned());
        let removable = |k: &str| fixture.rows.get(k).is_some_and(|r| r.removable);
        let probes = DiskProbes {
            size_bytes: &size_bytes,
            holders: &holders,
            devts: &devts,
            model: &model,
            removable: &removable,
        };
        let owned: Vec<String> = knames.iter().map(|s| (*s).to_owned()).collect();
        candidates_from_parts(&owned, mountinfo, &probes)
    }

    /// No virtual prefix may ever shadow a real-disk name.
    ///
    /// The filter matches by `starts_with`, so an entry like `"s"` would
    /// silently drop every SATA disk and empty the picker. This pins the
    /// families real disks live in.
    #[test]
    fn virtual_prefixes_do_not_shadow_real_disks() {
        for real in ["sda", "sdz", "vda", "nvme0n1", "mmcblk0"] {
            for p in VIRTUAL_PREFIXES {
                assert!(!real.starts_with(p), "prefix {p:?} would hide the real disk {real}");
            }
        }
    }

    /// Virtual names are dropped before any probe runs.
    ///
    /// The probes panic if consulted, so this pins the short-circuit
    /// itself: reordering the prefix check after a probe fails loudly.
    #[test]
    fn virtual_names_are_dropped_before_any_probe() {
        let size_bytes = |k: &str| -> Option<u64> { panic!("size probed for virtual {k}") };
        let holders =
            |k: &str| -> Result<HashSet<String>> { panic!("holders probed for virtual {k}") };
        let devts = |k: &str| -> Result<TargetDevts> { panic!("devts probed for virtual {k}") };
        let model = |k: &str| -> Option<String> { panic!("model probed for virtual {k}") };
        let removable = |k: &str| -> bool { panic!("removable probed for virtual {k}") };
        let boom = DiskProbes {
            size_bytes: &size_bytes,
            holders: &holders,
            devts: &devts,
            model: &model,
            removable: &removable,
        };
        let knames: Vec<String> = ["loop0", "zram0", "dm-3", "sr0", "md127", "nbd1", "fd0", "ram2"]
            .map(str::to_owned)
            .into();
        assert!(candidates_from_parts(&knames, "", &boom).is_empty());
    }

    /// The keep path, end to end through the real function.
    ///
    /// This is the row no CI host can reach through the real tree, and
    /// the reason the probes exist. Both keeps carry their full measured
    /// contents, and the fixed disk sorts after the removable one —
    /// through `order_for_display` inside the function under test, not
    /// a copy of its expression.
    #[test]
    fn qualifying_disks_are_kept_with_their_contents_removable_first() {
        let fixture = Fixture {
            rows: HashMap::from([
                ("sda", Row::keeper((8, 0), false)),
                ("sdb", Row::keeper((8, 16), true)),
            ]),
        };
        // sda's mount is whitelisted, so it must NOT disqualify.
        let mountinfo = "36 29 8:0 / /run/media/u/STICK rw - vfat /dev/sda rw\n";

        let got = run(&fixture, &["sda", "sdb"], mountinfo);
        assert_eq!(names(&got), ["sdb", "sda"], "removable first: {got:?}");

        let sdb = &got[0];
        assert_eq!(sdb.path(), std::path::Path::new("/dev/sdb"));
        assert_eq!(sdb.size_bytes(), 1024);
        assert_eq!(sdb.model(), Some("Fixture Stick"));
        assert!(sdb.removable());
        assert!(!got[1].removable());
    }

    /// A non-whitelisted mount disqualifies — the root-disk rule.
    #[test]
    fn a_system_mount_disqualifies() {
        let fixture = Fixture {
            rows: HashMap::from([
                ("sda", Row::keeper((8, 0), true)),
                ("sdb", Row::keeper((8, 16), true)),
            ]),
        };
        let mountinfo = "1 1 8:0 / / rw - ext4 /dev/sda rw\n";
        assert_eq!(names(&run(&fixture, &["sda", "sdb"], mountinfo)), ["sdb"]);
    }

    /// A block-layer holder disqualifies, and so does a failed probe.
    #[test]
    fn holders_present_or_unreadable_disqualify() {
        let mut held = Row::keeper((8, 0), true);
        held.holders = Ok(HashSet::from(["dm-0".to_owned()]));
        let mut unreadable = Row::keeper((8, 16), true);
        unreadable.holders = Err(err!("sysfs went away"));
        let fixture = Fixture {
            rows: HashMap::from([
                ("sda", held),
                ("sdb", unreadable),
                ("sdc", Row::keeper((8, 32), true)),
            ]),
        };
        assert_eq!(names(&run(&fixture, &["sda", "sdb", "sdc"], "")), ["sdc"]);
    }

    /// A failed devt resolution skips that disk and only that disk.
    ///
    /// The later keeper is the point: a `break` where `continue` belongs
    /// would drop it too, silently emptying the tail of the list.
    #[test]
    fn a_devt_failure_skips_only_that_disk() {
        let mut broken = Row::keeper((8, 0), true);
        broken.devt = None;
        let fixture =
            Fixture { rows: HashMap::from([("sda", broken), ("sdb", Row::keeper((8, 16), true))]) };
        assert_eq!(names(&run(&fixture, &["sda", "sdb"], "")), ["sdb"]);
    }

    /// No medium and no such disk are the same skip.
    #[test]
    fn missing_or_zero_size_disqualifies() {
        let mut empty = Row::keeper((8, 0), true);
        empty.size = None;
        let fixture =
            Fixture { rows: HashMap::from([("sda", empty), ("sdb", Row::keeper((8, 16), true))]) };
        assert_eq!(names(&run(&fixture, &["sda", "sdb", "ghost"], "")), ["sdb"]);
    }

    /// Removable devices sort ahead of fixed ones, stably.
    #[test]
    fn removable_sorts_first() {
        let mk =
            |kname: &str, removable: bool| CandidateDevice::fixture(kname, 512, None, removable);
        let mut v = [mk("sda", false), mk("sdb", true), mk("sdc", false), mk("sdd", true)];
        super::order_for_display(&mut v);
        let order: Vec<&str> = v.iter().map(|c| c.kname.as_str()).collect();
        assert_eq!(order, ["sdb", "sdd", "sda", "sdc"], "removable first, stable within");
    }
}
