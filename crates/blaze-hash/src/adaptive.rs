//! Adaptive placement, for the hottest tables whose keys an attacker may pick (hir-ty's
//! interned kinds, the watcher's names): the fixed fast hash until the table sees a probe walk
//! no honest set of keys makes, then, once and for the rest of the table's life, SipHash-1-3
//! under a key drawn at that moment ([`switch_key`]), every key in the table placed again by
//! it.
//!
//! Why not a key from the start: on these tables the fixed hash's lead is LLVM folding its
//! seed into the code, which no secret key allows. A hit in hir-ty's kinds costs 9.0 ns in
//! cache and 26 out of it under the fixed hash, 15.7 and 45 under SipHash, and more again under
//! foldhash with a seed loaded at run time (`interning_cost` in hir-ty, 2026-11-03). Honest
//! inputs never switch, so they keep the fast path; a flood switches the table after a bounded
//! amount of work, so it can't make an insert walk every key before it.
//!
//! The switch is sticky: a table that has seen a flood stays keyed, as whoever sent it can send
//! it again. Nothing output may depend on which hash a table uses (AGENTS.md rule 25): a key's
//! id is the table's counter, never where it lands.
//!
//! The signal, per kind of table:
//!
//! - **Linear probing at most half full** (hir-ty's index): a walk of [`refused_run`] full
//!   slots from a key's home. The bound is the run length whose odds under a random hash are
//!   under 2^-40 for the table's size, so an honest table switches with odds under 2^-23 over a
//!   process (summed over the about 22 sizes a shard grows through, 64 shards and 64 tables).
//! - **hashbrown's SwissTable** (the watcher's names), whose walk isn't visible: compares of
//!   keys that weren't the one asked for, of which a lookup makes about 0.1 (a full slot's 7-bit
//!   tag matching by chance). [`FLOODED_COMPARES`] of them in one lookup switch. A flood that
//!   shares only the home group and not the tag still pays: past 128 keys in a run some tag
//!   repeats, so one lookup in about every 128 more compares one more key, and the bound is
//!   reached by about `128 * (FLOODED_COMPARES + 1)` keys in one run.
//!
//! Prior art, the same idea elsewhere:
//!
//! - .NET's `Dictionary<string, _>` starts on a non-randomized string comparer and, when one
//!   insert walks more than `HashCollisionThreshold` (100) entries of a bucket's chain, swaps
//!   in the randomized (Marvin) comparer and rehashes every entry (`Resize(.., forceNewHashCodes:
//!   true)`). `ConcurrentDictionary` does the same since dotnet/runtime#81557, keeping the
//!   comparer on the tables object it swaps atomically so readers see the hashes and the
//!   comparer that placed them together: here, the two sets of shards and the `keyed` flag.
//! - Rust's adaptive hashing (rust-lang/rfcs#1796, contain-rs/hashmap2#5, pczarn): a fast hash
//!   until an insert's displacement passes 128, then SipHash. std took only the half that grows
//!   the table early on a long probe (rust-lang/rust#38368), dropped with the move to hashbrown;
//!   the idea of a second, keyed table for later inserts is bill-myers' in rust-lang/rust#11783.
//! - Java 8's `HashMap` turns a bucket of more than 8 entries into a balanced tree; Java 7u6's
//!   alternative hashing switched `String` keys to a seeded murmur3 past a capacity threshold.
//! - Go (a per-map random seed for its AES-based hash) and Python (SipHash-1-3 since 3.11,
//!   PEP 456) key every table from the start, as Rust's `RandomState` and hashbrown's default
//!   foldhash do: no fast path, no switch.

use crate::SipKey;

/// The shortest walk (full slots stepped past from a key's home, the key not found among them)
/// that a linear-probing table of `2^log_slots` slots, at most half full, refuses as a flood.
///
/// A walk of `k` slots lies in a run of at least `k` full slots. A run of `m` slots preceded
/// by an empty one holds `m` keys whose homes are in it, where a random hash puts `m / 2` on
/// average at half load; Chernoff's bound gives odds under `e^(-r m)`, `r = α - 1 - ln α =
/// ln 2 - 1/2 ≈ 0.193` at `α = 1/2`. Summed over every start and every `m ≥ k`, a run of `k`
/// exists with odds under `2^log_slots e^(-r k) / (1 - e^(-r))`, under `2^(log_slots + 2.6)
/// e^(-r k)`; that is at most 2^-40 when `k ≥ (log_slots + 43) ln 2 / r`, about 3.589 for each
/// unit of the sum. A table's runs only grow while it keeps its size, so its last moment at a
/// size holds every run it had there.
///
/// 169 at 16 slots, 187 at 512, 252 at 2^27. A flood of keys with one hash is refused once
/// that many share a run: about `k^2 / 2` slots of work before the switch, against `n^2 / 2`
/// for `n` keys without it.
pub const fn refused_run(log_slots: u32) -> usize {
    ((log_slots as usize + 43) * 3589).div_ceil(1000)
}

/// Compares of keys that aren't the one asked for, in one lookup of a hashbrown table, that
/// switch it: about 0.1 a lookup by chance (up to 16 full slots a group, 7 bits of tag), so 8
/// in one lookup has odds about 2e-13 with a random hash (Poisson), and a table that makes
/// 10^10 lookups in a process switches by chance with odds about 2e-3, where it costs only
/// speed.
pub const FLOODED_COMPARES: usize = 8;

/// The key an adaptive table switches to: drawn by [`SipKey::random`], never shown. Under the
/// model checker (`cfg(blaze_check)`) a fixed key instead, as intern's tables do: the checker
/// runs a model many times and needs each run to place keys alike.
pub fn switch_key() -> SipKey {
    if cfg!(blaze_check) { MODEL_KEY } else { SipKey::random() }
}

/// [`switch_key`] under the model checker.
const MODEL_KEY: SipKey = SipKey::new(0x6164_6170_7469_7630, 0x6164_6170_7469_7631);
