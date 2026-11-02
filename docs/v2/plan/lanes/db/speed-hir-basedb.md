---
status: plan
summary: speed-hir-basedb: base-db's file, source-root and crate inputs read with no lock, views set with no work per unchanged file, hashes batched; before/after against rustc's SourceMap, Unison and blaze-ids' tables.
date: 2026-10-29
---
# speed-hir-basedb

Paths: `crates/base-db/`. Notes for the speed pass of what every query reads first: a file's
text, its source root, the root's files, the crate graph, and `parse`.

## What the hot paths are

- **Every query**: `file_text`, `has_file_text`, `file_source_root`, `source_root`,
  `resolve_path` (each `mod` and `include!`), `relevant_crates`, `EditionedFileId::parse`.
  These run on every worker, for every file, in every view.
- **What queries note**: `included_file` (`include_bytes!`, `include_str!`),
  `include_source`, `env_is_read`, `env_is_unset`. The first call for a key records it, and
  every later call is a lookup.
- **Every edit**: `set_view_root` with the whole view (most of its files unchanged), or
  `set_file_text` for one file.
- **Loading**: `set_source_root_with_durability` per root, `CrateGraphBuilder::set_in_db`.

## Benchmarks

- **rustc's `SourceMap`**: files sit in an append-only vector, looked up by
  `StableSourceFileId` through a map keyed by the already-hashed ID. Reads go through
  `rustc_data_structures::sync` locks (`FreezeLock` and `RwLock`), which are free in a
  single-threaded build but are real locks in the parallel front end.
- **Unison**: a definition is named by the hash of its content. Names are metadata, so equal
  code is one thing wherever it sits.
- **`blaze_ids::Sharded`**: append-only levels, reads by acquire loads, one writer lock per
  shard, values that never move.

## Before and after

### Reads: locks and shared writes per call

| read | before | after |
|---|---|---|
| `file_text`, `has_file_text`, `file_source_root`, `source_root` | 1 `DashMap` shard `RwLock` read (2 atomic RMWs on a shared counter) | 0: two acquire loads (page, then word) |
| `resolve_path` | the above + an `Arc<SourceRoot>` clone and drop (2 RMWs) | 0 (`SourceRootInput::source_root_ref`) |
| `EditionedFileId::parse` | a `Parse` clone (2 `Arc`s: 4 RMWs on the file's lines) | 0 (`returns(ref)`) |
| `included_file`, already read | 1 read lock + 1 write lock, 2 `PathBuf` allocations; the write guard held across `std::fs::read` and a database call on a miss | 0 locks, 0 allocations: two lookups in lock-free tables; a miss reads and makes the input with nothing held |
| `env_is_read` / `env_is_unset` / `include_source`, already noted | 1-2 shard locks (a write lock for `env_is_unset`), 1-2 `String`/`PathBuf` allocations | 0 locks, 0 allocations |
| `on_disk` | `RwLock` read | 1 acquire load |

The four column reads now beat rustc's `SourceMap`: no lock in any build, and the dense ID is
the index, not a hash key. Lookups that take a key (`included_file`, the env tables) hash it
once with `FxHash` and probe a table that is read-only to readers, as `blaze_ids::Sharded`
does: same layout (shards by the top bits, levels that double, values in a `Log` that never
moves), same cost.

### Edits: work per file of a view

For a view of \(n\) files, \(k\) of them changed:

| | before | after |
|---|---|---|
| per unchanged file | `VfsPath` join (1 allocation), a clone into a seen-set (1 allocation and a hash of the whole path), 3 `DashMap` read locks (`by_path`, `in_roots`, `view_ids`) | 0 allocations, 0 locks: one `FxHash` of the relative path in the base's `ViewIndex`, two `u32`-keyed lookups |
| finding removed files | a pass over every path of the database (`DashMap` iteration, shard locks, a `starts_with` each) | a pass over the 4-byte marks of the base's paths |
| BLAKE3 per changed file | 1 bytes + 1 text ID + 1 bucket rehash (at every set, even for the same bucket) | 1 bytes + 1 text ID, each in one `of_many` batch; each dirty bucket once (at most 64) |
| BLAKE3 per root | 1 per `write_root_contents` call | 1 per root per edit, all roots in one batch |
| a file added or removed | the whole root re-indexed: a path key formatted and BLAKE3-hashed for every file of the root, all 64 buckets rehashed | the new file set is rebuilt (`FileSet` has no remove), but the root's content is updated in place: a path key and hash for the added files only (one batch), and only the dirty buckets |

So an unchanged file is compared, never hashed. That is the Unison property: identity is the
content ID, and the path is only the key that finds it.

`ContentId::of_many` (lane format's) derives the domain key once per batch and then hashes each
input serially. It does not use `blake3::platform::hash_many`'s SIMD lanes: format judged that
API (`#[doc(hidden)]`, "for benchmarks only") unsafe to build on. Batching here still saves one
compression per input (the key derivation), and a SIMD `of_many` would speed these call sites
up with no change to them. The inputs are 33 to 41 bytes for text IDs and 16 + 40 bytes per
file for buckets, which is where 8- or 16-lane hashing pays most.

### Bytes per file input

| | before | after |
|---|---|---|
| text and source-root inputs | 2 `DashMap` entries: 16 bytes each plus about 2 bytes of control and the shard's slack | 2 × 8 bytes in pages of 4096 IDs, made when an ID in them is first set; 16 bytes of directory per page (32 KiB per column) |
| the roots that have a file | `Vec<SourceRootId>`: 24 bytes plus a heap block | `SmallVec<[SourceRootId; 1]>`: 24 bytes, no heap block for the usual one root |
| a view base's index | none (every path was scanned) | per path under the base: its relative text, its file (4 bytes), a 4-byte mark |

## What changed (files)

- `src/table.rs` (new): `Column<T>` (dense paged salsa input IDs, `const` size check on the
  page slot), `Log<T>` (append-only chunks, `FIRST << k` slots in chunk `k`), and `Table<T>`
  (open-addressed `u32` places per shard, never more than half full, grown by moving to a table
  twice the size; the old table stays for readers still probing it).
- `src/files.rs` (new; `Files` moved out of `lib.rs`): reads on columns; the setters' own
  state in one `Mutex<Writer>` that no query takes; what queries note on tables; `views` as
  immutable lists in a `Log`, published by one release store of their index.
- `src/view.rs`: `ViewIndex` per base (made once from `by_path`, then kept by
  `Writer::note_path`); planning that only reads (bytes verified in one batch, refused with
  nothing written); writing with text IDs in one batch and root contents updated in place.
- `src/input.rs`: `CratesMap` is a `Mutex<FxHashMap>`, locked once per `set_in_db` (only the
  setter reads it).
- `src/content.rs`: `cfg_options_content` encodes every atom into one buffer (it was one
  `Vec` per atom); `path_key` writes the path into the key directly (it was a `String`, then a
  copy); `opt_bytes` and `hash_ranges` support the batches.
- `src/editioned_file_id.rs`: `parse` is `returns(ref)`.
- `src/lib.rs`: `SourceRootInput::source_root_ref`; `resolve_path` uses it.
- `Cargo.toml`: `dashmap` out, `smallvec` in (already a workspace dependency).

Hash domains are unchanged: every batch hashes exactly the bytes the `Hasher` calls hashed
before (bucket: count, then each key framed and its ID; root: `is_library`, then 64 IDs; text:
the vfs ID, then the framed appended module).

## What is left, and why

- `all_crates` returns an `Arc<[Crate]>` clone (2 RMWs per call). Its callers are in other
  lanes, and most read it once per crate graph walk, not per node. A `&[Crate]` return is
  the follow-on if a profile shows it.
- `Crate::data`, `cfg_options` and `env` are salsa input fields read by reference already: no
  lock and no clone.
- `FileSet` (crate `vfs`, lane vfs) has no remove, so a root that gains or loses a file
  rebuilds its file set (a clone of each path). That is \(O(\text{root})\) per add or remove,
  and nothing per edit of an existing file. A persistent map in `vfs::FileSet` would make it
  \(O(\log n)\).
- Views of agents' what-ifs share structure only through salsa's inputs. A view is set into
  the one database, so two views that differ in a few files cost the files that differ, not a
  second copy of the inputs.
- `set_view_root`'s errors are checked in two passes: paths, duplicates and source roots while
  planning, then the bytes in one batch. A view with both a duplicate path and a mismatched
  file now names the duplicate whatever their order; before, it named whichever came first.
