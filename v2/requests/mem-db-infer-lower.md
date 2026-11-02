# Requests of mem-db-infer-lower

Part `lower` of mem-db-infer (`v2/requests/mem-db-infer.md`, design and rules R1 to R8 in
`docs/v2/plan/lanes/mem/db-infer.md`, notes in `docs/v2/plan/lanes/mem/db-infer-lower.md`).
Files of `crates/hir-ty/src/`: `lower.rs`, `lower/{batch,diagnostics,path,persist}.rs`,
`generics.rs`, `utils.rs`, `variance.rs`, `variance/unused.rs`, `dyn_compatibility.rs`,
`builtin_derive.rs`, `effects.rs`. All are lane db's (db-ty-lower and its siblings), none is done,
so every change here is a request. The changes need mem-db-infer's 1 to 3 first: the `blaze-mem`
and `hashbrown` (feature `nightly`) dependencies, `#![feature(allocator_api)]`, and
`crate::db::INFER_TAG`. They also need `blaze_mem::frame::current` (mem-db request 1).

Quoted "before" code is as of 2026-10-29. Lane db is editing these files as this is written, and
line numbers move: find each site by the quoted lines. Several temporaries have just become
`SmallVec`s (`bound_vars`, `elisions`, `of_arguments`, `own_predicates`, `substs`). The frame
forms below replace them, because a frame list has no spill cliff and no inline copy. Each
change holds whether `db` is `&dyn HirDatabase` or `&Db`: none of them touches `db`.

Order: 1 to 9 (`lower.rs` types and the context), then 10 to 13 (`lower.rs` queries and helpers),
14 (`path.rs`), 15 (`batch.rs`), 16 (`persist.rs`), 17 to 21 (the other files), then 22 (the
dependent edits in other parts' files, which must land in the same change as 2 and 11 to 13
or the crate does not build).

## 1. lane db: frame collections, `crates/hir-ty/src/lower.rs`

After the `use smallvec::SmallVec;` line in the `use` block:

```rust
use blaze_mem::frame::Frame;
use rustc_hash::FxBuildHasher;

/// A set of a lowering run, in its frame (R3).
pub(crate) type FrameSet<'f, T> = hashbrown::HashSet<T, FxBuildHasher, &'f Frame>;
/// A map of a lowering run, in its frame (R3).
pub(crate) type FrameMap<'f, K, V> = hashbrown::HashMap<K, V, FxBuildHasher, &'f Frame>;

/// A memo's list of `items`, allocated once at its length: nothing when it is empty (R4).
pub(crate) fn exact_thin<T>(
    items: impl IntoIterator<Item = T, IntoIter: ExactSizeIterator>,
) -> ThinVec<T> {
    let items = items.into_iter();
    let mut list = ThinVec::with_capacity(items.len()); // alloc: a memo's own list, exact size
    list.extend(items);
    list
}
```

(If mem-db-infer prefers these in `lib.rs` for every part, they move there unchanged.)

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): the aliases are `lib.rs`'s (mem-db-infer 5): `lower.rs` has `pub(crate) use crate::{FrameMap, FrameSet};` and `use crate::{frame_map, frame_set};` (used in place of `with_hasher_in(FxBuildHasher, frame)` below), `exact_thin` as written, `use blaze_mem::Frame;`.

## 2. lane db: `TyLoweringContext` takes its run's frame (R2), `lower.rs`

`ImplTraitLoweringState`. Before:

```rust
#[derive(Debug, Default)]
struct ImplTraitLoweringState {
    ...
    opaque_type_data: ThinVec<SelfAndAssocBounds>,
}

impl ImplTraitLoweringState {
    fn new(mode: ImplTraitLoweringMode) -> ImplTraitLoweringState {
        Self { mode, opaque_type_data: ThinVec::new() }
    }
}
```

After (it loses `Default`, which nothing uses):

```rust
#[derive(Debug)]
struct ImplTraitLoweringState<'a> {
    ...
    opaque_type_data: Vec<SelfAndAssocBounds, &'a Frame>,
}

impl<'a> ImplTraitLoweringState<'a> {
    fn new(mode: ImplTraitLoweringMode, frame: &'a Frame) -> ImplTraitLoweringState<'a> {
        Self { mode, opaque_type_data: Vec::new_in(frame) }
    }
}
```

`Elision`. Before: `struct Elision<'db> { ... of_arguments: SmallVec<[Region<'db>; 4]>, ... }`.
After: `struct Elision<'db, 'a> { ... of_arguments: Vec<Region<'db>, &'a Frame>, ... }`. In
`with_elision`, `of_arguments: SmallVec::new()` becomes `of_arguments: Vec::new_in(self.frame)`,
and `signature_mut` returns `Option<&mut Elision<'db, 'a>>`. Its other uses (`as_slice`, `push`,
`contains`, `len`) are `Vec`'s.

The fields. Before:

```rust
    impl_trait_mode: ImplTraitLoweringState,
    ...
    pub(crate) unsized_types: FxHashSet<Ty<'db>>,
    pub(crate) diagnostics: ThinVec<TyLoweringDiagnostic>,
    ...
    pub(crate) defined_anon_consts: ThinVec<AnonConstId<'db>>,
    ...
    bound_vars: SmallVec<[(Vec<Name>, BoundVarKinds<'db>); 2]>,
    elisions: SmallVec<[Elision<'db>; 2]>,
    ...
    lifetime_lowering_mode: LifetimeLoweringMode,
}
```

After:

```rust
    impl_trait_mode: ImplTraitLoweringState<'a>,
    ...
    pub(crate) unsized_types: FrameSet<'a, Ty<'db>>,
    pub(crate) diagnostics: Vec<TyLoweringDiagnostic, &'a Frame>,
    ...
    pub(crate) defined_anon_consts: Vec<AnonConstId<'db>, &'a Frame>,
    ...
    /// The names the binders in scope bind, outermost first: binder `i`'s are
    /// `bound_names[bound_vars[i].0..]`, up to the next binder's start.
    bound_names: Vec<Name, &'a Frame>,
    bound_vars: Vec<(u32, BoundVarKinds<'db>), &'a Frame>,
    elisions: Vec<Elision<'db, 'a>, &'a Frame>,
    ...
    lifetime_lowering_mode: LifetimeLoweringMode,
    /// The frame of the run lowering: its query's, or that of the inference that made it.
    pub(crate) frame: &'a Frame,
}
```

`new` takes the frame last. Before:

```rust
        lifetime_lowering_mode: LifetimeLoweringMode,
    ) -> Self {
        let impl_trait_mode = ImplTraitLoweringState::new(ImplTraitLoweringMode::Disallowed);
        let in_binders = DebruijnIndex::ZERO;
        let interner = DbInterner::new_with(db, resolver.krate());
        let bound_vars = smallvec::smallvec![(
            Vec::new(),
            TyLoweringContext::bound_vars(db, generic_def, generics)
        )];
```

After:

```rust
        lifetime_lowering_mode: LifetimeLoweringMode,
        frame: &'a Frame,
    ) -> Self {
        let impl_trait_mode =
            ImplTraitLoweringState::new(ImplTraitLoweringMode::Disallowed, frame);
        let in_binders = DebruijnIndex::ZERO;
        let interner = DbInterner::new_with(db, resolver.krate());
        let mut bound_vars = Vec::with_capacity_in(1, frame);
        bound_vars.push((0, TyLoweringContext::bound_vars(db, generic_def, generics)));
```

and in the `Self { .. }` literal:

```rust
            unsized_types: FrameSet::with_hasher_in(FxBuildHasher, frame),
            diagnostics: Vec::new_in(frame),
            ...
            defined_anon_consts: Vec::new_in(frame),
            ...
            bound_names: Vec::new_in(frame),
            bound_vars,
            elisions: Vec::new_in(frame),
            ...
            lifetime_lowering_mode,
            frame,
```

`with_impl_trait_mode`. Before:
`Self { impl_trait_mode: ImplTraitLoweringState::new(impl_trait_mode), ..self }`. After:
`Self { impl_trait_mode: ImplTraitLoweringState::new(impl_trait_mode, self.frame), ..self }`.
The replaced state's list is empty and frees nothing.

`with_shifted_in` and `with_debruijn` take `f: impl FnOnce(&mut Self) -> T` in place of
`impl FnOnce(&mut TyLoweringContext<'db, '_>) -> T`. With a higher-ranked `'_`, the closure could
not return the bound iterators of 7, which now hold `&'a Frame`. Each caller's closure is at
least as general as the new bound, so no caller changes.

What a context costs: `new` makes one frame allocation, 16 bytes for the item's binder. Today it
makes no global allocation (the `SmallVec` is inline), but a push of a `for<'a>` binder
allocates its names (`binder.to_vec()`). After this change no context makes a global allocation
until it lowers something that has diagnostics.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written, with `frame_set(frame)` for `unsized_types`; dedyn-ty's `infer_vars: Option<InferVars<'a, 'db>>` kept. Outside callers told: borrowck `regions.rs` (db-mem-borrowck), `traits.rs` (db-mem-infer-b), `source_analyzer/view.rs` (frontend-tree-move-hir-next1), `tests/infer_vars.rs` (dedyn-ty).

## 3. lane db: binder names in one list, `lower.rs`

`push_bound_vars` and `pop_bound_vars`. Before:

```rust
    fn push_bound_vars(&mut self, binder: &[Name]) {
        let bound_vars = BoundVarKinds::new_from_iter(binder.iter().map(for_lifetime_kind));
        self.bound_vars.push((binder.to_vec(), bound_vars));
    }

    fn pop_bound_vars(&mut self) -> BoundVarKinds<'db> {
        self.bound_vars.pop().unwrap().1
    }
```

After:

```rust
    fn push_bound_vars(&mut self, binder: &[Name]) {
        let bound_vars = BoundVarKinds::new_from_iter(binder.iter().map(for_lifetime_kind));
        self.bound_vars.push((self.bound_names.len() as u32, bound_vars));
        self.bound_names.extend(binder.iter().cloned());
    }

    fn pop_bound_vars(&mut self) -> BoundVarKinds<'db> {
        let (start, bound_vars) = self.bound_vars.pop().unwrap();
        self.bound_names.truncate(start as usize);
        bound_vars
    }
```

`peek_bound_vars` and every `self.bound_vars.len()` stay as they are. In `lower_type_bound`, the
`of_assoc_ty` arm. Before:

```rust
                let top = self.bound_vars.last_mut().unwrap();
                let (names, kinds) = (top.0.len(), top.1);
                top.0.extend(binder.iter().cloned());
                top.1 = BoundVarKinds::new_from_iter(
                    kinds.iter().chain(binder.iter().map(for_lifetime_kind)),
                );
                self.with_elision(false, |ctx| lower_path_bound(ctx, path));
                let top = self.bound_vars.last_mut().unwrap();
                top.0.truncate(names);
                top.1 = kinds;
```

After (the innermost binder's names are the end of `bound_names`):

```rust
                let (names, kinds) = (self.bound_names.len(), self.peek_bound_vars());
                self.bound_names.extend(binder.iter().cloned());
                self.bound_vars.last_mut().unwrap().1 = BoundVarKinds::new_from_iter(
                    kinds.iter().chain(binder.iter().map(for_lifetime_kind)),
                );
                self.with_elision(false, |ctx| lower_path_bound(ctx, path));
                self.bound_names.truncate(names);
                self.bound_vars.last_mut().unwrap().1 = kinds;
```

`find_and_lower_hrtb_lifetime`. Before:

```rust
        if let LifetimeRef::Named(lt_name) = &self.store[lifetime] {
            self.bound_vars.iter().rev().enumerate().find_map(|(debruijn, (binder, _))| {
                binder.iter().enumerate().find_map(|(index, l)| {
```

After (the same first match, innermost binder first):

```rust
        if let LifetimeRef::Named(lt_name) = &self.store[lifetime] {
            let mut end = self.bound_names.len();
            for (debruijn, &(start, _)) in self.bound_vars.iter().rev().enumerate() {
                let names = &self.bound_names[start as usize..end];
                // A `for<'a>` binds its lifetime in a body as much as in a signature.
                if let Some(index) = names.iter().position(|l| l == lt_name) {
                    return Some(Region::new_bound(
                        self.interner,
                        DebruijnIndex::from_usize(debruijn),
                        BoundRegion {
                            var: BoundVar::from_u32(index as u32),
                            kind: for_lifetime_region_kind(&names[index]),
                        },
                    ));
                }
                end = start as usize;
            }
            None
        } else {
```

Test. hir-ty has no test database: its lowering tests are hir's, in
`crates/hir/src/tests/lower.rs`, beside `a_for_lifetime_binds_its_name` and in its style. Every
test here that needs a database goes there. This one holds nested binders, which the flat names
change:

```rust
#[test]
fn a_lifetime_is_the_innermost_binders_that_names_it() {
    let db = TestDB::with_files(
        r#"
//- minicore: sized
type G = for<'a> fn(&'a u8, for<'a, 'b> fn(&'a u8, &'b u8));
"#,
    );
    crate::attach_db(&db, || {
        let (_, ModuleDefId::TypeAliasId(alias)) = type_item(&db, "G") else {
            panic!("`G` is a type alias")
        };
        let ty = db.ty(TyDefId::TypeAliasId(alias)).skip_binder();
        let TyKind::FnPtr(outer, _) = ty.kind() else { panic!("`G` is a fn pointer: {ty:?}") };
        let outer = outer.skip_binder().inputs();
        let TyKind::FnPtr(inner, _) = outer[1].kind() else { panic!("a fn pointer: {ty:?}") };
        let inner = inner.skip_binder().inputs();
        // (binder, var) of a reference's region, the binder counted from the innermost.
        let bound = |ty: Ty<'_>| match ty.kind() {
            TyKind::Ref(region, ..) => match region.kind() {
                RegionKind::ReBound(BoundVarIndexKind::Bound(binder), it) => {
                    (binder.as_u32(), it.var.as_u32())
                }
                it => panic!("a bound region, not {it:?}"),
            },
            it => panic!("a reference, not {it:?}"),
        };
        assert_eq!(bound(outer[0]), (0, 0), "the outer `'a`");
        assert_eq!(bound(inner[0]), (0, 0), "the inner `'a` shadows the outer");
        assert_eq!(bound(inner[1]), (0, 1), "`'b`");
    });
}
```

(`Ty` from `hir_ide::next_solver`, and `RegionKind` and `BoundVarIndexKind` from wherever
`lower.rs` imports them.)

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written; the `of_assoc_ty` arm keeps db-apply-ty's `let binder = self.store.names(binder);` first. Test in `crates/hir/src/tests/lower.rs`.

## 4. lane db: owned results made once, and the frame form of a lowering, `lower.rs`

`take_defined_opaques`. Before:

```rust
    fn take_defined_opaques(&mut self) -> ThinVec<SelfAndAssocBounds> {
        self.impl_trait_mode.opaque_type_data.shrink_to_fit();
        mem::take(&mut self.impl_trait_mode.opaque_type_data)
    }
```

After (one exact allocation, none when there are no `impl Trait`s):

```rust
    fn take_defined_opaques(&mut self) -> ThinVec<SelfAndAssocBounds> {
        exact_thin(self.impl_trait_mode.opaque_type_data.drain(..))
    }
```

`TyLoweringResult`. Before:

```rust
    pub(crate) fn new(
        value: T,
        mut diagnostics: ThinVec<TyLoweringDiagnostic>,
        mut defined_anon_consts: ThinVec<AnonConstId<'db>>,
    ) -> Self {
        diagnostics.shrink_to_fit();
        defined_anon_consts.shrink_to_fit();
        Self { value, diagnostics, anon_consts: defined_anon_consts }
    }

    fn from_ctx(value: T, ctx: TyLoweringContext<'db, '_>) -> Self {
        Self::new(value, ctx.diagnostics, ctx.defined_anon_consts)
    }
    ...
    /// The value with what lowering it found, taken apart.
    pub(crate) fn into_parts(
        self,
    ) -> (T, ThinVec<TyLoweringDiagnostic>, ThinVec<AnonConstId<'db>>) {
        (self.value, self.diagnostics, self.anon_consts)
    }
```

After (`into_parts` goes: the batch copies from a `FrameLowered`):

```rust
    /// `value` with what lowering it found, each list allocated once at its length (none
    /// when empty, as for most items).
    pub(crate) fn new(
        value: T,
        diagnostics: impl IntoIterator<Item = TyLoweringDiagnostic, IntoIter: ExactSizeIterator>,
        defined_anon_consts: impl IntoIterator<Item = AnonConstId<'db>, IntoIter: ExactSizeIterator>,
    ) -> Self {
        Self {
            value,
            diagnostics: exact_thin(diagnostics),
            anon_consts: exact_thin(defined_anon_consts),
        }
    }

    fn from_ctx(value: T, mut ctx: TyLoweringContext<'db, '_>) -> Self {
        Self::new(value, ctx.diagnostics.drain(..), ctx.defined_anon_consts.drain(..))
    }
```

After `impl<'db, T> TyLoweringResult<'db, T>`, add the frame form that the four lowerings shared
by a per-item query and a crate's batch return (15):

```rust
/// A signature lowered in a run, with what lowering found still in the run's frame: a per-item
/// query makes its owned result of it ([`FrameLowered::owned`]), a crate's batch copies it
/// into its columns.
pub(crate) struct FrameLowered<'f, 'db, T> {
    pub(crate) value: T,
    pub(crate) diagnostics: Vec<TyLoweringDiagnostic, &'f Frame>,
    pub(crate) anon_consts: Vec<AnonConstId<'db>, &'f Frame>,
}

impl<'f, 'db, T> FrameLowered<'f, 'db, T> {
    /// `value` with what `ctx` found, in `frame`: `ctx`'s lists live as long as its resolver's
    /// borrow, shorter than the caller's frame. Both are empty for most items, and an empty
    /// list moves nothing.
    fn from_ctx(value: T, mut ctx: TyLoweringContext<'db, '_>, frame: &'f Frame) -> Self {
        fn moved<'f, T>(from: &mut Vec<T, &Frame>, frame: &'f Frame) -> Vec<T, &'f Frame> {
            let mut to = Vec::with_capacity_in(from.len(), frame);
            to.extend(from.drain(..));
            to
        }
        FrameLowered {
            value,
            diagnostics: moved(&mut ctx.diagnostics, frame),
            anon_consts: moved(&mut ctx.defined_anon_consts, frame),
        }
    }

    fn empty(value: T, frame: &'f Frame) -> Self {
        FrameLowered { value, diagnostics: Vec::new_in(frame), anon_consts: Vec::new_in(frame) }
    }

    pub(crate) fn owned(self) -> TyLoweringResult<'db, T> {
        TyLoweringResult::new(self.value, self.diagnostics, self.anon_consts)
    }

    /// As [`FrameLowered::owned`], the value made owned by `f`.
    pub(crate) fn owned_with<U>(self, f: impl FnOnce(T) -> U) -> TyLoweringResult<'db, U> {
        TyLoweringResult::new(f(self.value), self.diagnostics, self.anon_consts)
    }
}
```

Tests. Before (`mod tests`):

```rust
        let result = TyLoweringResult::<'static, u32>::new(7, ThinVec::new(), ThinVec::new());
    ...
        let mut diagnostics = ThinVec::with_capacity(8);
        diagnostics.extend([diagnostic(1), diagnostic(2)]);
        let result = TyLoweringResult::<'static, u32>::new(3, diagnostics, ThinVec::new());
        ...
        let (value, diagnostics, anon_consts) = result.clone().into_parts();
        assert_eq!((value, diagnostics.len(), anon_consts.len()), (3, 2, 0));
```

After:

```rust
        let result = TyLoweringResult::<'static, u32>::new(7, [], []);
    ...
        let mut diagnostics = Vec::with_capacity(8);
        diagnostics.extend([diagnostic(1), diagnostic(2)]);
        let result = TyLoweringResult::<'static, u32>::new(3, diagnostics, []);
        ...
        let clone = result.clone();
        assert_eq!((clone.value, clone.diagnostics().len(), clone.defined_anon_consts().len()), (3, 2, 0));
```

and one new test:

```rust
    #[test]
    fn a_frame_lowering_makes_one_allocation_per_list_with_something() {
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
            let mut lowered = FrameLowered::<'_, 'static, u32>::empty(5, frame);
            lowered.diagnostics.extend([diagnostic(1), diagnostic(2), diagnostic(3)]);
            let (result, used) = blaze_alloc_count::measure(|| lowered.owned());
            assert_eq!(used.allocations, 1, "the diagnostics, once; no anonymous constants");
            assert_eq!(result.diagnostics.capacity(), 3);
            let (_, used) = blaze_alloc_count::measure(|| FrameLowered::<'_, 'static, u32>::empty(1, frame).owned());
            assert_eq!(used.allocations, 0);
        });
    }
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written; `FrameLowered` derives `Debug` (`generic_predicates`' `tracing::instrument(.., ret)` prints it) and `empty` is `pub(crate)` (15 uses it).

## 5. lane db: `lower_closure_params` fills the caller's list, `lower.rs`

A frame `Vec` cannot leave `with_ty_lowering`'s higher-ranked closure, so the caller passes its
own list. Before:

```rust
    pub(crate) fn lower_closure_params(
        &mut self,
        params: &[Option<TypeRefId>],
    ) -> (Vec<Option<Ty<'db>>>, BoundVarKinds<'db>) {
        let (lowered, _) = self.with_shifted_in(&[], |ctx: &mut TyLoweringContext<'_, '_>| {
            ctx.with_elision(true, |ctx| {
                let params = params.iter().map(|&it| it.map(|it| ctx.lower_ty(it))).collect();
                (params, ctx.bound_vars_with_elided())
            })
        });
        lowered
    }
```

After:

```rust
    /// The types a closure's parameters are written with, pushed to `lowered`, and what their
    /// signature binds: ...
    pub(crate) fn lower_closure_params(
        &mut self,
        params: &[Option<TypeRefId>],
        lowered: &mut impl Extend<Option<Ty<'db>>>,
    ) -> BoundVarKinds<'db> {
        let (bound_vars, _) = self.with_shifted_in(&[], |ctx| {
            ctx.with_elision(true, |ctx| {
                lowered.extend(params.iter().map(|&it| it.map(|it| ctx.lower_ty(it))));
                ctx.bound_vars_with_elided()
            })
        });
        bound_vars
    }
```

The caller (`infer/closure.rs`, part closure) is in 22.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written; `infer/closure.rs:1021` already calls it this way.

## 6. lane db: `lower_fn_ptr`'s arguments, `lower.rs`

Before: `let mut args = Vec::with_capacity(fn_.params.len());`. After:
`let mut args = Vec::with_capacity_in(fn_.params.len(), self.frame);`. It runs once per fn pointer
type, its length is exact (the parameters and the return type), and it is interned by `Tys`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written.

## 7. lane db: bounds without a list per bound, `lower.rs`

`lower_type_bound` keeps an associated type's bounds in the frame and returns them as
`use<'a, 'db>`. Before:

```rust
    ) -> impl Iterator<Item = (Clause<'db>, GenericPredicateSource)> + use<'db> {
        let interner = self.interner;
        ...
                    if !ignore_bindings {
                        assoc_bounds = ctx
                            .assoc_type_bindings_from_type_bound(trait_ref, path.type_ref().into())
                            .map(|iter| iter.collect::<Vec<_>>());
                    }
```

After:

```rust
    ) -> impl Iterator<Item = (Clause<'db>, GenericPredicateSource)> + use<'a, 'db> {
        let interner = self.interner;
        let frame = self.frame;
        ...
                    if !ignore_bindings {
                        assoc_bounds = ctx
                            .assoc_type_bindings_from_type_bound(trait_ref, path.type_ref().into())
                            .map(|iter| {
                                let mut bounds = Vec::new_in(frame);
                                bounds.extend(iter);
                                bounds
                            });
                    }
```

`lower_where_predicate` collects nothing. The iterator `lower_type_bound` returns is already
computed: its clause and its associated bounds are made before it returns. Before:

```rust
        let lower_type_outlives = |ctx: &mut TyLoweringContext<'db, '_>,
                                   target: &TypeRefId,
                                   bound| {
            let self_ty = ctx.lower_ty(*target);
            let clause = ctx
                .lower_type_bound(bound, self_ty, ignore_bindings)
                .collect::<SmallVec<[_; 2]>>();
            Either::Left(clause.into_iter())
        };
```

After (needs 2's `with_shifted_in(impl FnOnce(&mut Self))`):

```rust
        let lower_type_outlives = |ctx: &mut TyLoweringContext<'db, 'a>,
                                   target: &TypeRefId,
                                   bound| {
            let self_ty = ctx.lower_ty(*target);
            Either::Left(ctx.lower_type_bound(bound, self_ty, ignore_bindings))
        };
```

Test: none new. This removes a copy of a list that is already computed, and the order and
binders of the clauses are what they were. The type-check corpus and the existing where-clause
tests hold them.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written.

## 8. lane db: `lower_dyn_trait`'s lists, `lower.rs`

Once per `dyn` type. Before:

```rust
            let mut auto_traits = SmallVec::<[_; 3]>::new();
            let mut projections = Vec::new();
            ...
            let mut projection_bounds = FxIndexMap::default();
            ...
            let mut ordered_associated_types = vec![];
            ...
            let mut projection_bounds: Vec<_> = ordered_associated_types
                .into_iter()
                .filter_map(|key| projection_bounds.get(&key).copied())
                .collect();
            ...
                    let args: Vec<_> = principal
                        .args
                        .iter()
                        // Skip `Self`
                        .skip(1)
                        .map(|arg| {
                            ...
                        })
                        .collect();

                    ExistentialPredicate::Trait(ExistentialTraitRef::new(
                        interner,
                        principal.def_id,
                        args,
                    ))
```

After (`auto_traits` stays: at most three auto traits are written, so it does not spill. The
map is read only by key and never iterated, so a frame hash map is the same as the index map.
`ExistentialTraitRef::new` takes `impl IntoIterator`, so the arguments need no list):

```rust
            let frame = self.frame;
            let mut auto_traits = SmallVec::<[_; 3]>::new();
            let mut projections = Vec::new_in(frame);
            ...
            let mut projection_bounds = FrameMap::with_hasher_in(FxBuildHasher, frame);
            ...
            let mut ordered_associated_types = Vec::new_in(frame);
            ...
            let mut projection_bounds = {
                let mut bounds = Vec::new_in(frame);
                bounds.extend(
                    ordered_associated_types
                        .iter()
                        .filter_map(|key| projection_bounds.get(key).copied()),
                );
                bounds
            };
            ...
                    let args = principal.args.iter().skip(1).map(|arg| {
                        if arg.walk().any(|arg| arg == dummy_self_ty.into()) {
                            // FIXME: Report an error.
                            self.types.types.error.into()
                        } else {
                            arg
                        }
                    });

                    ExistentialPredicate::Trait(ExistentialTraitRef::new(
                        interner,
                        principal.def_id,
                        args,
                    ))
```

`for proj in projections` iterates the frame `Vec` by value, unchanged. The type of
`projection_bounds.insert`, `contains_key` and `get` is the same with hashbrown. Test: the existing
`dyn` tests (`tests/traits.rs` `dyn_trait*`, `tests/display_source_code.rs`) hold the
projection order, which comes from `ordered_associated_types`, not from the map.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written, the map as `frame_map(frame)`; the `// Skip `Self`` comment moved above the `let args`.

## 9. lane db: `lower_impl_trait` (the context's method), `lower.rs`

Before:

```rust
        let mut predicates = Vec::new();
        let mut assoc_ty_bounds = Vec::new();
```

After:

```rust
        let mut predicates = Vec::new_in(self.frame);
        let mut assoc_ty_bounds = Vec::new_in(self.frame);
```

`predicates.extend(assoc_ty_bounds)` and `Clauses::new_from_slice(&predicates)` are unchanged
(the clauses are interned).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written.

## 10. lane db: the tracked queries of `lower.rs` (R1, R4, R6)

Each adds `mem_tag = crate::db::INFER_TAG` to its `#[salsa::tracked(..)]`. Each that makes
temporaries opens `blaze_mem::frame::current(|frame| { .. })` once, around its whole body, and
passes `frame` to `TyLoweringContext::new` and its helpers. Early returns and `?` stay inside the
closure. Allocation counts are of the global allocator, for the memo itself: interning
(`Clauses`, `Tys`, `GenericArgs`, `Stored*`) is the interner's and is not counted. d and a mean
one more allocation when the item has diagnostics, or defines anonymous constants.

| fn (line today) | result | frame temporaries | allocations after |
|---|---|---|---|
| `impl_trait_of_impl` (2025) | `Option<TyLoweringResult<StoredEarlyBinder<StoredTraitRef>>>` | the context | 0 + d + a |
| `type_for_const_with_diagnostics` (2195) | `TyLoweringResult<..>` | the context | 0 + d + a |
| `type_for_static_with_diagnostics` (2228) | `TyLoweringResult<..>` | the context | 0 + d + a |
| `type_for_type_alias_with_diagnostics` (2308) | `TyLoweringResult<WithDefinedOpaques<..>>` | context, opaque bounds | 0 + 1 if it defines `impl Trait`s + d + a |
| `impl_self_ty_of_impl` (2390) | `TyLoweringResult<StoredEarlyBinder<StoredTy>>` | the context | 0 + d + a |
| `const_param_types_with_diagnostics` (2456) | `TyLoweringResult<ConstParamTypes>` | the context | 1 when the item has const parameters, else 0; + d + a |
| `field_types_of_variant` (2614) | `TyLoweringResult<ThinVec<FieldType>>` | context, field list | 1 when it has fields, + d + a |
| `supertraits_info` (2714) | `SupertraitsInfo` | the three lists | 1 + 1 when it has associated types |
| `resolve_type_param_assoc_type_shorthand` (2806) | `AssocTypeShorthandResolution` | the context | 0 |
| `type_alias_bounds_with_diagnostics` (2978) | `TyLoweringResult<SelfAndAssocBounds>` | context, bounds, GAT map | 0 + d + a |
| `generic_predicates_of` (3336) | `TyLoweringResult<GenericPredicates>` | context, own predicates, the whole list | 0 + d + a |
| `trait_environment_query` (3481) | `StoredParamEnv`-like, interned | none of its own (`elaborate` is rustc_type_ir's) | 0, no frame |
| `const_conditions_query` (3502) | `StoredEarlyBinder<StoredClauses>` | context, conditions | 0 |
| `generic_defaults_with_diagnostics` (3757) | `TyLoweringResult<GenericDefaults>` | context, own defaults | 1 when the item or its parent has a default, + d + a |
| `fn_sig_for_fn` (3840) | `TyLoweringResult<WithDefinedOpaques<..>>` | two contexts, opaque bounds | 0 + 1 if it returns `impl Trait` + d + a |
| `elided_lifetimes` (4061) | `ElidedLifetimes` | five lists | 1 when it has parameters |
| `fn_sig_for_struct_constructor` (4263) | `StoredEarlyBinder<StoredPolyFnSig>` | none (`Tys::new_from_iter`) | 0, no frame |
| `fn_sig_for_enum_variant_constructor` (4271) | same | none | 0, no frame |

The per-query changes:

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): all 18 tags as `mem_tag = Infer` (salsa's option as filed, mem-db-salsa M2, takes a bare `blaze_mem::Tag` variant), not `crate::db::INFER_TAG`. Where a body is long or has early returns and `?` (`resolve_type_param_assoc_type_shorthand`, `type_alias_bounds_with_diagnostics`, `fn_sig_for_fn`, `elided_lifetimes`), the query is `blaze_mem::frame::current(|frame| <name>_in(.., frame))` and the body moved unchanged into `<name>_in`, as `infer_body_in` does; the same frame, the same allocations.

### 10a. `impl_trait_of_impl`, `lower_impl_trait` (the free fn)

Before:

```rust
#[salsa::tracked(returns(ref), cycle_result = impl_trait_with_diagnostics_cycle_result)]
fn impl_trait_of_impl<'db>(
    ...
) -> Option<TyLoweringResult<'db, StoredEarlyBinder<StoredTraitRef>>> {
    lower_impl_trait(db, impl_id, db.impl_self_ty(impl_id).skip_binder())
}

/// Lowers the trait `impl_id` implements, for the self type `self_ty`, which is the impl's.
pub(crate) fn lower_impl_trait<'db>(
    db: &'db Db,
    impl_id: ImplId,
    self_ty: Ty<'db>,
) -> Option<TyLoweringResult<'db, StoredEarlyBinder<StoredTraitRef>>> {
    ...
        LifetimeLoweringMode::Bound,
    );
    ctx.impl_header = true;
    let target_trait = impl_data.target_trait?;
    let trait_ref = ctx.lower_trait_ref(target_trait, self_ty)?;
    Some(TyLoweringResult::from_ctx(
        StoredEarlyBinder::bind(ctx.interner, StoredTraitRef::new(trait_ref)),
        ctx,
    ))
}
```

After:

```rust
#[salsa::tracked(
    returns(ref),
    cycle_result = impl_trait_with_diagnostics_cycle_result,
    mem_tag = crate::db::INFER_TAG
)]
fn impl_trait_of_impl<'db>(
    ...
) -> Option<TyLoweringResult<'db, StoredEarlyBinder<StoredTraitRef>>> {
    let self_ty = db.impl_self_ty(impl_id).skip_binder();
    blaze_mem::frame::current(|frame| {
        lower_impl_trait(db, impl_id, self_ty, frame).map(FrameLowered::owned)
    })
}

/// Lowers the trait `impl_id` implements, for the self type `self_ty`, which is the impl's.
pub(crate) fn lower_impl_trait<'db, 'f>(
    db: &'db Db,
    impl_id: ImplId,
    self_ty: Ty<'db>,
    frame: &'f Frame,
) -> Option<FrameLowered<'f, 'db, StoredEarlyBinder<StoredTraitRef>>> {
    ...
        LifetimeLoweringMode::Bound,
        frame,
    );
    ctx.impl_header = true;
    let target_trait = impl_data.target_trait?;
    let trait_ref = ctx.lower_trait_ref(target_trait, self_ty)?;
    let value = StoredEarlyBinder::bind(ctx.interner, StoredTraitRef::new(trait_ref));
    Some(FrameLowered::from_ctx(value, ctx, frame))
}
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written, `mem_tag = Infer`.

### 10b. `impl_self_ty_of_impl`, `lower_impl_self_ty`

The same shape. `lower_impl_self_ty<'db, 'f>(db, impl_id, frame: &'f Frame) ->
FrameLowered<'f, 'db, StoredEarlyBinder<StoredTy>>` ends with
`FrameLowered::from_ctx(StoredEarlyBinder::bind(ctx.interner, ty.store()), ctx, frame)`. The query
body is `blaze_mem::frame::current(|frame| lower_impl_self_ty(db, impl_id, frame).owned())`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written, `mem_tag = Infer`.

### 10c. `type_for_const_with_diagnostics`, `type_for_static_with_diagnostics`, `type_for_type_alias_with_diagnostics`, `resolve_type_param_assoc_type_shorthand`

Each wraps its body in `blaze_mem::frame::current(|frame| { .. })` and passes `frame` last to its
`TyLoweringContext::new`. Their `TyLoweringResult::from_ctx(.., ctx)` calls are unchanged (they
use 4's new `from_ctx`). `type_for_type_alias_with_diagnostics` reads `take_defined_opaques`,
which is exact after 4. Its `impl_traits: ThinVec::new()` arms (empty, no allocation) stay.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written, `mem_tag = Infer`; `type_for_type_alias_with_diagnostics` opens the frame in its non-extern arm only (the extern arm makes nothing); the shorthand query's body is `resolve_type_param_assoc_type_shorthand_in(.., frame)`.

### 10d. `const_param_types_with_diagnostics`: one exact column

Before:

```rust
) -> TyLoweringResult<'db, ConstParamTypes> {
    let mut result = ThinVec::new();
    ...
    for (local_id, param_data) in data.iter_type_or_consts() {
        if let TypeOrConstParamData::ConstParamData(param_data) = param_data {
            let index = local_id.into_raw().into_u32() as usize;
            if result.len() <= index {
                result.resize(index + 1, None);
            }
            result[index] = Some(ctx.lower_ty(param_data.ty).store());
        }
    }
    result.shrink_to_fit();
    TyLoweringResult::from_ctx(ConstParamTypes { tys: result }, ctx)
```

After (the same column: as long as the last const parameter's index, `None` elsewhere, lowered in
the same order):

```rust
) -> TyLoweringResult<'db, ConstParamTypes> {
    blaze_mem::frame::current(|frame| {
        let (data, store) = GenericParams::with_store(db, def);
        ...
            LifetimeLoweringMode::Bound,
            frame,
        );
        ctx.forbid_params_after(0, ForbidParamsAfterReason::ConstParamTy);
        let consts = || {
            data.iter_type_or_consts().filter_map(|(local_id, it)| match it {
                TypeOrConstParamData::ConstParamData(it) => {
                    Some((local_id.into_raw().into_u32() as usize, it))
                }
                TypeOrConstParamData::TypeParamData(_) => None,
            })
        };
        let len = consts().map(|(index, _)| index + 1).max().unwrap_or(0);
        let mut result = ThinVec::with_capacity(len); // alloc: the memo's column, exact size
        result.extend(iter::repeat_n(None, len));
        for (index, param_data) in consts() {
            result[index] = Some(ctx.lower_ty(param_data.ty).store());
        }
        TyLoweringResult::from_ctx(ConstParamTypes { tys: result }, ctx)
    })
```

Test (`crates/hir/src/tests/lower.rs`):

```rust
#[test]
fn const_parameters_between_type_parameters_have_their_types() {
    let db = TestDB::with_files(
        r#"
//- minicore: sized
struct S<T, const N: usize, U, const M: bool>([T; N], U);
"#,
    );
    crate::attach_db(&db, || {
        let (_, ModuleDefId::AdtId(adt)) = type_item(&db, "S") else { panic!("`S` is an ADT") };
        let def = hir_def::GenericDefId::AdtId(adt);
        let tys: Vec<String> = hir_def::hir::generics::GenericParams::of(&db, def)
            .iter_type_or_consts()
            .filter(|(_, it)| matches!(it, hir_def::hir::generics::TypeOrConstParamData::ConstParamData(_)))
            .map(|(local_id, _)| {
                let id = hir_def::ConstParamId::from_unchecked(hir_def::TypeOrConstParamId {
                    parent: def,
                    local_id,
                });
                format!("{:?}", db.const_param_ty(id))
            })
            .collect();
        assert_eq!(tys, ["usize", "bool"]);
    });
}
```

(The `Debug` form of a `Ty` as the other hir tests print it. Use the file's
`DisplayTarget`/`display` helper if it has one.)

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): the column as written, `mem_tag = Infer`; `len` is computed first and an item without const parameters returns `TyLoweringResult::empty` before the frame (today's early return, same result). The test (in `crates/hir/src/tests/lower.rs`) compares `TyKind`s, not the `Debug` text.

### 10e. `field_types_of_variant`, `lower_field_types`

Before:

```rust
fn field_types_of_variant<'db>(..) -> TyLoweringResult<'db, ThinVec<FieldType>> {
    lower_field_types(db, variant_id)
}

pub(crate) fn lower_field_types<'db>(
    db: &'db Db,
    variant_id: VariantId,
) -> TyLoweringResult<'db, ThinVec<FieldType>> {
    ...
    if fields.is_empty() {
        return TyLoweringResult::empty(ThinVec::new());
    }
    ...
    let mut res = ThinVec::with_capacity(fields.len());
    ...
    TyLoweringResult::from_ctx(res, ctx)
}
```

After:

```rust
fn field_types_of_variant<'db>(..) -> TyLoweringResult<'db, ThinVec<FieldType>> {
    blaze_mem::frame::current(|frame| lower_field_types(db, variant_id, frame).owned_with(exact_thin))
}

pub(crate) fn lower_field_types<'db, 'f>(
    db: &'db Db,
    variant_id: VariantId,
    frame: &'f Frame,
) -> FrameLowered<'f, 'db, Vec<FieldType, &'f Frame>> {
    ...
    if fields.is_empty() {
        return FrameLowered::empty(Vec::new_in(frame), frame);
    }
    ...
    let mut res = Vec::with_capacity_in(fields.len(), frame);
    ... (TyLoweringContext::new(.., frame))
    FrameLowered::from_ctx(res, ctx, frame)
}
```

One allocation per variant with fields, as today, now made from the frame list at its exact
length.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written, `mem_tag = Infer`.

### 10f. `supertraits_info`: frame lists, two exact boxes

Before:

```rust
fn push_new<T: PartialEq>(list: &mut Vec<T>, it: T) {
...
    fn new(
        mut all: Vec<TraitId>,
        direct: Vec<TraitId>,
        defined_assoc_types: Vec<(Name, TypeAliasId)>,
    ) -> SupertraitsInfo {
        let direct_start = u32::try_from(all.len()).expect("2^32 supertraits");
        all.extend(direct);
        SupertraitsInfo {
            supertraits: all.into_boxed_slice(),
            direct_start,
            defined_assoc_types: defined_assoc_types.into_boxed_slice(),
        }
    }
...
        #[salsa::tracked(returns(ref), cycle_result = supertraits_info_cycle)]
        fn supertraits_info(db: &Db, trait_: TraitId) -> SupertraitsInfo {
            let mut all_supertraits = vec![trait_];
            let mut direct_supertraits = Vec::new();
            let mut defined_assoc_types: Vec<(Name, TypeAliasId)> = trait_
                .trait_items(db)
                .items
                .iter()
                .filter_map(|(name, id)| match *id {
                    AssocItemId::TypeAliasId(id) => Some((name.clone(), id)),
                    _ => None,
                })
                .collect();
            ...
            SupertraitsInfo::new(all_supertraits, direct_supertraits, defined_assoc_types)
        }
```

After (the boxes are built once from exact-size iterators. Today's `extend` can double `all`, and
`into_boxed_slice` then reallocates):

```rust
fn push_new<T: PartialEq, A: Allocator>(list: &mut Vec<T, A>, it: T) {
...
    fn new(
        all: &[TraitId],
        direct: &[TraitId],
        defined_assoc_types: &[(Name, TypeAliasId)],
    ) -> SupertraitsInfo {
        let direct_start = u32::try_from(all.len()).expect("2^32 supertraits");
        SupertraitsInfo {
            // alloc: the memo's own lists, exact size
            supertraits: all.iter().chain(direct).copied().collect(),
            direct_start,
            defined_assoc_types: Box::from(defined_assoc_types),
        }
    }
...
        #[salsa::tracked(
            returns(ref),
            cycle_result = supertraits_info_cycle,
            mem_tag = crate::db::INFER_TAG
        )]
        fn supertraits_info(db: &Db, trait_: TraitId) -> SupertraitsInfo {
            blaze_mem::frame::current(|frame| {
                let mut all_supertraits = Vec::new_in(frame);
                all_supertraits.push(trait_);
                let mut direct_supertraits = Vec::new_in(frame);
                let mut defined_assoc_types = Vec::new_in(frame);
                defined_assoc_types.extend(trait_.trait_items(db).items.iter().filter_map(
                    |(name, id)| match *id {
                        AssocItemId::TypeAliasId(id) => Some((name.clone(), id)),
                        _ => None,
                    },
                ));
                ... (the loop unchanged)
                SupertraitsInfo::new(&all_supertraits, &direct_supertraits, &defined_assoc_types)
            })
        }
```

(`use std::alloc::Allocator;` at the top of `lower.rs`.) The tests change their calls to slices:
`SupertraitsInfo::new(&all, &direct, &[])` and `SupertraitsInfo::new(&[trait_id(1)],
&[trait_id(3)], &[])`. `push_new_keeps_the_first_of_each` is unchanged (a `Vec<_, Global>`). One
new test:

```rust
    #[test]
    fn supertraits_info_is_two_exact_boxes() {
        let (info, used) = blaze_alloc_count::measure(|| {
            SupertraitsInfo::new(&[trait_id(1), trait_id(2)], &[trait_id(2)], &[])
        });
        assert_eq!(used.allocations, 1, "the supertraits; no associated types");
        assert_eq!(info.all_supertraits().len() + info.direct_supertraits().len(), 3);
    }
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written, `mem_tag = Infer`; the two `Name` clones carry `// clone:` notes for hot_lint.

### 10g. `type_alias_bounds_with_diagnostics` and `remap_gat_vars_and_recurse_into_nested_projections`

Before:

```rust
    let mut bounds = Vec::new();
    let mut assoc_ty_bounds = Vec::new();
    ...
            let Some((predicate, source)) = remap_gat_vars_and_recurse_into_nested_projections(
                interner,
                item_trait_ref,
                type_alias,
                &generics,
                parent_predicate,
            ) else {
```

After (the body in `blaze_mem::frame::current(|frame| { .. })`, the context taking `frame`):

```rust
    let mut bounds = Vec::new_in(frame);
    let mut assoc_ty_bounds = Vec::new_in(frame);
    ...
            let Some((predicate, source)) = remap_gat_vars_and_recurse_into_nested_projections(
                interner,
                item_trait_ref,
                type_alias,
                &generics,
                parent_predicate,
                frame,
            ) else {
```

`remap_gat_vars_and_recurse_into_nested_projections` gets `frame: &'f Frame` as its last
parameter. Before:

```rust
    let mut mapping = FxIndexMap::default();
    ...
    let mut folder = MapAndCompressBoundVars {
        tcx: interner,
        binder: rustc_type_ir::INNERMOST,
        still_bound_vars: vec![],
        mapping,
    };
...
struct MapAndCompressBoundVars<'db> {
    ...
    still_bound_vars: Vec<BoundVariableKind<'db>>,
    ...
    mapping: FxIndexMap<BoundVar, GenericArg<'db>>,
}
```

After (the folder only calls `get` and `insert` on the map and never iterates it, so a hash map
is the same):

```rust
    let mut mapping = FrameMap::with_hasher_in(FxBuildHasher, frame);
    ...
    let mut folder = MapAndCompressBoundVars {
        tcx: interner,
        binder: rustc_type_ir::INNERMOST,
        still_bound_vars: Vec::new_in(frame),
        mapping,
    };
...
struct MapAndCompressBoundVars<'db, 'f> {
    ...
    still_bound_vars: Vec<BoundVariableKind<'db>, &'f Frame>,
    ...
    mapping: FrameMap<'f, BoundVar, GenericArg<'db>>,
}
```

and `impl<'db> TypeFolder<DbInterner<'db>> for MapAndCompressBoundVars<'db, '_>`. The
`FxIndexMap` import goes if nothing else uses it. Test: the GAT item-bound tests
(`tests/traits.rs` `gat_*`) hold the compressed binders.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written, `mem_tag = Infer`, the map `frame_map(frame)`; the body is `type_alias_bounds_in(db, type_alias, frame)`. The `FxIndexMap` and `FxHashSet` imports went (nothing else used them).

### 10h. `generic_predicates_of`, `generic_predicates`, `push_const_arg_has_type_predicates`

Before:

```rust
#[salsa::tracked(returns(ref), cycle_result = generic_predicates_cycle_result)]
... generic_predicates_of ... {
    generic_predicates(db, def)
}
...
pub(crate) fn generic_predicates<'db>(
    db: &'db Db,
    def: GenericDefId,
) -> TyLoweringResult<'db, GenericPredicates> {
    ...
    let mut own_predicates = SmallVec::<[Clause<'db>; 8]>::new();
    let mut own_assoc_ty_bounds = SmallVec::<[Clause<'db>; 8]>::new();
    ...
    let diagnostics = mem::take(&mut ctx.diagnostics);
    let defined_anon_consts = mem::take(&mut ctx.defined_anon_consts);

    let parent_predicates = parent.predicates.get().skip_binder().as_slice();
    let predicates = parent_predicates
        .iter()
        .chain(own_implicit_trait_predicate.iter())
        .chain(own_predicates.iter())
        .chain(own_assoc_ty_bounds.iter())
        .copied()
        .collect::<SmallVec<[Clause<'db>; 16]>>();
    ...
    return TyLoweringResult::new(predicates, diagnostics, defined_anon_consts);
...
fn push_const_arg_has_type_predicates<'db>(
    db: &'db Db,
    predicates: &mut SmallVec<[Clause<'db>; 8]>,
```

After:

```rust
... generic_predicates_of ... {
    blaze_mem::frame::current(|frame| generic_predicates(db, def, frame).owned())
}
...
pub(crate) fn generic_predicates<'db, 'f>(
    db: &'db Db,
    def: GenericDefId,
    frame: &'f Frame,
) -> FrameLowered<'f, 'db, GenericPredicates> {
    ... (TyLoweringContext::new(.., frame))
    let mut own_predicates = Vec::new_in(frame);
    let mut own_assoc_ty_bounds = Vec::new_in(frame);
    ...
    let parent_predicates = parent.predicates.get().skip_binder().as_slice();
    let mut predicates = Vec::with_capacity_in(
        parent_predicates.len()
            + usize::from(own_implicit_trait_predicate.is_some())
            + own_predicates.len()
            + own_assoc_ty_bounds.len(),
        frame,
    );
    predicates.extend_from_slice(parent_predicates);
    predicates.extend(own_implicit_trait_predicate);
    predicates.extend_from_slice(&own_predicates);
    predicates.extend_from_slice(&own_assoc_ty_bounds);
    ...
    return FrameLowered::from_ctx(predicates, ctx, frame);
...
fn push_const_arg_has_type_predicates<'db>(
    db: &'db Db,
    predicates: &mut Vec<Clause<'db>, &Frame>,
```

The two `mem::take` lines go: `add_sized_clause`'s borrow of `ctx` ends at its last use, before
`from_ctx`. Today's last line builds the `GenericPredicates` value from the list, so the list is
renamed `all` and the value keeps the name `predicates`, as in
`FrameLowered::from_ctx(predicates, ctx, frame)`. Test: the existing predicate tests hold the
order (the batch tests compare the batch with the per-item query). The allocation budget, at
most the interner's for one `Clauses` and nothing for an item without findings, is a line of
mem-db-infer's `tests/budget.rs` (its request 4).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written, `mem_tag = Infer`; `generic_predicates`' `tracing::instrument` skips `frame` too.

### 10i. `const_conditions_query`

Before: `let mut conditions = Vec::new();` and
`StoredEarlyBinder::bind(interner, Clauses::new_from_iter(conditions).store())`. After: the body
in `blaze_mem::frame::current(|frame| { .. })`, `let mut conditions = Vec::new_in(frame);`, the
context taking `frame`, and `Clauses::new_from_slice(&conditions)`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written, `mem_tag = Infer`.

### 10j. `generic_defaults_with_diagnostics`: one exact column

Before:

```rust
    let mut defaults = if let Some(parent) = generics.parent() {
        generic_defaults_with_diagnostics(db, parent.def()).value.clone()
    } else {
        GenericDefaults(ThinVec::new())
    };

    generics
        .iter_self_with_idx()
        .filter_map(|(idx, _id, p)| Some((idx as usize, handle_generic_param(&mut ctx, idx, p)?)))
        .for_each(|(idx, default)| defaults.push(idx, default));
    defaults.0.shrink_to_fit();
    return TyLoweringResult::from_ctx(defaults, ctx);
```

After (the same column: the parent's, then `None` up to each own default. An own index is past
the parent's, as `push` already asserts by its subtraction):

```rust
    let parent: &[_] = match generics.parent() {
        Some(parent) => &generic_defaults_with_diagnostics(db, parent.def()).value.0,
        None => &[],
    };
    let mut own = Vec::new_in(frame);
    for (idx, _id, p) in generics.iter_self_with_idx() {
        if let Some(default) = handle_generic_param(&mut ctx, idx, p) {
            own.push((idx as usize, default));
        }
    }
    let len = own.last().map_or(parent.len(), |&(idx, _)| idx + 1);
    let mut defaults = ThinVec::with_capacity(len); // alloc: the memo's column, exact size
    defaults.extend_from_slice(parent);
    for (idx, default) in own {
        defaults.extend(iter::repeat_n(None, idx - defaults.len()));
        defaults.push(Some(default));
    }
    return TyLoweringResult::from_ctx(GenericDefaults(defaults), ctx);
```

The body is in `blaze_mem::frame::current(|frame| { .. })`. `GenericDefaults::push` goes.
`GenericDefaults` keeps `Clone`, which other code may use. Test (`crates/hir/src/tests/lower.rs`):

```rust
#[test]
fn defaults_follow_the_parameters_they_are_of() {
    let db = TestDB::with_files(
        r#"
//- minicore: sized
struct S<A, B = u8, C = u16>(A, B, C);
"#,
    );
    crate::attach_db(&db, || {
        let (_, ModuleDefId::AdtId(adt)) = type_item(&db, "S") else { panic!("`S` is an ADT") };
        let defaults = db.generic_defaults(hir_def::GenericDefId::AdtId(adt));
        let has: Vec<bool> = (0..4).map(|idx| defaults.get(idx).is_some()).collect();
        assert_eq!(has, [false, true, true, false]);
    });
}
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written, `mem_tag = Infer`; the frame opens after the `has_no_params` early return. Test in `crates/hir/src/tests/lower.rs`.

### 10k. `fn_sig_for_fn`

The body in `blaze_mem::frame::current(|frame| { .. })`. Both contexts take `frame`.
`ctx_params.diagnostics.extend(ctx_ret.diagnostics)` and the `defined_anon_consts` line are
unchanged (frame lists of the same lifetime). `take_defined_opaques` is exact after 4. One
allocation if the function returns `impl Trait`, + d + a.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written, `mem_tag = Infer`, the body `fn_sig_for_fn_in(db, def, frame)`.

### 10l. `elided_lifetimes`, `input_lifetimes`, `path_lifetimes`

Before:

```rust
pub fn elided_lifetimes(db: &Db, function: FunctionId) -> ElidedLifetimes {
    ...
    let mut params = Vec::with_capacity(data.params.len());
    let mut distinct: Vec<ElidedOutput> = Vec::new();
    ...
    for (index, &param) in data.params.iter().enumerate() {
        let mut found = Vec::new();
        let mut self_references = Vec::new();
        if input_lifetimes(db, &resolver, store, param, &mut found, &mut self_references).is_none()
        {
            ...
        }
        let mut lifetimes = Vec::with_capacity(found.len());
        for it in found {
            lifetimes.push(match it {
                ...
            });
        }
        ...
        for it in lifetimes {
```

After (one set of frame lists for all parameters, cleared per parameter. `params` stays a global
`Vec` at its exact length, made into the memo's box without reallocating):

```rust
pub fn elided_lifetimes(db: &Db, function: FunctionId) -> ElidedLifetimes {
    blaze_mem::frame::current(|frame| {
    ...
    let mut params = Vec::with_capacity(data.params.len()); // alloc: the memo's column, exact size
    let mut distinct: Vec<ElidedOutput, _> = Vec::new_in(frame);
    let mut found = Vec::new_in(frame);
    let mut self_references = Vec::new_in(frame);
    let mut lifetimes = Vec::new_in(frame);
    ...
    for (index, &param) in data.params.iter().enumerate() {
        found.clear();
        self_references.clear();
        lifetimes.clear();
        if input_lifetimes(db, &resolver, store, param, &mut found, &mut self_references, frame)
            .is_none()
        {
            ...
        }
        lifetimes.extend(found.iter().map(|&it| match it {
            None => {
                elided += 1;
                ElidedOutput::Elided(elided - 1)
            }
            Some(lifetime) => ElidedOutput::Written(lifetime.into_raw().into_u32()),
        }));
        ...
        for &it in &lifetimes {
    ...
    })
}
```

`input_lifetimes` and `path_lifetimes` take `found: &mut Vec<Option<LifetimeRefId>, &Frame>`,
`self_references: &mut Vec<usize, &Frame>` and `frame: &Frame` (passed down the recursion).
`path_lifetimes` collects nothing. Before:

```rust
    let args: Vec<_> = path.generic_args().into_iter().flatten().flatten().collect();
    if args.iter().any(|it| {
        ...
    }) {
        return None;
    }
    let args = || args.iter().flat_map(|it| it.args.iter());
    let written: Vec<_> = args()
        .filter_map(|it| match it {
            hir_def::expr_store::path::GenericArg::Lifetime(it) => Some(*it),
            _ => None,
        })
        .collect();
    if written.is_empty() {
        found.extend(iter::repeat_n(None, params));
    } else {
        for it in written {
            found.push(lifetime(Some(it))?);
        }
    }
```

After (`generic_args()` is a borrowed view of the path, walked again in place of a copy):

```rust
    let segments = || path.generic_args().into_iter().flatten().flatten();
    if segments().any(|it| {
        ...
    }) {
        return None;
    }
    let args = || segments().flat_map(|it| it.args.iter());
    let mut written = args()
        .filter_map(|it| match it {
            hir_def::expr_store::path::GenericArg::Lifetime(it) => Some(*it),
            _ => None,
        })
        .peekable();
    if written.peek().is_none() {
        found.extend(iter::repeat_n(None, params));
    } else {
        for it in written {
            found.push(lifetime(Some(it))?);
        }
    }
```

With this, `frame` is not needed by `path_lifetimes` itself, only passed on to
`input_lifetimes`. Drop the parameter if nothing else uses it. Test: the existing
`elided_lifetimes` tests (`tests/regression.rs`, the `impl Trait` capture tests) hold the result.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written, `mem_tag = Infer`, the body `elided_lifetimes_in(db, function, frame)`. `frame` is dropped from both `input_lifetimes` and `path_lifetimes`: once `path_lifetimes` collects nothing neither uses it; they take the frame lists only.

### 10m. `trait_environment_query`, the constructor signatures

Only the tag. They make no temporaries of their own, so they open no frame.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): `mem_tag = Infer` on `trait_environment_query`, `fn_sig_for_struct_constructor`, `fn_sig_for_enum_variant_constructor`.

## 11. lane db: `ElidedLifetimes::regions` fills the caller's list, `lower.rs`

It is called from three places in other crates' queries (`hir-ty-wfcheck/src/lib.rs:328`,
`hir-ty-borrowck/src/regions.rs:1166` and `:5686`), each of which then reads the list. Before:

```rust
    pub fn regions<'db>(&self, errors: &[Vec<Region<'db>>]) -> Vec<Option<Region<'db>>> {
        let mut regions = Vec::new();
        for (index, count) in self.params.iter().enumerate() {
            let Some(count) = count.map(|it| it as usize) else { continue };
            match errors.get(index).map(Vec::as_slice).unwrap_or_default() {
```

After (the caller's list, a frame `Vec` once its crate is on the frame. `errors` may be a list of
frame lists):

```rust
    pub fn regions_into<'db, E: AsRef<[Region<'db>]>>(
        &self,
        errors: &[E],
        regions: &mut impl Extend<Option<Region<'db>>>,
    ) {
        for (index, count) in self.params.iter().enumerate() {
            let Some(count) = count.map(|it| it as usize) else { continue };
            match errors.get(index).map(AsRef::as_ref).unwrap_or_default() {
```

The body is otherwise unchanged (`regions.extend(..)` once per arm, with
`regions.extend([Some(*first)])` in place of `push`). The final `regions` line goes. The callers'
changes are in 22.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written; `with_elided_regions`' doc link now names `regions_into`. The three callers (borrowck `regions.rs:1225`, `:5816`, wfcheck `lib.rs:356`) told to db-mem-borrowck again at 21:51.

## 12. lane db: `bind_elided_lifetimes` takes its caller's frame, `lower.rs`

It runs when a function item is reified with elided lifetimes. The caller is
`infer/coerce.rs:940`, part expr. Before:

```rust
pub(crate) fn bind_elided_lifetimes<'db>(
    db: &'db Db,
    function: FunctionId,
    sig: PolyFnSig<'db>,
) -> PolyFnSig<'db> {
    ...
    let mut vars: Vec<BoundVariableKind<'db>> = sig.bound_vars().iter().collect();
    let mut of_params = Vec::new();
    let mut tys = Vec::with_capacity(inputs.len() + 1);
```

After:

```rust
pub(crate) fn bind_elided_lifetimes<'db>(
    db: &'db Db,
    function: FunctionId,
    sig: PolyFnSig<'db>,
    frame: &Frame,
) -> PolyFnSig<'db> {
    ...
    let mut vars = Vec::new_in(frame);
    vars.extend(sig.bound_vars().iter());
    let mut of_params = Vec::new_in(frame);
    let mut tys = Vec::with_capacity_in(inputs.len() + 1, frame);
```

and at the end `BoundVarKinds::new_from_slice(&vars)` in place of `new_from_iter(vars)`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): as written; `infer/coerce.rs:960` told to db-mem-infer-a-next1 and db-mem-infer-a-expr.

## 13. lane db: the R7 helpers of `lower.rs` (no change)

These are called on the hit path of many queries and make nothing:
- `const_param_ty`, `const_param_types`, `generic_defaults`, `impl_trait_query`,
  `impl_trait_with_diagnostics`, `impl_self_ty_with_diagnostics`, `field_types_with_diagnostics`,
  `callable_item_signature`;
- `GenericPredicates::query*`, `ImplTraitId::bounds`, `SelfAndAssocBounds::*`;
- `with_elided_regions` (a fold, with interned results);
- `associated_type_by_name_including_super_traits_allow_ambiguity` (its `name.clone()` is a
  4-byte `Name`);
- `elided_lifetime_param`, `type_for_adt`, `ctor_signature`.

Each reads memos or interns, and takes no frame.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): nothing to change.

## 14. lane db: `crates/hir-ty/src/lower/path.rs`

`PathLoweringContext` holds `ctx: &'a mut TyLoweringContext<'db, 'b>`, so it reaches the frame as
`self.ctx.frame` (`&'b Frame`) and needs no field of its own (R2).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:56): nothing of its own (14a, 14b read `self.ctx.frame`).

### 14a. `substs_from_args_and_bindings` (the free fn): the run's frame

Once per path segment that names a generic item (the hot part of lowering a path). Its length is
exact and it never grows. Before:

```rust
    let mut substs: SmallVec<[GenericArg<'db>; 8]> =
        SmallVec::with_capacity(def_generics.len(true));
```

After (a new last parameter `frame: &Frame`):

```rust
    let mut substs = Vec::with_capacity_in(def_generics.len(true), frame);
```

`GenericArgs::new_from_slice(&substs)` and the `&substs` passed to `inferred_kind` and
`object_lifetime_default` are unchanged (they are slices). Callers:
- `path.rs`, the method `substs_from_args_and_bindings`, passes `self.ctx.frame`;
- `method_resolution/confirm.rs:464`, part method, passes its inference's frame (22).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:56): as written; the method reads `let frame = self.ctx.frame;` before the call (the `LowererCtx` argument reborrows `self`). `confirm.rs`'s caller is told to db-mem-infer-b.

### 14b. `assoc_type_bindings_from_type_bound`: the bounds of a binding

Before:

```rust
        let interner = self.ctx.interner;
        self.current_or_prev_segment.args_and_bindings.map(|args_and_bindings| {
            ...
                    None => return SmallVec::new(),
            ...
                let mut predicates: SmallVec<[_; 1]> = SmallVec::with_capacity(
                    binding.type_ref.as_ref().map_or(0, |_| 1) + binding.bounds.len(),
                );
```

After (`Item: A + B` and `Item = T` with bounds spill today. A frame list does not, at 2 ns):

```rust
        let interner = self.ctx.interner;
        let frame = self.ctx.frame;
        self.current_or_prev_segment.args_and_bindings.map(|args_and_bindings| {
            ...
                    None => return Vec::new_in(frame),
            ...
                let mut predicates = Vec::with_capacity_in(
                    binding.type_ref.as_ref().map_or(0, |_| 1) + binding.bounds.len(),
                    frame,
                );
```

The return type `impl Iterator<..> + use<'a, 'b, 'db>` already covers `&'b Frame`. If nothing
else uses `smallvec::SmallVec` in `path.rs`, its import goes.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:56): as written; the `smallvec::SmallVec` import went.

### 14c. Left as they are

- `prohibit_generic_args(segments: Vec<u32>, ..)`, `with_args`, the `vec![..]` of one segment,
  and `handle_type_ns_resolution`'s `prefix`. These are an error's segments: `with_args` collects
  nothing (no allocation) unless a leading segment has generic arguments, which is always an
  error, and the list becomes the diagnostic's own `Box<[u32]>`.
- `binding.name.clone()`, `assoc_name.clone()`: a 4-byte `Name`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:56): nothing to change.

## 15. lane db: `crates/hir-ty/src/lower/batch.rs`

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:45): 15a to 15f applied below (db-mem-infer-b-lower), in the tree at `crates/hir-ty/src/lower/batch.rs:87` (`SideTableBuilder<'f, 'db>`), `:163` and `:292` (a task per item), `:351` to `:409` (the four queries, `mem_tag = Infer`).

### 15a. `SideTableBuilder` in the frame, exact columns

Before:

```rust
#[derive(Default)]
pub(crate) struct SideTableBuilder<'db> {
    rows: Vec<u32>,
    diagnostics_at: Vec<u32>,
    anon_consts_at: Vec<u32>,
    diagnostics: Vec<TyLoweringDiagnostic>,
    anon_consts: Vec<AnonConstId<'db>>,
}
...
    pub(crate) fn finish(self) -> SideTable<'db> {
        SideTable {
            rows: self.rows.into_boxed_slice(),
            diagnostics_at: self.diagnostics_at.into_boxed_slice(),
            anon_consts_at: self.anon_consts_at.into_boxed_slice(),
            diagnostics: self.diagnostics.into_boxed_slice(),
            anon_consts: self.anon_consts.into_boxed_slice(),
        }
    }
```

After:

```rust
pub(crate) struct SideTableBuilder<'f, 'db> {
    rows: Vec<u32, &'f Frame>,
    diagnostics_at: Vec<u32, &'f Frame>,
    anon_consts_at: Vec<u32, &'f Frame>,
    diagnostics: Vec<TyLoweringDiagnostic, &'f Frame>,
    anon_consts: Vec<AnonConstId<'db>, &'f Frame>,
}

impl<'f, 'db> SideTableBuilder<'f, 'db> {
    pub(crate) fn new(frame: &'f Frame) -> Self {
        SideTableBuilder {
            rows: Vec::new_in(frame),
            diagnostics_at: Vec::new_in(frame),
            anon_consts_at: Vec::new_in(frame),
            diagnostics: Vec::new_in(frame),
            anon_consts: Vec::new_in(frame),
        }
    }
    ... (push unchanged)
    pub(crate) fn finish(mut self) -> SideTable<'db> {
        SideTable {
            rows: exact(&mut self.rows),
            diagnostics_at: exact(&mut self.diagnostics_at),
            anon_consts_at: exact(&mut self.anon_consts_at),
            diagnostics: exact(&mut self.diagnostics),
            anon_consts: exact(&mut self.anon_consts),
        }
    }
}

/// A batch's own column of `items`, allocated once at their length: nothing when empty.
fn exact<T>(items: &mut Vec<T, &Frame>) -> Box<[T]> {
    items.drain(..).collect() // alloc: a batch's own column, exact size
}
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:16): as written; `Frame` is `blaze_mem::Frame`.

### 15b. `Batch::build`: one task per item

Before:

```rust
    fn build(keys: Vec<K>, mut lower: impl FnMut(K) -> TyLoweringResult<'db, V>) -> Self {
        let mut values = Vec::with_capacity(keys.len());
        let mut side = SideTableBuilder::default();
        for (row, &key) in keys.iter().enumerate() {
            let (value, diagnostics, anon_consts) = lower(key).into_parts();
            side.push(row as u32, diagnostics, anon_consts);
            values.push(value);
        }
        Batch { keys: keys.into_boxed_slice(), values: values.into_boxed_slice(), side: side.finish() }
    }

    fn filled(keys: Vec<K>, value: impl Fn() -> V) -> Self {
        let values = keys.iter().map(|_| value()).collect();
        Batch { keys: keys.into_boxed_slice(), values, side: SideTable::default() }
    }
```

After:

```rust
    /// `lower` of each of `keys`, which are ascending and distinct, each item in a task of its
    /// own: an item's temporaries are gone before the next, so a crate of 10,000 items peaks at
    /// its largest item's, in warm memory. What an item found is copied to `frame`'s side table.
    fn build(
        keys: &[K],
        frame: &Frame,
        mut lower: impl for<'f> FnMut(K, &'f Frame) -> FrameLowered<'f, 'db, V>,
    ) -> Self {
        let mut values = Vec::with_capacity(keys.len()); // alloc: the batch's own column, exact size
        let mut side = SideTableBuilder::new(frame);
        for (row, &key) in keys.iter().enumerate() {
            blaze_mem::frame::task(crate::db::INFER_TAG, |item| {
                let lowered = lower(key, item);
                side.push(row as u32, lowered.diagnostics, lowered.anon_consts);
                values.push(lowered.value);
            });
        }
        Batch { keys: Box::from(keys), values: values.into_boxed_slice(), side: side.finish() }
    }

    fn filled(keys: &[K], value: impl Fn() -> V) -> Self {
        let values = keys.iter().map(|_| value()).collect(); // alloc: the batch's column, exact size
        Batch { keys: Box::from(keys), values, side: SideTable::default() }
    }
```

This is a deviation from R1 ("not opened again deeper"). A crate's batch is the one query whose
run is thousands of items in a row. Without a task per item its frame grows by every item's
temporaries: about 200 bytes each, so 2 MB for std's 10,000 impls, written once and cold. With
one, each item reuses the same warm few hundred bytes, for about 10 ns of task entry against
microseconds of lowering per item. mem-db-infer has been told.

`values.into_boxed_slice()` does not reallocate: the capacity is exact and every key pushes one
value.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:16): as written (task tag `crate::db::INFER_TAG`, mem-db-infer 3).

### 15c. `CrateFieldTypes::build`

Before:

```rust
    pub(crate) fn build(
        variants: Vec<VariantId>,
        mut lower: impl FnMut(VariantId) -> TyLoweringResult<'db, ThinVec<FieldType>>,
    ) -> Self {
        let mut starts = Vec::with_capacity(variants.len() + 1);
        let mut fields = Vec::new();
        let mut side = SideTableBuilder::default();
        starts.push(0);
        for (row, &variant) in variants.iter().enumerate() {
            let (types, diagnostics, anon_consts) = lower(variant).into_parts();
            fields.extend(types);
            starts.push(u32::try_from(fields.len()).expect("more than 2^32 fields in one crate"));
            side.push(row as u32, diagnostics, anon_consts);
        }
        CrateFieldTypes {
            variants: variants.into_boxed_slice(),
            starts: starts.into_boxed_slice(),
            fields: fields.into_boxed_slice(),
            side: side.finish(),
        }
    }
```

After:

```rust
    pub(crate) fn build(
        variants: &[VariantId],
        frame: &Frame,
        mut lower: impl for<'f> FnMut(VariantId, &'f Frame) -> FrameLowered<'f, 'db, Vec<FieldType, &'f Frame>>,
    ) -> Self {
        let mut starts = Vec::with_capacity(variants.len() + 1); // alloc: the batch's column, exact size
        let mut fields = Vec::new_in(frame);
        let mut side = SideTableBuilder::new(frame);
        starts.push(0);
        for (row, &variant) in variants.iter().enumerate() {
            blaze_mem::frame::task(crate::db::INFER_TAG, |item| {
                let lowered = lower(variant, item);
                fields.extend(lowered.value);
                starts.push(
                    u32::try_from(fields.len()).expect("more than 2^32 fields in one crate"),
                );
                side.push(row as u32, lowered.diagnostics, lowered.anon_consts);
            });
        }
        CrateFieldTypes {
            variants: Box::from(variants), // alloc: the batch's column, exact size
            starts: starts.into_boxed_slice(),
            fields: fields.drain(..).collect(), // alloc: the batch's column, exact size
            side: side.finish(),
        }
    }
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:16): as written.

### 15d. `module_variants`, `module_impls`: in the caller's allocator

Before: `fn module_variants(db: &Db, krate: Crate) -> Vec<VariantId> { let mut variants =
Vec::new(); ..` and the same for `module_impls`. After:

```rust
fn module_variants<A: Allocator>(db: &Db, krate: Crate, alloc: A) -> Vec<VariantId, A> {
    let mut variants = Vec::new_in(alloc);
    ... (unchanged)
}

fn module_impls<A: Allocator>(db: &Db, krate: Crate, alloc: A) -> Vec<ImplId, A> {
    let mut impls = Vec::new_in(alloc);
    ... (unchanged)
}
```

The queries pass their frame. The cycle results (only code rustc rejects reaches them) pass
`std::alloc::Global`: they run once, cold, and the boxes copy from the list either way.
`radix::sort_copy_by_key` allocates its scratch with the global allocator, once per crate's sort.
blaze-radix's `sort_copy_by_key_in` takes a `&mut Vec<T>` (Global), so it is left. A
`Vec<T, A>` form there (fix-radix's file) would make it a frame list.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:16): as written (`radix::sort_copy_by_key` takes `&mut [T]`, so the frame list sorts in place).

### 15e. The four queries

Before:

```rust
#[salsa::tracked(returns(ref))]
fn crate_field_types<'db>(db: &'db Db, krate: Crate) -> CrateFieldTypes<'db> {
    CrateFieldTypes::build(module_variants(db, krate), |variant| lower_field_types(db, variant))
}

#[salsa::tracked(returns(ref), cycle_result = crate_impl_self_tys_cycle)]
fn crate_impl_self_tys<'db>(db: &'db Db, krate: Crate) -> CrateImplSelfTys<'db> {
    Batch::build(module_impls(db, krate), |impl_id| lower_impl_self_ty(db, impl_id))
}
...
    Batch::filled(module_impls(db, krate), || error.clone())
...
fn crate_impl_traits<'db>(db: &'db Db, krate: Crate) -> CrateImplTraits<'db> {
    let self_tys = CrateImplSelfTys::of(db, krate);
    let keys = self_tys.keys.to_vec();
    Batch::build(keys, |impl_id| {
        let self_ty = self_tys.get(impl_id).expect("the self types have every impl");
        match lower_impl_trait(db, impl_id, self_ty.value.get().skip_binder()) {
            Some(result) => {
                let (value, diagnostics, anon_consts) = result.into_parts();
                TyLoweringResult::new(Some(value), diagnostics, anon_consts)
            }
            None => TyLoweringResult::empty(None),
        }
    })
}
...
    Batch::filled(module_impls(db, krate), || None)
...
fn crate_impl_predicates<'db>(db: &'db Db, krate: Crate) -> CrateImplPredicates<'db> {
    let keys = CrateImplSelfTys::of(db, krate).keys.to_vec();
    Batch::build(keys, |impl_id| generic_predicates(db, impl_id.into()))
}
...
    Batch::filled(module_impls(db, krate), || {
```

After:

```rust
#[salsa::tracked(returns(ref), mem_tag = crate::db::INFER_TAG)]
fn crate_field_types<'db>(db: &'db Db, krate: Crate) -> CrateFieldTypes<'db> {
    blaze_mem::frame::current(|frame| {
        let variants = module_variants(db, krate, frame);
        CrateFieldTypes::build(&variants, frame, |variant, item| lower_field_types(db, variant, item))
    })
}

#[salsa::tracked(returns(ref), cycle_result = crate_impl_self_tys_cycle, mem_tag = crate::db::INFER_TAG)]
fn crate_impl_self_tys<'db>(db: &'db Db, krate: Crate) -> CrateImplSelfTys<'db> {
    blaze_mem::frame::current(|frame| {
        let impls = module_impls(db, krate, frame);
        Batch::build(&impls, frame, |impl_id, item| lower_impl_self_ty(db, impl_id, item))
    })
}
...
    Batch::filled(&module_impls(db, krate, std::alloc::Global), || error.clone())
...
#[salsa::tracked(returns(ref), cycle_result = crate_impl_traits_cycle, mem_tag = crate::db::INFER_TAG)]
fn crate_impl_traits<'db>(db: &'db Db, krate: Crate) -> CrateImplTraits<'db> {
    let self_tys = CrateImplSelfTys::of(db, krate);
    blaze_mem::frame::current(|frame| {
        Batch::build(&self_tys.keys, frame, |impl_id, item| {
            let self_ty = self_tys.get(impl_id).expect("the self types have every impl");
            match lower_impl_trait(db, impl_id, self_ty.value.get().skip_binder(), item) {
                Some(lowered) => FrameLowered {
                    value: Some(lowered.value),
                    diagnostics: lowered.diagnostics,
                    anon_consts: lowered.anon_consts,
                },
                None => FrameLowered::empty(None, item),
            }
        })
    })
}
...
    Batch::filled(&module_impls(db, krate, std::alloc::Global), || None)
...
#[salsa::tracked(returns(ref), cycle_result = crate_impl_predicates_cycle, mem_tag = crate::db::INFER_TAG)]
fn crate_impl_predicates<'db>(db: &'db Db, krate: Crate) -> CrateImplPredicates<'db> {
    let self_tys = CrateImplSelfTys::of(db, krate);
    blaze_mem::frame::current(|frame| {
        Batch::build(&self_tys.keys, frame, |impl_id, item| generic_predicates(db, impl_id.into(), item))
    })
}
...
    Batch::filled(&module_impls(db, krate, std::alloc::Global), || {
```

`FrameLowered::empty` becomes `pub(crate)` for this. The `self_tys.keys.to_vec()` copies go:
`build` boxes its keys once from the slice.

| fn (line today) | result | allocations |
|---|---|---|
| `crate_field_types` (317) | `CrateFieldTypes` | 3 columns (variants, starts, fields; none empty when the crate has a variant) + side table, 0 to 5 |
| `crate_impl_self_tys` (322) | `CrateImplSelfTys` | keys, values + side table 0 to 5 |
| `crate_impl_traits` (339) | `CrateImplTraits` | keys, values + side table 0 to 5 |
| `crate_impl_predicates` (364) | `CrateImplPredicates` | keys, values + side table 0 to 5 |

Plus each item's own owned parts, which it moves into the values: a `TyLoweringDiagnostic`'s
boxed sites, made once when the diagnostic is.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:16): the four queries, with `mem_tag = Infer` in place of `mem_tag = crate::db::INFER_TAG` (salsa's option as filed, mem-db-salsa M2, takes a bare `blaze_mem::Tag` variant). `FrameLowered::empty` made `pub(crate)` with 4 in `lower.rs`.

### 15f. Tests

Before: `let mut side = SideTableBuilder::<'static>::default();` and
`let side = SideTableBuilder::<'static>::default().finish();`. After, each test body in a task:

```rust
    #[test]
    fn a_side_table_keeps_only_rows_with_something() {
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
            let mut side = SideTableBuilder::<'_, 'static>::new(frame);
            ... (unchanged)
        });
    }

    #[test]
    fn an_empty_side_table_allocates_nothing_and_has_nothing() {
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
            let (side, used) =
                blaze_alloc_count::measure(|| SideTableBuilder::<'_, 'static>::new(frame).finish());
            assert_eq!(used.allocations, 0);
            assert_eq!(side.len(), 0);
            assert!(side.get(0).0.is_empty() && side.get(0).1.is_empty());
        });
    }

    #[test]
    fn a_side_table_column_is_its_exact_length() {
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
            let mut side = SideTableBuilder::<'_, 'static>::new(frame);
            for row in 0..5 {
                side.push(row, [diagnostic(row)], []);
            }
            let (side, used) = blaze_alloc_count::measure(|| side.finish());
            assert_eq!(used.allocations, 4, "rows, two starts, diagnostics; no constants");
            assert_eq!(side.len(), 5);
        });
    }
```

A batch's result must equal the per-item queries'. The existing persist round-trip tests and the
`hir` batch tests (`crates/hir/src/tests.rs:794`) compare them. Keep them.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:16): as written, all three tests.

## 16. lane db: `crates/hir-ty/src/lower/persist.rs`

The writers (`ColdBatch::of_*`, `impl_keys`, `with_table`, `ColdSide::of`) build the image's
columns from exact-size `map`s and `to_vec`s. They are the image writer's edge, once per crate
written, and their `Vec`s are the written result. They are left (mem-db-def's image lane owns the
format).

The readers collect `Result<Box<[_]>, _>` through a shunt whose size hint is 0: the `Vec` doubles,
then `into_boxed_slice` reallocates. Add:

```rust
/// A read batch's own column of `len` items, allocated once: `collect::<Result<Box<[_]>, _>>`
/// would grow it from nothing and copy it again to fit.
fn exact<T, E>(len: usize, items: impl Iterator<Item = Result<T, E>>) -> Result<Box<[T]>, E> {
    let mut column = Vec::with_capacity(len); // alloc: a read batch's own column, exact size
    for it in items {
        column.push(it?);
    }
    Ok(column.into_boxed_slice())
}
```

and in `impl_keys`, `field_types` (`variants` and `fields`), `impl_self_tys`, `impl_traits`,
`impl_predicates` and `side_table` (`anon_consts`), each
`self.X.iter()...map(..).collect::<Result<_, _>>()?` becomes `exact(self.X.len(), self.X.iter()
...map(..))?`. For `field_types`' variants, the length is `self.keys.len()`, and for its fields
`self.tys.len()`.

`side_table`'s diagnostics have no count up front. Before:

```rust
        let mut diagnostics = Vec::new();
        while !words.is_done() {
            diagnostics.push(words.diagnostic()?);
        }
```

After (a reader is an entry point called outside queries, so it opens a task. R1):

```rust
        let diagnostics: Box<[TyLoweringDiagnostic]> =
            blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
                let mut diagnostics = Vec::new_in(frame);
                while !words.is_done() {
                    diagnostics.push(words.diagnostic()?);
                }
                Ok::<_, FormatError>(diagnostics.drain(..).collect()) // alloc: the column, exact size
            })?;
```

and `diagnostics: diagnostics.into_boxed_slice()` becomes `diagnostics`. The `ensure` reads
`diagnostics.len()` as before. `Words::diagnostic`'s `sources`, `segments` and `sites`
(`collect::<Result<_, _>>` and `with_capacity(len)`) are a diagnostic's own boxes: `sites` is
exact already, and the other two go through `exact(len, ..)`. Test: the existing round-trip tests
(`mod tests`, 1150 on) hold the values. Add:

```rust
    #[test]
    fn a_read_column_is_one_allocation() {
        let (column, used) = blaze_alloc_count::measure(|| {
            exact::<u32, ()>(1000, (0..1000).map(Ok))
        });
        assert_eq!((used.allocations, column.unwrap().len()), (1, 1000));
    }
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:20): as written; the readers' closures name their error (`Ok::<_, FormatError>` or `-> Result<_, FormatError>`) where the old `collect` turbofish did.

## 17. lane db: `crates/hir-ty/src/lower/diagnostics.rs`, `generics.rs`, `utils.rs`: no change

- `lower/diagnostics.rs`: the diagnostic types. Their `Box<[..]>` sites are the diagnostic's own
  (owned, made once where the diagnostic is pushed).
- `generics.rs`: `generics(db, def)` builds `Generics { chain: ArrayVec<SingleGenerics, 2> }` of
  borrowed `&'db GenericParams`. It is called by many queries per path, makes nothing, and needs
  no frame (R7).
- `utils.rs`: `direct_super_traits` and `all_super_traits` read `SupertraitsInfo`'s memo (R7, no
  allocation). `detect_variant_from_bytes`' `Cow::Owned(Layout::for_variant(..))` is const eval's
  rendering result, owned and cold.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:37): nothing to change (checked `utils.rs`, `lower/diagnostics.rs` and `generics.rs` as they are now).

## 18. lane db: `crates/hir-ty/src/variance.rs`: the solution in the frame

`variances_of_query` (44: `cycle_fn`/`cycle_initial`, a fixpoint) and `ctor_variances_query` (89)
add the tag. Each fixpoint iteration is one run of the query (contract 1): its solution is a
frame slice of one byte per parameter, gone with the run. What it keeps for the next iteration is
the interned `StoredVariancesOf`, which is owned. Before:

```rust
    let variances =
        Context { generics, variances: smallvec![Variance::Bivariant; count], db }.solve();

    VariancesOf::new_from_slice(&variances).store()
...
    let mut cx = Context { generics, variances: smallvec![Variance::Bivariant; count], db };
...
/// An item's variances while they are worked out: one byte a parameter, on the stack for up
/// to 16.
type Variances = SmallVec<[Variance; 16]>;

struct Context<'db> {
    db: &'db Db,
    generics: Generics<'db>,
    variances: Variances,
}

impl<'db> Context<'db> {
    fn solve(mut self) -> Variances {
```

After:

```rust
    let variances = blaze_mem::frame::current(|frame| {
        let variances = frame.alloc_slice_fill_with(count, |_| Variance::Bivariant);
        VariancesOf::new_from_slice(Context { generics, variances, db }.solve()).store()
    });
    variances
...
    blaze_mem::frame::current(|frame| {
        let variances = frame.alloc_slice_fill_with(count, |_| Variance::Bivariant);
        let mut cx = Context { generics, variances, db };
        ... (unchanged, `let variances = cx.variances;`)
        VariancesOf::new_from_slice(variances).store()
    })
...
struct Context<'db, 'f> {
    db: &'db Db,
    generics: Generics<'db>,
    /// The variances while they are worked out, one byte a parameter, in the run's frame.
    variances: &'f mut [Variance],
}

impl<'db, 'f> Context<'db, 'f> {
    fn solve(self) -> &'f mut [Variance] {
```

(`let mut variances = self.variances;` in `solve` stays: indexing and `iter_mut` are a slice's.)
The `Variances` alias and the `smallvec` import go. This means no global allocation for any
parameter count (today one past 16), at about 2 ns. Test (`crates/hir/src/tests/lower.rs`), with
more than 16 parameters so the old spill is covered:

```rust
#[test]
fn the_variances_of_many_parameters() {
    let db = TestDB::with_files(
        r#"
//- minicore: sized
struct S<'a, A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R>(&'a A, fn(B), fn(C) -> C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R);
"#,
    );
    crate::attach_db(&db, || {
        use rustc_type_ir::Variance::{Contravariant, Covariant, Invariant};
        let (_, ModuleDefId::AdtId(adt)) = type_item(&db, "S") else { panic!("`S` is an ADT") };
        let variances = db.variances_of(hir_def::GenericDefId::AdtId(adt));
        let mut expected = vec![Covariant, Covariant, Contravariant, Invariant];
        expected.extend(std::iter::repeat_n(Covariant, 15));
        assert_eq!(variances.as_slice(), &expected[..]);
    });
}
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:28): with `mem_tag = Infer` (salsa M2's bare variant) in place of `crate::db::INFER_TAG`. `solve` keeps `mut self` (its variant closure calls `&mut self` methods), and the two `let mut variances` become `let variances` (a `&mut [_]` binding needs no `mut`). Test added to `crates/hir/src/tests/lower.rs`.

## 19. lane db: `crates/hir-ty/src/variance/unused.rs`

`adt_unused_params` is a pub entry point called outside queries (`crates/hir/src/lib.rs:1329`,
E0392's check through hir), so it opens a task (R1). Before:

```rust
pub fn adt_unused_params(db: &Db, adt: AdtId) -> Vec<GenericParamId> {
    let def = GenericDefId::AdtId(adt);
    let variances = super::variances_of(db, def);
    let params: SmallVec<[GenericParamId; 8]> = generics(db, def).iter_id(false).collect();
    let mut constrained: FxHashSet<GenericParamId> = params
        ...
        .collect();
    if constrained.len() == params.len() {
        return Vec::new();
    }

    let projections: SmallVec<[_; 4]> = GenericPredicates::query_own_explicit(db, def)
        ...
        .collect();
    let mut inputs = FxHashSet::default();
    ...
    let unused: Vec<GenericParamId> =
        params.iter().copied().filter(|param| !constrained.contains(param)).collect();
    if unused.is_empty() || has_error_deep(db, adt) {
        return Vec::new();
    }
    unused
}
```

After:

```rust
pub fn adt_unused_params(db: &Db, adt: AdtId) -> Vec<GenericParamId> {
    let def = GenericDefId::AdtId(adt);
    let variances = super::variances_of(db, def);
    blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
        let mut params = Vec::new_in(frame);
        params.extend(generics(db, def).iter_id(false));
        let mut constrained = FrameSet::with_hasher_in(FxBuildHasher, frame);
        constrained.extend(
            params
                .iter()
                .zip(variances.as_slice())
                .filter(|&(_, &variance)| variance != Variance::Bivariant)
                .map(|(&param, _)| param),
        );
        if constrained.len() == params.len() {
            return Vec::new();
        }

        let mut projections = Vec::new_in(frame);
        projections.extend(GenericPredicates::query_own_explicit(db, def).iter_identity().filter_map(
            |it| match it.skip_norm_wip().kind().skip_binder() {
                ClauseKind::Projection(projection) => Some(projection),
                _ => None,
            },
        ));
        let mut inputs = FrameSet::with_hasher_in(FxBuildHasher, frame);
        ... (the loop unchanged)

        let mut unused = Vec::new_in(frame);
        unused.extend(params.iter().copied().filter(|param| !constrained.contains(param)));
        if unused.is_empty() || has_error_deep(db, adt, frame) {
            return Vec::new();
        }
        unused.to_vec() // alloc: the caller's list, exact size
    })
}
```

`parameters_for(.., out: &mut FrameSet<'_, GenericParamId>)` and its `Collect<'a, 'f> { out: &'a
mut FrameSet<'f, GenericParamId> }`. `has_error_deep(db, adt, frame: &Frame)` with
`HasErrorDeep<'db, 'f> { seen: FrameSet<'f, AdtId> }` and
`seen: FrameSet::with_hasher_in(FxBuildHasher, frame)`. The `FxHashSet` and `SmallVec` imports
go. `inputs.is_subset(&constrained)` works: both are `FrameSet<'_, GenericParamId>` of the same
frame. The returned `Vec` stays: it is the public result, and its callers own it. Test: the
existing E0392 tests (`blaze-diagnostics`' unused-parameter tests through hir) hold the result.
Add (`crates/hir/src/tests/lower.rs`) one where a projection constrains a parameter, the
fixpoint's case:

```rust
#[test]
fn a_parameter_a_projection_constrains_is_used() {
    let db = TestDB::with_files(
        r#"
//- minicore: sized
trait T { type X; }
struct S<A, B, C>(A);
struct U<P, Q>(P) where P: T<X = Q>;
"#,
    );
    crate::attach_db(&db, || {
        let adt = |name| match type_item(&db, name) {
            (_, ModuleDefId::AdtId(it)) => it,
            it => panic!("`{name}` is an ADT, not {it:?}"),
        };
        assert_eq!(hir_ide::adt_unused_params(&db, adt("S")).len(), 2);
        assert!(hir_ide::adt_unused_params(&db, adt("U")).is_empty());
    });
}
```

Its budget (one allocation, the returned list, once the queries it reads are warm) is a line of
mem-db-infer's `tests/budget.rs`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:31): with `crate::frame_set(frame)` (mem-db-infer 5) in place of `FrameSet::with_hasher_in(FxBuildHasher, frame)`. Test added to `crates/hir/src/tests/lower.rs`.

## 20. lane db: `crates/hir-ty/src/dyn_compatibility.rs`

`dyn_compatibility_of_trait_query` (126) and `generics_require_sized_self` (140) add the tag.
They make no temporaries after this change and open no frame.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:34): `mem_tag = Infer` on both (salsa M2's bare variant, not `crate::db::INFER_TAG`).

### 20a. The supertraits are a memo already

`contains_illegal_self_type_reference` collects the trait's supertraits once per visitor, into a
`SmallVec<[TraitId; 4]>` that spills past four, through `elaborate::supertrait_def_ids` (a
`HashSet` and a stack of its own). The interner's `supertrait_def_ids`
(`next_solver/interner.rs:2126`) is `crate::utils::all_super_traits`, a memo slice, and the
solver already relies on the two being the same set. Before:

```rust
    struct IllegalSelfTypeVisitor<'db> {
        db: &'db Db,
        trait_: TraitId,
        super_traits: Option<SmallVec<[TraitId; 4]>>,
        allow_self_projection: AllowSelfProjections,
    }
    ...
                        if self.super_traits.is_none() {
                            self.super_traits = Some(
                                elaborate::supertrait_def_ids(interner, self.trait_.into())
                                    .map(|super_trait| super_trait.0)
                                    .collect(),
                            )
                        }
                        if self.super_traits.as_ref().is_some_and(|s| s.contains(&trait_)) {
    ...
    let mut visitor =
        IllegalSelfTypeVisitor { db, trait_, super_traits: None, allow_self_projection };
```

After:

```rust
    struct IllegalSelfTypeVisitor<'db> {
        db: &'db Db,
        trait_: TraitId,
        allow_self_projection: AllowSelfProjections,
    }
    ...
                        if crate::utils::all_super_traits(self.db, self.trait_).contains(&trait_) {
    ...
    let mut visitor = IllegalSelfTypeVisitor { db, trait_, allow_self_projection };
```

A differential test holds the equivalence (`crates/hir/src/tests/lower.rs`):

```rust
#[test]
fn the_memo_supertraits_are_what_elaboration_finds() {
    let db = TestDB::with_files(
        r#"
//- minicore: sized
trait A {}
trait B: A {}
trait C: A {}
trait D: B + C where Self: Sized {}
trait F<'a> {}
trait E: for<'a> F<'a> {}
trait G where Self: D + E {}
"#,
    );
    crate::attach_db(&db, || {
        let interner = DbInterner::new_no_crate(&db);
        for name in ["A", "B", "C", "D", "E", "F", "G"] {
            let (_, ModuleDefId::TraitId(trait_)) = type_item(&db, name) else {
                panic!("`{name}` is a trait")
            };
            let mut elaborated: Vec<_> =
                rustc_type_ir::elaborate::supertrait_def_ids(interner, trait_.into())
                    .map(|it| it.0)
                    .collect();
            let mut memo = hir_ide::all_super_traits(&db, trait_).to_vec();
            elaborated.sort();
            memo.sort();
            assert_eq!(elaborated, memo, "`{name}`");
        }
    });
}
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:34): as written (both lists include the trait itself); test added to `crates/hir/src/tests/lower.rs`.

### 20b. The first `impl Trait` in the return type, without a set

`contains_illegal_impl_trait_in_trait` collects every opaque of the return type into an
`FxHashSet`, then looks for one of a `ReturnTypeImplTrait`. Stopping at the first such opaque is
the same answer, with nothing collected. Before:

```rust
    struct OpaqueTypeCollector<'db>(FxHashSet<InternedOpaqueTyId<'db>>);

    impl<'db> rustc_type_ir::TypeVisitor<DbInterner<'db>> for OpaqueTypeCollector<'db> {
        type Result = ControlFlow<()>;

        fn visit_ty(..) -> Self::Result {
            if let rustc_type_ir::TyKind::Alias(
                _,
                AliasTy { kind: AliasTyKind::Opaque { def_id }, .. },
            ) = ty.kind()
            {
                self.0.insert(def_id.0);
            }
            ty.super_visit_with(self)
        }
    }

    let ret = sig.skip_binder().output();
    let mut visitor = OpaqueTypeCollector(FxHashSet::default());
    _ = ret.visit_with(&mut visitor);

    // Since we haven't implemented RPITIT in proper way like rustc yet,
    // just check whether `ret` contains RPIT for now
    for opaque_ty in visitor.0 {
        let impl_trait_id = opaque_ty.loc(db);
        if matches!(impl_trait_id, ImplTraitId::ReturnTypeImplTrait(..)) {
            return Some(MethodViolationCode::ReferencesImplTraitInTrait);
        }
    }

    None
```

After:

```rust
    struct ReturnTypeImplTrait<'db>(&'db Db);

    impl<'db> rustc_type_ir::TypeVisitor<DbInterner<'db>> for ReturnTypeImplTrait<'db> {
        type Result = ControlFlow<()>;

        fn visit_ty(..) -> Self::Result {
            if let rustc_type_ir::TyKind::Alias(
                _,
                AliasTy { kind: AliasTyKind::Opaque { def_id }, .. },
            ) = ty.kind()
                && matches!(def_id.0.loc(self.0), ImplTraitId::ReturnTypeImplTrait(..))
            {
                return ControlFlow::Break(());
            }
            ty.super_visit_with(self)
        }
    }

    // Since we haven't implemented RPITIT in proper way like rustc yet,
    // just check whether `ret` contains RPIT for now
    let ret = sig.skip_binder().output();
    ret.visit_with(&mut ReturnTypeImplTrait(db))
        .is_break()
        .then_some(MethodViolationCode::ReferencesImplTraitInTrait)
```

The `FxHashSet` and `SmallVec` imports go if nothing else uses them. Test: the existing
dyn-compatibility tests (`dyn_compatibility/tests.rs`, `ReferencesImplTraitInTrait` cases) hold
the answer.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:34): as written; the `FxHashSet`, `SmallVec` and `InternedOpaqueTyId` imports went (nothing else used them).

### 20c. Left

`receiver_is_dispatchable` builds an inference context and interned clauses: the solver's
allocations (mem-db-solver). The pub entry points `dyn_compatibility` and
`dyn_compatibility_with_callback` (called by hir and the IDE) make nothing of their own after
20a and 20b, so they open no task.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:34): nothing to change.

## 21. lane db: `crates/hir-ty/src/builtin_derive.rs` and `crates/hir-ty/src/effects.rs`

`builtin_derive::predicates` (173) adds the tag and opens `blaze_mem::frame::current` around its
body. `simple_trait_predicates` takes `frame: &Frame` last (from the two call sites in
`predicates`). Before:

```rust
    let mut assoc_type_bounds = Vec::new();
...
fn extend_assoc_type_bounds<'db>(
    interner: DbInterner<'db>,
    assoc_type_bounds: &mut Vec<Clause<'db>>,
...
    struct ProjectionFinder<'a, 'db> {
        interner: DbInterner<'db>,
        assoc_type_bounds: &'a mut Vec<Clause<'db>>,
```

After:

```rust
    let mut assoc_type_bounds = Vec::new_in(frame);
...
fn extend_assoc_type_bounds<'db>(
    interner: DbInterner<'db>,
    assoc_type_bounds: &mut Vec<Clause<'db>, &Frame>,
...
    struct ProjectionFinder<'a, 'f, 'db> {
        interner: DbInterner<'db>,
        assoc_type_bounds: &'a mut Vec<Clause<'db>, &'f Frame>,
```

(`impl<'db> TypeVisitor<DbInterner<'db>> for ProjectionFinder<'_, '_, 'db>`.)
`Clauses::new_from_iter(..chain(assoc_type_bounds))` takes the frame list by value, unchanged.
`param_env` (pub, used by hir) makes nothing.

`effects.rs`: `const_param_env_query` (166) adds the tag. It has no temporaries of its own
(`elaborate` is rustc_type_ir's, `Clauses::new_from_iter` interns) and opens no frame. Left:
- `builtin_derive_const_conditions` returns a `Vec` that its one caller (the interner's
  `const_conditions`, `next_solver/interner.rs:1671`) consumes, where the other arms collect a
  `Vec` too. It is mem-db-solver's to make that an iterator; this file keeps the `Vec` (the
  interface's owned value).
- `const_call_error`'s `unsatisfied: Vec` is `NonConst::Unsatisfied`'s field: the returned error,
  owned, on an error path.
- `non_const_calls` (pub, `hir-ide/src/tests/const_effects.rs:38`) returns its `Walk.found` list.
  It is owned, and it makes no other temporaries, so it opens no task.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:37): `builtin_derive.rs` as written, with `mem_tag = Infer` (salsa M2's bare variant); `frame::current` wraps the `match` only (the lets before it make nothing), so the arms keep their indentation.
> applied by db-mem-infer-b-lower (21:52): `effects.rs`'s `const_param_env_query` tag, `mem_tag = Infer`.

## 22. Dependent edits in other parts' files

These are in files of other parts of mem-db-infer and of other lanes. Each goes in the same change
as the signature it follows, or the crate does not build. mem-db-infer passes them on.

| file (part or lane) | site | change |
|---|---|---|
| `hir-ty/src/infer/diagnostics.rs` (ctx) | `InferenceTyLoweringContext::new` | takes `frame: &'a Frame` last and passes it to `TyLoweringContext::new(.., lifetime_lowering_mode, frame)`. Its callers (`infer.rs` `with_ty_lowering` and the other two, `infer/path.rs:158`) pass the `InferenceContext`'s `self.frame` (`&'f Frame` coerces to `&'a`) |
| `hir-ty/src/infer/diagnostics.rs` (ctx) | `Drop for InferenceTyLoweringContext` | `push_ty_diagnostics(std::mem::take(&mut self.ctx.diagnostics))` becomes `push_ty_diagnostics(self.ctx.diagnostics.drain(..))`, and `Diagnostics::push_ty_diagnostics` takes `impl IntoIterator<Item = TyLoweringDiagnostic>`. `forget_diagnostics`' `clear()` is unchanged |
| `hir-ty/src/infer/closure.rs:1021` (closure) | `lower_closure_params` | `let mut supplied = Vec::with_capacity_in(decl_inputs.len(), self.frame); let bound_vars = self.with_ty_lowering(\|ctx\| ctx.lower_closure_params(decl_inputs, &mut supplied));` in place of the tuple. The rest reads `supplied` as before |
| `hir-ty/src/infer/coerce.rs:940` (expr) | `bind_elided_lifetimes(self.db, function, sig)` | passes the inference's frame last |
| `hir-ty/src/method_resolution/confirm.rs:464` (method) | `substs_from_args_and_bindings(..)` | passes the inference's frame last |
| `hir-ty/src/traits.rs:405` (method) | `where_predicate_must_hold`, pub, called outside queries (`hir/src/source_analyzer.rs:395`) | opens `blaze_mem::frame::task(crate::db::INFER_TAG, \|frame\| ..)`, passes `frame` to `TyLoweringContext::new`, and its `.collect::<Vec<_>>()` of clauses becomes a frame `Vec` |
| `hir/src/source_analyzer.rs:459`, `:1888`, `:2093` (lane frontend/hir) | `TyLoweringContext::new` outside queries | each in `blaze_mem::frame::task(blaze_mem::Tag::Infer, \|frame\| ..)`, `frame` last. hir adds the `blaze-mem` dependency |
| `hir-ty-borrowck/src/regions.rs:4312`, `:4676` (mem-db-checks regions) | `hir_ty::TyLoweringContext::new` inside borrowck's query | passes its run's frame (`frame::current` at its query's top) |
| `hir-ty-borrowck/src/regions.rs:1166`, `:5686`, `hir-ty-wfcheck/src/lib.rs:328` (mem-db-checks) | `.regions(&errors)` | `let mut regions = Vec::new_in(frame); elided.regions_into(&errors, &mut regions);` (or a global `Vec::new()` until that crate is on the frame) |

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): `hir/src/source_analyzer.rs`' three calls (`type_of_type`, `resolve_hir_path_`, `resolve_hir_path_qualifier`) each in `blaze_mem::frame::task(blaze_mem::Tag::Infer, ..)`, `store` taken before the task where it was `?`-ed (hir already depends on blaze-mem). The other rows are their owners': `infer/diagnostics.rs` and `infer/closure.rs` already match; `infer/coerce.rs` (db-mem-infer-a-next1/-expr), `confirm.rs` and `traits.rs` (db-mem-infer-b; `traits.rs` has its task, missing only the `frame` argument), borrowck `regions.rs` and wfcheck `lib.rs` (db-mem-borrowck) told by tmux at 21:51-21:53. Not in this table: frontend-tree-move-hir's new `hir/src/source_analyzer/view.rs:191` and dedyn-ty's new `hir-ty/src/tests/infer_vars.rs:97`, told the same way.
> applied by db-mem-borrowck (22:02): the last two rows. Borrowck `regions.rs` `elided_groups` and the lowering helper pass `self.ctx.frame` last to `TyLoweringContext::new`; the two `.regions(&errors)` there and wfcheck `lib.rs`'s become `regions_into(&errors, &mut regions)` into a frame `Vec`. Both crates are on the frame, so `errors` is a frame list of frame lists too (borrowck `filled(frame, n, Vec::new_in(frame))`, wfcheck `mem::collect(frame, repeat_n(..))`), and the three `// alloc:` notes that waited on this are gone.

## 23. Every `TyLoweringContext::new` and the frame it passes

`rg -n "TyLoweringContext::new" crates/`. `infer.rs:2387`, `:2698` and `infer/path.rs:158` are
`InferenceTyLoweringContext`, imported as `TyLoweringContext`.

| call | in | frame |
|---|---|---|
| `lower.rs` `lower_impl_trait` | `impl_trait_of_impl`, `crate_impl_traits` | its `frame` parameter: the query's `current`, or the batch item's task |
| `lower.rs` `type_for_const_with_diagnostics`, `type_for_static_with_diagnostics`, `type_for_type_alias_with_diagnostics` | their queries | `current` |
| `lower.rs` `lower_impl_self_ty` | `impl_self_ty_of_impl`, `crate_impl_self_tys` | its parameter, as `lower_impl_trait` |
| `lower.rs` `const_param_types_with_diagnostics` | its query | `current` |
| `lower.rs` `lower_field_types` | `field_types_of_variant`, `crate_field_types` | its parameter |
| `lower.rs` `resolve_type_param_assoc_type_shorthand` | its query | `current` |
| `lower.rs` `type_alias_bounds_with_diagnostics` | its query | `current` |
| `lower.rs` `const_conditions_query` | its query | `current` |
| `lower.rs` `generic_predicates` | `generic_predicates_of`, `crate_impl_predicates` | its parameter |
| `lower.rs` `generic_defaults_with_diagnostics` | its query | `current` |
| `lower.rs` `fn_sig_for_fn` (two) | its query | `current`, both |
| `infer/diagnostics.rs:118` | `InferenceTyLoweringContext::new` | the `InferenceContext`'s `frame` (22) |
| `traits.rs:405` | `where_predicate_must_hold` | its own task (22) |
| `hir/src/source_analyzer.rs:459`, `:1888`, `:2093` | IDE, outside queries | a task each (22) |
| `hir-ty-borrowck/src/regions.rs:4312`, `:4676` | borrowck's query | its run's frame (22) |

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): every `lower.rs` call passes its frame (checked with `rg`); the rest as 22 says.

## 24. R1 entry points of these files called outside queries

| entry point | callers | frame |
|---|---|---|
| `variance::unused::adt_unused_params` | `hir/src/lib.rs:1329` | `frame::task(INFER_TAG)` (19) |
| `lower::persist` readers (`field_types`, `impl_self_tys`, `impl_traits`, `impl_predicates`) | the image loader, `hir/src/tests.rs:794` to `834` | a task in `side_table` only (16). The rest are exact columns with no temporaries |
| `ElidedLifetimes::regions_into` | borrowck and wfcheck queries | the caller's list (11) |
| `TyLoweringContext::new` (pub) | hir's source analyzer | the caller's task (22) |
| `dyn_compatibility`, `dyn_compatibility_with_callback`, `builtin_derive::param_env`, `effects::non_const_calls`, `effects::const_call_error`, `effects::const_param_env` | hir, IDE, the interner | none: they make no temporaries of their own after 20 and 21 |

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): nothing more to change (19, 16, 11 and 22 carry them).

## 25. Tags

`mem_tag = crate::db::INFER_TAG` on every tracked fn of these files (28):
- `lower.rs` (18):
  - `impl_trait_of_impl`, `type_for_const_with_diagnostics`, `type_for_static_with_diagnostics`,
    `type_for_type_alias_with_diagnostics`, `impl_self_ty_of_impl`;
  - `const_param_types_with_diagnostics`, `field_types_of_variant`, `supertraits_info`,
    `resolve_type_param_assoc_type_shorthand`, `type_alias_bounds_with_diagnostics`;
  - `generic_predicates_of`, `trait_environment_query`, `const_conditions_query`,
    `generic_defaults_with_diagnostics`;
  - `fn_sig_for_fn`, `elided_lifetimes`, `fn_sig_for_struct_constructor`,
    `fn_sig_for_enum_variant_constructor`.
- `lower/batch.rs` (4): `crate_field_types`, `crate_impl_self_tys`, `crate_impl_traits`,
  `crate_impl_predicates`.
- `variance.rs` (2): `variances_of_query`, `ctor_variances_query`.
- `dyn_compatibility.rs` (2): `dyn_compatibility_of_trait_query`, `generics_require_sized_self`.
- `builtin_derive.rs` (1): `predicates`.
- `effects.rs` (1): `const_param_env_query`.

For one with more options, the tag goes last:
`#[salsa::tracked(returns(ref), cycle_result = X, mem_tag = crate::db::INFER_TAG)]`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): all 28, each written `mem_tag = Infer` (salsa's option as filed, mem-db-salsa M2, takes a bare `blaze_mem::Tag` variant), last among the options.

## 26. What stays, and why

Every remaining `.clone()`, `Vec::new()`, `vec![`, `.collect()`, `.to_vec()`, `Box::new(`,
`FxHashMap::default()`, `FxHashSet::default()`, `SmallVec`, `ThinVec::new()`, `IndexMap` or
`IndexSet` in these files after 1 to 21:

| site | why it stays |
|---|---|
| `lower.rs` `push_infer_vars_not_allowed`, `push_elision_failure`: `Box::new([span])`, `Box::new([site])`, `into_vec` + push + `into_boxed_slice` | an error's sites, owned by the diagnostic, cold (already `// alloc:`) |
| `lower.rs` `for_lifetime_region_kind`'s `name.symbol().clone()`, `supertraits_info`'s `name.clone()` and `it.clone()`, shorthand's `assoc_name.clone()` and `this_trait_resolution.clone()`, `associated_type_by_name_..`'s `name.clone()` | copies of a 4-byte `Symbol`/`Name`, or of an interned row (`StoredEarlyBinder<..>` is `Copy`-sized) |
| `lower.rs` `lower_dyn_trait`'s `SmallVec::<[_; 3]>` auto traits | never spills: at most three auto traits are written in practice. A fourth spills once, correctly |
| `lower.rs` `TyLoweringResult::empty`, the `ThinVec::new()` of `impl_traits`, `GenericDefaults(ThinVec::new())`, `ConstParamTypes::default()` | empty: no allocation |
| `lower.rs` `elided_lifetimes`' `params` `Vec::with_capacity(len)` + `into_boxed_slice` | the memo's column, one exact allocation |
| `lower.rs` `GenericDefaults: Clone` | a derive. Nothing on a hit path clones it after 10j |
| `lower.rs` tests' `vec![..]`, `Vec::new()`, `Box::new([segment])` | test data |
| `lower/path.rs` `prohibit_generic_args`, `with_args`' `collect`, the `vec![one segment]`s, `handle_type_ns_resolution`'s `prefix` | an error's segments, owned by the diagnostic. `with_args` allocates only on that error |
| `lower/path.rs` `binding.name.clone()`, `assoc_name.clone()` | a 4-byte `Name` |
| `lower/batch.rs` `values` `Vec::with_capacity(keys.len())`, `starts` `with_capacity(n + 1)`, `filled`'s `collect` | the batch's own columns, exact, one allocation each |
| `lower/batch.rs` cycle results' `module_impls(.., Global)`, `error.clone()`, `empty.clone()` | the cycle results, reached only by code rustc rejects. The clones are of interned rows |
| `lower/batch.rs` `radix::sort_copy_by_key` | blaze-radix's scratch (global), once per crate's sort: fix-radix's to take `Vec<T, A>` |
| `lower/batch.rs` tests' `Box::new([segment])` | test data |
| `lower/persist.rs` writers (`ColdBatch::of_*`, `ColdSide::of`, `Vec::new()` fields, `collect`, `to_vec`) | the image's written columns, owned output, once per crate written |
| `lower/persist.rs` `exact`'s `with_capacity`, `Box::from(self.starts)`, `Box::from(side.rows)` etc. | a read batch's own columns, exact |
| `lower/persist.rs` `Words::diagnostic`'s `sites` | a diagnostic's own box, exact |
| `lower/persist.rs` tests | test data |
| `generics.rs` `ArrayVec::new()` | inline, no allocation |
| `utils.rs` `Cow::Owned(Layout::for_variant(..))` | const eval's rendered result, owned, cold |
| `variance/unused.rs` the returned `unused.to_vec()`, the `return Vec::new()`s | the public result (exact, one allocation), or empty |
| `effects.rs` `builtin_derive_const_conditions`' `Vec::new()`/`collect` | the interner interface's owned value (mem-db-solver to change, 21) |
| `effects.rs` `const_call_error`'s `unsatisfied` `collect` | the returned error's list, owned, an error path |
| `effects.rs` `non_const_calls`' `Vec::new()`s and `found` | the returned list, owned |

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): nothing to change; hot_lint's remaining `lower.rs` findings are these `Name`/`Symbol` clones.

## 27. Speed

Per allocation: about 2 ns from the frame, against 15 to 25 ns from the general allocator (with
its later free and the cache misses of scattered blocks).

**A 1,000-expression body** (inference's lowering of the types and paths written in it).
Inference makes one `InferenceTyLoweringContext` per path expression, annotation, cast and
turbofish: about 250 in such a body. About 150 path segments name a generic item, and about 10
closures have annotated parameters.

| temporary | per body today | after |
|---|---|---|
| `TyLoweringContext` binder list (`bound_vars`) | 250 inline `SmallVec`s (72 bytes copied each), plus one global allocation per `for<'a>` binder's names | 250 frame lists of 16 bytes: 4 KB, 250 × 2 ns = 0.5 µs, no global allocation |
| `substs_from_args_and_bindings`' list | 150 inline `SmallVec<[_; 8]>`s (one global allocation each past 8 arguments) | 150 frame lists of about 3 × 8 bytes: 3.6 KB, 0.3 µs |
| `lower_closure_params`' `collect` | 10 global allocations: 10 × 20 ns = 0.2 µs | in the closure's frame list: 0.02 µs |
| `bind_elided_lifetimes` (a function item reified with elided lifetimes, a few per body) | 3 global allocations each | 3 frame lists |
| diagnostics, anonymous constants, `unsized_types`, `elisions` | none in a body without errors (empty lists) | none |

Total: about 8 KB of frame per body and 0.8 µs of frame allocation, against about 15 global
allocations (0.3 µs) plus the `SmallVec` copies today. Lane db's `SmallVec`s have already taken
the hot path of a body near zero global allocations. What this part adds there is that nothing
spills: no cliff at more than 2 binders, 8 generic arguments, 4 named lifetimes or 8 own
predicates. The frame lists are freed with the run, at no cost.

**Signatures** (per item, once per crate, not per body). For a crate of 10,000 items, today's
temporaries make about 4 global allocations per item:
- `generic_predicates`' combined list;
- `supertraits_info`'s three lists and their reallocations;
- `elided_lifetimes`' five lists per parameter;
- `const_param_types`' and `generic_defaults`' grow-then-shrink;
- the readers' doubling columns.

That is 40,000 × 20 ns = 0.8 ms. After: 0, plus one exact allocation per non-empty owned list (the
memo's own), about 1.2 per item: 12,000 × 20 ns = 0.24 ms. Temporaries move to the frame at
about 2 ns each, 0.08 ms. A crate's batch peaks at its largest item's temporaries (15b), not at
the sum of all of them.

**Speed-of-light bound.** What cannot go is one allocation per non-empty list a memo owns. Every
other byte of lowering is interned or in the frame. Lowering a path costs 50 to 200 ns (name
resolution, `generics`, interning `GenericArgs`). The 2 ns of frame per temporary leaves the
allocator under 2% of lowering, against about 10% for a lowering that allocated each temporary
globally.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-b-lower (21:52): nothing to change (notes only).
