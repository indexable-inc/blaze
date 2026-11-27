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
| Deterministic output: the seed rebuilt by itself is byte-identical | `deterministic-output` (rule 25) already requires it |
| A target-agnostic seed | wasm |
| A simple runtime to run the seed | Wasmtime's Pulley interpreter, or any spec interpreter, or wasm2c |
| A seed that cross-compiles to every target | native code is copy-and-patch stencils (rule 10); the stencils are data and can ship inside the seed |

It also keeps one rule for proc macros: their crates are planned with the host's cfg (as rustc
does) and built for wasm64. When blaze itself can run as wasm, "the host" can be
wasm too, and the rule stays one rule.

## What blocks blaze from compiling itself to wasm

| Piece | Why it can't go in the seed as is | Seed answer |
|---|---|---|
| LLVM (`llvm-sys`, the pinned LLVM) | C++, tens of MB, links into blaze | The seed has no LLVM. Stencils come prebuilt as data. Release (LLVM) builds come after stage 1. |
| Wasmtime with Cranelift for proc macros | Cranelift JITs to native code, which a wasm guest can't run | Wasmtime with Cranelift emitting Pulley bytecode, run by Pulley, inside the seed. Shown working on wasm32-wasip1 and -threads after a 46-line patch: `std` without virtual memory lacks `sys::mmap` upstream. The embedder supplies two TLS functions. Not the MIR evaluator or wasmi: a second macro path (`one-concept-one-home`). |
| Sandbox (Linux namespaces, macOS seatbelt) | OS-specific syscalls | wasm is the sandbox: build scripts and macros run as wasm modules in the seed. |
| Sockets, live patching (mmap of code) | OS-only | Not in the seed. It is a one-shot batch build. |
| Fibers (corosensei) | no wasm backend: wasm has no stack to swap | None needed: "Fibers in the seed" below. |
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
| libmimalloc-sys | allocator | out: std's allocator |
| blake3, blake3-servil | SIMD C and asm | blake3's `wasm32_simd` (Rust simd128, no C); servil is not reached on wasm32. Native builds keep the asm. |
| zstd-sys | the store's, cold columns', Arrow IPC's and Parquet's compression | vendor/zstd-sys: native builds compile C zstd 1.5.7 (fastest on aarch64); on wasm it compiles no C and links libzstd-rs-sys, a Rust translation of zstd's dev branch after 1.5.7 (it reports 1.5.8), whose bytes matched C's on a store's values, a measurement the blaze-cas and blaze-format fixtures pin (fastest pure-Rust on wasm32; ruzstd has no dictionary encoder). Every caller keeps the `zstd` crate. |
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

## Fibers in the seed

There are none. A task that lacks a fact returns `Need`, parks, and resumes when the producers
have merged; nothing starts over and no task holds a stack across a wait (D157, D161). So the
seed needs no stack switching, and wasm having no native stack to swap does not matter.

The seed builds with panic=abort: that is wasm32-wasip1's `panic-strategy` in the pinned nightly's
target spec. So a job's panic aborts the whole seed, naming it: rule 20 (panics fail one job)
can't hold in the seed until blaze's wasm codegen supports exception handling (panic=unwind).


## The seed is wasm64, and its runner

Andrew's decision (2026-11-02): the seed targets wasm64 (memory64), so it is not held to
wasm32's 4 GiB. Native blaze is unchanged. The target is blaze's own; this section is mostly
the host side.

**The target** is `wasm64-blaze-wasip1`: `target_arch="wasm64"`,
`target_vendor="blaze"`, `target_os="wasi"`, `target_env="p1"`, `target_family="wasm"`,
`target_pointer_width="64"`, panic abort, crt-static, data layout
`e-m:e-p:64:64-p10:8:8-p20:8:8-i64:64-i128:128-n32:64-S128-ni:1:10:20`, entry `__main_void`.
Code is memory64: addresses, the `__stack_pointer` global, `memory.size`/`memory.grow` and the
`memory.copy`/`memory.fill` operands are `i64`; the function table stays 32-bit and a function
pointer is wrapped to `i32` at `call_indirect`. Relocations are `WasmMemory64`/`WasmTable64`
(`R_WASM_MEMORY_ADDR_SLEB64`/`_I64`, `R_WASM_TABLE_INDEX_SLEB64`/`_I64`); the linker writes
memory64 modules and refuses an object of the other width. There is no
wasi-libc for it and the seed has no C: std links `library/wasi_rt`, a Rust runtime giving
`_start`, malloc (dlmalloc) and the libc symbols std calls, over the `wasip1` crate, whose raw
imports take `usize` pointers and sizes from `wasi_snapshot_preview1_64` on 64-bit. libc's
`-lc` is provided by that crate on this target alone.

**WASI at 64 bits.** The seed makes WASI preview 1 calls from the import module
`wasi_snapshot_preview1_64`. Each call keeps its witx name and meaning; every `pointer` and
`size` argument is an `i64`, and every `size` in memory is 8 bytes at 8-byte alignment: an
`iovec` is 16 bytes (`buf` at 0, `buf_len` at 8), a `prestat` is 16 (tag at 0, `pr_name_len`
at 8), each `argv` and `environ` pointer is 8, and `nwritten`, `nread`, `bufused` and the
`*_sizes_get` outputs are 8. Everything else keeps its witx type and layout: descriptors,
`errno` and flags `i32`; timestamps, rights, offsets and cookies `i64`; `fdstat` (24 bytes),
`filestat` (64) and `dirent` (24 plus its name) unchanged. A distinct module name keeps a
32-bit guest from ever linking the 64-bit calls, or the reverse.

**One WASI host.** `crates/blaze-wasi` is the one implementation of these calls' arguments
and memory layout, generic over the guest's pointer width (`W32`, `W64`), over a `System`
trait that says what each call does. The macro sandbox's rules are its `System` (reads
traced below the crate's roots, writes and waits refused, stdout dropped); the seed runner's is
the host's real files and stdio. A wasm64 macro links the same calls at `W64` over the same
sandbox.

**The seed's ABI is blaze's own** (Andrew, 2026-11-02). The seed's host need not be WASI:
`wasi_snapshot_preview1_64` is already no standard. The p1-shaped calls that work stay as
they are; everything the seed lacks is blaze's own function in the module `blaze_seed`, shaped
for blaze's std rather than copied from p1: every pointer, length, offset and time an `i64`,
descriptors and flags `i32`, an `i32` answer (0 or the WASI `errno` number), outputs through an
`i64` pointer, paths `(ptr, len)` and absolute, no rights, fdflags or preopens. Its one home is
`blaze_wasi::seed` (`MODULE`, `FUNCTIONS`, `VERSION`, the `Seed` trait, `link`); it is
versioned (`abi_version()` answers `VERSION`, 1 now; a change to a call's shape or meaning bumps
it, a new call does not). Version 1: `abi_version`, `decommit`, `create_dir`, `remove_dir`,
`unlink`, `rename`, `hard_link`, `symlink`, `set_times` and `fd_set_times` (`u64::MAX` keeps a
time, `u64::MAX - 1` sets it to now), `set_len`, `sync`, `pread`, `pwrite`, and `fiber_new`,
`fiber_resume`, `fiber_suspend` and `fiber_drop`. Only an embedder whose store implements
`seed::EmbedSeed` links them: the seed runner.

**The runner** is to be `blaze-seed-run` (not built:
`blaze-seed-run [--memory-max=SIZE] SEED.wasm [ARG...]`),
not a blaze subcommand: it runs before any blaze exists on the machine, so it links Wasmtime
and `blaze-wasi` and none of blaze's engine.

- Memory: a Wasmtime `MemoryCreator` reserves the runner's maximum (`--memory-max`, 8 TiB by
  default) as one `PROT_NONE`, `MAP_NORESERVE` mapping; growth `mprotect`s pages writable
  without charging commit (Linux's `VM_NORESERVE` under `vm.overcommit_memory` 0 or 1; macOS
  charges nothing until touched); `memory_reservation` equal to the maximum,
  `memory_may_move(false)`, no guard region, no store limiter, data segments copied
  (`memory_init_cow(false)`). Measured with Wasmtime 50.0.1's defaults instead: every
  grow copies the whole memory, 21 to 33 GB resident before the seed died. A page costs memory
  only once the seed touches it: grown to 1 TiB with 100 MiB touched, the memory holds 100 MiB
  resident (measured with `mincore`).
- Why 8 TiB: native blaze, in the seed's configuration with one worker, held 2,879 GiB of
  reservations in 8,276 regions at its committed peak (query 953 GiB in 485 regions, link 883
  GiB in 7,028, chunks 770 GiB in 36, intern 129 GiB), and that build stopped after 66 of about
  500 units (2026-11-02), so a whole self-build reserves more. On wasm64 each
  reservation is linear memory, so the seed's memory grows to that sum.
- Transparent huge pages are off for the reservation (`MADV_NOHUGEPAGE` on Linux): blaze-mem's
  wasm64 reservations are 64 KiB-aligned and lightly touched, and under
  `transparent_hugepage=always` each first touch would fault in 2 MiB. blaze-mem's page on
  wasm64 is the 64 KiB wasm page, which every host page divides.
- `blaze_seed::decommit(ptr: i64, len: i64)` gives pages back; the range reads zeros after
  (Linux `MADV_DONTNEED`; macOS a fresh fixed mapping, since its `MADV_DONTNEED` keeps the
  bytes; partial pages at the ends are zeroed). blaze-mem's `os.rs` on wasm64 reserves with
  `memory.grow`, never the global allocator (whose reuse zeroes a range by writing it, making
  it resident), and decommits, purges and releases through the import. A released range,
  decommitted and reading zeros, goes to a free list by address (neighbours merged) that the
  next reservation takes first (smallest fit), so link's thousands of short regions reuse
  address space instead of growing the memory by each. wasm32 (whose hosts have no
  `blaze_seed` import) keeps reservations from the global allocator.
- The seed runs on a thread with a 256 MiB stack, 192 MiB of it for wasm.
- Linux and macOS only; Windows is a typed error naming it.

**Next: a multi-threaded seed** (not built yet). Host functions make it possible: the seed's
memory becomes shared (`shared` memory64), and a host `blaze_seed.thread_spawn(entry, arg)`
starts a new instance of the seed's module on a host thread over that same memory, each with
its own stack and TLS, as wasi-threads does. blaze's pool then runs its workers on those
threads instead of one. Until then the seed runs one worker.

Open: how the runner binary itself reaches a machine with no blaze (shipped per platform with
the seed, built by the blaze that made the seed; or the seed through wasm2c and the system C
compiler, as Zig does, which brings a C compiler back).

## Steps

1. **Native blaze builds blaze** (`blaze-builds-blaze`). The runner gates move to blaze.
2. **blaze builds its workspace for `wasm64-blaze-wasip1`.** This is `superset-of-rust` for wasm targets.
   Every crate that can't is a named blocker.
3. **A `seed` profile**: front end, stencil backend (std too, by `codegen-backend = "stencil"`),
   rsasm for assembly, in-process linker, Pulley for macros, no LLVM, no daemon, no sandbox. It is a cfg profile of the same crates, not a fork (`one-concept-one-home`).
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
  for wasm64 (memory64).

## Open questions

- **Seed size.** rust-analyzer-derived front end plus stencils: tens of MB of wasm. Zig's is a
  few MB compressed. Measure it at step 3.
- **Seed runtime speed.** Pulley ran a call-heavy fib about 13.5x slower than native Cranelift, and inside
  a wasm guest only about 8% slower again; macros in a seed that itself ran on an interpreter would be
  interpreted twice (about 100x), so the seed runs under a JIT (Wasmtime) or wasm2c, not an interpreter.
- **Memory.** Settled: the seed is wasm64 (above). How much it touches is measured at step 3.
- **Pulley speed** for a whole self-build. Zig's wasm2c path is faster than an interpreter, and
  wasm2c of the seed could be the default runtime.
- **Who owns stencil generation** once LLVM is gone from the seed: LLVM-made stencil libraries
  committed as data, which every LLVM build checks it made exactly, or blaze's own code
  generator.
