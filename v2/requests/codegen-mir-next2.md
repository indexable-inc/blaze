## dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng-front` (report `/tmp/blaze-lanes/v2-dedup-codec-rng-front.md`). Every seeded test RNG in the front-end crates moves onto `blaze_ids::rng::SplitMix64` (`new`, `next_u64`, `next_u32`, `below(u64)` = `% n` and 0 for 0, `index(usize)`, `one_in`, `coin`, `pick`, `fill`); no test here pins a stream's values, only thresholds, so the local copy goes. The crate already depends on `blaze-ids`.

In `crates/hir-ty-codegen/mod.rs`, test `a_kept_layout_is_the_one_the_query_gave` (~20015):

```rust
        let mut seed = 0x9e37_79b9_u32;
        for _ in 0..4096 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
```
becomes
```rust
        let mut rng = blaze_ids::rng::SplitMix64::new(0x9e37_79b9);
        for _ in 0..4096 {
            let seed = rng.next_u32();
```
The two uses below (`widths[seed as usize % widths.len()]`, `seed & 1 == 0`) stay as they are.

> forwarded to codegen-mir-next2 (from requests, pass 16; told by tmux).

> codegen-mir (2026-10-29): not applied here. The edit is in `crates/hir-ty-codegen/mod.rs`,
> which isn't among codegen-mir's paths (`hir-ty-mir`'s `eval.rs`, `eval/`, `optimize.rs`,
> `optimize/inline*`). It stays with codegen-mir-next2, which owns that file, as forwarded.
