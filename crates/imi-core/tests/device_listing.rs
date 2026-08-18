//! Live structural checks of the candidate list, on whatever this host has.
//!
//! The unit tests pin the policy against synthetic inputs; this file pins
//! the *wrapper* against the real `/sys` — needing no root, since the
//! attributes it reads are world-readable — with assertions that hold on
//! any host: a CI runner with zero candidates passes vacuously through
//! the per-candidate loop but still exercises the enumeration itself.

#![expect(
    clippy::tests_outside_test_module,
    reason = "integration-test crate: it has no cfg(test) module by design"
)]
// The workspace denies unused crate dependencies per binary; an
// integration test that exercises one corner still links them all.
use bzip2 as _;
use flate2 as _;
use libc as _;
use nix as _;
use xz2 as _;
use zstd as _;

use imi_core::candidate_devices;

/// Every returned candidate is structurally sound on the real tree.
#[test]
#[cfg_attr(miri, ignore)]
fn live_candidates_are_structurally_sound() {
    let list = candidate_devices().expect("enumeration must succeed on a Linux host");
    for c in &list {
        assert!(c.size_bytes() > 0, "{}: media-less devices are filtered", c.kname());
        assert!(
            c.path().to_str().is_some_and(|p| p == format!("/dev/{}", c.kname())),
            "path and kname must agree: {c:?}"
        );
        assert!(
            !c.kname().starts_with("loop") && !c.kname().starts_with("zram"),
            "virtual devices must never appear: {c:?}"
        );
        assert!(
            std::path::Path::new("/sys/block").join(c.kname()).exists(),
            "{}: a candidate must exist in /sys/block",
            c.kname()
        );
    }
    // Removable-first ordering holds as a property of the whole list.
    let first_fixed = list.iter().position(|c| !c.removable());
    if let Some(i) = first_fixed {
        assert!(
            list[i..].iter().all(|c| !c.removable()),
            "no removable device may follow a fixed one: {list:?}"
        );
    }
}

/// The system disk never appears.
///
/// Resolved the same way the filter sees it: the disk whose mount target
/// is `/` is, by the whitelist rule, excluded. On hosts where `/` is an
/// overlay or tmpfs (containers), there is no such disk and the test
/// holds vacuously — which is the correct reading, not a gap: the rule
/// under test is "no listed disk carries a non-whitelisted mount", and
/// the strongest observable consequence is asserted below for every
/// host shape.
#[test]
#[cfg_attr(miri, ignore)]
fn no_candidate_carries_the_root_filesystem() {
    let list = candidate_devices().expect("enumeration must succeed");
    let root_dev = std::fs::read_to_string("/proc/self/mountinfo").ok().and_then(|t| {
        t.lines()
            .find(|l| l.split(' ').nth(4) == Some("/"))
            .and_then(|l| l.split(" - ").nth(1)?.split(' ').nth(1).map(str::to_owned))
    });
    if let Some(dev) = root_dev {
        for c in &list {
            assert!(
                !dev.contains(c.kname()),
                "the root filesystem's device {dev} must not be offered: {c:?}"
            );
        }
    }
}
