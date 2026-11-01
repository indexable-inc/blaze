---
status: record
summary: Step 2 of the locks brief (2026-11-01) - each shared map, locked collector and locked queue either moved to papaya (blaze_hash::SharedMap), crossbeam's SegQueue/ArrayQueue or par's ordered results, or kept as a lock, with the reason.
date: 2026-11-01
---
# Shared maps, collectors and queues (locks brief, step 2)

The choices are AGENTS.md rule 9 (`standard-structures`): a shared map is `papaya::HashMap`
(`blaze_hash::{SharedMap, SharedSet, SharedByHash}`, on blaze's fixed-seed and identity hashers), a
multi-producer queue is `crossbeam_queue::{SegQueue, ArrayQueue}`, and the results of parallel items
come back from `blaze_graph::par` in order. A site stays a lock where the structure can't do what
it does: hand a value out by move, hold the lock across IO on purpose, keep an invariant over
several entries, or iterate while other threads write.

A papaya `pin()` guard is never held across a scheduler wait (rule 22): every converted site clones
what it needs out (an `Arc`, a `Copy` value) and lets the guard go before it waits or compiles.

## Converted

| Site | Was | Now | Why it fits |
|---|---|---|---|
| blaze-codegen `flight.rs` `Flights::{table, summaries}` | one `Mutex<HashMap>` each | `SharedByHash<InstanceKey, _>` | per key and independent: claim is `try_insert`, land a `compute` that checks the round, release a `remove_if`; a wait reads without a lock |
| blaze-codegen `flight.rs` `owners` | `Mutex<HashMap<u64, String>>` | `SharedMap` | read only to name a wait |
| hir-ty-codegen `counted.rs` `compiles_of` | `Mutex<HashMap<Crate, usize>>` | `SharedMap<Crate, AtomicUsize>` | one `get_or_insert_with` and `fetch_add` per compile |
| hir-ty-codegen `flight.rs` `LIVE` | `Mutex<HashMap>` | `SharedMap` | insert and remove per compile; a slow compile's name is cloned out |
| hir-ty-codegen `debuginfo/types.rs` `ByEnv` | 64 locked shards of nested maps | one `SharedMap<(env, key), V>` | looked up with a borrowed pair (`Equivalent`), so a hit copies nothing |
| hir-ty-codegen `imports.rs` `EXPORTS` | 64 locked shards | `SharedMap<Key, Arc<OnceLock<..>>>` | the slot's `Arc` is cloned out before the export is built |
| blaze-macro-eval `tier/build.rs` `IMAGES` | `Mutex<HashMap<ContentId, Cell>>` | `SharedByHash` | compile-once cells; the guard is gone before the compile |
| blaze-macro-eval `expander.rs` `sessions` | `Mutex<HashMap>` | `SharedMap` | get-or-create of each crate's session handle |
| blaze-doc `places.rs` `elsewhere`, `roots` | `Mutex<HashMap>` | `SharedMap` | memos; a racing duplicate computes the same value |
| blaze-registry `sources.rs` `registries`, `directories` | `Mutex<HashMap>` | `SharedMap` | opening a registry reads nothing, so a lost race drops a spare and takes the winner's |
| blaze-registry `registry.rs` `read` | `Mutex<HashMap<String, Arc<Read>>>` | `SharedMap` | single flight per index file: `try_insert` picks the reader, `remove_if` with `Arc::ptr_eq` forgets only its own read |
| blaze-registry `sparse.rs` `fresh` | `Mutex<HashSet<String>>` | `SharedSet<String>` | membership tests and inserts, asked with `&str` |
| blaze-lazy `runner.rs` `Runs::senders` | `Mutex<HashMap<u32, Sender>>` | `SharedMap` | an answer sends through the sender in place: no `Arc` clone per answer, and `stop` kills without collecting |
| blaze-graph `par.rs` `gave` | `Lock<Vec<usize>>` | `SegQueue` | pushed by many workers, sorted once at the end |
| blaze-graph `pump/table.rs` `retired` | `Lock<Vec<Box<Table>>>` | `SegQueue` | pushed under a grow, freed with the table |
| blaze-core `cgroup.rs` `SPARE` | `Mutex<Vec<Vec<u8>>>` | `ArrayQueue` of `SPARES` | a bounded free list: a full queue drops the path |
| blaze-profile `NOTED`, `STEPS`, `THREADS` | `Mutex<Vec>` | `SegQueue` (const) | push from any thread, drained in push order when a part is written |
| blaze-cargo `serve/job.rs` `Paths::back` | `Arc<Mutex<Vec<Image>>>` | `Arc<SegQueue>` | pushed by compiles that give way, drained by `take_images` each compile with no lock |
| blaze-cargo `run/inproc.rs` `Failures` | `Arc<Mutex<Vec<String>>>` | `Arc<SegQueue>` | errors pushed from serving threads, drained once a run |
| blaze-lazy `runner.rs` `unbound` | `Mutex<Vec>` | `SegQueue` | failed binds, drained at close |
| proc-macro-api `kept/tally.rs` `free` | `Mutex<Vec<&Block>>` | `SegQueue` | blocks given back at thread end, taken by the next thread |
| proc-macro-api `kept/puts.rs` `ORPHANS` | `Mutex<Vec<Put>>` | `SegQueue` (const) | each ending thread's puts keep their order |
| base-db `db.rs` `EventLog::{events, executed}` | `Mutex<Option<Vec>>`, `Mutex<Vec>` | `SegQueue` | pushed by the query hook while logging, drained once |
| hir-ty `consteval/deps.rs` `pending` | 16 locked shards | one `SegQueue` | drained once by the driver, which sorts by key |
| blaze-v2-proof `tree.rs` `Found` | four `Mutex` collectors under `par::for_each` | `par::map` results | each directory's listing and each file batch's hashes come back in order; the error reported is the first in the walk's order, not the first thread's |

## Kept as a lock

| Site | Why |
|---|---|
| blaze-inproc `host.rs` `waiting` | entries are taken out by value (OS handles); papaya lends references only |
| blaze-inproc `host.rs` `written` | held across the file write so two writers of one path don't interleave |
| blaze-link `target.rs` `found`, `asked` | a handful of entries, held across the one toolchain ask per triple so it is asked once |
| blaze-lazy `runner.rs` `spare` | popped and pushed once a lone hit, which `budget.rs` holds to zero allocations: a `SegQueue` takes a new block every 31 pushes, and the asking threads have no bound to size an `ArrayQueue` by |
| hir-ty-codegen `backend.rs` `alone` | one thread's own map, a `BTreeMap` for the report's order, taken whole |
| hir-ty-codegen `flight.rs` `DONE` | a bounded top-k of the slowest compiles (replaces the fastest when full); only compiles over `SLOW` take it |
| hir-ty-codegen `llvm/pgo.rs` `GIVEN` | weak entries, retained on each give; a few profiles |
| blaze-registry `sparse.rs` `config`, `login_url`, `auth_error_headers` | one value each, swapped whole |
| blaze-macro-eval `expander.rs` `LIBRARIES` | weak registry scanned and retained by `flush` |
| proc-macro-api `kept/tally.rs` `blocks` | walked by `Tally::get` for a sum |
| blaze-core `fsevents.rs` `flushes` | a few entries, removed by value |
| blaze-core `cgroup.rs` `READYING` | serializes a one-time setup that does IO |
| blaze-inproc `sys/macos/rendezvous.rs` `PENDING` | entries taken out by value (ports) |
| blaze-cargo `serve/plane.rs` `RAISED` | entries removed by message, not only first in |
| blaze-cargo `serve/unit.rs` `later`, `seeds`; `unit/deps.rs` `puts` | read in place or replaced whole by one owner |
| blaze-cargo `sandbox/windows.rs` `JOBS`, `windows/reads.rs` `roots` | looked up and iterated under the lock |
| blaze-profile `flight.rs` `RINGS`, `owners`, `PARKED` | a sorted list searched by `partition_point`, and a slab indexed in place |
| blaze-query `db/storage.rs` `lru` | the sweep needs the whole list exclusively |
| base-db `db/executions.rs` `RAN`, `ORDER` | only while tracing; `ORDER` is deduplicated against `RAN` under one lock |
| blaze-graph `exec.rs` `SPARE` | searched by pool size, taken only when a build starts or ends |
| blaze-graph `par.rs` `panic` | keeps the first panic; a payload is not `Sync`, so no `OnceLock` |
| blaze-graph `pump/sharded.rs` | the test oracle and the bench's lock baseline |
| test-only collectors and fake stores | they record what ran for the test to compare |
