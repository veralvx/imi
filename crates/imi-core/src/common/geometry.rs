//! Device-layout constants shared by more than one phase.
//!
//! Exists because of the rule in [`crate::common`]: something two phases
//! depend on belongs here, so they do not reach into each other for it.
//! [`WIPE_REGION`] is the case that forced it — Phase 3 does the wiping
//! and Phase 0 refuses a device too small to be wiped, so both need the
//! size and neither owns it.
//!
//! Only the shared constant lives here. `tail_wipe_offset`, which
//! computes where the tail wipe goes, is used by Phase 3 alone and stays
//! there; the rule cuts both ways.

/// Bytes zeroed at each end of the target in Phase 3.
///
/// One MiB at the head and one at the tail. The head covers MBR, the
/// primary GPT and almost every filesystem superblock placed near the
/// start of a volume; the tail covers the backup GPT header, which lives
/// in the last 33 sectors and would otherwise survive to make the device
/// look like it still holds the old partition table.
///
/// Phase 0 refuses any device smaller than twice this, because below
/// that the two wipes overlap and the arithmetic that places the tail
/// stops being meaningful. That check runs in Phase 0 rather than here
/// so the refusal lands before the guard is armed — a device rejected
/// after arming would earn the FATAL notice despite never being written.
pub(crate) const WIPE_REGION: u64 = 1024 * 1024;

// The backup GPT header occupies the last 33 sectors of 512 bytes. A
// tail wipe shorter than that would leave it intact, and the device
// would still advertise the partition table the wipe was meant to
// destroy. Checked here rather than in a test because it is a fact about
// the constant, knowable at compile time.
const _: () = assert!(
    WIPE_REGION >= 33 * 512,
    "the tail wipe must cover the backup GPT header at LBA-1..-33"
);

#[cfg(test)]
mod tests {
    use super::WIPE_REGION;

    /// The wipe size is a documented on-disk constant, not a tunable.
    ///
    /// The wipe's correctness and Phase 0's minimum-size floor are both
    /// expressed in terms of it — the floor is `2 * WIPE_REGION`, the
    /// tail offset is `device_size - WIPE_REGION` — so an arithmetic
    /// slip here silently changes how much of the device is destroyed
    /// and how small a device the tool will accept. Pinned rather than
    /// left to arithmetic that would still typecheck.
    #[test]
    fn wipe_region_is_exactly_one_mib() {
        assert_eq!(WIPE_REGION, 0x0010_0000, "WIPE_REGION must be exactly 1 MiB");
        assert_eq!(WIPE_REGION, 1024 * 1024);
    }
}
