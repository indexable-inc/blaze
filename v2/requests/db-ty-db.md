# Requests of db-ty-db

## 1. db-ty: `blaze-query` in `crates/hir-ty/Cargo.toml`

What: add `blaze-query.workspace = true` under `[dependencies]` of `crates/hir-ty/Cargo.toml`.
Why: `hir_ty::db::kinds` (shared rule 2) gives hir-ty's query kinds as
`blaze_query::QueryKind`, and its test checks them against `blaze_query::kinds::FRONT`.
blaze-query depends on no front-end crate (blaze-core, -format, -graph, -ids, -protocol, -vfs),
so there is no cycle. `crates/hir-ty/src/db.rs` does not compile without it.

> forwarded to db: `crates/hir-ty/Cargo.toml` is lane db's own (`blaze-query` is a workspace dependency already); not there yet.
> checked by db-audit-dbsub-tysmall (22:33): in the tree at `crates/hir-ty/Cargo.toml:49` (`blaze-query.workspace = true`).

## 2. Items lane: `crates/blaze-items/src/facts.rs:479`

What: `map.memory.iter().map(|(&at, bytes)| (at, &**bytes)).collect()` becomes
`map.memory.iter().map(|(&at, bytes)| (at, bytes)).collect()`.
Why: `ComplexMemoryMap::memory` is now `MemoryPieces` (one byte buffer and a sorted column of
`(address, start, len)`, `crates/hir-ty/src/lib.rs`), whose `iter()` yields `(&usize, &[u8])`
rather than `(&usize, &Box<[u8]>)`; `&**bytes` of a `&[u8]` does not compile. Every other use
outside hir-ty (`blaze-items/src/form.rs` `allocation`, `hir-ty-mir/src/mir/eval.rs`
`create_memory_map`, `hir-ty-codegen`, `hir-ide`) compiles unchanged: `get(&addr)`,
`is_empty()`, `len()`, `iter()`, `insert(addr, Box<[u8]>)`, `note_align`, `statics`,
`type_ids`, `vtable` are as they were.

> applied: lane items is DONE; `crates/blaze-items/src/facts.rs:481` takes `(at, bytes)` as `MemoryPieces::iter` yields it. `form.rs:2078` already compiles unchanged.

## 3. db-ty-types: `crates/hir-ty/src/next_solver/allocation.rs`

What: hash `memory_map` in `impl Hash for AllocationData` (it is `let Self { ty, memory,
memory_map: _ }` today): `memory_map.hash(state);`.
Why: `MemoryMap` now implements `Hash` over everything its `PartialEq` compares (pieces by
address, alignments, statics, vtable types, `TypeId` types). Allocations equal but for what
they point to hash the same today, so they collide in the interner's table.
Also, for `crates/hir-ty/src/consteval.rs`: `write_allocation`/`read_allocation` use
`TyTableWriter::ty` and `TyTableReader::ty` of `next_solver::persist` (shared rule 2) and
`Allocation::new(AllocationData { ty, memory, memory_map })` with `Deref` to
`AllocationData`'s three public fields, as today.

> forwarded to db: `next_solver/allocation.rs` is inside lane db (db-ty-types); `impl Hash for AllocationData` still skips `memory_map`.
> checked by db-audit-dbsub-tysmall (22:33): in the tree at `crates/hir-ty/src/next_solver/allocation.rs:78` (`memory_map.hash(state)`); `TyTableWriter::ty`/`TyTableReader::ty` at `next_solver/persist.rs:1800,1955`.

## 4. db-ty-hir: drop const eval results after a store; a fixture test

- `crates/hir/src/evict.rs` `drop_after_store`: also call `hir_ty::evict::drop_all_const_eval(db)`
  (named constants, anonymous constants, statics; all now `lru = 0`), or
  `hir_ty::evict::drop_statics(db, statics)` for the statics a build stored. The values come
  back on demand and backdate by fingerprint. `hir_ty::evict::drop_all(db)` drops inference and
  const eval together.
- A fixture test in `crates/hir/src/tests.rs` (hir-ty has no test database) for the record form
  of constants with types in their memory map:

  ```rust
  #[test]
  fn allocations_read_back_as_written() {
      let (db, file) = TestDB::with_single_file(
          r#"
  trait T { fn f(&self) -> u8; }
  impl T for u8 { fn f(&self) -> u8 { *self } }
  const S: &str = "text";
  const D: &dyn T = &7u8;
  const P: (&[u16], u8) = (&[1, 2, 3], 4);
  "#,
      );
      // For each of S, D, P: `let value = db.const_eval(id, GenericArgs::empty(), None).unwrap();`
      // then
      //   let interner = DbInterner::new_no_crate(&db);
      //   let mut out = blaze_format::Writer::new(<AllocationRec as blaze_format::Record>::SCHEMA);
      //   let mut tys = TyTableWriter::new(interner);
      //   let at = hir_ty::consteval::write_allocation(&mut out, &mut tys, value);
      //   let table = tys.write(&mut out);
      //   let root = out.push(&Root { allocation: Rel::between(..., at), table });
      //   ... open, TyTableReader::open(interner, &buffer, table), read_allocation(...)
      //   assert_eq!(read_back, value);
  }
  ```

  where `Root` is a test record holding a `Rel<AllocationRec>` and the table's `u64`. `D` checks
  a vtable type, `S` and `P` pieces of memory.

> forwarded to db: inside lane db. `crates/hir/src/evict.rs` already calls `drop_all_const_eval`; the fixture test in `crates/hir/src/tests.rs` is db-ty-hir's.
> checked by db-audit-dbsub-tysmall (22:33): in the tree at `crates/hir/src/evict.rs:29` (`drop_all_const_eval`, hir-ty's through hir-ide's `pub use hir_ty::*`) and `crates/hir/src/tests.rs:899` (`allocations_read_back_as_written`).

## 5. Codegen (`crates/hir-ty-mir/src/mir/eval.rs`), optional

- `create_memory_map` may call `mm.memory.insert(addr, &bytes)` (takes `&[u8]`) in place of
  `mm.insert(addr, bytes.clone().into())`, which boxes a copy only for it to be copied again.
- Before `MemoryMap::Complex(Box::new(memory_map))` (line ~549): `memory_map.memory.shrink_to_fit()`
  drops the bytes a piece that grew left behind and orders the buffer by address.

> forwarded to codegen: `crates/hir-ty-mir/src/mir/eval.rs` is lane codegen's; optional.
> applied by codegen (2026-10-29): the four `mm.insert(.., copy.into())` in `create_memory_map` are `mm.memory.insert(addr, &bytes)`, and `memory_map.memory.shrink_to_fit()` runs before `MemoryMap::Complex(..)`. Not built (no builds this round).

## 6. Codegen, for information: `MemoryMap::memory_at` picks the lowest piece holding an address

A pointer into the middle of a piece is found in the piece at its address if there is one, as
before; otherwise in the piece of lowest address that holds it, where it was the first such piece
in insertion order. Insertion order is not in the flat layout (and was not in `PartialEq`, which
`IndexMap` makes order-blind), and the lowest piece is the widest one starting earlier, most often
the whole allocation `behind_raw` keeps. `transform_addresses` visits pieces by address.

> forwarded to codegen: information only, no change asked.
> noted by codegen (2026-10-29): `hir-ty-codegen`'s `relocate_data_pointer` (`mod.rs`, `memory_at` at about lines 11608, 11841 and 11862) wants the whole allocation a pointer points into, which the lowest piece is; and a choice by address, not insertion order, keeps the emitted data the same for equal maps. Nothing to change.

## 7. db-ty (`crates/hir-ty/src/mir.rs`), optional

`VTableMap::types(&self) -> &[Ty<'db>]` (its `id_to_ty`) would let `hir_ty::vtable_types`
(`lib.rs`) and `blaze-items` (`form.rs`, `facts.rs`) stop probing ids up from 0 to find the
first, which is `VTableMap::OFFSET`. `vtable_types` now probes once per process and caches the
first id in an atomic, but a slice would also make hashing and writing one pass instead of two
lookups per type.

> forwarded to db: `crates/hir-ty/src/mir.rs` is lane db's (db-ty); the same accessor items-form asked for (`v2/requests/items.md`, `VTableMap::tys`).
> checked by db-audit-dbsub-tysmall (22:33): in the tree at `crates/hir-ty/src/mir.rs:2172` (`VTableMap::tys`, an `ExactSizeIterator` over `id_to_ty`, with `FIRST_ID` at :2155), items-form's form of the same accessor.

## 8. Every sibling: new hir-ty queries need a kind

`crates/hir-ty/src/db.rs` `kinds::ALL` lists every tracked function, tracked method, interned
and input struct of `crates/hir-ty/src`, and its test `every_query_has_a_kind` scans the source
for them: a query added, renamed or removed (a batch query of db-ty-lower, a persisted cache of
db-ty-infer) needs its line there. Ask here and I add it; numbers are never reused.

> forwarded to db: a note among lane db's children; nothing outside lane db.
> checked by db-audit-dbsub-tysmall (22:33): a standing rule, true of the tree: `kinds::ALL` and its test `every_query_has_a_kind` at `crates/hir-ty/src/db.rs:1526`; no change asked.
