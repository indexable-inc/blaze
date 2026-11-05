---
status: plan
summary: dedup-intern (2026-10-29): one SwissTable probe and one u32 index table, `blaze_mem::swiss` (re-exported as `intern::swiss`), under intern's arena and mapped index, salsa's node and interned maps, blaze_ids::Sharded, the hash-conser and base-db's tables; the interners that differ on purpose and why; the speed of light of a lookup.
date: 2026-10-29
---
# dedup: interners and their hash-table probes

## What was copied

Two table designs, each written several times:

| design | copies | now |
|---|---|---|
| SwissTable groups: a control word of tag bytes beside 7 or 8 slots, probed group by group | `intern/src/arena.rs` (7 pointers), `intern/src/table.rs` (8 IDs, mapped from the image; two loops), `vendor/salsa/src/graph/nodes.rs` (7 `u32` nodes), `vendor/salsa/src/interned/table.rs` (7 pointers, with dead slots) | `blaze_mem::swiss::{Ctrl, find, vacant, tag, live_tag, DEAD}` |
| leveled open addressing of `u32`/`u64` "places", lock-free find, inserts under the shard's lock, grown into the next level | `blaze_ids::Sharded`, `blaze-hashcons/src/table.rs`, `base-db/src/table.rs` (which says it copies `Sharded`) | `blaze_mem::swiss::Index` |

`blaze_ids::Interner` is the trait; `Sharded` and `HashCons` implement it, and neither is a
copy of `intern`'s interners (below).

## Where it lives, and why not in `intern`

The dedup table puts interner probes in `intern`. The probe went to `blaze-mem` instead and
`intern` re-exports it as `intern::swiss`: salsa already depends on `blaze-mem` (a leaf with
`blaze-reclaim` under it), while depending on `intern` would put `blaze-image`, `blaze-core`,
`blaze-format` and zstd's C build ahead of salsa on every clean build. And `blaze-ids` sits
below `intern` (intern depends on it), so `Sharded` could only share a table placed under both.

## The probe

`Ctrl<SLOTS>` is one `u64` of tag bytes; `SLOTS` is 7 where a group's slots are pointers (word
plus seven pointers is one 64-byte line) and 8 where they are `u32`s. `matching(tag)` is exact,
`!(((x & 0x7f..) + 0x7f..) | x) & 0x80..` with `x = word ^ tag * 0x01..`: no borrow false
positive, so the four copies' per-hit re-check of the byte is gone. `find` and `vacant` take a
closure that loads a group's word, so each table keeps its own atomics (std, `blaze_check`,
shuttle) and orderings, and the image's mapped `&[u64]` uses the same probe. The persisted tag
(`0x80 | hash >> 57`) is unchanged, so `intern`'s image layout is too.

SWAR over one word, not SSE2/NEON over 16 bytes: a 16-tag group can't keep its slots in the
same line as its tags, and a hit is then a second line. On a lookup that hits the home group
(at three quarters load, about 90 percent of them) the whole probe is one line.

## `Index`

Groups of a control word and eight `u32`s, 40 bytes; level `k` has `2^k` groups; it fills to
three quarters and grows by copying into the next level, published with one release store;
outgrown levels stay until `release_outgrown(&mut)`. It stores no hash: growing asks the caller
(`hash_of`), which every user can answer from its column (a content ID's bits, a stored hash, a
fingerprint). It allocates nothing until the first insert.

| | before | `Index` |
|---|---|---|
| `Sharded` (u64 places, at most half full) | 16 to 32 bytes a value, about 1.5 place loads a find | 6.7 to 13.3 bytes a value, one word and one slot of one 40-byte group |
| `HashCons` (u64 places, half) | 16 to 32 | 6.7 to 13.3 |
| base-db `Table` (u32 places, half) | 8 to 16, a value compared per place probed | 6.7 to 13.3, a value compared only on a tag match |

## Speed of light

A lookup of a value present must load the table's line holding the value's slot and the value
itself (to compare): two dependent cache lines, about 2 × 80 ns from DRAM or 2 × 4 ns from L1,
plus the hash. The table's part is one line when the home group holds the value; with 8 slots
at three quarters load, the chance a probe needs a second group is about the chance the home
group is full, under 10 percent at the average load of 9/16 between growths, so the
expected table lines per hit are at most 1.1. That is the bound, and the probe hits it: one
acquire load of the level (a line shared by every reader and written only on growth), one of
the control word and the slot in the same line, then the caller's compare. A false tag match
costs one extra value line with probability about (slots compared) / 128 ≈ 4 percent, within
the 10 percent. A miss costs the same group line and no value line. Inserts are bounded by the
caller's lock and the copy on growth, amortized to under two slot writes an insert.

The ALU part is 6 operations for `matching` and 2 for `empty`, under 5 ns: memory-bound.

## Interners that differ on purpose

| interner | why it stays |
|---|---|
| `intern::{Interned, Symbol, Mapped}` | the shared interners: by value, for the process; `Symbol`/`Mapped` IDs stable across an image |
| `blaze_ids::Sharded` | keyed by a content ID the caller computed (it hashes nothing), owns values of any type and drops them, dense per-run `Idx`; now on `Index` |
| `blaze-hashcons` `HashCons` | keyed by fingerprint, panics on a collision, sorted output; requested onto `Index` |
| base-db `Table` | values keyed by a caller's hash, held by `Files`; requested onto `Index` |
| `vfs` `PathInterner` | per-`Vfs`, `&mut`, `FileId`s in insertion order over `IndexSet`; the crate is replaced by `blaze-vfs` (crates.md) |
| `blaze-units` unit interner | cargo's `UnitInterner`: one graph build, `&mut`, profile compared outside the key; requested renamed `UnitInterner` with that comment |
| `hir-ty` `TyColumn` dictionary, `TyTableWriter` places, `tuple_field_accesses_rev` | one result or one inference, single-threaded, and the first-seen order is the persisted format (`from_parts` checks it) |
| `blaze-vfs` `Names` | the VFS image's own persisted name table, one writer, carried across generations |
| `blaze-link` symbol shards | one link, `&mut` per shard, partitioned by the link's own threads |
| `blaze-cas` index | the CAS's on-disk 16-byte-group table with SSE2/NEON matchers, mapped read-only |
| `blaze-query` `Map` | removals and grace-period freeing of rows; a map, not an interner |
| `blaze-fmt` `Symbol` | not kept: its global `Mutex<HashSet>` is requested onto `intern::Symbol` |
