---
status: current
summary: What `blaze build`, `run`, `check` and `test` show: on a terminal a region redrawn while a build runs and at most one line when it ends, off one stable `blaze:` lines; and the one sans-IO core (blaze-screen) that makes both from the build's events.
---
# What a build shows

A build's progress is not kept. While it runs, a terminal shows a region that is redrawn; when
it ends the region is erased and at most one line stays, then the program runs on a clean
terminal. Diagnostics, failures and warnings of wasted work stay. A pipe gets lines that stay
and no escape byte.

## On a terminal

| what ran | what stays in the scrollback |
|---|---|
| `run` or `test` that built nothing | nothing: the program's output, the tests' summary |
| `build` or `check` that built nothing | `up to date · 21ms → /home/me/.cache/…/debug/hello` (dim; the program's path, cut in its middle) |
| an edit | `✓ hello::add recompiled in 0.70s · src/main.rs changed · tier 0` |
| a larger build | `✓ built 81 units, 4 cached in 45.8s · tier 0 · 3 warnings · blaze slow says why`, and `hint: 2 warnings have a fix: blaze check --fix` when some do |
| a failure | the diagnostics, then `✗ run failed in 0.14s · 1 error in hello (bin) · blaze explain hello` |
| tests | each failure as it happens (name, executable, its last 40 lines of output), then `✗ 8 of 9 ran: 1 failed · 7 passed · 1 ignored · 0 filtered · 3 binaries · 31.8s` (how many of the tests selected ran comes first and is always said; `filtered` is what the filter left out) and `rerun: blaze test -p calc --lib`; a run that ran none is refused, naming its filter ([cargo.md](cargo.md#a-run-that-ran-no-test-is-refused)) |

While a build runs (ripgrep from an empty store, 80 by 24; `tests/golden/` in blaze-screen has
every size):

```text
 ▶ globset                     codegen    6.4s  ← 22 wait on it
 ▶ serde_json (build script)   codegen    5.7s  ← 23 wait on it
 ▶ crossbeam-deque (build sc…  codegen    5.5s  ← 18 wait on it
 ▶ crossbeam-epoch (build sc…  codegen    5.5s  ← 23 wait on it
 ▶ crossbeam-utils (build sc…  codegen    5.5s  ← 28 wait on it
 ▶ zmij (build script)         codegen    4.8s  ← 28 wait on it
   … 6 more running
 building  ━━━━━━━━━━━━━──────  297/423 · 12 running · 20.0s
```

- **A row a unit under way**, the longest first so rows keep their place: its stage (`names`,
  `check`, `codegen`, `link`, `run build script`; two at once joined), how long, and how many
  tasks wait on it. With room: the wait it is in (`waits on ...`), and the function it is
  inside once that has lasted 2 s or the build is a handful of tasks (12). A path is cut in
  its middle and a macro's nesting said once: `libc::…::<impl#0 in macro>::default`.
- **Why something waits**, only while it does, a row each above the status row: every core
  busy with tasks ready, memory holding tasks back, the daemon's pause of the whole build
  (` ◦ paused: daemon at 7.6 of 81.5 GiB, waiting on 1 task: calc 0.1.0 lib (host)`). No
  memory or core count otherwise.
- **The status row**: the phase, done of total, how many run, the time; what the build waits
  on when no unit runs (a download, its plan); `⚠ nothing finished for 12.0s` in place of the
  bar once nothing has settled for 10 s.
- **Size**: the region takes a third of the terminal's rows, at least 4 and at most 20, never
  its last row or its last column. More units than rows are counted (`… 8 more running`).
- **Granularity**: units while many build, functions when the work is a handful. An edit's
  line names the functions recompiled (up to three, then a count); a larger build's names
  units; a row names a function only once it is slow.
- **What earns a line**: a warning (work that should not have been done: a comment-only edit
  recompiled, a step with no recorded reason, an image linked whole though one was kept) always;
  a note (why the build made again what it did, each link, each unit) only under `-v`. `-q`
  says errors only.
- **Colour** says nothing a glyph and a word do not (`▶` running, `✓` done, `✗` failed, `⚠`
  warned, `◦` waiting); without a UTF-8 locale the glyphs are ASCII (`>`, `ok`, `x`, `!`).
  [The palette](#rich-text-and-the-palette) picks the values.
- **Motion**: a frame when an event changed what is shown, and once a second for the timers.
  No spinner.

Tried on recorded builds and not kept: a dependency tree (nom's) took 56 rows for ripgrep's 12
units under way, most of them `waits` rows repeated under each parent; a lane a worker took 30
rows, 20 of them `(no task) llvm compile` a few milliseconds old. The frontier above took 8.

What the terminal offers beyond text, each detected (`blaze_report::terminal`) and never
written to a pipe: its own progress report (OSC 9;4: Ghostty 1.2, WezTerm, Windows Terminal,
ConEmu), cleared on every way out; hyperlinks (OSC 8) on a diagnostic's `--> path:line:col`
(a `file://` URL of the file), on an error's code (its page in the error index) and on the
built program's path, the visible text unchanged; a notification (OSC 777, OSC 9, kitty's OSC
99) of the end of a build of `[terminal] notify_after_secs` or longer, as `[terminal] notify`
asks (a failed one by default, `always` any), whether or not the terminal has the focus, which
blaze does not ask; synchronized output (mode 2026) around every frame. A
region's rows are erased one at a time (EL), never to the end of the screen (ED): tmux takes
an ED from its top-left corner for a clear and copies the screen into its history. Not used: the window title (no terminal restores it reliably) and prompt
marks (OSC 133 is the shell's).

## Rich text and the palette

A message is never a string with backticks for a renderer to guess at. It is a
`blaze_report::Text`: spans, each with what it is (words, code, a command the reader can run, a
path, a package, a count, a time, a hash, emphasis). It is written as easily as `format!`, and
an argument's type picks its role, so no fragment is untyped:

```rust
say!(Code(name), " recompiled in ", took, " · ", Cmd("blaze explain why:input"))
```

A literal is words; `Code`, `Cmd`, `At` (a path), `Pkg`, `Count`, a `Duration` and `Strong` are
the roles; a `String` is no part, and prose known only at run time says so as `Words`. Each
reader draws the same value: a terminal draws code and commands in their colours with no
backticks, a path from the checkout as a link to its file; without colour (`NO_COLOR`, a
pipe) code and commands get their backticks back; JSON carries the spans with their roles
(`{"Code":"hello::add"}`), so an agent parses no prose. The screen's own lines, a build's why
and blaze's reports are this type. What is not yet: the messages that still format an
interpolation between backticks by hand, 5,129 in 99 crates as merged on 2026-11-12
(`crates/blaze-report/tests/backticks.txt`, a crate a row; its test holds each crate to its
number, which only goes down), a diagnostic of code among them (`blaze_diagnostics::Diag`'s
message is a string).

Code names what a colour means (`blaze_report::palette::Token`: success, failure, warning,
running, waiting, cached, code, command, path, number, dim, emphasis), never a colour. The
value is picked for the terminal: 24-bit where `COLORTERM` says `truecolor` or `24bit` or the
terminal is Ghostty, kitty, iTerm2, WezTerm or VS Code's; 256 where `TERM` says `256color`;
else the eight basic colours with bold and underline. The background is `[terminal]
background = "dark"` or `"light"` of blaze's `config.toml`, else what `COLORFGBG` says, else
dark. The 24-bit values meet WCAG AA (4.5 or more) against black, `#1c1c1c`, `#1e1e1e`,
`#282c34` and `#002b36` (dark; the least is cached, 4.55) and white, `#f5f5f5`, `#fefefe` and
`#fdf6e3` (light; the least is path, 5.84). Under simulated protanopia and deuteranopia,
success and failure, and running and waiting, are a few ΔE apart: hue does not tell them
apart, their glyph and word do, and failure is bold. A command is underlined at every depth.

## After a `run`

The program a `run` becomes is attached to the terminal itself, with no pipe or pty between:
a program that sees no terminal behaves differently, so stdout and stderr are not told apart.
When whoever started it waits for it and says how it ended (`Event::Ran`: its code or its
signal, wall time, the processor's time, peak memory), a terminal gets one dim footer:

```text
✗ exit 3 · 40ms
✗ killed by SIGSEGV (core dumped) · 40ms
✓ exit 0 · 2.10s · peak 14.2 MB
✓ exit 0 · 12ms · peak 14.2 MB · cpu 9ms user, 2ms system      (--stats)
```

Always when it did not end well; after a clean exit only for a run of a second or more
(`Options::footer_after_ms`; 0 is always) or `--stats`. A panic's words are the program's own
and are not repeated. The footer starts on a line of its own when the program may have left
the cursor mid-line (a mark and a line of blanks first, as a shell does before its prompt). A
pipe gets nothing added. Built: the event, the footer and its tests. Not built: today `run`
becomes the program (`exec`), so nobody waits for it and no `Ran` is ever sent; the launcher
(or `cli/run.rs` and the thin `blaze`) has to start it as a child, wait with `wait4` (macOS:
`proc_pid_rusage` for the footprint), restore the terminal modes it saved, exit with the
program's code (128 plus the signal's number for a signal) and send the event; `--stats` and
the setting are theirs to read.

## Off a terminal

A pipe, a file, CI, `TERM=dumb`, `BLAZE_UI=plain`, `--message-format json`. An agent needs to
know that the command is alive, each failure with its place, whether its edit was taken, and
what to run next, in few lines. stderr gets, each a whole line:

```text
blaze: progress phase=compiling done=144/147 running=1 elapsed_ms=10556 longest="codegen hello 0.1.0 bin hello (host)" for_ms=4001
blaze: ok built=1 cached=31 warnings=0 elapsed_ms=703 tier="tier 0 · 0% optimized" cause="src/main.rs changed" recompiled=hello::add program=/w/target/debug/hello
blaze: failed errors=1 warnings=0 unbuilt=0 elapsed_ms=15 unit="hello (bin)" next="blaze explain hello"
```

`progress` every 10 s of a build under way, never sooner; the diagnostics as before; one `ok`
or `failed` line always (and `blaze: hint fixable=2 run="blaze check --fix"` when warnings
have a fix), `key=value` with a value quoted when it is not one word. No start
line: it says nothing its reader did not type. stdout is the program's and the JSON messages',
unchanged. Test executables write libtest's own output, and the run ends with its counts
(`blaze_screen::tests_line`), so a pass says how many tests it is a pass of:

```text
blaze: tests ok selected=9 ran=8 passed=8 failed=0 ignored=1 measured=0 filtered_out=0 unreached=0 binaries=3 unbuilt=0 elapsed_ms=31800
```

## How it is built

blaze-screen is the core (`#![blaze::sans_io]`, `no_std`): `Screen::take(Event)`,
`advance_clock(ms)`, `frame() -> Option<Frame>`, `next_deadline()`. An `Event` is the build's
model as the daemon says it (`blaze_protocol::now::Now`), a unit started, lines that stay
(a diagnostic drawn for this client, `serve/unit/drawn.rs`), what the build waits on, the
build's end (`Built`) and its why, a test's start and end, the terminal resized, or `Clear`
(what follows owns the terminal). A `Frame` is data: rows to erase, lines appended, the
region, each span with its role and its link, the progress report. `encode` is the only place
a frame becomes bytes, for one terminal's capabilities. Nothing of the terminal reaches the
daemon beyond what a diagnostic is drawn with, which no key holds.

The driver is `crates/blaze-cargo/src/cli/show.rs`: it reads the clock and the size, writes
the bytes, and on a terminal runs on a thread of its own so timers turn while the daemon says
nothing. `Printer::clear` is the one call for "the build is over": it returns once nothing
live is on the terminal. While a region is up the terminal does not echo typed keys (they
would land in it); a drop, a panic's unwind and an interrupt put the terminal back. The
thin `blaze` shows a no-op answer through the same core. `BLAZE_UI_TRANSCRIPT=<file>` records
the events, one a line as JSON; the tests replay such transcripts into a reference terminal.
