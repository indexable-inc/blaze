---
status: plan
summary: blaze-malloc, blaze's general-purpose allocator for memory not in a frame and for bstd programs: a heap per thread with no atomics on its own blocks, 81 size classes in header-free spans of 2 MiB segments, remote frees by a lock-free queue per heap, large blocks as their own growable mappings, ended threads' heaps adopted. Built and measured; not yet anyone's global allocator.
date: 2026-11-16
---
# blaze-malloc

## For a successor

State on 2026-11-16: the in-band free-list design below is built, tested and measured; the
out-of-band form (free bits in the span header) was built behind a seam, measured against it,
and deleted ("Free state: in the block or beside it"). Under Miri the suite is clean under
Stacked Borrows except `tests/relink.rs`, and under Tree Borrows except that and the named
limit; both are ignored under Miri with the reason in the file. The allocator is parked until
something adopts it.

**Next step: the hybrid, each form where it won (written down, not built).**
- **Local frees stay in-band.** The block just freed is handed back while it is still in cache, so the local rows should not change.
- **Remote frees go out-of-band.**
  - A remote free is one `fetch_or` on the span's remote bitmap row in the segment header.
  - The freer that first sets the span's `pending` pushes the *span* (its descriptor, not the block) on the owner's swap-all queue, once.
  - The owner takes each row word with `swap(0)` and allocates reclaimed blocks straight from that word (count trailing zeros, clear the low bit, `base + i * size`).
  - The owner never writes a link into a block it did not free itself.
  - An `inflight` count keeps the owner from retiring an empty span while a freer is still between its bit and its last touch of the descriptor.
- **Expected, to be measured when built:**
  - the cross-thread and cold-remote rows at the bitmap's numbers (5.8 to 6.1 against 6.7 ns);
  - the local rows unchanged;
  - `tests/relink.rs` passing under both models, because neither a remote free nor the owner's drain touches the block. Then drop its Miri ignore.
- **Model tests the remote path needs,** each with its planted bug:
  - a lost `fetch_or` (planted: load then store);
  - a span pushed twice (planted: no `pending` check);
  - a bit reclaimed twice (planted: read the word, then clear it, instead of one `swap`).
  - Also keep the ones the deleted form had: `pending` cleared after the words are taken (strands a bit), and a retire while a freer is in flight (races the descriptor).
- **Starting code:** the deleted three-strategy tree, with `bitmap.rs`, the span and segment fields, `post`, `collect` and `may_retire`, and the unrun `model_bits*` tests. It is at `/Volumes/Projects/blaze-int-1112-notes/malloc-bitmap/`: `src/`, `tests/`, `examples/` and `tracked.diff`.

After that: the 128-core rows, and profiling the 3.5x gap to the pair floor ("Not done").

**Where everything is.** `crates/blaze-malloc/src/`:
- `lib.rs`: API, `GlobalAlloc`, `Allocator`, `reallocate`, `Stats`, `Source`, `probe`.
- `heap.rs`: per-thread heaps (`HEAPS`, 1024), fast path `alloc`, `refill`, `free`, `free_local`, `settle`.
- `segment.rs`: `SEG` and the span shifts (the one place sized down under Miri), `Given`, `header_of` and `Found` (the one place an address becomes a pointer), `Mapping` (blaze-mem `Region`, or `posix_memalign` under Miri), the segment and large caches, `trim`.
- `remote.rs`: the Treiber queue of freed blocks, with planted bugs (`Plant`).
- `adopt.rs`: the registry of abandoned heaps.
- `tls.rs`: the pthread key and the direct TSD read on macOS arm64.
- `class.rs`: the 81 size classes.
- `sync.rs`: the atomics switch for the model checker.

**Tests and how to run them.**
- `src/tests.rs`: the tape test, classes, the thread word, a thread ring, large grow.
- `src/model.rs` (`--cfg blaze_check`, filter `model_`): six tests, four of them planted bugs.
- `tests/adopt.rs`, `tests/large.rs` (counter test: large mappings cached or returned, `trim` to zero) and `tests/global.rs` (installed as `#[global_allocator]`, Miri-sized) each have a binary of their own.
- `tests/toy/mod.rs` with `tests/toy_own.rs`, `tests/toy_caller.rs` and `tests/toy_nothing.rs`: the Miri control for finding 4. It is a static-buffer, last-in-first-out global allocator. In `dealloc` it writes the link through our provenance (own), writes it through the caller's pointer (caller), or writes nothing and reuses the block at once (nothing). Each variant runs three cases: a `Box` dropped inside its by-value function, the same then reused inside, and a thread spawned and joined.
- `tests/relink.rs`: forces the one remaining write through the allocator's provenance (finding 4's `free_owned` re-link of a drained remote block while the freer's `Box` is still protected). Natively it passes; under Miri it is ignored, refused by both models.
- Run: `bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh blaze-malloc test` (green, 22 tests, 2026-11-16) and `examplerun.sh blaze-malloc cost`. The coordinator runs Miri and the model checker.

**The checker's verdicts** (coordinator's runs, 2026-11-16), the column beside the cost table:

| Test | Stacked Borrows | Tree Borrows |
|---|---|---|
| in-crate suite, `tests/adopt.rs`, `tests/large.rs`, `tests/global.rs` (installed as the global allocator) | clean (21 passed) | clean except the toy pattern below |
| toy own (link through the allocator's provenance) | pass, all three cases | fails all three |
| toy caller (link through the caller's pointer) | pass | fails drop-then-reuse only |
| toy nothing (no write; reuse at once) | pass | fails drop-then-reuse only |
| `tests/relink.rs` (in-band drain while the `Box` is protected) | refused: "would remove [Unique] which is weakly protected" | refused: "read access ... is forbidden" |
| model checker, `model_` | 6 of 6, four planted bugs caught | |

**The named limit (Tree Borrows).** `toy_nothing::box_dropped_then_reused_inside_its_by_value_function`
is its proof: an allocator that writes nothing into a freed block, and only hands the same bytes
out again while the freed `Box`'s function is still running, is refused by Tree Borrows. So no
allocator that reuses memory passes that pattern under it; the protector outlives the drop. The
toy test stays, expected to fail under Tree Borrows only (comment in `tests/toy/mod.rs`).

**The in-band limit (both models).** A remote free reaches its owner as a block on a queue;
draining it, the owner must read and re-link the block's first word, and the only pointer it has
is its own. If the freer is still inside a by-value function of that `Box`, both models refuse
it (`tests/relink.rs`). Deferring the re-link removes the write, not the read. Only free state
outside the block avoids it, and that form lost on speed (below), which decides; the checker
is the tie-breaker and there was no tie.

**The contract with compiled code.** Callers see `alloc`, `dealloc` and the rest as the opaque
standard calls: every entry point (`GlobalAlloc`, `Allocator`, `try_allocate`) is
`#[inline(never)]`. Inlined, a caller's optimizer would see the same bytes handed out, given
back and handed out again under pointers it may assume unrelated (the allocation's result is
`noalias`), and could move a store of the old owner past one of the new. The checker's limits
above are the same fact seen from the model: the allocator relies on the call staying a call.

**Miri history**, each finding with its cause:
1. SIGKILL of `tests/global.rs`. Under Miri the page source reserved through the global allocator, which is this crate, so it recursed without end. Now `posix_memalign`, with the sizes shrunk in `segment.rs`. The suite went from 613 s to 34 s, mostly by fixing a test helper that filtered every index of each large block.
2. Stacked Borrows: the header was read through the caller's pointer, whose provenance covers its block only. Now the caller's pointer becomes `Given` (an address only) and goes through `header_of`, which uses exposed provenance to read the header's own mapping pointer. Every kept pointer is formed from that (strict).
3. Leaks: cached large mappings were kept by address-made pointers, which Miri's leak check cannot follow. Fixed by finding 2's strict derivation, not by roots.
4. `Remote::push_with` wrote the link into a block during `dealloc` through the allocator's own provenance, while the block was still weakly protected as a `Box` argument (`std::thread::lifecycle::init(self: Box<Self>)` frees its own box). Both Stacked and Tree Borrows refused it (logs `port27/miri-blaze-malloc-0049.txt`, `-0050.txt`): the allocator's pointer is foreign to the protected tag. The wrapper control (a write through the pointer `dealloc` was given) passed both models (`-005437`, `-005438`). Change (2026-11-16, clean under Miri's Stacked Borrows, 21 passed): `remote::Freed` carries the caller's pointer beside `Given` to exactly the two link writes at free time (`free_local`, `Remote::push`); the lists keep the allocator's pointers, and the later read of the link goes through those. One write is still through the allocator's provenance: `free_owned` re-links a block the owner drained from its queue, after the remote `dealloc` returned; a caller still inside its by-value function at that moment would see a foreign write (forced by `tests/relink.rs`; "The in-band limit").

**Free state: in the block or beside it (measured 2026-11-16, deleted).** Three strategies ran
behind one type parameter (rule 78), each passing the same tape test case for case and the
thread ring:
- *in-band* (kept): free lists through the blocks' first word; a remote free pushes the block.
- *words*: free bits per span in the segment header (64 words a 64 KiB span; header 36 KiB of
  slot 0); the fast path one cached bitmap word and base per class (`tzcnt`, clear the low bit,
  `base + i * size`, the index by a 40-bit magic multiply); a remote free one `fetch_or` on the
  span's remote row, and the first freer to set the span's `pending` pushes the span (not the
  block) on the heap's queue; an `inflight` count keeps the owner from retiring a span a freer
  is still inside. Model tests with three planted bugs (load-store bit, late `pending` clear,
  retire in flight) were written; never run, as the form was deleted first.
- *indices*: the same bits, the fast path an array of up to 64 free indices per class.

Two runs, best of 5 each, all four columns alternated; ns per unit, the second run:

| Row | system | in-band | words | indices | floor |
|---|---:|---:|---:|---:|---|
| pair 16 B | 5.84 | 3.77 | 3.75 | 4.15 | ~1.1 (twenty instructions) |
| pair 64 B | 6.73 | 3.78 | 3.71 | 4.15 | ~1.1 |
| pair 256 B | 6.71 | 3.78 | 3.88 | 4.29 | ~1.1 |
| pair 1024 B | 17.01 | 3.86 | 3.74 | 4.16 | ~1.1 |
| pair 4096 B | 13.18 | 4.16 | 4.88 | 5.54 | ~1.1 |
| batch 1M x 64 B, free same | 9.24 | 5.70 | 5.44 | 5.61 | the pair's |
| batch, free reverse | 8.82 | 5.88 | 5.34 | 5.93 | the pair's |
| batch, free shuffled | 30.41 | 10.05 | 7.42 | 7.90 | the pair's |
| cold free 1M, same order (free only, after a 256 MiB walk) | 5.90 | 2.72 | 2.58 | 2.53 | one miss per header line |
| cold free 1M, shuffled (free only) | 25.60 | 6.42 | 4.32 | 4.33 | one miss per header line |
| cross-thread 64 B | 15.89 | 6.66 | 6.08 | 5.83 | one atomic RMW a block |
| churn 1 thread | 39.43 | 6.37 | 9.11 | 10.31 | the pair's |
| churn 6 threads | 24.56 | 1.44 | 2.01 | 2.07 | 1/6 of one thread's |
| churn 12 threads | 13.85 | 0.87 | 1.11 | 1.21 | 1/12 |
| churn 18 threads | 12.17 | 0.42 | 0.34 | 0.81 | noisy: run 1 had 0.37 / 0.73 / 0.47 |
| compiler-shaped trace | 12.02 | 6.08 | 6.42 | 6.86 | the pair's |
| peak RSS after churn (MiB, 256 live) | 1362 | 1183 | 1190 | 1190 | 256 plus bookkeeping |

Run 1 agreed row for row within 8% (compiler trace 6.04 / 6.56 / 6.86; churn 1 thread 6.78 /
9.92 / 11.79). Decision, for blaze's workloads (many short-lived small blocks per task, frees
mostly on the allocating thread): **in-band**, 5 to 8% faster on the compiler trace and 30 to
45% on single-thread churn, where its last-in-first-out reuse hands back the block just freed,
still in cache, while a bitmap hands out the lowest free index and refills from sparse words.
*indices* lost to *words* everywhere and is dropped outright. *words* would have been better at
frees far from their allocation: shuffled and cold frees (4.3 against 6.4 ns, it never touches
the block) and cross-thread frees (6.1 against 6.7), and it alone passes the checker's
`relink` pattern. The deleted code is kept outside the tree at `/Volumes/Projects/blaze-int-1112-notes/malloc-bitmap/`
(the next step above builds on it).

**Large blocks:** `large_free` writes only the header (`purged`, the cache) and purges pages, which is a no-op under Miri: nothing into the freed block's range. The header does sit just before the block in its first page, so an underflow can reach it.

`crates/blaze-malloc`: `GlobalAlloc` and `Allocator`, `#![no_std]`, no third-party crates, its
pages from blaze-mem's `Region` (rule 30). Nothing uses it yet: adopting it is the coordinator's
decision once Miri and the model checker have run its tests.

## Choices, each against what was rejected

| Chosen | Rejected, and why |
|---|---|
| A heap per thread, its own blocks freed with plain loads and stores | Per-CPU caches (TCMalloc's rseq): Linux-only, and macOS has no rseq; a global heap with locks: the churn rows below are what locks cost |
| 81 classes, 8 B, 16 B steps to 128, then 8 per doubling to 64 KiB (12.5% apart) | Power-of-two classes: up to 50% internal waste; finer classes: more partly-used spans |
| No block header: a block's 2 MiB segment is its address masked, its span the next bits | A per-block header (ptmalloc): 8 to 16 bytes a block, and 16-byte blocks double |
| Metadata reached by exposed provenance, used once: each mapping's pointer is exposed when reserved; `segment::header_of` takes a `Given` (an address, no deref), forms the header pointer with `with_exposed_provenance_mut`, and reads from it only the header's own mapping pointer; the segment, span and block pointers the lists and caches keep are formed from that (strict) | A page map from address to segment pointer (strict throughout): a dependent load on every free. Masking the caller's pointer: undefined behaviour, its provenance covers its block only (Miri, 2026-11-16). Keeping address-made pointers in the caches: Miri's leak check cannot follow them and reported cached large mappings as leaked. Cost: pairs 3.72 to 3.98 ns after, 3.83 to 4.02 before (one run each, quiet machine) |
| A remote free is a Treiber push on the owner's queue; the owner takes all with one swap | mimalloc's per-span `thread_free` with a delayed-free flag: two protocols and an ABA guard; a swap-all consumer has no ABA, and one queue per heap is one cache line to watch |
| Thread end abandons the heap whole to a tagged stack; the next thread to need a heap adopts it and drains its queue | Freeing every span back at thread end: blocks still live in other threads keep pointing at the heap, so they must stay valid; walking them is unbounded |
| Large blocks (past 64 KiB) a mapping each, reserved at twice the next power of two so realloc grows in place | `mremap`: Linux-only; copying on every grow |
| `alloc_zeroed` skips the fill on fresh pages (a span's bump region never used, a new mapping) | Always `memset`: a 64 KiB zeroed table costs its fill |
| Empty segments decay by count: 8 kept committed, 64 more purged (`MADV_FREE_REUSABLE` / `DONTNEED`), then unmapped | A timer or clock read: nothing in an allocator may wait or read a clock (rule 75); count is deterministic |
| The thread's heap: on macOS arm64, the pthread key's TSD slot read directly (`mrs tpidrro_el0`, a private layout, rule 45, tested equal to `pthread_getspecific`); elsewhere `#[thread_local]` | `pthread_getspecific` each time: 0.70 ns against 0.24 ns (below); a `#[thread_local]` alone has no thread-end hook, and the key's destructor is that hook |
| Large pages for large mappings only, opt-in by `set_large_pages` | Reading `BLAZE_HUGE` from the environment: it allocates, inside the allocator |

## Cost (`examplerun.sh blaze-malloc cost`, M5 Max, 2026-11-16, best of 5, alternating)

| Row (ns per unit) | system | blaze | ratio |
|---|---:|---:|---:|
| pair 16 B | 5.35 | 3.88 | 1.38x |
| pair 64 B | 6.09 | 3.72 | 1.64x |
| pair 256 B | 6.33 | 3.74 | 1.69x |
| pair 1024 B | 17.11 | 3.99 | 4.29x |
| pair 4096 B | 13.07 | 3.95 | 3.31x |
| batch 1M x 64 B, free same | 9.12 | 6.46 | 1.41x |
| batch, free reverse | 9.22 | 5.71 | 1.62x |
| batch, free shuffled | 30.31 | 9.60 | 3.16x |
| cold free 1M x 64 B, same order (free only, after a 256 MiB walk) | 5.75 | 2.60 | 2.21x |
| cold free, shuffled (free only) | 25.57 | 6.47 | 3.95x |
| cross-thread 64 B | 15.43 | 6.02 | 2.56x |
| churn 1 thread (per op, wall) | 39.66 | 6.17 | 6.43x |
| churn 6 threads | 23.19 | 1.43 | 16.20x |
| churn 12 threads | 13.20 | 0.86 | 15.32x |
| churn 18 threads | 10.63 | 0.44 | 24.36x |
| compiler-shaped trace | 11.74 | 6.19 | 1.90x |
| fragmentation: peak RSS (MiB), 256 MiB peak live | 1362 | 1183 | 1.15x |
| thread word: `#[thread_local]` / `pthread_getspecific` / chosen | | 0.23 / 0.79 / 0.24 | |

Every row here is slower than the same row on the morning's quiet machine (compiler trace 5.73
then): about 28 lanes share it now, so the strategy table above, whose columns alternated within
each run, is the comparison; this one is the system ratio.

The floor (rule 67) is about twenty instructions a pair, near 1.1 ns: the pairs run at about
3.5 times it. The fragmentation row's peak includes the test's own bookkeeping vector (up to
192 MiB, the same for both), so the ratio to live is overstated for both.

## Not done

- 128 cores are unmeasured. Measure on a 128-core Linux VM: the churn rows at 32, 64 and 128
  threads; cross-thread frees with many producers into one owner (the queue's one cache line);
  the segment cache's spin lock under contention; and the 1024-heap cap.
- Within a live heap, an empty span is not purged until its segment empties: a phase that frees
  most of a segment's blocks keeps its pages. Span-level purge on a decay is the next step for
  fragmentation.
- The 3.5x gap to the floor is not yet profiled: the counters (two relaxed load-store pairs a
  pair) and the class lookup are the first suspects.
- On Linux, `Region::ensure` prefaults with `MADV_POPULATE_WRITE`, which makes committed pages
  resident at once; an allocator wants them faulted on touch.
- Verdicts so far (coordinator, snapshot 497b540b13): the model tests (`model_`) 6 of 6, all
  four planted bugs caught; Miri on the in-crate suite and `tests/adopt.rs` clean. Under Miri
  the page source is sized down in one place (`segment::SEG` 512 KiB, spans 16 and 128 KiB) and
  reserves with `posix_memalign`, never the global allocator, so `tests/global.rs` (installed as
  `#[global_allocator]`) runs there too.
- Miri and provenance: `with_exposed_provenance_mut` needs Miri's permissive provenance, which
  is its default; it prints a warning on the first such cast, which `-Zmiri-permissive-provenance`
  silences; `-Zmiri-strict-provenance` must not be passed (it rejects the cast). Sound: exposing
  and re-acquiring is defined Rust (the exposed-provenance API), and Miri gives the pointer a
  wildcard tag that may use only tags exposed for that allocation, which can hide a bug but never
  invent one; the mappings exposed are the allocator's own and no caller's.

## Adopting it as blaze's global allocator

One `#[global_allocator] static A: blaze_malloc::Malloc = blaze_malloc::Malloc;` in each binary
crate (`blaze`, the helper, the test harness). It touches: blaze-mem's `Region`, which must not
allocate (it does not today) and must stay that way; anything that measures memory by the system
zone (`malloc_zone_statistics`, the footprint report) now sees only large mappings; Miri and
sanitizer runs, which then exercise this crate on every allocation; and the frame allocators,
whose own chunks would come from it unless they keep going to `Region` directly.
