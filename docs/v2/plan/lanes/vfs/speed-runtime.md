---
status: plan
summary: blaze v2 vfs speed (speed-runtime-vfs) - the node store readers share without a lock, reclamation at task boundaries, commit, diff, journal and watcher costs, and the systems they are measured against (git's index and trees, Watchman, fanotify, Unison).
date: 2026-10-29
---
# blaze v2 vfs: speed of the runtime paths

What `crates/blaze-vfs` does on its hot paths after speed-runtime-vfs (2026-10-29), what each
costs, and what it is to match or beat. The design is [../vfs.md](../vfs.md) and
[../../../../v2-vfs.md](../../../../v2-vfs.md); the rules are
[../../design/concurrency.md](../../design/concurrency.md). Nothing here is measured yet (no
lane builds until every lane is done): the costs are counted from the code, and the benchmarks
below say how `proof` measures them.

## What changed

| Path | Before | Now |
|---|---|---|
| Node storage | `Vec<Node>` with an `FxHashMap<ContentId, NodeId>` re-hashing 32-byte keys, and a second copy of every node in `reader.rs` for readers | One append-only column store (`store.rs`) the tree writes and readers read: 32-byte ID, 8-byte shape, open-addressed index on the ID's own first 64 bits, half full at most, grown by a level |
| Directory entry | `(NameId, NodeId, Mode)` in a boxed slice per directory, names as `Arc<str>` (about 56 bytes with the name's allocation) | Three columns, `u32` name, `u32` node, mode byte: 9 bytes; names interned once as spans of one byte column, by a hash computed once (`hashbrown::HashTable`, entry API) |
| Read from another thread | `RwLock` read guard plus an `Arc` count per file (two shared writes) | `blaze_reclaim::enter` (thread-local count inside a plane task; else a store to the thread's own 128-byte slot and one fence), acquire loads, an index probe for the root and a binary search per component. `Reader::file` takes one `Arc` count; `Reader::pin()` lends bytes and names with none |
| Reclamation | `unsafe fn Vfs::reclaim` the daemon had to call at quiescence | A collection's old table goes to a `blaze_reclaim::Limbo` and is freed by the next commit, apply, flush or collect once every task that could read it has ended; no caller contract |
| Directory hash | Incremental `Hasher` calls per entry | One scratch buffer, one `ContentId::of`: the same bytes, so the same IDs (`"blaze v2 vfs directory 2"` unchanged) |
| Commit | Per-change path sorting by string (which also split a component's changes: `a/x` sorts after `a.b`) | Changes sorted by component, each directory rebuilt by one merge of its sorted entries with its change groups; untouched subtrees are reused by node (path copying), each new node interned once |
| Diff | Pushed every path then compared | Equal node or equal `NameId` skips first; a path is built in one buffer, nothing allocated per unchanged entry |
| Journal append | Batch bytes cloned for a retry | Bytes moved into the effect; a failed append or sync becomes one atomic rewrite holding every commit. Records framed in place (`frame`), written-node set a bitset |
| Watcher change | `Stat`, wait, then `ReadAll`: two round trips per batch; each read hashed to compare with the head | `Stat` and `ReadAll` asked together: one round trip; bytes compared with the head's in place, hashed once by the commit |

## Costs per operation

- **Read** (`Reader::file`, `Pinned::file`): inside a plane task, two thread-local counter
  writes; outside one, a store to the thread's own slot and a `SeqCst` fence. Then about 1.5
  index probes (32-byte compare each), one binary search of a name column per component
  (`log2(entries)` compares of interned bytes), and for `Reader::file` one `Arc` increment.
  No shared cache line is written (`perf c2c` in `proof` holds this).
- **Commit** of `k` changes at depth `d`: one BLAKE3 per new file, one per directory on the
  `k` paths (at most `k * d`, fewer where paths share directories), one index probe each; no
  hash of an unchanged subtree; a journal record per new node only (the written bitset).
- **Change batch** of `n` paths: `2n` effects in one submission (one round trip), one read
  of each file's bytes, a memcmp against the head's, one transaction.
- **Collection**: a bitset walk of what the live roots reach, a copy of those nodes into a new
  table (no rehash: the index is placed by the ID's bits), one release store.

## Benchmarks: what to match or beat

| System | Per read | Per commit | Per change |
|---|---|---|---|
| git's index and tree objects (`git status` with the untracked cache and fsmonitor) | an `lstat` per tracked file unless fsmonitor says unchanged; a tree object read is a zlib inflate and a parse | one SHA-1 (or SHA-256) per new blob and per tree on the path, plus a whole index rewrite | fsmonitor gives the changed paths; git still stats each and rehashes its bytes |
| Watchman | a query over its in-memory tree under a lock per root | n/a | one inotify/FSEvents event, then a stat (and optional SHA-1) per path, batched by settle time |
| fanotify (`FAN_REPORT_DFID_NAME`) | n/a | n/a | one event per change with the directory's handle and name: no stat needed for the kind |
| Unison | an archive lookup per path | a full archive write per sync | a scan of every file's stat (fastcheck) or bytes; no notification batching |

blaze's read is a lock-free probe with no syscall and no inflate, so it is to beat git's tree
read and Watchman's query by orders of magnitude per path. Its commit hashes only what
changed, like git's trees, but never rewrites an index of every file. Its change batch is
fanotify's events with one round trip of stat and read. It still stats each path, which
fanotify and FSEvents make unnecessary. `proof` measures: reads per second per core on
1, 16, 64 cores and every core (near-linear), commit latency for 1 and 1000 changes in a tree
of 100k files against `git commit` of the same, and the time from a disk change to the head
moving against Watchman's settle-to-query latency.

## What it would take to beat these further

- **No fence on the read path outside a task**: readers inside a plane task already pay none
  beyond the task's own entry. For readers outside one, asymmetric fences (Linux
  `membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED)`, Windows `FlushProcessWriteBuffers`) move the
  fence to the collector; this is blaze-reclaim's to offer.
- **No count per file read**: the database holding a view's bytes as lent through
  `Pinned::file` (v2/requests/vfs.md item 8, db's).
- **No stat per change**: a batched look effect and the change's kind from the driver
  (v2/requests/core.md, "From speed-runtime-vfs"). fanotify and FSEvents already say removed or
  directory.
- **Batched hashing**: BLAKE3's multi-input API over every new file of a commit at once, as
  wide as the core's SIMD lanes, instead of one call per file.
- **Mapped file bytes**: a large file read straight into the store's memory (or a mapping of
  it) instead of a `Vec` turned into an `Arc<[u8]>`.

## Unsafe ([../../design/unsafe.md](../../design/unsafe.md))

- All of it is in `store.rs`, behind a safe API. `lib.rs` denies `unsafe_code` everywhere else
  (`deny`, not `forbid`, so `store` alone can allow it).
- Each read is checked against a column length published with release: one acquire load and a
  compare, so a bad node number panics instead of reading uninitialized memory.
- Each push goes through the one `Writer` (`&mut self`) or a `Building` only the collection
  holds.
- A reader reaches a table only through a `Snapshot` that holds its reclamation task. What the
  snapshot lends can't outlive it.
- Why unsafe at all: the safe versions cost every read a shared write (a lock's count on each
  column) or every commit a copy of the tree.
- Proof:
  - `store/tests.rs` checks the columns, index and names against `Vec`, `FxHashMap` and
    `Vec<String>`.
  - `model_a_node_reads_whole_or_not_at_all` runs under blaze-check (`sync.rs` switches the
    atomics).
  - safety-fuzz-vfs fuzzes the journal's recovery.

## Tests

- `transactions_match_a_model`: seeded transactions (writes, edits, deletes of files and
  directories, renames) on paths with `.` and `-` beside `/`. Each root's ID is checked
  against the old incremental hashing of the model, along with its diff, its files and the
  reader's reads, through collections that renumber the table.
- `a_directory_deleted_and_written_under_again_keeps_its_dotted_sibling`: the grouping bug.
- `readers_on_many_threads_see_whole_roots_while_commits_and_collections_run`: four readers,
  each pinned to one root per read, while the writer commits and collects. Every root reads
  whole or as none, never as part of a tree.
- The watcher tests cover a read that comes back before its stat, and a read the stat made
  moot.
