# Requests from mem-format-hir

Child of `mem-format` (lane mem). Paths: hir-def's `expr_store.rs`, `expr_store/`, `hir.rs`,
`hir/`. None is free (speed-hir-store and its children are IN PROGRESS), so every change is
here, exact, for the holder to apply. Report: `/tmp/blaze-lanes/v2-mem-format-hir.md`.

Lane mem's ruling (`v2/requests/mem.md`, answer to frontend-tokens and frontend-deps-reparse):
a memo value (a store, a body, a source map) has no revision-arena lifetime. It is owned, made
once at its exact size from the general allocator, and its `Copy` columns are packed into one
block. The temporaries of one lowering are per-thread scratch, kept warm. The budget bar is 0
for a warm lowering's scratch and for every read. A finished store is held to its own blocks,
counted exactly.

What speed-hir-store already did, and these sections build on:
- A per-thread pool of builder scratch (`expr_store.rs` `Scratch`, `POOL`).
- One slab (`hir/col.rs` `Col`, `SlabWriter`) for the `Copy` columns of `HirColumns` and
  `TypeColumns`.

What still allocates on a warm lowering:
1. Twelve builder tables start empty for every store and grow by doubling: the three arenas,
   four hash maps and five vectors. That is several allocations each per body.
2. `finish`'s stable sorts (`SourceTable::from_pushed`, `IdTable::from_pushed`) take a heap
   buffer once a table outgrows std's 4 KB stack buffer, about 150 entries.
3. The store's five sorted side columns and three of the record columns are one box each.

Sections 1-4 fix these. Sections 5 and 6 move encode (`Words`, the store's and body's column
copies) onto per-thread scratch.

None of this needs hir-def to depend on `blaze-mem`. Every buffer here is a type of hir-def's
own, kept in its own thread-local, which is lane mem's ruling (`v2/requests/mem.md`, the answer
to frontend-deps-reparse). A `Vec<T, &Frame>` gains nothing over the pooled scratch: with about
30 interleaved growing columns, a bump frame wastes each column's old space when it grows,
where the pool keeps every column at its high-water mark. So mem-db is not asked for the
dependency or for `#![feature(allocator_api)]`. Lane db applying a frame per query run
(`docs/v2/plan/lanes/mem/db.md` items 1-2) changes nothing here.

## mem-format-hir: the builder's per-store tables on the pooled scratch (`expr_store.rs`)

Holder: speed-hir-store (window `speed-hir-store-next1`).

Why: `ExpressionStoreBuilder::new` makes these fresh for every store, so a warm lowering grows
each by doubling, which is about log2(n) allocations each:
- `bindings`, `labels`, `lifetimes` (`Arena::new()`);
- `binding_owners`, `ident_hygiene`, `block_editions` (`FxHashMap`), `overflow_inheriting`
  (`FxHashSet`);
- `block_scopes`, `type_hygiene`, `field_map_back`, `pat_field_map_back` (`Vec`).

`finish` then reallocates them again: `shrink_to_fit`, `into_boxed_slice`, and a `collect` per
side column. After this change they are part of `Scratch`: taken warm in `new` and given back
emptied in `finish`. `finish` copies each into the finished store exactly once:
- each arena is one exact allocation (`exact_arena`);
- the five `Copy` side columns (`block_scopes`, `ident_hygiene`, `block_editions`,
  `binding_owners`, `overflow_inheriting`) share one slab (`SlabWriter`), so there is one
  allocation for all five and none when all are empty;
- `type_hygiene` is one exact box.

Lowering code is unchanged: the builder's fields keep their names and types (`lower.rs` only
calls `alloc`, `insert`, `remove`, `push` and indexing on them).

Semantics and determinism are the same:
- The arenas are the same elements in the same order.
- `ident_hygiene` is still sorted by its unique id. `sort_unstable_by_key` on unique keys
  gives one order whatever the hash map's iteration order.
- `block_editions` and `overflow_inheriting` get the same stable radix sort as before, now
  with a pooled buffer (`sort_copy_by_key_in`).
- `binding_owners` fills the same indexed column.

Edits, in order:

1. Imports. Before:
   ```rust
           columns::{self, Frozen, Growing, HirColumns},
       },
   ```
   After:
   ```rust
           col::{Col, SlabLayout, SlabWriter},
           columns::{self, Frozen, Growing, HirColumns},
       },
   ```

2. `ExpressionOnlyStore`'s five side columns. `Col` derefs to the slice as `Box<[T]>` did, so
   readers are unchanged, and decode (`persist.rs` `binding_owners: self.binding_owners.into()`
   and the `.collect()`s) compiles unchanged through `Col`'s `From<&[T]>` and `FromIterator`.
   - `binding_owners: Box<[u32]>,` becomes `binding_owners: Col<u32>,`
   - `block_scopes: Box<[BlockId]>,` becomes `block_scopes: Col<BlockId>,`
   - `ident_hygiene: Box<[(ExprOrPatIdPacked, HygieneId)]>,` becomes
     `ident_hygiene: Col<(ExprOrPatIdPacked, HygieneId)>,`
   - `block_editions: Box<[(ExprId, Edition)]>,` becomes `block_editions: Col<(ExprId, Edition)>,`
   - `overflow_inheriting: Box<[ExprId]>,` becomes `overflow_inheriting: Col<ExprId>,`

3. `ExpressionStoreBuilder`: add a field after `freeze: FreezeScratch,`:
   ```rust
       /// What `finish` gathers and sorts the store's side columns in, empty until then.
       packing: Packing,
   ```

4. `Scratch`: replace the struct, its doc unchanged, with:
   ```rust
   #[derive(Default)]
   struct Scratch {
       columns: HirColumns<Growing>,
       types: TypeColumns<Growing>,
       lists: Lists,
       bindings: Arena<Binding>,
       labels: Arena<Label>,
       lifetimes: Arena<LifetimeRef>,
       binding_owners: FxHashMap<BindingId, ExprId>,
       block_scopes: Vec<BlockId>,
       ident_hygiene: FxHashMap<ExprOrPatIdPacked, HygieneId>,
       type_hygiene: Vec<(TypeRefId, HygieneId)>,
       block_editions: FxHashMap<ExprId, Edition>,
       overflow_inheriting: FxHashSet<ExprId>,
       expr_map: Vec<(ExprSource, ExprOrPatIdPacked)>,
       pat_map: Vec<(PatSource, ExprOrPatIdPacked)>,
       label_map: Vec<(LabelSource, LabelId)>,
       types_map: Vec<(TypeSource, TypeRefId)>,
       lifetime_map: Vec<(LifetimeSource, LifetimeRefId)>,
       expansions: Vec<(InFile<MacroCallPtr>, MacroCallId)>,
       expr_map_back: BackPushes<Expr<'static>, Either<ast::Expr, ast::Pat>>,
       pat_map_back: BackPushes<Pat<'static>, Either<ast::Expr, ast::Pat>>,
       label_map_back: BackPushes<Label, Either<ast::Label, ast::BlockExpr>>,
       types_map_back: BackPushes<TypeRef<'static>, ast::Type>,
       lifetime_map_back: BackPushes<LifetimeRef, ast::Lifetime>,
       binding_definitions: Vec<(BindingId, PatId)>,
       field_map_back: Vec<(ExprId, FieldSource)>,
       pat_field_map_back: Vec<(PatId, PatFieldSource)>,
       /// What [`ExpressionStoreBuilder::finish`] builds the back maps and sorts the forward
       /// maps in.
       freeze: FreezeScratch,
       packing: Packing,
   }

   /// What [`ExpressionStoreBuilder::finish`] gathers the store's side columns in, and sorts
   /// them in, before it copies them into their slab.
   #[derive(Debug, Default, PartialEq, Eq)]
   struct Packing {
       owners: Vec<u32>,
       hygiene: Vec<(ExprOrPatIdPacked, HygieneId)>,
       editions: Vec<(ExprId, Edition)>,
       inheriting: Vec<ExprId>,
       /// The radix sorts' buffers.
       editions_sort: Vec<(ExprId, Edition)>,
       inheriting_sort: Vec<ExprId>,
   }

   impl Packing {
       fn clear(&mut self) {
           let Packing { owners, hygiene, editions, inheriting, editions_sort, inheriting_sort } =
               self;
           owners.clear();
           hygiene.clear();
           editions.clear();
           inheriting.clear();
           editions_sort.clear();
           inheriting_sort.clear();
       }

       fn capacity(&self) -> usize {
           self.owners.capacity()
               + self.hygiene.capacity()
               + self.editions.capacity()
               + self.inheriting.capacity()
               + self.editions_sort.capacity()
               + self.inheriting_sort.capacity()
       }
   }

   /// An exact copy of a pooled arena: one allocation of its length, none when it is empty.
   fn exact_arena<T: Clone>(pooled: &Arena<T>) -> Arena<T> {
       let mut arena = Arena::with_capacity(pooled.len());
       arena.alloc_many(pooled.values().cloned());
       arena
   }
   ```

5. `Scratch::give`. Before:
   ```rust
       fn give(mut self) {
           self.clear();
           if self.capacity() > POOL_ELEMENTS {
               return;
           }
   ```
   After (an `Arena` reports only its length, so the size is measured before clearing):
   ```rust
       fn give(mut self) {
           let held = self.capacity();
           self.clear();
           if held > POOL_ELEMENTS {
               return;
           }
   ```

6. `Scratch::clear`. Replace the whole body with:
   ```rust
       fn clear(&mut self) {
           let Scratch {
               columns,
               types,
               lists,
               bindings,
               labels,
               lifetimes,
               binding_owners,
               block_scopes,
               ident_hygiene,
               type_hygiene,
               block_editions,
               overflow_inheriting,
               expr_map,
               pat_map,
               label_map,
               types_map,
               lifetime_map,
               expansions,
               expr_map_back,
               pat_map_back,
               label_map_back,
               types_map_back,
               lifetime_map_back,
               binding_definitions,
               field_map_back,
               pat_field_map_back,
               freeze,
               packing,
           } = self;
           columns.clear();
           types.clear();
           lists.clear();
           bindings.clear();
           labels.clear();
           lifetimes.clear();
           binding_owners.clear();
           block_scopes.clear();
           ident_hygiene.clear();
           type_hygiene.clear();
           block_editions.clear();
           overflow_inheriting.clear();
           expr_map.clear();
           pat_map.clear();
           label_map.clear();
           types_map.clear();
           lifetime_map.clear();
           expansions.clear();
           expr_map_back.clear();
           pat_map_back.clear();
           label_map_back.clear();
           types_map_back.clear();
           lifetime_map_back.clear();
           binding_definitions.clear();
           field_map_back.clear();
           pat_field_map_back.clear();
           freeze.clear();
           packing.clear();
       }
   ```

7. `Scratch::capacity`. After `+ self.lists.capacity()` add:
   ```rust
               + self.bindings.len()
               + self.labels.len()
               + self.lifetimes.len()
               + self.binding_owners.capacity()
               + self.block_scopes.capacity()
               + self.ident_hygiene.capacity()
               + self.type_hygiene.capacity()
               + self.block_editions.capacity()
               + self.overflow_inheriting.capacity()
   ```
   and after `+ self.binding_definitions.capacity()` add:
   ```rust
               + self.field_map_back.capacity()
               + self.pat_field_map_back.capacity()
               + self.packing.capacity()
   ```

8. `ExpressionStoreBuilder::new`. Replace the body with:
   ```rust
       pub(crate) fn new() -> Self {
           let Scratch {
               columns,
               types,
               lists,
               bindings,
               labels,
               lifetimes,
               binding_owners,
               block_scopes,
               ident_hygiene,
               type_hygiene,
               block_editions,
               overflow_inheriting,
               expr_map,
               pat_map,
               label_map,
               types_map,
               lifetime_map,
               expansions,
               expr_map_back,
               pat_map_back,
               label_map_back,
               types_map_back,
               lifetime_map_back,
               binding_definitions,
               field_map_back,
               pat_field_map_back,
               freeze,
               packing,
           } = Scratch::take();
           ExpressionStoreBuilder {
               columns,
               types,
               lists,
               bindings,
               labels,
               lifetimes,
               binding_owners,
               block_scopes,
               ident_hygiene,
               type_hygiene,
               block_editions,
               overflow_inheriting,
               inference_roots: None,
               expr_map,
               expr_map_back,
               pat_map,
               pat_map_back,
               label_map,
               label_map_back,
               types_map_back,
               types_map,
               lifetime_map_back,
               lifetime_map,
               binding_definitions,
               field_map_back,
               pat_field_map_back,
               template_map: None,
               expansions,
               // alloc: empty unless lowering reports one, the error path.
               diagnostics: Vec::new(),
               freeze,
               packing,
               record: true,
           }
       }
   ```

9. `ExpressionStoreBuilder::finish`. Replace the doc and the whole function with:
   ```rust
       /// The finished store and source map: each column, table and map copied out of the pooled
       /// scratch at its exact length, the store's `Copy` columns into slabs, the scratch given
       /// back to this thread's pool. The gathering and sorting run in the pooled scratch too, so
       /// the copies are the only allocations.
       pub fn finish(self) -> (ExpressionStore, ExpressionStoreSourceMap) {
           let Self {
               block_scopes,
               columns,
               types,
               lists,
               labels,
               bindings,
               binding_owners,
               ident_hygiene,
               type_hygiene,
               block_editions,
               overflow_inheriting,
               inference_roots: mut expr_roots,
               lifetimes,

               expr_map,
               expr_map_back,
               pat_map,
               pat_map_back,
               label_map,
               label_map_back,
               types_map_back,
               types_map,
               lifetime_map_back,
               lifetime_map,
               binding_definitions,
               field_map_back,
               pat_field_map_back,
               template_map,
               expansions,
               diagnostics,
               freeze,
               packing,
               record,
           } = self;
           debug_assert_eq!(lists.stacked(), None, "lowering left a list on this stack");
           let mut scratch = Scratch {
               columns,
               types,
               lists,
               bindings,
               labels,
               lifetimes,
               binding_owners,
               block_scopes,
               ident_hygiene,
               type_hygiene,
               block_editions,
               overflow_inheriting,
               expr_map,
               pat_map,
               label_map,
               types_map,
               lifetime_map,
               expansions,
               expr_map_back,
               pat_map_back,
               label_map_back,
               types_map_back,
               lifetime_map_back,
               binding_definitions,
               field_map_back,
               pat_field_map_back,
               freeze,
               packing,
           };
           let columns = scratch.columns.freeze_into();
           let types = scratch.types.freeze_into();
           let has_exprs =
               !columns.is_empty() || !scratch.labels.is_empty() || !scratch.bindings.is_empty();
           let source_map = match record {
               false => ExpressionStoreSourceMap::default(),
               true => {
                   // Each back map is dense over its arena, so a source map can be checked on its
                   // own.
                   let freeze = &mut scratch.freeze;
                   let expr_map_back = scratch.expr_map_back.freeze(columns.expr_count(), freeze);
                   let pat_map_back = scratch.pat_map_back.freeze(columns.pat_count(), freeze);
                   let label_map_back = scratch.label_map_back.freeze(scratch.labels.len(), freeze);
                   let types_map_back = scratch.types_map_back.freeze(types.len(), freeze);
                   let lifetime_map_back =
                       scratch.lifetime_map_back.freeze(scratch.lifetimes.len(), freeze);
                   let binding_definitions = BindingPats::from_pushed(
                       &mut scratch.binding_definitions,
                       scratch.bindings.len(),
                   );
                   let sort = &mut freeze.sort;
                   let expr_map = SourceTable::from_pushed_in(&mut scratch.expr_map, &mut sort.exprs);
                   let pat_map = SourceTable::from_pushed_in(&mut scratch.pat_map, &mut sort.pats);
                   let label_map =
                       SourceTable::from_pushed_in(&mut scratch.label_map, &mut sort.labels);
                   let types_map =
                       SourceTable::from_pushed_in(&mut scratch.types_map, &mut sort.types);
                   let lifetime_map =
                       SourceTable::from_pushed_in(&mut scratch.lifetime_map, &mut sort.lifetimes);
                   let expansions =
                       SourceTable::from_pushed_in(&mut scratch.expansions, &mut sort.expansions);
                   let field_map_back =
                       IdTable::from_pushed_in(&mut scratch.field_map_back, &mut sort.fields);
                   let pat_field_map_back =
                       IdTable::from_pushed_in(&mut scratch.pat_field_map_back, &mut sort.field_pats);
                   let template_map = template_map.map(|template| {
                       let FormatTemplate {
                           format_args_to_captures,
                           asm_to_captures,
                           implicit_capture_to_source,
                       } = *template;
                       // alloc: once per store with format templates, the cold side of the map.
                       Box::new(FormatTemplate {
                           format_args_to_captures: IdTable::from_pushed(format_args_to_captures),
                           asm_to_captures: IdTable::from_pushed(asm_to_captures),
                           implicit_capture_to_source: IdTable::from_pushed(
                               implicit_capture_to_source,
                           ),
                       })
                   });
                   let expr_only = (has_exprs || !expansions.is_empty() || !diagnostics.is_empty())
                       .then(|| {
                           // alloc: once per store, keeps the map small for signatures.
                           Box::new(ExpressionOnlySourceMap {
                               expr_map,
                               expr_map_back,
                               pat_map,
                               pat_map_back,
                               label_map,
                               label_map_back,
                               binding_definitions,
                               field_map_back,
                               pat_field_map_back,
                               template_map,
                               expansions,
                               diagnostics: ThinVec::from_iter(diagnostics),
                           })
                       });
                   ExpressionStoreSourceMap {
                       expr_only,
                       types_map_back,
                       types_map,
                       lifetime_map_back,
                       lifetime_map,
                   }
               }
           };

           let expr_only = match has_exprs {
               false => None,
               true => {
                   if let Some(expr_roots) = &mut expr_roots {
                       expr_roots.shrink_to_fit();
                   }
                   let Scratch {
                       bindings,
                       labels,
                       binding_owners,
                       block_scopes,
                       ident_hygiene,
                       block_editions,
                       overflow_inheriting,
                       packing,
                       ..
                   } = &mut scratch;
                   if !binding_owners.is_empty() {
                       packing.owners.resize(bindings.len(), NO_OWNER);
                       for (binding, owner) in binding_owners.iter() {
                           packing.owners[binding.into_raw().into_u32() as usize] =
                               owner.into_raw().into_u32();
                       }
                   }
                   packing.hygiene.extend(ident_hygiene.iter().map(|(&id, &hygiene)| (id, hygiene)));
                   packing.hygiene.sort_unstable_by_key(|&(id, _)| id);
                   packing.editions.extend(block_editions.iter().map(|(&id, &edition)| (id, edition)));
                   radix::sort_copy_by_key_in(
                       &mut packing.editions,
                       |&(id, _)| id.into_raw().into_u32(),
                       &mut packing.editions_sort,
                   );
                   packing.inheriting.extend(overflow_inheriting.iter().copied());
                   radix::sort_copy_by_key_in(
                       &mut packing.inheriting,
                       |it| it.into_raw().into_u32(),
                       &mut packing.inheriting_sort,
                   );
                   // Copied in the order they are added: widest first, so no padding between.
                   let mut layout = SlabLayout::default();
                   layout.add::<BlockId>(block_scopes.len());
                   layout.add::<(ExprOrPatIdPacked, HygieneId)>(packing.hygiene.len());
                   layout.add::<(ExprId, Edition)>(packing.editions.len());
                   layout.add::<u32>(packing.owners.len());
                   layout.add::<ExprId>(packing.inheriting.len());
                   let mut slab = SlabWriter::new(layout);
                   let block_scopes = slab.copy(&block_scopes[..]);
                   let ident_hygiene = slab.copy(&packing.hygiene[..]);
                   let block_editions = slab.copy(&packing.editions[..]);
                   let binding_owners = slab.copy(&packing.owners[..]);
                   let overflow_inheriting = slab.copy(&packing.inheriting[..]);
                   // alloc: once per store, keeps signature-only stores small.
                   Some(Box::new(ExpressionOnlyStore {
                       columns,
                       bindings: exact_arena(bindings),
                       labels: exact_arena(labels),
                       binding_owners,
                       block_scopes,
                       ident_hygiene,
                       block_editions,
                       overflow_inheriting,
                       expr_roots: expr_roots
                           .expect("should always finish with a `Some(_)` expr_roots"),
                   }))
               }
           };
           let lifetimes = exact_arena(&scratch.lifetimes);
           // alloc: the column's one exact copy, only for a store with macro-expanded types.
           let type_hygiene = (!scratch.type_hygiene.is_empty())
               .then(|| Box::<[_]>::from(&scratch.type_hygiene[..]));
           scratch.give();
           (ExpressionStore { expr_only, types, lifetimes, type_hygiene }, source_map)
       }
   ```

10. `expr_store/body.rs` ~L206 and `expr_store/persist.rs` (test) ~L2399. A closure that
    returns `&Col<BlockId>` where `&[BlockId]` is expected needs the explicit reborrow
    (`expr_store.rs` ~L1255 already writes it this way):
    `map_or(&[][..], |it| &it.block_scopes)` becomes `map_or(&[][..], |it| &*it.block_scopes)`.

The tests are the last section.

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).
> applied by mem-format-hir-apply (2026-10-29): as written, edits 1-10. The tree had not moved
> under them beyond line numbers: `finish` still copied `field_map_back`/`pat_field_map_back` out
> of the builder, which are now pooled as edit 4 says. Adapted: the slab comment says why there
> is no padding (every element type is 4-byte aligned; `BlockId` is a salsa `Id`, two `u32`s)
> where the request said "widest first". `give` measures before clearing, with that reason on
> its line. Status: applied.

## mem-format-hir: `finish`'s stable sorts without a heap buffer (`expr_store/source_map.rs`)

Holder: speed-hir-store (window `speed-hir-store-next1`; `source_map.rs` is also in
speed-hir-store-lint's brief).

Why: `SourceTable::from_pushed` and `IdTable::from_pushed` use std's stable sort
(`sort_by_key`). Driftsort takes a heap buffer of about `len` elements once that outgrows its
4 KB stack buffer. For `(ExprSource, ExprOrPatIdPacked)` (about 28 bytes) that is past about
150 entries, so a medium body's `finish` allocates a buffer per forward map.

`blaze_radix::sort_copy_by_key_in` is the same stable sort into a buffer the caller keeps. The
source key `(u8, u64, u32, u32, u16)` is 152 bits, so it takes two passes:
- the low key `(start, end, kind)` as a `u128`;
- then the high key `(file kind, file)` as a `u128`.

A stable sort by the low key followed by a stable sort by the high key is a stable sort by the
whole tuple: the same permutation as now, so the same table. The radix sort's survey skips the
digits on which all keys agree; within one body that is the whole high key. Its counters are
on the stack, so with a warm buffer it allocates nothing. The existing `from_pushed`s stay for
their other callers (`moved`, decode, the format templates and tests).

Edits:

1. Imports. After `use std::{fmt, marker::PhantomData};` add `use blaze_ids::radix;`.

2. `FreezeScratch`: add a field after `run_files: Vec<HirFileId>,`:
   ```rust
       /// The buffers `finish` sorts the forward maps in.
       pub(in crate::expr_store) sort: SortScratch,
   ```
   In `FreezeScratch::clear` add `self.sort.clear();`, and in `FreezeScratch::capacity` add
   `+ self.sort.capacity()` after `self.run_files.capacity()`.

3. New, after `FreezeScratch`'s impl:
   ```rust
   /// The buffers of `finish`'s stable sorts, one per forward map ([`SourceTable::from_pushed_in`],
   /// [`IdTable::from_pushed_in`]): their contents are unspecified after a sort.
   #[derive(Default, Debug, PartialEq, Eq)]
   pub(in crate::expr_store) struct SortScratch {
       pub(in crate::expr_store) exprs: Vec<(ExprSource, ExprOrPatIdPacked)>,
       pub(in crate::expr_store) pats: Vec<(PatSource, ExprOrPatIdPacked)>,
       pub(in crate::expr_store) labels: Vec<(LabelSource, LabelId)>,
       pub(in crate::expr_store) types: Vec<(TypeSource, TypeRefId)>,
       pub(in crate::expr_store) lifetimes: Vec<(LifetimeSource, LifetimeRefId)>,
       pub(in crate::expr_store) expansions: Vec<(InFile<MacroCallPtr>, MacroCallId)>,
       pub(in crate::expr_store) fields: Vec<(ExprId, FieldSource)>,
       pub(in crate::expr_store) field_pats: Vec<(PatId, PatFieldSource)>,
   }

   impl SortScratch {
       fn clear(&mut self) {
           let SortScratch { exprs, pats, labels, types, lifetimes, expansions, fields, field_pats } =
               self;
           exprs.clear();
           pats.clear();
           labels.clear();
           types.clear();
           lifetimes.clear();
           expansions.clear();
           fields.clear();
           field_pats.clear();
       }

       fn capacity(&self) -> usize {
           self.exprs.capacity()
               + self.pats.capacity()
               + self.labels.capacity()
               + self.types.capacity()
               + self.lifetimes.capacity()
               + self.expansions.capacity()
               + self.fields.capacity()
               + self.field_pats.capacity()
       }
   }
   ```

4. `IdTable`: a new impl after `impl<K: Ord + Copy, V> IdTable<K, V> { .. }`:
   ```rust
   impl<T, V: Copy + Send + Sync> IdTable<Idx<T>, V> {
       /// [`IdTable::from_pushed`] of a pooled column, sorted in `scratch` (a stable radix sort
       /// by id, allocation-free once `scratch` has held as many): the same table. `pushed` is
       /// left empty with its capacity; the table is one allocation of its length.
       pub(crate) fn from_pushed_in(
           pushed: &mut Vec<(Idx<T>, V)>,
           scratch: &mut Vec<(Idx<T>, V)>,
       ) -> Self {
           radix::sort_copy_by_key_in(pushed, |&(id, _)| id.into_raw().into_u32(), scratch);
           // Stable, so equal ids stay in push order; keep the last of each run.
           pushed.dedup_by(|later, kept| {
               let same = later.0 == kept.0;
               if same {
                   std::mem::swap(later, kept);
               }
               same
           });
           IdTable(take_exact(pushed))
       }
   }
   ```
   The table is the same as `from_pushed`'s: that one reverses, stable-sorts and keeps the
   first of each run, which is the last pushed; this one stable-sorts and keeps the last.

5. `SourceTable`: in `impl<K: SourceKey, V> SourceTable<K, V>`, after `from_pushed`:
   ```rust
       /// [`Self::from_pushed`] sorting in `scratch`, a buffer of the caller's that keeps its
       /// capacity: allocation-free but for the table once `scratch` has held as many. Two
       /// stable radix sorts, by the key's last three fields and then by its first two, are one
       /// stable sort by the whole key, so the table is the same.
       pub(crate) fn from_pushed_in(pushed: &mut Vec<(K, V)>, scratch: &mut Vec<(K, V)>) -> Self
       where
           K: Copy + Send + Sync,
           V: Copy + Send + Sync,
       {
           radix::sort_copy_by_key_in(
               pushed,
               |(key, _)| {
                   let (_, _, start, end, kind) = key.source_key();
                   (u128::from(start) << 48) | (u128::from(end) << 16) | u128::from(kind)
               },
               scratch,
           );
           radix::sort_copy_by_key_in(
               pushed,
               |(key, _)| {
                   let (file_kind, file, ..) = key.source_key();
                   (u128::from(file_kind) << 64) | u128::from(file)
               },
               scratch,
           );
           pushed.dedup_by(|later, kept| {
               let same = later.0.source_key() == kept.0.source_key();
               if same {
                   std::mem::swap(later, kept);
               }
               same
           });
           SourceTable(take_exact(pushed))
       }
   ```
   Every key type here is `InFile<AstPtr<N>>`, which is `Copy`, and `AstPtr`'s
   `PhantomData<fn() -> N>` is `Send + Sync`. Every value type is a `Copy` id.

Model test, in `source_map.rs` `mod tests` after `a_source_table_answers_as_a_hash_map_does`.
It uses the same seeded keys (`seeded_src`: repeated keys, three files, files and macro files),
with up to 600 pushes, past std's stack buffer. `from_pushed_in` must give the table
`from_pushed` gives, and keep both vectors' room. An `IdTable` must be the same from both. A
warm call must allocate only the table:
```rust
    #[test]
    fn sorting_in_scratch_makes_the_same_table() {
        let mut scratch = Vec::new();
        for seed in 0..64 {
            let mut rng = SplitMix64::new(seed);
            let mut pushed: Vec<(ExprSource, u32)> =
                (0..rng.below(600) as u32).map(|value| (seeded_src(&mut rng), value)).collect();
            let mut model = pushed.clone();
            let room = pushed.capacity();
            let table = SourceTable::from_pushed_in(&mut pushed, &mut scratch);
            assert!(pushed.is_empty() && pushed.capacity() == room, "the column keeps its room");
            assert_eq!(table, SourceTable::from_pushed(&mut model));
        }

        let mut ids = Vec::new();
        for seed in 0..64 {
            let mut rng = SplitMix64::new(seed);
            let mut pushed: Vec<(ExprId, u32)> = (0..rng.below(600) as u32)
                .map(|value| (ExprId::from_raw(RawIdx::from_u32(rng.below(40) as u32)), value))
                .collect();
            let model = pushed.clone();
            let table = IdTable::from_pushed_in(&mut pushed, &mut ids);
            assert!(pushed.is_empty(), "the column is left empty");
            assert_eq!(table, IdTable::from_pushed(model));
        }

        let mut pushed: Vec<(ExprSource, u32)> =
            (0..600).map(|at| (src(0, false, SyntaxKind::CALL_EXPR, at, at + 1), at)).collect();
        let mut again = pushed.clone();
        drop(SourceTable::from_pushed_in(&mut again, &mut scratch));
        blaze_alloc_count::assert_at_most(1, "a warm sort in scratch: the table alone", || {
            SourceTable::from_pushed_in(&mut pushed, &mut scratch)
        });
    }
```
`SplitMix64`, `seeded_src`, `src`, `RawIdx`, `ExprId` and `SyntaxKind` are already in scope in
that module. `SourceTable` and `IdTable` derive `PartialEq` and `Debug`.

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).
> applied by mem-format-hir-apply (2026-10-29): edits 1-5 and the model test. Adapted:
> `BackPushes::freeze` destructures `FreezeScratch` field by field, so it now names `sort: _`.
> The test's last check pushes its 600 keys in reverse order: in order, the radix sort's survey
> finds them sorted and returns before using `scratch`, which would make the warm budget
> trivial. Status: applied.

## mem-format-hir: the record columns and names in the store's slab (`hir.rs`, `hir/columns.rs`)

Holder: `hir.rs` and `hir/columns.rs` are speed-hir-store's and its paths children's (windows
`speed-hir-store-next1`, `speed-hir-store-paths-next2`).

Why: `HirColumns::freeze_into` boxes `arms`, `record_fields`, `field_pats` and `names` one
each, though all four are plain records of `Copy` fields: ids, and `Name`, which is a `u32`
symbol. A body with a `match`, a struct literal, a struct pattern and a method call makes four
allocations more than it needs. In the slab they cost none. All four are 4-byte aligned, as
`Option<TypeRefId>` is, so adding them after it puts no padding between columns.

Edits:

1. `hir.rs`: `MatchArm`, `RecordLitField` and `RecordFieldPat` derive `Copy`:
   `#[derive(Debug, Clone, Eq, PartialEq)]` becomes `#[derive(Debug, Clone, Copy, Eq, PartialEq)]`
   above each of `pub struct MatchArm`, `pub struct RecordLitField` and
   `pub struct RecordFieldPat`. Their sizes are unchanged, so the `const` asserts stand.
   Clippy's `clone_on_copy` (style, warn) will flag any consumer's `.clone()` of one; there is
   none in hir-def (`rg '\.clone\(\)'` on their uses).

2. `hir/columns.rs` `HirColumns::freeze_into`. After
   `layout.add::<Option<TypeRefId>>(self.type_lists.len());` add:
   ```rust
           layout.add::<MatchArm>(self.arms.len());
           layout.add::<RecordLitField>(self.record_fields.len());
           layout.add::<RecordFieldPat>(self.field_pats.len());
           layout.add::<Name>(self.names.len());
   ```
   After `let type_lists = slab.take(&mut self.type_lists);` add:
   ```rust
           let arms = slab.take(&mut self.arms);
           let record_fields = slab.take(&mut self.record_fields);
           let field_pats = slab.take(&mut self.field_pats);
           let names = slab.take(&mut self.names);
   ```
   In the `HirColumns { .. }` literal, these four lines:
   ```rust
               arms: take_exact(&mut self.arms).into(),
               record_fields: take_exact(&mut self.record_fields).into(),
               field_pats: take_exact(&mut self.field_pats).into(),
               names: take_exact(&mut self.names).into(),
   ```
   become `arms,`, `record_fields,`, `field_pats,` and `names,`. In the doc, "the `Copy` ones"
   stays true.

Decode (`persist.rs` ~L1299) still boxes them (`arms.into()`), which `Col` accepts. Decode
builds them as temporaries first, so slabbing there saves nothing until decode borrows its
buffer, which is speed-hir-store's queued "borrowed decode columns".

Test: `a_warm_store_is_its_blocks_and_nothing_else` (last section) checks
`columns.arms.in_slab()`.

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).
> applied by mem-format-hir-apply (2026-10-29): edits 1-2. The slab layout now starts with
> `macro_defs`, so the four columns are added and taken right after `type_lists` in both lists,
> which are still in the same order. `Name` is 4 bytes now (a `NonZeroU32` symbol id), so all
> four are 4-byte aligned and add no padding. Not fixed here, and named in the report: that
> change left three `const` size asserts stale, `hir.rs` (`Label == 8`,
> `RecordLitField == 16`, `RecordFieldPat == 16`, now 4, 8 and 8) and `expr_store/path.rs`
> (`Name == 8`, now 4). Status: applied.

## mem-format-hir: allocation-budget tests for lowering (`expr_store.rs` `scratch_tests`)

Holder: speed-hir-store. These need hir-def's counting allocator, which speed-hir-store already
filed (`v2/requests/db.md` "## speed-hir-store: one counting allocator for hir-def's tests, from
`blaze-alloc-count`", forwarded to db).

Add to `mod scratch_tests`, after `fn finished`:
```rust
    /// Pushes to every table lowering fills beside the columns: the arenas, the hash maps, the
    /// side vectors, and `n` entries of a forward map and of the field map.
    fn lower_every_table(builder: &mut ExpressionStoreBuilder, n: u32) {
        lower_one(builder, 0);
        let missing = builder.alloc_expr(Expr::Missing);
        let arm = MatchArm { pat: builder.alloc_pat(Pat::Wild), guard: None, expr: missing };
        let matched = builder.alloc_expr(Expr::Match { expr: missing, arms: &[arm] });
        let binding = builder.bindings.alloc(Binding {
            name: Name::missing(),
            mode: BindingAnnotation::Unannotated,
            problems: None,
            hygiene: HygieneId::ROOT,
        });
        builder.labels.alloc(Label { name: Name::missing() });
        let lifetime = builder.lifetimes.alloc(LifetimeRef::Static);
        builder.binding_owners.insert(binding, matched);
        builder.ident_hygiene.insert(missing.into(), HygieneId::ROOT);
        builder.block_editions.insert(matched, Edition::Edition2021);
        builder.overflow_inheriting.insert(matched);
        let unit = builder.alloc_type_ref(TypeRef::UNIT);
        builder.type_hygiene.push((unit, HygieneId::ROOT));
        for at in 0..n {
            let range = (10 * at, 10 * at + 5);
            builder.expr_map.push((src(0, false, SyntaxKind::MATCH_EXPR, range.0, range.1), matched.into()));
            builder.field_map_back.push((missing, src(0, false, SyntaxKind::RECORD_EXPR_FIELD, at, at + 1)));
        }
        builder.lifetime_map.push((src(0, false, SyntaxKind::LIFETIME, 1, 3), lifetime));
    }

    /// A warm builder lowers without allocating: every table it pushes to is pooled scratch.
    #[test]
    fn a_warm_lowering_allocates_nothing() {
        empty_pool();
        let mut cold = ExpressionStoreBuilder::new();
        lower_every_table(&mut cold, 1000);
        drop(cold.finish());
        let mut warm = ExpressionStoreBuilder::new();
        assert_at_most(0, "lowering on warm scratch", || lower_every_table(&mut warm, 1000));
        drop(warm.finish());
    }

    /// `finish` allocates as many blocks for a body with a thousand map entries as for one with
    /// ten: no sort takes a buffer that grows with the body.
    #[test]
    fn finishing_allocates_no_scratch() {
        empty_pool();
        let finish_of = |n| {
            let mut builder = ExpressionStoreBuilder::new();
            lower_every_table(&mut builder, n);
            measure(|| builder.finish()).1.allocations
        };
        finish_of(1000);
        assert_eq!(finish_of(1000), finish_of(10), "a sort's buffer grew with the body");
    }

    /// Without a source map, a warm lowering and `finish` allocate exactly the finished store's
    /// blocks: the expression box, the column slab, the side-column slab, the three arenas,
    /// the type slab and the type hygiene.
    #[test]
    fn a_warm_store_is_its_blocks_and_nothing_else() {
        empty_pool();
        let lowered = || {
            let mut builder = ExpressionStoreBuilder::new();
            builder.skip_source_map();
            lower_every_table(&mut builder, 10);
            builder.finish()
        };
        drop(lowered());
        let ((store, _), used) = measure(lowered);
        assert_eq!(used.allocations, 8, "a store of 8 blocks");
        let only = store.expr_only.as_deref().expect("a store with expressions");
        assert!(only.columns.arms.in_slab(), "match arms are in the column slab");
        assert!(only.block_editions.in_slab() && only.binding_owners.in_slab());
        assert_eq!(&*only.overflow_inheriting, &[only.block_editions[0].0]);
    }
```
and to the module's imports:
`use blaze_alloc_count::{assert_at_most, measure};` and `use crate::hir::BindingAnnotation;`.

What they hold:
- (1) is the 0 for a warm lowering's scratch.
- (2) shows `finish`'s sorts take no buffer that grows with the body.
- (3) is the memo value counted exactly as lane mem's ruling allows. The count assumes that
  `TypeColumns::freeze_into`, which is paths' file, makes no allocation for empty path tables;
  if paths adds a block there, the count is that block more and the comment says which.
- `expr_store/tests/alloc.rs` (speed-hir-store's) already holds reading a body, a memo hit and
  a scope lookup to 0.

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).
> applied by mem-format-hir-apply (2026-10-29): the three tests and both imports, with the long
> `push` lines split. They need the `blaze-alloc-count` dev-dependency
> (`v2/requests/db.md`, "## speed-hir-store: one counting allocator for hir-def's tests"), which
> `crates/hir-def/Cargo.toml` does not have yet; the other budget tests in hir-def wait on it
> too. The count of 8 holds against the current `TypeColumns::freeze_into`: one slab, and
> `PathColumns::freeze_into`'s `exact` columns are empty. Status: applied.

## mem-format-hir: `Words` on per-thread scratch, names without a `Box<str>` each (`hir/persist.rs`)

Holder: `hir/persist.rs` is in the briefs of speed-hir-store's paths children and of
speed-hir-store-lint (windows `speed-hir-store-next1`, `speed-hir-store-paths-next2`). Every
encode of a store, a body, a signature or an item tree (`deps/signatures.rs`,
`item_tree/persist.rs`) goes through it. None of those call sites needs an edit.

Why: `Words::new` starts four containers empty, and two hash maps, for every buffer. `name`
boxes every distinct text as a map key (`by_text: FxHashMap<Box<str>, u32>`), and `place`
collects the name column into a new `Vec`. A warm encode therefore allocates for each:
- every grown container;
- every distinct non-symbol text;
- the name column.

After this change the containers come from a per-thread stack of spares, are emptied, and go
back when the `Words` is dropped. Texts are found by a 64-bit Fx hash with a chain of earlier
names of the same hash, compared by their bytes in `text`. So `name` gives the same index for
the same text, in the same order, with no allocation of its own. `place` builds the name
column in a kept buffer. hir-def needs no `blaze-mem` for this. Lane mem's ruling is that a
crate's own scratch types live in its own thread-locals, as `expr_store.rs`'s builder pool
does.

Edits:

1. Imports: `use std::cell::RefCell;` stays. Add `use std::hash::BuildHasher;` and
   `use std::ops::{Deref, DerefMut};`. Change `use rustc_hash::FxHashMap;` to
   `use rustc_hash::{FxBuildHasher, FxHashMap};`.

2. Replace `pub(crate) struct Words { .. }` and its `new` with:
   ```rust
   /// The words and names of a buffer being written. Its vectors and maps are this thread's
   /// spares ([`Words::new`]), given back emptied when it is dropped, so a warm write grows
   /// none of them.
   pub(crate) struct Words(WordsBuffers);

   #[derive(Default)]
   pub(crate) struct WordsBuffers {
       words: Vec<u32>,
       text: String,
       /// Each name's start in `text` and its length.
       names: Vec<(u32, u32)>,
       /// The last name of each text's hash; earlier names of the same hash are chained in
       /// `same_hash`.
       by_hash: FxHashMap<u64, u32>,
       /// For each name, the name before it with the same hash, [`NO_NAME`] for none.
       same_hash: Vec<u32>,
       by_symbol: FxHashMap<Symbol, u32>,
       /// What [`Words::place`] builds the name column in.
       placed: Vec<Str>,
   }

   const NO_NAME: u32 = u32::MAX;
   /// The spares a thread keeps (writes nest: a buffer's values are written while another's
   /// are), and the words above which a spare is freed rather than kept.
   const SPARE_WORDS: usize = 4;
   const SPARE_WORDS_CAP: usize = 1 << 22;

   thread_local! {
       // alloc: const-empty, once per thread.
       static SPARES: RefCell<Vec<WordsBuffers>> = const { RefCell::new(Vec::new()) };
   }

   impl WordsBuffers {
       fn clear(&mut self) {
           let WordsBuffers { words, text, names, by_hash, same_hash, by_symbol, placed } = self;
           words.clear();
           text.clear();
           names.clear();
           by_hash.clear();
           same_hash.clear();
           by_symbol.clear();
           placed.clear();
       }

       fn held(&self) -> usize {
           self.words.capacity() + self.text.capacity() / 4 + self.names.capacity()
       }
   }

   impl Deref for Words {
       type Target = WordsBuffers;

       fn deref(&self) -> &WordsBuffers {
           &self.0
       }
   }

   impl DerefMut for Words {
       fn deref_mut(&mut self) -> &mut WordsBuffers {
           &mut self.0
       }
   }

   impl Drop for Words {
       fn drop(&mut self) {
           let mut buffers = std::mem::take(&mut self.0);
           if buffers.held() > SPARE_WORDS_CAP {
               return;
           }
           buffers.clear();
           // A thread that is ending frees it.
           let _ = SPARES.try_with(move |spares| {
               let mut spares = spares.borrow_mut();
               if spares.len() < SPARE_WORDS {
                   spares.push(buffers);
               }
           });
       }
   }

   impl Words {
       /// Words on one of this thread's spares, empty.
       pub(crate) fn new() -> Words {
           let spare = SPARES.try_with(|spares| spares.borrow_mut().pop()).ok().flatten();
           Words(spare.unwrap_or_default())
       }
   ```
   (The `impl Words {` block continues with `at` and the rest as they are.) Every method
   reads `self.words` and the rest through `DerefMut`, unchanged. `WordsBuffers` has no `Drop`,
   so a spare that is freed, whether the stack is full, the spare is too big or the thread is
   ending, is plain memory dropped once, with no recursion.

3. `name`, whole function:
   ```rust
       fn name(&mut self, text: &str) -> u32 {
           let hash = FxBuildHasher.hash_one(text);
           let mut at = self.by_hash.get(&hash).copied().unwrap_or(NO_NAME);
           while at != NO_NAME {
               let (start, len) = self.names[at as usize];
               if &self.text[start as usize..(start + len) as usize] == text {
                   return at;
               }
               at = self.same_hash[at as usize];
           }
           let index = u32::try_from(self.names.len()).expect("more than 2^32 names in one buffer");
           let start = u32::try_from(self.text.len()).expect("more than 4 GiB of names in one buffer");
           let len = u32::try_from(text.len()).expect("a name of 4 GiB or more");
           self.text.push_str(text);
           self.names.push((start, len));
           let earlier = self.by_hash.insert(hash, index).unwrap_or(NO_NAME);
           self.same_hash.push(earlier);
           index
       }
   ```

4. `place` takes `&mut self`; every caller already holds its `Words` as `let mut`
   (`expr_store/persist.rs`, `deps/signatures.rs` L244, `item_tree/persist.rs` L319 and
   L989, and this file's tests). The whole function:
   ```rust
       pub(crate) fn place(&mut self, out: &mut Writer) -> Placed {
           let text_at = out.push_slice(self.text.as_bytes());
           let names_at = out.next::<Str>();
           let WordsBuffers { names, placed, .. } = &mut self.0;
           placed.clear();
           placed.extend(names.iter().enumerate().map(|(index, &(start, len))| {
               let field_at = names_at + (index * size_of::<Str>()) as u64;
               Str::between(field_at, text_at + u64::from(start), len as usize)
           }));
           let pushed = out.push_slice(placed);
           debug_assert_eq!(pushed, names_at, "`Writer::next` is not where `push_slice` writes");
           let names = Column { at: names_at, len: placed.len() as u32 };
           let words_at = out.push_slice(&self.words);
           Placed { words: Column { at: words_at, len: self.at() }, names }
       }
   ```

5. This file's test `words_of` (~L1174): `w.words` (a move out of a `Drop` type) becomes
   `std::mem::take(&mut w.words)`.

Test, in this file's `tests`:
```rust
    /// Names: one index per text whether it comes as a `str` or a symbol, in first-use order,
    /// with texts of one hash told apart by their bytes; a warm `Words` allocates nothing.
    #[test]
    fn names_are_one_index_per_text_on_warm_words() {
        let texts = ["a", "b", "a", "", "ab", "b", ""];
        let indices = |w: &mut Words| -> Vec<u32> { texts.iter().map(|text| w.name(text)).collect() };
        let mut cold = Words::new();
        assert_eq!(indices(&mut cold), [0, 1, 0, 2, 3, 1, 2]);
        cold.symbol(&Symbol::intern("ab"));
        assert_eq!(cold.words.last(), Some(&3), "a symbol's text is the same name");
        drop(cold);
        // A collision: two names chained under one hash are still found by their text.
        let mut w = Words::new();
        let (first, second) = (w.name("x"), w.name("y"));
        let hash = FxBuildHasher.hash_one("x");
        w.by_hash.insert(hash, second);
        w.same_hash[second as usize] = first;
        assert_eq!(w.name("x"), first);
        drop(w);

        let name_all = || {
            let mut warm = Words::new();
            texts.iter().for_each(|text| {
                warm.name(text);
            });
            warm.symbol(&Symbol::intern("ab"));
        };
        name_all();
        blaze_alloc_count::assert_at_most(0, "names on warm words", name_all);
    }
```

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).
> mem-format-hir-apply: held for frontend-deps (hir/persist.rs). `frontend-deps-sigs` names this
> file in its brief and frontend-deps is IN PROGRESS. Told window `frontend-deps-next2`.
> mem-format-hir-apply (2026-10-29 21:16): `hir/persist.rs` is now free. frontend-deps-sigs is
> DONE, frontend-db holds nothing here, and the speed-hir-store family is DONE. hir-def is lane
> db's, so this waits on db (db-next3, asked by tmux): write `> db: ok` or `> db: no <why>` here.
> Status: open (waiting on db).
> db: ok (db-audit-mem for db-next4, 23:03). Not applied yet: `crates/hir-def/src/hir/persist.rs:277` still has `by_text: FxHashMap<Box<str>, u32>`. `hir/` is format's in `docs/v2/plan/reference/crates.md` (row format), so mem-format-hir-apply applies it. Lane db's one caller, `deps/signatures.rs:305`, already binds `let mut words`, like every other `place` caller, so no db file needs an edit. mem-format and mem-format-hir-apply are DONE with no window, so it goes to requests (requests-next3), filed in `v2/requests/db.md`; told by tmux.
> applied by requests-words (2026-10-29 23:20): edits 1 to 5 and the test
> `names_are_one_index_per_text_on_warm_words` (after `a_count_past_the_words_is_refused`),
> text exactly as written here, in `crates/hir-def/src/hir/persist.rs`. The file had not moved
> (`struct Words` at 277, `name` at 356, `place` at 371, `words_of`'s `w.words` at 1184, not
> ~1174). Every `place` caller still binds `let mut`: `deps/signatures.rs:305`,
> `item_tree/persist.rs:389` and `:1059`, `expr_store/persist.rs:789`,
> `expr_store/persist/source_map.rs:518` (the line numbers in edit 4 were older). Nothing else
> builds a `Words` by its fields or with `Default`. hot_lint: nothing in the file. Then, as the
> next section's apply note said, `expr_store/persist.rs`' `a_warm_encode_reuses_its_scratch`
> asserts `assert_at_most(0, "a warm encode", ..)` on the warm encode, in place of
> "fewer warm than cold".

## mem-format-hir: a store's and a body's encode on per-thread scratch (`expr_store/persist.rs`)

Holder: speed-hir-store-lint (window `speed-hir-store-lint`) and speed-hir-store's encode
appliers (`speed-hir-store-apply-encode`). This is written against the text with the four path
columns (`path_kind` .. `path_slots`).

Why: `StoreColumns::write` copies about 14 columns into fresh `Vec`s per store:
- the three node columns from `column`;
- the arms, the four id lists, the five side columns, the expression roots and the type
  hygiene;
- each field column (`FieldNames::column`), and `FieldNames`' own text and map.

`Persist for Body` adds the params. The comment says pushing each column as it is made would
reorder the buffer. That is only true in the order they are made now. Made in the order they
are pushed, each column can be gathered in one kept buffer and pushed at once, with the same
bytes. After this change a warm encode allocates nothing but its output's growth:
- the node columns share one kept buffer, split at known ends;
- every other column is gathered into a kept buffer of its record type and pushed in the
  order the `StoreColumns { .. }` literal pushed it (field order).

Edits:

1. Imports: add `use std::cell::RefCell;`.

2. Replace `fn column` with:
   ```rust
   /// Writes each value's fields and appends the node of each to `nodes`.
   fn column<'v, T: 'v>(
       w: &mut Words,
       values: impl Iterator<Item = &'v T>,
       put: fn(&T, &mut Words) -> u32,
       nodes: &mut Vec<Node>,
   ) {
       nodes.extend(values.map(|value| {
           let at = w.at();
           let kind = put(value, w);
           Node { kind, at }
       }));
   }

   /// Writes `values`, each as `f` makes its record, as a column gathered in `buffer` (a kept
   /// one, left holding them).
   fn push_mapped<T, R: blaze_format::Record>(
       out: &mut Writer,
       buffer: &mut Vec<R>,
       values: &[T],
       f: impl FnMut(&T) -> R,
   ) -> Column {
       buffer.clear();
       buffer.extend(values.iter().map(f));
       Column::push(out, buffer)
   }

   /// The columns an encode gathers before it pushes them, kept per thread. Taken for one
   /// encode at a time: an encode writes no other store or body inside it.
   #[derive(Default)]
   struct EncodeScratch {
       nodes: Vec<Node>,
       u32s: Vec<u32>,
       u64s: Vec<u64>,
       pairs: Vec<IdPair>,
       arms: Vec<ArmRec>,
       roots: Vec<ExprRootRec>,
       fields: Vec<FieldRec>,
       names: FieldNames,
   }

   thread_local! {
       static ENCODE: RefCell<EncodeScratch> = RefCell::new(EncodeScratch::default());
   }
   ```

3. `FieldNames`: add
   ```rust
       fn clear(&mut self) {
           self.text.clear();
           self.by_symbol.clear();
       }
   ```
   and `FieldNames::column` takes a last parameter `records: &mut Vec<FieldRec>`. Its
   `let records: Vec<FieldRec> = (fields.enumerate()).map(..)/* alloc .. */.collect();`
   becomes `records.clear(); records.extend((fields.enumerate()).map(..));` with the same
   closure, and `Column::push(out, &records)` becomes `Column::push(out, records)`.

4. `StoreColumns::write`. Replace the whole function with:
   ```rust
       fn write(store: &ExpressionStore, out: &mut Writer) -> StoreColumns {
           ENCODE.with(|scratch| Self::write_in(store, out, &mut scratch.borrow_mut()))
       }

       /// [`Self::write`]: each column gathered and pushed in turn, in the buffer's order.
       fn write_in(
           store: &ExpressionStore,
           out: &mut Writer,
           scratch: &mut EncodeScratch,
       ) -> StoreColumns {
           let ExpressionStore { expr_only, types, lifetimes, type_hygiene } = store;
           let expr_only = expr_only.as_deref();
           let hir = expr_only.map(|it| &it.columns);
           let EncodeScratch { nodes, u32s, u64s, pairs, arms: arm_recs, roots, fields, names } =
               scratch;
           let mut w = Words::new();
           let side = SideTables::put(&mut w, hir);
           let ty_side = TypeSide::put(&mut w, types);
           nodes.clear();
           column(&mut w, expr_only.into_iter().flat_map(|it| it.bindings.values()), put_binding, nodes);
           let bindings_end = nodes.len();
           column(&mut w, expr_only.into_iter().flat_map(|it| it.labels.values()), put_label, nodes);
           let labels_end = nodes.len();
           column(&mut w, lifetimes.values(), put_lifetime, nodes);
           let words = w.place(out);

           let (record_fields, field_pats) =
               (slice(hir, |c| &c.record_fields[..]), slice(hir, |c| &c.field_pats[..]));
           names.clear();
           record_fields.iter().for_each(|field| names.add(&field.name));
           field_pats.iter().for_each(|field| names.add(&field.name));
           let text_at = out.push_str(&names.text);
           let record_fields = names.column(
               out,
               text_at,
               record_fields.iter().map(|field| (&field.name, raw(field.expr))),
               fields,
           );
           let field_pats = names.column(
               out,
               text_at,
               field_pats.iter().map(|field| (&field.name, raw(field.pat))),
               fields,
           );

           let expr_kind = Column::push(out, slice(hir, |c| &c.expr_kind[..]));
           let expr_data = Column::push(out, slice(hir, |c| &c.expr_data[..]));
           let pat_kind = Column::push(out, slice(hir, |c| &c.pat_kind[..]));
           let pat_data = Column::push(out, slice(hir, |c| &c.pat_data[..]));
           let extra = Column::push(out, slice(hir, |c| &c.extra[..]));
           let expr_lists = push_mapped(out, u32s, slice(hir, |c| &c.expr_lists[..]), |&e| raw(e));
           let pat_lists = push_mapped(out, u32s, slice(hir, |c| &c.pat_lists[..]), |&p| raw(p));
           let stmt_kind = Column::push(out, slice(hir, |c| &c.stmt_kind[..]));
           let stmt_data = Column::push(out, slice(hir, |c| &c.stmt_data[..]));
           let arms = push_mapped(out, arm_recs, slice(hir, |c| &c.arms[..]), |arm| ArmRec {
               pat: raw(arm.pat),
               guard: opt_raw(arm.guard),
               expr: raw(arm.expr),
           });
           let type_lists =
               push_mapped(out, u32s, slice(hir, |c| &c.type_lists[..]), |&ty| opt_raw(ty));
           let bindings = Column::push(out, &nodes[..bindings_end]);
           let labels = Column::push(out, &nodes[bindings_end..labels_end]);
           let ty_kind = Column::push(out, &types.kind[..]);
           let ty_data = Column::push(out, &types.data[..]);
           let ty_extra = Column::push(out, &types.extra[..]);
           let ty_lists = push_mapped(out, u32s, &types.lists[..], |&ty| raw(ty));
           let path_kind = Column::push(out, &types.paths.kind[..]);
           let path_data = Column::push(out, &types.paths.data[..]);
           let path_extra = Column::push(out, &types.paths.extra[..]);
           let path_slots = Column::push(out, &types.paths.slots[..]);
           let lifetimes = Column::push(out, &nodes[labels_end..]);
           let owners = expr_only.map_or(&[][..], |it| &*it.binding_owners);
           let binding_owners = Column::push(out, owners);
           let blocks = expr_only.map_or(&[][..], |it| &*it.block_scopes);
           let block_scopes = push_mapped(out, u64s, blocks, |&block| bits(block));
           let hygiene = expr_only.map_or(&[][..], |it| &*it.ident_hygiene);
           let ident_hygiene = push_mapped(out, pairs, hygiene, |&(id, hygiene)| IdPair {
               a: packed_bits(id),
               b: hygiene_bits(hygiene),
           });
           let editions = expr_only.map_or(&[][..], |it| &*it.block_editions);
           let block_editions = push_mapped(out, pairs, editions, |&(block, edition)| IdPair {
               a: raw(block),
               b: edition as u32,
           });
           let inheriting = expr_only.map_or(&[][..], |it| &*it.overflow_inheriting);
           let overflow_inheriting = push_mapped(out, u32s, inheriting, |&closure| raw(closure));
           let expr_roots = expr_only.map_or(&[][..], |it| &it.expr_roots[..]);
           let expr_roots = push_mapped(out, roots, expr_roots, |root| ExprRootRec {
               root: raw(root.root),
               exprs_end: raw(root.exprs_end),
               pats_end: raw(root.pats_end),
               bindings_end: raw(root.bindings_end),
           });
           let type_hygiene_pairs = type_hygiene.as_deref().unwrap_or(&[]);
           let type_hygiene_column = push_mapped(out, pairs, type_hygiene_pairs, |&(ty, hygiene)| {
               IdPair { a: raw(ty), b: hygiene_bits(hygiene) }
           });
           let flags = (if expr_only.is_some() { HAS_EXPRS } else { 0 })
               | (if type_hygiene.is_some() { HAS_TYPE_HYGIENE } else { 0 });
           StoreColumns {
               words,
               side,
               ty_side,
               expr_kind,
               expr_data,
               pat_kind,
               pat_data,
               extra,
               expr_lists,
               pat_lists,
               stmt_kind,
               stmt_data,
               arms,
               record_fields,
               field_pats,
               type_lists,
               bindings,
               labels,
               ty_kind,
               ty_data,
               ty_extra,
               ty_lists,
               path_kind,
               path_data,
               path_extra,
               path_slots,
               lifetimes,
               binding_owners,
               block_scopes,
               ident_hygiene,
               block_editions,
               overflow_inheriting,
               expr_roots,
               type_hygiene: type_hygiene_column,
               flags,
           }
       }
   ```
   Why the bytes are the same:
   - The pushes happen in the order the old literal made them: the words and names, the
     field text and the two field columns, then the literal's columns in field order.
   - An empty column pushes an empty slice of the same record type as before.
   - The old `binding_owners` vector was an exact copy of the slice it now pushes in place.
   - `w` lives to the end of the function, as before, and goes back to the spares then.
   - The `ExpressionOnlyStore { .. }` destructuring that named every field is gone. Encode
     reads the five side columns by name, so a new field of the store no longer fails to
     compile here. `StoreColumns::check` and the round-trip tests still catch a column the
     encode leaves out.

5. `Persist for Body::write`: the params
   (`let params: Vec<IdPair> = params.iter().map(param_pair).collect(); let params =
   Column::push(out, &params);`, with its `// alloc` line) become:
   ```rust
           let params =
               ENCODE.with(|scratch| push_mapped(out, &mut scratch.borrow_mut().pairs, params, param_pair));
   ```
   It is taken after `StoreColumns::write` has returned its borrow.

Test, in this file's `tests`:
```rust
    /// An encode on warm per-thread scratch writes the same bytes as on cold, and, into an
    /// output that already has the room, allocates nothing.
    #[test]
    fn a_warm_encode_allocates_nothing() {
        let (db, defs) = bodies(EVERY_KIND);
        let body = Body::of(&db, defs[0]);
        let cold = blaze_format::to_bytes(&*body);
        let warm = blaze_format::to_bytes(&*body);
        assert_eq!(warm, cold, "the same bytes on reused scratch");
        let mut out = Writer::with_buffer(<Body as Persist>::SCHEMA, Vec::with_capacity(2 * cold.len()));
        blaze_alloc_count::assert_at_most(0, "a warm encode into an output with room", || {
            body.write(&mut out)
        });
    }
```
If the 0 fails, the allocation left is in a `Code` impl (`SideTables::put`, `TypeSide::put`,
a path's or literal's `put`). That allocation is the finding, and goes in the holder's report.
The source maps' encode (`expr_store/persist/source_map.rs`) is not in this section. It
collects its key and value columns the same way and gets the same treatment, with one kept
buffer per record type, once srcmap's schema 4 is settled.

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).
> applied by mem-format-hir-apply (2026-10-29): edits 1-5, with the same push order and bytes.
> Adapted in two places:
> - `ENCODE` is not borrowed for the whole encode. `EncodeScratch::with` takes the set out of
>   the thread-local, runs the encode on it and puts it back, so a nested encode gets an empty
>   set where a `RefCell` borrow would panic. None nests today; a fresh set allocates nothing
>   until it grows.
> - The test is `a_warm_encode_reuses_its_scratch`, not a 0 budget. With section 5 held,
>   `Words::new` still starts empty on every encode, so a warm encode is not 0. The test checks
>   that the bytes are the same on fresh and reused scratch, that no kept buffer grows on a warm
>   encode (`EncodeScratch::capacity`, test-only), and that a warm encode allocates fewer times
>   than a cold one. With section 5 in, the warm count is 0 and the last check can become
>   `assert_at_most(0, ..)`.
> Status: applied.
