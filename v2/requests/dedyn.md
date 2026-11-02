# dedyn: requests to the coordinator

## 1. `tools/v2/hot_lint.py`: the `crates` filter crashes, and hir-ty-codegen is never linted

- `Input.crates: list[str]` makes `typed_input` raise ("has a type it does not read") on any
  input that names `crates`. Change it to `crates: tuple[str, ...] = HOT`.
- `hir-ty-codegen` keeps its sources at the crate root (`cached.rs`, `mod.rs`, `v2/`, ...), not
  under `src/`, so `if not src.is_dir(): continue` skips it silently; it has 125 `dyn [A-Z]`
  lines. When `src/` is missing, walk `root / "crates" / crate` instead (still skipping `tests`
  and `*tests.rs`).
- Add `"blaze-diagnostics"` to `HOT`: its late passes run per body.
- The `dyn` pattern `\bdyn\s+[A-Z]` misses path-qualified traits (`&dyn ast::HasAttrs`,
  `&dyn cranelift_codegen::isa::TargetIsa`, `&mut dyn FnMut(&dyn radix::Workers)`): about 25
  such lines in the hot crates. Make it `\bdyn\s+(::)?([a-z_][a-z0-9_]*::)*[A-Z]`.
- A module declared `#[cfg(test)] mod reference;` in its parent is test code, but the lint only
  skips `#[cfg(test)]` blocks inside a file, so it flags every line of
  `hir-expand/src/builtin/derive_macro/reference.rs` (15 `dyn`), `mbe/src/differential.rs`,
  `blaze-core/src/wheel/reference.rs` and the like. Before linting, collect the paths of modules
  declared under `#[cfg(test)]` (the `mod x;` line after the attribute: `x.rs` and `x/` beside
  the declaring file, or `<parent dir>/<file stem>/x.rs` for a non-`mod.rs` parent) and skip them.

> applied by the coordinator (checked: `tools/v2/hot_lint.py` has `crates: tuple[str, ...] = HOT`, walks the crate root when `src/` is missing, and `HOT` names `blaze-diagnostics`) (requests, pass 18).

## 2. No-fallback question (user's call, 2026-10-29): io_uring refused at run time

`blaze_core::Native` is `Uring` on Linux, with no runtime choice (requests in
`v2/requests/mem-core.md` and `cargo.md` under "## dedyn:"). Where io_uring is unavailable (a
container's seccomp policy blocking `io_uring_setup`, `kernel.io_uring_disabled=2`, an old
kernel), `blaze serve` stops with "starting this machine's native driver ..." and the kernel's
error. Nothing picks `Threads` instead. Question for the coordinator and the user: is stopping the
answer there (and should the error say how to allow io_uring), or is a Threads daemon a supported
target that has to be chosen at build time (a cfg/feature), never at run time? Also: the BSDs lose
`blaze serve` (`compile_error!`); their kqueue branch never compiled, since blaze-core's `kqueue`
is macOS-only.

> Answered by the user (2026-10-29 21:04): on Linux blaze stops when io_uring is unavailable,
> with an error naming the `io_uring_setup` errno, the likely cause and the fix (newer kernel,
> `sysctl kernel.io_uring_disabled=0`, or a seccomp profile allowing io_uring_setup,
> io_uring_enter and io_uring_register). Never `Threads` at run time. Written as
> `v2/requests/mem-core.md` "## dedyn: crates/blaze-core/src/uring.rs: io_uring refused is an
> error ..." with its tests. No code today picks `Threads` when `Uring::new` fails. The BSD
> question stays open.
> applied: answered by the user (21:04); the code is mem-core.md's "io_uring refused is an error" section. The BSD question (blaze-core's kqueue driver is macOS-only, so the BSDs get no `blaze serve`) goes to the coordinator for the user, on the requests report's list (requests, pass 21).
