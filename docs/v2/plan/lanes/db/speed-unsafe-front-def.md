---
status: plan
summary: hir-def, hir-expand and base-db under unsafe.md - lifetime-erasing ids made safe, dyn_map's transmutes gone, unsafe left only in the item tree's block allocator and the HIR column type, the crate root denying unsafe_code.
date: 2026-10-30
---
# speed-unsafe-front-def: hir-def, hir-expand, base-db under unsafe.md

Against [unsafe.md](../../design/unsafe.md): unsafe only where it buys measured speed, in small
modules behind a safe API, every block with a `// SAFETY:` comment, and untrusted bytes
verified before any view of them.

## What the unsafe was, and what is left

| Where | Before | After | Why |
|---|---|---|---|
| `lib.rs` `BlockIdLt::to_static`, `BlockId::to_db`, `ModuleIdLt::to_static`, `ModuleId::to_db` | 4 `unsafe fn` (transmutes) | 0 | salsa's `FromId`/`AsId` are safe for every lifetime; a stale tracked id is never UB |
| their call sites (`nameres*`, `facts/*`, `find_path`, `visibility`, `expr_store/lower`) | 26 blocks | 0 | the conversions are safe |
| `dyn_map.rs` `Index`/`IndexMut` | 2 transmutes | 0 | each key's map is a `KeyMap` field of `Maps`; indexing returns the field |
| `item_tree/block.rs` | 20 blocks, 5 `unsafe fn`; `columns!`' 3 blocks and 2 `unsafe impl` | same | one allocation per tree, not one per column |
| `hir/col.rs` | 5 blocks (one with two ops), 2 `unsafe impl` | 6 blocks, one op each | one slab per store; columns read in place in an image |
| `hir/persist.rs` `syntax_context` | 1 block | 0 | span made `SyntaxContext::from_u32` a safe `fn` (queue 00792): it was `unsafe` only as a marker of trust |

The ids' `unsafe` never bought speed, and memory safety never rested on it. A tracked struct is
`Struct(Id, PhantomData<&'db ()>)`, and salsa's macro gives a safe `FromId` for every `'db`
(`vendor/salsa-macro-rules/src/setup_tracked_struct.rs:253-262`), so safe code could already do
what `to_static` did. A deleted id is either reused from a page that `Table::page` checks is of
the same type, or `acquire_read_lock` panics on it (`vendor/salsa/src/tracked_struct.rs`).
That gives unspecified fields, never UB. The `'db` lifetime is a guard against using ids across
revisions, and `to_static` opts out of it, so the functions say so in a `# Contract` section.
`nameres/tests/stale_ids.rs` holds an id across revisions that delete its module and make
others.

`dyn_map.rs`: the transmute reinterpreted `&DynMap` as a `#[repr(transparent)]` `&KeyMap` so
that `Index` could return a typed view. With `Maps`' fields typed `KeyMap<AstPtr<N>, Id>`,
`Index` returns `&maps.field`: the same field access as before, no cast. Callers keep
`map[KEY].insert/get/is_empty`, and `insert` still returns `()`.

## The two unsafe modules

- `item_tree/block.rs`: a tree's columns in one allocation, placed by offsets. The safe version,
  a `Box<[T]>` per column, costs an allocation and a free for each non-empty column (a dozen or
  more per item tree) and two words per column in the tree. The safe API: a column is read only
  as its declared type (`columns!` generates the accessors from the `SHAPES` list), a sink
  writes only within its column, a tree is only made full, and a fill that fails drops what it
  wrote. Misusing the `pub(super)` `disarm`, or swapping two sinks of the same type, leaks or
  misplaces values but is never UB: the `for<'b>` brand stops sinks of two builds from mixing,
  and `is_full` means every slot was written since the last `disarm`. `columns!` is a local
  `macro_rules!`, so its expansion is linted where it is invoked (`item_tree.rs`). Its items
  carry `#[allow(unsafe_code)]`.
- `hir/col.rs`: a store's `Copy` columns share one slab (one allocation per store, where boxes
  would take 8 to 20 per body), or are read in place in a dependency image (no copy).
  `Col::borrowed` relies on `Held::bytes()` staying put while a count lives. `Held` can stay a
  safe trait: a borrow of what `bytes()` returns may last as long as any `&self`, so no sound
  impl frees or writes those bytes while the image is shared, and the column's count keeps it
  shared.

Reference tests, seeded: `block.rs` `columns_read_back_what_was_written` (200 trees against a
`Vec` per column, every alignment step, drops counted) and `col.rs`
`a_slab_column_reads_as_its_box` (64 seeds, four alignments, against `Vec` clones; the copies
outlive the writer, the sources and the other columns).

## Root gate

`crates/hir-def/src/lib.rs`: `#![deny(unsafe_code)]`, with `#[allow(unsafe_code)]` on
`item_tree.rs`'s `mod block;`, on `hir.rs`'s `mod col;` and inside `columns!`' expansion.
`hir-expand` and `base-db` already say `#![forbid(unsafe_code)]`. Their only `unsafe` is tokens
inside `quote_in!` (user code that `attr_macro.rs` emits) and salsa's `unsafe(no_lifetime)`
attribute argument. None of the three crates has a test, bench or example root besides
`src/lib.rs`.

Macros from other crates (`blaze_format::record!`, salsa's, `intern::impl_internable!`) need
no allow. rustc's `lint_level` drops every lint without `report_in_external_macro` when the
span comes from another crate's macro (`rustc_middle/src/lint.rs`, `Span::in_external_macro`),
and `UNSAFE_CODE` doesn't set it. A proc-macro attribute's expansion counts as external too
(`ExpnKind::Macro { .. } => true`). Salsa doesn't re-emit the user's `unsafe` token from
`unsafe(no_lifetime)`. It quotes a new one with a call-site span.

## Untrusted bytes

All hir-def store readers run after `ImageStore::get`, which returns bytes checked against
their content hash (`blaze_cas::Reader::pinned` in production, `deps/serve.rs:38`). Then:
- `ImageRef::open` checks the header, schema, root key, the parts' count, order, bounds and
  16-byte alignment.
- The item tree, expression stores and source maps read through `open_as` and `Slice::follow`
  (bounds-checked), with kind tags range-checked (`ItemKind::from_u8`, `expand_to_of`, the
  stores' kind bytes). `decode` then checks every id and range (`check`,
  `HirColumns::check`/`TypeColumns::check`).
- None of the readers has `unsafe`. `syntax_context` refuses `u32::MAX`.

What is left:
- An ast id's kind is checked against its item's kind: `check` calls `ItemTree::ast_id_fits`,
  which uses span's `ErasedFileAstId::may_be::<N>()` (queue 00800).
- `ItemTreeRef::top_level` yields unchecked ids, but nothing outside `decode` reads it, and
  `kind`/`ast_id`/`children` return `None` on a bad id.
- A corrupt signature record panics in `deps/signatures.rs::served`, naming the crate, item and
  reason. The bytes passed the hash check, so this means an encoder bug, and the query has no
  error channel. This is never UB.

Fuzz targets: the item tree and the four expression-store readers have them (`fuzz/targets/hir_*`).
The dependency image, `Reads::decode`, the signatures part and the frozen def map are asked of
safety (queue 00801).
