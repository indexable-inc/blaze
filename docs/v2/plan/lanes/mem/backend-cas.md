---
status: plan
summary: mem-backend-cas - blaze-cas's in-memory tables (delta, values made whole, flag columns) onto reserved regions tagged Tag::Store with values in a slot region recycled after their grace, segment chunks on regions of their own, a chunked put's temporaries in reused scratch, what is not moved and why, and the tests that hold it.
date: 2026-10-29
---

# mem-backend-cas: the store onto blaze-mem

Part of [backend.md](backend.md), under lane mem's [memory.md](../../design/memory.md). Every
change is in `crates/blaze-cas`; the on-disk formats, the bytes a put writes, what a read returns
and the order of everything are unchanged. Speed of light: [speed-of-light-cas.md](speed-of-light-cas.md).

## What lives how long in the store

| what | lifetime | was | is |
|---|---|---|---|
| a table's slots and flag words (each delta, the values made whole) | the table | a `Box<[AtomicPtr]>` and a `Box<[AtomicU64]>` a level, each level allocated when made | one region each, reserved for the most rows the table can have, a level committed when made (`words.rs`) |
| a table's values (`Spot`, `Decoded`) | until swapped out and collected | one `Box` a value | slots of a region the table owns, carved with one `fetch_add` (`table.rs` `Values`) |
| a value swapped out (a commit pointing a key at its pack, a settle) | until a grace begun after the swap passes | handed to the store's `Limbo`, one `Box` a hold | kept in its slot; `Table::collect` (from `tidy`) drops it and keeps the slot for the next swap |
| the index's flag columns (`Bits`) | the mapped index | a `Box<[AtomicU64]>` | a region, committed whole |
| a writer's segment chunk | until drained and its grace passes | a boxed `Chunk` and a `vec![0; cap]` | a region of its own, the header at its base, the bytes after |
| a chunked put's temporaries (the cut list, the chunk refs, each chunk's LZ4, a raw chunk's copy, the payload) | one put | five kinds of `Vec`, one LZ4 `Vec` a chunk | none: cuts walked lazily, refs encoded straight into a reused payload buffer, LZ4 into a second reused buffer, a raw chunk borrowed (`scratch::with_bytes`, two deep) |

Every region is tagged `Tag::Store`, so the store's memory shows in the tag report and its budget.
A region that cannot be reserved or committed, or a table that would grow past its reservation,
stops naming the tag, the reservation and what was wanted (`words::stop`); nothing falls back to
the heap.

## Why not `AppendVec`, `Column`, `Slab` or `Pool`

- **Slots and flags.** `AppendVec` has one writer; any thread inserting into a delta may make the
  next level. The levels are therefore stretches of one `Region`, made under the level's
  `OnceLock`: `Region::ensure` is safe from any thread and the kernel's zeroed pages are a valid
  null pointer and a clear flag word (`words::Zeroed`). Under `cfg(blaze_check)` the checker's
  atomics are not zero bytes, so each stretch is a boxed slice of them instead.
- **Values.** A value is read by any thread under a guard and never moves, so it needs a stable
  address and reclamation by grace. `Pool<T>` hands out a `Pooled<'p, T>` tied to the pool's
  borrow (self-referential inside the table) and frees at once; `Slab` frees at once too. The
  table's own slot region plus a recycling list only the store touches (`Recycle`: the slots
  swapped out with their grace, and the slots freed) gives both without a lock: inserts carve,
  the store's swaps take freed slots first.
- **Recycling state.** Only the store swaps and collects, one call at a time; a flag that stops a
  second caller at once guards it (`Owner::with`), as `AppendVec` guards its writer. The crate's
  `clippy.toml` refuses `std::sync::Mutex`.

## What is not moved, and why

- `Tags.ops`, `fetching` and the remote tier's `Client.waiting` / `Server` maps: keyed by an I/O
  tag or a content id and holding `FileName`s or request state; a hashbrown map keeps its
  capacity, so a warm insert past its high-water mark allocates nothing already, and renumbering
  I/O tags into a dense column would change the effects the store emits. The remote tier's
  *index* is a tier's delta and mapped index, which are the tables above.
- `View` rebuilt on each publish (a `Vec` of tiers and their pack lists): once per commit step,
  freed by grace; moving it needs views in a region with grace-aware reuse, a follow-up.
- `Decoded.bytes`: the value made whole is what the reader gets, kept until the table is let go
  (`OWNED_MOST`); it is sized by the value and is a cold path (decompression).
- The per-commit `Vec` of unlinked chunks held in the limbo, and the limbo's own box a hold: one
  a commit.
- Error strings (`Corrupt`, `Mismatch`, a failed compress): at the edge, on a path that stops.

## Invariants the code relies on

- A value slot is written once by `place` (claimed by the `fetch_add`) or, for a recycled slot,
  by the store after the slot's old value was dropped past its grace; it is published by the
  slot's release CAS or swap and never written again while linked.
- A value placed for an insert that then finds its key published by another is read back out of
  its slot and returned; the slot stays carved and unused. Such slots are bounded by the lost
  races; `ROWS_MOST` leaves room for them, the live values (at most half a level) and those
  swapped out awaiting their grace.
- A spot moved off a segment is swapped before the grace of the chunks it named begins, so a
  reader that still holds the old spot began before the chunks' grace and the chunk outlives it.
- A chunk is freed by reading it out of its region (`ChunkRef::free`): the copy owns the region,
  so dropping it unmaps the original; the model's byte tracking sees the free (`track::free`).

## Tests beside the code

- `words.rs`: stretches read as a `Vec` across 14 doublings (many commit steps); pointers start
  null; a stretch past the reservation stops naming the store.
- `table.rs`: inserts across levels; a replace keeps the row and the old value until collected;
  the table against an `FxHashMap` model over 200 000 random inserts, replaces, collects, flags
  and rotations across 8+ levels; collected slots take the next swaps (no new carve, each value
  dropped once); flags per row; racing inserts keep one value a key; allocation budgets: hits,
  inserts that make new levels past the high-water mark, and warm replaces with collects, zero.
- `flags.rs`: the columns against six `Vec<bool>`s over 3 M rows (many commit steps).
- `segment.rs`: a chunk reads back what was written across its pages and is zero when made.
- `cold.rs`: `lz4_into` a reused buffer makes exactly `lz4`'s bytes and keeps exactly when it does.
- `tests.rs`: `hits_and_warm_puts_allocate_nothing`: a writer's warm puts (past chunk rolls and
  the delta's first level), after a commit too, and hits in segments, packs and the index, zero
  allocations; `reclaim_all` also waits for the deltas' swapped-out spots.
- `model.rs`: the replace model now collects through the table rather than a limbo.

Nothing here was built or run (the brief forbids it): the first `blaze test -p blaze-cas` and the
`cfg(blaze_check)` model run are the check.

## Follow-ups

- Carve value slots in blocks a writer (64 at a time) so concurrent writers do not share the
  `carved` line (see speed-of-light-cas.md).
- Recycle chunk regions through a store-owned free list instead of unmapping each drained chunk.
- Size `ROWS_MOST` per table kind: a delta reserves about 8 GB of address space for its values
  (no memory: reservations are not committed), which a constrained `ulimit -v` would refuse.
