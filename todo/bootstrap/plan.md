---
status: plan
summary: Bootstrap blaze from a wasm seed (blaze's own source plus blaze compiled to core WebAssembly), so no rustc, cargo or older blaze is ever needed to build blaze from nothing.
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

## Prior art

- **Zig** has bootstrapped this way since 0.11 (2022). `zig1.wasm` is committed, `wasm2c` turns it
  into C, the system C compiler builds that, and the result builds the real compiler. The C++
  stage 1 is deleted.
- **OCaml** commits a bytecode compiler (`boot/ocamlc`) that its own bytecode interpreter runs.
- **Pascal's p-code** is the original version of the idea.
- **rustc as wasm**: rustc has been built for `wasm32-wasip1-threads` and run in a browser
  ([Mearman/wasm-rustc](https://github.com/Mearman/wasm-rustc),
  [oligamiq/rubrc](https://github.com/oligamiq/rubrc)). A Rust compiler fits in wasm. The hard
  part is a compiler's dependencies, not its language.

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
| Wasmtime with Cranelift for proc macros | Cranelift JITs to native code, which a wasm guest can't run | Wasmtime with Pulley (its portable interpreter) in the seed. Not the MIR evaluator: that would be a second macro path (`one-concept-one-home`). |
| Sandbox (Linux namespaces, macOS seatbelt) | OS-specific syscalls | wasm is the sandbox: build scripts and macros run as wasm modules in the seed. |
| Daemon, sockets, fibers, live patching (mmap of code) | OS-only | Not in the seed. It is a one-shot batch build. |
| Threads | `wasm32-wasip1` has none | Single-threaded seed first, then `wasm32-wasip1-threads`. |
| C dependencies (cc-rs build scripts) | need a C compiler | The bootstrap needs a C compiler, as Zig's needs one for wasm2c output. Each C dependency of blaze gets listed and kept to a minimum. |

## Steps

1. **Native blaze builds blaze** (`blaze-builds-blaze`). Stage 0 is still cargo; the runner gates
   move to blaze.
2. **blaze builds its workspace for `wasm32-wasip1`.** This is `superset-of-rust` for wasm targets.
   Every crate that can't is a named blocker.
3. **A `seed` profile**: front end, stencil backend, in-process linker, Pulley for macros, no LLVM,
   no daemon, no sandbox. It is a cfg profile of the same crates, not a fork (`one-concept-one-home`).
4. **Bootstrap without rustc**: run `blaze-seed.wasm` under a runtime to build a native blaze from
   source, then have that blaze build the seed again. The two seeds must be byte-identical. This
   becomes a harness check.
5. **Diverse double-compiling**: rebuild the seed with a different compiler path (stage 1 built
   by cargo, for example). Building the source with that seed must give the committed seed byte
   for byte. This doesn't prevent a trusting-trust attack but can detect one, as matklad notes.
6. **Ship the seed per release**: `blaze-seed.wasm`, compressed, kept outside the git history or
   in LFS, with its content hash in the source tree.
7. **Stage 0 is the seed**: CI, the runner and `docs/contributing.md` bootstrap from it. The last
   `cargo +nightly` in blaze's own build goes away, and `no-rustc` holds from nothing.

## Rule changes when adopted

- **`no-rustc`**: add "stage 0 comes from the seed, never from rustc or cargo".
- **`proc-macros-on-wasm`**: in the seed profile Wasmtime runs macros on Pulley, since Cranelift
  can't JIT inside wasm. Native blaze keeps Cranelift-in-Wasmtime.
- **`proc-macros-on-wasm`, macro cfg**: proc-macro crates are planned with the host's cfg and built
  for wasm64 (memory64). This is the decision still open for Andrew.

## Open questions

- **Seed size.** rust-analyzer-derived front end plus stencils: tens of MB of wasm. Zig's is a
  few MB compressed. Measure it at step 3.
- **Pulley speed** for a whole self-build. Zig's wasm2c path is faster than an interpreter, and
  wasm2c of the seed could be the default runtime.
- **Who owns stencil generation** once LLVM is gone from the seed: stencils are built with LLVM
  outside the seed and committed as data with their hash, or produced by blaze's own code
  generator.
