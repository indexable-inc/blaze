---
status: current
summary: rustc's own test suites under blaze: which suites run and why, what is compared, the pinned corpus, the ratchet.
---
# rustc's tests under blaze

`compiletest` (`scripts/src/bin/compiletest.rs`, modules in `scripts/src/compiletest/`) runs the
suites of rust-lang/rust's `tests/` that state something blaze must also do, at the commit of
blaze's nightly. A test's directives say what is right (accept or refuse, an error on this line,
the program exits 0); that is what is held. What rustc printed is never compared: no `.stderr`
snapshot, no message text (rules `superset-of-rust`, `useful-not-parity`). rustc is never run.

    compiletest '{"command": "fetch", "corpus": DIR}'
    compiletest '{"command": "list", "corpus": DIR}'
    compiletest '{"command": "run", "corpus": DIR, "blaze": BLAZE, "work": DIR, "expectations": "check", "out": FILE}'

Its fields are in the script's usage text. While cargo builds blaze it is built with
`cargo +nightly-<date> build --release -p scripts --bin compiletest`, and its own tests run with
`cargo +nightly-<date> test --release -p scripts --bin compiletest`.

## The suites

| Suite | Run | Why |
|---|---|---|
| `tests/ui` | yes | The language's behaviour, a test a claim: this is accepted, this is refused here, this program prints that. |
| `tests/incremental` | yes | Revisions of one source built on kept state: what blaze's edit loop is. |
| `tests/crashes` | yes, inverted | Each input crashes rustc. blaze may accept or refuse it; it may not panic, hang or crash. |
| `tests/run-make`, `run-make-cargo`, `build-std` | no | They drive rustc's and cargo's command lines. What has a blaze equivalent worth writing by hand: linking against a C library and being linked from C (`blaze-link`'s own tests), symbol visibility of a `cdylib`, reproducible output (`harness/v2/determinism.py` has it), `--remap-path-prefix`, and a build of std from source, which every blaze build already is. |
| `tests/codegen-llvm`, `assembly-llvm` | no | They pin LLVM IR and assembly rustc emits. Interesting when blaze promises a code shape (a niche check elided, a call inlined): then as blaze's own FileCheck-style tests over its LIR. |
| `tests/mir-opt` | no | They pin rustc's MIR after each of its passes; blaze's MIR and passes are its own (docs/mir-passes.md), proved against the Lean semantics. |
| `tests/codegen-units` | no | rustc's partitioning into codegen units; blaze compiles a function at a time. |
| `tests/pretty` | no | rustc's AST printer. `blaze fmt` has its own tests. |
| `tests/coverage`, `coverage-run-rustdoc` | no | `-C instrument-coverage`, which blaze does not have. They become the suite to run the day it does: the expectation is which lines ran, not rustc's output. |
| `tests/rustdoc*` | no | rustdoc's HTML and JSON; `blaze doc` makes its own pages. |
| `tests/ui-fulldeps` | no | Tests of rustc's own crates as libraries (`rustc_private`). |
| `tests/debuginfo` | later | A debugger over blaze's debug info is what users do, and the expectations are values printed by gdb and lldb, not rustc's output. It needs: gdb and lldb on the runner, the `gdb-command`/`gdb-check` and `lldb-*` directives, `-g` builds run under the debugger in the sandbox, and rustc's pretty printers (`src/etc`) or blaze's own. `harness/debuginfo.py` and `realdebug.py` cover a little of it today. |
| `library/coretests`, `alloctests`, std's own tests | not yet | Not compiletest, but the same kind of evidence, and nothing runs them: no harness, workflow or ledger row builds them. They need `blaze test` of a package that is part of the standard library's own workspace. |

## What a test asks, and what is held

The directive parser (`directives.rs`) and the plan (`plan.rs`) are pure functions with tests.
Every directive is in one table of `plan.rs`: it has a meaning here, it changes nothing blaze
does (how rustc's output is normalized), it makes the test not for this target (`ignored`), or
it has no blaze meaning yet (`unsupported`, naming the directive or flag). One in no table is
`unknown directive`, and the test is not run. Nothing is skipped quietly: the report counts each.

- **Mode**: `check-pass` is `blaze check`; `build-pass` is `blaze build`; `run-pass`, `run-fail`
  and `run-crash` build and run the program; `check-fail` must be refused by `blaze check`;
  `build-fail`, and a ui test with no mode, by the check or else by the build.
- **Annotations**: for `//~ ERROR` and `//~ WARN` (with `^`, `v`, `|`, `?` and `[revision]`),
  blaze must report a diagnostic of that level on that line, with the error code when the
  annotation carries one. The message is rustc's wording and is not compared. `NOTE`, `HELP` and
  `SUGGESTION` are not held, and a pass whose test has any says so (`not_held`).
- **The program**: it runs in blaze's own sandbox (`blaze sandbox`: the machine read-only, no
  network, writes kept to the run). Held: how it ends, `check-run-results`' `.run.stdout`, and
  for a failing program its `error-pattern` (the program's own words).
- **Flags**: `--cfg`, `--check-cfg`, lint levels, `--edition`, `--crate-type`, `--crate-name`,
  `-O`, `-g`, `-C opt-level|debuginfo|debug-assertions|overflow-checks|panic` are the package's
  manifest and profile. Flags that only word rustc's output are dropped. Any other makes the
  test `unsupported: flag <name>`.
- **Auxiliaries** (`aux-build`, `aux-crate`, `proc-macro`) are packages the test depends on.
- **Revisions**: a ui revision is a test of its own (`path.rs#rev`) with `--cfg rev`. An
  incremental test's revisions are built in order in one directory on one daemon, each by its
  name's mode (`rpass2`, `cfail3`).
- **`known-bug`** tests and `tests/crashes` state rustc's bug, not an expectation: they only
  have to be survived.
- **`#[rustc_clean]`** (`clean.rs`): on a function with no `except`, its code must not be made
  again in that revision (`blaze why --all --json`); with an `except` naming `optimized_mir` it
  is expected to be, which is reported and not failed (other source can give the same code).
  Every other form names rustc's queries or codegen units and is counted as not held. The
  attributes themselves are written over with spaces in the copy blaze builds.

A test is `pass`, `fail` (a short cause shared by every test that fails the same way, and its
first difference), `panic` (blaze's own, or its client ended by a signal), `timeout`,
`unsupported` or `ignored`.

## The corpus

`harness/compiletest/corpus.json` pins it: the Rust project's source archive of blaze's nightly
(`library/UPSTREAM`'s commit) by sha256. `fetch` downloads it, refuses other bytes, and unpacks
only the pinned paths; `run` refuses a directory whose marker is not the pin. The toolchain bump
moves the pin with the nightly. The corpus is untrusted data: blaze compiles it (proc macros in
the wasm sandbox) and its programs run only in the sandbox.

## The ratchet

`harness/compiletest/<suite>.toml` lists the tests that do not pass, grouped by cause, and how
many tests the suite had and ran when it was recorded. With `"expectations": "check"` the run
fails when a test that is not listed does not pass, when a listed test passes, no longer runs or
is gone (take its row out), and when fewer tests ran than were recorded. A run of a part
(`only`, `stride`) must say `min_ran`. `"expectations": "record"` writes the files from a run.
The ledger rows are U1 (ui), U2 (incremental), U3 (crashes) and U4 (the standard library's own tests) in
`docs/v2/plan/objectives.json`.

The older importer of `tests/incremental` in the revision tests
(`crates/blaze-engine/tests/revisions/rustc.rs`) is replaced by this for the corpus; the
revision tests' own cases and oracle are not.
