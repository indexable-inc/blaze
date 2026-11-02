# Requests from mem-format-fwd

The halves of forwarded sections that are outside hir-def's `expr_store*`/`hir*`.

## F1. deps/signatures/table.rs: relocate a store's macro calls (from codegen.md "codegen-mir-store: the store keeps its macro calls")

Where: `crates/hir-def/src/deps/signatures/table.rs` (frontend-deps-sigs, DONE; to the
coordinator).

`ExpressionStore` now keeps `macro_calls()` (mem-format-fwd applied it in `expr_store.rs`,
`expr_store/lower.rs`, `expr_store/persist.rs`). The store buffer writes them as a new column
`StoreRoot::macro_calls: Slice<u64>`, each call's salsa id bits, like `block_scopes`; decode
refuses a word whose low half is 0. That is a fifth kind of id outside the words, so a
dependency image must move it through the record's table, which already has `Kind::MacroCall`
(written portably by `enc.call`).

What, in `table.rs`:
- module doc: "four kinds of word" becomes five: "... each block of `block_scopes` (its bits),
  each macro call of `macro_calls` (its bits), ...".
- `Sites`: add `/// Each `macro_calls` entry, a `u64`. calls: Vec<usize>,` and include
  `self.calls.is_empty()` in `is_empty`.
- `sites`: `sites.calls.extend(root.macro_calls.follow(buffer)?.iter().map(|it| place(it, buffer)));`
- `relocate_store`: as for `scopes`, with `Kind::MacroCall`:
  ```rust
  for at in sites.calls {
      let index = table.index(Kind::MacroCall, wide(bytes, at));
      set_wide(bytes, at, u64::from(index) + 1);
  }
  ```
- `unrelocate_store`: as for `scopes`, with `Kind::MacroCall` and the message
  `"a macro call not in the table"`.

Why: without it an image's store holds the writing process's salsa ids, which name nothing (or
the wrong call) in the reading process, and MIR lowering's `failed_expansion` reads
`macro_calls()` of an anon const in a dependency's signature. The `+ 1` keeps a relocated word
nonzero, which the store's own check requires. `calls()` then also names the store's calls,
which a record is made from beyond its crate's syntax. Test: `deps/signatures/tests.rs`'s
round trip over a signature with a type macro (`fn f() -> m!() {}`) or an array length with a
macro call; the read store's `macro_calls()` resolve to the same `MacroCallLoc`s.

> forwarded to frontend-deps (frontend-deps-next2, holds `hir-def/src/deps/`; lane frontend-deps is still running; told by tmux; requests, pass 23).
> applied by frontend-deps (2026-10-29), in `crates/hir-def/src/deps/signatures/table.rs`:
> - the module doc now says five kinds of word;
> - `Sites::calls`, counted in `is_empty`, filled from `root.macro_calls` in `sites`;
> - `relocate_store` writes `Kind::MacroCall` index + 1, and `unrelocate_store` reads it back
>   ("a macro call not in the table").
> Test `a_store_s_macro_calls_read_back_as_this_database_s` in `deps/signatures/tests.rs`: a
> fixture with a type macro and macros in array lengths is stored, then served in a fresh
> database. The served `FunctionSignature`'s `macro_calls()` equal the lowered one's, and every
> item passes `assert_served` with values. hot_lint finds nothing. Not built.
