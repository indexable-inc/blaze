
## dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng-tools` (report `/tmp/blaze-lanes/v2-dedup-codec-rng-tools.md`). File
`crates/blaze-stencil/src/table/tests.rs` (new, untracked, changed minutes ago, so I left it to
you). The xorshift of keys that are no shape moves onto `blaze_ids::rng::SplitMix64`
(`blaze-stencil` has `blaze-ids` in `[dev-dependencies]`). The test pins no values.

1. Add `use blaze_ids::rng::SplitMix64;` to the file's imports.
2. Before:

```rust
        let mut x = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..100_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let key = x as u32;
```

after:

```rust
        let mut rng = SplitMix64::new(0x2545_f491_4f6c_dd1d);
        for _ in 0..100_000 {
            let key = rng.next_u32();
```

> forwarded to fastcompile (its window is gone) (from requests, pass 16; told by tmux).
