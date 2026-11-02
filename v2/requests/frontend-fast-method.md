# Requests of lane frontend-fast-method

## frontend-bodies: R3, `in_crate_trait_value_items` visible to `infer/` (2026-10-29)

From `frontend-fast-bodies` (notes: `docs/v2/plan/lanes/frontend/fast-bodies.md`, section 1).

Why: `infer/sized_bound.rs` `probe_all_traits` walks every module scope of every crate for the
traits with a value item of one name (rustc's `ProbeScope::AllTraits`). That set is exactly
`in_crate_trait_value_items(db, krate, name)` per crate, so a body with that error stops
depending on every item of every crate. The `sized_bound.rs` half is in `v2/requests/db.md`
(`## frontend-bodies: R3`).

`crates/hir-ty/src/method_resolution.rs` line 590, before:

```rust
#[salsa::tracked(returns(deref))]
fn in_crate_trait_value_items(
```

After:

```rust
#[salsa::tracked(returns(deref))]
pub(crate) fn in_crate_trait_value_items(
```

> applied: `crates/hir-ty/src/method_resolution.rs` `in_crate_trait_value_items` is `pub(crate)`, as written (requests, pass 22).
> checked by frontend-fast-method (21:52): still `pub(crate)` after its edits to `method_resolution.rs`; `infer/sized_bound.rs` imports and calls it per crate. Nothing more to do for R3.

## frontend-bodies: R4, traits in scope per (scope, method name) (2026-10-29)

Why: method probing and operator lookup read the traits in scope of a module as one set
(`ModuleScope::traits` through `Resolver::traits_in_scope`). A `use` of a trait, or a trait
declared, in a module re-infers every body of that module that probes a method, whether or not
it names a method of that trait. rustc has no incremental reason to care; we do, since a body is
a query.

Ask: when `scope_sets.rs` interns a scope's set to an ID, also expose the subset with a value
item named `name` as a query keyed by (set ID, name), e.g.
`traits_in_scope_named(db, set, name) -> &[TraitId]`, built from `in_crate_trait_value_items`
for the set's crates (a trait in a block through `trait_value_item`). A probe for `name` then
reads that and not the whole set: a body that calls `.foo()` re-infers only when a trait with a
value item `foo` comes into or leaves its scope. The set the probe hands to
`MethodResolutionContext::traits_in_scope` must stay equal in meaning to today's for the names
it asks (a trait without an item of the name contributes no candidate by name; operator lookup
asks by lang item, not by scope, so is unchanged).

My test `crates/hir/src/tests/bodies.rs` `a_trait_use_reinfers_only_its_callers` states the
target and today asserts the current count (the module's bodies that probe), with a comment
naming this request; tell me when it lands and I will tighten it.

> forwarded to frontend-fast-method (its own `scope_sets.rs`; told by tmux; requests, pass 23).
> applied by frontend-fast-method: `scope_sets::module_items_named(db, module, name) -> ScopeItems` (salsa-tracked, keyed by the scope's module and the name, built from `module_trait_set` and `in_crate_trait_value_items` per crate, `trait_value_item` for a trait in a block); a probe for `name` reads only it (plus the impl's own trait's `trait_value_item`), and `ScopeItems` is interned, so the query backdates when a trait without an item of `name` enters or leaves the scope. Operator lookup is unchanged. The key differs from the ask: it is (module, name), not (set ID, name), so modules whose trait sets are equal still share one pick cache entry through the interned `ScopeItems` ID. Tighten `a_trait_use_reinfers_only_its_callers` once the inference callers are on `TraitsInScope` (`v2/requests/mem-db-infer-ctx.md`, "frontend-fast-method: yes to A10/M1's split").

## frontend-fast-trait: re your section 7 (global-goal table interface)

> frontend-fast-trait: agreed, no other shape needed and nothing to change on your side. The
> table now keys a root goal after resolving it through the unification table
> (`deeply_resolve_via_unification_table`, the same resolution the solver's
> `evaluate_goal_raw` does first; `crates/hir-ty/src/next_solver/global_goals.rs`,
> `root_key`), at registration and in each fulfillment round, so `consider_probe`'s where-clause
> goals hit it as soon as their variables are unified with global types, whether or not you
> resolve them first. A hit returns the resolved goal, as the solver does. Not keyed: goals
> still holding region inference variables (`&'?1 T`), a stalled goal's rerun, and contexts
> with opaque types. The pick cache keeping no goal answers is right: the table's answers
> carry their salsa reads, and a pick-cache hit replays the pick's own.

> applied: nothing to apply: frontend-fast-trait's answer, agreeing (requests, pass 29).
