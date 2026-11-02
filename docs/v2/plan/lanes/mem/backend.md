---
status: plan
summary: blaze v2 mem-backend - moving the back end's long-lived tables (vfs's directory table, the linker's, the store's, items', live patching's) and objects with IDs onto crates/blaze-mem, by lifetime, with what each costs against its bound.
date: 2026-10-29
---

# mem-backend: the back end onto blaze-mem

Lane mem's plan is [../../design/memory.md](../../design/memory.md). This child moves the back
end: `blaze-vfs` (here), `blaze-link` ([backend-link.md](backend-link.md)), `blaze-live`,
`blaze-splice`, `blaze-lazy`, `blaze-tier`, `blaze-in-place` ([backend-live.md](backend-live.md)),
`blaze-cas` ([backend-cas.md](backend-cas.md)) and `blaze-items`
([backend-items.md](backend-items.md)). `crates/vfs`, `crates/vfs-notify` and `crates/blaze-store`
are replaced at integration and are left alone.

## vfs: the directories outside every view (`blaze_vfs::outside::Dirs`)

What it was: directories in 20 chunks of `OnceLock` slots, each chunk a boxed slice made once per
doubling; each directory's path a `Box<Path>`; the index 21 boxed tables of `AtomicU64`, one
made per doubling. Four general-purpose allocations per doubling plus one per directory, all
justified as cold by `// alloc:`.

What it is: three `blaze_mem::AppendVec` columns, tagged `Tag::Vfs`, one writer (under the
adding lock) and lock-free readers:
- `entries: AppendVec<Entry>`: 32 bytes per directory (`since`, `changed`, `subsumed_by`, and
  the start and length of its path), at most \(2^{20}\) directories;
- `paths: AppendVec<u8>`: every directory's path bytes, one after another, 64 MB at most;
- `places: AppendVec<AtomicU64>`: the index's 16 tables end to end, table \(k\) of
  \(64 \cdot 2^k\) places from place \(64(2^k-1)\); growth rehashes into the next table and
  keeps the old ones for readers still probing them.

Publication order is the same as before: path bytes, then the entry (both by the columns'
release stores of their lengths), then the index place (release), then `Dirs::len` (release).
A reader that finds a place (acquire) sees the entry and the bytes; a reader that sees `len`
finds every directory below it. `Outside`'s per-directory watches are a `RegionVec<Watch>` by
directory number (never removed: a retired watch stays, its number is its tag), not a slab.

Cost against the bound. A check by path is, per ancestor, one acquire load of the level, one of
the places' length, the probe's load, one load each of the entries' and paths' lengths and a
byte compare: the same loads as the chunked `OnceLock` version (which loaded a chunk's and a
slot's state) on lines that stay in L1 between checks, so the speed of light of
[../vfs/speed-of-light-outside.md](../vfs/speed-of-light-outside.md) section 1 is unchanged;
a check through a `Cover` is still three loads. A `want` past the high-water mark costs its
path's bytes appended (one store and one release store each, under the lock), one entry, and a
commit system call per quarter of each column's growth; a doubling of the index rehashes its
places as before. No general-purpose allocation on any of them.

Tests (`crates/blaze-vfs/src/outside/tests.rs`):
- `wants_past_the_high_water_mark_allocate_nothing`: 5,000 wants past the entries' first commit
  step and through index level 8 make zero allocations; entries never move; each is found
  after, with zero allocations.
- `checks_wants_and_rounds_allocate_nothing` (kept): warm reads, covers, wants of directories
  wanted or covered, a round and a change heard, zero allocations.
- `the_columns_answer_as_a_plain_table_does`: 3,000 random wants against a reference
  `Vec<String>`: the same directories in the same order, each found, each `subsumed_by` the
  first later directory above it.
- `model_a_directory_appended_is_found_whole_or_not_at_all` (model checker) and the kept
  `model_a_check_sees_a_directory_whole_and_vouches_only_after_it_is_confirmed` and
  `reads_race_with_wants_and_find_every_published_directory`.

## vfs: a watch's looks in flight

`Watcher::pending` (`crates/blaze-vfs/src/watch.rs`, frontend-journal's) is an
`FxHashMap<Tag, Pending>` of looks that come and go. They belong in a `blaze_mem::Slab`, the
tag's offset being the slot: requested exactly in `v2/requests/mem-backend.md` items 2 (lane mem:
`Slab::handle_at`) and 3 (frontend-journal: the change and its budget test).
