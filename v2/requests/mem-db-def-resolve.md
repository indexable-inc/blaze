# Requests from mem-db-def-resolve (2026-10-29)

Slice: `crates/hir-def/src/` `nameres/path_resolution.rs`, `item_scope.rs`, `resolver.rs`,
`per_ns.rs`, `visibility.rs`, `find_path.rs`, `dyn_map.rs`. Notes:
`docs/v2/plan/lanes/mem/db-def-resolve.md`. Parent: mem-db-def (`v2/requests/mem-db-def.md`, whose
H1 to H3 these need: the `blaze-mem`/`hashbrown` deps, `#![feature(allocator_api)]`, and
`#[cfg(test)] #[global_allocator] static ALLOC: blaze_alloc_count::Counting` in `lib.rs`).

Owners: `item_scope.rs`, `resolver.rs`, `nameres/path_resolution.rs` to **db-expand-names**;
`find_path.rs`, `visibility.rs`, `dyn_map.rs`, `per_ns.rs` to **db-expand**; `crates/hir/` to
**db** and **mem-db-def-hir**; `crates/hir-ty/src/infer.rs` to **mem-db-infer-ctx**.

What is already right, and stays:

- `path_resolution.rs` allocates nothing on a warm def map: segments are subslices, `PathQuery`
  goes by value, `PerNs` is `Copy`, and each segment is one `ItemScope::get` (an Fx hash and a
  probe) or a memo hit of `name_in_scope`. `TEST_HARNESS` interns once per process. The only open
  request there is speed-hir-sol-nameres' (`v2/requests/db-expand.md` §3, `name_in_module`); this
  file adds T1 to hold it at 0 for every shape of path.
- `per_ns.rs`: no allocation, nothing to change.
- `visibility.rs`: every check is a walk up `containing_module` with no buffer;
  `field_visibilities` builds its owned `ArenaMap` at exact size. One change, R7.
- The resolver's scope stack inline (`SmallVec<[Scope<'db>; 8]>`, `ModuleId::resolver`'s
  `Vec::new()`) is speed-hir-sol-nameres' request (`v2/requests/db-expand.md` §2) and is not
  filed again here (R6).

Signatures the collector calls stay as they are: `resolve_path_fp_with_macro`,
`resolve_path_fp_recording`, `resolve_path_fp_with_macro_single`, `ItemScope::push_res_with_import`
(made generic over the allocator, so existing callers infer `Global`), `ItemScope::get`. The one
rename, `ItemScope::shrink_to_fit` to `settle`, is mem-db-def-collect's E1 (its A4 calls
`settle`), which R2 and R3 fill in. R10 (mem-db-def's follow-up) moves the def map being
collected into the collector's frame. Calls keep their method syntax and arguments, but the
receiver becomes a `BuildingDefMap<'f>` and callers import the `ResolvePath` trait.

## R1. `ItemScope`: by-name and by-def tables built at settle, small scopes sorted on the stack (`item_scope.rs`, db-expand-names)

Today `sorted()` (1110) and `by_def()` (1388) build their `Box<[u32]>` on the first read after
a change, collecting into a fresh `Vec` (and `radix::sort_by_key`'s own scratch) and boxing it.
In a collected scope that read happens on a hit path: the first `entries()`, `resolutions()`,
`name_of` or `names_of` after the memo is made allocates the table, and `entries()` on a scope
of eight names or fewer allocates a 32-byte table it then keeps.

After: a settled scope (R2's `settle`, and `seal` for frozen scopes) has both tables when it has
more than `LINEAR` names, built in blaze-mem scratch and copied out once at exact size. A scope
of at most `LINEAR` names keeps neither: `entries()` sorts its positions in an inline
`SmallVec<[u32; LINEAR]>` each call, and `naming` already scans the column there. The lazy
`get_or_init` remains only for a scope still being collected, where a change takes the table
(`added`, 1074) and the next read rebuilds it.

Replace `sorted` (1109 to 1118):

```rust
    /// Every name once, by text, in a scope of more than [`LINEAR`] names: made by
    /// [`Self::settle`] or [`Self::seal`] for the memo's scope, and here only for a scope still
    /// being collected, once per change it is read after.
    fn sorted(&self) -> &[u32] {
        // alloc: a scope being collected, once per change; a settled one has its table
        self.sorted.get_or_init(|| self.sorted_table())
    }

    /// Every name once, by text, for a scope of at most [`LINEAR`] names.
    fn sorted_small(&self) -> SmallVec<[u32; LINEAR]> {
        // alloc: none, at most `LINEAR` positions inline
        let mut all: SmallVec<[u32; LINEAR]> =
            self.all_names().map(|(ns, pos, _)| ns << 30 | pos as u32).collect();
        let kept = self.by_name(&mut all);
        all.truncate(kept);
        all
    }

    /// The table [`Self::sorted`] keeps, sorted in scratch and copied out at its length.
    fn sorted_table(&self) -> Box<[u32]> {
        scratch::with_u32s(|all| {
            all.extend(self.all_names().map(|(ns, pos, _)| ns << 30 | pos as u32));
            let kept = self.by_name(all);
            // alloc: the table, at its exact length
            Box::from(&all[..kept])
        })
    }

    /// Sorts `all` by name and moves the first position of each name to the front, returning how
    /// many names there are.
    fn by_name(&self, all: &mut [u32]) -> usize {
        all.sort_unstable_by(|&a, &b| self.sorted_name(a).cmp(self.sorted_name(b)));
        let mut kept = 0;
        for at in 0..all.len() {
            if kept == 0 || self.sorted_name(all[kept - 1]) != self.sorted_name(all[at]) {
                all[kept] = all[at];
                kept += 1;
            }
        }
        kept
    }

    /// Builds the tables a settled scope is read by when it has more than [`LINEAR`] names, and
    /// drops them when it has fewer (a small scope scans its columns).
    fn fill_tables(&mut self) {
        if self.indexed_len() <= LINEAR {
            self.sorted = OnceLock::new();
            self.by_def = OnceLock::new();
            return;
        }
        if self.sorted.get().is_none() {
            let table = self.sorted_table();
            self.sorted = OnceLock::from(table);
        }
        if self.by_def.get().is_none() {
            let table = self.by_def_table();
            self.by_def = OnceLock::from(table);
        }
    }
```

`by_name` keeps the first of each run, as `dedup_by` did. `Name::cmp` is `Symbol::cmp_order`,
which compares a precomputed `u64` key first, so the sort costs about one load per comparison.
`seal` (1121 to 1139) calls `self.fill_tables();` just before `Ok(())`.

`entries()` (1145 to 1151) becomes:

```rust
    pub fn entries(&self) -> impl Iterator<Item = (&Name, PerNs)> + '_ {
        // FIXME: shadowing
        let order = match self.indexed_len() <= LINEAR {
            true => Either::Left(self.sorted_small().into_iter()),
            false => Either::Right(self.sorted().iter().copied()),
        };
        order.map(move |at| {
            let name = self.sorted_name(at);
            (name, self.get(name))
        })
    }
```

Replace `by_def` (1387 to 1411):

```rust
    /// The entries of the three namespaces, by namespace, def and then position, for a scope of
    /// more than [`LINEAR`] names: made by [`Self::settle`] or [`Self::seal`], and here only for a
    /// scope still being collected.
    fn by_def(&self) -> &[u32] {
        // alloc: a scope being collected, once per change; a settled one has its table
        self.by_def.get_or_init(|| self.by_def_table())
    }

    /// The table [`Self::by_def`] keeps, sorted in scratch and copied out at its length.
    fn by_def_table(&self) -> Box<[u32]> {
        scratch::with_u32s(|all| {
            all.extend(
                (named(TYPES, &self.types.entries))
                    .chain(named(VALUES, &self.values.entries))
                    .chain(named(MACROS, &self.macros.entries))
                    .map(|(ns, pos, _)| ns << 30 | pos as u32),
            );
            // The position last keeps a def's names in column order, which is the order
            // `name_of` (the first) and `names_of` (the first `Some`) answer in.
            // `(at >> 30, it.def_tag, it.def, at & POS_MASK)` packed in order: an `Option<Id>`
            // orders `None` first, then by index and generation.
            scratch::with_u32s(|spare| {
                radix::sort_copy_by_key_in(
                    all,
                    |&at| {
                        let it = self.entry_at(at);
                        let def = it.def.map_or(0, |id| {
                            1u128 << 64 | u128::from(radix::pack_32(id.index(), id.generation()))
                        });
                        u128::from(at >> 30) << 97
                            | u128::from(it.def_tag) << 89
                            | def << 24
                            | u128::from(at & POS_MASK)
                    },
                    spare,
                )
            });
            // alloc: the table, at its exact length
            Box::from(&all[..])
        })
    }
```

The key is the one `sort_by_key` used, and its last field makes every key unique, so the order is
the same. Imports: `use blaze_mem::scratch;`.

`imports()` (1166 to 1176) has no caller anywhere in the tree and allocates (`.sorted().dedup()`
collects). Delete it. `use itertools::Itertools;` (21) then serves only `mod tests` (its
`.sorted()` at 2417) and moves there.

Held by T3 (every read of a settled scope, wide and small, with no warm-up read) and T1.

> forwarded to db (db-expand-names-next2) (requests, pass 18; told by tmux).
> applied by db-expand-names: `sorted`/`by_def` lazy only for a scope being collected, `sorted_small`, `sorted_table`, `by_name`, `by_def_table` (scratch, `radix::sort_copy_by_key_in`), `fill_tables` from `seal` and `settle`, `entries` on the stack at most `LINEAR`, `imports()` gone, `Itertools` into `mod tests`. Needs H1 (`blaze-mem`) to build.

## R2. `ItemScope::settle`: each column made again at its length (`item_scope.rs`, db-expand-names)

This is mem-db-def-collect's E1: "`ItemScope::shrink_to_fit` becomes `ItemScope::settle`: each
column made again at its length, not shrunk". The memo's scope is built once at exact size from
the columns that grew while it was collected. It does not keep a doubled buffer realloc'd down.

Add above `impl ItemScope` (or next to `named`):

```rust
/// `v` again at exactly its length: one allocation for a non-empty column, none for an empty one
/// or one already exact.
fn exact<T>(v: &mut ThinVec<T>) {
    if v.capacity() == v.len() {
        return;
    }
    // alloc: the memo's column, at its length
    let mut out = ThinVec::with_capacity(v.len());
    out.extend(v.drain(..));
    *v = out;
}
```

`Keyed::shrink_to_fit` (715 to 719) becomes:

```rust
    /// The map again at its length, indexed with no spare room.
    fn settle(&mut self) {
        exact(&mut self.entries);
        let unique = self.reindex(self.entries.len());
        debug_assert!(unique, "a map of a scope with a key twice");
    }
```

`ItemScope::shrink_to_fit` (1970 to 2029) becomes `settle`. The destructure is unchanged. Each
`x.shrink_to_fit()` on a `ThinVec` field (`underscore_imports`, `glob_ambiguities`,
`shadowed_imports`, `extern_blocks`, `types.entries`, `values.entries`, `macros.entries`,
`unresolved`, `declarations`, `impls`, `trait_impl.0`, `reservation_impl.0`,
`builtin_derive_impls`, `unnamed_consts`, `unnamed_trait_imports`, `extern_crate_decls`,
`use_decls`) becomes `exact(x)`. Each `Keyed` field (`use_imports_*`, `legacy_macros`,
`attr_macros`, `derive_macros`, `macro_invocations`) becomes `x.settle()`. Then a spilled list is
made exact before the map is settled:

```rust
        for (_, macs) in legacy_macros.entries.iter_mut() {
            macs.shrink_to_fit();
        }
        for (_, calls) in derive_macros.entries.iter_mut() {
            calls.shrink_to_fit();
        }
```

(`SmallVec::shrink_to_fit` moves a list back inline when it fits and otherwise reallocates to its
length. These are the only nested buffers of a scope.) The body ends:

```rust
        let unique = self.reindex(self.indexed_len());
        debug_assert!(unique, "a namespace of a scope with a name twice");
        self.fill_tables();
    }
```

The doc comment becomes: "Called once when collection is done, to make the scope the memo keeps:
every column and map made again at its length, the index as small as its load allows, and, for a
scope of more than [`LINEAR`] names, the by-name and by-def tables, so that no read of it
allocates."

If any field is a `Vec` rather than a `ThinVec` when this lands, `exact` gets a `Vec` twin with
the same body. The tests at 2550 and 2593 call `scope.settle()`, and the one at 2627 calls
`keyed.settle()`. Held by T3 and by mem-db-def-collect's Part F.

> forwarded to db (db-expand-names-next2) (requests, pass 18; told by tmux).
> applied by db-expand-names: `exact`, `Keyed::settle`, `ItemScope::settle` (SmallVecs of `legacy_macros`/`derive_macros` shrunk, then `fill_tables`); the one caller (`nameres.rs:697`) and the three tests renamed.

## R3. `PerNsGlobImports<A>` (`item_scope.rs` 40 to 44, 125 to 135, 1675; db-expand-names)

This is the other half of mem-db-def-collect's E1. The collector keeps its glob marks in its
frame (`PerNsGlobImports<&'f Frame>`), and the facts engine's carried state keeps `Global`.

```rust
/// The names of a scope being collected that a glob gave their binding, per namespace: a
/// build-time table, in the collector's frame or (for the facts engine's carried state) the heap.
pub struct PerNsGlobImports<A: Allocator + Clone = Global> {
    types: hashbrown::HashSet<(ModuleId, Name), FxBuildHasher, A>,
    values: hashbrown::HashSet<(ModuleId, Name), FxBuildHasher, A>,
    macros: hashbrown::HashSet<(ModuleId, Name), FxBuildHasher, A>,
}

impl<A: Allocator + Clone> fmt::Debug for PerNsGlobImports<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PerNsGlobImports")
            .field("types", &self.types)
            .field("values", &self.values)
            .field("macros", &self.macros)
            .finish()
    }
}

impl Default for PerNsGlobImports {
    fn default() -> Self {
        PerNsGlobImports::new_in(Global)
    }
}

impl<A: Allocator + Clone> PerNsGlobImports<A> {
    pub(crate) fn new_in(alloc: A) -> Self {
        PerNsGlobImports {
            types: hashbrown::HashSet::with_hasher_in(FxBuildHasher, alloc.clone()),
            values: hashbrown::HashSet::with_hasher_in(FxBuildHasher, alloc.clone()),
            macros: hashbrown::HashSet::with_hasher_in(FxBuildHasher, alloc),
        }
    }

    /// The glob marks of `scope`, the scope of `module`, sized to them.
    pub(crate) fn of_scope_in(module: ModuleId, scope: &ItemScope, alloc: A) -> Self {
        let types = || {
            scope.types.iter().filter(|(_, it)| matches!(it.import, Some(ImportOrExternCrate::Glob(_))))
        };
        let values =
            || scope.values.iter().filter(|(_, it)| matches!(it.import, Some(ImportOrGlob::Glob(_))));
        let macros = || {
            scope.macros.iter().filter(|(_, it)| matches!(it.import, Some(ImportOrExternCrate::Glob(_))))
        };
        let set = |len: usize, alloc: A| {
            hashbrown::HashSet::with_capacity_and_hasher_in(len, FxBuildHasher, alloc)
        };
        let mut marks = PerNsGlobImports {
            types: set(types().count(), alloc.clone()),
            values: set(values().count(), alloc.clone()),
            macros: set(macros().count(), alloc),
        };
        marks.types.extend(types().map(|(name, _)| (module, *name)));
        marks.values.extend(values().map(|(name, _)| (module, *name)));
        marks.macros.extend(macros().map(|(name, _)| (module, *name)));
        marks
    }

    // `contains_type`, `contains_value`, `contains_macro` (125 to 135) move here unchanged.
}

impl PerNsGlobImports {
    pub(crate) fn of_scope(module: ModuleId, scope: &ItemScope) -> PerNsGlobImports {
        PerNsGlobImports::of_scope_in(module, scope, Global)
    }
}
```

`of_scope` is the function `facts/macros.rs:1054` already calls, which
frontend-nameres-core-macros filed with db (`v2/requests/db.md`, around line 3581) with the same
answer. Land it once, as here.

`push_res_with_import` (1675) becomes
`pub(crate) fn push_res_with_import<A: Allocator + Clone>(&mut self, glob_imports: &mut PerNsGlobImports<A>, lookup: (ModuleId, Name), def: PerNs, import: Option<ImportOrExternCrate>) -> bool`,
with the body unchanged (`hashbrown::HashSet` has the same `insert`/`remove`/`contains`).

Imports: `std::alloc::{Allocator, Global}` (H2's feature), and `rustc_hash::FxBuildHasher`
(already there). `FxHashSet` goes if nothing else uses it. Callers that build with
`PerNsGlobImports::default()` (`collector.rs:788`, `facts/unit.rs:70`,
`facts/collect.rs:432,469,821`, `facts/macros.rs:900`, and the tests at 2262, 2487 and 2568) are
unchanged. The collector's switch to `new_in(frame)` is mem-db-def-collect's B table, line 328.

> forwarded to db (db-expand-names-next2) (requests, pass 18; told by tmux).
> applied by db-expand-names: `PerNsGlobImports<A>` on `hashbrown`, `Debug`, `Default`, `new_in`, `of_scope_in`, `of_scope`; `push_res_with_import<A>`; `FxHashSet` import moved into `mod tests`. Needs H1-H2 (`hashbrown`, `allocator_api`) to build.

## R4. `Resolver`: the traits in scope through a visitor (`resolver.rs` 849 to 889, db-expand-names)

`traits_in_scope` builds a fresh `FxHashSet` on every call. `can_use_trait_methods` (hir) builds
the whole set for one `contains`, and inference builds one per body. The walk itself reads memos
(`ModuleData::traits_in_scope` iterates the collected scope's `traits()` memo, and the
impl-trait path goes through `ImplSignature::of`) and allocates nothing. Only the set allocates.

Replace 855 to 889 with:

```rust
    /// Adds the traits in scope to `traits`, for a caller that refills one set as it walks
    /// (clear it first for exactly [`Self::traits_in_scope`]), or that keeps it in its frame.
    /// The resolver's [`Self::module`] names which traits these are: two positions of one walk
    /// with the same module have the same traits in scope.
    pub fn traits_in_scope_into(&self, db: &Db, traits: &mut impl Extend<TraitId>) {
        self.for_each_trait_in_scope(db, |it| traits.extend(iter::once(it)));
    }

    /// Calls `f` with each trait in scope, innermost scope first. A trait in scope twice (from
    /// a block and from the module) comes twice.
    pub fn for_each_trait_in_scope(&self, db: &Db, mut f: impl FnMut(TraitId)) {
        let _ = self.try_for_each_trait_in_scope(db, &mut |it| {
            f(it);
            ControlFlow::<()>::Continue(())
        });
    }

    /// Whether `trait_` is in scope: [`Self::for_each_trait_in_scope`]'s walk, stopping at it.
    pub fn has_trait_in_scope(&self, db: &Db, trait_: TraitId) -> bool {
        self.try_for_each_trait_in_scope(db, &mut |it| match it == trait_ {
            true => ControlFlow::Break(()),
            false => ControlFlow::Continue(()),
        })
        .is_break()
    }

    fn try_for_each_trait_in_scope<B>(
        &self,
        db: &Db,
        f: &mut impl FnMut(TraitId) -> ControlFlow<B>,
    ) -> ControlFlow<B> {
        // FIXME(trait_alias): Trait alias brings aliased traits in scope! Note that supertraits of
        // aliased traits are NOT brought in scope (unless also aliased).
        for scope in self.scopes() {
            match scope {
                Scope::BlockScope { def_map, module_id } => {
                    for it in def_map.module(db, module_id).traits_in_scope(db) {
                        f(it)?;
                    }
                }
                Scope::GenericParams { def: GenericDefId::ImplId(impl_), .. } => {
                    let impl_data = ImplSignature::of(db, impl_);
                    if let Some(target_trait) = impl_data.target_trait
                        && let Some(TypeNs::TraitId(trait_)) = self
                            .resolve_path_in_type_ns_fully(db, &impl_data.store[target_trait.path])
                    {
                        f(trait_)?;
                    }
                }
                _ => (),
            }
        }
        if let Some((prelude, _use)) = self.module_scope.def_map.prelude() {
            let prelude_def_map = prelude.def_map(db);
            for it in prelude_def_map.module(db, prelude).traits_in_scope(db) {
                f(it)?;
            }
        }
        let module = self.module_scope.def_map.module(db, self.module_scope.module_id);
        for it in module.traits_in_scope(db) {
            f(it)?;
        }
        ControlFlow::Continue(())
    }
```

The order is the old one (block and impl scopes, then the prelude, then the module), and
`traits_in_scope` (849) stays as the owned wrapper. Imports: `std::{fmt, iter, mem,
ops::ControlFlow}`. No caller of `traits_in_scope_into` outside this file passes anything but an
`FxHashSet`, which implements `Extend<TraitId>`.

Held by T2 (`for_each_trait_in_scope`, `has_trait_in_scope`, and `traits_in_scope_into` into a
kept set, all 0).

> forwarded to db (db-expand-names-next2) (requests, pass 18; told by tmux).
> applied by db-expand-names: `traits_in_scope_into(impl Extend)`, `for_each_trait_in_scope`, `has_trait_in_scope`, `try_for_each_trait_in_scope`, old order kept.

## R5. `Resolver::names_in_scope_into` (`resolver.rs` 774 to 817, 1282, 1527 to 1566; db-expand-names)

`names_in_scope` builds an `FxIndexMap` and a `SmallVec` per name every call, and sorts the
`macro_use` prelude by collecting it into an itertools `Vec` (799). Its caller
(`hir::SemanticsScope::process_all_names`, completion) calls it once per completion request.

Add the map's type, which hir cannot name today because `FxIndexMap` is private to the crate:

```rust
/// What [`Resolver::names_in_scope`] returns: each name with its meanings, innermost first.
pub type NamesInScope = FxIndexMap<Name, SmallVec<[ScopeDef; 1]>>;

thread_local! {
    /// The `macro_use` prelude of [`Resolver::names_in_scope_into`], put in name order.
    static MACRO_USE_PRELUDE: RefCell<Scratch<(Name, MacroId)>> =
        const { RefCell::new(Scratch::new()) };
}
```

Replace 774 to 817:

```rust
    pub fn names_in_scope(&self, db: &Db) -> NamesInScope {
        let mut names = NamesInScope::default();
        self.names_in_scope_into(db, &mut names);
        names
    }

    /// [`Self::names_in_scope`] into `names`, which it empties first: a caller that keeps one map
    /// allocates only while the map grows, and for a name with more than one meaning (its list
    /// leaves its inline slot).
    pub fn names_in_scope_into(&self, db: &Db, names: &mut NamesInScope) {
        names.clear();
        let mut res = ScopeNames { map: names };
        for scope in self.scopes() {
            self.process_names(scope, &mut res, db);
        }
        let ModuleItemMap { def_map, module_id, local_def_map } = self.module_scope;
        // FIXME: should we provide `self` here?
        def_map.module(db, module_id).scope(db).entries().for_each(|(name, def)| {
            res.add_per_ns(name, def);
        });
        def_map.module(db, module_id).scope(db).legacy_macros().for_each(|(name, macs)| {
            macs.iter().for_each(|&mac| {
                res.add(name, ScopeDef::ModuleDef(ModuleDefId::MacroId(mac)));
            })
        });
        MACRO_USE_PRELUDE.with_borrow_mut(|sorted| {
            let sorted = sorted.take();
            sorted.extend(def_map.macro_use_prelude().iter().map(|(name, &(def, _))| (*name, def)));
            sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            for &(name, def) in sorted.iter() {
                res.add(&name, ScopeDef::ModuleDef(def.into()));
            }
        });
        local_def_map.extern_prelude().for_each(|(name, (def, _extern_crate))| {
            res.add(name, ScopeDef::ModuleDef(ModuleDefId::ModuleId(def)));
        });
        BUILTIN_SCOPE.iter().for_each(|(name, &def)| {
            res.add_per_ns(name, def);
        });
        if let Some((prelude, _use)) = def_map.prelude() {
            let prelude_def_map = prelude.def_map(db);
            for (name, def) in prelude_def_map.module(db, prelude).scope(db).entries() {
                res.add_per_ns(name, def)
            }
        }
    }
```

The commented-out `f(Name::self_param(), ..)` block under the FIXME can go: it is dead text. The
`macro_use` prelude is a map, so its names are unique and the unstable sort gives the old order.
`res.add` doesn't reenter the resolver, so the `RefCell` borrow can't be taken twice. `ScopeNames`
(1527) loses `#[derive(Default)]` and borrows the map:

```rust
struct ScopeNames<'a> {
    map: &'a mut NamesInScope,
}
```

Its `impl` becomes `impl ScopeNames<'_>`, and `process_names` (1282) takes
`acc: &mut ScopeNames<'_>`. In `add` (1534) and `add_local` (1554), `name.clone()` becomes
`*name`; in `extern_crates_in_scope` (846), `(name.clone(), module_id.0)` becomes
`(*name, module_id.0)` (`Name` is `Copy`). `use itertools::Itertools as _;` (11) has no other use
and goes. Add the imports `std::cell::RefCell` and `blaze_mem::scratch::Scratch`, and re-export
`NamesInScope` where hir imports the resolver's types.

Held by T2 (`names_in_scope_into` into a kept map: 0).

> forwarded to db (db-expand-names-next2) (requests, pass 18; told by tmux).
> applied by db-expand-names: `NamesInScope`, `MACRO_USE_PRELUDE`, `names_in_scope_into`, `ScopeNames<'a>`, `*name`, dead FIXME text and `Itertools` gone. The hir re-export of `NamesInScope` is hir's (not done here). Needs H1.

## R6. The resolver's scope stack (reference only)

`Resolver::scopes` as `SmallVec<[Scope<'db>; 8]>` and `ModuleId::resolver`'s `Vec::new()`
(1578) are filed in `v2/requests/db-expand.md` §2 by speed-hir-sol-nameres. Its budget
`a_function_resolver_allocates_at_most_one` (`nameres_budgets.rs`) goes to `assert_at_most(0, ..)`
with that change.

> applied: nothing to apply (reference only) (requests, pass 18).

## R7. `Visibility::is_visible_from` without a span (`visibility.rs` 56, db-expand)

`#[tracing::instrument(skip_all)]` on `is_visible_from` makes a span on every check. It is the
hottest call of `find_path` (two per module visited) and of method resolution's visibility
filter. Delete the line. Nothing else in the file changes. Held by T4, which runs it many times.

> forwarded to db (db-expand-next2) (requests, pass 18; told by tmux).
> applied by db-expand (db-expand-paths, 2026-10-29): the `#[tracing::instrument(skip_all)]` line on `is_visible_from` deleted; nothing else changed.

## R8. `find_path`: a per-worker scratch (`find_path.rs` 45 to 143, 713 to 808; db-expand)

`find_path` makes `FindPathScratch::default()` per call. The first insert into `visited`, the
first segment and each depth's worklist allocate, and all of it is freed at return.
`find_path_with` exists, but nothing outside the file calls it, and its scratch borrows `'db`, so
a caller can't keep one across database revisions or in a `thread_local!`.

After: the scratch has no lifetime (module ids go in as their `salsa::Id`), and `find_path`
lends the thread's own.

```rust
/// What one [`find_path`] call works in: the visited set, the segment arena every candidate
/// points into, and a worklist per depth of the local search. Each thread keeps one, so a call
/// allocates only while it grows.
pub struct FindPathScratch {
    /// `(item, module)` pairs the local searches on the current stack have looked at, the module
    /// as its `salsa::Id`. Empty between top-level searches: each local search removes what it
    /// added.
    visited: FxHashSet<(ItemInNs, salsa::Id)>,
    /// The segments of every candidate of the call, each linked to the one before it, so a
    /// candidate extended by a name shares its prefix instead of copying it.
    segments: Vec<Segment>,
    /// Worklists of [`find_local_import_locations`], one per depth of its recursion.
    worklists: Vec<Vec<(salsa::Id, bool)>>,
}

impl FindPathScratch {
    pub const fn new() -> FindPathScratch {
        FindPathScratch {
            visited: FxHashSet::with_hasher(FxBuildHasher),
            segments: Vec::new(),
            worklists: Vec::new(),
        }
    }
}

impl Default for FindPathScratch {
    fn default() -> Self {
        FindPathScratch::new()
    }
}

thread_local! {
    static SCRATCH: RefCell<FindPathScratch> = const { RefCell::new(FindPathScratch::new()) };
}

pub fn find_path(
    db: &Db,
    item: ItemInNs,
    from: ModuleIdLt<'_>,
    prefix_kind: PrefixKind,
    ignore_local_imports: bool,
    cfg: FindPathConfig,
) -> Option<ModPath> {
    // Taken rather than borrowed: a query `find_path` reads may print a path itself, and gets
    // an empty scratch instead of a borrow error.
    let mut scratch = SCRATCH.take();
    let found =
        find_path_with(&mut scratch, db, item, from, prefix_kind, ignore_local_imports, cfg);
    SCRATCH.set(scratch);
    found
}
```

The `'db` on `FindPathScratch` goes: every `&mut FindPathScratch<'db>` and
`&mut FindPathScratch<'_>` (67, 207, 257, 368, 426, 466, 520, 557, 605, 628, 633, 641, 683, 715,
717) becomes `&mut FindPathScratch`, and `impl<'db> FindPathScratch<'db>` (126) becomes
`impl FindPathScratch`. In `find_local_import_locations`, the worklist holds ids and the loop
turns each back:

```rust
    let mut worklist = st.worklists.pop().unwrap_or_default();
    worklist.extend(
        ctx.from_def_map
            .module(db, ctx.from)
            .children(db)
            .values()
            .copied()
            .chain(iter::successors(ctx.from.containing_module(db), |m| m.containing_module(db)))
            .map(|it| (it.as_id(), false)),
    );
    ...
    while let Some(&mut (id, ref mut processed)) = worklist.get_mut(cursor) {
        cursor += 1;
        if !st.visited.insert((item, id)) {
            continue;
        }
        *processed = true;
        let module = ModuleIdLt::<'db>::from_id(id);
        ...
        for (module, vis) in data.scope(db).modules_in_scope() {
            ...
            if st.visited.contains(&(item, module.as_id())) {
                continue;
            }
            if vis.is_visible_from(db, ctx.from) {
                worklist.push((module.as_id(), false));
            }
        }
    }
    for &(id, processed) in &worklist {
        if processed {
            st.visited.remove(&(item, id));
        }
    }
```

Every id in the scratch comes from `db` during one call, and the scratch is empty between calls
(`reset`, and each local search removes what it added), so `from_id` always gets back an id of
the same database. Imports: `std::cell::RefCell`, `rustc_hash::FxBuildHasher`,
`salsa::plumbing::{AsId, FromId}`. In 298, 359 and 394, `name.clone()` becomes `*name`. The
model in `mod tests` (2438 on) keeps its own fresh set: it is the reference.

Held by T4.

> forwarded to db (db-expand-next2) (requests, pass 18; told by tmux).
> applied by db-expand (db-expand-paths, 2026-10-29): as written. `FindPathScratch` has no lifetime (`visited` and the worklists hold `salsa::Id`), `const fn new`, `Default` by hand, the `SCRATCH` thread-local taken and set back by `find_path`; all 14 `FindPathScratch<'db>`/`<'_>` uses and `impl<'db>` lose the lifetime; `find_local_import_locations` turns ids back with `ModuleIdLt::<'db>::from_id`; the three `name.clone()` are `*name`. The `mod tests` model keeps its own set. The `info_span!` in `find_path_with` is §7's (db-expand.md).

## R9. `DynMap`: a struct of typed maps (`dyn_map.rs`, db-expand; hir change in R9h)

> superseded in the tree (checked by mem-db-def, 2026-10-30 01:11): `dyn_map.rs` already is a
> struct of typed `KeyMap` fields with no `transmute` (speed-unsafe-front-def), and hir-def is
> `#![deny(unsafe_code)]`. Don't apply the file below: its `Index`/`IndexMut` `transmute`s
> would bring back `unsafe`. Only R9h's hir side, if still open, is checked against today's
> `dyn_map.rs`.

Today a `DynMap` is an `stdx::anymap::Map`: each kind of child is a `Box<dyn Any>` holding an
`FxHashMap`, looked up by `TypeId` hash, and each access downcasts. There are 25 keys, every
`(AST, ID)` pair different, and the generic `Policy for (K, V)` (143 to 156) has no user.

After: one field per key, generated by a macro, and `Policy` implemented per pair. Indexing is a
field access, there is no `dyn` or `Box`, and an empty `DynMap` allocates nothing. The struct is
25 × 32 = 800 bytes inline, about the anymap's footprint once three kinds are used. hir caches
one per (container, file) it has looked into, which is tens per request.

The new file, in full:

```rust
//! This module defines a `DynMap`: the maps from a container's syntax to the ids of its children,
//! one map per kind of child, each a field.
//!
//! It is used like this:
//!
//! ```ignore
//! # use hir_def::dyn_map::{DynMap, keys};
//! let mut map = DynMap::default();
//! // To access a specific map, index the `DynMap` by `Key`:
//! map[keys::FUNCTION].insert(ptr, function_id);
//! let value = map[keys::FUNCTION].get(&ptr);
//! ```
//!
//! This is a work of fiction. Any similarities to Kotlin's `BindingContext` are
//! a coincidence.

use std::{
    hash::Hash,
    marker::PhantomData,
    ops::{Index, IndexMut},
};

use rustc_hash::FxHashMap;

pub struct Key<K, V, P> {
    _phantom: PhantomData<(K, V, P)>,
}

impl<K, V, P> Key<K, V, P> {
    pub(crate) const fn new() -> Key<K, V, P> {
        Key { _phantom: PhantomData }
    }
}

impl<K, V, P> Copy for Key<K, V, P> {}

impl<K, V, P> Clone for Key<K, V, P> {
    fn clone(&self) -> Key<K, V, P> {
        *self
    }
}

pub trait Policy {
    type K: Hash + Eq;
    type V;

    fn map(map: &DynMap) -> &FxHashMap<Self::K, Self::V>;
    fn map_mut(map: &mut DynMap) -> &mut FxHashMap<Self::K, Self::V>;
}

macro_rules! dyn_map {
    ($($key:ident $field:ident: $ast:ty => $id:ty;)*) => {
        pub mod keys {
            use std::marker::PhantomData;

            use syntax::{AstPtr, ast};

            use super::*;

            pub type Key<K, V> = crate::dyn_map::Key<AstPtr<K>, V, AstPtrPolicy<K, V>>;

            $(pub const $key: Key<$ast, $id> = Key::new();)*

            /// XXX: AST Nodes and SyntaxNodes have identity equality semantics: nodes are
            /// equal if they point to exactly the same object.
            ///
            /// In general, we do not guarantee that we have exactly one instance of a
            /// syntax tree for each file. We probably should add such guarantee, but, for
            /// the time being, we will use identity-less AstPtr comparison.
            pub struct AstPtrPolicy<AST, ID> {
                _phantom: PhantomData<(AST, ID)>,
            }

            $(
                impl Policy for AstPtrPolicy<$ast, $id> {
                    type K = AstPtr<$ast>;
                    type V = $id;
                    fn map(map: &DynMap) -> &FxHashMap<AstPtr<$ast>, $id> {
                        &map.$field
                    }
                    fn map_mut(map: &mut DynMap) -> &mut FxHashMap<AstPtr<$ast>, $id> {
                        &mut map.$field
                    }
                }
            )*
        }

        /// The children of one container by their syntax, a map per kind of child.
        #[derive(Default)]
        pub struct DynMap {
            $($field: FxHashMap<syntax::AstPtr<$ast>, $id>,)*
        }
    };
}

use either::Either;
use hir_expand::{MacroCallId, attrs::AttrId};
use syntax::ast;

use crate::{
    BlockId, BuiltinDeriveImplId, ConstId, EnumId, EnumVariantId, ExternBlockId, ExternCrateId,
    FieldId, FunctionId, ImplId, LifetimeParamId, Macro2Id, MacroRulesId, ProcMacroId, StaticId,
    StructId, TraitId, TypeAliasId, TypeOrConstParamId, UnionId, UseId,
};

dyn_map! {
    BLOCK block: ast::BlockExpr => BlockId;
    FUNCTION function: ast::Fn => FunctionId;
    CONST const_: ast::Const => ConstId;
    STATIC static_: ast::Static => StaticId;
    TYPE_ALIAS type_alias: ast::TypeAlias => TypeAliasId;
    IMPL impl_: ast::Impl => ImplId;
    EXTERN_BLOCK extern_block: ast::ExternBlock => ExternBlockId;
    TRAIT trait_: ast::Trait => TraitId;
    STRUCT struct_: ast::Struct => StructId;
    UNION union_: ast::Union => UnionId;
    ENUM enum_: ast::Enum => EnumId;
    EXTERN_CRATE extern_crate: ast::ExternCrate => ExternCrateId;
    USE use_: ast::Use => UseId;
    ENUM_VARIANT enum_variant: ast::Variant => EnumVariantId;
    TUPLE_FIELD tuple_field: ast::TupleField => FieldId;
    RECORD_FIELD record_field: ast::RecordField => FieldId;
    TYPE_PARAM type_param: ast::TypeParam => TypeOrConstParamId;
    CONST_PARAM const_param: ast::ConstParam => TypeOrConstParamId;
    LIFETIME_PARAM lifetime_param: ast::LifetimeParam => LifetimeParamId;
    MACRO_RULES macro_rules: ast::MacroRules => MacroRulesId;
    MACRO2 macro2: ast::MacroDef => Macro2Id;
    PROC_MACRO proc_macro: ast::Fn => ProcMacroId;
    MACRO_CALL macro_call: ast::MacroCall => MacroCallId;
    ATTR_MACRO_CALL attr_macro_call: ast::Item => MacroCallId;
    DERIVE_MACRO_CALL derive_macro_call: ast::Meta => (
        AttrId,
        /* derive() */ MacroCallId,
        /* actual derive macros */
        Box<[Option<Either<MacroCallId, BuiltinDeriveImplId>>]>,
    );
}

#[repr(transparent)]
pub struct KeyMap<KEY> {
    map: DynMap,
    _phantom: PhantomData<KEY>,
}

impl<P: Policy> KeyMap<Key<P::K, P::V, P>> {
    pub fn insert(&mut self, key: P::K, value: P::V) {
        P::map_mut(&mut self.map).insert(key, value);
    }

    pub fn get(&self, key: &P::K) -> Option<&P::V> {
        P::map(&self.map).get(key)
    }

    pub fn is_empty(&self) -> bool {
        P::map(&self.map).is_empty()
    }
}

impl<P: Policy> Index<Key<P::K, P::V, P>> for DynMap {
    type Output = KeyMap<Key<P::K, P::V, P>>;
    fn index(&self, _key: Key<P::K, P::V, P>) -> &Self::Output {
        // SAFETY: `KeyMap` is `#[repr(transparent)]` over a `DynMap` (its other field is a
        // zero-sized `PhantomData`), so the two references have one layout and validity.
        unsafe { std::mem::transmute::<&DynMap, &KeyMap<Key<P::K, P::V, P>>>(self) }
    }
}

impl<P: Policy> IndexMut<Key<P::K, P::V, P>> for DynMap {
    fn index_mut(&mut self, _key: Key<P::K, P::V, P>) -> &mut Self::Output {
        // SAFETY: as in `index`: `KeyMap` is `#[repr(transparent)]` over a `DynMap`.
        unsafe { std::mem::transmute::<&mut DynMap, &mut KeyMap<Key<P::K, P::V, P>>>(self) }
    }
}
```

After this, `stdx::anymap` has no user (stdx owner: delete `stdx/src/anymap.rs` and its `mod`).

**R9h, to db / mem-db-def-hir** (`hir/src/semantics/source_to_def.rs` 482): the generic `to_def`
now needs the pair's `Policy` named:

```rust
    fn to_def<Ast: AstNode + 'static, ID: Copy + 'static>(
        &mut self,
        src: InFile<&Ast>,
        key: Key<Ast, ID>,
    ) -> Option<ID>
    where
        keys::AstPtrPolicy<Ast, ID>: Policy<K = AstPtr<Ast>, V = ID>,
    {
        self.dyn_map(src)?[key].get(&AstPtr::new(src.value)).copied()
    }
```

The callers pass concrete keys and are unchanged. Add `dyn_map::Policy` to the import at line 97.
`child_by_source.rs` is unchanged (`DynMap::default()`, `map[keys::X].insert/get/is_empty`).

> forwarded to db (db-expand-next2; R9h's hir part to db-next2) (requests, pass 18; told by tmux).
> applied by dedyn-ide (2026-10-29): `dyn_map.rs` as typed fields from a `keys!` macro, `stdx/src/anymap.rs` deleted; R9h's generic hir bound differs, see `v2/requests/dedyn-ide.md` §2. Test `dyn_map::tests::each_key_has_its_own_map`. Not run.

## Requests to callers outside the slice

**To db and mem-db-def-hir (`crates/hir/src/semantics.rs`):**

- `can_use_trait_methods` (2788): `self.resolver.traits_in_scope(self.db).contains(&t.id)`
  becomes `self.resolver.has_trait_in_scope(self.db, t.id)`. That's no set and no allocation, and
  it stops at the trait.
- `process_all_names` (2757) keeps a map per thread and fills it through R5:

  ```rust
  thread_local! {
      static NAMES_IN_SCOPE: RefCell<resolver::NamesInScope> =
          RefCell::new(resolver::NamesInScope::default());
  }

      /// Calls the passed closure `f` on all names in scope.
      pub fn process_all_names(&self, f: &mut dyn FnMut(Name, ScopeDef<'db>)) {
          // Taken rather than borrowed: `f` may ask another scope for its names.
          let mut scope = NAMES_IN_SCOPE.take();
          self.resolver.names_in_scope_into(self.db, &mut scope);
          for (name, entries) in scope.iter() {
              for &entry in entries {
                  let def = match entry { /* the arms of 2761 to 2780, unchanged */ };
                  f(*name, def)
              }
          }
          NAMES_IN_SCOPE.set(scope);
      }
  ```

- `visible_traits` (2751) and `VisibleTraits(pub FxHashSet<TraitId>)` stay: the set is the
  caller's to keep (completion holds it across many calls).
- `attrs.rs:547` (`resolve_impl_trait_item`, doc links) stays: `MethodResolutionContext` takes
  `&FxHashSet<TraitId>`, and a doc-link resolution builds an inference context that costs far more.
  It follows whatever type mem-db-infer-method gives that field.

**To mem-db-infer-ctx (`crates/hir-ty/src/infer.rs`):** `traits_in_scope: resolver.traits_in_scope(db)`
(1894) becomes a set in the inference frame filled by
`resolver.traits_in_scope_into(db, &mut set)`, which R4 makes generic over `Extend<TraitId>`, so a
`hashbrown::HashSet<TraitId, FxBuildHasher, &'f Frame>` works as is. `get_traits_in_scope` (3046)
does the same into the per-module cache. The field's type (1780) and `MethodResolutionContext`'s
are your slice and mem-db-infer-method's.

**To mem-db-def-collect (done, recorded):** R2 and R3 are your E1 as filed. Nothing on the
collector's side changes beyond your A4 and B tables.

> forwarded to db (db-next2 for `crates/hir/`, db-ty-next2 for `hir-ty/src/infer.rs`) (requests, pass 18; told by tmux).

> applied by db-mem-def (21:39): hir's part. `can_use_trait_methods` calls `has_trait_in_scope`; `process_all_names` takes the thread's `NAMES_IN_SCOPE` (its `thread_local!` inside the function, since an `impl` block can't hold one) and fills it through `names_in_scope_into`. The tree's signature, `f: &mut impl FnMut`, is kept (the text had `&mut dyn`). `visible_traits` and `attrs.rs:547` stay as the text says. The `infer.rs` part is db-ty-next2's. The rest of this file is applied by db-expand-names and db-expand, or deferred with R10 and T5 by Andrew's decision; R9 (and its hir part R9h) is db-expand-next2's and not applied yet.

## Budget tests

Each needs H3's counting allocator. Each asserts with `blaze_alloc_count::assert_at_most(0, ..)`
and builds everything it reads (paths, the resolver, the kept buffers) outside the measure.

> forwarded to db (T1 to T3 db-expand-names-next2, T4 db-expand-next2) (requests, pass 18; told by tmux).
> applied by db-expand-names: T1 (`nameres/tests/paths.rs`), T2 (`resolver/tests.rs`), T3 (`item_scope/tests/budgets.rs`) as written. Not run (no builds in this lane); they need H3's counting allocator in `lib.rs`.
> applied by db-expand (db-expand-paths, 2026-10-29): T4 as written, `a_warm_find_path_allocates_nothing` at the end of `find_path.rs`'s `mod tests`, with `use blaze_alloc_count::assert_at_most;` (`Name`, `PathKind` come through `super::*`). Not run.

### T1. Every shape of path on a warm def map (`nameres/tests/paths.rs`, db-expand-names)

Replace `resolved_path_allocates_nothing` (696 to 716) and delete the hand-rolled
`#[global_allocator]` and `mod alloc_count` (718 to 778). H3's allocator replaces them, and two
global allocators in one test binary don't link. The imports gain
`use std::hint::black_box;` and `use blaze_alloc_count::assert_at_most;`.

```rust
#[test]
fn resolved_path_allocates_nothing() {
    let db = TestDB::with_files(THREE_SEGMENTS);
    let (map, local, path) = three_segment_lookup(&db);
    let resolve = || {
        Res::from(map.resolve_path_fp_with_macro(
            local,
            &db,
            ResolveMode::Other,
            map.root,
            &path,
            BuiltinShadowMode::Module,
            None,
        ))
    };
    let warm = resolve();
    assert!(warm.def.take_types().is_some() && warm.segment_index.is_none());
    let res = assert_at_most(0, "a resolved three-segment path", resolve);
    assert_eq!(res, warm);
}

const SHAPES: &str = r#"
//- /main.rs crate:main deps:dep,pre
#[prelude_import]
use ::pre::prelude::*;
pub mod m {
    pub struct S;
    impl S {
        pub fn new() -> S {
            S
        }
    }
    pub enum E {
        A,
        B(u8),
    }
    pub mod inner {
        pub fn f() {}
    }
}
mod g {
    pub use super::m::*;
}
use g::*;
fn body() {
    mod mb {
        pub struct T;
    }
    $0
}
//- /dep.rs crate:dep
pub mod a {
    pub struct X;
}
//- /pre.rs crate:pre
pub mod prelude {
    pub struct Pre;
}
"#;

/// Budget: one lookup of each shape of path on a warm def map (plain, `self::`, `super::`,
/// `crate::`, through a glob, from the prelude, from the extern prelude with and without `::`, an
/// enum variant, an associated item left unresolved, and from a block's def map, both inside it
/// and out to its crate): 0 allocations.
#[test]
fn every_shape_of_resolved_path_allocates_nothing() {
    let (db, position) = TestDB::with_position(SHAPES);
    let db = &db;
    let block_module = db.module_at_position(position);
    let pair = crate_local_def_map(db, block_module.krate(db));
    let (map, local) = (pair.def_map(db), pair.local(db));
    let block_map = block_module.def_map(db);
    assert!(block_map.block.is_some(), "`$0` is not in a block with items");
    let inner = map
        .modules(db)
        .find(|(id, _)| id.name(db).is_some_and(|it| it.as_str() == "inner"))
        .map(|(id, _)| id)
        .expect("the fixture has `m::inner`");
    let path = |kind: PathKind, segments: &[&str]| {
        ModPath::from_segments(kind, segments.iter().map(|it| name(it)))
    };
    let root = map.root;
    let lookups: [(&DefMap, ModuleId, ModPath, &str); 12] = [
        (map, root, path(PathKind::Plain, &["m", "S"]), "plain"),
        (map, inner, path(PathKind::SELF, &["f"]), "`self::`"),
        (map, inner, path(PathKind::Super(1), &["S"]), "`super::`"),
        (map, root, path(PathKind::Crate, &["m", "inner", "f"]), "`crate::`"),
        (map, root, path(PathKind::Plain, &["S"]), "through a glob"),
        (map, root, path(PathKind::Plain, &["Pre"]), "the prelude"),
        (map, root, path(PathKind::Plain, &["dep", "a", "X"]), "the extern prelude"),
        (map, root, path(PathKind::Abs, &["dep", "a", "X"]), "the extern prelude, `::`"),
        (map, root, path(PathKind::Plain, &["m", "E", "A"]), "an enum variant"),
        (map, root, path(PathKind::Plain, &["m", "S", "new"]), "an associated item"),
        (block_map, block_module, path(PathKind::Plain, &["mb", "T"]), "in a block"),
        (block_map, block_module, path(PathKind::Plain, &["m", "S"]), "a block's crate"),
    ];
    let resolve = |(map, from, path, _): &(&DefMap, ModuleId, ModPath, &str)| {
        Res::from(map.resolve_path_fp_with_macro(
            local,
            db,
            ResolveMode::Other,
            *from,
            path,
            BuiltinShadowMode::Module,
            None,
        ))
    };
    let warm: Vec<Res> = lookups.iter().map(resolve).collect();
    for ((_, _, _, what), res) in lookups.iter().zip(&warm) {
        assert!(res.fixed && !res.def.is_none(), "{what}: does not resolve: {res:?}");
    }
    let shape = |what: &str| &warm[lookups.iter().position(|it| it.3 == what).unwrap()];
    assert!(
        matches!(
            shape("through a glob").def.types,
            Some(it) if matches!(it.import, Some(ImportOrExternCrate::Glob(_)))
        ),
        "`S` at the root is not glob-imported: {:?}",
        shape("through a glob")
    );
    assert!(shape("the extern prelude").differing_crate);
    assert!(shape("the extern prelude, `::`").differing_crate);
    assert!(shape("an enum variant").enum_variant);
    assert_eq!(shape("an associated item").segment_index, Some(2));
    for (it, res) in lookups.iter().zip(&warm) {
        if it.3 != "an associated item" {
            assert_eq!(res.segment_index, None, "{}: not resolved to its end", it.3);
        }
    }
    assert_at_most(0, "a lookup of each shape of path", || {
        for it in &lookups {
            black_box(resolve(it));
        }
    });
}
```

### T2. A resolver's walks on a warm body (new `resolver/tests.rs`, `#[cfg(test)] mod tests;` at the end of `resolver.rs`; db-expand-names)

```rust
//! Allocation budgets of a resolver's walks at the innermost scope of a warm body: a lookup of
//! each kind of name in the type and value namespaces, the traits in scope through a visitor, and
//! the names in scope into a kept map.

use std::hint::black_box;

use blaze_alloc_count::assert_at_most;
use hir_expand::{
    mod_path::{ModPath, PathKind},
    name::Name,
};
use intern::Symbol;
use rustc_hash::FxHashSet;
use test_fixture::WithFixture;

use crate::{
    AssocItemId, DefWithBodyId, TraitId,
    expr_store::{Body, HygieneId, path::Path, scope::ExprScopes},
    hir::ExprId,
    nameres::{assoc::ImplItems, crate_def_map},
    resolver::{HasResolver, NamesInScope, ResolveValueResult, TypeNs, ValueNs},
    test_db::{TestDB, TestDBExt},
};

const CODE: &str = r#"
//- /main.rs crate:main deps:pre
#[prelude_import]
use ::pre::prelude::*;
pub mod m {
    pub struct S;
    impl S {
        pub fn new() -> S {
            S
        }
    }
    pub enum E {
        A,
    }
}
pub trait Tr {
    fn go<T: Copy>(&self, x: T) -> T;
}
pub struct B;
fn f<U>(x: U) -> U {
    x
}
impl Tr for m::S {
    fn go<T: Copy>(&self, x: T) -> T {
        let y = x;
        {
            trait Local {}
            f(y)$0
        }
    }
}
//- /pre.rs crate:pre
pub mod prelude {
    pub struct Pre;
    pub trait PreTr {}
}
"#;

fn name(text: &str) -> Name {
    Name::new_symbol_root(Symbol::intern(text))
}

fn path(segments: &[&str]) -> ModPath {
    ModPath::from_segments(PathKind::Plain, segments.iter().map(|it| name(it)))
}

/// The database and the impl's `go`.
fn fixture() -> (TestDB, DefWithBodyId) {
    let (db, position) = TestDB::with_position(CODE);
    let krate = db.module_at_position(position).krate(&db);
    let map = crate_def_map(&db, krate);
    let impl_ = map
        .module(&db, map.root)
        .scope(&db)
        .impls()
        .next()
        .expect("the fixture's root has `impl Tr for m::S`");
    let go = ImplItems::of(&db, impl_)
        .0
        .items
        .iter()
        .find_map(|&(_, it)| match it {
            AssocItemId::FunctionId(it) => Some(it),
            _ => None,
        })
        .expect("the impl has `go`");
    (db, go.into())
}

/// The expression of `def` with the most scopes around it: `f(y)` in the block.
fn innermost(db: &TestDB, def: DefWithBodyId) -> ExprId {
    let body = Body::of(db, def);
    let scopes = ExprScopes::of(db, def);
    body.expr_ids()
        .max_by_key(|&it| scopes.scope_chain(scopes.scope_for(it)).count())
        .expect("`go` has expressions")
}

fn the_trait(resolver: &crate::resolver::Resolver<'_>, db: &TestDB, text: &str) -> TraitId {
    match resolver.resolve_path_in_type_ns_fully(db, &Path::BarePath(&path(&[text]))) {
        Some(TypeNs::TraitId(it)) => it,
        other => panic!("`{text}` is not a trait in scope: {other:?}"),
    }
}

/// Budget: at the innermost scope of a warm body, a type-namespace lookup of a generic
/// parameter, a two-segment path, a root item, a prelude item, a builtin, an enum variant, a
/// module's trait and a block's trait, and a value-namespace lookup of a `let`, a parameter, a
/// root function, an associated item left partial, an enum variant and a unit struct: 0
/// allocations.
#[test]
fn type_and_value_lookups_at_a_warm_scope_allocate_nothing() {
    let (db, def) = fixture();
    let at = innermost(&db, def);
    let mut resolver = def.resolver(&db);
    let _guard = resolver.update_to_inner_scope(&db, def, at);

    let type_paths =
        [&["T"][..], &["m", "S"], &["B"], &["Pre"], &["u32"], &["m", "E", "A"], &["Tr"], &["Local"]]
            .map(path);
    let types = type_paths.each_ref().map(Path::BarePath);
    let found: Vec<_> = types.iter().map(|it| resolver.resolve_path_in_type_ns(&db, it)).collect();
    let kinds: Vec<&str> = found
        .iter()
        .map(|it| match it {
            Some((TypeNs::GenericParam(_), None, _)) => "generic param",
            Some((TypeNs::AdtId(_), None, _)) => "adt",
            Some((TypeNs::BuiltinType(_), None, _)) => "builtin",
            Some((TypeNs::EnumVariantId(_), None, _)) => "variant",
            Some((TypeNs::TraitId(_), None, _)) => "trait",
            other => panic!("an unexpected type-namespace result: {other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        ["generic param", "adt", "adt", "adt", "builtin", "variant", "trait", "trait"]
    );

    let value_paths = [&["y"][..], &["x"], &["f"], &["m", "S", "new"], &["m", "E", "A"], &["B"]]
        .map(path);
    let values = value_paths.each_ref().map(Path::BarePath);
    let found: Vec<_> = values
        .iter()
        .map(|it| resolver.resolve_path_in_value_ns(&db, it, HygieneId::ROOT))
        .collect();
    assert!(
        matches!(
            &found[..],
            [
                Some(ResolveValueResult::ValueNs(ValueNs::LocalBinding(_))),
                Some(ResolveValueResult::ValueNs(ValueNs::LocalBinding(_))),
                Some(ResolveValueResult::ValueNs(ValueNs::FunctionId(_))),
                Some(ResolveValueResult::Partial(TypeNs::AdtId(_), _)),
                Some(ResolveValueResult::ValueNs(ValueNs::EnumVariantId(_))),
                Some(ResolveValueResult::ValueNs(ValueNs::StructId(_))),
            ]
        ),
        "unexpected value-namespace results: {found:?}"
    );

    assert_at_most(0, "type and value lookups at the innermost scope", || {
        for it in &types {
            black_box(resolver.resolve_path_in_type_ns(&db, it));
        }
        for it in &values {
            black_box(resolver.resolve_path_in_value_ns(&db, it, HygieneId::ROOT));
        }
    });
}

/// Budget: the traits in scope at the innermost scope (a block's trait, the impl's trait, the
/// prelude's), walked through a visitor, asked for one trait, and added to a kept set once it
/// has grown: 0 allocations.
#[test]
fn the_traits_in_scope_through_a_visitor_allocate_nothing() {
    let (db, def) = fixture();
    let at = innermost(&db, def);
    let mut resolver = def.resolver(&db);
    let _guard = resolver.update_to_inner_scope(&db, def, at);
    let all = resolver.traits_in_scope(&db);
    let [tr, local, pre] = ["Tr", "Local", "PreTr"].map(|it| the_trait(&resolver, &db, it));
    for it in [tr, local, pre] {
        assert!(all.contains(&it), "{it:?} is not among the traits in scope: {all:?}");
    }
    let mut kept = FxHashSet::default();
    resolver.traits_in_scope_into(&db, &mut kept);

    let (visited, has) = assert_at_most(0, "the traits in scope through a visitor", || {
        let mut visited = 0;
        resolver.for_each_trait_in_scope(&db, |_| visited += 1);
        let has = resolver.has_trait_in_scope(&db, pre);
        kept.clear();
        resolver.traits_in_scope_into(&db, &mut kept);
        (visited, has)
    });
    assert!(has);
    assert!(visited >= all.len());
    assert_eq!(kept, all);
}

/// Budget: the names in scope at the innermost scope into a map kept from an earlier call: 0
/// allocations (no name of the fixture has two meanings in one scope).
#[test]
fn the_names_in_scope_into_a_kept_map_allocate_nothing() {
    let (db, def) = fixture();
    let at = innermost(&db, def);
    let mut resolver = def.resolver(&db);
    let _guard = resolver.update_to_inner_scope(&db, def, at);
    let fresh = resolver.names_in_scope(&db);
    for it in ["y", "x", "T", "Local", "f", "m", "Pre", "u32"] {
        assert!(fresh.contains_key(&name(it)), "`{it}` is not among the names in scope");
    }
    let mut kept = NamesInScope::default();
    resolver.names_in_scope_into(&db, &mut kept);
    assert_at_most(0, "the names in scope into a kept map", || {
        resolver.names_in_scope_into(&db, &mut kept);
    });
    assert!(kept.iter().eq(fresh.iter()));
}
```

A unit struct (`B`, `m::S`) is in both namespaces with one def, which `ScopeNames::add` keeps
once, so no list spills.

### T3. Reads of a collected `ItemScope` (new `item_scope/tests/budgets.rs`, `mod budgets;` inside `item_scope.rs`'s `mod tests`; db-expand-names)

```rust
//! Allocation budgets of reading a collected scope: a settled scope has its tables, so every read
//! (names in order, resolutions, traits, the name of a def) allocates nothing, from the first
//! read on.

use std::hint::black_box;

use base_db::relevant_crates;
use blaze_alloc_count::assert_at_most;
use hir_expand::name::Name;
use intern::Symbol;
use test_fixture::WithFixture;

use crate::{
    ModuleDefId,
    item_scope::{ItemInNs, LINEAR},
    nameres::crate_def_map,
    test_db::TestDB,
};

const CODE: &str = r#"
pub mod small {
    pub struct A;
    pub fn b() {}
    pub trait T {}
}
pub mod wide {
    pub struct W0;
    pub struct W1;
    pub struct W2;
    pub struct W3;
    pub struct W4;
    pub struct W5;
    pub struct W6;
    pub struct W7;
    pub struct W8;
    pub struct W9;
    pub trait Tr {}
    pub use super::small::A as Again;
    pub use super::small::T as _;
}
"#;

fn name(text: &str) -> Name {
    Name::new_symbol_root(Symbol::intern(text))
}

/// Budget: `entries`, `resolutions`, `traits` and `name_of` of every type in a scope of at most
/// [`LINEAR`] names and one of more, with no read before the measure: 0 allocations.
#[test]
fn reading_a_settled_scope_allocates_nothing() {
    let (db, file) = TestDB::with_single_file(CODE);
    let krate = relevant_crates(&db, file.file_id(&db))[0];
    let map = crate_def_map(&db, krate);
    let root = map.module(&db, map.root).scope(&db);
    let module = |text: &str| match root.get(&name(text)).take_types() {
        Some(ModuleDefId::ModuleId(it)) => map.module(&db, it).scope(&db),
        other => panic!("`{text}` is not a module of the root: {other:?}"),
    };
    let scopes = [module("small"), module("wide")];
    let items: Vec<ItemInNs> = scopes
        .iter()
        .flat_map(|scope| scope.types().map(|(_, it)| ItemInNs::Types(it.def)))
        .collect();

    let counts = assert_at_most(0, "reading a settled scope", || {
        scopes.map(|scope| {
            for &item in &items {
                black_box(scope.name_of(item));
            }
            (scope.entries().count(), scope.resolutions().count(), scope.traits().count())
        })
    });
    assert_eq!(counts[0], (3, 3, 1), "`small`: three names, one trait");
    assert!(counts[1].0 > LINEAR, "`wide` does not reach the index: {:?}", counts[1]);
    assert_eq!(counts[1].1, counts[1].0 + 1, "`wide`'s resolutions: its names and `T as _`");
    assert_eq!(counts[1].2, 2, "`wide`'s traits: `Tr` and `T as _`");
}
```

`LINEAR` (149) is private to `item_scope`, which a descendant module can read as is.

### T4. `find_path` once its scratch has grown (`find_path.rs` `mod tests`, db-expand)

```rust
    /// Budget: `find_path` for an item reached through the local import search (a re-export two
    /// modules away) and for one in another crate, each a second time on the same thread once the
    /// thread's scratch has grown and the path is interned: 0 allocations.
    #[test]
    fn a_warm_find_path_allocates_nothing() {
        let (db, pos) = TestDB::with_position(
            r#"
//- /main.rs crate:main deps:dep
mod a {
    pub mod b {
        pub struct S;
    }
}
mod c {
    pub use crate::a::b::S as Again;
}
mod d {
    $0
}
//- /dep.rs crate:dep
pub mod x {
    pub struct Y;
}
"#,
        );
        let module = db.module_at_position(pos);
        let (def_map, local_def_map) = module.local_def_map(&db);
        let item = |kind: PathKind, segments: &[&str]| {
            let path = ModPath::from_segments(
                kind,
                segments.iter().map(|it| Name::new_symbol_root(intern::Symbol::intern(it))),
            );
            let res = def_map
                .resolve_path(
                    local_def_map,
                    &db,
                    module,
                    &path,
                    crate::item_scope::BuiltinShadowMode::Module,
                    None,
                )
                .0;
            ItemInNs::Types(res.take_types().expect("the fixture's item resolves"))
        };
        let items = [item(PathKind::Crate, &["c", "Again"]), item(PathKind::Plain, &["dep", "x", "Y"])];
        let cfg = FindPathConfig {
            prefer_no_std: false,
            prefer_prelude: false,
            prefer_absolute: false,
            allow_unstable: false,
        };
        let find = || {
            items.map(|it| find_path(&db, it, module, PrefixKind::Plain, false, cfg))
        };
        let warm = find();
        assert!(warm.iter().all(Option::is_some), "a fixture item has no path: {warm:?}");
        let found = assert_at_most(0, "a warm `find_path`", find);
        assert_eq!(found, warm);
    }
```

The test module gains `use blaze_alloc_count::assert_at_most;` and the `Name`/`PathKind` imports it
needs (`super::*` brings `ModPath`, `ItemInNs`, `FindPathConfig`, `PrefixKind`).

## R10. The def map being collected lives in the collector's frame, read through `DefMapRead` (db-expand-names: `nameres.rs`, `path_resolution.rs`, `item_scope.rs`; `visibility.rs` to db-expand)

Asked by mem-db-def, about mem-db-def-collect's residual. During a cold collection the building
`DefMap`'s `ModulesMap`, every `ItemScope` column, the children maps and `macro_use_prelude` grow
on the global heap. They do so because path resolution reads them mid-collection through
`&DefMap`, and the same values become the memo. This part gives path resolution a view that
reads either a collected def map or one in the frame. The building state then moves into the
collector's frame (`&'f Frame`, collect's B tables), and the memo is made from it once, at exact
size. A cold `crate_local_def_map` then makes a fixed number of global allocations for a given
set of non-empty tables, however many names they hold. T5 holds that.

R10 replaces R2's in-place `settle` with a settle that consumes the building scope (R10.2). R1's
table builders (`sorted_table`, `by_def_table`, `by_name`, `sorted_small`) are reused as they are.

> forwarded to db (db-expand-names-next2; R10.4 `visibility.rs` to db-expand-next2) (requests, pass 19; told by tmux).
> outcome (requests pass 19, R10.1-R10.3, R10.5): deferred, not applied; reasons below.
> (21:20) mem-db-def-collect's Part H (H6 sends facts to frontend-nameres) answers reason (1) for
> the building side; reason (2) stands, and R10 and Part H land together once the base has built.
> deferred by db-expand-names (2026-10-29 21:05, Andrew's decision): not started. Two reasons.
> (1) The premise is partly stale. Since frontend-nameres-core's switch-over (`v2/requests/db.md`,
> applied 21:00), a cold `crate_local_def_map` runs through `nameres/facts/` (`facts::collect_crate`
> in `facts/driver.rs`, frontend-nameres's), and `collector.rs` is `#[cfg(test)]`, kept only as the
> differential reference. R10.3's `BuildingDefMap<'f>` and its 33 collector call sites would land
> on the test-only path. The cold path T5 measures is facts'. Please retarget the building side:
> say which facts queries hold the building scopes and def map (`unit.rs`, `macros.rs`,
> `driver.rs`), and send that part to frontend-nameres. R10.1 (`DefMapRead`/`ResolvePath`) and
> R10.2 (`ItemScope<S>`) still stand as the shared base.
> (2) hir-def does not build yet: H1-H3, R1-R5, T1-T3 and the facts switch-over are all unbuilt.
> R10 rewrites `item_scope.rs` and `path_resolution.rs` over them. It waits until that tree has
> built and run once in an ix VM.
> R10.4 (and R10.9) not applied by db-expand (db-expand-paths, 2026-10-29): `DefMapRead` (R10.1) is not in the tree (deferred above), so `&impl DefMapRead` has no trait to name. `is_visible_from_def_map`, `max` and `min` keep `&DefMap`; they move with R10.1 when it lands.

### R10.1 The view (`nameres/path_resolution.rs`)

```rust
/// What path resolution, `resolve_visibility` and the textual macro scope read of the def map a
/// path is resolved in: a collected [`DefMap`] through its memos, or the collector's
/// [`BuildingDefMap`] in its frame. The two answer alike, and path resolution is written once
/// against this and compiled for each.
pub(crate) trait DefMapRead {
    type Store: ScopeStore;

    fn krate(&self) -> Crate;
    fn block(&self) -> Option<BlockInfo>;
    fn root(&self) -> ModuleId;
    fn prelude(&self) -> Option<(ModuleId, Option<UseId>)>;
    fn edition(&self) -> Edition;
    /// The scope of `module`, a module of this def map.
    fn scope<'a>(&'a self, db: &'a Db, module: ModuleId) -> &'a ItemScope<Self::Store>;
    /// What `name` means in `module`, a module of this def map. In a collected map this is a
    /// memo hit of the module's per-name fact, so the reader depends on that name alone.
    fn name_in_module(&self, db: &Db, module: ModuleId, name: &Name) -> PerNs;
    fn legacy_macros<'a>(&'a self, db: &'a Db, module: ModuleId, name: &Name)
    -> Option<&'a [MacroId]>;
    fn parent(&self, db: &Db, module: ModuleId) -> Option<ModuleId>;
    fn child(&self, db: &Db, module: ModuleId, name: &Name) -> Option<ModuleId>;
    fn macro_in_use_prelude(&self, name: &Name) -> Option<(MacroId, Option<ExternCrateId>)>;

    fn block_id(&self) -> Option<BlockId> {
        self.block().map(|it| it.block)
    }

    fn crate_root(&self, db: &Db) -> ModuleId {
        match self.block() {
            Some(_) => crate_def_map(db, self.krate()).root,
            None => self.root(),
        }
    }
}

/// Where a lookup's extern prelude is read: the crate's collected [`LocalDefMap`], or the
/// collector's list while the crate is collected.
pub(crate) trait ExternPreludeRead {
    fn extern_crate(&self, name: &Name) -> Option<(ModuleId, Option<ExternCrateId>)>;
}

impl ExternPreludeRead for LocalDefMap {
    fn extern_crate(&self, name: &Name) -> Option<(ModuleId, Option<ExternCrateId>)> {
        self.extern_prelude.get(name).copied()
    }
}
```

`impl DefMap { .. }` in `path_resolution.rs` (every function from `resolve_visibility`, 148, to
`resolve_in_prelude_reading`, 862) becomes the provided methods of one extension trait:

```rust
pub(crate) trait ResolvePath: DefMapRead {
    fn resolve_visibility(&self, local_def_map: &impl ExternPreludeRead, db: &Db, ..) -> Option<Visibility> { .. }
    fn resolve_path(&self, local_def_map: &impl ExternPreludeRead, db: &Db, ..) -> (PerNs, Option<usize>) { .. }
    fn resolve_path_fp_with_macro(&self, local_def_map: &impl ExternPreludeRead, db: &Db, ..) -> ResolvePathResult { .. }
    fn resolve_path_fp_recording(..) -> ResolvePathResult { .. }
    fn resolve_path_fp_with_macro_single(..) -> ResolvePathResult { .. }
    // .. and the private steps, unchanged in their arguments ..
}

impl<M: DefMapRead> ResolvePath for M {}
```

Every other parameter and the return types stay as they are, and `LocalDefMap` implements
`ExternPreludeRead`. So each caller keeps its call and adds
`use crate::nameres::path_resolution::ResolvePath;`. `PathQuery<'a>` becomes
`PathQuery<'a, L: ExternPreludeRead>`. Inside the bodies:

| Today | Becomes |
|---|---|
| `self.data.edition` (353, 460) | `self.edition()` |
| `self.block`, `self.root`, `self.krate`, `self.prelude` | `self.block()`, `self.root()`, `self.krate()`, `self.prelude()` |
| `self.module(db, m).name_in_scope(db, n)` (730, 846) | `self.name_in_module(db, m, n)` |
| `if m.block(db) == self.block_id() { self.module(db, m) } else { m.def_map(db).module(db, m) }` then `.name_in_scope(db, n)` (617 to 625) | `match m.block(db) == self.block_id() { true => self.name_in_module(db, m, n), false => m.def_map(db).name_in_module(db, m, n) }` |
| `is_test_harness` (511 to 517): `def_map.module(db, module).parent` and `.children(db).get(&name)` | the same two-way match on `self.parent`/`self.child` against `module.def_map(db)`'s |
| `declared_enum_visibility` (537 to 558): `module_data.name_in_scope` and `module_data.scope(db).types()` | the two-way match, the scan in a generic `fn declared_in<S: ScopeStore>(scope: &ItemScope<S>, def: ModuleDefId) -> Option<Visibility>` |
| `if prelude.krate(db) == self.krate { self } else { prelude.def_map(db) }` (869) | the two-way match |
| `self.macro_use_prelude.get(name)` (820) | `self.macro_in_use_prelude(name)` |
| `local_def_map.extern_prelude.get(name)` (809) | `local_def_map.extern_crate(name)` |
| `vis.is_visible_from_def_map(db, self, m)` (602, 681) | unchanged; R10.4 makes it take `&impl DefMapRead` |

`legacy_macro_in_textual_scope` and `legacy_macro_before` (`nameres.rs` 1013 to 1052) move into
`ResolvePath`. Their first read becomes `self.legacy_macros(db, module, name)`. The enclosing
blocks' reads stay on `parent.def_map(db)`, which is always collected.

`impl DefMapRead for DefMap` (`nameres.rs`):

```rust
impl DefMapRead for DefMap {
    type Store = Settled;

    fn krate(&self) -> Crate {
        self.krate
    }
    fn block(&self) -> Option<BlockInfo> {
        self.block
    }
    fn root(&self) -> ModuleId {
        self.root
    }
    fn prelude(&self) -> Option<(ModuleId, Option<UseId>)> {
        self.prelude
    }
    fn edition(&self) -> Edition {
        self.data.edition
    }
    fn scope<'a>(&'a self, db: &'a Db, module: ModuleId) -> &'a ItemScope {
        self.module(db, module).scope(db)
    }
    fn name_in_module(&self, db: &Db, module: ModuleId, name: &Name) -> PerNs {
        self.module(db, module).name_in_scope(db, name)
    }
    fn legacy_macros<'a>(&'a self, db: &'a Db, module: ModuleId, name: &Name) -> Option<&'a [MacroId]> {
        self.module(db, module).legacy_macro_in_scope(db, name)
    }
    fn parent(&self, db: &Db, module: ModuleId) -> Option<ModuleId> {
        self.module(db, module).parent
    }
    fn child(&self, db: &Db, module: ModuleId, name: &Name) -> Option<ModuleId> {
        self.module(db, module).children(db).get(name).copied()
    }
    fn macro_in_use_prelude(&self, name: &Name) -> Option<(MacroId, Option<ExternCrateId>)> {
        self.macro_use_prelude.get(name).copied()
    }
}
```

speed-hir-sol-nameres' `Modules::name` memo and `def_map.name_in_module(db, module, name)`
(`v2/requests/db-expand.md` §3) are this impl's `name_in_module`: land it there, and nothing else
changes. Its `Resolver::scopes` SmallVec (§2) is untouched by R10.

### R10.2 `ItemScope<S: ScopeStore = Settled>` (`item_scope.rs`)

The scope's lists move behind a store. A settled scope keeps `ThinVec`s, which are exact and 8
bytes each, as today. A building one keeps frame `Vec`s, which grow in place in the frame and
never reach the global heap.

```rust
/// Where a scope's lists live: [`Settled`] for the scope a memo keeps (every list exact, the
/// tables made), [`Building`] for one being collected (every list in the collector's frame, the
/// tables made on the first read after a change).
pub(crate) trait ScopeStore: Sized {
    type Vec<T>: Deref<Target = [T]>;
    /// A list held in a map's value: the macros of a name, the derive calls of an item.
    type List<T>: Deref<Target = [T]>;
    type Tables;

    fn sorted(scope: &ItemScope<Self>) -> &[u32];
    fn by_def(scope: &ItemScope<Self>) -> &[u32];
}

pub(crate) enum Settled {}

/// The tables of a settled scope: empty for one of at most [`LINEAR`] names.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct SettledTables {
    sorted: Box<[u32]>,
    by_def: Box<[u32]>,
}

impl ScopeStore for Settled {
    type Vec<T> = ThinVec<T>;
    type List<T> = SmallVec<[T; 1]>;
    type Tables = SettledTables;

    fn sorted(scope: &ItemScope) -> &[u32] {
        &scope.tables.sorted
    }
    fn by_def(scope: &ItemScope) -> &[u32] {
        &scope.tables.by_def
    }
}

pub(crate) struct Building<'f>(PhantomData<&'f Frame>);

impl<'f> ScopeStore for Building<'f> {
    type Vec<T> = Vec<T, &'f Frame>;
    type List<T> = Vec<T, &'f Frame>;
    type Tables = BuildingTables<'f>;

    fn sorted(scope: &ItemScope<Self>) -> &[u32] {
        scope.tables.sorted.get(scope.tables.frame, |out| scope.fill_sorted(out))
    }
    fn by_def(scope: &ItemScope<Self>) -> &[u32] {
        scope.tables.by_def.get(scope.tables.frame, |out| scope.fill_by_def(out))
    }
}

/// The tables of a scope being collected.
pub(crate) struct BuildingTables<'f> {
    frame: &'f Frame,
    sorted: Remade<'f>,
    by_def: Remade<'f>,
}

/// A table made on the first read after a change, into the buffer the table before it was in:
/// one buffer per table in the frame, however often the scope changes.
struct Remade<'f> {
    table: OnceCell<Vec<u32, &'f Frame>>,
    spare: Cell<Option<Vec<u32, &'f Frame>>>,
}

impl<'f> Remade<'f> {
    fn get(&self, frame: &'f Frame, fill: impl FnOnce(&mut Vec<u32, &'f Frame>)) -> &[u32] {
        self.table.get_or_init(|| {
            let mut out = self.spare.take().unwrap_or_else(|| Vec::new_in(frame));
            fill(&mut out);
            out
        })
    }

    /// After a change: the table is gone, its buffer kept for the next one.
    fn take(&mut self) {
        if let Some(mut old) = self.table.take() {
            old.clear();
            self.spare.set(Some(old));
        }
    }
}
```

The struct:

```rust
pub struct ItemScope<S: ScopeStore = Settled> {
    pub(crate) types: Column<ModuleDefId, ImportOrExternCrate, S>,
    pub(crate) values: Column<ModuleDefId, ImportOrGlob, S>,
    pub(crate) macros: Column<MacroId, ImportOrExternCrate, S>,
    pub(crate) unresolved: S::Vec<Name>,
    pub(crate) names: Index<S>,
    pub(crate) tables: S::Tables,
    // every other `ThinVec<T>` field becomes `S::Vec<T>`, every `Keyed<K, V>` becomes
    // `Keyed<K, V, S>`, `trait_impl`/`reservation_impl` become `Bits<S>`, and:
    pub(crate) legacy_macros: Keyed<Name, S::List<MacroId>, S>,
    pub(crate) derive_macros: Keyed<AstId<ast::Adt>, S::List<DeriveMacroInvocation<S>>, S>,
}

pub(crate) struct DeriveMacroInvocation<S: ScopeStore = Settled> {
    pub(crate) attr_id: AttrId,
    pub(crate) attr_call_id: MacroCallId,
    pub(crate) derive_call_ids: S::List<Option<Either<MacroCallId, BuiltinDeriveImplId>>>,
}
```

`Column<D, I, S>` holds `entries: S::Vec<Entry>`; `Keyed<K, V, S>` holds `entries: S::Vec<(K, V)>`
and `index: Index<S>`; `Index<S>` holds `slots: S::Vec<u32>`; `Bits<S>` holds `S::Vec<u64>`. The
`sorted`/`by_def` `OnceLock`s are gone. `sorted()` and `by_def()` call `S::sorted(self)` and
`S::by_def(self)`. R1's `sorted_table`/`by_def_table` become `fill_sorted(&self, out)` and
`fill_by_def(&self, out)`, which sort in place in `out`: `by_name` on `out`, then
`out.truncate(kept)`, and `radix::sort_copy_by_key_in(out, key, spare)` with `spare` from
`scratch::with_u32s`.

The methods split three ways:

- `impl<S: ScopeStore> ItemScope<S>` holds every read: `get`, `entries` (R1's small path
  unchanged), `types`, `values`, `macros`, `name_of`, `names_of`, `traits`, `resolutions`,
  `get_legacy_macro`, `legacy_macros`, `declarations`, `impls`, `modules_in_scope`,
  `fully_resolve_import`, `dump` and the rest. They read slices only, so one body serves both
  stores.
- `impl<'f> ItemScope<Building<'f>>` holds `new_in(frame: &'f Frame)` (every list
  `Vec::new_in(frame)`), every write (`declare`, `push_res_with_import`, `define_legacy_macro`,
  `add_macro_invoc`, `add_attr_macro_invoc`, `add_derive_macro_invoc`, `define_impl`,
  `update_visibility_*`, `settle_glob_ambiguities`, `added`, whose two `take`s become
  `self.tables.sorted.take(); self.tables.by_def.take();`), and `settle(self) -> ItemScope`.
- `impl ItemScope` (that is, `Settled`) holds the frozen read's `reserve`/`push_unindexed`
  (its lists are exact from the start), `seal`, and `to_building<'f>(&self, frame) ->
  ItemScope<Building<'f>>` for a facts query that extends a scope another query made.

`settle` makes each list once, at its length, from the frame:

```rust
/// A settled list from a frame one: one allocation of exactly its length, none when empty.
fn settled<T>(v: Vec<T, &Frame>) -> ThinVec<T> {
    // alloc: the memo's list, at its length
    let mut out = ThinVec::with_capacity(v.len());
    out.extend(v);
    out
}

/// A settled map value from a frame list: inline when it has one element.
fn settled_list<T>(v: Vec<T, &Frame>) -> SmallVec<[T; 1]> {
    // alloc: none for one element, else the list at its length
    let mut out = SmallVec::with_capacity(v.len());
    out.extend(v);
    out
}

impl<'f> ItemScope<Building<'f>> {
    /// The scope the memo keeps, made once collection is done: every list made at its length
    /// from the frame, the index as small as its load allows, and the tables of a scope of more
    /// than [`LINEAR`] names, so that no read of it allocates.
    pub(crate) fn settle(mut self) -> ItemScope {
        self.settle_glob_ambiguities();
        let Self { types, values, macros, unresolved, names: _, tables: _, declarations, /* every field */ .. } = self;
        let mut scope = ItemScope {
            types: types.map(settled),
            values: values.map(settled),
            macros: macros.map(settled),
            unresolved: settled(unresolved),
            declarations: settled(declarations),
            // .. `settled` for every list, `Keyed::settle_with(settled)` for every map, and for
            // the two maps with list values:
            legacy_macros: legacy_macros.settle_with(settled_list),
            // alloc: none for one invocation, else the list at its length (exact size hint)
            derive_macros: derive_macros.settle_with(|calls| {
                calls.into_iter().map(DeriveMacroInvocation::settle).collect::<SmallVec<[_; 1]>>()
            }),
            names: Index::default(),
            tables: SettledTables::default(),
        };
        let unique = scope.reindex(scope.indexed_len());
        debug_assert!(unique, "a namespace of a scope with a name twice");
        scope.fill_tables();
        scope
    }
}
```

The destructure lists every field, as `shrink_to_fit` does today, so a new field cannot be
missed. `Column::map(self, f)` rebuilds the entries with `f`. `Keyed::settle_with(self, f)`
settles the entries, maps each value with `f`, and indexes at the exact length.
`DeriveMacroInvocation::settle` makes `derive_call_ids` with `settled_list`. A derive list of one
element stays inline: `SmallVec<[_; 1]>` here, where today it holds up to 4 inline, so a scope
with longer derive lists makes one exact allocation per list.
`fill_tables` (R1) writes `scope.tables = SettledTables { sorted, by_def }` instead of the
`OnceLock`s. `PartialEq` and `Clone` are implemented for `ItemScope` (`Settled`) only, since memo
backdating compares settled scopes. `Default` goes, and a building scope is made with `new_in`.

`PerNsGlobImports<A>` (R3) is unchanged, and `push_res_with_import` is on
`ItemScope<Building<'f>>`.

### R10.3 `BuildingDefMap<'f>` (`nameres.rs`)

```rust
/// A def map while it is collected, in the collector's frame: what the collector writes and
/// path resolution reads mid-collection (through [`DefMapRead`]). [`Self::settle`] makes the
/// owned [`DefMap`] from it, each table once at its exact size.
pub(crate) struct BuildingDefMap<'f> {
    frame: &'f Frame,
    krate: Crate,
    block: Option<BlockInfo>,
    pub(crate) root: ModuleId,
    pub(crate) prelude: Option<(ModuleId, Option<UseId>)>,
    pub(crate) data: Arc<DefMapCrateData>,
    /// The modules in the order they were added, found by id through `module_at`.
    modules: Vec<(ModuleId, BuildingModule<'f>), &'f Frame>,
    module_at: hashbrown::HashMap<ModuleId, u32, FxBuildHasher, &'f Frame>,
    /// Every module's children by name, for `child`; each module lists its own in order.
    child_at: hashbrown::HashMap<(ModuleId, Name), ModuleId, FxBuildHasher, &'f Frame>,
    pub(crate) macro_use_prelude:
        hashbrown::HashMap<Name, (MacroId, Option<ExternCrateId>), FxBuildHasher, &'f Frame>,
    pub(crate) derive_helpers_in_scope: hashbrown::HashMap<
        AstId<ast::Item>,
        Vec<(Name, MacroId, Either<MacroCallId, BuiltinDeriveImplId>), &'f Frame>,
        FxBuildHasher,
        &'f Frame,
    >,
    pub(crate) diagnostics: Vec<DefDiagnostic, &'f Frame>,
    pub(crate) names: uses::NameRecords,
    pub(crate) session_macros: bool,
}

pub(crate) struct BuildingModule<'f> {
    pub(crate) origin: ModuleOrigin,
    pub(crate) visibility: Visibility,
    pub(crate) parent: Option<ModuleId>,
    pub(crate) scope: ItemScope<Building<'f>>,
    /// The modules declared in this one, in declaration order.
    children: Vec<(Name, ModuleId), &'f Frame>,
}
```

Its methods:

- `new_in(frame, db, krate, data, root_module_data, block)` replaces `DefMap::empty` (632).
- `add_module(&mut self, id, origin, visibility, parent) -> &mut BuildingModule<'f>`.
- `add_child(&mut self, parent, name, child)` pushes to the parent's `children` and inserts
  into `child_at`.
- `module(&self, id) -> &BuildingModule<'f>` and `module_mut(&mut self, id)` go through
  `module_at`.
- `modules(&self)` iterates in order.

`DefMapRead for BuildingDefMap<'f>` has `type Store = Building<'f>`. Its `scope`,
`name_in_module` and `legacy_macros` read `self.module(module).scope`. Its `parent` reads
`self.module(module).parent`, `child` reads `self.child_at.get(&(module, *name)).copied()`, and
`macro_in_use_prelude` reads `self.macro_use_prelude.get(name).copied()`.

`settle` replaces `DefMap::shrink_to_fit` (collect's A4 `settle`) and `hand_over_scopes`:

```rust
impl<'f> BuildingDefMap<'f> {
    /// The owned def map: every module's scope and children settled and handed to the database,
    /// every map made once at its length.
    pub(crate) fn settle(self, db: &Db) -> DefMap {
        let modules = self.modules.into_iter().map(|(id, module)| {
            // alloc: the memo's children map, at its length
            let mut children =
                FxIndexMap::with_capacity_and_hasher(module.children.len(), FxBuildHasher);
            children.extend(module.children);
            (id, module.origin, module.visibility, module.parent, module.scope.settle(), children)
        });
        DefMap::from_settled(
            db,
            DefMapParts {
                krate: self.krate,
                block: self.block,
                root: self.root,
                prelude: self.prelude,
                data: self.data,
                macro_use_prelude: exact_map(self.macro_use_prelude),
                derive_helpers_in_scope: exact_map_with(self.derive_helpers_in_scope, settled_vec),
                diagnostics: settled_vec(self.diagnostics),
                names: self.names,
                session_macros: self.session_macros,
            },
            modules,
        )
    }
}

impl DefMap {
    /// A def map from settled modules: each scope becomes its `ModuleScope`, the table its
    /// `Modules`. Used by [`BuildingDefMap::settle`] and by the frozen read, whose scopes are
    /// exact from the start.
    pub(crate) fn from_settled(
        db: &Db,
        parts: DefMapParts,
        modules: impl ExactSizeIterator<
            Item = (ModuleId, ModuleOrigin, Visibility, Option<ModuleId>, ItemScope, FxIndexMap<Name, ModuleId>),
        >,
    ) -> DefMap {
        let mut table = ModulesMap::with_capacity(modules.len());
        for (id, origin, visibility, parent, scope, children) in modules {
            let scope = ModuleScope::new(db, id, scope, children);
            table.insert(id, ModuleData { origin, visibility, parent, scope: scope.as_id() });
        }
        let DefMapParts { krate, block, root, prelude, data, macro_use_prelude, derive_helpers_in_scope, diagnostics, names, session_macros } = parts;
        DefMap {
            diagnostics_by_module: DiagnosticsByModule::of(&diagnostics),
            modules: Modules::new(db, root, table).as_id(),
            krate, block, root, prelude, data, macro_use_prelude, derive_helpers_in_scope,
            diagnostics, names, session_macros,
        }
    }
}
```

`exact_map` and `settled_vec` are collect's (`with_capacity_and_hasher(len)` plus `extend`, and
`Vec::with_capacity(len)` plus `extend`). `ModulesMap::with_capacity` is
`FxIndexMap::with_capacity_and_hasher(n, FxBuildHasher)`. `DefMapParts` is a plain struct of
`DefMap`'s fields other than the modules, so that `from_settled` names each one.

What goes from `DefMap`:

- `ModulesSlot` and `ScopeSlot` go, with `BuildingScope` and `ModulesSlot::building`.
  `DefMap::modules` becomes `salsa::Id` (the `Modules`) and `ModuleData::scope` becomes
  `salsa::Id` (the `ModuleScope`).
- Every `ModuleData` read (`scope`, `name_in_scope`, `legacy_macro_in_scope`, `traits_in_scope`,
  `children`) loses its `Building` arm. `traits_in_scope` becomes the memo's
  `traits(db).iter().copied()`.
- `scope_mut`, `children_mut`, `ModuleData::new` and `with_scope` go: the collector writes
  `BuildingModule`.
- `DefMap::module` and `table` lose the `Building` arm.

`ModuleData` stays at most 16 bytes of scope, and the `const _: () = assert!` (536) moves to
`size_of::<ModuleData>()`.

The collector (`collector.rs`) holds `def_map: BuildingDefMap<'f>` in place of `DefMap`. The 33
`modules[id].scope_mut()` become `def_map.module_mut(id).scope`, `children_mut().insert(..)`
becomes `def_map.add_child(..)`, and `collect_defs` returns
`(collector.def_map.settle(db), local_def_map)`. `crate_local_def_map` (583) and
`block_def_map` (617) no longer call `hand_over_scopes`. The collector's in-progress
`LocalDefMap` is collect's A5 frame list, read through
`impl ExternPreludeRead for BuildingLocal<'f>`: a
`Vec<(Name, (ModuleId, Option<ExternCrateId>)), &'f Frame>` with a `hashbrown` index in the
frame, built into the owned `LocalDefMap` in `finish`. `frozen::served` builds settled scopes
(`push_unindexed`, then `seal`) and calls `DefMap::from_settled`. The facts engine
(`facts/driver.rs` 445, which assembles from final rows) builds its scopes as
`ItemScope<Building<'f>>` in the frame of the query that returns them, settles each, and calls
`from_settled`. A facts query that carries a scope across rounds as a memo keeps it `Settled`
and extends a `to_building` copy. That last part goes to frontend-nameres (`nameres/facts`).

> frontend-nameres (2026-10-29 21:24): the facts part of R10.3 is accepted, and it lands with R10, not before. None of its types are in the tree (no `ItemScope<S>`, `Building<'f>`, `to_building` or `DefMap::from_settled`), and R10 is deferred until hir-def has built (R10's note, 21:05). It lands with collect's H6, in the same change as R10.2/R10.6. Where it goes in `nameres/facts/`: a unit query (`unit.rs`: `local_scope`, `scc_state`) and a round's macro step (`macros.rs`) build each member scope as `ItemScope<Building<'f>>` in their own query frame and settle it into the `ModuleScope` they return. A scope carried from one round to the next (an inactive module's fact, or the previous round's row) stays `Settled` in its memo, and a query that extends it works on a `to_building` copy. Assembly (`driver.rs`) holds no building scope: per R10.10 (H6), `collect_crate` builds the map from the final round's `ModuleScope` handles through `DefMap::from_table`. `collect_block` takes and settles a `BuildingDefMap<'f>`.

### R10.4 `Visibility::is_visible_from_def_map` (`visibility.rs` 82, db-expand)

```rust
    pub(crate) fn is_visible_from_def_map<'db>(
        self,
        db: &'db Db,
        def_map: &impl DefMapRead,
        from_module: ModuleIdLt<'db>,
    ) -> bool {
        if cfg!(debug_assertions) {
            _ = def_map.parent(db, from_module);
        }
        // .. the body unchanged: it reads `def_map.krate()` and `def_map.block_id()` only ..
    }
```

The callers (`resolver.rs`, `collector.rs`, `facts/unit.rs`, `path_resolution.rs`,
`tests/paths.rs`) pass a `&DefMap` or a `&BuildingDefMap` and don't change.

> not applied by db-expand (22:34): waits on R10.1. `trait DefMapRead` is not in the tree, so
> `&impl DefMapRead` names nothing (`rg 'trait DefMapRead' crates/hir-def` finds nothing), and
> R10.1 is deferred by Andrew's decision. Until then `is_visible_from_def_map` keeps
> `def_map: &'db DefMap` (`visibility.rs:81`). The change is one line once R10.1 lands; tell
> db-expand when it does.

### R10.5 Why this shape

- **A trait rather than an enum view.** The collected side is the hot one (inference, the IDE,
  diagnostics), and a generic keeps its code exactly as it is now, with no branch per read. The
  building side gets its own copy. Path resolution is about 900 lines compiled twice, which
  costs less than a branch in every segment of every lookup.
- **A store parameter on `ItemScope` rather than a second scope type.** Reads are about 60
  functions over slices, and one body serves both stores, so a read can't behave differently
  while building and after.
- **Building state in the frame, settled once.** Nothing read mid-collection reaches the
  global heap. The memo's tables are each made once at their length, which is the memo contract
  (`docs/v2/plan/lanes/mem/db.md`, point 3). Frame growth is in place in reserved memory, so it
  makes no global allocation (blaze-mem `frame.rs`).
- **Nothing escapes the frame.** `settle` copies every list out, the `ModuleScope` and
  `Modules` take owned values, and `frame::task` resets the frame after the query returns. A
  `BuildingDefMap<'f>` can't outlive `'f`, so the borrow checker enforces this.

### R10.6 The ruling's names, and the exact types (2026-10-29, after mem-db-def's ruling)

mem-db-def's ruling (`docs/v2/plan/lanes/mem/db-def.md`, "Ruling: the scopes a collection
builds live in its frame") takes the full fix: the building map in the frame, not a
reservation. Its tmux text names the parameter `A: Allocator + Copy = Global` on `ItemScope`,
`ModuleData`, `ModulesMap` and `DefMap`, with the `ThinVec`s becoming `Vec<T, A>`. The ruling's
text keeps R10's `ItemScope<S: ScopeStore = Settled>`. mem-db-def-collect has written its Part H
(H1 to H9, `v2/requests/mem-db-def-collect.md`) against R10's names exactly. So the names stay
R10's. How each name in the tmux text maps, and why:

| Asked | Is, in R10 | Why |
|---|---|---|
| `ItemScope<A: Allocator + Copy = Global>`, `ThinVec` becoming `Vec<T, A>` | `ItemScope<S: ScopeStore = Settled>`: `S::Vec<T>` is `Vec<T, &'f Frame>` for `Building<'f>` and `ThinVec<T>` for `Settled` | `ThinVec` takes no allocator, and a settled `Vec<T, Global>` is 24 bytes where a `ThinVec` is 8. The scope has 22 lists and 8 maps, so that is about 350 bytes more per module in every memo, about 70 MB for 200 000 modules. A settled scope also needs different tables (exact boxes, not remade cells), which an allocator parameter cannot select. `ScopeStore` is the allocator plus those three choices. |
| `ModuleData<A>`, `ModulesMap<A>` | `BuildingModule<'f>` (inline scope and children in the frame), and `BuildingDefMap::modules`/`module_at`; the collected `ModuleData` holds a `ModuleScope` id | A collected module's scope is a salsa `ModuleScope`, not an inline scope. One type for both would keep today's `ScopeSlot` branch in every read of every collected module. |
| `DefMap<A = Global>` | `BuildingDefMap<'f>` while collecting; `DefMap` once settled | the same: the building map holds scopes and the settled one holds ids |
| path resolution as `impl<A: Allocator + Copy> DefMap<A>` | `trait ResolvePath: DefMapRead`, `impl<M: DefMapRead> ResolvePath for M` | One body compiled for each, as asked. It serves the building map and the settled maps it walks into (dependencies, enclosing blocks, the prelude's crate). |
| `per_ns` | unchanged | `PerNs` and `Item` own nothing |

The exact types, before and after (`item_scope.rs`):

```rust
// before (158, 542, 607, 744)
pub(crate) struct Index { slots: ThinVec<u32> }
pub(crate) struct Column<D, I> { entries: ThinVec<Entry>, of: PhantomData<fn() -> Item<D, I>> }
pub(crate) struct Keyed<K, V> { entries: ThinVec<(K, V)>, index: Index }
pub(crate) struct Bits(ThinVec<u64>);

// after
pub(crate) struct Index<S: ScopeStore = Settled> { slots: S::Vec<u32> }
pub(crate) struct Column<D, I, S: ScopeStore = Settled> {
    entries: S::Vec<Entry>,
    of: PhantomData<fn() -> Item<D, I>>,
}
pub(crate) struct Keyed<K, V, S: ScopeStore = Settled> { entries: S::Vec<(K, V)>, index: Index<S> }
pub(crate) struct Bits<S: ScopeStore = Settled>(S::Vec<u64>);
```

`ItemScope` (773 to 842), every field. Before:

```rust
pub struct ItemScope {
    pub(crate) types: Column<ModuleDefId, ImportOrExternCrate>,
    pub(crate) values: Column<ModuleDefId, ImportOrGlob>,
    pub(crate) macros: Column<MacroId, ImportOrExternCrate>,
    pub(crate) unresolved: ThinVec<Name>,
    pub(crate) names: Index,
    pub(crate) sorted: OnceLock<Box<[u32]>>,
    pub(crate) by_def: OnceLock<Box<[u32]>>,
    pub(crate) declarations: ThinVec<ModuleDefId>,
    pub(crate) impls: ThinVec<ImplId>,
    pub(crate) trait_impl: Bits,
    pub(crate) reservation_impl: Bits,
    pub(crate) builtin_derive_impls: ThinVec<BuiltinDeriveImplId>,
    pub(crate) extern_blocks: ThinVec<ExternBlockId>,
    pub(crate) unnamed_consts: ThinVec<ConstId>,
    pub(crate) unnamed_trait_imports: ThinVec<(TraitId, Item<(), ImportOrExternCrate>)>,
    pub(crate) underscore_imports: ThinVec<UnderscoreImport>,
    pub(crate) glob_ambiguities: ThinVec<GlobAmbiguity>,
    pub(crate) shadowed_imports: ThinVec<ShadowedImport>,
    pub(crate) use_imports_types: Keyed<ImportOrExternCrate, ImportOrDef>,
    pub(crate) use_imports_values: Keyed<ImportOrGlob, ImportOrDef>,
    pub(crate) use_imports_macros: Keyed<ImportOrExternCrate, ImportOrDef>,
    pub(crate) use_decls: ThinVec<UseId>,
    pub(crate) extern_crate_decls: ThinVec<ExternCrateId>,
    pub(crate) legacy_macros: Keyed<Name, SmallVec<[MacroId; 1]>>,
    pub(crate) attr_macros: Keyed<AstId<ast::Item>, MacroCallId>,
    pub(crate) macro_invocations: Keyed<AstId<ast::MacroCall>, MacroCallId>,
    pub(crate) derive_macros: Keyed<AstId<ast::Adt>, SmallVec<[DeriveMacroInvocation; 1]>>,
}
```

After (the doc comments of today's fields stay on each; the `sorted` and `by_def` comments
move to `SettledTables` and `BuildingTables`):

```rust
pub struct ItemScope<S: ScopeStore = Settled> {
    pub(crate) types: Column<ModuleDefId, ImportOrExternCrate, S>,
    pub(crate) values: Column<ModuleDefId, ImportOrGlob, S>,
    pub(crate) macros: Column<MacroId, ImportOrExternCrate, S>,
    pub(crate) unresolved: S::Vec<Name>,
    pub(crate) names: Index<S>,
    pub(crate) tables: S::Tables,
    pub(crate) declarations: S::Vec<ModuleDefId>,
    pub(crate) impls: S::Vec<ImplId>,
    pub(crate) trait_impl: Bits<S>,
    pub(crate) reservation_impl: Bits<S>,
    pub(crate) builtin_derive_impls: S::Vec<BuiltinDeriveImplId>,
    pub(crate) extern_blocks: S::Vec<ExternBlockId>,
    pub(crate) unnamed_consts: S::Vec<ConstId>,
    pub(crate) unnamed_trait_imports: S::Vec<(TraitId, Item<(), ImportOrExternCrate>)>,
    pub(crate) underscore_imports: S::Vec<UnderscoreImport>,
    pub(crate) glob_ambiguities: S::Vec<GlobAmbiguity>,
    pub(crate) shadowed_imports: S::Vec<ShadowedImport>,
    pub(crate) use_imports_types: Keyed<ImportOrExternCrate, ImportOrDef, S>,
    pub(crate) use_imports_values: Keyed<ImportOrGlob, ImportOrDef, S>,
    pub(crate) use_imports_macros: Keyed<ImportOrExternCrate, ImportOrDef, S>,
    pub(crate) use_decls: S::Vec<UseId>,
    pub(crate) extern_crate_decls: S::Vec<ExternCrateId>,
    pub(crate) legacy_macros: Keyed<Name, S::List<MacroId>, S>,
    pub(crate) attr_macros: Keyed<AstId<ast::Item>, MacroCallId, S>,
    pub(crate) macro_invocations: Keyed<AstId<ast::MacroCall>, MacroCallId, S>,
    pub(crate) derive_macros: Keyed<AstId<ast::Adt>, S::List<DeriveMacroInvocation<S>>, S>,
}
```

Each struct's `impl` blocks split into three:

- `impl<.., S: ScopeStore>` holds the reads: `Index::probe`, `Column::iter`/`get`/`len`,
  `Keyed::get`/`iter`/`keys`/`values`/`len`, `Bits::get`.
- `impl<.., 'f> .. <.., Building<'f>>` holds the writes: `push`, `insert`, `set`, `reserve`, and
  `new_in(frame)` in place of `Default`.
- `impl ..` on `Settled` holds the frozen read's `reserve`/`push_unindexed` and `reindex`.

The rules for `ItemScope` itself are R10.2's. The `ScopeSlot` and `ModulesSlot` building arms
and `BuildingScope` go, as R10.3 says.

### R10.7 `ItemScope::owned_allocations` (asked by mem-db-def-collect, H9.1; `item_scope.rs`)

```rust
#[cfg(test)]
impl Index {
    fn owned_allocations(&self) -> u64 {
        u64::from(!self.slots.is_empty())
    }
}

#[cfg(test)]
impl<K, V> Keyed<K, V> {
    fn owned_allocations(&self) -> u64 {
        u64::from(!self.entries.is_empty()) + self.index.owned_allocations()
    }
}

#[cfg(test)]
impl ItemScope {
    /// The heap buffers this settled scope owns, as `settle` makes them: one per non-empty list,
    /// map, index and table, and one per map value that left its inline slot.
    pub(crate) fn owned_allocations(&self) -> u64 {
        let Self {
            types,
            values,
            macros,
            unresolved,
            names,
            tables,
            declarations,
            impls,
            trait_impl,
            reservation_impl,
            builtin_derive_impls,
            extern_blocks,
            unnamed_consts,
            unnamed_trait_imports,
            underscore_imports,
            glob_ambiguities,
            shadowed_imports,
            use_imports_types,
            use_imports_values,
            use_imports_macros,
            use_decls,
            extern_crate_decls,
            legacy_macros,
            attr_macros,
            macro_invocations,
            derive_macros,
        } = self;
        let list = |len: usize| u64::from(len > 0);
        let spilled = |len: usize| u64::from(len > 1);
        let lists = [
            types.entries.len(),
            values.entries.len(),
            macros.entries.len(),
            unresolved.len(),
            tables.sorted.len(),
            tables.by_def.len(),
            declarations.len(),
            impls.len(),
            trait_impl.0.len(),
            reservation_impl.0.len(),
            builtin_derive_impls.len(),
            extern_blocks.len(),
            unnamed_consts.len(),
            unnamed_trait_imports.len(),
            underscore_imports.len(),
            glob_ambiguities.len(),
            shadowed_imports.len(),
            use_decls.len(),
            extern_crate_decls.len(),
        ];
        let maps = [
            use_imports_types.owned_allocations(),
            use_imports_values.owned_allocations(),
            use_imports_macros.owned_allocations(),
            legacy_macros.owned_allocations(),
            attr_macros.owned_allocations(),
            macro_invocations.owned_allocations(),
            derive_macros.owned_allocations(),
        ];
        let macro_lists: u64 = legacy_macros.values().map(|it| spilled(it.len())).sum();
        let derive_lists: u64 = derive_macros
            .values()
            .map(|calls| {
                spilled(calls.len())
                    + calls.iter().map(|it| spilled(it.derive_call_ids.len())).sum::<u64>()
            })
            .sum();
        lists.into_iter().map(list).sum::<u64>()
            + maps.into_iter().sum::<u64>()
            + names.owned_allocations()
            + macro_lists
            + derive_lists
    }
}
```

A settled list is exact, so it is allocated exactly when it is non-empty. An empty `ThinVec` and
an empty box allocate nothing, and a `SmallVec<[_; 1]>` allocates only past one element.

### R10.8 `Declared` and `reserve_declared` (asked by mem-db-def-collect, H9.2; `item_scope.rs`)

Collect's `Declared` struct, as its H9.2 gives it, goes in `item_scope.rs`. Then:

```rust
impl Bits<Building<'_>> {
    fn reserve(&mut self, bits: usize) {
        self.0.reserve(bits.div_ceil(64));
    }
}

impl<'f> ItemScope<Building<'f>> {
    /// Room for the items one list declares into this scope: a capacity hint in the frame, so
    /// the lists the item tree already counts grow once.
    pub(crate) fn reserve_declared(&mut self, d: &Declared) {
        let n = |it: u32| it as usize;
        self.declarations.reserve(n(d.named));
        self.types.entries.reserve(n(d.named));
        self.values.entries.reserve(n(d.named));
        self.impls.reserve(n(d.impls));
        self.trait_impl.reserve(n(d.impls));
        self.reservation_impl.reserve(n(d.impls));
        self.use_decls.reserve(n(d.uses));
        self.extern_crate_decls.reserve(n(d.extern_crates));
        self.macro_invocations.reserve(n(d.macro_calls));
        self.legacy_macros.reserve(n(d.macro_rules));
        self.unnamed_consts.reserve(n(d.unnamed_consts));
        self.extern_blocks.reserve(n(d.extern_blocks));
    }
}
```

`named` reserves both `types` and `values`. A named item lands in one of them, or both for a
unit or tuple struct, so this over-reserves in the frame by at most `named` entries, which costs
no global allocation.

### R10.9 `Visibility::max` and `min` (asked by mem-db-def-collect, H9.3; `visibility.rs` 159, 222; db-expand)

In both, `def_map: &DefMap,` becomes `def_map: &impl DefMapRead,`. The bodies read only
`def_map.krate()` and `def_map.block_id()`, which `DefMapRead` provides, and are unchanged.
Callers pass `&DefMap` (resolver, facts) or `&BuildingDefMap` (`collector.rs` 1716, 1857).

### R10.10 After db-expand-names' deferral

- **(1) The cold path is facts'.** mem-db-def-collect's Part H has retargeted the building side:
  - H2 covers the two queries.
  - H6: `facts::collect_crate` assembles the crate's map from the final round's `ModuleScope`
    handles, so it never holds a building map. It builds through `DefMap::from_table`, and
    `collect_block` takes and settles a `BuildingDefMap<'f>`.
  - H7 covers the frozen read, through `from_settled`.
  - The facts unit's own types are frontend-nameres'.

  R10.3's collector sites stay on `collector.rs`, the `#[cfg(test)]` differential reference,
  which has to compile against the same types. T5 measures whichever path
  `crate_local_def_map` runs, and mem-db-def-collect's T6 sits beside it.
- **(2) Order: agreed.** R10 lands after hir-def has built and run once with H1 to H3, R1 to R5,
  T1 to T3 and the facts switch-over. R10.1 (`DefMapRead`/`ResolvePath`) and R10.2 plus R10.6
  (`ItemScope<S>`) go first, since collect's Part H builds on them. R10.3, R10.7 to R10.9 and
  collect's Part H follow.
- **Two corrections to R10.3, from collect's H1.** `BuildingDefMap::new_in` takes the root's
  `(ModuleOrigin, Visibility)`, since there is no building `ModuleData` to pass. `child` keys
  `child_at` by `(module, *name)` (`Name` is `Copy`).

## T5. A cold `crate_local_def_map` allocates the same for any width (`nameres/tests/budget.rs`, collect's Part F file; db-expand-names)

> deferred by db-expand-names (2026-10-29 21:05) with R10; see R10's note.

With R10, a cold collection's global allocations are the settled tables (one per non-empty list,
map, index and table) plus salsa's own costs for the query and the ids it makes. Neither depends
on how many names a list holds. The fixture adds names without adding ids: one `use` with n
renames of one struct gives one `UseId` and no new interned id per name. The item tree, which
interns the names and makes the `AstIdMap`, is computed outside the measure. The widest crate is
collected first, so this thread's blaze-mem scratch shelves and frames have grown to it before
anything is measured. Without R10, the count grows with n (about log2 n reallocations per
growing column), so this test fails today and passes once R10 lands.

```rust
/// A crate whose module `wide` binds `n` names (types and values) through one `use`.
fn wide_crate(n: usize) -> String {
    let renames: Vec<String> = (0..n).map(|i| format!("S as A{i}")).collect();
    format!(
        "//- /lib.rs crate:main\npub mod m {{ pub struct S; }}\npub mod wide {{ pub use super::m::{{{}}}; }}\n",
        renames.join(", ")
    )
}

/// The global allocations of a cold `crate_local_def_map` of [`wide_crate`]`(n)`, with its item
/// tree made first.
fn cold_collection(n: usize) -> u64 {
    let (db, files) = TestDB::with_many_files(&wide_crate(n));
    let krate = db.fetch_test_crate();
    black_box(file_item_tree(&db, files[0].into(), krate));
    let (pair, used) = measure(|| crate_local_def_map(&db, krate));
    let map = pair.def_map(&db);
    let wide = map
        .modules(&db)
        .find(|(id, _)| id.name(&db).is_some_and(|it| it.as_str() == "wide"))
        .map(|(id, _)| id)
        .expect("the fixture has `wide`");
    assert_eq!(map.module(&db, wide).scope(&db).entries().count(), n);
    used.allocations
}

/// Budget: a cold collection allocates the same whether a module holds 9 names or 513: every
/// table being collected grows in the frame, and the memo's tables are made once each at their
/// length. Each width crosses one more doubling of every growing column, and all stay above
/// `LINEAR`, so the settled tables are the same in number.
#[test]
fn a_cold_collection_allocates_the_same_for_any_width() {
    const WIDTHS: [usize; 7] = [9, 17, 33, 65, 129, 257, 513];
    black_box(cold_collection(WIDTHS[WIDTHS.len() - 1]));
    let counts = WIDTHS.map(cold_collection);
    assert!(
        counts.iter().all(|&it| it == counts[0]),
        "a cold collection's allocations grew with the number of names: {:?}",
        WIDTHS.iter().zip(&counts).collect::<Vec<_>>()
    );
}
```

The file's imports gain `crate::nameres::crate_local_def_map`. `file_item_tree`, `measure` and
`black_box` are already there (Part F). The test runs on the calling thread, as Part F's do
(`TestDB`'s `RUN_ORDER`).

> forwarded to frontend-nameres (frontend-nameres-next1, `nameres/tests/`, collect's) (requests, pass 19; told by tmux).

> frontend-nameres (2026-10-29 21:24): accepted, deferred with R10 as db-expand-names noted above. The test fails until R10 and H6 land (each growing column costs about log2 n reallocations), and a red test in `nameres/tests/budget.rs` would hold up hir-def's first build. It goes in the change that lands R10.3's facts part and H6, as written here, and the cold path it measures is `facts::collect_crate`.

## Speed of light

- A path of n segments on a warm map: n Fx hashes and n probes of a 4-byte-slot index (one cache
  line each), or a scan of at most 8 40-byte entries, and 0 allocations. That is the floor for a
  hash lookup per segment, and T1 holds it.
- A resolver walk at depth d: d `Copy` scope reads, plus one probe per scope that can hold the
  name. The traits walk reads each scope's memoized `traits()` slice once and allocates nothing;
  a caller that needs a set fills its own, once.
- A settled scope of n names keeps 4n bytes per table (by-name and by-def) above `LINEAR`, and
  none below. Building them borrows two `u32` scratch buffers of n entries per worker
  (blaze-mem's shelf, reused), and the one exact copy is the only allocation.
- A cold collection makes one global allocation per non-empty list, map, index and table of the
  memo, plus salsa's own per query and per id, and nothing per name. With R10, everything that
  grows while collecting grows in the frame, in place, in reserved memory. That is the floor,
  since the memo must own each table, and T5 holds it. Without R10, each growing column costs
  about log2 n more.
- `find_path`: per thread, the scratch grows to the widest search once. A call then allocates
  only for a `ModPath` intern miss.

> applied: nothing to apply (notes) (requests, pass 18).
