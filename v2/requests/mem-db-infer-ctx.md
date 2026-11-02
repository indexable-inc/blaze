# mem-db-infer-ctx: requests

Part of `mem-db-infer` (`docs/v2/plan/lanes/mem/db-infer.md`, rules R1 to R8). Files, all in
`crates/hir-ty/src/`: `infer.rs`, `infer/{unify,columns,persist,fallback,diagnostics,ambiguity,
at_error,mutability,opaques,path,autoderef}.rs`. Their owner is lane db (none done), so every
change below is a request. Lines are as of 2026-10-29 21:30 (UTC-7); `infer.rs` moved by 16 lines
while this was written, so each entry quotes its "before" code to find it by. The `db`
parameters (`&'db Db` now) are left as they are.

mem-db-infer-expr's file (`v2/requests/mem-db-infer-expr.md`, X1 to X4) already wrote the
changes to my files that its files call (`write_expr_adj` taking `&[Adjustment]`,
`adjust_steps_as_infer_ok(frame)`, `remove_deferred_call_resolutions` returning a frame list, the
`DynamicCoerceMany<'db, 'f>`/`BreakableContext<'db, 'f>`/`DeferredCallResolution<'db, 'f>` field
types). This file takes them as written there, so the two apply in either order; where an entry
here repeats one of them it says so.

## What these files allocate, in short

| temporary | made | before | after |
|---|---|---|---|
| the dense tables (`type_of_expr`, `type_of_pat`, `type_of_binding`, `type_of_type_placeholder`, `method_resolutions`, `field_resolutions`, `binding_modes`, the variant and assoc maps) | per body, grown per node | `ArenaMap` (global `Vec<Option<V>>`, about 10 doublings each) | `DenseMap<'f, K, V>` (frame `Vec<Option<V>>`); the four node-indexed ones made once at the body's node count |
| the CSR tables (`expr_adjustments`, `assignment_coercions`, `pat_adjustments`) | per body | `CsrBuilder` (2 global `Vec`s each) | `CsrBuilder<'f, ..>` on 2 frame `Vec`s; `expr_adjustments`' ranges made once at the expression count |
| the growing sets (`nodes_with_type_mismatches`, `skipped_ref_pats`, `coercion_casts`, `closures_without_expectation`) | per body | `IdxSet` (global `Vec<u64>`), then `shrink_to_fit` (a realloc each) | `FrameIdxSet<'f, K>`/`FrameExprOrPatSet<'f>`; `freeze` copies the words once, at their length |
| the hash side tables (`type_of_opaque`, `asm_sym_tys`, `closures_data`) | per body | `FxHashMap` | `FrameMap<'f, K, V>` (`hashbrown` in the frame) |
| `from_entries`' temporaries (`dense`, `by_ty`, `tys`, `words`, `ranks`, `rows`, `fields`, `ends`, the CSR compaction slots) | per column per body | global, grown, then `into_boxed_slice` (a shrink realloc each) | frame where temporary (`by_ty`, `tys`, CSR slots); each owned column made once at its exact length, counted first |
| `SortedMap::from_entries` | per side table per body | `collect` + stable `sort_by` (its scratch buffer is a global allocation past 20 entries) | frame `Vec` + `sort_unstable_by` (keys are distinct, so the order is the same), copied once at its length |
| `CsrBuilder::values_mut`'s live ranges | per CSR table at write-back | `collect` + `radix::sort_copy_by_key` (2 global) | frame `Vec` + `sort_unstable_by_key` (starts of non-empty lists are distinct) |
| context temporaries (`unreachable`, `breakables`, `deferred_cast_checks`, `deferred_call_resolutions`, `defined_anon_consts`, `diagnostics`) | per body | global `Vec`/`ThinVec` | frame `Vec`s |
| `vars_emitted_type_must_be_known_for`, write-back's span set | per body (error path) | `FxHashSet` | `FrameSet<'f, _>` |
| `tuple_field_accesses_rev` | per body | `IndexSet<Tys>` (global `Vec` + table) | `TupleFieldTypes<'db, 'f>`: frame `Vec<Tys>` + `FrameMap<Tys, u32>` (R3) |
| `traits_in_scope` and the block-traits cache | per body, per block module with `use Trait` | `FxHashSet<TraitId>` from `Resolver::traits_in_scope` (about 7 doublings) and an `Rc<FxHashSet>` | `FrameSet<'f, TraitId>` filled by `traits_in_scope_into`; the cache is a `Cell<(Option<ModuleId>, Option<&'f FrameSet<'f, TraitId>>)>`, one frame set per block module with `use Trait` |
| `InferenceTable::diverging_type_vars`, fallback's `diverging_roots` | per body | `VarSet` (global `Vec<u64>`) | `VarSet` on `scratch::lend_u64s()` (the table has no `'f`, R2) |
| fallback's `unresolved_variables` and `diverging_fallback` | per body with unsolved variables | `Vec<Ty>` (`collect`), `FxHashMap` | frame `Vec` at exact length, `FrameMap` |
| write-back's `goals` | per resolved value | `vec![]` (empty unless normalization returns goals, which it then dropped) | gone: the goals are dropped where they are returned |
| write-back's `diagnostics`/`unresolved_vars` | per body (error path) | `ThinVec`, `shrink_to_fit` | frame `Vec`s; the result's `Box` made once from them |
| `resolve_projections_within`'s `traits` | per path resolution with a projection | `collect` into `Vec<TraitRef>` | one pass, no list |
| persist encode (`InferenceRecord::of`, `Sink`) | per record | 3 global `Vec`s grown, `Sink`'s 2 `Vec`s | the thread's `scratch` buffers (`u64`, `u32`, `u8`: warm across records), each array copied once at its length |
| persist decode (`decode`, `Source`) | per record | `collect::<Result<..>>` per column (grows, then shrinks) | `exact`/`Source::list`: one allocation of the column's length; temporaries (`buffer`, `Source::tys`' lists, the closure decoder's) in the frame of the task `to_result` opens |

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:44): a summary of C, A, U, E and P below, each settled in its own section (columns `crates/hir-ty/src/infer/columns.rs`, context `infer.rs`, persist `infer/persist.rs:1064`).

## Prerequisites (other parts)

- **mem-db** request 1: `blaze_mem::frame::current(f: impl FnOnce(&Frame) -> R) -> R` (the
  frame of the running task; panics outside one). Not in `crates/blaze-mem/src/frame.rs` yet.
- **mem-db-infer** 1 to 3: `blaze-mem` and `hashbrown` (with `nightly`) in hir-ty's
  `[dependencies]`, `#![feature(allocator_api)]` in `lib.rs`, `crate::db::INFER_TAG`.
- **mem-db-salsa**: the `mem_tag = <path>` option of `#[salsa::tracked]` (opens the query's
  `frame::task` with that tag, which `frame::current` below reads).
- **mem-db-infer-lower** (R2): `TyLoweringContext::new(.., lifetime_lowering_mode, frame: &'a
  Frame)`, the frame as its last parameter. Used in `infer/diagnostics.rs` below.
- **mem-db-def**: `Resolver::traits_in_scope_into` takes any `Extend<TraitId>` (entry D1 at the
  end). Used by `InferenceContext::new`.
- **mem-db-infer-method**: `MethodResolutionContext::traits_in_scope` becomes a
  `TraitsInScope<'a>` (the inference's frame set, or a caller's `FxHashSet`; entry M1 at the end),
  so the context's frame set goes in as it is and `crates/hir`'s API does not change.

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:44): in the tree at `crates/blaze-mem/src/frame.rs:1254` (`current`), `crates/hir-ty/Cargo.toml:51`, `crates/hir-ty/src/lib.rs:5`, `crates/hir-ty/src/db.rs:46` (`INFER_TAG`), `crates/hir-def/src/resolver.rs:907` (`traits_in_scope_into`), `crates/hir-ty/src/lower.rs:355` (the lowering context's frame); salsa's `mem_tag` in `vendor/salsa/src/function/execute.rs`.

## C. `infer/columns.rs`

Owned columns (`TyColumn`, `IndexColumn`, `ExprOrPatColumn`, `FieldColumn`, `Csr`, `IdxSet`,
`ExprOrPatSet`, `ModeColumn`, `SortedMap`, `Presence`, `Range`) keep their fields, layout and
derived `Eq`/`Hash` exactly: persist encodes them and `from_parts`/`from_words`/`from_codes`/
`from_sorted` are unchanged. What changes is how they are made, and the builders inference writes
(`CsrBuilder`, `ExprOrPatMap`, the new `DenseMap`, `FrameIdxSet`, `FrameExprOrPatSet`), which
move onto the frame.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-columns (21:28): C1 to C14 below, on top of dedup's `blaze_ids::bits` change (kept).

### C1. Imports (lines 11 to 29)

Before:

```rust
use std::{
    fmt,
    marker::PhantomData,
    ops::Index,
};

use blaze_ids::radix;
use hir_def::{
    FieldId, TupleFieldId,
    hir::{ExprId, ExprOrPatIdPacked, PatId},
};
use la_arena::{ArenaMap, Idx, RawIdx};
use rustc_ast_ir::Mutability;
use rustc_hash::FxHashMap;
```

After:

```rust
use std::{
    fmt,
    marker::PhantomData,
    ops::Index,
};

use blaze_mem::{Frame, scratch};
use hir_def::{
    FieldId, TupleFieldId,
    hir::{ExprId, ExprOrPatIdPacked, PatId},
};
use la_arena::{Idx, RawIdx};
use rustc_ast_ir::Mutability;
use rustc_hash::FxBuildHasher;
```

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-columns (21:28): imports are `blaze_ids::bits::{BitSet, Bits, Ranked}` (dedup's, `radix` gone with C8), `blaze_mem::Frame` (no `scratch`: `VarSet` is gone, see C12), `la_arena::{Idx, RawIdx}`, no `rustc_hash` (the hasher is in `lib.rs`'s aliases), and `pub(crate) use crate::{FrameMap, FrameSet, frame_map, frame_set};`.

### C2. Frame maps, and `dense` (lines 112 to 132)

Before:

```rust
/// Sets `index`'s bit in `words`, growing them.
fn set_bit(words: &mut Vec<u64>, index: usize) {
    let at = index / 64;
    if at >= words.len() {
        words.resize(at + 1, 0);
    }
    words[at] |= 1 << (index % 64);
}

/// The values of `entries` by key, in key order.
fn dense<K: DenseKey, V>(entries: impl IntoIterator<Item = (K, V)>) -> Vec<Option<V>> {
    let mut dense: Vec<Option<V>> = Vec::new();
    for (key, value) in entries {
        let index = key.to_index();
        if index >= dense.len() {
            dense.resize_with(index + 1, || None);
        }
        dense[index] = Some(value);
    }
    dense
}
```

After (`set_bit` is used by the tests only now, so it moves into `mod tests`, unchanged; `dense`
is used by the test-only `from_entries` below):

```rust
/// A hash map in an inference's frame, hashed as `FxHashMap` is.
pub(crate) type FrameMap<'f, K, V> = hashbrown::HashMap<K, V, FxBuildHasher, &'f Frame>;

/// A hash set in an inference's frame, hashed as `FxHashSet` is.
pub(crate) type FrameSet<'f, T> = hashbrown::HashSet<T, FxBuildHasher, &'f Frame>;

pub(crate) fn frame_map<K, V>(frame: &Frame) -> FrameMap<'_, K, V> {
    hashbrown::HashMap::with_hasher_in(FxBuildHasher, frame)
}

pub(crate) fn frame_set<T>(frame: &Frame) -> FrameSet<'_, T> {
    hashbrown::HashSet::with_hasher_in(FxBuildHasher, frame)
}

/// How many of `slots` a column of them keeps: up to the last with a value.
fn used<V>(slots: &[Option<V>]) -> usize {
    slots.iter().rposition(Option::is_some).map_or(0, |last| last + 1)
}

/// Which of `slots` have a value, as a [`Presence`] made at its length.
fn presence_of<V>(slots: &[Option<V>]) -> Presence {
    let used = used(slots);
    // alloc: the column's own words, at their length
    let mut words = vec![0u64; used.div_ceil(64)];
    for (index, slot) in slots[..used].iter().enumerate() {
        if slot.is_some() {
            words[index / 64] |= 1 << (index % 64);
        }
    }
    Presence::new(words).expect("the last used slot has a value")
}

/// The values of `entries` by key, in key order, in `frame`.
#[cfg(test)]
fn dense_in<K: DenseKey, V>(
    frame: &Frame,
    entries: impl IntoIterator<Item = (K, V)>,
) -> Vec<Option<V>, &Frame> {
    let mut dense = Vec::new_in(frame);
    for (key, value) in entries {
        let index = key.to_index();
        if index >= dense.len() {
            dense.resize_with(index + 1, || None);
        }
        dense[index] = Some(value);
    }
    dense
}
```

`Presence::new` (line 70) is unchanged: `vec![0; n]` has capacity `n`, so `into_boxed_slice`
moves it without a realloc, and `ranks` is `with_capacity(words.len())`.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-columns (21:28): the aliases are the re-export of `lib.rs`'s (mem-db-infer.md section 5), not defined here; `set_bit` was already gone (dedup: `BitSet::insert_growing`), so `presence_of` writes its exact-length words through `BitSet::from_words(vec![0; n]).insert`; `Presence` is dedup's `Ranked` wrapper, whose `from_words` boxes the `Vec` without a realloc.

### C3. `DenseMap` (new, after `dense_in`)

What `ArenaMap` is to inference, its slots in the frame. Only the methods inference calls are
there (`iter`, `get_mut` and a draining iterator have no caller in hir-ty, and an unused
`pub(crate)` method is a dead-code warning): `ArenaMap`'s `insert` (returning the old value),
`get`, `contains_idx`, `values_mut` and `Index` are the uses, in `infer.rs`, `infer/expr.rs`,
`infer/pat.rs`, `infer/mutability.rs`, `infer/diagnostics.rs`, `infer/closure/`.

```rust
/// A value per arena index while inference writes it, in the frame: `ArenaMap`'s slots, a
/// `None` per index without a value.
pub(crate) struct DenseMap<'f, K, V> {
    slots: Vec<Option<V>, &'f Frame>,
    key: PhantomData<fn() -> K>,
}

impl<'f, K: DenseKey, V> DenseMap<'f, K, V> {
    pub(crate) fn new_in(frame: &'f Frame) -> Self {
        DenseMap { slots: Vec::new_in(frame), key: PhantomData }
    }

    /// Room for the keys below `len`, so that writing them moves nothing in the frame.
    pub(crate) fn reserve_keys(&mut self, len: usize) {
        if len > self.slots.len() {
            self.slots.resize_with(len, || None);
        }
    }

    pub(crate) fn insert(&mut self, key: K, value: V) -> Option<V> {
        let index = key.to_index();
        if index >= self.slots.len() {
            self.slots.resize_with(index + 1, || None);
        }
        self.slots[index].replace(value)
    }

    #[inline]
    pub(crate) fn get(&self, key: K) -> Option<&V> {
        self.slots.get(key.to_index())?.as_ref()
    }

    pub(crate) fn contains_idx(&self, key: K) -> bool {
        self.get(key).is_some()
    }

    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.slots.iter_mut().flatten()
    }

    /// The slots, a `None` for each key without a value.
    pub(crate) fn slots(&self) -> &[Option<V>] {
        &self.slots
    }

    pub(crate) fn slots_mut(&mut self) -> &mut [Option<V>] {
        &mut self.slots
    }
}

impl<K: DenseKey, V> Index<K> for DenseMap<'_, K, V> {
    type Output = V;

    #[track_caller]
    fn index(&self, key: K) -> &V {
        self.get(key).expect("no value for this key")
    }
}

impl<K: DenseKey + fmt::Debug, V: fmt::Debug> fmt::Debug for DenseMap<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let entries = (self.slots.iter().enumerate())
            .filter_map(|(index, slot)| Some((K::from_index(index), slot.as_ref()?)));
        f.debug_map().entries(entries).finish()
    }
}
```

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-columns (21:28): as written, with C14's `Debug` body.

### C4. `TyColumn::from_entries` (lines 150 to 166)

Before:

```rust
impl<K: DenseKey> TyColumn<K> {
    pub(crate) fn from_entries(entries: impl IntoIterator<Item = (K, StoredTy)>) -> Self {
        let dense = dense(entries);
        let mut by_ty: FxHashMap<StoredTy, u32> = FxHashMap::default();
        let mut tys = Vec::new();
        let at = dense
            .into_iter()
            .map(|ty| match ty {
                None => NONE,
                Some(ty) => *by_ty.entry(ty).or_insert_with_key(|ty| {
                    tys.push(ty.clone());
                    place(tys.len() - 1)
                }),
            })
            .collect();
        TyColumn { at, tys: tys.into_boxed_slice(), key: PhantomData }
    }
```

After (the dictionary is made in the frame and copied once at its length; `at` is made at its
length; the slots are emptied):

```rust
impl<K: DenseKey> TyColumn<K> {
    /// The column of `slots`, which it takes the types out of.
    pub(crate) fn from_dense(frame: &Frame, slots: &mut [Option<StoredTy>]) -> Self {
        let used = used(slots);
        let mut by_ty: FrameMap<'_, StoredTy, u32> = frame_map(frame);
        let mut tys = Vec::new_in(frame);
        // alloc: the column's own places, at their length
        let mut at = Vec::with_capacity(used);
        for slot in &mut slots[..used] {
            at.push(match slot.take() {
                None => NONE,
                Some(ty) => *by_ty.entry(ty).or_insert_with_key(|ty| {
                    tys.push(*ty);
                    place(tys.len() - 1)
                }),
            });
        }
        TyColumn { at: at.into_boxed_slice(), tys: tys.drain(..).collect(), key: PhantomData }
    }

    #[cfg(test)]
    pub(crate) fn from_entries(entries: impl IntoIterator<Item = (K, StoredTy)>) -> Self {
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
            Self::from_dense(frame, &mut dense_in(frame, entries))
        })
    }
```

(`StoredTy` is `Copy`, as mem-db-infer-expr's X1 notes: `*ty` is the copy `ty.clone()` was.
`tys.drain(..)` is `TrustedLen`, so the `Box` is made at its length.)

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-columns (21:28): as written; `from_parts`' doc links name `from_dense` (the test-only `from_entries` would be a broken link).

### C5. `IndexColumn::from_entries` (lines 257 to 268)

Before:

```rust
impl<K: DenseKey, V> IndexColumn<K, V> {
    pub(crate) fn from_entries(entries: impl IntoIterator<Item = (K, V)>) -> Self {
        let (mut words, mut rows) = (Vec::new(), Vec::new());
        for (index, row) in dense(entries).into_iter().enumerate() {
            if let Some(row) = row {
                set_bit(&mut words, index);
                rows.push(row);
            }
        }
        let present = Presence::new(words).expect("a bit is set in every word that is made");
        IndexColumn { present, rows: rows.into_boxed_slice(), key: PhantomData }
    }
```

After:

```rust
impl<K: DenseKey, V> IndexColumn<K, V> {
    /// The column of `slots`, which it takes the rows out of.
    pub(crate) fn from_dense(slots: &mut [Option<V>]) -> Self {
        let present = presence_of(slots);
        // alloc: the column's own rows, at their length
        let mut rows = Vec::with_capacity(present.count());
        rows.extend(slots.iter_mut().filter_map(Option::take));
        IndexColumn { present, rows: rows.into_boxed_slice(), key: PhantomData }
    }

    #[cfg(test)]
    pub(crate) fn from_entries(entries: impl IntoIterator<Item = (K, V)>) -> Self {
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
            Self::from_dense(&mut dense_in(frame, entries))
        })
    }
```

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-columns (21:28): applied to dedup's `BitSet::insert_growing` form of the old body; same result.

### C6. `ExprOrPatMap` (lines 353 to 393)

Before:

```rust
/// A map of expressions or patterns to values while inference writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExprOrPatMap<V> {
    exprs: ArenaMap<ExprId, V>,
    pats: ArenaMap<PatId, V>,
}

impl<V> Default for ExprOrPatMap<V> {
    fn default() -> Self {
        ExprOrPatMap { exprs: ArenaMap::default(), pats: ArenaMap::default() }
    }
}

impl<V> ExprOrPatMap<V> {
```

After (the methods' bodies, `insert` to `values_mut`, are unchanged; `freeze` changes):

```rust
/// A map of expressions or patterns to values while inference writes it, in the frame.
#[derive(Debug)]
pub(crate) struct ExprOrPatMap<'f, V> {
    exprs: DenseMap<'f, ExprId, V>,
    pats: DenseMap<'f, PatId, V>,
}

impl<'f, V> ExprOrPatMap<'f, V> {
    pub(crate) fn new_in(frame: &'f Frame) -> Self {
        ExprOrPatMap { exprs: DenseMap::new_in(frame), pats: DenseMap::new_in(frame) }
    }
```

and `freeze`, before:

```rust
    pub(crate) fn freeze(self) -> ExprOrPatColumn<V> {
        ExprOrPatColumn {
            exprs: IndexColumn::from_entries(self.exprs),
            pats: IndexColumn::from_entries(self.pats),
        }
    }
```

after:

```rust
    pub(crate) fn freeze(mut self) -> ExprOrPatColumn<V> {
        ExprOrPatColumn {
            exprs: IndexColumn::from_dense(self.exprs.slots_mut()),
            pats: IndexColumn::from_dense(self.pats.slots_mut()),
        }
    }
```

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-columns (21:28): as written (no `Default`, so persist's decoder takes P's `ExprOrPatMap::new_in(frame)`).

### C7. `FieldColumn::from_entries` (lines 410 to 439)

Before:

```rust
impl FieldColumn {
    pub(crate) fn from_entries(
        entries: impl IntoIterator<Item = (ExprId, either::Either<FieldId, TupleFieldId>)>,
    ) -> Self {
        let dense = dense(entries);
        let (mut fields, mut tuple_fields) = (Vec::new(), Vec::new());
        let mut tags = Vec::with_capacity(dense.len());
        let mut data = Vec::with_capacity(dense.len());
        for field in dense {
```

After (each of the four parts counted, then made at its length; the loop body from `let (tag,
word) = match field {` to the end of the function is unchanged):

```rust
impl FieldColumn {
    pub(crate) fn from_dense(slots: &[Option<either::Either<FieldId, TupleFieldId>>]) -> Self {
        let slots = &slots[..used(slots)];
        let tuple_count = slots.iter().filter(|it| matches!(it, Some(either::Either::Right(_)))).count();
        let field_count = slots.iter().filter(|it| it.is_some()).count() - tuple_count;
        // alloc: the column's own parts, each at its length
        let (mut fields, mut tuple_fields) =
            (Vec::with_capacity(field_count), Vec::with_capacity(tuple_count));
        let mut tags = Vec::with_capacity(slots.len());
        let mut data = Vec::with_capacity(slots.len());
        for &field in slots {
```

and after the function:

```rust
    #[cfg(test)]
    pub(crate) fn from_entries(
        entries: impl IntoIterator<Item = (ExprId, either::Either<FieldId, TupleFieldId>)>,
    ) -> Self {
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| Self::from_dense(&dense_in(frame, entries)))
    }
```

(`Either<FieldId, TupleFieldId>` is `Copy`. Before, `dense` ended at the last key with a field,
so the tags never ended in `FIELD_NONE`; `used` keeps that.)

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-columns (21:28): as written.

### C8. `CsrBuilder` (lines 631 to 780)

Before (header to `start`):

```rust
#[derive(Clone, Debug)]
pub(crate) struct CsrBuilder<K, T> {
    ranges: Vec<Range>,
    items: Vec<T>,
    key: PhantomData<fn() -> K>,
}

impl<K, T> Default for CsrBuilder<K, T> {
    fn default() -> Self {
        CsrBuilder { ranges: Vec::new(), items: Vec::new(), key: PhantomData }
    }
}

impl<K: DenseKey, T> CsrBuilder<K, T> {
```

After (`range` through `push` and `iter` unchanged):

```rust
#[derive(Clone, Debug)]
pub(crate) struct CsrBuilder<'f, K, T> {
    ranges: Vec<Range, &'f Frame>,
    items: Vec<T, &'f Frame>,
    key: PhantomData<fn() -> K>,
}

impl<'f, K, T> CsrBuilder<'f, K, T> {
    pub(crate) fn new_in(frame: &'f Frame) -> Self {
        CsrBuilder { ranges: Vec::new_in(frame), items: Vec::new_in(frame), key: PhantomData }
    }
}

impl<'f, K: DenseKey, T> CsrBuilder<'f, K, T> {
    /// Room for the keys below `len`, so that writing them moves nothing in the frame.
    pub(crate) fn reserve_keys(&mut self, len: usize) {
        if len > self.ranges.len() {
            self.ranges.resize(len, Range::ABSENT);
        }
    }
```

`values_mut`, before:

```rust
        let CsrBuilder { ranges, items, .. } = self;
        // An empty list may start inside another one; lists with items never overlap.
        let mut live: Vec<Range> =
            ranges.iter().copied().filter(|it| !it.is_absent() && it.len != 0).collect();
        radix::sort_copy_by_key(&mut live, |range| range.start);
```

after (the rest of `values_mut` unchanged):

```rust
        let CsrBuilder { ranges, items, .. } = self;
        let mut live = Vec::new_in(*ranges.allocator());
        // An empty list may start inside another one; lists with items never overlap, so their
        // starts are distinct and an unstable sort orders them as a stable one does.
        live.extend(ranges.iter().copied().filter(|it| !it.is_absent() && it.len != 0));
        live.sort_unstable_by_key(|range| range.start);
```

`freeze`, before:

```rust
    pub(crate) fn freeze(self) -> Csr<K, T> {
        let CsrBuilder { ranges, items, .. } = self;
        let mut words = Vec::new();
        let mut ends = Vec::new();
        let mut next = 0usize;
        let mut in_order = true;
        for (index, range) in ranges.iter().enumerate() {
            if range.is_absent() {
                continue;
            }
            set_bit(&mut words, index);
            in_order &= range.start as usize == next;
            next += range.len as usize;
            ends.push(place(next));
        }
        let present = Presence::new(words).expect("a bit is set in every word that is made");
        let ends = ends.into_boxed_slice();
        if in_order && next == items.len() {
            return Csr { present, ends, items: items.into_boxed_slice(), key: PhantomData };
        }
        let mut items: Vec<Option<T>> = items.into_iter().map(Some).collect();
        let mut flat = Vec::with_capacity(next);
        for range in ranges.iter().filter(|range| !range.is_absent()) {
            flat.extend(items[range.slice()].iter_mut().map(|it| it.take().unwrap()));
        }
        Csr { present, ends, items: flat.into_boxed_slice(), key: PhantomData }
    }
```

after:

```rust
    pub(crate) fn freeze(self) -> Csr<K, T> {
        let CsrBuilder { ranges, mut items, .. } = self;
        let frame = *items.allocator();
        let used = ranges.iter().rposition(|range| !range.is_absent()).map_or(0, |last| last + 1);
        let ranges = &ranges[..used];
        let lists = ranges.iter().filter(|range| !range.is_absent()).count();
        // alloc: the column's own words and ends, at their lengths
        let mut words = vec![0u64; used.div_ceil(64)];
        let mut ends = Vec::with_capacity(lists);
        let mut next = 0usize;
        let mut in_order = true;
        for (index, range) in ranges.iter().enumerate() {
            if range.is_absent() {
                continue;
            }
            words[index / 64] |= 1 << (index % 64);
            in_order &= range.start as usize == next;
            next += range.len as usize;
            ends.push(place(next));
        }
        let present = Presence::new(words).expect("the last used range is a list");
        let ends = ends.into_boxed_slice();
        if in_order && next == items.len() {
            return Csr { present, ends, items: items.drain(..).collect(), key: PhantomData };
        }
        let mut slots = Vec::with_capacity_in(items.len(), frame);
        slots.extend(items.drain(..).map(Some));
        // alloc: the column's own items, at their length
        let mut flat = Vec::with_capacity(next);
        for range in ranges.iter().filter(|range| !range.is_absent()) {
            flat.extend(slots[range.slice()].iter_mut().map(|it| it.take().unwrap()));
        }
        Csr { present, ends, items: flat.into_boxed_slice(), key: PhantomData }
    }
```

Why: the ranges were made at the key count (`reserve_keys`), so trailing absent ranges are cut
before the words are counted; before, `set_bit` reached the same last word. In the fast path the
items were the builder's `Vec` boxed (a shrink realloc when it had slack); now they are copied
once from the frame at their length.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-columns (21:28): applied to dedup's form of `freeze` (its words a `BitSet`): the exact-length words are `BitSet::from_words(vec![0; n])` with `insert`; otherwise as written.

### C9. `IdxSet`, `ExprOrPatSet` and their frame forms (lines 782 to 897)

Delete `IdxSet::shrink_to_fit` (lines 840 to 842; `freeze` below makes the words at their
length, R4). Mark `ExprOrPatSet::insert` (line 873) `#[cfg(test)]`: inference writes the frame
form below, and only persist's test builds an owned set by inserting. After `impl ExprOrPatSet`
(line 897) add:

```rust
/// An [`IdxSet`] while inference writes it, its words in the frame and made at the key count.
#[derive(Debug)]
pub(crate) struct FrameIdxSet<'f, K> {
    words: Vec<u64, &'f Frame>,
    key: PhantomData<fn() -> K>,
}

impl<'f, K: DenseKey> FrameIdxSet<'f, K> {
    pub(crate) fn new_in(frame: &'f Frame) -> Self {
        FrameIdxSet { words: Vec::new_in(frame), key: PhantomData }
    }

    /// Adds `key`, returning whether it was not in the set.
    pub(crate) fn insert(&mut self, key: K) -> bool {
        let index = key.to_index();
        let word = index / 64;
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }
        let bit = 1u64 << (index % 64);
        let new = self.words[word] & bit == 0;
        self.words[word] |= bit;
        new
    }

    #[inline]
    pub(crate) fn contains(&self, key: &K) -> bool {
        let index = key.to_index();
        self.words.get(index / 64).is_some_and(|word| word & (1 << (index % 64)) != 0)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.words.iter().all(|&word| word == 0)
    }

    /// The set, its words copied once at their length, the last never zero.
    pub(crate) fn freeze(&self) -> IdxSet<K> {
        let used = self.words.iter().rposition(|&word| word != 0).map_or(0, |last| last + 1);
        // alloc: the set's own words, at their length
        IdxSet { words: self.words[..used].to_vec(), key: PhantomData }
    }
}

impl<K: DenseKey> Extend<K> for FrameIdxSet<'_, K> {
    fn extend<I: IntoIterator<Item = K>>(&mut self, iter: I) {
        iter.into_iter().for_each(|key| _ = self.insert(key));
    }
}

/// An [`ExprOrPatSet`] while inference writes it, in the frame.
#[derive(Debug)]
pub(crate) struct FrameExprOrPatSet<'f> {
    exprs: FrameIdxSet<'f, ExprId>,
    pats: FrameIdxSet<'f, PatId>,
}

impl<'f> FrameExprOrPatSet<'f> {
    pub(crate) fn new_in(frame: &'f Frame) -> Self {
        FrameExprOrPatSet { exprs: FrameIdxSet::new_in(frame), pats: FrameIdxSet::new_in(frame) }
    }

    pub(crate) fn insert(&mut self, node: ExprOrPatIdPacked) -> bool {
        match (node.as_expr(), node.as_pat()) {
            (Some(expr), _) => self.exprs.insert(expr),
            (None, Some(pat)) => self.pats.insert(pat),
            (None, None) => false,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.exprs.is_empty() && self.pats.is_empty()
    }

    pub(crate) fn freeze(&self) -> ExprOrPatSet {
        ExprOrPatSet { exprs: self.exprs.freeze(), pats: self.pats.freeze() }
    }
}
```

(`IdxSet`'s `insert`, `FromIterator` and `Extend` stay: persist's test and the `FromIterator`
impl use them. Its `words: Vec<u64>` stays a `Vec`, as persist decodes it.)

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-columns (21:28): `IdxSet`'s words are dedup's `BitSet` now (still a `Vec<u64>` inside); `FrameIdxSet`'s are `Bits<Vec<u64, &'f Frame>>`, so `insert`/`contains`/`is_empty` are `blaze_ids::bits`' own, and `freeze` makes `BitSet::from_words(words[..used].to_vec())`. Names and signatures as written.

### C10. `ModeColumn::from_entries` (lines 940 to 944)

Before:

```rust
    pub(crate) fn from_entries(entries: impl IntoIterator<Item = (K, BindingMode)>) -> Self {
        let codes = dense(entries).into_iter().map(|mode| mode.map_or(MODE_NONE, mode_code));
        ModeColumn { codes: codes.collect(), key: PhantomData }
    }
```

After (a slice iterator is `TrustedLen`, so the `Box` is made at its length):

```rust
    pub(crate) fn from_dense(slots: &[Option<BindingMode>]) -> Self {
        let codes = slots[..used(slots)].iter().map(|mode| mode.map_or(MODE_NONE, mode_code));
        // alloc: the column's own codes, at their length
        ModeColumn { codes: codes.collect(), key: PhantomData }
    }

    #[cfg(test)]
    pub(crate) fn from_entries(entries: impl IntoIterator<Item = (K, BindingMode)>) -> Self {
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| Self::from_dense(&dense_in(frame, entries)))
    }
```

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-columns (21:28): as written.

### C11. `SortedMap::from_entries` (lines 997 to 1004)

Before:

```rust
    /// The map of `entries`, of distinct keys.
    pub(crate) fn from_entries(entries: impl IntoIterator<Item = (K, V)>) -> Self {
        let mut entries: Vec<(K, V)> = entries.into_iter().collect();
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        debug_assert!(entries.windows(2).all(|it| it[0].0 != it[1].0), "a key twice in a map");
        SortedMap { entries: entries.into_boxed_slice() }
    }
```

After:

```rust
    /// The map of `entries`, of distinct keys, sorted in `frame`.
    pub(crate) fn from_entries_in(frame: &Frame, entries: impl IntoIterator<Item = (K, V)>) -> Self {
        let mut sorted = Vec::new_in(frame);
        sorted.extend(entries);
        // The keys are distinct, so an unstable sort orders them as a stable one does.
        sorted.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        debug_assert!(sorted.windows(2).all(|it| it[0].0 != it[1].0), "a key twice in a map");
        SortedMap { entries: sorted.drain(..).collect() }
    }

    #[cfg(test)]
    pub(crate) fn from_entries(entries: impl IntoIterator<Item = (K, V)>) -> Self {
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| Self::from_entries_in(frame, entries))
    }
```

`sort_by` (stable) allocates a scratch buffer past 20 entries (`driftsort`); `sort_unstable_by`
allocates nothing.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-columns (21:28): as written, with `// alloc:` on the owned copy.

### C12. `VarSet` (lines 1091 to 1117)

Before:

```rust
#[derive(Clone, Default, Debug)]
pub(crate) struct VarSet {
    words: Vec<u64>,
}

impl VarSet {
    pub(crate) fn insert(&mut self, var: u32) {
```

After (`insert`, `contains`, `iter` unchanged; `self.words` derefs to the `Vec`):

```rust
#[derive(Debug)]
pub(crate) struct VarSet {
    /// Lent from the thread's shelf: the table that holds this set has no frame lifetime
    /// (`InferenceTable<'db>`, R2), and the buffer stays warm across bodies.
    words: scratch::Lent<Vec<u64>>,
}

impl VarSet {
    pub(crate) fn new() -> VarSet {
        VarSet { words: scratch::lend_u64s() }
    }

    pub(crate) fn insert(&mut self, var: u32) {
```

`Lent` is `Debug`; nothing clones a `VarSet` (its two makers, `unify.rs:175` and
`fallback.rs:158`, change to `VarSet::new()` below). A thread nesting more than `DEPTH` (4) lent
`u64` buffers gets a new one, which goes on the shelf when dropped.

> forwarded to db (from requests, pass 17; told by tmux).
> not applied by db-mem-infer-a-columns (21:28): `VarSet` is gone from `columns.rs` (dedup bits: `unify.rs`' `diverging_type_vars` and `fallback.rs`' `diverging_roots` are `blaze_ids::bits::BitSet<u32>` now). Its rule (a lent `scratch` buffer, R2) would now land on those two `BitSet<u32>` sites in `unify.rs`/`fallback.rs`, not this file; handed to db-mem-infer-a.

### C13. Tests (`mod tests`, line 1120 on)

1. `use la_arena::{Idx, RawIdx};` becomes `use la_arena::{ArenaMap, Idx, RawIdx};` (the tests
   still model with `ArenaMap`), and add `use blaze_mem::frame;`. Move `set_bit` (C2) into the
   module, unchanged.
2. The four tests that make a `CsrBuilder` (`csr_builder_agrees_with_a_map_of_lists`,
   `a_csr_written_in_another_order_is_the_same_column`,
   `a_list_removed_at_the_end_leaves_the_same_column`, `an_empty_list_is_not_an_absent_one`)
   put their body inside `frame::task(crate::db::INFER_TAG, |frame| { .. })`, and each
   `CsrBuilder::<Key, u32>::default()` becomes `CsrBuilder::<Key, u32>::new_in(frame)`.
3. `var_set_lists_what_it_holds`: `VarSet::default()` becomes `VarSet::new()`.
4. New: the frame builders against the builders as they were. Add at the end of `mod tests`:

```rust
    /// The builders as they were before they moved onto the frame, to hold the new ones to.
    mod before {
        use rustc_hash::FxHashMap;

        use super::super::*;
        use super::set_bit;

        fn dense<K: DenseKey, V>(entries: impl IntoIterator<Item = (K, V)>) -> Vec<Option<V>> {
            let mut dense: Vec<Option<V>> = Vec::new();
            for (key, value) in entries {
                let index = key.to_index();
                if index >= dense.len() {
                    dense.resize_with(index + 1, || None);
                }
                dense[index] = Some(value);
            }
            dense
        }

        pub(super) fn ty_column<K: DenseKey>(entries: impl IntoIterator<Item = (K, StoredTy)>) -> TyColumn<K> {
            let mut by_ty: FxHashMap<StoredTy, u32> = FxHashMap::default();
            let mut tys = Vec::new();
            let at = dense(entries)
                .into_iter()
                .map(|ty| match ty {
                    None => NONE,
                    Some(ty) => *by_ty.entry(ty).or_insert_with_key(|ty| {
                        tys.push(*ty);
                        place(tys.len() - 1)
                    }),
                })
                .collect();
            TyColumn { at, tys: tys.into_boxed_slice(), key: PhantomData }
        }

        pub(super) fn index_column<K: DenseKey, V>(entries: impl IntoIterator<Item = (K, V)>) -> IndexColumn<K, V> {
            let (mut words, mut rows) = (Vec::new(), Vec::new());
            for (index, row) in dense(entries).into_iter().enumerate() {
                if let Some(row) = row {
                    set_bit(&mut words, index);
                    rows.push(row);
                }
            }
            let present = Presence::new(words).unwrap();
            IndexColumn { present, rows: rows.into_boxed_slice(), key: PhantomData }
        }

        pub(super) fn mode_column<K: DenseKey>(entries: impl IntoIterator<Item = (K, BindingMode)>) -> ModeColumn<K> {
            let codes = dense(entries).into_iter().map(|mode| mode.map_or(MODE_NONE, mode_code));
            ModeColumn { codes: codes.collect(), key: PhantomData }
        }

        pub(super) fn csr<K: DenseKey, T: Clone>(ranges: &[Range], items: &[T]) -> Csr<K, T> {
            let mut words = Vec::new();
            let mut ends = Vec::new();
            let mut next = 0usize;
            for (index, range) in ranges.iter().enumerate() {
                if range.is_absent() {
                    continue;
                }
                set_bit(&mut words, index);
                next += range.len as usize;
                ends.push(place(next));
            }
            let mut flat = Vec::with_capacity(next);
            for range in ranges.iter().filter(|range| !range.is_absent()) {
                flat.extend(items[range.slice()].iter().cloned());
            }
            let present = Presence::new(words).unwrap();
            Csr { present, ends: ends.into_boxed_slice(), items: flat.into_boxed_slice(), key: PhantomData }
        }

        pub(super) fn sorted_map<K: Ord, V>(entries: impl IntoIterator<Item = (K, V)>) -> SortedMap<K, V> {
            let mut entries: Vec<(K, V)> = entries.into_iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            SortedMap { entries: entries.into_boxed_slice() }
        }
    }

    #[test]
    fn frame_builders_make_the_columns_the_old_builders_made() {
        let types = &crate::next_solver::default_types().types;
        let palette = [types.i32, types.u8, types.bool, types.unit, types.never];
        for seed in 0..32 {
            frame::task(crate::db::INFER_TAG, |frame| {
                let mut rows = ArenaMap::default();
                let mut tys = ArenaMap::default();
                let mut modes = ArenaMap::default();
                let mut dense_rows = DenseMap::<Key, u32>::new_in(frame);
                let mut dense_tys = DenseMap::<Key, StoredTy>::new_in(frame);
                let mut dense_modes = DenseMap::<Key, BindingMode>::new_in(frame);
                // Made at more keys than are written, as inference makes them at the node count.
                dense_rows.reserve_keys(400);
                dense_tys.reserve_keys(400);
                dense_modes.reserve_keys(400);
                for (k, v) in writes(seed, 200, 300, 1000) {
                    let ty = palette[v as usize % palette.len()].store();
                    let mode = BINDING_MODES[v as usize % BINDING_MODES.len()];
                    assert_eq!(dense_rows.insert(key(k), v), rows.insert(key(k), v));
                    dense_tys.insert(key(k), ty);
                    tys.insert(key(k), ty);
                    dense_modes.insert(key(k), mode);
                    modes.insert(key(k), mode);
                }
                for k in 0..450 {
                    assert_eq!(dense_rows.get(key(k)), rows.get(key(k)));
                }
                assert!(IndexColumn::from_dense(dense_rows.slots_mut()) == before::index_column(rows));
                assert!(TyColumn::from_dense(frame, dense_tys.slots_mut()) == before::ty_column(tys));
                assert!(ModeColumn::from_dense(dense_modes.slots()) == before::mode_column(modes));

                let mut builder = CsrBuilder::<Key, u32>::new_in(frame);
                builder.reserve_keys(64);
                let mut set = FrameIdxSet::<Key>::new_in(frame);
                set.reserve_keys_for_test(700);
                let mut model_set = std::collections::BTreeSet::new();
                for (step, (k, v)) in writes(seed, 300, 40, 1000).into_iter().enumerate() {
                    match step % 3 {
                        0 => builder.insert(key(k), [v, v + 1]),
                        1 => _ = builder.remove_in_place(&key(k)),
                        _ => builder.push(key(k), v),
                    }
                    assert_eq!(set.insert(key(v % 700)), model_set.insert(v % 700));
                }
                let old = before::csr::<Key, u32>(&builder.ranges, &builder.items);
                assert!(builder.freeze() == old);
                let frozen = set.freeze();
                assert!(frozen == model_set.iter().map(|&k| key(k)).collect::<IdxSet<Key>>());
                assert_eq!(frozen.words().len(), frozen.words.capacity(), "made at its length");

                let entries: Vec<(u32, u32)> = writes(seed, 100, 1000, 1000).into_iter().collect::<BTreeMap<_, _>>().into_iter().rev().collect();
                assert!(SortedMap::from_entries_in(frame, entries.iter().copied()) == before::sorted_map(entries));
            });
        }
    }
```

and in `FrameIdxSet` (C9), for this test, a reservation like the dense tables':

```rust
    #[cfg(test)]
    fn reserve_keys_for_test(&mut self, len: usize) {
        self.words.resize(len.div_ceil(64), 0);
    }
```

(`TyColumn`, `Csr`, `IndexColumn`, `ModeColumn`, `SortedMap`, `IdxSet` compare with their derived
`PartialEq`, which is what persist's round trip and salsa's backdating compare with. The test
reserves more keys than it writes, so a trailing run of empty slots, absent ranges or zero words,
which inference's `reserve_nodes` leaves, is shown not to reach the columns.)

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-a-columns (21:28): 1, 2, 4 applied (no `set_bit` to move: the `before` builders use `BitSet::insert_growing`, as dedup's old builders did; "made at its length" checks `frozen.words.into_words()`'s capacity, `IdxSet`'s words being a `BitSet`; `reserve_keys_for_test` is `self.words.grow(len)`); 3 not needed, `var_set_lists_what_it_holds` already tests dedup's `BitSet<u32>`.

### C14. `DenseMap`: the rest of `ArenaMap`'s API (after C3's `slots_mut`)

So that `DenseMap` offers what `ArenaMap` offered every caller, not only what today's callers
use (`get_mut`, `iter` and a draining walk in key order; `ArenaMap::into_iter` is the drain).
`Debug` (C3) then walks `iter`:

```rust
    #[allow(dead_code, reason = "ArenaMap's API, which DenseMap replaces for inference")]
    pub(crate) fn get_mut(&mut self, key: K) -> Option<&mut V> {
        self.slots.get_mut(key.to_index())?.as_mut()
    }

    /// The keys with a value and their values, in key order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (K, &V)> {
        (self.slots.iter().enumerate())
            .filter_map(|(index, slot)| Some((K::from_index(index), slot.as_ref()?)))
    }

    /// The keys with a value and their values, taken out in key order: `ArenaMap::into_iter`.
    #[allow(dead_code, reason = "ArenaMap's API, which DenseMap replaces for inference")]
    pub(crate) fn drain(&mut self) -> impl Iterator<Item = (K, V)> {
        (self.slots.drain(..).enumerate())
            .filter_map(|(index, slot)| Some((K::from_index(index), slot?)))
    }
```

and C3's `Debug` body becomes `f.debug_map().entries(self.iter()).finish()`. In C13's new test,
after the `for k in 0..450` loop, add:

```rust
                assert!(dense_rows.iter().map(|(k, &v)| (k, v)).eq(rows.iter().map(|(k, &v)| (k, v))));
                *dense_rows.get_mut(key(7)).unwrap_or(&mut 0) += 1;
                if let Some(row) = rows.get_mut(key(7)) {
                    *row += 1;
                }
                let drained: Vec<(Key, u32)> = DenseMap::<Key, u32>::drain(&mut dense_rows.clone_for_test()).collect();
                assert_eq!(drained, rows.iter().map(|(k, &v)| (k, v)).collect::<Vec<_>>());
```

with, beside `reserve_keys_for_test` (C13), a test-only copy:

```rust
impl<'f, K: DenseKey, V: Clone> DenseMap<'f, K, V> {
    #[cfg(test)]
    fn clone_for_test(&self) -> Self {
        DenseMap { slots: self.slots.clone(), key: PhantomData }
    }
}
```

(the test then freezes `dense_rows`, whose `get_mut` changed key 7 as `rows`' did, against
`before::index_column(rows)`, so move the three `from_dense` assertions after these lines.)

> applied by db-mem-infer-a-columns (21:28): as written; the test also compares `contains_idx` with `ArenaMap`'s.

## A. `infer.rs`

Lines as of 2026-10-29 20:50 (UTC-7); the file moves while others edit it, so each entry quotes
its "before" code.

> forwarded to db (db-ty-next2, owner of hir-ty's inference files) (requests, pass 18; told by tmux).
> handed back by db-ty-next2 (21:20): not started by db-ty. db-mem-infer-a (lane db's applier)
> applies A0 to A13, together with G6 item 1. db-ty-d-infer2 is still editing `infer.rs` and
> has added `impl InferenceContext<'db>` headers in new files (`infer/ambiguity.rs`,
> `infer/callee.rs`), so A0 ends with `rg -n "InferenceContext<'db>" crates/hir-ty/src`
> finding nothing.
> checked by db-audit-mem (22:44): A0 to A13 applied below (db-mem-infer-a-ctx); A0's end condition holds: `rg -n "InferenceContext<'db>" crates/hir-ty/src` finds nothing.

### A0. Every `InferenceContext<'db>` in the crate, for one mechanical edit

R2. The parts' own entries (mem-db-infer-expr E0, mem-db-infer-method, mem-db-infer-closure)
give each of these with its file; this is the whole list (`rg -n "InferenceContext<'db>"
crates/hir-ty/src` on 2026-10-29), so one edit covers all files:

1. `impl<'db> InferenceContext<'db> {` becomes `impl<'db, 'f> InferenceContext<'db, 'f> {`:
   `infer.rs:1938`, `infer/fallback.rs:15`, `infer/ambiguity.rs:5`, `infer/at_error.rs:15`,
   `infer/mutability.rs:16`, `infer/opaques.rs:15` and `:66`, `infer/path.rs:30`,
   `infer/expr.rs:63`, `infer/pat.rs:242`, `infer/callee.rs:38`, `infer/op.rs:22`,
   `infer/place_op.rs:28`, `infer/coerce.rs:895`, `infer/closure.rs:51`,
   `infer/closure/analysis.rs:197`, `method_resolution.rs:139`, `method_resolution/confirm.rs:53`.
2. `&mut InferenceContext<'db>` / `&InferenceContext<'db>` in the signature of an item with no
   `'f` of its own becomes `InferenceContext<'db, '_>`: `infer.rs:3075` (the nested fn's `ctx`),
   `infer.rs:3207` (`Expectation::rvalue_hint`), `infer/coerce.rs:1294, 1317, 1339, 1456`
   (`CoerceMany`), `infer/callee.rs:548` (`DeferredCallResolution::resolve`, which mem-db-infer-expr
   E13 names with `'f` instead), `infer/cast.rs:186, 227, 344, 390, 566, 581, 599, 649`,
   `infer/closure/analysis.rs:1221, 1241, 1256, 1275, 1303, 1311`, and the `Delegate` trait and
   its impls in `infer/closure/analysis/expr_use_visitor.rs:305, 320, 328, 337, 345, 350, 361,
   366, 370, 378, 383, 387, 391, 399` (an elided lifetime in a trait method's argument is a
   late-bound parameter of the method, the same in the trait and the impls).
3. Inside an `impl<'db, 'f>`, a closure or a nested item naming the context names it with the
   `'f`: `infer/expr.rs:2177` `|this: &mut InferenceContext<'db, 'f>, ..|`.
4. A struct holding `&'a mut InferenceContext<'db>` gains `'f` after `'db`:
   `infer/coerce.rs:867` `InferenceCoercionDelegate<'a, 'db, 'f>`, `method_resolution/confirm.rs:41,
   76, 353` `ConfirmContext<'a, 'db, 'f>`, `autoderef.rs:136, 249, 257`
   `InferenceContextAutoderefCtx<'a, 'db, 'f>` (mem-db-infer-expr X3),
   `infer/closure/analysis/expr_use_visitor.rs:409, 422` `ExprUseVisitor`'s `cx`.
5. The free fn `infer_finalize` (`infer.rs:254`): A2.

> applied by db-mem-infer-a-ctx (21:36): `infer.rs`'s lines (the impl header, `infer_finalize`, the nested fn's `ctx`, `rvalue_hint`); the other files' lines are their holders' (`method_resolution.rs`, `confirm.rs`, `path.rs`, `mutability.rs`, `prelude_lints.rs` still had `InferenceContext<'db>` at 21:36).

### A1. Imports (lines 40 to 125)

Before (the lines that change):

```rust
use std::{
    cell::{OnceCell, RefCell},
```

```rust
use indexmap::IndexSet;
use la_arena::ArenaMap;
```

```rust
use rustc_hash::{FxHashMap, FxHashSet};
```

```rust
        columns::{
            Csr, CsrBuilder, ExprOrPatColumn, ExprOrPatMap, ExprOrPatSet, FieldColumn, IdxSet,
            IndexColumn, ModeColumn, SortedMap, TyColumn,
        },
```

After (`IndexSet`, `ArenaMap`, `FxHashMap` and `FxHashSet` have no other use outside `mod tests`,
which imports its own `FxHashMap`):

```rust
use std::{
    cell::{Cell, OnceCell, RefCell},
```

```rust
use blaze_mem::Frame;
```

(no `indexmap`, `la_arena` or `rustc_hash` line), and

```rust
        columns::{
            Csr, CsrBuilder, DenseMap, ExprOrPatColumn, ExprOrPatMap, ExprOrPatSet, FieldColumn,
            FrameExprOrPatSet, FrameIdxSet, FrameMap, FrameSet, IdxSet, IndexColumn, ModeColumn,
            SortedMap, TyColumn, frame_map, frame_set,
        },
```

> applied by db-mem-infer-a-ctx (21:36): also `Hasher`, `FxIndexMap` and `SmallVec` left (closure X2 removed their last uses); `ThinVec` stays (`CaptureSourceStack`).

### A2. The query functions, their frame and tags (lines 132 to 285, 1557 to 1576)

R1: `infer_query` and `infer_anon_const_query` run only inside their tracked fns, so they open
`frame::current`. `infer_query_with_inspect` is `pub` and called outside any query
(`crates/hir/src/semantics.rs:2581`, `crates/blaze-engine/src/borrowck.rs:410`): it opens
`frame::task`. Their shared body moves to `infer_body_in`, which takes the frame; the budget
tests (mem-db-infer's `tests/budget.rs`) call it inside their own `frame::task` to read that
frame's `peak()` (instead of `infer_query_with_inspect`, which now has a task of its own).

Before:

```rust
/// The entry point of type inference.
fn infer_query<'db>(db: &'db Db, def: DefWithBodyId) -> InferenceResult<'db> {
    infer_query_with_inspect(db, def, None, LoweringMode::Analysis)
}

pub fn infer_query_with_inspect<'db>(
    db: &'db Db,
    def: DefWithBodyId,
    inspect: Option<ObligationInspector<'db>>,
    lowering_mode: LoweringMode,
) -> InferenceResult<'db> {
    let _p = tracing::info_span!("infer_query").entered();
    let resolver = def.resolver(db);
    let body = Body::of(db, def);
    let mut ctx = InferenceContext::new(
        db,
        InferBodyId::DefWithBodyId(def),
        ExpressionStoreOwnerId::Body(def),
        def.generic_def(db),
        &body.store,
        resolver,
        true,
        lowering_mode,
    );
```

After (the rest of the body, from `if let Some(inspect) = inspect {` to `infer_finalize(ctx)`,
unchanged):

```rust
/// The entry point of type inference.
fn infer_query<'db>(db: &'db Db, def: DefWithBodyId) -> InferenceResult<'db> {
    blaze_mem::frame::current(|frame| infer_body_in(db, def, None, LoweringMode::Analysis, frame))
}

/// [`infer_body_in`] for a caller outside any query, in a task of its own.
pub fn infer_query_with_inspect<'db>(
    db: &'db Db,
    def: DefWithBodyId,
    inspect: Option<ObligationInspector<'db>>,
    lowering_mode: LoweringMode,
) -> InferenceResult<'db> {
    blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
        infer_body_in(db, def, inspect, lowering_mode, frame)
    })
}

/// The inference of `def`'s body, its temporaries in `frame`.
pub(crate) fn infer_body_in<'db>(
    db: &'db Db,
    def: DefWithBodyId,
    inspect: Option<ObligationInspector<'db>>,
    lowering_mode: LoweringMode,
    frame: &Frame,
) -> InferenceResult<'db> {
    let _p = tracing::info_span!("infer_query").entered();
    let resolver = def.resolver(db);
    let body = Body::of(db, def);
    let mut ctx = InferenceContext::new(
        db,
        InferBodyId::DefWithBodyId(def),
        ExpressionStoreOwnerId::Body(def),
        def.generic_def(db),
        &body.store,
        resolver,
        true,
        lowering_mode,
        frame,
    );
```

`infer_anon_const_query`, before:

```rust
fn infer_anon_const_query<'db>(
    db: &'db Db,
    def: AnonConstId<'db>,
) -> InferenceResult<'db> {
    let _p = tracing::info_span!("infer_anon_const_query").entered();
```

after: the whole body inside `blaze_mem::frame::current(|frame| { .. })`, and its
`InferenceContext::new(..)` call ends `LoweringMode::Analysis, frame,` (was
`LoweringMode::Analysis,`):

```rust
fn infer_anon_const_query<'db>(
    db: &'db Db,
    def: AnonConstId<'db>,
) -> InferenceResult<'db> {
    blaze_mem::frame::current(|frame| {
        let _p = tracing::info_span!("infer_anon_const_query").entered();
        // .. the body as it is, `frame` last in `InferenceContext::new` ..
        infer_finalize(ctx)
    })
}
```

`infer_finalize`, before:

```rust
fn infer_finalize<'db>(mut ctx: InferenceContext<'db>) -> InferenceResult<'db> {
```

```rust
    let cast_checks = std::mem::take(&mut ctx.deferred_cast_checks);
```

after (the second line is mem-db-infer-expr's X4; a frame `Vec` has no `Default`):

```rust
fn infer_finalize<'db>(mut ctx: InferenceContext<'db, '_>) -> InferenceResult<'db> {
```

```rust
    let cast_checks = std::mem::replace(&mut ctx.deferred_cast_checks, Vec::new_in(ctx.frame));
```

The tracked fns (R6), before:

```rust
#[salsa::tracked(returns(ref), cycle_result = infer_cycle_result, lru = 0)]
pub(crate) fn infer_body_query<'db>(
```

```rust
#[salsa::tracked(returns(ref), cycle_result = infer_anon_const_cycle_result, lru = 0)]
pub(crate) fn infer_anon_const_body_query<'db>(
```

after:

```rust
#[salsa::tracked(
    returns(ref),
    cycle_result = infer_cycle_result,
    lru = 0,
    mem_tag = crate::db::INFER_TAG
)]
pub(crate) fn infer_body_query<'db>(
```

```rust
#[salsa::tracked(
    returns(ref),
    cycle_result = infer_anon_const_cycle_result,
    lru = 0,
    mem_tag = crate::db::INFER_TAG
)]
pub(crate) fn infer_anon_const_body_query<'db>(
```

The cycle results (`infer_cycle_result`, `infer_anon_const_cycle_result`) build
`InferenceResult::new`, every column empty: no allocation, no frame.

> applied by db-mem-infer-a-ctx (21:36): as written, with `mem_tag = Infer` (bare variant) in place of `crate::db::INFER_TAG`; `infer_body_in(db, def, inspect, lowering_mode, frame)` as written.

### A3. `InferenceTables<'db, 'f>` (lines 1060 to 1244)

R5. Before: the struct as quoted at `pub(crate) struct InferenceTables<'db> {` (with
`#[derive(Clone, Debug)]`), its `new(error_ty: Ty<'_>)`, and `freeze`. After, the struct
(nothing clones it: `rg 'result\.clone\(\)|tables\.clone' crates/hir-ty/src/infer*` is empty):

```rust
/// What inference has written so far: [`InferenceResult`]'s tables as the body is walked, in
/// the run's frame, each written in place (dense slots, CSR lists that grow at the end of their
/// flat array, bitsets) and frozen into the result's columns by [`InferenceTables::freeze`].
#[derive(Debug)]
pub(crate) struct InferenceTables<'db, 'f> {
    method_resolutions: DenseMap<'f, ExprId, (FunctionId, StoredGenericArgs)>,
    field_resolutions: DenseMap<'f, ExprId, Either<FieldId, TupleFieldId>>,
    variant_resolutions: ExprOrPatMap<'f, VariantId>,
    assoc_resolutions: ExprOrPatMap<'f, (CandidateId, StoredGenericArgs)>,
    type_of_expr: DenseMap<'f, ExprId, StoredTy>,
    type_of_pat: DenseMap<'f, PatId, StoredTy>,
    type_of_binding: DenseMap<'f, BindingId, StoredTy>,
    type_of_type_placeholder: DenseMap<'f, TypeRefId, StoredTy>,
    type_of_opaque: FrameMap<'f, InternedOpaqueTyId<'db>, StoredTy>,
    has_errors: bool,
    nodes_with_type_mismatches: FrameExprOrPatSet<'f>,
    error_ty: StoredTy,
    expr_adjustments: CsrBuilder<'f, ExprId, Adjustment>,
    assignment_coercions: CsrBuilder<'f, ExprId, Adjustment>,
    pat_adjustments: CsrBuilder<'f, PatId, PatAdjustment>,
    binding_modes: DenseMap<'f, PatId, BindingMode>,
    skipped_ref_pats: FrameIdxSet<'f, PatId>,
    coercion_casts: FrameIdxSet<'f, ExprId>,
    closures_without_expectation: FrameIdxSet<'f, ExprId>,
    asm_sym_tys: FrameMap<'f, (ExprId, u32), StoredTy>,
    closures_data: FrameMap<'f, ExprId, ClosureData>,
}
```

`impl<'db> InferenceTables<'db> {` becomes `impl<'db, 'f> InferenceTables<'db, 'f> {`, and `new`:

```rust
    fn new(error_ty: Ty<'_>, store: &ExpressionStore, frame: &'f Frame) -> Self {
        let mut tables = InferenceTables {
            method_resolutions: DenseMap::new_in(frame),
            field_resolutions: DenseMap::new_in(frame),
            variant_resolutions: ExprOrPatMap::new_in(frame),
            assoc_resolutions: ExprOrPatMap::new_in(frame),
            type_of_expr: DenseMap::new_in(frame),
            type_of_pat: DenseMap::new_in(frame),
            type_of_binding: DenseMap::new_in(frame),
            type_of_type_placeholder: DenseMap::new_in(frame),
            type_of_opaque: frame_map(frame),
            has_errors: false,
            nodes_with_type_mismatches: FrameExprOrPatSet::new_in(frame),
            error_ty: error_ty.store(),
            expr_adjustments: CsrBuilder::new_in(frame),
            assignment_coercions: CsrBuilder::new_in(frame),
            pat_adjustments: CsrBuilder::new_in(frame),
            binding_modes: DenseMap::new_in(frame),
            skipped_ref_pats: FrameIdxSet::new_in(frame),
            coercion_casts: FrameIdxSet::new_in(frame),
            closures_without_expectation: FrameIdxSet::new_in(frame),
            asm_sym_tys: frame_map(frame),
            closures_data: frame_map(frame),
        };
        // Every expression gets a type and most an adjustment list's slot, every pattern a
        // type and a binding mode: made once at the node count, they never move in the frame.
        let (exprs, pats) = (store.expr_count(), store.pat_count());
        tables.type_of_expr.reserve_keys(exprs);
        tables.expr_adjustments.reserve_keys(exprs);
        tables.type_of_pat.reserve_keys(pats);
        tables.binding_modes.reserve_keys(pats);
        tables
    }
```

(`ExpressionStore::expr_count`/`pat_count`, `crates/hir-def/src/expr_store.rs:1607, 1613`. The
sparse tables, written for a few nodes each, grow in the frame as they are written.)

The readers from `expr_ty` to `variant_resolution_for_pat` are unchanged: `DenseMap::get`,
`CsrBuilder::get`, `FrameIdxSet::contains` and `ExprOrPatMap::get` take what `ArenaMap`,
`CsrBuilder` and `IdxSet` took.

`freeze`, before:

```rust
    /// The result's columns, every type in them resolved.
    fn freeze(
        self,
        tuple_field_access_types: Box<[StoredTys]>,
        diagnostics: Box<[InferenceDiagnostic]>,
        unreachable: Box<[Unreachable]>,
        defined_anon_consts: Box<[AnonConstId<'db>]>,
    ) -> InferenceResult<'db> {
```

through its end. After:

```rust
    /// The result's columns, every type in them resolved, each made once at its length from
    /// the frame's tables (their builders' temporaries in `frame` too).
    fn freeze(
        self,
        tuple_field_access_types: Box<[StoredTys]>,
        diagnostics: Box<[InferenceDiagnostic]>,
        unreachable: Box<[Unreachable]>,
        defined_anon_consts: Box<[AnonConstId<'db>]>,
        frame: &Frame,
    ) -> InferenceResult<'db> {
        let InferenceTables {
            mut method_resolutions,
            field_resolutions,
            variant_resolutions,
            assoc_resolutions,
            mut type_of_expr,
            mut type_of_pat,
            mut type_of_binding,
            mut type_of_type_placeholder,
            type_of_opaque,
            has_errors,
            nodes_with_type_mismatches,
            error_ty,
            expr_adjustments,
            assignment_coercions,
            pat_adjustments,
            binding_modes,
            skipped_ref_pats,
            coercion_casts,
            closures_without_expectation,
            asm_sym_tys,
            closures_data,
        } = self;
        InferenceResult {
            method_resolutions: IndexColumn::from_dense(method_resolutions.slots_mut()),
            field_resolutions: FieldColumn::from_dense(field_resolutions.slots()),
            variant_resolutions: variant_resolutions.freeze(),
            assoc_resolutions: assoc_resolutions.freeze(),
            tuple_field_access_types,
            type_of_expr: TyColumn::from_dense(frame, type_of_expr.slots_mut()),
            type_of_pat: TyColumn::from_dense(frame, type_of_pat.slots_mut()),
            type_of_binding: TyColumn::from_dense(frame, type_of_binding.slots_mut()),
            type_of_type_placeholder: TyColumn::from_dense(frame, type_of_type_placeholder.slots_mut()),
            type_of_opaque: SortedMap::from_entries_in(frame, type_of_opaque),
            has_errors,
            diagnostics,
            unreachable,
            nodes_with_type_mismatches: nodes_with_type_mismatches.freeze(),
            error_ty,
            expr_adjustments: expr_adjustments.freeze(),
            assignment_coercions: assignment_coercions.freeze(),
            pat_adjustments: pat_adjustments.freeze(),
            binding_modes: ModeColumn::from_dense(binding_modes.slots()),
            skipped_ref_pats: skipped_ref_pats.freeze(),
            coercion_casts: coercion_casts.freeze(),
            closures_without_expectation: closures_without_expectation.freeze(),
            asm_sym_tys: SortedMap::from_entries_in(frame, asm_sym_tys),
            closures_data: SortedMap::from_entries_in(frame, closures_data),
            defined_anon_consts,
        }
    }
```

(The five `shrink_to_fit` calls are gone: the `FrameIdxSet`s' `freeze` copies their words once at
their length, C9.)

> applied by db-mem-infer-a-ctx (21:36): plus the new table `unstable_candidates` (`ExprOrPatMap<'f, Box<[(CandidateId, Symbol)]>>`, `ExprOrPatMap::new_in(frame)`, frozen as before), which the file gained after A3 was written.

### A4. `InferenceContext<'db, 'f>` and `BreakableContext<'db, 'f>` (lines 1838 to 1936)

Before: the struct from `pub(crate) struct InferenceContext<'db> {` to
`defined_anon_consts: RefCell<ThinVec<AnonConstId<'db>>>,` and `struct BreakableContext<'db>`.
After, the fields that change (the others as they are):

```rust
#[derive(Debug)]
pub(crate) struct InferenceContext<'db, 'f> {
    // .. `db` to `features` unchanged ..
    /// The run's frame (docs/v2/plan/lanes/mem/db-infer.md, R1): `result`'s tables and every
    /// temporary below live in it until the result is frozen.
    pub(crate) frame: &'f Frame,
    /// The traits in scope, disregarding block modules. This is used for caching purposes.
    traits_in_scope: FrameSet<'f, TraitId>,
    /// The traits in scope in the innermost block module the walk was last in: which module,
    /// and `traits_in_scope` together with the block's (`None` when the block adds none).
    block_traits: Cell<(Option<hir_def::ModuleId>, Option<&'f FrameSet<'f, TraitId>>)>,
    pub(crate) result: InferenceTables<'db, 'f>,
    tuple_field_accesses_rev: TupleFieldTypes<'db, 'f>,
    // .. `return_ty` unchanged ..
    return_coercion: Option<DynamicCoerceMany<'db, 'f>>,
    // .. `resume_yield_tys` to `diverges` unchanged ..
    /// What [`InferenceContext::warn_if_unreachable`] found, empty (and unallocated) unless
    /// something diverged before more code.
    unreachable: Vec<Unreachable, &'f Frame>,
    breakables: Vec<BreakableContext<'db, 'f>, &'f Frame>,
    types: &'db crate::next_solver::DefaultAny<'db>,

    deferred_cast_checks: Vec<CastCheck<'db>, &'f Frame>,

    /// Each call of a closure whose kind is not known yet, by the expression defining the
    /// closure or coroutine closure, in the order they were recorded.
    deferred_call_resolutions: Vec<(ExprId, DeferredCallResolution<'db, 'f>), &'f Frame>,

    diagnostics: Diagnostics,
    vars_emitted_type_must_be_known_for: FrameSet<'f, Term<'db>>,

    defined_anon_consts: RefCell<Vec<AnonConstId<'db>, &'f Frame>>,
}

#[derive(Clone, Debug)]
struct BreakableContext<'db, 'f> {
    /// Whether this context contains at least one break expression.
    may_break: bool,
    /// The coercion target of the context.
    coerce: Option<DynamicCoerceMany<'db, 'f>>,
    /// The optional label of the context.
    label: Option<LabelId>,
    kind: BreakableKind,
}
```

(`Frame` is `Debug`, `crates/blaze-mem/src/frame.rs:383`; `Vec<T, &Frame>` and hashbrown's maps are
`Debug` for any allocator; `Cell<T>: Debug` needs `T: Copy`, which `ModuleId` (a tracked
struct) and `Option<&_>` are. `find_breakable`/`find_continuable` take `&[BreakableContext<'_, '_>]`,
X4. `diagnostics` keeps its type: B2 says why.)

`tuple_field_accesses_rev` was an `IndexSet<Tys<'db>, FxBuildHasher>`; R3's form of it, added
after `BreakableKind`:

```rust
/// The tuple types of the body's tuple field accesses, each once, by their place: the values in
/// insertion order and their places by value, an `IndexSet` in the frame (R3).
#[derive(Debug)]
pub(crate) struct TupleFieldTypes<'db, 'f> {
    tys: Vec<Tys<'db>, &'f Frame>,
    places: FrameMap<'f, Tys<'db>, u32>,
}

impl<'db, 'f> TupleFieldTypes<'db, 'f> {
    fn new_in(frame: &'f Frame) -> Self {
        TupleFieldTypes { tys: Vec::new_in(frame), places: frame_map(frame) }
    }

    /// The place of `tys`, which is added at the end if it is new: `IndexSet::insert_full`'s
    /// index.
    pub(crate) fn insert(&mut self, tys: Tys<'db>) -> u32 {
        let TupleFieldTypes { tys: list, places } = self;
        *places.entry(tys).or_insert_with(|| {
            let place = u32::try_from(list.len()).expect("2^32 tuple types in a body");
            list.push(tys);
            place
        })
    }
}
```

Its one caller outside this file, `infer/expr.rs:1736`
`TupleId(self.tuple_field_accesses_rev.insert_full(substs).0 as u32)` (mem-db-infer-expr's file),
becomes `TupleId(self.tuple_field_accesses_rev.insert(substs))`.

> applied by db-mem-infer-a-ctx (21:36): as written (the old `block_traits` was a `RefCell<(Option<ModuleId>, bool, Rc<FxHashSet>)>` by then; replaced the same). The `traits_in_scope`/`block_traits` fields are subject to A10's outcome.

### A5. `InferenceContext::new` (lines 1938 to 1986)

Before: the signature ending `lowering_mode: LoweringMode,\n    ) -> Self {` and the struct literal
quoted at `InferenceContext {\n            result: InferenceTables::new(types.types.error),`. After:

```rust
impl<'db, 'f> InferenceContext<'db, 'f> {
    fn new(
        db: &'db Db,
        owner: InferBodyId<'db>,
        store_owner: ExpressionStoreOwnerId,
        generic_def: GenericDefId,
        store: &'db ExpressionStore,
        resolver: Resolver<'db>,
        allow_using_generic_params: bool,
        lowering_mode: LoweringMode,
        frame: &'f Frame,
    ) -> Self {
        let trait_env = db.trait_environment(generic_def);
        let table = unify::InferenceTable::new(db, trait_env, resolver.krate(), owner);
        let types = crate::next_solver::default_types();
        let mut traits_in_scope = frame_set(frame);
        resolver.traits_in_scope_into(db, &mut traits_in_scope);
        InferenceContext {
            result: InferenceTables::new(types.types.error, store, frame),
            return_ty: types.types.error, // set in collect_* calls
            types,
            target_features: OnceCell::new(),
            lang_items: table.interner().lang_items(),
            features: resolver.top_level_def_map().features(),
            edition: resolver.krate().data(db).edition,
            table,
            tuple_field_accesses_rev: TupleFieldTypes::new_in(frame),
            resume_yield_tys: None,
            coroutine_desugaring: None,
            return_coercion: None,
            db,
            owner,
            store_owner,
            generic_def,
            allow_using_generic_params,
            generics: OnceCell::new(),
            identity_args: OnceCell::new(),
            store,
            traits_in_scope,
            block_traits: Cell::new((None, None)),
            resolver,
            diverges: Diverges::Maybe,
            unreachable: Vec::new_in(frame),
            breakables: Vec::new_in(frame),
            deferred_cast_checks: Vec::new_in(frame),
            diagnostics: Diagnostics::default(),
            vars_emitted_type_must_be_known_for: frame_set(frame),
            deferred_call_resolutions: Vec::new_in(frame),
            defined_anon_consts: RefCell::new(Vec::new_in(frame)),
            lowering_mode,
            frame,
        }
    }
```

`traits_in_scope_into` taking any `Extend<TraitId>` is entry D1 (hir-def, below); it fills the
set as `Resolver::traits_in_scope` filled its `FxHashSet` (same calls, same order).

> applied by db-mem-infer-a-ctx (21:36): as written.

### A6. `merge` (lines 1988 to 2082)

The body is unchanged but for the tuple place and the three helpers' `dest` types. Before:

```rust
                tuple_field.tuple =
                    TupleId(self.tuple_field_accesses_rev.insert_full(tys).0 as u32);
```

```rust
        fn merge_index<'a, K, V: Clone + PartialEq + 'a>(
            dest: &mut ArenaMap<la_arena::Idx<K>, V>,
            source: impl Iterator<Item = (la_arena::Idx<K>, &'a V)>,
        ) {
```

```rust
        fn merge_map<'a, K: Hash + Eq + Clone + 'a, V: Clone + PartialEq + 'a>(
            dest: &mut FxHashMap<K, V>,
            source: impl Iterator<Item = (&'a K, &'a V)>,
        ) {
            for (key, value) in source {
                debug_assert!(dest.get(key).is_none_or(|it| it == value));
                dest.insert(key.clone(), value.clone());
            }
        }
```

```rust
            dest: &mut CsrBuilder<K, T>,
```

After:

```rust
                tuple_field.tuple = TupleId(self.tuple_field_accesses_rev.insert(tys));
```

```rust
        fn merge_index<'a, K: columns::DenseKey, V: Clone + PartialEq + 'a>(
            dest: &mut DenseMap<'_, K, V>,
            source: impl Iterator<Item = (K, &'a V)>,
        ) {
```

```rust
        fn merge_map<'a, K: Hash + Eq + Clone + 'a, V: Clone + PartialEq + 'a>(
            dest: &mut FrameMap<'_, K, V>,
            source: impl Iterator<Item = (&'a K, &'a V)>,
        ) {
            for (key, value) in source {
                debug_assert!(dest.get(key).is_none_or(|it| it == value));
                // clone: an inline const's entries, owned by its own result and by the body's
                dest.insert(key.clone(), value.clone());
            }
        }
```

```rust
            dest: &mut CsrBuilder<'_, K, T>,
```

(`merge_index`'s body is unchanged: `dest.get(key)`, `dest.insert(key, value.clone())`; with the
key types `ExprId`, `PatId`, `BindingId`, `TypeRefId` themselves `DenseKey`s, which is what
`IndexColumn::iter`/`TyColumn::iter` yield. The `skipped_ref_pats.extend(..)`,
`coercion_casts.extend(..)`, `closures_without_expectation.extend(..)` and
`nodes_with_type_mismatches.insert(node)` lines compile against `FrameIdxSet`'s `Extend` and
`FrameExprOrPatSet::insert`, C9. `self.defined_anon_consts.borrow_mut().extend(..)` and
`self.unreachable.extend(..)` are frame `Vec`s now and unchanged.)

> applied by db-mem-infer-a-ctx (21:36): as written; the new `unstable_candidates` loop compiles as it is.

### A7. `merge_anon_consts` (lines 2113 to 2127)

Before:

```rust
        let mut defined_anon_consts = std::mem::take(&mut *self.defined_anon_consts.borrow_mut());
```

After (the rest unchanged; `append` moves between two frame `Vec`s):

```rust
        let mut defined_anon_consts =
            std::mem::replace(&mut *self.defined_anon_consts.borrow_mut(), Vec::new_in(self.frame));
```

> applied by db-mem-infer-a-ctx (21:36): as written.

### A8. `resolve_all` (lines 2129 to 2266)

Before, the lines that change:

```rust
        let InferenceContext {
            table,
            mut result,
            tuple_field_accesses_rev,
            diagnostics,
            types,
            vars_emitted_type_must_be_known_for,
            defined_anon_consts,
            mut unreachable,
            db,
            owner,
            ..
        } = self;
        let diagnostics = diagnostics.finish();
        if !unreachable.is_empty() && in_automatically_derived_impl(db, owner) {
            unreachable = Vec::new();
        }
```

```rust
        let defined_anon_consts = defined_anon_consts.into_inner();

        let mut resolver =
            WriteBackCtxt::new(table, diagnostics, vars_emitted_type_must_be_known_for);
```

```rust
        let tuple_field_access_types: Box<[StoredTys]> = tuple_field_accesses_rev
            .into_iter()
            .map(|mut subst| {
                resolver.resolve_completely(&mut subst);
                subst.store()
            })
            .collect();

        let (diagnostics, resolver_has_errors) = resolver.resolve_diagnostics();
        *has_errors |= resolver_has_errors;

        result.freeze(
            tuple_field_access_types,
            diagnostics.into_iter().collect(),
            unreachable.into_boxed_slice(),
            defined_anon_consts.into_iter().collect(),
        )
```

After:

```rust
        let InferenceContext {
            table,
            mut result,
            tuple_field_accesses_rev,
            diagnostics,
            types,
            vars_emitted_type_must_be_known_for,
            defined_anon_consts,
            mut unreachable,
            db,
            owner,
            frame,
            ..
        } = self;
        let diagnostics = diagnostics.finish();
        if !unreachable.is_empty() && in_automatically_derived_impl(db, owner) {
            unreachable.clear();
        }
```

```rust
        let mut defined_anon_consts = defined_anon_consts.into_inner();

        let mut resolver =
            WriteBackCtxt::new(table, diagnostics, vars_emitted_type_must_be_known_for, frame);
```

```rust
        // A slice's iterator is `TrustedLen`: the `Box` is made once at its length.
        let tuple_field_access_types: Box<[StoredTys]> = tuple_field_accesses_rev
            .tys
            .iter()
            .map(|&subst| {
                let mut subst = subst;
                resolver.resolve_completely(&mut subst);
                subst.store()
            })
            .collect();

        let (diagnostics, resolver_has_errors) = resolver.resolve_diagnostics();
        *has_errors |= resolver_has_errors;

        // A `Vec`'s drain is `TrustedLen`: each `Box` is made once at its length, none when empty.
        result.freeze(
            tuple_field_access_types,
            diagnostics,
            unreachable.drain(..).collect(),
            defined_anon_consts.drain(..).collect(),
            frame,
        )
```

(`resolve_diagnostics` returns the result's `Box<[InferenceDiagnostic]>` itself, U6. The
destructuring of `InferenceTables` and the `values_mut` loops are unchanged: `DenseMap::values_mut`
and `CsrBuilder::values_mut` are the same iterators.)

The closure data's `shrink_to_fit` calls (`place.projections.shrink_to_fit()`,
`source.shrink_to_fit()`, `sources.shrink_to_fit()`, `min_capture.shrink_to_fit()`,
`min_captures.shrink_to_fit()`) cut `ClosureData`'s own lists, which closure analysis builds
(mem-db-infer-closure's files) and the result keeps. They stay here exactly as long as those
lists are built by pushing; when mem-db-infer-closure's entries build each at its length, its
entry deletes these lines (with `dummy_place`'s `projections: Vec::new()`, which allocates
nothing). Told to mem-db-infer below.

> applied by db-mem-infer-a-ctx (21:36): as written, then closure X3 over the closure-data loop (mem-db-infer.md section 6 item 2): the `shrink_to_fit` lines are gone, `dummy_place` has `projections: Box::default()`.

### A9. `with_ty_lowering` and `resolve_variant` (lines 2445 to 2465, 2750 to 2775)

R2 and mem-db-infer-lower's table (its `infer/diagnostics.rs` rows): the lowering context takes
the frame last, and the vars context and the inference lowering context take the `'f` of the
tables and the anon-const list they hold (section B). Before:

```rust
    fn with_ty_lowering<R>(&mut self, f: impl FnOnce(&mut TyLoweringContext<'db, '_>) -> R) -> R {
```

```rust
            &mut infer_vars,
            &self.defined_anon_consts,
            LifetimeLoweringMode::LateParam,
        );
        f(&mut ctx)
```

After:

```rust
    fn with_ty_lowering<R>(
        &mut self,
        f: impl FnOnce(&mut TyLoweringContext<'db, '_, 'f>) -> R,
    ) -> R {
```

```rust
            &mut infer_vars,
            &self.defined_anon_consts,
            LifetimeLoweringMode::LateParam,
            self.frame,
        );
        f(&mut ctx)
```

and in `resolve_variant`, `&mut vars_ctx,\n            &self.defined_anon_consts,\n
LifetimeLoweringMode::LateParam,\n        );` gains `self.frame,` after
`LifetimeLoweringMode::LateParam,` the same way. (`TyLoweringContext` here is the
`InferenceTyLoweringContext as TyLoweringContext` import; the closures callers pass infer the
lifetimes.)

> applied by db-mem-infer-a-ctx (21:36): as written (`resolve_variant` takes `path: Path<'_>` now, db-apply-ty's; the call's new last argument is the same).

### A10. `get_traits_in_scope` (lines 3107 to 3131)

The cache was an `Rc<FxHashSet>` refilled in place and cloned per method call. After: each block
module that adds traits gets its set once, in the frame, and every method call inside it borrows
it; the cell holds the module and the set's reference (`Copy`, so no `RefCell` borrow is held
while the probe runs). Before:

```rust
    /// Filled once each time the walk enters a block module that adds traits; every method call
    /// inside it then takes an `Rc` clone, with no allocation or hashing.
    pub(crate) fn get_traits_in_scope(
        &self,
    ) -> Either<std::rc::Rc<FxHashSet<TraitId>>, &FxHashSet<TraitId>> {
```

to the end of the function. After:

```rust
    /// Filled once each time the walk enters a block module that adds traits, in the frame;
    /// every method call inside it then borrows it, with no allocation or hashing.
    pub(crate) fn get_traits_in_scope(&self) -> &FrameSet<'f, TraitId> {
        let module = self.resolver.module();
        let (key, traits) = self.block_traits.get();
        if key == Some(module) {
            return traits.unwrap_or(&self.traits_in_scope);
        }
        let mut block_traits = self.resolver.traits_in_scope_from_block_scopes(self.db).peekable();
        let traits = match block_traits.peek() {
            None => None,
            Some(_) => {
                let mut set = frame_set(self.frame);
                set.reserve(self.traits_in_scope.len());
                set.extend(self.traits_in_scope.iter().copied());
                set.extend(block_traits);
                Some(&*self.frame.alloc(set))
            }
        };
        self.block_traits.set((Some(module), traits));
        traits.unwrap_or(&self.traits_in_scope)
    }
```

(`Frame::alloc` gives `&'f mut FrameSet` from `self.frame: &'f Frame`; the set is never dropped,
which frees nothing a frame reset does not. Walking back into a module seen before refills it:
one set per switch into a block module with `use Trait`, bounded by the body's blocks, as the old
refill was. The probe iterates the set: a fresh set's order is hashbrown's for these keys, as the
reused `Rc` set's was, which already varied with what the previous block held; only the order of
an ambiguity's candidate sources depends on it.) Its caller, `method_resolution.rs:194`
`with_method_resolution`, is entry M1.

> superseded, applied otherwise by db-mem-infer-a-ctx (21:45): frontend-fast-method's `TraitsInScope` (`method_resolution/scope_sets.rs`, docs/v2/plan/lanes/frontend/fast-method.md section 7) replaces the body's set: `with_method_resolution` passes `TraitsInScope::of_resolver(self.db, &self.resolver)` (written by frontend-fast-method), so `infer.rs`'s `traits_in_scope`, `block_traits` and `get_traits_in_scope` are deleted (A4 and A5's lines for them with it, and `Cell`'s import). `sized_bound.rs`' all-traits probe moves to `TraitsInScope::Set(TraitSet::new(..))` (handed to db-mem-infer-a-small).

> superseded, applied otherwise by db-mem-infer-a-ctx (21:45): frontend-fast-method's `TraitsInScope` (`method_resolution/scope_sets.rs`, docs/v2/plan/lanes/frontend/fast-method.md section 7) replaces the body's set: `with_method_resolution` passes `TraitsInScope::of_resolver(self.db, &self.resolver)` (written by frontend-fast-method), so `infer.rs`'s `traits_in_scope`, `block_traits` and `get_traits_in_scope` are deleted (A4 and A5's lines for them with it, and `Cell`'s import). `sized_bound.rs`' all-traits probe moves to `TraitsInScope::Set(TraitSet::new(..))` (handed to db-mem-infer-a-small).

### A11. The two context helpers of mem-db-infer-expr's X1 and X4

Taken as written there: `write_expr_adj(&mut self, expr: ExprId, adjustments: &[Adjustment])`
(X1), `record_deferred_call_resolution(&mut self, closure_def_id: ExprId, r:
DeferredCallResolution<'db, 'f>)` and `remove_deferred_call_resolutions` returning
`Vec<DeferredCallResolution<'db, 'f>, &'f Frame>` (X4).

> applied by db-mem-infer-a-ctx (21:36): `write_expr_adj` (X1), `record_deferred_call_resolution` and `remove_deferred_call_resolutions` (X4) as written there.

### A12. Tests (`mod tests` at the end of `infer.rs`)

Add, after the existing tests:

```rust
    #[test]
    fn tuple_field_types_place_tuples_as_an_index_set_does() {
        let types = &crate::next_solver::default_types().types;
        let lists = [
            Tys::new_from_slice(&[types.i32]),
            Tys::new_from_slice(&[types.u8, types.bool]),
            Tys::new_from_slice(&[]),
            Tys::new_from_slice(&[types.unit, types.never, types.i32]),
        ];
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
            let mut ours = TupleFieldTypes::new_in(frame);
            let mut theirs = indexmap::IndexSet::<Tys<'_>, rustc_hash::FxBuildHasher>::default();
            for step in 0..64usize {
                let tys = lists[(step * 7 + step / 5) % lists.len()];
                assert_eq!(ours.insert(tys) as usize, theirs.insert_full(tys).0, "step {step}");
            }
            assert!(ours.tys.iter().eq(theirs.iter()), "the same tuples in the same order");
        });
    }
```

(`indexmap` and `rustc-hash` are hir-ty dependencies already.) The semantics of every other
change here (tables, freeze, traits in scope, write-back) are held by hir-ty's inference tests
(`crates/hir-ty/src/tests/`) and persist's round trips, which compare whole results; C13's test
holds the columns byte for byte against the old builders.

> applied by db-mem-infer-a-ctx (21:36): as written, with closure X2's `min_captures_split_their_block_by_root` in the same module (`use super::*;`), in place of the two `hash_unordered` tests.

### A13. Since A was written: `value_resolutions` and `infer/sized_bound.rs`

Lines as of 2026-10-29 21:00 (UTC-7).

**`value_resolutions`.** Lane db added `value_resolutions: ExprOrPatMap<ValueNs>` to
`InferenceTables` (and `ExprOrPatColumn<ValueNs>` to `InferenceResult`) after A3 was written. In
A3's struct, after `variant_resolutions: ExprOrPatMap<'f, VariantId>,`:

```rust
    value_resolutions: ExprOrPatMap<'f, ValueNs>,
```

in A3's `new`, after `variant_resolutions: ExprOrPatMap::new_in(frame),`:

```rust
            value_resolutions: ExprOrPatMap::new_in(frame),
```

and in A3's `freeze`, `value_resolutions,` after `variant_resolutions,` in the destructuring and
`value_resolutions: value_resolutions.freeze(),` after `variant_resolutions: variant_resolutions.freeze(),`
in the literal. `merge`'s loop (`for (node, value) in other.value_resolutions.iter() { ..
value_resolutions.insert(node, *value); }`), `write_value_resolution` and `resolve_all`'s
`value_resolutions: _,` compile as they are: C6 keeps `ExprOrPatMap`'s `get`, `insert` and `iter`.

**`infer/sized_bound.rs`** (new, untracked, in no part's list; it calls A10's
`get_traits_in_scope` and builds a `MethodResolutionContext`, so it follows A10, M1 and
mem-db-infer-method's P1 here).

1. Line 32, `impl<'db> InferenceContext<'db> {` becomes `impl<'db, 'f> InferenceContext<'db, 'f> {` (A0).
2. Imports, before:

```rust
use base_db::all_crates;
use either::Either;
use hir_def::{
    FunctionId, GenericDefId, ItemContainerId, Lookup, ModuleDefId, TraitId, hir::ExprId,
    nameres::crate_def_map, signatures::FunctionSignature,
};
use hir_expand::name::Name;
use rustc_hash::FxHashSet;
```

```rust
    method_resolution::{
        CandidateId, CandidateSource, MethodError, MethodResolutionContext, Mode, Pick,
        trait_value_item,
    },
```

after (`Either`, `TraitId` and `FxHashSet` have no other use in the file):

```rust
use base_db::all_crates;
use hir_def::{
    FunctionId, GenericDefId, ItemContainerId, Lookup, ModuleDefId, hir::ExprId,
    nameres::crate_def_map, signatures::FunctionSignature,
};
use hir_expand::name::Name;
```

```rust
    method_resolution::{
        CandidateId, CandidateSource, MethodError, MethodResolutionContext, Mode, Pick,
        TraitsInScope, trait_value_item,
    },
```

3. `probe_all_traits`, before:

```rust
        let db = self.db;
        // alloc: the error path's set of every trait with an item of the name.
        let mut traits: FxHashSet<TraitId> = match self.get_traits_in_scope() {
            Either::Left(it) => (*it).clone(),
            Either::Right(it) => it.clone(),
        };
```

```rust
            traits_in_scope: &traits,
            edition: self.edition,
            features: self.features,
            call_span: call_expr.into(),
            receiver_span: receiver.into(),
        };
```

after (the `for &krate in all_crates(db)` loop unchanged):

```rust
        let db = self.db;
        // clone: the error path's set of every trait with an item of the name, in the frame. A
        // clone keeps the set's table, so the probe walks the traits in scope as before.
        let mut traits = self.get_traits_in_scope().clone();
```

```rust
            traits_in_scope: TraitsInScope::Inference(&traits),
            edition: self.edition,
            features: self.features,
            call_span: call_expr.into(),
            receiver_span: receiver.into(),
            frame: self.frame,
        };
```

(hashbrown's `HashSet<T, S, A>: Clone` for `A: Allocator + Clone`, which `&Frame` is: the clone is
in the frame. `push_illegal_sized_bound`'s `candidates` `collect()` and `Box::from([it])` are the
diagnostic's own list, on an error path: unchanged.)

> applied by db-mem-infer-a-small (21:26): `infer/sized_bound.rs` items 1 to 3, with one difference: `TraitId` and `AssocItemId` stay imported (the file's new `has_value_item` closure uses them); `Either` and `FxHashSet` go. Compiles once A10 (`get_traits_in_scope` returning the frame set), M1 (`TraitsInScope`) and method P1 (`MethodResolutionContext::frame`) land.
> superseded for item 3 (db-mem-infer-a-small, 21:50): frontend-fast-method's `TraitsInScope::Set(TraitSet::new(&mut traits))` (section at the end of this file) replaced the frame-set clone and `TraitsInScope::Inference`; `frame: self.frame` stays. The file no longer calls `get_traits_in_scope`.

> applied by db-mem-infer-a-ctx (21:36): the `value_resolutions` part was already in A3's form (the file had it); `infer/sized_bound.rs` is db-mem-infer-a-small's.

## B. `infer/diagnostics.rs`

Lines as of 2026-10-29 21:00 (UTC-7). R2 and mem-db-infer-lower's section 22 (its two
`infer/diagnostics.rs` rows, taken as written there): the lowering context takes the frame last,
and its `diagnostics` and `defined_anon_consts` are frame lists (`Vec<_, &'a Frame>`, lower's
entry for `TyLoweringContext`'s fields). The two structs here hold the inference's frame tables
by `&mut`/`&RefCell`, both invariant, so they gain the `'f` of those tables.

> forwarded to db (db-ty-next2) (requests, pass 20; told by tmux).
> applied by db-mem-infer-a-small (21:47): B1 to B4, on the tree as dedyn-ty left it (the `TyLoweringInferVarsCtx` impl gone, `with_inference_vars` concrete) and with `v2/requests/db-ty.md`'s dedyn outcome (db-ty-next3): `InferenceTyLoweringContext` also keeps `type_of_type_placeholder: &'a mut DenseMap<'f, TypeRefId, StoredTy>`, taken from the vars context in `new` (which calls `.with_inference_vars(&mut **table)`), and its `Drop` drains `ctx.type_placeholders` into it. `blaze_mem::Frame` imported after `base_db` (alphabetical). Needs from `lower.rs` (db-mem-infer-b-lower, told): `with_inference_vars(self, table: &'a mut InferenceTable<'db>)` and the `type_placeholders` frame list.

### B1. Imports (lines 17 to 31)

Before:

```rust
use la_arena::{ArenaMap, RawIdx};
use thin_vec::ThinVec;
```

```rust
    infer::unify::InferenceTable,
```

After:

```rust
use blaze_mem::Frame;
use la_arena::RawIdx;
use thin_vec::ThinVec;
```

```rust
    infer::{columns::DenseMap, unify::InferenceTable},
```

### B2. `Diagnostics` keeps its `ThinVec` (lines 33 to 62)

The type is unchanged, and so is every method but `push_ty_diagnostics`. Why it stays global: it
is empty, so unallocated, in a body without a diagnostic (an error path), and a frame lifetime on
it would reach `PathDiagnosticCallbackData<'a>` (line 64) and through it `lower/path.rs`'s
`PathDiagnosticCallback<'a, 'db>`, whose `data: CallbackData<'a>` sits under `&mut` in every
`PathLoweringContext` (invariant): a lifetime on lowering's path API for an error-path list.
`resolve_diagnostics` (U6) copies it once into the result at its length.

`push_ty_diagnostics`, before:

```rust
    fn push_ty_diagnostics(&self, diagnostics: ThinVec<TyLoweringDiagnostic>) {
        self.0
            .borrow_mut()
            .extend(diagnostics.into_iter().map(|diag| InferenceDiagnostic::TyDiagnostic { diag }));
    }
```

after (lower's section 22; a lowering with no diagnostic, nearly every one, borrows nothing):

```rust
    fn push_ty_diagnostics(&self, diagnostics: impl ExactSizeIterator<Item = TyLoweringDiagnostic>) {
        if diagnostics.len() == 0 {
            return;
        }
        self.0.borrow_mut().extend(diagnostics.map(|diag| InferenceDiagnostic::TyDiagnostic { diag }));
    }
```

(`Vec::drain` is `ExactSizeIterator`.) `extend` (line 45, cloning an inline const's diagnostics
into the body's) stays: an error path, `// clone:` the diagnostics an inline const's own result
keeps too.

### B3. `InferenceTyLoweringVarsCtx<'a, 'db, 'f>` (lines 69 to 94)

Before:

```rust
pub(super) struct InferenceTyLoweringVarsCtx<'a, 'db> {
    pub(super) table: &'a mut InferenceTable<'db>,
    pub(super) type_of_type_placeholder: &'a mut ArenaMap<TypeRefId, StoredTy>,
}

impl<'db> TyLoweringInferVarsCtx<'db> for InferenceTyLoweringVarsCtx<'_, 'db> {
```

After (the methods unchanged: `DenseMap::insert` takes the key by value as `ArenaMap::insert` did):

```rust
pub(super) struct InferenceTyLoweringVarsCtx<'a, 'db, 'f> {
    pub(super) table: &'a mut InferenceTable<'db>,
    pub(super) type_of_type_placeholder: &'a mut DenseMap<'f, TypeRefId, StoredTy>,
}

impl<'db> TyLoweringInferVarsCtx<'db> for InferenceTyLoweringVarsCtx<'_, 'db, '_> {
```

Its three literals (`infer.rs` `with_ty_lowering` and `resolve_variant`, `infer/path.rs:155`)
are unchanged: `&mut self.result.type_of_type_placeholder` is the `DenseMap` of A3.

### B4. `InferenceTyLoweringContext<'db, 'a, 'f>` (lines 96 to 195)

Before:

```rust
pub(super) struct InferenceTyLoweringContext<'db, 'a> {
    ctx: TyLoweringContext<'db, 'a>,
    diagnostics: &'a Diagnostics,
    defined_anon_consts: &'a RefCell<ThinVec<AnonConstId<'db>>>,
}

impl<'db, 'a> InferenceTyLoweringContext<'db, 'a> {
    #[inline]
    pub(super) fn new(
```

```rust
        infer_vars: &'a mut InferenceTyLoweringVarsCtx<'a, 'db>,
        defined_anon_consts: &'a RefCell<ThinVec<AnonConstId<'db>>>,
        lifetime_lowering_mode: LifetimeLoweringMode,
    ) -> Self {
        let mut ctx = TyLoweringContext::new(
            db,
            resolver,
            store,
            def,
            generic_def,
            generics,
            lifetime_elision,
            lifetime_lowering_mode,
        )
```

After (`at_path`, `at_path_forget_diagnostics` and `forget_diagnostics` unchanged; they return
`PathLoweringContext<'b, 'a, 'db>` as before):

```rust
pub(super) struct InferenceTyLoweringContext<'db, 'a, 'f> {
    ctx: TyLoweringContext<'db, 'a>,
    diagnostics: &'a Diagnostics,
    defined_anon_consts: &'a RefCell<Vec<AnonConstId<'db>, &'f Frame>>,
}

impl<'db, 'a, 'f> InferenceTyLoweringContext<'db, 'a, 'f> {
    #[inline]
    pub(super) fn new(
```

```rust
        infer_vars: &'a mut InferenceTyLoweringVarsCtx<'a, 'db, 'f>,
        defined_anon_consts: &'a RefCell<Vec<AnonConstId<'db>, &'f Frame>>,
        lifetime_lowering_mode: LifetimeLoweringMode,
        frame: &'a Frame,
    ) -> Self {
        let mut ctx = TyLoweringContext::new(
            db,
            resolver,
            store,
            def,
            generic_def,
            generics,
            lifetime_elision,
            lifetime_lowering_mode,
            frame,
        )
```

(`Some(infer_vars)` still coerces to `Option<&'a mut dyn TyLoweringInferVarsCtx<'db>>`: the vars
context outlives `'a`, since `'f: 'a` and `'db: 'a` are implied by the parameter types. The
callers pass `self.frame`, `&'f Frame`, which coerces to `&'a Frame`. That `dyn` is
`lower.rs:274`'s, which lane dedyn may make concrete; if it does, B4 passes `Some(infer_vars)` to
whatever type it chose, and nothing else here changes. This part adds no `dyn`.)

The trait impls, before:

```rust
impl<'db, 'a> Deref for InferenceTyLoweringContext<'db, 'a> {
```

```rust
impl DerefMut for InferenceTyLoweringContext<'_, '_> {
```

```rust
impl Drop for InferenceTyLoweringContext<'_, '_> {
    #[inline]
    fn drop(&mut self) {
        self.diagnostics.push_ty_diagnostics(std::mem::take(&mut self.ctx.diagnostics));
        self.defined_anon_consts.borrow_mut().extend(self.ctx.defined_anon_consts.iter().copied());
    }
}
```

after (`Deref::Target` stays `TyLoweringContext<'db, 'a>`):

```rust
impl<'db, 'a> Deref for InferenceTyLoweringContext<'db, 'a, '_> {
```

```rust
impl DerefMut for InferenceTyLoweringContext<'_, '_, '_> {
```

```rust
impl Drop for InferenceTyLoweringContext<'_, '_, '_> {
    #[inline]
    fn drop(&mut self) {
        self.diagnostics.push_ty_diagnostics(self.ctx.diagnostics.drain(..));
        let anon_consts = &mut self.ctx.defined_anon_consts;
        if !anon_consts.is_empty() {
            self.defined_anon_consts.borrow_mut().extend(anon_consts.drain(..));
        }
    }
}
```

The callers: `infer.rs` `with_ty_lowering` and `resolve_variant` (A9, whose
`TyLoweringContext<'db, '_, 'f>` is this type through the `as TyLoweringContext` import) and
`infer/path.rs` (G4).

## U. `infer/unify.rs`

Lines as of 2026-10-29 21:00 (UTC-7). `InferenceTable<'db>` keeps no frame lifetime (R2): it is a
field of `InferenceContext` and is moved into `WriteBackCtxt`, and its only buffers of its own
are an error-path list and the diverging set.

> forwarded to db (db-ty-next2) (requests, pass 20; told by tmux).
> applied by db-mem-infer-a-small (21:26): U1, U4 to U7 as written. U2: `VarSet` is gone (dedup-bits-db's `db.md` columns section, part 2, which I applied with it), so `diverging_type_vars` was first `BitSet<u32>`; then (21:52, the parent's call) C12's goal against it: the field is a private `scratch::Lent<Vec<u64>>` (`scratch::lend_u64s()`), `set_diverging` grows it with `words_for` and inserts through `BitSliceMut::<u32>::from_words`, and `diverging_type_vars()` hands fallback a `BitSlice<'_, u32>` (a `Lent` is not `Words`, so views over its slice); the `// alloc:` on `trait_errors` as written. U3 and U8: no change. The `infer.rs` callers (`WriteBackCtxt::new(.., frame)`, `resolve_diagnostics` returning `Box<[_]>`) are A8's.

### U1. Imports (lines 12 and 13)

Before:

```rust
use smallvec::SmallVec;
use thin_vec::ThinVec;
```

After (`ThinVec`'s one use outside `mod resolve_completely`, `emit_trait_errors`' parameter, goes
in U5):

```rust
use smallvec::SmallVec;
```

and inside `mod resolve_completely` (lines 604 to 622), before:

```rust
    use rustc_hash::FxHashSet;
```

```rust
    use crate::{
        InferenceDiagnostic, Span,
        infer::unify::InferenceTable,
```

after:

```rust
    use blaze_mem::Frame;
```

```rust
    use crate::{
        InferenceDiagnostic, Span,
        infer::{
            columns::{FrameSet, frame_set},
            unify::InferenceTable,
        },
```

(`Goal` and `Predicate` in its `next_solver::{..}` import have no other use after U6: drop them
from that list.)

### U2. `InferenceTable` and `InferenceTable::new` (lines 148 to 178)

Its one caller is `InferenceContext::new` (`rg -n 'InferenceTable::new\(' crates/` finds only
`infer.rs`), so it is not an R1 entry point. Before:

```rust
            diverging_type_vars: VarSet::default(),
            trait_errors: Vec::new(),
```

After:

```rust
            diverging_type_vars: VarSet::new(),
            // alloc: an error path, unallocated until the body's first trait error
            trait_errors: Vec::new(),
```

`diverging_type_vars` lends the thread's `u64` buffer (C12): warm across bodies, so a body that
coerces `!` allocates nothing. `trait_errors` stays a global list: in a body that compiles,
`try_evaluate_obligations` and `structurally_normalize_*` return no error, so it is never
allocated; a frame lifetime for it would put `'f` on `InferenceTable` and on everything that
names it (autoderef, the probe, coercion's delegate), for an error path.

### U3. `obligations_for_self_ty` (lines 212 to 236): stays

`SmallVec<[Obligation<'db, Predicate<'db>>; 4]>`, made per call (a closure's expected signature,
`type_var_is_sized`): the pending goals whose self type is one variable are its `Sized` and its
`Fn*`/projection bounds, at most four in practice, so it never spills. The returned list is the
callers' (`infer/closure.rs`, `infer/expr.rs`), so it stays as it is.

### U4. `resolve_projections_within` (lines 555 to 577)

Per path resolved on a type (`infer/path.rs:343`), so per path expression with an associated item.
The list of trait refs was collected only to be walked once. Before:

```rust
        let interner = self.interner();
        let traits: Vec<_> = ty
            .walk()
            .filter_map(|arg| arg.as_type())
            .filter(|ty| ty.has_non_region_infer())
            .filter_map(|ty| match ty.kind() {
                TyKind::Alias(_, alias)
                    if matches!(alias.kind, crate::next_solver::AliasTyKind::Projection { .. }) =>
                {
                    Some(alias.trait_ref(interner))
                }
                _ => None,
            })
            .collect();
        if traits.is_empty() {
            return ty;
        }
        for trait_ref in traits {
            let cause = ObligationCause::new(Span::Dummy);
            self.register_predicate(Obligation::new(interner, cause, self.param_env, trait_ref));
        }
        self.resolve_vars_with_obligations(ty)
```

After (the walk reads only `ty` and `interner`, never the table, so registering as it goes
registers the same obligations in the same order):

```rust
        let interner = self.interner();
        let traits = ty
            .walk()
            .filter_map(|arg| arg.as_type())
            .filter(|ty| ty.has_non_region_infer())
            .filter_map(|ty| match ty.kind() {
                TyKind::Alias(_, alias)
                    if matches!(alias.kind, crate::next_solver::AliasTyKind::Projection { .. }) =>
                {
                    Some(alias.trait_ref(interner))
                }
                _ => None,
            });
        let mut registered = false;
        for trait_ref in traits {
            registered = true;
            let cause = ObligationCause::new(Span::Dummy);
            self.register_predicate(Obligation::new(interner, cause, self.param_env, trait_ref));
        }
        if !registered {
            return ty;
        }
        self.resolve_vars_with_obligations(ty)
```

### U5. `emit_trait_errors` (lines 587 to 592)

Before:

```rust
    fn emit_trait_errors(&mut self, diagnostics: &mut ThinVec<InferenceDiagnostic>) {
```

After (the body unchanged; its one caller, U6, passes a frame list):

```rust
    fn emit_trait_errors(&mut self, diagnostics: &mut impl Extend<InferenceDiagnostic>) {
```

### U6. `WriteBackCtxt<'db, 'f>` (lines 624 to 723)

Per body, once. Before:

```rust
    pub(crate) struct WriteBackCtxt<'db> {
        table: InferenceTable<'db>,
        diagnostics: ThinVec<InferenceDiagnostic>,
        /// "type annotations needed" for the variables left unresolved, reported only if the
        /// body has no other error.
        unresolved_vars: ThinVec<InferenceDiagnostic>,
        has_errors: bool,
        spans_emitted_type_must_be_known_for: FxHashSet<Span>,
        types: &'db DefaultAny<'db>,
    }

    impl<'db> WriteBackCtxt<'db> {
        pub(crate) fn new(
            table: InferenceTable<'db>,
            diagnostics: ThinVec<InferenceDiagnostic>,
            vars_emitted_type_must_be_known_for: FxHashSet<Term<'db>>,
        ) -> Self {
            let spans_emitted_type_must_be_known_for = vars_emitted_type_must_be_known_for
                .into_iter()
                .filter_map(|term| match term.kind() {
```

```rust
                })
                .collect();

            Self {
                types: table.interner().default_types(),
                table,
                diagnostics,
                unresolved_vars: ThinVec::new(),
                has_errors: false,
                spans_emitted_type_must_be_known_for,
            }
        }
```

After (the `filter_map` arms unchanged):

```rust
    pub(crate) struct WriteBackCtxt<'db, 'f> {
        table: InferenceTable<'db>,
        /// What inference reported, as [`super::super::diagnostics::Diagnostics`] kept it.
        diagnostics: ThinVec<InferenceDiagnostic>,
        /// "type annotations needed" for the variables left unresolved, reported only if the
        /// body has no other error.
        unresolved_vars: Vec<InferenceDiagnostic, &'f Frame>,
        has_errors: bool,
        spans_emitted_type_must_be_known_for: FrameSet<'f, Span>,
        types: &'db DefaultAny<'db>,
        frame: &'f Frame,
    }

    impl<'db, 'f> WriteBackCtxt<'db, 'f> {
        pub(crate) fn new(
            table: InferenceTable<'db>,
            diagnostics: ThinVec<InferenceDiagnostic>,
            vars_emitted_type_must_be_known_for: FrameSet<'f, Term<'db>>,
            frame: &'f Frame,
        ) -> Self {
            let mut spans_emitted_type_must_be_known_for = frame_set(frame);
            spans_emitted_type_must_be_known_for.extend(
                vars_emitted_type_must_be_known_for.into_iter().filter_map(|term| match term.kind() {
```

```rust
                }),
            );

            Self {
                types: table.interner().default_types(),
                table,
                diagnostics,
                unresolved_vars: Vec::new_in(frame),
                has_errors: false,
                spans_emitted_type_must_be_known_for,
                frame,
            }
        }
```

(The span set is only asked whether a span is new, so its order does not matter.)

`resolve_completely_with_default`, before:

```rust
            let value = self.table.resolve_vars_if_possible(value);

            let mut goals = vec![];

            // FIXME(next-solver): Handle `goals`.

            *value_ref = value.fold_with(&mut Resolver::new(self, true, &mut goals));
```

after (the list only gathered the goals deep normalization returns, to drop them when the call
ended: they are dropped where they are returned, U7):

```rust
            let value = self.table.resolve_vars_if_possible(value);
            *value_ref = value.fold_with(&mut Resolver::new(self, true));
```

`resolve_diagnostics`, before:

```rust
        pub(crate) fn resolve_diagnostics(mut self) -> (ThinVec<InferenceDiagnostic>, bool) {
            let has_errors = self.has_errors;

            self.table.emit_trait_errors(&mut self.diagnostics);

            // Ignore diagnostics made from resolving diagnostics.
            let unresolved_vars = std::mem::take(&mut self.unresolved_vars);
            let mut diagnostics = std::mem::take(&mut self.diagnostics);
            diagnostics.retain_mut(|diagnostic| {
```

```rust
            if !diagnostics.iter().any(InferenceDiagnostic::taints_body) {
                diagnostics.extend(unresolved_vars);
            }
            diagnostics.shrink_to_fit();

            (diagnostics, has_errors)
        }
```

after (the `retain_mut` closure unchanged; the same diagnostics in the same order):

```rust
        /// The body's diagnostics, resolved, as the result keeps them: made once at their
        /// number, nothing when there are none.
        pub(crate) fn resolve_diagnostics(mut self) -> (Box<[InferenceDiagnostic]>, bool) {
            let has_errors = self.has_errors;

            let mut diagnostics = Vec::new_in(self.frame);
            diagnostics.extend(std::mem::take(&mut self.diagnostics));
            self.table.emit_trait_errors(&mut diagnostics);

            // Ignore diagnostics made from resolving diagnostics.
            let unresolved_vars =
                std::mem::replace(&mut self.unresolved_vars, Vec::new_in(self.frame));
            diagnostics.retain_mut(|diagnostic| {
```

```rust
            if !diagnostics.iter().any(InferenceDiagnostic::taints_body) {
                diagnostics.extend(unresolved_vars);
            }
            // A `Vec`'s drain is `TrustedLen`: one allocation of exactly the number left.
            (diagnostics.drain(..).collect(), has_errors)
        }
```

`DiagnoseInferVars<'a, 'db>` (line 726) becomes `DiagnoseInferVars<'a, 'db, 'f>` with
`ctx: &'a mut WriteBackCtxt<'db, 'f>`, and its impls `impl<'db> DiagnoseInferVars<'_, 'db, '_>`
and `impl<'db> TypeFolder<DbInterner<'db>> for DiagnoseInferVars<'_, 'db, '_>`; their bodies are
unchanged (`self.ctx.unresolved_vars.push(..)` and `.insert(span)` are the frame list's and set's).

### U7. `Resolver<'a, 'db, 'f>` (lines 839 to 931)

Before:

```rust
    pub(super) struct Resolver<'a, 'db> {
        ctx: &'a mut WriteBackCtxt<'db>,
        /// Whether we should normalize, disabled when resolving predicates.
        should_normalize: bool,
        nested_goals: &'a mut Vec<Goal<'db, Predicate<'db>>>,
    }

    impl<'a, 'db> Resolver<'a, 'db> {
```

```rust
        pub(super) fn new(
            ctx: &'a mut WriteBackCtxt<'db>,
            should_normalize: bool,
            nested_goals: &'a mut Vec<Goal<'db, Predicate<'db>>>,
        ) -> Resolver<'a, 'db> {
            Resolver { ctx, nested_goals, should_normalize }
        }
```

```rust
                    Ok((value, goals)) => {
                        self.nested_goals.extend(goals);
                        value
                    }
```

```rust
    impl<'db> TypeFolder<DbInterner<'db>> for Resolver<'_, 'db> {
```

After (`NORMALIZE_FLAGS` and the rest of `handle_term` unchanged):

```rust
    pub(super) struct Resolver<'a, 'db, 'f> {
        ctx: &'a mut WriteBackCtxt<'db, 'f>,
        /// Whether we should normalize, disabled when resolving predicates.
        should_normalize: bool,
    }

    impl<'a, 'db, 'f> Resolver<'a, 'db, 'f> {
```

```rust
        pub(super) fn new(
            ctx: &'a mut WriteBackCtxt<'db, 'f>,
            should_normalize: bool,
        ) -> Resolver<'a, 'db, 'f> {
            Resolver { ctx, should_normalize }
        }
```

```rust
                    // FIXME(next-solver): Handle the goals normalization returns.
                    Ok((value, _goals)) => value,
```

```rust
    impl<'db> TypeFolder<DbInterner<'db>> for Resolver<'_, 'db, '_> {
```

`let universes = vec![None; outer_exclusive_binder(value).as_usize()];` (line 886) stays: it is
the argument `deeply_normalize_with_skipped_universes_and_ambiguous_coroutine_goals` takes
(`Vec<Option<UniverseIndex>>`, the solver's), and it is empty, so unallocated, for a term with no
escaping binder, which is every term write-back resolves outside a binder.

### U8. `could_unify`, `could_unify_deeply` (lines 109 to 146): no task

R1 entry points (`crates/hir/src/lib.rs` `Type::could_unify_with`, `could_unify_with_deeply`, and
the IDE through them), but they make no temporary of their own: the `InferCtxt`, its
`ObligationCtxt` and the error list `select` returns are the solver's, which recycles their
storage per worker (`v2/requests/mem-db-solver.md`, "The one design decision", requests 5 and 6),
not from a frame. A `frame::task` here would open and reset a frame for nothing; they stay as
they are. (If the solver's storage ever moves onto `frame::current`, these two gain
`blaze_mem::frame::task(crate::db::INFER_TAG, |_| ..)` around `could_unify_impl`.)

`take_ambiguity_errors`' two `collect`s (lines 472 to 483) already carry their `// alloc:` (an
error path, none for a body without an ambiguity); `collect_remaining_errors` returns the solver's
list, empty for a body that compiles.

## E. `infer/fallback.rs`

Per body with unsolved variables (every body with an integer literal of no other type, a `!`
coercion, or `Default::default()`-like calls resolved late).

> forwarded to db (db-ty-next2) (requests, pass 20; told by tmux).
> applied by db-mem-infer-a-small (21:26): E1 to E3, with one difference: `VarSet` had become `BitSet::<u32>::default()` (dedup-bits-db), so E1 drops `columns::VarSet` and keeps `blaze_ids::bits::Bits`, and E3's `diverging_roots` is frame words, `Bits::<_, u32>::from_words(Vec::<u64, _>::new_in(self.frame))` (its `insert_growing` and `contains` unchanged).

### E1. Imports (lines 3 to 13)

Before:

```rust
use rustc_hash::FxHashMap;
```

```rust
    infer::{InferenceContext, columns::VarSet},
```

After (no `rustc_hash` line):

```rust
    infer::{
        InferenceContext,
        columns::{FrameMap, VarSet, frame_map},
    },
```

and line 15 `impl<'db> InferenceContext<'db> {` becomes `impl<'db, 'f> InferenceContext<'db, 'f> {` (A0).

### E2. `fallback_types` (lines 62 to 91)

Before:

```rust
        let interner = self.interner();
        let unresolved_variables: Vec<Ty<'db>> = unresolved_ty
            .into_iter()
            .map(|vid| Ty::new_var(interner, vid))
            .chain(unresolved_int.into_iter().map(|vid| Ty::new_int_var(interner, vid)))
            .chain(unresolved_float.into_iter().map(|vid| Ty::new_float_var(interner, vid)))
            .collect();
```

After (one frame list of exactly their number; the same order):

```rust
        let interner = self.interner();
        let mut unresolved_variables = Vec::with_capacity_in(
            unresolved_ty.len() + unresolved_int.len() + unresolved_float.len(),
            self.frame,
        );
        unresolved_variables.extend(unresolved_ty.into_iter().map(|vid| Ty::new_var(interner, vid)));
        unresolved_variables
            .extend(unresolved_int.into_iter().map(|vid| Ty::new_int_var(interner, vid)));
        unresolved_variables
            .extend(unresolved_float.into_iter().map(|vid| Ty::new_float_var(interner, vid)));
```

The loop `for ty in unresolved_variables { .. self.fallback_if_possible(ty, &diverging_fallback) }`
is unchanged: the list holds a copy of `self.frame`, not a borrow of `self`. The three lists
`unresolved_root_variables` returns are the solver's (`next_solver/infer/mod.rs:664`), one each
per body with an unsolved variable (S1 below).

### E3. `fallback_if_possible`, `calculate_diverging_fallback` (lines 107 to 176)

Before:

```rust
        diverging_fallback: &FxHashMap<Ty<'db>, Ty<'db>>,
```

```rust
    ) -> FxHashMap<Ty<'db>, Ty<'db>> {
```

```rust
        let mut diverging_roots = VarSet::default();
```

```rust
        let mut diverging_fallback = FxHashMap::default();
```

After (the bodies unchanged):

```rust
        diverging_fallback: &FrameMap<'_, Ty<'db>, Ty<'db>>,
```

```rust
    ) -> FrameMap<'f, Ty<'db>, Ty<'db>> {
```

```rust
        let mut diverging_roots = VarSet::new();
```

```rust
        let mut diverging_fallback = frame_map(self.frame);
```

(`diverging_roots` lends a second `u64` buffer while the table's `diverging_type_vars` holds the
first: two of the shelf's four, C12. The map is only looked up, so its order does not matter.)

## F. `infer/autoderef.rs`

mem-db-infer-expr's X2 (`adjust_steps_as_infer_ok(&mut self, frame: &'f Frame) -> InferOk<'db,
Vec<Adjustment, &'f Frame>>`, one frame list of exactly the step count, `use blaze_mem::Frame;`) is
the whole change to this file; taken as written there. `autoderef` and `autoderef_with_tracking`
build the solver-side `Autoderef` (mem-db-infer-method's `autoderef.rs`) and allocate nothing here.

> forwarded to db (db-ty-next2) (requests, pass 20; told by tmux).
> applied by db-mem-infer-a-small (21:26): X2 as written. Callers told by tmux: `method_resolution/confirm.rs` (db-mem-infer-b), `infer/{expr,callee,place_op}.rs` (db-mem-infer-a-expr); `coerce.rs` already passes `self.frame`.

## G. The small files: `ambiguity.rs`, `at_error.rs`, `mutability.rs`, `opaques.rs`, `path.rs`

> forwarded to db (db-ty-next2) (requests, pass 20; told by tmux).
> checked by db-audit-mem (22:44): each file's subsection is applied below (db-mem-infer-a-small), the headers `impl<'db, 'f> InferenceContext<'db, 'f>` in the tree.

### G1. `infer/ambiguity.rs`

Line 5 header only (A0). `take_ambiguity_errors`' lists are U8's error path; `ambiguity_diagnostics`
(`solver_errors.rs`, mem-db-infer-eval's) runs only when there is an ambiguity.

> applied by db-mem-infer-a-small (21:26): the header.

### G2. `infer/at_error.rs`

Line 15 header only (A0). Its `Box::new((expected.store(), found.store()))` is an error's own pair,
already `// alloc:`; `type_at_error` (new) allocates nothing (a fold of interned types).

> applied by db-mem-infer-a-small (21:26): the header.

### G3. `infer/mutability.rs`

Line 16 header only (A0). The walk allocates nothing: `self.result.expr_adjustments.get_mut(&tgt_expr)`
is C8's `CsrBuilder::get_mut` and `self.result.type_of_expr[tgt_expr]` C3's `Index`, as they were
for `CsrBuilder` and `ArenaMap`.

> applied by db-mem-infer-a-small (21:36): the header (db-apply-ty's report has its sections there done).

### G4. `infer/path.rs`

Line 30 header (A0). `resolve_value_path_inner`, before:

```rust
            &mut vars_ctx,
            &self.defined_anon_consts,
            LifetimeLoweringMode::LateParam,
        );
        let mut path_ctx = if no_diagnostics {
```

after:

```rust
            &mut vars_ctx,
            &self.defined_anon_consts,
            LifetimeLoweringMode::LateParam,
            self.frame,
        );
        let mut path_ctx = if no_diagnostics {
```

and its nested fn, before:

```rust
        fn drop_ctx(mut ctx: TyLoweringContext<'_, '_>, no_diagnostics: bool) {
```

after (B4's third lifetime):

```rust
        fn drop_ctx(mut ctx: TyLoweringContext<'_, '_, '_>, no_diagnostics: bool) {
```

`resolve_ty_assoc_item`'s two `name.clone()`s are a `Name`'s (an interned `Symbol`, a refcount):
the probe takes the name by value, and the second is an error's, already `// clone:`.
`resolve_projections_within` is U4.

> applied by db-mem-infer-a-small (21:36): as written (header, `self.frame` last to `InferenceTyLoweringContext::new`, `drop_ctx`'s third lifetime).

### G5. `infer/opaques.rs`

Lines 15 and 66, headers only (A0). `handle_opaque_type_uses`' `clone_opaque_types()` is the
solver's list (`next_solver/infer/mod.rs:958`, already `// alloc:`: once per body, none without an
opaque type; S2 below), moved into `compute_definition_site_hidden_types`, which resolves it in
place. `self.result.type_of_opaque.insert(..)` is A3's `FrameMap`.

> applied by db-mem-infer-a-small (21:26): both headers.

## P. `infer/persist.rs`

Lines as of 2026-10-29 21:10 (UTC-7); lane db is adding to this file right now (`value_resolutions`,
`CallArguments`, `CastPointers`), so each entry quotes its "before" code. The format does not change:
the same words in the same order, no schema bump. mem-db-infer-closure's X4 (the closure decoder's
`place`, `sources` and `closure(&mut self, tys, frame)`, and their tests) is taken as written there;
P5 passes it the frame.

**R1.** `InferenceResult::to_record` and `InferenceRecordRef::to_result` are `pub` and run outside
any query (today `crates/hir/src/tests.rs:978` to `982` and this file's tests; later the image
writer and loader). Encoding needs no frame: every buffer it fills holds `u64`, `u32` or `u8`, so it
writes into the thread's `scratch` buffers (R7), warm across records, and copies each array once at
its length. Decoding builds `Ty<'db>` lists and the closure decoder's frame lists (X4), so `decode`
opens `blaze_mem::frame::task(crate::db::INFER_TAG, ..)`; a call inside a query's task is one level
deeper, and resets only its own memory.

> forwarded to db (db-ty-next2) (requests, pass 20; told by tmux).

> applied by db-mem-infer-a-persist (21:58): P1 to P7 as written, plus the same rule on code lane db
> added since: `unstable_candidates`' map (`ExprOrPatMap::new_in(frame)`) and its candidate lists
> (`exact`), `TraitUnimplemented::parent_impls` (`exact`), and db-ty-d-infer2's test
> `every_cause_and_parent_impl_round_trips` (`Sink::new()`, its `Source` in a task). P1 also adds
> `BindingId` (X4's `seen` set is typed by it). P7.1 differs in form only: `result()` is
> `frame::task(INFER_TAG, result_in)` and the body moved unchanged into `fn result_in(frame: &Frame)`,
> so no 110-line reindent.

### P1. Imports (lines 11 to 70)

Before:

```rust
use std::mem::offset_of;
```

After (`rustc_hash::FxBuildHasher` and `MinCaptures` are X4's, added with it; the `columns::{..}`
import is unchanged):

```rust
use std::mem::offset_of;

use blaze_mem::{Frame, scratch};
```

### P2. `table_bytes` (lines 261 to 267)

Per record that was never written (a test, or a record compared in memory). Before:

```rust
fn table_bytes(words: &[u128]) -> Aligned {
    let mut bytes = Vec::with_capacity(words.len() * size_of::<u128>());
    words.iter().for_each(|word| bytes.extend_from_slice(&word.to_le_bytes()));
    Aligned::copy_from(&bytes)
}
```

After (the copy is `Aligned`'s own, at its length; the bytes in between are the thread's):

```rust
fn table_bytes(words: &[u128]) -> Aligned {
    scratch::with_bytes(|bytes| {
        bytes.reserve(words.len() * size_of::<u128>());
        words.iter().for_each(|word| bytes.extend_from_slice(&word.to_le_bytes()));
        Aligned::copy_from(bytes)
    })
}
```

`table_words` (line 253) stays: `chunks_exact(16).map(..)` is `TrustedLen`, so its `collect` is one
allocation of exactly the record's words, which the record keeps.

### P3. Encoding: `Columns` in place of the record's growing arrays (lines 609 to 829)

Per record: `of` pushed into the record's three `Vec`s, each doubling about ten times on a
1,000-expression body and kept with their slack. Now they fill the thread's scratch buffers and
the record gets each once at its length. Before (lines 609 to 624):

```rust
impl InferenceRecord {
    fn empty() -> InferenceRecord {
        InferenceRecord {
            u64s: Vec::new(),
            u32s: Vec::new(),
            bytes: Vec::new(),
            spans64: [[0; 2]; N64],
            spans32: [[0; 2]; N32],
            spans8: [[0; 2]; N8],
            error_ty: 0,
            flags: 0,
            diagnostics: 0,
            table: Vec::new(),
            table_root: 0,
        }
    }

    fn span(start: usize, end: usize) -> [u32; 2] {
```

After (`empty` has no other caller):

```rust
/// An [`InferenceRecord`]'s three arrays while [`InferenceRecord::of`] writes them: the thread's
/// scratch buffers, warm across records, each copied once at its length by [`Columns::finish`].
struct Columns {
    u64s: scratch::Lent<Vec<u64>>,
    u32s: scratch::Lent<Vec<u32>>,
    bytes: scratch::Lent<Vec<u8>>,
    spans64: [[u32; 2]; N64],
    spans32: [[u32; 2]; N32],
    spans8: [[u32; 2]; N8],
}

impl Columns {
    fn new() -> Columns {
        Columns {
            u64s: scratch::lend_u64s(),
            u32s: scratch::lend_u32s(),
            bytes: scratch::lend_bytes(),
            spans64: [[0; 2]; N64],
            spans32: [[0; 2]; N32],
            spans8: [[0; 2]; N8],
        }
    }

    /// The record of these arrays, with no type table yet.
    fn finish(self, error_ty: u32, flags: u32, diagnostics: u32) -> InferenceRecord {
        InferenceRecord {
            // alloc: the record's own arrays, each once at its length
            u64s: self.u64s.to_vec(),
            u32s: self.u32s.to_vec(),
            bytes: self.bytes.to_vec(),
            spans64: self.spans64,
            spans32: self.spans32,
            spans8: self.spans8,
            error_ty,
            flags,
            diagnostics,
            table: Vec::new(),
            table_root: 0,
        }
    }

    fn span(start: usize, end: usize) -> [u32; 2] {
```

That is, `impl InferenceRecord {` (line 609) becomes `impl Columns {`, and `span`, `put64`, `put32`,
`put8`, `put_variants`, `put_assocs`, `put_tys`, `put_lists` and `put_adjustments` (lines 626 to
701) move with it, unchanged (`self.u64s.len()`, `self.u64s.extend(..)` go through the `Lent`'s
`Deref`/`DerefMut`). Then `of` and `as_ref` go in an `impl InferenceRecord {` of their own, started
just before `of`'s doc comment (`/// \`result\` with its types written into \`tys\`.`). In `of`,
before:

```rust
        let mut record = InferenceRecord::empty();
```

```rust
        let mut words = Sink::default();
        diagnostics.iter().for_each(|it| words.diagnostic(tys, it));
        record.diagnostics = u32::try_from(diagnostics.len()).expect("2^32 diagnostics");
```

```rust
        record.put32(C32::Words, words.words);
        record.put8(C8::Text, words.text);

        record.put64(C64::AnonConsts, defined_anon_consts.iter().map(|&it| id_bits(it)));
        record.error_ty = tys.ty(error_ty.as_ref());
        record.flags = u32::from(*has_errors);
        record
    }
```

after (the error type is still the last type written, so the type table is the same):

```rust
        let mut record = Columns::new();
```

```rust
        let mut words = Sink::new();
        diagnostics.iter().for_each(|it| words.diagnostic(tys, it));
        let diagnostic_count = u32::try_from(diagnostics.len()).expect("2^32 diagnostics");
```

```rust
        record.put32(C32::Words, words.words.iter().copied());
        record.put8(C8::Text, words.text.iter().copied());

        record.put64(C64::AnonConsts, defined_anon_consts.iter().map(|&it| id_bits(it)));
        let error_ty = tys.ty(error_ty.as_ref());
        record.finish(error_ty, u32::from(*has_errors), diagnostic_count)
    }
```

(`words` is dropped at the end of `of`, its two buffers back on the shelf. At most two `u32` and
two byte buffers are lent at once, within the shelf's `DEPTH` of 4. `to_record`'s
`record.table_root = ..` and `record.table = table_words(&bytes)` are unchanged; `Writer` and
`TyTableWriter` are `blaze_format`'s and `next_solver::persist`'s.)

### P4. `Sink` on scratch (lines 1252 to 1262)

Before:

```rust
/// The side tables' words being written, and the text of the names in them.
#[derive(Default)]
struct Sink {
    words: Vec<u32>,
    text: Vec<u8>,
}

impl Sink {
    fn word(&mut self, word: u32) {
```

After (every method unchanged: `push`, `extend`, `extend_from_slice`, `len` and indexing go through
the `Lent`, and `lowering_words`' `put(&mut self.words)` derefs to `&mut Vec<u32>`):

```rust
/// The side tables' words being written, and the text of the names in them: the thread's
/// scratch buffers, which [`InferenceRecord::of`] copies into the record's arrays.
struct Sink {
    words: scratch::Lent<Vec<u32>>,
    text: scratch::Lent<Vec<u8>>,
}

impl Sink {
    fn new() -> Sink {
        Sink { words: scratch::lend_u32s(), text: scratch::lend_bytes() }
    }

    fn word(&mut self, word: u32) {
```

### P5. Decoding in a task; each column made at its length (lines 1013 to 1249)

Per record read. A `collect` through `Result` gets no length from its iterator (the shunt's
`size_hint` is `(0, _)`), so each column grew from nothing by doubling and was shrunk into its
`Box` (a realloc); now each is one allocation of exactly its length. The helper, added after
`local_of` (line 292):

```rust
/// The values of `items`, `len` of them unless one is an error: one allocation of exactly `len`,
/// none when it is 0. Every `len` here is bounded by the record (a column's length, or a count
/// [`Source::len`] checked against the words left).
fn exact<T>(
    len: usize,
    items: impl IntoIterator<Item = Result<T, FormatError>>,
) -> Result<Vec<T>, FormatError> {
    // alloc: a decoded column of the result, once at its length
    let mut values = Vec::with_capacity(len);
    for item in items {
        values.push(item?);
    }
    debug_assert_eq!(values.len(), len, "a column of another length than counted");
    Ok(values)
}
```

(`Vec::with_capacity(len)` has capacity `len` for a sized `T`, so `into_boxed_slice` moves it
without a realloc.) Each site, its `map` closure verbatim:

| site (in `impl InferenceRecordRef`) | before | after |
|---|---|---|
| `variants` | `let rows = kinds.iter().zip(ids).map(..).collect::<Result<_, _>>()?;` | `let rows = exact(kinds.len(), kinds.iter().zip(ids).map(..))?.into_boxed_slice();` |
| `assocs` | `let rows = kinds.iter().zip(ids).zip(args).map(..).collect::<Result<_, FormatError>>()?;` | `let rows = exact(kinds.len(), kinds.iter().zip(ids).zip(args).map(..))?.into_boxed_slice();` |
| `tys` | `self.u32s(column).iter().map(\|&it\| tys.ty(it)).collect()` | `let column = self.u32s(column);` then `Ok(exact(column.len(), column.iter().map(\|&it\| tys.ty(it)))?.into_boxed_slice())` |
| `adjustments` | `let items = kinds.iter().zip(targets).map(..).collect::<Result<_, FormatError>>()?;` | `let items = exact(kinds.len(), kinds.iter().zip(targets).map(..))?.into_boxed_slice();` |
| `decode`, method rows | `let rows = fns.iter().zip(args).map(..).collect::<Result<_, FormatError>>()?;` | `let rows = exact(fns.len(), fns.iter().zip(args).map(..))?.into_boxed_slice();` |
| `decode`, `fields` | `let fields = kinds.iter().zip(variants).zip(locals).map(..).collect::<Result<_, FormatError>>()?;` | `let fields = exact(kinds.len(), kinds.iter().zip(variants).zip(locals).map(..))?.into_boxed_slice();` |
| `decode`, `tuple_fields` | `let tuple_fields = words.chunks_exact(2).map(..).collect::<Result<_, FormatError>>()?;` | `let tuple_fields = exact(words.len() / 2, words.chunks_exact(2).map(..))?.into_boxed_slice();` |
| `decode`, `type_of_opaque` | `let entries = ids.iter().zip(opaque_tys).map(..).collect::<Result<_, FormatError>>()?;` | `let entries = exact(ids.len(), ids.iter().zip(opaque_tys).map(..))?;` (`from_sorted` takes the `Vec`) |
| `decode`, `pat_adjustments` | `let items = kinds.iter().zip(sources).map(..).collect::<Result<_, FormatError>>()?;` | `let items = exact(kinds.len(), kinds.iter().zip(sources).map(..))?.into_boxed_slice();` |
| `decode`, `asm_sym_tys` | `let entries = words.chunks_exact(3).map(..).collect::<Result<_, FormatError>>()?;` | `let entries = exact(words.len() / 3, words.chunks_exact(3).map(..))?;` |
| `decode`, `unreachable` | `words.chunks_exact(UNREACHABLE_WORDS).map(..).collect::<Result<Box<[_]>, FormatError>>()?` | `exact(words.len() / UNREACHABLE_WORDS, words.chunks_exact(UNREACHABLE_WORDS).map(..))?.into_boxed_slice()` |
| `decode`, `closures_data` | `let entries = self.u32s(C32::ClosureExprs).iter().map(\|&expr\| Ok((local_of(expr), words.closure(tys)?))).collect::<Result<_, FormatError>>()?;` | `let exprs = self.u32s(C32::ClosureExprs);` then `let entries = exact(exprs.len(), exprs.iter().map(\|&expr\| Ok((local_of(expr), words.closure(tys, frame)?))))?;` (X4's frame) |
| `decode`, `defined_anon_consts` | `self.u64s(C64::AnonConsts).iter().map(\|&bits\| self.id::<AnonConstId<'db>>(bits)).collect::<Result<_, _>>()?` | `let anon_consts = self.u64s(C64::AnonConsts);` then `exact(anon_consts.len(), anon_consts.iter().map(\|&bits\| self.id::<AnonConstId<'db>>(bits)))?.into_boxed_slice()` |

`index_column`, `idx_set` and `lists` (`to_vec()`, `.into()` of a slice) and
`ModeColumn::from_codes(self.u8s(..).into())` are already one exact copy each: unchanged.

`to_result` and `decode`, before:

```rust
        let table = TyTableReader::open(interner, bytes, self.table_root)?;
        self.decode(&TableDecode { interner, table, at: self.at })
    }

    fn decode<'db>(&self, tys: &impl TyDecode) -> Result<InferenceResult<'db>, FormatError> {
        let (fns, args) = (self.u64s(C64::MethodFns), self.u32s(C32::MethodArgs));
```

after (`to_result` unchanged; `decode` keeps its signature, so the tests' `read.decode(&model)`
calls stay):

```rust
        let table = TyTableReader::open(interner, bytes, self.table_root)?;
        self.decode(&TableDecode { interner, table, at: self.at })
    }

    /// [`Self::decode_in`] in a task of its own: decoding runs outside any query (R1).
    fn decode<'db>(&self, tys: &impl TyDecode) -> Result<InferenceResult<'db>, FormatError> {
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| self.decode_in(tys, frame))
    }

    /// The result of this record, its temporaries in `frame`.
    fn decode_in<'db>(
        &self,
        tys: &impl TyDecode,
        frame: &Frame,
    ) -> Result<InferenceResult<'db>, FormatError> {
        let (fns, args) = (self.u64s(C64::MethodFns), self.u32s(C32::MethodArgs));
```

In `decode_in`, the tuple list buffer, before:

```rust
            let mut buffer = Vec::new();
```

after (one frame list for the record, grown to its longest tuple):

```rust
            let mut buffer = Vec::new_in(frame);
```

The side tables' reader, before:

```rust
        let mut words = Source { words: self.u32s(C32::Words), next: 0, text: self.u8s(C8::Text), at: self.at };
        let diagnostics = (0..self.diagnostics)
            .map(|_| words.diagnostic(tys))
            .collect::<Result<Box<[_]>, _>>()?;
```

after (`self.diagnostics` is a count read from the record: each diagnostic is a word or more, so a
count past the words is refused before anything is made for it):

```rust
        let side = self.u32s(C32::Words);
        let mut words = Source { words: side, next: 0, text: self.u8s(C8::Text), at: self.at, frame };
        let count = self.diagnostics as usize;
        self.ensure(count <= side.len(), "more diagnostics than side table words")?;
        let diagnostics = exact(count, (0..count).map(|_| words.diagnostic(tys)))?.into_boxed_slice();
```

and the value path resolutions, before:

```rust
            let mut map = ExprOrPatMap::default();
```

after:

```rust
            let mut map = ExprOrPatMap::new_in(frame);
```

### P6. `Source` carries the frame; its lists at their length (lines 2054 to 2137, 2318 to 2722)

`Source<'a>`, before:

```rust
struct Source<'a> {
    words: &'a [u32],
    next: usize,
    text: &'a [u8],
    /// Where the record's root is, for errors.
    at: u64,
}
```

after:

```rust
struct Source<'a> {
    words: &'a [u32],
    next: usize,
    text: &'a [u8],
    /// Where the record's root is, for errors.
    at: u64,
    /// The decode's frame, for the lists of types it interns.
    frame: &'a Frame,
}
```

`Source::tys`, per tuple and signature read, before:

```rust
    fn tys(&mut self, tys: &impl TyDecode) -> Result<StoredTys, FormatError> {
        let len = self.len()?;
        let list = (0..len).map(|_| self.ty(tys)).collect::<Result<Vec<_>, _>>()?;
        let list: Vec<Ty<'_>> = list.iter().map(StoredTy::as_ref).collect();
        Ok(Tys::new_from_slice(&list).store())
    }
```

after (two global lists become one frame list of exactly `len`):

```rust
    fn tys(&mut self, tys: &impl TyDecode) -> Result<StoredTys, FormatError> {
        let len = self.len()?;
        let mut list = Vec::with_capacity_in(len, self.frame);
        for _ in 0..len {
            list.push(self.ty(tys)?.as_ref());
        }
        Ok(Tys::new_from_slice(&list).store())
    }
```

(`StoredTy::as_ref<'db>(&self) -> Ty<'db>` borrows nothing, so the list outlives each `StoredTy`.)

Every list of a diagnostic, read as `let len = self.len()?;` then
`(0..len).map(|_| EXPR).collect::<Result<_, _>>()?`, becomes `exact(len, (0..len).map(|_| EXPR))?`,
with `.into_boxed_slice()` where the field is a `Box<[_]>`; `EXPR` verbatim:

| site | field type | after |
|---|---|---|
| `call_arguments`' `tys_list` closure: `(0..len).map(\|_\| self.ty(tys)).collect()` | `Box<[StoredTy]>` | `Ok(exact(len, (0..len).map(\|_\| self.ty(tys)))?.into_boxed_slice())` |
| `call_arguments`' `compatible_ends`: `(0..provided.len()).map(\|_\| self.word()).collect::<Result<_, _>>()?` | `Box<[u32]>` | `exact(provided.len(), (0..provided.len()).map(\|_\| self.word()))?.into_boxed_slice()` (`provided.len()` was a checked `len`) |
| `call_arguments`' `compatible` | `Box<[u32]>` | `exact(len, (0..len).map(\|_\| self.word()))?.into_boxed_slice()` |
| `ambiguity_note`'s `candidates` | `Vec<AmbiguityCandidate>` | `exact(len, (0..len).map(\|_\| { Ok(match self.word()? { .. }) }))?` |
| diagnostic 22's and 23's `unmet` | `Vec<StoredGoal>` | `exact(len, (0..len).map(\|_\| self.goal(tys)))?` |
| `CastPointers::added_auto_traits` | `Box<[TraitId]>` | `exact(len, (0..len).map(\|_\| self.id()))?.into_boxed_slice()` |
| `MethodCallIllegalSizedBound::candidates` | `Box<[TraitId]>` | `exact(len, (0..len).map(\|_\| self.id()))?.into_boxed_slice()` |
| `Ambiguity::related` | `Vec<AmbiguityNote>` | `exact(len, (0..len).map(\|_\| self.ambiguity_note(tys)))?` |
| `TraitUnimplemented::parent_trait_predicates` | `Vec<StoredTraitPredicate>` | `exact(len, (0..len).map(\|_\| self.trait_predicate(tys)))?` |
| `RecordMissingFields::missed_fields` | `Vec<LocalFieldId>` | `exact(len, (0..len).map(\|_\| self.local()))?` |

The closure decoder's lists (`place`, `sources`, `closure`) are X4's. `Source::name`'s
`Symbol::intern` and `StoredGoal::UnstableFeature`'s `symbol().clone()` are interned names (a
refcount): unchanged.

### P7. Tests (`mod tests`, line 2853 on)

1. `result()` (line 3356) makes its builders in a frame: its body goes inside
   `blaze_mem::frame::task(crate::db::INFER_TAG, |frame| { .. })` (it returns an owned
   `InferenceResult<'static>`), and before:

```rust
        let mut expr_adjustments = CsrBuilder::default();
```

```rust
        let mut assignment_coercions = CsrBuilder::default();
```

```rust
        let mut pat_adjustments = CsrBuilder::default();
```

```rust
                let mut values = ExprOrPatMap::default();
```

   after:

```rust
        let mut expr_adjustments = CsrBuilder::new_in(frame);
```

```rust
        let mut assignment_coercions = CsrBuilder::new_in(frame);
```

```rust
        let mut pat_adjustments = CsrBuilder::new_in(frame);
```

```rust
                let mut values = ExprOrPatMap::new_in(frame);
```

   (`type_of_expr` stays an `ArenaMap` handed to C4's test-only `TyColumn::from_entries`, and
   `ExprOrPatSet::insert` is C9's test-only one.)

2. `truncated_words_are_refused_not_misread` (line 3565), before:

```rust
        let mut sink = Sink::default();
```

```rust
        for len in 0..sink.words.len() {
            let mut source = Source { words: &sink.words[..len], next: 0, text: &sink.text, at: 0 };
            let read = (0..diagnostics.len())
                .map(|_| source.diagnostic(&model))
                .collect::<Result<Vec<_>, _>>()
                .and_then(|_| source.closure(&model));
            assert!(read.is_err(), "{len} of {} words read as a whole", sink.words.len());
        }
```

   after:

```rust
        let mut sink = Sink::new();
```

```rust
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
            for len in 0..sink.words.len() {
                let mut source =
                    Source { words: &sink.words[..len], next: 0, text: &sink.text, at: 0, frame };
                let read = (0..diagnostics.len())
                    .map(|_| source.diagnostic(&model))
                    .collect::<Result<Vec<_>, _>>()
                    .and_then(|_| source.closure(&model, frame));
                assert!(read.is_err(), "{len} of {} words read as a whole", sink.words.len());
            }
        });
```

3. New, after `a_corrupt_record_is_an_error`: the encoder's arrays are exactly their words, and a
   second encoding of the same result is the same record (the scratch buffers come back empty):

```rust
    #[test]
    fn a_record_is_made_at_its_length_and_again_the_same() {
        let mut model = Model::default();
        let record = InferenceRecord::of(&result(), &mut model);
        assert_eq!(record.u64s.capacity(), record.u64s.len());
        assert_eq!(record.u32s.capacity(), record.u32s.len());
        assert_eq!(record.bytes.capacity(), record.bytes.len());
        let mut again = Model::default();
        assert_eq!(InferenceRecord::of(&result(), &mut again), record);
    }

    #[test]
    fn a_diagnostic_count_past_the_words_is_refused() {
        let mut model = Model::default();
        let mut record = InferenceRecord::of(&result(), &mut model);
        record.diagnostics = u32::MAX;
        assert!(!decodes(&record, &model));
    }
```

   The existing round trips (`round_trip`, `decodes`, and `crates/hir/src/tests.rs:978`) hold the
   rest: the decoded result equals the one encoded, column by column.

## Entries in other parts' files

> forwarded to db (db-ty-next2), M1 included: `method_resolution.rs` is items' (DONE) and requests gives db-ty-next2 M1's lines there so they land with A10 in `infer.rs` and compile together; M1's `crates/hir` lines are db's too. D1, S1, S2, N1 are notes (requests, pass 20; told by tmux).

### M1. `TraitsInScope<'a>` (mem-db-infer-method: `method_resolution.rs`; lane db: `crates/hir`)

Per method call. A10 hands out the inference's `&FrameSet<'f, TraitId>`; the two callers in
`crates/hir` have an `FxHashSet<TraitId>` from `Resolver::traits_in_scope`. A field of either
kind keeps `crates/hir`'s code as it is and the probe's loop unchanged. In `method_resolution.rs`,
after `MethodResolutionContext` (line 74), add:

```rust
/// The traits a probe looks in: an inference run's set, in its frame, or the set of a caller
/// outside inference.
#[derive(Clone, Copy, Debug)]
pub enum TraitsInScope<'a> {
    Inference(&'a hashbrown::HashSet<TraitId, FxBuildHasher, &'a Frame>),
    Caller(&'a FxHashSet<TraitId>),
}

impl<'a> IntoIterator for TraitsInScope<'a> {
    type Item = &'a TraitId;
    type IntoIter = Either<
        hashbrown::hash_set::Iter<'a, TraitId>,
        std::collections::hash_set::Iter<'a, TraitId>,
    >;

    fn into_iter(self) -> Self::IntoIter {
        match self {
            TraitsInScope::Inference(set) => Either::Left(set.iter()),
            TraitsInScope::Caller(set) => Either::Right(set.iter()),
        }
    }
}
```

(the full type, not `FrameSet`, since `FrameSet` is `pub(crate)` and the enum is `pub`; `Frame` is
S1's import, `FxBuildHasher`, `FxHashSet` and `Either` are imported already). The field (line 78),
before: `pub traits_in_scope: &'a FxHashSet<TraitId>,` after: `pub traits_in_scope:
TraitsInScope<'a>,`. In `with_method_resolution` (line 188), before:

```rust
        let traits_in_scope = self.get_traits_in_scope();
        let traits_in_scope = match &traits_in_scope {
            Either::Left(it) => std::borrow::Borrow::<FxHashSet<TraitId>>::borrow(it),
            Either::Right(it) => *it,
        };
        let ctx = MethodResolutionContext {
            infcx: &self.table.infer_ctxt,
            resolver: &self.resolver,
            param_env: self.table.param_env,
            traits_in_scope,
```

after (with P1's `frame: self.frame,` last, as mem-db-infer-method wrote it):

```rust
        let ctx = MethodResolutionContext {
            infcx: &self.table.infer_ctxt,
            resolver: &self.resolver,
            param_env: self.table.param_env,
            traits_in_scope: TraitsInScope::Inference(self.get_traits_in_scope()),
```

`probe.rs:1215` `for &trait_did in self.ctx.traits_in_scope {` is unchanged (the field is `Copy`, its
`IntoIterator` yields `&TraitId`). In `crates/hir/src/attrs.rs` (line 25) the import becomes
`method_resolution::{self, CandidateId, MethodError, MethodResolutionContext, TraitsInScope},` and
line 560 `traits_in_scope: &traits_in_scope,` becomes `traits_in_scope:
TraitsInScope::Caller(&traits_in_scope),`; in `crates/hir/src/lib.rs` (line 100) the import becomes
`method_resolution::{self, InherentImpls, MethodResolutionContext, TraitsInScope},` and line 5815
`traits_in_scope,` becomes `traits_in_scope: TraitsInScope::Caller(traits_in_scope),`. Any other
`MethodResolutionContext { .. }` mem-db-infer-method adds (H1, H2) takes `TraitsInScope::Caller`.
Tests: `crates/hir-ide/src/tests/method_resolution.rs` (the inference path) and the
`crates/hir/src/tests/` attribute and method-candidate tests (the caller path), unchanged.

> superseded (db-mem-infer-a-ctx, 21:45): frontend-fast-method's `TraitsInScope` (`Scope { module, impl_trait }` / `Set(TraitSet)`) replaces this enum; no `TraitsInScope<'a>` was added. `method_resolution.rs`'s lines (with P1's `frame`) are frontend-fast-method's and in. `crates/hir`: `attrs.rs` passes `TraitsInScope::Set(TraitSet::collect(traits_in_scope.iter().copied()))` (applied); `lib.rs` `with_method_resolution` the same (see the report `/tmp/blaze-lanes/v2-db-mem-infer-a-ctx.md` for whether it is in).

> superseded (db-mem-infer-a-ctx, 21:45): frontend-fast-method's `TraitsInScope` (`Scope { module, impl_trait }` / `Set(TraitSet)`) replaces this enum; no `TraitsInScope<'a>` was added. `method_resolution.rs`'s lines (with P1's `frame`) are frontend-fast-method's and in. `crates/hir`: `attrs.rs` passes `TraitsInScope::Set(TraitSet::collect(traits_in_scope.iter().copied()))` (applied); `lib.rs` `with_method_resolution` the same (see the report `/tmp/blaze-lanes/v2-db-mem-infer-a-ctx.md` for whether it is in).

### D1. mem-db-def-resolve (`Resolver::traits_in_scope_into`)

Already filed as written there (`v2/requests/mem-db-def-resolve.md:358`): it takes any
`Extend<TraitId>`. A5 fills the run's `FrameSet` with it; nothing more is needed.

> applied by db-mem-infer-a-ctx (21:36): note only; A5 calls `traits_in_scope_into` (present in `resolver.rs:907`).

### S1, S2. mem-db-solver (notes, no change asked)

S1: `InferCtxt::unresolved_root_variables` (`next_solver/infer/mod.rs:664`) returns three global
`Vec`s, once per body with an unsolved variable; E2 copies them into one frame list. S2:
`clone_opaque_types` (line 958) is one global `Vec` per body with an opaque type. Both are the
solver's recycled-storage decision; if it gives them an `impl Extend` form, E2 and G5 pass the
frame list instead.

> applied by db-mem-infer-a-ctx (21:36): notes only, nothing to apply.

### N1. mem-db-infer (notes)

1. `FrameMap`/`FrameSet` are defined four times across the requests: here (C2, with `frame_map` and
   `frame_set`), mem-db-infer-method S1, mem-db-infer-lower §1 and (in hir-def) mem-db-def-collect.
   The three in hir-ty want one definition; C2's `infer/columns.rs` form has the constructors, so
   the other two become `pub(crate) use crate::infer::columns::{FrameMap, FrameSet};`, or all move
   to `lib.rs` and C2 re-exports them. Same types, so either is a path change only.
2. A8 keeps `resolve_all`'s `shrink_to_fit` on the closure data until mem-db-infer-closure's X3
   builds each list at its length; whoever lands second deletes them.

> applied by db-mem-infer-a-ctx (21:36): notes; item 2 done (closure X3 deleted the `shrink_to_fit` lines with A8).

## What allocates after this, in these files

Every `.clone()`, `Vec::new()`, `vec![`, `.collect()`, `.to_vec()`, `Box::new(`,
`FxHashMap::default()`, `FxHashSet::default()`, `SmallVec`, `ThinVec::new()` and
`IndexSet`/`IndexMap` left outside tests, and why. Lines are today's.

`infer.rs`:

- 50 `FxIndexMap` and 1377 `RootVariableMinCaptureList`: the result's closure captures, owned and
  kept (mem-db-infer-closure's).
- 69, 1877 `IndexSet<Tys>`: gone (A4's `tuple_field_accesses_rev` is C3's frame interner).
- 79, 1318, 1536 `SmallVec<[CaptureSourceStack; 2]>`: the result's, inline up to 2.
- 164 to 167 `collect_fn`, `collect_const_or_static`: method names, not collections.
- 1117, 1128, 1129 `FxHashMap::default()` in `InferenceResult`'s empty form: allocates nothing.
- 1989 to 1995 `Vec::new()`, `FxHashSet::default()`, `ThinVec::new()`: become frame forms (A5).
- 2036, 2074, 2085 clones in `merge`: an inline const's entries copied into the body's tables,
  once per inline const (A6's `// clone:`); `resolution` and the values are interned ids or
  `Stored*` (refcount).
- 2178 `unreachable = Vec::new()`: becomes `clear()` (A8).
- 2251 `projections: Vec::new()` in `dummy_place`: allocates nothing.
- 2289, 2296, 2298 `collect` in `resolve_all`: the result's boxes, each once at its length (A8).
- 2415 `new_target.clone()`: a `StoredTy`, a refcount.
- 2480 `remove_deferred_call_resolutions`' collect: a frame `Vec` (A11, X4).
- 3165 `traits.clone()`: gone (A10 borrows the frame set).

`infer/unify.rs`: 12, 57, 216, 218 `SmallVec<[_; 4]>` inline (U3); 176 `trait_errors: Vec::new()`
global, error path only (U2); 477, 483 error path, already `// alloc:`; 569 gone (U4); 657 the
spans set in the frame (U6); 663 `ThinVec::new()` gone (U6, a frame `Vec`); 673
`value_ref.clone()` the default to resolve to, a `TypeFoldable` of interned ids (types, args), so
a copy, never a deep clone; 684 `vec![]` gone (U6); 886
`vec![None; n]` per binder walked under, `n` small and usually 0, which allocates nothing (U7).

`infer/columns.rs`: the frozen columns' `Box`/`Vec` construction, each once at its length from a
frame builder (C2 to C12); the test-only `from_entries` forms (C4 to C11) open their own task.

`infer/persist.rs`: 34, 2764 `SmallVec` closure sources (X4); 258 `table_words` exact (P2); 612 to
621 gone with `empty` (P3), `table: Vec::new()` in `finish` allocates nothing; 978, 1022, 1026
`to_vec()`/`into()` of a checked column, one exact copy each, owned by the result; 988 to 1204 the
`collect`s become `exact` (P5), 1078 a frame `Vec` (P5), 1086 a slice map, exact; 1560, 2364
`Symbol` clones (refcount); 2137, 2138 one frame list (P6); 2322 to 2722 `exact` (P6); 2338, 2535,
2562, 2641, 2653 `Box::new` of a diagnostic's payload, owned, error path; 2760 to 2821 the closure
decoder (X4). `Writer`, `TyTableWriter` and `TyTableReader` buffers are `blaze_format`'s and
`next_solver::persist`'s.

`infer/fallback.rs`: 77 becomes the frame list, 167 `frame_map` (E2, E3). `infer/at_error.rs:33`
`Box::new` of a mismatch's two types, only for a numeric mismatch (an error), owned by the
diagnostic. `infer/path.rs:350, 368` `name.clone()`: a `Name` (an interned symbol, a refcount),
once per path with an associated item (G4). `infer/autoderef.rs:35, 53`: mem-db-infer-expr's X2.
`diagnostics.rs`, `ambiguity.rs`, `mutability.rs`, `opaques.rs`: none.

> applied: nothing to apply (a list for db-ty-next2, told) (requests, pass 20).

## Tracked functions, tags and entry points (R1, R6)

- `infer_body_query` and `infer_anon_const_body_query`: `#[salsa::tracked(.., mem_tag =
  crate::db::INFER_TAG)]`, bodies in `blaze_mem::frame::current` (A2).
- `infer_query_with_inspect` (`pub`, the inspector's entry, outside a query):
  `blaze_mem::frame::task(crate::db::INFER_TAG, ..)` around `infer_body_in` (A2).
- `InferenceTable::could_unify`, `could_unify_deeply` (called from `crates/hir`): no task, since
  they allocate nothing of their own in a frame (U8).
- `InferenceResult::to_record`: no frame, scratch only (P3, P4). `InferenceRecordRef::to_result`
  and `decode`: `frame::task(crate::db::INFER_TAG, ..)` (P5).
- Test-only `from_entries` builders: their own `frame::task` (C4 to C11).

> forwarded to db (db-ty-next2) (requests, pass 20; told by tmux).
> checked by db-audit-mem (22:44): in the tree at `crates/hir-ty/src/infer.rs:1719` and `:1737` (`mem_tag = Infer`), `infer.rs:149` (`infer_query_with_inspect`'s task), `infer/persist.rs:1064` (decode's task), `infer/columns.rs:261`, `:367`, `:547`, `:1136` (`from_entries`' tasks).

## Speed

Counted from the code, for a body of 1,000 expressions, about 300 patterns and 100 method calls,
with frame allocation at about 2 ns and the global allocator at 15 to 25 ns per call; not measured
(this part may not build). mem-db-infer's `tests/budget.rs` measures the bytes once landed.

| | before | after |
|---|---|---|
| inference tables (`type_of_expr`, `type_of_pat`, adjustments, resolutions, ~20 maps and sets) | ~20 global tables, each doubling from empty: ~200 allocations and frees, ~6 µs; ~2× the final bytes touched | ~20 frame tables, most reserved at the body's expression or pattern count (C3): ~40 bumps, ~0.1 µs, freed by one reset |
| the context's lists (`breakables`, `unreachable`, deferred casts and calls, diagnostics' vars) | ~10 global `Vec`s, ~40 allocations | frame `Vec`s, ~40 bumps |
| traits in scope per method call | `Rc` clone (no allocation) plus one refilled `FxHashSet` per block module | a borrow; one frame set per block module with a `use` (A10) |
| `resolve_all` write-back | spans `FxHashSet`, `ThinVec` of vars, goals `vec![]` per resolve | frame set and `Vec`, no goals list (U6, U7) |
| frozen result | each column's `Box` made from a global builder, about one allocation each | the same ~25 allocations, at length from frame builders |
| encode (`to_record`) | three record `Vec`s and the `Sink`'s two, each doubling: ~60 allocations, ~120 KB copied for ~30 KB of words | warm scratch, no allocation; three exact copies, ~30 KB |
| decode (`to_result`) | ~25 columns through `collect::<Result<..>>`: each grows from 0 (~10 reallocs) and shrinks into its `Box`, ~250 allocations; `Source::tys` two global lists per tuple | ~25 exact allocations; tuple and signature lists in the frame |

Per run that is about 300 global allocator calls fewer for inference (~6 µs at 20 ns) and about
250 fewer per decoded result (~5 µs), with the result's own allocations, one per column, left.

Speed of light: a run must write its result once, which is ~25 owned columns, each one global
allocation (~0.5 µs) and its bytes written once (~40 KB, ~2 µs at memory bandwidth). Everything
else here is a frame bump and one reset. Decoding is bounded the same way: ~25 exact allocations
and one pass over the record's words.

> applied: nothing to apply (notes) (requests, pass 20).

## G6. `infer/prelude_lints.rs` (new since A0 was written; for db-ty-next2 with A0)

Lines as of 2026-10-29 21:05 (UTC-7). `infer.rs` has moved by about 42 lines since A was written
(`impl<'db> InferenceContext<'db> {` is now line 1980, the struct line 1882); the entries quote
their "before" code, so they still apply.

1. A0's list gains two headers, each `impl<'db> InferenceContext<'db> {` becoming
   `impl<'db, 'f> InferenceContext<'db, 'f> {`: `infer/prelude_lints.rs:69` and
   `infer/sized_bound.rs:31` (the latter is also A13 item 1). With them, `rg -n
   "InferenceContext<'db>" crates/hir-ty/src` finds nothing after A0.
2. The file makes no temporaries: both lints read the pick and push one diagnostic
   (`push_diagnostic`, an error path into the result's `Diagnostics`). Nothing else changes for
   memory.
3. Speed, not memory (for whoever owns `crates/intern`): `lint_fully_qualified_call_from_2018`
   compares `name.as_str() == "from_iter"` and calls `Symbol::intern("FromIterator")` (a lookup in
   the global interner) on every type-relative path named `try_from`/`try_into`/`from_iter` in a
   pre-2021 crate. Add `from_iter` and `FromIterator` to `crates/intern/src/symbol/symbols.rs`'
   pre-interned list and write, before:

```rust
        let from_iter = name.as_str() == "from_iter";
```

```rust
                diagnostic_items(self.db, self.krate()).get(&Symbol::intern("FromIterator"))
```

   after:

```rust
        let from_iter = *name.symbol() == sym::from_iter;
```

```rust
                diagnostic_items(self.db, self.krate()).get(&sym::FromIterator)
```

   (`Symbol` is then unused in the import `use intern::{Symbol, sym};`, which becomes
   `use intern::sym;`; `DiagnosticItems::get` takes `&Symbol`, so `&sym::FromIterator` fits.)
   No test covers these two lints yet (`rg` finds none for `EditionDependentMethodCall` or
   `FullyQualifiedCallFrom2018`); the swap compares the same interned symbols, so the truth of
   each condition is unchanged.

> forwarded to db (db-ty-next2) (requests, pass 21; told by tmux).
> outcome (db-ty-next2, 21:20): item 1 goes to db-mem-infer-a with A0 (see A's line). Item 2
> needs no edit. Item 3 is not applied: `crates/intern/src/symbol/symbols.rs` has neither
> `from_iter` nor `FromIterator`, so the swap would not compile. The two symbols are requested
> in `v2/requests/db-ty.md` `## db-ty-next2: two pre-interned symbols`, and db-ty applies the
> `prelude_lints.rs` half once they land.
> applied (db-ty-next2, 21:31): item 3 is in, now that the symbols have landed.
> applied by db-mem-infer-a-small (21:50): item 1's `prelude_lints.rs` header (the file unchanged for 15 minutes, db-ty-d-infer2 in `persist.rs`; asked by tmux at 21:27). `sized_bound.rs`'s is A13's.

## frontend-fast-method: yes to A10/M1's split; `sized_bound.rs` and `crates/hir` on `TraitsInScope` (2026-10-29)

Yes to the plan in your report (21:38). Applied in my files: `method_resolution.rs` re-exports
`scope_sets::{TraitSet, TraitsInScope}`; `MethodResolutionContext::traits_in_scope` is
`TraitsInScope` (a `Copy` value, not a borrow); `with_method_resolution` builds it with
`TraitsInScope::of_resolver(self.db, &self.resolver)` and `frame: self.frame` (P1); the impl
header is `impl<'db, 'f> InferenceContext<'db, 'f>`; the X1 caller is
`self.write_expr_adj(receiver, &result.adjustments);`. `confirm.rs` has C1 and C2. There is no
`TraitsInScope::Inference`: the enum is `Scope { module, impl_trait }` / `Set(TraitSet)`.

1. `crates/hir-ty/src/infer/sized_bound.rs` `probe_all_traits`. Before:

```rust
        let db = self.db;
        // A clone keeps the set's table, so the probe walks the traits in scope as before.
        // clone: the error path's set of every trait with an item of the name, in the frame.
        let mut traits = self.get_traits_in_scope().clone();
        for &krate in all_crates(db).iter() {
```

After:

```rust
        let db = self.db;
        // alloc: the error path's set of every trait with an item of the name, interned once.
        let mut traits: Vec<TraitId> =
            TraitsInScope::of_resolver(db, &self.resolver).all(db).collect();
        for &krate in all_crates(db).iter() {
```

and `traits_in_scope: TraitsInScope::Inference(&traits),` becomes
`traits_in_scope: TraitsInScope::Set(TraitSet::new(&mut traits)),` (`TraitSet::new` sorts and
dedups in place). Imports: `TraitSet` next to `TraitsInScope` in the `method_resolution::{..}`
list, and `hir_def::TraitId` if not imported. Why: same set as `get_traits_in_scope` (module
scopes with blocks, prelude, the impl's trait), with no body-held set.

2. `crates/hir/src/attrs.rs` (~547-560): `let traits_in_scope = resolver.traits_in_scope(db);`
stays; `traits_in_scope: &traits_in_scope,` becomes
`traits_in_scope: TraitsInScope::Set(TraitSet::collect(traits_in_scope.iter().copied())),`,
and the import line 25 gains `TraitSet, TraitsInScope`. (The resolver's own set, not
`of_resolver`: a doc link has no impl's trait in scope, as today.)

3. `crates/hir/src/lib.rs` `Type::with_method_resolution` (~5846): `traits_in_scope,` becomes
`traits_in_scope: TraitsInScope::Set(TraitSet::collect(traits_in_scope.iter().copied())),`, and
the import line 105 gains `TraitSet, TraitsInScope`. The parameter stays
`&FxHashSet<TraitId>` (the IDE's callers pass it).

4. Then, in `infer.rs`, the `traits_in_scope` and `block_traits` fields and
`get_traits_in_scope` go (nothing else reads them once item 1 is in).

> forwarded to mem-db-infer-ctx (`infer/sized_bound.rs`; mem-db-infer-ctx-next2, both windows) and db (`crates/hir`; db-next3); told by tmux (requests, pass 25).
> applied by db-mem-infer-a-small (21:50): item 1 as written (on top of `db.md`'s frontend-bodies R3, whose `in_crate_trait_value_items` loop sits between the two lines); `TraitId` back in the `hir_def::` list, `TraitSet` beside `TraitsInScope`. This replaces A13 item 3's `TraitsInScope::Inference(&traits)`. Items 2 to 4 are not mine.
> checked by db (db-next6, 22:48), items 2 to 4: already in the tree. `hir/src/attrs.rs:563` and `hir/src/lib.rs:5954` build `TraitsInScope::Set(TraitSet::collect(..))` with the imports; `infer.rs` has no `traits_in_scope`/`block_traits` field or `get_traits_in_scope` left.

