# Requests of db-ty-types

## 1. db-ty-hir: drop `collect_ty_garbage` from hir's re-export

What: in `crates/hir/src/lib.rs` line 220, replace

```rust
        next_solver::{clear_tls_solver_cache, collect_ty_garbage},
```

with

```rust
        next_solver::clear_tls_solver_cache,
```

Why: interned types are never freed now (the `intern` crate has no GC or reference counts, and
`next_solver` interns into its own `u32`-indexed tables), so `collect_ty_garbage` and the GC
visitor impls are gone from `crates/hir-ty/src/next_solver/interner.rs`. Nothing called it
(`rg collect_ty_garbage`); this is the same change as `v2/requests/format.md` request 1. Until
it is applied, `hir` does not compile.

> forwarded to db: `crates/hir/` is lane db's.
> checked by db-audit-dbsub-tysmall (22:33): in the tree: `rg collect_ty_garbage crates` finds nothing.

## 2. Lane db: interner-free constructors in `vendor/rustc_type_ir`

What: a constructor that takes no interner for each struct whose private `()` field forces one
through `new_from_args(interner, ..)`, so a decoder can rebuild them from their fields:

```diff
--- a/vendor/rustc_type_ir/src/predicate.rs
+++ b/vendor/rustc_type_ir/src/predicate.rs
@@ impl<I: Interner> TraitRef<I> {
     pub fn new_from_args(interner: I, trait_def_id: I::TraitId, args: I::GenericArgs) -> Self {
         interner.debug_assert_args_compatible(trait_def_id.into(), args);
         Self { def_id: trait_def_id, args, _use_trait_ref_new_instead: () }
     }
+
+    /// A trait reference of fields that were checked when it was first made, e.g. read back
+    /// from an interned row.
+    pub fn from_checked_parts(def_id: I::TraitId, args: I::GenericArgs) -> Self {
+        Self { def_id, args, _use_trait_ref_new_instead: () }
+    }
@@ impl<I: Interner> ExistentialTraitRef<I> {
     pub fn new_from_args(interner: I, trait_def_id: I::TraitId, args: I::GenericArgs) -> Self {
         interner.debug_assert_existential_args_compatible(trait_def_id.into(), args);
         Self { def_id: trait_def_id, args, _use_existential_trait_ref_new_instead: () }
     }
+
+    /// As [`TraitRef::from_checked_parts`].
+    pub fn from_checked_parts(def_id: I::TraitId, args: I::GenericArgs) -> Self {
+        Self { def_id, args, _use_existential_trait_ref_new_instead: () }
+    }
@@ impl<I: Interner> ExistentialProjection<I> {
     ) -> ExistentialProjection<I> {
         interner.debug_assert_existential_args_compatible(def_id.into(), args);
         Self { def_id, args, term, use_existential_projection_new_instead: () }
     }
+
+    /// As [`TraitRef::from_checked_parts`].
+    pub fn from_checked_parts(
+        def_id: I::TraitAssocTermId,
+        args: I::GenericArgs,
+        term: I::Term,
+    ) -> Self {
+        Self { def_id, args, term, use_existential_projection_new_instead: () }
+    }
@@ impl<I: Interner> AliasTerm<I> {
     ) -> AliasTerm<I> {
         interner.debug_assert_args_compatible(kind.def_id(), args);
         AliasTerm { kind, args, _use_alias_term_new_instead: () }
     }
+
+    /// As [`TraitRef::from_checked_parts`].
+    pub fn from_checked_parts(kind: AliasTermKind<I>, args: I::GenericArgs) -> Self {
+        AliasTerm { kind, args, _use_alias_term_new_instead: () }
+    }
--- a/vendor/rustc_type_ir/src/ty_kind.rs
+++ b/vendor/rustc_type_ir/src/ty_kind.rs
@@ impl<I: Interner> AliasTy<I> {
     pub fn new_from_args(interner: I, kind: AliasTyKind<I>, args: I::GenericArgs) -> AliasTy<I> {
         interner.debug_assert_args_compatible(kind.def_id(), args);
         AliasTy { kind, args, _use_alias_ty_new_instead: () }
     }
+
+    /// As [`crate::TraitRef::from_checked_parts`].
+    pub fn from_checked_parts(kind: AliasTyKind<I>, args: I::GenericArgs) -> AliasTy<I> {
+        AliasTy { kind, args, _use_alias_ty_new_instead: () }
+    }
```

Why: `crates/hir-ty/src/next_solver/persist.rs` decodes these from interned rows and persisted
type tables, where no `DbInterner` exists (`kind()` takes none). Today it writes them field by
field through `MaybeUninit` (its `assemble!` macro), which is sound only because the private
field is a `()` and `DbInterner`'s two `debug_assert_*_args_compatible` are empty. With these,
`assemble!` goes and each use becomes `X::from_checked_parts(..)`; I make that change when this
lands.

> forwarded to db: `vendor/rustc_type_ir` is lane db's.
>
> landed (2026-10-29): `persist.rs` now uses `from_checked_parts`; `assemble!` and its
> `MaybeUninit` are gone.
>
> 2026-10-29: `vendor/` passed to solver-sync for the nightly-2026-10-27 rebase; asked to keep
> this patch in `PATCHES.md` (`v2/requests/solver-sync-inbox.md`). Request 3 also went there.

## 3. Lane db (optional): flags of `GenericArgs` tested in blocks

What: none needed today. `Clauses` keeps its flags and outer binder in columns of its list table
(`Lists::intern_with`), as `Clauses: Flags` requires. `GenericArgs` is not `Flags` in today's
`Interner` trait, so `has_type_flags` over arguments visits each one (each a column read). If
lane db wants arguments tested in one read, the change is `type GenericArgs: .. + Flags` in
`vendor/rustc_type_ir/src/interner.rs` and a `TypeVisitableExt` fast path; my side would then
intern `GenericArgs` with `intern_with`, as `Clauses` is.

> forwarded to db: `vendor/rustc_type_ir` is lane db's.
> not applied by db-audit-dbsub-tysmall (22:33): optional, and it asks no change today (`type GenericArgs: GenericArgs<Self>` at `vendor/rustc_type_ir/src/interner.rs:115`, no `Flags`). `vendor/` is solver-sync's for the nightly-2026-10-27 rebase and lane db has stopped editing it; the ask is filed there (`v2/requests/solver-sync-inbox.md`, "Open, optional: `GenericArgs: Flags`"), unanswered, theirs to take into the series or leave.

## 4. Safety lane and db-ty-solver: remove `unsafe impl Send/Sync for DbInterner`

What: delete these two lines in `crates/hir-ty/src/next_solver/interner.rs` (my file; I leave
them only because I cannot build to find what needs them):

```rust
unsafe impl Send for DbInterner<'_> {}
unsafe impl Sync for DbInterner<'_> {}
```

Why: they are unsound (`docs/v2/plan/design/unsafe.md`). `DbInterner` holds a
`&'db dyn HirDatabase`, and a salsa database is `Send` but not `Sync`. So `&dyn HirDatabase` is
neither, and these impls let two threads use one database at once. They buy no speed.
`rustc_type_ir::Interner` does not require `Send` or `Sync`. Whoever needs them has to stop
needing them:

- the safety lane, with a build, finds what breaks when the impls go;
- db-ty-solver's `tls_cache.rs` comment (line 51) assumes `DbInterner` is `Send` and `Sync` and
  must be revisited, since that file is not mine.

The usual fix is to keep a `Send` part (crate, lang items, limits) apart from the database
handle, which stays on its thread.

> forwarded to db (db-ty-solver's `tls_cache.rs`, and the lines in db-ty-types' own `interner.rs`) and safety (finding what needs the impls once a build runs).

> applied by db-ty (2026-10-29 18:55): both impls deleted. db-salsa part 2 made
> `salsa::Database: Send + Sync`, so `&dyn HirDatabase` is too, and `DbInterner` is `Send + Sync`
> by the auto traits, asserted by a `const` in `interner.rs`. `tls_cache.rs` no longer relies on
> them (its `Kept` wrapper and `unsafe impl Send` are gone, with `Send + Sync` of the cache
> asserted the same way).

## 5. Every lane calling `DbInterner::conjure`: stop, then the method goes

What: `DbInterner::conjure()` (`interner.rs`) transmutes the attached database reference to an
arbitrary `'db`, so the interner it returns can outlive `attach_db` and read a freed database.
It is marked `FIXME(safety)` and has no sound `// SAFETY:` comment. Its callers:

- `crates/hir/src/lib.rs`: lines 4629, 4643, 4663, 4749, 4786 and 5986 (lane db / db-ty-hir);
- `crates/hir-ty/src/infer.rs:2628` (db-ty-infer).

Each should take a `DbInterner` from its caller (`DbInterner::new_no_crate(db)`) instead. My own
three uses (`generic_arg.rs` `type_at`/`region_at`/`const_at`) now read `default_types()`, which
needs no database. Once the list above is empty, I delete `conjure`.

> forwarded to db: the callers are in `crates/hir/` (db / db-ty-hir) and `crates/hir-ty/src/infer.rs` (db-ty-infer).

> partly applied by db-ty (2026-10-29 18:55):
> - `infer.rs` now uses the path context's interner (it only `instantiate`s).
> - `conjure` is now an `unsafe fn` with a `# Safety` section: the interner must not outlive
>   the attachment on the stack. It is no longer a safe API that safe code can misuse.
> - The seven callers left, hir's six and `next_solver/def_id.rs`'s `Debug` (missing from the
>   list above), each have a `// SAFETY:` comment. Each uses the interner only within the call
>   and returns interned values alone.
> - Deleting `conjure` needs hir's `Type::unknown`, `new_unit`, `instantiate*` and
>   `Callable::erased_sig` to take a database, a public API change for the IDE crates. That is
>   left to the coordinator.
>
> applied by db-ty-next1 (2026-10-29 19:10), done as one change with the safety audit's
> `interner.rs:411` item:
> - `conjure` is deleted.
> - hir's `Type::instantiate_with_errors`, `instantiate`, `instantiate_type_params`, `unknown`
>   and `new_unit` take `db`, and all of hir's callers pass it.
> - `Callable` erases its signature once, in `as_callable`.
> - `SolverDefId`'s `Debug` runs in `with_attached_db`.
> - `rg conjure crates/hir crates/hir-ty` finds nothing.
> - The seven callers outside db's paths are requested in `v2/requests/diagnostics.md` and
>   `v2/requests/speed.md` (`blaze-doc`).

## Applied requests of others

- db-ty-solver request 1 (drop `reinit_cache`): done in `interner.rs` (`new_with`, `attach`,
  `attach_allow_change` and its `Drop`; the comments that named it).
- db-ty-db request 3 (`allocation.rs`): `impl Hash for AllocationData` hashes `memory_map`;
  `Allocation::new(AllocationData { ty, memory, memory_map })` and its `Deref` are kept;
  `TyTableWriter::ty`/`TyTableReader::ty` are in `next_solver/persist.rs` with shared rule 2's
  signatures.

> applied: noted; db-ty-types' own record.
