
## dedup: blaze-link's code-signature SHA-256 onto blaze-sha256

From `dedup-codec` (report `/tmp/blaze-lanes/v2-dedup-codec.md`). The four SHA-256 copies
(`blaze-cargo/build.rs`, `blaze-engine/build.rs`, `blaze-link/src/macho/sha256.rs`,
`blaze-splice/src/sha256.rs`) are now one crate, `crates/blaze-sha256` (no dependencies;
`digest`, `fast`, and a streaming `Sha256` with `new`/`update`/`finish` that never allocates;
SHA-NI on x86-64, ARMv8 SHA-256 on aarch64, portable otherwise; every copy's tests moved there plus
a proptest against the build scripts' copy). Its workspace dependency line is requested in
`v2/requests/coordinator.md` ("## dedup: workspace dependency `blaze-sha256`"). `blaze-link`'s copy was the source of its SIMD paths, moved as they were.

1. `crates/blaze-link/Cargo.toml` `[dependencies]`: add `blaze-sha256.workspace = true`.
2. `crates/blaze-link/src/macho.rs` line 27, before: `mod sha256;` after:
   `use blaze_sha256 as sha256;` (with the other `use`s if you prefer). `sign.rs`'s
   `use super::sha256;` and `tests.rs`'s `use super::{.., sha256, ..}` keep working.
3. Delete `crates/blaze-link/src/macho/sha256.rs` (its tests: known digests, every path against
   portable, random states, all in `blaze-sha256/src/tests.rs`).

> taken by requests (link and requests-link-fuzz are DONE): child `requests-link-next`; the outcome follows.
> applied by requests (child requests-link-next): `blaze-sha256.workspace = true` in blaze-link's `[dependencies]`; `macho.rs` `mod sha256;` -> `use blaze_sha256 as sha256;` (with the other `use`s); `macho/sha256.rs` deleted. `sign.rs` and `macho/tests.rs` call `sha256::digest` unchanged.

# mem-backend-link: blaze-link's tables and stage scratch onto blaze-mem

From `mem-backend-link` (report `/tmp/blaze-lanes/v2-mem-backend-link.md`, design
`docs/v2/plan/lanes/mem/backend-link.md`). Every file below is held by an agent that is not DONE
(`requests-link-fuzz-core-model` for `symbols.rs`, `state.rs`, `native.rs`, `cow.rs`;
`requests-link-fuzz-core-stage` for `lib.rs`, `stage.rs`, `place.rs`, `write.rs`, `file.rs`,
`live.rs`), so each change is written here exactly; I apply any the holder leaves once it is
DONE. Two new files are mine and already written: `crates/blaze-link/src/intern.rs` (the
symbol table's shards on reserved regions, with their tests) and `crates/blaze-link/src/arena.rs`
(the stage's frame and `FrameVec`/`collect_in`). Neither compiles until `lib.rs` declares it
(L1). Apply in order: L1, L2, then L3, then L4.

## L1 (core-stage, `lib.rs`): declare the modules, turn on `allocator_api`

1. After the `//!` doc comment, before `mod commit;`, add the crate attribute:
   ```rust
   #![feature(allocator_api)]
   ```
   (`blaze-mem` uses it already; `Vec<T, &Frame>` needs it.)
2. In the `mod` list add, in alphabetical place: `mod arena;` (before `mod commit;`) and
   `mod intern;` (after `mod file;`).

> taken by requests (link and requests-link-fuzz are DONE): child `requests-link-next`; the outcome follows.
> applied by requests (child requests-link-next): found in the tree already (`#![feature(allocator_api)]`, `mod arena;`, `mod intern;` in `lib.rs`); checked, nothing to change.

## L2 (core-model, `symbols.rs`; core-stage, `lib.rs`): the symbol table on reserved regions

Why: each of `Symbols`' 192 shards grows its `Vec`s (`keys`, `hashes`, `ids`, `ends`) and its
`String` by doubling (realloc and copy) and its `hashbrown::HashTable` by rehashing into a new
allocation, for the image's life: a warm relink that interns a new function's keys can pay a
realloc of a whole shard. `intern.rs` keeps each shard in `RegionVec`s tagged `Tag::Link`
(address space reserved once per image, pages committed as entries arrive, never moved) with an
open-addressing index over a region of its own that doubles in place. Same ids, same order, same
bytes: the shard's index is still its entry's position, and each shard still interns in the
batch's order. Bounds (per shard, address space only): 2^20 entries per table, 64 MiB of name
bytes; 10.5 GiB reserved per image (`intern::RESERVED`), none committed until used.

`symbols.rs`, exact edits:

1. Module doc, lines 4 to 6, before:
   ```rust
   //! Each table is 64 shards of a SwissTable (`hashbrown::HashTable`: 16 control bytes probed
   //! at once with SSE2 or NEON) holding `u32` indexes, with each entry's hash kept beside it so
   //! growing never hashes again. A key is hashed once; bits 32..38 of that hash pick its shard
   ```
   after:
   ```rust
   //! Each table is 64 shards ([`crate::intern`]), each its entries in reserved regions and an
   //! open-addressing index of `u32` indexes beside the high half of their hashes, doubled in
   //! place and never moved. A key is hashed once; bits 32..38 of that hash pick its shard
   ```
2. Imports: delete
   ```rust
   use hashbrown::HashTable;
   use hashbrown::hash_table::Entry;
   ```
   and add, with the other `use`s:
   ```rust
   use blaze_mem::MemError;
   ```
   ```rust
   use crate::intern::{IdShard, KeyShard, NameShard, Shard, shards};
   ```
3. Make the three hash functions crate-visible (the index rehashes with them when it doubles):
   `fn hash_key(key: SymKey) -> u64` -> `pub(crate) fn hash_key(key: SymKey) -> u64`;
   `fn hash_name(name: &str) -> u64` -> `pub(crate) fn hash_name(name: &str) -> u64`;
   `fn hash_id(id: &ContentId) -> u64` -> `pub(crate) fn hash_id(id: &ContentId) -> u64`.
4. Delete everything from `/// One shard of one table: finds a key or adds it, returning its index in the shard.`
   (`trait Shard<K>`) through the end of `impl<'k> Shard<&'k ContentId> for IdShard { .. }`:
   the trait, `KeyShard`, `NameShard`, `name_at`, `IdShard` and their impls (now in `intern.rs`,
   same methods: `find`, `get`, `get_or_insert`, plus `key(at)`, `id(at)`, `len()`).
5. Delete `shards`:
   ```rust
   /// `SHARDS` empty shards: once per image.
   fn shards<S: Default>() -> Vec<S> {
       let mut out = Vec::with_capacity(SHARDS);
       out.resize_with(SHARDS, S::default);
       out
   }
   ```
6. `impl Default for Symbols`, before:
   ```rust
   impl Default for Symbols {
       fn default() -> Self {
           Symbols { keys: shards(), names: shards(), ids: shards() }
       }
   }
   ```
   after:
   ```rust
   impl Symbols {
       /// Reserves the tables' regions, committing nothing until a key goes in: once per image,
       /// three allocations (the lists of shards).
       pub fn new() -> Result<Symbols, MemError> {
           Ok(Symbols { keys: shards(KeyShard::new)?, names: shards(NameShard::new)?, ids: shards(IdShard::new)? })
       }
   }

   impl Default for Symbols {
       /// [`Symbols::new`], stopping with the error when the address space can't be reserved.
       fn default() -> Self {
           Symbols::new().unwrap_or_else(|error| panic!("{error}"))
       }
   }
   ```
7. `content`, before: `&self.ids[shard].ids[at]` after: `self.ids[shard].id(at)`.
8. `key`, before: `self.keys[shard].keys[at]` after: `self.keys[shard].key(at)`.
9. `dense_len`, before: `self.keys.iter().map(|it| it.keys.len()).max().unwrap_or(0) * SHARDS`
   after: `self.keys.iter().map(|it| it.len()).max().unwrap_or(0) * SHARDS`.

Nothing else in `symbols.rs` changes: `batch` takes `S: Shard<K>` and runs
`shards.par_iter_mut()` as before (`RegionVec<T: Send>` is `Send`); `name`, `find_name`, `str`,
`id`, `find_id`, `intern`, `find` call the same shard methods. The existing tests stay and must
pass unchanged (`hits_and_lookups_allocate_nothing` included: a hit still allocates nothing,
`intern_all` still only its result).

`lib.rs` (core-stage), `Image::new`, before:
```rust
            symbols: Symbols::default(),
```
after:
```rust
            // alloc: error path
            symbols: Symbols::new().map_err(|error| fail(error.to_string()))?,
```

Tests that hold it (in `intern.rs`): `shards_intern_as_a_map_and_a_vector_would` (model: the same
indexes and bytes as a `HashMap` and a `Vec` across 40 000 entries per shard, past every
column's commit step and six index doublings), `names_read_back_as_their_checked_bytes` (the one
`unsafe` view against `str::from_utf8`), `table_growth_allocates_nothing` (0 allocations for
40 000 new keys, names and ids per shard, and for their lookups),
`a_warm_relinks_table_growth_allocates_nothing` (an image of 4098 functions linked, relinked after
a one-function change, then 150 000 one-function relinks' keys, ids and names interned: 0
allocations; every shard passes its first commit step), `a_new_table_reserves_and_commits_nothing`
(3 allocations to make one, 10.5 GiB reserved).

> taken by requests (link and requests-link-fuzz are DONE): child `requests-link-next`; the outcome follows.
> applied by requests (child requests-link-next): found in the tree already (`symbols.rs` on `crate::intern`'s shards, the hash functions `pub(crate)`, `Symbols::new() -> Result<_, MemError>`, `key`/`id`/`len` accessors, `Image::new` calls `Symbols::new()` with the error path); checked against every edit, nothing to change.

## L3 (core-stage: `lib.rs`, `stage.rs`, `place.rs`, `write.rs`, `file.rs`, `live.rs`; core-model: `state.rs`, `native.rs`): the stage's scratch on a `Frame`

Why: the stage's scratch is a `bumpalo::Bump` the image keeps and resets per stage. `reset`
keeps only the last chunk, so a stage that needed more than one chunk calls the global allocator
again for the rest on every warm relink. A `blaze_mem::Frame` with its own reservation
(`arena::STAGE_RESERVE`, 64 GiB of address space) keeps every page it committed and never calls
the global allocator. The shape of the code is unchanged: the image owns the frame, `stage`
resets it and lends `&Frame` as it lent `&Bump`. Output bytes unchanged: only where temporaries
live moves.

Mechanical, in every file listed (apply each that the file has):

| before | after |
|---|---|
| `use bumpalo::Bump;` | `use blaze_mem::Frame;` |
| `use bumpalo::collections::Vec as BumpVec;` | `use crate::arena::{FrameVec, collect_in};` (import only what the file uses) |
| `&'a Bump` / `&'b bumpalo::Bump` / `&'a bumpalo::Bump` | `&'a Frame` / `&'b blaze_mem::Frame` / `&'a blaze_mem::Frame` |
| `BumpVec<'a, T>` | `FrameVec<'a, T>` |
| `BumpVec::new_in(x)` | `Vec::new_in(x)` |
| `BumpVec::with_capacity_in(n, x)` | `Vec::with_capacity_in(n, x)` |
| `BumpVec::from_iter_in(items, x)` | `collect_in(items, x)` |
| `v.into_bump_slice()` | `v.leak()` (`Vec<T, &'a Frame>::leak` gives `&'a mut [T]`; the frame frees it at the next reset, as the bump did) |
| `x.alloc_slice_fill_copy(n, value)` | `x.alloc_slice_fill_with(n, \|_\| value)` |
| `x.alloc_slice_copy(s)` | unchanged (`Frame::alloc_slice_copy`) |

Sites as of now: `lib.rs` 49, 50, 143, 185, 224 to 232; `stage.rs` 20, 21, 70, 75, 76, 133,
145 to 159, 231, 235, 318, 355, 377, 426, 480, 528, 546 to 548, 575, 576, 586, 694, 852, 920,
933, 968 to 971, 1005, 1043 to 1045, 1118, 1119, 1378; `place.rs` 18, 297, 299, 345, 372, 386,
387, 419, 511, 519, 532, 589, 600, 601, 625, 630; `write.rs` 23, 643, 655, 686, 701, 702, 750,
752, 753, 851, 852, 857, 858, 893, 912, 1184, 1228, 1328, 1329, 1337 to 1340; `file.rs` 16, 251,
253, 258, 280, 286, 378; `live.rs` 12, 88 to 96, 104; `state.rs` 299, 311; `native.rs` 1726,
1737.

Three lines are not the table's:
- `lib.rs` field: `arena: Bump,` -> `arena: Frame,` (doc comment unchanged).
- `lib.rs` `Image::new`, before: `arena: Bump::new(),` after:
  ```rust
              // alloc: error path
              arena: arena::stage_frame().map_err(|error| fail(error.to_string()))?,
  ```
- `stage.rs` test (line 1378), before: `let arena = Bump::new();` after:
  `let arena = crate::arena::stage_frame().unwrap();`

`self.arena.reset();` and `let arena = &self.arena;` in `Image::stage` stay as they are
(`Frame::reset(&mut self)`). `Frame` is `Send` and not `Sync`, as `Bump` is, so no parallel
closure could capture it before and none can now. `bumpalo` stays a dependency: the readers
(`macho/tbd.rs`, `macho/archive.rs`, `wasm/module.rs`, `elf/read*`) still use it.

Tests: `arena.rs`'s `a_warm_stage_allocates_nothing` (0 allocations for a stage's vectors,
collected vectors, filled slices and leaked slices on a warm frame; the same room after a reset).
`tests.rs`'s `an_incremental_stage_allocates_a_constant_whatever_the_image_holds` and
`relocation_allocates_nothing_per_fixup` hold the whole stage; with L3 and L4 their counts drop
(lower `STAGE_BUDGET` to what they measure once the coordinator runs them).

> taken by requests (link and requests-link-fuzz are DONE): child `requests-link-next`; the outcome follows.
> applied by requests (child requests-link-next): found in the tree but one site: `native.rs` ~2966 still had `let arena: &'a bumpalo::Bump = self.arena;`, now `&'a blaze_mem::Frame` (`Frame::alloc_str`). No `Bump`, `BumpVec`, `into_bump_slice`, `alloc_slice_fill_copy` or `from_iter_in` is left in the crate, and no reader uses `bumpalo` any more, so blaze-link's `bumpalo` line is gone (`link.md` align dedup item 4).

## L4 (core-model: `cow.rs`; core-stage: `stage.rs`, `lib.rs`): a stage's sets in its frame

Why: `Stage::fresh`, `write`, `dirty_cells` (`IdSet`) and `changed` (`SymSet`) are made empty
for every stage and grow their buffers by doubling from nothing: a warm relink allocates them
again each time (`SymSet` alone is 64 `IdSet`s of three vectors). They live exactly as long as
the stage, so they belong in its frame. After L3.

`cow.rs`:
1. Add `use std::alloc::{Allocator, Global};` to the imports.
2. `IdSet`, before:
   ```rust
   #[derive(Default, Debug)]
   pub(crate) struct IdSet {
       bits: Vec<u64>,
       listed: Vec<u64>,
       list: Vec<u32>,
       len: usize,
   }

   impl IdSet {
   ```
   after:
   ```rust
   #[derive(Debug)]
   pub(crate) struct IdSet<A: Allocator + Copy = Global> {
       bits: Vec<u64, A>,
       listed: Vec<u64, A>,
       list: Vec<u32, A>,
       len: usize,
   }

   impl Default for IdSet {
       fn default() -> Self {
           IdSet::new_in(Global)
       }
   }

   impl<A: Allocator + Copy> IdSet<A> {
       /// An empty set whose buffers are in `alloc` (a stage's frame).
       pub fn new_in(alloc: A) -> Self {
           IdSet { bits: Vec::new_in(alloc), listed: Vec::new_in(alloc), list: Vec::new_in(alloc), len: 0 }
       }

   ```
   (every method body unchanged; `sorted` still returns a `Vec<u32>` on the heap and
   `sorted_into` still takes `&mut Vec<u32>`.)
3. `SymSet`, before:
   ```rust
   #[derive(Debug)]
   pub(crate) struct SymSet {
       shards: Vec<IdSet>,
       len: usize,
   }

   impl Default for SymSet {
       fn default() -> Self {
           let mut shards = Vec::with_capacity(Symbols::SHARDS);
           shards.resize_with(Symbols::SHARDS, IdSet::default);
           SymSet { shards, len: 0 }
       }
   }

   impl SymSet {
   ```
   after:
   ```rust
   #[derive(Debug)]
   pub(crate) struct SymSet<A: Allocator + Copy = Global> {
       shards: Vec<IdSet<A>, A>,
       len: usize,
   }

   impl Default for SymSet {
       fn default() -> Self {
           SymSet::new_in(Global)
       }
   }

   impl<A: Allocator + Copy> SymSet<A> {
       /// An empty set of a shard per symbol shard, its buffers in `alloc`.
       pub fn new_in(alloc: A) -> Self {
           let mut shards = Vec::with_capacity_in(Symbols::SHARDS, alloc);
           shards.extend((0..Symbols::SHARDS).map(|_| IdSet::new_in(alloc)));
           SymSet { shards, len: 0 }
       }

   ```
4. Add to `cow.rs`'s tests:
   ```rust
   /// A set in a warm frame makes no global allocation however many members it takes.
   #[test]
   fn sets_in_a_frame_allocate_nothing() {
       let mut frame = crate::arena::stage_frame().unwrap();
       let fill = |frame: &blaze_mem::Frame| {
           let mut ids = IdSet::new_in(frame);
           let mut syms = SymSet::new_in(frame);
           for n in 0..50_000u32 {
               ids.insert(n * 3);
               syms.insert(Symbols::join(n as usize % Symbols::SHARDS, n as usize));
           }
           (ids.len(), ids.iter().nth(7), syms.len())
       };
       assert_eq!(fill(&frame), (50_000, Some(21), 50_000));
       frame.reset();
       let again = blaze_alloc_count::assert_at_most(0, "a stage's sets in a warm frame", || fill(&frame));
       assert_eq!(again, (50_000, Some(21), 50_000));
       let mut heap = IdSet::default();
       for n in [5, 900, 3] {
           heap.insert(n);
       }
       assert_eq!(heap.sorted(), vec![3, 5, 900], "the heap's sets as before");
   }
   ```

`stage.rs`, `Stage` fields, before:
```rust
    pub fresh: IdSet,
```
```rust
    pub write: IdSet,
```
```rust
    pub changed: SymSet,
```
```rust
    pub dirty_cells: IdSet,
```
after: `pub fresh: IdSet<&'a Frame>,`, `pub write: IdSet<&'a Frame>,`,
`pub changed: SymSet<&'a Frame>,`, `pub dirty_cells: IdSet<&'a Frame>,`.

`lib.rs`, `Image::stage`, before:
```rust
            fresh: Default::default(),
            write: Default::default(),
            changed: Default::default(),
            dirty_cells: Default::default(),
```
after:
```rust
            fresh: cow::IdSet::new_in(arena),
            write: cow::IdSet::new_in(arena),
            changed: cow::SymSet::new_in(arena),
            dirty_cells: cow::IdSet::new_in(arena),
```
No other code names these fields' types: `place.rs` and `write.rs` make their own
`IdSet::default()` (heap, unchanged) and `write.rs`'s `splice(&self, drop: &IdSet, ..)` takes one
of those.

> taken by requests (link and requests-link-fuzz are DONE): child `requests-link-next`; the outcome follows.
> applied by requests (child requests-link-next): found in the tree already (`IdSet<A>`/`SymSet<A>` with `new_in`, the stage's four sets in its frame, `sets_in_a_frame_allocate_nothing`); checked, nothing to change. `link.md` "dedup: bits" item 2 is held back by it (see there).

## L5 (lane mem, `crates/blaze-mem`): what the image state's copy-on-write chunks need

Not a blaze-link edit; for `mem-backend` to take to lane mem. After L2 to L4 the warm relink's
remaining global allocations are the image state's copy-on-write (`cow.rs`: `CowVec`, `CowList`,
`CowBytes`): the first write after a stage's O(1) clone copies a container's chunk table and the
chunk written, each a new `Arc` (four allocations per container touched; about 25 containers),
and `CowBytes` a 16 KiB page per page written. These are not tables growing by doubling (a
`RegionVec` can't replace them: the state must clone in O(1) and keep 16 commits of history), but
chunks that come and go, shared by reference count. blaze-mem has no shared (reference-counted)
pooled object: `Pooled<'p, T>` has one owner. The type that would take them to zero: a
`blaze_mem::Shared<T>` pool, fixed-size chunks in a reserved region handed out by `u32` index with
an atomic count per slot, `clone` a count increment, the last drop returning the slot to the
pool's free list, `make_mut` copying into a fresh slot only when shared (Arc's `make_mut`
semantics). With it, `CowVec`/`CowList` chunks and `CowBytes` pages move onto pools per image and
a warm relink's copies come from slots the history's dropped states gave back.

Also for lane mem: each `RegionVec` is one reservation, and on Linux a reservation that is partly
committed is two mappings. `intern.rs` makes 448 regions per image (896 mappings); with
`vm.max_map_count` at its default of 65 530, a daemon holding about 70 images would exhaust it on
the symbol tables alone. Packing several vectors into one reservation does not help (each
partly committed vector is still its own mapping). What does: blaze raises `vm.max_map_count`
at install (current Fedora and Arch ship 1 048 576), and the daemon reads it at start and stops
naming it when it is below what its image budget needs (no-fallback). Recorded in
`docs/v2/plan/lanes/mem/backend-link.md`.

> forwarded to mem (`crates/blaze-mem`, lane mem; told by tmux).

> done by mem (2026-10-29). `blaze_mem::Shared<'p, T>` (`crates/blaze-mem/src/pool.rs`) is a
> slot of a `Pool<pool::Counted<T>>` with an atomic count of its owners. `Cache::share(value)`
> (and `try_share`) makes one with one owner. `clone` is one relaxed add, and the last drop
> drops the value and returns the slot. `Shared::make_mut(&mut s, &mut cache)` follows `Arc`'s
> rule (copy into a fresh slot only while shared), with `get_mut`, `owners`, `ptr_eq` and
> `index` beside it. It is `Send + Sync` for `T: Send + Sync`. Tests: a model against `Arc`, drop
> once, copy only while shared, owners on 8 threads, and zero allocations once warm. For
> the image state, keep one pool per chunk type in a `static` (`OnceLock<Pool<Counted<Chunk>>>`,
> `Tag::Link`): chunks are then `Shared<'static, _>`, the state needs no lifetime, and every
> image's history gives slots back to the same pool. Each worker that writes keeps a `Cache` of
> it.
>
> On `vm.max_map_count`: agreed, and it stays a stop, not a fallback. Raising it at install and
> checking it at daemon start belongs to the daemon and install lanes, not blaze-mem.
> blaze-mem's part is that a failed reservation already reports `MemError` naming its tag and
> size.

## dedup: LEB128 and seeded RNGs onto `blaze_format::leb` and `blaze_ids::rng`

From `dedup-codec-leb` (report `/tmp/blaze-lanes/v2-dedup-codec-leb.md`). The one LEB128 is
`crates/blaze-format/src/leb.rs` (written, tested against the copies below as its reference):
`write_u64`/`write_i64` (into a `Vec`), `put_u64`/`put_i64` (into a slice, return the length),
`len_u64`/`len_i64`, `read_u64`/`read_i64`/`read_u32`/`read_i32`/`skip` (`(bytes, &mut at)`,
`Err(LebError::{Truncated, Overflow})`, `at` unchanged on error; `LebError::as_str` gives a
`&'static str`), `padded_u32`/`padded_i32` (`[u8; PADDED]`), `put_padded_u64`/`put_padded_i64`,
`PADDED` (5), `MAX_LEN` (10). The readers refuse a value past 64 bits (or past 32 bits / 5 bytes
for the `32` ones) exactly as `elf/dynamic.rs` and `wasm/reader.rs` do now. The RNG is
`blaze_ids::rng::SplitMix64` (`new`, `next_u64`, `below`, `index`, `one_in`, `shuffle`, `fill`,
...). `blaze-link` already depends on both crates. Every edit below is in `crates/blaze-link/src/`.
`macho/dylib.rs` went to `v2/requests/requests-link-fuzz.md` (it names that file).

> taken by requests (link and requests-link-fuzz are DONE): child `requests-link-next`; the outcome follows.
> applied by requests (child requests-link-next): as written, in every file listed (`wasm/leb.rs`, `wasm/reader.rs`, `stage.rs`, `unwind.rs`, `macho/linkedit.rs`, `elf/dynamic.rs`, `macho/unwind.rs`, `macho/input.rs`, `macho/tests.rs`, and the test RNGs of `place.rs`, `elf/read/strings.rs`, `macho/archive.rs`, `macho/sign.rs`, `macho/tbd.rs`); the moved tests deleted. Also `macho/tests.rs` `the_locals_in_parallel_are_the_locals_one_by_one`, a later xorshift copy, onto `SplitMix64`.

### `wasm/leb.rs`: the numbers become re-exports

Delete `PADDED`, `uleb`, `uleb_len`, `uleb_into`, `sleb`, `sleb_into`, `padded_uleb`,
`padded_sleb` (everything above `fn name`), and in their place:

```rust
pub(super) use blaze_format::leb::{
    PADDED, len_u64 as uleb_len, padded_i32 as padded_sleb, padded_u32 as padded_uleb,
    put_i64 as sleb_into, put_u64 as uleb_into, write_i64 as sleb, write_u64 as uleb,
};
```

Signatures match every caller (`module.rs`, `code.rs`, `reloc.rs`, `object.rs`, `wasm.rs`)
unchanged. In its tests, delete `unsigned`, `lengths`, `signed` and `padded` (moved to
`blaze-format/src/leb/tests.rs`); keep `round_trips`, `writes_into_room_allocate_nothing`,
`framing`.

> taken by requests (link and requests-link-fuzz are DONE): child `requests-link-next`; the outcome follows.
> applied by requests (child requests-link-next): as written, in every file listed (`wasm/leb.rs`, `wasm/reader.rs`, `stage.rs`, `unwind.rs`, `macho/linkedit.rs`, `elf/dynamic.rs`, `macho/unwind.rs`, `macho/input.rs`, `macho/tests.rs`, and the test RNGs of `place.rs`, `elf/read/strings.rs`, `macho/archive.rs`, `macho/sign.rs`, `macho/tbd.rs`); the moved tests deleted. Also `macho/tests.rs` `the_locals_in_parallel_are_the_locals_one_by_one`, a later xorshift copy, onto `SplitMix64`.

### `wasm/reader.rs`: `unsigned`/`signed` become the shared readers

Delete `fn unsigned` and `fn signed`; replace `u32`, `u64`, `i32`, `i64` with:

```rust
    fn leb<T>(&mut self, read: fn(&[u8], &mut usize) -> Result<T, LebError>) -> Result<T, String> {
        // alloc: error path
        read(self.bytes, &mut self.at).map_err(|error| error.to_string())
    }

    pub(super) fn u32(&mut self) -> Result<u32, String> {
        self.leb(leb::read_u32)
    }

    pub(super) fn u64(&mut self) -> Result<u64, String> {
        self.leb(leb::read_u64)
    }

    pub(super) fn i32(&mut self) -> Result<i32, String> {
        self.leb(leb::read_i32)
    }

    pub(super) fn i64(&mut self) -> Result<i64, String> {
        self.leb(leb::read_i64)
    }
```

and `use blaze_format::leb::{self, LebError};` after `use std::ops::Range;`. Its tests
(`numbers`, `too_long_or_too_large`, `reads_allocate_nothing`) hold unchanged.

> taken by requests (link and requests-link-fuzz are DONE): child `requests-link-next`; the outcome follows.
> applied by requests (child requests-link-next): as written, in every file listed (`wasm/leb.rs`, `wasm/reader.rs`, `stage.rs`, `unwind.rs`, `macho/linkedit.rs`, `elf/dynamic.rs`, `macho/unwind.rs`, `macho/input.rs`, `macho/tests.rs`, and the test RNGs of `place.rs`, `elf/read/strings.rs`, `macho/archive.rs`, `macho/sign.rs`, `macho/tbd.rs`); the moved tests deleted. Also `macho/tests.rs` `the_locals_in_parallel_are_the_locals_one_by_one`, a later xorshift copy, onto `SplitMix64`.

### `stage.rs`: `uleb` becomes a wrapper

```rust
/// Reads a LEB128 at `*at`, moving past it.
fn uleb(bytes: &[u8], at: &mut usize) -> Result<u64, String> {
    // alloc: error path
    blaze_format::leb::read_u64(bytes, at).map_err(|error| format!("wasm: {error} in a code header"))
}
```

> taken by requests (link and requests-link-fuzz are DONE): child `requests-link-next`; the outcome follows.
> applied by requests (child requests-link-next): as written, in every file listed (`wasm/leb.rs`, `wasm/reader.rs`, `stage.rs`, `unwind.rs`, `macho/linkedit.rs`, `elf/dynamic.rs`, `macho/unwind.rs`, `macho/input.rs`, `macho/tests.rs`, and the test RNGs of `place.rs`, `elf/read/strings.rs`, `macho/archive.rs`, `macho/sign.rs`, `macho/tbd.rs`); the moved tests deleted. Also `macho/tests.rs` `the_locals_in_parallel_are_the_locals_one_by_one`, a later xorshift copy, onto `SplitMix64`.

### `unwind.rs`: the writers become re-exports

Replace `pub(crate) fn uleb` and `pub(crate) fn sleb` with
`pub(crate) use blaze_format::leb::{write_i64 as sleb, write_u64 as uleb};` (`dwarf.rs` imports
them from here, unchanged). Delete the test `leb128_encodes_both_signs` (moved:
`both_signs_in_one_buffer`).

> taken by requests (link and requests-link-fuzz are DONE): child `requests-link-next`; the outcome follows.
> applied by requests (child requests-link-next): as written, in every file listed (`wasm/leb.rs`, `wasm/reader.rs`, `stage.rs`, `unwind.rs`, `macho/linkedit.rs`, `elf/dynamic.rs`, `macho/unwind.rs`, `macho/input.rs`, `macho/tests.rs`, and the test RNGs of `place.rs`, `elf/read/strings.rs`, `macho/archive.rs`, `macho/sign.rs`, `macho/tbd.rs`); the moved tests deleted. Also `macho/tests.rs` `the_locals_in_parallel_are_the_locals_one_by_one`, a later xorshift copy, onto `SplitMix64`.

### `macho/linkedit.rs`

Replace `pub(super) fn uleb`, `pub(super) fn sleb` and `fn uleb_len` with:

```rust
pub(super) use blaze_format::leb::{write_i64 as sleb, write_u64 as uleb};

fn uleb_len(value: u64) -> u64 {
    blaze_format::leb::len_u64(value) as u64
}
```

> taken by requests (link and requests-link-fuzz are DONE): child `requests-link-next`; the outcome follows.
> applied by requests (child requests-link-next): as written, in every file listed (`wasm/leb.rs`, `wasm/reader.rs`, `stage.rs`, `unwind.rs`, `macho/linkedit.rs`, `elf/dynamic.rs`, `macho/unwind.rs`, `macho/input.rs`, `macho/tests.rs`, and the test RNGs of `place.rs`, `elf/read/strings.rs`, `macho/archive.rs`, `macho/sign.rs`, `macho/tbd.rs`); the moved tests deleted. Also `macho/tests.rs` `the_locals_in_parallel_are_the_locals_one_by_one`, a later xorshift copy, onto `SplitMix64`.

### `elf/dynamic.rs`

`Cursor::uleb` and `Cursor::sleb` become:

```rust
    fn uleb(&mut self) -> Result<u64, &'static str> {
        blaze_format::leb::read_u64(self.bytes, &mut self.at).map_err(LebError::as_str)
    }

    fn sleb(&mut self) -> Result<i64, &'static str> {
        blaze_format::leb::read_i64(self.bytes, &mut self.at).map_err(LebError::as_str)
    }
```

with `use blaze_format::leb::LebError;` at the top. In `mod tests`: delete `fn uleb` and use
`blaze_format::leb::write_u64` at its two calls (`uleb(&mut body, 1)`, `uleb(&mut body, 16)`);
delete `struct Rng` and its `impl`, add `use blaze_ids::rng::SplitMix64;`, and replace
`&mut Rng` with `&mut SplitMix64`, `Rng(` with `SplitMix64::new(`, `rng.next()` with
`rng.next_u64()` (the draws change from xorshift64 to SplitMix64; the tests compare against
gimli, not fixed outputs).

> taken by requests (link and requests-link-fuzz are DONE): child `requests-link-next`; the outcome follows.
> applied by requests (child requests-link-next): as written, in every file listed (`wasm/leb.rs`, `wasm/reader.rs`, `stage.rs`, `unwind.rs`, `macho/linkedit.rs`, `elf/dynamic.rs`, `macho/unwind.rs`, `macho/input.rs`, `macho/tests.rs`, and the test RNGs of `place.rs`, `elf/read/strings.rs`, `macho/archive.rs`, `macho/sign.rs`, `macho/tbd.rs`); the moved tests deleted. Also `macho/tests.rs` `the_locals_in_parallel_are_the_locals_one_by_one`, a later xorshift copy, onto `SplitMix64`.

### `macho/unwind.rs`

`Reader::uleb`/`Reader::sleb`: the same two bodies as `elf/dynamic.rs` above, same import. In
`mod tests`: delete `struct Rng` and its `impl` (it is SplitMix64, same stream), add
`use blaze_ids::rng::SplitMix64 as Rng;`, and replace `Rng(seed)` with `Rng::new(seed)`
(`below` and `shuffle` are the same functions, so every case is the same).

> taken by requests (link and requests-link-fuzz are DONE): child `requests-link-next`; the outcome follows.
> applied by requests (child requests-link-next): as written, in every file listed (`wasm/leb.rs`, `wasm/reader.rs`, `stage.rs`, `unwind.rs`, `macho/linkedit.rs`, `elf/dynamic.rs`, `macho/unwind.rs`, `macho/input.rs`, `macho/tests.rs`, and the test RNGs of `place.rs`, `elf/read/strings.rs`, `macho/archive.rs`, `macho/sign.rs`, `macho/tbd.rs`); the moved tests deleted. Also `macho/tests.rs` `the_locals_in_parallel_are_the_locals_one_by_one`, a later xorshift copy, onto `SplitMix64`.

### `macho/input.rs`

`Cursor::uleb` (a skip) becomes:

```rust
    /// An unsigned or signed LEB128, skipped.
    fn uleb(&mut self) -> Result<(), &'static str> {
        blaze_format::leb::skip(self.bytes, &mut self.at).map_err(|_| "a truncated call frame entry")
    }
```

In `mod tests`: delete `struct Rng` and its `impl`, add `use blaze_ids::rng::SplitMix64;`,
`let mut rng = Rng(0x9e37_...)` becomes `let mut rng = SplitMix64::new(0x9e37_...)` (same
seed expression); every `rng.below(n)` stays.

> taken by requests (link and requests-link-fuzz are DONE): child `requests-link-next`; the outcome follows.
> applied by requests (child requests-link-next): as written, in every file listed (`wasm/leb.rs`, `wasm/reader.rs`, `stage.rs`, `unwind.rs`, `macho/linkedit.rs`, `elf/dynamic.rs`, `macho/unwind.rs`, `macho/input.rs`, `macho/tests.rs`, and the test RNGs of `place.rs`, `elf/read/strings.rs`, `macho/archive.rs`, `macho/sign.rs`, `macho/tbd.rs`); the moved tests deleted. Also `macho/tests.rs` `the_locals_in_parallel_are_the_locals_one_by_one`, a later xorshift copy, onto `SplitMix64`.

### `macho/tests.rs`

Delete `fn uleb` and `fn sleb` (the readers) and add:

```rust
fn uleb(bytes: &[u8], at: &mut usize) -> u64 {
    blaze_format::leb::read_u64(bytes, at).expect("a ULEB128")
}

fn sleb(bytes: &[u8], at: &mut usize) -> i64 {
    blaze_format::leb::read_i64(bytes, at).expect("an SLEB128")
}
```

Delete the test `uleb_and_sleb` (moved: `round_trips`). In `trie_is_the_insertion_trie`, the
`uleb_len` closure becomes `let uleb_len = |value: u64| blaze_format::leb::len_u64(value) as u64;`
and the xorshift closure becomes
`let mut rng = blaze_ids::rng::SplitMix64::new(0x9e37_79b9_7f4a_7c15); let mut next = move || rng.next_u64();`.

> taken by requests (link and requests-link-fuzz are DONE): child `requests-link-next`; the outcome follows.
> applied by requests (child requests-link-next): as written, in every file listed (`wasm/leb.rs`, `wasm/reader.rs`, `stage.rs`, `unwind.rs`, `macho/linkedit.rs`, `elf/dynamic.rs`, `macho/unwind.rs`, `macho/input.rs`, `macho/tests.rs`, and the test RNGs of `place.rs`, `elf/read/strings.rs`, `macho/archive.rs`, `macho/sign.rs`, `macho/tbd.rs`); the moved tests deleted. Also `macho/tests.rs` `the_locals_in_parallel_are_the_locals_one_by_one`, a later xorshift copy, onto `SplitMix64`.

### RNG copies in tests (`place.rs`, `elf/read/strings.rs`, `macho/archive.rs`, `macho/sign.rs`, `macho/tbd.rs`)

- `place.rs` `radix_sort_agrees_with_a_comparison_sort` and `elf/read/strings.rs` (the
  `let mut state = 0x9e37_...; let mut next = || { xorshift }` pair): replace with
  `let mut rng = blaze_ids::rng::SplitMix64::new(0x9e37_79b9_7f4a_7c15); let mut next = || rng.next_u64();`.
- `macho/archive.rs` `seeded_indexes_name_the_members_they_were_written_for`: replace `seed`
  and the `below` closure with
  `let mut rng = blaze_ids::rng::SplitMix64::new(0x2545_f491_4f6c_dd1d); let mut below = |n: u64| rng.below(n);`.
- `macho/sign.rs` `noise`:

  ```rust
  fn noise(seed: u64, len: usize) -> Vec<u8> {
      let mut out = vec![0; len];
      blaze_ids::rng::SplitMix64::new(seed).fill(&mut out);
      out
  }
  ```

- `macho/tbd.rs` `mod tests`: `Rng` is SplitMix64 (same stream), so keep it as a newtype for
  `pick`/`subset`: `struct Rng(blaze_ids::rng::SplitMix64);`, delete `next`'s body for
  `self.0.next_u64()`, `below` becomes `self.0.index(n)`, `one_in` becomes
  `self.0.one_in(n as u64)`; `pick`/`subset` stay; each `Rng(x)` becomes
  `Rng(blaze_ids::rng::SplitMix64::new(x))`. Outputs are bit for bit the same.

> taken by requests (link and requests-link-fuzz are DONE): child `requests-link-next`; the outcome follows.
> applied by requests (child requests-link-next): as written, in every file listed (`wasm/leb.rs`, `wasm/reader.rs`, `stage.rs`, `unwind.rs`, `macho/linkedit.rs`, `elf/dynamic.rs`, `macho/unwind.rs`, `macho/input.rs`, `macho/tests.rs`, and the test RNGs of `place.rs`, `elf/read/strings.rs`, `macho/archive.rs`, `macho/sign.rs`, `macho/tbd.rs`); the moved tests deleted. Also `macho/tests.rs` `the_locals_in_parallel_are_the_locals_one_by_one`, a later xorshift copy, onto `SplitMix64`.
