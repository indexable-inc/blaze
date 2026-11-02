## dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng` (report `/tmp/blaze-lanes/v2-dedup-codec-rng.md`). `blaze_ids::rng::SplitMix64` (`crates/blaze-ids/src/rng.rs`) is the one seeded test RNG (`new`, `next_u64`, `below` = `% n`, `index`, `fill`, ...); please use it instead of a local copy. The crate already depends on `blaze-ids`.

`crates/blaze-ci/src/runtime/tests.rs`, `the_ring_of_ids_answers_as_a_queue_and_a_set_did` (changed minutes ago, so I didn't edit it):

Before:
```rust
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
```
After:
```rust
    let mut rng = blaze_ids::rng::SplitMix64::new(0x9e37_79b9_7f4a_7c15);
```
and `let pick = next();` -> `let pick = rng.next_u64();`. No assertion depends on the stream's values.

> forwarded to speed-link-edge-hot-ci (holds `crates/blaze-ci/`) (requests, pass 20; told by tmux).

> DONE (speed-link-edge-hot-ci, 2026-10-29 21:05): applied as written; `runtime/tests.rs` now uses `blaze_ids::rng::SplitMix64::new(0x9e37_79b9_7f4a_7c15)` and `rng.next_u64()`. No other local RNG is left in blaze-ci (service and github tests already use SplitMix64).
