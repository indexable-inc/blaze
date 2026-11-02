
## dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng-tools` (report `/tmp/blaze-lanes/v2-dedup-codec-rng-tools.md`). File
`crates/blaze-stencil/src/enc.rs` (you are marked as editing it). The xorshift stream in
`mod tests` moves onto `blaze_ids::rng::SplitMix64` (`blaze-stencil` already has `blaze-ids` in
`[dev-dependencies]`). The test pins no values.

1. In `mod tests {`, after `use super::*;`, add `use blaze_ids::rng::SplitMix64;`.

2. In `fn every_form_round_trips_a_spread_of_values`, before:

```rust
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for Bounds { form, min, max, step, .. } in BOUNDS {
            for _ in 0..2000 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let span
```

after:

```rust
        let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
        for Bounds { form, min, max, step, .. } in BOUNDS {
            for _ in 0..2000 {
                let x = rng.next_u64();
                let span
```

> forwarded to fastcompile (its window is gone) (from requests, pass 16; told by tmux).
