
## dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng-tools` (report `/tmp/blaze-lanes/v2-dedup-codec-rng-tools.md`). File
`crates/blaze-cargo/src/remote.rs` (you are marked as editing it). Two xorshift streams in the
`#[cfg(test)] pub mod tests` move onto `blaze_ids::rng::SplitMix64` (`blaze-cargo` already
depends on `blaze-ids`). No test pins their values. (The parent lane `dedup-codec-rng` may also
send one for a SplitMix64 copy in this file; the two don't overlap.)

1. In `pub mod tests {`, after `use super::*;`, add `use blaze_ids::rng::SplitMix64;`.

2. In `pub fn changed(seed: &[u8], runs: usize)`, before:

```rust
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % bound.max(1) as u64) as usize
        };
```

after:

```rust
        let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
```

and in the same function every `next(` becomes `rng.index(` (`next(4)`, `next(bytes.len())`,
`next(5)`, `next(8)`, `next(7)`; `index(0)` is 0, as `bound.max(1)` gave).

3. In `fn a_packed_pack_is_all_written_before_it_is_unpacked`, before:

```rust
        let mut seed = 0x9e3779b97f4a7c15u64;
        while pack.len() < (64 << 10) + 9 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            pack.extend_from_slice(&seed.to_le_bytes());
        }
```

after:

```rust
        let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
        while pack.len() < (64 << 10) + 9 {
            pack.extend_from_slice(&rng.next_u64().to_le_bytes());
        }
```

> forwarded to speed-link-edge (from requests, pass 16; told by tmux).

> applied: (speed-link-edge, 2026-10-29) as written, both streams in `remote.rs`'s `tests` module.

## dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng` (report `/tmp/blaze-lanes/v2-dedup-codec-rng.md`). `blaze_ids::rng::SplitMix64` (`crates/blaze-ids/src/rng.rs`) is the one seeded test RNG: `new`, `next_u64`, `below` (`% n`, 0 for 0), `index`, `one_in`, `shuffle`, `fill`, and more. Each local copy below gives the same stream; please delete it and use the shared one.

`crates/blaze-v2-proof/src/lib.rs` (the crate already depends on `blaze-ids`): replace the `/// SplitMix64: the same numbers ...` comment, `pub struct Rng(u64);` and its `impl Rng { new, next_u64, below }` with

```rust
pub use blaze_ids::rng::SplitMix64 as Rng;
```

Every user (`bench.rs`, `wire.rs`, `columns.rs`, `bytes.rs`, `tests.rs`, `crates/blaze-format/src/tiers/tests.rs`) calls only `Rng::new`, `next_u64` and `below`, which keep their names. Two differences: `below` is `next_u64() % n` instead of Lemire's multiply-high (no test pins a drawn value: `rng_is_the_same_from_one_seed_and_stays_below` checks `next_u64` from 0, which is unchanged, and bounds), and `below(0)` returns 0 instead of panicking (`sample` never passes 0).

> forwarded to speed-link-edge (from requests, pass 16; told by tmux).

> applied by dedup-codec-rng at 20:59, after speed-link-edge-sol-proof said it had stopped editing the crate. This also covers its new users in `bench.rs`, `join.rs` tests, `wire.rs` tests and `bytes.rs` tests, which call only `new`, `next_u64` and `below(u64)`.

> applied: (speed-link-edge, 2026-10-29; done by dedup-codec-rng) checked. `crates/blaze-v2-proof/src/lib.rs:469` is `pub use blaze_ids::rng::SplitMix64 as Rng;`, and no local `Rng` is left. Nothing more to do.
