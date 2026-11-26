---
status: current
summary: "blaze-shell: blaze's own shell. A nushell-like language (`crates/blaze-sh`, `bstd::sh`: structured values through pipelines, O(1) CoW forkable state, sans-IO, `no_std`, on bstd's owning types) and a native macOS app (`crates/blaze-shell`) that draws values with blaze's own renderer (Metal, blaze-ui's Rust-authored shaders), not a TUI. External programs run on a PTY inside an output block through blaze-vt; full-screen programs take the window. The prompt is Rust in-process and `rust { ... }` / `:rs` cells execute live via `blaze run`."
date: 2026-11-24
---
# sh: blaze's own shell

Why: a prompt that forks starship, a shell that forks git, a terminal that draws text cells: each
layer is a process and a byte stream. blaze-shell is one process: the language, the prompt and the
renderer share memory, and output is values drawn as values (tables, records, sizes, errors with
spans), not text re-parsed by eye. It is nushell's model, owned: no nushell, no reedline, no TUI.

## Shape

```
keys ─▶ editor (blaze-shell) ─▶ source ─▶ blaze-sh: lex ▸ parse ▸ eval ─▶ Value
                                               │ needs IO?
                                               ▼
                                     Request (spawn, read dir, stat, read file, cd, env…)
                                               │ the app performs it, answers
                                               ▼
                                     Machine::answer(..) ─▶ eval continues
Value ─▶ view (table / record / list / scalar / error / terminal block) ─▶ blaze-ui draw lists ─▶ Metal
```

- **`crates/blaze-sh`** (`bstd::sh`, `#![no_std]`, `#![blaze::sans_io]`, `forbid(unsafe_code)`,
  blaze-own allocators). A `Machine` runs one submitted line: `Machine::step() -> Step` returns
  `Step::Done(Value)`, `Step::Need(Request)` (the host answers with `Machine::answer(Answer)`), or
  `Step::Stream(Chunk)` for a pipeline stage that produces incrementally (an external program's
  bytes, `ls` of a big directory). No clock, file, process or environment read happens inside it:
  `now`, `ls`, `open`, `cd`, `$env` reads and external commands are requests.
- **`crates/blaze-shell`** (the app; the only crate that touches AppKit, Metal, PTYs, the file
  system). It owns the history of blocks (one per submitted line: the source, the prompt it was
  run under, its value or its terminal), lays them out in a scrolling column and draws them.

## The language (nushell-like, our own)

Values: `nothing`, `bool`, `int` (i64), `float`, `string`, `size` (bytes; `10kb`, `3MiB`),
`duration` (ns; `5sec`, `2hr`), `date` (UTC ns + offset), `range` (`1..10`, `1..<10`, `1..2..10`),
`list`, `record` (ordered), `table` (a list of records, held columnar when the keys agree),
`closure` (`{|x| $x + 1}`), `binary`, `cell-path` (`$x.a.0.b`), `error` (message, span, help).

Syntax, as nushell's where it is the same idea:
- pipelines `a | b | c`; `;` and newlines separate; `$in` is the input.
- `let x = …`, `mut x = …`, `$x = …`; `def name [a: int, --flag(-f)] { … }`.
- `if … { } else { }`, `match`, `for x in … { }`, `while`, `loop`, `break`, `continue`, `return`,
  `try { } catch {|e| }`.
- literals: numbers with units, `"interp ($x)"` strings (`$"…"` in nushell; here every
  double-quoted string interpolates `($expr)`), `'raw'`, `r#'…'#`, `[1 2 3]`, `{a: 1}`,
  `[[a b]; [1 2] [3 4]]`.
- operators: arithmetic, comparison, `and or not xor`, `=~ !~`, `in`, `starts-with`, `ends-with`,
  `++`, ranges, `?.` optional cell paths.
- external commands: a word that is not a builtin or `def` runs as a program (`^git` forces it);
  its stdout becomes a byte stream (a string to the next stage, or a terminal block when it is
  the last stage and stdout is the screen).

Builtins (first set, the ones a day at a prompt uses): `ls`, `cd`, `pwd`, `open` (json, toml,
csv by extension, through bstd's parsers), `save`, `cp`, `mv`, `rm`, `mkdir`, `touch`, `which`,
`ps`, `sys`, `env`/`$env`, `where`, `select`, `reject`, `get`, `sort-by`, `reverse`, `first`,
`last`, `skip`, `take`, `uniq`, `length`, `each`, `filter`, `reduce`, `group-by`, `flatten`,
`insert`, `update`, `upsert`, `rename`, `enumerate`, `transpose`, `math sum/avg/min/max`,
`str` (`trim`, `replace`, `split`, `join`, `upcase`, `downcase`, `contains`, `length`),
`lines`, `split row/column`, `parse "{a} {b}"`, `into int/float/string/size/datetime`,
`to json/csv/text`, `from json/csv/toml`, `describe`, `echo`, `print`, `do`, `ignore`, `help`.

Errors carry spans into the source line; the app underlines them.

## Engine state forks in O(1)

The engine state (variables, `def`s, aliases, env, cwd, the module scope) is persistent: every
map and list in it is shared structure (`blaze_own::Rc` nodes with copy-on-write through
`Rc::make_mut`; maps as hash-array-mapped tries, lists as chunked trees), so `state.clone()` is
a pointer copy and a write copies only the path it touches. Values are the same: a big table or
string passed down a pipeline or bound to a variable is shared, never deep-copied. What this buys:

- **Fork anywhere.** Each block keeps the state it ran in. Running a line "from" an earlier
  block forks that block's state; both branches live side by side in the history, each with its
  own later blocks (a tree of sessions, not a line).
- **Try without commit.** A line runs on a fork; its delta is merged back only when it
  succeeds (nushell's `StateDelta`, made free).
- **Parallel stages.** `par-each` and background jobs read a snapshot without locks.

## Rust cells

`rust { … }` (or a `:rs` line) is a cell of Rust compiled by the blaze daemon against a session
crate and loaded into the running app with blaze's live patching (the path harness-native runs
on, `blaze run -p harness-native`). `$in` arrives as a typed argument (`Value` converts to Rust
types: a table to a `Vec` of records, a string to a `String`), the cell's result converts back
to a `Value` and is drawn like any other. Definitions from earlier cells live in the session
crate, so later cells see them; a fork of the shell state forks the cell list it compiles. It
needs one thing blaze does not have yet: a daemon entry point that compiles a snippet against a
live session and hands back a loadable unit. That is written up as its own design before it is
built (edition.md, "Why it also gives a REPL").

## The app

- **Blocks.** One per submitted line, in `bstd::ecs` (blaze-ecs): `Block { source, prompt, cwd,
  started, finished, status }` with one of `Value`, `Terminal(blaze_vt::Terminal)` or
  `Error`. History scrolls; a block's table folds after N rows with a "show all".
- **Views.** `table`: a header row, columns sized to content (capped), numbers right-aligned,
  sizes and durations formatted, alternate row tint, nested values as `[list 3]`/`{record 2}`
  that expand on click. `record`: key/value two columns. `list` of scalars: one column with
  indices. `string`: text, wrapped. `error`: the source line with the span underlined and the
  message. `terminal`: blaze-vt's screen drawn in the block; a program that enters the
  alternate screen (vim, htop, less) takes the whole window until it leaves.
- **Editor.** The input line is the app's own: cursor, selection, history (↑/↓, ctrl-r search),
  completion of commands, paths and flags from the language's own tables, syntax colour from
  the lexer.
- **Prompt.** Computed in Rust before each line: cwd (home as `~`), git branch from
  `.git/HEAD` read directly (no process), duration and status of the last line. Git dirtiness
  from a background job (gitoxide or a status read on a worker thread), the prompt redrawn when
  it lands; never blocks a keystroke.
- **Drawing.** blaze-ui's Rust-authored shaders (`ui_sdf_*`, `ui_glyph_*`, emitted as MSL), the
  glyph atlas rasterised by CoreText (a BZG1/Slug path later, when blaze has a font encoder).

## Lanes

| lane | files | needs |
|---|---|---|
| sh-core | `crates/blaze-sh/src/{value,span,lex,parse,ast,eval,machine,error}.rs` | this doc |
| sh-builtins | `crates/blaze-sh/src/builtins/*` | sh-core's `Value`, `Machine`, `Request` |
| app-views | `crates/blaze-shell/src/{view,layout,blocks,editor,prompt}.rs` | `Value` (value.rs, frozen first) |
| app-host | `crates/blaze-shell/src/{host,pty,main}.rs`: performs requests, runs externals into blaze-vt blocks | `Request`/`Answer` |

`value.rs` and the `Request`/`Answer`/`Step` types are written first and frozen, so the lanes
build against them in parallel.

## Tests

- blaze-sh: every builtin and syntax form has tests as `(source, value)` pairs; the machine is
  driven by a scripted host in tests (a fake file system and process table), so every request
  path is tested without IO. A `blaze_sim` property: any interleaving of answers in the order
  requests were made gives the same value as answering at once.
- The app: views tested as layout (block → rows of cells with styles) without a GPU; one
  screenshot check on the Mac.
