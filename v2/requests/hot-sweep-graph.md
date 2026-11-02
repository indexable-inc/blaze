# Requests from hot-sweep-graph

## `tools/v2/hot_lint.py`: recognize every test `cfg`, and test modules in their own files

What: `in_test_module` matches only the literal `#[cfg(test)]`, and only an item with a body
on the following lines. Make it (a) match any `#[cfg(...)]` whose predicate enables on `test`
(`#[cfg(all(test, not(blaze_check)))]`, `#[cfg(any(test, feature = "fuzz"))]`), and (b) skip a
file that is the body of a `#[cfg(<test predicate>)] mod name;` declaration (and its
submodules), e.g. `crates/blaze-graph/src/plane_tests.rs` and its `plane_tests/*.rs`, or
`crates/blaze-link/src/macho/tbd/reference.rs` (declared `#[cfg(any(test, feature = "fuzz"))]`
in `macho/tbd.rs`, a reference reader for differential tests and fuzzing).

Where: `tools/v2/hot_lint.py`, `in_test_module` and `lint_file`'s file walk.

Also: `in_test_module` counts braces inside char, byte and string literals, so a test module
holding `b'{'` or `"{}"` ends early and its later lines are linted. Seven findings at
`crates/blaze-link/src/macho/tbd.rs:2498..2546` (in its `#[cfg(test)] mod tests`, after
`b"'\"#[]{}\n"`) are that; count braces outside literals and comments only.

Why: 284 of the 446 findings over `blaze-graph`, `blaze-link` and `blaze-codegen` are test
code the lint means to skip: all of `blaze-graph/src/pool.rs`'s and `pump.rs`'s (in
`#[cfg(all(test, not(blaze_check)))] mod tests`), `plane_tests/*` (53) and
`macho/tbd/reference.rs` (77). The brief says test code is not ours to change, so they stay
as findings until the lint skips them.

> forwarded to the coordinator: `tools/` is its; on requests' coordinator list (with hot-sweep-cg-v2llvm-fold, -mir, -syntax).
