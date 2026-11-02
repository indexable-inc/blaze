# Requests of db-ty-unify

The inference tables in `crates/hir-ty/src/next_solver/infer/` are off `ena`: union-find in flat
columns (`infer/union_find.rs`), snapshots as length marks, and region constraints as columns
(`RegionConstraints` in `infer/region_constraints/mod.rs`: a tag byte and two `u32` operands per
row, concrete regions in a side column). One public field changes type,
`RegionConstraintData::constraints`, from `Vec<Constraint<'db>>` to `RegionConstraints<'db>`.
It has `len()`, `is_empty()`, `get(index) -> Constraint`, `iter()`, `iter_from(start)`,
`iter_with_visibility()` and `to_vec()`. Items are returned by value (`Constraint` is now `Copy`),
so slicing (`[start..]`), `&[Constraint]` and `*constraint` stop compiling. The callers below
need these changes. `Constraint::regions(interner) -> (sub, sup)` is new and does what each
caller's own match does.

## 1. db-ty (parent), `crates/hir-ty/Cargo.toml`: remove `ena`

What: delete `ena = "0.14.3"` from `[dependencies]`.

Why: `rg '\bena::' crates/hir-ty` finds nothing; only comments mention it.
`vendor/rustc_type_ir` still depends on `ena` (`data_structures/mod.rs`), so `Cargo.lock` keeps
it.

> forwarded to db: `crates/hir-ty/Cargo.toml` is lane db's.
> checked by db-audit-dbsub-tysmall (22:33): in the tree: `crates/hir-ty/Cargo.toml` has no `ena`.

## 2. db-ty-hir (sibling), `crates/hir-ty-wfcheck/src/outlives.rs`, `Wfcx::needs_since`

Replace

```rust
        let constraints = inner.unwrap_region_constraints().region_constraint_data().constraints
            [mark.1..]
            .to_vec();
```

with

```rust
        let constraints: Vec<Constraint<'db>> = inner
            .unwrap_region_constraints()
            .region_constraint_data()
            .constraints
            .iter_from(mark.1)
            .collect();
```

`mark()`'s `.constraints.len()` compiles unchanged.

> forwarded to db: `crates/hir-ty-wfcheck/` is lane db's.
> checked by db-audit-dbsub-tysmall (22:33): in the tree at `crates/hir-ty-wfcheck/src/outlives.rs:489-492`: `.constraints.iter_from(mark.1)`, collected into the task's frame (`mem::collect`) rather than a heap `Vec`.

## 3. db-ty-borrowck (sibling), `crates/hir-ty-borrowck/src/regions.rs`

- `constraint_pairs` (line ~6437): take an iterator.

  ```rust
  fn constraint_pairs<'db>(
      interner: hir_ty::next_solver::DbInterner<'db>,
      constraints: impl IntoIterator<Item = hir_ty::next_solver::infer::region_constraints::Constraint<'db>>,
  ) -> Vec<(Region<'db>, Region<'db>)> {
      constraints.into_iter().map(|constraint| constraint.regions(interner)).collect()
  }
  ```

- In the trait proving path (line ~6236):
  `constraint_pairs(interner, inner.unwrap_region_constraints().region_constraint_data().constraints.iter_from(constraints_before))`.
- `prove_query` (line ~6497) and `normalize_query` (line ~7359):
  `constraint_pairs(interner, constraints.region_constraint_data().constraints.iter())`.
- `normalize_full` (line ~7168): replace `.constraints[constraints_before..].to_vec()` with
  `.constraints.iter_from(constraints_before).collect::<Vec<_>>()`.

The `.constraints.len()` calls compile unchanged.

> forwarded to db: `crates/hir-ty-borrowck/` is lane db's.
> checked by db-audit-dbsub-tysmall (22:33): in the tree at `crates/hir-ty-borrowck/src/regions.rs:6590` (`constraint_pairs` takes an iterator, extending a frame `FVec` in place of returning a `Vec`), callers at :6348, :6647, :7368 (`iter_from`) and :7551; `rg 'constraints\['` finds no slicing left.

## 4. db-ty-solver (sibling), `crates/hir-ty/src/next_solver/solver.rs`

- `make_deduplicated_region_constraints` (line ~141): the iterator yields `Constraint` by value.
  Replace the `match *constraint { ... }` block with
  `let (a, b) = constraint.regions(interner);`, and drop the now unused `var` closure and the
  `use super::infer::region_constraints::Constraint;`. Each row now also records its
  `VisibleForLeakCheck`. rustc's `make_query_region_constraints` carries it to the response, so
  to match rustc, iterate `iter_with_visibility()` and pass each row's visibility, not
  `VisibleForLeakCheck::Yes`. Type outlives obligations stay `Yes`.
- `leak_check` (line ~81) returns `Ok(())` and so never runs rustc's leak check. `InferCtxt` now
  has rustc's `leak_check` (`infer/relate/higher_ranked.rs`, on the port in
  `infer/region_constraints/leak_check.rs`). rustc's delegate is:

  ```rust
  fn leak_check(&self, max_input_universe: rustc_type_ir::UniverseIndex) -> Result<(), NoSolution> {
      InferCtxt::leak_check(&self.0, max_input_universe, None).map_err(|_| NoSolution)
  }
  ```

  (fully qualified: `self.leak_check(..)` would resolve to the delegate method itself.)

Done (db-ty-solver's note in its own request 1): the stale comment about reinitializing the
cache in `InferCtxtBuilder::build` now reads "Solving needs the crate's trait environment and
solver cache."

> forwarded to db: `next_solver/solver.rs` is lane db's.
> checked by db-audit-dbsub-tysmall (22:33): in the tree at `crates/hir-ty/src/next_solver/solver.rs:465-495` (`make_deduplicated_region_constraints_into` takes `query_region_constraints()`'s rows by value with each row's visibility, no `match *constraint`) and `:344-348` (`InferCtxt::leak_check(&self.0, max_input_universe, None)`).
