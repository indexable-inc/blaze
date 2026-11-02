---
status: plan
summary: lane item-hash-store's speed of light: the item store's entry points with no store, a hit, an offer and a flush, their arithmetic and how `hir_ty::body_store` reaches them.
date: 2026-10-30
---
# item-hash-store: speed of light

For `crates/hir-ty/src/body_store.rs` and `body_store/`, sections 7 and 8 of
[design/item-hash.md](../../design/item-hash.md). The identity, a hit's decode and an offer's
classification are bounded in [item-hash/speed-of-light.md](../item-hash/speed-of-light.md);
this note covers what the store itself adds. Machine as there: one ~4 GHz core, BLAKE3 at
~3-6 GB/s, mapped store pages in the page cache; the shared machine table is below.

| path | bound | arithmetic | how it is reached |
|---|---|---|---|
| no store installed (every `infer_body_query`, `borrowck`, `gate::checked`) | ~1 ns | one relaxed load of a shared, read-mostly cache line and a predicted branch | `INSTALLED` checked before anything else; `stored` and `lookup_passed_in` are `#[inline(never)]` so the hot caller keeps the load and branch only; `capture` with no store runs the step directly. Budget test: zero allocations |
| finding the database's store | ~5-20 ns | `blaze_reclaim::enter` plus at most 16 pointer loads and nonce compares, one cache line each | a fixed `[AtomicPtr; 16]` registry, no lock, no map |
| a hit's keys | ~0.3 µs | base key, slot key, answer key: three BLAKE3 calls of 100-200 bytes (one block chain each, ~60 ns) plus one 32-byte update per fact; corrected below: five keys and four domain lookups | one `Hasher` stream per key, no intermediate buffer |
| a hit's records | two store probes | shape then answer, each read in place from the `ImageBuf` (`open_as`, bounds checked, no copy) | `ShapeRef` iterates the mapped rows; the answer's body is a slice into it. A shape whose facts changed costs one more probe per slot tried, at most `SLOTS` = 4 |
| a hit's allocations | the decoded result's | what `InferenceResult` owns, plus the item list `items_of` builds (one entry per item named) | budget test `a_hit_allocates_only_the_decoded_result_and_no_store_allocates_nothing` |
| an offer | the answer's encoding plus one append | the facts sorted (f ≈ 5-30, `sort_unstable`), the shape written (72 bytes a fact), one uncontended lock and a `memcpy` of ~1-10 KB into the thread's shard (~20 GB/s: < 0.5 µs) | per-thread `Scratch` buffers reused across offers; 64 cache-line-padded shards, a thread always its own; nothing written to the store on the worker |
| a flush | store-bound | per step: up to 2 probes and 2 puts; the puts are queued (`Images::put` appends) and written by the loop | entries read in place from the shard buffers; written on the store's pool (`ImageStore::for_each`), one task a step, no shared write but each result's `OnceLock` |

## Where it is short of the bound

- `deps::Images::get` (lane cargo's adapter, `hir_def::deps::ImageStore`) allocates an `Arc` per
  get, so a production hit makes two small allocations besides the decode. Removing them needs
  `ImageStore::get` to lend a borrowed view under the store's guard instead of an owned
  `ImageBuf`: an interface change of lane frontend-deps.
- A hit hashes the step's base key again each time (`record::base_key`); salsa's memo of the
  inference makes that once a body a revision, below the noise of the decode.

## The machine (speed-sol2-small)

Figures from [link/speed-of-light.md](../link/speed-of-light.md) and
[hardware.md](../../design/hardware.md), not yet measured: 3.5 GHz, about 2 instructions a cycle
for branchy code, an L1 hit 4 to 5 cycles, DRAM about 110 ns, BLAKE3 of one lone block (a key of
up to 64 bytes) 60 to 80 cycles, about 20 ns. Reference workloads: a fresh daemon that serves a
workspace of 10,000 bodies from the store (every body a hit), and a body edit (one miss, one
offer, one flush entry).

## A hit, counted from the code (speed-sol2-small)

The table above counts "three BLAKE3 calls" for a hit's keys. The code makes more, so that row
is corrected here (2026-10-30, from `body_store.rs` `Store::lookup`, `find`, `answer_key_now`
and `record.rs`):

| step | work in the code | cost |
|---|---|---|
| `record::base_key` | one `Hasher::new(BASE_DOMAIN)` and 1 to 2 compressions (an identity, a byte, three 32-byte ids: 129 bytes, 3 blocks) | ~60 ns |
| per slot tried: `shape_key`, `shape_at` | two keys: `Hasher::new(SHAPE_DOMAIN)` over 36 bytes, then `QueryKey::id` (`Hasher::new(QueryKey::DOMAIN)` over 34 bytes) | ~40 ns |
| per slot: shape probe | `ImageStore::get` (lane cargo's `deps::Images`: a CAS probe and one `Arc::new`) | ~50 ns warm, one DRAM miss cold |
| per slot: answer key | `Hasher::new(ANSWER_DOMAIN)`, one 32-byte id per fact (f ≈ 5 to 30: 1 to 15 blocks) and a `BodyFacts::value` per fact (a salsa memo hit each, ~10 to 20 ns) | ~0.1 to 0.6 µs |
| per slot: `answer_at` and the answer probe | `QueryKey::id`, `ImageStore::get` (another `Arc`) | ~70 ns |
| decode | the step's own decoder over the mapped answer | the result's size |

Every `Hasher::new(&'static str)` also finds the domain's context key by FNV-1a over the name
(`blaze-ids` `context_key`, 26 to 30 bytes, one dependent multiply a byte: about 100 cycles,
30 ns) and a slot compare: five per first-slot hit, about 150 ns of a hit's ~0.5 µs before
the facts. That is the one part of a hit over its bound by more than 10%: the bound for the keys
is the compressions alone (seven blocks, ~140 ns), today's is ~290 ns, **2× off for the keys and
about 30% off for a hit of a body with few facts**. What removes it: `Hasher::new` finds a
domain's key by the name's pointer first (one acquire load and a compare, ~5 cycles), which
fixes all 251 callers at once and changes no hash: queue 00885, to `speed-sol2-format`, who holds
`blaze-ids` this round. Making the three domains in `record.rs` `blaze_ids::Domain` statics
(`Hasher::of`) would do the same for this file alone, but its path is lane item-hash's, which
is not done.

The fact values dominate a hit with many facts, and they are the bound: the answer key has to
read every fact's value now, one memo hit each.

## What `proof` should measure (speed-sol2-small)

- No store: `infer_body_query` with nothing installed: zero allocations (the existing budget
  test) and the time per call against the step's own time, under 1% more.
- A hit: per hit, BLAKE3 compressions (7 + one per two facts on the first slot), domain-name
  hashes (0 once the domains are statics; 5 today), store probes
  (2 on the first slot), allocations beyond the decoded result (0 once `ImageStore::get` lends a
  view; 2 today), and time per hit against `7 × 20 ns + f × 15 ns + 2 probes + decode`.
- An offer: allocations per offer once the thread's `Scratch` is warm (0), lock acquisitions
  (1, the thread's own shard) and bytes copied (the answer's size).
- A flush: store puts per step (at most 2) and probes per step (at most 2 + slots tried), and
  wall time against the store's write bandwidth for the bytes written.

## Fastest known

The shape-then-answer probe is the goal store's (`next_solver::goal_store`) one level up, and
Shake's and Bazel's constructive traces: one probe for the trace, one for the answer under the
trace's values, so a changed fact misses in one probe rather than by re-verifying every edge as
rustc's red-green graph does.
