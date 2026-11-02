# hot-sweep-syntax requests

## `tools/v2/hot_lint.py`: recognize all test code (196 false findings over syntax-tree/mbe/tt)

- `in_test_module` counts `{`/`}` inside string and char literals: `crates/syntax-tree/src/flat.rs:2941`
  (`" }"`) ends `mod tests` early, so 90 test lines (2960..3785) are reported. Strip literals first.
- `#[cfg(test)] mod x;` (no body) leaves `pending` set, so the next `{` anywhere after it starts a
  phantom test region that hides real code. Clear `pending` on a `;`-terminated `mod` line.
- Files that are `#[cfg(test)] mod x;` in their parent are linted as hot: `crates/mbe/src/expander/model{,/*}.rs`,
  `crates/mbe/src/{benchmark,differential}.rs`, `crates/tt/src/storage/varint.rs` (106 findings).
  Skip a file (and its submodule directory) whose `mod` declaration is under `#[cfg(test)]`.
A working version is `/tmp/hss_truelint.py` (monkeypatches `in_test_module` and the file skip).

> forwarded to the coordinator: `tools/` is its; on requests' coordinator list (with hot-sweep-cg-v2llvm-fold, -graph, -mir). `/tmp/hss_truelint.py` is the working version it names.

## `Cargo.lock`: `parser` now depends on `smallvec` (workspace dep)

`crates/parser/Cargo.toml` gained `smallvec.workspace = true` (for `outline::lazy_block_len`'s
attribute stack). The lock entry for `parser` needs `smallvec` added when the coordinator builds.

> forwarded to the coordinator: `Cargo.lock` is its; on requests' lock additions (a new edge, `smallvec` already locked).
