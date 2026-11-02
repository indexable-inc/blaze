---
status: plan
summary: speed pass over hir-ty's method probe, impl tables and impls in bodies (items' files), against rustc's probe and impl index; coherence, drop, opaques and layout in the children's notes.
date: 2026-10-29
---
# speed-types-probe: method probe and impl tables

Files: in `crates/hir-ty/src/`, `method_resolution.rs` and `method_resolution/{probe,confirm,frozen}.rs`;
`crates/blaze-items/src/tests/narrow.rs`. Coherence, specialization, drop and opaques
(`traits.rs`, `specialization.rs`, `drop.rs`, `opaques.rs`) are in
[speed-types-probe-coherence.md](speed-types-probe-coherence.md), layout (`layout.rs`,
`layout/`) in [speed-types-probe-layout.md](speed-types-probe-layout.md). Nothing here was run
(principles.md): the costs are counted from the code, per call, not measured.

## The method probe (`probe.rs`)

**Benchmark.** rustc's `rustc_hir_typeck/src/method/probe.rs`: per autoderef step one
`inherent_impls` lookup of the step's simplified type; per trait in scope
`associated_items(trait).filter_by_name_unhygienic(name)`, a binary search in a name-sorted
`SortedIndexMultiMap`; inference variables only for a trait that has an item of the name.

**Before.**
- Per trait in scope, per call site: `fresh_args_for_item` (an inference variable per trait
  parameter, a `GenericParams` read, an interned `GenericArgs`) before looking at the name.
  With the prelude's ~40 traits in scope, ~40 sets of variables for the one or two traits
  that have the method.
- Per trait in scope, per call site: one tracked `trait_value_item(trait, name)`: an
  interned `(TraitId, Name)` key (a hash and a table probe), a memo check and a dependency
  edge. Memos for the build: traits in scope times method names.
- Per pick attempt (four per autoderef step): a `Vec` of the applicable candidates.
- Per probe: the body's opaque types copied into a `Vec`, then into a second `Vec`.

**After.**
- Trait arguments are made once per trait, at its first item of the name: none for a trait
  without one. The variables live inside the probe's `infcx.probe` and each candidate is
  matched in its own snapshot, so only their numbering inside the probe changes.
- A trait declared in a module is answered from `TraitValueItems` (in `method_resolution.rs`),
  which the probe keeps in its `ProbeForNameChoice`: per crate of the traits in scope, one
  `in_crate_trait_value_items(krate, name)` (the run of one name, sorted by trait, cut from
  `trait_value_items(krate)`, every (name, trait, first function or const of the name) of the
  crate's module traits, built once per crate), then one binary search per trait. The runs are
  a `SmallVec` of 4 (core, alloc, std, the local crate), so a call site allocates nothing for
  them. A trait in a block keeps `trait_value_item`. Memos and edges per call site fall from
  (traits in scope) to (crates of those traits), typically 40 to 4; memos for the build from
  (traits x names) to (crates x names).
- Applicable candidates in a `SmallVec<[_; 4]>`; the opaque types interned straight from the
  storage's iterator (`new_from_iter`, a stack buffer of 8).

Matches rustc: one lookup per crate plus a binary search per trait, against rustc's binary
search per trait. Left: rustc's name-sorted items are per trait and built with the trait;
here `trait_value_items` sorts per crate. A name-sorted `TraitItems`/`ImplItems` in `hir-def`
(requested by items-hirty, `v2/requests/speed-types-probe.md`) would remove that sort and turn
`impl_value_item`'s scan into a binary search.

Invalidation: an item of name N added to a trait of crate C reruns `trait_value_items(C)` and
`in_crate_trait_value_items(C, N)`; a probe of another name sees its run come out equal.

## Impl tables (`method_resolution.rs`, `frozen.rs`)

`TraitImpls` and `InherentImpls` were already columns (items-hirty): sorted trait ids with CSR
rows, self types as a hash column sorted within each trait's rows with the types beside it,
impls as one flat array per crate with CSR runs. A lookup is a binary search on traits and one
on hashes and returns a slice into the table. That matches rustc's `TraitImpls`
(`blanket_impls` plus `non_blanket_impls: FxIndexMap<SimplifiedType, Vec<DefId>>`) in lookups
and beats it in allocation (one array per crate, not a `Vec` per key). Unchanged here.

- `loaded_crates` built an `FxHashSet<&str>` of every identifier of each file that might name
  a dependency (an allocation and a hash per identifier) to test a handful of names. Now each
  identifier is compared with the dependencies still unnamed and the walk stops when none is
  left: no allocation, no hashing.
- `frozen::frozen` cloned the crate's `Frozen` (an `Arc` increment) per read; it borrows it.

## Impls in bodies (`impl_holders`, `impls_in_bodies`)

**Benchmark.** rustc has every body's items after expansion and collects impls as it goes; it
lowers nothing on demand, so there is no direct benchmark. The fastest exact way is a token
prefilter that is a superset of the bodies that can hold an impl.

**Before.** A local crate lowered every body to find impls in it (`local || may_hold(..)`), so
`TraitImpls::for_crate` and `InherentImpls::for_crate` of a workspace crate read `Body::of` of
every function, static and associated constant: the lowering of every body ran before the
first trait query of the crate. items-hirty kept it because the token check then knew no
procedural macros; since 18:10 it does (a dependency's procedural macro and an unknown macro
name both count as writing).

**After.** Local crates use `may_hold` as dependencies do: a body is lowered for impls when its
text has an `impl`, a call of a macro not proven inert or an attribute that is not inert.
Module constants are still all looked into (`const _` has no identifier, so the lexer finds no
initializer after it).

A gap in the lexer is closed for both: it took the first `{` after `fn name` for the body, even
inside brackets or in a const argument (`-> [u8; { 4 }]`, `-> Wrap<{ 4 }>`), and then never
saw the real body, so an impl there was missed. Now a `{` is a body only at the signature's
bracket depth, and one after `<`, `,` or `=` counts as a body without ending the signature, so
the next one does too.

## Tests (`crates/blaze-items/src/tests/narrow.rs`)

- `the_trait_name_index_answers_as_each_trait_alone`: ten traits in three crates, in a module
  file, written by a macro, in a block and a nested block, with repeated names, constants and
  types; for every name and a missing one, `TraitValueItems::get` (one index asked of every
  trait, forward then back) equals `trait_value_item`.
- `a_trait_method_of_another_name_leaves_the_lookup` and
  `removing_the_trait_method_reruns_the_lookup`: the incremental behavior above.
- `a_local_body_is_lowered_for_impls_only_when_it_may_hold_one`: which local bodies are
  holders, including the bracketed and const-argument signatures, and that all four impls in
  bodies are found.
