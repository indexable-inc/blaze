---
status: plan
summary: "Everything blaze says to a person or an agent becomes data first: a `Report` (headline, rule, facts, fixes, blocks, causes) in one crate, `blaze-report`, carried by the daemon's API as CBOR and drawn by one pure renderer as styled text, plain text or JSON; the rendering rules, the build-script `cargo:blaze-report=` directive, `blaze explain`, terminal capabilities and the `[terminal]` config, the progress and summary design for the next slice, and the inventory of what is left to port (2026-11-04, lane structured-output: errors through `ApiError`, the build-script failure, the llvm-sys pin and the sandbox's directive refusal are ported)."
date: 2026-11-04
---
# Structured output

Lane structured-output, 2026-11-04. Everything blaze says (errors first, then warnings, notes,
progress, summaries and test results) is a record before it is text. One crate,
`crates/blaze-report`, holds the record and the one renderer. A person reads styled text on a
terminal or plain text through a pipe. An agent reads JSON or the API's CBOR. Both come from the
same record, so they can't drift apart.

This replaces, rather than sits beside, blaze's flat `ApiError { what, at }` and the ad hoc
`error:` strings (`one-concept-one-home`). The target, for the example that started this lane:

```
error: llvm-sys can't find LLVM  [llvm: pin]
  ┌ build script of llvm-sys 231.0.1  (vendor/llvm-sys/build.rs)
  │ needs LLVM 23.1.4 (e152d9f0), pinned in crates/hir-ty-codegen/llvm.json
  │ `LLVM_SYS_231_PREFIX` is not set
  └ blocks 247 packages, including blaze-engine

  fix   nix develop
   or   python3 .github/scripts/llvm.py fetch --target aarch64-apple-darwin --dir ~/.blaze/llvm
        export LLVM_SYS_231_PREFIX=~/.blaze/llvm

  log   ~/.cache/blaze/workers/67e8ace3/log      more  blaze explain llvm-sys
```

## The record

`blaze_report::Report`, with its fields declared in CBOR key order (shorter names first, then
bytewise), so blaze-protocol's deterministic writer emits them without permuting
(`deterministic-output`):

| field | type | what |
|---|---|---|
| `at` | `Text` | the thing it is about: a unit, a build script, a file, a line |
| `log` | `Option<String>` | the full log, a path, whole in the data |
| `rule` | `Option<String>` | the rule that refused, `area:name` (`sandbox:link-dir`) |
| `facts` | `Vec<Fact>` | one line each of what was expected or found, `{key, text}` |
| `fixes` | `Vec<Fix>` | ways out, each `{note, lines}`: commands to run in order, or a note where no command does it |
| `blocks` | `Option<Blocks>` | what it stops, `{all, count}`: every package in the data, a count and up to three names in text |
| `causes` | `Vec<Report>` | the reports under it, each a whole report |
| `headline` | `Text` | the cause in plain words |
| `severity` | `Severity` | `error`, `warning` or `note` |

`Text` is a list of `Span`s: plain words, `code`, a `path`, a `hash`, a `count`, a `time`, a
`name`. The record keeps the whole value (a full path, a full hash). Only the renderer shortens
it. `Text::marked` reads backticks as code, so a script or an unported caller can write marked
text. `Text::plain` is a single span of words.

Why this shape: the headline must state the cause; the at box says where; facts say what was
expected against what was found; fixes say what to do. Every error blaze has written so far
splits into those parts. The flat string merged them, and only a person could pull them apart
again. Blocks is a field of its own because it comes from somewhere else: the graph knows it, not
the failing unit.

## The flow

1. **The daemon builds the record.** A unit's failure becomes `Response::Report { unit, report }`,
   sent after the unit's diagnostics. A refused request becomes `Response::Error(ApiError)`.
   `ApiError` *is* its `Report`: on the wire it serializes as the report alone. Its `what` and
   `at` fields are flat text derived from the report for callers that still read strings. They
   are never assigned. An error changes through `ApiError::and` or `From<Report>`.
2. **The API carries it.** Schema 36 (`crates/blaze-api/src/lib.rs`) adds `Response::Report` and
   `Behind::id`, the graph index of the unit a held unit waits behind. CBOR is deterministic and
   in key order (`cbor::check`, `Writer::permuted() == 0`). `ResponseRef::Report` reads the
   report owned: reports are rare, and the zero-copy path stays for the hot responses.
3. **The CLI fills in what only it knows.** The printer holds each report by its unit id. When
   `Response::Unchecked` arrives, every held unit whose `behind[].id` names a held report adds
   its package to that report's `blocks`. A package is the first token of a unit name, deduped,
   in order. The notable one, named first in text, is the last: the one the user asked for.
4. **The CLI renders.** `blaze_report::render(report, caps, roots)` is pure: one report, the
   terminal's capabilities and the roots that paths are shown relative to, in; bytes, out. It
   never reads the environment, so a golden test pins every case. `crates/blaze-cargo/src/cli/terminal.rs`
   is the one driver that reads the config file, the environment and stderr's TTY-ness, and calls
   `blaze_report::terminal::detect`.
5. **The CLI remembers.** At the end of a command that failed, the printer writes every failure
   report to `last-failure.json` in blaze's state directory (`.json.part`, then rename), for
   `blaze explain`.

## LLM access

- `--message-format=json` prints each report on stdout as one line:
  `{"reason":"blaze-report","report":{...}}`, beside cargo's own messages. An agent that reads
  cargo's JSON reads blaze's reports with the same loop.
- The API: an agent on the daemon socket gets `Response::Report` and `Response::Error` as CBOR,
  in the one versioned schema (`no-mcp`).
- `blaze explain PACKAGE --json` prints the remembered failure of PACKAGE as JSON, with its
  blocks list in full.
- `blaze explain RULE` prints a rule's long text: what it refuses, why, and the ways out.

JSON has the same fields and the same order as CBOR. `Text` becomes a list of `{kind, text}`
spans, so an agent can still tell a path from a code span.

## Rendering rules

1. The headline states the cause in plain words: `llvm-sys can't find LLVM`, never
   `failed to run custom build command`.
2. Name at most three things (`NAMED = 3`). Past that, give a count; the full list is in the data
   and in `blaze explain`.
3. Paths are relative to the workspace, else to `~`, else absolute.
4. Hashes are cut to 8 characters in text and kept whole in the data.
5. Each refusal names its rule after the headline: `[sandbox: link-dir]`. `blaze explain
   sandbox:link-dir` gives its long text.
6. Code spans are drawn Notion-style on a terminal (a dim background, no backticks) and as
   backticks when piped. `NO_COLOR` turns colour off.
7. Paths are OSC 8 hyperlinks (`file://`, percent-encoded) where the terminal draws them, never
   when piped.
8. Colour belongs to words: severity words in their colour, names bold, times, counts and paths
   dim, code in the code style. Never a whole line in one colour.
9. The at box wraps at the terminal width. A continuation line keeps the box's rule. Words never
   break mid-word.
10. A fix's first line follows `fix`, each further way out follows `or`, and a way out's next
    commands line up under its first.

## Build scripts: `cargo:blaze-report=`

A build script that fails prints `cargo::error=MESSAGE` as cargo wants, and may also print
`cargo:blaze-report=FIELD=TEXT` lines. The single colon is cargo's old metadata form. Cargo keeps
it as metadata for dependents and never refuses it, so the script still builds under cargo
(`superset-of-rust`). blaze reads these lines into the failure's report
(`crates/blaze-report/src/script.rs`):

- `headline=TEXT`: the cause in plain words;
- `rule=ID`: the rule;
- `fact.KEY=TEXT`: one line of what was expected or found;
- `fix=COMMAND` starts a way out; `fix+=COMMAND` adds its next command; `note=TEXT` says what to
  do where no command does it.

TEXT marks code in backticks. A line blaze can't read becomes an `unread` fact of the report and
is never dropped (`no-fallback`). A script's headline, rule and fixes override what blaze worked
out from the exit status. Its facts replace blaze's when it gave a headline, and come first
otherwise.

vendor/llvm-sys's `pin.rs` is the first script to speak it: the pin message in the example above
comes from its `pin_report`.

Without these lines, blaze builds the report from the run itself
(`crates/blaze-cargo/src/serve/unit/script.rs`, `failure`):

| what happened | rule | headline | facts |
|---|---|---|---|
| exit status not success | `build-script:failed` | `the build script failed` | `ended`, then the last 20 lines of stderr and of stdout, leaving out `cargo:` lines |
| one `cargo::error=` | `build-script:error` | the message | |
| several `cargo::error=` | `build-script:error` | `the build script logged N errors` | one `error` fact each |
| output blaze can't parse | `build-script:output` | the parse error | the remaining lines as `output` |
| a directive the sandbox refuses | `sandbox:link-dir`, `sandbox:link-arg`, `sandbox:rustc-env` | `a build script printed a directive its sandbox refuses` | each refused line as code, then why |

A refusal's fixes name the way out the sandbox gives: `build-script-link-dirs` for a link
directory, then `unsandboxed-build-scripts = ["pkg"]`.

## `blaze explain`

`blaze explain NAME` looks NAME up in this order:

1. a rule (`blaze_report::rules::RULES`): its long text;
2. a package with a remembered failure (`last-failure.json`): the failure rendered in plain text,
   then `blocks N packages:` and every name;
3. a history view, as before.

Each failure's text ends `more  blaze explain PACKAGE`. That gives three levels: the text names
three, `blaze explain` names all, and the JSON holds everything.

## Terminal capabilities and config

`blaze_report::terminal::Terminal` is the pure input that holds everything a terminal can do:
`caps` (colour, OSC 8 links, width), `progress` (OSC 9;4), `notifier`, `notify`,
`notify_after_secs` and `tmux`. `detect(config, var, is_tty, width)` derives it from the config
and the environment through an injected `var` (`sans-io-cores`). Goldens test each capability by
building the `Terminal` directly.

Config: `[terminal]` in blaze's `config.toml`, the file the launcher's `[update]` lives in
(`$BLAZE_HOME/config.toml`, else `~/.blaze/config.toml`):

```toml
[terminal]
color = "auto"            # auto | always | never; below --color and CARGO_TERM_COLOR
progress = "auto"         # auto | off: OSC 9;4
hyperlinks = "auto"       # auto | off: OSC 8
notify = "failure"        # failure | always | off
notify_after_secs = 10    # under "always", a success notifies only past this
tmux_passthrough = false  # true once `set -g allow-passthrough on`
```

A key `[terminal]` doesn't take is an error that names the key and the file. A missing file means
the defaults. Under `color = "auto"`, `NO_COLOR` turns colour off and `CLICOLOR_FORCE` turns it
on.

Detection:

- **No escapes at all** when stderr is not a TTY, under CI (`CI`), or with `TERM=dumb`. That
  covers OSC 8, OSC 9;4 and notifications.
- **OSC 9;4 progress** in Ghostty 1.2 and later (`TERM_PROGRAM=ghostty`, `TERM_PROGRAM_VERSION`),
  WezTerm, Windows Terminal (`WT_SESSION`) and ConEmu (`ConEmuANSI=ON`). This slice has the
  detection and the encoder (`Terminal::progress_into`); the driver comes with the progress slice
  below. Updates are rate-limited to one per percent change and at least 100 ms apart. State 2
  (error) is set on the first failure. The bar is always cleared (`ESC ] 9;4;0 ST`) on exit, on
  Ctrl-C and from the panic hook. A stuck bar in the tab is worse than none.
- **Notifications:** OSC 777 (`notify;title;body`) for Ghostty, WezTerm and foot; OSC 9 for
  iTerm2 (`TERM_PROGRAM=iTerm.app`); OSC 99 for kitty (`KITTY_WINDOW_ID`). The default,
  `notify = "failure"`, notifies only on failure, because whether the terminal has focus can't be
  known without asking it. `"always"` also notifies a success that took longer than
  `notify_after_secs` (`Terminal::notifies`). Next: with focus reporting (`CSI ? 1004 h`), the
  default also notifies a slow success while the terminal is unfocused, which is the behaviour
  Andrew asked for. The driver that sends notifications at the end of a command comes with the
  progress slice.
- **tmux** (`TMUX`): no OSC 8, since tmux older than 3.4 drops it. Progress and notifications
  are wrapped in tmux's DCS passthrough (`ESC P tmux; … ESC \`, every ESC inside doubled) only
  when `tmux_passthrough = true`, because blaze can't ask tmux whether passthrough is allowed
  without a round trip.

## Progress and summary (next slice)

Not ported in this slice. It is designed here so that its records come from the same place.

**Records.** Progress is not text either. The daemon already streams unit events
(`UnitStarted`, `UnitFinished`, `Unchecked`). A `Progress` record is a fold of them:
`{done, total, running: [unit], cores: {busy, all}, failed}`. The CLI renders that fold. Each
summary line is a `Report`-like `Outcome {name, path, ok, took, rerun}`. Both live in
blaze-report beside `Report`, and JSON mode prints them as `blaze-progress` (rate-limited) and
`blaze-outcome` lines.

**On a TTY, a live region** of at most three lines, redrawn in place, with no per-crate lines:

```
  ━━━━━━━━━━━━━━━━━━━━━━╸━━━━━━━━━━━━━  412/690 packages   9/10 cores
  blaze-engine  blaze-cargo  hir-ty  +6
```

It is cleared before any report is drawn and drawn again after it, so reports scroll above it.
OSC 9;4 mirrors the same percentage in the tab.

**On a TTY, an end summary** with one line per outcome that matters (each failure, each test
binary):

```
✓ built 690 packages in 41.2s
✗ blaze-engine  crates/engine   3 tests failed   rerun  blaze test -p blaze-engine
```

- Green ✓ and red ✗ are used only for outcomes. Names are bold. Times, counts and paths are
  dim. Commands are drawn in the code style. No line is drawn all in one colour.
- Workspace members carry no version (their path says which).
- Each failure ends with the exact command that reruns only it.

**Piped (plain mode)**, one tidy line per crate, aligned, with no bar and no escapes:

```
Compiling astlog-core      tools/astlog/core
Compiling serde 1.0.228
```

The inventory below counts the progress-line sites. blaze-ui's `Plain`, `Live` and
`summary_line` look unused by the CLI's printer. The next slice either ports them into the fold
or deletes them, keeping one home.

## Inventory and port order

Counted on this branch, 2026-11-04. These are the counts the port drives to zero.

**`ApiError::new(what, at)`**, which wraps flat text as a report with one plain headline
(`Report::text`): 262 outside tests, 30 in tests. By area:

| area | count |
|---|---|
| blaze-cargo `serve/unit/` | 137 (compile.rs 36, unit.rs 25, test.rs 18, deps.rs 13, script.rs 9, others 36) |
| blaze-cargo `serve/job.rs` | 57 |
| blaze-api | 31 |
| blaze-cargo `serve/observe/` | 13 |
| blaze-cargo, other `serve/` | 12 |
| blaze-intent | 8 |
| blaze-ci | 2 |
| blaze-ui | 1 |
| blaze-cargo `cli/daemon` | 1 |

Count: `rg -c 'ApiError::new\(' crates`. Each call left is a marked, unported error: it renders,
but with no rule, facts or fixes.

**Other text that should become records:**

- `eprint!`/`print!` family in CLI-facing crates: 249 (175 eprint, 68 print). By area: sandbox 49,
  blaze-fmt 44, blaze-engine 28, blaze-cargo `cli/` 61, `serve/` 21, blaze-registry 16,
  blaze-ci 7.
- `say::` calls: error 38, refused 15, status 9, warning 5. The `say::refused` and `say::cargo`
  lines that are still diagnostics, such as script.rs's `error()` path for a script's own
  warnings, are counted here.
- `"error:"`, `"warning:"`, `"note:"`, `"help:"` string literals: 71.
- Error plumbing as text, ported when a caller reaches them: `anyhow` 950 uses,
  `Result<_, String>` about 2113, `map_err(|e| format!(…))` 564.

**Port order:**

1. Done in this slice: `ApiError` as `Report`; `Response::Report`; the build-script failure
   (exit, logged errors, unparseable output); the sandbox's directive refusal; the llvm-sys pin
   message; blocks via `Behind::id`; `blaze explain`; `--message-format=json` reports; the
   `[terminal]` config, detection and escape encoders (colour and OSC 8 are live; progress and
   notifications wait for step 5).
2. `serve/unit/` (137): compile, link and test failures. These are the errors users see most.
   compile.rs's errors carry rustc-style diagnostics already. They become causes of a unit's
   report.
3. `serve/job.rs` (57): graph and request refusals, each with its rule.
4. The CLI's own `say::` and `eprint!` (61 + 66): flags, config and daemon start-up.
5. Progress and summary (above), deleting blaze-ui's unused printers.
6. blaze-api's own errors (31), `serve/observe/` (13), blaze-intent (8), then the remaining
   crates (sandbox, blaze-fmt, blaze-engine, blaze-registry).
7. Once `ApiError::new` reaches zero, remove it and the derived `what`/`at` fields. `ApiError`
   is then a `Report` in name too.

Each step is a whole port of its area (`whole-ports`): the area's callers build reports, and its
count drops to zero.
