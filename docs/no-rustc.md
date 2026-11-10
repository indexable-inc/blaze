---
status: current
summary: The no-rustc rule, what counts as running rustc, the end state, and the live list of the places that still name rustc or cargo.
---
# blaze never runs rustc

## The rule

blaze compiles everything itself. When someone builds, runs, tests or checks a program with
blaze, no rustc runs: not for a function, a crate, the standard library, a build script, a proc
macro, a check, a test, a doctest or a link, and not through cargo either. There is no path that
falls back to cargo or rustc when blaze cannot do something: blaze says what it could not do, the
build fails, and that failure is a bug to fix (AGENTS.md, rules `no-rustc` and `no-fallback`).
The same holds for building blaze itself (rule `blaze-builds-blaze`).

Andrew, 2026-10-22: "Everything should all be us 100%."

## What counts as running rustc

Any of these, whoever starts the process:

- blaze starting `rustc`, `rustdoc` or another rustc driver, including to ask it something
  (`-vV`, `--print cfg`, `--print sysroot`) or to assemble an `asm!` block;
- blaze starting a cargo command that runs rustc (`cargo build`, `check`, `test`, `rustc`, and
  `metadata` without `--no-deps`: cargo runs `$RUSTC` to learn about the target too);
- a process blaze runs that runs rustc in turn: a build script that probes `$RUSTC` (`autocfg`,
  `version_check`, `rustversion`), unless `$RUSTC` is blaze;
- blaze using what rustc wrote, even when rustc ran earlier or on another machine: the sysroot's
  rlibs, objects cargo had rustc build, rustc's metadata.

A blaze extension is not a rustc run: code that opts into one need not build under rustc.

## The end state

1. **The standard library is compiled by blaze**, from `library/`, kept in the store per target
   and profile like any other crate. No rlib of a sysroot is read or linked.
2. **Proc macros are compiled by blaze and run by blaze**, as `wasm32-wasip1` in Wasmtime
   (`crates/blaze-macro-eval`).
3. **Build scripts are compiled, linked and run by blaze.** `RUSTC` names blaze while they run
   (`crates/blaze-engine/src/as_rustc.rs`), so their probes are answered by blaze, from the data
   `harness/answers.py` writes at a toolchain bump (`crates/blaze-cargo/src/answers/own.rs`).
4. **The cold build is entirely blaze's**: no first build by cargo and rustc.
5. **Tests and doctests are built by the daemon**; no rustdoc runs.
6. **No wrapper and no fallback**: there is no `blaze-rustc`, and no build is handed to cargo.

## The plan

The steps, by the letter code and docs cite them with (issues #81 to #93). Whether each has
landed in full has no check yet (objective C5, `docs/v2/plan/objectives.md`); what the counter
below still finds is the list after it.

| Step | What it removed |
|---|---|
| A | nothing: every rustc run counted, new ones refused (`harness/rustcsites.py`, `harness/norustc.py`) |
| B | every question asked of rustc (`-vV`, `--print cfg`): blaze answers |
| C | the assembler run for `asm!` |
| D | the fallback on an error: blaze's verdict is the final word |
| E | the functions left to rustc: a refusal fails the build |
| F | the sysroot's rlibs: the standard library compiled by blaze |
| G | rustc's rlibs, objects and link line: every crate compiled and linked by blaze |
| H | the test harness written by rustc |
| I | build scripts compiled and run by rustc |
| J | proc macros built by rustc and the toolchain's proc-macro server |
| K | cargo's first build |
| L | doctests by rustc and rustdoc |
| M | the wrapper and the fallback |

## What is left

`python3 harness/rustcsites.py` (CI's `scripts` job) counts, per file under `crates/`, the places
that start rustc, rustdoc, cargo or rustup or read which one to start, and holds them to
`harness/norustc/sites.json`: one more in a file fails, and a place that goes lowers the count
(`--record`). On 2026-11-10 it counts three places in two files, and none of them starts rustc:

| Place | What it is | Why it is still counted | What removes it |
|---|---|---|---|
| `crates/blaze-build-script/src/elide/tests.rs`, 1 | the text of rust-analyzer's proc-macro-srv build script, a test's fixture of a script that runs `$RUSTC --version`; the test checks blaze does not elide it | the fixture is an escaped string on one line of code, which the counter reads as code | writing the fixture as a raw string over lines, which the counter knows is data |
| `crates/blaze-cargo/src/answers/own.rs`, 2 | a unit test that makes two `Command::new("rustc")` to read their environment (`Vars::Command`) and never spawns them | the counter matches the command's name | naming any other program in the test |

Two more places ask `$RUSTC` and are listed on every run, not counted, because the compiler
asked is blaze: `crates/blaze-codegen-code/src/lib.rs`'s `compiler`, a build script's `-vV` for
the code identity, under an `// asks blaze:` comment (end state 3).

Outside `crates/`, which the counter does not read, cargo or rustc still run in:

| Place | What runs | What removes it |
|---|---|---|
| `harness/v2/engine.py`, `reference_env` and `cargo` | plain cargo with the pinned nightly, to fetch sources for the proof scripts (`cargo fetch`, `cargo vendor`, `cargo metadata`); nothing it builds is compared | blaze's own `fetch` and `vendor` as commands (`crates/blaze-metadata` has both; `metadata` is wired) |
| `.github/workflows/nix-live.yaml`, the demo jobs | `cargo build --locked` of `demo/life`, on purpose: it checks the demo builds with rustc as a user's would | deleting the flake and that workflow (decided 2026-11-04) |
| `tools/nix/unit_graph.py` and `tools/nix/unit_build.py` | cargo's unit graph, and one rustc per unit in the flake's per-unit builds | the same deletion |

`harness/norustc.py` (the release train) measures the same thing from outside: how often rustc
and rustdoc run when `harness/norustc/fixture` is built, edited, tested, doctested and checked
with blaze, which has to be never.

The long form of this page (the measurements of October 2026, each step's design and the lanes
that did them) is in git: `git show 9112d5e40e:docs/no-rustc.md`.
