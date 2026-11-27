---
status: plan
summary: How blaze runs proc macros and pure-Rust build scripts (compiled by blaze itself to wasm64 (memory64) modules and run in-process under Wasmtime, sandboxed; native tools and C-linking build scripts go to the hypervisor micro-VM sandbox), and how it keys and keeps the expansions.
date: 2026-11-01
---
# Proc macros

blaze compiles proc-macro crates and pure-Rust build scripts itself, off `blaze-lir`, to wasm64
(memory64) modules, and runs them in-process under Wasmtime, sandboxed: no ambient files,
network or clock, fuel and bounded memory (rule 18). 64-bit pointers match the host's layout, so
there is no wasi-libc, wasm linker or register allocator, and a module is the same on every
host. An existing crate's macro runs unchanged: its `proc_macro` library is blaze's. Native tools
and C-linking build scripts (`cc`, `clang`, `cmake`) are the remaining gap: they go to blaze's
hypervisor micro-VM sandbox ([end-state](end-state.md) D177's `Level::Execution`), which is not
run without the owner's word. blaze's own code needs no macro: `#[comptime]` reflection. The
decision is [end-state](end-state.md) D175.

Part of [usercode](../../plan.md). Code:

- `crates/blaze-expand/`: macro calls and their expansion records, item-relative.
- `crates/blaze-cli/src/wasm64_compile.rs`: a macro crate compiled to a module, through
  `crates/blaze-export/src/lir_wasm64.rs` and `crates/blaze-export/src/wasm64.rs`.
- `crates/blaze-proc-macro/src/wasm64_runtime.rs`: the Wasmtime store (memory64, fuel, bounded
  memory and tables, deterministic NaNs) and its three host imports, bounds-checked.
- `crates/blaze-proc-macro/`: the bridge's server, the sandbox that serves the package's files
  and variables read-only and records every read, and the expansion record.

A module that cannot be built or run is an error that names the macro. It is never run
natively in its place.

## Keys

An expansion is a row of its crate's results table, `base -> read list -> answer`
(`crates/blaze-proc-macro/src/record.rs`):

- The base is computed from text before the run: the module's digest, the macro's index, name
  and kind, the edition, and the input's and the attribute's tokens as the macro sees them
  (kind, spacing and text; never a position or a context, which it cannot see).
- The read list names what the run read (`env:NAME`, `file:path`, `dir:path`, `stat:path`,
  paths from the workspace root) and the answer each had. A lookup values each now, so a
  changed file or variable misses exactly the expansions that read it.
- The answer is the expansion in canonical bytes. Two runs of one key write equal bytes
  wherever the checkout is; two answers under equal reads and digests are refused (rule 21).

## The runtime inside `blaze`, the identity without it

- Wasmtime is linked into `blaze`, behind blaze-cli's feature `sandbox` (on by default): no
  helper process, no second executable, one code path. A command that expands nothing must not
  pay for it.
- blaze-proc-macro is split by its feature `runtime`. Without it are a module's identity and
  key, the module store's lookup and the expansion record's key and lookup, so a command that
  only asks whether an answer is kept links no Wasmtime. With it are the host, the bridge's
  server and the build scripts' run.

## Environment variables

- `BLAZE_NO_COMPTIME_PATCH`: when set, disables the built-in comptime patch library (`serde_derive`, `bytemuck_derive`, `thiserror-impl`, `derive_more`, `strum_macros`, `paste`) in `crates/blaze-proc-macro/src/hook.rs`.
- `BLAZE_PROC_MACRO_SRV`: explicit path to `rust-analyzer-proc-macro-srv` used by `crates/blaze-proc-macro/src/native_bridge.rs` before falling back to `rustc --print sysroot`.

