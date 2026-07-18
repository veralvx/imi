default:
  @just --list --unsorted

cargo-check:
  cargo check --workspace --all-features
  cargo check --workspace --no-default-features
  
test: 
  cargo test --workspace --all-features
  cargo test --workspace --no-default-features

clippy: 
  #!/bin/sh
  export RUSTFLAGS="-Dwarnings"
  cargo clippy --all-targets --all-features --workspace
  cargo clippy --all-targets --no-default-features --workspace
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

  # 5. Bump the version in Cargo.toml (first top-level occurrence only)
  awk -v ver="$raw_version" '
    /^version[[:space:]]*=/ && !done {
      print "version = \"" ver "\""
      done = 1
      next
    }
    1
  ' Cargo.toml > Cargo.toml.tmp && mv Cargo.toml.tmp Cargo.toml

  # 6. Regenerate Cargo.lock to reflect the new version
  cargo check --quiet

  # 7. Regenerate the full changelog
  git cliff --bump {{semver}} --output CHANGELOG.md

  # 8. Stage exactly the three files modified by the release process
  git add Cargo.toml Cargo.lock CHANGELOG.md

  # 9. Commit and tag — clear the trap first: once committed, a push
  #    failure is solved by retrying the push, not by reverting files.
  trap - ERR
  git commit -m "chore(release): prepare for $tag"
  git tag -a "$tag" -m "Release $tag"

  # 10. Push branch and tag atomically
  git push --follow-tags origin main

  echo "Released $tag"
