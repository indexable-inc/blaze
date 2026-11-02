---
status: plan
summary: mem-db-salsa's plan to move vendor/salsa onto blaze-mem; each run of a query's code in a frame task of its tag (the mem_tag option), the per-run temporaries and which are reused, what of the tables still comes from the global allocator and the moves onto regions, the per-call scratch on hit and verify paths, the budget tests and the speed-of-light bounds.
date: 2026-10-29
---
# mem-db-salsa: salsa onto blaze-mem

Part of `mem-db` ([db.md](db.md), whose **contract** this programs against). The paths
(`vendor/salsa`, `vendor/salsa-macros`, `vendor/salsa-macro-rules`) belong to lane `db`, so every
change is a request: M1 to M9 in `v2/requests/mem-db-salsa.md`, each with the code before and
after and the test that holds it. Speed-salsa's audit
([speed-salsa.md](../db/speed-salsa.md)) is applied and is the base: lock-free hits, the shared
edge array and node table, the 24-byte memo header, the claim-free verify.

## Each run in a frame (M1, M2, M4)

The user's code runs in four places, all inside the claim: `C::execute` (every run and every
fixpoint iteration), `C::cycle_initial` (the first provisional value, and every value of a
`FallbackImmediate` cycle) and `C::recover_from_cycle`. Each is a `blaze_mem::frame::task` of
`Configuration::MEM_TAG`, a constant that defaults to `Tag::Query` and is set by
`#[salsa::tracked(mem_tag = Infer)]` (methods too: tracked impls pass options through). The
argument is made outside the task (it is owned); the result can't borrow the frame
(`frame::task`'s closure type), so a memoized value is owned, as contract item 4 says. The
user's `Eq` and `Hash`, called by backdating, run outside any task.

`frame::task` keeps eight spare frames per thread and maps a new 4 GB frame for a task nested
deeper; salsa's runs nest as deep as the query stack (50 to 300). M4 asks lane mem to run a
nested task in the thread's frame between a marker and its reset: one frame per thread at any
depth. salsa's code is the same under either design, and `tests/mem_frames.rs` holds both to
the contract.

## salsa's own per-run temporaries (M3)

| temporary | where | made | reused per thread? |
|---|---|---|---|
| the query stack, a `Vec<ActiveQuery>` | `active_query.rs:473`, in `ZalsaLocal` | grows to the deepest stack once | yes: slots reset, not dropped |
| a run's edges, `FxIndexSet<QueryEdge>` | `active_query.rs:45` | per run | yes: drained into the shared edge array, capacity kept |
| a run's edge fingerprints, `ThinVec<EdgeFingerprint>` | `active_query.rs:49` | per run that reads an `lru` function | **no**: moved to the memo and shrunk, regrown by doubling next run; M3 keeps the buffer and gives the memo an exact copy |
| disambiguators, tracked struct identities | `active_query.rs:61`, `65` | per run that makes tracked structs | yes: cleared, table kept |
| capture frames | `capture.rs:248` | per `capture_reads` | yes: `spare` frames |
| cycle heads, `CycleHeads` | `active_query.rs:78` | only while a cycle iterates | owned: moved into the memo |
| accumulated values | `active_query.rs:70` | per run that accumulates | owned: moved into the memo |
| a cycle head's flattening sets | `execute.rs:941` `FLATTEN_MAPS` | per cycle iteration | yes |
| a cycle head's flattened fingerprints | `execute.rs:991` | per cycle iteration | **no**: `collect`ed; M3 fills a pooled buffer and copies it exactly |
| unfound cycle heads, `SmallVec<[_; 4]>` | `execute.rs:663` | per cycle head completion | inline up to 4 |

The edges a run records became the memo's revisions with S5 (a range of the shared array), so
nothing per edge is allocated.

## Tables (M5, M6)

The memo slots and rows are on regions already (`table/memo.rs`). Still from the global
allocator, and moved:

- the table's page list (`table.rs:29`, a `boxcar::Vec`) onto a `Column<Page>` tagged `Query`,
  which also takes a dependent load (the bucket pointer) off the memo hit;
- each page's 128 slots (`table.rs:599`, a `Box` per page) onto one of two bump arenas over a
  reserved `Region`: `Query` for inputs and tracked structs, `Intern` for interned values
  (`Slot::MEM_TAG`);
- the edge and node pages (`graph.rs:183`, `alloc_zeroed`) onto a kernel-zeroed `Region` each.

Kept, with the reason on each (M5's table): setup-time ingredient data and memo ingredient
indices (immutable, a few hundred bytes per ingredient), the interned and node hash tables'
growth (a shard's first tables are 128 bytes; a region each would be a whole page), the miss
path's reusable-value lists, a task's last-page map, the free page lists and the tracked
struct free list (deletion only). IDs are unchanged (page index by push order, `page << 7 |
slot`), and nothing observable depends on where a page's bytes sit.

## Hit and verify paths (M7)

A memo hit, a claimed verify, a claim, backdating and recording a read allocate nothing warm.
The one per-call heap use left is the claim-free walk's stack (`maybe_changed_after.rs:392`,
eight frames inline), which spills and regrows on every verify of a chain deeper than eight; M7
lends the walk a stack the thread keeps.

## Budget tests (M8)

In `vendor/salsa/tests/alloc_budget.rs` (which installs `blaze_alloc_count::Counting` as its
global allocator): the memo hit (existing, plain, `lru`, multi-argument, inside a query), a deep
verify of a twelve-memo chain that changes nothing (0, new), and the warm steady state of a
re-check, an edit a query absorbs and backdates (0, new). `tests/mem_frames.rs` checks the tag
of each run, that a nested run leaves the outer frame's bytes and tag as it found them, and
that fixpoint iterations and both cycle functions run in frames.

## Speed of light

- **A run's wrapper**: two thread-local accesses and a tag store, about 3 ns, against the
  cheapest query run's few hundred nanoseconds (claim, stack push, edge record, memo insert):
  under 1%. A temporary in the frame costs an align, an add and a compare (about 2 ns) against
  15 to 25 ns for a small allocation and its free; the bound is the temporaries' bytes written
  once to warm memory. Past depth 8 today's `frame::task` costs an `mmap`, an `munmap` and a
  page fault per page touched (microseconds per run), which is why M4 is needed to reach it.
- **A run's fingerprints**: the memo needs its `16 + 20n` bytes (owned until memos persist), so
  the bound is one allocation and a `20n`-byte copy; M3 reaches it, from `log2 n + 2`
  allocations and `log2 n` copies of the growing buffer.
- **Slot pages**: per 128 slots one compare-and-swap and, once per quarter of the arena's
  growth, a commit system call; the bound is the page faults of the bytes written, which both
  the old `Box` and the arena pay. Per slot that is under 1 ns of bookkeeping.
- **A memo hit**: speed-salsa's bound is two lines; after S5 it takes five, four deep. The
  `Column` page list leaves the page's address computed from the column's base after one load
  of a ready word that 64 pages share and nothing writes after publication, so the page no
  longer waits on a bucket pointer: four lines, three deep.
- **A deep verify that changes nothing**: speed-salsa's per-edge bound (one decode and the hit
  check, as rustc's `try_mark_green`), plus a frame of about 50 bytes pushed and popped in L1 per memo,
  and no allocation once the thread has walked a chain that deep.
- **Graph pages**: one `mmap` per page, at most 20 per array, as `alloc_zeroed` of pages this
  large already costs in glibc and mimalloc; they are now counted under `Tag::Query`.

The fastest known designs these follow: rustc's `VecCache` and `try_mark_green` for hits and
verification, game-engine per-frame linear allocators (one marker per nested scope) for the run's
temporaries, and reserved, never-moving virtual-memory columns (as in mold's and wild's output
buffers) for the tables.
