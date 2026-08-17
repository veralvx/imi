# Contributing to `imi`

Thank you for considering contributing to `imi`!

## Prerequisites

To build and test `imi` locally, you will need the following tools:

- **Rust:** Version 1.95 or later (stable; pinned in `rust-toolchain.toml`).
- **Just:** A command runner for our project automation.
- **Dprint:** Used for standardizing markdown and JSON formatting.
- **Cocogitto:** Used to enforce Conventional Commits.
- **A nightly toolchain with the `miri` component**
  (`rustup toolchain install nightly --component miri`). `just checks`
  runs `cargo +nightly miri test`, and so does CI.
- **cargo-audit** (`cargo install cargo-audit`), for the dependency
  advisory scan.

The last two are easy to miss: without them `just checks` fails on
tooling rather than on anything wrong with your change.

## Setup

1. Fork this repository and create your branch from `main`.
2. Clone your forked repository locally:

```sh
git clone https://github.com/veralvx/imi && cd imi
```

## Architecture and Guidelines

Before you start, please read the [AGENTS.md](AGENTS.md) file. It contains the core rules of the repository and routes to documentation under `.agents/docs/` aimed at both human contributors and AI-assisted workflows.

## Testing Strategy

When adding new features or fixing bugs, please keep the following in mind:

- **Pure Functions:** Isolate core logic from side effects wherever possible and cover it with pure unit tests.
- **Regressions:** If you are fixing a bug, include a test that explicitly reproduces the previous failure state.
- **Destructive pipeline:** two `#[ignore]`d suites flash real loop devices end-to-end — `crates/imi/tests/loop_pipeline.rs` drives the `imi` binary as a black box, and `crates/imi-core/tests/phase_pipeline.rs` drives `imi-core`'s per-phase API directly. Run them on a machine where destroying a loop device's backing file is
  acceptable:

  ```sh
  sudo -E cargo test -p imi --test loop_pipeline -- --ignored --test-threads=1
  sudo -E cargo test -p imi-core --test phase_pipeline -- --ignored --test-threads=1
  ```

## Development Workflow

1. [Conventional Commits](https://www.conventionalcommits.org/) specification is enforced.
2. Run automated checks locally. This repository includes a `justfile`
   that mirrors our GitHub Actions CI pipeline.

```sh
just checks
```

That runs nine recipes in order: `cargo check`, the test suite, Miri,
`cargo clippy -D warnings`, `cargo fmt --check`, a docs build with
warnings denied, `dprint check`, `cog check`, and `cargo audit`. If
`just checks` passes on your machine, your code likely passes on CI —
the workflows under `.github/workflows/` run the same set.

## Creating a Pull Request

1. Ensure your code passes `just checks` locally.
2. Open a Pull Request against the `main` branch.
3. In your PR description, clearly outline the problem you are solving. Link to the relevant open issue (e.g., `Fixes #123`), if any.
4. Wait for a maintainer to review your code.
