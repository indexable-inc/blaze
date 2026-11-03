---
status: plan
summary: Bootstrap blaze from a wasm seed (blaze's own source plus blaze compiled to core WebAssembly), so blaze is built only by its seed and by blaze, never rustc or cargo, and its source may use blaze-only Rust.
date: 2026-11-02
---
# Bootstrap: blaze from a wasm seed

Today the first blaze on a machine (stage 0) is built by `cargo +nightly`, and so is every
runner gate. That is the one place `no-rustc` still cannot hold: something has to build blaze
before blaze exists. This plan removes it with the seed matklad describes in
[Reasonable Bootstrap](https://matklad.github.io/2023/04/13/reasonable-bootstrap.html):

- blaze's source at a version, and
- that source compiled by blaze to core WebAssembly (`blaze-seed.wasm`).

A small wasm runtime runs the seed, the seed builds blaze from the source for the host, and the
result rebuilds the seed byte for byte. Any version of blaze bootstraps in constant time from its
own seed, with no chain of older compilers. Nothing here is built.

## Only blaze builds blaze

The seed and the blaze toolchain are the only ways blaze gets built. Real rustc and cargo are not
part of the bootstrap and are not kept working for blaze's own source:

- **Seed N is made by blaze.** It is source N compiled to wasm by blaze N-1, or by blaze N itself
  as a fixed point. No rustc is involved.
- **blaze's source may use blaze-only Rust.** Extensions (`superset-of-rust`), `blaze::` attributes,
  blaze's own std (`library/`) and its pinned nightly features can all appear in blaze's own crates.
  Only the seed and blaze have to read that source, so it never has to build under rustc.
- **The first seed** is made once, by the blaze that exists when this lands. After that, every seed
  comes from the one before it, and today's cargo-built stage 0 is deleted.

## Prior art

- **Zig** has bootstrapped this way since 0.11 (2022). `zig1.wasm` is committed, `wasm2c` turns it
  into C, the system C compiler builds that, and the result builds the real compiler. The C++
  stage 1 is deleted.
- **OCaml** commits a bytecode compiler (`boot/ocamlc`) that its own bytecode interpreter runs.
- **Pascal's p-code** is the original version of the idea.
- **A Rust compiler in wasm** has been done: rustc built for `wasm32-wasip1-threads`
  ([Mearman/wasm-rustc](https://github.com/Mearman/wasm-rustc),
  [oligamiq/rubrc](https://github.com/oligamiq/rubrc)). That only shows a Rust compiler fits in
  wasm; blaze's seed is blaze, not rustc. The hard part is the dependencies, not the language.

## Why it fits blaze

The seed relies on four properties. blaze already promises or has most of them:

| Property the seed needs | blaze today |
|---|---|
| Deterministic output: the seed rebuilt by itself is byte-identical | `deterministic-output` (rule 22) already requires it |
| A target-agnostic seed | wasm |
| A simple runtime to run the seed | Wasmtime's Pulley interpreter, or any spec interpreter, or wasm2c |
| A seed that cross-compiles to every target | native code is copy-and-patch stencils (rule 8); the stencils are data and can ship inside the seed |

It also settles a question that is open now: proc-macro crates should be planned with the host's
cfg (as rustc does) and built for wasm64. When blaze itself can run as wasm, "the host" can be
wasm too, and the rule stays one rule.

## What blocks blaze from compiling itself to wasm

| Piece | Why it can't go in the seed as is | Seed answer |
|---|---|---|
| LLVM (`llvm-sys`, the pinned LLVM) | C++, tens of MB, links into blaze; `blaze-stencil/build.rs` makes stencils with it | The seed has no LLVM. Stencils come prebuilt as data. Release (LLVM) builds come after stage 1. |
| Wasmtime with Cranelift for proc macros | Cranelift JITs to native code, which a wasm guest can't run | Wasmtime with Cranelift emitting Pulley bytecode, run by Pulley, inside the seed. Shown working on wasm32-wasip1 and -threads (lane wasmhost, 2026-11-02) after a 46-line patch: `std` without virtual memory lacks `sys::mmap` upstream. The embedder supplies two TLS functions. Not the MIR evaluator or wasmi: a second macro path (`one-concept-one-home`). |
| Sandbox (Linux namespaces, macOS seatbelt) | OS-specific syscalls | wasm is the sandbox: build scripts and macros run as wasm modules in the seed. |
| Daemon, sockets, fibers, live patching (mmap of code) | OS-only | Not in the seed. It is a one-shot batch build. |
| Threads | `wasm32-wasip1` has none | Single-threaded seed first, then `wasm32-wasip1-threads`. |
| C dependencies (cc-rs build scripts) | need a host C compiler | The seed has no C: see below. |

## The seed has no C

`blaze` (package in `crates/blaze-engine`) reaches 13 crates whose build scripts compile C, C++
or assembly through cc-rs, which runs whatever `cc` is on the host. A host C compiler would be
a second compiler in the bootstrap, so the seed profile has none of them. Count from Cargo.lock,
2026-11-02:

| Crate | What its C is for | In the seed |
|---|---|---|
| llvm-sys | C++ wrappers around LLVM | out: the seed has no LLVM |
| tree-sitter, tree-sitter-{python,rust,toml-ng} | C parsers | out |
| flecs-c | the site's Flecs runtime | out |
| libmimalloc-sys | allocator | out: std's allocator |
| blake3, blake3-servil | SIMD C and asm | blake3's `wasm32_simd` (Rust simd128, no C); servil is not reached on wasm32. Native builds keep the asm. |
| zstd-sys | store compression | a pure-Rust zstd (ruzstd), same frame format |
| ring | TLS for registry fetches | out: the seed builds offline from vendored sources |
| psm, stacker | stack growth for deep recursion | already not built on wasm: every call goes through `stdx::ensure_sufficient_stack`, a plain call there. The seed gets a big stack instead (below). |
| wasmtime, wasmtime-internal-{fiber,jit-debug} | small C and asm helpers | none on wasm32: its build script drops the C helpers, signals and mmap there, and fiber/jit-debug come only with `async`/`debug-builtins`, which blaze doesn't enable. |

blaze compiling C itself (owning a C front end) would also remove the host compiler, for every
C dependency anywhere. That is larger and is not needed for the seed.

Every native build keeps the fastest implementation for its architecture (C zstd, blake3 asm,
mimalloc, LLVM). Only the seed takes the fastest implementation without C.

## Stack in the seed

The deepest expression nesting over all 21,570 files the seed compiles is 98 levels (32 in blaze's
own crates); macro expansion and the trait solver stop at `recursion_limit` (512 at most). At the
release binary's frame sizes that is about 0.4 MiB for expressions and 3 MiB for the solver. The
seed links with a 64 MiB stack (the linker's `Output.stack`, unset today, so 1 MiB), the runtime
gives the engine a 192 MiB stack, and on wasm `ensure_sufficient_stack` checks the room left and
stops with an error naming it, never an overflow into the heap.

## Steps

1. **Native blaze builds blaze** (`blaze-builds-blaze`). The runner gates move to blaze.
2. **blaze builds its workspace for `wasm32-wasip1`.** This is `superset-of-rust` for wasm targets.
   Every crate that can't is a named blocker.
3. **A `seed` profile**: front end, stencil backend, in-process linker, Pulley for macros, no LLVM,
   no daemon, no sandbox. It is a cfg profile of the same crates, not a fork (`one-concept-one-home`).
4. **Bootstrap without rustc**: run `blaze-seed.wasm` under a runtime to build a native blaze from
   source, then have that blaze build the seed again. The two seeds must be byte-identical. This
   becomes a harness check.
5. **Cross-version check**: seed N-1 builds source N into a blaze that builds seed N, and that
   seed must equal the one blaze N makes of itself, byte for byte. Two seeds of different versions
   agreeing is the only second opinion; there is no rustc path to compare against.
6. **Ship the seed per release**: `blaze-seed.wasm`, compressed, kept outside the git history or
   in LFS, with its content hash in the source tree.
7. **Stage 0 is the seed**: CI, the runner and `docs/contributing.md` bootstrap from it. The cargo
   stage 0 is deleted, nothing keeps blaze's source rustc-compatible, and `no-rustc` holds from
   nothing.

## Rule changes when adopted

- **`no-rustc`**: add "stage 0 comes from the seed, never from rustc or cargo; blaze's own source
  need not build under rustc".
- **`proc-macros-on-wasm`**: in the seed profile Wasmtime runs macros on Pulley, since Cranelift
  can't JIT inside wasm. Native blaze keeps Cranelift-in-Wasmtime.
- **`proc-macros-on-wasm`, macro cfg**: proc-macro crates are planned with the host's cfg and built
  for wasm64 (memory64). This is the decision still open for Andrew.

## Open questions

- **Seed size.** rust-analyzer-derived front end plus stencils: tens of MB of wasm. Zig's is a
  few MB compressed. Measure it at step 3.
- **Seed runtime speed.** Pulley ran a call-heavy fib about 13.5x slower than native Cranelift, and inside
  a wasm guest only about 8% slower again; macros in a seed that itself ran on an interpreter would be
  interpreted twice (about 100x), so the seed runs under a JIT (Wasmtime) or wasm2c, not an interpreter.
- **Memory.** The seed must fit wasm32's 4 GiB, or move to wasm64.
- **Pulley speed** for a whole self-build. Zig's wasm2c path is faster than an interpreter, and
  wasm2c of the seed could be the default runtime.
- **Who owns stencil generation** once LLVM is gone from the seed: stencils are built with LLVM
  outside the seed and committed as data with their hash, or produced by blaze's own code
  generator.
