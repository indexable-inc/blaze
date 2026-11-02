## dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng` (report `/tmp/blaze-lanes/v2-dedup-codec-rng.md`). `blaze_ids::rng::SplitMix64` (`crates/blaze-ids/src/rng.rs`) is the one seeded test RNG: `new`, `next_u64`, `below` (`% n`, 0 for 0), `index`, `one_in`, `shuffle`, `fill`, and more. Each local copy below gives the same stream; please delete it and use the shared one.

`crates/blaze-graph/Cargo.toml` already has `blaze-ids.workspace = true` under `[dev-dependencies]` (added by me).

1. `crates/blaze-graph/src/plane_tests.rs`: delete the `/// SplitMix64, so the simulation needs no dependency ...` comment and `struct SplitMix(u64);` with its whole `impl SplitMix { next_u64, below, one_in }` (lines 24-43 today). Add `use blaze_ids::rng::SplitMix64;` above `use blaze_protocol::events::...`. Then replace every `SplitMix(` with `SplitMix64::new(` and every `&mut SplitMix` with `&mut SplitMix64` (14 uses). Optional, same values: `ids[random.below(ids.len() as u64) as usize]` -> `ids[random.index(ids.len())]`, likewise `running.swap_remove(random.index(running.len()))`. `one_in(n)` differs only at `n == 0` (shared: never), and every call passes 3 or 4.
2. `crates/blaze-graph/src/pool.rs` test module: delete `/// SplitMix64, as blaze-core's sim::Rng.` and `struct Rng(u64);` with its `impl Rng { next, below }`; add `use blaze_ids::rng::SplitMix64 as Rng;` to the module's `use` lines; replace `Rng(seed)`, `Rng(7)`, `Rng(11)` with `Rng::new(seed)`, `Rng::new(7)`, `Rng::new(11)`. No `rng.next()` is called.
3. `crates/blaze-graph/src/pump.rs` test module: the same as pool.rs: delete the `Rng` struct and impl, add `use blaze_ids::rng::SplitMix64 as Rng;`, `Rng(seed)` -> `Rng::new(seed)`.
4. With 1: `crates/blaze-graph/src/plane_tests/memory.rs` imports the deleted struct: `use super::{SplitMix, add, built, close, config, done, keyed, open, runs, work};` becomes `use super::{add, built, close, config, done, keyed, open, runs, work};` plus `use blaze_ids::rng::SplitMix64;` above it, and `SplitMix(seed)` becomes `SplitMix64::new(seed)`. 1 and 4 go together or neither.

> forwarded to mem-core-plane (from requests, pass 16; told by tmux).

> done by mem-core-plane: 1-4 as written, all four together (the optional `index` rewrites not
> taken). One addition: `pool.rs`'s `a_job_in_a_cell_runs_as_the_same_closure_boxed` called
> `rng.next()`, now `rng.next_u64()`, the same draw. Every `one_in` passes a nonzero constant,
> so each seed's stream is unchanged. Unbuilt, as the lane allows no build.
