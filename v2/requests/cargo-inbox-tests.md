## dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng` (report `/tmp/blaze-lanes/v2-dedup-codec-rng.md`). `blaze_ids::rng::SplitMix64` (`crates/blaze-ids/src/rng.rs`) is the one seeded test RNG (`new`, `next_u64`, `below` = `% n`, `index`, `fill`, ...); please use it instead of a local copy. The crate already depends on `blaze-ids`.

`crates/blaze-cargo/src/serve/unit/test/tests.rs` (new; I didn't edit it while you write it):

1. Delete `/// splitmix64.` and `struct Rng(u64);` with its `impl Rng { fn below .. }` (an exact SplitMix64 `below`, so the values stay the same).
2. `use blaze_ids::ContentId;` -> `use blaze_ids::{ContentId, rng::SplitMix64 as Rng};`.
3. `Rng(7)` -> `Rng::new(7)`, `Rng(11)` -> `Rng::new(11)`. `fn lines(rng: &mut Rng, ..)` and the `rng.below(..)` calls stay as they are.

> forwarded to cargo-inbox-tests (writing `serve/unit/test/tests.rs`) (requests, pass 20; told by tmux).

> applied (cargo-inbox-tests): all three edits, as written. `crates/blaze-cargo/src/serve/unit/test/tests.rs:8` imports `blaze_ids::{ContentId, rng::SplitMix64 as Rng}`, the local `Rng` is deleted, and the seeds are `Rng::new(7)` and `Rng::new(11)`. The values are the same: no `below` is ever asked for 0.
