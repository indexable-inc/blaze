---
status: plan
summary: mem-backend-link - blaze-link's symbol table onto reserved regions, a stage's scratch and sets onto a blaze-mem Frame, the image state's copy-on-write chunks onto shared pools, why the jump table's slots are not moved, and each cost against its bound.
date: 2026-10-29
---

# mem-backend-link: the linker onto blaze-mem

Part of [backend.md](backend.md), under lane mem's [memory.md](../../design/memory.md). Every
file of `crates/blaze-link` was held by an agent still working when this was written
(`requests-link-fuzz-core-model`, `-core-stage`, `-pe`, `-wasm`, `speed-link-edge-sol-link`), so
the moves into existing files are exact edits in `v2/requests/mem-backend-link.md` (L1 to L5); the
new code is in two new files, `src/intern.rs` and `src/arena.rs`, which compile once `lib.rs`
declares them (L1).

## What lives how long in the linker

| what | lifetime | was | is |
|---|---|---|---|
| symbol keys, names, content ids (`Symbols`, 3 tables × 64 shards) | the image | `Vec`s, a `String` and a `hashbrown::HashTable` per shard, each grown by doubling | `RegionVec`s tagged `Tag::Link` and an in-place index ([`intern.rs`](#the-symbol-table)) |
| a stage's temporaries (piece contents, fixup lists, per-piece orders, write lists) | one stage | a `bumpalo::Bump` the image keeps, reset per stage | a `blaze_mem::Frame` the image keeps, reset per stage (L3) |
| a stage's sets (`fresh`, `write`, `dirty_cells`, `changed`) | one stage | heap `IdSet`/`SymSet`, grown from empty every stage | the same sets over the stage's frame (L4) |
| the image's state (pieces, definitions, lists, region bytes) | the image, 16 commits of history | copy-on-write chunks behind `Arc`s, O(1) clone per stage | chunks, pages and table nodes in `blaze_mem::Shared` slots of static pools, O(1) clone ([below](#the-image-states-copy-on-write-chunks)) |
| jump-table slots, piece ids | the image | append-only columns in the state | unchanged: never freed, so not a `Slab` (below) |
| a commit's effects, the patch message | one commit | `VecDeque`s and `Vec<u8>`s | unchanged: `blaze_core::Effect` owns its bytes and file names (the edge) |
| full-link stubs (`table::stubs`) | the image's region bytes | one `Vec<u8>` per full link | unchanged: it becomes `CowBytes`' base for the image's life, once per full link |

## The symbol table

Each shard of each table is entries in `RegionVec`s and an `Index`:

- keys: `RegionVec<SymKey>` (20 B) + index; names: `RegionVec<u8>` bytes, `RegionVec<u32>` ends,
  + index; ids: `RegionVec<ContentId>` (32 B) + index.
- `Index`: `RegionVec<u64>` slots, a power of two of them, at most half full; a slot is the high
  32 bits of the entry's hash beside its index plus one (0 is empty), probed linearly from the
  hash's low bits. Bits 32 to 37 of every hash in a shard are the shard, so the tag compare
  filters on the other 26; the probe start fixes the low bits independently.
- Growth: a column commits pages (`max(committed / 4, 64 KiB)` per system call) and never moves;
  the index doubles by committing its next half and putting every entry back from its hash.
  Keys and ids hash in a few instructions (three multiplies; the id's first eight bytes), names
  through FxHash again, so no hash column is kept: 8 B per entry saved, and a name is hashed at
  most twice more over a shard's life (once per doubling it lives through, a geometric sum).
- Bounds per shard: \(2^{20}\) entries per table (\(2^{26}\), 67 million, per image: past ten times
  Chromium's 4 to 5 million symbols) and 64 MiB of name bytes (4 GiB per image; every end fits a
  `u32`). Reserved: \(64 \times (2^{20} \cdot (20 + 32 + 4) + 2^{26} + 3 \cdot 2^{21} \cdot 8)\) B
  = 10.5 GiB of address space per image, none committed until used. Past a bound: the
  `MemError` naming `Tag::Link`, the stage stops (no-fallback).
- Same ids, same order: an entry's index is its position in its shard, and each shard interns in
  the batch's order, as before. `batch`'s parallel path is unchanged (`RegionVec<T: Send>` is
  `Send`).
- One `unsafe` block: a name's bytes viewed as the `&str` they were appended from
  (`name_at`), with the invariant (the only writer appends a `&str` and then its end) stated and
  a checked reference in the tests.

Mappings: 448 regions per image (7 per shard), 896 Linux mappings once partly committed. With
`vm.max_map_count` at 65 530 that caps a daemon near 70 images on the symbol tables alone; lane
mem is asked (L5) for the install-time setting and a start-up check that stops naming it.

### Speed of light

- Lookup (a hit): one hash (keys: 3 multiplies, about 2 ns), one slot load (a cache miss in a
  cold table, 80 ns from DRAM; an L2 hit, 4 ns, warm), one tag compare, one entry load (a second
  miss cold) and a 20-byte compare. The bound is the two dependent memory accesses; the
  SwissTable design also needs two (control group, then entry), so this is at the bound and
  equal to what it replaced. Load at most 1/2: a hit probes 1.5 slots on average and a miss 2.5
  (linear probing), eight slots to a 64-byte line, so nearly always one line.
- Insert (a miss): the lookup, one store of the entry (sequential, write-combined) and one slot
  store. Amortized rebuild: \(\le 2\) slot writes per entry over its life (geometric doubling),
  the same as a SwissTable's rehash, with no second table allocated and freed.
- Growth: one `mprotect` per commit step (\(\ge 64\) KiB, so \(\le 1\) system call per 3 277
  keys); the old code's `realloc` of a doubling copied the whole shard (memory bandwidth, 20 GB/s:
  50 µs per MiB) and called the allocator. Zero copies now.
- Footprint: 16 to 32 B of index per entry (load between 1/4 and 1/2), against the SwissTable's
  5.7 B (4 B and a control byte per bucket at load 7/8) plus the 8 B hash column it kept for
  keys and names: 2 to 18 B more per entry, about 40 MB for a 4-million-symbol image, traded for
  no rehash allocation, no copy on growth and a one-line probe. A load of 3/4 would halve it at
  8.5 slots per miss; not taken, since every insert is a miss first.

## A stage's scratch

The stage already used a per-image bump arena reset per stage, which is the frame pattern; the
move is to blaze-mem's `Frame` with its own 64 GiB reservation (`arena::STAGE_RESERVE`, for
images up to 16 GiB: a stage holds at most the bytes of the pieces it rewrites, their fixup
lists and per-piece columns). `bumpalo::Bump::reset` keeps its last chunk only, so a stage
needing more than one chunk allocated again on every relink; a frame keeps every committed page.
`frame::task` is not used: its 4 GiB reservation would cap a full link of a large image, and the
image-held frame keeps the stage's pages warm across stages exactly as the bump did. Speed: a
frame allocation is an align, an add, a compare and two stores (about 2 ns), the bump's cost.
The stage's four sets move into the same frame (L4), so a warm relink makes none of their
growth calls.

## Why not a `Slab` for live-patch sites

Generational handles are for objects freed and reused. The linker frees none by ID: a jump-table
slot is given out once and stays the function's for the image's life (`Slot` is "stable for as
long as the image lives"), a piece id is never reused (a removed piece's columns stay readable
for rollback and `State::any`), and history is a ring of 16 states. A `Slab` would add a
generation check to every slot and piece read and could only ever hold generation 0. The slots
are an append-only column of the state (`slot_syms`, `cells`, `slot_next`), copy-on-write with
the rest of it.

## The image state's copy-on-write chunks

Was (L5): `CowVec<T>`, `CowList<T>` and `CowBytes` kept a chunk table `Arc<Vec<Arc<..>>>` with
each chunk or 16 KiB page behind its own `Arc`, so the first write after a stage's O(1) clone
made about four global allocations per container touched (about 25 containers) and one per page
written. Is (`crates/blaze-link/src/cow.rs`, by `mem-backend-cow`): every chunk, page and table
node is a `blaze_mem::Shared<'static, [E; N]>` slot of a static `Pool<Counted<[E; N]>>`
(`Tag::Link`), made through the writing thread's `Cache` of it. Once a thread's caches are warm,
a relink's copies take slots the history's dropped states gave back: no global allocation.

- **Pools without generic statics.** A trait `Pools<A>` names, for a family type, the pool of
  slot type `A` and the thread's cache of it (`pool!` declares a `static` `OnceLock<Pool>` and a
  `thread_local!` `RefCell<Option<Cache>>` inside the impl's one function, so each impl has its
  own). `Chunked` is `Pools` of each chunk class and of the three node types; `chunked!`
  implements it for every element type the state uses: `Sym`, `u8`, `u32` (also `PieceId`),
  `u64`, `[u32; 2]`, `ListRef`, `Fixup`, `Bind`, `Def`, `Hot`, `What`, `Sort`, `FunctionInfo`,
  `Line` in `cow.rs`, and the private `Lists`, `Loader`, `SymCold`, `IdCold` in `state.rs`.
  `CowBytes` is the family `Bytes` (pages and its nodes). A pool is reserved on its first use,
  so a type used only as a column never reserves the longer-list classes.
- **Not `Copy`.** All are `Copy` but `IdCold` (it holds `Option<Spans>`). Chunks are filled by
  cloning, so `Chunked` needs only `Clone`: a chunk's slots past its elements hold clones of
  the value it was made with (a column's first push into it, a list's first element), dropped
  with the chunk's last owner (test `chunks_of_a_type_that_is_not_copy_drop_what_they_hold`).
- **Length beside the chunk.** A column's chunks are full but the last, which `len` bounds. A
  list arena keeps `fill` (elements in its last chunk) and `open` (whether more lists go in it);
  each list's `ListRef` holds its own start and length.
- **Longer lists.** A list never spans chunks (it reads as one slice). A list of at most
  `CHUNK` (1 024) elements goes in the last shared chunk or a new one; a longer one gets a chunk
  of its own of the first class holding it, classes 4×, 16×, ... 16 384× `CHUNK` (4 Ki to 16 Mi
  elements), and the next list starts a new chunk. Bound: a list of at most \(2^{24}\) elements
  (`LONGEST_LIST`, 512 MiB of 32-byte `Fixup`s, past any one piece's relocations); a push past it
  stops naming it. A longer list's chunk is at most 4× its length, so at most 3/4 of a class
  chunk is unused, and only for lists past 1 024 elements.
- **Tables.** A container's chunk table is a three-level tree of 64-entry nodes (`Leaf`, `Mid`,
  `Top`), each node a `Shared` slot of its own pool: \(64^3 = 2^{18}\) chunks or pages, so columns
  of \(2^{28}\) elements (past the \(2^{26}\) symbols an image holds) and regions of 4 GiB. A clone
  shares the root (one count); the first write after it copies the at most three nodes on its
  path and the chunk; a container not written costs nothing. Truncation copies only the nodes
  that change. The old flat table could not move into fixed slots (its length is the chunk
  count), and a two-level table of 512 entries would copy 8 KiB nodes per first write; three
  levels of 64 copy at most three nodes of about 2 KiB.
- **In place.** A chunk is up to 88 KiB (`FunctionInfo`) and a longer list's up to 512 MiB, so it
  must never pass through the stack, as `Cache::share(value)` and `Shared::make_mut` would make
  it. `cow.rs` stays `#![forbid(unsafe_code)]`; the in-place write is one constructor in
  blaze-mem, `Cache::share_from(head, fill)` (request M1 in `v2/requests/mem-backend-cow.md`,
  `pool.rs` being lane mem's). `cow.rs`'s `unique` copies with `share_from(&old[..], &old[0])`
  while `Shared::owners` is above one, then takes `Shared::get_mut`: `Arc::make_mut`'s rule in
  one pass over the slot.
- **Reservations**, address space only, committed as slots are carved: 32 GiB per pool of column
  chunks or pages (`CHUNKS`: the largest image's \(2^{26}\) symbols in about seven 4-byte `Sym`
  columns is 1.8 GiB per state, 28 GiB if each of 16 history states rewrote every chunk),
  8 GiB per class of longer lists (`LONG`: 16 lists of the longest class), 2 GiB per node pool
  (`NODES`: a million nodes; a full column needs \(2^{12}\) leaves). With about 20 families, under
  1 TiB of the 128 TiB address space. Past a reservation the pool stops with the `MemError`
  naming `Tag::Link` (no-fallback). Lane mem is asked (M3) to commit large slots fewer than 64 at
  a time, since a class pool's first carve commits 64 of them.
- **Kept behaviors.** O(1) clone with no allocation, the same `get`, `slice` and `read`
  borrowing, `changed_since` by `Shared::ptr_eq` on pages (a leaf found once per 64 pages),
  `same`, truncation (zeros on regrowth) and growth, `Debug`.

### Speed of light

- Clone: one relaxed atomic add per container (and one more for `CowBytes`' buffer), the bound.
- Read: three dependent loads to the chunk entry, then the element, against two for the flat
  table. The top and mid nodes of a hot container stay in L1 (one node per 4 096 chunks), so a
  warm read costs the leaf and the element, the same two misses the flat table had.
- First write after a clone: copying the chunk (4 KiB of `u32`s to 88 KiB of `FunctionInfo`s, at
  about 20 GB/s 0.2 to 4.4 µs) and up to three nodes (about 6 KiB, 0.3 µs), each node copy also
  one relaxed add per present entry (at most 192, uncontended, about 1 ns each). The bound is the
  bytes copied; the adds are within 10% of it for every chunk type but the smallest, where a
  column holding fewer than 64 chunks has fewer entries to count. Slots come from the cache: an
  array pop, and one compare-and-swap per 64.
- Warm steady state: zero global allocations (test `clones_and_reads_allocate_nothing`, every
  first write after a clone under `assert_at_most(0, ..)`), and a history of 16 states carves no
  new slots once warm (`a_history_of_writes_reuses_its_slots`).

## What is left

The readers' own arenas (`macho/tbd.rs`, `macho/archive.rs`, `wasm/module.rs`, `elf/read*`) are
per input file read once per full link and are held by their `requests-link-fuzz-*` and
`speed-link-edge-sol-link` agents; not moved here.

## Tests

In `intern.rs`: `shards_intern_as_a_map_and_a_vector_would` (model, 40 000 entries per shard,
past every commit step and six index doublings), `names_read_back_as_their_checked_bytes`,
`table_growth_allocates_nothing` (0), `a_warm_relinks_table_growth_allocates_nothing` (an image
linked, relinked after a one-function change, then 150 000 one-function relinks' table work: 0),
`a_new_table_reserves_and_commits_nothing` (3 allocations). In `arena.rs`:
`a_warm_stage_allocates_nothing` (0). Asked in L4 for `cow.rs`: `sets_in_a_frame_allocate_nothing`
(0). In `cow.rs` for L5: `clones_and_reads_allocate_nothing` (a clone 0; once warm, a first write
after a clone 0 for a column chunk, a list push, a shared page and an unwritten page), models
against plain vectors across clones in a 16-state history for each container
(`columns_answer_as_vectors_do_across_clones`, `list_arenas_answer_as_vectors_do_across_clones`
with longer lists of several classes, `region_bytes_answer_as_vectors_do_across_clones` with
`changed_since` covering every changed byte), `a_history_of_writes_reuses_its_slots`,
`chunks_of_a_type_that_is_not_copy_drop_what_they_hold` and `a_list_past_the_longest_class_stops`.
