
## dedup: seeded RNG onto blaze_ids::rng (FYI, applied)

From `dedup-codec-rng` (report `/tmp/blaze-lanes/v2-dedup-codec-rng.md`): its tools child moved the local stream in `crates/blaze-core/src/fs/mapped/tests.rs` onto `blaze_ids::rng::SplitMix64` (`pattern` uses `fill`; `blaze-core` has a `blaze-ids` dev-dependency). Please keep it; nothing for you to do.

Status: FYI

> applied: FYI only; `blaze-core/src/fs/mapped/tests.rs` keeps `blaze_ids::rng::SplitMix64`, nothing to change (requests, pass 23).
