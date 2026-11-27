---
status: plan
summary: "Everything blaze says to a person or an agent is data first: a `Report` (headline, rule, facts, fixes, blocks, causes) in crates/blaze-report, drawn by one pure renderer as styled text, plain text or JSON; the rendering rules, the build-script `cargo:blaze-report=` directive, `blaze explain`, and terminal capabilities with the `[terminal]` config. The record, renderer and directive reader are built, not wired: the CLI draws its findings through blaze-report's `frame` and detects the terminal through `terminal::detect`. Progress while a command works is end-state.md D181."
date: 2026-11-04
---
# Structured output

Everything blaze says (errors first, then warnings, notes,
progress, summaries and test results) is a record before it is text. One crate,
`crates/blaze-report`, holds the record and the one renderer. A person reads styled text on a
terminal or plain text through a pipe. An agent reads JSON or the API's CBOR. Both come from the
same record, so they can't drift apart.

It is the one home for what blaze says (`one-concept-one-home`): no ad hoc `error:` strings
beside it. The target, for the example that started it:

```
error: llvm-sys can't find LLVM  [llvm: pin]
  ┌ build script of llvm-sys 231.0.1  (vendor/llvm-sys/build.rs)
  │ needs LLVM 23.1.4 (e152d9f0), pinned in pins/llvm.json
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

## What is wired

`Report`, `render`, `json` and `script::read` are built, not wired: bstd re-exports them
(`bstd::report`) and no blaze command builds a `Report` yet. The CLI draws its findings with
blaze-report's `frame` (crates/blaze-report/src/frame/mod.rs) and detects the terminal with
`terminal::detect` (crates/blaze-cli/src/run_line.rs, `Config::DEFAULT` with `--color`).
`render` is pure: one report, the terminal's capabilities and the roots that paths are shown
relative to, in; bytes, out. It never reads the environment, so a golden test pins every case.

JSON has the same fields and the same order as CBOR. `Text` becomes a list of `{kind, text}`
spans, so an agent can still tell a path from a code span. A command's JSON prints each report as
one line, `{"reason":"blaze-report","report":{...}}` (`json_message`), beside cargo's own
messages, so an agent that reads cargo's JSON reads blaze's reports with the same loop.

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

Without these lines, blaze builds the report from the run itself (the rules are in
`blaze_report::rules::RULES`; the step that applies them on the rebuilt path is not built):

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

`blaze explain [STATUS]` prints what each exit status means (blaze-cli main.rs `STATUSES`). Not
built: `blaze explain NAME` for a rule (`blaze_report::rules::RULES`: its long text) or a package
with a remembered failure (rendered in plain text, then `blocks N packages:` and every name). With
it, each failure's text ends `more  blaze explain PACKAGE`: the text names three, `blaze explain`
names all, and the JSON holds everything.

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

`terminal::Config::from_toml` reads it; the CLI does not read the file yet. A key `[terminal]`
doesn't take is an error that names the key and the file. A missing file means
the defaults. Under `color = "auto"`, `NO_COLOR` turns colour off and `CLICOLOR_FORCE` turns it
on.

Detection:

- **No escapes at all** when stderr is not a TTY, under CI (`CI`), or with `TERM=dumb`. That
  covers OSC 8, OSC 9;4 and notifications.
- **OSC 9;4 progress** in Ghostty 1.2 and later (`TERM_PROGRAM=ghostty`, `TERM_PROGRAM_VERSION`),
  WezTerm, Windows Terminal (`WT_SESSION`) and ConEmu (`ConEmuANSI=ON`). The detection and the
  encoder (`Terminal::progress_into`) are built; the driver is not. Updates are rate-limited to one per percent change and at least 100 ms apart. State 2
  (error) is set on the first failure. The bar is always cleared (`ESC ] 9;4;0 ST`) on exit, on
  Ctrl-C and from the panic hook. A stuck bar in the tab is worse than none.
- **Notifications:** OSC 777 (`notify;title;body`) for Ghostty, WezTerm and foot; OSC 9 for
  iTerm2 (`TERM_PROGRAM=iTerm.app`); OSC 99 for kitty (`KITTY_WINDOW_ID`). The default,
  `notify = "failure"`, notifies only on failure, because whether the terminal has focus can't be
  known without asking it. `"always"` also notifies a success that took longer than
  `notify_after_secs` (`Terminal::notifies`). Next: with focus reporting (`CSI ? 1004 h`), the
  default also notifies a slow success while the terminal is unfocused, which is the behaviour
  Andrew asked for. The driver that sends notifications at the end of a command is not built.
- **tmux** (`TMUX`): no OSC 8, since tmux older than 3.4 drops it. Progress and notifications
  are wrapped in tmux's DCS passthrough (`ESC P tmux; … ESC \`, every ESC inside doubled) only
  when `tmux_passthrough = true`, because blaze can't ask tmux whether passthrough is allowed
  without a round trip.

## Progress and summary

What a command shows while it works, and its end summary, is end-state.md D181.
