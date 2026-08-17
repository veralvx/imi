default:
  @just --list --unsorted

# Strongest undefined-behaviour checks available in this environment.
# Runs Miri when present; always reports what it could not verify.
#   just ub            -> everything
#   just ub aligned    -> filter to matching tests
ub filter='':
    ./.agents/tools/ub-check.sh {{filter}}


cargo-check:
  cargo check --workspace --all-features
  #cargo check --workspace --no-default-features
  
test: 
  cargo test --workspace --all-features
  sudo cargo test --workspace --all-features -- --ignored
  #cargo test --workspace --no-default-features

# Run the suite as an unprivileged user.
#
# The development container is root, which hides any test that only
# passes because of it. One did: a chain-walk assertion that reached a
# wrapped io::Error as root and the root refusal — which wraps nothing —
# as anyone else. It passed here and failed for the first person to run
# `cargo test` normally.
test-unprivileged:
    cargo test --workspace --no-run 2>&1 \
      | grep -oE 'target/debug/deps/[a-z_]+-[0-9a-f]+' | sort -u > /tmp/imi-bins
    chmod -R a+rX target/debug/deps .
    while read -r b; do \
      [ -x "$b" ] || continue; \
      printf '%-24s ' "$(basename "$b" | sed 's/-[0-9a-f]*$//')"; \
      setpriv --reuid=65534 --regid=65534 --clear-groups "$b" 2>&1 \
        | grep -oE '[0-9]+ passed; [0-9]+ failed' | head -1; \
    done < /tmp/imi-bins

clippy: 
  #!/bin/sh
  export RUSTFLAGS="-Dwarnings"
  cargo clippy --all-targets --all-features --workspace
  #cargo clippy --all-targets --no-default-features --workspace
  #cargo +nightly clippy --all-targets --all-features --workspace
  #cargo +nightly clippy --all-targets --no-default-features --workspace

miri:
  cargo +nightly miri test

fmt-check:
  cargo fmt --check
    
docs-check:
  #!/bin/sh
  export RUSTFLAGS="-Dwarnings"
  cargo doc --workspace --no-deps --all-features

cog:
  cog check 
  
dprint-check:
  dprint check

audit:
  cargo audit

checks:
  just cargo-check
  just test
  just miri
  just clippy
  just fmt-check
  just docs-check
  just dprint-check
  just cog
  just audit

fmt: 
  cargo fmt 
  dprint fmt

[doc("Release a new version: [major|minor|patch]")]
release semver:
  #!/usr/bin/env bash
  set -euo pipefail

  # 0. Validate argument
  case "{{semver}}" in
    major|minor|patch) ;;
    *) echo "Error: argument must be 'major', 'minor', or 'patch' (got '{{semver}}')" >&2; exit 1 ;;
  esac

  # 1. Safety: working directory must be clean
  if [ -n "$(git status --porcelain)" ]; then
    echo "Error: Working directory is not clean. Commit or stash your changes first." >&2
    exit 1
  fi

  # 2. Safety: must be on main
  current_branch="$(git branch --show-current)"
  if [ "$current_branch" != "main" ]; then
    echo "Error: You must be on the 'main' branch to cut a release (on '$current_branch')." >&2
    exit 1
  fi

  # 3. Safety: local main must not be behind remote
  git fetch origin
  if [ "$(git rev-list --count HEAD..origin/main)" -ne 0 ]; then
    echo "Error: Your local 'main' branch is behind 'origin/main'. Please pull first." >&2
    exit 1
  fi

  # 4. Compute the bumped version tag
  # Handles both git-cliff v1 (array) and v2+ (object with .releases) JSON.
  tag="$(git cliff --unreleased --bump {{semver}} --context \
    | jq -r 'if type == "array" then .[0].version else .releases[0].version end')"

  if [ -z "$tag" ] || [ "$tag" = "null" ]; then
    echo "Error: Could not determine bumped version. Are there unreleased conventional commits?" >&2
    exit 1
  fi

  # If anything below fails before the commit, restore the three files.
  # Uses HEAD (not index) so cleanup works even after git-add.
  trap 'echo "Release aborted — restoring modified files." >&2; git checkout HEAD -- Cargo.toml Cargo.lock CHANGELOG.md 2>/dev/null' ERR

  # Strip leading 'v' for Cargo (e.g. v1.2.3 → 1.2.3)
  raw_version="${tag#v}"

  # 5. Bump the version in Cargo.toml.
  #
  # Two places carry it and both must move together:
  #   [workspace.package] version   — inherited by imi and imi-core
  #   [workspace.dependencies] imi-core = { version = ... }
  #
  # Bumping only the first leaves imi depending on the *previous*
  # imi-core. That still resolves for a patch bump (0.1.7 -> 0.1.8
  # satisfies ^0.1.7) but hard-fails the moment the minor changes
  # (0.2.0 does not satisfy ^0.1.7), and in between it lets a fresh
  # `cargo install imi` pick an older imi-core that may lack APIs the
  # new binary calls.
  awk -v ver="$raw_version" '
    /^version[[:space:]]*=/ && !done {
      print "version = \"" ver "\""
      done = 1
      next
    }
    /^imi-core[[:space:]]*=/ {
      sub(/version = "[^"]*"/, "version = \"" ver "\"")
      print
      next
    }
    1
  ' Cargo.toml > Cargo.toml.tmp && mv Cargo.toml.tmp Cargo.toml

  # 5b. Guard: the two version sites must agree.
  #
  # Cargo has no `version.workspace = true` for a dependency spec, so the
  # `imi-core` pin cannot be derived from `[workspace.package] version`
  # and has to be maintained. This check makes a mismatch fail the
  # release loudly instead of at `cargo publish`, and it holds whatever
  # rewrote the manifest — awk above, `cargo release`, or a hand edit.
  ws_ver="$(awk -F'"' '/^version[[:space:]]*=/ {print $2; exit}' Cargo.toml)"
  dep_ver="$(awk -F'"' '/^imi-core[[:space:]]*=/ {print $2; exit}' Cargo.toml)"
  if [ "$ws_ver" != "$dep_ver" ]; then
    echo "Error: workspace version ($ws_ver) != imi-core dependency pin ($dep_ver)." >&2
    echo "       imi would be published depending on the wrong imi-core." >&2
    exit 1
  fi

  # 6. Regenerate Cargo.lock to reflect the new version
  cargo check --quiet

  # 7. Regenerate the full changelog
  git cliff --bump {{semver}} --output CHANGELOG.md

  # 8. Stage exactly the three files modified by the release process
  git add Cargo.toml Cargo.lock CHANGELOG.md

  # 9. Commit and tag — clear the trap first: once committed, a push
  #    failure is solved by retrying the push, not by reverting files.
  trap - ERR
  git commit -m "chore(release): bump to $tag"
  git tag -a "$tag" -m "Release $tag"

  # 10. Push branch and tag atomically
  git push --follow-tags origin main
  cargo publish

  echo "Released $tag"






# ---------------------------------------------------------------------
# Valgrind
#
# memcheck is the one worth gating on. helgrind is not: Rust's `mpsc` is
# built on the lock-free `mpmc` channel, whose happens-before edges
# helgrind cannot model, so it reports races that are not there. A
# ten-line program doing nothing but `mpsc` send/recv reproduces the same
# reports. Use helgrind to investigate, never to gate.
#
# `--show-leak-kinds=definite,indirect --errors-for-leak-kinds=definite,indirect` is deliberate. The default
# also counts "possibly lost", and this project has exactly one such
# block: 48 bytes allocated by libtest's own result-collection channel
# (`test_main_static` -> `mpmc::Channel::recv` -> thread-local Context).
# It is never freed by design and is not ours. Counting it would make a
# clean run exit non-zero forever.
# ---------------------------------------------------------------------

# Memcheck every test binary in the workspace (no root needed).
valgrind:
  #!/usr/bin/env bash
  set -euo pipefail
  vg=(valgrind --tool=memcheck --leak-check=full
      --show-leak-kinds=definite,indirect --errors-for-leak-kinds=definite,indirect
      --track-origins=yes --error-exitcode=99 -q)
  cargo test --workspace --no-run --message-format=json 2>/dev/null \
    | jq -r 'select(.reason=="compiler-artifact" and .profile.test and .executable)
             | "\(.target.name)\t\(.executable)"' \
    | sort -u > /tmp/imi-vg-targets.tsv

  # Read from a file rather than a pipe: a `while` on the right of a pipe
  # runs in a subshell, so an `exit 1` inside it would not fail the
  # recipe — the failure would be silently swallowed.
  failed=0
  while IFS=$'\t' read -r name exe; do
    # loop_pipeline drives the binary as a subprocess and spends most of
    # its time in the hardware cooldown; under valgrind that runs for
    # many minutes. Use `valgrind-live` for the real binary.
    if [ "$name" = "loop_pipeline" ]; then
      echo "skip             $name (subprocess driver; see valgrind-live)"
      continue
    fi
    if [ "$name" = "phase_pipeline" ]; then
      echo "skip             $name (root-gated; see valgrind-root)"
      continue
    fi
    printf '%-16s ' "$name"
    if "${vg[@]}" "$exe" --test-threads=1 >/dev/null 2>"/tmp/vg-$name.err"; then
      echo "clean"
    else
      echo "ERRORS -> /tmp/vg-$name.err"
      failed=1
    fi
  done < /tmp/imi-vg-targets.tsv
  rm -f /tmp/imi-vg-targets.tsv
  exit "$failed"

# Memcheck the root-gated suites, which drive real loop devices.
# Requires root and a free /dev/loopN.
valgrind-root:
  #!/usr/bin/env bash
  set -euo pipefail
  exe=$(cargo test -p imi-core --test phase_pipeline --no-run --message-format=json 2>/dev/null \
        | jq -r 'select(.reason=="compiler-artifact" and .executable) | .executable' | tail -1)
  valgrind --tool=memcheck --leak-check=full \
    --show-leak-kinds=definite,indirect --errors-for-leak-kinds=definite,indirect --track-origins=yes \
    --error-exitcode=99 -q "$exe" --ignored --test-threads=1

# Memcheck one real flash through the binary itself. Pass a loop device.
#   just valgrind-live /dev/loop0 /tmp/image.iso
valgrind-live device image:
  #!/usr/bin/env bash
  set -euo pipefail
  cargo build
  valgrind --tool=memcheck --leak-check=full \
    --show-leak-kinds=definite,indirect --errors-for-leak-kinds=definite,indirect --track-origins=yes \
    --error-exitcode=99 -q \
    ./target/debug/imi -i "{{image}}" -d "{{device}}" --yes --skip-cooldown

# Helgrind over the threaded arms. Investigation only — see the note
# above; expect reports from inside std's channel that are not defects.
helgrind:
  #!/usr/bin/env bash
  set -euo pipefail
  exe=$(cargo test -p imi-core --lib --no-run --message-format=json 2>/dev/null \
        | jq -r 'select(.reason=="compiler-artifact" and .target.name=="imi_core" and .executable) | .executable' | tail -1)
  valgrind --tool=helgrind -q "$exe" pipelined worker cancel_mirror --test-threads=1 || true
