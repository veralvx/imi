#!/usr/bin/env bash
# Verify the two things a container with only loop devices cannot.
#
#   A. Does this device expose the identity attributes at all?   (read-only)
#   B. Do two different sticks actually differ in them?          (read-only)
#   C. Is a partition of a partitioned device refused?           (safe: refusal)
#   D. Does the replug check fire?                               (DESTRUCTIVE)
#
# A, B and C touch nothing. D flashes the device if the check under test is
# broken, which is the whole point of running it — use a stick you would be
# happy to lose.
#
# Usage:  sudo ./verify-on-hardware.sh /dev/sdX [/dev/sdY]

set -uo pipefail

DEV="${1:-}"
DEV2="${2:-}"
if [[ -z $DEV ]]; then
    echo "usage: $0 <device> [<second-device>]   (the second enables test B)" >&2
    echo "  e.g. $0 /dev/sdc /dev/sdd   — real nodes, not placeholders" >&2
    exit 2
fi

IMI=${IMI:-imi}
if ! command -v "$IMI" >/dev/null; then
    echo "error: '$IMI' not on PATH. Build it and either install it, or run:" >&2
    echo "         IMI=./target/release/imi sudo -E $0 $*" >&2
    exit 2
fi

# Refuse a device that is not there. Without this, probing a missing node
# reports every attribute as "(absent)" — which is also what a real stick
# with no VPD data reports, so the two cases look identical. The usage
# line's own placeholder was pasted in verbatim once and produced a
# confident-looking report about nothing.
require_block_device() {
    local d=$1 what=$2
    if [[ ! -e $d ]]; then
        echo "error: $d does not exist ($what)." >&2
        [[ $d == *sdX* || $d == *sdY* ]] &&
            echo "       That looks like the usage line's placeholder — substitute a real device." >&2
        exit 2
    fi
    if [[ ! -b $d ]]; then
        echo "error: $d is not a block device ($what)." >&2
        exit 2
    fi
}

require_block_device "$DEV" "first argument"
[[ -n $DEV2 ]] && require_block_device "$DEV2" "second argument"

kname_of() { basename "$(readlink -f "$1")"; }
# Three states, not two. A sysfs attribute can be missing, or present and
# unreadable — SCSI registers `wwid` for every device but the read returns
# ENXIO when there is no VPD page 0x83 to report. imi treats both as
# absent and is right to; someone diagnosing a stick needs them apart.
attr() {
    local k=$1 a=$2 p v err
    p="/sys/class/block/$k/device/$a"
    if [[ ! -e $p ]]; then
        echo "(no such attribute)"
        return
    fi
    if ! v=$(tr -d '\0' <"$p" 2>/tmp/imi-attr-err); then
        err=$(sed -n '1s/.*: //p' /tmp/imi-attr-err)
        rm -f /tmp/imi-attr-err
        echo "(unreadable: ${err:-read failed})"
        return
    fi
    rm -f /tmp/imi-attr-err
    v=$(printf '%s' "$v" | head -1 | sed 's/^ *//;s/ *$//')
    [[ -n $v ]] && echo "$v" || echo "(empty)"
}

echo "=== A. identity attributes on $DEV ==="
K=$(kname_of "$DEV")
echo "  kname   : $K"
echo "  model   : $(attr "$K" model)"
echo "  serial  : $(attr "$K" serial)      <- VPD page 0x80"
echo "  wwid    : $(attr "$K" wwid)        <- VPD page 0x83"
echo "  size    : $(blockdev --getsize64 "$DEV" 2>/dev/null || echo '?') bytes"
echo
echo "  What this tells you:"
echo "    neither usable -> imi falls back to rdev+model+size. The strengthening"
echo "                     is inert on this device; test D would prove nothing"
echo "                     about it. Try another stick."
echo "    serial only    -> the common case. Test D exercises check_serial."
echo "    both usable    -> the case never exercised anywhere. Worth test D."
echo
echo "  A note on the three states above. \"(no such attribute)\" means the"
echo "  file is not there. \"(unreadable: ...)\" means it is — SCSI registers"
echo "  wwid for every device — but the read failed, typically ENXIO when the"
echo "  device reports no VPD page 0x83. imi treats both as absent and is right"
echo "  to; the distinction matters only when you are working out whether a"
echo "  given stick can exercise the check."
echo

if [[ -n $DEV2 ]]; then
    echo "=== B. do two sticks actually differ? ==="
    K2=$(kname_of "$DEV2")
    for a in model serial wwid; do
        v1=$(attr "$K" "$a"); v2=$(attr "$K2" "$a")
        if [[ $v1 == "(absent)" && $v2 == "(absent)" ]]; then
            verdict="both absent - cannot discriminate on this field"
        elif [[ $v1 == "$v2" ]]; then
            verdict="IDENTICAL - this field would NOT catch a swap"
        else
            verdict="differ - this field discriminates"
        fi
        printf "  %-7s %-24s %-24s %s\n" "$a" "$v1" "$v2" "$verdict"
    done
    echo
    echo "  This predicts whether test D is worth running. Both sticks are"
    echo "  plugged in here only so the fields can be compared cheaply - D"
    echo "  itself uses ONE stick at a time, swapped in the same port."
    echo
    echo "  Two sticks plugged in together get different rdev, so a check"
    echo "  against that pair would pass on rdev alone and prove nothing."
    echo "  The hazard D tests is one stick at a time landing on the SAME"
    echo "  minor: then rdev matches, model matches if same make, size"
    echo "  matches if same capacity, and only serial or wwid separates"
    echo "  them. If both read IDENTICAL above, D will fall through to"
    echo "  model/size and exercise nothing new - find another pair first."
    echo
    echo "  The interesting failure is two same-make sticks whose serials are"
    echo "  identical or blank. That is the case wwid exists for, and finding"
    echo "  one in the wild is itself a useful result - note the make."
    echo
fi

echo "=== C. is a partition refused? (safe - expects a refusal) ==="
PART=""
for p in /sys/class/block/"$K"/"$K"*; do
    [[ -d $p ]] && PART="/dev/$(basename "$p")" && break
done
if [[ -z $PART ]]; then
    echo "  $DEV has no partitions; partition the stick or skip."
    echo "  (A container loop device cannot produce this case, which is why it"
    echo "   went untested until 2026-08-02. It has since passed on a real"
    echo "   partitioned stick - this check is now a regression guard rather"
    echo "   than an open question.)"
else
    # A real regular file: Phase 0 checks the image before the target, so
    # passing /dev/null here refuses on "not a regular file" and the
    # partition gate never runs. That produced a false FAIL once.
    img=$(mktemp) && printf 'x' >"$img"
    echo "  trying: $IMI -i <tempfile> -d $PART --yes"
    out=$("$IMI" -i "$img" -d "$PART" --yes 2>&1); rc=$?
    rm -f "$img"
    echo "  exit=$rc"
    echo "$out" | head -2 | sed 's/^/    /'
    if [[ $rc -ne 0 ]] && grep -qi 'partition' <<<"$out"; then
        echo "  PASS: refused, and the message names the reason."
    else
        echo "  *** FAIL: a partition was not refused as a partition. ***"
        echo "  This is the ordering ensure_whole_disk is supposed to guarantee."
    fi
fi
echo

cat <<'EOF'
=== D. does the replug check fire? (DESTRUCTIVE IF BROKEN) ===

  Do this only with two sticks you are willing to destroy.

  1. Plug in stick 1. Note its /dev name.
  2. Run WITHOUT --yes, so it stops at the confirmation:

         sudo imi -i some.img -d /dev/sdX

  3. It prints the banner and waits at "Type 'yes' to proceed:".
     Leave it waiting.
  4. Physically unplug stick 1 and plug stick 2 into the SAME port.
     Confirm it took the same name:  lsblk -o NAME,SERIAL
     If it got a different name, the test is void - the check exists for
     the same-name case. Retry, or use a port that reassigns.
  5. Now type: yes

  PASS looks like a refusal naming the field that changed:

      error: device serial changed between confirmation and O_EXCL claim
      error: device WWID changed between confirmation and O_EXCL claim
      error: device model changed between confirmation and O_EXCL claim
      error: device number changed between confirmation and O_EXCL claim

  Any of those four is a pass - which one fires tells you which field did
  the work, and that is worth recording.

  FAIL is a flash that proceeds. If it does, stick 2 is being overwritten:
  Ctrl+C, and expect the FATAL notice. Then the check is not working on
  this hardware and the finding is real.

  Also worth trying: replug the SAME stick into the same port. That must
  NOT refuse - a check that fires on an unchanged device is a false alarm
  that trains operators to ignore it.
EOF
