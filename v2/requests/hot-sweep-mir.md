# Requests from hot-sweep-mir

## `tools/v2/hot_lint.py`: skip test modules declared out of line

What: in `in_test_module`/`main`, treat a file as test code when its parent file declares it as
`#[cfg(test)] mod <name>;` (attribute on the line before a `mod <name>;` with no braces), and
when the file is only reached from a `tests/` target through `#[path = ...]`.

Where: `tools/v2/hot_lint.py`.

Why: 118 of the 283 findings over hir-ty-mir, hir-def and hir-expand are test code the lint
reports as hot: `hir-def/src/macro_expansion_tests/**` (`#[cfg(test)] mod macro_expansion_tests;`
in `hir-def/src/lib.rs`), `hir-expand/src/builtin/derive_macro/reference.rs`
(`#[cfg(test)] mod reference;` in `derive_macro.rs`) and
`hir-ty-mir/src/mir/optimize/inline/fixtures.rs` (`#[path]` from `hir-ty-mir/tests/mir/main.rs`).
The brief says test code is not ours to change, so the lint can't reach zero over these crates
without this.

> forwarded to the coordinator: `tools/` is its; on requests' coordinator list (with hot-sweep-cg-v2llvm-fold, -graph, -syntax).
