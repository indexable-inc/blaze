# Requests from lane items

## items-facts: `InferenceResult::defined_anon_consts` (`crates/hir-ty/src/infer.rs`)

What: a public accessor for the anonymous consts a body's inference defined (its inline
`const { .. }` blocks, array lengths and const arguments written in the body):

```rust
impl<'db> InferenceResult<'db> {
    /// The anonymous consts of the body this inferred, including those inside them.
    pub fn defined_anon_consts(&self) -> &[AnonConstId<'db>] {
        &self.defined_anon_consts
    }
}
```

Where: `crates/hir-ty/src/infer.rs`, on `InferenceResult` (the field `defined_anon_consts` is
private, line ~862).

Why: `Fact::Captures(key)` resolves its key to `Resolved::Closure { store, expr, kind }` and
has to rebuild the closure's `InternedClosureId`, whose owner is the `InferBodyId` that
inferred it: the body, or the anonymous const the closure is inside (`infer_anon_const_query`
infers those apart and `merge_anon_consts` merges them). Guessing the body as owner would name
a closure id inference never made, so its captures would be wrong. For signatures and fields
`AnonConstId::all_from_signature` and `field_types_with_diagnostics(..).defined_anon_consts()`
already list them; for bodies only the inference result has the list.
`crates/blaze-items/src/facts.rs` (`closure_owner`) calls
`InferenceResult::of(db, body).defined_anon_consts()` as written above; it does not compile
until this lands. Read-only, no behavior change.

> forwarded to db: `crates/hir-ty/src/infer.rs` is db's (db-ty).
> checked by db-audit-other (22:39): in the tree at `crates/hir-ty/src/infer.rs:1914` (`InferenceResult::defined_anon_consts`).

## items-form: `VTableMap::tys` (`crates/hir-ty/src/mir.rs`)

Applied by speed-types (2026-10-29). `form.rs` `Cx::allocation` now reads `map.vtable.tys()`;
the `facts.rs` half of speed-types' note below is items-facts'.

What: a public iterator over a vtable map's types in id order:

```rust
impl<'db> VTableMap<'db> {
    /// The types of the vtables, in the order of their ids.
    pub fn tys(&self) -> impl ExactSizeIterator<Item = Ty<'db>> + '_ {
        self.id_to_ty.iter().copied()
    }
}
```

Why: `form.rs` (`Cx::allocation`) spells a constant's vtables. `OFFSET` and `id_to_ty` are
private, so it finds the first id `ty(id)` accepts by counting up from 0 and then reads on
until one fails. That is correct but leans on how ids are laid out, and it costs about 1000
failed `ty` calls (each building a `MirEvalError`) per constant that holds a vtable, in every
form that spells one. With `tys` it is one line and no wasted work. Read-only, no behavior
change.

> forwarded to db: `crates/hir-ty/src/mir.rs` is db's (db-ty).
> checked by db-audit-other (22:39): in the tree at `crates/hir-ty/src/mir.rs:2172` (`VTableMap::tys`), as speed-types' line above says.

## items-form: `tait_hidden_types` public (`crates/hir-ty/src/opaques.rs`), optional

What: `pub(crate) fn tait_hidden_types` → `pub fn`.

Why: `Cx::hidden` gets a TAIT's hidden type through
`DbInterner::type_of_opaque_hir_typeck(OpaqueTyIdWrapper(..))`, which reads it. Reading the
query itself would say what is meant. Not blocking.

> forwarded to items: `crates/hir-ty/src/opaques.rs` is items' own file (crates.md).
> applied: lane items is DONE; `tait_hidden_types` is `pub` and re-exported beside `rpit_hidden_types` (`hir_ty::tait_hidden_types`), since `mod opaques` is private.

## items-form, for lane items: who observes a type's name through a generic wrapper

`Cx` spells the `item_key` and path of nominal types only where `type_name` / `type_id` (the
intrinsics) are called directly, and for the `TypeId`s a constant holds. A body that calls
`std::any::type_name::<A>()` (a generic fn that calls the intrinsic) gets `A` as a slot, and
the slot's hash does not include `A`'s name, so renaming `A` keeps that body's hash while the
program's output changes. Fixing it needs a transitive fact: "this generic fn, instantiated
with these parameters, observes their names", computed over the call graph, and the form
spelling names for a call to any fn with that fact. That is a cross-item fact, which is the
parent's (`hash`/`facts`), not something one form can see.

> forwarded to items: settled by items below.

## items-form, for lane items: share type forms across items and revisions

`form::Cx` memoizes a type's `Sub` in a `RefCell<FxHashMap<Ty, Rc<Sub>>>` that lives only as
long as one `Cx`. Every item's form re-spells the types it shares with others (`Vec<T>`,
`Option<&str>`, every std type in a signature), and every revision re-spells them all. A
salsa-tracked `ty_form(db, Ty) -> Sub` would compute each once per revision and keep it across
edits that don't touch it. That needs `Sub: salsa::Update` (its fields are `ContentId`,
`Vec<ItemId>`, `Option<&'static str>`), and the `spelling` cycle guard turned into salsa cycle
handling. I didn't make the change without a compiler to check it; the parent, which builds,
should decide where it goes and measure it.

> forwarded to items: the parent's call, in its own crate.
> forwarded to coordinator: lane items is DONE and left this among its "costs that need profiling"; a tracked `ty_form` changes items' own crate, not an interface type, and wants measuring after the first compile.

## items: an instance of a name-observing fn is keyed on its arguments' names; a const's data on its value (for `codegen`)

This settles items-form's "who observes a type's name through a generic wrapper" above. A
body hashes a direct call of `type_name`/`type_id` with the key and path of the nominal types
in its arguments (`form`), so only the instance that calls the intrinsic embeds a name: with
callees named by identity (the request above), `std::any::type_name::<A>`'s caller calls that
instance and holds no name itself. What codegen must do:

- An instance of a fn for which `blaze_items::observes_names(db, func)` holds, or whose body
  calls one with its own generic parameters in the arguments, is keyed on the
  `Fact::Name` of every nominal type in its instance arguments as well as its code hash. An
  instance that inlines one (release, deep hash) carries the same.
- A const's or static's data is keyed on its `Fact::ConstValue` (the evaluated bytes), never
  on its code hash: `const C: &str = f::<A>()` keeps its code hash when `A` is renamed while
  its bytes change.

Why: otherwise a rename keeps a stale `type_name` string or `TypeId` (docs/v2/plan/lanes/items.md 6.1:
names are in a hash only where the program observes them, and here it does, one instance away).

> forwarded to codegen

## items: `ContentId::of_many` and one derived key per domain (for `format`, `crates/blaze-ids`)

What, both bit-identical to today's output on every path:

1. `ContentId::of_many(domain: &'static str, inputs: &[&[u8]]) -> Vec<ContentId>`, exactly
   `inputs.iter().map(|it| ContentId::of(domain, it))`, hashing many small inputs per SIMD
   pass (16 lanes with AVX-512, 8 with AVX2, 4 with NEON and SSE4.1), picked at run time by
   CPU feature. This is store's request 5 (`v2/requests/store.md`); items calls it too.
   `crates/blaze-items/src/key.rs` (`hash_all`) hashes the keys of a module's, an impl's, a
   trait's and an enum's items in one call, and doesn't build until it lands.
2. `Hasher::new(domain)` derives the domain's key (`new_derive_key` hashes the context
   string, one compression) on every call. Derive it once per domain and clone a prototype
   `blake3::Hasher` (it is `Clone`; the output is unchanged): a per-domain cache keyed by
   the `&'static str`'s address, or a `Domain` const type made once. Items makes millions of
   small hashes (two per `form::Form`, one per key, fact and component), so this is up to
   half their cost.
3. The root `Cargo.toml`: `blake3` with the `neon` feature (store's request 5), so aarch64
   (Graviton, Apple) gets NEON.

Why: docs/v2/plan/design/hardware.md, "BLAKE3 of sources, items and blobs ... one batched
call per many small inputs, not one call each".

> forwarded to format: items 1 and 2 are `crates/blaze-ids` (format's).
> applied: item 3, `blake3` with `neon` in the root `Cargo.toml` (store's request 5).

## items-facts, for lane items: the API of `crate::table::FactSet` (`crates/blaze-items/src/table.rs`)

What: the signatures of `FactSet` (and `HashColumns`, if facts.rs should use it), with how a
set is built, how a fact's subject and object are given (as `ContentId`s, mapped to `u32`
through the sorted dictionary?), and the order iteration yields. `table.rs` is not in the
tree yet and I may not touch it, so facts.rs cannot call it.

Where facts.rs gathers several facts, so where it would use it, and the order each value
hashes them in today, which must not change:
- `implements_value` (index.rs): the `Implements` run for the self type, then the blanket
  run, each sorted by key;
- `Vtable`: per supertrait in `all_super_traits` order, its interface and its `Implements`
  value; then its methods in declaration order;
- `Method` (`write_method`): inherent impls sorted by (interface hash, items), then traits
  sorted by (items, `Implements` value);
- `Coherence`: one `Implements` value, or every impl of the trait in index order.

If `FactSet` iterates in (kind, subject, object) order with the dictionary sorted by
`ContentId`, the declaration-order ones (Vtable's supertraits and methods) must still be
written from their own order, not the set's.

Why: docs/v2/plan/design/data.md, items row: "facts as `(kind, subject, object)` triples
sorted for range scans".

> answered (items): `table.rs` is in the tree. `FactSet` is `FromIterator<Fact>`; `iter()`
> yields facts sorted by (kind, subject, object) by content, distinct; `about(kind, subject:
> ContentId)` is one kind's range about one subject; `contains(&Fact)`. It is for a query's
> read set (which facts it depends on), not for a fact's value: the four places above hash
> in their own order and stay as they are.
> applied: answered by items above; nothing to change (facts.rs keeps its own orders).

## items-hirty: 4-byte interned ids (db, `vendor/salsa`)

`TraitImpls` and `InherentImpls` are now columns (`crates/hir-ty/src/method_resolution.rs`), but
every `ImplId`, `TraitId` and `BuiltinDeriveImplId` in them is 8 bytes: salsa's `Id` is an index
plus a generation. These ids are interned with `revisions = usize::MAX` (`impl_intern_key!` in
`base-db`, `BuiltinDeriveImplId` in `hir-def`), so the generation is never bumped and is always
the same. Asked: an id type of 4 bytes (`NonZeroU32` alone) for interned structs that are never
collected. That halves every impl column, and every other long-lived id array too. The asserts
beside the columns say `<= 8` today, so they keep holding when ids shrink; tighten them to
`== 4` then. Why: docs/v2/plan/design/data.md, "a type is a `u32`" and the size budgets.

> forwarded to db: `base-db`'s `impl_intern_key!`, `hir-def` and `vendor/salsa` are lane db's. The `<= 8` asserts in `method_resolution.rs` (items DONE, now mine) tighten to `== 4` once ids shrink.

## items-hirty: simplified self types as `u32` (db, type tables)

The self type column of the impl tables keeps the `SimplifiedType<SolverDefId>` (16 to 24 bytes)
plus a `u64` hash column that is searched, because `SimplifiedType` has no `Ord`. When db's
type tables give each simplified type a `u32` (a tag byte plus the def's 4-byte id would do),
the self type column becomes that `u32`, sorted directly, and the hash column goes away. Asked:
a `u32` encoding of `rustc_type_ir::fast_reject::SimplifiedType` from db, or `Ord` on it and on
`SolverDefId` in `vendor/rustc_type_ir`. Nothing waits on this; the tables are correct without it.

> forwarded to db: the type tables and `vendor/rustc_type_ir` are lane db's.

> applied by solver-sync (20:15): `SimplifiedType` derives `PartialOrd + Ord`
> (`vendor/rustc_type_ir/patches/0005-simplified-type-ord.patch`), and `SolverDefId` already did,
> so `SimplifiedType<SolverDefId>` sorts. Its order follows `SolverDefId`'s, so use it only as a
> table key, never for anything output.

## speed-types: read vtables with `VTableMap::tys` (`form.rs` 2092-2099, `facts.rs` 497-510)

`crates/hir-ty/src/mir.rs` now has `VTableMap::tys()` (the types in id order, an
`ExactSizeIterator`) and `VTableMap::FIRST_ID` (items-form's request, applied by speed-types).
Both sites probe ids up from 0, building about 1000 `MirEvalError`s for each constant with a
vtable. Edits, bit-identical:

- `form.rs` `Cx::allocation`: `let vtables: Vec<_> = map.vtable.tys().collect();` replaces
  the `first` search and `map_while`. Or skip the `Vec`: `f.len(map.vtable.tys().len());
  for ty in map.vtable.tys() { .. }`.
- `facts.rs` `const_value`: for a non-empty map, `h.update(&(hir_ty::mir::VTableMap::FIRST_ID
  as u64).to_le_bytes())`, then `for ty in map.vtable.tys() { vtables.push(ty_hash(db, owner,
  ty)?) }`. That is the same `id` the loop found, so the value is unchanged.

speed-types takes these itself once items-form and items-facts are idle, unless you get there
first.

> applied by items-facts: the `facts.rs` half, as written (`FIRST_ID`, then `tys()`). The
> `form.rs` half is items-form's.
> applied: both halves are in: `facts.rs` by items-facts, `form.rs` `Cx::allocation` reads `f.len(map.vtable.tys().len())` then `tys()`.

## items-facts, for lane items: `#![forbid(unsafe_code)]` on blaze-items (`src/lib.rs`, `src/key.rs`)

What: `key.rs:334`, `Siblings::Module(unsafe { module.to_static() })`, is the crate's only
`unsafe`. It erases a lifetime so that the module can be compared with `siblings(db, id)`.
That buys no measured speed and reaches no kernel or hardware, so under
docs/v2/plan/design/unsafe.md it should not be `unsafe`. Compare without it, for example by
making `Siblings` generic over the module's lifetime, or by comparing the salsa id. Then
`lib.rs` can take `#![forbid(unsafe_code)]`: `facts.rs`, `index.rs` and `tests/facts.rs`
have no `unsafe` and need none.

Why: docs/v2/plan/design/unsafe.md, "Crates with no unsafe module get
`#![forbid(unsafe_code)]`".

> applied (items): `module_item_keys` compares the module by its salsa id
> (`salsa::plumbing::AsId::as_id`), and `lib.rs` has `#![forbid(unsafe_code)]`. salsa's
> macros expand to `unsafe` from `salsa-macro-rules`, an external macro, which the lint does not
> report; the first build confirms it.

## items-hirty: impl attribute bits in the item scope (format, `hir-def`)

Building `TraitImpls` (`method_resolution.rs`, `collect_def_map`) calls `AttrFlags::query` on
every trait impl of every crate to drop `#[rustc_reservation_impl]` ones, which is a memo
lookup and an edge per impl (std and windows-sys have tens of thousands). Asked: keep the
`ItemScope`'s impls as two CSR lists (trait impls, inherent impls) in place of `(ImplId, bool)`
filtered on each call, with a bitset column beside the trait impls for "reservation impl",
filled when the item tree is collected. Then the collection reads one bit per impl and adds no
edges. Same impls, same order; faster because it drops one query read per impl, the filter
branch, and the edges (docs/v2/plan/design/data.md: flags as bitsets, lists as CSR).

> forwarded to db: `ItemScope` (`hir-def/src/nameres`) is lane db's, not format's. The reader in `method_resolution.rs` `collect_def_map` (now mine) moves to the bitset once it exists.
> applied by db-expand: `ItemScope::impls` is a `ThinVec<ImplId>` in declaration order, beside two bitsets of its length, `trait_impl` and `reservation_impl` (`item_scope.rs` `Bits`, a `u64` word per 64 impls). `impls()`, `trait_impls()` and `inherent_impls()` keep their signatures and declaration order. New `trait_impls_without_reservation()`. The collector sets the bit from `attrs.by_key(sym::rustc_reservation_impl).exists()` on the cfg-expanded attrs, which agrees with `AttrFlags::RUSTC_RESERVATION_IMPL`. It is compared in `PartialEq`, shrunk, encoded and decoded in `nameres/frozen.rs` (`ENCODING` bumped to 8). I kept one ordered list with bitsets, not the two CSR lists, because three readers rely on declaration order. Test `impls_keep_their_order_and_their_reservation_bit` (`nameres/tests.rs`: a plain, a reserved and two `cfg_attr` impls; the frozen bytes round-trip under `Mode::Verify`). What waits: `method_resolution.rs:1655-1670` uses `trait_impls_without_reservation()` and drops its `AttrFlags::query` per impl.

## items-hirty: candidate assembly across crates (db, `next_solver`)

Following my earlier request (`next_solver/solver.rs:235-254`): the solver walks
`TraitImpls::for_crate_and_deps` and does one search per loaded crate for each goal, so a
binary with 300 dependencies makes 300 binary searches per trait goal. The narrow query
`TraitImpls::in_crate_and_deps_for_trait_and_self_ty(krate, trait, self_ty)` answers it with
one memo lookup and gives the same impls in the same order: each crate's impls, then its
derives, crate by crate, which is the order the walk gives today. Asked: use it (and
`blanket_in_crate_and_deps`) there. Faster per goal, and an edit adding an impl of another
trait no longer reruns the goal.

> forwarded to db: `next_solver/solver.rs` is lane db's; the narrow queries it should call are in `method_resolution.rs`.
> checked by db-audit-other (22:39): in the tree. Candidate assembly is in `crates/hir-ty/src/next_solver/interner.rs`: `TraitImpls::in_crate_and_deps_for_trait_and_self_ty` at :1921 and `TraitImpls::blanket_in_crate_and_deps` at :2076. `for_crate_and_deps` is no longer called anywhere under `next_solver/`.

## speed-hir-store: blaze-items onto the HIR column views (from speed-hir-store-tools)

From speed-hir-store-tools, 2026-10-29. 4 sites in 4 files of `crates/blaze-items/src/`, all the
same edit. The rule is "## format-hir: expressions and patterns become columns" in
`v2/requests/format.md` (contract: `~/restart-2026-10-28/briefs/v2/format-hir-columns-api.md`).
These land together with the format-hir change. `store.exprs()` / `body.exprs()` now yields
`(ExprId, Expr<'_>)`, so `Expr::Closure { closure_kind, .. }` binds `ClosureKind` by value. The
closure keys hash a closure's place in `exprs()` order (`key.rs`, `nth`); the columns keep ids
dense and in allocation order, so that order and the key bytes are unchanged. No site builds an
`Expr`, uses the builder, or stores, hashes or compares a view.

Files that import the HIR types and need no change: `facts.rs` (`ExprId` and
`walk_child_exprs` only), `form.rs` (`ClosureKind`, `MirSpan::ExprId`).

> applied: the four sites (`key.rs` `let kind = closure_kind;`, `tests/{facts,form,hashes}.rs` `kind: closure_kind`). They compile with format-hir's `exprs()` yielding `Expr<'_>`.

### crates/blaze-items/src/key.rs  (1 site)
- ~L889  `let kind = *closure_kind;`  ->  `let kind = closure_kind;`

### crates/blaze-items/src/tests/facts.rs  (1 site)
- ~L259  `InternedClosure { owner: owner.into(), expr, kind: *closure_kind },`  ->  `InternedClosure { owner: owner.into(), expr, kind: closure_kind },`

### crates/blaze-items/src/tests/form.rs  (1 site; file of speed-types-form)
- ~L40  `InternedClosure { owner: owner.into(), expr, kind: *closure_kind },`  ->  `InternedClosure { owner: owner.into(), expr, kind: closure_kind },`

### crates/blaze-items/src/tests/hashes.rs  (1 site)
- ~L322  `InternedClosure { owner: owner.into(), expr, kind: *closure_kind },`  ->  `InternedClosure { owner: owner.into(), expr, kind: closure_kind },`

## items-form, for lane items: auto-trait checks of one type in one inference context (`crates/hir-ty/src/traits.rs`)

What: next to `implements_trait_unique`:

```rust
/// For each of `traits`, whether `ty` implements it: what `implements_trait_unique` answers
/// for each, from one interner and one inference context.
pub fn implements_traits_unique<'db>(
    ty: Ty<'db>,
    db: &'db dyn HirDatabase,
    env: ParamEnvAndCrate<'db>,
    traits: &[TraitId],
) -> Vec<bool> {
    let interner = DbInterner::new_with(db, env.krate);
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    traits
        .iter()
        .map(|&trait_| {
            infcx.probe(|_| {
                let args = infcx.fill_rest_fresh_args(Span::Dummy, trait_.into(), [ty.into()]);
                let trait_ref = rustc_type_ir::TraitRef::new_from_args(interner, trait_.into(), args);
                if trait_ref.self_ty().is_ty_error() {
                    return false;
                }
                let obligation =
                    Obligation::new(interner, ObligationCause::dummy(), env.param_env, trait_ref);
                infcx.predicate_must_hold_modulo_regions(&obligation)
            })
        })
        .collect()
}
```

Why faster: `form.rs` `Cx::auto_traits` asks every auto trait of the loaded crates (`Send`,
`Sync`, `Unpin`, `UnwindSafe`, `RefUnwindSafe`, `Freeze` and the crates' own) of each
`impl Trait` hidden type. Today each check builds its own `DbInterner` and `InferCtxt`; this
builds one for all of them. Each check runs in its own `probe`, so nothing one leaves behind
reaches the next, and the answers are the same as the one-at-a-time calls. Once it lands,
`auto_traits_form` calls it once and writes `f.item(..)` and `f.bool(..)` per trait in the
same order, so the form is bit-identical.

> items-hirty, to items-form: landed in `crates/hir-ty/src/traits.rs`, one definition (a second copy added at the same time is removed). Checked against the solver: `probe` restores the universe, every variable table, region constraints, region obligations and assumptions, and opaque types; `evaluate_obligation` measures opaque types and regions against its own inner snapshot; the evaluation context's taint is per root goal, and `InferCtxt::tainted_by_errors` is never read while solving. Same `TypingMode::non_body_analysis()` and `env.param_env` as `implements_trait_unique`, and both go through one `trait_ref_holds`, so they cannot drift. Test: `implements_traits_unique_answers_as_one_at_a_time` (with an auto trait, a negative impl, a generic type and a trait asked twice).
> applied: `hir_ty::traits::implements_traits_unique` as written, its check being the existing `trait_ref_holds` inside `infcx.probe`. `form.rs` `auto_traits_form` calls it once and writes `f.item`, `f.bool` per trait in the same order, so the form is unchanged; `tests/narrow.rs` already compares it with one-at-a-time.

## items-form, for db: the symbol attributes gated on their flags (`crates/hir-def/src/attrs.rs`)

What:
1. `AttrFlags::export_name`, `link_name`, `link_section` and `linkage` return `None`
   without making their tracked query when `AttrFlags::query(db, owner)` lacks
   `HAS_EXPORT_NAME`, `HAS_LINK_NAME`, `HAS_LINK_SECTION` or `HAS_LINKAGE`. That is how
   `lang_item_with_attrs` and `unstable_feature` already work: an outer `#[inline]` fn checks
   the bit and calls the inner tracked fn.
2. `AttrFlags::is_rustc_nounwind` reads `AttrFlags::query(db, owner.into())
   .contains(AttrFlags::RUSTC_NOUNWIND)` instead of walking the attributes. It is not tracked,
   so today every call re-walks the item's attribute syntax.

Why faster: the flags are set by the same attribute pass, with the same one-segment match
(`name_text` and `is1` both strip `r#`), so a missing bit means the query finds nothing. Today
every item that asks, including blaze-items' forms for every fn, static and ADT and the link's
symbol names, creates four memos that hold `None`, and every fn form re-walks its attributes
for `rustc_nounwind`. `form.rs` already gates on the bits itself (`Cx::attrs`). This makes
every other caller cheaper too, with the same answers.

> forwarded to db: `crates/hir-def/src/attrs.rs` is lane db's.
> checked by db-audit-other (22:39): in the tree at `crates/hir-def/src/attrs.rs`. 1: `link_name`, `link_section`, `linkage` and `export_name` return before their tracked query when the bit is missing (:1457, :1525, :1539, :1553). 2: `is_rustc_nounwind` is `AttrFlags::query(..).contains(RUSTC_NOUNWIND)` (:1822-1823).

## speed-types-probe-coherence, for lane items: tests of the coherence and opaques speed pass (`crates/blaze-items/src/tests/`)

What: add `mod coherence;` to `crates/blaze-items/src/tests/mod.rs` (after `mod db;`) and a new
file `crates/blaze-items/src/tests/coherence.rs` with exactly the contents below.

Why: speed-types-probe-coherence changed `crates/hir-ty/src/{specialization,traits,opaques,drop}.rs`
(notes: `docs/v2/plan/lanes/items/speed-types-probe-coherence.md`). hir-ty has no test
database, and blaze-items' `TestDB` is where these files' fixture tests already live
(`narrow.rs`). Each test checks the answers the code gave before the change on a fixture
(blanket, inherent and specializing impls, impls in two crates, aliases with and without
`impl Trait`, enums for `is_freeze`), and where it can, that the skipped work is skipped:
`specializes_query` only for impls whose headers unify, no inference for an alias without
`impl Trait`, no `trait_environment_query` for an orphan check with no alias in its header.

```rust
//! Coherence, specialization, opaque types and `Freeze` over fixtures: each answers as it
//! did before speed-types-probe-coherence's pass
//! (docs/v2/plan/lanes/items/speed-types-probe-coherence.md), and skips what it now skips.

use base_db::{Crate, all_crates};
use hir_def::{
    AdtId, DefWithBodyId, FunctionId, ImplId, ModuleDefId, TypeAliasId,
    nameres::crate_def_map,
    signatures::{
        EnumSignature, FunctionSignature, StructSignature, TypeAliasSignature, UnionSignature,
    },
};
use hir_ty::{
    InferenceResult, check_orphan_rules,
    drop::is_freeze,
    impl_unconstrained_params,
    next_solver::{
        DbInterner, GenericArgs, ParamEnv, Ty, TyKind, TypingMode, infer::DbInternerInferExt,
    },
    tait_hidden_types,
};
use rustc_type_ir::{UintTy, inherent::IntoKind as _};
use test_fixture::WithFixture;

use super::db::TestDB;

/// How many times `query` is among the salsa debug names in `executed`.
fn runs(executed: &[String], query: &str) -> usize {
    executed.iter().filter(|it| it.trim_end_matches('_').ends_with(query)).count()
}

fn crate_named(db: &TestDB, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| {
            let display = &it.extra_data(db).display_name;
            display.as_ref().is_some_and(|it| it.crate_name().as_str() == name)
        })
        .unwrap_or_else(|| panic!("no crate `{name}`"))
}

fn declarations(db: &TestDB, krate: Crate) -> Vec<ModuleDefId> {
    let def_map = crate_def_map(db, krate);
    def_map
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .collect()
}

/// The impls of `krate`'s modules, in the order they are written.
fn impls(db: &TestDB, krate: Crate) -> Vec<ImplId> {
    let def_map = crate_def_map(db, krate);
    def_map
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).impls().collect::<Vec<_>>())
        .collect()
}

fn function_named(db: &TestDB, krate: Crate, name: &str) -> FunctionId {
    declarations(db, krate)
        .into_iter()
        .find_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no function `{name}`"))
}

fn type_alias_named(db: &TestDB, krate: Crate, name: &str) -> TypeAliasId {
    declarations(db, krate)
        .into_iter()
        .find_map(|def| match def {
            ModuleDefId::TypeAliasId(it)
                if TypeAliasSignature::of(db, it).name.as_str() == name =>
            {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no type alias `{name}`"))
}

fn adt_named(db: &TestDB, krate: Crate, name: &str) -> AdtId {
    declarations(db, krate)
        .into_iter()
        .find_map(|def| match def {
            ModuleDefId::AdtId(adt) => {
                let adt_name = match adt {
                    AdtId::StructId(it) => &StructSignature::of(db, it).name,
                    AdtId::EnumId(it) => &EnumSignature::of(db, it).name,
                    AdtId::UnionId(it) => &UnionSignature::of(db, it).name,
                };
                (adt_name.as_str() == name).then_some(adt)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no type `{name}`"))
}

fn has_type_mismatches(db: &TestDB, function: FunctionId) -> bool {
    hir_ty::attach_db(db, || {
        InferenceResult::of(db, DefWithBodyId::from(function)).has_type_mismatches()
    })
}

const UNCONSTRAINED: &str = r#"
pub trait It { type Item; }
pub trait Tr {}
pub struct W<T>(T);
pub struct S;
impl<T, U> Tr for W<T> where T: It<Item = U> {}
impl<T, U, V> Tr for (T,) where U: It<Item = V>, T: It<Item = U> {}
impl<T, U> Tr for S {}
impl<T: It<Item = U>, U> W<T> {}
impl<T, U> It for W<T> where W<T>: It<Item = U> { type Item = (); }
impl<const N: usize> Tr for [u8; 1] {}
impl<'a, T> Tr for &'a [T] {}
"#;

/// A projection constrains its value once what it names is, over as many rounds as a chain
/// takes (`V` after `U`, written first); one of the impl's own trait ref never does. The
/// unconstrained parameters come in declaration order, const ones too, lifetimes never.
#[test]
fn unconstrained_params_are_as_before_in_declaration_order() {
    let (db, _) = TestDB::with_single_file(&format!("//- /main.rs crate:main\n{UNCONSTRAINED}"));
    let main = crate_named(&db, "main");
    let found: Vec<Vec<u32>> = hir_ty::attach_db(&db, || {
        impls(&db, main)
            .into_iter()
            .map(|impl_| {
                impl_unconstrained_params(&db, impl_)
                    .into_iter()
                    .map(|param| u32::from(param.local_id.into_raw()))
                    .collect()
            })
            .collect()
    });
    let expected: [&[u32]; 7] = [&[], &[], &[0, 1], &[], &[1], &[0], &[]];
    assert_eq!(found, expected.map(<[u32]>::to_vec));
}

const SPECIALIZATION: &str = r#"//- /dep.rs crate:dep
#![feature(specialization)]
pub trait Tr { type A; }
impl<T> Tr for T { default type A = u8; }
//- /main.rs crate:main deps:dep
#![feature(specialization)]
use dep::Tr;
pub struct S;
pub struct O;
impl Tr for S {}
impl Tr for O { type A = u16; }
impl Tr for u32 { type A = u32; }
pub fn good(x: <S as Tr>::A) -> u8 { x }
pub fn bad(x: <S as Tr>::A) -> u16 { x }
"#;

/// `impl Tr for S` leaves `A` out and takes it from the blanket impl of another crate, as
/// before. The impls for `O` and `u32` define `A` too, but their headers cannot unify with
/// `S`'s, so no `specializes_query` is made for them: at most the pair `S`, blanket and its
/// reverse, where every impl that defines `A` (four queries) was asked before.
#[test]
fn a_specializing_impl_asks_only_the_impls_its_header_unifies_with() {
    let (db, _) = TestDB::with_many_files(SPECIALIZATION);
    let main = crate_named(&db, "main");
    let good = function_named(&db, main, "good");
    let bad = function_named(&db, main, "bad");
    let executed = db.executed(|| {
        assert!(!has_type_mismatches(&db, good), "`<S as Tr>::A` is `u8`");
        assert!(has_type_mismatches(&db, bad), "`<S as Tr>::A` is no `u16`");
    });
    assert!(runs(&executed, "specializes_query") <= 2, "{executed:#?}");
}

const TAITS: &str = r#"
#![feature(type_alias_impl_trait)]
pub trait Tr {}
impl Tr for u32 {}
pub type Plain = u32;
pub type Opaque = impl Tr;
#[define_opaque(Plain)]
pub fn plain() -> Plain { 0 }
#[define_opaque(Opaque)]
pub fn opaque() -> Opaque { 0u32 }
"#;

/// An alias with no `impl Trait` has no hidden types, as before, and its defining function
/// is no longer inferred to find that out.
#[test]
fn an_alias_without_impl_trait_infers_no_body() {
    let (db, _) = TestDB::with_single_file(&format!("//- /main.rs crate:main\n{TAITS}"));
    let plain = type_alias_named(&db, crate_named(&db, "main"), "Plain");
    let executed = db.executed(|| {
        hir_ty::attach_db(&db, || assert_eq!(tait_hidden_types(&db, plain).values().count(), 0))
    });
    assert_eq!(runs(&executed, "infer_body_query"), 0, "{executed:#?}");
}

#[test]
fn an_alias_with_impl_trait_takes_its_hidden_type_from_its_defining_body() {
    let (db, _) = TestDB::with_single_file(&format!("//- /main.rs crate:main\n{TAITS}"));
    let opaque = type_alias_named(&db, crate_named(&db, "main"), "Opaque");
    let executed = db.executed(|| {
        hir_ty::attach_db(&db, || {
            let hidden: Vec<_> = tait_hidden_types(&db, opaque)
                .values()
                .map(|it| it.get().skip_binder())
                .collect();
            assert_eq!(hidden.len(), 1, "{hidden:?}");
            assert!(matches!(hidden[0].kind(), TyKind::Uint(UintTy::U32)), "{hidden:?}");
        })
    });
    assert!(runs(&executed, "infer_body_query") >= 1, "{executed:#?}");
}

const ORPHAN: &str = r#"//- /dep.rs crate:dep
pub trait Foreign<T> {}
pub trait Proj { type Out; }
//- /main.rs crate:main deps:dep
pub struct Local;
impl dep::Proj for Local { type Out = Local; }
impl dep::Foreign<<Local as dep::Proj>::Out> for u32 {}
impl dep::Foreign<Local> for u8 {}
impl dep::Foreign<u8> for u16 {}
"#;

/// A header with an alias is normalized before the orphan rules look at it (the projection
/// is `Local`), as before; one without is looked at as written, with no parameter
/// environment made for it.
#[test]
fn orphan_rules_normalize_only_a_header_with_an_alias() {
    let (db, _) = TestDB::with_many_files(ORPHAN);
    let impls = impls(&db, crate_named(&db, "main"));
    let answers: Vec<bool> =
        hir_ty::attach_db(&db, || impls.iter().map(|&it| check_orphan_rules(&db, it)).collect());
    assert_eq!(answers, [true, true, true, false]);

    let (db, _) = TestDB::with_many_files(ORPHAN);
    let impls = self::impls(&db, crate_named(&db, "main"));
    let plain = db.executed(|| hir_ty::attach_db(&db, || assert!(check_orphan_rules(&db, impls[2]))));
    assert_eq!(runs(&plain, "trait_environment_query"), 0, "{plain:#?}");
    let alias = db.executed(|| hir_ty::attach_db(&db, || assert!(check_orphan_rules(&db, impls[1]))));
    assert!(runs(&alias, "trait_environment_query") >= 1, "{alias:#?}");
}

const FREEZE: &str = r#"
#[lang = "unsafe_cell"]
pub struct UnsafeCell<T>(T);
pub struct Cell<T>(UnsafeCell<T>);
pub enum Plain { A(u8), B { x: u32 }, C }
pub enum Shared { A(u8), B(Cell<u8>) }
pub struct Wrap(Plain, Shared);
pub union U { a: u8 }
pub struct Empty;
pub enum Never {}
"#;

/// `Freeze` walks every variant's fields in place: an enum is `Freeze` when each variant is.
#[test]
fn is_freeze_walks_every_variant() {
    let (db, _) = TestDB::with_single_file(&format!("//- /main.rs crate:main\n{FREEZE}"));
    let main = crate_named(&db, "main");
    let names = ["UnsafeCell", "Cell", "Plain", "Shared", "Wrap", "U", "Empty", "Never"];
    let adts = names.map(|name| adt_named(&db, main, name));
    let answers = hir_ty::attach_db(&db, || {
        let interner = DbInterner::new_with(&db, main);
        let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
        adts.map(|adt| {
            let args = GenericArgs::identity_for_item(interner, adt.into());
            is_freeze(&infcx, Ty::new_adt(interner, adt, args), ParamEnv::empty())
        })
    });
    assert_eq!(answers, [false, false, true, false, false, false, true, true]);
}
```

> applied (items): `crates/blaze-items/src/tests/coherence.rs` with the contents above, and `mod coherence;` in `tests/mod.rs` (before `mod db;`, in order).

## speed-backend-sanitize, for lane items: a function's effective `#[sanitize]` set in its form (`crates/blaze-items/src/form.rs`)

What: where the form hashes a function's `AttrFlags` (`form.rs:832`), also hash
`hir_def::attrs::disabled_sanitizers(db, func)` (requested of db, `v2/requests/db.md`, same
date) as one byte. It is inherited from the impl, module and crate, so it changes when a
parent's `#[sanitize(..)]` does, with no change to the function's own attributes.
Why: the attributes LLVM gets differ with it (S2 of `v2/requests/codegen.md`), so the code hash
of the function, and with it the unit key, must too.

> written by speed-backend-sanitize (2026-10-29); told items by tmux.

> applied (items): `f.u8(hir_def::attrs::disabled_sanitizers(db, func))` after the target
> features in `FormCx::function` (`crates/blaze-items/src/form.rs`). It needs db's query
> (`v2/requests/db.md`, "From speed-backend-sanitize"), which isn't in `hir-def` yet.
> forwarded to items (told by speed-backend-sanitize).

## items, for db-ty, codegen, safety and requests: the layout queries return interned layouts (fastcompile-ident-hashcons 5)

What items did (`crates/hir-ty/src/layout.rs`, `layout/adt.rs`):

- `pub struct InternedLayout(pub Layout)`, with `Deref<Target = Layout>`, `AsRef<Layout>` and
  `blaze_hashcons::Consed` (domain `"blaze v2 layout 1"`, content: `Layout`'s derived `Hash`).
- One `static LAYOUTS: LazyLock<HashCons<InternedLayout>>`, never freed.
- `pub fn intern_layout(Layout) -> Interned<'static, InternedLayout>`, for code that makes a
  layout of its own.
- `layout_of_ty_query` and `layout_of_adt_query` return
  `Result<Interned<'static, InternedLayout>, LayoutError>` (still `returns(clone)`; a copy is
  one pointer).

Reads like `layout.size`, `layout.variants` and `&layout` (as `&Layout`) work unchanged
through the two `Deref`s. Only code that names `Arc<Layout>` changes. With
`use blaze_hashcons::Interned;` and `use hir_ty::layout::{InternedLayout, intern_layout};`:

1. **db-ty** (`v2/requests/db-ty.md`):
   - `crates/hir-ty/Cargo.toml`: `blaze-hashcons.workspace = true` under `# local deps`. It is
     already in the root `[workspace.dependencies]`, and depends only on `blaze-format` and
     `blaze-ids`.
   - `crates/hir-ty/src/db.rs`: `fn layout_of_adt` and `fn layout_of_ty` (lines 259-276)
     return `Result<Interned<'static, InternedLayout>, LayoutError>`. Import `InternedLayout`
     beside `Layout` at line 30, and drop `Arc` there if nothing else uses it.
   - `crates/hir/src/lib.rs`:
     - `pub struct Layout<'db>(Arc<TyLayout>, &'db TargetDataLayout)` (line 6069) holds
       `Interned<'static, InternedLayout>`;
     - `Variant::layout` (line 1195) makes `intern_layout(TyLayout::for_variant(&parent_layout.0, ..))`
       in place of `Arc::new(..)`;
     - the `.0` reads stay as they are.
2. **codegen** (`crates/hir-ty-mir/`, `crates/hir-ty-codegen/`):
   - `hir-ty-mir/src/mir/eval.rs`:
     - `layout_cache` (line 279) maps to `Interned<'static, InternedLayout>`;
     - `fn layout` and `fn layout_adt` (lines 813 and 825) return it;
     - the variant tuple at line 1999 holds it, and line 2037 makes `intern_layout(variant_layout)`.
   - `hir-ty-mir/src/mir/eval/shim.rs:213`: `layout: Interned<'static, InternedLayout>`.
   - `hir-ty-mir/src/mir/transform.rs:113`: `layout_of` returns
     `Option<Interned<'static, InternedLayout>>`.
   - `hir-ty-codegen/sve.rs:174`: `storage` takes and returns `Interned<'static, InternedLayout>`:
     `let mut storage = layout.0.clone(); ..; intern_layout(storage)`.
   - `hir-ty-codegen/mod.rs`:
     - in `fn layout` (line 8665), `let layout: &'cx Layout = &sve::storage(layout).get().0;`;
     - `KeptLayouts::asked` (line 8355) and its test (line 19862) go: a `&'static Layout`
       needs no arena to outlive `build`.
3. **safety** (`crates/hir-ty-ub/src/check/valid.rs`): `layouts` (line 267) and `fn layout`
   (line 857) use `Interned<'static, InternedLayout>` in place of `Arc<Layout>`.
4. **requests** (`crates/hir-ide/src/tests/layout.rs`): `eval_goal` and `eval_expr` (lines 41
   and 112) return `Result<Interned<'static, InternedLayout>, LayoutError>`.

No other caller names the type: `rg 'Arc<Layout>|Arc<crate::layout::Layout>|Arc<TyLayout>' crates`
gives the lines above and nothing else.

Why: `v2/requests/fastcompile.md`, fastcompile-ident-hashcons 5.
- A layout read clones an `Arc` today: a shared counter write on the read path, which
  `docs/v2/plan/design/concurrency.md` rules out.
- Interned, a read is a pointer copy and equality is a pointer compare.
- Equal layouts are kept once, and salsa's fingerprint of the value hashes 16 bytes computed
  at intern time.

> written by items (2026-10-29).
> applied (`requests'` part 4): `crates/hir-ide/src/tests/layout.rs` `eval_goal`/`eval_expr` return `Result<Interned<'static, InternedLayout>, LayoutError>` (`use blaze_hashcons::Interned`, `crate::layout::InternedLayout`; `triomphe::Arc` and `Layout` imports gone); `crates/hir-ide/Cargo.toml` dev-dependency `blaze-hashcons.workspace = true`. Parts 1-3 are db's, codegen's and safety's (items wrote them to each).
> applied (requests' part, item 4): `crates/hir-ide/src/tests/layout.rs` already returns `Interned<'static, InternedLayout>` from `eval_goal` and `eval_expr`, with `blaze_hashcons::Interned` imported (written alongside items). > forwarded to db (1), codegen (2), safety (3).
> applied by db-ty-next1 (part 1): `hir-ty/Cargo.toml` and `hir/Cargo.toml` gain `blaze-hashcons.workspace = true` (hir names `Interned` too; `hir-ty`'s `layout.rs` already used it); `hir-ty/src/db.rs` `layout_of_adt`/`layout_of_ty` return `Result<Interned<'static, InternedLayout>, LayoutError>` (`triomphe::Arc` import gone, `Layout` import now `InternedLayout`); `hir/src/lib.rs` `Layout<'db>` holds `Interned<'static, InternedLayout>`, `Variant::layout` makes `intern_layout(TyLayout::for_variant(..))`, `triomphe::Arc` import gone (no other use).
> checked by db (db-next2, 2026-10-29 22:28): part 1 is in the tree (`hir-ty/src/db.rs:307`, `:772`; `hir/src/lib.rs:6373`, `:1253`); no `Arc<Layout>`/`Arc<TyLayout>` left in hir or hir-ty. Not compiled.

## frontend-nameres-impls: per-module impl collection (2026-10-29)

> held by requests (items is DONE, so its paths and hir-ide are requests'): parts 1 to 4 go in together once `mod facts;` and db's `pub use self::facts::impls::{ModuleImpls, module_impls};` are in `hir-def/src/nameres.rs` and db-ty's nine kinds are in `hir-ty/src/db.rs`; neither is in the tree yet (checked pass 12).

> concrete database: frontend-db ran `tools/v2/concrete_db.py` on `method_resolution.rs` and
> `method_resolution/`; the old text below is refreshed to it and checked verbatim against
> every file (2026-10-29 20:13), and the tests use `Db::log_executed`. Both files already
> import `base_db::Db`, so the request adds no import for it.

What: hir-ty's impl tables and blaze-items' indexes built per module from
`hir_def::nameres::module_impls` (frontend.md items 2 and 3), so a bucket reruns only when a
module's impls in it change, and `in_crate_and_deps_for_trait_and_self_ty` reads each crate's
own `(crate, trait)` bucket instead of every crate's whole table through `for_crate_and_deps`.
Lane items is DONE (no window), so this is for `requests` to apply (paths of a DONE lane).
Parts 1 and 2 land together with:
- db's `## frontend-nameres-impls: re-export module_impls` in `v2/requests/db.md` (and core's
  `mod facts;`): the `hir_def::nameres::module_impls` path;
- db-ty's `## frontend-nameres-impls: kinds for the per-module impl tables` in
  `v2/requests/db-ty.md`: `db.rs`'s `every_query_has_a_kind` fails on the nine new tracked
  methods without it.
Line numbers are from 2026-10-29 20:30; every old text below was read there.

Why now: today `TraitImpls::for_crate` and `InherentImpls::for_crate` walk
`crate_def_map(krate).modules(db)` and every module's whole scope, so any item added anywhere
reruns them, and every narrow memo above (`in_crate_for_self_ty`,
`in_crate_for_trait_and_self_ty`, `in_crate_and_deps_for_trait_and_self_ty` through
`for_crate_and_deps`, `blanket_in_crate_and_deps`, blaze-items' `trait_impls_in`,
`inherent_impls_in`) reruns for every impl added anywhere in the crate or a dependency.

The layers after this change, each a memo that backdates when equal:

| memo | key | reruns when |
|---|---|---|
| `module_impls` (hir-def) | module | the module's scope changes; equal unless an impl comes or goes |
| `TraitImpls::for_module`, `InherentImpls::for_module` | module | its `module_impls` or the impls in its bodies' blocks change |
| `TraitImpls::modules_by_trait`, `InherentImpls::modules_by_self_ty` | crate | a module's table changes; equal unless a module gains its first or loses its last impl of a trait / for a type |
| `TraitImpls::modules_of_trait`, `InherentImpls::modules_of_self_ty` | crate, trait / type | only that trait's / type's module list changes |
| `TraitImpls::in_module_for_trait`, `InherentImpls::in_module_for_self_ty` | module, trait / type | that module's impls of the trait / for the type |
| `TraitImpls::for_crate_and_trait` | crate, trait | the trait's modules or one of their impls of it |
| narrow memos (`in_crate_*`, `blanket_in_crate_and_deps`) | as today | the bucket they read |
| `for_crate` (both) | crate | some module's table changes (no longer any item) |

A frozen crate (`frozen::served`) keeps its one frozen table: its buckets read `for_crate`,
which never changes. `for_crate` merges the module tables in module order, so every run holds
its impls in the order the old walk found them (same order of modules, same order inside each
module, blocks of bodies inside their module), and `frozen::encode`/`check` see the same table.

> taken by requests (pass 22): the preconditions are in the tree (`mod facts;`, `module_impls` re-export, db-ty nine kinds); child `requests-impls` applies parts 1 to 4 and writes its outcome here.
> applied by requests-impls (2026-10-29): parts 1 to 4 in, against today's files and hir-def's real `module_impls(db, ModuleId) -> &ModuleImpls` (fields `trait_impls`, `inherent_impls`, `builtin_derive_impls`, as guessed). `for_crate` (both) merges the per-module tables in `crate_def_map(..).modules(db)` order; frozen crates keep the one frozen table and their buckets read it; `for_crate_and_deps` stays for the solver's whole-table walks. db-ty's nine kinds match the new names. Not built; report /tmp/blaze-lanes/v2-requests-impls.md.

### 1. `crates/hir-ty/src/method_resolution.rs`

1a. Import (L35). Old:

```rust
    nameres::{DefMap, block_def_map, crate_def_map},
```

New:

```rust
    nameres::{DefMap, block_def_map, crate_def_map, module_impls},
```

1b. `InherentImpls::for_crate` and `in_crate_for_self_ty` (L1339-1353). Old:

```rust
        let crate_def_map = crate_def_map(db, krate);

        Self::collect_def_map(db, crate_def_map)
    }

    /// The inherent impls of `krate` for one type. An impl added for another type changes
    /// `for_crate()`, and leaves this as it was.
    #[salsa::tracked(returns(deref))]
    pub fn in_crate_for_self_ty(
        db: &'db Db,
        krate: Crate,
        self_ty: SelfTyKey<'db>,
    ) -> Box<[ImplId]> {
        Self::for_crate(db, krate).for_self_ty(&self_ty.0).into()
    }
```

New:

```rust
        let mut groups = SelfTyGroups::default();
        for (module, _) in crate_def_map(db, krate).modules(db) {
            Self::for_module(db, module).merge_into(&mut groups);
        }
        Self::from_groups(groups)
    }

    /// The inherent impls one module of a crate or block def map gives its table: its own
    /// ([`module_impls`]) and those of the blocks in the bodies it holds, in the order
    /// [`Self::for_crate`] holds them. Reruns when the module's scope changes, and is equal
    /// unless one of those impls is added, removed or changes its self type.
    #[salsa::tracked(returns(ref))]
    pub fn for_module(db: &'db Db, module: ModuleId) -> InherentImpls<'db> {
        let mut groups = SelfTyGroups::default();
        Self::collect_module(db, module, &mut groups);
        Self::from_groups(groups)
    }

    /// The modules of `krate` with an inherent impl for each simplified self type, in module
    /// order. Equal unless a module gains its first impl for a type or loses its last.
    #[salsa::tracked(returns(ref))]
    fn modules_by_self_ty(db: &'db Db, krate: Crate) -> SelfTyModules<'db> {
        let mut groups: SelfTyGroups<'db, Vec<ModuleId>> = SelfTyGroups::default();
        for (module, _) in crate_def_map(db, krate).modules(db) {
            for (self_ty, _) in Self::for_module(db, module).iter() {
                groups.entry(*self_ty).push(module);
            }
        }
        SelfTyModules::new(groups)
    }

    /// The modules of `krate` with an inherent impl for one type: equal unless that list
    /// changes, whatever other types gain.
    #[salsa::tracked(returns(deref))]
    fn modules_of_self_ty(
        db: &'db Db,
        krate: Crate,
        self_ty: SelfTyKey<'db>,
    ) -> Box<[ModuleId]> {
        Self::modules_by_self_ty(db, krate).of(&self_ty.0).into()
    }

    /// The inherent impls of `module` for one type: equal unless one for that type is added
    /// there or removed.
    #[salsa::tracked(returns(deref))]
    fn in_module_for_self_ty(
        db: &'db Db,
        module: ModuleId,
        self_ty: SelfTyKey<'db>,
    ) -> Box<[ImplId]> {
        Self::for_module(db, module).for_self_ty(&self_ty.0).into()
    }

    /// The inherent impls of `krate` for one type, in the order [`Self::for_crate`] holds
    /// them. Reads only the modules with impls for the type, so an impl added for another type,
    /// or anything else added anywhere, leaves this as it was. A frozen crate's are read from
    /// its frozen table.
    #[salsa::tracked(returns(deref))]
    pub fn in_crate_for_self_ty(
        db: &'db Db,
        krate: Crate,
        self_ty: SelfTyKey<'db>,
    ) -> Box<[ImplId]> {
        if frozen::served(db, krate) {
            return Self::for_crate(db, krate).for_self_ty(&self_ty.0).into();
        }
        let mut found = Vec::new();
        for &module in Self::modules_of_self_ty(db, krate, self_ty) {
            found.extend_from_slice(Self::in_module_for_self_ty(db, module, self_ty));
        }
        found.into()
    }
```

1c. `InherentImpls::collect_def_map` (L1381-1411). Old:

```rust
impl<'db> InherentImpls<'db> {
    fn collect_def_map(db: &'db Db, def_map: &'db DefMap) -> Self {
        let mut map = SelfTyGroups::default();
        collect(db, def_map, &mut map);
        return Self::from_groups(map);

        fn collect<'db>(
            db: &'db Db,
            def_map: &DefMap,
            map: &mut SelfTyGroups<'db, Vec<ImplId>>,
        ) {
            for (_module_id, module_data) in def_map.modules(db) {
                for impl_id in module_data.scope(db).inherent_impls() {
                    let interner = DbInterner::new_no_crate(db);
                    let self_ty = db.impl_self_ty(impl_id);
                    let self_ty = self_ty.instantiate_identity().skip_norm_wip();
                    if let Some(self_ty) =
                        simplify_type(interner, self_ty, TreatParams::InstantiateWithInfer)
                    {
                        map.entry(self_ty).push(impl_id);
                    }
                }

                for owner in impl_holders(db, def_map.krate(), module_data.scope(db)) {
                    for (_, block_def_map) in Body::of(db, owner).blocks(db) {
                        collect(db, block_def_map, map);
                    }
                }
            }
        }
    }
```

New:

```rust
impl<'db> InherentImpls<'db> {
    fn collect_def_map(db: &'db Db, def_map: &'db DefMap) -> Self {
        let mut map = SelfTyGroups::default();
        for (module, _) in def_map.modules(db) {
            Self::collect_module(db, module, &mut map);
        }
        Self::from_groups(map)
    }

    /// Adds the inherent impls of `module` and of the blocks in the bodies it holds to `map`.
    fn collect_module(
        db: &'db Db,
        module: ModuleId,
        map: &mut SelfTyGroups<'db, Vec<ImplId>>,
    ) {
        let interner = DbInterner::new_no_crate(db);
        for &impl_id in module_impls(db, module).inherent_impls.iter() {
            let self_ty = db.impl_self_ty(impl_id);
            let self_ty = self_ty.instantiate_identity().skip_norm_wip();
            if let Some(self_ty) =
                simplify_type(interner, self_ty, TreatParams::InstantiateWithInfer)
            {
                map.entry(self_ty).push(impl_id);
            }
        }

        let scope = module.def_map(db).module(db, module).scope(db);
        for owner in impl_holders(db, module.krate(db), scope) {
            for (_, block_def_map) in Body::of(db, owner).blocks(db) {
                for (block_module, _) in block_def_map.modules(db) {
                    Self::collect_module(db, block_module, map);
                }
            }
        }
    }

    /// Adds every impl of this table to `groups`, each type's after those already there.
    fn merge_into(&self, groups: &mut SelfTyGroups<'db, Vec<ImplId>>) {
        for (self_ty, impls) in self.iter() {
            groups.entry(*self_ty).extend_from_slice(impls);
        }
    }
```

(`from_groups`, `for_self_ty`, `iter`, `for_each_crate_and_block` after it stay.)

1d. Two index types, before `OneTraitImplsBuilder` (L1444). Old:

```rust
#[derive(Default)]
struct OneTraitImplsBuilder<'db> {
```

New:

```rust
/// Modules by simplified self type, as columns: the self types sorted by hash, each with its
/// run of modules in module order.
#[derive(Debug, PartialEq, Eq, SalsaValue)]
struct SelfTyModules<'db> {
    // SAFETY: as for `InherentImpls::self_tys`: salsa may keep this column past `'db` only if
    // nothing in it borrows from the database. `SelfTys<'db>` holds `u64`s and
    // `SimplifiedType<'db>`s, which are `fast_reject::SimplifiedType<SolverDefId<'db>>`
    // (`next_solver/ty.rs`), whose variants (`vendor/rustc_type_ir/src/fast_reject.rs`) hold
    // only plain data and `SolverDefId<'db>`, which is `SalsaValue`; no `&'db`.
    #[salsa_value(unsafe(prove(SolverDefId<'db>: SalsaValue)))]
    self_tys: SelfTys<'db>,
    modules: Runs<ModuleId>,
}

impl<'db> SelfTyModules<'db> {
    fn new(groups: SelfTyGroups<'db, Vec<ModuleId>>) -> Self {
        let groups = groups.into_sorted();
        let modules = Runs::new(groups.iter().map(|(_, modules)| &**modules));
        let self_tys = SelfTys::new(groups.into_iter().map(|(self_ty, _)| self_ty).collect());
        SelfTyModules { self_tys, modules }
    }

    fn of(&self, self_ty: &SimplifiedType<'db>) -> &[ModuleId] {
        match self.self_tys.find(0..self.self_tys.len(), self_ty) {
            Some(i) => self.modules.run(i),
            None => &[],
        }
    }
}

/// Modules by trait, as columns: the traits sorted as [`TraitImpls`] sorts them, each with its
/// run of modules in module order.
#[derive(Debug, PartialEq, Eq)]
struct TraitModules {
    traits: Box<[TraitId]>,
    modules: Runs<ModuleId>,
}

impl TraitModules {
    fn new(by_trait: FxHashMap<TraitId, Vec<ModuleId>>) -> Self {
        let mut rows = by_trait.into_iter().collect::<Vec<_>>();
        radix::sort_by_key(&mut rows, |&(trait_, _)| {
            let id = trait_.as_id();
            radix::pack_32(id.index(), id.generation())
        });
        let modules = Runs::new(rows.iter().map(|(_, modules)| &**modules));
        let traits = rows.into_iter().map(|(trait_, _)| trait_).collect();
        TraitModules { traits, modules }
    }

    fn of(&self, trait_: TraitId) -> &[ModuleId] {
        match self.traits.binary_search(&trait_) {
            Ok(t) => self.modules.run(t),
            Err(_) => &[],
        }
    }
}

#[derive(Default)]
struct OneTraitImplsBuilder<'db> {
```

1e. `TraitImpls::for_crate`'s body (L1538-1541). Old:

```rust
        let crate_def_map = crate_def_map(db, krate);
        let result = Self::collect_def_map(db, crate_def_map);
        Arc::new(result)
    }
```

New:

```rust
        let mut map = FxHashMap::default();
        for (module, _) in crate_def_map(db, krate).modules(db) {
            Self::for_module(db, module).merge_into(&mut map);
        }
        Arc::new(Self::from_traits(map))
    }

    /// The trait impls one module of a crate or block def map gives its tables: its own
    /// ([`module_impls`]) and those of the blocks in the bodies it holds, in the order
    /// [`Self::for_crate`] holds them. Reruns when the module's scope changes, and is equal
    /// unless one of those impls is added, removed or changes its trait or self type.
    #[salsa::tracked(returns(ref))]
    pub fn for_module(db: &'db Db, module: ModuleId) -> TraitImpls<'db> {
        let lang_items = hir_def::lang_item::lang_items(db, module.krate(db));
        let mut map = FxHashMap::default();
        Self::collect_module(db, module, lang_items, &mut map);
        Self::from_traits(map)
    }

    /// The modules of `krate` with an impl of each trait, in module order. Equal unless a
    /// module gains its first impl of a trait or loses its last.
    #[salsa::tracked(returns(ref))]
    fn modules_by_trait(db: &'db Db, krate: Crate) -> TraitModules {
        let mut by_trait: FxHashMap<TraitId, Vec<ModuleId>> = FxHashMap::default();
        for (module, _) in crate_def_map(db, krate).modules(db) {
            for &trait_ in Self::for_module(db, module).traits.iter() {
                by_trait.entry(trait_).or_default().push(module);
            }
        }
        TraitModules::new(by_trait)
    }

    /// The modules of `krate` with an impl of `trait_`: equal unless that list changes,
    /// whatever other traits gain.
    #[salsa::tracked(returns(deref))]
    fn modules_of_trait(db: &'db Db, krate: Crate, trait_: TraitId) -> Box<[ModuleId]> {
        Self::modules_by_trait(db, krate).of(trait_).into()
    }

    /// The impls of `trait_` in `module`, as a table of that trait alone: equal unless one of
    /// them is added, removed or changes its self type.
    #[salsa::tracked(returns(ref))]
    fn in_module_for_trait(
        db: &'db Db,
        module: ModuleId,
        trait_: TraitId,
    ) -> TraitImpls<'db> {
        let mut map = FxHashMap::default();
        Self::for_module(db, module).merge_trait_into(trait_, &mut map);
        Self::from_traits(map)
    }

    /// The impls of `trait_` in `krate`, as a table of that trait alone, in the order
    /// [`Self::for_crate`] holds them: the bucket every narrow memo of one crate reads. Reads
    /// only the modules with impls of `trait_`, so an impl of another trait, or anything else
    /// added anywhere, leaves this as it was. A frozen crate's is read from its frozen table.
    #[salsa::tracked(returns(ref))]
    pub fn for_crate_and_trait(
        db: &'db Db,
        krate: Crate,
        trait_: TraitId,
    ) -> TraitImpls<'db> {
        let mut map = FxHashMap::default();
        if frozen::served(db, krate) {
            Self::for_crate(db, krate).merge_trait_into(trait_, &mut map);
        } else {
            for &module in Self::modules_of_trait(db, krate, trait_) {
                Self::in_module_for_trait(db, module, trait_).merge_trait_into(trait_, &mut map);
            }
        }
        Self::from_traits(map)
    }
```

1f. `in_crate_and_deps_for_trait_and_self_ty` (L1560-1576): one memo per (crate, trait, self
type) reading each loaded crate's own bucket. Old:

```rust
    /// The impls of one trait for one type in `krate` and its dependencies. An impl added for
    /// another trait or type changes `for_crate()`, and leaves this as it was.
    #[salsa::tracked(returns(deref))]
    pub fn in_crate_and_deps_for_trait_and_self_ty(
        db: &'db Db,
        krate: Crate,
        trait_: TraitId,
        self_ty: SelfTyKey<'db>,
    ) -> Box<[AnyImplId]> {
        let mut found = Vec::new();
        for impls in Self::for_crate_and_deps(db, krate) {
            let (impls, builtin_derive_impls) = impls.for_trait_and_self_ty(trait_, &self_ty.0);
```

New:

```rust
    /// The impls of one trait for one type in `krate` and its dependencies, in the order of
    /// [`loaded_crates`]. Reads each crate's [`Self::for_crate_and_trait`], so an impl of
    /// another trait anywhere leaves this unvalidated-equal, and one of `trait_` for another
    /// type reruns it equal.
    #[salsa::tracked(returns(deref))]
    pub fn in_crate_and_deps_for_trait_and_self_ty(
        db: &'db Db,
        krate: Crate,
        trait_: TraitId,
        self_ty: SelfTyKey<'db>,
    ) -> Box<[AnyImplId]> {
        let mut found = Vec::new();
        for &dep in loaded_crates(db, krate) {
            let impls = Self::for_crate_and_trait(db, dep, trait_);
            let (impls, builtin_derive_impls) = impls.for_trait_and_self_ty(trait_, &self_ty.0);
```

(the three lines after it stay.)

1g. `blanket_in_crate_and_deps` (L1585-1589). Old:

```rust
        Self::for_crate_and_deps(db, krate)
            .iter()
            .flat_map(|impls| impls.blanket_impls(trait_))
            .copied()
            .collect()
```

New:

```rust
        loaded_crates(db, krate)
            .iter()
            .flat_map(|&dep| Self::for_crate_and_trait(db, dep, trait_).blanket_impls(trait_))
            .copied()
            .collect()
```

1h. `in_crate_for_trait_and_self_ty` (L1600). Old:

```rust
        Self::for_crate(db, krate).for_trait_and_self_ty(trait_, &self_ty.0).0.into()
```

New:

```rust
        Self::for_crate_and_trait(db, krate, trait_).for_trait_and_self_ty(trait_, &self_ty.0).0.into()
```

1i. `in_crate_for_trait` (L1626) and the doc of `in_crate_and_deps_for_trait` (L1634-1636).
Old:

```rust
        Self::for_crate(db, krate).for_trait(trait_, |impls| {
```

New:

```rust
        Self::for_crate_and_trait(db, krate, trait_).for_trait(trait_, |impls| {
```

Old:

```rust
    /// The impls of one trait in `krate` and its dependencies, each crate's as
    /// [`Self::in_crate_for_trait`]. An impl of another trait changes `for_crate()` and
    /// `in_crate_for_trait()` reruns equal, so this does not rerun.
```

New:

```rust
    /// The impls of one trait in `krate` and its dependencies, each crate's as
    /// [`Self::in_crate_for_trait`]. An impl of another trait leaves every crate's
    /// [`Self::for_crate_and_trait`] for this one as it was, so this does not rerun.
```

1j. `TraitImpls::collect_def_map` (L1651-1711). Old: from

```rust
impl<'db> TraitImpls<'db> {
    fn collect_def_map(db: &'db Db, def_map: &DefMap) -> Self {
        let lang_items = hir_def::lang_item::lang_items(db, def_map.krate());
        let mut map = FxHashMap::default();
        collect(db, def_map, lang_items, &mut map);
        return Self::from_traits(map);
```

through the end of the nested `fn collect` and of `collect_def_map`:

```rust
                for owner in impl_holders(db, def_map.krate(), module_data.scope(db)) {
                    for (_, block_def_map) in Body::of(db, owner).blocks(db) {
                        collect(db, block_def_map, lang_items, map);
                    }
                }
            }
        }
    }
```

New (the impl bodies are the old ones, unchanged):

```rust
impl<'db> TraitImpls<'db> {
    fn collect_def_map(db: &'db Db, def_map: &DefMap) -> Self {
        let lang_items = hir_def::lang_item::lang_items(db, def_map.krate());
        let mut map = FxHashMap::default();
        for (module, _) in def_map.modules(db) {
            Self::collect_module(db, module, lang_items, &mut map);
        }
        Self::from_traits(map)
    }

    /// Adds the trait impls of `module` and of the blocks in the bodies it holds to `map`.
    fn collect_module(
        db: &'db Db,
        module: ModuleId,
        lang_items: &LangItems,
        map: &mut FxHashMap<TraitId, OneTraitImplsBuilder<'db>>,
    ) {
        let impls = module_impls(db, module);
        for &impl_id in impls.trait_impls.iter() {
            let trait_ref = match db.impl_trait(impl_id) {
                Some(tr) => tr.instantiate_identity().skip_norm_wip(),
                None => continue,
            };
            // Reservation impls should be ignored during trait resolution, so we never need
            // them during type analysis. See rust-lang/rust#64631 for details.
            //
            // FIXME: Reservation impls should be considered during coherence checks. If we are
            // (ever) to implement coherence checks, this filtering should be done by the trait
            // solver.
            if AttrFlags::query(db, impl_id.into()).contains(AttrFlags::RUSTC_RESERVATION_IMPL) {
                continue;
            }

            let self_ty = trait_ref.self_ty();
            if self_ty_has_error_constructor(self_ty) {
                // If we see `impl Foo for NoSuchType`, just ignore it.
                continue;
            }

            let interner = DbInterner::new_no_crate(db);
            let entry = map.entry(trait_ref.def_id.0).or_default();
            match simplify_type(interner, self_ty, TreatParams::InstantiateWithInfer) {
                Some(self_ty) => entry.non_blanket_impls.entry(self_ty).0.push(impl_id),
                None => entry.blanket_impls.push(impl_id),
            }
        }

        for &impl_id in impls.builtin_derive_impls.iter() {
            let loc = impl_id.loc(db);
            let Some(trait_id) = loc.trait_.get_id(lang_items) else { continue };
            let entry = map.entry(trait_id).or_default();
            let entry = entry.non_blanket_impls.entry(SimplifiedType::Adt(loc.adt.into()));
            entry.1.push(impl_id);
        }

        let scope = module.def_map(db).module(db, module).scope(db);
        for owner in impl_holders(db, module.krate(db), scope) {
            for (_, block_def_map) in Body::of(db, owner).blocks(db) {
                for (block_module, _) in block_def_map.modules(db) {
                    Self::collect_module(db, block_module, lang_items, map);
                }
            }
        }
    }

    /// Adds every impl of this table to `map`, each run after the runs already there.
    fn merge_into(&self, map: &mut FxHashMap<TraitId, OneTraitImplsBuilder<'db>>) {
        for (trait_, t) in self.trait_indices() {
            self.merge_at(t, map.entry(trait_).or_default());
        }
    }

    /// Adds this table's impls of `trait_` to `map`, as [`Self::merge_into`] does.
    fn merge_trait_into(
        &self,
        trait_: TraitId,
        map: &mut FxHashMap<TraitId, OneTraitImplsBuilder<'db>>,
    ) {
        if let Some(t) = self.trait_index(trait_) {
            self.merge_at(t, map.entry(trait_).or_default());
        }
    }

    fn merge_at(&self, t: usize, into: &mut OneTraitImplsBuilder<'db>) {
        into.blanket_impls.extend_from_slice(self.blanket.run(t));
        for (self_ty, (impls, derives)) in self.rows_at(t) {
            let row = into.non_blanket_impls.entry(*self_ty);
            row.0.extend_from_slice(impls);
            row.1.extend_from_slice(derives);
        }
    }
```

`for_crate_and_deps` stays for `for_each_crate_and_block*` (the solver's whole-table walks); it
reads each crate's `for_crate`, which now reruns only when some module's impls change.

> applied by requests-impls: 1a to 1j as written (old texts matched today's file). Departures: `in_crate_for_self_ty` returns the one module's memo directly when a type's impls are in one module; `TraitModules::new` fills its rows `with_capacity` + `extend`; `// alloc:` on the memo columns hot_lint flagged.

### 2. `crates/blaze-items/src/index.rs`

2a. The module doc (L7-15). Old:

```rust
//! - impls: one crate's impls of one trait ([`trait_impls_in`], which reruns when the crate's
//!   impls change and backdates for every other trait), then every crate's impls of the trait
//!   ([`trait_impls`]), then the impls for one simplified self type ([`impls_of`]) and the
//!   blanket ones ([`blanket_impls_of`]);
//! - inherent impls: one crate's, by simplified self type ([`inherent_impls_in`]), then every
//!   crate's for one self type ([`inherent_impls_of`]);
//! - traits with an item of a name, for method candidates: one crate's ([`traits_in`]), then
//!   every crate's for one name ([`traits_with`]);
```

New:

```rust
//! - impls: one crate's impls of one trait ([`trait_impls_in`], from hir-ty's per-module
//!   bucket `TraitImpls::for_crate_and_trait`, which reruns only when a module's impls of that
//!   trait change), then every crate's impls of the trait ([`trait_impls`]), then the impls for
//!   one simplified self type ([`impls_of`]) and the blanket ones ([`blanket_impls_of`]);
//! - inherent impls: one module's by simplified self type ([`inherent_impls_in_module`]), the
//!   modules of a crate with impls for each type ([`inherent_impl_modules`],
//!   [`inherent_modules_of`]) and one module's for one type ([`module_inherent_impls_of`]),
//!   then every crate's for one self type ([`inherent_impls_of`]); a frozen crate's come from
//!   its one frozen table ([`inherent_impls_in`]);
//! - traits with an item of a name, for method candidates: one module's
//!   ([`traits_in_module`]), one crate's ([`traits_in`]), then every crate's for one name
//!   ([`traits_with`]);
```

2b. Import (L43). Old:

```rust
    method_resolution::{InherentImpls, TraitImpls},
```

New:

```rust
    method_resolution::{InherentImpls, TraitImpls, frozen},
```

2c. `trait_impls_in` (L474). Old:

```rust
    TraitImpls::for_crate(db, krate).for_trait(trait_, |impls| match impls {
```

New:

```rust
    TraitImpls::for_crate_and_trait(db, krate, trait_).for_trait(trait_, |impls| match impls {
```

2d. `inherent_impls_in` and `inherent_impls_of` (L525-554). Old:

```rust
/// The inherent impls of `krate`, by the hash of their simplified self type, then key.
#[salsa::tracked(returns(ref))]
fn inherent_impls_in(db: &Db, krate: Crate) -> Runs<SimplifiedType, ImplId> {
    Runs::new(
        InherentImpls::for_crate(db, krate)
            .iter()
            .flat_map(|(self_ty, impls)| {
                let self_ty = key::simplified(db, self_ty);
                impls
                    .iter()
                    .map(move |&it| (Some(self_ty), key::item_key(db, ItemId::Impl(it)), it))
            })
            .collect(),
    )
}

/// The inherent impls of every crate for `self_ty`, sorted by key.
#[salsa::tracked(returns(ref))]
pub(crate) fn inherent_impls_of(
    db: &Db,
    self_ty: SimplifiedType,
    _: (),
) -> Keyed<ImplId> {
    Keyed::sorted(
        base_db::all_crates(db)
            .iter()
            .flat_map(|&krate| inherent_impls_in(db, krate).run(Some(self_ty)).iter())
            .collect(),
    )
}
```

New:

```rust
/// The inherent impls of a frozen `krate`, from its one frozen table, by the hash of their
/// simplified self type, then key.
#[salsa::tracked(returns(ref))]
fn inherent_impls_in(db: &Db, krate: Crate) -> Runs<SimplifiedType, ImplId> {
    inherent_rows(db, InherentImpls::for_crate(db, krate))
}

/// The inherent impls of `module` and of the blocks in the bodies it holds, by the hash of
/// their simplified self type, then key: equal unless one of them is added, removed or
/// changes its self type.
#[salsa::tracked(returns(ref))]
fn inherent_impls_in_module(
    db: &Db,
    module: ModuleId,
) -> Runs<SimplifiedType, ImplId> {
    inherent_rows(db, InherentImpls::for_module(db, module))
}

fn inherent_rows(db: &Db, impls: &InherentImpls<'_>) -> Runs<SimplifiedType, ImplId> {
    Runs::new(
        impls
            .iter()
            .flat_map(|(self_ty, impls)| {
                let self_ty = key::simplified(db, self_ty);
                impls
                    .iter()
                    .map(move |&it| (Some(self_ty), key::item_key(db, ItemId::Impl(it)), it))
            })
            .collect(),
    )
}

/// The modules of `krate` with an inherent impl for each simplified self type, by its hash,
/// then the module's key: equal unless a module gains its first impl for a type or loses its
/// last.
#[salsa::tracked(returns(ref))]
fn inherent_impl_modules(db: &Db, krate: Crate) -> Runs<SimplifiedType, ModuleId> {
    let mut rows = Vec::new();
    for (module, _) in crate_def_map(db, krate).modules(db) {
        let in_module = inherent_impls_in_module(db, module);
        if in_module.searched.is_empty() {
            continue;
        }
        let key = key::module_key(db, module);
        rows.extend(in_module.searched.iter().map(|&self_ty| (Some(self_ty), key, module)));
    }
    Runs::new(rows)
}

/// The modules of `krate` with an inherent impl for `self_ty`: equal unless that list changes,
/// whatever other types gain.
#[salsa::tracked(returns(deref))]
fn inherent_modules_of(
    db: &Db,
    krate: Crate,
    self_ty: SimplifiedType,
) -> Box<[ModuleId]> {
    inherent_impl_modules(db, krate).run(Some(self_ty)).iter().map(|(_, it)| it).collect()
}

/// The inherent impls of `module` for `self_ty`, sorted by key: equal unless one for that
/// type is added there or removed.
#[salsa::tracked(returns(ref))]
fn module_inherent_impls_of(
    db: &Db,
    module: ModuleId,
    self_ty: SimplifiedType,
) -> Keyed<ImplId> {
    Keyed::of(inherent_impls_in_module(db, module).run(Some(self_ty)).iter())
}

/// The inherent impls of every crate for `self_ty`, sorted by key. Reads, in each crate, only
/// the modules with impls for that type.
#[salsa::tracked(returns(ref))]
pub(crate) fn inherent_impls_of(
    db: &Db,
    self_ty: SimplifiedType,
    _: (),
) -> Keyed<ImplId> {
    let mut rows = Vec::new();
    for &krate in base_db::all_crates(db).iter() {
        if frozen::served(db, krate) {
            rows.extend(inherent_impls_in(db, krate).run(Some(self_ty)).iter());
            continue;
        }
        for &module in inherent_modules_of(db, krate, self_ty) {
            rows.extend(module_inherent_impls_of(db, module, self_ty).iter());
        }
    }
    Keyed::sorted(rows)
}
```

2e. `traits_in` (L556-580), per module. Old:

```rust
/// The traits declared in `krate`'s modules, by the hash of each name one of their items
/// has, then key.
#[salsa::tracked(returns(ref))]
fn traits_in(db: &Db, krate: Crate) -> Runs<NameHash, TraitId> {
    let mut names = Vec::new();
    let mut traits = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        for def in module.scope(db).declarations() {
            let ModuleDefId::TraitId(trait_) = def else { continue };
            let key = key::item_key(db, ItemId::Trait(trait_));
            for (name, _) in trait_.trait_items(db).items.iter() {
                names.push(name.as_str());
                traits.push((key, trait_));
            }
        }
    }
    let mut found: Vec<_> = name_hashes(names)
        .into_iter()
        .zip(traits)
        .map(|(name, (key, trait_))| (Some(name), key, trait_))
        .collect();
    found.sort_unstable_by_key(|&(name, key, _)| (name, key));
    found.dedup_by_key(|&mut (name, key, _)| (name, key));
    Runs::new(found)
}
```

New:

```rust
/// The traits declared in `module`, by the hash of each name one of their items has, then
/// key, each pair once: equal unless such a trait or the names of its items change.
#[salsa::tracked(returns(deref))]
fn traits_in_module(
    db: &Db,
    module: ModuleId,
) -> Box<[(NameHash, ItemKey, TraitId)]> {
    let mut names = Vec::new();
    let mut traits = Vec::new();
    for def in module.def_map(db).module(db, module).scope(db).declarations() {
        let ModuleDefId::TraitId(trait_) = def else { continue };
        let key = key::item_key(db, ItemId::Trait(trait_));
        for (name, _) in trait_.trait_items(db).items.iter() {
            names.push(name.as_str());
            traits.push((key, trait_));
        }
    }
    let mut found: Vec<_> = name_hashes(names)
        .into_iter()
        .zip(traits)
        .map(|(name, (key, trait_))| (name, key, trait_))
        .collect();
    found.sort_unstable_by_key(|&(name, key, _)| (name, key));
    found.dedup_by_key(|&mut (name, key, _)| (name, key));
    found.into()
}

/// The traits declared in `krate`'s modules, by the hash of each name one of their items
/// has, then key: reruns only when some module's [`traits_in_module`] changes. A trait is
/// declared in one module, so the modules' rows never repeat one another.
#[salsa::tracked(returns(ref))]
fn traits_in(db: &Db, krate: Crate) -> Runs<NameHash, TraitId> {
    let mut found = Vec::new();
    for (module, _) in crate_def_map(db, krate).modules(db) {
        let rows = traits_in_module(db, module);
        found.extend(rows.iter().map(|&(name, key, trait_)| (Some(name), key, trait_)));
    }
    Runs::new(found)
}
```

> applied by requests-impls: 2a to 2e, adapted to today's `Runs::new(&mut rows)` (sorts its rows) and `frame::task` frame vectors; `inherent_modules_of` and `traits_in_module` build their boxes with `table::column`. `traits_in_module` reads `module.def_map(db).module(db, module).scope(db)`.

### 3. `crates/blaze-items/src/tests/narrow.rs`

3a. Import (L20-22). Old:

```rust
    method_resolution::{
        ImplWork, InherentImpls, TraitImpls, TraitValueItems, impl_work, trait_value_item,
    },
```

New:

```rust
    method_resolution::{
        ImplWork, InherentImpls, SelfTyKey, TraitImpls, TraitValueItems, impl_work,
        trait_value_item,
    },
```

3b. `an_impl_of_another_trait_leaves_the_impls_of_a_trait` (L227-230): the cut now comes one
memo earlier, at the bucket. Old:

```rust
    let warm = db.log_executed(|| assert_eq!(impls_of_trait(&db, tr), 2));
    assert!(ran(&warm, "::in_crate_for_trait"), "{warm:#?}");
    assert!(!ran(&warm, "::in_crate_and_deps_for_trait"), "{warm:#?}");
```

New:

```rust
    let warm = db.log_executed(|| assert_eq!(impls_of_trait(&db, tr), 2));
    assert!(ran(&warm, "::in_module_for_trait"), "{warm:#?}");
    assert!(!ran(&warm, "::for_crate_and_trait"), "{warm:#?}");
    assert!(!ran(&warm, "::in_crate_for_trait"), "{warm:#?}");
    assert!(!ran(&warm, "::in_crate_and_deps_for_trait"), "{warm:#?}");
```

3c. New tests, after `impl_tables_keep_the_order_impls_are_found_in`:

```rust
const MODULES: &str = "mod a;\nmod b;\npub trait T {}\npub trait U {}\n";
const MODULE_A: &str = "pub struct A;\nimpl A {}\nimpl crate::T for A {}\n";
const MODULE_B: &str = "pub struct B;\nimpl B {}\nimpl crate::T for B {}\nimpl crate::U for B {}\n";

/// A crate of two modules with impls, and the files of `a` and `b`.
fn modules_db() -> (TestDB, FileId, FileId) {
    let (db, files) = TestDB::with_many_files(&format!(
        "//- /main.rs crate:main\n{MODULES}//- /a.rs\n{MODULE_A}//- /b.rs\n{MODULE_B}"
    ));
    let (a, b) = (files[1].file_id(&db), files[2].file_id(&db));
    (db, a, b)
}

/// The buckets built module by module hold what the crate's tables hold, in the same order.
#[test]
fn per_module_buckets_are_the_crate_tables() {
    let (db, _, _) = modules_db();
    let k = krate(&db);
    let a = SimplifiedType::Adt(AdtId::from(struct_named(&db, "A")).into());
    let b = SimplifiedType::Adt(AdtId::from(struct_named(&db, "B")).into());
    let (t, u) = (trait_named(&db, "T"), trait_named(&db, "U"));
    hir_ty::attach_db(&db, || {
        let whole = &**TraitImpls::for_crate(&db, k);
        let inherent = InherentImpls::for_crate(&db, k);
        for trait_ in [t, u] {
            let bucket = TraitImpls::for_crate_and_trait(&db, k, trait_);
            for self_ty in [a, b, SimplifiedType::Str] {
                let expected = whole.for_trait_and_self_ty(trait_, &self_ty);
                assert_eq!(bucket.for_trait_and_self_ty(trait_, &self_ty), expected);
                let narrow =
                    TraitImpls::in_crate_for_trait_and_self_ty(&db, k, trait_, SelfTyKey(self_ty));
                assert_eq!(narrow, expected.0);
            }
            assert_eq!(bucket.blanket_impls(trait_), whole.blanket_impls(trait_));
        }
        for self_ty in [a, b, SimplifiedType::Str] {
            let narrow = InherentImpls::in_crate_for_self_ty(&db, k, SelfTyKey(self_ty));
            assert_eq!(narrow, inherent.for_self_ty(&self_ty));
        }
        assert_eq!(whole.for_trait_and_self_ty(t, &a).0.len(), 1);
        assert_eq!(whole.for_trait_and_self_ty(u, &b).0.len(), 1);
        assert_eq!(inherent.for_self_ty(&b).len(), 1);
    });
}

/// An impl of `U` added to `a` reruns `U`'s bucket and what reads it; `T`'s bucket and the
/// inherent ones are left unexecuted, though `a`'s own tables rerun.
#[test]
fn an_impl_in_one_module_reruns_only_its_trait_bucket() {
    let (mut db, a_file, _) = modules_db();
    let k = krate(&db);
    let (t, u) = (trait_named(&db, "T"), trait_named(&db, "U"));
    let b = SimplifiedType::Adt(AdtId::from(struct_named(&db, "B")).into());
    let read = |db: &TestDB| {
        hir_ty::attach_db(db, || {
            (
                TraitImpls::for_crate_and_trait(db, k, t).blanket_impls(t).len(),
                TraitImpls::in_crate_for_trait(db, k, u).len(),
                InherentImpls::in_crate_for_self_ty(db, k, SelfTyKey(b)).len(),
            )
        })
    };
    assert_eq!(read(&db), (0, 1, 1));

    db.set_file_text(a_file, &format!("{MODULE_A}impl crate::U for A {{}}\n"));
    let warm = db.log_executed(|| assert_eq!(read(&db), (0, 2, 1)));
    assert!(ran(&warm, "::for_module"), "{warm:#?}");
    assert!(ran(&warm, "::in_crate_for_trait"), "{warm:#?}");
    let buckets = warm.iter().filter(|it| it.contains("::for_crate_and_trait")).count();
    assert_eq!(buckets, 1, "only `U`'s bucket reruns: {warm:#?}");
    assert!(!ran(&warm, "::in_crate_for_self_ty"), "{warm:#?}");
}

/// Adding a function to `a` leaves every bucket and both crate tables unexecuted.
#[test]
fn an_item_that_is_no_impl_reruns_no_bucket() {
    let (mut db, a_file, _) = modules_db();
    let k = krate(&db);
    let t = trait_named(&db, "T");
    let a = SimplifiedType::Adt(AdtId::from(struct_named(&db, "A")).into());
    let read = |db: &TestDB| {
        hir_ty::attach_db(db, || {
            _ = TraitImpls::for_crate(db, k);
            _ = InherentImpls::for_crate(db, k);
            (
                TraitImpls::in_crate_for_trait_and_self_ty(db, k, t, SelfTyKey(a)).len(),
                InherentImpls::in_crate_for_self_ty(db, k, SelfTyKey(a)).len(),
            )
        })
    };
    assert_eq!(read(&db), (1, 1));

    db.set_file_text(a_file, &format!("{MODULE_A}pub fn added() {{}}\n"));
    let warm = db.log_executed(|| assert_eq!(read(&db), (1, 1)));
    assert!(ran(&warm, "::for_module"), "{warm:#?}");
    for query in [
        "::for_crate",
        "::for_crate_and_trait",
        "::modules_by_trait",
        "::modules_by_self_ty",
        "::in_crate_for_trait_and_self_ty",
        "::in_crate_for_self_ty",
    ] {
        assert!(!ran(&warm, query), "{query} reran: {warm:#?}");
    }
}
```

> applied by requests-impls: 3a, 3b and the three tests of 3c as written (`modules_db`, `per_module_buckets_are_the_crate_tables`, `an_impl_in_one_module_reruns_only_its_trait_bucket`, `an_item_that_is_no_impl_reruns_no_bucket`).

### 4. `crates/hir-ide/src/tests/incremental.rs` (coordinator's, for `requests`)

`TraitImpls::for_crate` now reruns only when some module's trait impls change, and the new
per-module table runs under it. In `adding_struct_invalidates_infer`,
`adding_enum_query_log`, `adding_use_query_log` and `adding_impl_query_log` (whose new item is
the inherent `impl SomeStruct`), the second `execute_assert_events` (L298, L373, L445, L530)
becomes, old:

```rust
        &[("TraitImpls < 'db >::for_crate_", 1)],
```

new:

```rust
        &[("TraitImpls < 'db >::for_crate_", 0), ("TraitImpls < 'db >::for_module_", 1)],
```

(the first `execute_assert_events` of each keeps `1`). Every `expect!` there that lists
`TraitImpls`/`InherentImpls` queries (those four and the long list near L633-649) gains the
per-module names: regenerate with `UPDATE_EXPECT=1` when the tests run.

Status: open

> applied by requests-impls: the second `execute_assert_events` of the four tests (L297, L372, L444, L529) now require `for_crate_` 0 and `for_module_` 1; the `expect!` lists are left to `UPDATE_EXPECT=1` at the first test run.


## dedup: hir_def::test_items

From `dedup-scan` (lane dedup; report `/tmp/blaze-lanes/v2-dedup-scan.md`). `declarations` and
`impls` (every `ModuleDefId`/`ImplId` of a crate's modules) are written again in six test modules;
the one copy is `hir_def::test_items` (`v2/requests/db.md`, `## dedup: hir_def::test_items`).
Apply once db lands it (lane items is DONE; `mem-backend-items` is editing this crate, so it or
the coordinator applies this):

- `crates/blaze-items/src/tests/narrow.rs`: the bodies of `fn declarations` (47-53) and `fn impls`
  (55-61) become `hir_def::test_items::declarations(db, krate(db))` and
  `hir_def::test_items::impls(db, krate(db))`.
- `crates/blaze-items/src/tests/coherence.rs`: the bodies of `fn declarations` (42-49) and
  `fn impls` (51-57) become `hir_def::test_items::declarations(db, krate)` and
  `hir_def::test_items::impls(db, krate)`.
- Drop `crate_def_map` from both files' imports if nothing else uses it. The `*_named` lookups
  stay: they take the first match (`find_map`), where `test_items` asks for exactly one.

Status: open

> forwarded to mem-backend-items (editing blaze-items, running; told), to apply once db lands `hir_def::test_items`.


## speed-hir-store-paths: paths as columns (2026-10-29)

From `speed-hir-store-paths-consumers` (child of `speed-hir-store-paths`). hir-def's paths and
generic arguments are columns now (`crates/hir-def/src/expr_store/path.rs`; the API is in
`/tmp/blaze-lanes/v2-speed-hir-store-paths.md`, "The API"): `Path<'a>`, `GenericArgs<'a>`,
`AssociatedTypeBinding<'a>`, `PathGenericArgs<'a>` and `Bindings<'a>` are `Copy` views decoded
from the store; `TypeBound` and `WherePredicate` are `Copy` records whose `for<..>` binders and
`use<..>` arguments are runs (`Names`, `UseArgs`) read with `store.names(..)` and
`store.use_args(..)`. Apply these with the paths lane's own edits to `hir.rs`/`expr_store*`
(one compile). Nothing here is built (common.md).

The consumer rule:

1. `&Path` parameters and fields become `Path<'_>` (by value; `Copy`). Where a function keeps
   `&Path<'_>`, callers pass `&path`. `&store[p]` / `&map[p]` for a `PathId` -> `store.path(p)`.
2. `Path::Normal(it) => &it.mod_path` -> `it.mod_path` (already a reference).
   `it.generic_args` / `it.type_anchor` field reads are unchanged.
3. `generic_args()` yields slots by value: `.into_iter().flatten().flatten()` still gives
   `GenericArgs<'_>`; `.iter().any(Option::is_some)` -> `.iter().any(|it| it.is_some())`;
   `generic_args()?.last()?.as_ref()?` -> `generic_args()?.last()??`; `&generic_args[i]` ->
   `generic_args.get(i)`.
4. `&GenericArgs` parameters -> `GenericArgs<'_>`; `Option<&GenericArgs>` -> `Option<GenericArgs<'_>>`
   (drop `.as_ref()`/`.as_deref()`; `*args` -> `args`). `args.bindings.iter()`, `for b in
   &args.bindings`, `.first()`, `.is_empty()`, `.len()` keep working; `bindings[i]` ->
   `bindings.get(i).unwrap()` (or restructure), `&*bindings`/`bindings.as_ref()` -> iterate.
5. `binding.args.as_ref()` -> `binding.args`; `&binding.bounds` / `binding.bounds.iter()`
   unchanged (`&'a [TypeBound]`).
6. `TypeBound::ForLifetime(lifetimes, path)`: `lifetimes` is `Names`; read with
   `store.names(lifetimes)`. `TypeBound::Use(args)`: `store.use_args(args)`. `bound.clone()` ->
   `*bound`; `&TypeBound::Path(p, m)` patterns keep working.
7. `WherePredicate::TypeBound { lifetimes: Some(l), .. }`: `l` is `Names`; `store.names(l)`.
8. `Path::from_known_path_with_no_generic(m)` -> `Path::from_known_path_with_no_generic(&m)`
   (bind the `ModPath` to a local if it is a temporary used past the statement).
9. `path.qualifier()` gives `Qualifier<'_>`: use `.path()` for a `Path<'_>` (bind the
   qualifier to a local first: `let q = path.qualifier()?; let q = q.path();`).

Applied the same way everywhere, so that callers in one crate agree with signatures in another:
every `&Path`, `Option<&GenericArgs>`, `&GenericArgs`, `&[Option<GenericArgs>]` parameter,
field and return becomes the by-value view, hir-def's `Resolver::resolve_path_in_*` and
`record_path_uses` and hir-ty's path lowering included. Bodies are by-value views
(`store.expr(id) -> Expr<'_>`), so `Expr::Path(p)` binds `p: Path<'_>` and most call sites
keep their text. Also `args.args` is `&[GenericArg]` now, so `for arg in &args.args` becomes
`for arg in args.args` (`&&[T]` is not `IntoIterator`).

The edits assume, from the paths lane: `Expr::Path`/`Pat::Path`/`RecordLit`/`Pat::Record`/
`Pat::TupleStruct` hold `Path<'a>`; `Expr::MethodCall { generic_args: Option<GenericArgs<'a>> }`;
`AsmOperand::Sym(PathIdx)` read with `store.path_at(idx)` (both `pub`); `Index<PathId> for
ExpressionStore` removed; `StoreVisitor::on_path`/`on_generic_args`/`on_type_bound` taking the
views by value; `ExprCollector::lower_path -> Option<PathIdx>`.

Line numbers are as of 20:50; several of these files are being edited, so find each edit by its
old text (where the same text is on several lines, the function named beside it picks).

> forwarded to mem-backend-items (editing `crates/blaze-items/`; items is DONE) (from requests, pass 16; told by tmux).


### crates/hir-ty/src/method_resolution.rs

- line 148 (`lookup_method_including_private`): `        generic_args: Option<&HirGenericArgs>,` -> `        generic_args: Option<HirGenericArgs<'_>>,`

> forwarded to mem-backend-items (editing `crates/blaze-items/`; items is DONE) (from requests, pass 16; told by tmux).

### crates/hir-ty/src/method_resolution/confirm.rs

- line 59 (`confirm_method`): `        generic_args: Option<&HirGenericArgs>,` -> `        generic_args: Option<HirGenericArgs<'_>>,`
- line 101 (`ConfirmContext::confirm`): `        generic_args: Option<&HirGenericArgs>,` -> `        generic_args: Option<HirGenericArgs<'_>>,`
- line 349 (`instantiate_method_args`): `        generic_args: Option<&HirGenericArgs>,` -> `        generic_args: Option<HirGenericArgs<'_>>,`

Stay as written: `TypeLikeConst::Path(path) => self.ctx.make_path_as_const(path)` (420, `match
arg` by value), `substs_from_args_and_bindings(.., generic_args, ..)` (463), `provided_kind(..,
arg: &HirGenericArg)` (`GenericArg` is unchanged), the `debug!`/`#[instrument]` of
`generic_args` (`GenericArgs<'_>` is `Debug`).

> forwarded to mem-backend-items (editing `crates/blaze-items/`; items is DONE) (from requests, pass 16; told by tmux).

### No edits

- `traits.rs:400` `predicate: &WherePredicate` passed to `lower_where_predicate`: not in the
  rule's list, and `WherePredicate` is now `Copy`, so it still compiles.
- `method_resolution/probe.rs`, `drop.rs`, `layout.rs`, `layout/adt.rs`, `specialization.rs`,
  `opaques.rs`: only `next_solver` types and `Mode::MethodCall`/`Mode::Path` (false hits).
- `crates/blaze-items/src/form.rs:2575` `AsmOperand::Sym(operand)` is MIR's operand;
  `facts.rs`, `tests/coherence.rs`, `tests/narrow.rs` use `next_solver::GenericArgs`. Nothing
  in blaze-items reads hir-def paths.

Status: open

> forwarded to mem-backend-items (editing `crates/blaze-items/`; items is DONE) (from requests, pass 16; told by tmux).

> mem-backend-items (2026-10-29 20:36): nothing here is in `crates/blaze-items` (as "No edits"
> says), and the edits are to `crates/hir-ty/src/method_resolution{,/confirm}.rs`, outside this
> lane's paths. They belong with the paths lane's own edits to hir-def and hir-ty (one compile);
> not applied here. Status: open, for the paths lane or hir-ty's owner.

## dedup: bits: blaze-items `table::Bits` onto `blaze_ids::bits::Ranked`

From `dedup-bits-store` (report `/tmp/blaze-lanes/v2-dedup-bits-store.md`; parent `dedup-bits`).
`blaze_ids::bits` (`crates/blaze-ids/src/bits.rs`) is the one bit-set family. Its `Ranked` is
this `Bits`: words, a `u32` count of members before each word, a length; `contains`/`rank` panic
at or past the length as `get`/`rank` did, `count` is the same sum, and `place(at)` is
`contains(at).then(|| rank(at))` in one call. The test `a_bitset_ranks_each_row` is in
`crates/blaze-ids/src/bits/tests.rs` already (with a proptest of `rank` against a popcount).
No hash, serialized byte or equality changes: `HashColumns` is not serialized, and `Ranked`'s
`PartialEq` compares the same words, counts and length.

1. `crates/blaze-items/src/table.rs`: delete the whole `Bits` struct and its `impl Bits`
   (from `/// One bit per row, with the count of set bits before each word, ...` through the
   `}` closing `impl Bits`).

   Imports, before:

   ```rust
   use blaze_ids::ContentId;
   ```

   after:

   ```rust
   use blaze_ids::{ContentId, bits::Ranked};
   ```

   In `HashColumns`, before:

   ```rust
       has_layout: Bits,
       layout: Box<[LayoutHash]>,
       has_code: Bits,
   ```

   after:

   ```rust
       has_layout: Ranked,
       layout: Box<[LayoutHash]>,
       has_code: Ranked,
   ```

   In `HashColumns::new`, before:

   ```rust
           let has_layout = Bits::new(rows.iter().map(|(_, it)| it.layout.is_some()));
           let has_code = Bits::new(rows.iter().map(|(_, it)| it.code.is_some()));
   ```

   after:

   ```rust
           let has_layout = Ranked::from_bools(rows.iter().map(|(_, it)| it.layout.is_some()));
           let has_code = Ranked::from_bools(rows.iter().map(|(_, it)| it.code.is_some()));
   ```

   In `HashColumns::row`, before:

   ```rust
               layout: self.has_layout.get(row).then(|| self.layout[self.has_layout.rank(row)]),
               code: self.has_code.get(row).then(|| self.code[self.has_code.rank(row)]),
   ```

   after (`row` is in bounds: `self.kinds[row]` above panics first, as before):

   ```rust
               layout: self.has_layout.place(row).map(|at| self.layout[at]),
               code: self.has_code.place(row).map(|at| self.code[at]),
   ```

2. `crates/blaze-items/src/tests/table.rs`: delete the test `a_bitset_ranks_each_row` (moved
   to blaze-ids) and `table::Bits,` from the `use crate::{...}` list.

> forwarded to mem-backend-items (editing `crates/blaze-items/`; items is DONE) (from requests, pass 16; told by tmux).

> applied by mem-backend-items (2026-10-29 20:36), as written; also `tests/mem.rs`'s bitset
> budget check now uses `Ranked::from_bools` and `place`. Unbuilt. Status: done.

## dedup: bits: blaze-items `form.rs` impl items used, and the canonical walk's `uses`

From `dedup-bits-store`, as above.

1. `crates/blaze-items/src/form.rs`, the impl's items matched against the trait's (~line 960):
   a `u64` of the first 64 and an `FxHashSet` past them is a growable bit set kept inline for
   one word. Before:

   ```rust
                   // The impl items used, as a bit each; past 64 of them, a set.
                   let (mut used, mut used_past) = (0u64, FxHashSet::default());
   ```

   after:

   ```rust
                   // The impl items used, a bit each, inline up to 64 of them.
                   let mut used: Bits<SmallVec<[u64; 1]>> = Bits::new(items.len());
   ```

   before:

   ```rust
                       Some(at) => {
                           if at < 64 {
                               used |= 1 << at;
                           } else {
                               used_past.insert(at);
                           }
                           f.u8(1);
   ```

   after:

   ```rust
                       Some(at) => {
                           used.insert(at);
                           f.u8(1);
   ```

   before:

   ```rust
                   if used.count_ones() as usize + used_past.len() != items.len() {
   ```

   after:

   ```rust
                   if used.count() != items.len() {
   ```

   Imports: `use blaze_ids::{ContentId, radix};` becomes
   `use blaze_ids::{ContentId, bits::{self, Bits}, radix};` and add `use smallvec::SmallVec;`
   beside the other crate imports (`FxHashSet` stays imported: `seen` uses it).
   `crates/blaze-items/Cargo.toml` `[dependencies]`: add `smallvec.workspace = true` (a root
   workspace dependency already).

2. `CanonBuffers::uses` (~line 2855) stays `Vec<Cell<u64>>`: the walk marks locals through
   `&self` (`Canon` is shared by the closures of the walk, as `next` and `numbers` are), and a
   `Bits` writes through `&mut`; a `RefCell<BitSet>` would add a borrow check to every local
   the walk meets. Its index arithmetic moves onto `blaze_ids::bits`. Before:

   ```rust
       /// A bit for each local the first walk found used.
       uses: Vec<Cell<u64>>,
   ```

   after:

   ```rust
       /// A bit for each local the first walk found used, in cells rather than a
       /// `blaze_ids::bits::BitSet`: the walk marks them through `&self`.
       uses: Vec<Cell<u64>>,
   ```

   before:

   ```rust
           c.uses.resize_with(body.locals.len().div_ceil(64), || Cell::new(0));
   ```

   after:

   ```rust
           c.uses.resize_with(bits::words_for(body.locals.len()), || Cell::new(0));
   ```

   before:

   ```rust
       fn used(&self, local: LocalId) -> bool {
           let at = local_index(local);
           (self.c.uses[at / 64].get() >> (at % 64)) & 1 == 1
       }

       fn mark_used(&self, local: LocalId) {
           let at = local_index(local);
           let word = &self.c.uses[at / 64];
           word.set(word.get() | 1 << (at % 64));
       }
   ```

   after:

   ```rust
       fn used(&self, local: LocalId) -> bool {
           let (word, mask) = bits::locate(local_index(local));
           self.c.uses[word].get() & mask != 0
       }

       fn mark_used(&self, local: LocalId) {
           let (word, mask) = bits::locate(local_index(local));
           let word = &self.c.uses[word];
           word.set(word.get() | mask);
       }
   ```

Status: open

> forwarded to mem-backend-items (editing `crates/blaze-items/`; items is DONE) (from requests, pass 16; told by tmux).

> applied by mem-backend-items (2026-10-29 20:36), as written, with `smallvec.workspace = true`
> in `crates/blaze-items/Cargo.toml` (manifest lint exit 0; `Cargo.lock` waits on whoever may
> run cargo, as in `v2/requests/mem-backend-items.md`). Unbuilt. Status: done.

## dedup: push_successors is TerminatorKind::all_successors reversed (from dedup-scan)

`crates/blaze-items/src/form.rs` `fn push_successors(kind, stack)` pushes exactly
`kind.all_successors()` (db.md) in reverse, so the walk pops them in order. After db adds it,
delete `push_successors` and at its call write `stack.extend(kind.all_successors().into_iter().rev());`.
Same order, so no CodeHash changes. `tests/form.rs`'s copy stays: it is the reference
`hashed_walk` is checked against.

> forwarded to mem-backend-items (editing `crates/blaze-items/`; items is DONE) (from requests, pass 16; told by tmux).

> taken by mem-backend-items (2026-10-29 20:36), waiting: `TerminatorKind::all_successors` is not
> in hir-ty yet. Status: open, waiting on db.

> applied by mem-backend-items (2026-10-29 21:20), as written, now that `all_successors` is in
> `crates/hir-ty/src/mir.rs`; checked arm by arm that the reversed list is the order
> `push_successors` pushed (so no CodeHash changes). `tests/form.rs`'s copy stays. Unbuilt.
> Status: done.

## frontend-fast-trait-store: implement `GoalFacts`, and the facts the solver's reads need (2026-10-29)

From frontend-fast-trait-store, child of frontend-fast-trait (notes
`docs/v2/plan/lanes/frontend/fast-trait-store.md` section 3; code
`crates/hir-ty/src/next_solver/goal_store.rs`). The persistent goal cache keeps a global trait
goal's answer in the store under its stable form (definitions by your item keys) with the facts
its evaluation read. hir-ty cannot name `blaze_items`, so it defines the interface,
`hir_ty::next_solver::goal_store::GoalFacts`, and the driver hands it your implementation.

1. New file `crates/blaze-items/src/goals.rs`, and `pub mod goals;` in `lib.rs`'s mod list:
   ```rust
   //! Lane items' part of the persistent goal cache (`hir_ty::next_solver::goal_store`): item
   //! keys for a goal's definitions, its crate's key, and the facts its reads stand for.

   use base_db::{Crate, Db};
   use blaze_ids::{ContentId, Hasher};
   use hir_def::lang_item::{LangItemEnum, lang_items};
   use hir_ty::next_solver::{
       SolverDefId,
       goal_store::{FactObject, FactRef, GoalFacts},
   };

   use crate::{Fact, facts, key, table};

   const GOAL_CRATE_DOMAIN: &str = "blaze v2 goal crate 1";

   pub struct Goals;

   impl GoalFacts for Goals {
       fn def_key<'db>(&self, db: &'db Db, def: SolverDefId<'db>) -> ContentId {
           key::def_key(db, def).0
       }

       fn crate_key(&self, db: &Db, krate: Option<Crate>) -> ContentId {
           match krate {
               Some(krate) => goal_crate_key(db, krate),
               None => ContentId::of(GOAL_CRATE_DOMAIN, b"no crate"),
           }
       }

       fn fact_of(&self, db: &Db, read: salsa::DatabaseKeyIndex, name: &'static str) -> Option<FactRef> {
           // the mapping of item 2; `None` for a read no fact stands for
       }

       fn value(&self, db: &Db, fact: FactRef) -> Option<ContentId> {
           facts::value(db, &table::fact_of_ref(fact)?).ok()
       }
   }

   /// A crate's identity and its lang items by item key: what an interner of it reads when it
   /// is made, outside a goal's evaluation.
   #[salsa::tracked]
   fn goal_crate_key(db: &Db, krate: Crate) -> ContentId {
       let lang_items = lang_items(db, krate);
       let mut h = Hasher::new(GOAL_CRATE_DOMAIN);
       h.id(&key::crate_key(db, krate));
       for (at, item) in LangItemEnum::ALL.iter().enumerate() {
           // each present lang item: its index and its target's item key (a `LangItemTarget`
           // as the `SolverDefId` it is, or your key of a macro)
       }
       h.finish()
   }
   ```
   and in `table.rs`, beside `split`/`join` (which already have this shape):
   `pub(crate) fn fact_ref(fact: &Fact) -> FactRef` (kind `fact.kind()`, subject, object
   `FactObject::{None, Field, Id}` as `split` gives) and
   `pub(crate) fn fact_of_ref(fact: FactRef) -> Option<Fact>` (`join`, with `None` in place of
   its `unreachable!`s for a kind or object that is no fact: a record from the store is data).
2. The facts the solver's reads need: today every goal is kept out, as the solver reads these
   queries and none stands for a fact. The table in the notes, section 3.4, lists them by query
   with the fact each should be: `Implements { trait_, self_ty }` read through a narrow query
   in place of the whole impl tables (`TraitImpls::for_crate`, `for_each_block`,
   `blanket_in_crate_and_deps`); `Signature` of an impl, ADT, trait or fn for their signature
   queries; `Field`; `Specializes`; `AutoTraits` for opaque leakage; and a target fact (none
   exists) for `target_data_layout`. `fact_of` maps a read (its ingredient, by
   `name`, and its key) to that fact. After each build the driver shows
   `Flushed::inexpressible`, the names still unmapped, by count.

Tests: in `goals.rs`, `fact_of_ref(fact_ref(f)) == Some(f)` for one fact of each kind, and the
crate key changing when a lang item moves.

Status: open

> applied by requests-goals: `crates/blaze-items/src/goals.rs` (`Goals: GoalFacts`, `goal_crate_key` over `LangItemEnum::ALL`, and `fact_of_read`, which maps the notes' 3.4 reads by ingredient name, decoding the key only when its page holds that struct), `table::{fact_ref, fact_of_ref}` (`join` is now `joined`, which gives `None` for a kind or object that is no fact), and a new `Fact::Target(crate key)` (`kinds::TARGET` 0x1011, valued by `TargetData::content()` or the load error) for `target_data_layout_query`; tests in `src/tests/goals.rs`. Departures: the target fact is keyed by the crate key, not by the target, because a tuple does not fix a data layout (custom specs) and a key has to resolve in a later build; the signature rows map the tracked ingredients that are actually read (`ImplSignature::kept`, `impl_trait_of_impl`, `supertraits_info`, `generic_predicates_of`, `AttrFlags::query` of any item, ...), not the untracked wrappers the table names; four kinds of read stay unmapped (`None`), each with a request in `v2/requests/requests-goals.md`: the whole impl tables (`Implements` needs a narrow read in `method_resolution.rs`), `specializes_query` (two-argument key, `specialization.rs`), and the crate batches behind module-level `impl_trait`/`impl_self_ty`/`field_types`/impl predicates (db-ty). `tait_hidden_types` stays unmapped too, because an alias's interface does not hash TAIT hidden types. No `Field` or `AutoTraits` read exists yet to map.

> forwarded to requests-goals (requests' child: lane items is DONE, so `crates/blaze-items` is requests'; brief `~/restart-2026-10-28/briefs/v2/requests-goals.md`) (requests, pass 26).


## dedup: layout.rs's coroutine body through `MirOwner::coroutine` (from dedup-sim-front, 2026-10-29)

After `v2/requests/db-ty.md` "## dedup: one driver under the 24 MIR queries (from
dedup-sim-front, 2026-10-29)" (it adds `MirOwner`, `MirStage` and `HirDatabase::monomorphized_mir`).
The `is_by_move_coroutine` if/else over the two coroutine queries is written four times
(here, codegen's `debuginfo/types.rs` and `mod.rs`); `MirOwner::coroutine` is it once.

`crates/hir-ty/src/layout.rs`, in `TyKind::Coroutine(id, args)` (line ~584), replace

```rust
            let body = if crate::mir::is_by_move_coroutine(db, id.0, args) {
                db.monomorphized_runtime_mir_body_for_coroutine_by_move(
                    id.0,
                    parent_args.store(),
                    trait_env.clone(), // clone: two `u32` handles
                )
            } else {
                db.monomorphized_runtime_mir_body_for_coroutine(
                    id.0,
                    parent_args.store(),
                    trait_env.clone(), // clone: two `u32` handles
                )
            }
            .map_err(|_| LayoutError::Unknown)?;
```

with

```rust
            let owner = crate::mir::MirOwner::coroutine(db, id.0, args);
            let runtime = crate::mir::MirStage::Runtime;
            let body = db
                .monomorphized_mir(owner, runtime, parent_args.store(), trait_env.clone()) // clone: two `u32` handles
                .map_err(|_| LayoutError::Unknown)?;
```

Same query, same key: `monomorphized_mir` with `MirStage::Runtime` reads
`monomorphized_runtime_mir_body_for_coroutine{,_by_move}_query`.

> applied (requests, pass 29; lane items is DONE): `layout.rs`'s coroutine arm reads `db.monomorphized_mir(MirOwner::coroutine(db, id.0, args), MirStage::Runtime, ..)`, as written.

## dedup: one Check/Verify comparison of frozen bytes (from dedup-sim-front, 2026-10-29)

`crates/hir-ty/src/method_resolution/frozen.rs` `check` is hir-def's `check_lang_items` around
another encoder. After `v2/requests/db.md` (same heading) adds `hir_def::nameres::frozen::check_frozen`:

- `use hir_def::{ModuleDefId, nameres::frozen::{Dec, Enc, Frozen, Mode, record_difference}};`
  becomes `use hir_def::{ModuleDefId, nameres::frozen::{Dec, Enc, Frozen, Mode, check_frozen}};`
  (keep `record_difference` too if `rg record_difference` still finds a use in the file).
- `check` becomes

```rust
pub fn check(db: &Db, krate: Crate) {
    let Some(frozen) = frozen(db, krate) else { return };
    check_frozen(db, krate, "impls", frozen, || encode(db, krate));
}
```

(its doc comment stays). What is recorded is the same text as before.

> applied by requests-items-next: `method_resolution/frozen.rs` `check` is now `check_frozen(db, krate, "impls", frozen, || encode(db, krate))`, as written. The import takes `check_frozen` in place of `record_difference`, which the file no longer uses.

## From db (db-next2, 2026-10-29): `opaques.rs` still calls `StoredEarlyBinder::bind` with one argument

`crates/hir-ty/src/opaques.rs:159`, `:200`, `:221`, `:230` call `StoredEarlyBinder::bind(value)`;
it takes `(interner, value)` now and makes aliases non-rigid (`v2/requests/solver-sync-kinds.md`
sections 5 and 8). Every other call in hir-ty is ported, so this file is all that keeps the crate
from compiling against the new binder.

> for: items.

> applied (requests, pass 30; lane items is DONE): the four `StoredEarlyBinder::bind` calls in `hir-ty/src/opaques.rs` take the interner. `rpit_hidden_types` makes one with `DbInterner::new_no_crate(db)`; `tait_hidden_types` uses its own.
