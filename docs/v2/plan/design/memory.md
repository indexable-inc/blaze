---
status: plan
summary: blaze manages memory like a game engine: allocators by lifetime (frame, request, view's revision, kept blocks, the program's root), virtual-memory-reserved arenas with stable addresses, pools, generational handles, and zero allocations from a general-purpose heap in the warm loop. Built in crates/blaze-mem; the warm-loop proof is not.
date: 2026-10-29
---
# Memory, the way a game engine does it

A command's memory is as [parallel.md](parallel.md), "End state (architect, 2026-11-16)", points
5 to 7, says: a frame per worker reset per item, records as what outlives a task, footprint the
one measure, nothing admitted by memory (rule 50), and the page cache as the only cache (rule 51).
There is no global allocator to reach: the heap is a grant `main` names (D173). This page is the
allocators those rest on.

A game has a frame budget and never calls `malloc` per frame. blaze's frame is an edit: from
a file changing to code patched in. The warm loop should allocate nothing from a
general-purpose heap. Every byte comes from an allocator chosen by how long it lives.
All of it lives in `crates/blaze-mem`.

## Allocators by lifetime

| lifetime | allocator | freed |
|---|---|---|
| a task (one query run, one codegen unit) | **frame allocator**: a bump pointer per worker, with markers for scoped rollback | all at once when the task ends, by resetting the pointer |
| a request (one edit, one build, one test run) | **request arena**, one per request, chunked | when the request completes |
| a view's revision | **revision arenas**, double-buffered: the next revision builds while the current one is read | when no reader holds the old revision (task boundaries) |
| what outlives a task (results, tables) | **records**: written once to the store, read in place (small ones by pread into the task's frame, large plain arrays mapped read-only) | by the store's sweep |
| fixed-size objects that come and go (plane tasks, I/O operations, timers, watch slots) | **pools**: per-type slabs with free lists, per worker, refilled in batches | back to the pool, never to the OS in the warm loop |
| image values of any size that outlive requests and die one by one (syntax-tree blocks, token trees, crate sources' bytes) | **kept blocks**, `blaze_mem::Kept(tag)`: an `Allocator` over `blaze_mem::blocks`' size classes (four per doubling, 64 B to the largest class), one reserved region per class, no header (the class comes from the layout), bytes counted to the tag | one by one, onto its class's free list for the next value of that size; never to the general-purpose allocator. A class of blocks of 256 KiB or more gives a block's pages back to the OS as it is freed; a smaller class, once its free bytes pass 2 MiB, gives back the whole pages its free blocks cover until 1 MiB is left. The address range stays reserved, so a block keeps its address and the free list stays lock-free |
| a whole program or test | **root**, `blaze_mem::Root`: one reservation in size classes, made at the entry (or a test's top) and handed down as `Alloc` (D102, D173) | when the program ends |

Owned text names its allocator too (`blaze_mem::text`). std's `String`, `OsString` and
`PathBuf` take none, so blaze_mem has its own, each a `Vec<u8, A>` with no default `A`:

| type | is | made in a frame with |
|---|---|---|
| `blaze_mem::String<A>` | UTF-8, as std's `String`; compares, orders and hashes as `str` and borrows as it, so it is a map key found by `&str`; `fmt::Write`, `into_boxed_str` | `frame::string(frame, text)`, `String::new_in(frame)`, `format_in!(frame, ..)` |
| `blaze_mem::OsString<A>` | any bytes, as std's `OsString` on unix; as `OsStr` | `frame::os_string(frame, text)` |
| `blaze_mem::PathBuf<A>` | as std's `PathBuf` on unix (`push`, `pop`, `join`); as `Path` | `frame::path(frame, path)` |

`OsString` and `PathBuf` are unix only: on Windows an `OsStr` is WTF-8, and std has no public
way to join two of them as bytes soundly, so there the types are absent (naming one is a compile
error), never a quiet global-heap stand-in. `blaze::implicit_allocator` points at these.

Single-owner maps and sets name theirs the same way: `blaze_hash::HashMap<K, V, A>`,
`HashSet<K, A>`, `ByHash<K, V, A>` and `ByHashSet<K, A>` (over blaze-swiss) have no default
`A`, built with `new_in(alloc)`, `with_capacity_in(n, alloc)` and `from_iter_in(iter, alloc)`. A
map that lives within a task takes the frame (`&Frame`); one that outlives it takes the request,
revision or root allocator. Never a frame's allocator in something that outlives the frame.

## Techniques

- **Virtual memory reserved up front.** Each growable arena and column reserves a large
  address range at startup (64 GB, which is free on 64-bit) and commits pages as it grows
  (`mmap` with `PROT_NONE`, then `mprotect`; `VirtualAlloc` with `MEM_RESERVE` then
  `MEM_COMMIT`). Growing never moves data, so addresses and `u32` offsets stay stable, and
  there is no realloc-and-copy. Huge pages back the hot regions.
  Address space is not free on macOS: a `PROT_NONE` reservation costs in proportion to its
  size (arm64, 2026-11-15: 4 GiB 7 us, 64 GiB 98 us, 256 GiB 290 us), so one 256 GiB chunk
  space made a command's first allocation 0.3 ms. There a big range is reserved in 4 GiB
  segments as it is used (`chunks::Segments`); Linux reserves in constant time, in one.
- **Generational handles.** Objects that are freed and reused (tasks, I/O operations, watch
  slots, live-patch sites) are referred to by `(index: u32, generation: u32)`. A stale handle
  is detected, not dereferenced.
- **SoA tables, as in an ECS.** Data of the same kind sits in columns with a `u32` ID per row. Adding or removing rows touches only the columns involved, and a scan
  reads only the columns it needs.
- **Per-thread caches, batched returns.** Pools hand out from a worker-local cache, and refills
  and returns move to the shared pool in batches of 64, so the shared lists see one atomic
  operation per batch.
- **Scratch buffers reused per worker**, grown once to their high-water mark and never freed in
  the warm loop: formatting, hashing, encoding, decompression.
- **Ring buffers for streams**: the journal, I/O completions, the event feed to the agent API.

## Proof

- Allocation-budget tests (not built): the warm loop (edit, re-check, codegen, patch) makes zero
  allocations from a general-purpose heap on the corpus's edit scripts.
- Fragmentation: resident memory after 10,000 edits is within 10% of resident memory after
  the first.
