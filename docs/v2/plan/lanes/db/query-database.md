---
status: plan
summary: blaze v2's query database as lane db wrote it: constructive traces keyed by input hashes, a fingerprint on every edge, views as immutable roots, ordered verification, the plane's single flight, cold packs, the daemon image and the settle swap, the rerun explainer; and the front end's changes in salsa and hir-*.
date: 2026-10-29
---
# blaze v2: the query database (lane db)

Design notes for [the db lane](../db.md), as written on 2026-10-29. The code is
`crates/blaze-query/`; the front end's changes are in `vendor/salsa*`, `crates/base-db`,
`crates/hir-expand`, `crates/hir-def`, `crates/hir-ty`, `crates/hir` and the borrow checker.

## Memos: constructive traces

- A key (`QueryKey`: kind and the content ID of its arguments) keeps **every memo made of it**,
  each with its edges in the order they were read and the fingerprint each had then. There is no
  revision and no durability.
- In a view, a memo is the key's result when every edge, in order, has the fingerprint it saw.
  Verifying stops at the first that differs (#350): a key read after a changed one may not exist
  any more, and since a query is deterministic, every edge up to the first difference is one its
  execution would read too, so verifying never does work execution wouldn't.
- At most one memo of a key holds in a view (the first differing edge of two memos is the same
  key with two fingerprints), so the order memos are tried in is only a matter of speed: newest
  first.
- A memo of another producer (the code of the query's function, `Query::PRODUCER`) or value
  schema is never reused (#1439).

## Views

- A view is a root, which never changes, so what a view resolved stays resolved: each view has
  a table of key to memo, input to value, and why each key it ran did. Opening one is constant
  time; closing one retires its tables.
- Inputs are the only per-view queries: `FILE` (the file's content ID), `DIR` (the hash of its
  sorted names, not of what is under it), `CONFIG` (the bytes the view was opened with). The
  database reads them through a `Tree` (`file`, `read_dir`), a pure function of a root that the
  VFS implements over its immutable nodes (`blaze_vfs::Reader`). `read_dir` lends a
  directory's names sorted, and the database hashes them as lent, stopping (naming the root
  and directory) at a name out of order. A view keeps a directory's names only once one of
  its queries reads them (`Cx::dir`), so verifying an edge to a directory copies nothing.
- A view opened is sized like the view opened last, if still open: each shard of its result
  and input tables gets a table that holds as many entries as that view's shard does, so a
  view of the same tree resolves and verifies with no table grown and nothing allocated
  (`verifying_in_a_new_view_of_the_same_tree_allocates_nothing`,
  blaze-units' `a_warm_load_allocates_nothing`).
- A query reaches what is no input (a code generator's front end and backends, the daemon's
  memo of templates) through `Cx::context::<T>()`, which the database's owner gave with
  `Database::provide`, one value a type. No edge records it, so a context must change no
  value; what does (a build's policy) goes in the key or the view's configuration.
- A database view is not a VFS view: the daemon opens one per build configuration of a VFS
  view and keeps a root's last one open past its VFS view (the last plan, and `Request::Why`
  through the session's `kept`), so the database names its views itself.
- What-if views (#276) are views of roots the VFS made without moving a head; best-of-N is N of
  them at once, sharing every memo whose inputs are equal.

## Single flight and cancellation

- One run at a time holds a key, whatever view asked. The single flight is the plane's
  (`blaze_graph::pump::Flights`, keyed by `QueryKey::task_key`), so a query's wait is one of the
  plane's waits, in the one wait graph that also holds every job pumped above a waiting task.
  The others wait, pumping, and then verify the memo it made in their own view.
- A wait that would close a cycle is refused with `Cancelled` (#1301): through other tasks'
  waits, or through a job pumped above a waiting query on its thread (a job that can't go on
  until that query does). The run that would close it unwinds, letting go of what it holds, and
  its request is reposted. `blaze_query::Cancelled` is the plane's `Cancelled`.
- A key asked again on its own run's stack is a query cycle and panics naming the chain.
- A job pumped under a waiting query is a task of its own and may ask the database. Only a
  query's own code may not: `Pinned::get` (from `Database::pin()`) inside `Query::execute`
  panics (ask through `Cx::get`, which records the edge).
- `Cx::get_all(keys)` asks independent keys at once (a workspace's member manifests):
  - It gives each key's value, with the same edges in the same order as a loop of `Cx::get`,
    so verification and `why` see what a sequential caller makes.
  - When the view has resolved every key, it is one results lookup a key and allocates only
    the returned list.
  - From the first key the view hasn't resolved, each later unresolved key is a job of the
    plane's pool (`blaze_graph::exec::shared()`, in a scope) that resolves it ahead. Meanwhile
    the asker asks every key in order and waits, as a `Cx::get` does, for one a job is
    running.
  - A job ahead never waits (`Flights::run_if_idle`). Any key another task runs, its own or
    one it reads, makes it give way at once, and the asker meets that key in its turn. The
    asker's wait for the scope's jobs is no edge of the wait graph, so a job that waited
    could close a cycle no check sees.
  - The first key that is cancelled or panics ends the call with that once the jobs have
    ended. What they resolved stays as memos.

## Keys by bytes

A memo's edge names a dependency by its hash only. To verify a memo whose dependency has no
valid memo in the view, the database runs the dependency by its key's bytes (deterministic CBOR),
kept beside its memos and in the image (`Query::Key: Serialize + DeserializeOwned + 'static`). A
dependency it can't run (a kind no one registered, a fact recorded with `Cx::read` that no query
answers, a key that no longer decodes) makes the memo unverifiable, never valid: the query runs
again.

## Values: memory, disk, evicted

- A value is its bytes in the zero-copy format (`Persist`), at a 16-byte boundary, read in
  place. Its default fingerprint is the first 128 bits of the hash of those bytes, which
  `Query::fingerprint` is given as written, so a run writes its value once.
- `collect(kept, effects)` keeps what an open view or a kept key reaches (an edge reaches the
  memos of its key with the fingerprint it saw) and drops the rest. Reached values no open view
  uses are written to a cold pack (`<dir>/cold/<n>`, one atomic write), mapped, checked value by
  value against their content IDs, and then read from the mapping (#806, #734).
- `evict(key)` drops values kept elsewhere (compiled code once it is in the store): the
  fingerprint and edges stay, so cutoff still works, and a value asked for again is made again
  and must have its fingerprint, or the query is named as not deterministic.
- A kind with `Query::SHARE` keeps equal values once, whichever view or key made them: a memo
  of a value with the same schema and fingerprint as one already kept (its bytes compared; a
  mismatch panics naming the query) names the same value row. A shared row counts the memos
  naming it. Evicting, moving to a cold pack or freeing one memo lets go of its count and keeps
  the others' value. A collection retires a shared row once no memo names it. Other kinds pay
  nothing. A cold pack and the image write each distinct value once, for every kind, and every
  memo of it maps that one copy.
- Nothing a reader may hold is freed under it. Every reference the database hands out borrows a
  task's `blaze_reclaim::Guard`: `Cx` carries its task's guard, and outside a query
  `Database::pin()` gives a `Pinned` that holds one. A row, table, value, memo or view let go of
  is retired with a `blaze_reclaim::Grace` and freed (or its row reused) once every task that
  could have loaded it has ended. `collect` sweeps what is past its grace, and `&mut Database`
  (`reclaim`, `settle`) frees everything at once.

## Memory: rows, segments, no allocation on a hit

Everything is a `u32` row of a column in one reserved region that never moves (`table::Rows`
over `blaze_mem::Region`, tagged `Tag::Query`: 64 GiB of address space, pages committed as
rows are written), so a row's address is base + index × size and nothing is boxed per entry.
The tables every hit reads (memo lines, slots, a view's results) are on huge pages. The
registered kinds are a dense table by kind number (`Kinds`), so finding a query's kind probes
no hash.

| Table | Row | Per element | Lookup |
| --- | --- | --- | --- |
| A map on query keys (`table::Map`: slots, inputs, a view's results, inputs, reruns) | `Entry { key, value }` | the row (64 bytes, one line, for a view's result: the memo's ID and fingerprint), at most 16 bytes of places, a live byte | shard table pointer (its length in the top bits), ~1.5 place loads, one row read; no hash computed |
| Memo, hot (`memo::MemoHot`) | fingerprint, value word, a seqlocked copy of the value's address and length, edge range, code | 64 bytes, one line | one indexed load |
| Memo, cold (`memo::MemoCold`) | key, the edge segment it holds | 48 bytes | one indexed load, writers only |
| Value (`memo::ValueRow`) | bytes, length, segment, content ID | 64 bytes + 4 of generation + its bytes | one indexed load after the word |
| Edge (`table::EdgeRecord`, also the image's layout) | kind, input, fingerprint seen | 56 bytes in its memo's range | read in place, sequentially |
| Slot (`memo::Slot`) | key bytes span, up to 3 memo IDs inline or a list span | 64 bytes, + 4 a memo past 3 | in a map, as above |

- A map's place is one `u64`: a 32-bit tag (bits 64..96 of the key's input, mixed with the
  kind) and the row + 1. The shard is picked by bits 0..32. A `QueryKey`'s input is a content
  hash, so the key is indexed by its own bits and never hashed again. A replaced or removed
  entry's row is retired to its shard's free list and handed out again only after its grace.
- A memo's value location is one word: the value row and that row's generation, bumped each
  time the row is claimed. Evicting, refilling and moving to a cold pack are compare-and-swaps
  of the word, so a cold write that copied a value swaps it only if the word still names that
  very value, however often the row was reused since. Readers take no count.
- Edges, values (at `blaze_format::ALIGN`), key bytes and long memo lists are copied once into
  a segment of the thread that made them (`segment::Segments`: per-lane bump segments, 16 KiB
  growing to 1 MiB for edges, 4 MiB for values). A query's edges go onto its thread's edge stack
  as it reads them and are copied with its result as one range; a run's key stack is the same
  thread-local stack, so asking from outside a query allocates no run. A segment is freed whole
  at `collect` (or `&mut`) once it is sealed and no row holds a span of it: never per memo.
- Memos are freed in two graces: unlinked from their slots at one `collect`, checked against
  the open views at a later one (a view's results name memos by ID), and freed after a grace
  begun at that check. A memo a cold write will swap is pinned until the pack is mapped.
- A warm hit (`Pinned::get` or `Cx::get` of a key the view resolved) is: the kind's entry, the
  view's column slot, one map lookup and the memo's hot line, which holds a copy of where the
  value's bytes are; the value's row is read only when that copy is being rewritten or stale.
  It allocates nothing (`tests::a_warm_hit_allocates_nothing` counts with a test allocator):
  the key's bytes are encoded and an input's path is copied only on a miss.
- Verifying a memo walks its edge range in order, stopping at the first edge whose fingerprint
  differs. Each edge is one lookup in the view's results (or inputs), made before anything
  else; a result holds its memo's fingerprint, so the memo's line is not read. The lookups 8
  and 4 edges ahead are prefetched (place, then row), so misses overlap.
- No atomic read-modify-write on the write path but the publish: each lane's free lists are
  its own (no lock), a row's generation is a plain store by its claimer, and an open
  segment's hold count carries a bias that its lane settles with its own count at seal.
- Collection marks memos in bitsets sized by the store's IDs, not hash sets, and a cold pack
  is checked against the value still in memory by a `memcmp`, not a second hash.
- The hot line's copy is a seqlock rewritten after each swap of the word. The swap's
  compare-and-swap, the rewriter's load of `seen`, its store of `seen` and its reload of the
  word are SeqCst: under acquire and release, a second swap that sees the copy being rewritten
  and the rewriter's reload could both read the old value (store buffering), and the copy would
  stay stale until the next swap.
- The crate's synchronization types come from `sync.rs`: `std`'s, or under `--cfg blaze_check`
  the model checker's ([safety/model.md](../safety/model.md)), and segments report each copy,
  read and free of their bytes to it. Model tests (`model_*`, in `table/tests.rs`,
  `memo/tests.rs` and `tests.rs`) check `Map` growth and update, `Column` pushes, the value
  word's evict / refill / cold-write swaps and the inline copy, segment holds and sealing,
  `add_memo` racing `collect` (a key given a memo after the bump keeps it), and `INSIDE` per
  thread.
- Per-component bounds and how far off each is: [speed-of-light.md](speed-of-light.md),
  "blaze-query".

## The benchmark: the fastest known way to do each piece

Nothing has been built or measured yet. These are design comparisons, to be checked by
`blaze-query`'s benchmarks once the tree builds.

- **Incremental verification.** The fastest known are rustc's red-green dependency graph
  (`try_mark_green`: a node's dependencies, a range of one flat `u32` edge array, walked in
  order, stopping at the first red) and salsa (`deep_verify_memo`: inputs walked in order,
  skipped wholesale by durability). blaze-query matches rustc's walk: one contiguous range
  per memo, in read order, stopping at the first difference. It beats both on what can be
  reused. Every memo of a key is kept (constructive traces), so going back to an earlier
  edit, or to another view, reuses its results where rustc and salsa recompute. Views share
  memos without a revision, and every edge carries the fingerprint it saw, so cutoff happens at
  every edge, not only at a node's result.
  Where it falls short: a rustc edge is a 4-byte `DepNodeIndex`, resolved through a
  previous-to-current index map once per session; a blaze edge is 56 bytes, a content-hashed
  key plus its fingerprint, and each edge costs one hash-table probe in the view's results. To
  match: intern query keys to dense `u32` key IDs per database (an append-only table like
  `blaze_ids::Sharded`). An edge becomes `(key ID, fingerprint)`, 24 bytes, and a view's
  results become a dense column by key ID, one indexed load an edge. The image would keep
  content keys and map them to IDs as it reads them in.
- **The lock-free table.** The fastest known are papaya (Rust; lock-free reads and writes,
  incremental resize, `seize` reclamation), folly's `ConcurrentHashMap` (lock-free reads with
  hazard pointers, writers locking a segment) and Cliff Click's `NonBlockingHashMap` (open
  addressing, compare-and-swap on each slot, cooperative resize). blaze-query matches folly's
  model: wait-free reads, writers sharded by a short lock. It beats folly's read path, because a
  read here publishes nothing: folly writes a hazard pointer per read, while here one guard
  covers a whole task. A probe is one load of a tagged word, as in Swiss tables' metadata
  bytes. Where it falls short of Click and papaya: writer progress. A writer descheduled while
  holding a shard's lock stalls that shard's writers, though there are 4× the cores of shards
  and the lock is held only for a few stores. To match: insert and replace by compare-and-swap
  of the place, which is already one `u64`, and grow cooperatively with a forwarding mark in
  the place, Click's "prime" state.
- **Allocation.** The fastest known are bump arenas (rustc's `DroplessArena`, `bumpalo`) and
  mimalloc's per-thread segments and pages with free lists sharded by thread. blaze-query
  matches a bump arena's alloc path: a thread-local load, a bump, a `memcpy` and a lane-local
  count. Its rows are reused from per-lane free lists claimed 64 at a time, as mimalloc's
  pages are. Where it falls short of mimalloc: fragmentation. A segment
  stays while one memo in it lives, and nothing is compacted. To match: a compacting `collect`
  under `&mut` (at `settle`, or when a segment's live fraction drops) that copies live memos'
  edges and values into a fresh segment and rewrites their hot rows.
- What is still slower than it could be, on the miss path only: a run writes its value into a
  buffer its thread reuses (`Writer::with_buffer`, up to 8 buffers of at most 1 MiB kept a
  thread), and the store then copies it into its segment, so a value is written once and
  copied once. `QueryKey::task_key` hashes the key for the
  plane's single flight, once per miss. An input's `Tree::file` gives the VFS's own `Bytes`
  (its `Arc`, or its range of the mapped image), one count per file per view: a view's bytes
  are kept owned because a database view can outlive the VFS view it was opened for.

## The daemon image and the settle swap

- The database is five sections of the daemon's one image (`blaze-image`, restore.md), each
  with a schema of its own: memos sorted by key (192 bytes each, `DB_MEMOS`), edges (56 bytes
  each, `DB_EDGES`), an open-addressed table from key to its first memo (`DB_TABLE`), the
  inputs (`DB_INPUTS`), and a blob of values (16-byte aligned), key bytes and paths
  (`DB_BLOB`), which records name by offset. `write_image(builder, live)` writes them into the
  daemon's `Builder` and marks each symbol a value names (the kind's `Persist::symbols`) in
  `live`, so the symbol table the daemon writes last keeps it. Every kind with a memo must be
  registered (`DbError::Unknown` otherwise, before anything is written). The same database
  writes the same bytes.
- `from_image_with(config, image)` takes the daemon's `Arc<Image<Mapping>>`. It first checks
  that the symbol table was restored from this image (`intern::restored_from`, else
  `DbError::Symbols`), then finds the five sections in the spine (one missing or of another
  schema is `DbError::Refused`) and reads only the inputs. A memo is read the first time its
  key is asked, with its edges and value left in the mapping; each group is checked by its
  CRC32C the first time a read reaches it. A start is an mmap. On any of these errors
  (`DbError::is_image`) the daemon starts empty.
- A group that fails its CRC (`blaze_image::Corrupt`) costs only the keys whose memos it held:
  each gets an empty slot, so it is computed again and its dependents verify against the new
  fingerprint as after any miss, and the group is never read again. Each such group is
  reported once (`take_corrupt`), by section and group. The inputs are read whole at start, so
  a corrupt input or path starts the database empty (`DbError::Corrupt`). Bytes that pass
  their CRC but are not what `write_image` writes (a table slot past the memos, edges past the
  edge section) are a bug in the writer and stop the database naming it.
- A file of a restored VFS tree is a range of the same image, and `Tree::file` is `Err(Corrupt)`
  when its group fails. That stops the run as a cancellation does, keeping nothing in the
  view. The request fails with `DbError::Corrupt`, naming the section and group, never as a
  file that is not there. Once the daemon mends the file, the next request reads it again.
- The settle swap (#1295, #1299): after a cold build the daemon writes the image and maps it;
  `settle(image, effects)` makes the database the image's, carrying open views (each resolved
  memo matched to the image's equal one), registered queries and pending IO, and drops every
  arena of the cold build at once. Its symbols are this process's, so it skips the symbol
  check.

## Why did this rerun

`why(view, key)` answers for a key that ran in the view: the first edge of its newest earlier
memo that differed, then for that key the same (from its own rerun record, or by comparing the
memo with the old fingerprint against the view), back to an input, named as the view names it.
A key that was verified, not run, has no answer; one that ran for the first time has an empty
chain.

## The front end on salsa

The front end's queries stay `#[salsa::tracked]` on `vendor/salsa`, remade by the same rules:

- A memo keeps a value fingerprint through eviction, each read records the fingerprint it saw,
  and an evicted value that recomputes to an equal fingerprint is backdated, so the LRU caps no
  longer break early cutoff. No backdate when durability drops (#350).
- Interned values and symbols are never freed (`crates/intern`), so a `Hash` of their address
  is a sound fingerprint within one process. Salsa's fingerprints are not persisted.
- One `Sync` database shared by reference: no per-thread handles, no `fork`; parallel work (the
  def-map collector's expansion rounds, `SourceDatabase::run_all`) runs as plane tasks.
- The check (`hir::check`) matches rustc's parallel front end (`par_hir_for_each_module`, one
  module per rayon task) and beats it twice. A run of small modules is one plane task: 32
  items or more, except the last, cut in def-map order so the cut is deterministic. And the
  diagnostics come out in def-map order whatever the threads did, which rustc's don't.
- Well-formedness (`hir-ty-wfcheck`) matches rustc's `check_well_formed` under
  `enter_wf_checking_ctxt`. It builds one inference context per item, and only for an item
  with aliases to normalize. Outlives components live on the stack. The result is a zero-copy
  record (`WfResultRef`): 24-byte rows with the text after them.
- The borrow checker: [borrowck.md](borrowck.md).

## Not in `blaze-query`

- Scheduling: a query runs inline on the task that asks it, and its waits pump. The plane
  (`blaze-graph`) schedules requests' roots and admits them against `admission::Estimates`;
  wiring query kinds to task kinds is the coordinator's at integration.
- The store: cold packs are the database's own files. Values the store already holds (code) are
  evicted, not packed.
