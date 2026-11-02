---
status: plan
summary: How the concurrency lane made crates/blaze-cas meet design/concurrency.md on 2026-10-29: lock-free reads through a published view, append-only tables, per-worker write segments, reclamation at task boundaries, and the lock ban in clippy.toml.
date: 2026-10-29
---
# The store's reads and writes, made lock-free

The rule is [design/concurrency.md](../../design/concurrency.md): reads never lock or write
shared memory, values are immutable once published, memory is reclaimed at task boundaries,
and no lock guard lives across a query call. This is how `crates/blaze-cas` meets it.

## Reclamation: `crates/blaze-reclaim`

Quiescent-state reclamation, as in Linux RCU's QSBR flavour and the epoch reclamation of Silo
and Masstree: a reader pays nothing per read, because the task, not the read, is the critical
section. `enter()` returns a `Guard` (guards nest; the outermost one writes the thread's own
128-byte slot); a writer that unlinks a value starts a `Grace` and keeps the value in a `Limbo`
until every thread that was in a task then has left it. crossbeam-epoch pins per operation;
pinning per task makes the per-read cost zero, at the price that a task that never ends holds
memory (bounded: the store's limbo is collected only by the store's owner).

## Reads: `table.rs`, `read.rs`

- `Table<T>` is append-only open addressing over levels that double: a slot is published once
  by compare-and-swap and never moves, so a lookup is acquire loads and key compares. Only the
  store replaces a published value (`replace`), retiring the old one to its limbo. Each row
  has flag bits in words beside the slots, set with one relaxed `fetch_or` only when not set.
- A key lands in one level at most. An insert walks the levels oldest first. In a level at half
  full, it seals the empty slot that ends the key's run (a fixed address, never dereferenced)
  and moves on. Every inserter of a key meets that same slot, so one compare-and-swap decides
  for all of them. Before this, two racing inserts could land a key in two levels. The delta
  then kept a second spot that the commit never moved to its pack, and it pointed into a freed
  chunk; the model test `model_table_racing_inserts_of_a_key_across_a_new_level_keep_one`
  found it.
- `Shared` holds the `View` (packs, mapped index tables, delta tables, external files,
  dictionaries) behind one pointer the store swaps; a reader loads it once per lookup.
- A value made whole (decompressed or joined from chunks) goes once into the `Owned` table and
  is read in place after; the store swaps in a fresh table when it holds too much.
- `Held<'a>` is the proof a reference stays valid: `Held::task(&guard)` for a reader,
  `Held::writer(&store)` for the store's owner.
- `Reader` (from `Store::reader()`, `Send + Sync + Clone`) reads with `get`, `get_in_place`,
  `has` and `durable`, each taking the task's guard. LMDB's readers likewise take no lock;
  unlike LMDB they need no reader-table slot per transaction.

## Writes: `segment.rs`, the commit in `lib.rs`

- A worker puts through a `Writer` of its own (`Store::writer()`): it encodes the record into
  its own segment's chunk and publishes a `Spot` (key, content, place, ticket) into the tier's
  delta with one compare-and-swap. The ticket is `(segment << 40) | put`; it is durable once
  the segment's durable count passes it.
- A commit drains every segment into pack memory, then per pack `Truncate`, `WriteAt` (block
  aligned, so a driver may skip the page cache), `Sync`, then external files, the index and the
  head. Once a batch's packs are mapped, each spot is retargeted to its pack with one swap and
  the segment's chunks are retired. An index swap publishes a fresh delta; the old one stays
  readable as `prev` until a grace passes, then folds into the new one.
- RocksDB and WiredTiger share one log and serialise appends through a lock or a CAS on its
  tail; here no two workers write the same line until the commit, which is the only
  serialisation point and already is one (one sync per pack).

## Tests (`tests.rs`)

Under the sim driver: a task's reads stay whole and equal through random interleavings of
puts, flushes and single completions across index swaps, and are freed once the guard drops
(16 seeds); and threaded writers and readers keep going through commits, settles and index
swaps, after which every value is durable, correct and survives a reopen.

Under `cfg(blaze_check)` (`RUSTFLAGS="--cfg blaze_check" blaze test -p blaze-cas -p
blaze-reclaim --release model_`), each crate's `src/sync.rs` swaps std's atomics, `OnceLock` and
`thread_local!` for blaze-check's, and a segment chunk's bytes are tracked exactly, so `model.rs`
checks the table, the flag bits, the chunks and reclaim under every interleaving to the bound.
`enter`'s epoch store is `Release`: the model found that a relaxed one leaves a thread's
previous task unordered before a writer that reads it.

## The lock ban

clippy reads only the nearest `clippy.toml`, so each hot crate (`blaze-cas`, `blaze-reclaim`,
`blaze-query`; `blaze-ids`, `intern` and `blaze-graph` as requested in
`v2/requests/concurrency.md`) has a copy of the root rules plus `disallowed-types` for
`std::sync::{Mutex, RwLock}`, `parking_lot`'s and `DashMap`, citing concurrency.md.
