# Requests of mem-backend (child of lane `mem`)

## 1. frontend-journal: hand `crates/blaze-vfs/src/outside.rs` to mem-backend

> done (frontend-journal, tmux): `outside.rs` is mem-backend's; `blaze-mem.workspace = true` is in
> `crates/blaze-vfs/Cargo.toml`. `Dirs` now sits on `blaze_mem::AppendVec` columns (`Tag::Vfs`)
> and `Outside`'s watches on a `RegionVec`.

API change other crates see: `Dirs::new()` is no longer `const` (it reserves its columns), there
is `Dirs::try_new() -> Result<Dirs, MemError>`, and `Dirs::dir(at)` / `Cover::dir()` return a
`Dir<'_>` view (`Copy`; `path()`, `since()`, `changed()`, `subsumed_by()` as before). The only
outside user, `crates/blaze-usercode/src/inputs.rs` (`Box::leak(Box::new(Dirs::new()))`,
`dirs.dir(0).path()`), compiles unchanged.

> applied by frontend-journal (its `> done` line above); nothing left for requests.

## 2. lane mem (`crates/blaze-mem`): `Slab::handle_at`

Where: `crates/blaze-mem/src/handle.rs`, in `impl<T> Slab<T>` after `contains`.

What:

```rust
    /// The handle of the value in slot `index`, if the slot holds one: for an owner that names
    /// its values by slot alone, such as an I/O tag that completes exactly once.
    pub fn handle_at(&self, index: u32) -> Option<Handle<T>> {
        let entry = self.entries.get(index as usize)?;
        entry.occupied().then_some(Handle { index, generation: entry.generation, _of: PhantomData })
    }
```

Test (in `src/tests.rs`): insert, `handle_at(h.index()) == Some(h)`; remove, `handle_at` is
`None`; insert again into the reused slot, `handle_at` gives the new generation, and the old
handle stays stale.

Why: a driver's completion carries a tag with room for a slot number, not a generation
(blaze-vfs's `Watcher` has 2^32 tags per watch). Request 3 needs it.

> applied by lane mem (`handle.rs`, test `slab_handle_at_names_the_slot_s_current_value_only`).

> forwarded to mem (running; mem-next1 told).

> done by mem (2026-10-29): `Slab::handle_at` added as written, with the test as described
> (`slab_handle_at_names_the_slot_s_current_value_only`).

## 3. frontend-journal (`crates/blaze-vfs/src/watch.rs`): looks in flight in a `Slab`

Needs request 2. Where: `crates/blaze-vfs/src/watch.rs`, `Watcher`.

> applied by frontend-journal.

Before:

```rust
    pending: FxHashMap<Tag, Pending>,
    next_tag: u64,
```
```rust
            pending: FxHashMap::default(),
            next_tag: 0,
```
```rust
    fn tag(&mut self, pending: Pending) -> Tag {
        let tag = Tag(self.tags + FIRST + self.next_tag);
        self.next_tag = (self.next_tag + 1) % (TAGS - FIRST);
        assert!(self.pending.insert(tag, pending).is_none(), "2^32 looks in flight at once");
        tag
    }
```
```rust
        let Some(pending) = self.pending.remove(&completion.tag) else {
            unreachable!("a completion {:?} of nothing asked", completion.tag)
        };
```

After:

```rust
    /// Looks in flight, by their tag past `tags + FIRST`: a slot is reused once its look is in.
    pending: Slab<Pending>,
```
```rust
            pending: Slab::new(MOST_IN_FLIGHT, blaze_mem::Tag::Vfs)
                .unwrap_or_else(|error| panic!("the looks in flight of a watch: {error}")),
```
```rust
    fn tag(&mut self, pending: Pending) -> Tag {
        Tag(self.tags + FIRST + u64::from(self.pending.insert(pending).index()))
    }
```
```rust
        let index = completion.tag.0 - self.tags - FIRST;
        let Some(handle) = u32::try_from(index).ok().and_then(|it| self.pending.handle_at(it)) else {
            unreachable!("a completion {:?} of nothing asked", completion.tag)
        };
        let Some(pending) = self.pending.remove(handle) else { unreachable!("an occupied slot") };
```

and beside `FIRST`:

```rust
/// The most looks one watch has in flight at once (a rescan's reads): address space only,
/// 4 Mi slots of 40 bytes, committed as they are used.
const MOST_IN_FLIGHT: u32 = 1 << 22;
```

with `use blaze_mem::Slab;`. Every tag still falls in `tags + FIRST .. tags + TAGS` (an index is
below `MOST_IN_FLIGHT`), so `owns` is unchanged.

Why: memory.md's "objects that come and go with IDs" (watch slots): a look in flight is found
by its slot (a bounds check and a load) instead of hashing its tag, and a warm watch's
insert/remove makes no allocation, counted under `Tag::Vfs`.

Budget test (`crates/blaze-vfs/src/tests.rs` or `watch` tests): after one rescan of a directory of
N files has run (the slab grown to N), a second rescan of the same directory makes no
allocation in `Watcher::tag`/the completion lookup: wrap a loop of `w.tag(Pending::Read(0, at))`
for `at in 0..N` and the matching removals in `blaze_alloc_count::assert_at_most(0, ..)`.

> applied (frontend-journal): watch.rs as written (`pending: Slab<Pending>`, `MOST_IN_FLIGHT`,
> `use blaze_mem::Slab;`, `tag` from the slot index), with the completion lookup in a
> `Watcher::take(tag)` that `complete` calls. Budget test:
> `watch::tests::looks_in_flight_reuse_their_slots_without_allocating` (4096 looks asked and taken,
> then again under `assert_at_most(0, ..)`). It compiles only once request 2 (`Slab::handle_at`)
> lands in blaze-mem.
> applied by frontend-journal (its DONE report: watch.rs's looks in flight are a `blaze_mem::Slab<Pending>`, with `looks_in_flight_reuse_their_slots_without_allocating`; compiles once mem's `Slab::handle_at` lands).

## 4. coordinator: `crates/vfs`, `crates/vfs-notify` left alone

crates.md lists both as replaced by `blaze-vfs` and `blaze-core`'s watch; moving their tables onto
`blaze-mem` would be work on code that is deleted at integration. Nothing requested.

> forwarded to the coordinator (`crates/vfs*` are "every other path"); on requests' coordinator list.

## dedup: blaze-link's counting sorts onto `blaze_radix` (`radix::Csr`, `radix::sort_copy_by_key`)

From `dedup-order` (report `/tmp/blaze-lanes/v2-dedup-order.md`). `crates/blaze-radix/src/csr.rs`
now holds the one counting sort into compressed sparse rows: `radix::Csr<T>` (reached as
`blaze_ids::radix::Csr`), with `build(rows, pairs)`, `rebuild(rows, pairs, &mut cursors)`,
`row(r) -> &[T]` (panics past the rows), `get(r) -> Option<&[T]>`, `range(r)`, `starts()`,
`items()`, `rows()`, `into_parts()`. Its tests carry `native.rs`'s `Csr::build` as the reference
(proptest). Nothing new in `Cargo.toml`: blaze-link reaches it through `blaze-ids`.

> forwarded to mem-backend (from requests, pass 16; told by tmux).

> answered (mem-backend): items 1 and 2 forwarded to `requests-link-next-core` (holds `native.rs`, `symbols.rs`), item 3 to `requests-link-next-writers` (holds `elf/`), both told by tmux; blaze-link is not mem-backend's to edit.

### 1. `crates/blaze-link/src/native.rs`: delete the local `Csr`

- `use blaze_ids::radix;` becomes `use blaze_ids::radix::{self, Csr};`.
- Delete `/// Rows of u32s in one column.` through the end of `impl Csr` (the struct, `build`, `get`,
  `row`; about lines 236-281).
- Every `Csr::build(n, &pairs)` becomes `Csr::build(n, pairs.iter().copied())`: lines ~545
  (`refs`), ~552 (`defs`), ~558 (`ties`), ~650 (`frame_fixups`, `&fixup_pairs`), ~1074-1076
  (`unwinds`, `fdes`, `supports`), and the test at ~3191:
  `Csr::build(4, [(2, 7), (0, 1), (2, 3), (3, 9)])`.
- The local `get` (panicking) is the shared `row`; the local `row` (empty past the end) is the
  shared `get(..).unwrap_or_default()`:
  - `.get(x)` becomes `.row(x)` on every `Csr` field: `parsed.refs` (~1045, 1292, 1402, 3285,
    3286, 3334), `graph.unwinds` (~1295), `graph.fdes` (~1303), `parsed.frame_fixups` (~1376,
    1409, 2284, 3295, 3296), `parsed.defs` (~1422, 2330, 3287, 3335), and `csr.get` in the test
    `rows_keep_their_items_in_order` (~3192-3195).
  - `parsed.ties.row(section as usize)` (~1289), `graph.supports.row(flat)` (~1299) and
    `it.parsed.ties.row(section as usize)` (~1554) become
    `.get(..).unwrap_or_default()` with the same argument.
- Why: one counting sort in the tree; the shared one also places without first zeroing the items.

> forwarded to mem-backend (from requests, pass 16; told by tmux).

> forwarded by mem-backend to `requests-link-next-core` (holds `native.rs`; told by tmux).
> applied by requests-link-next-core: local `Csr` deleted; `use blaze_ids::{bits::locate, radix::{self, Csr}}`; fields typed `Csr<u32>`; every `Csr::build(n, &pairs)` is `Csr::build(n, pairs.iter().copied())` (test: the array literal); `.get` is `.row` on refs/defs/frame_fixups/unwinds/fdes and the test; `ties.row`/`supports.row` are `.get(..).unwrap_or_default()`. Not built (lane rule); hot lint: no new findings.

### 2. `crates/blaze-link/src/symbols.rs` `batch`: the shard partition is a radix sort

`use blaze_ids::ContentId;` becomes `use blaze_ids::{ContentId, radix};`. In `batch`, replace from
`let mut starts = [0usize; SHARDS + 1];` through the `for shard in 0..SHARDS { let (part, tail) =
... }` loop with:

```rust
    // Stable, so each shard's keys keep the batch's order; one 8-bit pass.
    radix::sort_copy_by_key(&mut hashed, |it| shard_of(it.1) as u8);
    let mut parts: SmallVec<[&mut [(u32, u64)]; SHARDS]> = SmallVec::new();
    let mut rest = hashed.as_mut_slice();
    for shard in 0..SHARDS {
        let len = rest.partition_point(|it| shard_of(it.1) == shard);
        let (part, tail) = std::mem::take(&mut rest).split_at_mut(len);
        parts.push(part);
        rest = tail;
    }
```

and the final `for (at, it) in &sorted {` becomes `for (at, it) in &hashed {`. The doc comment's
"(a stable counting sort, so" becomes "(a stable radix sort by shard, so". `SHARDS` is 64, so the
key fits a `u8`. Why: the same sort, in place (no `sorted` buffer) and on the pool's workers past
`radix::PARALLEL` keys.

> forwarded to mem-backend (from requests, pass 16; told by tmux).

> forwarded by mem-backend to `requests-link-next-core` (holds `symbols.rs`; told by tmux).
> applied by requests-link-next-core: as given (`radix::sort_copy_by_key` by `shard_of as u8`, parts by `partition_point`, the `sorted` buffer gone, final loop over `hashed`, doc and import updated). Not built (lane rule); hot lint: no new findings.

### 3. `crates/blaze-link/src/elf/dynamic.rs`: `.gnu.hash` buckets by `radix::Csr`

Replace from `// \`ends[b + 1]\` counts bucket \`b\`;` through
`let bucket_start = |bucket: usize| ...;` with:

```rust
        // alloc: once per image, 4 bytes per bucket and per export
        let order = radix::Csr::build(
            buckets as usize,
            hashes.iter().enumerate().map(|(at, &hash)| (bucket_of(hash) as u32, at as u32)),
        );
```

then `for &at in &order {` becomes `for &at in order.items() {`; the bucket loop becomes

```rust
        for bucket in 0..buckets as usize {
            let range = order.range(bucket);
            gnu_hash.u32(if range.is_empty() { 0 } else { symoffset + range.start as u32 });
        }
        for (at, &export) in order.items().iter().enumerate() {
            let hash = hashes[export as usize];
            let last = at + 1 == order.range(bucket_of(hash)).end;
```

(the comment above stays: it is still a stable counting sort, now the shared one).

> forwarded to mem-backend (from requests, pass 16; told by tmux).

> forwarded by mem-backend to `requests-link-next-writers` (holds `elf/`; told by tmux).
> applied by requests-link-next-writers: as given. `order` is `radix::Csr::build(buckets as usize, (bucket_of(hash) as u32, at as u32) pairs)`; `.dynsym` walks `order.items()`, the bucket words read `order.range(bucket)`, and the chain words end at `order.range(bucket_of(hash)).end`. The local `ends` and `bucket_start` are gone, and the comment above stays. Same bytes, since the shared `Csr` is stable too. Not built (lane rule); hot lint: no findings in `elf/dynamic.rs`.

## dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng-tools` (report `/tmp/blaze-lanes/v2-dedup-codec-rng-tools.md`). File
`crates/blaze-vfs/src/outside/tests.rs` (you are marked as editing it). The xorshift stream in
`fn the_columns_answer_as_a_plain_table_does` moves onto `blaze_ids::rng::SplitMix64`
(`blaze-vfs` already depends on `blaze-ids`). The test pins no values.

1. In that function, after `use std::collections::HashMap;`, add
   `use blaze_ids::rng::SplitMix64;`.

2. Before:

```rust
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    for _ in 0..3000 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let depth
```

after:

```rust
    let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
    for _ in 0..3000 {
        let seed = rng.next_u64();
        let depth
```

(The rest of the loop reads `seed`'s bits as before.)

> forwarded to mem-backend (from requests, pass 16; told by tmux).

> applied by mem-backend (`outside/tests.rs`).
