---
status: plan
summary: The no-rustc rule and the plan to remove every rustc use, step by step, with what is done.
date: 2026-10-22
---
# blaze never runs rustc

## The rule

blaze compiles everything itself. When someone builds, runs, tests or checks a program with
blaze, no rustc runs: not for a function, a crate, the standard library, a build script, a
proc macro, a check, a test, a doctest or a link, and not through cargo either. There is no
path that falls back to cargo or to rustc when blaze cannot do something. blaze says what
it could not do, the build fails, and that failure is a bug to fix.

Andrew, 2026-10-22: "Everything should all be us 100%."

The goal behind the rule is to own every layer, never by accepting less Rust. blaze moves step
by step toward owning everything from source to running code, because owning it is what makes
it fast, and it stays a superset of Rust on the way: every program the pinned nightly's rustc
(`harness/toolchain`) builds builds and behaves the same under blaze, with the real standard
library, C dependencies, build scripts and proc macros, and what blaze adds (live edits, tiers) comes on top. The one
difference allowed is that build scripts and proc macros run sandboxed
([#147](https://github.com/indexable-inc/blaze/issues/147); they do by default since
2026-10-26, [cargo.md](cargo.md#build-scripts-and-proc-macros-run-in-a-sandbox)); one that
needs more than the sandbox allows is the only exception, and is made to work rather than
refused where possible: the root manifest's `unsandboxed-build-scripts` and
`unsandboxed-proc-macros` name it.

Andrew, 2026-10-24: "we want to be a superset of Rust (i.e., any existing Rust program should
work, minus a proc macro that isn't sandboxed, which it should be tbh)."

Superset runs one way. Every Rust program must work under blaze, but a program that uses a blaze
extension (new syntax, attributes or semantics, such as const macros or
[#1448](https://github.com/indexable-inc/blaze/issues/1448)) only has to work under blaze; it
never has to build under rustc. An extension is opt-in, and turning it on must not change the
behaviour of code that doesn't use it.

Andrew, 2026-10-28: "we are superset and if using superset dont need rustc to compile it either."

This describes where blaze is going, not where it is. Today rustc still does much of the work:
it builds every program once before the daemon takes over, it compiles the standard library,
the build scripts and the proc macros, it has the last word on errors, and it keeps a copy of
every function blaze refuses. [Today](#today) lists those places. Each one is a bug with an
issue labelled `no-rustc`. They are removed one at a time; none is extended, and no new one is
added.

## What counts as running rustc

Any of these, whoever starts the process:

- blaze starting `rustc`, `rustdoc` or another rustc driver, including to ask it something
  (`-vV`, `--print cfg`, `--print sysroot`) or to assemble an `asm!` block.
- blaze starting a cargo command that runs rustc. Cargo runs `$RUSTC` for everything it
  compiles and also to learn about the target (`rustc -vV`, `rustc - --print=file-names ...`),
  so `cargo build`, `check`, `test`, `rustc` and `metadata` (except with `--no-deps`) all run it.
- A process blaze runs that runs rustc in turn: rust-analyzer's `cargo check
  --compile-time-deps` for build scripts and proc macros, or a build script that probes
  `$RUSTC` (`autocfg`, `version_check`, `rustversion`, the feature probes of `anyhow` and
  `proc-macro2`).
- blaze using what rustc wrote, even when rustc ran earlier or on another machine: the
  standard library's rlibs in the sysroot, the rlibs and objects cargo had rustc build, rustc's
  link line (`--print link-args`), an executable rustc linked as the base of patches, a proc
  macro's dynamic library or a build script's executable that rustc compiled, and rustc's copy
  of a function blaze refused ("left to rustc").

What it does not cover:

- The harnesses' reference builds. rustc is the reference
  ([contributing.md](contributing.md#rules-for-code), rule 1): the harnesses build with cargo
  and rustc and compare. The oracle stays rustc's. What changes is every expectation that blaze
  hands something to rustc or cargo.
- The platform's linker and C toolchain: `cc`, `ld`, `lld`, `link.exe`, and the C or C++ a
  build script compiles with the `cc` crate. They are not rustc. blaze runs the linker with a
  line it works out itself, not with rustc's.
- blaze's own executable, which is built with cargo and the pinned nightly's rustc. Building
  blaze with blaze is a later goal, not part of this rule.
- The standard library's source (`rust-src`). It is source, and stays an input.
- Tools cargo runs that compile no Rust: rustfmt, the resolver, fetching.

## The end state

What "100%" means, piece by piece. Each piece is where one of today's uses goes.

1. **The standard library is compiled by blaze.** `core`, `alloc`, `std`,
   `compiler_builtins`, `panic_unwind`, `panic_abort`, `unwind`, `proc_macro`, `test` and what
   they depend on, from `rust-src`, kept in the store per target and profile like any other
   crate. No rlib of the sysroot is read or linked.
2. **Proc macros are compiled by blaze and loaded by blaze.** The proc-macro crate and its
   dependencies (`syn`, `quote`, `proc-macro2`) are compiled by blaze into a library that
   rust-analyzer's proc-macro server, running inside blaze, loads. Not the toolchain's
   `rust-analyzer-proc-macro-srv`, and not a library rustc built.
3. **Build scripts are compiled, linked and run by blaze.** `RUSTC` names blaze while they
   run, so that their own probes are answered by blaze, the way the pinned rustc answers them.
4. **The cold build is entirely blaze's.** No first build by cargo and rustc, no executable
   of rustc's as the base of the first edits ([#11](https://github.com/indexable-inc/blaze/issues/11)):
   the first edits patch blaze's own executable, and the store and the remote store are what
   make that first build fast.
5. **No function is left to rustc.** A function blaze refuses fails the build, with the
   reason. The classes refused today ([#34](https://github.com/indexable-inc/blaze/issues/34),
   [#35](https://github.com/indexable-inc/blaze/issues/35), [support.md](support.md#language))
   are compiled.
6. **No fallback to cargo.** Not for source with an error, not for an item blaze cannot
   resolve ([#27](https://github.com/indexable-inc/blaze/issues/27) made such builds go to
   cargo; that goes), not for a flag blaze does not handle, not for a changed manifest. blaze
   reports an error itself, written as rustc writes it, and ends with the status cargo would.
7. **blaze's check is the final word.** `blaze check`, and the daemon's verdict on the
   source it builds, are rust-analyzer's diagnostics and blaze's borrow checker
   ([borrowck.md](borrowck.md)), which therefore have to find what rustc finds, lints that deny
   included. `cargo check` is not asked.
8. **Doctests are compiled and run by blaze.** The examples are built by the daemon, which has
   the workspace loaded, and never by rustc or rustdoc
   ([#28](https://github.com/indexable-inc/blaze/issues/28)).
9. **Tests are blaze's.** The test harness's `main`, the list of `#[test]` functions, is
   written by blaze, so adding a test is a build like any other.
10. **The link is blaze's.** The link line (the native libraries of `#[link]` and of build
    scripts, `-C link-arg`, the linker cargo's configuration names) and the shims rustc adds at
    link time (the allocator's, `main`) are blaze's.
11. **`asm!` is assembled in process.**
12. **rustc's questions are answered by blaze.** `-vV`, `--print cfg`, `sysroot`,
    `target-libdir`, `target-spec-json`, `file-names`, from blaze's own targets.
13. **Nothing of the wrapper is left.** No `blaze-rustc`, no `blaze-rustc-first`, no
    workspace wrapper and no stand-ins built by rustc.

Once nothing rustc compiled is linked with blaze's code, blaze no longer has to agree with
rustc on symbol names, the unstable Rust ABI, vtable layout or `TypeId` hashes for the two to
link. Its programs still have to behave as rustc's do.

## How blaze relates to cargo

Cargo stays the command people type and the source of a build's shape: manifests, the lock
file, features, profiles and configuration. Using cargo as a front end is fine as long as cargo
never starts rustc. Since cargo runs `$RUSTC` for everything it compiles and for what it asks
about the target, that holds only when `RUSTC` and `RUSTDOC` name blaze and blaze answers
all of it:

- **`blaze build`, `run`, `test` and `check`.** blaze plans the build itself:
  `crates/blaze-cargo/src/plan.rs` already works out the units, with the features and profile
  settings of each. It compiles and links them without starting `cargo build`. Where blaze
  needs cargo's resolution (`cargo metadata`), it runs cargo with `RUSTC` naming blaze.
- **Plain cargo** builds through no blaze since step M: `blaze` is the only way in, and a
  plain `cargo build` is cargo's and rustc's, as on a machine without blaze.
- **What went to cargo** (`--message-format`, `-Z`, several targets, an unknown triple, of a
  build; of a check only `--build-plan` and `--unit-graph`) is refused with the reason since
  [#571](https://github.com/indexable-inc/blaze/pull/571) (`refused` in `cargo/whole.rs`, `check`
  in `cargo/mod.rs`), until it is supported. None of it goes to cargo.

Every other subcommand is blaze's own, Andrew, 2026-10-26: those that are no builds "should go
to us and not cargo". `blaze fmt`, `clippy`, `doc` and the rest start no cargo, rustc,
rustdoc, clippy-driver or rustfmt; one blaze has not written yet fails, saying so, and is not
handed to cargo ([cargo.md](cargo.md#every-subcommand)).

## Self-contained: no rustup, no cargo

The same goal seen from the machine blaze is installed on
([self-contained.md](self-contained.md), [#298](https://github.com/indexable-inc/blaze/issues/298)):
installing is one `curl`, with no rustup, no toolchain pin and no cargo, and blaze resolves a
project exactly as cargo does. Until K, J and M land, blaze keeps a toolchain of its own,
private to it: the pinned nightly's `rustc`, `cargo`, `rust-std` (the host's and `wasm32-wasip1`'s) and
`rust-src` as the Rust project publishes them (123 to 160 MB), each piece removed as the step that ends its use lands. A
project's own pin does not change which Rust blaze builds with
([#269](https://github.com/indexable-inc/blaze/issues/269)). blaze's manifest reader, registry,
resolver and unit graph (`crates/blaze-manifest`, `blaze-registry`, `blaze-resolve`,
`blaze-units`) replace `cargo metadata`, `cargo build --unit-graph` and cargo's fetching, held
equal to cargo's by `harness/resolve.py`; with them K needs no cargo to plan a build. Since
2026-10-27 the standard library's workspace is read by them too (`OwnMetadata.library`,
`harness/resolve.py sysroot`), a workspace load's questions of the toolchain and configuration
(`cargo --version`, `cargo config get` in TOML for rust-analyzer and the rustflags `plan.rs`
reads, `cargo rustc --print cfg` and `target-spec-json`) are blaze's
(`crates/blaze-cargo/src/answers/cargo.rs` over blaze-manifest, `resolve.py toolchain`), and
`plan.rs` plans each load from blaze-units' graph with no `cargo build --unit-graph`
(`resolve.py units`); and a load's build scripts and proc macros are built and run by blaze
over that graph, with no `cargo check --compile-time-deps` (`cargo/compile_time.rs`,
`resolve.py ctdeps`). A question blaze cannot answer is an error naming it, never a cargo run. The cargo
processes of a cold `blaze build` and `test` from a clean `target/` (strace, Linux arm64,
the typed `blaze` included):

| run | before | sysroot read by blaze | and the toolchain's answers | and blaze-units' graph | and blaze's build scripts |
|---|---|---|---|---|---|
| harness/norustc/fixture `build` | 14 | 8 | 3 | 2 | 1 |
| harness/norustc/fixture `test` | 12 | 8 | 3 | 2 | 1 |
| ripgrep 15.1.0 `build` | 26 | 8 | 3 | 2 | 1 |
| ripgrep 15.1.0 `test` | 43 | 15 | 5 | 3 | 1 |

The one left is the typed `blaze` itself; `blaze build` and `blaze test` run no cargo at
all. The same holds with `--whole`, and none of the eight runs starts the toolchain's rustc or
rustdoc (the `rustc` a build script is given is blaze's stand-in). What blaze still keeps of
cargo is in the toolchain it ships, for a load into cargo's own target directory (the harness's
reference builds) and with `BLAZE_PROC_MACRO_SERVER=off`.

## While the paths remain

- A remaining path may be fixed where it is wrong: it must not miscompile while it exists. It
  gets no new capability and no new caller.
- New code adds none. A feature that would need a rustc run or rustc's output waits for, or
  builds, the piece of blaze that replaces it. Review turns down a new `Command` running rustc,
  rustdoc or a compiling cargo; a new read of `RUSTC` or `RUSTDOC`; a new reason to build "with
  cargo"; a new class of function "left to rustc"; and a new use of rustc's rlibs, objects or
  symbols.
- A lane whose work makes a rustc path faster or able to do more, rather than removing it, is
  redirected.
- Each remaining path is counted, and the counts only go down.

## Today

On the user's machine, nothing of blaze runs rustc or rustdoc. `blaze build`, `run`, `test`
and `check` are whole builds with no cargo build, rustc or rustdoc
([#408](https://github.com/indexable-inc/blaze/pull/408)); what blaze cannot build or plan
fails with the reason, and nothing is handed to cargo
([#571](https://github.com/indexable-inc/blaze/pull/571)). Plain cargo no longer builds through
blaze: `blaze enable`, its wrapper (`cargo/wrapper.rs`, `blaze-rustc`'s wrapper role,
`settle`'s `Built::by_cargo`) and the patching of the executable rustc linked
(`patch::Base::of_rustc`, [#11](https://github.com/indexable-inc/blaze/issues/11)) are gone
(step M, [#93](https://github.com/indexable-inc/blaze/issues/93)), and `blaze` removes what
an earlier `blaze enable` wrote in cargo's home (`cargo/home.rs`).

What is left starts rustc only outside a user's build. `harness/rustcsites.py` counts each place
(`harness/norustc/sites.json`), and the count only goes down:

| Where | What it does | When |
|---|---|---|
| `project-model/src/` (`toolchain_info/`, `cargo_workspace.rs`, `build_dependencies.rs`, `sysroot.rs`, `workspace.rs`, `cargo_config_file.rs`) | rust-analyzer's `cargo metadata`, `cargo check --compile-time-deps` and rustc's answers | a load into cargo's own target directory (the harness's reference builds) or with `BLAZE_PROC_MACRO_SERVER=off`; blaze's own loads answer these itself (B, I, J) |
| `blaze-cargo/src/cargo/load_wrapper.rs` | `blaze-rustc` as the `RUSTC_WRAPPER` of such a load's cargo: answers rustc's questions, and has rustc build a build script or proc macro blaze does not | the same loads only; it refuses any other caller |
| `blaze-cargo/src/plan.rs` `cargo_in`, `cargo/state.rs` `cargo_path` | cargo, with blaze answering its questions | the same loads |
| `blaze-cargo/src/stub.rs` | blaze standing in for rustc for a whole build (`BLAZE_WRAPS_RUSTC`) | the harness's "alone" builds (`difftest.py`, `alone.py`, `wholeprogram.py`) |
| `blaze-cargo/src/cargo/own_macro.rs` | `rustup target add wasm32-wasip1` for the wasi-libc a proc macro's module links | a toolchain without it |
| `proc-macro-srv/build.rs`, `proc-macro-srv/proc-macro-test/build.rs` | build scripts of blaze's own crates | building blaze |

### How often

Counted on dev-aws-8 (Linux x86-64, 96 cores given to the run, Cranelift), blaze at
494f0ee948, rustc 1.99.0, with the bench apps of `harness/bench.py` at their pinned revisions
and edits. Every `rustc`, `rustdoc` and `cargo` of a copy of the toolchain was replaced by a
script that logs its arguments and its parent and grandparent, and each run was put in the
step it fell in; a step ends when no process of the toolchain runs and the log has been
still for 10 s, so what the daemon does in the background after a command is counted with it.
Each step is one run. "Queries" are runs that compile nothing: `-vV`, `--print`, and cargo's
`- --crate-name ___ --print=file-names` probe of the target. "Compiles" are the rest:
compiling, checking or linking a crate, a build script, a proc macro, a doctest example, a
build script's probe, or an `asm!` block. bevy has no test step in bench.py, and its native
libraries (ALSA, udev, Wayland, X11, Vulkan) and C compiler came from a nixpkgs shell. The
scripts are in `harness/norustc/`: setup.sh, wrap.sh, count.py, classify.py and left.py.

`blaze build` and `test`, rustc runs per step (compiles + queries):

| Step | ripgrep | nushell | bevy (`--example breakout`) |
|---|---|---|---|
| cold build | 116 (43 + 73), 6.2 s | 618 (523 + 95), 40.0 s | 641 (553 + 88), 83.5 s |
| no-op build | 0 | 0 | 0 |
| body edit | 0 | 0 | 23 (2 + 21), 484 s: to cargo |
| body edit again | 0 | 0 | 44 (1 + 43), 467 s: to cargo |
| leaf edit | 0 | 0 | 80 (37 + 43), 489 s: to cargo, which compiles 36 workspace libraries |
| new function | 0 | 0 | 44 (1 + 43), 465 s: to cargo |
| signature edit | 0 | 128 (83 + 45), 8.6 s: to cargo | (no such edit) |
| an error | 6 (6 + 0): to cargo, rustc prints it | 4 (1 + 3): rustc prints it | 26 (1 + 25), 468 s: to cargo, rustc prints it |
| the error mended | 0 | 0 | 59 (1 + 58), 481 s: to cargo |
| first `test --no-run` | 106 (45 + 61), 5.5 s | 156 (115 + 41), 15.9 s | (not a step) |
| `test --no-run` after a body edit | 0 | 22 (0 + 22), 6.0 s | |
| `test --no-run` after a test was added | 75 (18 + 57): to cargo | 47 (11 + 36): to cargo | |
| doctests, first | 97 (27 + 70) and 2 rustdoc, 28.2 s | 212 (165 + 47) and 1 rustdoc, 13.3 s | |
| doctests, again | 90 (27 + 63) and 2 rustdoc, 28.0 s | 92 (78 + 14) and 1 rustdoc, 8.2 s | |
| `check` | 38 (35 + 3) | 413 (408 + 5) | 441 (419 + 22) |

Plain `cargo` on ripgrep after `blaze enable` (gone since step M), same counting ("cargo"
there is the user's command):

| Step | rustc runs | What |
|---|---|---|
| cold build | 111 (43 + 68), 6.7 s | cargo's build, as above |
| no-op build | 0 | |
| body edit, and again | 1 each | rustc writes the dep-info of the program the daemon built (wrapper.rs:315) |
| leaf edit | 6 | rustc compiles the 5 workspace libraries above the edited one, then the dep-info |
| first `test --no-run` | 77 (56 + 21) | cargo's build of the test profile |
| `test --no-run` after a body edit | 25 (3 + 22) | |
| doctests | 12 compiles and 9 rustdoc | all rustdoc's: plain `cargo test --doc` does not reach blaze |
| `check` | 34 | cargo's |

bun was not counted. Its workspace builds only after bun's own configure step has generated
code and built its C and C++ (`harness/bunprep.py` under `nix develop .#bench-bun`), which the
host did not have. What is known from [research/bun.md](research/bun.md) and the issues: bun
needs `BLAZE_OTHER_TOOLCHAIN=1` for its pinned nightly; the whole-program report left 1,645
functions to rustc, 907 of them reaching `type_info` and 304 `adt_const_params` (#34, #35); a
signature edit goes to cargo because `posix_signal_handler` is left to rustc and writes a
static ([#32](https://github.com/indexable-inc/blaze/issues/32)); the cold build is 164 s of
cargo plus 128 s until the daemon is ready, against 106 s for cargo.

What the numbers say:

- **Under `blaze`, edits of function bodies run no rustc** on ripgrep and nushell:
  no-op, body, leaf, new function, and a signature edit on ripgrep. That is what blaze was
  built for and it holds there. bevy and plain cargo are below.
- **Every cold build is rustc's.** ripgrep's 43 compiles are cargo's first build (25
  dependencies, 8 workspace libraries, 7 build scripts, the program's link) plus one `asm!`
  block and one build-script probe; nushell's 523 are 404 dependencies, 50 build scripts, 30
  proc macros, 26 workspace libraries, 11 probes, one `asm!`. The first `test` of a profile is
  the same again for the test profile.
- **Many runs are only questions**: 73 of 116 on ripgrep's cold build, 95 of 618 on
  nushell's, and all 22 of a nushell `test` after a body edit. Each worker
  and spare loads the workspace with `cargo metadata` (6 of them), `cargo rustc --print` (6) and
  `cargo build --unit-graph` (3), each asking rustc the same questions. The nushell `test`
  took 6.0 s for a body edit.
- **On bevy every edit went to cargo.** The first edit could not be spliced into rustc's
  executable ("the program is too large", [#79](https://github.com/indexable-inc/blaze/issues/79)),
  so the daemon built the whole program, which took about 8 minutes and then panicked in
  rust-analyzer ("inference vars shouldn't exist out of inference",
  [#96](https://github.com/indexable-inc/blaze/issues/96)); cargo then built the edit.
  Every later edit, the error and its mending included, found the daemon unable to load the
  workspace for the same panic, waited about 8 minutes, and went to cargo again. Those are bugs of their own; for this document they show how much of a build
  can end up rustc's without anyone seeing it.
- **Plain cargo after `blaze enable` runs rustc on every edit**: the dep-info of the program,
  and every workspace library above an edited one, since the wrapper reaches only what cargo
  asks it to and cargo recompiles the dependents. Its doctests are all rustdoc's.
- **The fallbacks to cargo seen:** an error in the source (rustc compiles and prints it);
  nushell's signature edit, which reaches `nu-derive-value`, a proc macro ("what runs during the
  build changed"), and then rust-analyzer's `cargo check --compile-time-deps` compiles 41
  libraries, 8 proc macros and 5 build scripts again; adding a test ("a library its unit tests
  are linked with as rustc built it changed"), since the list of tests is rustc's.
- **Doctests are the worst.** Each library starts a process that loads the workspace again
  (on nushell's first run that is 87 rustc compiles of rust-analyzer's `cargo check
  --compile-time-deps`), links a probe program with rustc to learn the link line, and has
  rustc check or link examples. And a bug sends whole packages to rustdoc: the doctest
  process runs `cargo build --unit-graph` on the workspace's root package with the
  library's features ("the package 'ripgrep' does not contain these features: default, log"),
  which fails, so `globset` and `grep-printer` on ripgrep and `nu-protocol` on nushell go to
  rustdoc.
- **`check` is all rustc's**: 35, 408 and 419 crates checked or compiled.
- **Functions left to rustc**, from the workers' logs: ripgrep's program had 1,769 of 1,769
  functions compiled by blaze; over every build (tests and doctests included) 53 distinct
  functions were left to rustc, all for "no rustc symbol". nushell: 534 distinct, 533 for "no
  rustc symbol", 1 for "frontend: mir: HasErrors". "No rustc symbol" is a consequence of linking
  ahead of rustc's rlibs: rustc exported nothing blaze's copy could take the place of, so
  rustc's copy is called. It goes away with step G, when nothing rustc built is linked.
  bun's [#34](https://github.com/indexable-inc/blaze/issues/34) and
  [#35](https://github.com/indexable-inc/blaze/issues/35) are the real refusals.

#### After A, B and C

Counted the same way on dev-aws-eph-norustc (Linux x86-64, 96 cores, Cranelift), blaze at
8155abc19c (2026-10-22): A, B and C merged, and D's first part and the spare that starts only
for a build that asks for one (#56) with them. rustc runs per step (compiles + queries); the
steps not shown ran none before and none now.

| Step | ripgrep before | ripgrep now | nushell before | nushell now |
|---|---|---|---|---|
| cold build | 116 (43 + 73), 6.2 s | 59 (42 + 17), 4.2 s | 618 (523 + 95), 40.0 s | 559 (522 + 37), 24.0 s |
| signature edit | 0 | 0 | 128 (83 + 45), 8.6 s | 90 (83 + 7), 5.6 s |
| an error | 6 (6 + 0) | 0 (D) | 4 (1 + 3) | 0 (D) |
| first `test --no-run` | 106 (45 + 61) | 56 (50 + 6) | 156 (115 + 41) | 123 (115 + 8) |
| `test --no-run` after a body edit | 0 | 0 | 22 (0 + 22), 6.0 s | 0 |
| `test --no-run` after a test was added | 75 (18 + 57) | 18 (18 + 0) | 47 (11 + 36) | 11 (11 + 0) |
| doctests, first | 97 (27 + 70) and 2 rustdoc, 28.2 s | 8 (7 + 1), 3.0 s | 212 (165 + 47) and 1 rustdoc | 132 (131 + 1), no rustdoc |
| doctests, again | 90 (27 + 63) and 2 rustdoc, 28.0 s | 6 (6 + 0), 2.9 s | 92 (78 + 14) and 1 rustdoc | 123 (123 + 0), no rustdoc |
| `check` | 38 (35 + 3) | 38 (35 + 3) | 413 (408 + 5) | 446 (441 + 5) |

- **Queries** went from 73 to 17 in ripgrep's cold build and from 95 to 37 in nushell's. What
  is left is each question's first ask per toolchain (cargo's probes, rust-analyzer's `cfg`
  and target spec, the sysroot), and the probes of `cargo check --compile-time-deps` and of
  plain `cargo check`, which blaze does not wrap (steps I and D).
- **Doctests** no longer go to rustdoc: globset, grep-printer and nu-protocol are blaze's.
  Their examples are still compiled and linked by rustc in the doctest process (step L), which
  is why nushell's second run compiles 123 where it compiled 78 and left nu-protocol to
  rustdoc. On ripgrep the run failed on about half the runs: one of globset's examples crashed
  ([#101](https://github.com/indexable-inc/blaze/issues/101), a library built with fewer
  features than cargo's; fixed under A below). That bug was there before; rustdoc hid it.
- **`check`** is plain `cargo check` (D) and moves with what the steps before it left built.
- **The fixture of `harness/norustc.py`**, in CI on Linux x86-64, Linux AArch64 and macOS
  arm64 under both backends: the cold build 74 to 76 rustc runs before A, 18 after A, 6 before I,
  5 with its build script compiled by blaze (I's first part, Linux x86-64
  cranelift, the one platform measured; below);
  edits 5 (277 to 297 on macOS, whose daemon did not listen, #106), 0 now; tests 16 to 19, 12
  after B, 7 after H, 6 with blaze's harness always (Linux x86-64 cranelift, the one platform measured; below); doctests 23, 6 after B, 3 after L's first part, 0 now (L's second, below); check 7, then 8 since the fixture
  has one library more, 5 now. These are `norustc/baseline.json`.

## The plan

The steps in the order they can land. Each has an issue labelled `no-rustc`, #81 to #93. Effort is a rough
size for one person: small is days, medium one to three weeks, large more. Each step ends with
the counts of [How often](#how-often) lower, measured by step A's check, and none higher.

| Step | Removes | Needs | Effort | Risk | Owner |
|---|---|---|---|---|---|
| [A](https://github.com/indexable-inc/blaze/issues/81). Count every rustc run and refuse new ones | nothing; makes the rest measurable | | small | low | the `no-rustc-abc` lane: done |
| [B](https://github.com/indexable-inc/blaze/issues/82). blaze answers rustc's questions | every query | A | medium | medium | the `no-rustc-abc` lane: done |
| [C](https://github.com/indexable-inc/blaze/issues/83). `asm!` assembled in process | the assembler run | A | small to medium | low | the `no-rustc-abc` lane: done |
| [D](https://github.com/indexable-inc/blaze/issues/84). blaze's verdict is the final word | the fallback on an error, `blaze check` as `cargo check` | A; borrowck work | large | high | the app-fixes worker's `no-rustc-d` lane, which also has [#96](https://github.com/indexable-inc/blaze/issues/96) |
| [E](https://github.com/indexable-inc/blaze/issues/85). No function left to rustc | the refused classes; a refusal fails the build | A | large, ongoing | medium | the bun lane (bun's classes, #34, #35) and the glancer lane (kept rejections, #37) |
| [F](https://github.com/indexable-inc/blaze/issues/86). The standard library compiled by blaze | the sysroot's rlibs | C, E | large | high | the no-rustc F+G lane |
| [G](https://github.com/indexable-inc/blaze/issues/87). Every crate compiled and linked by blaze | rustc's rlibs, objects, symbols and link line, "no rustc symbol" | E, F | large | high | the no-rustc F+G lane |
| [H](https://github.com/indexable-inc/blaze/issues/88). blaze writes the test harness | the fallback when a test is added | G | medium | medium | the `no-rustc-hl` lane: done; the `no-rustc-l2` lane: blaze's harness always, done |
| [I](https://github.com/indexable-inc/blaze/issues/89). Build scripts compiled and run by blaze | build scripts and their probes by rustc | B, G | medium | medium | the `no-rustc-l2` lane: first part done (scripts compiled and linked by blaze, their runs kept in the store); the F+G lane consumes it for the standard library's scripts |
| [J](https://github.com/indexable-inc/blaze/issues/90). Proc macros compiled and loaded by blaze | proc macros by rustc, the toolchain's proc-macro server | F, G | large | high | the self-contained lane ([#298](https://github.com/indexable-inc/blaze/issues/298)): plan, and J then K first |
| [K](https://github.com/indexable-inc/blaze/issues/91). The cold build is blaze's | cargo's first build, the rustc base (#11) | G, I, J | medium | high | the self-contained lane, with 54301ec5's first-build lane (#310); `BLAZE_WHOLE_BUILD=1`: the build from nothing is G's whole build, no `cargo build` |
| [L](https://github.com/indexable-inc/blaze/issues/92). Doctests by the daemon (#28) | probe and examples by rustc, rustdoc | B, G, D | medium | medium | the `no-rustc-hl` lane: first part done; the `no-rustc-l2` lane: second part (examples by blaze) done |
| [M](https://github.com/indexable-inc/blaze/issues/93). Remove the wrapper and the fallback | `blaze-rustc`, `blaze-rustc-first`, stand-ins, dep-info, settle, `by_cargo`, `only_cargo` | all | medium | low | done: `blaze enable`, the wrapper and the rustc base are gone; `blaze-rustc` is left as the wrapper of a load into cargo's target directory, `BLAZE_WRAPS_RUSTC` to the harness |

A, B and C landed on 2026-10-22. D and E run alongside everything: they are
correctness work whose bar rises as the others land. F, G, I and J are the core and go in that
order, I and J in parallel once G is in. K, H and L follow, M last.

**A. Count every rustc run and refuse new ones.** ([#81](https://github.com/indexable-inc/blaze/issues/81)) Every start of rustc, rustdoc or a cargo
that compiles goes through one function that records the reason, and `blaze-engine cargo status`
prints the counts. The counting of this document becomes a harness script run nightly on the
bench apps, failing when a count goes up, and the fmt job refuses a new `Command` of rustc,
rustdoc or cargo outside that function. Also fix the doctest features bug above, which sends
packages to rustdoc for no reason.

Done (the `no-rustc-abc` lane, [#97](https://github.com/indexable-inc/blaze/pull/97) and
[#100](https://github.com/indexable-inc/blaze/pull/100)), in a different form from the one
above. `harness/norustc.py`, in the fast tier on every platform and backend CI has, builds a
small workspace (`harness/norustc/fixture`: a program with an `asm!` block, a library with unit
tests, doctests and features the root package uses through another library, a proc macro, and a
path dependency outside the workspace with a build script) through a copy of the toolchain whose
rustc, rustdoc and cargo log every run, in five scenarios: a cold build, edits, tests, doctests
and check. A count above `harness/norustc/baseline.json` fails the release train; a lower one says to
lower it (`norustc.py --record`). The baselines are CI's: a 96-core dev host counts more, its
spares asking more. `harness/rustcsites.py`, in the fmt job, counts the places in `crates/` that
start rustc, rustdoc or cargo, or read which to start, per file against
`harness/norustc/sites.json`, and fails on a new one. Not done: one function every start goes
through, with `blaze-engine cargo status` printing the counts, and the bench apps counted nightly
(`harness/norustc/count.py` is still run by hand). The doctest bug: the doctest process planned
the workspace's root package and ran the build scripts with the member's features, which the
root does not have; it now selects the package (`--package`, from `CARGO_PKG_NAME`) and plans
the package rustdoc runs the examples in. That still built the library as a build of its package
alone does, while cargo built the libraries the examples link against with the features of the
whole build it was asked for: under `--workspace` a dependency two packages share has the
features of both, and blaze laid out its types without them
([#101](https://github.com/indexable-inc/blaze/issues/101): globset's example freed a
regex-automata cache whose `dfa-onepass` part blaze's code never wrote, and crashed when the
stack held a bad pointer there). `blaze test` now tells the doctest process its own
selection (`BLAZE_DOCTEST_BUILD`: packages, `--workspace`, features, profile, and where it was
run), and the library is planned from the same `cargo build --unit-graph`;
`blaze_cli.py`'s flags check has a case whose layout differs by a feature only another
package turns on.

**B. blaze answers rustc's questions.** ([#82](https://github.com/indexable-inc/blaze/issues/82)) `-vV`, `--print cfg`, `sysroot`, `target-libdir`,
`target-spec-json`, `file-names`, `split-debuginfo` and the rest are answered by blaze from
its own targets, byte for byte as the pinned rustc answers them. rust-analyzer's `toolchain_info`
asks blaze in process. `cargo metadata`, `cargo rustc --print` and `cargo build --unit-graph`
run with `RUSTC` naming blaze, and a load asks them once, not once per worker and spare. Risk:
a wrong `cfg` builds the wrong code without a word; the harness compares every answer with
rustc's on every target blaze knows.

Done in part (the `no-rustc-abc` lane, [#100](https://github.com/indexable-inc/blaze/pull/100)):
the answers are rustc's, asked once per toolchain and kept, not blaze's own.
`crates/blaze-cargo/src/answers.rs` keeps each answer to `-vV`, `--version` and the `--print` kinds
that read no source (cfg, target-spec-json, sysroot, target-libdir, cargo's file-names probe and
the rest) under blaze's cache, by the rustc it is of (its canonical path, length and time), the
arguments, the variables rustc reads for them and the input, and gives it again byte for byte;
only an answer with status 0 and nothing on stderr is kept, and `BLAZE_ANSWERS=0` asks rustc
every time. The processes that ask one question together wait for the first one's answer, so
each is asked once. cargo asks through blaze as the workspace's wrapper in builds and as
`RUSTC_WRAPPER` in the cargos blaze runs only to ask (`--unit-graph`; the
sysroot's `metadata`, `rustc --print` and `config get` are blaze's since 2026-10-27), unless the user
named a wrapper; rust-analyzer's queries go
through a hook in project-model's `utf8_stdout`. Left: the first ask of each question, 12 on
the fixture's cold build and 17 on ripgrep's, which answering from blaze's own targets removes;
`cargo check --compile-time-deps`'s probes (I) and plain `cargo check`'s (D).

Done (the `no-rustc-hl` lane, 2026-10-23): blaze answers from its own data, and even the first
ask runs no rustc. `crates/blaze-cargo/src/answers/rustc.json` has what the pinned rustc says of
itself (`-vV`) and of each of twelve targets (Linux GNU and musl, macOS and Windows on x86-64 and
AArch64, wasm32 unknown, WASI 1 and 2): its cfgs, those unstable features add, the default panic
strategy, the names of each kind of crate's file, the split-debuginfo kinds and the target spec
of each host (x86-64 Linux's rustc links with its own lld, so its specs differ). `answers/own.rs`
knows the release by rustup's manifest in the sysroot, and adds what the arguments decide, as
rustc does: `debug_assertions` from `-C debug-assertions` or the optimization level,
`overflow_checks` and `ub_checks` as a nightly prints them (`RUSTC_BOOTSTRAP=-1` makes it
answer as a stable release), `-C panic`, `--cfg`, `proc_macro`,
the sysroot and target libdir from where the toolchain is. A question with anything it does not
model (`-C target-cpu`, `-C target-feature`, other `-Z` flags, a deployment target or SDK in the
environment, a JSON target, another release, a host whose target specs it lacks) is asked of rustc
once per toolchain as before. `harness/answers.py`, in the fast tier on Linux and macOS, compares
238 questions with rustc's answers, byte for byte, on the toolchain rustup installed and on a copy
laid out as norustc.py's; `--write` regenerates the data after a toolchain bump, on each of x86-64
Linux, AArch64 Linux and arm64 macOS (written for 1.99.0 on 2026-10-23, for `nightly-2026-10-25`
since). cargos blaze does not drive (rust-analyzer's `cargo check --compile-time-deps`, `cargo
blaze check` and the subcommands blaze does not claim) ask through blaze as `RUSTC_WRAPPER`
with `BLAZE_ONLY_ASKS=1`, which answers questions and leaves every compile as it was. On the
fixture: the cold build 18 rustc runs to 6 (its 12 questions to none), check 8 to 5, and the
build-script check of rust-analyzer's that a 96-core host's tests ran lost its 3. Linux
AArch64's target specs came from answers.py's run in CI there, and its cold build went from 7 to
6 with them. Codegen options that change no answer (`-C link-dead-code`, `rpath`,
`force-frame-pointers`, `force-unwind-tables`) no longer send a question to rustc: count.py's
`RUSTFLAGS` have the first, so every question of ripgrep's went to rustc until then.

Done (the `nopin` lane, 2026-10-28; Andrew 2026-10-28 03:30Z, no Rust toolchain pinning): no
question goes to rustc at all, in any environment. Every rustc is answered as blaze's own
release, whichever one was named (`RUSTC`, `build.rustc`, rustup's proxy, a rustc on `PATH`):
the version and host from blaze's build, the cfgs and target specs from `rustc.json`, the
sysroot and target libdir from where blaze's release is (`cargo/toolchain.rs`, found on disk
with nothing run). A question `answers/own.rs` does not model (the list above) fails with
`blaze has no answer to rustc ...`, as no-fallback says, and nothing asks rustc and keeps its
answer any more: the cache of rustc's answers, `BLAZE_ANSWERS` and the reading of rustup's
`RUSTUP_TOOLCHAIN`/`RUSTUP_HOME` to tell which rustc a proxy runs are gone. Nor does
`toolchain::settle` ask the project's rustc its version and sysroot, or note that the project
names another toolchain, which is what made main's no-op `blaze build` run 8 rustc processes
(4 questions through rustup's proxy, #833). `rustcsites.py`: 33 places to 27. rustup is not
run either: loading a workspace took cargo from `rustup which cargo` and ran it through rustup's
proxy, and ran `rustup component add rust-src` when the library's source was missing; the
sysroot's own `bin/cargo` is run now, and a missing source is an error that says to add it.
`rustcsites.py` counts the places that start rustup too: 29 to 28. The one left,
`cargo/own_macro.rs`'s `rustup target add wasm32-wasip1`, installs WASI's libc for proc-macro
modules when blaze's release has none; it asks nothing.

Measured (ix VM, x86_64, 64 cores; release build with `--features llvm` and the proc-macro
server beside it; fresh project, daemon warm; median of 30 alternated no-op `blaze build -q`
after 3 warm-up runs; execs by `strace -f -e trace=execve` over the client and the daemon):

| setup | before (ee8870419e) | after |
|---|---|---|
| rustup, default nightly, `rust-toolchain.toml` pinning 1.80.0 | 18.4 ms, 2 rustc | 7.4 ms, 0 rustc |
| no Rust installed (rustc, cargo, rustup hidden) | 8.6 ms, 1 rustc (not found) | 8.4 ms, 0 rustc |
| plain `cargo build -q` with the rustup setup | 13.3 ms, 2 (cargo and rustc through rustup) | |

What is left of the 7 to 8 ms is starting the binaries (`blaze-engine --version` alone takes 6 ms
on that VM), not a question. A cold build of a project using anyhow, thiserror, rustversion,
num-traits (autocfg), indexmap, version_check and proc-macro-error2 with no Rust installed
runs 0 rustc: each build script's 11 runs of `$RUSTC` are blaze's stand-in.

**C. `asm!` assembled in process.** ([#83](https://github.com/indexable-inc/blaze/issues/83)) The assembly text blaze already produces goes to LLVM's
assembler (the LLVM backend links LLVM already) or to an assembler crate, in process. Risk is
low: the cache key and the output are the same, and `harness/asmedits.py` compares.

Done (the `no-rustc-abc` lane, [#102](https://github.com/indexable-inc/blaze/pull/102)). A
build of blaze with LLVM (CI's) assembles every block with LLVM's MC assembler in process,
whatever the backend; before, only the LLVM backend did. A build without LLVM, which the release
is, has the platform's C compiler assemble the block, clang for any target or `cc` for the
host's own, with the text as rustc hands it to its assembler (Intel syntax on x86-64). That is a
process, but not rustc, and the C toolchain is outside the rule. `BLAZE_ASSEMBLER=cc` picks the
C compiler in a build with LLVM, and asmedits.py also runs with it in the fast tier.

The rustc that assembled where there was neither LLVM nor a C compiler is gone (the `no-rustc-hl`
lane, 2026-10-23): such a build fails with what to install, clang or a build of blaze with LLVM.
Of what support.md lists, that is Windows x86-64 MSVC without clang on PATH and a build of blaze
without LLVM (CI's builds have it); the other places (cross targets other than the host's,
for which there is no `cc`) need clang as they did. An assembler in process for a build without
LLVM is still to write.

**D. blaze's verdict is the final word.** ([#84](https://github.com/indexable-inc/blaze/issues/84)) An error is blaze's, written as rustc writes it,
with the status cargo returns; the build does not go to cargo to have rustc print it.
`blaze check` is rust-analyzer's diagnostics and blaze's borrow checker, with the deny
lints. This needs blaze to reject what rustc rejects: [borrowck.md](borrowck.md)'s
measurements of the UI suite are the bar, and the step lands in parts as they pass (errors
first, then lints). [#27](https://github.com/indexable-inc/blaze/issues/27) is reversed: an
item blaze cannot resolve is an error to fix, not a reason to build with cargo. Risk: a
program rustc rejects runs, or one it accepts is refused.

Owner: the `no-rustc-d` lane (a Cursor agent, Andrew's), from 2026-10-22. The errors came
first: an error rust-analyzer or the borrow checker finds in the edited files, a file that
does not parse, and a function the frontend turns down as not compiling (an unresolved name,
method or field, a type error, incomplete code) fail the build as blaze writes them, in
rustc's form (`error[E0308]: ...`, ` --> file:line:col`, the line marked, then cargo's `could
not compile` line), with status 101, under `blaze` and under plain cargo after `blaze
enable`. What stays with cargo for now: what the frontend turns down as unsupported
(`NotSupported`, layouts, constants: step E) and blaze's own failures (a build that panicked, a
link that failed). An error in a file written before the worker loaded the workspace is blaze's
too since 2026-10-23: the worker leaves it beside its socket and every build of the checkout
fails with it until a source is written. The lints followed: a lint the source denies fails the
build for the eight lints `harness/lints_ui.py` finds rust-analyzer reports only where rustc does
(on rustc's UI tests that deny or forbid a lint, 2026-10-23: non_snake_case 31 right, 0 wrong,
9 missed; non_camel_case_types 22, 0, 2; unused_must_use 14, 0, 30; non_upper_case_globals 9,
0, 4; deprecated_safe_2024 3, 0, 0; elided_lifetimes_in_paths 1, 0, 0; unsafe_op_in_unsafe_fn
1, 0, 0; the same against 1.99.0 and the nightly of 2026-10-09). The last three have few UI
tests, so `harness/lints_probes` has a unit for each place they apply (a parameter, a return
type, a `let`, a closure, a trait method, a macro, an `unsafe fn`, editions 2021 and 2024; 85
units with the brace lints): elided_lifetimes_in_paths 25, 0, 6; unsafe_op_in_unsafe_fn 10, 0,
1; deprecated_safe_2024 8, 0, 2. Five had been wrong first: rust-analyzer's case rules were
rustc's of an old commit, its `unused_must_use` looked into `if` and `match` arms and took
`Result<(), Never>` for one to use, and it took a deprecated-safe call (`std::env::set_var`
before 2024) in an `unsafe fn` for the lint, which rustc reports there only where
`unsafe_op_in_unsafe_fn` is not allowed. The eighth, since 2026-10-24, is `unused_mut`, which
the borrow checker gives and `BLAZE_BORROWCK_REFUSE=0` turns off with its errors. Few UI tests
deny it, so `lints_ui.py --deny unused_mut` scores it on every UI test, each read with
`#![deny(unused_mut)]` first: 51 right, 0 wrong, 5 missed against the nightly of 2026-10-09
(6,446 units in scope), 49, 0, 5 against 1.99.0, and 71, 0, 3 on its 25 units in
`harness/lints_probes` (closures, `mut self`, patterns, `ref mut`, later assignment, autoref,
compound and field assignment, `Box`, `FnMut`, macros, `async`, shadowing, `asm!`). Seven had
been wrong first: an assignment, a block without a tail or a `break` without a value gave its
place no `()`, so `let mut z: () = x = 1; z = x = 2;` took the second write for the first (and
`let z: () = x = 1; let w = z;` was E0381), a comment-only `asm!` was lowered to nothing, so
its `inout` registers were not mutated, and a body whose array length fails to evaluate was
linted, which rustc's is not. The misses are in macro expansions and that body's length.
None of the eight is denied by default, so the level
is the source's, which only `--cap-lints` and `--force-warn` undo; with either among the flags
no lint fails a build. A lint in what a macro expands to does not, and `lints_ui.py` does not
count one (rustc reports it in the macro's definition). The other lints are not blaze's
verdict yet: a crate that denies one rust-analyzer does not find runs code rustc turns down.
Those with the most errors in rustc's UI tests are `dead_code` (294), `explicit_outlives_requirements`
(226), `unused_unsafe` (181), `unreachable_patterns` (128) and `unused_variables` (75). `blaze check` is the daemon's verdict on the build of the
same flags when it has that build loaded. Since 2026-10-23 any other check is the daemon's too,
not `cargo check`: the daemon starts a checker (`blaze-engine --check`, `crates/blaze-engine/src/check.rs`)
that loads the workspace as the worker of that build would and builds nothing, and answers each
check with what the build's verdict finds in every file of the workspace's crates (rust-analyzer's
sure errors, the borrow checker's, the deny lints). It stays loaded and takes in edits of the
files it has; a new source file, a changed manifest, or a changed build script or proc macro
has it load again. The build scripts and proc macros are cargo's from an earlier build; a
workspace that never had them built has blaze build and run them (`cargo/compile_time.rs`, since
2026-10-27; before, rust-analyzer's `cargo check --compile-time-deps` built them with rustc, which
I and J removed). Code rustc accepts that blaze takes for wrong fails such
a check, as it fails a build. On `harness/norustc.py`'s workspace (dev host, cranelift) the
`check unloaded` scenario (the daemon stopped, an error, the error mended, another package)
runs no rustc, and `check` itself none on every platform CI has (5 recorded before). Counted on ripgrep with `harness/norustc` (dev-aws-3, 2026-10-22), the
error step went from 6 rustc compiles in 0.9 s to none in 0.1 s. The first `test --no-run`
after it compiled 5 workspace libraries more (50 compiles, not 45): the cargo build of the
error had brought up to date the libraries an earlier edit had changed, which `cargo test`
now builds instead. Over the whole run that is 200 compiles, not 201. On `harness/norustc.py`'s
workspace (dev-aws-3, cranelift, two runs each) the edits went from 5 rustc runs to none, and
the tests that follow from 48 or 49 to 52 for the same reason: 138 runs in all, not 139 or 140.
`blaze check`
can't be an in-process load yet: on ripgrep, loading the workspace and the standard library
and checking its 97 files took 6 to 8.5 s, against 2.3 s for a warm `cargo check`, which is why
it asks the daemon, which keeps the workspace loaded.

**E. No function left to rustc.** ([#85](https://github.com/indexable-inc/blaze/issues/85)) The classes of #34 and #35 and the rest of
[support.md](support.md#language) are compiled; a refusal fails the build with its reason. Until
G lands, "no rustc symbol" cannot go; everything else can. Axionize's work on #34, #35 and #75
is this step.

The glancer lane owns the part of E that [#37](https://github.com/indexable-inc/blaze/issues/37)
found. The instances an own base refuses for a writable static whose rustc symbol blaze does not
find (helix's `__RUST_STD_INTERNAL_VAL`, a thread-local of std's, which throws a round of 29 s
away) are compiled, along with their callers, instead of being kept as rejections whose callers
are forwarded to rustc's copies. Done when helix's own base builds in one round with none of them
left to rustc, and `harness/norustc/left.py` counts no more functions left to rustc than before.

Done 2026-10-22. The one refused instance was `Context::with`'s closure, whose static std's
`thread_local!` declares in the body. rust-analyzer expands a macro used in a body twice, to items
and to statements, and blaze counted the static's place in the other expansion, so it never found
rustc's symbol. helix's own base now builds in one round. `count.py` and `left.py` at the branch's
base and with the fix, on dev-aws-5: ripgrep 53 distinct functions left to rustc both times,
nushell 534 both times, and every step's rustc runs the same but nushell's "test added" (47 runs
before, 41 after, 11 compiles both times).

Owner of bun's classes: the `bun-e` lane (2026-10-22). It compiles what bun leaves to rustc
today: `adt_const_params` constants, `core::mem::type_info`, inherent associated types,
`lol_html`'s macros, `core`'s methods in `no_std` crates and the rest of #35's table, plus what
bun's whole-program report finds after them. Each class lands with its `bun_repros.py` fixture
(#75) expecting `compiled`. Done when bun's whole-program report (`runbench.py bun`) leaves no
function to rustc but those step G removes ("no rustc symbol"). On Linux x86-64 it leaves none
(2026-10-22, `857eab0e9b`: 68,315 functions compiled, the program links and each of
`apps.json`'s runs matches cargo's); macOS arm64 is to be run again.

**F. The standard library compiled by blaze.** ([#86](https://github.com/indexable-inc/blaze/issues/86)) `core`, `alloc`, `std`, `compiler_builtins`
(with its intrinsics and `#[naked]` functions), `panic_unwind`, `panic_abort`, `unwind`,
`proc_macro`, `test` and their dependencies, from `rust-src`, with std's own build script and
cfgs, kept in the store per target and profile and served by the remote. The harness's std
tests (11 of 2,846 refused today) are the bar. Risk: lang items, intrinsics and
`compiler_builtins` are where blaze has had least practice.

**G. Every crate compiled and linked by blaze.** ([#87](https://github.com/indexable-inc/blaze/issues/87)) The harness's "alone" build, where blaze
compiles every crate of the program, becomes the only build. blaze works out the link line
itself: the libraries of `#[link]` and of build scripts' `cargo:rustc-link-lib`, the target's
system libraries and start files, `-C link-arg` and the linker cargo's configuration names, and
the shims rustc adds (the allocator's, `main`). Nothing of rustc's is linked, so symbols.rs,
weaken.rs, `Base::of_rustc` and the "ahead of rustc's rlibs" link go, and with them "no rustc
symbol", ABI and mangling parity, and [#52](https://github.com/indexable-inc/blaze/issues/52).
Risk: a link line that differs from rustc's on a platform; the harness links every bench app
on Linux, macOS, Windows and wasm and compares the programs' behaviour.

The no-rustc-fg lane owns F and G. Where they are (2026-10-22, Linux x86-64): with no rustc
symbols blaze compiles a program together with `std`, `core`, `alloc`, `compiler_builtins`,
`panic_unwind`, `unwind`, `libc` and the 11 crates they use from `rust-src`, and `cc` links
the one object with nothing rustc built. hello, and a program of collections, panics caught
and uncaught, threads, channels and formatting, print what cargo's builds do. The standard
library also builds alone, once, into an object of its own (a unit: its functions and statics
exported under blaze's names, a static in a generic function's body included), and a program
builds over it with `--prebuilt std.o`, compiling only its own crates and the generic functions
of the library it instantiates. A build with nothing of rustc's that names none of the
library's crates does that by itself: it takes the unit from the store (`BLAZE_CACHE`), which
fetches it from the remote, keyed by the library's crates, their cfgs and editions, the target
and the debug info asked for (the store adds blaze's build and the toolchain), or compiles it
once and keeps it there, and writes it beside `--out` as `<out>.std.o`; `harness/allblaze.py`
checks all three ways against cargo. The toolchain's commit, which the store keys
the library by and which its debug paths name (`/rustc/<commit>/library/...`), is read from the
toolchain's channel manifest, or for a nightly's manifest, which names it only by a short hash,
from blaze's own pins (`pins.txt`) when the manifest's version is blaze's nightly; until
2026-10-27 it was read out of the sysroot's `libstd` rlib, the one file rustc built a whole build
still opened. For G, such a build compiles every crate the program
depends on as well, not only those it names, and links them with the unit: ripgrep (60 crates)
builds, links and runs with nothing rustc built, and 19 of its commands (searches, `--json`,
`-U`, `-P`, `--files`, errors, `--help`) print what cargo's build does, timings aside. A function
blaze cannot compile (1,786 of the library's: 534 for f16 and f128, most of the rest
`core::arch`'s SIMD; the 222 intrinsics declared with no body are not functions to compile,
every call to one being the compiler's own code) is, where something calls it, a hidden stand-in
that says so when it runs. A program that reaches one is not built: a function of its own
crates blaze refused (its `main`, or one only code of the object calls, its own copy of an
`#[inline]` function of the library's among them) stops the build before the link, and a stand-in
the link keeps, the library unit's included, fails it after; either way the error names each
function and why it was refused (`` `p` reaches 1 function blaze could not compile, and a build
with nothing of rustc's has nothing else to compile it, so it is not built:``), and no
executable is left. A function the library exports by name that nothing in it calls has no
stand-in, as one exported under the name could take the place of a definition the program
links from elsewhere (libgcc's soft float is named as the library's is): the unit keeps a list
of those it refused beside it and in the store, and a link that fails on one of their names
is the same error, naming the function and why. The unit is what the program takes of the library and everything that depends on, but
`test`, `proc_macro` and the panic runtime the program does not link, which rust-analyzer's
graph has every crate depend on: 21 crates, `rand` and `foldhash` among them, which std's own
workspace adds to its graph and which nothing compiled uses. The library's build names only the
library's crates, so the program's own `memchr` is not taken for std's.

blaze links the program itself with `--exe`: `cc` with the arguments rustc gives it for the
target (on Linux `--as-needed`, `--eh-frame-hdr`, `-z noexecstack`, `--gc-sections`, `-pie`,
`-z relro,-z now` and `-nodefaultlibs`; on macOS `-arch` and `-dead_strip`), linking with the
toolchain's lld on ELF targets as rustc does on x86_64 Linux (GNU ld takes ten times as long
over ripgrep: 2.5 s against 0.25 s), and the native libraries the crates linked ask for with
`#[link(name, kind, modifiers, cfg)]`, read from their `extern` blocks, a crate's before those
of the crates it depends on; no list of libraries is fixed. What the link leaves out it proves
unreachable, so a stand-in left in the executable is a function the program can reach that
blaze could not compile: blaze says how many after the link and fails on any.
None is left in hello, the four `allblaze.py` programs or ripgrep. The rest of what rustc
does around the link: the allocator shim calls the `#[global_allocator]`'s methods (through the
four functions the attribute's expansion exports) or else std's defaults; the panic runtime is
`panic_abort` when the program's crate has `cfg(panic = "abort")` from its profile, else
`panic_unwind`, and a unit of each is kept apart since the crates are part of its key; the cfgs
`compiler_builtins`' and `libc`'s build scripts would set (`mem_unaligned`, `intrinsics_enabled`,
`f16_enabled` and `f128_enabled` from `target_has_reliable_f16`/`f128`, `feature = "mem"` where
there is no libc; `freebsd12` and the musl time cfgs) are worked out from the target's cfgs
when the sysroot's crates are added to the graph, nothing run; and `__rust_probestack` (a
naked function in a calling convention blaze does not write) is never called, Cranelift
probing the stack inline, and the link leaves it out.

**H. blaze writes the test harness.** ([#88](https://github.com/indexable-inc/blaze/issues/88)) The `main` of a test executable, the list of `#[test]`,
`#[should_panic]`, `#[ignore]` and benches as libtest's descriptors, is generated by blaze, so
adding a test is a build like another.

Done (the `no-rustc-hl` lane, 2026-10-23), for tests; for benches and an `ignore` with a list
after it since 2026-10-25.
`crates/blaze-engine/src/test_harness.rs` writes, into the daemon's text of the root file of each
crate a test program is built from, a module `__blaze_test_harness` that lists every `#[test]`
function and every `#[bench]` as a `test::TestDescAndFn` (its path, `#[ignore]` with its message, `#[should_panic]`
with `expected`, the file, line and column of its name, unit or integration test) and whose
`main` calls `test::test_main_static`. That `main` is the program's start, ahead of the one
rustc wrote. Every program of tests starts in blaze's harness, written when the program is
first asked for (a worker's first program at its load, which then starts from a build of its
own rather than what rustc linked; the others at the first `test` that asks for them), so the
whole build it takes runs while nobody edits and the first edit is a patch; a test added,
removed, renamed or with other attributes is an edit like another. The harness written into a
library's root file is an edit of the program of its tests alone: the programs that link the
library do not take it over from rustc for that. Unit and integration test programs alike: the plan marks every test program's directory
(`tests`). `cfg_attr`s are expanded by the crate's cfg. rust-analyzer resolves the module's
paths to private tests (a patch in `hir-def`'s path resolution, for that module only). A
bench is called with a `Bencher` (`test::StaticBenchFn`), an `ignore` with a list after it
ignores the test with no message, as rustc reads it, and a test inside a function is left out,
as rustc leaves it out: what kept rustc's harness and sent a test added to cargo is gone. Where
the module still can't say a test as rustc's harness does (no source, a `should_panic` whose
`expected` is not a string), the build that needs it goes to cargo with a message naming #88. A test program built without incremental compilation or optimized,
which rustc's harness calls private or inlined copies of the tests in, was cargo's; it is now the
daemon's when blaze wrote its harness (the plan leaves the reason in the program's directory,
`tests`, for the daemon to refuse with when it wrote none). A library's file a unit test is linked with as rustc built it (without
`cfg(test)`) was guarded by its text without the items marked `#[test]` or `#[cfg(test)]`,
every other token at its line and column: a change to its tests alone kept rustc's library,
and cargo's dep-info of it was made newer than the file so cargo did not build it again. On the
fixture "a test added" went from 5 rustc runs to none, so tests went from 12 to 7. The 7 are
the first `test --no-run`, cargo building the test programs and the libraries an edit before
changed, and the proc macro the worker's load compiles: the cold build's (K). Since
2026-10-25 any other change to such a file is no longer cargo's: the unit tests' worker loads
each library of the workspace as two crates, the library without `cfg(test)` and its unit tests
with it, so an edit to a library a unit test links is the daemon's like any other, which takes
the library over from rustc (`widen`) built as rustc built it. A change to its tests alone still
has cargo's dep-info made newer, so a later `cargo test --doc` does not have rustc build the
library again. On ripgrep an edit to `grep-matcher` went from cargo
running all 1168 tests to the daemon. An edit to a crate
an integration test's package depends on but the test does not link no longer takes that crate
over from rustc (`widen` keeps to the crates rustc linked): its functions were named as blaze's
and never compiled, and the link failed. On the bench apps (`harness/norustc/count.py`, dev-aws-11,
2026-10-23), "test-added" went from 18 rustc runs to none on ripgrep and from 11 to none on
nushell; "test-edit" is as before (0.2 s and 5.3 s).

Then (the `no-rustc-l2` lane, 2026-10-23) blaze's harness is always written, not only once
the tests differ from rustc's, so no program of tests depends on rustc's harness. Written at
the first edit, as it first was everywhere, it made that edit 0.4 s on ripgrep against 0.2 s:
the edit wrote the harness into seven libraries' root files, which took those libraries over
from rustc in every program that links them, and built two programs whole. On dev-aws-11 against
main, ripgrep: first `test --no-run` 4.0 s (3.9), "test-edit" 0.2 s (0.2), "test-added" 0.7 s
(1.0); nushell: first `test --no-run` 11.8 s (11.7), "test-edit" 1.4 s (4.3, and 4.6 with
rustc's harness on the same main), "test-added" 1.5 s (1.9).

**I. Build scripts compiled and run by blaze.** ([#89](https://github.com/indexable-inc/blaze/issues/89)) The daemon compiles, links and runs build
scripts with `RUSTC` naming blaze, so that `autocfg`, `version_check`, `rustversion` and the
feature probes of `anyhow` and `proc-macro2` are answered by blaze (B for their questions, D's
check for their test programs). rust-analyzer's `cargo check --compile-time-deps` is replaced by
the daemon's own run. Risk: a probe answered differently from rustc turns a feature off
silently; the harness compares every script's output with what it prints under rustc.

First part done (the `no-rustc-l2` lane, 2026-10-23): when cargo asks rustc for a build script,
blaze compiles and links it, and cargo runs it as it would rustc's. `crates/blaze-cargo/src/cargo/own_script.rs`
describes the script and every library it links as a `rust-project.json` (each crate's root,
edition, `--cfg`s, `debug_assertions` as rustc sets it, the variables of `env!` cargo set, and
its `--extern`s), from cargo's arguments and the wrapper's records of those libraries, and runs
blaze's build with nothing of rustc's over it (step G: every crate from source over the
standard library's unit from the store, linked by `cc`), with `--dep-info` writing the files of
the script's modules where rustc would have. To see the scripts of packages outside the
workspace, every crates.io package's among them, `blaze` names its wrapper as cargo's
`RUSTC_WRAPPER` as well as its workspace wrapper (`BLAZE_EVERY_CRATE`), unless the user has a
`rustc-wrapper` of their own; cargo keys no unit by that wrapper, so plain cargo's builds stay
this one's. A crate outside the workspace that is not a build script goes to rustc as before,
and a library of one is described (`crate.json` in its unit's records) for the scripts that link
it. A run of a script (`blaze-engine build-script`, `cargo/script.rs`) is kept in the store, keyed by
the script's executable, its directory, its arguments and its environment but the jobserver's:
what it printed and the files it left in `OUT_DIR`, and the contents of each file it named with
`rerun-if-changed`. A later run with the same key and those files unchanged prints and writes
that without running; a script that names nothing it reads is not kept, since cargo runs it
again for any change to its package. rustc still compiles a script, and blaze says why, when it
links a proc macro (J), when a library it links was compiled before blaze described it, when it
reads a variable cargo sets for rustc alone, or when it calls a function blaze could not
compile.

A script's compile needs the standard library's unit, which the store has once per toolchain
(and machine, until the remote has it): the first build on a new store waits for it. The
scripts of a cold build ask for it at once, so the store has one process make it while the
others wait (`Store::making`); before that, ripgrep's seven first scripts each built it, 10 s
apiece. A crate's frozen def map is no longer keyed by where cargo is (`CARGO`), which differs
between a script's compile and the daemon's, so a script finds the standard library resolved
in the store (0.25 s rather than 2 to 5 s). On dev-aws-11 (`harness/norustc/count.py`, against
main 24be505), "cold": with a store that has the standard library's unit, ripgrep 4.6 s (main
4.5), nushell 27.2 s (25.6 to 25.9); with an empty store, 11.2 s and 37.3 s. rustc compiles
the build scripts of the cold build: ripgrep 7 times before, 1 (crossbeam-utils, `include!`);
nushell 50 before, 5 (crossbeam-utils and portable-atomic, `include!`; bigdecimal, `env!`;
nu-cmd-lang and oem_cp, a proc macro). Done since (2026-10-27, the `ctdeps` lane): rust-analyzer's
`cargo check --compile-time-deps` replaced, see K. Not done: the harness that compares every
script's output with rustc's. For
the F+G lane: the printed lines a library is linked with are where cargo keeps them
(`cargo::script::printed`), whether blaze compiled the script or not, and a script blaze
compiles is one more crate of a `rust-project.json`, so the standard library's scripts can take
the same path.

Then (the `no-rustc-probes` lane, 2026-10-27) a script's `RUSTC` is blaze, so what it asks
rustc is blaze's to answer and no rustc runs (`crates/blaze-engine/src/as_rustc.rs`). Every run,
cargo's and the daemon's, has `RUSTC` naming blaze linked as `rustc` beside the run
(`script_sandbox.rs`), the toolchain's rustc in `BLAZE_SCRIPT_RUSTC`, and no
`RUSTC_WRAPPER` or `RUSTC_WORKSPACE_WRAPPER`, as with no wrapper configured (a script runs
`$RUSTC_WRAPPER $RUSTC`, and the wrapper would run rustc). `-vV` and `--print` are B's answers;
a probe (`autocfg`'s `--emit=llvm-ir` of standard input, `anyhow`'s and `thiserror`'s
`--emit=dep-info,metadata` of a file with `#![feature(error_generic_member_access)]`,
`rustix`'s `--emit=metadata -o`, `proc-macro2`'s) is checked as `blaze check` checks a
crate (D's verdict, `check::once`), and passes with the files asked for written (empty: the
scripts only see that they are there) or fails with the errors. What blaze can't answer that
way fails the probe with an error naming the command: a flag it does not know, `-D`, a probe
that links or names a library, or a `#![feature]` that is not in the unstable book
rust-analyzer was made with (rustc's own list of features is not shipped). B also answers
`-Clinker-features=-lld`, which cargo passes to its questions of rustc when configured to.
On dev-aws-14, blaze's own `blaze build -p blaze-engine --release --features llvm` (main
b11a8eb): every script printed what it prints under rustc (51 packages, `anyhow`, `thiserror`,
`proc-macro2`, `rustix`, `num-traits`, `memoffset`, `libc`, `serde`, `rustversion` among them;
the one difference is `blaze`'s commit), and of the rustc runs the probes and questions are
gone: 22 `rustc -vV` and `--version`, 10 probes and 5 `--print` before, none after (what
is left is the compiles of rust-analyzer's load, 82 against 119). A probe takes about 3 s, most of it loading the standard library. Risk: a probe
rust-analyzer accepts that rustc refuses (an error that is not among D's sure ones, an unknown
`extern crate`) turns a feature on; one it refuses that rustc accepts turns it off.

Then (the `no-rustc-l3` lane, 2026-10-23) a script that uses `include!`, `include_str!`,
`include_bytes!`, `env!` or `option_env!` is blaze's too. Its dep-info names what rustc's
does: the files the macros read beside the modules' (`include!`'s from the macro calls,
`include_str!`'s and `include_bytes!`'s as rust-analyzer reads them, recorded by crate), and a
`# env-dep:NAME=VALUE` line for each variable read, as rust-analyzer read it for the crate,
so cargo builds the script again when one changes. A crate's content id has the files its
`include_str!` and `include_bytes!` read, so the store gives no code compiled from another
version of one. Only a variable cargo sets for rustc alone (which the build blaze runs is not
given, so it would read it otherwise) still sends a script to rustc. On dev-aws-11, against
main cd48ca75f7 with the library shipped: ripgrep cold 39 -> 38 rustc runs (crossbeam-utils'
script), 4.4 -> 4.6 s; nushell cold 493 -> 490 (crossbeam-utils', portable-atomic's and
bigdecimal's), 27.7 -> 28.0 s.

Running scripts without cargo (the `blaze-scripts` lane, 2026-10-25, for K and
[#298](https://github.com/indexable-inc/blaze/issues/298)) starts with
`crates/blaze-build-script`, a crate that knows nothing of blaze and asks no cargo. Its
`env` gives the whole environment cargo runs a script with, ported from cargo's
`custom_build.rs` and `compilation.rs` at 5f94df478 (the commit `cargo 1.99.0 -vV` names, the
pin then; `harness/scripts.py` holds it to the pinned nightly's cargo now):
cargo's own environment with `OUT_DIR`, `TARGET`, `HOST`, `NUM_JOBS`, `OPT_LEVEL`, `DEBUG`,
`PROFILE`, `RUSTC`, `RUSTDOC`, `RUSTC_LINKER`, the wrappers (the workspace's for members
alone, taken away when cargo uses none), `CARGO`, `CARGO_MANIFEST_*`, `CARGO_PKG_*`,
`CARGO_FEATURE_*`, `CARGO_CFG_*` (values joined by commas, `CARGO_CFG_FEATURE` always,
`debug_assertions` from the profile and not from rustc), `CARGO_ENCODED_RUSTFLAGS` with
`RUSTFLAGS` taken away, `DEP_<links>_<key>` from the direct dependencies with `links`, `[env]`
(never over what cargo sets, nor over cargo's environment unless forced), the jobserver's
`CARGO_MAKEFLAGS`, and the host's dynamic library path as cargo builds it. A script is not
given `CARGO_CRATE_NAME`, `CARGO_BIN_NAME`, `CARGO_PRIMARY_PACKAGE` or `CARGO_TARGET_TMPDIR`,
which are rustc's. What it is computed from is a plain description of the run (`unit::Unit`:
the package's fields, features, profile, the target's cfgs, the dependencies' metadata), for
blaze's manifest reader and resolver to fill.

Its `output::parse` reads what a script prints as cargo's `BuildOutput::parse` does, with
cargo's error messages: `cargo:` and `cargo::` (the latter refused, with cargo's suggestion,
for a package whose `rust-version` is older than 1.77), an unknown `cargo:` key as metadata
(`cargo:error=` among them, which only `cargo::` makes an error) and an unknown `cargo::` one
refused, `rustc-flags` with `-l` and `-L` alone, `rustc-link-lib`, `rustc-link-search` (those
in `OUT_DIR` passed first), `rustc-link-arg` and its forms for bins, one bin, tests, benches,
examples and cdylibs (refused for a target the package lacks, a cdylib's only warned),
`rustc-cfg`, `rustc-check-cfg`, `rustc-env` (`RUSTC_BOOTSTRAP` refused unless cargo's
environment allows it), `rerun-if-changed`, `rerun-if-env-changed`, `warning`, `error` and
`metadata`. `apply` says who receives it: which scripts each unit is linked with (cargo's
`build_map`), what rustc is given for each target (`-l` for the package's library, or its
programs when it has none; link arguments for their targets and the package's own, a cdylib's
from its dependencies too; cfgs and variables for every target), and when a script runs
again (newer files below what it watches, a variable whose value changed in cargo's
environment or `[env]`, any file of the package when it named nothing).

Its `run` runs a script as cargo's `build_work` does: in the package's directory with no
input, `OUT_DIR` and the directory around it made first, `invoked.timestamp` written; the build
fails on a failed exit (in cargo's words, with the script's outputs) or a line that starts with
`cargo::error=` as it streams by (one indented is shown and fails nothing, as in cargo), and
once it succeeds cargo's `output` (dated when the run began), `stderr` and `root-output` are
written beside `OUT_DIR`, where cargo and blaze read a run back from. `Invocation` is the
program, directory and environment, for a sandbox to run the same; the jobserver is the
caller's (`Client::configure`). `env::cargo_started` is cargo's environment once it has
started: on Linux cargo's libgit2 sets `SSL_CERT_FILE` and `SSL_CERT_DIR` to the certificates
openssl-probe finds, and every script inherits them.

`harness/scripts.py` holds the crate to cargo. The pinned nightly's cargo checks each project with the
harness as its `RUSTC_WRAPPER`, which puts the crate's program where rustc wrote each build
script, so that cargo's run records what cargo gave it and then becomes the script. Each run
is described from `cargo metadata`, `cargo check --unit-graph` and the configuration, and the
crate computes the environment (against what cargo gave), reads cargo's `output` (against
cargo's `build-script-executed` message) and runs the script again into an `OUT_DIR` of its
own (against cargo's run: both fail, or both come to the same output). Status on dev-aws-8,
2026-10-25, at a3bc9d6e01, over the applications of `apps/apps.json` and `harness/scripts/`
(openssl-sys, zstd-sys, libz-sys, libgit2-sys, ring, rustix, num-traits, and openssl,
zstd-safe and libgit2-sys reading their metadata): 463 runs of 22 projects, environment 463/463, reading
460/460, runs 463/463, the same bytes 455/460 (cc prints the variables it looked up as often as
its threads happen to look). Three scripts fail under cargo for want of a system library
(openssl-sys twice, yeslogic-fontconfig-sys) and fail the same under blaze. The harness found
two things the port missed: a `cargo check` has no artifact directory, so `target/debug` is
not in the library path, and the certificates above. Not yet: the description filled by
blaze's own manifest reader and resolver (the harness fills it from cargo), and
`cargo/script.rs`, `inputs.rs` and `link.rs` reading output with this crate. The scripts cargo
runs under blaze, and the daemon's reruns, run inside `blaze sandbox` since 2026-10-26
(`cargo/script_sandbox.rs`, #147), their runs kept in the store under the keys of #105, with
what an undeclared script read; this crate's own runner, which only the harness calls, does not
yet.

**J. Proc macros compiled and loaded by blaze.** ([#90](https://github.com/indexable-inc/blaze/issues/90)) The proc-macro crate and its dependencies are
compiled by blaze into a library that rust-analyzer's proc-macro server, running inside blaze,
loads. `proc_macro` is blaze's from F, so the bridge matches the server, which is built from the
same rust-analyzer. [#38](https://github.com/indexable-inc/blaze/issues/38)'s expansion cache
makes recompiling a macro rare. Risk: the bridge's ABI between the library and the server.

The plan (the `no-rustc-l3` lane, 2026-10-23). What stands in the way, read in the vendored
rust-analyzer: the toolchain's server (`rust-analyzer-proc-macro-srv` in the sysroot's
`libexec`, which `ProcMacroServerChoice::Sysroot` starts) opens a library with
`rustc_metadata::locator::get_proc_macros`, which reads rustc's metadata in the library's
`.rustc` section: the rustc version it was compiled by and the stable crate id that names the
`__rustc_proc_macro_decls_<id>__` static, the list of the crate's macros. blaze writes no
rustc metadata and will not, so a library blaze compiles needs a server of its own. The
server's side of the bridge (`proc_macro::bridge::server`) is unstable and blaze is built
on stable, so blaze cannot link it into itself; it can compile it, as it compiles any crate,
from the toolchain's `library/proc_macro` and the release's `proc-macro-srv` (below). A server and a
library both compiled by blaze from the one `proc_macro` source agree on the bridge by
construction (its `Buffer`, `Closure` and `Client` are `repr(C)` with `extern "C"` functions,
but each toolchain encodes the messages its own way). The stages, each lowering a count:

- J0. A build script that links a proc macro is compiled by blaze, the macro expanded with
  the library rustc compiled of it (cargo names it in `--extern`) by the toolchain's server, as
  the daemon does. The wrapper describes a proc macro of a package outside the workspace as it
  does a library, and the script's description has the macro and its libraries, so what the
  store keeps of the script is keyed by them. Done: on dev-aws-11 nushell's cold build went
  from 490 rustc runs to 488 (nu-cmd-lang's and oem_cp's scripts), 28.0 -> 27.5 s; the norustc
  fixture's outer script now expands the workspace's proc macro and stays at 5 (6 without).
- J1. blaze's proc-macro server: `proc-macro-srv` and `proc-macro-srv-cli` built by blaze
  with nothing of rustc's (G) over the toolchain's `proc_macro`, kept in the store by
  toolchain and shipped with the standard library (above), started by `load_config` in place of
  the toolchain's. It opens a library with `dlopen` and reads its macros from a static blaze
  wrote (J2).
- J2. Proc-macro crates compiled by blaze into a library for J1's server: the crate and its
  libraries from source as code that can be loaded (position-independent, the standard
  library's unit built so too), the list of macros written into the crate's root as
  `test_harness.rs` writes a test program's `main`: a `#[no_mangle]` static of each
  `#[proc_macro*]` function's kind, name, helper attributes and
  `proc_macro::bridge::client::Client::{expand1, expand2}`, linked with `cc -shared`. Used where
  every crate that expands with the macro is blaze's: a build script's macros (J0's then need
  no rustc library of their own when nothing else of rustc's names them) and the daemon's
  builds. Count: the proc macros of scripts, and with K every proc macro and the libraries only
  they link (syn, quote, proc-macro2, ...), which is most of what rustc compiles in a cold build
  of a crate with derives.

What the pinned toolchain changes in that plan (read 2026-10-24 in 1.99.0's `library/proc_macro`
and in rust-lang/rust at b940084d7, the commit `rustc 1.99.0 -vV` names, while 1.99.0 was the
pin; what changed with the move to `nightly-2026-10-25` follows the list):

- A library rustc compiled cannot be served by any server but one that reads rustc's metadata.
  1.99.0's `proc_macro::bridge::client::Client` is the `run` function and nothing else, and
  there is no `ProcMacro` enum any more: the `__rustc_proc_macro_decls_<id>__` static is the
  macros' clients alone, and their names, kinds and helper attributes are only in the
  metadata, where `rustc_metadata::locator::get_proc_macros` pairs them up. rust-analyzer's
  old loader (`libloading` and the symbol, upstream until 2025) worked because the static then
  held them. Decoding rustc's metadata would mirror its private, per-release encoding, which
  blaze does not do. So J1's server serves only libraries blaze compiled, whose list blaze
  writes, and J1 and J2 land together. They are checked against the toolchain: every macro
  call of the bench apps and of rust-analyzer's `proc-macro-test` is expanded by the
  toolchain's server with rustc's library and by J1's with blaze's, and the outputs are held
  equal.
- The vendored `proc-macro-srv` follows rustc's development branch (it uses
  `bridge::IdentKind`, which 1.99.0 does not have), so it does not compile against 1.99.0's
  `proc_macro`. The server that does is the one the release ships, rust-lang/rust's
  `src/tools/rust-analyzer` at the release's commit, whose loader is
  `rustc_metadata`'s too. J1's source is that server pinned to the release, as
  `crates/rustc_abi` is rustc's crate at the pinned commit, with its loader replaced by the one
  that reads blaze's static. Its protocol is the toolchain server's: rust-analyzer already
  talks to that release's server today.
- Building the server needs serde's derives expanded (`proc-macro-api`'s messages), so the
  first server is built with what expands them today (rustc's `serde_derive` and the
  toolchain's server), once per release when `blaze-engine std` ships it; once J2 compiles
  `serde_derive`, each server is built with the one before it.

With the pin a nightly (2026-10-25), the rule behind the second point is the same and simpler:
a server and a library agree on the bridge only when both are built from one
`library/proc_macro`, so `vendor/proc-macro-srv` is `src/tools/rust-analyzer`'s crates and
`compiler/rustc_lexer` at the pinned nightly's commit (12433c307), which has
`bridge::IdentKind`, with blaze's changes carried over, and `harness/toolchain_bump.py` takes
it again at each bump.

Where J stands (2026-10-24):

- The static J2 writes compiles. `harness/allblaze.py`'s `macros` program holds
  `[Client::expand1(..), Client::expand2(..)]` in a `#[no_mangle]` static and is built with
  nothing of rustc's whole, over the standard library's unit and from the store: const
  evaluation reifies each client's closure into its `extern "C"` function as rustc does
  (`bridge::selfless_reify`), and the bridge it reaches, `proc_macro`'s symbol interner and its
  thread-local included, is compiled with the program. `proc_macro` (and `test`) are not in
  the unit, so they are the program's when it links them; the unit of a program that links
  `proc_macro` has `rustc-literal-escaper` too, and is shared by every such program.
- blaze links a shared library (`--exe` with `--shared`): the object carries the allocator
  shim, the link is `-shared -Wl,-Bsymbolic`, and only the crate's `#[no_mangle]` and
  `#[export_name]` items are exported, as rustc's `cdylib` is, so the rest is left out where
  they do not reach it. `allblaze.py` loads one with `dlopen` and calls it as cargo's library
  answers, whole, over the unit and stored.
- A proc-macro crate linked with `--shared` has the list of its macros: blaze adds a module
  to its root as it does a test harness (`crates/blaze-engine/src/macro_list.rs`), with an exported
  `static BLAZE_PROC_MACROS: &[Macro]` of each macro's kind (0 derive, 1 attribute, 2
  function-like, as blaze-expand numbers them), name, helper attributes and `Client`, a
  `#[repr(C)]` record. `allblaze.py` reads it off the library. rust-analyzer had taken a
  proc-macro function out of its crate's value namespace, so `crate::derive_fn` did not
  resolve; it is a function in its crate now, and a path from another crate sees only the
  macros.
- The release's server is `vendor/proc-macro-srv`: rust-lang/rust's `src/tools/rust-analyzer`
  crates the release's `rust-analyzer-proc-macro-srv` is built from, and `compiler/rustc_lexer`,
  at the pinned nightly's commit (12433c307 since 2026-10-25; 1.99.0's before), with that
  commit's `Cargo.lock` cut to these crates. What rustc's tree gave it is replaced, as its
  manifest's header lists: the loader `dlopen`s a library and reads blaze's list where the
  release reads rustc's metadata, `rustc_proc_macro` is the standard library's `proc_macro`,
  `rustc_lexer` is the vendored crate, and the version string and the commit are written down
  where build scripts ran rustc and git. blaze builds it with nothing of rustc's (`--exe`),
  over rustc's `serde_derive` and the toolchain's server, the bootstrap above.
- `BLAZE_PROC_MACRO_COMPARE` (`crates/blaze-expand/src/compare.rs`) names that server, and
  `BLAZE_PROC_MACRO_LIBS` a directory of blaze's libraries: each call the toolchain's server
  expands with rustc's library is expanded again by blaze's server with blaze's library of
  the crate, and the outputs, spans included, are compared. `harness/macroserver.py` builds
  the server and the libraries of serde_derive, thiserror-impl, tokio-macros and clap_derive,
  and holds every call of a package of derives with helper attributes, attributes and
  function-like macros equal (on dev-aws-11: the server linked in 10.6 s, each library in
  11-14 s). A proc-macro crate has no entry point: rust-analyzer took tokio's
  `#[proc_macro_attribute] pub fn main` for one.
- In a load (2026-10-25, the `blaze-j1` lane, behind `BLAZE_PROC_MACRO_SERVER`, which names
  the server): rust-analyzer's `cargo check --compile-time-deps` into blaze's own target
  directory (`--own-target`) asks the wrapper for each proc macro, and blaze links it there
  (`crates/blaze-cargo/src/cargo/own_macro.rs`), as `own_script.rs` does a build script, with the
  dep-info cargo reads; the load expands what blaze listed with that server
  (`load_cargo::listed`), and a build script that links one is expanded with it (J0). A
  library blaze links names libc, as rustc's does. A crate rustc compiles there that links
  one (a proc macro that uses another, as `serde_with_macros` uses `darling_macro`, or a
  build script's dependency) has rustc compile the proc macro again over blaze's, as rustc
  loads only its own; blaze's is kept beside its record. `macroserver.py all` builds fd,
  bat, hyperfine, just, nushell and helix whole that way and again with rustc's proc macros
  and the toolchain's server, each from an empty store, and expands every call with both
  (on dev-aws-8): 3,465 calls (3,192 derives, 149 attributes, 124 function-like), all the
  same, and the programs print the same. Left: `pest_consume_macros` (nushell, 12 calls),
  which blaze cannot compile
  ([#327](https://github.com/indexable-inc/blaze/issues/327)).
- Nothing of a load is rustc's (2026-10-27, `no-rustc-load-1027`): into blaze's own target
  directory the load's wrapper (`cargo/load_wrapper.rs`, `for_load`) has blaze compile and link every
  build script (I) and every proc macro, and a library is only described for those that link
  it (`crate.json`): cargo is given empty files where rustc would have written the library and
  its metadata, and a dep-info that names every Rust file of the crate's directory, so that it
  builds again what links the crate when one changes. Every crate that links a library there
  is blaze's and compiled from the descriptions, so nothing reads those files, and no proc
  macro is compiled again by rustc. A script or proc macro blaze cannot build, or a proc
  macro with no server beside blaze, fails the load with why. A crate cargo built before the
  directory was seeded from it is described from cargo's records. `BLAZE_PROC_MACRO_SERVER=0`
  is kept for `macroserver.py`'s reference build alone, which rustc builds whole, into a
  directory of its own (`target/rust-analyzer-rustc`): cargo does not fingerprint the
  variable, and in one directory rustc read the empty files (#460). The variable is part of
  which build and worker it is, so the daemon never keeps the mode it was started with. On blaze's
  own release build this took the load from about 100 compiles by rustc (54 scripts, 28
  libraries, 18 proc macros compiled again) to none.
- Shipped (2026-10-25): on Linux the release workflow's blaze builds the server
  (`build_release.py --server`, `--exe` with `proc_macro` compiled into it) into the archive
  as `blaze-proc-macro-srv`, beside `blaze`, which `BLAZE_PROC_MACRO_SERVER=1` names. The
  load gives its cargo the server it found, or takes the variable away when none is there
  (and says so), so the wrapper links a proc macro only for a server there is. The
  toolchain's server is started only when a library is not blaze's: a load of fd starts
  none.
- The default (2026-10-25): with the server beside `blaze`, every load uses it with no
  variable (`BLAZE_PROC_MACRO_SERVER=0` turns it off). A load whose proc macros cargo built
  with rustc, the daemon's from cargo's messages, the checker's and a build script's (J0),
  has blaze make each library from the crate's record (`blaze proc-macro-library`, four at
  once) and expands with that; one blaze cannot make is said and left to rustc's library
  and the toolchain's server. A library is kept in the store by the description of what it
  was linked from and the digests of the files its dep-info names, so a second target
  directory takes it from there. `macroserver.py all` builds each app a third time, into an
  empty target directory over the first build's store, and fails if a library is linked
  again: 3,465 calls the same on all three, every library from the store the third time
  (hyperfine 35.1 s to 9.5 s, nushell 123.0 s to 79.3 s). On fd, the daemon's load started
  blaze's server and no other, and made `clap_derive` in 19 s from an empty store, then took
  it from the store when it was deleted. Left: #327, and the key names the target
  directory's paths, so another checkout of the same crates links again.

J, K and M are planned together in [self-contained.md](self-contained.md#j-k-and-m), since
what they remove is most of the toolchain blaze still keeps: rustc and the sysroot's rlibs (116
of its 132 MB on Linux x86-64) go only once all three are in.

**K. The cold build is blaze's.** ([#91](https://github.com/indexable-inc/blaze/issues/91)) `blaze build` with nothing built plans the units
(plan.rs), and the daemon compiles them all, taking what the store and the remote have, links
the program with G's line and patches its own executable from the first edit on. No `cargo
build`, no rustc base. See [the cold start](#the-cold-start-without-rustc).

- Done (2026-10-25, the K lane): with `BLAZE_WHOLE_BUILD=1`, `blaze build` and `run`
  from nothing take the programs from cargo's unit graph and build each with G's whole build,
  putting it where cargo does; no `cargo build` runs, and a function the program can reach that
  blaze cannot compile fails the build with which and why ([cargo.md](cargo.md#what-a-build-does)).
  `harness/coldbuild.py` holds a fixture's programs, an example and a build script's output equal
  to cargo's. Measured on the bench projects ([self-contained.md](self-contained.md)): ripgrep,
  fd, hyperfine, just, bat and nushell build whole and run as cargo's builds; axum and biome missed
  a build script's variable, until the loads read the graph of the build with blaze's own
  resolution ([resolve.md](resolve.md), 2026-10-25): axum then builds, and biome reaches 9
  functions the frontend turns down (two LLVM intrinsics of `num-bigint`, `HasErrors` and
  unresolved methods in `boa_engine`, `boa_interner` and a derive of `biome_deserialize`). Build scripts and proc macros are still built by rustc for the
  load.
- Done (2026-10-25): the whole build is a request to the daemon (#91's design). Its worker
  loads the workspace with the build's own resolution, compiles and links each program, and
  answers only once the executable is linked; the next `blaze build` asks the same
  worker, which builds what was edited since on what it has loaded and links the program
  again. No spare worker is started for it: the build asks at once, and a spare loads its
  library first (ripgrep's cold build 7.3 s with one, 4.5 s without). The objects of a whole
  build keep each function in a section of its own, as a build of no daemon's does, so that
  the link leaves out what the program does not reach; the store keys the standard library
  by which it is.
  ripgrep on dev-aws-14 (shared): an edit to a body 2.3 s, cargo's 0.47 s. The edit compiles
  2 functions, and the rest of the 2 s is the full object written again and the link.
- Done (2026-10-25): the edits are patches spliced into the executable the worker linked, as
  the daemon's edits of cargo's builds are. The worker is given `--patch` and links with G's
  line (`link::command`), over the standard library's unit of its own, which a build that
  patches its library into the program's object leaves out (`patches_its_library`). On
  dev-aws-14 (`harness/coldbuild.py bench`): an edit to a body of ripgrep 0.19 s (cargo's
  0.46 s), of axum's example 0.25 s (0.35 s), of fd 0.22 s (0.44 s); of a signature, fd
  0.37 s (0.46 s), ripgrep 0.25 s (0.58 s), nushell 0.99 s (5.16 s). A whole build takes no
  crate over from rustc on an edit (`Program::widen`): every crate is the worker's already,
  and taking one over wrote the whole object again (ripgrep's signature edit 2.9 s before).
- Done (2026-10-27, the `ctdeps` lane): a load into blaze's own target directory builds and
  runs its build scripts and proc macros without cargo. rust-analyzer's `cargo check
  --compile-time-deps` was cargo scheduling compiles that were already blaze's; now
  `cargo/compile_time.rs` walks the build's unit graph (plan.rs, the same answer the plan
  reads, asked once per load), compiles each build script with `own_script.rs` from a
  description of the libraries it links (features, `debug_assertions`, the flags' `--cfg`s, what
  their own scripts printed, cargo's variables for rustc), runs it through `blaze-engine build-script`
  with `blaze-build-script`'s environment (`DEP_<links>_*` from the scripts it runs after) and
  reads what it printed as cargo does, and links each proc macro for blaze's server. It fills
  project-model's `OwnBuildScripts` hook, as `OwnMetadata` does for `cargo metadata`; what it
  cannot build or run fails the load, naming the package. `harness/resolve.py ctdeps`
  ([resolve.md](resolve.md)) holds what the load reads (cfgs, variables, `OUT_DIR`'s files,
  the proc macros) to cargo's: equal on 10 of the 14, and on fd too with the build-script
  sandbox off, whose environment is not cargo's ([resolve.md](resolve.md)); biome
  stops at a syn-1 proc macro blaze cannot compile (#327), and zed and bun fail for both, at a
  program or path package the machine lacks. A cold `blaze build` or `test` of the fixture or
  ripgrep now starts no cargo but the typed one ([the counts](#self-contained-no-rustup-no-cargo)).
- Next: the cold builds, which are slower than cargo's but axum's (dev-aws-14, quiet,
  2026-10-26: ripgrep 4.8 s against 4.0 s, fd 22.1 s against 16.8 s, bat 12.6 s against
  7.0 s, hyperfine 7.0 s against 3.8 s, just 11.0 s against 5.5 s, axum 6.1 s against 6.1 s,
  nushell 49.3 s against 23.8 s): fd's load waits 16.7 s on jemalloc's build script, which
  cargo builds beside the crates; biome expands its proc macros in 149 s and reaches 17
  functions the frontend turns down. Then the shipped standard library for the dev and
  release program units, and the flip.
- nushell's cold compile (2026-10-26, dev-aws-14, `BLAZE_STATS=1`): the store's entries no
  longer list a crate per thread (each crate once, outside the lock, a name of exactly one
  root taken as it, statics listed beside the compile), and their threads' wall went from
  310 s to 48 s. What is left of the 24 s compile is the object thread's ~6.4 s of waiting for
  roots: `main` alone takes 568 ms first, many small functions 300 to 900 ms each as they wait
  on queries they share, and from 4.25 s to 5.9 s one worker runs `add_default_context` and
  `add_extra_command_context` (bodies of 1389 calls, 0.87 s, and 1155, 0.81 s) before the 673 roots
  they name arrive; from 8.5 s to 12 s a few workers finish the tail. Finding those callees
  before a function is translated needs them read off its MIR first, which is not cheap.

**L. Doctests by the daemon.** ([#92](https://github.com/indexable-inc/blaze/issues/92)) [#28](https://github.com/indexable-inc/blaze/issues/28): the
examples are compiled by the daemon that has the workspace loaded, linked with G's line; no probe
program, no process per library, no rustdoc. `compile_fail` examples need D.

Done first (the `no-rustc-hl` lane, 2026-10-23): what rustc compiled of an example, and the
probe, stay in `<target>/<profile>/blaze-doctest`, each under a key of everything it was
given (the arguments, the source, the edition, the variables set for it, and the libraries
named with `--extern` by length and time written), and are taken again while every file and
variable its dep-info names is as it was. Cargo builds a library again when one it is built
against changes, so the `--extern` libraries stand for every library the program read. A
library's doctests run a second time with nothing changed run no rustc; after an edit to the
library, every example of it is compiled again. The fixture's doctests went from 6 to 3.
`harness/doctests.py` runs blaze twice on each crate and holds both runs to rustdoc's, and
the flag check's edit changes a constant an example reads, which a stale program would show.

Done second (the `no-rustc-l2` lane, 2026-10-23): blaze compiles and links the examples, with
no probe. The doctest process loads the library as before, and adds each example to
rust-analyzer's graph as a crate of its own: one file at the path rustdoc shows (so a panic's
location is rustdoc's), its text moved down to the line it starts at, depending on the library
and the library's dependencies, built with `cfg(doc, doctest)` and the example's edition. Each
is compiled as a program over the standard library's unit (G's, from the store or built once),
then linked with G's line by its own thread, run, and removed, while the next is compiled. An
example that is only to compile passes when rust-analyzer finds no error in it. rustc still
builds an example that is `compile_fail` (D), one rust-analyzer finds an error in (rustc
prints it), one naming a dependency that links a second copy of the library (two copies of a
trait rust-analyzer takes for one), and one whose build, link or stand-in check fails; the
memo of the first part keeps those. A program that denies lints (a line `#![deny(...)]` or
`#![forbid(...)]`, as `doc(test(attr(deny(warnings))))` gives every example of chrono) also
goes to rustc when rust-analyzer warns about it or it names a `#[deprecated]` item, macro
arguments included: rustdoc fails such an example, and blaze would pass it. The fixture's
doctests went from 3 to 0. On the 30 crates of `harness/doctests.py` (3,095 examples) blaze
builds 2,748 and rustc 340, every crate's results the same as rustdoc's (since the frozen def
maps, url's and textwrap's second runs panic before any example is built, on main too:
[#161](https://github.com/indexable-inc/blaze/issues/161)); most of rustc's are
serde_json's 97, serde_core's 68 and hashbrown's 63, which name `serde`, linking a second copy
of the library, and `time`'s 74, `compile_fail` and others with an error. Building whole programs
of examples found two codegen bugs that rustc's copies of the libraries had hidden:
`relocate_data_pointer` followed two allocations that seemed to point at each other forever
(url), and an `#[inline]` function of the standard library that takes its caller's overflow
checks was taken from blaze's build of the standard library, which has none (itertools; the
fixture's `should_panic` example now holds it). blaze's program of an example is kept in
`<target>/<profile>/blaze-doctest` under a key of its source, its edition and the blaze
executable, and the libraries it is built against as for rustc's, and is run again while they
are unchanged; an example that names `include!`, `include_str!`, `include_bytes!` or `env!` is
compiled each time. A few examples are compiled at once, each on its own snapshot of the
database (a sixteenth of the cores, from 1 to 4).

Counted as in [How often](#how-often) on dev-aws-11 (Linux x86-64, 96 cores, Cranelift), main
at 04bdd5b against this lane:

| Doctests | ripgrep, main | ripgrep, L | nushell, main | nushell, L |
|---|---|---|---|---|
| first run | 35 rustc, 12.0 s | 4, 28.7 s | 128, 9.3 s | 6, 25.3 s |
| again | 0, 8.3 s | 0, 8.5 s | 0, 7.7 s | 0, 5.3 s |

What rustc still runs on a first run is cargo's, not the examples': ripgrep's 3 workspace
libraries and a build script; nushell's 3 proc macros and 2 workspace libraries, and one
example of nu-protocol's left to rustc. The first run is slower: the standard library's unit
is built once per store (6.6 s), and each example is a whole program, so the library's
functions it reaches are compiled again for every example (10.7 s for ripgrep's 22). What is left of
L is that cost, the daemon taking the examples in place of a process per library, and rustdoc
for `-q` and a library blaze cannot load.

Done third (the `nopin` lane, 2026-10-28): nothing of a doctest run is handed to rustc, rustdoc
or cargo. `-q` and the other output flags were handled already, and a library blaze cannot load
or whose run it cannot read fails every example with the reason; nothing goes to rustdoc. What
was left was the doctest process's load of the workspace, which ran
`cargo check --compile-time-deps` (and the toolchain's rustc under it) for the build scripts
and proc macros on every `blaze test` with doctests, cold and warm. It now loads in blaze's own
target directory (`--own-target`), as a worker of the daemon does, so the scripts and macros
are blaze's (`cargo/compile_time.rs`). Counted with `strace -f -e trace=execve` over
`blaze test` and the daemon it talks to (Linux x86-64, LLVM, main at bd4e567a73 against the
lane; execs of the toolchain's rustc, rustdoc and cargo that blaze starts, not blaze's own
`rustc` stand-in a build script is given, nor what a test program starts):

| `blaze test` | main, cold | main, warm | lane, cold | lane, warm |
|---|---|---|---|---|
| a crate of ten examples, each kind | 1 cargo | 1 cargo | 0 | 0 |
| ripgrep 15.2.0 (91f5d22) | 0 (no library, no doctests) | 0 | 0 | 0 |
| ripgrep 15.2.0 `--workspace` (23 examples) | 24 rustc, 1 cargo | 1 cargo | 0 | 0 |
| serde 1.0.229 (0db9b3b) | 0 | 0 | 0 | 0 |

serde's `compiletest` is trybuild, which runs cargo itself from the test (2 cargo, on main
and the lane); it fails under blaze, as the cargo it runs is handed blaze's rustc stand-in,
and cargo stops before the doctests as it would.

Timed on the same machine (64 cores, shared, load 6 to 15), three rounds alternating cargo,
main and the lane; cold is no target directory and no daemon, with blaze's store kept:

| ripgrep | cargo, cold | cargo, warm | main, cold | main, warm | lane, cold | lane, warm |
|---|---|---|---|---|---|---|
| `test` | 5.8 to 9.1 s | 0.9 s | 11.7 to 23.8 s | 1.0 to 1.2 s | 12.7 to 24.0 s | 1.0 to 1.1 s |
| `test --workspace` | 9.6 to 10.0 s | 3.4 to 3.5 s | 56.9 to 61.5 s | 5.3 to 8.5 s | 50.4 to 53.9 s | 5.2 to 7.9 s |

With an empty store, `test --workspace` cold took 55.4 s on main and 52.5 s on the lane. The
doctests are most of blaze's time and why it is slower than cargo: each example is a whole
program, and what it reaches of the library and its dependencies is compiled again for each
(a library of 5 to 7 examples took about 25 s under strace), and the doctest process loads the workspace again on
every run, warm too. Compiling the library once for all its examples, and having the daemon
take the doctests, are what is left.

An example marked `compile_fail` passes only when blaze's front end finds an error in it that
rustc gives too, not any error rust-analyzer reports: one of `SURE` (the errors `blaze check`
fails a build on), the borrow checker's (`BORROWCK`, unless `BLAZE_BORROWCK_REFUSE=0`), or a
lint the example denies; and every error code it is marked with has to be among them, as
rustdoc checks (`compile_fail,E0499` passes on the borrow checker's E0499). Where blaze's
checker is weaker than rustc's it can't tell, and the example fails with the reason and is
counted in the `doctests:` line ("marked `compile_fail` that blaze can't tell fail as
marked"), never passed on a guess: only errors whose code is in neither list, which rust-analyzer
reports where rustc need not (`E0277`, a bound not met, is one: rust-analyzer reports it, and
`blaze check` does not trust it; a syntax error is another), no error at all (a code it does
not check yet, a body MIR lowering does not support and the borrow checker skipped, the borrow
checker turned off), or errors without every code marked. Before, any error passed the example
and the codes were not checked.

**M. Remove the wrapper and the fallback.** ([#93](https://github.com/indexable-inc/blaze/issues/93)) Plain cargo after `blaze enable` names blaze as
`build.rustc`; blaze answers cargo's questions (B) and writes for each unit the files cargo
expects, built by the daemon. Then `blaze-rustc`, `blaze-rustc-first`, the stand-ins, the
dep-info runs, `settle`, `Built::by`, `by_cargo`, `ToCargo`, `only_cargo`, `BLAZE_RUSTC_BASE`
and `BLAZE_WRAPS_RUSTC` are deleted, and step A's count is zero.

Done (2026-10-27) another way: plain cargo builds through no blaze at all. `blaze enable` and
`disable`, the workspace wrapper (`cargo/wrapper.rs`, `cargo/switch.rs`), `settle`,
`Built::by_cargo`, `only_cargo`, `BLAZE_RUSTC_BASE` and the patching of the executable rustc
linked (`patch::Base::of_rustc`, the resolver's reading of its symbols, the splice of a whole
program into it) are deleted. `blaze` takes out what an earlier `enable` wrote, in one line
(`cargo/home.rs`). Left: `blaze-rustc` as the `RUSTC_WRAPPER` of a load into cargo's own
target directory (`cargo/load_wrapper.rs`), which refuses any other caller, and
`BLAZE_WRAPS_RUSTC` (`stub.rs`) for the harness's "alone" builds.

### The cold start without rustc

Today `blaze build` from nothing returns in about cargo's time (results.md: ripgrep 6.96
s against cargo's 6.16, nushell 37.19 against 35.51) because it is cargo's build, and the
daemon is ready for edits at about twice that (#11). The first edits patch rustc's executable
so that they need not wait for blaze's own link. Without rustc the cold build is blaze
compiling every crate, the standard library included.

That is kept fast by not compiling, rather than by compiling as fast as rustc:

- **The store and the remote.** A crate compiled once, by anyone, for a target, profile and set
  of features is fetched, not compiled. On results.md's remote measurement, ripgrep with every
  function served from the remote compiled none and was ready in 7.3 s. The standard library
  and the most used crates of crates.io are compiled ahead by blaze and served by the remote,
  so a fresh machine compiles only the workspace.
- **Frozen dependencies** ([#40](https://github.com/indexable-inc/blaze/issues/40)) and **a
  warm-start manifest** ([#41](https://github.com/indexable-inc/blaze/issues/41)): an
  unchanged dependency, or an unchanged checkout, is linked without being analysed. These
  become the cold-start path; they no longer only save memory.
- **Compiling less.** Not built, to evaluate in step K: compiling a function the first time the
  program reaches it, so that a cold build compiles what the program uses rather than every
  function of every dependency.

Measured first on hello world (dev-aws-8, 2026-10-22, three runs each, all agreeing within
0.3 s), blaze compiling the program and the standard library's 18 crates into one object:

| Build of hello world | Wall time |
|---|---|
| `cargo build`, rustc's prebuilt standard library | 0.19 s |
| `cargo build -Zbuild-std=std,panic_unwind`, the standard library from source | 15.5 s |
| blaze, every crate, no store | 5.2 to 5.4 s, then 0.12 s to link |
| blaze, empty store | 5.4 to 5.5 s |
| blaze, warm store | 5.4 to 5.5 s: 5,683 of 8,260 functions from the store |

blaze is three times faster than cargo building the standard library, and 28 times slower
than cargo taking rustc's prebuilt one. A store of functions does not close that: of the 5.4 s,
3.0 s is resolving the 27 crates (what to compile, before any is compiled) and 0.5 s loading
the workspace; compiling is 1.6 s. So F keeps the standard library in the store as one
unit, an object and the list of what it exports under blaze's own names, built once per
target and profile and served by the remote, and a program's build resolves and compiles
only its own crates and the generic functions of the library it instantiates, as rustc's
prebuilt library lets cargo do. The memory of the 5.4 s build peaks at 3.7 GB.

The unit, measured the same way (dev-aws-8, 2026-10-22, `harness/allblaze.py` and three runs
of hello by hand, agreeing within 0.05 s):

| Build | Wall time |
|---|---|
| the standard library alone, into `std.o` (53 MB, 23,338 symbols) | 15.9 to 16.1 s, 4.2 GB |
| hello, with the standard library in the same build | 4.2 s, then 0.12 s to link |
| hello over `std.o` | 3.04 to 3.09 s, 2.2 GB, then 0.37 s to link |
| the program of collections, threads and panics over `std.o` | 3.22 s, then 0.37 s to link |
| hello on an empty store and remote: the unit compiled, kept and pushed | 17.4 to 17.6 s |
| the program of collections, threads and panics, the unit from the store | 3.30 to 3.34 s |
| the same on a second empty store, the unit from the remote (a directory) | 3.34 s |

Over the unit hello compiles 138 functions in 0.15 s; 2.5 s of its 3.1 s is resolving the
library's 27 crates, which the program's types and calls are checked against, and 0.4 s is
loading the workspace. So the unit takes the library's compiling out of a program's build but
not its resolving: that is what is left between blaze and cargo's 0.19 s, and a daemon pays
it once. The link was slower because `cc` took the whole of `std.o`; with blaze's own link
line (below) it takes what the program reaches, hello's in 0.19 s.

ripgrep with every crate blaze's, linked by blaze (`--exe`; dev-aws-8, 2026-10-23, both
builds pinned to the 96 cores of one socket):

| Build of ripgrep | Wall time |
|---|---|
| `cargo build`, cold, rustc's prebuilt standard library | 5.65 to 5.68 s, 0.38 GB |
| `cargo build` after touching `main.rs` | 0.53 s |
| blaze, empty store and remote: the unit compiled, kept and pushed, then linked | 14.4 s, 6.3 GB |
| blaze, warm store, linked | 5.66 to 5.73 s, 4.4 GB |
| blaze, a second empty store, the unit and functions from the remote (a directory), linked | 5.66 to 5.72 s |

With a warm remote ripgrep is 5.66 to 5.72 s against cargo's 5.65 to 5.68: at the K bar below,
within 1%. Of blaze's 5.7 s, 2.4 s is resolving the 60 crates and 0.5 s loading the workspace;
compiling the program's 4,843 functions and the 8,920 instances the store did not have is 2.3 s
(the other 24,703 come from the store), and the link 0.25 s. The empty store's 14.4 s is the
unit's 7.6 s of compiling on top. The executable is 55.7 MB against cargo's 54.0 MB (67 MB when
the link took the whole unit). Unpinned, over both sockets' 192 cores, both blaze builds take
7.3 to 7.6 s, main's and this one's alike. Until 2026-10-22 a build also spent 6.7 s (16.5 s over the flat syntax
tree, #42) after compiling: every function the frontend turned down walked all the macro calls
of its crate, `core`'s for most of the unit's 2,000; that is now once a crate, and never for the
unit, whose refusals nobody reads.

With frozen dependency artifacts (#40, 2026-10-23, same host and pinning; each crate's
`DefMap`, impls and lang items read from the store instead of resolved, [glancer.md](glancer.md),
item 1):

| Build | Wall time |
|---|---|
| hello, warm remote, nothing frozen | 3.02 to 3.03 s, 3.4 GB: resolving 2.2 s |
| hello, warm remote, frozen | 1.22 to 1.23 s, 2.3 GB: resolving 0.33 s, loading 0.39 s, linking 0.17 s |
| ripgrep, `cargo build`, cold | 5.50 to 5.56 s |
| ripgrep, empty store and remote: the unit compiled and its 153 frozen parts written | 14.8 s |
| ripgrep, warm store | 4.28 to 4.31 s |
| ripgrep, a second empty store, everything from the remote | 4.24 to 4.29 s, 3.8 GB |

ripgrep with a warm remote is now 23% under cargo's cold build, its executable printing what
cargo's does. Of its 4.25 s, loading the workspace is 0.46 s (0.12 s of it is the rustup proxy
in front of each of the dozen cargo and rustc calls), fetching from the remote 0.2 s beside it,
resolving 0.38 s, compiling ripgrep's 4,754 functions 3.0 s and linking 0.24 s. Compiling is
longer than when resolving ran live, which lowered the impl headers on idle threads: the next
frozen family. A build waits for the remote's packs before it reads what is frozen: the store
reads its packs once, when first opened, and one opened mid-fetch missed the standard library's.

The link takes only what is reached of the unit (2026-10-23, same host). The unit is described
to line tables at most, as rustc's own standard library is (its object 64 to 52 MB), and is
kept as an archive of 17 parts: each function's sections with its unwind entry and its
exception table, and each debug unit (256 functions, its own section set) in the part most of
its functions are in, so that lld reads the parts on its threads. On one object lld read 140,000
sections one after another, as fast on one thread as on 96.

| Link | Before | After |
|---|---|---|
| hello, blaze's link | 160 to 167 ms | 130 ms |
| hello, lld alone | 113 ms | 93 ms (cargo's: 25 ms) |
| hello's executable | 20.4 MB | 15.2 MB |
| ripgrep's executable | 55.1 MB | 50.1 MB (cargo's: 53.2 MB) |
| ripgrep, warm remote, the whole build | 4.24 to 4.29 s | 4.20 to 4.28 s |

ripgrep's link stays at 0.24 s: its own 64 MB object is now most of what lld reads, and it is
written each build. The `cc` in front of lld (nix's wrappers, `collect2`) costs 35 ms more.

A checkout's own functions come from the store too (2026-10-23, same host and pinning, the
roots-store lane): a root whose code the store has whole, with all it calls, is put in the
object from it instead of compiled, the roots are asked about at once on every core, and the
crates are listed on every core. The other bench apps, with every crate blaze's and linked by
blaze, each executable printing what cargo's does:

| App | `cargo build`, cold | blaze, empty store and remote | warm store | empty store, warm remote | Executable (cargo's) |
|---|---|---|---|---|---|
| ripgrep (`rg --version`) | 5.53 to 5.76 s | 16.8 to 17.4 s | 3.98 to 3.99 s | 4.01 to 4.02 s | 49.3 MB (53.2) |
| nushell (`nu -c "[3 1 2] \| math sum"`) | 33.1 to 33.2 s, 2.4 GB | 78.9 s, 23 GB | 31.3 s, 13.8 GB | 31.5 s | 346 MB (539) |
| helix (`hx --version`) | 31.3 to 31.4 s, 1.8 GB | 87.0 to 89.7 s, 23 GB | 25.5 to 25.7 s, 13 GB | 26.0 to 26.3 s | 302 MB (426) |

With a warm remote all three are under cargo's cold build: ripgrep by 28%, nushell by 5%,
helix by 16%. Before, nushell's warm build was 48.7 s: 15,786 of its 239,163 functions were
compiled, the roots that were only called, which compiling ahead did not ask the store about;
now 4 are. Of nushell's 31 s, resolving is 3.8 s, asking the store about the roots 3 s (its 32 MB
of unpacked blocks thrash), putting the functions in the object 4 s, describing them 4 s,
writing the object 7 s, listing 4.7 s and the link 1 s. An empty store is slower than cargo by
2.4 to 3x: it compiles what the store then has, and keeps each root whole, which costs nothing
measurable against main's build (helix 87 s against main's 92 s, ripgrep 17.3 against 16.8 s).
helix is built with `HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1` for cargo and blaze alike: its build
script fetches the tree-sitter grammars, which the executable does not contain. bevy
(`breakout`, cargo 78.2 s cold, 8.2 GB) does not build yet: its `main` is left to rustc
(`UnresolvedMethod("chain")`), and the SSE compares glam calls are not lowered in this build.

#### The standard library, shipped

Build scripts by blaze (step I, #186) put the standard library on every cold build's path: a
script is a program with nothing of rustc's, so the first one builds the unit and the frozen
parts of the library's crates, 7.5 s on 96 cores, while the others wait on it. On an empty
store ripgrep went from 4.5 to 11.2 s and nushell from 25.6 to 37.3 s. That cost is now
paid once, by the release, instead of on a user's machine (`crates/blaze-engine/src/shipped.rs`):

- `blaze-engine std REMOTE TOOLCHAIN` builds a script that does nothing over TOOLCHAIN (a sysroot)
  into a store of its own, running no rustc, and pushes that store to REMOTE as the project `std-<target>-<build>`, where `<build>` is what the store keys
  by: the blaze binary's bytes and the toolchain's commit and compiler name. Only that binary
  with that toolchain can use it, and every key in it says so. It then builds a program that
  prints a line, whole, with the `dev` and the `release` profile: a program keys the unit by the
  debug info its profile asks for, line tables for `dev` where a script asks for none, so
  without it every whole build of a dev program compiled the library again (about 10 s, step K).
  The `release` program takes the script's unit. It then builds a program that
  prints a line, whole, with the `dev` and the `release` profile: a program keys the unit by the
  debug info its profile asks for, line tables for `dev` where a script asks for none, so
  without it every whole build of a dev program compiled the library again (about 10 s, step K).
  The `release` program takes the script's unit.
- The release workflow runs it with each target's archived binary and the pinned nightly, and
  attaches the remote's files to the release with `.` for `/` (`manifests.std-…`, `packs.….pack.zst`,
  `keys.…`). A remote can be a plain `https://` address now (`remote/https.rs`): read-only,
  flat, redirects followed, which is what a release's download URL is.
- A build with nothing of rustc's fetches that project once per store, beside the build and
  never waited for (the store reads it when it is in, [cache.md](cache.md#the-standard-library)):
  from `BLAZE_STD_FROM` when set (`off` for nowhere), otherwise from its own release when it
  is one (`BLAZE_RELEASE`, set by the workflow on a tag) and the root manifest does not say
  `fetch-public = false` under `[workspace.metadata.blaze]`. Builds that start
  together take a lock and fetch once; `fetched-<project>` in the store dir records that it is
  done, and a failure is retried an hour later. A pack is used only once its bytes hash to the
  name the manifest lists, as with any remote.
- `push` now sends a pack zstd-compressed when that saves a quarter or more. A pack keeps
  values over 1 MB uncompressed so they can be read in place, which the unit's 17 archive parts
  are: the library's 65 MB of packs go over the wire as 18.6 MB.
- Builds from crates.io or nix are other binaries, so nothing is shipped for them and they build
  the unit locally, once per store.

`harness/norustc/count.py` does what a release does by default: it runs `blaze-engine std` with the
binary under test into `$NR_ROOT/std-remote` and points `BLAZE_STD_FROM` at it. `NR_STD=build`
leaves every build to make the library itself.

On dev-aws-11 (2026-10-23, against main cd48ca75f7), "cold" with an empty store: ripgrep 4.4 s
(main 11.6 s; 4.5 s before step I), nushell 27.7 s (main 38.9 s; 25.6 s before step I); with
`NR_STD=build`, 11.4 s and 36.8 s. The fetch and pull of the library take 0.1 to 0.2 s of that.
The rustc counts are main's (ripgrep cold 39, nushell cold 493).

Built here, the library is still 8 s of the cold build: resolving its 27 crates 1.9 s (core's
def map alone 0.9 s, which the others wait on), compiling 5 s, then splitting, freezing and
storing. The compile does not get faster past 16 cores (round 1 takes 4.5 s on 16, 3.9 s on 96):
a profile has a fifth of its samples in rust-analyzer's interning of types, generic arguments
and predicates, and the lock slow paths beside it, a contention the cores only add to. And a
round is compiled twice: 78 generic instances of the library do not compile (SIMD intrinsics
blaze has no lowering for), and the build starts over without them, 1.1 s. Cranelift now
compiles `cmpxchg16b`'s 128-bit compare-and-swap, which was 5 more of them.

The step K bar: with a warm remote, `blaze build` from nothing is no slower than cargo's cold build;
with an empty store, it is measured and published beside cargo's, and the gap is an issue of its
own. Until K lands, the rustc base stays as it is; it is not made faster or extended.

### Lanes in flight

Lanes whose work makes a rustc path do more, to redirect:

- **The rustc base and the cold start** (#11, `cold-start-2`): make the daemon's first edits
  fast on blaze's own executable, toward step K, rather than tune patching rustc's.
- **rustc-dead-code** (#52, done): kept code in rustc's link so that edits spliced into
  rustc's executable could call it. Its correctness fix landed (a patch calls no local of an
  optimized object, whose arguments LLVM may have changed); the keeping was deleted, and the
  edits it would have spliced are linked in full until step G.
- **The fallback to cargo** (#27, closed): "fails instead of going to cargo" was fixed by going to
  cargo. Step D reverses it; the resolution bug behind it (`.chain()` on a tuple of systems) is an
  E or D bug to fix in blaze.
- **bun-edits**: patched rustc's executable for const-generic statics so that functions left to
  rustc keep working. Redirected on 2026-10-22 as `bun-e`: the static naming, the exposure of
  rustc's local statics and the holding of functions left to rustc through edits were dropped;
  its two correctness fixes (no stale copy of an instance, no static counted twice) stay, and
  the lane owns bun's part of step E (above).
- **kept-rejections** ([#37](https://github.com/indexable-inc/blaze/issues/37)): sends callers
  to rustc's copies of rejected code. Redirected to compiling them (E), owned by the glancer
  lane, and done that way; the kept rejections are not merged.

Aligned: Axionize's #34, #35 and #75 work (step E), the proc-macro expansion cache #38 (J),
include-loop's own base (K), and glancer's #40 and #41 (the cold start).
