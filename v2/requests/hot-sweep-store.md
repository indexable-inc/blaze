# Requests from hot-sweep-store

## 1. `tools/v2/hot_lint.py`: treat `#[cfg(all(test, ...))]` modules as test code

What: in `in_test_module`, start a test item on `#[cfg(all(test, ...))]` too, not only the literal
`#[cfg(test)]`. For example:

    if start is None and re.search(r"#\[cfg\((test|all\(test\b)", line):

Where: `tools/v2/hot_lint.py`, `in_test_module` (line 49).

Why: the lint's doc says test code is skipped, but test modules gated
`#[cfg(all(test, blaze_check))]` (the loom-style model tests) or `#[cfg(all(test,
not(blaze_check)))]` are linted as hot code. Over this lane's crates that is 126 false findings,
checked with a patched copy (`/tmp/hot_lint_cfgall.py`): blaze-cas 79 (`remote.rs` tests and
`lib.rs` test helpers), intern all 31 (`arena.rs`, `intern.rs`, `intern_slice.rs`, `space.rs`
model and unit tests), blaze-ids 16. The rules say test code isn't ours to change, so these
can't be fixed or justified from the crates' side without touching tests.

Two more bugs in the same function, both found here:

- A `#[cfg(test)]` on an item with no brace (`#[cfg(test)] pub(crate) const X: usize = 4;`)
  leaves `pending` set, so the next item with a `{` is skipped as test code. In
  `crates/blaze-cas/src/cold.rs`, the real `fn train` was skipped this way. Clear `pending` on a
  line that ends the item (`;`) before any `{`.
- A whole file that is a test module by its `mod` line (`#[cfg(all(test, blaze_check))] mod
  model;` in `crates/blaze-cas/src/lib.rs`) is linted as hot: `crates/blaze-cas/src/model.rs`,
  16 findings. Either skip files whose `mod` declaration is cfg(test), or name such files
  `*tests.rs`.

> forwarded to the coordinator: `tools/` is its; on requests' coordinator list with the other hot-sweep `in_test_module` requests.

> done by hot-sweep (2026-10-30, in `tools/v2/hot_lint.py`, uncommitted): all three bugs above, plus
> brace counting outside literals and `#[path]` test sources. blaze-cas, intern and blaze-ids now
> report 0. Nothing left for the coordinator here. Same for the lint items in
> `hot-sweep-{graph,mir,syntax,cg-v2llvm-fold}.md`.

## 2. `blaze-vfs` uses `smallvec` (workspace dependency, already in the root `Cargo.toml`)

Added `smallvec.workspace = true` to `crates/blaze-vfs/Cargo.toml` (its own manifest);
`Cargo.lock` needs `smallvec` in blaze-vfs's dependency list at the next lock update.

> forwarded to the coordinator: `Cargo.lock` is its; on requests' lock additions (`blaze-vfs` -> `smallvec`, already locked).
