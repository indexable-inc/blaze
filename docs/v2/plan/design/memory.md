---
status: plan
summary: blaze v2 manages memory like a game engine: allocators by lifetime (frame, request, view, image), virtual-memory-reserved arenas with stable addresses, pools, generational handles, per-subsystem budgets, and zero general-purpose allocations in the warm loop.
date: 2026-10-29
---
# Memory, the way a game engine does it

A game has a frame budget and never calls `malloc` per frame. blaze's frame is an edit: from
a file changing to code patched in. The warm loop should allocate nothing from the
general-purpose allocator. Every byte comes from an allocator chosen by how long it lives.
All of it lives in `crates/blaze-mem`.

## Allocators by lifetime

| lifetime | allocator | freed |
|---|---|---|
| a task (one query run, one codegen unit) | **frame allocator**: a bump pointer per worker, with markers for scoped rollback | all at once when the task ends, by resetting the pointer |
| a request (one edit, one build, one test run) | **request arena**, one per request, chunked | when the request completes |
| a view's revision | **revision arenas**, double-buffered: the next revision builds while the current one is read | when no reader holds the old revision (task boundaries, [concurrency.md](concurrency.md)) |
| the image (results, interners, tables) | **mapped regions** in the store's packs ([restore.md](restore.md)) | by GC, by moving live data |
| fixed-size objects that come and go (plane tasks, I/O operations, timers, watch slots) | **pools**: per-type slabs with free lists, per worker, refilled in batches | back to the pool, never to the OS in the warm loop |
| image values of any size that outlive requests and die one by one (syntax-tree blocks, token trees, crate sources' bytes) | **kept blocks**, `blaze_mem::Kept(tag)`: an `Allocator` over `blaze_mem::blocks`' size classes (four per doubling, 64 B to the largest class), one reserved region per class, no header (the class comes from the layout), bytes counted to the tag | one by one, onto its class's free list for the next value of that size; never to the general-purpose allocator. A class of blocks of 256 KiB or more gives a block's pages back to the OS as it is freed; a smaller class, once its free bytes pass 2 MiB, gives back the whole pages its free blocks cover until 1 MiB is left. The address range stays reserved, so a block keeps its address and the free list stays lock-free |

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
`HashSet<K, A>`, `ByHash<K, V, A>` and `ByHashSet<K, A>` wrap hashbrown's tables with no default
`A`, built with `new_in(alloc)`, `with_capacity_in(n, alloc)` and `from_iter_in(iter, alloc)`. A
map that lives within a task takes the frame (`&Frame`); one that outlives it takes the request,
revision or image allocator, or `std::alloc::Global` with `// alloc: <why>` on its line. Never a
frame's allocator in something that outlives the frame. papaya's shared tables
(`blaze_hash::SharedMap`, `SharedSet`, `SharedByHash`) are exempt: papaya takes no allocator, and
they are long-lived by design, so `blaze::implicit_allocator` leaves them alone.

The general-purpose allocator (mimalloc, with huge pages) serves only startup, configuration
and rare large one-offs. The allocation-budget tests hold the warm loop to zero calls into it.

Which general-purpose allocator matters less than what is left on it. Measured 2026-11-03 on
`blaze build -p blaze-engine --no-default-features --features seed` with one worker, Linux
aarch64, THP `always`, eight runs started together and stopped at 900 s, each having built the
same 79 units (lane heap2, /tmp/blaze-lanes/v2-heap2.md): peak RSS was 13.6 to 14.7 GiB in every
configuration. mimalloc peaked 0.2 to 0.6 GiB above glibc, `MALLOC_ARENA_MAX=2` 0.1 GiB below
it, and huge pages only where advised (`prctl(PR_SET_THP_DISABLE, 1,
PR_THP_DISABLE_EXCEPT_ADVISED)`, Linux 6.18 and later) cut huge pages from 11 to 7 GiB but RSS
by only 0.25 GiB (0.5 GiB with both). So the process keeps the allocator it ships with, and
memory work goes to the bytes on the global heap (5.5 GiB at that peak) and to blaze-mem's own
tags (query 4.4 GiB). Moving syntax-tree blocks, token trees and crate sources to `Kept` took the
global heap from 8.0 to 5.5 GiB and left peak RSS the same: the bytes are live either way, and a
size class keeps the blocks it had at its most (the `chunks` tag, 0.24 to 2.35 GiB committed).
Counting each class (lane heap3, /tmp/blaze-lanes/v2-heap3.md, the same build, three runs of
each binary alternated in one VM) showed that gap was not free blocks: the classes' blocks
were 2.8 to 3.0 GiB live of 3.08 GiB carved, and their free blocks held 0.14 to 0.45 GiB over
the run (`committed` also counts pages committed ahead and never touched). Giving free pages
back (0.85 GiB over the run, free blocks 0.01 to 0.1 GiB) and moving trees' text, expression
stores and `hir_def` column slabs to `Kept` (global heap 6.4 to 5.2 GiB) moved peak RSS from
13.61 to 14.01 GiB to 13.47 to 13.91 GiB: within the runs' spread. The peak is live bytes:
the query tag (4.4 to 5.0 GiB), kept values (3.0) and what the global heap still holds.
macOS was not measured.

## Techniques

- **Virtual memory reserved up front.** Each growable arena and column reserves a large
  address range at startup (64 GB, which is free on 64-bit) and commits pages as it grows
  (`mmap` with `PROT_NONE`, then `mprotect`; `VirtualAlloc` with `MEM_RESERVE` then
  `MEM_COMMIT`). Growing never moves data, so addresses and `u32` offsets stay stable, and
  there is no realloc-and-copy. Huge pages back the hot regions ([hardware.md](hardware.md)).
- **Generational handles.** Objects that are freed and reused (tasks, I/O operations, watch
  slots, live-patch sites) are referred to by `(index: u32, generation: u32)`. A stale handle
  is detected, not dereferenced.
- **SoA tables, as in an ECS.** Data of the same kind sits in columns with a `u32` ID per row
  ([data.md](data.md)). Adding or removing rows touches only the columns involved, and a scan
  reads only the columns it needs.
- **Per-thread caches, batched returns.** Pools hand out from a worker-local cache, and refills
  and returns move to the shared pool in batches of 64, so the shared lists see one atomic
  operation per batch.
- **Scratch buffers reused per worker**, grown once to their high-water mark and never freed in
  the warm loop: formatting, hashing, encoding, decompression.
- **Ring buffers for streams**: the journal, I/O completions, the event feed to the agent API.

## Budgets and visibility

- **Every allocator is tagged with its subsystem** (parse, nameres, infer, MIR, codegen, link,
  store, VFS, usercode). It reports live bytes, high-water mark and allocation rate.
- **Each subsystem has a budget.** Memory admission ([core.md](../lanes/core.md)) reads these
  counters. A subsystem over budget is throttled before the machine is, so the daemon never
  OOMs.
- The agent API exposes the counters, so a regression shows up as a number, not a mystery.

## Proof

- Allocation-budget tests: the warm loop (edit, re-check, codegen, patch) makes zero calls into
  the general-purpose allocator on the corpus's edit scripts.
- Fragmentation: resident memory after 10,000 edits is within 10% of resident memory after
  the first.
