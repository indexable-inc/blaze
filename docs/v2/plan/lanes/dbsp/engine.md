---
status: plan
summary: Lane dbsp's generic delta engine (crates/blaze-dbsp) and base-db's maintainer hook (crates/base-db/src/maintain.rs): the state of the art each piece matches, how it is laid out, and the cost of each operation.
date: 2026-10-30
---
# dbsp: the delta engine and the maintainer hook

Design: [../../design/dbsp.md](../../design/dbsp.md) §3 and §13. Bounds:
[speed-of-light.md](speed-of-light.md). Code: `crates/blaze-dbsp/` and
`crates/base-db/src/maintain.rs`.

## What each piece matches

| piece | state of the art | ours |
|---|---|---|
| `ZSet<T>` | DBSP's Z-sets (Budiu et al., VLDB 2023): a multiset with integer weights; `+`, negation and consolidation | one `Vec<(T, i32)>`, sorted and summed by `consolidate`. A push in value order onto a consolidated set keeps it consolidated, so a delta built in order costs no sort |
| `diff_seq` | Myers/patience diff for text; Hack's and Flow's per-file fact diff | the op logs are multisets keyed by position, so an edit distance is not needed: equal prefix and suffix skipped, then a multiset diff of the middle by FxHash and `==` |
| `ReadIndex<K, R>` | Hack's reverse dependency index; salsa's and rustc's dependency edges, but kept both ways | two interned sides with inline lists and a pair map that gives each edge's place in both lists |
| `Arrangement<K, S, V>` | differential dataflow's arrangements (McSherry et al., CIDR 2013): an indexed, consolidated trace shared by operators | one sorted run of `(sort, value, weight)` per key, and the keys whose visible values changed |
| `closure` / `Closure` | DRed's over-delete (Gupta, Mumick, Subrahmanian, SIGMOD 1993) | a BFS with a kept visited set; the output buffer is the queue |
| `Worklist<T>` | semi-naive evaluation's frontier | a ring and a set |
| `SliceHasher` | BLAKE3 (`blaze-ids`'s `Hasher`) | length-prefixed rows under a domain |
| `Db::batch`, `Maintainer` | DBSP's circuit step, one per input transaction | one maintenance pass per write or batch, maintainers in `order` |

Differential dataflow arranges by key into batches merged LSM-style, because its traces hold
many timestamps. A maintainer keeps one version: the current one. So an arrangement here is one
consolidated run per key, and there is nothing to merge.

## Costs

`n` is the size of what is touched, `k` the size of a diff's middle. Every lookup is one FxHash
probe and allocates nothing (each has an allocation-budget test in `crates/blaze-dbsp/src/tests.rs`).

| operation | cost | notes |
|---|---|---|
| `Pos::child` | copy of the path | inline to depth 6, no allocation |
| `ZSet::push` | `O(1)` amortized | |
| `ZSet::consolidate` | `O(n log n)`, `O(n)` on sorted runs, `O(1)` when consolidated | pdqsort, then one pass summing and dropping zeros |
| `diff_seq` | `O(prefix + suffix)` compares, then `O(k log k)` | one FxHash per middle item, a sort of `(hash, index)` rows, a merge. The scratch rows are thread-local and kept, so a warm diff allocates nothing. A 200-op module with one changed op: about 400 compares, 0.4 µs |
| `ReadIndex::add` | two interning probes and one pair probe | idempotent: the pair probe finds the edge |
| `ReadIndex::forget(r)` | `O(keys of r)` | swap-remove from each key's list, one pair fix-up per moved edge |
| `ReadIndex::readers` / `keys` | one probe, then a contiguous scan | |
| `Arrangement::update` | one probe, a binary search, and a shift of the key's later rows for a new or vanished row | the published slice for that key is rewritten whole and hashed anyway, so the shift is within the same `O(rows of key)` |
| `Arrangement::get` / `contains_key` | one probe, then a contiguous scan / `O(1)` | |
| `Arrangement::take_changed` | `O(changed keys)` | a key counts as changed when a row crosses between positive and not |
| `Closure::run` | `O(nodes + edges)`, one set probe per edge | warm, it allocates nothing |
| `Worklist::push` / `pop` | one set probe each | |
| `SliceHasher` | the bytes once, about 1 GB/s per core on small rows | |
| `Db::with_state` | two probes of the state map (take, put back) | the placeholder left while the state is out is a zero-sized `Box`, so nothing is allocated |
| a write outside a batch | the write, a content compare (two loads), then one pass | a write that changes nothing runs no pass |

## The hook

- Every write method of `Db` runs in a `batch`, notes what it changed (a file whose content ID
  changed, a root and the files that came into it or left it, the crate graph when its content
  ID changed, an included file that was read again), and the outermost batch runs one pass.
- A pass sorts and dedups the pending lists, takes them out of the database, runs every
  maintainer in `order` over the one `Written`, and puts the cleared buffers back: a warm pass
  allocates nothing in the hook.
- Writes made while the maintainers run are not noted and start no pass: a maintainer's inputs
  are its output.
- A panic in a write or a maintainer drops every maintainer's state and marks the crate graph
  changed, so the next pass is the cold run. Dropping one crate's state alone would need the
  maintainer to catch its own panic; the hook can't tell crates apart.
- `Db::report_work` keeps `(pass, maintainer, crate, Work)` records only while
  `Db::keep_maintained(true)`, for the daemon's `blaze-maintained` line and the cost tests.

## Where it can fall short of the bound

- `Arrangement::update` on a key with very many rows (a trait like `From` with thousands of
  impls) shifts rows. The publication of that key's slice is `O(rows)` already, so this costs
  no more than a constant factor; if a profile shows it, the run becomes a B-tree per key.
- `diff_seq`'s middle sort is `O(k log k)`; a hash map would make it `O(k)`. For the middles an
  edit makes (a few ops) the sort is faster; a reformat of a whole file makes `k` large once.
