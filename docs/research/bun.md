---
status: record
summary: bun's Rust port built by blaze: the harness, what broke and was fixed, and its edit and runtime numbers.
date: 2026-10-21
---
# bun's Rust port through blaze

2026-10-21; Linux x86-64 and the whole-program build 2026-10-22. Claims are marked **read** (in a source we read, or measured here, with the
command), **reported** (someone says so, unchecked) or **unverified** (our inference). What was
fetched is in [docs/upstream/bun/](../upstream/bun/SOURCE.md).

## What bun is now

- bun was rewritten from Zig to Rust in one pull request, oven-sh/bun#30412, "Rewrite Bun in
  Rust", merged 2026-05-14: 1,009,257 lines added, 4,024 removed, 2,188 files (read,
  `pr-30412.json`).
- The port was mechanical, file by file from 1,448 `.zig` files, to behave as the Zig did and
  pass the same test suite, and is to be made idiomatic after 1.4 ships (reported, the blog
  post "Rewriting Bun in Rust", 2026-07-08). Bun was 535,496 lines of Zig without comments, and
  about 4% of the Rust (~27,000 of ~780,000 lines) is in `unsafe` blocks (reported, same post).
- The pin here is the tag `bun-v1.4.3`, commit `c6da4a4d3010e5553438c60f6bd76d981976867c`
  (read, `git ls-remote`). Its Rust is 1,247,251 lines in `src/**/*.rs` (972,091 that are not
  blank or a line comment) in 103 workspace crates (read, `wc -l` and `Cargo.toml`'s members at
  that commit), on `nightly-2026-09-15` (read, `rust-toolchain.toml`).
- It uses nightly features blaze's frontend does not all have: `adt_const_params` (on in 11
  crates), `core::mem::type_info` in `bun_collections::MultiArrayList`, and
  `inherent_associated_types` in `bun_bundler` (read, the sources).

## How bun is built, and what the harness builds

- bun's own build (`bun scripts/build.ts`, ninja) compiles its C and C++ (JavaScriptCore from a
  prebuilt WebKit, BoringSSL, uSockets, ...), builds its Rust as one static library,
  `bun_runtime`, and links the two with clang (read, `build.ninja` from `--configure-only`).
  It needs LLVM 23, cmake, ninja, go and a bun (read, `docs/project/contributing.mdx`).
- blaze builds and splices executables rustc links. `harness/bunprep.py` has bun's build make
  everything but the Rust once per revision (1,227 of its 1,478 ninja steps on macOS arm64;
  read), writes the native half of bun's link to a file of link arguments, and adds one
  workspace member, `bun-exe`: a `#![no_main]` binary of `bun_runtime` whose build script puts
  those arguments on rustc's link line. bun's tree changes in four places: the member in the workspace,
  `debug-assertions = false` in `[profile.dev]` (bun's C++ is its release build, and a Rust
  build with assertions calls functions only a C++ build with them has), a feature that leaves
  the allocator shim's marker to rustc, and on macOS the system linker in place of ld64.lld
  (read, `bunprep.py`'s `overlay`).
- The program that comes out is bun: `--version`, `--revision`, `-e`, `build --no-bundle
  --minify` and `Bun.Transpiler` print what bun's own release build prints (read, `apps.json`'s
  runs, compared by `runbench.py` between cargo's build and bun's).

## What broke, and what was fixed

Each fix is its own commit on `main` with a regression case (read, the commits):

| What | Where it showed | Case |
|---|---|---|
| `blaze` went to cargo for any rustc but 1.99.0; `BLAZE_OTHER_TOOLCHAIN=1` builds with it | every build of bun | `soakrows.py`, "a project pinned to a nightly" |
| nightly cargo passes `-Z embed-metadata=no`, which sent the build to cargo | every build | the same row |
| nightly cargo's second build-dir layout: build scripts' output and the seeded target were not found | every build | the same row |
| an edit that makes rustc's copy of a function left to rustc out of date failed the build; it is cargo's to build | signature edits | `soakrows.py`, "an item added beside a function left to rustc" |
| a module whose file is outside its package (`#[path = "../.."]`) was not loaded | `bun_runtime` | `soakrows.py`, "a module file outside its package" |
| a `macro_rules!` defined by another crate's macro (`bun_core::define_scoped_log!`) was not in scope in later modules (`IncompleteExpr`) | sent every signature edit of `bun_runtime` to cargo | breaker `macro_defined_by_foreign_macro`, hir-def test |
| the stand-in for a crate had an `extern crate` per `--extern`, and nightly cargo names each dependency twice (`E0259`) | whole-program build | stub.rs test |
| whole-program builds stopped when cargo's link of the stand-ins failed for want of `#[no_mangle]` functions C calls | whole-program build | `apps.json` `nativecalls` |
| LLVM's `instcombine` named in the `-O3` pipeline aborts the process when one run leaves work (AArch64) | whole-program build | `llvm.rs` test |
| an `asm!` of a feature LLVM's AArch64 assembler has no `.arch_extension` for (constant_time_eq's `mrs {}, dit`) was refused | the Linux aarch64 build | `asm.rs` test, corpus `inline_asm` |
| a body imported through another was called with the importer's type for it, which LLVM takes to be undefined: `ptr::write::<ForOf>` lost its write | the parser crashed in the whole-program build with imports | `llvm.rs` test |
| a function declared in a trait's provided method was compiled only when nothing was rustc's, so a call of it by blaze's name had nothing to link to | ripgrep's and helix's whole programs did not link | `wholeprogram.py` `trait_block` (fixed on main the same day, #160) |
| `simd_reduce_max`/`min` (NEON `vmaxvq_u8`) were not compiled | 164 functions | breaker `simd_reduce_max` |
| a `Box<[T], &Arena>` (an allocator with a size) was not dereferenced, and its drop read the pointer at offset 0 | 151 functions | breaker crate `allocated` |
| a borrowed constant with a cast in it (`&[b"browser" as &[u8], b"module"][..]`) was built on the stack, not promoted, and its slice outlived the frame | `new Bun.Transpiler()` crashed in the dev-loop build | breaker `promoted_index_of_casts` |
| `core::mem::type_info` in const eval (#34) | 907 functions | `bun_repros.py` `type_info` |
| a `const { .. }` block was code the function ran, not a constant per instance | 591 of 680 refusals on Linux | `type_info` |
| constants of `adt_const_params` types, and `#[derive(ConstParamTy)]` (#34) | 304 functions | `adt_const_params` |
| a path through an inherent impl's type alias (`inherent_associated_types`) | 14 functions | `inherent_associated_types` |
| a `macro_rules!` that `cfg_if!` of another crate defines was not in scope after it | 43 functions of lol_html | `cfg_if_macros` |
| an impl a macro writes in a dependency's function body was not found (dtoa's `DiyFp`) | dtoa | `macro_impls` |
| `--crate foldhash` also took std's vendored copy, which sees no `core` | 27 functions counted left | `std_copies` |
| an `$n:expr` passed on to another macro was not one operand (`task_tag::COUNT`) | `TaskTag::name` | `macro_fragments` |
| a private `#[no_mangle]` static had a name of its own, or was not compiled | `Bun__version` and the other statics bun's C++ reads were undefined | `exported_statics` |
| `--crate bun_sema` also took `bun_sema_standalone`'s binary `bun-sema` | `bun_sema_standalone::parse` undefined | `same_name` |
| blaze's object was one section, so the link kept code nothing reaches | `GetLastError` and other Windows-only externs undefined on Linux | `unreached_externs` |
| `-C force-frame-pointers` was not honoured, and LLVM gave the frame pointer to values | the whole-program binary faulted in `bun_core::return_address` | `frame_pointers` |
| bun's Linux build: `-gz=zstd` with nixpkgs' LLVM, built without zstd (bun's ELF link uses zlib); bun's position-dependent objects in a PIE; the dependencies bun links lazily and its C++ library, which it links statically | `bench-bun` on Linux | `bunprep.py` |
| bun's tree did not keep frame pointers, which bun's own build passes (`-Cforce-frame-pointers=yes`): cargo's own debug build faulted in `bun_core::return_address` on x86-64 | `bench.py bun`'s runtime on Linux | `bunprep.py` |

Filed, not fixed (read, the issues):

- [#33](https://github.com/indexable-inc/blaze/issues/33): an executable linked with
  `-no_compact_unwind` is linked in full at every edit on macOS. bunprep drops the flag.
  Fixed since (2026-10-22, `harness/nocompact.py`); bunprep drops it still, so the numbers
  here stay comparable.
- [#34](https://github.com/indexable-inc/blaze/issues/34): `adt_const_params` constants and
  `type_info` in const eval. The whole-program report left 1,645 functions out, including 907
  reaching `type_info` and 304 reaching `adt_const_params`. Both compile since 2026-10-22 (the
  table above); a constant that holds a `TypeId` is still refused, as the evaluator's ids are
  not rustc's hashes, and bun has none.
- [#35](https://github.com/indexable-inc/blaze/issues/35): everything the whole-program build
  can't compile yet, by cause, of which #34 is most. On Linux x86-64 it leaves nothing since
  2026-10-22 (Numbers, below); macOS arm64's NEON cases were fixed on the `bun` branch and the
  whole-program build was not run there again.
- [#47](https://github.com/indexable-inc/blaze/issues/47): the cold build starts over once per
  static a function left to rustc uses ("a static is used by a function left to rustc", five
  rounds on bun), where one round could leave them all.
- [#32](https://github.com/indexable-inc/blaze/issues/32) (commented): a signature edit to
  `bun_runtime` goes to cargo, because a function left to rustc (`posix_signal_handler`) uses a
  writable static (`SHOULD_ABORT`) rustc's copy has no symbol for.

## Numbers

`bench.py bun --backend cranelift --runs 3` on macOS arm64 (Apple M5 Max, 18 cores, 128 GiB),
blaze `1645168dc4`, 2026-10-21; the machine was shared, load average 28 to 45 while it ran, so
every number is noisy (read, [docs/benchmarks/bun-macos-arm64.json](../benchmarks/bun-macos-arm64.json)).
Median of three edits, warm, incremental `cargo build` → `blaze build`:

| Edit | cargo | blaze | How blaze built it |
|---|---:|---:|---|
| nothing changed | 0.17 s | 0.07 s | daemon |
| a function's body (`bun_runtime`'s CLI) | 12.2 s | 1.1 s | spliced into the executable |
| a widely used function (`bun_core`'s `StringBuilder::append`) | 26.7 s | 1.8 s | spliced |
| a generic function (`MultiArrayList::push`) | 31.8 s | 72.5 s | daemon, the whole object written and linked again |
| a new function | 12.3 s | 50.5 s | daemon, linked again |
| a signature (`bun_runtime`'s `multi_run`) | 9.8 s | 126 s | went to cargo (#32): `run` calls `posix_signal_handler`, left to rustc, which writes the static `SHOULD_ABORT` |

- Cold, until the daemon is ready: 291 s (164 s of build and 128 s until ready), against
  106 s for a clean `cargo build`; with another checkout's store, 97 s + 98 s. The daemon holds
  28 GB once ready; an edit peaks near 40 GB against cargo's 2 GB (read, same file).
- The executable runs: `Bun.Transpiler` minifying five of bun's TypeScript modules twenty times
  prints what cargo's build prints, in 1.06 s against 0.75 s for cargo's debug build and 0.12 s
  for its release build (median of ten; read, the result's `runtime`).
- Before the promotion fix in the table above, that run crashed (`SIGSEGV`): the dev loop's
  binary is the one bun's users of `Bun.Transpiler` would run (read, `bench.py` on the commit
  before it).
- Linux x86-64, `bench.py bun --runs 3` pinned to 16 cores of an AWS c8a.24xlarge (AMD EPYC
  9R45), blaze `7dcfe73922`, 2026-10-22, Cranelift (read,
  [docs/benchmarks/bun-linux-x86-64.json](../benchmarks/bun-linux-x86-64.json) as it was before
  2026-10-23): a body 9.1 s →
  0.49 s, a widely used function 16.5 s → 0.49 s, a generic function 16.7 s → 0.52 s, all
  spliced; a signature 9.2 s → 0.64 s, the object written again, where macOS's went to cargo;
  a new function 14.5 s → 19.8 s, the object written again; nothing changed 74 ms → 19 ms.
  Cold to ready 73 s against cargo's 73 s. LLVM's edits take the same within 0.02 s. The
  `Bun.Transpiler` run of the dev loop's program takes 1.58 s (Cranelift) and 3.72 s (LLVM at
  `-O2`), against 0.97 s for cargo's debug build and 0.14 s for its release build.
- blaze's whole-program build (`runbench.py bun --builds release,blaze --rounds 1`: every
  crate of bun blaze's, LLVM at `-O3`, nothing linked from rustc) links on Linux x86-64 (an
  AWS c8a.24xlarge, AMD EPYC 9R45, 96 cores, blaze `857eab0e9b`, 2026-10-22): 68,315
  functions compiled by blaze and none left, in 51.3 s against 131.4 s for cargo's release
  build. Each of `apps.json`'s eight runs gives cargo's status, stdout and stderr. Its
  `Bun.Transpiler` run takes 2.31 s against 0.39 s for cargo's release build (5.9x), 77 MB
  against 56 MB at most (read, `runbench.py`'s result). Pinned to 16 cores (`bench.py --runtime
  bun`, `7dcfe73922`): built in 86.9 s against 135.2 s, and 5.80x (read,
  [docs/benchmarks/runtime-bun-linux-x86-64.json](../benchmarks/runtime-bun-linux-x86-64.json)
  as it was before 2026-10-23). That the gap is mostly inlining across crates LLVM did not get
  to do was confirmed on 2026-10-23 (below).

### 2026-10-23: the whole program's speed, and the edit that adds a function

blaze `948d54e375` (the bun-perf lane), Linux x86-64 on 16 cores of an AWS c8a.24xlarge (AMD
EPYC 9R45) unless said otherwise.

- **Why the whole program ran 5.9x slower** (read, `perf record` of both binaries on the
  `Bun.Transpiler` run, and blaze's build log): bun's `main` is C's, so blaze built it as a
  library of entries, each function compiled on its own: LLVM saw no body of another crate's
  function and inlined nothing across crates, `#[inline]` included. A second cost was `derive(PartialEq)` of an enum, which compared
  the payloads of every variant in turn where rustc's compares the discriminants first: 23% of
  the run in one comparison chain of the lexer's tokens. Both are fixed: a program whose main
  is C's is built as a whole program, LLVM getting every body and inlining across crates
  (`edfa181e49`, test `harness/wholeprogram`'s `c_main`), and the discriminants are compared
  first (`3419d66b0e`, a unit test).
- **After** (`bench.py --runtime bun --runs 5`, read,
  [docs/benchmarks/runtime-bun-linux-x86-64.json](../benchmarks/runtime-bun-linux-x86-64.json)):
  0.495 s against 0.387 s for cargo's release build, 1.28x (was 5.80x), and 1.10x for cargo's
  release build with overflow checks; 54 MB against 57 MB at most. All 68,315 functions are
  blaze's. The build takes 565 s against 139 s for cargo's (was 87 s): every compile of a
  function now carries the bodies it may inline, 1.10 billion instructions shown to LLVM for
  89.6 million of bun's own, and LLVM's optimiser spends about 5.2k CPU-seconds on them (read,
  blaze's build statistics). A cache of the inlinable bodies across compiles would remove
  most of that; it was built the same day (below). Making shown bodies `available_externally` cut LLVM's
  optimise CPU by 41% and the build's wall time by 3%; capping what is shown made both the
  build and the program slower; neither was kept.
- **Where the remaining 1.28x is** (read, `perf`): `Lexer::next` 14.5% of the run against 13%
  in cargo's, `get_hashed` 4.3% against 2.8%, `__key_index` 3.4% against 1.2%; the rest is
  spread thinly. It is a general gap in blaze's LLVM IR (the runtime-parity lane's), not
  bun's.
- **Why adding a function took 19.8 s, slower than cargo's 14.5 s** (read, the daemon's log
  of the edit): three causes, each fixed with a test. Every function of a program whose main
  is C's is an entry, and a patch refused any change to the set of entries, so the edit
  became a full build and link (`b4399dc352`). An added function has no symbol in rustc's
  build, which the patch refused too (same commit). And the daemon of a build ahead of cargo
  listed `bun_runtime` before it had loaded the files its `include!`s name (BUN_CODEGEN_DIR's
  generated code), so 2,834 generated C-ABI functions were not entries; the first edit that
  listed it again patched 13,002 functions, and every later patch redirected about 1,940
  (`d59b7ca2cf`, `harness/latelisted.py`).
- **After** (`bench.py bun --runs 3`, read,
  [docs/benchmarks/bun-linux-x86-64.json](../benchmarks/bun-linux-x86-64.json)): a new function
  0.64 s against cargo's 14.7 s (was 19.8 s), spliced; a body 0.44 s, a signature 0.57 s, a
  widely used function 0.41 s, a generic function 0.44 s, nothing changed 18 ms; cold to
  ready 73.0 s against cargo's 73.0 s. The daemon's memory is in
  [docs/glancer.md](../glancer.md).
- **macOS arm64** (Apple M5 Max, 18 cores, by hand): the whole-program build failed to link
  for two reasons, both fixed. `runbench.py` built blaze's whole program with `-C
  link-dead-code`, which takes `-dead_strip` off the link, so the one object had to resolve
  bun's Windows-only externs (`bff512e014`); and blaze's Mach-O made every definition weak
  and private, so `-exported_symbols_list` could not export `napi_*` ("cannot export hidden
  symbol"; `108d569721`, a unit test). The run's "checked" build (cargo's release build with
  overflow checks) fails on bun's own overflow, so the Mac compares release and blaze only.
  After (`bench.py --runtime bun --runs 5 --builds release,blaze`, `948d54e375`, read,
  [docs/benchmarks/runtime-bun-macos-arm64.json](../benchmarks/runtime-bun-macos-arm64.json)):
  62,031 functions by blaze and none left, built in 983 s against 177 s for cargo's release
  build; the run takes 0.415 s against 0.333 s, 1.25x, 46 MB against 43 MB at most. This closed
  [#35](https://github.com/indexable-inc/blaze/issues/35).
- **macOS arm64 edits** (`bench.py bun --backend cranelift --runs 3`, `27077156cc`, the code of
  `948d54e375`, load average about 26 from other work, read,
  [docs/benchmarks/bun-macos-arm64.json](../benchmarks/bun-macos-arm64.json)), median, cargo →
  blaze: a body 7.5 s → 0.36 s, a widely used function 16.0 s → 0.46 s, a generic function
  14.9 s → 0.57 s (was 72.5 s), a new function 11.1 s → 0.56 s (was 50.5 s), all spliced; a
  signature 7.3 s → 0.82 s (was 126 s, cargo's); nothing changed 199 ms → 122 ms; cold to ready
  81.4 s against cargo's 83.3 s (was 291 s). The signature's median hides its first sample,
  574 s: the daemon gave its own base up at the memory bound of a 128 GiB machine, so that edit
  could not patch rustc's executable and linked bun in full, twice
  ([docs/glancer.md](../glancer.md), bun). The dev loop's `Bun.Transpiler` run takes 1.33 s,
  against 0.85 s for cargo's debug build and 0.11 s for its release build.

The reduced nightly fixtures in `harness/bun-repros` keep these two refusals measurable without
building Bun's native C and C++ dependencies. Run them with `python3 harness/bun_repros.py` after
installing `nightly-2026-09-15`. The fixtures compare blaze's binary with cargo's binary and fail
if the expected refusal reason disappears without changing the fixture's expectation.

### 2026-10-23: the whole-program build against cargo's release build

Linux x86-64 on 16 cores of an AWS c8a.24xlarge (`taskset -c 0-15`), `runbench.py NAME
--builds release,blaze`, against main `ac1ee2dd68` before.

- **Why the build took 565 s** (read, blaze's build statistics): each compile of a function
  built its callees again from MIR, as far as 16 calls deep and up to 120,000 instructions, and
  LLVM optimized them from scratch in every caller. Building the callees' IR took 3,572
  CPU-seconds and optimizing most of 3,757.
- **What replaces it** (`crates/hir-ty-codegen/imports.rs`): ThinLTO's import scheme across a
  whole program's compiles. Each callee is lowered alone and run through LLVM's ThinLTO
  pre-link pipeline once per process. A compile chooses, before LLVM runs, the exports small
  enough to be built in: 100 instructions, 300 for `#[inline]`, the 40,000 budget for
  `#[inline(always)]`, then 0.7 of that room for each call deeper, 8 deep. It links them in as
  `available_externally`, and what LLVM does not build in is dropped.
- **The limits, measured on bun** (read, build and median of 9 interleaved runs against
  cargo's release build):

  | Plain / `#[inline]` | Build | Runtime |
  |---|---:|---:|
  | 200 / 600 | 161.5 s | 1.242x |
  | 100 / 300 | 118.0 s | 1.244x |
  | 60 / 150 | 102.9 s | 1.272x |
  | 40 / 100 | 93.6 s | 1.311x |

  Other settings tried:
  - Imports 3 deep ran 1.785x: bun's hot paths go through chains of small wrappers.
  - 12 deep gave nothing over 8.
  - Leaving 0.9 of the room for each call deeper imported more and ran no faster.
  - Building the imports in before LLVM's `-O3` pipeline and dropping them there saved 19 s of
    161 but ran 1.293x.
- **Two bugs the imports showed**, each fixed with a test (the table above):
  - A body imported through another was called with the wrong type: bun's parser crashed.
  - Functions declared in provided methods were never compiled: ripgrep and helix failed to
    link. This was latent on main, where every such callee was built in.
- **After** (read):

  | Program | cargo release | main | now | Runtime, main → now |
  |---|---:|---:|---:|---:|
  | bun | 134.9 s | 563.8 s | 118.0 s | 1.267x → 1.244x |
  | ripgrep | 6.0 s | 37.5 s | 11.0 s | 1.222x → 1.15x |
  | helix | 33.4 s | 415.8 s | 64.7 s | not run → 1.24x |
  | nushell | 79.4 s | 1042.9 s | 190.4 s | 1.266x → 1.20x |

  With main of the same evening merged in (#160's ThinLTO pipelines, #171's constants), one
  run each: bun 129.2 s against 135.6 s at 1.22x, ripgrep 12.4 s against 6.0 s at 1.06x,
  helix 70.6 s against 33.5 s at 1.21x.

  Linux aarch64, bun, 16 cores of an AWS c8g.24xlarge (Neoverse-V2), main `ac1ee2dd68` before
  (one run) and `a1ca869492` after (median of 5):

  | Program | cargo release | main | now | Runtime, main → now |
  |---|---:|---:|---:|---:|
  | bun | 168.2 s | 694.1 s | 144.5 s | 1.178x → 1.198x |

  cargo's release binary ran 536 ms both times. blaze's ran 632 ms before and 643 ms after:
  about 2% slower; the limits above were tuned on x86-64 only. With main merged in (median of
  5): 160.2 s against 167.4 s, at 1.17x (624 ms against 535 ms).

- **Why helix and nushell are still slower than cargo** (read, built with nothing imported):
  every function is still a module of its own. With nothing imported:
  - helix's 225,349 compiles take 512 CPU-seconds in LLVM;
  - nushell's 421,385 take 1,151 CPU-seconds, 524 of them writing objects. That is about 72 s
    on 16 cores, against 79.4 s for cargo's whole build.

  With imports, each compile also optimizes its imports again: 52 per compile in helix, about
  three times the size of its own code. Codegen units that share their imports, as rustc
  partitions a crate, are
  [#176](https://github.com/indexable-inc/blaze/issues/176).

## What is not known

- Whether the `bench-bun` workflow runs on Linux: `bunprep.py` and `bench.py bun` do by hand in
  `nix develop .#bench-bun` on Linux x86-64 (read, Numbers), and the workflow has not run yet
  (unverified).
- bun's own test suite (`bun bd test`) was not run against blaze's binary; the comparison is
  the harness's command lines (unverified beyond those).
