# wasmtime

- Upstream: https://github.com/bytecodealliance/wasmtime (`crates/wasmtime`), crates.io
  `wasmtime` 50.0.1 (sha256 fdfacce893df54d319b29720e79edb001cd1b48faaf78fdb3b9c608eaeeea29f),
  taken from the crate with the change below.
- Licence: Apache-2.0 WITH LLVM-exception (LICENSE).
- Left out: Cargo.lock, Cargo.toml.orig. The dev-dependencies and `[[test]]` targets stay in
  Cargo.toml with the tests/ they name, unused: the root manifest excludes this directory from
  the workspace, so its own tests are not built.

Why: the wasm seed (todo/bootstrap/plan.md) builds blaze for `wasm32-wasip1`, where proc macros
run on Wasmtime's Pulley interpreter inside the seed. That target is neither Unix nor Windows, so
Wasmtime uses its `sys::custom` platform layer with `std` but no virtual memory, and upstream
does not build there: `vm.rs` imports `sys::mmap::open_file_for_mmap` under `std`, and
`MmapVec::from_file` maps the file, while `sys::custom` has `mmap` only with virtual memory
(found by lane wasmhost, 2026-11-02). The embedder supplies `wasmtime_tls_get` and
`wasmtime_tls_set` there (crates/blaze-macro-eval/src/wasm/tls.rs).

Changes, each marked `blaze:`. 1 and 2 are under `not(has_virtual_memory)`, so every target
with virtual memory (Unix and Windows, so every native blaze) compiles upstream's code there; 3
adds API under the `async` feature, which only the seed runner (blaze-wasi's `seed` feature)
enables:

1. `src/runtime/vm/sys/custom/mod.rs`: with `std` and no virtual memory, a `mmap` module whose
   `open_file_for_mmap` opens the file.
2. `src/runtime/vm/mmap_vec.rs`: `MmapVec::from_file` maps the file only with virtual memory;
   without it, the file is read into the allocator (`MmapVec::from_slice`).
3. `src/runtime/fiber.rs` `mod guest` (re-exported in `src/runtime.rs`): guest fibers of a core
   module, `GuestFiber` with `Caller::guest_fiber_{new,resume,suspend,dispose}` and
   `Store::guest_fiber_dispose`. Each runs a guest `(i64) -> ()` on one of Wasmtime's own async
   fibers (`make_fiber_unchecked`, `resume_fiber`, now `pub(crate)`), suspended from a host call
   and resumed from another, nesting. Upstream has this machinery only behind `call_async` and
   component tasks. The fiber calls the function through `TypedFunc::call_raw` (its `ty` and
   `func` now `pub(crate)`), since `TypedFunc::call` refuses a call from an async context, a
   check against a future polled on another thread that a guest fiber never has. This is the
   bootstrap seed's fiber switch (todo/bootstrap/plan.md, "Fibers in the seed").

Report upstream: `std` without virtual memory is a configuration Wasmtime's build script makes
(`has_virtual_memory` is false for any `std` target that is neither Unix nor Windows).
