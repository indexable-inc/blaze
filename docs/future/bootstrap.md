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

It also settles a question that is open now: proc-macro crates should be planned with the host's
cfg (as rustc does) and built for wasm64. When blaze itself can run as wasm, "the host" can be
wasm too, and the rule stays one rule.

## What blocks blaze from compiling itself to wasm

| Piece | Why it can't go in the seed as is | Seed answer |
|---|---|---|
| LLVM (`llvm-sys`, the pinned LLVM) | C++, tens of MB, links into blaze; `blaze-stencil/build.rs` makes stencils with it | The seed has no LLVM. Stencils come prebuilt as data. Release (LLVM) builds come after stage 1. |
| Wasmtime with Cranelift for proc macros | Cranelift JITs to native code, which a wasm guest can't run | Wasmtime with Cranelift emitting Pulley bytecode, run by Pulley, inside the seed. Shown working on wasm32-wasip1 and -threads (lane wasmhost, 2026-11-02) after a 46-line patch: `std` without virtual memory lacks `sys::mmap` upstream. The embedder supplies two TLS functions. Not the MIR evaluator or wasmi: a second macro path (`one-concept-one-home`). |
| Sandbox (Linux namespaces, macOS seatbelt) | OS-specific syscalls | wasm is the sandbox: build scripts and macros run as wasm modules in the seed. |
| Daemon, sockets, live patching (mmap of code) | OS-only | Not in the seed. It is a one-shot batch build. |
| Fibers (corosensei) | no wasm backend: wasm has no stack to swap | The seed host's fibers (`blaze_seed.fiber_*`, Wasmtime's own fibers) behind the scheduler's one stack layer: "Fibers in the seed" below. |
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

The scheduler runs every pool job on a stackful fiber, and a job that waits switches out holding
no thread. corosensei has no wasm backend and wasm has no native stack to swap, so the seed swaps
only the layer that switches stacks, `blaze-graph`'s `sched::stack`, chosen by target in that one
module. The scheduler, pool and wait graph above it are the same code on every target (rule 30),
and native targets always use corosensei (rule 3).

On wasm the layer is the host's fibers, through the seed's own calls (lane wasm64fiber,
2026-11-02; it replaced Binaryen's Asyncify, which never ran under blaze).
`blaze_seed.fiber_new(entry, arg, stack_pointer)` makes a fiber that runs the guest function at table slot `entry` as
`entry(arg)`; `fiber_resume(fiber)` runs it until it calls `fiber_suspend()` (0) or `entry`
returns (1); `fiber_drop(fiber)` ends it, unwinding a suspended one with no guest code run. The
runner runs each on one of Wasmtime's own fibers: a native stack like an async call's, with the
engine's per-call state (the thread's activations, the stack limit, the guard range, the
executor) saved and restored at each switch, as Wasmtime's `call_async` and component tasks do
(vendor/wasmtime change 3, `GuestFiber`). A fiber's wasm locals and return addresses are on that
native stack; the address-taken memory of its frames is on its shadow stack, a region of linear
memory, and the host switches `__stack_pointer` to it while the fiber runs and back as it
suspends (blaze-link exports `__stack_pointer` and the function table from every wasm64 module
for this). So a switch costs the same at any depth, no function is transformed or instrumented,
nothing in the module's code depends on which functions may suspend, and resumes nest as on
native. A host without these calls (any but the seed's runner, a wasm32 host) refuses the module
at instantiation, naming them. hir-ty-suspend's root is the layer's
`blaze_graph::sched::stack::Yielder::suspend`, the same path on native and on wasm, and a
`blaze_graph` without it is its error `MissingRoot` (in `Found::missing_root`). Left: blaze-lints
reads only `suspend_points` and `on_fiber`, which are empty then, so it must report
`missing_root` (and `no_send`) itself; today neither reaches a user.

The seed builds with panic=abort: that is wasm32-wasip1's `panic-strategy` in the pinned nightly's
target spec (`rustc +nightly-2026-10-27 -Z unstable-options --print target-spec-json --target
wasm32-wasip1`). So a job's panic aborts the whole seed, naming it, and the `catch_unwind` around
each job catches nothing: rule 20 (panics fail one job) can't hold in the seed until blaze's wasm
codegen supports exception handling (panic=unwind). A wasm fiber's stack has no guard page, so on
a fiber `stdx::ensure_sufficient_stack` checks the room left and panics, naming it, which aborts
the seed.

The stack size is chosen per target in `sched::stack` (`FIBER_STACK`), for both drivers: 16 MiB
on native (reserved, committed as touched); on wasm a 4 MiB shadow stack taken whole from linear
memory when the fiber is made, and the runner's native stack per fiber (`FIBER_STACK` in
blaze-seed-run: the 192 MiB wasm stack plus 16 MiB for host frames, reserved, committed as
touched).

Probe (lane wasm64fiber, 2026-11-02, ix VM, arm64): `sched/stack/wasm.rs` built into a probe for
wasm64-blaze-wasip1 by blaze (debug; probe-fiber.wasm, 3.81 MB, sha256 f7743f89...), run by
blaze-seed-run. It passed every check: two fibers interleaved, each suspending 50 frames deep;
each fiber's frames were on its own shadow stack, and the root's `__stack_pointer` was its own
after every switch; a body that returned had its frames intact; a fiber suspended mid-body was
dropped; resumes nested (a fiber resumed another from inside itself). 200,000 resume-and-suspend
round trips 20 frames deep took 402 ns each. The module's imports are the seed's 16 `blaze_seed`
calls (the four fiber calls among them) and 20 `wasi_snapshot_preview1_64` calls, nothing else.
`blaze build -p blaze-query --target wasm64-blaze-wasip1` builds.

Why not the others:

- **The stack-switching proposal** (`cont.new`, `resume`, `suspend`). Wasmtime 50.0.1 has it
  only for Cranelift on x86_64 Linux and macOS (`compiler_panicking_wasm_features`; its
  continuation stacks exist only for `unix, x86_64`): not on aarch64, not on Pulley, and it turns
  inlining off. wasm2c doesn't have it, and blaze's wasm codegen would have to emit typed
  continuations and tags. It is where this layer can go once the seed's runtimes have it on every
  host: the same calls, made by the guest's own instructions, so nothing above the layer changes.
- **Binaryen's Asyncify, or blaze's own pass like it.** Every function on a path from a fiber's
  entry to its suspend is instrumented (indirect calls count, so nearly all of the front end),
  which slows all of that code and grows it; each switch saves and reloads every frame between the
  body and the suspend, so a wait deep in the solver pays its depth; a frame left out is silent
  corruption; frames don't nest. Run as blaze's own linker pass it would be a second code
  transform of every function, to do what the host does in one switch.
- **Running the awaited job to completion on the waiter's stack.** A job started inside
  another's wait can end only before it. So a wait that native code parks becomes a deadlock the
  wait graph can't see, unless the inner run is limited to what the waiter waits on, which the
  pool's queues don't know. The seed's builds would then differ from native ones, and stacks would
  nest job inside job. That is a second scheduling discipline.
- **A thread per fiber** (wasm32-wasip1-threads). wasm32-wasip1 has no threads, and Wasmtime
  makes an instance per thread.

Limits: the seed's fibers need its runner (Wasmtime): a runtime without them, such as wasm2c or
a spec interpreter, has to implement the four calls (wasm2c's C host can, with its own stack
switch). Each fiber holds a native stack of the runner's for its life, reserved rather than
committed. A fiber suspended mid-body and dropped is unwound in the host with no guest code run,
and its shadow stack is leaked, since its frames may be linked from elsewhere.

**One thread** (built 2026-11-03). The seed starts no thread, so its pools are driven
(`Pool::driven`, `crates/blaze-graph/src/pool.rs`): `Pool::new` on a target without threads
starts none, and `Topology::detect` there is one CPU. Each worker's loop, the same `work` every
native worker runs, runs on a fiber of `sched::stack` (`sched::Driven`). The thread that made
the pool gives the workers turns whenever it waits on no fiber (`block_until`, under a client's
`sched::wait`, so `Pool::scope`, `install` and every query wait of it) and as the pool drops,
until every job given it has run. A worker gives its turn back after each job it ran, and where
a worker of its own thread would park. The driving thread then sleeps until a job's timed wait
is due, else asks for a stall; with neither on a target without threads, it panics naming every
wait of the graph, since no other thread could end it. Queues, fibers, parking, waits, cycles
and stalls are the native code; only who runs the loop differs (`one-concept-one-home`). Rule
24 holds by construction: a driven worker and its jobs are on the one thread that drives it.

Proof (2026-11-03): `crates/blaze-graph/tests/driven.rs` and `crates/blaze-query/tests/driven.rs`
run on native, and their scenarios (`tests/driven/scenarios.rs`, 6 of blaze-graph and 2 of
blaze-query) also ran built for `wasm64-blaze-wasip1` by blaze and under `blaze-seed-run`, all
passing: one worker freed by an event wait and by a query waiting on another's flight, a cycle
of key waits refused, nested scopes, a job of one driven pool waiting on another's, a pool's
drop running its jobs, and a cross-view query cycle in which exactly one wait gives way.

Still open: every blocking wait on a client has to be a scheduler wait (exec's channel
receives are not), the seed runner serves no sleep (`poll_oneoff`), so a timed wait stops it,
and a give-way that unwinds (`Cancelled::throw`) aborts the seed, which is built `panic=abort`.

## The seed is wasm64, and its runner

Andrew's decision (2026-11-02): the seed targets wasm64 (memory64), so it is not held to
wasm32's 4 GiB. Native blaze is unchanged. The target is blaze's own (lane wasm64cg defines it
in the backend and linker); this section is mostly the host side.

**The target** is `wasm64-blaze-wasip1`, defined once in
`crates/blaze-units/src/stdlib/own_targets.json` (merged into rustc's recorded targets by
`stdlib/target.rs`, never rewritten by `harness/answers.py`): `target_arch="wasm64"`,
`target_vendor="blaze"`, `target_os="wasi"`, `target_env="p1"`, `target_family="wasm"`,
`target_pointer_width="64"`, panic abort, crt-static, data layout
`e-m:e-p:64:64-p10:8:8-p20:8:8-i64:64-i128:128-n32:64-S128-ni:1:10:20`, entry `__main_void`.
Code is memory64: addresses, the `__stack_pointer` global, `memory.size`/`memory.grow` and the
`memory.copy`/`memory.fill` operands are `i64`; the function table stays 32-bit and a function
pointer is wrapped to `i32` at `call_indirect`. Relocations are `WasmMemory64`/`WasmTable64`
(`R_WASM_MEMORY_ADDR_SLEB64`/`_I64`, `R_WASM_TABLE_INDEX_SLEB64`/`_I64`); blaze-link writes
memory64 modules (`Target::Wasm64Wasip1`) and refuses an object of the other width. There is no
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
trait that says what each call does. The macro sandbox's rules are its `System`
(`blaze-macro-eval`'s `wasm/wasi.rs`, unchanged: reads traced below the crate's roots,
writes and waits refused, stdout dropped); the seed runner's is the host's real files and
stdio. A wasm64 macro later links the same calls at `W64` over the same sandbox.

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
time, `u64::MAX - 1` sets it to now), `set_len`, `sync`, `pread`, `pwrite`, and the fibers' `fiber_new`, `fiber_resume`,
`fiber_suspend` and `fiber_drop` ("Fibers in the seed"). Only an embedder
whose store implements `seed::EmbedSeed` links them: the seed runner. The macro sandbox stays
real wasip1 at 32 bits, unchanged: its 24 calls, no writes, and `blaze_seed`,
`wasi_snapshot_preview1_64` and p1's writing calls refused at load (pinned by blaze-macro-eval's
`a_macro_is_served_only_the_sandboxs_wasi_and_never_a_call_that_writes`).

**The runner** is `crates/blaze-seed-run` (library and
`blaze-seed-run [--memory-max=SIZE] SEED.wasm [ARG...]`),
not a blaze subcommand: it runs before any blaze exists on the machine, so it links Wasmtime
and `blaze-wasi` and none of blaze's engine.

- Memory: a Wasmtime `MemoryCreator` reserves the runner's maximum (`--memory-max`, 8 TiB by
  default) as one `PROT_NONE`, `MAP_NORESERVE` mapping; growth `mprotect`s pages writable
  without charging commit (Linux's `VM_NORESERVE` under `vm.overcommit_memory` 0 or 1; macOS
  charges nothing until touched); `memory_reservation` equal to the maximum,
  `memory_may_move(false)`, no guard region, no store limiter, data segments copied
  (`memory_init_cow(false)`). Lane seedmem measured Wasmtime 50.0.1's defaults instead: every
  grow copies the whole memory, 21 to 33 GB resident before the seed died. A page costs memory
  only once the seed touches it: grown to 1 TiB with 100 MiB touched, the memory holds 100 MiB
  resident (the crate's test, measured with `mincore`).
- Why 8 TiB: native blaze, in the seed's configuration with one worker, held 2,879 GiB of
  reservations in 8,276 regions at its committed peak (query 953 GiB in 485 regions, link 883
  GiB in 7,028, chunks 770 GiB in 36, intern 129 GiB), and that build stopped after 66 of about
  500 units (v2-seedmem, 2026-11-02), so a whole self-build reserves more. On wasm64 each
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
2. **blaze builds its workspace for `wasm32-wasip1`.** This is `superset-of-rust` for wasm targets.
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
  for wasm64 (memory64). This is the decision still open for Andrew.

## Open questions

- **Seed size.** rust-analyzer-derived front end plus stencils: tens of MB of wasm. Zig's is a
  few MB compressed. Measure it at step 3.
- **Seed runtime speed.** Pulley ran a call-heavy fib about 13.5x slower than native Cranelift, and inside
  a wasm guest only about 8% slower again; macros in a seed that itself ran on an interpreter would be
  interpreted twice (about 100x), so the seed runs under a JIT (Wasmtime) or wasm2c, not an interpreter.
- **Memory.** Settled: the seed is wasm64 (above). How much it touches is measured at step 3.
- **Pulley speed** for a whole self-build. Zig's wasm2c path is faster than an interpreter, and
  wasm2c of the seed could be the default runtime.
- **Who owns stencil generation** once LLVM is gone from the seed. Built (2026-11-02, lane
  seedengine): the stencil libraries are made with LLVM by native builds and committed as data
  in `crates/blaze-stencil/prebuilt/`, pinned by `stencils.json`. The seed includes those bytes;
  every LLVM build checks it made exactly them, so a generator change commits new libraries in
  the same change. No variable points the seed elsewhere (`BLAZE_STENCILS` is gone). Whether
  blaze's own code generator later makes them instead stays open.
