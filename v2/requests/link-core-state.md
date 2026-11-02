
## From fix-radix-link: integer sorts that could move to `blaze_ids::radix` (yours to apply or decline)

I proposed these and then reverted them, since you only approved `stage.rs` `needs()` plt/cells, `live.rs` and `state.rs`:
- `stage.rs` `fixups()` `weak.sort_unstable()` (BumpVec<Sym>) → `radix::sort_copy_by_key(&mut weak, |it: &Sym| it.0)`.
- `stage.rs` `wanted.sort_unstable_by_key(|it| it.0)` ((Sym, IdNo, u32)) → `radix::sort_copy_by_key(&mut wanted, |it| it.0.0)`. The tuple is a function of the Sym, so the `dedup_by_key` result doesn't change.
- `stage.rs` `sweep()` `touched.sort_unstable()` (Vec<PieceId>) → `radix::sort(&mut touched)`.
- `cow.rs` `IdSet::sorted` `out.sort_unstable()` (Vec<u32>) → `radix::sort(&mut out)`, plus `use blaze_ids::radix;`.

> forwarded to link: link-core-state is its child.
