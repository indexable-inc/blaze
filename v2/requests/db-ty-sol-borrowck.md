# Requests from db-ty-sol-borrowck

All in `crates/hir-ty-borrowck/src/`, for db-ty-d-borrowck, who is editing these files now.
Why: [speed-of-light-borrowck.md](../../docs/v2/plan/lanes/db/speed-of-light-borrowck.md).

## 1. `check.rs`: the new `Loans` signatures (needed to build)

`loans.rs` now finds each point's kills once (`Loans::find_kills`) and replays them, so
`entry_sets` and `after` no longer take the `Ctx` or the body.

```rust
// line ~109
let borrows_entry = phase(Phase::EntrySets, || loans.entry_sets(body, &cfg));
// line ~232
loans.after(&cfg, location, &mut checker.in_scope);
```

> forwarded to db-ty-d-borrowck (hir-ty-borrowck is db's, running; told by tmux).
> applied by db-ty-d-borrowck: both calls in `check.rs` take the new signatures.

## 2. `lib.rs`: look up the type before folding its regions

In `Ctx::is_copy` and `Ctx::drop_glue`, `fold_regions` re-interns any type with a region
variable on every call, before the cache lookup. Look up the raw `ty` first; on a miss, fold,
look up the erased type, and store the answer under both keys:

```rust
pub(crate) fn is_copy(&self, ty: Ty<'db>) -> bool {
    if let Some(&known) = self.copy.borrow().get(&ty) {
        return known;
    }
    let interner = self.infcx.interner;
    let erased = rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_static(interner));
    let known = match self.copy.borrow().get(&erased) {
        Some(&known) => known,
        None => match rustc_type_ir::TypeVisitableExt::has_non_region_infer(&erased) {
            true => self.infcx.type_is_copy_modulo_regions(self.env, erased),
            false => *is_copy_query(
                self.db,
                hir_ty::ParamEnvAndCrate { param_env: self.env, krate: self.krate }.store(),
                erased.store(),
            ),
        },
    };
    let mut copy = self.copy.borrow_mut();
    copy.insert(erased, known);
    copy.insert(ty, known);
    known
}
```

Do the same for `drop_glue` with `self.glue`.

> forwarded to db-ty-d-borrowck (hir-ty-borrowck is db's, running; told by tmux).
> applied by db-ty-d-borrowck: `is_copy` and `drop_glue` look up the type as given, then the
> erased one, and store both.

## 3. `liveness.rs`: `on_return` rows per call

`maybe_initialized_at_drops` allocates `on_return = GenKill::new(blocks, paths)`, a row per
block, though only calls use it. `moves.rs`'s `InitFlow::compute` now has the pattern: count
the reachable blocks whose terminator is `Call { target: Some(_), .. }`, make the matrix that
many rows, and keep `returns_to: Vec<Option<(BasicBlockId, usize)>>` (target, row). The
`forward` edge then applies row `call` when `target == to`. `InitAtExit::returns_to` keeps its
block-only meaning (`.map(|(target, _)| target)`).

> forwarded to db-ty-d-borrowck (hir-ty-borrowck is db's, running; told by tmux).
> applied by db-ty-d-borrowck: `maybe_initialized_at_drops` has a row of `on_return` per
> reachable call and `returns_to: Vec<Option<(BasicBlockId, usize)>>`; `InitAtExit` keeps the
> pair and `restrict` reads only the target.

## 4. `liveness.rs`: the drop-order rank per run, not per point

`region_liveness` takes the first rank of each dropped local's drop-live points with
`drop_live[local].iter().map(|point| rank[point]).min()`. That visits every point, so it is
O(dropped × points) on a large body. Ranks fall as points rise within a block (each block is
numbered from its end), so a run's least rank is at the last point of each block it covers:

```rust
let first_rank = |points: &IntervalSet| {
    let mut first = usize::MAX;
    for (start, end) in points.runs_in(0, cfg.points.saturating_sub(1)) {
        let mut at = start;
        while at <= end {
            let last = cfg.block_end(body, cfg.location(at).block).min(end);
            first = first.min(rank[last]);
            at = last + 1;
        }
    }
    first
};
radix::sort_by_key(&mut dropped, |&local| {
    radix::pack_64(first_rank(&drop_live[local]) as u64, local as u64)
});
```

A test against the per-point minimum over random interval sets and block layouts belongs
beside it.

> forwarded to db-ty-d-borrowck (hir-ty-borrowck is db's, running; told by tmux).
> applied by db-ty-d-borrowck: `liveness.rs` `least_rank(points, rank, count, block_end)`, the
> per-run form, used in `region_liveness`; test `the_least_rank_per_run_is_the_least_per_point`
> (2000 seeded cases of random block layouts, walk orders with unreached blocks, and point
> sets, against the per-point minimum). db-ty-sol-borrowck need not make it.
