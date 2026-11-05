---
status: plan
summary: dedup-codec-rng (2026-10-29): every seeded test RNG (SplitMix64 copies, xorshift, xorshift64*, LCG streams) moved onto blaze_ids::rng::SplitMix64; the copies that stay, and why.
date: 2026-10-29
---
# Seeded test RNGs

One generator, `blaze_ids::rng::SplitMix64` (`crates/blaze-ids/src/rng.rs`, written by
`dedup-codec`): Steele, Lea and Flood's SplitMix64, `new`, `state`, `next_u64`, `next_u32`,
`below` (`next_u64() % n`, 0 for 0), `index`, `range`, `one_in`, `coin`, `unit_f64`, `pick`,
`fill`, `shuffle`, and the free `GAMMA` and `mix` (the finalizer, a `const fn`).

## Why SplitMix64

Every copy in the tree was one of four generators: SplitMix64 (most), xorshift64
(`<< 13, >> 7, << 17`), xorshift64* (`* 0x2545_f491_4f6c_dd1d`) and a 64-bit LCG (Knuth's
`6364136223846793005`). SplitMix64 is the one that wins on every axis that matters here:

- quality: it passes BigCrush; xorshift64 fails it (linearity), the LCG's low bits are short
  cycles (every copy shifted them away to hide it);
- seeding: any `u64` is a good seed, 0 included, so the `seed.wrapping_mul(K) | 1` and
  "splitmix the seed, never 0" preambles the xorshift copies needed go away;
- speed: one add, two multiplies, three xor-shifts, about 1.5 ns a draw on a 4 GHz core and
  no loop-carried dependency but the add, so draws from independent loops overlap;
- splittable and stateless in its finalizer: `mix` is a `const fn`, which the CAS gear table
  uses at compile time.

## Speed of light

A draw is a dependent chain of `add` (1 cycle) then `mix` (3 shift-xor, 2 multiply: about
3 x 1 + 2 x 3 = 9 cycles of latency), but only the add carries across draws, so a loop of
draws issues at one per 2-3 cycles on a 4-wide core, about 1.5-2 G draws/s; `fill` writes 8
bytes a draw, about 12-16 GB/s, near L1 store bandwidth for one core. `below` adds a 64-bit
divide (25-40 cycles on x86, 7-9 on Apple cores) where a multiply-high would cost 3. The
callers are tests and simulations whose loops spend far more per draw on the work they drive,
so `%` (bit-identical everywhere, what most copies did) is kept over Lemire's multiply-high;
no hot path draws.

## What moved

Per file, in `/tmp/blaze-lanes/v2-dedup-codec-rng.md` and the children's reports
(`v2-dedup-codec-rng-tools.md` for the `blaze-*` xorshift and LCG streams,
`v2-dedup-codec-rng-front.md` for the front-end crates). Rules applied:

- A domain wrapper keeps its methods over a `SplitMix64` field (`blaze-stencil`'s `value`,
  `blaze-ui`'s `pick` of a `Copy` item, the fuzzer's `Random`).
- A copy that pre-mixed its seed keeps that at the call site
  (`blaze-stencil`: `SplitMix64::new(seed ^ GAMMA)`).
- Multiply-high `below` (`blaze-stencil`, `blaze-v2-proof`, the fuzzer) moved to `%`: no test
  pins a drawn value. `blaze-stencil`'s 24 fixed fuzz seeds now generate other programs.
- `blaze-cas`'s chunk gear table is part of the chunk format: it is `mix` over `GAMMA` steps
  from the same seed (the same 256 words), and a test pins its first three words and checks
  the whole table against `SplitMix64`.

## What stays, and why

- `blaze-core`'s `sim::Rng`: public (`sim::Program` takes it) and `blaze-core` stays below
  `blake3` (as [dedup.md](../dedup.md) keeps `blaze-radix` separate), so it can't depend on
  `blaze-ids`. Tests elsewhere that borrowed it as a plain random source moved off it.
- `blaze-radix`, `blaze-check`, `blaze-mem`: `blaze-ids` depends on them.
- `harness/` and `demo/`: standalone programs blaze builds as test inputs, outside the
  workspace.
- Hash multipliers (`x * 0x9e37_79b9_7f4a_7c15 >> shift`, Fibonacci hashing) are not RNGs.
