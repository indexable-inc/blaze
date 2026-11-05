---
status: plan
summary: blaze v2 borrows from Flecs, ECS and game engines: shapes, sparse overrides, relationship pairs, table change counters, archetype batching, fiber jobs, frame graphs, command buffers, LOD-style tiers, streaming, Live++-style hot reload, Tracy-style markers.
date: 2026-10-29
---
# Prior art: Flecs, ECS and game engines

Games ship a frame every 16 ms on fixed hardware, and their engines are the best prior art for
blaze's warm loop. Each technique below has a home in v2. Flecs itself stays in the site and
the build-page world view, not in the compiler's hot paths (Flecs's storage is dynamic, in
memory and not byte-stable; ours is static, mapped and deterministic).

## Data (lanes format, db, items, speed)

- **Shapes, as V8's hidden classes.** Small repeated records (function headers, field
  metadata, derive lists, generic bounds, whitespace) are interned. Each row stores a `u16`
  shape ID instead of the record.
- **Sparse overrides, as Flecs prefabs.** Settings that are inherited (lint levels, `cfg`,
  `#[doc(hidden)]`, edition, `#[track_caller]` defaults) are stored once per scope. An item
  stores only its overrides, which for almost every item means none.
- **Relationship pairs, as Flecs pairs.** "impl I implements trait T for self type S",
  "module M glob-imports N", "item X's signature mentions Y" are indexed pair tables, read per
  bucket. Each pair is a fact for invalidation ([frontend.md](frontend.md) items 1 to 3).
- **Archetype batching.** Rows of the same kind and shape sit together, so a pass over them
  is one branch-free loop: all private plain functions, all `Copy` structs.
- **Change counters per table, as Flecs's table change detection.** Every column keeps a
  revision counter, so a scan skips columns that haven't changed since it last ran, before
  looking at any row.
- **Bit-packed components.** Booleans and small enums live in bitsets and packed nibbles, not
  bytes ([data.md](data.md)).

## Scheduling (lanes core, db)

- **A fiber job system, as Naughty Dog's.** A query that must wait (single flight on another
  task's key, an I/O completion) parks its fiber, and the worker takes other work at once. The
  query's code stays straight-line, with no continuation passing, and no OS thread blocks.
  Fibers come from a pool with guard pages, and switching saves only the callee-saved registers.
- **A frame graph, as render graphs.** Each request's work is a graph of passes with declared
  inputs and outputs. The plane orders it by critical path, overlaps independent passes, and
  reuses the memory of results nothing reads again (transient-resource aliasing).
- **Command buffers.** A task records its writes (new facts, new interned values, store puts)
  in its own buffer, merged in one batch at its boundary, so shared tables see one publish per
  task ([concurrency.md](concurrency.md)).
- **Double buffering.** The next revision builds while readers use the current one
  ([memory.md](memory.md)).

## Work (lanes fastcompile, codegen, link)

- **Levels of detail.** As a game renders far objects coarsely, blaze compiles cold functions
  with copy-and-patch and hot ones with LLVM (tiers, [compile-time.md](compile-time.md)
  change 13).
- **Asset streaming.** As a game streams the next area before the player arrives, blaze
  prefetches the dependency image, global-cache values and higher tiers for what the next
  request will likely need (changes 7, 8, 14, 15).
- **Live++ for hot reload.** Live++ is the state of the art for hot-reloading C++ in games:
  - it patches functions with jump hooks;
  - it relocates and preserves statics;
  - it keeps function pointers and vtables valid;
  - it links only the changed code against the running image.

  `blaze-live` matches or beats it, and its notes compare the two point by point.
- **Precompiled shader caches.** As engines ship compiled shaders, blaze ships compiled
  dependencies through the global cache.

## Visibility (lanes core, mem)

- **Tracy-style markers.** Every plane task, query, allocator and I/O operation emits a marker
  into a per-worker ring buffer, at near-zero cost when nobody listens. A live timeline shows
  the critical path and memory per subsystem, streamed through the agent API.
- **Budgets per subsystem**, as engines budget memory per system ([memory.md](memory.md)).
