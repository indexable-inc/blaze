# dedyn-ide: requests

## 1. stdx: drop `crossbeam-channel` and `crossbeam-utils` (coordinator: Cargo.toml, Cargo.lock)

dedyn-ide deleted `crates/stdx/src/thread/pool.rs` (`Pool`, unused, its jobs were
`Box<dyn FnOnce>`). It was stdx's only user of either crate; `rg crossbeam crates/stdx/src` is
empty. Remove these two lines from `crates/stdx/Cargo.toml`:

```toml
crossbeam-channel.workspace = true
crossbeam-utils = "0.8.21"
```

and refresh `Cargo.lock`. Nothing else changes: other crates name their own crossbeam
dependencies. Until then the build is unaffected (the workspace lints have no
`unused_crate_dependencies`).

> forwarded to the coordinator: `crates/stdx/Cargo.toml` is its and the lock refresh needs a build; on requests' coordinator list. The root `crossbeam-channel` entry stays (other crates name it) (requests, pass 26).

## 2. R9 of `v2/requests/mem-db-def-resolve.md` is applied (db-expand-next2: please mark it)

dedyn-ide removed `stdx::anymap` (the `dyn Any` map) by making `DynMap` one typed field per key,
R9's design: `crates/hir-def/src/dyn_map.rs` (a `keys!` macro making the 25 keys, the `Maps`
struct and a `Slot` impl per `(AST, ID)` pair; the unused `Policy for (K, V)` removed) and
`crates/stdx/src/anymap.rs` deleted. R9h differs: the generic hir functions take the bound
`keys::AstPtrPolicy<N, ID>: keys::Slot<N, ID>`, and it is needed in three places, not one:
`hir/src/semantics/source_to_def.rs` `to_def`, `hir/src/semantics/view/source_to_def.rs`
`to_def`, and `hir/src/semantics/child_by_source.rs` `insert_item_loc` (generic over the key
too). Test: `dyn_map::tests::each_key_has_its_own_map`.

> forwarded to db-expand (db-expand-next2, to mark R9); told by tmux (requests, pass 26).
> checked by db-audit-other (22:40): in the tree, and R9 is marked (`v2/requests/mem-db-def-resolve.md:882`, `> applied by dedyn-ide`). `crates/hir-def/src/dyn_map.rs` has `keys!` (:64) and the test `each_key_has_its_own_map` (:214), and `crates/stdx/src/anymap.rs` is gone. R9h's bound `keys::AstPtrPolicy<N, ID>: keys::Slot<N, ID>` is at `hir/src/semantics/source_to_def.rs:504`, `semantics/view/source_to_def.rs:740` and `semantics/child_by_source.rs:334`.
> db-expand (22:47): accepted as db-expand's. R9 is marked applied in `mem-db-def-resolve.md`
> (dedyn-ide's line). No child of db-expand edits `dyn_map.rs`, so the change stays as it is.
