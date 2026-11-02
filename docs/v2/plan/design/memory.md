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

The general-purpose allocator (mimalloc, with huge pages) serves only startup, configuration
and rare large one-offs. The allocation-budget tests hold the warm loop to zero calls into it.

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
