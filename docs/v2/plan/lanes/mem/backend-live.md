---
status: plan
summary: blaze v2 mem-backend-live - the splice's symbol tables and undo journal on blaze-mem regions, and the requested moves of blaze-live's log of generations and blaze-lazy's waiters, with the cost of each against its bound.
date: 2026-10-29
---

# mem-backend-live: live patching and the splice onto blaze-mem

A child of [backend.md](backend.md), under lane mem's plan
([../../design/memory.md](../../design/memory.md)). Paths: `blaze-splice`, `blaze-in-place`
(written here), `blaze-live` and `blaze-lazy` (requested of their holders in
`v2/requests/mem-backend-live.md`). `blaze-tier` does not exist in the tree.

## By lifetime

| what | lifetime | before | after |
|---|---|---|---|
| ELF, Mach-O and PE symbol and import tables, PE rooms (`blaze-splice` `Image`) | the baseline: read once per link, looked up by every splice | `FxHashMap<Box<[u8]>, V>`: one allocation per name, a rehash per doubling | `names::Names<V>`: name bytes in one `RegionVec<u8>`, entries in a `RegionVec`, an open-addressed `u32` index at most half full, all `Tag::Link` |
| what a splice wrote over (`Baseline::undo`) | one splice to the next | `Vec<(u64, Vec<u8>)>`: one `Vec` per site per splice, and a clone of the whole list per Mach-O splice | `journal::Journal`: bytes and runs in two `RegionVec`s, emptied and refilled in place; the Mach-O splice keeps its copy in a second journal (`Baseline::undone`) and swaps it back on failure |
| the runs `Signature::of` rehashes | one Mach-O splice | a clone of every overwritten run's bytes | `(offset, len)` pairs from the addition itself |
| the names `can_call` and the PE `__imp_` lookup build | one call | `format!` / `concat()` | `blaze_mem::scratch::with_bytes` |
| `.eh_frame` frames counted for a patch | one call | a `Vec` built to take its `len()` | `Patch::each_frame`, a visitor |
| `blaze-live` `Log` (ten columns per generation) and `Lasting` ranges | the program's life | `Vec`s doubling on the warm apply path | `Vec`s, kept: refused, blaze-live takes no dependency (queue 00191) |
| `blaze-lazy` `Fetcher` waiters | a function's compile | a `Vec<Waiter>` per function in flight | a `Slab<Waiting>` list per function, the map holding only first and last handle (requested) |

`blaze-in-place` (wait for the daemon's write, copy with holes) holds no table and no per-call
buffer: nothing to move.

What stays: `Baseline::originals` (address to the linker's bytes) is cloned into every
`Baseline::view` and grows only at a site never spliced before, so it is not on the warm path;
`debug::Described::sections` is a dozen names; `Lasting` (the splice's) is `Clone` and handed to
callers. The per-patch `Patch` maps (`targets`, `in_place`) live for one splice and are next
for the task's `Frame` once `Vec<T, &Frame>` and an allocator-aware map are threaded through
`Patch<'a>`.

## Speed of light

- **A symbol lookup.** Work: hash the name (Fx, 8 bytes per multiply, 2-4 ns for a 40-byte
  mangled name), one load of the index slot (a cache miss on a cold table, about 80 ns), one load
  of the entry, one compare of the name's bytes (another line). Bound: two dependent misses plus
  the hash, about 170 ns cold, 10 ns warm. `Names` does exactly that: at half load a linear
  probe finds the name in 1.5 slots on average, and the stored 64-bit hash makes a wrong slot
  cost no byte compare. A `HashMap<Box<[u8]>, _>` costs the same probes plus a pointer chase to
  the boxed key, a third miss.
- **Reading the tables.** Work: one pass over the symbol table, writing each name once. Bound:
  the bytes of the names at memory bandwidth (a 1M-symbol executable has about 60 MB of names:
  3 ms at 20 GB/s), plus one index store per name. `Names` copies each name once into committed
  pages and never moves it; growth of the index rehashes from the stored hashes without touching
  a name. The ELF and Mach-O tables are sized from the symbol count up front, so they never grow.
- **The undo journal.** Work: read each overwritten run once (a `pread`), store it, write it
  back once. Bound: the syscalls; the journal adds one copy of bytes already in cache and no
  allocation once warm, so it is within 1% of the syscalls' cost.

## Tests

- `names::tests`: a model test against `FxHashMap` over 5,000 names (with repeats, `get_mut`,
  the empty name), a test that a value is made only for a new name, and the budget test: reading
  20,000 names makes no global allocation and looking each up makes none.
- `journal::tests`: a model test against `Vec<(u64, Vec<u8>)>` over three rounds (order both
  ways), a failed read recording nothing, a copy put back, and the budget test: two warm splices'
  record, save and put-back make no allocation.
