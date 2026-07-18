# Contributing to `imi`

Thank you for considering contributing to `imi`!

## Prerequisites

To build and test `imi` locally, you will need the following tools:

- **Rust:** Version 1.97 or later (stable; pinned in `rust-toolchain.toml`).
- **Just:** A command runner for our project automation.
- **Dprint:** Used for standardizing markdown and JSON formatting.
- **Cocogitto:** Used to enforce Conventional Commits.

## Setup

1. Fork this repository and create your branch from `main`.
2. Clone your forked repository locally:

```sh
git clone https://github.com/veralvx/imi && cd imi
```

## Architecture and Guidelines

Before you start coding, please read the [AGENTS.md](AGENTS.md) file. It contains the core rules of the repository and routes to documentation under `.agents/docs/` aimed at both human contributors and AI-assisted workflows.

## Testing Strategy

When adding new features or fixing bugs, please keep the following in mind:

- **Pure Functions:** Isolate core logic from side effects wherever possible and cover it with pure unit tests.
- **Regressions:** If you are fixing a bug, include a test that explicitly reproduces the previous failure state.
- **Destructive pipeline:** `tests/loop_pipeline.rs` holds `#[ignore]`d integration tests that flash real loop devices end-to-end. Run them with `sudo -E cargo test --test loop_pipeline -- --ignored --test-threads=1` on a machine where destroying a loop device's backing file is acceptable.

## Development Workflow

1. We enforce the [Conventional Commits](https://www.conventionalcommits.org/) specification.
2. Run our automated checks locally. Our repository includes a `Justfile` that mirrors our GitHub Actions CI pipeline.

```sh
just checks
```

This single command will run formatting (`cargo fmt`, `dprint`), linting (`cargo clippy -D warnings`), testing (`cargo test`), and commit validation (`cog check`). If `just checks` passes on your machine, your code likely passes on CI.

## Creating a Pull Request

1. Ensure your code passes `just checks` locally.
2. Open a Pull Request against the `main` branch.
3. In your PR description, clearly outline the problem you are solving. Link to the relevant open issue (e.g., `Fixes #123`), if any.
4. Wait for a maintainer to review your code.
