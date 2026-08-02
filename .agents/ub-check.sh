#!/usr/bin/env bash
# Strongest undefined-behaviour checking available in the current
# environment. Runs Miri when it exists and falls back with an explicit
# note when it does not, so a clean run never silently means "the good
# checker was missing".
#
#   .agents/ub-check.sh              # everything available
#   .agents/ub-check.sh aligned      # filter to matching tests
#
# Exits non-zero if any check fails. A skipped check is reported but does
# not fail the run — the point is to say plainly what was and was not
# verified.
set -uo pipefail

FILTER="${1:-}"
failed=0
skipped=()

note() { printf '\n\033[1m== %s\033[0m\n' "$1"; }
skip() { skipped+=("$1"); printf '   SKIPPED: %s\n' "$2"; }

# ---------------------------------------------------------------- Miri
note "Miri"
if command -v cargo-miri >/dev/null 2>&1 || cargo miri --version >/dev/null 2>&1; then
    # Miri interprets the code and checks aliasing (Stacked Borrows),
    # provenance, alignment and uninitialised reads — the things no
    # other tool here covers. Tests touching real devices or spawning
    # threads against the kernel cannot run under it.
    MIRIFLAGS="-Zmiri-disable-isolation" cargo miri test -p imi-core --lib -- "$FILTER" || failed=1
else
    skip "miri" "not installed. Needs a rustup-managed nightly with the miri
            component, or rustc-dev to build from source. Neither is
            reachable in the offline container: static.rust-lang.org is
            blocked by the egress allowlist. CI runs it — see
            .github/workflows/rust-ci.yml."
fi

# ------------------------------------------------- strict provenance
note "strict provenance (compile-time)"
# Catches pointer<->integer casts that discard provenance, which is a
# subset of what Miri would flag and is available on this nightly.
# `-Zcrate-attr` injects the feature gate so the crate itself stays
# usable on stable.
if RUSTFLAGS="-Zcrate-attr=feature(strict_provenance_lints) \
              -Wfuzzy_provenance_casts -Wlossy_provenance_casts" \
        cargo check --workspace --all-targets 2>&1 | tee /tmp/ub-prov.log |
        grep -qE 'strict provenance'; then
    grep -E 'strict provenance' -A3 /tmp/ub-prov.log | head -40
    failed=1
else
    echo "   clean"
fi

# ------------------------------------------------------------ Valgrind
note "valgrind memcheck"
if command -v valgrind >/dev/null 2>&1; then
    # Catches invalid reads/writes and leaks in the real allocation
    # paths. Complements Miri rather than replacing it: memcheck sees
    # actual memory errors, Miri sees rule violations that have not yet
    # become memory errors.
    bin=$(cargo test -p imi-core --lib --no-run --message-format=json 2>/dev/null |
        python3 -c "
import json,sys
for line in sys.stdin:
    try: m = json.loads(line)
    except ValueError: continue
    if (m.get('reason') == 'compiler-artifact' and m.get('profile', {}).get('test')
            and m.get('target', {}).get('name') == 'imi_core'):
        print(m['executable'])
" | tail -1)
    if [ -n "$bin" ]; then
        valgrind --tool=memcheck --leak-check=full \
            --show-leak-kinds=definite,indirect \
            --errors-for-leak-kinds=definite,indirect \
            --error-exitcode=99 -q "$bin" "$FILTER" --test-threads=1 || failed=1
    else
        skip "memcheck" "could not locate the test binary"
    fi
else
    skip "memcheck" "valgrind not installed"
fi

# ------------------------------------------------------------- verdict
note "verdict"
if [ ${#skipped[@]} -gt 0 ]; then
    printf 'NOT VERIFIED BY: %s\n' "${skipped[*]}"
    printf 'A clean result below covers only the checks that ran.\n'
fi
if [ "$failed" -ne 0 ]; then
    echo "FAILED"
    exit 1
fi
echo "PASSED (for the checks that ran)"
