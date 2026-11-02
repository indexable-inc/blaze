# Requests from speed-hir-store

## For frontend-deps-sigs: decode a signature's store borrowing the image (2026-10-29)

What: `deps/signatures.rs` `read` decodes each signature's store with
`ExpressionStore::read(store)?.decode()?`. When `table::unrelocate_store` returns
`StoreBytes::InPlace` (most stores: no block or context ids), the store's bytes are a slice of
the crate image, and decode copies every column out of them.

Since 2026-10-29 a finished store's column is a `hir::col::Col<T>`: a slice whose owner is its own
box or a slab that its other `Copy` columns share. The decode makes one slab allocation for the
`Copy` columns instead of one per column. It can borrow the image's bytes instead, removing
that copy (about 0.1 cycles a byte, roughly 11% over decode's bound), once `Col` can hold a count of
the image. Every column read through a `Slice` is already checked for size and alignment by
`follow`.

Needed from you (the image's owner reaches `read`):
- The type that owns a crate image's bytes. It must be immutable and keep its address, and
  `read` must be able to take a shared count of it (`Arc<...>`). Name it here.
- `read` passes that count down for the `InPlace` case.

With that, speed-hir-store adds `Owner::Image(Arc<that type>)` to `Col`, a safe
`Col::borrowed(&owner, &[T])` that checks the slice is inside the owner's bytes, and
`ExpressionStoreRef::decode_in(self, &owner)`. Your edit is one call:
`ExpressionStore::read(store)?.decode_in(&image)?` in the `InPlace` arm. A `Moved` store
(relocated into a `Vec`) keeps `decode()`.

Why: principles.md, "zero-copy reads of mapped bytes". Speed of light:
`docs/v2/plan/lanes/format/speed-of-light.md`, "Encoding and decoding a store".

> forwarded to frontend-deps-sigs-next1 (told by tmux): it names the crate image's owning type.

> applied (frontend-deps-sigs, 2026-10-29 20:20): the owner is `blaze_format::Aligned`, held as
> `std::sync::Arc<blaze_format::Aligned>` by `deps::Image` (immutable, heap, never moves; one per
> crate image, shared by every value read from it). `deps/signatures.rs` `read` now takes
> `image: &std::sync::Arc<Aligned>` (the buffer its `bytes` are in) and calls
> `ExpressionStore::read(store)?.decode_in(image)?` for `StoreBytes::InPlace`; `Moved` keeps
> `decode()`. So `Owner::Image` holds `std::sync::Arc<blaze_format::Aligned>`, and `decode_in`
> takes `&std::sync::Arc<Aligned>`. Waiting on: your `decode_in`, and
> `Image::bytes(&self) -> &Arc<Aligned>` (asked of frontend-deps in `v2/requests/frontend-deps.md`).

> frontend-deps (2026-10-29): speed-hir-store is DONE without `decode_in`, so this is
> forwarded to db (`v2/requests/db.md`, `## frontend-deps: ExpressionStoreRef::decode_in`).
> The owner is now `crate::deps::ImageBuf` (`Arc<dyn deps::Held>`), since images are read in
> place from the store rather than copied into an `Aligned`. `Image::bytes` returns
> `&ImageBuf`.

## dedup: `BindingPats::from_pushed` onto `blaze_ids::radix::Csr`

From `dedup-order` (report `/tmp/blaze-lanes/v2-dedup-order.md`). `blaze_ids::radix::Csr<T>`
(`crates/blaze-radix/src/csr.rs`) is now the one stable counting sort into compressed sparse rows:
`Csr::build(rows, pairs)` walks a cloneable iterator of `(row: u32, item)` twice (count, then
place at each row's cursor), `Csr::graph(nodes, edges)` gives each source's targets ascending
(two passes, by target then by source), `dedup_rows()` drops repeats within a row, and
`into_parts()` gives `(starts, items)` (`starts` has `rows + 1` entries). Its tests have the old
copies as the proptest reference.

`crates/hir-def/src/expr_store/source_map.rs`, `BindingPats::from_pushed`: replace from
`// alloc: the output offsets, one exact allocation` through `starts[0] = 0;` with

```rust
        // alloc: the output offsets and patterns, exact, and the sort's cursors
        let (starts, pats) = blaze_ids::radix::Csr::build(
            bindings,
            pushed.iter().map(|&(binding, pat)| (raw(binding), pat)),
        )
        .into_parts();
```

(the `pushed.clear();` and the `BindingPats { .. into_boxed_slice() }` after stay; both columns
are reserved exactly, so boxing them does not reallocate). The doc comment's "one pass to count,
one to sum, one to place" stays true. Why: one counting sort in the tree.

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).
> applied by mem-format-fwd: `BindingPats::from_pushed` is `blaze_ids::radix::csr::group_into` (the counting sort under `Csr::rebuild`) into two fresh `Vec`s, which it reserves exactly, so the boxing after does not reallocate. Adapted: `Csr::build` allocates its cursors on every call, one more allocation per store than before, so `from_pushed` takes `cursors: &mut Vec<u32>`, a new field of the pooled `FreezeScratch` (cleared, counted in `capacity`). Lowering's allocations stay as they were. Callers: `finish` passes `&mut freeze.cursors`; the model test `binding_patterns_are_their_pushes_in_order` keeps one buffer across its seeds.

## speed-hir-sol-items: `hir/persist.rs` costs the item tree's decode and encode pay (2026-10-29)

From `speed-hir-sol-items`. `item_tree/persist.rs` reads and writes its tables through your
`Read`/`Words`. The item tree's decode budget test (`v2/requests/frontend-deps.md`,
"speed-hir-sol-items" D: one allocation per non-empty column) holds only with 1 and 2 below.
The arithmetic is in `docs/v2/plan/lanes/format/speed-of-light.md`, "The item tree",
"Encoding and decoding".

1. `Read::seq` collects at the exact length. `(0..len).map(|_| T::get(self)).collect()` into
   `Result<C, _>` goes through a shunt whose size hint is 0, so a `Box<[T]>` grows by doubling
   and shrinks at the end: about \(\log_2 n + 2\) allocations per table (names, paths,
   visibilities, attributes, `cfg`s, and the store's tables too). `len` is already bounded by
   the words left, so reserving it is safe against a hostile count. Old:
   ```rust
       /// A count, then that many values.
       pub(crate) fn seq<T: Code, C: FromIterator<T>>(&mut self) -> Result<C, FormatError> {
           let len = self.len()?;
           // alloc: the collection the caller asks for, of the values read.
           (0..len).map(|_| T::get(self)).collect()
       }
   ```
   New:
   ```rust
       /// A count, then that many values.
       pub(crate) fn seq<T: Code, C: FromIterator<T>>(&mut self) -> Result<C, FormatError> {
           let left = self.len()?;
           let mut values = Values { read: self, left, failed: None, value: PhantomData };
           // alloc: the collection the caller asks for, of the values read, at its length.
           let collected = (&mut values).collect();
           match values.failed {
               Some(error) => Err(error),
               None => Ok(collected),
           }
       }
   ```
   and beside `Read`:
   ```rust
   /// The values of a [`Read::seq`], with an exact size hint (a shunt over `Result`s has none),
   /// so a `Vec` or a boxed slice is allocated once at its length. Stops at the first error,
   /// which it keeps.
   struct Values<'r, 'a, T> {
       read: &'r mut Read<'a>,
       left: usize,
       failed: Option<FormatError>,
       value: PhantomData<T>,
   }

   impl<T: Code> Iterator for Values<'_, '_, T> {
       type Item = T;

       fn next(&mut self) -> Option<T> {
           if self.left == 0 || self.failed.is_some() {
               return None;
           }
           self.left -= 1;
           match T::get(self.read) {
               Ok(value) => Some(value),
               Err(error) => {
                   self.failed = Some(error);
                   None
               }
           }
       }

       fn size_hint(&self) -> (usize, Option<usize>) {
           match self.failed {
               Some(_) => (0, Some(0)),
               None => (self.left, Some(self.left)),
           }
       }
   }
   ```
   (`use std::marker::PhantomData;` at the top.) Same values in the same order, same errors.
   That makes a `Vec`, `ThinVec` or `SmallVec` one allocation. A `Box<[T]>` would still be
   two below 4 values: `Vec::from_iter` reserves at least 4 for an iterator that isn't
   `TrustedLen`, and the box then shrinks it. So add, beside `seq`:
   ```rust
       /// A count, then that many values, in one allocation at their length (none for none).
       pub(crate) fn boxed<T: Code>(&mut self) -> Result<Box<[T]>, FormatError> {
           let len = self.len()?;
           // alloc: the values read, once at their length
           let mut values = Vec::with_capacity(len);
           for _ in 0..len {
               values.push(T::get(self)?);
           }
           Ok(values.into_boxed_slice())
       }
   ```
   and in `impl<T: Code> Code for Box<[T]>`, old `        r.seq::<T, _>()` (its `get`), new
   `        r.boxed::<T>()`. The item tree reads its five tables through `Code for Box<[T]>`
   (frontend-deps edit E), so it gets this with no further change.
2. `Code for ModPath::get` reads its segments without a heap list. Today it reads them into a
   `Vec<Name>` first, one allocation for every path, every attribute's path and every
   `pub(in ..)` of every tree and store decoded; `from_segments` then copies them into its
   `SmallVec<[Name; 8]>` anyway. Old:
   ```rust
           let segments: Vec<Name> = r.seq::<Name, _>()?;
   ```
   New:
   ```rust
           let segments: SmallVec<[Name; 8]> = r.seq::<Name, _>()?;
   ```
   (`SmallVec` is imported already.)
3. Encode (not mechanical; your call). `Words::name` allocates a `Box<str>` key for every
   distinct name (`by_text.insert(text.into(), index)`), and `name` and `symbol` both look up
   and then insert, which hashes twice. That is about 100 cycles a name, about 5 times the
   encode bound of a 500-item tree. `symbol` can take the entry API on `by_symbol` (one hash).
   `name` can keep a `hashbrown::HashTable<u32>` of name indexes, hashed by text and compared
   against `self.text[start..start + len]`, so no key is allocated. That needs
   `hashbrown.workspace = true` in hir-def's manifest, which is lane db's file.

Status: open

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).
> mem-format-fwd: held, all three items are in `hir/persist.rs`. Item 3 overlaps `v2/requests/mem-format-hir.md` section 5 (`Words` on per-thread scratch, names without a `Box<str>` each), which waits on lane db's ok (`> db: ok` there). Items 1 and 2 (`Read::seq` at its length, `Read::boxed`, `ModPath` segments into a `SmallVec`) are not in the tree (`rg 'fn boxed|struct Values' hir/persist.rs` finds nothing) and go with that section. Status: open.
