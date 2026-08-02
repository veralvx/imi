# 00 — CLI Surface and Terminal UX

**Source:** `crates/imi/src/cli.rs`, `crates/imi-core/src/lib.rs` (phase sequencing), `crates/imi/src/progress.rs` (bar templates — the rendered lines below are built there, in the **binary**), `crates/imi-core/src/phases/phase_4.rs`,
`crates/imi-core/src/phases/phase_5.rs` (progress bars).

## CLI surface

Summarised, not quoted: the descriptions below are shorter than the ones
`clap` prints. The flag names, short forms and the optional-value syntax
on `-t` are exact, and those are what a script depends on — run
`imi --help` for the full text.

```
imi [OPTIONS] --img <PATH> --dev <DEVICE>

  -i, --img <PATH>           Source image (raw or gzip/xz/bzip2/zstd compressed)
  -d, --dev <DEVICE>         Target whole-disk block device (not a partition)
  -t, --throttle [<RATE>]    Bandwidth cap. Suffixes K/M/G are binary multiples.
  -y, --yes                  Skip TTY confirmation
  --skip-verification        Skip Phase 5b read-back compare (Phases 6 + 7 still run)
  --skip-cooldown            Skip the 10 s Phase 5a hardware cooldown
  -h, --help                 Print help
  -V, --version              Print version
```

### `--throttle` is tri-state

The flag has three operationally distinct states, and downstream code
relies on the distinction:

| Invocation    | `cli.throttle`  | Behaviour                                   |
| ------------- | --------------- | ------------------------------------------- |
| (flag absent) | `None`          | Unthrottled — write/verify at maximum speed |
| `-t`          | `Some(8 MiB/s)` | Default safe cap for thermal-sensitive USBs |
| `-t 20M`      | `Some(parsed)`  | User-specified cap                          |

Implemented via `clap`'s `num_args = 0..=1` plus `default_missing_value =
"8M"`. The custom `parse_rate` value parser rejects zero, malformed
numerics, and unknown suffixes.

### `--skip-verification` and `--skip-cooldown` semantics

`--skip-verification` skips **only** Phase 5b (the byte-for-byte
read-back compare); `--skip-cooldown` skips **only** Phase 5a. Each is
independent, and everything else still runs:

- Phase 5a — 10-second hardware cooldown, unless `--skip-cooldown`.
  Cheap USB-NAND bridges drain writes from DRAM cache to flash on their
  own schedule, performing TLC/QLC garbage collection and FTL table
  updates while doing so. Pulling power before that completes corrupts
  the flash regardless of whether we ran a read-back compare. The
  cooldown is its own phase — `phase_5::cooldown(seconds, cancel, events)`
  — called from `phases/phase_5.rs::run` in the Phase 5a branch. It
  reports through `Events` like every other phase; the countdown a
  terminal shows is the binary rendering those `progress` calls.
- Phase 6 — `BLKRRPART` ioctl under the still-held `O_EXCL` claim.
- Phase 7 — multi-pass automount defense after the lock is released.

When operators ask to skip "the slow part", they almost always mean the
read-back compare, not the cooldown — so `--skip-verification` matches
that intent without touching the cooldown's safety contribution. The
cooldown requires its own explicit `--skip-cooldown`, added for loop
devices and automated tests; cli.rs documents the unplug-corruption
risk of using it on cheap bridges.

## Terminal output contract

The output format is fixed. It is not for human comfort; it is for
operators piping `imi` into log aggregators and parsing for
success/failure markers. Any change to these lines is a breaking change.

**On a TTY**, a full interactive run renders as:

```
Type 'yes' to proceed: yes
Wiping partition signatures...
Flashing image to /dev/sdc...
   [========================================] 100%  754.00 MiB / 754.00 MiB (4.01 MiB/s)
Cooldown and FTL sync...
Verifying data integrity...
   [========================================] 100%  754.00 MiB / 754.00 MiB (4.05 MiB/s)
Securing kernel and blocking automounts...
SUCCESS: You can now safely remove /dev/sdc.
```

**When stdout is not a TTY** — the case the parsing contract actually
serves — the rendering differs, and a parser must be written against
this, not against the block above:

- The two progress-bar lines are **absent**. `indicatif` draws nothing
  on a non-TTY, and nothing is emitted in their place — no blank line.
  The next phase line follows immediately (see _Progress bar hygiene_
  below for why no newline is written).
- The cooldown is an in-place countdown written with carriage returns.
  On a TTY it settles to `Cooldown and FTL sync...`; in a byte stream it
  arrives as every frame, CR-separated, ending with a final frame padded
  with spaces to erase the last countdown:
  `\rCooldown and FTL sync... (10s)   \r...(1s)   \rCooldown and FTL sync...`.
  There is no `done` token. Parsers wanting the cooldown's end should
  match the CR-delimited frame with no `(Ns)` in it, or simply wait for
  the next phase line.
- With `--yes` the `Type 'yes' to proceed:` prompt is absent entirely.
- Warnings such as `warning: BLKRRPART failed (...); proceeding anyway`
  go to **stderr**, never stdout, so they cannot corrupt a stdout parse.
  The final `error: ...` line on failure is likewise stderr.

So a piped, `--yes --skip-cooldown` run produces exactly this on stdout:

```
Wiping partition signatures...
Flashing image to /dev/sdc...
Skipping cooldown (--skip-cooldown).
Verifying data integrity...
Securing kernel and blocking automounts...
SUCCESS: You can now safely remove /dev/sdc.
```

Every phase line sits directly beneath the last, with no blank between.
`finish_and_clear` erases the bar and leaves the cursor at the start of
the row it occupied, so the next phase line reuses that row — emitting a
newline there would leave a gap after `Flashing` and `Verifying` that
the phases without bars do not have.

With `--skip-cooldown`, the `Cooldown and FTL sync...` countdown is
replaced by the single line `Skipping cooldown (--skip-cooldown).`
With `--skip-verification`, `Verifying data integrity...` and its bar
are replaced by `Skipping verification (--skip-verification).`
The two flags are independent and may be combined.

The success marker to parse for is the `SUCCESS:` prefix on stdout;
the failure marker is a non-zero exit status, with `error:` on stderr.

## Progress bar hygiene

`indicatif` does not clear the terminal line when a bar finishes by
default; subsequent text overwrites whatever rendering state was last
emitted. The fix is one line at every bar termination, in the binary's
`phase_finished`:

```rust
bar.finish_and_clear();
```

`finish_and_clear` removes the bar's last rendered output **and leaves
the cursor at the start of the row it occupied**, so the next phase line
reuses that row. There is deliberately no `println!()` after it: one
would leave a blank line after `Flashing` and `Verifying` that the
phases without bars do not have. An earlier revision of this document
prescribed that `println!()`, and the paragraph below — which says the
opposite — is the one that matches the code.

**Do not** use `finish_with_message`; it leaves the final state visible
and bleeds into the next line.

### Bar color: default terminal foreground

The `{bar:40}` template token has _no_ color suffix (no `.cyan/blue`,
no `.green/blue`). This is intentional: the bar inherits whatever
color the operator's terminal uses for normal text. Reasons:

- Light-themed terminals make ANSI cyan/green nearly unreadable.
- Operators piping output through `tee`, `script`, or log aggregators
  often have ANSI rendering disabled or recoloured by their terminal
  multiplexer; matching the default avoids surprises.
- The bar is informational, not a status indicator; we don't have
  semantic colors to assign (red for danger, etc.) — a bar advancing
  is a bar advancing regardless of hue.

If a future contributor wants to add color, the indicatif syntax is
`{bar:40.<fg>/<bg>}` where each is a name like `red`, `cyan`,
`bright.green`, or hex like `#ff8800`. **Do not** add color without
gating it on a `--color=auto|always|never` flag and respecting
`NO_COLOR=1` (per <https://no-color.org>).

## Throttle UI rate-spike fix

A naïve `--throttle 4M` setup using `indicatif` out of the box reports
an enormous initial rate (often > 100 MiB/s) that decays toward the
configured cap over a few seconds. Two contributing factors:

1. **The first chunk writes at full kernel speed.** Writes are
   instantaneous; the post-write sleep is what enforces the cap. So the
   raw "bytes per wall-second" sample for chunk 1 is huge — the
   throttle hasn't kicked in yet.
2. **Bar elapsed-time starts when the front end creates the bar**, not at the
   first write. Setup work (allocating the aligned buffer, opening the
   decompressor, reading magic bytes) counts toward the elapsed-time
   denominator and biases the average rate.

What `indicatif` already does for us: the `{bytes_per_sec}` token feeds
a **double-smoothed exponentially weighted estimator** internally
(`indicatif::state::Estimator`, file `state.rs`). The "smoothing" the
original spec asked for is built into the default token — there is no
`with_smoothing()` API on `ProgressStyle`, despite what the spec
suggested as an example. Custom tokens like `{smoothed_bytes_per_sec}`
do not exist; indicatif silently renders unknown tokens as empty
strings, producing a visible bug.

What we add on top:

```rust
ProgressStyle::with_template(
    "   [{bar:40}] {percent:>3}%  {bytes} / {total_bytes} ({bytes_per_sec})",
)
```

```rust
// In the binary's `Events` impl. The phase emits
// `progress(phase, 0, total)` where `reset_elapsed()` used to sit, and
// the front end creates the bar on `phase_started` — so it renders at
// zero immediately — then starts its clock on the first `progress`.
if self.bar_started {
    bar.set_position(done);
} else {
    bar.reset_elapsed();
    self.bar_started = true;
}
```

Creating the bar lazily on that first `progress` instead would start the
clock _after_ the first chunk had arrived, and the rate would then be
computed over a near-zero window: a 3 MiB image once reported
21.91 GiB/s that way.

`reset_elapsed()` zeroes the bar's internal start-time, so setup cost
doesn't enter the rate calculation. Combined with the built-in
double-smoothed estimator, this is enough to make the displayed rate
converge to the throttle cap within ~1 second instead of 5+.

If you find yourself wanting more aggressive smoothing (e.g. on
particularly slow USB controllers where chunk-to-chunk rate variance is
high), the right move is to compute and display a custom rate via a
template callback, not to look for a config flag — there isn't one.

### Unified bar format across phases

Phase 4 (flash, raw image) and Phase 5b (verify) both render with the
**same** template:

```
[{bar:40}] {percent:>3}%  {bytes} / {total_bytes} ({bytes_per_sec})
```

The three leading spaces are part of the constant, not this document's
indentation — they are what puts the bar under the phase line rather
than against the margin.

The template is not the whole style. Both builders also call
`.progress_chars("=> ")`, which is what makes the bar render as
`====>` rather than the block characters `█░` indicatif uses by
default. Reproducing the format from the template string alone gives a
bar that does not look like the samples below.

producing lines like:

```
[==================>                     ]  47%  476.84 MiB / 1.00 GiB (1.35 GiB/s)
[=============================>          ]  75%  762.94 MiB / 1.00 GiB (1.81 GiB/s)
```

Five components in fixed positions: a 40-cell bar in brackets, a
percentage right-aligned to three columns (so the "%" stays in the same
column from " 0%" through "100%"), `{bytes} / {total_bytes}` with
binary-suffixed values, and the current rate in parentheses. The
operator's eye doesn't have to recalibrate when the pipeline transitions
from writing to verifying — the layout is identical, only the semantic
meaning ("how much have we written/read so far?") changes.

Phase 4 with a _compressed_ image is the one exception: the
decompressed size isn't known, so we fall back to a spinner with
bytes-written and rate but no percent or total. This is the same
limitation `dd` has when reading from a stream of unknown length, and
operators flashing compressed images already accept it.

## TTY confirmation

The `--yes` bypass exists for automation, but the default path opens
`/dev/tty` _explicitly_ (read **and** write) — not `stdin` and not
`stdout`. Two separate concerns:

1. **Reading from /dev/tty, not stdin.** A piped invocation
   (`echo yes | imi ...`) must not bypass the prompt — that is
   precisely the failure mode that turns a typo into a destroyed root
   drive. `/dev/tty` always refers to the controlling terminal
   regardless of stdin redirection.
2. **Writing the prompt to /dev/tty, not stdout.** A pipeline like
   `imi ... > log.txt 2>&1` would otherwise show no prompt while
   still waiting for input — appearing to hang. Writing the prompt to
   the same `/dev/tty` we're reading from keeps it visible regardless
   of how the operator redirected the rest of the program's output.

The accepted answer is `yes` after trimming: `input.trim() == "yes"`.
Surrounding whitespace and the terminal's `\r\n` are stripped, so
`yes  \r\n` is accepted — but nothing else is. `YES`, `y`, and
`yes please` all abort, and the comparison is case-sensitive on purpose.
Stating this precisely matters more here than elsewhere: a reader
auditing the gate should not come away thinking it is tighter than it
is.

## Interrupting a run

Ctrl+C (or `SIGTERM`) sets a flag; it does not end the process. Every
waiting or looping phase polls that flag and returns an error, so the run
unwinds normally and `FlashGuard`'s FATAL notice reaches the operator.

A second Ctrl+C therefore does nothing, and during a blocking operation —
a write to slow media, or an `fdatasync` a cheap controller takes seconds
to acknowledge — the process can look unresponsive for a few seconds
after the first one. That is the intended behaviour: escalating to
`exit()` would skip the unwind, and with it the only notice telling the
operator the device is half-written.

The advice that follows from it, which belongs in any operator-facing
text: wait. Do not pull the device because Ctrl+C appears not to have
worked. `SIGKILL` is available for a process that genuinely must go, and
skips the notice for the same reason.

## Manual smoke tests for CLI changes

```sh
cargo test                                      # unit tests for parse_rate
./target/release/imi --help                 # surface inspection
./target/release/imi -t 0 -i /tmp/x -d /dev/null      # rejects zero
./target/release/imi -t 8X -i /tmp/x -d /dev/null     # rejects bad suffix
./target/release/imi -i /dev/null -d /dev/null        # rejects same-path
./target/release/imi -i /tmp/img -d /dev/null         # rejects non-block
```

If you change the help-text wording, update the example output blocks at
the top of this doc.
