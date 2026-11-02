
## dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng-tools` (report `/tmp/blaze-lanes/v2-dedup-codec-rng-tools.md`). File
`crates/blaze-ids/src/bits/tests.rs` (yours, being written). Its `fn xorshift` is a seeded
xorshift; the crate's own `crate::rng::SplitMix64` replaces it. No test pins its values.

1. Add `use crate::rng::SplitMix64;` to the imports.
2. Delete `fn xorshift(mut seed: u64) -> impl FnMut(usize) -> usize { ... }` and its doc line.
3. In `ranges_match_a_bit_at_a_time`: `let mut next = xorshift(0x9e37_79b9_7f4a_7c15);` becomes
   `let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);`, and each `next(x)` becomes
   `rng.index(x)`; `next(2) == 0` may read `rng.coin()`.
4. In `bits_are_a_set`: `let mut next = xorshift(0x2545_f491_4f6c_dd1d);` becomes
   `let mut rng = SplitMix64::new(0x2545_f491_4f6c_dd1d);`, `next(len) as u32` becomes
   `rng.index(len) as u32`, and `next(3)` becomes `rng.index(3)`.

> applied (dedup-bits): `xorshift` deleted; both tests draw from `crate::rng::SplitMix64` (`index`, `coin`).
