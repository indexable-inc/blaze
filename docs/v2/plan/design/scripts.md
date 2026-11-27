---
status: plan
summary: Python leaves the repository: Rust scripts run by blaze (`blaze run script.rs`, bstd, typed input, stage 0) replace every remaining `.py` file, in two tiers, with the order of the ports, the guard that the list only shrinks, and each port's parity check.
date: 2026-11-11
---
# Scripts in Rust, run by blaze

Python leaves the repository (Andrew, 2026-11-10). First what no longer runs is deleted; then
every remaining script becomes a single-file Rust script that blaze builds and runs.

What ships to a browser, a Worker or a user's shell (the site, its worker, the app's UI,
`site/install.sh`) stays in its language; a repository script in another language follows the
Python.

## The design

Rust replaces Python for every script, in two tiers, because of one rule.

### Stage 0 for scripts (the bootstrap rule)

A script that checks, tests, lands or releases blaze is built and run by the pinned released
blaze (`.github/blaze-version`, the stage 0 that builds blaze), never by the tree under test: a
tree that does not compile must still be checked, landed on and released from. So:

- A script uses only the language and libraries that release has. One that needs a newer
  feature waits one release: the feature lands, a release is cut and pinned, then the script
  uses it. CI runs every script with the stage-0 binary, as it builds blaze with it: built by
  it once per change of their inputs and kept in the Actions cache under a key of all of them
  (`.github/actions/script`).
- A script builds from its own file and its helper crate alone, never from blaze's crates: the
  helper (`crates/blaze-script`) depends on no compiler crate, so a broken one cannot stop `land` or the gate's checkers
  from building. A script that measures the tree's blaze takes it as data (the `engine` field
  of its input, as harness/v2 does now) and starts it as a program.
- The helper crate is held to the same rule as the scripts: it must build under stage 0. Its
  own change is checked by stage 0 building every script (the gate's `scripts` job).
- `land.py`'s successor is the `land` script: stage 0 builds it from `scripts/` and
  `crates/blaze-script`, both of which build whatever state main's compiler is in.

### Tier 0: now, on the released blaze

Plain Rust on `std` plus `crates/blaze-script`, as ordinary binaries of one workspace package,
`scripts/` (`scripts/src/bin/<name>.rs`, one self-contained file a script), run as

    blaze run -p scripts --bin docs -- check

Stage 0 builds that today. `blaze-script` is small and has what the Python uses (imports
counted over 192 files on 2026-11-11): the typed input (`typed_input`, 80 files),
JSON (92), processes with captured output and exit status (`subprocess`, `asyncio`; 96),
files, directories and temporary directories (`os`, `pathlib`, `shutil`, `tempfile`), regular
expressions (59), TOML reading (14), SHA-256 and BLAKE3 (31), tar (7), HTTP GET and PUT (12,
the release uploads, done by starting `curl` as the Python's `gh` and `aws` calls are started),
time, N jobs at once (`concurrent`, 13), and the table and report printer (`report`). JSON is
`serde_json`; TOML and regex are blaze-script's own (`toml.rs`, `regex.rs`).

### Tier 1: the long-term form

    #!/usr/bin/env blaze
    ---
    [dependencies]
    blaze-script = { path = "../crates/blaze-script" }
    [package.metadata.blaze]
    experimental = ["capabilities"]
    ---
    /// Checks every doc's front matter and writes the index.
    struct Input { /** check or write */ command: Command }

    fn main(cx: &Cx, out: Out, checkout: Dir<ReadWrite>, input: Input) -> Result<(), Error> { .. }

- **Single file.** `blaze run path/to/script.rs`, `blaze path/to/script.rs` and the shebang
  line. The file is a crate root. Its manifest is cargo's `-Zscript` front matter, unchanged, so
  editors and cargo read it (rule useful-not-parity: no syntax of blaze's own); blaze's gates
  are the `[package.metadata.blaze]` keys a `Cargo.toml` already takes. What the tree has: the
  lexer and parser accept the shebang and the front matter (`crates/parser/src/frontmatter.rs`,
  cargo's parser copied; the `frontmatter` feature gate is diagnosed), and the command line
  finds an embedded manifest (`crates/blaze-cargo/src/cli/place.rs`) but only under `-Zscript`,
  after which `crates/blaze-manifest/src/manifest.rs` refuses it ("requires `-Zscript`"):
  there is no manifest synthesis and no `blaze run file.rs`. To build: the manifest from the
  front matter with cargo's defaults (name from the file, one `bin` at the file, no build
  script, no auto targets, a workspace of its own), on by default with no flag; the script's
  build directory and lock under blaze's home, keyed by the file's path; the unit keyed as any
  unit is, by the file's content and its dependency set, so a second run is a no-op build and
  an exec.
- **Where scripts live.** `scripts/`, one file each, for what a workflow, the gate or a
  person runs; a generator stays beside the file it generates. Shared code is the helper crate,
  a path dependency: no `sys.path`.
- **Fast start.** The budget: a warm script starts within 5 ms of the program's own start, and
  cold is one compile of the file against dependencies already built. Today a no-op `blaze
  build` is 38 to 136 ms, of which the daemon's answer is 0.1 to 0.4 ms: the rest is
  the `blaze` process. What takes it away is specified in docs/v2/plan/design/build-scripts.md by the edit-speed lane
  (the command asks the daemon before it sets anything up; build on save; D34, D35)
  and docs/client.md (`blaze-fast`), and is not repeated here: `blaze run script.rs` is the
  same no-op lane followed by an exec of the kept executable, with no second process between.
  With no daemon running the command starts one, as every build does, and the first script pays
  for it; there is no in-process route (rule no-fallback).
- **bstd is the library.** `main` gets the task's `Cx` and each grant it names (D104: a whole
  `Io` cannot be asked for: `checkout: Dir<ReadWrite>`, `env: Env`, ...) and narrows them
  (`checkout.subdir(..)`, `env.only(..)`), so a script's signature says what it can touch.
  Against bstd on `lang-int-1110` (docs/v2/plan/design/std.md there, and
  `crates/blaze-std/src`):

  | need | bstd has | missing, for the bstd lane |
  |---|---|---|
  | files, directories | `fs` (`Dir`, `File`, `list`, `stat`, `read_all`, `publish`, `remove`, `rename_replacing`), `path` | a recursive walk; a glob; removing a tree; a temporary directory that removes itself |
  | environment, arguments, exit | `env`, `args`, `host` | |
  | processes | `procs` lists and signals, in the simulation only | running a program with captured output and its exit status: std.md's `Sandboxes` and `OsSandbox` are designed, no module is written |
  | JSON read and write | none (`data` is designed on reflection) | `bstd::data` with JSON; until reflection phase 2, a JSON value tree |
  | TOML read | none | a TOML reader (blaze-manifest's, moved) |
  | regular expressions | none | `bstd::text::regex`, or the `regex` crate re-exported |
  | hashing | `digest` (BLAKE3, SHA-256) | |
  | tar, zip, xz | `image/zlib` inflates, for PNG | tar read and write, gzip and xz streams (zip is not used) |
  | HTTP GET and PUT | `requests` (`Http`), `http` (h1 core), `net`, `roots`, `url` | a native provider of `Io::http` (the simulation's alone exists); TLS |
  | time | `clock` | wall-clock dates formatted (ISO 8601) |
  | N jobs at once | `task` (scopes, deadlines) | |
  | a table and report printer | `report`, `term` | a plain column table |

- **The typed input.** `bstd::input`: the script's entry parses its one argument (a JSON value
  inline, the path of a file, or `-`) into `main`'s third parameter and refuses what
  `typed_input` refuses, in its words, naming the field: `lean_gate: no field rot (the fields
  are command, lake, out, packages, root)`, `lean_gate: field command is required`, `lean_gate:
  command is one of "check", "sources", "write", not "chek"`, exit status 1. `selftest` as the one argument runs the
  script's `fn selftest(cx, io)`, as now. `--help` prints the struct's and each field's doc
  comment with its type and default. The decoder and the help are derived from the struct by
  reflection, no derive macro: reflection phase 2 (the field loop of `blaze_comptime`,
  docs/v2/plan/design/comptime.md on `lang-int-1110`) and `bstd::data` (reflect.md item 11),
  plus doc comments in a field's reflected data, which phase 2 does not list. The interim form
  is tier 0's: a hand-written `Input::parse` over `blaze_script::input`'s field reader, which
  gives the same errors.

Tier 1 waits on, each in a release: single-file scripts; bstd with a native `Io` and
the missing pieces above; reflection phase 2 with `bstd::data`; the edition's
`fn main(cx, io)` entry. A tier 0 script moves by the lints that move blaze's own crates
(`blaze::std_use`'s renames, `blaze check --fix`), not by hand.

### The engine and the report (harness/v2's shared modules)

`crates/blaze-script`'s `engine` (the blaze under test) and `report` (a run's result) are
harness/v2's `engine.py` and `report.py` ported whole: every name keeps its
meaning and every message its words, so a harness ports line by line.

- **Types.** `EngineSpec` (the `engine` field: `kind`, `blaze`, `backend`, `dev_backend`, read
  through `Input`) and `Engine`, made by `Engine::of(&spec)`, which runs the preflight. Python's
  two exceptions are one error, `Fault::{Failed, Unmeasurable}`, and every fallible method
  returns `Result<T, Fault>`. `Ran` (argv, seconds, status, stdout, stderr), `Work`, `Table`,
  `Mark` and storelib's `Layout` keep their fields. `report` has `Verdict`, `Number` (int,
  float, str, bool or none), `Outcome` and `Report` with `add`, `guarded`, `failing` and
  `finish`.
- **A harness** is a bin of `scripts/` like any other: an `Input` struct with
  `engine: EngineSpec` and `out`, then `main` does
  `let eng = Engine::of(&args.engine).unwrap_or_else(|f| fail(SCRIPT, f.said()))`, a
  `Report::new(SCRIPT, &args.engine, &args.out)`, one `rep.add(...)` per thing checked, and ends
  with `rep.finish()`. A step that may fail is a closure returning `Result<_, Fault>`, handed to
  `rep.guarded(name, ...)` as Python's `guarded` takes a callable.
- **Fan-out.** `ThreadPoolExecutor(jobs)` and asyncio's tasks under a semaphore are both
  "N at once, results in input order": `blaze_script::fan_out(jobs, items, f)` runs `f` on `jobs`
  named threads, each taking the next item, and returns the results in the items' order. A
  subprocess with a deadline (`asyncio.timeout`) is `engine::run` with its timeout, blocking its
  thread. `Engine`'s own records (what each build did, by home) sit behind a lock, so the
  threads share one engine as the Python's do under the GIL.
- **Byte compatibility while Python harnesses remain.** The result file is the same JSON:
  `dumps_indent(1)` with the same keys in the same order (`script`, `engine` as `asdict` writes
  it, `host`, `commit`, `started`, `ended`, `outcomes`, `counts` in first-seen order, `extra`),
  a float written as `repr` writes it and `round(x, n)` done as Python does. So `proof.py`
  reads a Rust harness's result as it reads a Python one, and `SCRIPT` stays
  `<name>.py` until the last reader moves. The printed lines (`PASS       name  k=v  detail`,
  the closing count line) are the same, and so is the exit status. What cannot be the same is
  listed in a port's parity row: random temporary names, timings, and the
  `blake3`-hashing parts (`digest`, `digests`, `outputs`), which come with the first harness
  that uses them (`proof.py`).

### The order, and the guard

Nothing is ported one for one that can be deleted instead. Sizes are from 2026-11-11.

1. **Checkers** (13 files, 4,841 lines): `tools/docs.py`, harness/v2's `ledger` (deleted since), `invariants`,
   `guards`, `changed`, `diff_rules`, `lean_gate`, then `tools/v2/domains.py`,
   `harness/rustcsites.py`, `harness/failures.py`, `harness/imports.py`,
   `.github/scripts/gate_changes.py`, with `typed_input` itself. Pure functions over files; they
   need `blaze-script`'s input, JSON, TOML, regex and `git` runs. `tools/agents/rules.ts` goes
   with them.
2. **harness/v2's harnesses** (26 files, 19,177 lines): they need the engine handle
   (`engine.py`), the report (`report.py`) and process running with deadlines.
3. **The older harness** (83 files, 28,520 lines): each script is deleted, or becomes a row
   of a harness/v2 harness, before any is translated; `storelib` and `blaze_cli` (68 importers)
   are the helpers to settle first.
4. **tools/ and the generators** (28 files, 6,632 lines): each generator when its output is
   next regenerated.
5. **.github/scripts** (14 files, 3,914 lines) last: `land`, the release and the uploads, once
   a release has proved the tiers on everything before them.

A port keeps the Python's interface exactly (the one argument, `selftest`, the text printed,
the exit status), so a workflow or a gate row changes only its command. Ports land in
pairs: the Rust script beside the Python, then, once both were run on the same inputs and
agreed byte for byte, the switch that deletes the Python and its line of `tools/python-allowed.txt`.

**No new Python** (rule guard-the-cause): `tools/python-allowed.txt` lists every tracked `.py`
file. The `docs` script's `check` (`blaze run -q -p scripts --bin docs -- check`, on the pinned
release), which every gate runs, fails for a `.py` file that is not on it (write it in Rust)
and for a line whose file is gone (its `write` takes it out, and never adds one), so the list
only shrinks. Python is gone when `... --bin docs -- python '{"left": 0}'` passes: the
list is empty and no `.py` file is tracked; then the check holds that there is no `.py` file at
all.

## Ports and their parity

Tier 0 is in the tree: `crates/blaze-script` (the typed input with `tools/typed-input`'s
refusals word for word, JSON written as `json.dumps` writes it, a program run with its output
captured) and the package `scripts/`, both checked by stage 0 (`blaze check -p blaze-script -p
scripts`, `blaze test -p blaze-script`, on 0.20261105.4).

A ported script is switched to (its workflow step, gate row and docs changed, the Python and
its line of `tools/python-allowed.txt` deleted) only after both were run on the same tree and
agreed. `R` is `blaze run -p scripts --bin`; every command runs at the root of one checkout with
stage 0; "equal" is standard output, standard error and the exit status, byte for byte, unless
the row says otherwise. `state`: `written` (the Rust is in the tree, not yet compared), `agreed`
(compared, equal), `switched` (the Python is deleted, its callers run the Rust).

| script | state | Python | Rust | must be equal |
|---|---|---|---|---|
| lean_gate | written | `uv run --frozen python harness/v2/lean_gate.py selftest` | `R lean_gate -- selftest` | output (`lean_gate.py selftest: ok`), status 0 |
| lean_gate | written | `... lean_gate.py '{"command": "sources"}'`, and with `"out": "/tmp/lg-py.json"` | `R lean_gate -- <the same>` with `"out": "/tmp/lg-rs.json"` | clean tree: `lean_gate.py: ok: sources hold`, status 0, the two out files the same bytes. Planted one at a time: `sorry` in a proof of `lean/`; `-- sorry` (passes); a theorem of lean/theorems.json renamed; one listed twice; an `allow` entry whose file does not say its word; lean/analysis/lean-toolchain changed; `rev` in lean/analysis/lakefile.toml changed; the manifest deleted; docs/v2/plan/proofs.md edited: the same lines, status 1, and the out files the same bytes |
| lean_gate | written | `... lean_gate.py '{"command": "write"}'` | `R lean_gate -- '{"command": "write"}'` | `git status --short` empty after each on a clean tree; with a theorem's `says` changed, proofs.md the same bytes from both |
| lean_gate | written | `... lean_gate.py '{"command": "check", "out": "/tmp/lg-py.json"}'` (needs the pinned Lean under `~/.elan`) | `R lean_gate -- <the same>` with `/tmp/lg-rs.json` | the same `lake` commands in the same directories (the lines `lean_gate.py: \`<args>\` in <dir>: exit <n>, <s> s` but their seconds), the same table, final line and status; the out files the same bytes. `{"command": "check", "lake": "lake"}`: `mir_semantics.py: \`lake\` is \`lake\`: ...`, status 1, from both |
| nostd | written | `uv run --frozen python harness/v2/nostd.py selftest` | `R nostd -- selftest` | output (`nostd.py selftest: ok`), status 0 |
| nostd | written | `... nostd.py '{"command": "check"}'`, `"progress"`, `"list"`, and `"list"` with `"root": "."` | `R nostd -- <the same>` | output and status on a clean tree. Then with ban lists of the wrong shape (`banned` missing, `sys` a string) and one listing a path twice and a path that names nothing (`check`, `progress`); then planted together, a crate `crates/blaze-core-x` (after `blaze-core` by parts, before it by text) with a `std::{fs, time::Instant, net}` group, a CRLF file with a byte that is not UTF-8, a test module, a `tests/` file, `extern crate std` with `libc::`, and `extern crate std` under its `target/`: the same lines and status for `check`, `progress`, `list`. `{}`, `{"command": "lst"}`, `"bans": 3`, an unknown field, `[]`, no argument, two arguments: the same lines, status 1. A ban list that is not there: status 1 from both, the Python's a traceback |

Switched (the Python deleted, its callers on the Rust): docs, invariants, changed,
gate_changes, domains, rustcsites.

Known differences, on purpose: the words of a JSON syntax error are serde_json's, and a TOML
syntax error is in blaze-script's words. `nostd` ends with a line where the Python raised on a
file it could not read, a ban list that is not there, and a manifest or lock file that is not
TOML; its module pattern's lookahead is tried by hand after each candidate (no candidate can
start inside a failed one, so the counts agree).
