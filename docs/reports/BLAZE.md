---
status: record
summary: The early project plan and harness.
date: 2026-10-20
---
# blaze: project plan and harness

blaze is a build daemon that keeps a Rust workspace loaded in rust-analyzer's database and
compiles functions straight from RA's MIR with Cranelift. During the prototype rustc still
builds everything once, and it is the reference we test against. This file says what we are aiming for, how we know whether
we are getting there, and how the work is split up.

Current numbers live in harness/SCOREBOARD.md (since removed). They are generated; this
file should not repeat them.

## 1. Goal

Make the edit-to-running-code loop on a real Rust project take tens of milliseconds instead of
seconds, without ever running code that behaves differently from what rustc would have built.

The target is the Rust ecosystem in general, not any one project.

Not goals, at least for now:

- Release builds and optimised code. blaze produces debug-quality output.
- Diagnostics. Errors and borrow checking come from `cargo check` running beside the daemon.
- Hot patching. The output is a relinked binary.

Later, not now: producing `.rlib`/`.rmeta` so other crates can depend on blaze's output. Once
blaze compiles whole crates this becomes necessary.

## 2. The two rules

**Rule 1: rustc is the reference.** A function compiled by blaze must be observably the same as
rustc's version. When they differ, blaze is wrong.

**Rule 2: a skipped function is a bug.** The backend returns `unsupported` for anything it has no
explicit support for, and that function is left to rustc so the rest of the crate can still be
linked and tested. That mechanism is scaffolding. In the prototype phase we do not design around
it or count on it: the target for every crate on the ladder is 100% of functions compiled by
blaze, and the scoreboard's skip list is the bug list. Whether a production version keeps a
fallback is a later decision.

What the backend must never do is guess. Unknown input is an explicit skip (visible, counted),
not a best-effort lowering (invisible, wrong).

## 3. Metrics

Four numbers, in priority order. A change that improves a lower one while hurting a higher one
does not land.

| # | Metric | What it measures | How |
|---|---|---|---|
| 1 | Mismatches | Targets whose behaviour differs from rustc. Must be 0. | `harness/difftest.py` |
| 2 | Skips | Functions blaze did not compile, per crate, by reason. Target 0. | `harness/scoreboard.py` |
| 3 | Edit latency | Time from file save to runnable binary, split into re-analysis, codegen, link | `harness/bench.py`, `harness/editbench.py` |
| 4 | Footprint | Daemon RSS, cold load time | same |

Coverage is counted over functions rustc emitted a symbol for (the ones blaze can replace).
Generic and `#[inline]` functions have no symbol of their own; they are compiled as instances
when something reachable calls them and are reported separately.

Latency numbers are only comparable to cargo once a crate is at 100%. Before that blaze is doing
less work than cargo. Get things working first, then optimise.

## 4. The harness

### What exists

- `harness/difftest.py <workspace> --bin <pkg> --crate <name>...`
  Builds the binary with cargo, compiles the named crates with blaze, links blaze's objects in
  front of rustc's rlibs, runs both binaries and compares stdout, stderr and exit status.
  With `--bisect` it also links each blaze function in on its own and names the ones that change
  behaviour, with the first differing output line.
- `harness/corpus/`: a lib crate with one module per backend area (scalars, control flow,
  aggregates, enums, slices, constants, generics, closures, trait objects, heap) and a runner that
  calls every function and prints the result. This is the unit-test suite for the backend.
- `harness/scoreboard.py`: runs every target in `targets.json`, writes `scoreboard.json` and
  `SCOREBOARD.md`, fails on any mismatch or on coverage going down. `--update` accepts new numbers.
- `blaze --report <tsv>` (per-function outcome), `--deny <file>` (leave these functions to rustc),
  `--isolate <dir>` (one object per function).

- `harness/cratetest.py <crate-dir>`: the same comparison using a crate's own integration tests.
  Each test binary is relinked with blaze's objects and must report the same result per test.
  `vendor.sh` unpacks a crates.io release for it.
- `harness/editbench.py`: starts the daemon, flips a line back and forth, records the daemon's
  timings and RSS, then times cargo on the same edits. Writes `BENCH.md`.
- `harness/bench.py`: through `blaze` on pinned ripgrep, cargo and nushell, times a cold
  build, a build with nothing changed and four kinds of edit against cargo, with the daemon's
  peak memory and whether each edit was spliced. Writes JSON; `--compare BASELINE` fails over a
  threshold. Results are in `PERF.md`.
- `harness/fuzz/fuzz.py smoke` and `fuzz.py run --seed S --rounds N`: programs made by
  `harness/fuzz/gen.py` from a seed, built by cargo and by blaze under each backend, and run. A
  case whose output differs, a function left to rustc or a blaze crash is reduced and written to
  `harness/fuzz/found/`. What it has found is in [FUZZ.md](FUZZ.md).
- A Cranelift panic costs one function (reported as `backend panic`), not the run.

Bugs it has found so far:

- 16-byte aggregates were returned in registers; rustc returns them through a pointer. Found by
  the first real-crate run, then six corpus functions showed the same cause.
- RA lowered `<` on raw pointers to a `PartialOrd::lt` call that recursed into itself. Found by
  httparse's test suite. Fixed in RA's MIR lowering.
- The daemon rediscovered uncompilable instances over nine restart rounds on every edit. Found by
  the bench (blaze was slower than cargo on two of three crates).

### What to build next

1. **Panic and abort fixtures.** The corpus only has functions that return. Overflow, bounds
   checks, `unwrap` on `None` and explicit `panic!` need their own binaries, compared on exit
   status and panic message.
2. **Unit tests, not only integration tests.** `cratetest.py` uses `tests/*.rs`. Tests inside
   `src/` are compiled with `cfg(test)` into a different crate instance and need the symbol table
   pointed at that build.
3. **Layout agreement check.** Compare RA's layout for every type in a target against
   `rustc -Zprint-type-sizes` and count disagreements.
4. **Random programs.** Rustlantis generates programs for this kind of differential testing.
   Worth wiring in once the corpus is at 100%.
5. **CI.** `scoreboard.py` on every push to the fork.

### Target ladder

Each rung is a set of entries in `targets.json`. The bar for every rung is the same: no
mismatches, no skipped functions, the crates' own tests pass. We start the next rung when the
skip reasons on the current one are down to ones the next rung also needs.

| Rung | Targets |
|---|---|
| 0 | corpus, toy |
| 1 | small crates: semver, itoa, hex, httparse, heck |
| 2 | mid-size sync crates: regex-syntax, serde_json, toml, syn |
| 3 | a multi-crate workspace built end to end: ripgrep |
| 4 | async code: a tokio-based service |
| 5 | very large workspaces: cargo, rust-analyzer itself |

## 5. Workstreams

Each one has an owner (a subagent on the same machine), a list of scoreboard reasons it is responsible for removing, and
corpus modules it must keep green. They touch different parts of `codegen.rs` (which should be
split into a directory along these lines before parallel work starts).

| | Workstream | Removes these scoreboard reasons | Depends on |
|---|---|---|---|
| A | Harness (section 4 list) | n/a | nothing; goes first |
| B | Calls and ABI: odd-sized aggregates, fn pointers, indirect calls, closures, floats | `odd-sized aggregate`, `indirect call`, `ReifyFnPointer cast`, `closure aggregate` | A |
| C | Trait objects: vtable layout matching rustc, unsizing, dyn calls | `call through a vtable`, `unsizing` | B |
| D | Drop glue and unwinding: drop shims, cleanup blocks, landing pads | `drop glue` | B |
| E | Core intrinsics and builtin derives | `const_eval_select`, `builtin derive method`, other intrinsics | A |
| F | Constants and statics: thread locals, statics with relocations, promoted consts | `static without a rustc symbol` | A |
| G | MIR gaps in RA itself: range-pattern consts, `offset_of!`, and later async | `mir: NotSupported` | A |
| H | Daemon: file watching, cargo integration, private artifact dir, per-function reuse, several crates at once | n/a (latency) | rung 1 |
| I | Link time: faster linker, incremental relink | n/a (latency) | H |
| J | Async: coroutine state-machine transform on RA MIR | async bodies | D, G |
| K | Crate output: `.rlib`/`.rmeta` so blaze can build dependencies too | n/a | rung 2 |
| L | CAS: content-addressed store for rustc artifacts and blaze objects, remote backend | n/a (clean builds) | H |

B through G are independent of each other apart from the arrows and can run as parallel sessions
once A is in place. H and I are the path to a usable daemon. J is the largest single
item and the one most likely to need changes inside RA rather than beside it.

How a change lands, for every workstream:

1. Add or extend a corpus function that needs the feature. It shows up as skipped.
2. Implement. The function moves to compiled.
3. `scoreboard.py` passes: no mismatch anywhere, no coverage drop. `cratetest.py` on the
   affected crates still matches.
4. Commit code and the updated scoreboard together, so the history of the scoreboard is the
   history of the project.

## 6. Integration points

**rustc.** We link against its rlibs, so we must agree with it on symbol names (v0 mangling),
type layout, the unstable Rust call ABI, vtable layout and panic entry points. All of these can
change between rustc versions. blaze is pinned to one toolchain at a time, the pin is recorded
in the repo, and a toolchain bump is a scoreboard run like any other change. Layout agreement
deserves its own check: compare RA's computed layout for every type in a target against
`rustc -Zprint-type-sizes` and count disagreements. Any disagreement is a silent miscompile
waiting to happen, and today we do not measure it.

The pin is `harness/toolchain`, a stable release, and the layout code has to be that release's.
blaze computes layouts with rustc's `rustc_abi`, and the `ra-ap-rustc_abi` snapshots on crates.io
are cut from rustc's master weekly, so none is the code of a stable release. With 0.166 (master
of 2026-05-10) against stable 1.97.1 and 1.99.0, `None` of an `Option` around a struct holding two
`Option<u64>` was 2 to rustc and `u64::MAX` to us, as was `Err` of semver's
`Result<(Comparator, ..), Error>`, so every `?` on it took the wrong branch; and against 1.99.0 an
`Option` of `#[repr(i8)] enum { A = -128, B = 127, C = 5 }` was one byte to rustc and two to us.
So `lib/rustc_abi` is `compiler/rustc_abi/src` of the pinned tag, unchanged, patched in for the
crates.io crate, and a toolchain bump copies that directory again. Sizes alone do not show such
a difference: the corpus case `niche_values` makes these values on one side of the crate
boundary and reads them on the other.

What a bump has also moved so far: how rustc hashes a type into its `TypeId`
(`codegen/type_id.rs` follows 1.99.0), and what std's constants ask of the const evaluator.

**cargo.** The daemon needs the unit graph, the rustc command line for each crate and the final
link line. Today that is scraped from `cargo rustc -- --print link-args`. The cleaner source is
`cargo build --unit-graph` / `--build-plan` or a `RUSTC_WORKSPACE_WRAPPER` that records
invocations. The daemon must keep its own copy of the artifacts it links against: sharing
`target/` with a concurrent `cargo build` already broke one link.

**rust-analyzer.** We use `hir-ty`'s MIR, monomorphisation, layout and const-eval queries, and
`load-cargo` for workspace loading. The backend lives in `crates/hir-ty/src/mir/codegen.rs`
because it needs crate-private access; the goal is to shrink that to a small set of `pub`
additions so the backend can sit in its own crate.

**Cranelift.** Used as a library through `cranelift-object`. No changes needed so far.

**LLVM.** An optional second producer of a function's machine code, built with
`cargo build -p blaze-engine --release --features llvm` (needs LLVM 20; set `LLVM_SYS_201_PREFIX` to
its prefix) and chosen with `BLAZE_BACKEND=llvm`. `BLAZE_OPT=0..3` sets the level, 2 by
default. It takes the Cranelift IR the front half builds, so layout and calling conventions
are decided once for both, and gives back the same thing Cranelift does: bytes, relocations
against the function's requests, and unwind rules (`codegen/backend.rs` says what a producer
owes, `codegen/llvm.rs` how LLVM pays it). Each function is compiled alone, with nothing
inlined across functions. A function LLVM has no answer for is compiled by Cranelift and
counted with the reason on the `backend:` lines of the log. `harness/backends.py` compares
the producers on a program.

**The target.** What blaze emits for is an input of the process and not a property of the
machine it runs on: `blaze --target <triple>`, then `CARGO_BUILD_TARGET`, then the host. It is
named once, before anything is compiled, and one process has one target. `codegen/target.rs`
turns the triple into a `Target` (architecture, object format, C calling convention, unwind
format), and code that differs between platforms asks that and never `cfg!(target_arch)`.
The triple is in the key of the code cache. `x86_64-unknown-linux-gnu` and
`aarch64-unknown-linux-gnu` are accepted today; another triple is turned down by name. The
splice does not take the target from the daemon: it reads the machine from the executable.
The harnesses build for and run another target with `BLAZE_TARGET=<triple>` (`difftest.py`
has the linker, the emulator and the binutils of each). `AARCH64.md` is the state of AArch64.

**CAS.** Keys are (toolchain, crate source hash, flags, dependency keys) for rustc artifacts and
(function MIR hash, instance args, toolchain) for blaze objects. Independent of the backend
and useful on its own for clean builds.

## 7. Tracking upstream rust-analyzer

The fork is RA plus two things: the `blaze-engine` crate and the codegen module. Policy:

- Rebase onto upstream weekly. The scoreboard is the acceptance test for the rebase.
- Keep changes to existing RA files minimal and mechanical (visibility, a module declaration).
  Count them; that number is the rebase cost and should go down.
- MIR fixes that are simply correct (workstream G) are candidates for upstreaming by a human.
  RA's AI policy does not allow us to open PRs or issues there ourselves.

## 8. Known unknowns

- Whether RA's inference and MIR are right often enough. "Lowered" is not "lowered correctly":
  one RA lowering bug has turned up already. Crate test suites are how we find the rest.
- Warm re-analysis time on a large workspace. 3 ms on a small crate says little.
- RA's memory use: 0.5 to 0.9 GB on small crates, 8 GB on a 43k-line workspace with ~690
  dependencies. That was RA loading the workspace the way an IDE does, not what blaze needs
  per crate, so measure it on a mid-size crate first. Then look at cutting it: there is a
  write-up by a compiler engineer on how RA's memory could be reduced (not yet located) that
  should be read before designing anything here.
- Unwinding. RA's MIR has no cleanup edges at all; adding them is real work inside RA's lowering.
- Debug info. Line tables, functions and (under Cranelift) variables on Linux x86-64 ELF,
  following the profile's `debug` setting; see `DEBUGINFO.md` for what is covered and what is open.
