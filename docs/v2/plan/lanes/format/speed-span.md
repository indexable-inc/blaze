---
status: plan
summary: speed pass on the format lane's span, token tree, macro and interner crates; benchmarks named, before and after.
date: 2026-10-29
---
# speed-span: interning, spans, token trees, macro_rules

Crates: `intern` (here), and through children `mbe` ([speed-span-mbe.md](speed-span-mbe.md)),
`tt` and `syntax-bridge` ([speed-span-tt.md](speed-span-tt.md)), `span` and `const-ast`
([speed-span-ast.md](speed-span-ast.md)).

## intern: the store under every `Symbol`, `Interned<T>` and `InternedSlice<T>`

Every name, path and type of the front end goes through it (hir-ty's `Ty`, `Const`, `Region`,
predicates and generic argument lists are `Interned`/`InternedSlice`; names are `Symbol`s), so
lookups run on every core at once and must neither lock nor write a shared line
(docs/v2/plan/design/concurrency.md).

**Benchmarks.**
- Abseil's SwissTable (`flat_hash_map`): one control byte a slot in 16-byte groups matched with
  SSE2, slots in a separate array, load up to 7/8. A hit reads the group's control line, the
  slot's line and the value.
- papaya: lock-free reads and inserts, incremental resizing; reads enter an epoch guard (a
  thread-local write).
- lasso's `ThreadedRodeo`: dashmap shards behind `RwLock`s, so every lookup is an atomic
  read-modify-write on its shard's lock word, the line every reader of the shard shares.
- boxcar: append-only chunks of doubling size that never move.

**Before** (format-span-intern's version): 64 shards fixed; per shard one struct holding the
table pointer and a `std::sync::Mutex`, 64-byte aligned only by accident of layout, so a writer
taking shard A's lock dirtied the line readers of A (and its neighbours) load the table pointer
from. A table was control words (8 tags in a `u64`) in one array and 16-byte slots
(hash, value pointer) in another: a hit read the control line, the slot line and the value.
Values in boxcar-style never-moving chunks (kept). No lock and no write on a read (kept).

**Now** (`crates/intern/src/arena.rs`):
- Shards: `available_parallelism() * 4`, next power of two, at least 16, chosen once per
  process for every store (concurrency.md "Writers shard by hash"; concurrency-audit finding 1 in
  `v2/requests/format.md`). More shards also shorten the copy a growing table does under its lock.
- Reads and writes on separate lines: readers load a packed array of table pointers (stored
  only when a table grows, a few dozen times in a shard's life); writers lock `InsertLock`s padded
  to 128 bytes in their own array. A read writes nothing.
- One cache line a probe group: `Group` is 64 bytes, a control word with 7 tag bytes and the 7
  value pointers (`const` size assertion). A hit reads the table header (cached), one group line
  (at half load almost always the home group) and the value; one line fewer than SwissTable and
  than before. Matching 7 tags is SWAR on one `u64` (one load, four ALU ops), as wide as the
  group, so SSE2/NEON would gain nothing here.
- Hashes move to a column read only when a table grows (still no rehash on growth); the read
  path no longer compares a stored hash, since a tag hit (1 in 128 for a foreign value) goes to
  the caller's `Eq`, which a true hit runs anyway.
- The lock is a named type (`InsertLock`) whose comment bounds what runs under it, the only
  `Mutex` in the crate (concurrency.md "Where a lock is truly needed").
- Cost: 9.1 bytes of table a slot plus an 8-byte cold hash, at a quarter to a half load: 34 to
  68 bytes a value besides the value. Lookups: 3 dependent loads plus `Eq`, 0 writes, 0 locks.

**Against the benchmarks.** Reads beat lasso (no RMW, no shared line) and match papaya (no
guard to enter, since values are never freed) and SwissTable (one line fewer a hit, at a lower
load factor, so more memory). Inserts still take their shard's lock, where papaya claims a slot
with a CAS: with 4 shards a core, two writers meet in one shard rarely, and the lock is held
only to look again, bump-allocate, write the value and publish. Matching papaya's lock-free
insert would take slot claiming by CAS on the control word (with a "being written" tag that
readers wait out for an equal value), a lock-free bump per shard, and incremental cooperative
migration on growth; that is worth doing only if `perf c2c` shows shard locks in a profile.
What's left out, as before: NUMA-local and per-worker placement (needs core's scheduler), and
huge pages on macOS (none for arm64).

Tests (`arena.rs`): shard count scales with cores and locks are 128-aligned; the table against a
`HashMap` model over seeded hashes chosen to collide in tag, group and shard (4 seeds); 16
racing writers over seeded values agree on one address per value; SWAR match finds every equal
slot byte and never byte 7; colliding hashes grow one shard; tags and groups tell values apart;
lock-free find after insert; the bump allocator aligns and never overlaps.

## Unsafe (docs/v2/plan/design/unsafe.md)

- `intern` is unsafe by nature (raw arena memory, published pointers) behind a safe API; every
  block is one operation with its own `// SAFETY:` (the safety-audit items in
  `v2/requests/format.md`, applied), except `symbol.rs`, handed to fix-orderkeys while it edits
  that file. The safe reference is the `HashMap` model in `arena.rs`'s tests.
- Model checking: `crates/intern/src/sync.rs` switches `arena.rs`'s atomics and lock to
  `blaze-check`'s under `cfg(blaze_check)`; `model_racing_inserts_across_growth` runs two writers
  racing across a table growth and a lock-free reader, against the sequential model. `Table::new`
  zeroes pages in a normal build (std atomics are plain words) and makes each atomic in a model
  build. The manifest line waits on `blaze-check` becoming a workspace dependency
  (`v2/requests/speed.md`, "speed-span" item 1).
- `mbe`, `tt`, `syntax-bridge` and `const-ast` forbid `unsafe_code`; `span` denies it outside its
  one module (`hygiene.rs`, salsa ids).
