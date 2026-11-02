---
status: plan
summary: lane mem's child mem-format: the speed of light of the front end's interners, a map of a file's ast ids, the lexer's columns and the per-call scratch of parse, lowering and macro expansion once they sit on crates/blaze-mem, the arithmetic, and the designs they are held against.
date: 2026-10-29
---
# mem-format: speed of light

The machine and the bar are those of `speed-of-light.md` in this directory: a 4 GHz core, an
L1 hit 4 to 5 cycles, an L2 hit about 14, a miss to memory about 80 ns, an uncontended atomic
read-modify-write 10 to 20 cycles, a global `malloc`/`free` pair 20 to 50 ns warm and a page
fault on first touch about 0.25 µs. All numbers are to be checked by the coordinator's
benchmarks once everything builds. The changes are in `v2/requests/mem-format.md` and the
children's request files; the notes are in `format.md`.

## An interner hit (`intern`)

The least an interner hit can do is hash the key, load one group of the table and compare one
stored value: two cache lines, the group (tags and pointers share a 64-byte line) and the
value. Warm, that is two L1 hits and the hash: about 10 ns for a short string with FxHash, and
2 × 80 ns cold. The store's probe is already that (`swiss.rs`, one group per line). What
blaze-mem changes is what a hit cannot pay for: no lock, no allocation, no reference count, so
the hit is within 10% of the two loads. The design it is held against is rustc's
`Symbol::intern` (a sharded `FxHashMap` behind a lock plus a `DroplessArena`), whose hit pays
the lock's two atomics, 20 to 40 cycles, on top.

A miss adds the copy of the value into the store's region and the publish of its slot. The
copy is bound by bandwidth; claiming room is a bump within the shard's chunk, and a new chunk
is one `fetch_add` on the shared region's cursor per chunk (64 KiB, so 1/1000 of an atomic per
32-byte value). The first touch of each page is the kernel zeroing it, about 0.25 µs per 4 KiB,
0.06 ns a byte: within 10% of the copy for values past a few bytes. `Box::new` per chunk
before cost a `malloc` and, for big chunks, an `mmap` and its faults; both go.

## An ID's value (`intern::table`, `Symbol` to value)

The least is one load of the slot at `base + 8 × id`. A slot column on a reserved region is
exactly that: the base is fixed, so there is no bucket to find. The bucketed column before
found the bucket with a `leading_zeros` and loaded its pointer first, a dependent load that
cold is a second miss. The region removes it.

## `blaze_ids::Sharded`

A hit is the probe of one level's table (one line) and the read of the value at its index in
the `Column` (one line, the ready bit beside it), the same two lines as the intern store's.
A miss's index is claimed in blocks of 64 per shard (`Column::claim`, request 1 to mem), so
the shared counter is one atomic per 64 values and the assignment of IDs stays exactly as it
is. The probe tables stay boxed: a column-backed table would put a ready-bit load in every
probe of a hit, a 10% loss on the path that matters, to save one allocation per doubling.

## A file's ast id map (`span::AstIdMap`)

A build walks the flat tree's arrays in order, about 6 bytes read per node plus a hash per
item: bandwidth-bound, 1 to 2 ns a node. The global allocator added about a dozen
`malloc`s per build (entries doubling, then shrunk; the bodies found; three walk stacks; a
hash map rehashed as it grew; the radix sorts' scratch), 0.5 to 1 µs per file, which for a
small file is as much as the walk. With the buffers on the task's frame and the sorts on kept
scratch (request 4), a warm build allocates its result only, three blocks, or eight with fn
bodies, at their exact length, and the frame's pages are warm, so there is no fault either.
What is left above the walk is the copy of the result out of the frame, one pass over the
entries: within 10%.

## Per-call buffers of the lexer, parser, lowering and mbe

These are the children's sections in `format.md` (`mem-format-tokens`, `mem-format-tree`,
`mem-format-hir`). The bound for each is the same: the pass over its input with the buffers
already committed and in cache. A warm `scratch` lend is two thread-local accesses and a
`clear`, a few ns; a `frame::task` is the same. Against that, a fresh `Vec` per call is a
`malloc` and, past a page, a fault per 4 KiB the first time each block is touched, so the
lexer's two columns of 4 bytes a token fault once per 1024 tokens: 0.25 ns a token on a pass
that costs 2 to 4. Lending the columns removes that and the `malloc` and is within 10%.
