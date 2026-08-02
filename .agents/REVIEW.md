# File-by-file review

One Rust file per turn. `.agents/review-map.json` is the map: every file,
what references it, where the docs mention it, its stakes, its review
order, and whether it is done. Update the map in the same commit as the
review.

Progress: **36 / 36** reviewed.

---

## How a review turn runs

These directives hold for **every** turn, start to finish. They encode
`efficient-code-review` (the pass architecture) and `efficient-reason`
(the reasoning loop), plus the failure modes this project has actually
produced.

### The scope contract — settle before reading a line

1. **What is under review** — the file, _plus its blast radius_: its
   `referenced_by_rust`, the invariants it upholds for callers, the
   phase it sits in. The reading target is always larger than the review
   target.
2. **What it is supposed to do** — from `referenced_in_docs`, the
   module doc, and the commit that introduced it. Without intent a
   review can only check internal consistency, never correctness. If
   intent had to be inferred, say so in the verdict.
3. **What the stakes are** — the map's `stakes` field:
   - `irreversible` — writes to the device, or holds the fd that does.
     Earns every sweep.
   - `safety-gate` — refuses before anything destructive. A gate that
     silently passes is worse than one that wrongly refuses.
   - `integrity` — detects corruption after the fact.
   - `recovery` — runs after the write; failure here is recoverable.
   - `contract` / `supporting` — fewer sweeps, still read in full.
4. **What was asked** — one file per turn. A severe issue found outside
   it is flagged briefly as out-of-scope, neither dropped nor inflated
   into an unrequested audit.

### Pass 0 — Orient

Read the intent artifacts, then skim the territory. Run what is runnable:

```sh
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo test --workspace -- --ignored --test-threads=1   # needs root
cargo mutants -p imi-core --in-place --file <the file> # commit first
```

For anything the operator sees, capture it: `.agents/ptycap.py` renders
through a pseudo-terminal, and redirected output hides progress bars
entirely.

For anything with `unsafe`, run `.agents/ub-check.sh` (or `just ub`). It
runs every undefined-behaviour check available and **names the ones it
could not run**, so a clean result never silently means the strongest
checker was missing.

**Miri is not obtainable in the offline container.** Established, so it
need not be re-derived:

| route                       | result                                                                                                 |
| --------------------------- | ------------------------------------------------------------------------------------------------------ |
| `rustup component add miri` | no rustup; the toolchain is a manually installed nightly                                               |
| download the component      | `static.rust-lang.org` → 403, outside the egress allowlist                                             |
| build from source           | source fetches fine (`raw.githubusercontent.com` → 200) but `rustc-dev` is absent and not downloadable |
| distro package              | none exists                                                                                            |
| `-Zsanitizer=address`       | runtimes not shipped with this toolchain; the link fails                                               |

What _is_ available here and covers part of the same ground: the
strict-provenance lints, injected with
`-Zcrate-attr=feature(strict_provenance_lints)` so the crate stays
stable-compatible, and valgrind memcheck over the real allocation paths.
Neither sees aliasing violations — Stacked Borrows is Miri's alone — so
a file with `unsafe` gets a verdict that says so. `just miri` runs it
where it exists, and CI does.

**Note which verifications degrade into assumptions** when something
cannot be run; the verdict inherits that uncertainty.

### Pass 1 — Structural

Judge the shape before the lines: what this module owns, what it assumes
its callers own, which invariant it is the sole guardian of, where its
`pub(crate)` boundary sits. Trace the main flow end to end. Note
structural findings and move on — **reading at mixed altitude is how
both design flaws and line bugs get missed**.

### Pass 1b — Design adequacy: is this the right approach at all?

Correctness and adequacy are different questions. A module can do
exactly what it intends, correctly, and still be the wrong shape. **This
pass asks whether the approach itself is the best available**, and it
runs before line-reading because a wrong approach makes line-level
findings moot.

Ask, in this order:

1. **What problem does this actually solve?** State it without reference
   to the implementation. If that sentence is hard to write, the module
   may be solving two problems and should be split.
2. **Has std or an existing dependency already solved it?** The
   highest-yield question. `anyhow` was carried for a job this crate
   ended up doing itself in eighty lines. Before writing a mechanism,
   check `std`, then `nix`, then `libc` — and read how they intend it to
   be used, not just whether the function exists.
3. **Is this the narrowest seam that does the job?** Prefer the
   mechanism that composes over the one that replaces. `E: Events +
   ?Sized` on the bound serves both a concrete sink and a trait object;
   a blanket `impl Events for &mut T` reached the same place through
   thirty lines of forwarding. **The narrower seam is almost always the
   better one, and finding it is the design work.**
4. **Is it idiomatic Rust?** Types over comments — `ArmedPhase` cannot
   spell `Disarmed`, so the invariant needs no assertion. `Result` over
   panics on anything an operator can trigger. `#[non_exhaustive]` on
   anything that may grow. Where this project departs from the idiom,
   the departure must be argued in the file, and this pass tests the
   argument.
5. **What are the alternatives, and why is this one better?** Name at
   least one real alternative and say what it costs. "No alternative
   considered" is itself a finding on anything load-bearing.
6. **Does it fail loudly, and in the build a consumer ships?** Prefer a
   compile error over a runtime check, a runtime check over a
   `debug_assert`. **`debug_assert` is compiled out of release** — phase
   ordering was guarded that way and the guard was absent exactly where
   it mattered. Anything that must hold in production needs a mechanism
   that exists in production.
7. **Is the unsafe necessary, is it justified, and is its precondition
   enforced?** Three questions, in that order, for every `unsafe` block.

   **Necessary.** Look for the safe equivalent before accepting the
   block at all, and then check the equivalent actually _is_ one. Two
   `libc::fcntl` calls in `direct_io.rs` went away entirely once `nix`
   — already a dependency — turned out to wrap them over `AsFd`. But
   `BLKROGET` in `phase_0.rs` stays: the sysfs `ro` file looks like the
   same value and is not. The ioctl returns
   `bdev_test_flag(bdev, BD_READ_ONLY) || get_disk_ro(bdev->bd_disk)`;
   `disk_ro_show` returns the right operand alone, so reading sysfs
   would silently narrow a write-protect refusal. **A same-named safe
   alternative is a hypothesis to check against the source, not a
   replacement.** Record the answer either way, so the next reviewer
   inherits it instead of re-deriving it.

   **Justified.** A SAFETY comment must name what the block relies on,
   and the block must cover the unsafe operation and nothing else — an
   enclosed `?` puts an early return inside `unsafe` and quietly adopts
   whatever is added after it.

   **Enforced.** Ask what _makes_ the precondition true, not what
   asserts it. `AlignedBuf` is `Send` because it exclusively owns its
   allocation; that it is not `Sync` is a `const _` assertion, because a
   comment was not enough. `identity.rs`'s fd is valid because the
   `&FlashGuard` borrow keeps the owner alive; `phase_0.rs`'s is valid
   because `File` ownership outlives the last use, which nix's
   `unsafe fn(fd: c_int)` cannot express in the type.

A finding here is graded like any other, and the severity is the
_consequence of the wrong shape_.

### Pass 2 — Detailed, full fresh coverage

- **Read every line, in order, including the boring parts.** Not by
  grep. Every defect worth finding in this project came from reading a
  mechanism; grep finds only what you already suspected — and a grep for
  `println!` is how a whole `write!`-based countdown survived a refactor
  that claimed to remove all terminal output.
- **Treat every comment as a claim under test, never as evidence.** A
  comment saying the blank line after a progress bar is load-bearing was
  wrong; the bar's own clear already repositions the cursor.
- **Trace values, not syntax** — follow each value from construction
  through every consumer. Most defects are a value no longer being what
  its name says.
- **Park rabbit holes** in one line and keep going. The parked list is
  Pass 3's queue; depth-first digression is how coverage dies.

### Pass 3 — Adversarial

Understanding is not verification. Switch stance and hunt. Ask of each
branch: what if the flag is already set on entry? what if the device
changed size since Phase 0? what if the image is exactly the device
size, or one byte over? what if the caller drives the phases out of
order, or skips one? what if this runs in release, where the
`debug_assert` is gone?

**A finding you can demonstrate is a defect; one you cannot is a
question** — label them differently. Where a probe settles it, write the
probe: five lines that demonstrate the bug beat an argument about it.

### Pass 4 — Sweeps, one concern across the whole file

Error paths · panic paths (`unwrap`, `expect`, indexing, integer
overflow in release) · **device-state branches** — the code that only
runs when a mount, a swap area or a stacked volume exists, which no
default fixture creates · naming-and-comment drift · **omissions** — the
check that should exist, the state that should be refused, the event
nobody emits. Omissions are invisible to line reading, which is why they
get their own sweep.

### Verdict

Lead with it: **sound / fix first / needs rework**, and the
one-sentence reason tied to the worst finding. Each finding gets
location, mechanism, consequence, severity
(Blocker/Major/Minor/Nit/Question), and a direction. **State coverage
honestly** — what was read, what was assumed, what was not reached. A
clean file gets a short, plain "sound"; manufacturing findings to look
thorough is its own failure.

---

## Sourcing — what counts as evidence

**Every claim about behaviour outside this repository must be checked
against a primary source before it is relied on or repeated.** Not
memory, not this repository's comments, not a search snippet.

In descending order of authority:

1. **The source at the pinned version** — `std` at the toolchain in
   `rust-toolchain.toml`, the `nix`/`libc`/`indicatif` version in
   `Cargo.lock`, the kernel tree for ioctl semantics.
2. **The manual page** for the syscall — `open(2)` for `O_EXCL` on a
   block device, `ioctl_blkdiscard(2)`, `swapoff(2)`, `pwrite(2)` for
   short-write semantics, `proc(5)` for `mountinfo` field order.
3. **The Rust API Guidelines and the Rustonomicon** for interface shape
   and for anything `unsafe`.
4. **Issue trackers and changelogs** — where the reason a thing exists
   is usually written down.

Rules that have each caught a real error here:

- **Pin the source to the version in use.** An API can exist on master
  and not on the release.
- **Read the condition, not the message.** An assertion whose text says
  one thing may gate on another.
- **Prefer the mechanism over the summary.** Where a doc and the code
  disagree, the code decides; say so when they differ.
- **Search for the negative**: `<thing> broken`, `<thing> deprecated`,
  `<thing> undefined behaviour`. Searching only for how to use something
  finds only people using it.
- **`O_EXCL` on a block device is not `O_EXCL` on a file.** Semantics
  that differ by file type must be checked for _this_ file type.

---

## Editing discipline

- **Reason the edit before making it.** State what changes, why, and
  what could break — the blast radius from the map.
- **Exact-string replacement only. Never slice by line or index.** Four
  files were damaged this way: a swallowed closing brace, a duplicated
  comment block, and one edit that silently deleted four tests because
  two doc comments had merged and the slice spanned them.
- **Assert the anchor matched, and fail loudly if not.** A script that
  reports success without applying its change produces a partial edit
  that looks complete. `run()` went a full round without its example
  because an anchor missed and the script said nothing.
- **Verify the artefact, not the script's exit code.** Re-read the
  written result; for anything an operator sees, capture it through the
  pty harness and read it.
- **One logical change at a time.** Bundled edits hide which one broke
  things.
- **Fix the documentation in the same edit as the code.** Stale prose
  left by a correct code change is the most common defect class here.
- **Watch the turn after a correction.** Error rate is measurably
  highest in the edit immediately following a successful fix.

## Committing

- **One commit per file reviewed**, even when the finding is "sound" —
  the history is the record that the review happened.
- **The message carries the evidence**: what was checked, against which
  source, what changed and why. A future reader must be able to
  re-verify without redoing the search.
- **Run the gates before every commit** and do not commit through a
  failure.
- **Commit before running `cargo mutants --in-place`**, and check
  `git status` after: a killed run leaves the mutation applied to the
  source. One survived into `phase_0.rs` and was a commit away from
  shipping a wrong size floor.
- **Update `.agents/review-map.json` in the same commit**: set
  `reviewed`, `reviewed_at`, record `findings`, and update the progress
  count at the top of this file.

---

## Failure modes this project has actually produced

Read this before each turn; every item is a real defect from this
repository, not a hypothetical.

| mode                                                                                                            | countermeasure                                                                                                                                                                                                                                                                                 |
| --------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Grepping one spelling and generalising (`println!` missed a `write!` countdown)                                 | enumerate every form the thing can take                                                                                                                                                                                                                                                        |
| Testing one branch of two and declaring the pair correct (compressed path passed, raw path was broken)          | test each branch, name which you tested                                                                                                                                                                                                                                                        |
| A slice-based edit spanning content you did not see                                                             | exact-string replacement, assert the anchor                                                                                                                                                                                                                                                    |
| A script reporting success without applying its change                                                          | verify the artefact afterwards                                                                                                                                                                                                                                                                 |
| `cargo mutants --in-place` killed mid-run, leaving a mutation in the source                                     | commit first; `git status` after; suspect an impossible-looking survivor                                                                                                                                                                                                                       |
| A stale binary used after a failed build (disk full)                                                            | read the build output; check `df`                                                                                                                                                                                                                                                              |
| Comparing a single sample of a non-deterministic value (bar throughput varies 11x)                              | run the reference three times before concluding                                                                                                                                                                                                                                                |
| A test that passes without asserting anything                                                                   | count assertions; make the test fail on purpose                                                                                                                                                                                                                                                |
| A root-gated test believed to kill a mutant                                                                     | `cargo mutants` runs without `--ignored`; extract the decision                                                                                                                                                                                                                                 |
| An ad-hoc checker producing false findings                                                                      | trust clippy, rustdoc and the doctest runner over a hand-written scan                                                                                                                                                                                                                          |
| Doc comment or `#[test]` orphaned from its item by an insertion                                                 | anchor on the **first line of the doc block**, never the `fn` line; a `#[test]` split from its `fn` silently disables that test                                                                                                                                                                |
| Fixing an instance and pinning the instance, leaving the category open                                          | enumerate what the code can produce and cover the enumeration                                                                                                                                                                                                                                  |
| Assertions that never see the whole output                                                                      | run it and read the session end to end                                                                                                                                                                                                                                                         |
| Cleanup as a trailing statement, so it never runs on a failing test                                             | a `Drop` guard; check `/tmp` for residue after a mutants run                                                                                                                                                                                                                                   |
| A guard whose predicate is a substring of the thing it rejects — `contains("println!")` is true for `eprintln!` | assert the _negative_ form, and mutate the source to watch the guard fail before trusting it                                                                                                                                                                                                   |
| A test that reports `ok` having asserted nothing                                                                | six root-gated tests returned early when a tool was missing. An `eprintln!` notice does not fix it: cargo captures output from _passing_ tests, so the line is invisible in exactly the run nobody inspects. The outcome is the only signal — fail, and name the tool and what went unverified |
| A test that passes only because the container is root                                                           | the dev container is uid 0 and most users are not. Run `just test-unprivileged` — a chain-walk assertion reached a wrapped io::Error as root and the sourceless root refusal as anyone else, passing here and failing for the first person to run `cargo test`                                 |
| A test filter that matches nothing, reporting "ok" for zero tests                                               | check the count in `test result:` — `0 passed` is not a pass; `--list` first when unsure of the name                                                                                                                                                                                           |
| A `cargo mutants` run filling the disk mid-review                                                               | it builds a full target tree per mutant; `rm -rf` the output dir and check `df` after, not before the next command fails                                                                                                                                                                       |
| Dismissing a surviving mutant as untestable without checking why                                                | ask what the mutant would break in production first                                                                                                                                                                                                                                            |

---

## Checklist

Order is dependency depth first: a file is reviewed only after
everything it relies on, so a consumer can be judged against understood
foundations. Within a tier the more destructive file comes first.

<!-- CHECKLIST:BEGIN -->
<!-- regenerated from .agents/review-map.json -->

| #  | file                                      | stakes       | lines | refs | done |
| -- | ----------------------------------------- | ------------ | ----- | ---- | ---- |
| 0  | `crates/imi-core/src/error.rs`            | contract     | 720   | 27   | x    |
| 1  | `crates/imi-core/src/events.rs`           | contract     | 252   | 27   | x    |
| 2  | `crates/imi-core/src/config.rs`           | contract     | 98    | 12   | x    |
| 3  | `crates/imi-core/src/common/ioctl.rs`     | irreversible | 45    | 4    | x    |
| 4  | `crates/imi-core/src/common/aligned.rs`   | irreversible | 231   | 6    | x    |
| 5  | `crates/imi-core/src/common/direct_io.rs` | irreversible | 37    | 2    | x    |
| 6  | `crates/imi-core/src/common/cancel.rs`    | supporting   | 151   | 3    | x    |
| 7  | `crates/imi-core/src/common/throttle.rs`  | supporting   | 106   | 2    | x    |
| 8  | `crates/imi-core/src/common/image.rs`     | integrity    | 530   | 9    | x    |
| 9  | `crates/imi-core/src/common/sysfs.rs`     | safety-gate  | 404   | 10   | x    |
| 10 | `crates/imi-core/src/common/guard.rs`     | irreversible | 398   | 15   | x    |
| 11 | `crates/imi-core/src/common/identity.rs`  | safety-gate  | 180   | 11   | x    |
| 12 | `crates/imi-core/src/common/mount.rs`     | safety-gate  | 1206  | 19   | x    |
| 13 | `crates/imi-core/src/common/context.rs`   | contract     | 263   | 20   | x    |
| 14 | `crates/imi-core/src/phases/phase_0.rs`   | safety-gate  | 763   | 5    | x    |
| 15 | `crates/imi-core/src/phases/phase_1.rs`   | safety-gate  | 64    | 4    | x    |
| 16 | `crates/imi-core/src/phases/phase_2.rs`   | safety-gate  | 111   | 5    | x    |
| 17 | `crates/imi-core/src/phases/phase_3.rs`   | irreversible | 281   | 6    | x    |
| 18 | `crates/imi-core/src/phases/phase_4.rs`   | irreversible | 1268  | 20   | x    |
| 19 | `crates/imi-core/src/phases/phase_5.rs`   | integrity    | 1104  | 14   | x    |
| 20 | `crates/imi-core/src/phases/phase_6.rs`   | recovery     | 39    | 3    | x    |
| 21 | `crates/imi-core/src/phases/phase_7.rs`   | recovery     | 105   | 3    | x    |
| 22 | `crates/imi-core/src/common.rs`           | supporting   | 26    | 16   | x    |
| 23 | `crates/imi-core/src/phases.rs`           | supporting   | 18    | 19   | x    |
| 24 | `crates/imi-core/src/lib.rs`              | contract     | 369   | 23   | x    |
| 25 | `crates/imi/src/cli.rs`                   | supporting   | 345   | 12   | x    |
| 26 | `crates/imi/src/progress.rs`              | supporting   | 213   | 7    | x    |
| 27 | `crates/imi/src/signals.rs`               | supporting   | 22    | 2    | x    |
| 28 | `crates/imi/src/ui.rs`                    | supporting   | 389   | 25   | x    |
| 29 | `crates/imi/src/main.rs`                  | supporting   | 45    | 0    | x    |
| 30 | `crates/imi-core/tests/public_api.rs`     | supporting   | 436   | 0    | x    |
| 31 | `crates/imi-core/tests/phase_pipeline.rs` | supporting   | 838   | 0    | x    |
| 32 | `crates/imi/tests/loop_pipeline.rs`       | supporting   | 471   | 0    | x    |
| 33 | `crates/imi-core/src/common/geometry.rs`  | contract     | 55    | 0    | x    |
| 34 | `crates/imi-core/src/common/testing.rs`   | supporting   | 136   | 0    | x    |
| 35 | `crates/imi-core/tests/api_consumer.rs`   | contract     | 116   | 0    | x    |

<!-- CHECKLIST:END -->
