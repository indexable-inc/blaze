## dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng-front` (report `/tmp/blaze-lanes/v2-dedup-codec-rng-front.md`). Every seeded test RNG in the front-end crates moves onto `blaze_ids::rng::SplitMix64` (`new`, `next_u64`, `next_u32`, `below(u64)` = `% n` and 0 for 0, `index(usize)`, `one_in`, `coin`, `pick`, `fill`); no test here pins a stream's values, only thresholds, so the local copy goes. The crate already depends on `blaze-ids`.

In `crates/hir-def/src/item_scope.rs`, `mod tests`:

1. Add `use blaze_ids::rng::SplitMix64;` as the first line of the external group, before `use indexmap::map::Entry as MapEntry;`.
2. Delete `struct Rng(u64);` and its `impl Rng { next, below, chance }` (lines ~2156-2173).
3. Every `&mut Rng` (in `def`, `makro`, `vis`, `import`, `per_ns` and the `item` closure) becomes `&mut SplitMix64`.
4. `Rng(seed)`, `Rng(7)`, `Rng(11)` (~2484, ~2614, ~2664) become `SplitMix64::new(seed)`, `SplitMix64::new(7)`, `SplitMix64::new(11)`.
5. Every `rng.chance(n)` becomes `rng.one_in(n)`. `rng.below(n)` stays (same `u64 -> u64`).

> forwarded to db-expand-names-next1 (from requests, pass 16; told by tmux).
> applied by db-expand-names: `item_scope.rs` `mod tests` uses `blaze_ids::rng::SplitMix64` (`new`, `below`, `one_in`); the local `Rng` is gone.
