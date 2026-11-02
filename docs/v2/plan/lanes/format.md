---
status: plan
summary: blaze v2: data formats: IDs, zero-copy records, flat syntax, side tables.
date: 2026-10-29
---
# blaze v2: format

Part of the [v2 plan](../../plan.md). Section numbers refer to the plan as first written.

## 5. Data formats

One rule: **the bytes in memory are the bytes on disk.** Every long-lived structure is
pointer-free, aligned and position-independent, so it can be mmapped and used in place.

- **IDs, two kinds**: a dense `u32` index per arena for speed, and a content hash for identity.
  Dense indices are assigned in arrival order by parallel workers, so they differ between runs
  and never appear in a hash, a key or the bytes of an output; hashes, keys and outputs use
  content IDs only, and each arena maps one to the other. No `Arc` in long-lived data. Interned
  values (symbols, types, paths) sit in sharded, append-only interners keyed by content hash.
- **Syntax**: one flat tree per file everywhere (no rowan, no IDE to keep): tokens as (kind,
  length) with text read from the mmapped source; nodes in preorder as (kind, subtree size);
  parents and offsets by arithmetic. About 8-12 bytes a node instead of ~40 and an allocation.
  An edit reparses the changed item into a new flat tree. Builds on #42 (flat-syntax).
- **Schema**: every on-disk structure has a versioned schema hash. The image and packs are
  keyed by it, so a blaze build that changes a layout reads nothing it can't read; it rebuilds
  the image from the store's results (fast, section 7) instead of from source.
- **HIR, bodies, MIR**: struct of arrays (kinds, types, spans in their own columns), `u32`
  indices.
- **Spans** in a side table relative to their anchor, out of every hash; a formatting edit
  changes the side table only.
- **Machine code**: each function a relocatable blob with its relocation list, keyed by its code
  hash; debug info and panic locations in a side table, keyed by the same hash.

## Issues this lane takes in

- **#806, #734**: memory goes to compiled code kept after it's in the store (23%), the MIR of
  every compiled body (15%), and parses of every dependency body kept for `declared_in` (10%).
  v2 moves results to disk once stored, and parses dependency bodies only on demand (below).
  (`v2-db`, `v2-format`)
- **#1293 demand-driven front end** and **#1294 outline parse**: dependency crates are parsed
  as item outlines (4-byte packed tokens), and their bodies are parsed, lowered and inferred
  only when codegen reaches them. `blaze check` still checks the workspace's crates fully.
  (`v2-format`, `v2-db`)

## Done when

- blaze-format's records, schemas and the three compression tiers round-trip exactly
- flat syntax replaces rowan in syntax, parser, tt, mbe and their users
- HIR bodies and item trees are columns within their size budgets (const asserts)
- outline parse and on-demand bodies for dependency files
- nothing in the lane's paths matches `rg 'unimplemented!|todo!'`; tests for all of it are written; its report says `STATUS: DONE`
