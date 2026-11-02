---
status: plan
summary: speed-types-probe-coherence's notes on coherence, specialization, opaque types and drop in `crates/hir-ty/src/` (`traits.rs`, `specialization.rs`, `opaques.rs`, `drop.rs`). They cover the benchmark (rustc's coherence, specialization graph and opaque type collection), what each cost before and after the change, and what is left.
date: 2026-10-29
---
# speed-types-probe-coherence: coherence, specialization, opaques, drop

Notes of `speed-types-probe-coherence` (a child of `speed-types-probe`, lane
[items](../items.md)) on four files of `crates/hir-ty/src/`. Every answer and its order is
kept, as is every public signature. No query was added, so `db.rs` `kinds!` is unchanged.
The tests are requested in `v2/requests/items.md` as `crates/blaze-items/src/tests/coherence.rs`,
because hir-ty has no test database.

## Benchmark

rustc does each of these once per item. It runs coherence and the orphan check once per impl
(`rustc_trait_selection::traits::coherence`, `orphan_check_trait_ref`), and builds the
specialization graph once per trait (`specialization_graph_of`). In that graph a pair of
impls is related only after `DeepRejectCtxt::args_may_unify` has kept it
(`overlapping_impls`), and the graph is grouped by `simplify_type`. It checks
`Freeze` and drop glue per type, walking each variant's fields in place, and collects
opaque types per opaque (`rustc_hir_analysis::collect::type_of::opaque`). An alias without
`impl Trait` has no opaque, so rustc infers no body for it.

## Before, change, after

| Path | Before | After |
|---|---|---|
| `specialized_ancestor` | one `specializes_query` per impl of the trait that has the item, in the crate and its dependencies: a salsa memo, a `trait_environment`, an inference context and an `eq` per pair, and a `Vec` per call | rustc's fast reject (`relate_rigid_infer`, the specializing impl's parameters rigid, the parent's inference variables, as the query has them) comes first. A pair whose headers can't unify costs one memo hit on `impl_trait` and a structural walk with no query. Candidates go in a `SmallVec<[_; 4]>` |
| `impl_unconstrained_params` | a new `FxHashSet` per projection per round, the own-trait-ref test per projection per round, a `Vec` for the projections | the own-trait-ref test runs once when the projections are collected into a `SmallVec`, and one input set is cleared and reused |
| `impl_has_unconstrained_params` | built the whole `Vec<TypeOrConstParamId>` to test `is_empty` | stops at the first unconstrained parameter and allocates nothing for the answer |
| `check_orphan_rules` (`orphan_rules_hold`) | for every foreign-trait impl, a `trait_environment` query, an inference context and `deeply_normalize`, even with no alias in the header | all three only when `trait_ref.has_aliases()`. The normalizer folds only aliases (`NormalizationFolder::try_fold_ty` returns a type without one as it is), so an alias-free header is already its normal form |
| `tait_hidden_types` | built an inference context and inferred every defining body even when the alias has no `impl Trait`; collected the defining bodies into a `Vec` | returns the empty map before any of that, which is what it answered before, since no `TypeAliasImplTrait(alias, _)` id exists for such an alias. Defining bodies are visited in place, in the same order |
| `define_opaque_functions` | a `Vec<DefWithBodyId>` of holders per module, per block | each holder is visited where it is found, in the same order |
| `is_freeze` | a `Vec<VariantId>` per ADT (a `vec![]` per struct) | walks the struct's fields or each variant of `enum_variant_ids` in place |

## Checked and left as they are

- `destructor` already reads the narrow `in_crate_for_trait_and_self_ty` and
  `in_block_for_trait_and_self_ty`.
- `inherent_impl_coherent` and `orphan_rules_hold` are tracked once per impl already.
- `is_freeze` and `has_drop_glue` build an `ObligationCtxt` per type. That allocates nothing
  (`FulfillmentCtxt::new` holds empty vectors), and `structurally_normalize_term` returns a
  non-alias type on its first test. `has_drop_glue`'s `visited` set is one per call and
  hashes once (`insert`).
- `implements_traits_unique` returns a `Vec<bool>`: a public signature `blaze-items` calls.
- `define_opaque_at` scans the file with `str::match_indices` (two-way search) once per file.
  That is tracked, so it runs once per file revision.

## What is left

Done since by the parent (speed-types-probe): `specialized_ancestor` walked every impl of the
trait in the crate and its dependencies. rustc's graph groups children by simplified self
type, so one lookup reaches the candidates. Now its candidates are
`TraitImpls::blanket_in_crate_and_deps` followed by `in_crate_and_deps_for_trait_and_self_ty`
for its simplified self type (or the blanket impls alone when it has none): every impl it can
specialize is at least as general, so one of those. The blanket impls of every crate now come
before the rows, where before it went crate by crate; the closest ancestor is the one that
specializes every other, so the same one is picked whenever one is (two impls that specialize
each other are overlapping impls, a coherence error). A tracked "parents of this impl" query would also save
the walk, but it would add a salsa cycle through the solver, which calls
`fetch_eligible_assoc_item`, which calls `specialized_ancestor`. The cycle answers of
`specializes_query` would then no longer be the only ones, so it was not done.
