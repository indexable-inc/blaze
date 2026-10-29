---
status: current
summary: What is supported, cell by cell, each pointing at the report or harness run behind it.
---
# What is supported

Every cell here points at a report or a harness run. "CI" is GitHub Actions run
39057174862 on commit 888e87df74, the last run recorded in the reports that covered all
four native platforms under both backends ([reports/PLAT-ABI2.md](reports/PLAT-ABI2.md)).
"Local" is a run of the gate on Linux x86-64 at f191c6e42a, the commit these docs were
written against, under both backends (see [results.md](results.md)). [platforms.md](platforms.md) describes each port. "Unverified" means
nobody recorded running it; it does not mean it is known to fail.

"Yes" has a narrow meaning: the named harness passes there. It does not mean your project
will build: what blaze cannot build fails the build with the reason.

## Toolchain

One toolchain: the dated nightly in `harness/toolchain`, `nightly-2026-10-25` today, which
says `rustc 1.101.0-nightly (12433c307 2026-10-24)` (rust-lang/rust 12433c307b5a, LLVM
23.1.4). blaze links against what rustc built, so it must agree with that rustc on symbol
mangling (v0), type layout, the unstable Rust ABI, vtable layout and `TypeId` hashes, and
everything that mirrors rustc is taken from that one commit: `crates/rustc_abi` is its
`compiler/rustc_abi`, `vendor/proc-macro-srv` its proc-macro server with blaze's changes,
and the answers to rustc's questions (`crates/blaze-cargo/src/answers/rustc.json`) are what that
rustc says on x86-64 Linux, AArch64 Linux and arm64 macOS. Stable Rust is not supported as a
toolchain of its own: a stable release's code builds because the nightly builds it. The pin
moves to a newer nightly with `harness/toolchain_bump.py`
([contributing.md](contributing.md#moving-to-a-newer-nightly)).

Reports and results dated before 2026-10-25 were measured on stable 1.99.0, and some older
ones on `nightly-2026-05-10`; they are marked where they are used and are not re-measured
by the move.

`blaze` builds every project with the pinned nightly, whatever toolchain the project names
([cargo.md](cargo.md)): it ignores rust-toolchain files, rustup's settings, `RUSTUP_TOOLCHAIN`
and `RUSTC`, and runs no rustc to ask what it is. A project that needs another Rust pins
another blaze ([toolchain.md](toolchain.md)).

## Platforms

| | Linux x86-64 | Linux AArch64 | Windows x86-64 MSVC | macOS arm64 | wasm32 |
|---|---|---|---|---|---|
| Cranelift | yes (1) | yes (1) | yes (1) | yes (1) | does not apply (2) |
| LLVM | yes (1) | yes (1) | yes (1) | yes (1) | no |
| Full link | yes (1) | yes (1) | yes (1) | yes (1) | yes, with nothing of rustc's (2) |
| Splice | yes (3) | yes (3) | yes (4) | yes, with a limit (5) | yes (2) |
| Debug info | yes (6) | yes (6) | lines only (7) | unverified (7) | no |
| `blaze` | yes (8) | unverified (9) | yes, by hand (9) | yes, by hand (9) | `wasm32-wasip1` and `wasm32-unknown-unknown` cdylibs, by hand (2) |
| Disk store | yes (10) | yes (10) | yes (10) | yes (10) | unverified |
| Remote store | yes, not real AWS (11) | unverified | unverified | unverified | unverified |
| Fuzzed | yes (12) | no | no | no | no |

1. CI: `difftest.py corpus --layouts` compiles every function of the corpus with blaze and
   matches rustc under both backends: 822 of 822 functions on Linux x86-64, 825 of 825 on
   native AArch64, 830 of 830 on Windows, 828 of 828 on macOS, with 0 type layouts
   differing. `programs`, `stress`, `breakers`, `unwinds`, `splices`, `asmedits`,
   `includes`, `worktrees` and `keys` ran on every leg. Under LLVM, `backends.py` reports no
   corpus function left to Cranelift on any of the four. Local: 822 of 822 under both.
2. wasm32 uses its own lowering from Cranelift IR, not Cranelift's or LLVM's code
   generation. `wasm32-wasip1` under wasmtime, 2026-10-27, arm64 ix VMs, pinned nightly,
   main fdc1c53f15: corpus MATCH with 957 of 959 functions from blaze (964 of 966 with
   simd128 and relaxed-simd), stress 100 of 100, splices 71 of 71. With nothing of rustc's,
   wasip1 programs, a program over blaze-manifest and blaze-resolve, wasm-host's `no_std`
   simd128 guests and two Cloudflare Workers (workers-rs, wasm-bindgen) build, link and answer
   as rustc's builds ([platforms.md](platforms.md), wasm32); `blaze build --target
   wasm32-wasip1` built the second by hand, its build scripts and proc macros blaze's too,
   and `blaze build` the guest and, under worker-build, both workers. The train runs the corpus,
   stress and `allblaze_wasm.py` on every push to main. No unwinding is emitted; the wasm
   targets are `panic=abort`. Modules are 8 to 50 times the size of rustc's.
3. `splices.py` 71 of 71 in CI on both Linux architectures; `farsplices.py` 6 of 6 on
   AArch64, where a branch reaches only 128 MB. Not every edit can be spliced: about 60 of
   the 71 steps are, and the rest link in full by design (an executable somebody else
   wrote, a static that grew, a running executable, and similar).
4. [platforms.md](platforms.md), Windows: 71 of 71 with 57 spliced, 13 full links and
   1 step with nothing edited, and the harness fails on Windows if the count is not 57.
5. [platforms.md](platforms.md), macOS: `splices.py` 71 of 71. The daemon links with
   `-no_fixup_chains`, so a deployment target of 12 or later splices as 11.0 does
   (`headerpad.py` builds for 14.0); an executable linked with chained fixups by a line of
   one's own is declined and linked in full.
6. [reports/DEBUGINFO.md](reports/DEBUGINFO.md) for x86-64 and PLAT-ABI2 for native
   AArch64: `debuginfo.py` passes under both backends. See "Debug info" below for what that
   covers.
7. On Windows blaze writes CodeView procedures and line tables, checked by `debuginfo.py`
   with `llvm-pdbutil` and `cdb` (WINDOWS.md). There are no types, parameters or locals,
   and the PDB is stale after a splice until the next full link. On macOS debug info is
   not checked here.
8. `harness/blaze_cli.py`, Cranelift only: seven real projects with no difference from
   cargo, 66 of 66 flag steps, 11 of 11 lifecycle checks
   ([reports/CARGO.md](reports/CARGO.md)). Not in CI. Run by hand for these docs on a new
   project and on ripgrep 15.1.0.
9. `blaze` links ELF, Mach-O and COFF executables (`unsupported_target` in
   `cargo/cli.rs`). On Windows x86-64 MSVC it links with `rust-lld` and was run by hand on a
   toy crate and `demo/life` ([platforms.md](platforms.md#windows-x86-64-msvc)). On macOS
   arm64 it was run by hand on bun 1.4.3's dev loop ([research/bun.md](research/bun.md)).
   Nobody recorded running it on Linux AArch64.
10. CI: `storekills.py`, `storecorrupts.py` and `storeshared.py` on all four platforms.
    `storefull.py` on Linux only. The earlier
    [reports/DISKCACHE.md](reports/DISKCACHE.md) says "Linux x86-64 only"; PLAT-ABI2 is
    later and is what this row follows.
11. `harness/remote.py` against MinIO and SeaweedFS over loopback
    ([reports/REMOTE.md](reports/REMOTE.md)); MinIO runs in the Linux x86-64 CI job. Real
    AWS S3 has not been run.
12. [reports/FUZZ.md](reports/FUZZ.md). Linux x86-64 only; the smoke run is in that CI job.

Other targets. `x86_64-apple-darwin` is refused: with the triple forced through, the link
fails on the unwind table ([platforms.md](platforms.md)). Windows targets other than x86-64 MSVC have
nothing implemented or run. Cross compiling from Linux x86-64 to
`aarch64-unknown-linux-gnu` and `x86_64-unknown-linux-musl` is covered by the build-model
harness below.

## Backends

Cranelift is the default and needs nothing installed. LLVM is in every released binary (LLVM 23,
linked statically); a blaze built from source needs `--features llvm`. It is selected with `BLAZE_BACKEND=llvm`. Everything that was
measured under LLVM used `BLAZE_OPT=2 BLAZE_EMIT=fast`. Other levels are accepted and are
not covered by the gate.

Neither backend inlines one function into another in a build, so LLVM at `-O2` is not
comparable to a release build. LLVM gives a function it has no lowering for to Cranelift. One
LLVM bug is worked around by turning off a vectoriser pass (finding 6 in FUZZ.md).

## MIR optimizations

`BLAZE_MIR_OPT=1` runs the passes rustc runs on a debug build's MIR before code is generated
from it, `BLAZE_MIR_OPT=2` the ones it runs above `-Copt-level=0` that are ported; 0, the
default, runs none ([mir-passes.md](mir-passes.md#optimizations),
[research/mir-gap.md](research/mir-gap.md)). Coroutine bodies are never optimized.

| | 0 | 1 | 2 |
|---|---|---|---|
| `difftest.py corpus --mixed 8` | yes | yes | yes |
| `difftest.py mixfutures --mixed 16 --layouts` | yes | yes | yes |
| `growth.py`, `movedlines.py` | yes | yes | yes |
| The interpreter's tests give the same result | yes | yes | yes |
| Identical to rustc's MIR, corpus `cases` | 4.0% (against rustc's level 1) | 21.5% | 14.5% |

## Tiers

A program that takes edits with `blaze-live` has its hot functions compiled again while it
runs, at LLVM `-O3` with what they call built in and then with a profile, and patched in
([tiers.md](tiers.md)). macOS arm64 with `--features llvm` only, like live edits; on by
default there, off with `BLAZE_TIER=off`.

| | macOS arm64 | elsewhere |
|---|---|---|
| Sampling, under 1% of a core at 500 Hz | yes (local, 2026-10-20) | no: live edits are macOS only (Linux would sample with `perf_event_open`, [tiers.md](tiers.md#linux)) |
| The sampler stops once the worker reading it is gone | yes (local, by killing the worker) | no |
| Levels 2 and 3, output identical to cargo's | yes, `harness/tiers.py` (local) | no |
| An edit demotes a tiered function in the same build | yes, `harness/tiers.py` (local) | no |
| An edit does not wait for a compile at a higher level | yes (local, with LLVM's part stretched to a second) | no |
| Code no patch can hold stays out, the program is not started over | yes (local, 12 runs of the Life benchmark under one daemon, [tiers.md](tiers.md#code-no-patch-can-hold)) | no |
| `BLAZE_TIER=off` changes nothing | yes, `harness/tiers.py` (local) | does not apply |
| Within 10% of a release build | within 6% of a release build that checks overflow as the dev profile does, which is as far as the tiered code may go, and 88% of release, on the Life benchmark under load | no |

## Debug info

On ELF, every function blaze compiles is described in DWARF 4 with line tables and a
subprogram entry. Checked on Linux x86-64 and native AArch64: backtraces with
`RUST_BACKTRACE=1` have the same workspace frames as cargo's build, `llvm-dwarfdump
--verify` finds no errors (also after a splice), and gdb stops and steps by line.

What is missing:

- Under LLVM no parameters or locals are described. Under Cranelift they are, without
  lexical scopes, so a local is visible from the start of the function, and a name bound
  more than once is not described at all.
- Enums, unions and closures are described by name and size only.
- Under LLVM at `-O2` a backtrace can have a frame fewer than rustc's, where a tail call
  became a jump.
- `line-tables-only` and `limited` are treated the same as full debug info.
- DWARF 5 and split debug info are not done. An executable with compressed debug sections
  is not spliced.
- After a splice, a breakpoint set by function name can land on the old copy. Breaking by
  file and line finds the new code.

## Language

blaze compiles what rust-analyzer can lower to MIR and what the code generator has an
explicit lowering for. The rest is refused per function, with a reason, and rustc's copy of
that function is used. Measured on rustc's own tests
([reports/RUSTCSUITES.md](reports/RUSTCSUITES.md), Linux x86-64): of the `tests/ui`
run-pass tests that can run on stable, blaze compiled 5558 of 5615 functions and refused
57; in the standard library's test suites it refused 11 of 2846.

Known refusals:

| What | Why |
|---|---|
| `sym` operands in `asm!` that is not naked; `sym` naming a generic function or a thread-local | the block's function has no way yet to carry what its symbols name; generic arguments of a `sym` path are not inferred |
| a naked function whose assembly defines a symbol or has data in another section | not read back from the assembler |
| an `asm!` constant that is not a literal | not lowered |
| some C-variadic definitions: on x86-64 System V a named parameter on the stack or one that is not an integer or float of at most eight bytes; on Windows a named parameter wider than eight bytes and a definition with no named parameter; on AArch64 a named aggregate | each convention's `va_list` is implemented for the common shapes only |
| an argument passed in memory aligned to more than 16 bytes, and a vector in a C ABI | no lowering |
| the `win64` calling convention on Linux | no lowering |
| pattern matching on a union | not lowered from MIR |
| upcasting a `dyn` type with bound regions | not lowered |
| `type_name` of a function item or of a `dyn` type with a binder | rustc's exact text is not reproduced |
| a body in which rust-analyzer's type inference fails or finds errors rustc does not | no MIR, so no code; examples are range patterns over associated constants and an empty array whose element type comes from a single `PartialEq` impl |
| 15 functions of the `generic-array` crate | a trait-solver gap on `GenericArray<T, N>` |

Known differences from rustc, where blaze does compile the code. These are the cases
marked open in `harness/breakers.py`:

| Case | Difference |
|---|---|
| `assert_message` | `assert!` of a condition longer than a line prints it on one line; rustc breaks it over several |
| `future_unwind_flags` | a future can be one byte smaller than rustc's, because rustc keeps a drop flag blaze does not. Only code that depends on the exact size of a future sees it |
| `raw_write_then_match` | a value written through a raw pointer into a local rustc believes is `None`, then partly moved out, is dropped by blaze and leaked by rustc |
| `fragment_edition_through_macro` | blaze does what rustc's own test says rustc will do once rust-lang/rust#130484 is fixed |

The differential fuzzer found twenty-one problems in generated programs, most of them
miscompiles in the order temporaries are dropped. Twenty are fixed or worked around and one
is the first row above. Nearly every full-size run found something, so expect that more
exist.

The daemon refuses source in which rust-analyzer finds an error (a type mismatch, an
unresolved name, a missing field), and under `blaze` cargo then runs and rustc
reports it. A function with a macro call whose expansion is not what rustc makes of it is
refused with the reason: one that failed to expand, and a call in parentheses or brackets
with no `;` after it whose expansion is not the one expression rustc parses it as, such as a
call of a macro of several statements in the parentheses of another macro's call
([#154](https://github.com/indexable-inc/blaze/issues/154)). An expansion of one expression
and a `;` is that expression, as rustc takes the `;`: `bail!(e)` last in a block, expanding to
`return Err(e);`, is built when the macro is another crate's (rustc warns), and refused when it
is the crate's own, where rustc's `semicolon_in_expressions_from_macros` is an error. An error
rust-analyzer does not find is not refused, and a crate blaze compiles without rustc, as it
does a whole program's, is then built and run.
Borrow-check errors are the main case: blaze's borrow checker ([borrowck.md](borrowck.md)) refuses what it finds, and it
finds nothing rustc accepts in rustc's tests or in seven real projects, but it misses about a
third of the errors rustc gives (67% of rustc's borrow-check tests get rustc's verdict), and
an edit with one it misses builds and runs. Run `blaze check` or `cargo check` to get
rustc's verdict. `BLAZE_BORROWCK_REFUSE=0` stops the daemon refusing borrow-check errors.

## Nightly features

Nightly features are in scope. The toolchain is a nightly, so what it accepts is Rust as far
as blaze is concerned, and a nightly feature is added to blaze like any other part of the
language. A crate with `#![feature(...)]` is built like any other, under the same rules: each
construct is lowered explicitly or its function is refused with a reason. What is checked so
far:

- `harness/live-alloc`, a program on `allocator_api` (`Vec<T, A>` and `Box<T, A>` in a bump
  arena) that takes live edits; `live_e2e.py` runs it with `LIVE_PROGRAM=live-alloc`, in the
  gate.
- The corpus's `nightly` feature, which `toolchain_bump.py checks '{}'` runs with `--layouts`.

The nightly's own removals apply: `box` patterns are gone upstream in this nightly, replaced
by `deref_patterns`, and blaze's support for deref patterns is being added. `-Z` flags in
`RUSTFLAGS` are described in their own section ("-Z flags").

## Cargo's build model

There are two layers, and they support different things.

The daemon itself, driven by the harness, follows most of cargo's build model.
`harness/buildmodel.py` has a small project per feature and compares against cargo under
both backends. The latest recorded result is 208 of 208 on Linux x86-64 and 204 of 204 on
native AArch64, where four rows that need an x86-64 host are not run (CI on `51339e5`).
[reports/BUILDMODEL.md](reports/BUILDMODEL.md) has the table at 97 cases per backend, of
which 92 were supported and 5 refused.

`blaze` is narrower. Its column was recorded while it still handed builds to cargo. Since
2026-10-27 it hands none ([#408](https://github.com/indexable-inc/blaze/pull/408),
[#571](https://github.com/indexable-inc/blaze/pull/571)): what it does not build fails with the
reason (`refused` in `crates/blaze-cargo/src/cargo/whole.rs`). A cell that says "cargo" or "handed to
cargo" now means one of the two, and which is unverified.

| Feature | Daemon (`buildmodel.py`) | `blaze` |
|---|---|---|
| dev and test profiles | supported | served |
| release and custom profiles, `opt-level`, `debug-assertions`, `overflow-checks`, `codegen-units`, `strip`, per-package overrides | supported | handed to cargo for `--release` and `--profile`; unverified for profile settings changed in the manifest |
| `panic = "abort"` | supported | unverified |
| `lto = true`, `lto = "thin"` | refused: rustc recompiles the whole program at link time and leaves no library to link blaze's functions against | cargo |
| `lto = "off"` | supported | unverified |
| crate types `bin`, `lib`, `rlib`, `dylib`, `cdylib`, `staticlib` | supported | programs, and a library's `cdylib`, linked as a shared library and linked again after each edit rather than spliced, and its `staticlib`, archived again after each edit with the static native libraries it bundles; a build that would write a `dylib` stops with an error, since blaze does not link one yet ([#262](https://github.com/indexable-inc/blaze/issues/262)) |
| proc macros (attribute, derive, function-like) | supported: rustc's build of the macro is loaded and its expansion is compiled by blaze | served; an edit to the proc-macro crate itself is cargo's |
| build scripts (`rerun-if-changed`, `rerun-if-env-changed`, `rustc-cfg`, `rustc-env`, `rustc-link-lib`, `OUT_DIR`, `links`) | supported | served; a change to a script or its inputs is cargo's, then the daemon reloads |
| unit tests, integration tests, examples, benches | supported | served; adding or removing a `#[test]` is cargo's |
| doctests | supported through `blaze test --doc` (`doctests.py`, 30 crates); documentation behind `cfg(doc)` is not seen | served; an example blaze cannot build fails with the reason, as does a `compile_fail` example whose error blaze can't confirm |
| features, optional and weak dependencies, feature unification under resolvers 1, 2 and 3 | supported | `--features` served; `--all-features` and `--no-default-features` handed to cargo |
| `resolver.feature-unification` (`-Zfeature-unification`) | `selected` and `workspace` supported; `package` supported for one package, and refused with a reason for several, which it builds with other features each | as for a build |
| path, git, registry, renamed, vendored, `[patch]` and `[replace]` dependencies | supported | served |
| an edit to a path dependency the daemon was not asked to build | refused with a reason | cargo |
| `RUSTFLAGS` and `[build] rustflags`, including `-C target-cpu` and `target-feature` | supported | handed to cargo when set in the environment; unverified in configuration |
| `.cargo/config.toml` `[env]`, `linker`, `runner` | supported | unverified |
| `env!` and `option_env!` | supported, and followed when the variable changes | served; a changed variable that rustc reported reading sends the build to cargo |
| mixed editions 2015 to 2024 | supported | served |
| `#![no_std]`, `#![no_main]` | supported | unverified |
| cross compilation to `aarch64-unknown-linux-gnu` and `x86_64-unknown-linux-musl` from Linux x86-64 | supported | unverified; a target that is not ELF is handed to cargo |
| manifest, lock file or workspace membership changing under a running daemon (`git checkout` between commits) | supported | cargo builds, then the daemon reloads |

"Supported" for a release profile means the program printed the same as cargo's. It does
not mean the code is as fast: blaze's code is not optimised across functions under either
backend.

## `-Z` flags for rustc

The `-Z` flags cargo gives rustc, from `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, `[build]` or
`[target]` `rustflags` and a profile's `rustflags`, still reach rustc for what rustc compiles,
and build scripts see them in `CARGO_ENCODED_RUSTFLAGS`. What blaze does with each in its own
code is one table, `crates/blaze-cargo/src/unstable.rs`, which the daemon's plan, the plan of a
program linked against rustc's libraries, the build scripts and proc macros blaze compiles and
blaze's own answers to rustc's questions all read:

- **ignored**: it changes how rustc works (speed, what it prints, its own checks, how its
  backend lays out code) and nothing a program does or how blaze's code links with rustc's;
- **modelled**: it changes what a program does, and blaze's code already does that;
- **refused**: blaze does not build code with it. A crate built with it is left to rustc and
  the reason names the flag (`` `zf` is built with -Z location-detail=none: it changes what
  `Location::caller()` says ``, in the line `blaze` prints and in `blaze status`);
  in the daemon's plan of a whole build it fails the load with the same words. A flag not in
  the table is refused as one blaze does not know.

The names are those of `rustc -Z help` of the pinned nightly, every one of them, spelled
`-Zname=value`, `-Z name=value`, `-Zname` or with `_` for `-`.

| What | Why | Flags |
|---|---|---|
| ignored | how fast rustc compiles | `cache-proc-macros`, `disable-fast-paths`, `disable-incr-comp-backend-caching`, `incremental-ignore-spans`, `no-parallel-backend`, `no-steal-thir`, `threads` |
| ignored | how rustc reads its own argument files | `shell-argfiles` |
| ignored | how rustc's backend optimizes or lays out its own code; the program does the same | `box-noalias`, `codegen-source-order`, `fewer-names`, `function-sections`, `human-readable-cgu-names`, `inline-llvm`, `llvm-writable`, `merge-functions`, `no-unique-section-names`, `plt`, `relax-elf-relocations`, `trap-unreachable`, `tune-cpu` |
| ignored | it lets other unstable options be given, and changes nothing | `unstable-options` |
| ignored | what rustc checks of itself while it compiles | `assert-incr-state`, `eagerly-emit-delayed-bugs`, `extra-const-ub-checks`, `incremental-verify-ich`, `lint-llvm-ir`, `lint-mir`, `query-dep-graph`, `treat-err-as-bug`, `validate-mir`, `verify-llvm-ir` |
| ignored | what rustc prints or writes beside its output | `binary-dep-depinfo`, `check-cfg-all-expected`, `checksum-hash-algorithm`, `deduplicate-diagnostics`, `dep-info-omit-d-target`, `dump-dep-graph`, `dump-mir`, `dump-mir-dataflow`, `dump-mir-dir`, `dump-mir-exclude-alloc-bytes`, `dump-mir-exclude-pass-number`, `dump-mir-graphviz`, `dump-mono-stats`, `dump-mono-stats-format`, `future-incompat-test`, `graphviz-dark-mode`, `graphviz-font`, `hint-msrv`, `identify-regions`, `ignore-directory-in-diagnostics-source-blocks`, `incremental-info`, `input-stats`, `llvm-time-trace`, `macro-backtrace`, `macro-stats`, `meta-stats`, `metrics-dir`, `mir-include-spans`, `move-size-limit`, `nll-facts`, `nll-facts-dir`, `print-codegen-stats`, `print-codegen-stats-json`, `print-llvm-passes`, `print-mono-items`, `print-type-sizes`, `proc-macro-backtrace`, `profile-closures`, `remark-dir`, `self-profile`, `self-profile-counter`, `self-profile-events`, `span-free-formats`, `teach`, `temps-dir`, `terminal-urls`, `time-llvm-passes`, `time-passes`, `time-passes-format`, `trace-macros`, `track-diagnostics`, `trim-diagnostic-paths`, `ui-testing`, `verbose-asm`, `verbose-internals`, `write-long-types-to-disk` |
| ignored | what rustc's own objects tell a debugger; blaze writes its own debug info | `annotate-moves`, `debug-info-type-line-numbers`, `debuginfo-compression`, `debuginfo-for-profiling`, `dwarf-version`, `embed-source`, `inline-mir-preserve-debug`, `mir-strip-debuginfo`, `no-generate-arange-section`, `split-dwarf-inlining`, `split-dwarf-kind`, `split-dwarf-out-dir`, `src-hash-algorithm` |
| ignored | what rustdoc documents | `normalize-docs` |
| ignored | where rustc puts a library's metadata; the objects are the same | `embed-metadata` |
| ignored | which programs rustc's type checker accepts, not the code of one it accepts | `assumptions-on-binders`, `disable-param-env-normalization-hack`, `enforce-type-length-limit`, `higher-ranked-assumptions`, `min-recursion-limit`, `next-solver`, `no-implied-bounds-compat`, `polonius`, `renormalize-rigid-aliases` |
| modelled when off, refused when on | blaze compiles `contract_checks()` as false | `contract-checks` |
| modelled when off, refused when on | blaze compiles `ub_checks()` as false | `ub-checks` |
| refused | it changes how rustc links, which blaze's link does not do | `export-executable-symbols`, `external-clangrt`, `implicit-sysroot-deps`, `link-directives`, `link-native-libraries`, `link-only`, `no-link`, `no-profiler-runtime`, `osx-rpath-install-name`, `packed-bundled-libs`, `pre-link-arg`, `pre-link-args`, `profiler-runtime`, `staticlib-allow-rdylib-deps`, `staticlib-hide-internal-symbols`, `staticlib-prefer-dynamic`, `staticlib-rename-internal-symbols`, `use-ctors-section`, `wasi-exec-model` |
| refused | it changes the ABI or symbols rustc's code has, which blaze's code must match | `allow-partial-mitigations`, `default-visibility`, `deny-partial-mitigations`, `direct-access-external-data`, `fixed-x18`, `llvm-target-feature`, `metadata-crate-hash`, `packed-stack`, `pointer-authentication`, `reg-struct-return`, `regparm`, `tls-model`, `wasm-c-abi` |
| refused | it changes the code rustc generates, which blaze's code generator does not do | `autodiff`, `autodiff-post-passes`, `branch-protection`, `cf-protection`, `ehcont-guard`, `emit-stack-sizes`, `flatten-format-args`, `force-intrinsic-fallback`, `function-return`, `indirect-branch-cs-prefix`, `large-data-threshold`, `llvm-module-flag`, `llvm-plugins`, `min-function-alignment`, `mir-enable-passes`, `mir-move-elimination`, `mir-opt-bisect-limit`, `mir-preserve-ub`, `offload`, `patchable-function-entry`, `precise-enum-drop-elaboration`, `retpoline`, `retpoline-external-thunk`, `small-data-threshold`, `stack-protector`, `uninit-const-chunk-threshold`, `unsound-mir-opts`, `use-sync-unwind` |
| refused | it changes the layout of types | `layout-seed`, `randomize-layout` |
| refused | it changes the layout of vtables | `no-trait-vptr` |
| refused | it changes the paths `file!()` and panics name | `remap-cwd-prefix` |
| refused | it changes the paths panics name | `simulate-remapped-rust-src-base`, `translate-remapped-path-to-local-path` |
| refused | it changes what `#[derive(Debug)]` prints | `fmt-debug` |
| refused | it changes what `Location::caller()` says | `location-detail` |
| refused | it changes what `cfg(target_thread_local)` says | `has-thread-local` |
| refused | it changes what `cfg(version)` says | `assume-incomplete-release` |
| refused | it changes what the program does, which blaze does not model | `async-panic`, `experimental-default-bounds`, `force-unstable-if-unmarked`, `internal-testing-features`, `namespaced-crates`, `no-leak-check`, `on-broken-pipe`, `panic-in-drop`, `span-debug`, `strict-init-checks`, `tiny-const-eval-limit`, `unleash-the-miri-inside-of-you` |
| refused | it changes which functions rustc's libraries export, which blaze links against | `always-encode-mir`, `cross-crate-inline-threshold`, `dylib-lto`, `hint-mostly-unused`, `inline-mir`, `inline-mir-forwarder-threshold`, `inline-mir-hint-threshold`, `inline-mir-threshold`, `mir-opt-level`, `share-generics`, `split-lto-unit`, `thinlto`, `virtual-function-elimination` |
| refused | it instruments the code, which blaze's code generator does not do | `codegen-emit-retag`, `coverage-options`, `instrument-mcount`, `instrument-xray`, `maximal-hir-to-mir-coverage`, `sanitizer`, `sanitizer-cfi-canonical-jump-tables`, `sanitizer-cfi-diag`, `sanitizer-cfi-generalize-pointers`, `sanitizer-cfi-minimal-runtime`, `sanitizer-cfi-normalize-integers`, `sanitizer-cfi-recover`, `sanitizer-dataflow-abilist`, `sanitizer-ignorelist`, `sanitizer-kcfi-arity`, `sanitizer-memory-track-origins`, `sanitizer-recover` |
| refused | blaze does not check which features a crate enables | `allow-features` |
| refused | blaze links against what rustc's LLVM backend builds, not another backend's | `codegen-backend` |
| refused | blaze runs proc macros its own way | `proc-macro-execution-strategy` |
| refused | blaze's front end does not see the attribute it adds | `crate-attr` |
| refused | blaze's test harness does not run tests built with panic=abort | `panic-abort-tests` |
| refused | rustc does not build the crate's code with it | `build-sdylib-interface`, `dual-proc-macros`, `help`, `ls`, `no-analysis`, `no-codegen`, `parse-crate-root-only`, `unpretty`, `wasm-proc-macros` |
