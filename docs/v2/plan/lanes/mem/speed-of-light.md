---
status: plan
summary: lane mem's speed of light: for each module of crates/blaze-mem, the work counted, the bound the hardware sets with its arithmetic, what the code does today counted the same way, the gap, and what proof's budgets should measure.
date: 2026-10-30
---
# Lane mem: speed of light

The bar is "Within 10% of the theoretical maximum" in `docs/v2/plan/goals/principles.md`. Each
section names the work, the bound with its arithmetic, today's count from reading the code,
the gap and what `proof` should measure. Sections marked `(speed-sol2-small-mem)` were added in
the second round; corrections to the first round's sections say so where they are. All figures
are from reading the code, none measured: the coordinator's benchmarks confirm them once
everything builds, and a gap over 10% there is a bug.

## The machine

The target machines are in [hardware.md](../../design/hardware.md): EPYC Turin, Graviton 5 and
Apple M-series. Unless a section says otherwise, the figures are these, not yet measured:

| what | figure |
|---|---|
| clock | 3.5 GHz |
| L1 read, streaming | 64 B a cycle |
| L2 read, streaming | 32 B a cycle (about 110 GB/s) |
| one core from DRAM, streaming | about 25 GB/s |
| a socket from DRAM | about 400 GB/s (Turin, 12 channels of DDR5 at 70% of peak) |
| cores a link may use | 16 (a dev machine), 64 (a CI machine) |
| page-cache copy (`read`, `write`, `copy_file_range`) | about 10 GB/s a core |
| NVMe write | about 7 GB/s |
| system call on an open descriptor | 0.15 µs |
| system call that resolves a path | 0.4 µs |
| `mmap` plus first touch of a 4 KiB page | 0.25 µs a page |
| `mprotect` | 0.5 µs, plus a TLB shootdown of about 2 µs on each other core running the process |
| IPC | about 2 instructions a cycle for branchy code, about 4 for straight-line integer code |
| SHA-256 with SHA-NI or ARMv8 crypto | about 2 GB/s a core |
| BLAKE3 with AVX-512 | about 6 GB/s a core |

Added for this lane, from the same machines: an L1 hit is 4 to 5 cycles; a store forwarded to a
load about 5; an uncontended atomic read-modify-write (RMW) on a line the core owns about 18
cycles on x86-64 (`lock` prefix) and 10 to 20 on arm64 with LSE; a line moving between cores
50 to 100 cycles. A release store is a plain `mov` on x86-64 and `stlr` on arm64; an acquire
load a plain load and `ldar`. A commit (`mprotect` from `PROT_NONE` of pages never touched) has
no page table entries to flush, so it costs the 0.5 µs without the shootdown.

The reference workloads:
- **A cold build of a large workspace** (rustc-sized): about 1 million tasks, 10^8 frame
  allocations, 10^7 interned values, 10^5 pool objects live at once, a few thousand tables
  (memo columns, interners, slabs), requests holding 100 to 1,000 chunks (200 MB to 2 GB).
- **A body edit**: 1 to 10 functions re-run, about 10^3 tasks and 10^5 frame allocations, one
  request arena of a few chunks, one revision published.
- **A cache hit**: lookups only (index probes, column and slab reads, revision reads), which
  must write no shared line and allocate nothing.

## Frame allocation (`Frame::alloc_layout`, `frame::task`)

- **Work**: per allocation, read the cursor, align it, add the size, compare with the end,
  store the cursor: five dependent operations. Per task: its frame state (cursor, end, a link
  to the enclosing frame) written at start and read back at end.
- **Bound**: with the cursor in a `Cell`, the dependent chain between two allocations is the
  store-forward of the cursor (5 cycles) plus the add and the mask: about 7 cycles, 2 ns, and
  about 8 instructions on the path. A task needs about ten stores and loads of frame state and
  warm memory: about 20 instructions, 6 ns.
- **Today** (corrected by speed-sol2-small-mem): the first round's text said the peak update
  was "off the cursor's dependency chain". It was off the chain but on the path: every
  allocation also loaded `bias` and `peak`, added, compared and stored `peak`, about 16
  instructions against 8, so in code that interleaves allocation with other work (throughput,
  not latency, bound) an allocation cost about 1.6 times its bound. Now `peak` is folded only
  where the cursor moves back (`Frame::fold_peak` before a free or shrink of the top
  allocation, a rollback, a reset) and on read (`Frame::peak` is `max(peak, used())`): between
  two such points `used()` only rises (a block entered ahead starts at the bytes used so far),
  so the fold sees the most it reached. The fast path is the bump alone: at the bound. The cold
  path (`alloc_cold`, `grow_top`) still updates `peak` eagerly, off the hot path.
- **A task**: about 45 instructions of frame state (the bound's 20 plus `floor`, `bias`, `held`,
  `limit`, `window`, `lent`, `peak`, the tag, the thread's measure stack and
  `tag::task_ended`'s thread-local add), about 13 ns against the bound's 6. Twice the bound in
  isolation, under 1% of the smallest task (1 µs), which is what a task's framing is measured
  against. Removing the rest would move `contains`, `measure` and the cap's bookkeeping out of
  the frame; not worth their loss.
- **Design**: the frame is a chain of blocks (2 MB chunks, and for an allocation over 1 MB a
  span: a region of its own reserved four times past it), the bump's end cached in `end`, so
  the common case reads no atomic and touches no line another core writes. Freeing or growing
  the top allocation moves the cursor in place, so a `Vec<T, &Frame>` pushed in a loop never
  copies within a chunk, and in a span never copies at all: growth commits more of the
  reservation, one system call per step of a quarter of what is committed (at least 2 MB), 0.5
  µs per 2 MB or less against 125 µs of first-touch zeroing of those 2 MB (0.4%). An inner task
  starts in the rest of its enclosing task's chunk, so 300 nested tasks of 1 KB each use about
  320 KB of one chunk. A task that needs a block takes it from the thread's shelf (8 chunks, 64
  spans, no atomic) and only past that from the chunk cache.
- **Measure** (`frame::measure`): at task and measure boundaries only (a few loads, adds and
  stores each), never per allocation. It errs high by at most an enclosing frame's growth while
  an inner task runs.
- **Proof**: instructions retired per allocation over 10^6 small allocations (target at most 9),
  ns per empty `frame::task` (target 15), zero global allocations for 300 nested warm tasks
  (`frame_300_nested_tasks_keep_their_bytes_and_cost_nothing_once_warm`), and `peak()` equal to
  the eager rule over seeded operations
  (`frame_peak_folded_lazily_is_the_most_used_after_every_operation` in `tests.rs`).
- **State of the art**: bumpalo, rustc's `DroplessArena`, LLVM's `BumpPtrAllocator` and
  game-engine frame allocators. The frame takes their chain without their global allocator,
  LLVM's big-allocation slab grown in place by commit, and a stack allocator's nesting.

## The frame cap (`frame/cap.rs`) (speed-sol2-small-mem)

- **Work**: per chunk or span commit step past the thread's shelf, reserve the bytes from a
  shared total; give them back when the frame does.
- **Bound**: one RMW on the cap's total per step: 18 cycles per 2 MB, about 0.0001 cycles a byte.
- **Today**: a thread already holding bytes reserves with one compare-and-swap and gives back
  with one subtraction; only starting or ending a holding, a wait and a give-back while someone
  waits take the lock. A warm task reserves nothing (its blocks come from the shelf). At the
  bound.
- **Proof**: cap RMWs per committed MB on the cold-build workload (target 0.5 or fewer); zero
  lock acquisitions per warm task.

## Request arena (`ArenaLocal`, `Arena::blocks`, `Arena::reset`)

- **Work**: per allocation, the frame's bump; per 64 KB block, one claim on the arena's shared
  word; per request end, every chunk back to the cache.
- **Bound**: the bump (7 cycles chained), plus one RMW per block: 18 cycles over the 2,048
  allocations of 32 bytes a block holds, 0.01 cycles each. A reset must relink each chunk (a
  load and a store of its link, about 4 cycles) and publish the lot: one compare-and-swap per
  run and one counter move per tag pair.
- **Today** (corrected by speed-sol2-small-mem): a block refill is not one compare-and-swap,
  as the first round wrote, but three RMWs: the compare-and-swap on `current`, `allocated`'s
  `fetch_add` and `tag::allocated`'s, about 54 cycles per block, 0.026 cycles per 32-byte
  allocation against a 2-cycle bump: 1.3%, within the bar. A reset used to pop each held
  chunk with a compare-and-swap, move its 2 MB between tags (two RMWs and a `fetch_max`) and
  push it with another compare-and-swap: five RMWs, about 90 cycles a chunk, 9,000 cycles (2.6
  µs) for a request of 100 chunks. Now `Arena::reset` takes the whole held list with `&mut`
  (`Stack::take_all`, no RMW), walks it with plain loads and gives back runs of 64
  (`chunks::give_all`): per run one compare-and-swap (`Stack::push_all`) and one tag transfer
  of the run's bytes; per chunk a link load and store. 100 chunks are about 2 × 60 + 100 × 4 =
  520 cycles (0.15 µs): at the bound.
- **Order** (a change): the chunks go back with the held list's top (the most recently taken,
  warmest) on top of the cache, where one at a time put the least recent there. Which chunk a
  later take gets is not observable in any output.
- **Proof**: RMWs per block refill (3) and per reset run (3), ns per 100-chunk reset (target
  0.2 µs), zero global allocations for a warm request
  (`arena_allocates_nothing_from_the_global_allocator`,
  `arena_reset_gives_back_every_chunk_in_runs`).
- **State of the art**: mimalloc's thread-local free-list sharding and jemalloc's thread
  caches keep allocation off shared lines the same way; they free per object, a request frees
  in runs of 64 chunks per compare-and-swap.

## Chunk cache (`chunks.rs`) (speed-sol2-small-mem)

- **Work**: take a 2 MB chunk for an owner (warm if one was given back), give it back; count
  its bytes from the cache's tag to the owner's.
- **Bound**: one pop (a compare-and-swap) and one counter move between two tags. Two tags are
  two lines, so the move is two RMWs: 18 + 2 × 18 = 54 cycles a chunk, 0.00003 cycles a byte.
- **Today**: a take is a thread-local load of the place, an acquire load of its space's
  `OnceLock`, the pop, and `tag::transfer` (a `fetch_sub` and a `fetch_add`, then a relaxed load
  of the high-water mark, raised only when passed). Before this round `tag::add` always ran a
  `fetch_max` too: four RMWs, 72 cycles, 33% over. Now at the bound. Carving a new chunk is one
  `fetch_add` and, every quarter of what is committed, a commit call.
- **NUMA**: a chunk taken on a node goes back to its own node's space whichever thread gives
  it, so a worker's frames stay local with no system call per chunk.
- **Proof**: RMWs per warm take and per give (3 each), zero system calls per warm take.

## Lock-free stack (`stack.rs`) (speed-sol2-small-mem)

- **Work**: Treiber's stack of `u32` indices, links in the owner's side table, a 32-bit tag
  beside the top index against ABA.
- **Bound**: one compare-and-swap per push or pop uncontended (18 cycles); a chain of `k` items
  needs its `k - 1` links written and one compare-and-swap.
- **Today**: `push` and `pop` are one compare-and-swap each. New this round: `push_all` links a
  chain with plain stores and publishes it with one; `pop_some` walks up to `k` links from the
  top (each link only ever names an index pushed before, so a stale walk reads live memory and
  its compare-and-swap fails on the moved tag) and takes them with one; `take_all` empties it
  with `&mut` and no RMW. At the bound.
- **Proof**: the model checker's
  `model_a_pop_of_some_racing_a_chain_push_holds_each_item_once` (`model.rs`) and
  `stack_chain_pushes_and_pops_of_some_match_a_vec` (`tests.rs`); RMWs per batch moved.

## Pools (`Cache::alloc`, `Cache::free`, batches of 64)

- **Work**: per object, a pop or push of an index in the worker's cache and the value's write;
  per 64, moving a batch between the cache and the shared stacks.
- **Bound**: about 3 cycles for an allocation and 3 for a free (load the length, step it, load
  or store the index, write the value), plus the batch traffic a cross-thread round trip cannot
  avoid: the batch published once and taken once, two compare-and-swaps per 64 objects (0.56
  cycles each), plus copying the 256-byte batch in and out (about 0.25 cycles each). An
  allocation and free together: 6 + 0.56 + 0.25 = 6.8 cycles.
- **Today** (corrected by speed-sol2-small-mem): the first round's "one compare-and-swap per 64"
  missed the descriptors: a refill popped the full stack and pushed the emptied descriptor on
  the empty stack, and a spill popped the empty stack and pushed the full one, four
  compare-and-swaps per batch round trip, 1.125 cycles per object: 7.4 cycles, 8% over. Now
  each cache keeps up to 8 empty descriptors (`STASH`) and moves them 4 at a time
  (`push_all`, `pop_some`): a cache that refills and spills in turn never touches the empty
  stack, and a pure producer or pure consumer touches it once per 4 batches. That is 2.5
  compare-and-swaps per batch at worst, 0.7 cycles per object: 6.95 cycles, 2% over the bound.
  `SPARE_BATCHES` is now 16,384 (room for 2,048 caches' stashes; 4 MB of address space,
  committed only as descriptors are made).
- **Shared values** (`pool::Shared`): a clone is one relaxed add; the last drop an
  acquire-release subtract and a slot push; `make_mut` with one owner one acquire load. The
  same atomics as `Arc`, without the global allocator: at the bound.
- **Proof**: compare-and-swaps per 64 objects on a producer/consumer pair (target 2.5) and the
  descriptors made stay bounded
  (`pool_a_producer_and_a_consumer_move_batches_and_reuse_descriptors`); zero global allocations
  for a warm pool round.
- **State of the art**: tcmalloc's per-CPU caches with transfer caches of batches, mimalloc's
  free lists per page. A pool matches tcmalloc's batch transfer, keeps its descriptors local as
  tcmalloc keeps its transfer-cache slots, and has no size-class lookup.

## Generational handles (`Slab::get`, `insert`, `remove`)

- **Work**: a lookup checks the handle's index and generation and returns the value.
- **Bound**: one load of the entry's generation (one line), a bounds compare and a compare;
  insert and remove an intrusive free-list pop or push.
- **Today**: exactly that. Corrected by speed-sol2-small-mem: the first round said the
  generation and value share a line for values up to 56 bytes; that holds only where the
  entry's stride divides 64 (a 24-byte value's 32-byte entry). A 40-byte value's 48-byte entry
  straddles a line in 2 of every 4 entries. The generation is at the entry's start, so the
  lookup still reads one line; what straddles is the caller's read of the value, which is the
  value's own cost. At the bound.
- **Proof**: zero global allocations for a slab round; ns per `get` over 10^6 random live
  handles.
- **State of the art**: slotmap's `SlotMap`, Bevy's entities. Same layout and cost; this one
  grows in place without a realloc and retires a slot whose generation would wrap.

## Reserved regions (`Region::ensure`, `RegionVec::push`)

- **Work**: grow a table in place: commit the next pages, let the kernel zero them on first
  touch; push a value.
- **Bound for growth**: first touch of fresh memory, 0.25 µs per 4 KiB page (16 GB/s);
  nothing is faster for fresh memory. Commits add 0.5 µs each.
- **Today**: `ensure` is one acquire load and a compare when the bytes are committed. Commits
  are geometric (a quarter of what is committed, at least a granule, at most 64 MB): `O(log n)`
  calls before 64 MB and one per 64 MB after, 0.5 µs per 64 MB against 4 ms of zeroing it
  (0.01%). Huge pages cut page faults 512-fold. Growth never copies, where a doubling `Vec`
  copies every byte about once more. `RegionVec::push` is one compare and one store, the
  length a plain field: at the bound.
- **Proof**: commit calls per 64 MB grown (1), zero copies on growth
  (`region_vec_addresses_are_stable_across_growth`), zero global allocations for pushes
  (`region_vec_push_allocates_nothing`).
- **State of the art**: virtual-memory-reserved arrays in game engines and in V8's and .NET's
  heaps.

## `AppendVec` (one writer, lock-free readers)

- **Work**: append a value that readers on other threads see; read the published prefix.
- **Bound**: per append, the value's store and one release store of the length (about 3
  cycles); a bulk append of `n` values one copy at L1 bandwidth (64 B a cycle) and one release
  store. Per read one acquire load of the length, issued beside the element's load.
- **Today** (corrected by speed-sol2-small-mem): each `append` also took and dropped the writer
  flag (an uncontended swap and a store, about 20 cycles), so an append cost about 23 cycles,
  seven times its bound, and `blaze-vfs`'s path bytes, appended one at a time, cost 23 cycles a
  byte against a copy's 0.02. Now `AppendVec::writer()` takes the flag once and returns a
  `Writer` whose `append` is the value store and the release store, and whose
  `extend_from_slice` and `extend_with` publish many values with one store; `append` on its
  own is a writer for one value, the flag's swap the price of a safe `&self` API that cannot
  race. Through a writer: at the bound. Callers that loop are asked to move to it: `blaze-vfs`
  (queue 00900) and base-db's input tables (queue 00936).
- **Proof**: instructions per append through a `Writer` (target 6), bytes per cycle of
  `extend_from_slice` (target 32), `append_vec_writer_and_bulk_appends_match_vec`, zero global
  allocations (`append_vec_writer_appends_allocate_nothing`), a second writer stops
  (`append_vec_second_writer_at_once_stops`).
- **State of the art**: `boxcar` and append-only vectors index a chunk per read; a reserved
  region needs none.

## `Column` (any number of appenders)

- **Work**: claim an index, write the value, publish it so a reader sees it whole.
- **Bound**: claiming among many pushers needs one RMW on the shared counter (18 cycles; once
  per block with `Column::claim`); publishing a value only the claimer writes needs one release
  store. Per push about 22 cycles, per value filled from a block about 3. A read is a bounds
  compare and one acquire load beside the value's.
- **Today** (changed by speed-sol2-small-mem): a value was published by a release `fetch_or` on
  a ready word shared by 64 values, an RMW: a push cost about 40 cycles (1.8 times its bound)
  and a fill from a block about 22 (seven times). Now each value has a ready byte of its own,
  set with one release store: a push is at the bound and so is a fill. The price is 1 byte of
  reserved address space per value instead of 1 bit (committed only as the column grows); its
  one caller, salsa's page table (`vendor/salsa/src/table.rs`), stores pages of many slots, so
  it is noise there. Reads are unchanged in cost: one acquire load.
- **Proof**: RMWs per push (1) and per fill (0); `column_matches_a_model_of_options` (a seeded
  model of pushes, claims and fills against a `Vec<Option<_>>`, zero global allocations) and the
  multi-threaded tests in `tests.rs`.

## SwissTable probe and `Index` (`swiss.rs`) (speed-sol2-small-mem)

- **Work**: find a value by its hash in a table of groups, each a control word of 7-bit tags
  beside its slots; insert one under the caller's lock. `find` and `vacant` serve every
  interner's table (intern, salsa's interned and node maps); `Index` is the table of `u32`s
  `blaze_ids::Sharded`, the hash-conser and base-db's tables use.
- **Bound for a hit**: the group's line and the caller's column line it then checks: 2 cache
  lines, one tag match in a word (about 6 ALU operations, no branch per slot). A miss: the group
  line, almost always with an empty slot.
- **Today**: the probe is SWAR over a 64-bit word (hashbrown's generic group). `Index` groups
  were 8 slots and a word, 40 bytes, so half the groups crossed a 64-byte line and about 31% of
  the slots a hit reads sat a line away from their word: 1.31 lines for the index's part, 2.31
  with the caller's column against 2, 15% over. Now a group is 6 slots and a word, 32 bytes
  aligned to 32 (`#[repr(C, align(32))]`), two groups a line and none straddling: a hit reads
  the group's line, plus another only when its probe runs from an odd group into the next even
  one (about 6% at three quarters full), 2.06 lines, 3% over. The price is memory: 7.1 to 14.2
  bytes of table per value instead of 6.7 to 13.3 (6.7% more). Growth asks the caller for each
  value's hash again (the index stores none), about `n` extra hashes over the index's life:
  free for callers with stored or trivial hashes.
- **Why not 16 wide**: an SSE2 or NEON group holds 16 tags, but a group's slots of `u32`s would
  no longer share its line; the SWAR word with its slots in the same line is the faster layout
  for these tables (one line where hashbrown reads two: control bytes, then the bucket).
- **Proof**: lines touched per hit over 10^6 random keys at three quarters full (target 2.1),
  `index_groups_never_straddle_a_line`, the equivalence and growth proptests in `swiss.rs`, and
  the model checker's reader-races-growth test.
- **State of the art**: hashbrown and Abseil's `flat_hash_map` (SIMD tag groups, control bytes
  apart from slots); folly's F14 (14 slots and their tags in one chunk). `Index` takes F14's
  in-line chunk at line size.

## Revisions (`Revisions::read`, `publish`)

- **Bound for a read**: one acquire load of a pointer, then the reader's own loads.
- **Today**: exactly that. The published record `{number, value}` sits in the revision's arena,
  so the number and value share a line; the task guard is the reader's own thread slot
  (`blaze_reclaim`), no shared write. At the bound.
- **Publish**: one arena reset (now runs of 64 chunks per compare-and-swap, see the request
  arena), the build, two release stores and one `Grace::start` (a fence and a `fetch_add`). It
  never waits on a reader: `PublishError::Readers` instead.
- **Proof**: zero shared writes per read (a counter of RMWs on the read path: 0); ns per publish
  of an empty revision.
- **State of the art**: RCU and crossbeam-epoch, whose read side is the same one load;
  reclamation here is per revision arena, not per object.

## Counters (`tag`)

- **Work**: count live bytes, the high-water mark and bytes handed out per tag, at coarse grain
  (a commit, a chunk, a batch, a task's end).
- **Bound**: one RMW per event on the tag's own line, and no second RMW for a high-water mark
  that is not passed (the mark never falls, so a load that sees it at or past the new value is
  the same answer as a `fetch_max`).
- **Today** (corrected by speed-sol2-small-mem): `add` and the committed-bytes counter ran a
  `fetch_add` and a `fetch_max`, two RMWs per event. Now `raise` loads the mark and runs the
  `fetch_max` only when passed: one RMW and a load of a line the add just made the core's own.
  `task_ended` writes the peak only when it rises and gathers bytes per thread, folded every 1
  MB. Each tag has its own 128-byte line. `committed_now()` is one load (queue 00863, done; its
  caller in `blaze-graph`, queue 00890). A snapshot is 20 × 5 relaxed loads, read by admission
  and the agent API, never on an allocation path. At the bound.
- **Proof**: RMWs per chunk take (3, see the chunk cache) and per warm task end (0).

## Scratch buffers (`scratch`)

- **Work**: lend the thread's warm buffer, empty, for a closure or until dropped.
- **Bound**: a call nested inside the closure must get another buffer, so the shelf cannot be
  held across the closure: two thread-local accesses (take, give back), each with its
  `RefCell` flag, and a `clear` (a store of the length). About 25 instructions.
- **Today**: that, plus a second `clear` before the shelf gets it back (one store, kept because
  `Shelf::give` is public and a shelf promises empty buffers), about 27 instructions: within the
  bar, and noise against the formatting or hashing the closure does.
- **Proof**: zero global allocations for a warm lend; instructions per `with_bytes` of an empty
  closure (target 30).

## Kernel memory (`os.rs`, `vm.rs`) (speed-sol2-small-mem)

- **Work**: reserve address space aligned to 64 KB, commit, purge, release, place on a node;
  `vm` adds aligned reservations and read-only ranges for a guest's address space.
- **Bound**: one system call per operation; a reservation on huge pages one more (`madvise`).
- **Today**: commit (`mprotect`), purge (`madvise`), release (`munmap`) and bind (`mbind`) are one
  call each. The page size is read once and cached. A Unix reservation is an `mmap` of 64 KB
  more than asked and two `munmap`s trimming it to alignment (each skipped when its piece is
  empty, rarely), plus `madvise(MADV_HUGEPAGE)` for a huge region: 3 calls (4 huge) against 1
  (2), about 0.3 µs more per reservation.
- **Gap, not closed**: per reservation that is 2 to 3 times the bound. It matters only for a
  table reserved and barely touched (a memo column of a few pages: about 1.6 µs of reserve,
  commit and first touch, of which 0.3 µs is the trim, 20%); a region made in the warm loop
  (a frame span, an arena's large allocation) commits and zeroes at least 2 MB, 125 µs, so its
  trim is 0.2%. Small tables are made once per daemon, about 5,000 of them, 1.5 ms at start. What
  would close it: a reservation slab that carves 64 KB-aligned small regions out of one big
  reservation with free lists by size (one `mmap` per thousands of tables), which changes how
  every region is released; or address hints placing each reservation aligned below the last,
  which saves the trims only when the kernel honours the hint, a rate to be measured first. Left
  for the coordinator's measurements: a change here without them could cost more (a missed hint
  is two calls more) than it saves.
- **Proof**: system calls per `Region::reserve` (target 1, today 3) and per commit (1);
  reservations per daemon start.

## Huge pages (`huge.rs`, `benches/huge.rs`) (speed-sol2-small-mem)

- **Work**: choose once per process which pages a huge region is made of (transparent, the
  reserved pool, base) and commit by that rule.
- **Bound**: for random reads over a table larger than the base-page TLB reach, no page walk per
  read: 1 GiB on 2 MB pages is 512 entries, inside every target's second-level TLB.
- **Today**: the choice is one `OnceLock` read at the first huge reservation. A huge region
  commits base pages while it is at most one huge page long, then whole huge pages, and under
  transparent huge pages collapses its first 2 MB when it outgrows it (`MADV_COLLAPSE`). The
  bench chases a random cycle through 1 GiB and fails when a huge mode walks more than 0.1 times
  per read: the bar is stated and measured there.
- **Proof**: `benches/huge.rs`'s walks per read (target at most 0.1 on huge pages).

## Rounding, errors, the atomics switch (`align.rs`, `error.rs`, `sync.rs`, `model.rs`) (speed-sol2-small-mem)

- `align`: rounding to a power of two at run time is one add and one mask, where
  `next_multiple_of` divides (tens of cycles): at the bound, checked against a model in
  `align/tests.rs`.
- `error`: built only when an allocation fails; no cost on a successful path.
- `sync` and `model`: `std`'s atomics, or the model checker's under `cfg(blaze_check)`; no cost
  in a build, the model tests check the stack, the pools and the index.

## Where the bar is proved

Every allocator has an allocation-budget test in `crates/blaze-mem/src/tests.rs`: a warm frame
task, a request arena (and its reset in runs), a pool round, a slab round, a limbo round,
`RegionVec`, `AppendVec` and `Column` pushes and reads and scratch lends make zero calls to the
global allocator. The cycle, instruction, line and system-call counts above are for the
coordinator's benchmarks and `proof`'s budgets to confirm, with any gap over 10% treated as a
bug. The one gap left open is the reservation's trim (`os.rs`), for the reason given there.
