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
2. **Proc macros and pure-Rust build scripts are compiled by blaze and run by blaze**, as wasm64
   (memory64) modules in-process under Wasmtime, sandboxed (`crates/blaze-proc-macro`). Native
   tools and C-linking build scripts go to the hypervisor micro-VM sandbox, not built yet.
3. **Build scripts are run by blaze.** `RUSTC` is answered by blaze while they run
   (`crates/blaze-build-script`), so their probes are answered by blaze, from the data
   `harness/answers.py` writes at a toolchain bump (`pins/answers/rustc.json`, read by `crates/blaze-pins`).
4. **The cold build is entirely blaze's**: no first build by cargo and rustc.
5. **Tests and doctests are built by blaze**; no rustdoc runs.
6. **No wrapper and no fallback**: there is no `blaze-rustc`, and no build is handed to cargo.

## The plan

The steps, by the letter code and docs cite them with (issues #81 to #93). Whether each has
landed in full has no check yet (objective C5, `docs/v2/plan/objectives.md`); what the counter
below still finds is the list after it.

| Step | What it removed |
|---|---|
| A | nothing: every rustc run counted, new ones refused (`scripts/src/bin/rustcsites.rs`) |
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

`scripts/src/bin/rustcsites.rs` (the gate's `rustc sites` row) counts, per file under `crates/`,
the places that start rustc, rustdoc, cargo or rustup or read which one to start, and holds them
to the counts in `harness/norustc/sites.json`: one more in a file fails, and a place that goes
lowers the count (`--record`). It holds eight places in six files: `crates/blaze-bstd-convert/src/tests.rs`
2, `crates/blaze-proc-macro/src/` `comptime_patch.rs` 1, `native_bridge.rs` 2 and
`native_script.rs` 1, `crates/blaze-std/build.rs` 1 and `crates/blaze-std/tests/refusals.rs` 1.

Outside `crates/`, which the counter does not read, cargo or rustc still run in:

| Place | What runs | What removes it |
|---|---|---|
| `tools/nix/unit_graph.py` and `tools/nix/unit_build.py` | cargo's unit graph, and one rustc per unit in the flake's per-unit builds | deleting the flake (decided 2026-11-04) |
| `blaze.yaml`'s `cargo` job, `.github/actions/script/build.sh`, `land.py`'s smoke line | `cargo check`, the unit tests and the scripts' build with the pinned nightly | blaze building, checking and testing blaze there too (a gap) |

The long form of this page (the measurements of October 2026, each step's design and the lanes
that did them) is in git: `git show 9112d5e40e:docs/no-rustc.md`.
