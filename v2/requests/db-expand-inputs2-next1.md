## dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng` (report `/tmp/blaze-lanes/v2-dedup-codec-rng.md`). `blaze_ids::rng::SplitMix64` (`crates/blaze-ids/src/rng.rs`) is the one seeded test RNG: `new`, `next_u64`, `below` (`% n`, 0 for 0), `index`, `one_in`, `shuffle`, `fill`, and more. Each local copy below gives the same stream; please delete it and use the shared one.

`crates/base-db/src/tests.rs` (`base-db` already depends on `blaze-ids`):

1. Delete `/// splitmix64: the seeded inputs of the model tests.` and `struct Rng(u64);` with its `impl Rng { next, below }` (lines 404-419 today).
2. Add `use blaze_ids::rng::SplitMix64 as Rng;` after `use cfg::{CfgAtom, CfgOptions};`.
3. Replace each `Rng(N)` constructor with `Rng::new(N)`: `Rng(7)`, `Rng(thread)`, `Rng(11)`, `Rng(3)`, `Rng(19)`, `Rng(23)`, `Rng(seed)`. Any `rng.next()` becomes `rng.next_u64()`. `fn seeded_view(rng: &mut Rng)` stays as it is.

> forwarded to db (its window is gone) (from requests, pass 16; told by tmux).
> applied by db-audit-dbsub-macros (2026-10-29 22:36): as written in `crates/base-db/src/tests.rs` (the `use` sits before `use cfg::..`, rustfmt's order); the file has no `rng.next()`, and every `below` there takes a nonzero `n`, so each stream is unchanged.
