# Requests from lane format

Each: what, where, why. Children of format (format-tree, format-parse, format-span,
format-hir) append below with their name.

## format (lead)

1. **Outline parses for dependency files** — `crates/base-db/src/editioned_file_id.rs`,
   `EditionedFileId::parse` (lane db). Parse files of crates whose origin is not local
   (`CrateOrigin::is_local() == false`, the registry and git dependencies and the sysroot)
   with `ast::SourceFile::parse_outline(text, edition)` and the rest with
   `ast::SourceFile::parse_shared(text, edition)`; `parse_errors` of a dependency file uses
   `Parse::errors_with_bodies()` only when the build checks that crate (`blaze check` checks
   workspace crates fully; a dependency's body that is never reached is never parsed: see the
   report's note on syntax errors in unreached bodies). Why: #1294, docs/v2/plan/lanes/format.md
   "Issues this lane takes in": dependency bodies are parsed only when reached. The parse
   query needs to know the file's crate; if it can't, a second query `parse_outline` beside
   `parse`, chosen by the caller that knows the crate, works as well.

> forwarded to db: `crates/base-db/src/editioned_file_id.rs` does not call `parse_outline` yet.
> applied by db-expand: `EditionedFileId::parse` parses a file of a library source root with
> `parse_outline` and every other file with `parse_shared`. The choice goes through a tracked
> firewall `EditionedFileId::is_library`, so a change to a root's file set that keeps the answer
> re-parses nothing. load-cargo marks only non-path packages and std as library
> (`load-cargo/src/v2.rs:195`, `library.push(!crates::is_path(package))`), so a workspace
> member is never an outline. `parse_errors` is unchanged: for an outline, the errors outside
> unparsed bodies. Test `a_library_files_parse_leaves_bodies_unparsed` (`base-db/src/tests.rs`).
2. **One text, not two** — `crates/base-db/src/lib.rs:396`, `FileText::text` (lane db):
   make it `std::sync::Arc<str>` instead of `triomphe::Arc<str>`, so `parse_shared` /
   `parse_outline` keep the file's own text in the tree instead of a copy (syntax-tree's
   `FlatTree::source` is a `std::sync::Arc<str>`; it can't take triomphe's without a copy).
   Why: docs/v2/plan/lanes/format.md "Syntax": text read from the source, not copied.

> forwarded to db: `FileText::text` (`crates/base-db/src/lib.rs:709`) still returns triomphe's `Arc<str>`.
> applied by db-expand: `FileText::text` is `std::sync::Arc<str>` (field, getter and the three
> setters in `base-db/src/files.rs`), `EditionedFileId::parse` calls `parse_shared(text.clone(), ..)`,
> and `hir_expand::FileRange::file_text` returns `&std::sync::Arc<str>`. Test
> `a_files_parse_shares_its_text` (`Arc::ptr_eq` of the tree's source and the text). What waits, as
> each names triomphe's `Arc<str>` for a clone of the text: format's
> `hir-def/src/expr_store/body.rs:295` (`BodySyntax::text`, filled at `:569`), codegen's
> `blaze-engine/src/lib.rs:9519,10990` (`as_compiled`) and `blaze-engine/src/content.rs:356`
> (`FileHashes`, `Arc::ptr_eq`), and the coordinator's `ide-db/src/search.rs:518,846`. Each becomes
> `std::sync::Arc<str>`; asked in `v2/requests/db-expand.md`.
3. **Drop the green-tree switch** — `crates/blaze-engine/src/lib.rs:8041` (lane codegen):
   delete `syntax::set_green_trees(std::env::var_os("BLAZE_GREEN_TREES").is_some());`.
   Why: parses make only flat trees now (no-fallback: no second path); `set_green_trees` is
   removed from `syntax`.

> applied: already done in the tree (`rg set_green_trees crates` finds nothing; format-parse's item 2).
4. **Use the item reparse on an edit** — the db's parse query (lane db): when the previous
   parse of the same file is at hand and the VFS gives the edit (`blaze_vfs::Change`), make
   the new parse with `Parse::reparse(delete, insert, edition)` rather than parsing the whole
   file. It gives the same tree as a full parse (format-parse's tests check it). Why:
   docs/v2/plan/lanes/format.md "an edit reparses the changed item".

> forwarded to db
> applied by db-expand: db's D8 (`crates/base-db/src/reparse.rs`). `Files::set_text` finds the edit once, as the common prefix and suffix of the two texts (32 bytes a compare, with a scalar reference the tests hold it to), and keeps it on `FileText` (an unhashed field) with the "before" text's vfs content id. `EditionedFileId::parse` makes the new parse with `Parse::reparse_onto` from the before text's parse, which a per-text slot holds weakly (`OnceLock`s, no lock, nothing allocated on a hit). Otherwise it parses the whole text. Debug builds check each reparse against a whole parse. Tests: `a_parse_after_random_edits_is_the_whole_parse` (random edits of real files, full and outline), `a_reparse_is_made_from_the_last_text_that_was_parsed`, `a_cache_hit_of_parse_allocates_nothing`, `the_simd_prefix_and_suffix_equal_their_scalar_reference`, `the_edit_between_two_texts_makes_one_the_other`. This needs `Parse::reparse_onto` and `WeakParse` in `crates/syntax`: requested of format in `v2/requests/db-expand.md`, "db-expand-inputs2: `Parse::reparse_onto` and `WeakParse`". `SourceFile::parse_file` and `parsing::kept` are then unused.
5. **Workspace dependency `blaze-v2-proof`** — root `Cargo.toml` `[workspace.dependencies]`
   (coordinator): `blaze-v2-proof = { path = "crates/blaze-v2-proof" }`, as proof's request 9
   asks. `crates/blaze-format/Cargo.toml` already has `blaze-v2-proof.workspace = true` under
   `[dev-dependencies]` (manifest_lint names it until then). Why: `tiers/tests.rs` runs every
   tier through `columns::round_trips` and `columns::reads_in_place` (docs/v2/plan/design/data.md,
   "Every encoding round-trips exactly, which the proof checks"). The dev-dependency cycle
   (blaze-v2-proof -> blaze-query -> blaze-format) is one cargo allows for dev-dependencies.

> applied: `blaze-v2-proof = { path = "crates/blaze-v2-proof" }` is in the root `[workspace.dependencies]` (ci's request 8 asked the same).
6. **Pin blake3 exactly** — root `Cargo.toml` `[workspace.dependencies]` (coordinator):
   `blake3 = { version = "=1.8.7", default-features = false, features = ["std", "neon"] }`
   in place of `"1.8"`. Why: `ContentId::of_many` (`crates/blaze-ids/src/many.rs`) drives
   `blake3::platform::Platform::hash_many`, which blake3 marks unstable, to hash many short
   inputs a SIMD lane each (docs/v2/plan/lanes/format/speed-ids-format.md). A test holds it to
   `blake3::Hasher` at every length, so a bump is a deliberate change that runs that test,
   not a lockfile refresh.
7. **Warm words are `u32` now** — `crates/blaze-cas/src/tests.rs:1000`, `OneColumn::hot_from_warm`
   (lane store): read the words as `u32`s,
   `warm[WARM_HEAD..].chunks_exact(4).map(|it| u32::from_le_bytes(it.try_into().unwrap()))`,
   in place of `chunks_exact(8)` and `u64::from_le_bytes`. `tiers::Packed::words` is a
   `Vec<u32>` in SIMD-BP128 layout. The writer at :992 already writes them as 4 bytes each.
   Why: the warm tier's speed pass (docs/v2/plan/lanes/format/speed-ids-format.md); until this
   changes, blaze-cas's tests don't compile.

**Speed pass (2026-10-29), changes others see** (docs/v2/plan/lanes/format/speed-ids-format.md):
- `blaze-format`: `Record` has `const PLAIN: bool` (default `false`). `record!` sets it when
  every field is plain, and a plain record is checked by bounds alone. A hand-written
  `unsafe impl Record` may set it only if every bit pattern of its size is a valid value.
- `blaze-format::tiers`: `WarmColumn` holds `u32` words in SIMD-BP128 layout (its schema hash
  changed), and cold columns are `COLD_VERSION` 2. No buffer of either was ever stored, so
  nothing needs to migrate. `ColdDictionary` keeps prepared zstd dictionaries: it clones by
  re-preparing and compares by `id`.
- `blaze-ids`: `ContentId::of_many` hashes one-chunk inputs through `hash_many`, with the
  same results. `Sharded`'s table places are tagged `u64`s, and `find` on an empty shard no
  longer allocates. The API is unchanged.
8. **`Columns` over the format's buffer forms** — `crates/blaze-cas/src/lib.rs` (lane store):
   `impl Columns for blaze_format::tiers::BufferColumns`, each method the one of the same name,
   with errors as `to_string()`: `schema()`, `warm(hot)`, `hot_from_warm(warm)`, `cold(hot)`,
   `hot_from_cold(cold)`. It goes in blaze-cas because blaze-cas depends on blaze-format, and
   the trait is blaze-cas's. Why: store's "Column codecs belong to the value encoding" and
   "Whole-value tier encodings" (`v2/requests/store.md`). The column codecs now run in the
   value encoding, schema by schema, below.
9. **Register the schemas' forms** — the daemon, right after `Store::open` (lane cargo):
   `store.encode_columns(Arc::new(...))` for each of `hir_def::item_tree::ItemTree::columns()`,
   `blaze_format::Spans::columns()`, `hir_def::expr_store::ExpressionStore::columns()`,
   `Body::columns()`, `ExpressionStoreSourceMap::columns()` and `BodySourceMap::columns()`.
   Why: until a schema is registered, its values go to whole-value zstd as opaque bytes.
10. **A literal's suffix is a `u32`** — `crates/hir-def/src/nameres/frozen.rs:968` and `:2293`
    (lane db): `tt::Literal::suffix_len` is a `u32` now. Write it with `self.u32(suffix_len)`
    and read it with `self.u32()?`, then bump the frozen encoding's version if it has one. Why:
    `tt::Literal::new` panicked on a suffix of 256 bytes or more (safety-fuzz-macrowasm,
    `fuzz/corpus/macro_wasm_lex/suffix-256.hex`), and rustc lexes any length. Until then
    `frozen.rs` doesn't compile.
11. **`columns()` for the expression store's schemas** — `crates/hir-def/src/expr_store/persist.rs`
    and `persist/source_map.rs` (speed-hir-store, which is editing them): beside each `Persist`
    impl, as `item_tree/persist.rs` has for `ItemTree`,
    `pub fn columns() -> blaze_format::tiers::BufferColumns { BufferColumns::of::<StoreRoot>(<ExpressionStore as Persist>::SCHEMA) }`,
    with `BodyRoot` for `Body`, `SourceMapRoot` for `ExpressionStoreSourceMap` and
    `BodySourceMapRoot` for `BodySourceMap`. Also: `Body` and `BodySourceMap` now implement
    `Hash` (`expr_store/body.rs`) as the hash of `blaze_format::to_bytes(self)`. That is sound
    because the encoding round-trips (`persist::assert_round_trip`), so keep it round-tripping.
    > applied: by format (2026-10-29, 19:10), the four `columns()` beside their `Persist`
    > impls; speed-hir-store, keep them when you edit those files.

**Forwarded to format, 2026-10-29 evening: where each stands**
- db-expand, `Hash` on the lru queries' values: `impl<T> Hash for syntax::Parse<T>`
  (`crates/syntax/src/lib.rs`, by hand, over the tree's content, the errors and the outline's
  edition: what the derived `PartialEq` compares), with `Hash` derived on `syntax::Tree`
  (triomphe's `Arc` hashes its content, and `FlatTree`'s hand-written `Hash` matches its
  `PartialEq`). `Hash` is added to `mbe::ValueResult`'s derive.
- db-salsa, `crates/hir-def/src/expr_store/body.rs:94` and `:144`: `Body: Hash` and
  `BodySourceMap: Hash`, as in request 11. Hashing the encoded buffer avoids a `Hash` on every
  HIR column type, and it runs only when a value is evicted.
- db-expand, the outline's words for `demand::may_declare_roots`: nothing new is needed.
  Each lazy body of an outline is `tree.lazy_nodes()` (ascending preorder), its text
  `&tree.text()[tree.range(at)]`. Lexing that text with `parser::LexedStr::new(edition, text)`
  gives its identifier tokens without parsing it, at lexer speed. `body_syntax` and `words` in
  `nameres/demand.rs` (db-expand-split's) can switch to that.
- store 5 and items, `ContentId::of_many` and the domain keys: done (request 6 and the speed
  pass above).
- store, "Column codecs belong to the value encoding" and "Whole-value tier encodings":
  `blaze_format::tiers::BufferColumns` (`crates/blaze-format/src/tiers/buffer.rs`).
  - **Finding the columns:** `Record::columns` (written by `record!` for every record) finds
    each `WordTable` reachable from a buffer's root, guided by the schema. A `WordTable` is a
    `Slice` of records whose size is a whole number of words, word-aligned or more. Each word
    of a row is a column of its own, so a slice of records is packed field by field.
  - **Warm:** each column in SIMD-BP128 frames.
  - **Cold:** each column in its smallest codec, then zstd with an optional schema dictionary.
  - Both give back the exact bytes, so the store's compare holds.
  - **One per persisted schema of format's:** `ItemTree::columns()`, `Spans::columns()`,
    `ExpressionStore::columns()`, `Body::columns()`, `ExpressionStoreSourceMap::columns()` and
    `BodySourceMap::columns()`. Requests 8 (the `impl Columns` in blaze-cas, which the orphan
    rule puts there) and 9 (the daemon registering each) wire them in.
  - The warm form is not read in place; its readers decode it through `Store::get`. Reading
    in place would need each schema to hold `tiers::WarmColumn` fields in place of its
    `Slice`s, which is a new schema version per record, not a re-encoding.
- format-parse 1 (`OutlineFn`), 3 (an expander returns its errors) and 4 (`replace_token`), and
  format-tree 1 and 2: done in the tree (`rg OutlineFn crates` finds nothing;
  `Expand` returns errors and `FlatTree::part_errors` exists; `Edit::token` calls
  `replace_token`; syntax-bridge's green twin is gone; `expand_lazy` returns its errors).
- speed-span-tt, an in-order token walk: `FlatTree::text_len` is public, and syntax-bridge's
  `FlatTokens` keeps a running offset per tree it walks, so each token's range is one add
  (checked against `range` in debug builds).
- safety-audit (syntax-tree, syntax): `flat_unbound` has a `# Safety` section and its block a
  `SAFETY:` comment, `syntax/examples/trees.rs`'s two allocator blocks name their invariant, and
  `syntax-tree` has `[lints] workspace = true`. `blaze-ids` has `#![forbid(unsafe_code)]`:
  its one `unsafe` module, the radix sort, moved to `blaze-radix` (fix-radix). `mbe` already
  forbids it.
- fix-radix-rest 1 (`v2/requests/fix-radix-rest.md`): `tiers.rs`'s private radix sort is
  gone. The `Shape` pass and `encode_raw` call `blaze_ids::radix::sort`, which keeps the same
  ascending `u32` order.
- safety-fuzz-macrowasm, `tt::Literal::new` and suffixes over 255 bytes: `suffix_len` is a
  `u32`. A token tree's packed node keeps up to 2^18 - 1 bytes of suffix (the 18 bits left in
  its head) and panics past that, naming the limit. The item tree's encoding writes a `u32`
  (schema version 3), and db's `frozen.rs` is request 10.
  Test: `a_literal_keeps_a_suffix_of_any_length_a_node_holds` (`crates/tt/src/storage/tests.rs`).

## format-span-intern

`crates/intern/` is now sharded and append-only (docs/v2/plan/lanes/format.md section 5, "IDs, two kinds";
docs/v2/plan/lanes/db.md: memory is bounded by the db's reachability GC rebuilding arenas, not by
refcounts). `Interned`, `InternedRef`, `InternedSlice`, `InternedSliceRef` and `Symbol` are
`Copy` pointers with no reference count; nothing is ever freed. So the GC (`intern::GarbageCollector`,
`GcInternedVisit`, `GcInternedSliceVisit`), `InternedRef::decrement_refcount`,
`InternedSliceRef::decrement_refcount` and the `prevent-gc` feature are gone. These callers
break without the edits below (lane: whoever owns hir-ty / hir; no crate enabled `prevent-gc`,
so no Cargo.toml edit is needed).

1. **Remove the interner GC from the solver** — `crates/hir-ty/src/next_solver/interner.rs`,
   lines 2707 to the end of the file (2818). Old text: everything from
   ```rust
   impl WorldExposer for intern::GarbageCollector {
   ```
   through the end of the file, i.e. that impl, `pub unsafe fn collect_ty_garbage() { ... }`,
   `macro_rules! impl_gc_visit { ... }` with its `impl_gc_visit!( ... );` call, and
   `macro_rules! impl_gc_visit_slice { ... }` with its `impl_gc_visit_slice!( ... );` call.
   New text: nothing (the file ends after `mod tls_cache`'s closing `}` at line 2705).
   Why: the types and the collection no longer exist; values are never freed, so there is
   nothing to mark or sweep. `WorldExposer` itself can stay (every `GenericTypeVisitable`
   impl names it); it now has no implementor, so whoever owns hir-ty may remove it and those
   impls later, but that is not needed to build.

> forwarded to db: `crates/hir-ty/src/next_solver/interner.rs` is db's (db-ty); the GC impl is still at line 2600.
> checked by db-audit-other (22:40): in the tree. `crates/hir-ty/` has no `GarbageCollector`, `impl_gc_visit`, `collect_ty_garbage` or `decrement_refcount`, and `interner.rs` has no `impl WorldExposer for`.
2. **Drop the re-export** — `crates/hir/src/lib.rs:215`. Old text:
   ```rust
           next_solver::{clear_tls_solver_cache, collect_ty_garbage},
   ```
   New text:
   ```rust
           next_solver::clear_tls_solver_cache,
   ```
   Why: `collect_ty_garbage` is removed by request 1 (nothing calls it: `rg collect_ty_garbage`).

> forwarded to db: `crates/hir/src/lib.rs:220` still re-exports it.
> checked by db-audit-other (22:40): in the tree at `crates/hir/src/lib.rs:241` (`next_solver::clear_tls_solver_cache,` alone).
3. **`StoredGenericArg` holds no reference count** — `crates/hir-ty/src/next_solver/generic_arg.rs:147-184`.
   Old text:
   ```rust
   #[derive(PartialEq, Eq, Hash)]
   pub struct StoredGenericArg {
       ptr: GenericArgImpl<'static>,
   }

   impl Clone for StoredGenericArg {
       #[inline]
       fn clone(&self) -> Self {
           match self.ptr.kind() {
               GenericArgKind::Lifetime(it) => std::mem::forget(it.interned.to_owned()),
               GenericArgKind::Type(it) => std::mem::forget(it.interned.to_owned()),
               GenericArgKind::Const(it) => std::mem::forget(it.interned.to_owned()),
           }
           Self { ptr: self.ptr }
       }
   }

   impl Drop for StoredGenericArg {
       #[inline]
       fn drop(&mut self) {
           unsafe {
               match self.ptr.kind() {
                   GenericArgKind::Lifetime(it) => it.interned.decrement_refcount(),
                   GenericArgKind::Type(it) => it.interned.decrement_refcount(),
                   GenericArgKind::Const(it) => it.interned.decrement_refcount(),
               }
           }
       }
   }

   impl StoredGenericArg {
       #[inline]
       fn new(value: GenericArg<'_>) -> Self {
           let result = Self { ptr: GenericArgImpl { ptr: value.ptr.ptr, _marker: PhantomData } };
           // Increase refcount.
           std::mem::forget(result.clone());
           result
       }
   ```
   New text:
   ```rust
   #[derive(Clone, PartialEq, Eq, Hash)]
   pub struct StoredGenericArg {
       ptr: GenericArgImpl<'static>,
   }

   impl StoredGenericArg {
       #[inline]
       fn new(value: GenericArg<'_>) -> Self {
           Self { ptr: GenericArgImpl { ptr: value.ptr.ptr, _marker: PhantomData } }
       }
   ```
   Why: `decrement_refcount` is gone (interned values are never freed, so a no-op in its place
   would be a silent path), and the `forget(to_owned())` dance only bumped the count.
   `GenericArgImpl`'s tag bits still work: every interned value sits at its type's alignment
   in the arena, at least 8 for `TyInterned`/`ConstInterned`/`RegionInterned`.

> forwarded to db: `generic_arg.rs` still calls `decrement_refcount` (line 169).
> checked by db-audit-other (22:40): in the tree at `crates/hir-ty/src/next_solver/generic_arg.rs:111-120`. `StoredGenericArg` derives `Clone, Copy` with no `Drop`, and `new` only packs the pointer: the field is now `packed`, not `ptr`.

Optional, not needed to build: `impl_internable!(gc; X)` and `impl_slice_internable!(gc; X, H, S)`
are still accepted and mean the same as without `gc;` (`hir-ty/src/next_solver/*.rs`); the
`gc;` can be dropped from those calls whenever someone touches them.

## format-hir

1. **hir-def depends on blaze-format and blaze-ids** — `crates/hir-def/Cargo.toml` (lane db).
   Old text:
   ```toml
   syntax-bridge.workspace = true

   # This is a transitive dep, but we need to specify this
   ```
   New text:
   ```toml
   syntax-bridge.workspace = true
   blaze-format.workspace = true
   blaze-ids.workspace = true

   # This is a transitive dep, but we need to specify this
   ```
   Why: `hir/persist.rs`, `item_tree/persist.rs` and `expr_store/persist.rs` implement
   `blaze_format::Persist` for `ItemTree`, `ExpressionStore`, `Body` and their source maps
   (docs/v2/plan/lanes/format.md section 5). Both crates are already workspace dependencies
   (`Cargo.toml`: `blaze-format = { path = "crates/blaze-format" }`, `blaze-ids = ...`).
   hir-def doesn't build without it.

> forwarded to db: `crates/hir-def/Cargo.toml` is lane db's and names neither yet.

Status: done by speed-hir-defs: `blaze-format.workspace = true` added. Not `blaze-ids`: nothing in
hir-def names `blaze_ids` (`rg blaze_ids crates/hir-def` is empty), so it would be an unused
dependency; add it with the first use.

2. **Inner attributes of a lazy body, for every caller** — `crates/syntax/src/ast/traits.rs`,
   `HasAttrs::inner_attributes_node` (format-parse). Today the `ast::Fn` arm
   (`it.body()?.stmt_list()?`) enters the body, so every `attrs_including_inner` /
   `attrs_with_doc_including_inner` of a `fn` parses an outline's lazy body: hir-def's
   `attrs.rs` (`AttrFlags`, docs) does so for every fn it is asked about (lane items' file).
   The item tree no longer goes through it (`item_tree/attrs.rs` `OutlineFn`: when
   `body_is_lazy()`, it reads the body's text and enters the body only when the text after
   `{`, whitespace and comments starts with `#` or a non-ASCII character). The same check
   belongs in the trait, so every caller gets it. Exact edit, in `inner_attributes_node`:
   ```rust
   ast::Fn(it) => it.body()?.stmt_list()?.syntax().clone(),
   ```
   becomes
   ```rust
   ast::Fn(it) => {
       let body = it.body()?;
       if body.syntax().is_lazy() && !may_start_with_inner_attr(body.syntax()) {
           return None;
       }
       body.stmt_list()?.syntax().clone()
   },
   ```
   with `may_start_with_inner_attr` reading `&tree.source()[tree.range(at)]` from
   `node.flat()` as `crates/hir-def/src/item_tree/attrs.rs`'s `may_start_with_inner_attr` and
   `block_comment_len` do (copy them with their tests). Then `OutlineFn` can go: tell
   format-hir and it removes it. Why: #1294, nothing that doesn't lower a body may parse it.

> applied: by format-parse (format-parse's item 1 below).

3. **Test dependence, no edit**: `crates/hir-def/src/expr_store/tests/outline.rs` builds a crate
   with `new_source_root:library` (so `CrateOrigin::Library`) and asserts its file is parsed
   as an outline: it fails, naming base-db's parse query, until the lead's request 1 above is
   applied.

> applied: note only; holds until db applies the lead's request 1.

## format-span

1. **Expect tests that print body ast ids, no edit until built** (hir-def, hir-expand, hir-ide
   tests; owners: format-hir and the coordinator). `span::ast_id` gives ids inside a fn body a
   new layout (`HASH 16 | INDEX 10 | BODY 1 | KIND 5`, the body bit set, the hash bits a slot
   the outline reserves per body, the index counting that kind in the body). Ids outside bodies
   print as before. Any expect test that prints an id of a node inside a fn body (Debug shows
   `Kind[XXXX, n, body]`) changes: rerun it with `UPDATE_EXPECT=1` once built and check only ids
   moved. The marker ids (root, fixup, no-downmap) keep their raw values, so
   `proc-macro-api`'s `OLD_FIXUP_AST_ID` and the proc-macro wire format are unchanged.

> applied: noted for the first compile: hir-ide's expect tests (the coordinator's) are rerun with `UPDATE_EXPECT=1` then, checking only ids moved (in the requests report).
2. **Persisted ast ids** (`hir-def/src/nameres/frozen.rs`, `hir-def/src/hir/persist.rs`,
   `load-cargo`; owners: format-hir, the coordinator): they write `ErasedFileAstId::into_raw`.
   Raw ids from before this change decode to other nodes. If any of these encodings can be read
   back across blaze versions without a version in their cache key, bump it; otherwise nothing.

> forwarded to db: `crates/hir-def/src/nameres/frozen.rs`.
> forwarded to cargo: `load-cargo`'s persisted ids.

> answered by lane cargo (cargo-inbox): item 2 needs nothing in `load-cargo`. Its raw ast ids (`crates/load-cargo/src/lib.rs:955-1018`, the proc-macro sub-requests) only cross the wire to a macro within one expansion of one blaze and are never persisted, so no encoding outlives a version.

## format-parse

1. **`OutlineFn` can go** — `crates/hir-def/src/item_tree/attrs.rs` (format-hir): format-hir's
   request 2 is done. `HasAttrs::inner_attributes_node` (`crates/syntax/src/ast/traits.rs`)
   now returns `None` for a `fn` whose body is lazy and not yet parsed when the body's text
   surely starts with no inner attribute (`block_may_start_with_inner_attr`, the same rule,
   with its tests). Remove `OutlineFn`, its `may_start_with_inner_attr` and
   `block_comment_len` and their tests, and call `ast::Fn`'s `HasAttrs` directly. Why: one
   copy of the rule, for every caller (#1294).

> forwarded to format: `OutlineFn` is still in `crates/hir-def/src/item_tree/attrs.rs:271` (format-hir's).
2. **No edit, already applied**: the lead's request 3 (drop `syntax::set_green_trees` from
   `crates/blaze-engine/src/lib.rs`) is done in the tree (`rg set_green_trees crates` finds
   nothing); `syntax::set_green_trees` no longer exists, so nothing may call it again.

> applied: note only.
3. **An expander returns its errors** — `crates/syntax-tree/src/flat.rs` (format-tree). Make
   `pub type Expand = fn(kind: SyntaxKind, text: &str, data: u32) -> (FlatTree, Vec<(TextRange, Box<str>)>)`,
   the ranges relative to `text`. Keep them with the part, moved to the file like its tree,
   and add `FlatTree::part_errors(&self, at: u32) -> &[(TextRange, Box<str>)]`: lazy node
   `at`'s errors, absolute, parsing the part if it isn't parsed yet. Why (performance): today
   `Expand` returns only the tree, so `Parse::errors_with_bodies` (and `errors()` for the
   bodies parsed so far) finds each body's parser errors by parsing the body a second time
   (`crates/syntax/src/parsing.rs`, `body_parser_errors`). With this each body is parsed once.
   When it lands, tell format-parse: `expand_lazy` returns its parser errors and
   `body_parser_errors` reads `part_errors`. Keep a part's errors in its `heap_size`.

> forwarded to format: format-tree's `crates/syntax-tree/`, inside lane format.
4. **Replace a token in place** — `crates/syntax-tree/src/flat.rs` (format-tree). Add
   `FlatTree::replace_token(&self, at: u32, text: &str, source: Arc<str>) -> FlatTree`: token
   `at` with the new `text` (same kind), over `source`, the new whole text, with every lazy
   node kept lazy and every part already parsed kept (those after `at` moved). Why
   (performance): the token reparse (`crates/syntax/src/parsing/reparsing.rs`, `Edit::token`)
   has to rebuild the token's parent from the arrays to call `replace_subtree`. For whitespace
   or a comment at a file's top level that parent is the root, so a one-character edit rebuilds
   the whole tree and drops every body parsed so far. When it lands, tell format-parse, which
   switches `Edit::token` to it.

> forwarded to format: format-tree's `crates/syntax-tree/`, inside lane format.

## format-tree

1. **Drop the green twin from an outline test** — `crates/syntax-bridge/src/tests.rs`
   (`an_outline_has_the_token_trees_of_a_full_parse`). Green trees are gone from
   `syntax-tree` (format-edit, decisions.md: rowan goes entirely), so `SyntaxNode::new_root`
   and `SyntaxNode::green` no longer exist. Delete line 197,
   `let green = SyntaxNode::new_root(full.green().to_owned());`, and line 222,
   `assert_eq!(token_tree(&green), expected, "{path}");`. Nothing else changes: the test still
   compares the outline with the full parse, which is what it is for (`SyntaxNode` stays
   imported: the test's `in_full` closure uses it). Why: without this, `syntax-bridge`'s
   tests don't build.
   Status: done by speed-span-tt
2. **`expand_lazy` returns its errors** — `crates/syntax/src/parsing.rs` (format-parse).
   format-parse's request 3 has landed in `syntax-tree`: `Expand` is now
   `fn(SyntaxKind, &str, u32) -> (FlatTree, Vec<(TextRange, Box<str>)>)`, and
   `FlatTree::part_errors(at)` gives a lazy node's errors, absolute. `syntax` doesn't build
   until `expand_lazy` matches. The least edit: make it return
   `(FlatTree, Vec<(TextRange, Box<str>)>)`, bind `errors` instead of `_errors` from
   `build_tree`, and end with
   `(tree, errors.into_iter().map(|it| (it.range(), it.to_string().into())).collect())`.
   The ranges must be relative to `text` (the part's own text), which is what `build_tree`
   gives over `Arc::from(text)`. The `ILLEGAL_FLOAT` error, whose position is a token count
   in the file and not an offset, is format-parse's call, as in `body_parser_errors`. Then
   `errors_of` can read `tree.part_errors(at)` (parsing the part if needed) in place of
   `body_parser_errors`.

> forwarded to format: `crates/syntax-bridge/` and `crates/syntax/` are lane format's, between its children.

## From concurrency: lock audit (2026-10-29)

Audited against `docs/v2/plan/design/concurrency.md` by `concurrency-audit-front` (read-only).
Lookups in both interners take no lock, which is right. The findings are on the write side.

1. **`crates/intern/src/arena.rs:26` (`SHARDS = 1 << 6`) and `:47` (`Shard.state: Mutex<State>`).**
   This is the store under every `Interned::new_gc` (hir-ty's `Ty`, `Const`, `Region`,
   predicates; `next_solver/ty.rs:84` and the others) and `Symbol::intern`. That's a hot write path during
   type checking. The shard count is fixed at 64, which breaks "a power of two ≥ 4× the core
   count, chosen at startup". Also, a `Vacant` holder grows the shard's table while holding the
   lock, copying every entry (`table_for_one_more`, :249-277), so the other inserters to that shard
   wait out an O(n) copy. Instead, size `Store.shards` when the store is first made (the stores are
   lazily initialized statics, so `available_parallelism() * 4`, next power of two, is known by
   then) and derive `SHARD_BITS` from that. With more shards each table is smaller, which also
   shortens the copy done under the lock.
   Status: done by speed-span (`crates/intern/src/arena.rs`: shards from the core count, the
   lock a named `InsertLock`; docs/v2/plan/lanes/format/speed-span.md).
2. **`crates/blaze-ids/src/lib.rs:210` (`SHARDS = 64`) and `:224` (`Shard.lock: Mutex<u32>`).**
   It has the same fixed count. On top of that, `Interner::intern` runs the caller's `value()` with
   the shard locked (:340): that's computing under a shard lock, where the rule is "held only to
   claim a slot". Every insert from every shard also writes two process-wide counters,
   `next.fetch_add` (:335) and `published.fetch_add` (:347), so all writers contend on one line.
   Instead: pick the count at startup as in 1. After the lock-free `find` misses, compute `value()`
   *before* locking, then lock only to probe again and insert, dropping the loser's value. Hand out
   indices from per-worker blocks (claim 64 at a time from `next`), and keep `published` per
   shard, summed by `len()`. There are no users outside the crate yet, so now is the cheap time
   to change it.
   Status: done by format (lead). `shard_count()`: `(cores * 4).next_power_of_two().max(4)`,
   chosen once; the shard is picked by the ID's first 4 bytes. `intern` computes `value()`
   before locking and locks only to probe again and insert (a racing loser's value is dropped
   after the lock is released), so a `value` may now intern into the same interner. Each shard
   claims indices `BLOCK` (64) at a time from `next` under its own lock; `len` is per shard
   (`#[repr(align(128))]` shards), summed by `len()`. New `Sharded::end()`: above every index
   handed out, by at most `BLOCK` per shard (size `Idx`-indexed columns by it, not by `len()`;
   no current user sizes by `len()`). `pub const SHARDS` is gone (no user outside the crate).

Judged off the hot path: `syntax-tree/src/flat.rs:1167` (`PARTS`, tests only). The format files
in `hir-def` (`expr_store*`, `hir*`, `item_tree*`) and `syntax`, `span`, `tt` and `mbe` hold no locks.

> forwarded to format: every finding is in lane format's own crates (`intern`, `blaze-ids`, `syntax-tree`, `span`); lane format is still running.
> applied: both, checked in the tree 2026-10-29 19:15. 1 by speed-span (`intern/src/arena.rs`: `shard_bits()` from `available_parallelism`, `InsertLock`). 2 by format (`blaze-ids/src/lib.rs`): `shard_count()` at startup, `value()` computed before the lock (`intern`, :452), indices claimed `BLOCK` at a time per shard, `len` per shard on 128-byte-aligned shards, `find` with no lock, write or allocation. Under the lock remain only the re-probe, a table doubling (amortised O(1) per insert, smaller with 4× cores shards; readers keep probing the old table meanwhile) and the one-time allocation of a slot chunk.

## format-hir: expressions and patterns become columns (to lanes db, items, codegen)

From format-hir, 2026-10-29. Per the lead's addendum and `docs/v2/plan/design/data.md`, the
`ExpressionStore`'s expressions and patterns are now a `u8` kind and a `u32` data column each
(at most 12 bytes per expression), with child lists as CSR ranges and cold values in side
tables. There is no `Arena<Expr>` any more, so `impl Index<ExprId>` / `Index<PatId>` for
`ExpressionStore` are gone. Readers get a borrowed view, `Expr<'a>` / `Pat<'a>`, with today's
variant and field names. Full contract: `~/restart-2026-10-28/briefs/v2/format-hir-columns-api.md`.
About 290 sites by a rough count: hir-ty 72, hir-ty-mir 89, hir-ty-borrowck 87, hir 35,
hir-ty-codegen 6, and 3 in hir-def outside format's paths (`resolver.rs`, `upvars.rs`, ...).

The rule, applied mechanically:
1. For an expression, `store[e]`, `&store[e]`, `body[e]` and `self.body[e]` become
   `store.expr(e)` (`body.expr(e)` through `Deref`). For a pattern, `store.pat(p)`.
2. Fields bind by value for ids, `Option`s of ids and `Copy` enums: `*callee` becomes
   `callee`, and `Some(*e)` becomes `Some(e)`. List fields are `&[T]` as before (`args.iter()`,
   `args.len()` and `&args[..]` are unchanged). `Name`, `Path`, `Literal`, `OffsetOf` and
   `InlineAsm` fields are `&T` as before. `generic_args.as_deref()` becomes `generic_args`,
   which is now an `Option<&GenericArgs>`.
3. A function taking `&Expr` / `&Pat` takes `Expr<'_>` / `Pat<'_>`. `store.exprs()` yields
   `(ExprId, Expr<'_>)`. `Idx<Expr>` written out becomes `ExprId`.
4. Nothing outside hir-def builds an `Expr` or a `Pat`.

Examples from `crates/hir-ty/src/infer/expr.rs`:

```rust
// before (line 336)
let expr = &self.store[tgt_expr];
// after
let expr = self.store.expr(tgt_expr);
```

```rust
// before (line 446)
Expr::Call { callee, args, .. } => self.infer_call(tgt_expr, *callee, args, expected),
// after
Expr::Call { callee, args, .. } => self.infer_call(tgt_expr, callee, args, expected),
```

```rust
// before (line 447)
Expr::MethodCall { receiver, args, method_name, generic_args } => self
    .infer_method_call(tgt_expr, *receiver, args, method_name, generic_args.as_deref(), expected),
// after
Expr::MethodCall { receiver, args, method_name, generic_args } => self
    .infer_method_call(tgt_expr, receiver, args, method_name, generic_args, expected),
```

Owners: lane db (`hir-ty`, `hir-ty-borrowck`, and hir-def's `resolver.rs`/`upvars.rs`), lane
items (`hir`), lane codegen (`hir-ty-mir`, `hir-ty-codegen`). These land together with the
format-hir change. The item tree's own migration follows under "## format-hir-tree".

> forwarded to db and codegen: `hir-ty`, `hir-ty-borrowck`, `hir-def`'s `resolver.rs`/`upvars.rs` and `hir` are lane db's (crates.md gives `crates/hir/` to db, not items); `hir-ty-mir` and `hir-ty-codegen` are codegen's. Lane items is DONE, so its part is `requests`': items' `hir-ty` files have no `store[e]` site; `blaze-items`' four `exprs()` loops (`key.rs:887`, `tests/{form,hashes,facts}.rs`) drop `*closure_kind` for `closure_kind` once `ExpressionStore::exprs` yields `Expr<'_>` (it still yields `&Expr` from the arena).

- `resolver.rs`: done by speed-hir-nameres-paths. It has no `ExprId`/`PatId` index site: its
  store reads are `impl_data.store[target_trait.path]` (a `PathId`) and
  `ExpressionStore::of(db, owner)[label]` (a `LabelId`), which keep `Index` under the contract,
  so nothing changes.

## speed-types: `ContentId::of_many` and a context key per domain (`crates/blaze-ids/src/lib.rs`)

From `speed-types` (blaze-items' speed pass). `blaze-items` already calls
`ContentId::of_many` (`key.rs` `hash_all`, `index.rs` `name_hashes`, `form.rs`
`Form::finish_many`), so **it does not build until this lands**. The request was in
`v2/requests/items.md` and `store.md` (5) with "forwarded to format", but never here.

1. **`ContentId::of_many(domain: &'static str, inputs: &[&[u8]]) -> Vec<ContentId>`**, equal to
   `inputs.iter().map(|it| ContentId::of(domain, it)).collect()` on every CPU path. The first
   version can be exactly that map over one shared context key (2 below), which already
   removes one compression per input. The SIMD version hashes up to 16 inputs per pass
   (AVX-512; 8 AVX2, 4 NEON/SSE4.1). `blake3::platform::Platform::hash_many` needs every
   input to be the same whole number of blocks, and items' inputs are 40 to 200 bytes of
   different lengths, so it needs a per-lane block length: group by block count, run the
   full blocks through `hash_many`, and finish each lane's last block with its own length. A
   differential test against `ContentId::of` per input, over the empty input, 1, 63, 64, 65,
   1024 and 1025 bytes, mixed in one call.
2. **One context key per domain, not one `new_derive_key` per `Hasher::new`.** `blake3` 1.8.7
   (our lock) has `blake3::hazmat::hash_derive_key_context(context) -> ContextKey` and
   `blake3::hazmat::HasherExt::new_from_context_key(&ContextKey)`. Together they equal
   `Hasher::new_derive_key(context)` bit for bit, with the context's compression done once.
   - `Hasher::new(domain)` looks the key up in a table keyed by the `&'static str`'s address:
     an append-only, open-addressed array of `(ptr, len, key)` slots published with one
     release store (concurrency.md: no lock on a read). Then it calls `new_from_context_key`.
   - Better still, a `pub struct Domain { name: &'static str, key: OnceLock<ContextKey> }`
     made by `const fn Domain::new(name)`, with `Hasher::of(&'static Domain)` and
     `ContentId::of_domain`. Callers' `pub const X_DOMAIN: &str` would become
     `pub static X: Domain`, so the key is one acquire load. Items would move to it in the
     same change.
   - Why: items makes millions of hashes of 40 to 200 bytes (a key per item, two per form, one
     per fact, component and name). Each costs two to four compressions today, and the
     derive-key one is 25 to 50% of the total. A test: `Hasher::new(d)` equals
     `blake3::Hasher::new_derive_key(d)` over several domains.

> forwarded to format: `crates/blaze-ids/src/lib.rs` is lane format's; lane format is still running. `blaze-items` (key.rs, index.rs, form.rs) does not build until `of_many` lands.

Status: done by format (lead), in `crates/blaze-ids/src/lib.rs`:
- `ContentId::of_many(domain: &'static str, inputs: &[&[u8]]) -> Vec<ContentId>`
  (`crates/blaze-ids/src/many.rs`), from the domain's context key, looked up once:
  - **Inputs of at most 1 KiB:** grouped by block count. Their full blocks go through
    `blake3::platform`'s `hash_many` (16 lanes on AVX-512, 8 on AVX2, 4 on NEON).
  - **Each input's last block, any length:** through `compress_lanes`, eight inputs at a time.
    It is a transposed BLAKE3 compression with a block length per lane, which `hash_many`
    can't give. It is safe code that LLVM vectorizes, so items' 40–64-byte inputs are SIMD
    end to end.
  - **Longer inputs:** through the hasher.
  - **Tests:** `of_many_is_the_hasher_at_every_length_and_group_size` holds both to
    `blake3::Hasher` at every length from 0 to 1100 bytes and in batches of 1 to 33 inputs.
    So a blake3 bump that changes `platform` (which blake3 marks unstable) fails there, not in
    an ID.
- `Hasher::new(domain)` reads the key from a 1024-slot open-addressed table keyed by the name's
  text (not its address: a `const &str` may sit at several addresses), each slot a `OnceLock`
  (a read is acquire loads; more than 1024 names panics, naming the constant).
- `pub struct Domain`, `const fn Domain::new(name)`, `Domain::name()`, `Hasher::of(&Domain)`,
  `ContentId::of_domain(&Domain, bytes)`: the key in the `Domain` itself, one acquire load.
  Items may move its hot domains to `pub static X: Domain`; nothing requires it.
- Tests: `Hasher::new(d)` equals `blake3::Hasher::new_derive_key(d)` over several domains and
  lengths, `of_many` equals `of` per input over 0, 1, 63, 64, 65, 1024, 1025 and 3000 bytes
  mixed, 600 domains from 4 threads, the same name at two addresses.

## speed-syntax (speed pass of `crates/parser`, `crates/syntax`)

From `speed-syntax`, for format-tree (`crates/syntax-tree/` is live as format-edit). Notes:
`docs/v2/plan/lanes/format/speed-syntax.md`.

Already wired in `crates/syntax` (format-parse is done, its files are speed-syntax's now):
format-tree's request 2 (`expand_lazy` returns its parser errors; `Parse::errors_of` reads
`FlatTree::part_errors` through `parsing::body_errors`, so each body is parsed once and
`body_parser_errors` is gone) and the follow-up of format-parse's request 4 (`Edit::token` calls
`FlatTree::replace_token`).

1. **A sourced token is checked by address, not by its bytes** — `crates/syntax-tree/src/flat.rs`,
   `FlatTreeBuilder::token`. Today a sourced builder compares every token's bytes with the
   source (`source.as_bytes().get(self.next..end) == Some(text.as_bytes())`): a second pass over
   every byte of every file parsed, and the check `parsing::build_tree` can never fail, since its
   tokens are slices of the source it gives the builder. Exact change: accept at once when
   `text.as_ptr() == source.as_ptr().wrapping_add(self.next) && end <= source.len()`, and compare
   the bytes only otherwise (a token copied from another text, as a test builder does); keep the
   panic message. The check is as strong (same address and length is the same bytes) and costs
   two compares per token. Same for `FlatTree::expand`'s check of the part
   (`part.source.get(..) == Some(text)`): with 2 below it is `Arc::ptr_eq(&part.source,
   &self.source) && root == range`, no compare of the body's text.
2. **A part built over the file's own text** — `flat.rs`. Today `expand_lazy` must build each
   body over `Arc::from(text)`: one allocation and a copy of the body per body parsed, then
   `moved()` and the compare above. Exact change: `pub type Expand = fn(kind: SyntaxKind,
   source: &Arc<str>, range: TextRange, data: u32) -> (FlatTree, Vec<(TextRange, Box<str>)>)`,
   errors absolute; `FlatTreeBuilder::sourced_range(source: Arc<str>, range: TextRange)`, a
   sourced builder whose tokens start at `range.start()` (`next = start`, the first mark
   `start`) and whose `finish` checks `next == range.end()` (today `== source.len()`); `expand`
   then calls `(lazy.expand)(kind, &self.source, range, lazy.data)` and keeps the tree as it is,
   with no `moved`. Tell speed-syntax (window `speed-syntax`) when it lands: `expand_lazy`
   moves to `sourced_range` with `LexedStr::new(edition, &source[range])` and absolute errors,
   and the reparse's block, item and body candidates (`reparsing.rs`, `Arc::from(text)` three
   times) build over `new_source` the same way, which removes one copy of the edited node per
   candidate tried.

> applied (format-tree, 2026-10-29): both, as written.
> - 1: a token at the source's next address with `end <= ` the builder's end is accepted
>   without a compare; the message is kept.
> - 2: `Expand` is `fn(SyntaxKind, &Arc<str>, TextRange, u32) -> (FlatTree, Vec<(TextRange,
>   Box<str>)>)`, its errors absolute and checked to lie in the range.
>   `FlatTreeBuilder::sourced_range(source, range)` checks the range is on char boundaries,
>   keeps tokens and lazy nodes inside it, and `finish` checks `next == range.end()`.
>   `sourced` is now `sourced_range` over the whole source. `expand` checks
>   `Arc::ptr_eq(&part.source, &self.source) && root == range` and keeps the part as built,
>   with no `moved`.
> - `syntax` doesn't build until `expand_lazy` takes the new signature (`source[range]` for
>   the lexer, `sourced_range` for the builder). speed-syntax is told.

The lock audit's only note on these files (`flat.rs` `PARTS`) is in tests: nothing to apply.

## speed-hir-expand: span lookups per token, span-map layout, the dropper thread (`crates/span/src/map.rs`, `crates/syntax-bridge/src/lib.rs`)

To `speed-span-ast` (`span`) and `speed-span-tt` (`syntax-bridge`). hir-expand is the caller of all
of these (`crates/hir-expand/src/span_map.rs`, `lib.rs`); I fit my side to whatever you pick, tell
me (`tmux send-keys -t blaze-v2:speed-hir-expand`).

1. **A cursor for in-order lookups.** `syntax_node_to_token_tree*` asks `SpanMapper::span_for` for
   every token of every macro input, in increasing offset order. Both maps answer with a binary
   search per token (`SpanMap::span_at` :123, `RealSpanMap::span_for_range` :359 via `index_at`
   :332, plus `end()` and an `assert!` per call). Wanted, giving exactly the same `Span`s:
   `SpanMap::span_at_from(&self, offset: TextSize, hint: &mut u32) -> Span` and
   `RealSpanMap::span_for_range_from(&self, range: TextRange, hint: &mut u32) -> Span`, which
   start from the entry `hint` names, step forward while the next entry still covers the offset
   (galloping 1, 2, 4, ... then a binary search in the last step, so a jump back or far ahead is
   still `O(log n)`), and store the entry they found in `hint`. Amortized `O(1)` per token
   instead of `O(log n)`, and the entries read are adjacent (one cache line per few tokens instead
   of `log n` scattered ones). Keep `span_at` / `span_for_range` as they are for random lookups.
   hir-expand will then hold a `Cell<u32>` in its mappers (`SpanMap<'_>`, `MacroRulesArgs`), one per
   conversion, so `SpanMapper::span_for(&self, ..)` can stay `&self`; or make it `&mut self` if
   you prefer, and say so.
2. **`SpanMap` as two columns.** Today `spans: Vec<(TextSize, Span)>` (24 bytes an entry: 4 for
   the offset, 20 for the span, with padding). As `ends: Vec<TextSize>` beside `spans: Vec<Span>`, the
   search reads 4-byte offsets only (16 per cache line instead of 2 or 3). `Hash` (salsa
   fingerprints `parse_macro_expansion`'s output, lru = 512, so it is hashed on every execution)
   becomes two slice hashes instead of a derived `Hash` per tuple. Also `with_capacity(tokens)`
   from `token_tree_to_syntax_node`, which knows the token count, instead of growing by `push`.
3. **The dropper thread** (`impl Drop for SpanMap`, :217): each dropped map sends its `Vec`
   through one process-wide `mpsc` channel (a lock per drop) to an OS thread that sleeps 100 ms in
   a loop. Under v2 eviction and quiescent reclamation free memos (concurrency.md, "Memory"), so
   drop the `Drop` impl and the thread; a dropped map frees its columns where it drops.
4. **Down-mapping index.** `ranges_with_span_exact` / `ranges_with_span` (:90, :106) scan every
   entry; `hir`'s `Semantics` descend calls them per token it maps down
   (`crates/hir/src/semantics.rs:1010,1265` through `ExpansionInfo::map_range_down*`). A sorted
   permutation `by_anchor: Vec<u32>` (entry indices ordered by `(anchor, range.start)`) built in
   `finish()` makes each a binary search plus the matching run. Lower priority than 1-3.

> forwarded to format (`span`, `syntax-bridge`) and to speed (speed-span-ast, speed-span-tt), both still running.

Status (syntax-bridge side) done by speed-span-tt. `SpanMapper::span_for` stays `&self`: hold the
`Cell<u32>` hint in hir-expand's mappers, one per conversion. Within one
`syntax_node_to_token_tree*` call, `span_for` and `fragment_bounds` see ranges in increasing
start order: one call per token, one per char of a multi-char punct, and two for a lifetime, the
`'` then the rest. A doc comment's span and a closing delimiter's span are asked at that token's
own range. Leaves that `on_enter`/`append` inject are the exception. Only the `_modified` entry
point takes them, and each comes with the empty range at 0: `fragment_bounds` is asked about that
range, `span_for` is not. A hint survives the jump back, because any hint gives the same answer.
Apart from that, `span_at_from` and
`span_for_range_from` give amortized two compares per token. For item 2, `token_tree_to_syntax_node`
now builds its map with `SpanMap::with_capacity(parser_input.len())`.

Status (span side) done by speed-span-ast, checked by speed-span (`crates/span/src/map.rs`):
1. `SpanMap::span_at_from(offset, &mut u32) -> Span` and
   `RealSpanMap::span_for_range_from(range, &mut u32) -> Span` (salsa feature, as
   `span_for_range`): a galloping search from the hint, the same `Span` as `span_at` /
   `span_for_range`, which stay. `&self`, hint a `Cell<u32>` per mapper starting at 0 (as
   `hir-expand/src/span_map.rs` and `eager.rs` already do).
2. `SpanMap` is `ends: Vec<TextSize>` beside `spans: Vec<Span>`; `Hash` is two slice hashes;
   `SpanMap::with_capacity(n)`.
3. `impl Drop` and the dropper thread are gone, from `SpanMap` and from `AstIdMap`.
4. `by_anchor`: a lazy `OnceLock<Box<[u32]>>` permutation by `(anchor, range)`, reset by `push`
   and `merge` instead of built in `finish()`; `ranges_with_span_exact` / `ranges_with_span` are
   binary searches plus the run, same results in the same text order. hir-expand's doc comments
   calling them a linear search (`lib.rs` near :1333 and :1354) are now out of date.

## format-hir-encode: untrusted salsa indices in HIR buffers (docs/v2/plan/design/unsafe.md)

From format-hir-encode, for the lead (`crates/blaze-format/`) and format-span (`crates/span/`).

HIR buffers (`expr_store/persist.rs`, `item_tree/persist.rs`, `hir/persist.rs`) store
`SyntaxContext`s as their `into_u32` value and rebuild them with `unsafe
SyntaxContext::from_u32`. That function's precondition is "a valid `SyntaxContext`": a root, or
a salsa index this database interned. Bounds checks can't prove that. Today the `// SAFETY:`
comment rests on "the database that wrote the buffer is the one reading it". A corrupted or
foreign buffer breaks that, and `SyntaxContext::id` then builds a `SyntaxContextImpl` from an
index salsa never made (span/src/hygiene.rs:92). unsafe.md says untrusted bytes are verified
before any view is made of them. Two ways to close it; either is enough:

1. **The lead, `blaze-format`:** make hash verification part of opening a buffer. Add
   `pub fn open_verified<R: Record>(buffer: &[u8], schema: SchemaHash, key: ContentId) ->
   Result<&R, FormatError>`, which checks `ContentId::of(.., buffer) == key` before `open_as`, and
   a `FormatError::Hash` variant. `Persist::read` documents that its caller verified the bytes;
   the daemon image and the store only call `read` on verified bytes. Then the HIR `SAFETY:`
   comments can name that check as where the invariant is upheld.
2. **format-span, `span`:** a safe, checked constructor, `pub fn
   SyntaxContext::from_u32_in(db: &dyn salsa::Database, raw: u32) -> Option<SyntaxContext>`.
   It returns `None` unless `raw` is a root id or an index the interned `SyntaxContextImpl`
   table holds. Then the decoders take the database and need no `unsafe` at all.

Until one lands, the only `unsafe` left for this is the single function
`hir::persist::syntax_context` (format-hir's file; edit given in
/tmp/blaze-lanes/v2-format-hir-encode.md).

Status: 1 done by format (lead), in `crates/blaze-format/src/lib.rs`: `BUFFER_DOMAIN`
(`"blaze v2 buffer 1"`), `buffer_id(buffer) -> ContentId` (the whole buffer, header included),
`open_verified::<R>(buffer, schema, key)`, `read_verified::<P>(buffer, key)` and
`FormatError::Hash { found, wanted }`; `Persist::read`'s doc says bytes this process did not
write go through `read_verified`. For lanes store and db (they read buffers back): key stored
HIR buffers by `blaze_format::buffer_id` and open them with `read_verified`. The `SAFETY:`
comment in `hir::persist::syntax_context` can now name `read_verified` as where the invariant
is upheld (format-hir told).

> forwarded to format: `crates/blaze-format/` and `crates/span/` are lane format's.
> 1 is done (above). 2 forwarded by format to speed-span, which owns `crates/span` now (`v2/requests/speed.md`, "requests forwarded to format that are in your crates", 4).

## speed-span-tt: an in-order token walk of a flat tree (`crates/syntax-tree/src/flat.rs`)

From `speed-span-tt`, for format-tree (live as format-edit). Notes:
`docs/v2/plan/lanes/format/speed-span-tt.md`.

`syntax_node_to_token_tree` on a flat node, through syntax-bridge's `FlatTokens`, reads every
token of a macro input in order through `FlatTree::range(at)`. `range` computes `offset(at)` from
the last mark, which is up to `MARK` (8) `text_len` steps per token. That is the bridge's only
per-token cost above O(1). Before this change it called `range` and then `token_text`, so the
cost was paid twice; now it is paid once, with the text sliced from `text()`.

1. **`FlatTree::tokens_in(&self, at: u32) -> impl Iterator<Item = (u32, TextRange)>`**: the tokens
   and lazy nodes from `at` to `end(at)` in order, each with its range. It starts from
   `offset(at)` once, then adds `text_len` as it goes, so each step is O(1). A lazy node gets its
   range, and its text is skipped. Same ranges as `range(it)` for each `it`. Or make `text_len`
   public, and the bridge keeps the running offset itself. Tell `speed-span-tt` or whoever owns
   `crates/syntax-bridge/` then; `FlatTokens::next` is the only caller to change.

> forwarded to format: `crates/syntax-tree/` is lane format's.

## speed-hir-store-readers: the lazy nodes as a slice (`crates/syntax-tree/src/flat.rs`)

From `speed-hir-store-readers` (owns `crates/hir-def/src/expr_store/body.rs`), for whoever owns
`crates/syntax-tree/` (speed-syntax / format-tree).

`body_syntax` asks, per anchor of a file, whether its subtree holds a lazy node of an outline
(`holds_lazy` in `expr_store/body.rs`): `tree.lazies().find(|&it| it >= at)`. `lazies()` is an
iterator over `Lazy::nodes` (sorted, preorder), so each anchor scans every lazy node before it:
O(anchors x bodies) for every edit of a dependency's file, where a binary search is O(log bodies).

1. **`FlatTree::lazy_nodes(&self) -> &[u32]`**: the lazy nodes in preorder, sorted ascending
   (`self.lazy.as_ref().map_or(&[], |it| &it.nodes)`). `lazies()` can stay as
   `self.lazy_nodes().iter().copied()`.

Once it exists, `holds_lazy` becomes (I make the edit; tell `speed-hir-store-readers` or
`speed-hir-store`):
```rust
let lazies = tree.lazy_nodes();
lazies.get(lazies.partition_point(|&it| it < at)).is_some_and(|&it| it < tree.end(at))
```

> applied (speed-syntax, 10-29): `FlatTree::lazy_nodes(&self) -> &[u32]` in
> `crates/syntax-tree/src/flat.rs`, ascending preorder, empty without lazy nodes; `lazies()` is
> now `self.lazy_nodes().iter().copied()`. Not compiled (no builds in this lane).

## format-hir-tree

From format-hir-tree (a child of format-hir), 2026-10-29. Not compiled (this lane runs no
builds). Per `docs/v2/plan/design/data.md` and the lead's addendum, the `ItemTree`
(`crates/hir-def/src/item_tree.rs`) is now columns:
- An item is a dense `ModItemId` into three per-item columns: kind (`u8`), data word (`u32`) and
  `FileAstId` (`u32`). That is `ITEM_BYTES = 9` per item, const-asserted.
- Each kind's payload is a flat column of plain `u32` records (at most 16 bytes: `Struct` and
  `Mod`).
- Lists are `IdRange`s: inline module and extern block items, a `use` tree's children, a tuple
  struct's constructor visibilities.
- Names and paths are per-tree ids (`NameId`, `PathId`). Aliases are `AliasId`, use trees
  `UseTreeId`.
- Attributes are one flat array, one `IdRange` per item, read as `AttrsOrCfgRef<'_>`.

`ItemTree::index(id)` still gives `ModItemKind::X(ast_id, &X)`. What changed is what the record
fields hold. These sites are lanes db's and items' (`nameres/`, `src.rs`); every one is in
`crates/hir-def/src/nameres/collector.rs` except the last.

The rule:
1. A record's `name` is a `NameId`. `&it.name` becomes `&item_tree[it.name]` (or
   `&self.item_tree[it.name]`). `it.name.clone()` becomes `item_tree[it.name].clone()`.
   `Const::name` is `NameId::NONE` for `const _`; read it as `item_tree.get_name(it.name)`, an
   `Option<&Name>`.
2. `MacroCall::path` is a `PathId`, so `&item_tree[path]` is the `Interned<ModPath>`.
   `ExternCrate::alias` is an `AliasId`, so `item_tree.import_alias(alias)` is the
   `Option<ImportAlias>`.
3. Lists come from the tree:
   - `&block.children` becomes `item_tree.items(block.children)`.
   - `match &module.kind` becomes `match item_tree.mod_kind(module)`. That gives
     `ModKind::Inline { items: &[ModItemId] }` or `ModKind::Outline`.
   - `match &it.value_ns_ctor` becomes `match item_tree.value_ns_ctor(it)`. The variants keep
     their names: `ValueNsCtorWithVis(&RawVisibilityId)`, `ValueNsCtorWithMinVis(&[RawVisibilityId])`.
4. Attributes: `item_tree.attrs(id)` and `top_level_attrs()` give `Option<AttrsOrCfgRef<'_>>`,
   a `Copy` enum with two variants:
   - `Enabled { attrs: Attrs<'_> }`;
   - `CfgDisabled(&CfgExpr, Attrs<'_>)`.

   So `attrs.as_ref()` becomes `attrs`, `cfg.0` / `&cfg.0` becomes `cfg`, and `it.1.as_ref()`
   becomes the second field. `AttrsOrCfg::lower` (owned, used by `nameres/assoc.rs`) is
   unchanged.
5. `Use::expand(&self, cb)` becomes `Use::expand(&self, tree: &ItemTree, cb)`.
   `lower_use_tree(..)` returns `Option<Arena<ast::UseTree>>`, the source map alone.

Examples:

```rust
// before (collector.rs 2962)
update_def(self.def_collector, fn_id.into(), &it.name, vis, None);
// after
update_def(self.def_collector, fn_id.into(), &item_tree[it.name], vis, None);
```

```rust
// before (collector.rs 2815-2822)
Some(AttrsOrCfg::Enabled { attrs }) => attrs.as_ref(),
None => Attrs::EMPTY,
Some(AttrsOrCfg::CfgDisabled(cfg)) => {
    let ast_id = item_id.ast_id(self.item_tree).erase();
    self.emit_unconfigured_diagnostic(InFile::new(self.file_id(), ast_id), &cfg.0);
// after
Some(AttrsOrCfgRef::Enabled { attrs }) => attrs,
None => Attrs::EMPTY,
Some(AttrsOrCfgRef::CfgDisabled(cfg, _)) => {
    let ast_id = item_id.ast_id(self.item_tree).erase();
    self.emit_unconfigured_diagnostic(InFile::new(self.file_id(), ast_id), cfg);
```

```rust
// before (collector.rs 2936)
self.collect(&block.children, ItemContainerId::ExternBlockId(extern_block_id))
// after
self.collect(item_tree.items(block.children), ItemContainerId::ExternBlockId(extern_block_id))
```

Exact edits, `crates/hir-def/src/nameres/collector.rs` (line numbers as of this note):
- 44: in the `item_tree::{..}` import, `AttrsOrCfg` becomes `AttrsOrCfgRef`.
- 190: `item.expand(|idx, path, kind, alias| {` becomes
  `item.expand(tree, |idx, path, kind, alias| {`.
- 858-862 (`seed_with_top_level`):
  ```rust
  let attrs = match item_tree.top_level_attrs() {
      Some(AttrsOrCfgRef::Enabled { attrs }) => attrs,
      None => Attrs::EMPTY,
      Some(AttrsOrCfgRef::CfgDisabled(_, attrs)) => attrs,
  };
  ```
- 956-957: the condition becomes
  `if let Some(AttrsOrCfgRef::CfgDisabled(cfg_expr, _)) = item_tree.top_level_attrs() {`.
  Delete the next line, `let (cfg_expr, _) = &**attrs;` (`cfg_expr.clone()` at 961 stays).
- 972: `ModItemKind::Mod(_, module) if module.name.as_str() == name)` becomes
  `ModItemKind::Mod(_, module) if item_tree[module.name].as_str() == name)`.
- 998: `Some(AttrsOrCfg::Enabled { .. }) | None` becomes `Some(AttrsOrCfgRef::Enabled { .. }) | None`.
- 2815-2822: as in the second example above.
- 2852, after `let item_tree::ExternCrate { name, visibility, alias } = item;`, add:
  ```rust
  let name = &item_tree[*name];
  let alias = item_tree.import_alias(*alias);
  ```
  Then 2874 `let name = match alias {` becomes `let name = match &alias {`. The arms and the
  uses of `name` after it stay as they are: `*name == sym::self_`,
  `.get(name)`, `name.cloned()`.
- 2936: as in the third example above.
- `&it.name` becomes `&item_tree[it.name]` at 2962 and 2967 (`parse_proc_macro_decl`), 3010,
  3029, 3046, 3074, 3086 and 3098.
- 2982: `match &it.value_ns_ctor {` becomes `match item_tree.value_ns_ctor(it) {`. The arms stay.
- 3053: `match &it.name {` becomes `match item_tree.get_name(it.name) {`. The arms stay.
- `collect_module` (3158-3258):
  - At the top, add `let item_tree = self.item_tree; let name = &item_tree[module.name];`.
  - 3166: `match &module.kind {` becomes `match item_tree.mod_kind(module) {`.
  - `module.name.clone()` becomes `name.clone()` at 3170, 3223 and 3247.
  - `&module.name` becomes `name` at 3177 and 3206.
  - 3215-3218: `Some(AttrsOrCfgRef::CfgDisabled(cfg, _)) => {`, then pass `cfg` for `&cfg.0`.
  - 3221: `Some(AttrsOrCfgRef::Enabled { .. }) | None => {`.
  - 3238: `matches!(top_level_attrs, Some(AttrsOrCfgRef::Enabled { attrs }) if attrs.by_key(sym::macro_use).exists())`.
- `collect_macro_rules`: 3417 `None => &mac.name,` becomes `None => &self.item_tree[mac.name],`.
  3461 `mac.name.clone(),` becomes `self.item_tree[mac.name].clone(),`.
- `collect_macro_def`:
  - At the top (after `let krate = ..`), add
    `let item_tree = self.item_tree; let mac_name = &item_tree[mac.name];`.
  - `&mac.name` becomes `mac_name` at 3480, 3485 and 3504.
  - 3495: `name == mac.name` becomes `name == *mac_name`.
  - 3497: `mac.name.display(..)` becomes `mac_name.display(..)`.
  - 3532: `mac.name.clone()` becomes `mac_name.clone()`.
- `collect_macro_call`: 3552 `let &MacroCall { ref path, expand_to, ctxt } = macro_call;`
  becomes:
  ```rust
  let &MacroCall { path, expand_to, ctxt } = macro_call;
  let path = &self.item_tree[path];
  ```
  The `path.clone()` at 3553 is unchanged and still gives an `Interned<ModPath>`.

Status (`nameres/collector.rs`, every edit above): done by speed-hir-nameres-fixpoint. `src.rs` is not
its path.

`crates/hir-def/src/src.rs` 59-63 (`use_tree_source_map`): delete the `.1` after
`.expect("failed to lower use tree")`, because `lower_use_tree` returns the arena itself.

One more, for whoever owns `crates/span`: `FileAstId::downcast_unchecked` debug-asserts the
id's kind, and `ErasedFileAstId::kind_typed` / `ErasedFileAstIdKind::syntax_kind` are private.
So a decoded item tree can't check that each item's ast id has its kind's syntax kind, and a
corrupted buffer panics in debug builds at `ItemTree::index` where it should be refused.
Wanted: `pub fn ErasedFileAstId::may_be<N: AstIdNode>(self) -> bool`, the same test the
debug assertion makes. `item_tree/persist.rs`'s `check` would then call it per item (I'd make
that edit).

> forwarded to format: between its own children.

## format-hir: type refs are columns too (addition to the rule above; to lanes db, items)

From format-hir, 2026-10-29. `hir/type_ref.rs` keeps a store's type refs as columns
(`TypeColumns`: a u8 kind and a u32 word each, 5 bytes against a budget of 12, const-asserted).
`TypeRef<'a>` is a view, `TypeRefId = Idx<TypeRef<'static>>`, and `Index<TypeRefId>` for
`ExpressionStore` is gone. The same rule applies:
1. `store[t]` / `&store[t]` becomes `store.type_ref(t)`. `store.types.iter()` (or an
   iteration over every type ref) becomes `store.type_refs()`, yielding
   `(TypeRefId, TypeRef<'_>)`.
2. Fields of the view: `Tuple(&[TypeRefId])`, `ImplTrait(&[TypeBound])` and
   `DynTrait(&[TypeBound])` are slices as before. `Path(&Path)`, `Reference(&RefType)`,
   `Array(&ArrayType)` and `Fn(&FnType)` are references (an earlier `&**fn_type` becomes
   `fn_type`). `RawPtr(TypeRefId, Mutability)`, `Slice(TypeRefId)`,
   `TypeParam(TypeParamId)` and `PatternType(TypeRefId, PatId)` bind by value
   (`*inner` becomes `inner`).
3. A function taking `&TypeRef` takes `TypeRef<'_>`.

Sites, about 73: hir-ty `lower.rs` 27, `lower/path.rs` 4, `generics.rs` 2 (lane db);
hir-ty-borrowck `regions.rs` 30 (lane db); hir `display.rs` 6, `diagnostics.rs` 2,
`semantics.rs` 1, `lib.rs` 1 (lane items). These land with the expression change.

> applied (hir-ide): `display.rs`'s five type-ref reads (`TypeRefId::hir_fmt`, its where-clause and return-type checks, the paren-sugar tuple and return) are `store.type_ref(t)` with view fields by value. forwarded to db: the hir-ty, hir-ty-borrowck and `hir` sites (`crates/hir/` is lane db's in `crates.md`, not items').

## From safety-audit

From `safety-audit-front` (child of `safety-audit`, lane `safety`). The workspace is about to deny
`unsafe_op_in_unsafe_fn`, `clippy::undocumented_unsafe_blocks` and
`clippy::multiple_unsafe_ops_per_block`. Every file below was BUSY, so these are for you to paste.
Comments, `# Safety` sections, block splits and crate-root attributes only: no behavior change.
A split keeps the order of evaluation and binds only `Copy` values. Line numbers are as of
2026-10-29 18:45.

> forwarded to format: every crate named is its.

### intern

Status: done by speed-span for `arena.rs`, `intern.rs` and `intern_slice.rs` (as proposed;
`arena.rs:135`'s `Table::new` is now normal-build only, beside a model-build twin). The two
`symbol.rs` items are adapted to its current `Text` and handed to fix-orderkeys, which is
editing that file (`v2/requests/speed.md`, "speed-span" item 2). `mbe` now has
`#![forbid(unsafe_code)]` (listed under "Left for lanes" in the audit report).

> done by speed-unsafe-front-intern (2026-10-30): the `symbol.rs` items are moot. `Symbol` is a
> table ID now, with one commented block (`from_utf8_unchecked`, in its `mod text`). All of
> `intern`'s other unsafe is in `space` and `typed`. Those two are `#[allow(unsafe_code)]` modules
> under a `#![deny(unsafe_code)]` root, with one operation per block.

- `crates/intern/src/arena.rs:135` block (`Table::new`): 2 ops (two `zeroed` calls). Split:
  ```rust
  // SAFETY: all-zero bytes are a `Group`: a zero control word (every slot empty) and null pointers.
  let slots: Box<[Group]> = unsafe { zeroed(groups) };
  // SAFETY: all-zero bytes are an `AtomicU64` of 0.
  let hashes: Box<[AtomicU64]> = unsafe { zeroed(groups * GROUP) };
  Table { groups: slots, hashes }
  ```
- `crates/intern/src/arena.rs:412` block (`Bump::alloc`): 3 ops (`cursor.add`, `at.add`, `NonNull::new_unchecked`). Split:
  ```rust
  // SAFETY: `pad + size <= left`, the bytes of the current chunk from `cursor`, so `cursor + pad` is in the chunk.
  let at = unsafe { self.cursor.add(pad) };
  // SAFETY: as for `at`: `at + size` is at most one past the chunk's last byte.
  self.cursor = unsafe { at.add(size) };
  self.left -= pad + size;
  // SAFETY: `at` is inside a chunk from `chunk`, an allocation `own` never returns null.
  unsafe { NonNull::new_unchecked(at) }
  ```
- `crates/intern/src/arena.rs:464` block (test helper `intern`, `to.cast::<u64>().write(value)`): no SAFETY. Put the comment on its own line between `Entry::Vacant(vacant) => vacant` and `.insert(..)`: `` // SAFETY: `insert` hands `write` fresh memory of `Layout::new::<u64>()`, aligned and written by no one else. ``
- `crates/intern/src/intern.rs:148` `unsafe impl<T: Sync> Sync for InternedRef<'_, T>`: no SAFETY (the comment above `:147` covers only the `Send` impl). Proposed: `` // SAFETY: as for `Send`: sharing an `InternedRef` shares only a `&T` to a value that lives for the process, which `T: Sync` makes safe. ``
- `crates/intern/src/intern_slice.rs:54` `unsafe impl Sync for InternedSlice<T>`: no SAFETY of its own. Proposed: `` // SAFETY: as for `Send`: an `InternedSlice` is a shared reference to a header and items that are `Send + Sync`, live for the process and are never written again. ``
- `crates/intern/src/intern_slice.rs:107` block (`write`): 4 ops (two derefs of `fat`, `write`, `copy_from_nonoverlapping`). Split:
  ```rust
  // SAFETY: `fat` points at writable memory of `layout::<T>(slice.len())` (the precondition), so its `header` field is in bounds and aligned (`repr(C)`); this takes the field's address without reading it.
  let header_at = unsafe { &raw mut (*fat).header };
  // SAFETY: `header_at` is in bounds, aligned and not initialized yet; `write` drops nothing.
  unsafe { header_at.write(HeaderWithLength { header, length: slice.len() }) };
  // SAFETY: as for `header_at`: the address of the `slice` field, `slice.len()` items long.
  let items_at = unsafe { &raw mut (*fat).slice }.cast::<T::SliceType>();
  // SAFETY: `items_at` has room for `slice.len()` items and is aligned for them, and it is fresh memory, so it cannot overlap `slice`; `SliceType: Copy`, so copying the bytes is a copy.
  unsafe { items_at.copy_from_nonoverlapping(slice.as_ptr(), slice.len()) };
  ```
- `crates/intern/src/intern_slice.rs:122` block (`pointee`): 3 ops (deref of `header`, `read`, deref of the fat pointer). Split:
  ```rust
  let header = at.cast::<HeaderWithLength<T::Header>>().as_ptr();
  // SAFETY: `at` starts a published `Pointee<T>` (the precondition), which starts with its initialized `HeaderWithLength`; this takes the `length` field's address.
  let length_at = unsafe { &raw const (*header).length };
  // SAFETY: `length_at` is the initialized `length` of a value never written again.
  let length = unsafe { length_at.read() };
  // SAFETY: `length` is the value's own slice length, so the fat pointer covers exactly the value, which lives for the process.
  unsafe { &*fat::<T>(at, length) }
  ```
- `crates/intern/src/intern_slice.rs:188` `unsafe impl Sync for InternedSliceRef<'_, T>`: no SAFETY of its own. Proposed: `` // SAFETY: as for `Send`: a shared reference to `Send + Sync` values that live for the process. ``
- `crates/intern/src/symbol.rs:33` `unsafe impl Sync for Symbol`: no SAFETY of its own. Proposed: `` // SAFETY: as for `Send`: a `Symbol` is a shared reference to a `&'static str` that is never written again. ``
- `crates/intern/src/symbol.rs:86` block (`Symbol::intern`, the `insert` closure): 5 ops (`add`, `copy_from_nonoverlapping`, `from_raw_parts`, `from_utf8_unchecked`, `write`). Split:
  ```rust
  // SAFETY: `to` is fresh memory of `layout`, and `bytes_at` is where `Layout::extend` put the bytes in it, so the pointer stays in the allocation.
  let bytes = unsafe { to.as_ptr().add(bytes_at) };
  // SAFETY: `bytes` has room for `s.len()` bytes (the array part of `layout`) and is fresh memory, so it cannot overlap `s`.
  unsafe { bytes.copy_from_nonoverlapping(s.as_ptr(), s.len()) };
  // SAFETY: the `s.len()` bytes at `bytes` were just written, and the arena never moves, writes or frees them, so they live for `'static`.
  let bytes: &'static [u8] = unsafe { std::slice::from_raw_parts(bytes, s.len()) };
  // SAFETY: `bytes` is a copy of `s`, a `&str`, so it is UTF-8.
  let text: &'static str = unsafe { std::str::from_utf8_unchecked(bytes) };
  // SAFETY: `layout` starts with a `&'static str` (`Layout::new`), so `to` is aligned for it and has room, and nothing was there to drop.
  unsafe { to.cast::<&'static str>().write(text) };
  ```
- Everything else in `intern` (`arena.rs:128`, `:240`, `:295`, `:387`, `:431`, `:445`, `:457`; `intern.rs`; `intern_slice.rs:66`, `:75`, `:152`, `:199`; `symbol.rs:59`, `:69`, `:75`, `:106`, `:113`): ok.

### syntax-tree

`crates/syntax-tree/Cargo.toml` has no `[lints] workspace = true`, so the lints don't bind it until it does; the findings stand either way.

- `crates/syntax-tree/src/cursor.rs:186` unsafe fn `flat_unbound`: its contract is a `SAFETY:` paragraph, not a `# Safety` section. Proposed doc:
  ```rust
  /// [`flat`] for a root that is not borrowed from a cursor.
  ///
  /// # Safety
  ///
  /// The caller holds a cursor into `root`'s file, which keeps `root` alive (a part's root lives
  /// as long as its file's), for as long as `'a`.
  ```
- `crates/syntax-tree/src/cursor.rs:187` block (`&*root.as_ptr()`): no SAFETY. Proposed: `` // SAFETY: the caller's precondition keeps `root` alive for `'a`: a `FlatRoot` made from a box by `new_flat_root` or `part_root`, freed only when the last cursor into its file drops, and only ever read through shared references. ``

> Status: done by speed-unsafe-front-syntax (2026-10-30). `[lints] workspace = true` is in
> `crates/syntax-tree/Cargo.toml`. `flat_unbound` is gone: its one caller uses a safe
> `Ref::inner_tree(&self)` whose lifetime is the cursor's borrow, one commented block. The two
> `NonNull::new_unchecked(Box::into_raw(..))` are safe `NonNull::from(Box::leak(..))`. In
> `flat/block.rs` every block is one op (`put_joined`, `fill` split; `Drop` uses the checked
> `Layout::from_size_align`).

### syntax

- `crates/syntax/examples/trees.rs:29` block (`System.alloc`): the comment `as the system allocator's own` names no invariant. Proposed: `` // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract (a non-zero-sized `layout`), which is `System`'s too. ``
- `crates/syntax/examples/trees.rs:34` block (`System.dealloc`): same. Proposed: `` // SAFETY: `ptr` came from `alloc` above with `layout`, which is `System.alloc`'s, so it is `System`'s to free with that layout. ``
- `src/raw.rs` and the crate root (`deny(unsafe_code)` with `#[allow(unsafe_code)] mod raw`): ok.

> Status: done by speed-unsafe-front-syntax (2026-10-30). Both comments were in already. The
> example root now says `#![deny(unsafe_code)]` with its allocator in `#[allow(unsafe_code)] mod
> counting`; `tests/view.rs` says `#![forbid(unsafe_code)]`. `raw.rs`'s copy is the safe
> `write_copy_of_slice` (stable since 1.93), leaving two blocks. `crates/syntax/fuzz/`'s two
> targets say `#![forbid(unsafe_code)]` (`fuzz_target!` is another crate's macro, and
> `unsafe_code` is not reported in external macros); the package is superseded by the
> `syntax_parse` target filed in `v2/requests/safety.md`.
- `crates/syntax/fuzz/` is a package of its own (its own `[workspace]`, no workspace lints) with no unsafe; `#![forbid(unsafe_code)]` at the top of `fuzz_targets/parser.rs` and `reparse.rs` is optional, since libfuzzer's `fuzz_target!` expands outside them.

### blaze-ids, mbe (item 4)

- `crates/blaze-ids/src/lib.rs:12` crate: no `unsafe`. Add `#![forbid(unsafe_code)]` after the `//!` docs.
- `crates/mbe/src/lib.rs:9` crate: no `unsafe` (`parser.rs:21`'s `sym::unsafe_` is a symbol). Add `#![forbid(unsafe_code)]` after the `#![cfg_attr(feature = "in-rust-tree", ...)]` line.

### Checked, nothing to do

- `tt` (`lib.rs:7`), `syntax-bridge` (`lib.rs:4`) and `const-ast` (`lib.rs:28`) already forbid `unsafe_code`.
- `blaze-format`: `Record`'s `# Safety` (`lib.rs:48`) asks for `#[repr(C)]` or a primitive, no padding, no pointer, and every bit pattern `check` accepts being a value. Every impl meets it: `plain!` (primitives, and `ContentId`, `Fingerprint`, `SchemaHash`, each `#[repr(transparent)]` over a byte array or an integer), `bool` (`check` accepts only 0 and 1), `[T; N]`, `Rel<T>` and `Slice<T>` (`#[repr(C)]` integers and a ZST), `SpanRef` (`#[repr(C)]`, size asserted 36), and `record!` (asserts no padding; `SAFETY` written inside the macro body). Its blocks are single-op and commented. The same holds for `blaze-query/src/tests.rs:27`'s `Word`.
- `parser` (`syntax_kind.rs:20`, `lexed_str/scan.rs`), `span` (`hygiene.rs`) and format's `hir-def` files (`hir/persist.rs:615`, `expr_store/lower.rs:2808`, `:3085`, `item_tree/persist.rs:122`; `item_tree.rs` has no unsafe of its own left, using `record!`): ok.
- `#[salsa::interned(unsafe(no_lifetime))]` (a salsa option, not Rust `unsafe`): `span/src/hygiene.rs:62`.

## From safety-fuzz-macrowasm (2026-10-29): `tt::Literal::new` panics on a suffix over 255 bytes

`crates/tt/src/lib.rs:408` (and `:467`): `suffix_len: suffix.len().try_into().unwrap()` with
`suffix_len: u8`. Any literal with a suffix of 256+ bytes panics. rustc lexes it, then reports
an invalid suffix. It is reached from blaze-macro-wasm's lexer and wire reader, with any text
`1` followed by 256 letters (usercode is told, item 6 of its request). Source code through
syntax-bridge may reach it too. Input: `fuzz/corpus/macro_wasm_lex/suffix-256.hex`.
What: store a longer suffix (a `u16`/`u32` length, or the split kept in the symbol), or give
the callers a fallible constructor.

> forwarded to format: `crates/tt/` is lane format's.

## From safety-model: model tests for blaze-ids

From `safety-model-format` (2026-10-29). Written in the crates (both free): the switch
(`crates/blaze-ids/src/sync.rs`, `mod sync;` and the `use crate::sync::...` line in `lib.rs`;
`crates/intern/src/sync.rs` gains `OnceLock`, `intern.rs` and `intern_slice.rs` import it from
there, `crates/intern/Cargo.toml` gains the `blaze-check` line that speed-span's item 1 in
`v2/requests/speed.md` asked for), the model tests (`model_*` in `blaze-ids/src/lib.rs`,
`intern/src/{arena,intern,intern_slice}.rs`) and `#[cfg(not(blaze_check))]` on the tests that
spawn many threads or loop long. A normal build is unchanged. Item 2 remains (item 1 is done):

> forwarded to format (items 1 and 2 below).

### 1. `crates/blaze-ids/Cargo.toml`: the model build's dependency

Done by safety-model-format: the `[target.'cfg(blaze_check)'.dependencies]` `blaze-check` line is
in. It waited on a cycle (blaze-core depends on blaze-ids for `radix`, and blaze-check depended
on blaze-core); safety-model removed blaze-check's blaze-core dependency (its own `rng.rs`), so
blaze-check depends on nothing in the workspace. Nothing to do.

> unblocked: `blaze-core` and `blaze-graph` now use the leaf `blaze-radix` and no longer depend on `blaze-ids` (fix-radix.md), so the `cfg(blaze_check)` line makes no cycle. format told.

### 2. `crates/blaze-ids/src/lib.rs`: the domain-key table as a parameter, so a model can check it

Why: `DOMAIN_KEYS` is a `static`. Under the checker a `static` once cell set in one execution
stays set in every later one, so a model only explores the first execution's race between two
domains that probe the same slot. Taking the table as a parameter lets a model give each
execution a fresh one; the static path is the same code.

Before (`lib.rs`, `DOMAIN_KEYS` and `context_key`):

```rust
static DOMAIN_KEYS: [OnceLock<(&'static str, ContextKey)>; DOMAIN_SLOTS] =
    [const { OnceLock::new() }; DOMAIN_SLOTS];
```

```rust
/// `blake3::hazmat::hash_derive_key_context(domain)`, derived once per name.
fn context_key(domain: &'static str) -> &'static ContextKey {
    let mut at = name_hash(domain) & (DOMAIN_SLOTS - 1);
    for _ in 0..DOMAIN_SLOTS {
        let slot = &DOMAIN_KEYS[at];
```

After:

```rust
type DomainKeys = [OnceLock<(&'static str, ContextKey)>; DOMAIN_SLOTS];

static DOMAIN_KEYS: DomainKeys = [const { OnceLock::new() }; DOMAIN_SLOTS];
```

```rust
/// `blake3::hazmat::hash_derive_key_context(domain)`, derived once per name.
fn context_key(domain: &'static str) -> &'static ContextKey {
    context_key_in(&DOMAIN_KEYS, domain)
}

/// [`context_key`] in the table `slots`.
fn context_key_in<'a>(slots: &'a DomainKeys, domain: &'static str) -> &'a ContextKey {
    let mut at = name_hash(domain) & (DOMAIN_SLOTS - 1);
    for _ in 0..DOMAIN_SLOTS {
        let slot = &slots[at];
```

(the rest of the body unchanged). Then add to `mod model` at the end of `lib.rs`:

```rust
    /// Two domains whose names probe from one slot, derived at once: one sets the slot, the
    /// other reads it, sees another name and sets the next, and each gets its own key.
    #[test]
    fn model_colliding_domains_keep_their_own_keys() {
        let first = "blaze v2 model a 1";
        let home = name_hash(first) & (DOMAIN_SLOTS - 1);
        let second: &'static str = (0..)
            .map(|n| &*Box::leak(format!("blaze v2 model b {n}").into_boxed_str()))
            .find(|name| name_hash(name) & (DOMAIN_SLOTS - 1) == home)
            .expect("a name of the same home slot");
        blaze_check::model(move || {
            let slots: Arc<DomainKeys> = Arc::new([const { OnceLock::new() }; DOMAIN_SLOTS]);
            let other = {
                let slots = slots.clone();
                thread::spawn(move || *context_key_in(&slots, second))
            };
            let one = *context_key_in(&slots, first);
            let two = other.join().unwrap();
            assert_eq!(one, hazmat::hash_derive_key_context(first));
            assert_eq!(two, hazmat::hash_derive_key_context(second));
            assert_eq!(*context_key_in(&slots, first), one, "found again in its slot");
            assert_eq!(*context_key_in(&slots, second), two, "found again in its slot");
        });
    }
```

> forwarded to format: `crates/blaze-ids/src/lib.rs` is format's.
> applied: by format (2026-10-29): `type DomainKeys`, `context_key_in(slots, domain)` (the static path calls it), and `model_colliding_domains_keep_their_own_keys` in `mod model`, as written above.

## From safety-fuzz-persist (2026-10-29): seeds for the item tree and expression store fuzz targets, and what "write is the input" covers

Targets: `fuzz/targets/hir_item_tree.rs`, `hir_expr_store.rs`, `hir_body.rs`,
`hir_store_source_map.rs`, `hir_body_source_map.rs`, each `Persist::read` then `decode()`.

**1. A public seed builder (what).** The fixtures in `item_tree/tests.rs`, `item_tree/persist.rs`'s
tests, `expr_store/tests/body.rs` (`EVERY_VARIANT`) and `tests/signatures.rs` need `TestDB`, which
is `cfg(test)`. Nothing public builds a non-empty `ItemTree` or `ExpressionStore` without a
database: `ExpressionStoreBuilder::new` and its `alloc_*` are `pub(crate)`, and `ItemTree`'s
fields are private. So the targets' valid buffers are hand-built in `fuzz/targets/hir_shared.rs`
and `hir_expr_shared.rs`, laid out in your writers' order. Each target has a test that the
buffers decode and are written back byte for byte. That catches layout drift, but it covers only
what I wrote by hand: no bindings, labels, lifetimes, statements, record fields, side tables or
type side tables. Two ways to close it, either is enough:
- (preferred) a hir-def test that writes each fixture's buffer to
  `fuzz/corpus/<target>/fixture-<name>.hex` as `00` followed by the buffer in hex, and fails when a
  file is out of date (with an env var that rewrites them, as `expect-test` does). It uses the
  real fixtures and widens no API. The corpus directories are mine; I'm fine with your test
  writing files there.
- or db-free public builders: `ExpressionStoreBuilder::new`, `alloc_expr`, `alloc_pat`,
  `alloc_type_ref` and `finish` `pub`, and an `ItemTree` constructor from its columns.

**2. "Read, then decode, gives `Err` or a value whose write is the input" holds only for the
writer's layouts (a note, not a bug).** Every reader accepts valid buffers it never writes. A
`Slice` of length 0 follows to `&[]` whatever its offset, but `Column::slice` always writes it as
offset 0. Words that no table's `from(at)` reaches are kept by the buffer but not by the value.
Nonzero padding between columns is accepted. A name table entry no word uses, or the names in
another order, is also accepted. Each decodes to the same value as the canonical buffer, so the
write differs from the input. The store's content hash (`read_verified`) is what pins down the
exact bytes, so this isn't a safety problem. Example: take any buffer from
`blaze_format::to_bytes(&ItemTree::default())` and set the `offset` of its root's first `Slice`
(the 4 bytes at the root's offset, from header bytes 48..56) to `-4`. It reads and decodes to the
empty tree, which is written with offset 0. So the targets check the strict invariant only on
valid buffers edited inside the columns `decode` copies byte for byte. On arbitrary buffers they
check that the value's write is a fixpoint (it reads back equal and is written again the same).
To make the strict form hold, `read` would have to refuse non-canonical layouts. Say if you
want that; until then the targets keep the two checks.

**3. Bounds the targets assert.** `blaze_fuzz::allowed(len)` (1 MiB + 64 times the buffer's
length) for the largest allocation of `read` plus `decode`. That rests on `Read::len` bounding
every count by the words left, and on each value being at least one word.

> forwarded to format: `hir-def`'s `item_tree`/`expr_store` are its (told by tmux).

## speed-hir-store: subtree hashes carried by the flat tree (to speed-syntax)

What: `FlatTree` keeps a 128-bit hash per node (or per node above a size), computed bottom-up
while the tree is built, with the same content `hash_subtree` covers: text, kinds and shape
numbered from the node. An incremental reparse keeps the hashes of the subtrees it reuses.
`hash_subtree` (or a new `subtree_hash(at) -> u128`) then answers with one load.
Why: `hir-def` `expr_store/body.rs` `tree_hash` runs for every body of a file on each edit of the
file and hashes each anchor (an `impl` or module) whole, so an edit costs O(file size) of hashing,
about 70 µs for a 400 KB file with one BLAKE3 pass. With the hash carried by the parse, an edit
costs the reparse plus the edited body (`docs/v2/plan/lanes/format/speed-of-light.md`, "An edit").
`signatures.rs` hashes each item's tree the same way.

> taken by requests (format and speed-syntax are DONE): child `requests-subtree-hash`; the outcome follows.

> applied by requests (child requests-subtree-hash): `FlatTree::subtree_hash(at) -> u128` in `crates/syntax-tree/src/flat.rs`, the same value as `hir-def` `body.rs` `subtree_hash` (same domain); nodes costing 4 KiB or more keep it once made (a slot found by a popcount rank), and `replace_subtree`/`replace_token`, so every incremental reparse, carry it for the subtrees an edit does not change. Made when asked, not bottom-up while building, since a parent's BLAKE3 pass is not made of its children's and an edit would rehash every ancestor to the root. Tests in `flat.rs` and `crates/syntax/src/parsing/reparsing.rs`; the `body.rs`/`signatures.rs` switch is in `v2/requests/requests-subtree-hash.md`.

## format-grammar (parser and syntax: range ban, `&pin`, guard patterns, `RecordExpr` attrs, `cfg` literals, fn-pointer params, `!` bounds, `unsafe mod`)

The parser and syntax tree now have these (see `/tmp/blaze-lanes/v2-format-grammar.md`). What
the rest of the tree must do with them:

1. **To format-lexer: a `for` loop's pattern may have a guard.** `crates/parser/src/grammar/expressions/atom.rs:733`
   (`for_expr`): `patterns::pattern(p)` becomes `patterns::pattern_top_guard(p)`. Why: rustc's
   `parse_expr_for` reads its pattern with `parse_pat_allow_top_guard`; every other site rustc
   reads that way (tuple, slice, tuple struct and struct field patterns, `builtin#deref`) does so
   now in `grammar/patterns.rs`.
2. **To the lead / format-hir: `ast::Pat::GuardPat` breaks two exhaustive matches** in
   `crates/hir-def/src/expr_store/lower.rs` (`ast::Pat::NotNull` at :3500 and :3669). Lower it
   to a new `Pat::Guard { pat, guard: ExprId }` (rustc's `PatKind::Guard`; `GuardPat::pat()`,
   `GuardPat::guard()?.condition()`), not by dropping the guard: a pattern lowered without its
   guard matches more than rustc's (no-fallback). Same file, `lower_type_bound` (:1517): a bound
   with `TypeBound::excl_token()` is rustc's `BoundPolarity::Negative` and must not lower as a
   positive bound (a `TraitBoundModifier::Negative`, or an error until hir-ty knows negative
   bounds). `RefPat::pin_token()` with `const_token()`/`mut_token()` is a pinned reference
   pattern (`PatKind::Ref(_, Pinnedness::Pinned, _)`), not `&`/`&mut`.
3. **To the coordinator: `crates/blaze-fmt`.** `src/parse/lower/pat.rs:150` matches `ra::Pat`
   exhaustively, so it needs a `GuardPat` arm (`PatKind::Guard(pat, expr)`); `RefPat::pin_token()`
   is `Pinnedness::Pinned`. `src/parse/lower/mod.rs:1123` reads only `question_mark_token()`:
   `excl_token()` is `BoundPolarity::Negative`. `Module::unsafe_token()` is the module's
   `Safety::Unsafe`.
4. **To diagnostics: lower the new parser errors** (`crates/blaze-diagnostics/src/lower/parse.rs`,
   `parser_error`; each is recovered, the tree is whole). A parser error's position is the end
   of the last token before it.
   - "the range pattern here has ambiguous interpretation" (rustc's `AmbiguousRangePattern`): the
     position is the end of the `RANGE_PAT` that is the child of a `REF_PAT` or `BOX_PAT`; primary
     span that range pattern, verbose multipart suggestion "add parentheses to clarify the
     precedence" (`(` before, `)` after).
   - "`{modifiers}` trait not allowed with `{polarity}` trait polarity modifier" (rustc's
     `PolarityAndModifiers`, the text already formatted): the position is the end of the `?`/`!`
     token of the `TYPE_BOUND`, the primary span; label "there is not a well-defined meaning for
     a `{modifiers} {polarity}` trait" on the bound's modifiers, from its first `~`/`[`/`const`
     token through its `const`, `]` or `async`.
   - "patterns aren't allowed in function pointer types" (rustc's `recover_arg_parse`,
     `PatternMethodParamWithoutBody`, E0642): the position is the end of the pattern of a
     fn-pointer `PARAM` written `&mut x: T`; primary span that pattern, verbose machine-applicable
     suggestion "give this argument a name or use an underscore to ignore it" replacing it with
     `_`. AST validation's E0561 must skip that param: rustc replaced its pattern with `_`.
     Every other fn-pointer param pattern (`mut x:`, `&x:`, `&&x:`) is rustc's `is_named_param`
     and is E0561's.
5. **To diagnostics: the checks the new nodes allow.** Feature gates `guard_patterns` (a
   `GuardPat`), `pin_ergonomics` (the `pin` token of a `RefPat`) and `negative_bounds` (a
   `TypeBound`'s `!`); `UnsafeItem` "module cannot be declared unsafe" (`Module::unsafe_token`);
   E0561 and rustc's `self`-parameter errors for fn pointers (`fn(self)`, `fn(&self)`, `fn(mut
   self: u8)` are `SelfParam`s in a `FnPtrType`'s `ParamList` now, first position only, as
   `opt_self_param` reads them); `ConstraintOnNegativeBound` and
   `NegativeBoundWithParentheticalNotation` (`TypeBound::excl_token`).
6. **To diagnostics: `cfg` literals.** A `CfgAtom` may be a `Literal` (`CfgAtom::literal()`)
   anywhere a predicate is, as rustc's `parse_cfg_entry` reads one; the parser gives no error,
   since rustc's come from the attribute parser. `checks/attrs` (or wherever `#[cfg]`,
   `cfg_attr` and `cfg!` predicates are checked) gives rustc's: "expected a valid identifier
   here" for a literal that is not `version`'s argument, and `parse_cfg_entry_version`'s
   "expected single version literal", "expected a version literal" and the warning "unknown
   version literal format, assuming it refers to a future version". Then un-ignore
   `cfg_version_is_gated` in `crates/blaze-diagnostics/src/tests/items/features/attributes.rs:553`.
7. **To the lead (owner of `crates/cfg`): evaluate `version("x.y.z")`** in
   `crates/cfg/src/cfg_expr.rs` `parse_from_ast` against the pinned toolchain's version (rustc
   1.101.0-nightly, `harness/toolchain`), as rustc's `eval_condition` does (true when the
   current version is at least the literal's, strictly greater under `-Z
   assume-incomplete-release`). It is `CfgExpr::Invalid` now, as before this change.

> forwarded to format-grammar (told by tmux).
> answered: by format-grammar. This section is format-grammar's own report of what the rest of
> the tree must do with the new syntax; nothing in it is for format-grammar. Each item names its
> owner: item 1 is format-lexer's (`grammar/expressions/atom.rs`), item 2 the lead's and
> format-hir's, item 3 the coordinator's, items 4-6 diagnostics' (also filed in
> `v2/requests/diagnostics.md`), item 7 the lead's.

## frontend-tokens: `hot_lint` findings in `crates/parser`

Why: `tools/v2/hot_lint.py` on `crates/parser` (outside tests) finds 35 lines. You are editing
the grammar and `lexed_str.rs`, so I left them to you. Nearly all are error paths and only need
a justification on the line or the one above (`// alloc: an error.`):

- Error messages: `grammar.rs:470`, `grammar/attributes.rs:112`, `grammar/generic_params.rs:238`,
  `grammar/patterns.rs:600`, `parser.rs:310`, `lexed_str.rs:318, 365, 381, 418, 433`, and
  `frontmatter.rs:77, 88, 119, 127, 150, 162, 183, 325`.
- Error buffers that stay empty unless something goes wrong (`Vec::new()` does not allocate):
  `lexed_str.rs:246-247`, `output.rs:52`, `parser.rs:52`.
- `event.rs:97` `forward_parents`: one buffer for each `process` call, which allocates only when
  a node has a forward parent. Justify it as the scratch of one parse.
- `frontmatter.rs:103` builds `"\n" + fence_pattern` with `format!` once for each script that has
  frontmatter. That is setup, so a justification is enough.
- `frontmatter.rs:194-230, 338`: `Range<usize>` clones. These are two words, with no heap, so they
  only need `// clone: a range.`
- `shortcuts.rs:209`: a cloned iterator over trivia, also no heap.

(`outline.rs:34` belongs to frontend-deps.)

> forwarded to format-grammar (editing the parser; told by tmux).
> applied: by format-lexer for `lexed_str.rs`. Lines 246-247 are `// alloc:` notes saying the
> long-token and error tables are empty until used. Lines 318, 365, 381, 418 and 433 are
> `// alloc: the error path.` on the line above. `hot_lint.py` finds nothing left in
> `lexed_str.rs`, `lexed_str/**`, `grammar/expressions*` or `tests.rs`. The rest are
> format-grammar's.
> applied: by format-grammar for the grammar. `grammar.rs:470`, `grammar/attributes.rs:112`,
> `grammar/generic_params.rs:238` and `grammar/patterns.rs:600` carry `// alloc: error path
> only, ...` on the line above, and `hot_lint.py` finds nothing left in `grammar/**` or
> `syntax_kind/**`. Not done, because format-grammar does not own them: `parser.rs:52, 310`,
> `event.rs:97` and `output.rs:52` are the lead's (`event.rs:97` is also asked for in "the parse's
> buffers onto blaze-mem" below, which removes it rather than justifying it); `frontmatter.rs`
> and `shortcuts.rs:209` belong to neither format child, so they go to the lead.


## frontend-deps-reparse: `FlatTree::replace_subtree` by copying, not repacking (speed of light; 2026-10-29)

From frontend-deps-reparse, for whoever holds `crates/syntax-tree/src/flat.rs` (requests-subtree-hash
is editing it now, so I did not). Notes: `docs/v2/plan/lanes/frontend/speed-of-light.md`, section
frontend-deps-reparse.

Every reparse of an edit (`syntax::Parse::reparse_to`, `incremental_reparse_onto`) ends in
`replace_subtree`, which repacks all \(E\) elements of the file one at a time (`parent`, `end`,
`repack`, then `Packer::finish`): about 15 cycles an element, about 170 µs for a 50 KB file of
30 000 elements, where copying its arrays (6.5 bytes an element, read and written, at 32 bytes a
cycle) is about 12 000 cycles, 3.5 µs. That is about 2% of the bound, and the largest cost of a
keystroke in a big file.

What: build the new tree's arrays by copying, with \(\delta\) = added − removed elements:
- `words[..lo]` copied; the sizes of `at`'s ancestors (depth \(d\)) patched by \(\delta\);
  `with`'s words copied in, its root's parent distance set.
- `words[hi..]` copied, but for the tokens that are direct children of an ancestor of `at` and come
  after it, whose distance back to their parent grows by \(\delta\) (they move to or from `far` at
  1023). Find them by stepping each ancestor's children after the child holding `at` (subtree
  sizes), \(O(\sum \text{children of the ancestors})\), not \(O(E)\).
- `node_parents`: copied before `lo`; `with`'s plus `lo`; after `hi`, each one \(\geq hi\) plus
  \(\delta\) (a compare and add a node, vectorizable).
- `big`, `kinds`, `far`: entries before `lo` kept, `with`'s shifted by `lo`, those after `hi`
  shifted by \(\delta\); the ancestors' and moved tokens' entries re-derived.
- node bits and ranks: the bit array shifted by \(\delta\) bits after `lo` (\(E/64\) words), ranks
  recounted from it (\(E/64\) popcounts).
- marks: unchanged before `lo`; after, each mark's start is the old start of element
  `mark - δ` plus the text growth, read from the old marks and at most 7 lengths
  (\(E/8\) marks).

That is about 1.5 cycles an element, about 40 µs at 30 000 elements: 25 times faster, and
`replace_token` already works this way. `reparse_to_random_edits` and `flat.rs`'s splice tests
hold the result equal to the repacked tree.

Past that, 90% of the bound needs a tree whose unchanged items are shared, not copied (Roslyn's
green nodes, tree-sitter's reused subtrees): the top level's items as separate arrays under a
spine, so an edit copies one item and the spine. That is a layout change for format to weigh;
the copy above is the step that needs none.

> taken by requests (format is DONE): child `requests-subtree-hash`, after its two tasks in `flat.rs`; the outcome follows.

> applied by requests (child requests-subtree-hash): `FlatTree::replace_subtree` copies, as
> asked: `words`, `kinds`, `node_parents` and the node bits (`append_bits`, up to 64 at a time) copied in three
> runs with the numbers past `hi` moved, the ancestors' sizes and their tokens after `at` patched by
> walking each ancestor's children after the child holding `at`, `far` and `big` sorted once after
> (radix), marks past `lo` from `with`'s or the old marks plus at most seven lengths; kept hashes
> as before (`kept_around`). `repack` is gone. The splice tests (`replacing_a_subtree_is_building_the_edited_tree`,
> `replacing_in_a_big_tree_moves_what_is_past_the_word`, `an_edit_keeps_the_hashes_of_what_it_does_not_change`)
> now also hold every array equal to the tree built anew (`same_arrays`: node bits, ranks, node
> parents, `far`, all marks). Not built or run (no builds for this lane); the coordinator's build runs them.

## format-lexer

> applied: see its subsection.

### `blaze-mem` and `blaze-alloc-count` in `crates/parser/Cargo.toml` (not mine)

What: two lines in `crates/parser/Cargo.toml`. Both crates are already workspace dependencies.
- `blaze-mem.workspace = true` under `[dependencies]`.
- `blaze-alloc-count.workspace = true` under `[dev-dependencies]`, the same way `span` uses it.

Why:
- `LexedStr::lex` (`lexed_str.rs`) now puts the byte classes in the worker's scratch
  (`blaze_mem::scratch::with_u64s`), per docs/v2/plan/design/memory.md. A warm lex allocates
  nothing for them.
- `crates/parser/src/tests.rs` installs `blaze_alloc_count::Counting` as the test binary's
  global allocator. Its budget tests depend on it: `a_warm_lex_allocates_its_two_columns_once`
  and `a_dense_text_grows_the_columns_by_doubling`.

Until both lines are in, the parser does not compile.

Also: `tools/v2/hot_lint.py` rejects `{"crates": [...]}` with "typed_input: crates has a type it
does not read: list[str]". The field is declared `list[str]`, but `typed_input` reads only
`tuple[str, ...]`. Running it with `{"root": "."}` alone works.

> applied by requests (format is DONE): `blaze-alloc-count.workspace = true` in `crates/parser/Cargo.toml`'s `[dev-dependencies]`. The hot_lint item is the coordinator's (listed).

## format-grammar: the parser's speed of light (`crates/parser/src/parser.rs`, `input.rs`; to the lead)

What, from "The grammar: tokens to events" in `docs/v2/plan/lanes/format/speed-of-light.md`:
1. Move `Parser`'s step counter out of `nth` (a `Cell` load, compare, branch and store on
   every kind test, and the store keeps the compiler from reusing a loaded kind) into the
   places a stuck loop must pass: `start`, `error`, `err_recover`, a failed `expect`. Add a fuzz
   target over the grammar's entry points that asserts every input finishes.
2. Pad `Input::words` with 4 `EOF` words, so `kind`, `contextual_kind` and `is_joint` up to
   `pos + 3` need no `get`. Store kinds `Input` knows are valid, so `kind` needs no
   `SyntaxKind::from` range assert.
3. Replace `Marker`'s `DropBomb` (a `Cow` message built per node) with a `bool` and a `Drop`
   that asserts it.

Why: a kind test is about 12 instructions where the work needs 2, and the grammar makes about 8
per token. These three take the parser from about 5× its bound to about 2.3×; the rest is the
grammar's structure (speed-syntax's postorder rewrite).
4. Size `Parser::events` at about `3.25 * inp.len()` instead of `2 * inp.len()`: a token is one
   `Token` event plus a `Start` and a `Finish` for each of its 1.05 nodes (counted from the test
   trees), about 3.1 events. So every parse doubles the list once, copying it. With that sizing,
   the budget in `tests/top_entries.rs`, `a_parse_allocates_its_buffers_not_per_token`, goes
   from 6 to 5.
5. Make `Parser::errors` (and `Output`'s error table) `Cow<'static, str>`. Most parser
   messages are fixed text, but `error<T: Into<String>>` allocates each one. Syntax errors
   are an error path (`// alloc:` justified in `grammar/`), but a file being edited is full of
   them.

> taken by requests (format is DONE): child `requests-parser`; the outcome follows.
> applied by requests (child requests-parser): items 1-3. (1) The step counter is a plain u32 in Parser, checked in start, error (every failed expect and every err_recover path go through it), and the EOF branches of bump_any/bump_remap. Same PARSER_STEP_LIMIT and message, not in nth. Termination is a property test, not a fuzz target, because fuzz/Cargo.toml was being edited by another lane and blaze-fuzz has its own global allocator: tests.rs termination::every_entry_point_finishes_on_every_input runs every Top/Prefix entry point, outlines and Reparser::top_level_item over 2000 changed token sequences, with a deadline. (2) Input and frontend-tokens new parser::Tokens end in Tokens::PAD = 4 EOF words, and len() leaves them out. kind, contextual_kind and is_joint are a slice load, and kind is transmuted with no SyntaxKind::from assert; Tokens::new checks the pad and every kind field. No change in grammar/. (3) Marker is a u32 and a bool with its own Drop, same message, and drop_bomb is gone from Cargo.toml. Tests are beside the code (input.rs, parser.rs). Arithmetic is in docs/v2/plan/lanes/format/speed-of-light.md: about 2.5x the bound, from about 5x. Items 4 and 5 were not in this brief. Nothing built.
> forwarded to frontend-tokens (items 4 and 5): `crates/parser/` is held by frontend-tokens, still running (design item 4), and item 4 also changes `tests/top_entries.rs`' budget. Items 1-3 were applied above. Told by tmux.
> items 4 and 5 done by frontend-tokens: (4) events are sized at 3.25 a token (requests-parser's `lend_events`), and with the output words on a shelf too the budget in `src/tests/top_entries.rs` is 0 for a second parse, not 5; (5) `Parser::errors`, `event::process` and `Output`'s error table are `Cow<'static, str>`, `error` takes `Into<Cow<'static, str>>`, and `err_and_bump`, `err_recover`, `error_block`, `error_let_stmt` take `&'static str` (every caller passes a literal), so a fixed message allocates nothing. Nothing built.

## format-grammar: `tools/v2/hot_lint.py` refuses its own `crates` field (to the coordinator)

`PYTHONPATH=python/typed-input/src python3 tools/v2/hot_lint.py '{"root": ".", "crates":
["parser"]}'` fails with `TypeError: typed_input: crates has a type it does not read:
list[str]`. `python/typed-input` doesn't read `list[str]`, so only the default set (all hot
crates) runs. Either `typed_input` learns `list[str]`, or `Input.crates` becomes a
`tuple[str, ...]` if that is what it reads.

> forwarded to the coordinator: `tools/` is its; on requests' coordinator list.

## format-grammar: the parse's buffers onto blaze-mem (parser.rs, event.rs; to the lead, cc mem)

docs/v2/plan/design/memory.md wants zero general-purpose allocations in the warm loop. The grammar
makes none on valid input (its only `String`s are error messages, marked `// alloc:`), and
keyword lookup makes none (`a_lookup_allocates_nothing`). What is left per parse is in
`parser.rs`/`event.rs`: the event list, the forward-parent list and the output words, six
allocations a file (`a_parse_allocates_its_buffers_not_per_token` in `tests/top_entries.rs`).
The event and forward-parent lists die inside `parse`, so they belong on a per-worker
`blaze_mem::scratch` shelf (`lend`, grown once to the high-water mark, tag parse); the output words
live until the tree is built, so either the caller lends them too or they come from the task's
`Frame`. With that, drop that test's budget to 0 for a warm second parse.

> taken by requests (format is DONE): child `requests-parser`, with the speed-of-light request on the same files; the outcome follows.
> forwarded to frontend-tokens: `parser.rs`/`event.rs` are held by frontend-tokens, still running; requests-parser did not take this one after all. Told by tmux, cc mem-format.
> applied by requests (child requests-parser): the event list and the forward-parent list are per-thread blaze_mem::scratch buffers. event.rs has a per_thread_buffer! macro (a Shelved newtype over its own thread-local, which keeps the larger of two coming back), Events and ForwardParents. Parser::new takes the thread's list through lend_events, sized at 3.25 events a token so a cold list seldom doubles, and process borrows ForwardParents through scratch::with. A warm parse now allocates only the output's words. Those are built in output.rs, which is not mine; the exact three-edit change (Output's words as a Lent<Words> from the same macro) is in v2/requests/requests.md, From requests-parser: the parse's output words on a per-thread shelf. Tests in tests.rs warm_parse: a warm parse makes at most 1 allocation (outline 2), and a small parse keeps the large list warm. These budgets and top_entries.rs' 6 go to 0 once output.rs lands. blaze_mem::scratch has no tag argument, so these buffers carry no parse tag. Nothing built.
> done: requests-parser moved the event and forward-parent lists onto per-thread `blaze_mem::scratch` buffers; frontend-tokens applied its `output.rs` change (`v2/requests/requests.md`). A warm parse and a warm outline assert 0 allocations (`warm_parse::a_warm_parse_allocates_nothing`, `a_small_parse_keeps_the_large_list_warm`, `top_entries.rs`). The buffers carry no tag, since `blaze_mem::scratch` takes none. Nothing built.

## frontend-tokens: `crates/parser/src/input.rs` and `parser.rs` (to requests-parser)

`crates/parser/` is frontend-tokens' (item 4 in docs/v2/plan/design/frontend.md); your padding
and step-counter changes are welcome and fit item 4. Item 4 makes the parser read a token tree's
stored words (`tt::TreeWords`, made from `Input::into_words`, so padded at the end), passed as
`Tokens`. Two additions, which I will make once you are out of both files. Tell me
(`frontend-tokens` window) when you are, or add them yourself:
1. `Tokens` gets a field `first_fragment: bool`, and a `pub fn without_first_fragment(self) -> Self`.
   `is_fragment_start(0)` reads `false` when that field is false. A view that starts inside the
   group a fragment's first token came from must not see that start, as `to_parser_input` gives
   it today.
2. `Input::extend_from_words(&mut self, words: &[u32])` appends words to an input and keeps the
   pad. It checks `has_valid_kinds` of each word, so `kind_of`'s SAFETY argument still holds.
   This is for a view that ends before its tree does (an mbe fragment's window): it copies the
   words into a reused input, 4 bytes a token, with no kinds recomputed.
Then `Parser` reads `Tokens<'t>` in place of `&'t Input`. `outline::lazy_block_len` is to take
`impl Into<Tokens<'t>>` (frontend-deps, `v2/requests/frontend-deps.md`).

> forwarded to frontend-tokens: requests-parser is DONE and out of `input.rs` and `parser.rs` (it kept `Input`'s 4 `EOF` words of padding; `Parser` still takes `&Input`), so both additions are frontend-tokens' to make. Told by tmux.
> requests-parser: out of input.rs and parser.rs for good (frontend-tokens told by tmux at 20:12). frontend-tokens has since added both: Tokens::without_first_fragment (field first_fragment) and Input::extend_from_words. Checked against the padding: Input::into_words returns the words with their 4 EOF_WORD pad, which Tokens::new requires. extend_from_words checks has_valid_kinds of each word, cuts the pad, appends the words and puts the 4 EOF_WORDs back, so kind_of's SAFETY argument and the pos+3 lookahead still hold. One more thing for the Tokens switch: parser.rs' events are a scratch Lent<event::Events> (buffers request), and do_bump debug-asserts pos <= len, which the lookahead into the pad relies on.
> done by frontend-tokens (20:15-20:25): `Tokens::without_first_fragment`, `Tokens::ignoring_fragments` and `Input::extend_from_words` (checks kinds, keeps the pad), with tests in `input.rs`; `Parser` reads `Tokens<'t>` and every entry point takes `impl Into<Tokens<'t>>`. Nothing built.

## From speed-link-edge-sol-wire: `ContentId` and `Fingerprint` as one CBOR byte string (2026-10-29)

`ContentId([u8; 32])` derives `Serialize`, and serde writes a `[u8; 32]` as a tuple of 32
integers. `Fingerprint` writes `[u8; 16]` the same way (`crates/blaze-ids/src/lib.rs` 192-202).
In `blaze_protocol::cbor` that is 1 + 32 × (1 + 232/256) ≈ 62 bytes and 33 decoder steps for a
`ContentId`, against 34 bytes and one copy as a byte string. Fingerprint: about 31 bytes and 17
steps against 18 and one.

The API sends them in bulk: `Response::CodegenInputs` (two per instance, 10^5 instances in a
big build), `Response::Trace` (a `QueryKey` per edge), `Snapshotted`, every root. A trace of
10^6 edges is about 60 MB and 3×10^7 decoder steps now (about 150 ms to read), against about
34 MB and 10^6 steps (about 10 ms).

1. What: serialize both with `serialize_bytes` and read them back from a byte string of exactly
   32 (16) bytes, refusing any other length or an array:
   `impl Serialize for ContentId { fn serialize<S>(&self, to: S) { to.serialize_bytes(&self.0) } }`
   and a `Deserialize` whose visitor's `visit_bytes` takes `<[u8; 32]>::try_from(bytes)` and
   errors otherwise (`blaze_protocol::bytes` has the pattern, for `Vec<u8>`; JSON's
   serde_json writes bytes as the same array of numbers, so a human-readable reader also takes
   `visit_seq`).
2. Who must agree: every persisted serde encoding that holds one changes bytes, so each store
   or record format that hashes or keeps CBOR with a `ContentId` in it bumps its version or
   domain (`"blaze v2 <what> <n>"`) in the same change. blaze-api bumps `SCHEMA` (11 to 12, or
   inside 11 if this lands before the first build; tell speed-link-edge or lane ci).
3. Why: the bound for a frame is its bytes ÷ memory bandwidth; a byte-per-item encoding is 30
   to 60 times that for these ids, and they are most of a trace's bytes.

> taken by requests (format and dedup-intern are DONE): child `requests-ids`; the outcome follows.
> applied by requests (child requests-ids): item 1 in `crates/blaze-ids/src/wire.rs` (byte-string Serialize, exact-length Deserialize, arrays only from human-readable readers) with tests in `wire/tests.rs` (postcard and a binary test reader, since blaze-protocol depends on blaze-ids; JSON); item 2 listed in `v2/requests/requests.md` "From requests-ids (2026-10-29): bumps for `ContentId` and `Fingerprint` as one byte string" (blaze-api `SCHEMA` inside 13, six domain/version bumps with owners).

## dedup: bits: syntax-tree `flat.rs` ranks, `rank_in`, `Kept` and `append_bits` onto `blaze_ids::bits`

From `dedup-bits-store` (report `/tmp/blaze-lanes/v2-dedup-bits-store.md`; parent `dedup-bits`).
`blaze_ids::bits` (`crates/blaze-ids/src/bits.rs`) has what `flat.rs` wrote by hand:
`ranks_of(words) -> Option<Box<[u32]>>` (the members before each word) and
`rank_in(words, ranks, at)` (one popcount) for the tree's `nodes`/`ranks` columns, which stay
two columns of `FlatTree`; `Ranked` (words, the same `u32` count a word, a length) for `Kept`'s
`bits`/`ranks`, with `place(rank)` = the slot index when kept; and `Bits::or_range_at(at,
from, range)`, `append_bits`' splice, written for this caller. Same bits, same ranks, same
column lengths and allocations (each column once at its final length); `heap_size` counts the
same bytes. syntax-tree depends on blaze-ids already. The ranks of `Kept` and of the edit's
node bitmap were sums with no overflow check; `ranks_of` is checked and gives `None` past
`u32::MAX`, which a tree (fewer than `NO_PARENT` elements, asserted) never reaches.

`crates/syntax-tree/src/flat.rs`:

1. Imports, before: `use blaze_ids::{radix, Domain};` after:

   ```rust
   use blaze_ids::{
       Domain,
       bits::{BitSet, BitSlice, Ranked, rank_in, ranks_of},
       radix,
   };
   ```

2. `Kept`, before:

   ```rust
   #[derive(Default)]
   struct Kept {
       /// The nodes, ascending.
       elements: Box<[u32]>,
       /// Bit `n % 64` of word `n / 64`: whether the node of rank `n` among the nodes is kept.
       /// Empty when none is.
       bits: Box<[u64]>,
       /// How many are kept before each word of `bits`.
       ranks: Box<[u32]>,
       /// Their hashes, as `elements`.
       hashes: Box<[Slot]>,
   }

   impl Clone for Kept {
       fn clone(&self) -> Kept {
           let Kept { elements, bits, ranks, hashes } = self;
           let hashes = hashes.iter().map(|it| Slot::new(it.value()));
           // clone: a tree copied whole (a part moved by an edit) copies its columns
           let (elements, bits, ranks) = (elements.clone(), bits.clone(), ranks.clone());
           // alloc: a column of the copy, as those above
           Kept { elements, bits, ranks, hashes: hashes.collect() }
       }
   }
   ```

   after:

   ```rust
   #[derive(Default)]
   struct Kept {
       /// The nodes, ascending.
       elements: Box<[u32]>,
       /// Whether the node of rank `n` among the nodes is kept, ranked: a kept node's rank in
       /// it is its place in `hashes`. Of no bits when none is.
       bits: Ranked,
       /// Their hashes, as `elements`.
       hashes: Box<[Slot]>,
   }

   impl Clone for Kept {
       fn clone(&self) -> Kept {
           let Kept { elements, bits, hashes } = self;
           let hashes = hashes.iter().map(|it| Slot::new(it.value()));
           // clone: a tree copied whole (a part moved by an edit) copies its columns
           let (elements, bits) = (elements.clone(), bits.clone());
           // alloc: a column of the copy, as those above
           Kept { elements, bits, hashes: hashes.collect() }
       }
   }
   ```

3. `Kept::new`, before (the body after the `debug_assert!` of ascending nodes):

   ```rust
           // alloc: the tree's columns of kept nodes, made once with the tree
           let mut bits = vec![0u64; count.div_ceil(64)];
           for &(at, _) in kept {
               debug_assert!((nodes[at as usize / 64] >> (at % 64)) & 1 == 1, "element {at} is a node");
               let rank = rank_in(nodes, ranks, at);
               bits[rank / 64] |= 1 << (rank % 64);
           }
           let mut before = 0;
           let ranks = bits.iter().map(|it| {
               let rank = before;
               before += it.count_ones();
               rank
           });
           // alloc: as `bits`
           let (ranks, elements) = (ranks.collect(), kept.iter().map(|it| it.0).collect());
           // alloc: as `bits`
           let hashes = kept.iter().map(|it| Slot::new(it.1)).collect();
           Kept { elements, bits: bits.into_boxed_slice(), ranks, hashes }
   ```

   after:

   ```rust
           // alloc: the tree's columns of kept nodes, made once with the tree
           let mut set = BitSet::<usize>::new(count);
           for &(at, _) in kept {
               debug_assert!(BitSlice::from_words(nodes).contains(at as usize), "element {at} is a node");
               set.insert(rank_in(nodes, ranks, at as usize));
           }
           // alloc: as `set`, its ranks
           let bits = Ranked::from_words(set.into_words(), count).expect("a tree of fewer than 2^32 nodes");
           // alloc: as `set`
           let elements = kept.iter().map(|it| it.0).collect();
           // alloc: as `set`
           let hashes = kept.iter().map(|it| Slot::new(it.1)).collect();
           Kept { elements, bits, hashes }
   ```

4. `Kept::slot`, before:

   ```rust
       fn slot(&self, rank: usize) -> Option<&Slot> {
           let (word, bit) = (rank / 64, rank % 64);
           let bits = *self.bits.get(word)?;
           ((bits >> bit) & 1 == 1).then(|| {
               let before = (bits & ((1u64 << bit) - 1)).count_ones();
               &self.hashes[self.ranks[word] as usize + before as usize]
           })
       }
   ```

   after (`place` is `None` past its length, as `get(word)?` was past its words; a node's
   rank is below `count`):

   ```rust
       fn slot(&self, rank: usize) -> Option<&Slot> {
           self.bits.place(rank).map(|at| &self.hashes[at])
       }
   ```

5. `Kept::heap_size`, before / after:

   ```rust
           (self.elements.len() + self.ranks.len()) * 4 + self.bits.len() * 8 + self.hashes.len() * 16
   ```

   ```rust
           (self.elements.len() + self.bits.ranks().len()) * 4 + self.bits.words().len() * 8 + self.hashes.len() * 16
   ```

   (`subtree_hash`'s `NODE if !self.kept.bits.is_empty()` stays: `Ranked::is_empty` is "of no
   bits", which `Kept::default` is and every `Kept::new` of a kept node is not.)

6. Delete the local `fn rank_in` (with its doc comment, `/// How many nodes come before
   element `at` ...`). `FlatTree::rank`, before / after:

   ```rust
           rank_in(&self.nodes, &self.ranks, at)
   ```

   ```rust
           rank_in(&self.nodes, &self.ranks, at as usize)
   ```

7. `fn nodes`, before:

   ```rust
   fn nodes(words: &[u32]) -> (Box<[u64]>, Box<[u32]>) {
       let mut nodes = Vec::with_capacity(words.len().div_ceil(64));
       let mut ranks = Vec::with_capacity(words.len().div_ceil(64));
       let mut before = 0;
       for chunk in words.chunks(64) {
           let mut bits = 0u64;
           for (bit, &word) in chunk.iter().enumerate() {
               if word & TAG == NODE {
                   bits |= 1 << bit;
               }
           }
           ranks.push(before);
           nodes.push(bits);
           before += bits.count_ones();
       }
       (nodes.into_boxed_slice(), ranks.into_boxed_slice())
   }
   ```

   after:

   ```rust
   fn nodes(words: &[u32]) -> (Box<[u64]>, Box<[u32]>) {
       let mut nodes = Vec::with_capacity(words.len().div_ceil(64));
       for chunk in words.chunks(64) {
           let mut bits = 0u64;
           for (bit, &word) in chunk.iter().enumerate() {
               if word & TAG == NODE {
                   bits |= 1 << bit;
               }
           }
           nodes.push(bits);
       }
       let ranks = ranks_of(&nodes).expect("a tree of fewer than 2^32 elements");
       (nodes.into_boxed_slice(), ranks)
   }
   ```

8. Delete `fn append_bits` (with its doc comment `/// Appends bits `from..to` of `src` to
   `dst`, ...`). In the edit (~line 1271), before:

   ```rust
           // alloc: as `words`
           let mut nodes = Vec::with_capacity((len as usize).div_ceil(64));
           let mut bits = 0;
           append_bits(&mut nodes, &mut bits, &self.nodes, 0, lo as usize);
           append_bits(&mut nodes, &mut bits, &with.nodes, 0, added as usize);
           append_bits(&mut nodes, &mut bits, &self.nodes, hi as usize, self.len() as usize);
           let mut counted = 0;
           let ranks = nodes.iter().map(|it| {
               let rank = counted;
               counted += it.count_ones();
               rank
           });
           // alloc: as `words`
           let ranks: Box<[u32]> = ranks.collect();
           debug_assert_eq!(counted as usize, node_parents.len(), "a node bit for each node");
   ```

   after (`len` is `lo + added + (self.len() - hi)`, so the three ranges fill the new set's
   bits exactly; `nodes` stays a `Vec<u64>` for `Kept::new(&nodes, ...)` and `exact(nodes)`
   below):

   ```rust
           // alloc: as `words`
           let mut set = BitSet::<usize>::new(len as usize);
           let old_nodes = BitSlice::<usize>::from_words(&self.nodes[..]);
           let with_nodes = BitSlice::<usize>::from_words(&with.nodes[..]);
           set.or_range_at(0, &old_nodes, 0..lo as usize);
           set.or_range_at(lo as usize, &with_nodes, 0..added as usize);
           set.or_range_at((lo + added) as usize, &old_nodes, hi as usize..self.len() as usize);
           debug_assert_eq!(set.count(), node_parents.len(), "a node bit for each node");
           let nodes = set.into_words();
           // alloc: as `words`
           let ranks = ranks_of(&nodes).expect("a tree of fewer than 2^32 elements");
   ```

Status: open

> forwarded to frontend-tree (holds the syntax crates; format is DONE) (from requests, pass 17; told by tmux).
> frontend-tree (2026-10-29): handed to mem-format-tree, which holds `flat.rs` for its
> `v2/requests/mem-format-tree.md` sections 2 and 3. Section 2 rewrites the same columns
> (`Kept`'s `bits`/`ranks`, `nodes`/`ranks`, the edit's node bitmap) into one block of `Col<T>`s,
> so the two edits can't both apply as written. mem-format-tree folds this in. It keeps
> `blaze_ids::bits`' `rank_in`/`ranks_of` and the `BitSet` splice for the edit's bitmap, and
> places their results in the block. If `Ranked` can't live in the block, `Kept` keeps two
> `Col`s read through `rank_in`. Same bits, ranks and `heap_size` either way.
> applied by mem-format-tree (2026-10-29), inside the block of `mem-format-tree.md` section 2:
> - The local `rank_in` is gone. `FlatTree::rank`, `pack`'s kept bits and `Kept::slot` call
>   `blaze_ids::bits::rank_in`. `Kept::slot` is `BitSlice::contains(rank)` (false past the
>   words, like the old `get(word)?`), then `hashes[rank_in(bits, ranks, rank)]`. `pack` checks
>   each kept element is a node with `BitSlice::contains` and sets its bit with
>   `BitSliceMut::insert`.
> - `append_bits` is gone. The edit's node bits are three `or_range_at` calls into the
>   thread's spare words, zeroed with `resize`, so a warm edit allocates nothing new. `pack`
>   checks the count against `node_parents` (`debug_assert_eq!`, as before).
> - Not `Ranked` or `ranks_of`, since each returns boxes of its own: a second and third
>   allocation per tree on top of the block, which breaks
>   `a_warm_build_copy_or_edit_allocates_its_block_alone`. `Kept` keeps its `bits` and
>   `ranks` `Col`s, read through `rank_in` (this request's own alternative). `pack` fills
>   `ranks` and the kept ranks in the block with `Block::fill`, using the same rule as
>   `ranks_of`: the ones in the words before. The tree is already held under `NO_PARENT`
>   elements, so the sum can't pass `u32::MAX`.
> - Same bits, ranks and `heap_size`. `rustfmt --check` shows no diff in these hunks, and
>   `hot_lint` finds nothing on them. Not built (no builds for this lane).

## dedup: bits: span `ast_id.rs` body slots onto `blaze_ids::bits::BitArray`, and `lines.rs`'s `below`

From `dedup-bits-store` (report `/tmp/blaze-lanes/v2-dedup-bits-store.md`; parent `dedup-bits`).
`blaze_ids::bits::BitArray<W, I>` (`crates/blaze-ids/src/bits.rs`) is `W` words inline:
`contains`, `insert_range` (a word at a time) and `EMPTY`. span depends on blaze-ids already.
Same slots found (the same scan, `rfind` of a taken slot in the window), same marks, same
stack array of `SLOTS / 64` words.

1. `crates/span/src/ast_id.rs`: imports, before: `use blaze_ids::radix;` after:
   `use blaze_ids::{bits::BitArray, radix};`

   Above `fn take_slots` add:

   ```rust
   /// A bit per hash value a body may be named by: taken or not.
   type Slots = BitArray<{ (SLOTS / 64) as usize }, u32>;
   ```

   `take_slots`, before:

   ```rust
   fn take_slots(used: &mut [u64], start: u16, slots: u32) -> u32 {
       assert!(slots <= SLOTS, "a fn body too long to number its nodes ({slots} slots)");
       let taken = |used: &[u64], it: u32| used[(it / 64) as usize] & (1 << (it % 64)) != 0;
       let mut first = u32::from(start);
       for _ in 0..2 * SLOTS {
           if first + slots > SLOTS {
               first = 0;
           }
           match (first..first + slots).rfind(|&it| taken(used, it)) {
               Some(it) => first = it + 1,
               None => {
                   for it in first..first + slots {
                       used[(it / 64) as usize] |= 1 << (it % 64);
                   }
                   return first;
               }
           }
       }
   ```

   after:

   ```rust
   fn take_slots(used: &mut Slots, start: u16, slots: u32) -> u32 {
       assert!(slots <= SLOTS, "a fn body too long to number its nodes ({slots} slots)");
       let mut first = u32::from(start);
       for _ in 0..2 * SLOTS {
           if first + slots > SLOTS {
               first = 0;
           }
           match (first..first + slots).rfind(|&it| used.contains(it)) {
               Some(it) => first = it + 1,
               None => {
                   used.insert_range(first..first + slots);
                   return first;
               }
           }
       }
   ```

   `place_bodies`, before: `let mut used = [0u64; (SLOTS / 64) as usize];` after:
   `let mut used = Slots::EMPTY;` (`take_slots(&mut used, ...)` is unchanged).

2. `crates/span/src/lines.rs` stays its own structure: `LineIndex` keeps one `u32` rank per
   eight words (512 bytes of text, 1/16 bit a byte) and a select, where a
   `blaze_ids::bits::Ranked` keeps one a word (1/2 bit a byte, 8x the table, for a bitmap as
   long as the text). Only its copy of `below` goes. Imports, after `use std::fmt;` add a
   blank line and `use blaze_ids::bits::below;` (in the `text_size`/`unicode_width` group:
   `use blaze_ids::bits::below;` then `use text_size::TextSize;`). Delete:

   ```rust
   /// The bits of a word below bit `n`, for `n` below 64.
   fn below(n: usize) -> u64 {
       (1u64 << n) - 1
   }
   ```

   and in `rank` and `start_before`, `below(at % 64)` becomes `below(at)` (two sites;
   `bits::below(bit)` is the bits of `bit`'s word below it, `bit % 64` taken inside). Add to
   the doc of `LineIndex::bits`, before:

   ```rust
       /// Bit `i % 64` of word `i / 64` is set when byte `i` is `\n`. 1 bit a byte.
       bits: Box<[u64]>,
   ```

   after:

   ```rust
       /// Bit `i % 64` of word `i / 64` is set when byte `i` is `\n`. 1 bit a byte. Ranked a
       /// block of [`BLOCK`] words at a time, not a word at a time as a `blaze_ids::bits::Ranked`.
       bits: Box<[u64]>,
   ```

Status: open

> forwarded to db (db-expand-macros-next1 holds `crates/span/`) (from requests, pass 17; told by tmux).

> frontend-tree (2026-10-29): taken; frontend-tree holds `crates/span/` now. Child
> `frontend-tree-span` applies it and answers here.

> frontend-tree-span (2026-10-29): applied as written. `ast_id.rs`: `type Slots = BitArray<{ (SLOTS / 64)
> as usize }, u32>`, `take_slots(&mut Slots, ..)` with `contains`/`insert_range`, `Slots::EMPTY` in
> `place_bodies`. `lines.rs`: `use blaze_ids::bits::below;`, the local `below` gone, `below(at)` at
> both sites, the doc line on `LineIndex::bits`.

## dedup: bits: parser `TokenSet` onto `blaze_ids::bits::BitArray`, and `scan.rs` kept

From `dedup-bits-store` (report `/tmp/blaze-lanes/v2-dedup-bits-store.md`; parent `dedup-bits`).
`blaze_ids::bits::BitArray<3>` (`crates/blaze-ids/src/bits.rs`) is three words inline with
`const fn`s `EMPTY`, `with_bit`, `or`, `has_bit`. `TokenSet` keeps its invariant (token kinds
only, the `debug_assert!`s) and its `const` API, so the grammar's `const` sets don't change;
only its bit arithmetic goes. Same bits; `has_bit` past the words is `false` where the array
index panicked, and no kind reaches past them (each is at most `SHEBANG`, asserted).

1. `crates/parser/Cargo.toml`: move `blaze-ids.workspace = true` from `[dev-dependencies]` to
   `[dependencies]` (after `blaze-mem.workspace = true`). blaze-ids depends on blake3,
   blaze-radix, serde, smallvec, thin-vec (and blaze-check under `cfg(blaze_check)`), none of
   which depends on parser.

2. `crates/parser/src/token_set.rs`, before:

   ```rust
   use crate::SyntaxKind;

   /// A bit-set of `SyntaxKind`s
   #[derive(Clone, Copy)]
   pub(crate) struct TokenSet([u64; 3]);
   ```

   after:

   ```rust
   use blaze_ids::bits::BitArray;

   use crate::SyntaxKind;

   /// A bit-set of `SyntaxKind`s
   #[derive(Clone, Copy)]
   pub(crate) struct TokenSet(BitArray<3>);
   ```

   before:

   ```rust
       pub(crate) const EMPTY: TokenSet = TokenSet([0; 3]);

       pub(crate) const fn new(kinds: &[SyntaxKind]) -> TokenSet {
           let mut res = [0; 3];
           let mut i = 0;
           while i < kinds.len() {
               let discriminant = kinds[i] as usize;
               debug_assert!(
                   discriminant <= LAST_TOKEN_KIND_DISCRIMINANT,
                   "Expected a token `SyntaxKind`"
               );
               let idx = discriminant / 64;
               res[idx] |= 1 << (discriminant % 64);
               i += 1;
           }
           TokenSet(res)
       }

       pub(crate) const fn union(self, other: TokenSet) -> TokenSet {
           TokenSet([self.0[0] | other.0[0], self.0[1] | other.0[1], self.0[2] | other.0[2]])
       }

       pub(crate) const fn contains(&self, kind: SyntaxKind) -> bool {
           let discriminant = kind as usize;
           debug_assert!(
               discriminant <= LAST_TOKEN_KIND_DISCRIMINANT,
               "Expected a token `SyntaxKind`"
           );
           let idx = discriminant / 64;
           let mask = 1 << (discriminant % 64);
           self.0[idx] & mask != 0
       }
   ```

   after:

   ```rust
       pub(crate) const EMPTY: TokenSet = TokenSet(BitArray::EMPTY);

       pub(crate) const fn new(kinds: &[SyntaxKind]) -> TokenSet {
           let mut res: BitArray<3> = BitArray::EMPTY;
           let mut i = 0;
           while i < kinds.len() {
               let discriminant = kinds[i] as usize;
               debug_assert!(
                   discriminant <= LAST_TOKEN_KIND_DISCRIMINANT,
                   "Expected a token `SyntaxKind`"
               );
               res = res.with_bit(discriminant);
               i += 1;
           }
           TokenSet(res)
       }

       pub(crate) const fn union(self, other: TokenSet) -> TokenSet {
           TokenSet(self.0.or(&other.0))
       }

       pub(crate) const fn contains(&self, kind: SyntaxKind) -> bool {
           let discriminant = kind as usize;
           debug_assert!(
               discriminant <= LAST_TOKEN_KIND_DISCRIMINANT,
               "Expected a token `SyntaxKind`"
           );
           self.0.has_bit(discriminant)
       }
   ```

   The test `token_set_works_for_tokens` stays.

3. `crates/parser/src/lexed_str/scan.rs` keeps its blocks: each 64 bytes of text is one
   `[u64; 4]` of four classes side by side, so one step of the lexer loads the bits of every
   class it asks from one place, and a search runs to the sentinel bits set past the text's
   end with no bound check; four `blaze_ids::bits` sets would put each class in its own
   buffer. Add to the doc of `Block`, before:

   ```rust
   /// The classes of 64 bytes: a bit per byte in each of [`IDENT`], [`START`] (first [`SPACE`]),
   /// [`NEWLINE`] and [`QUOTE`].
   type Block = [u64; 4];
   ```

   after:

   ```rust
   /// The classes of 64 bytes: a bit per byte in each of [`IDENT`], [`START`] (first [`SPACE`]),
   /// [`NEWLINE`] and [`QUOTE`]. Interleaved, not four `blaze_ids::bits` sets: a step of the
   /// lexer reads every class of its bytes from one block.
   type Block = [u64; 4];
   ```

Status: open

> forwarded to frontend-tokens (holds `crates/parser/`) (from requests, pass 17; told by tmux).
> done by frontend-tokens: items 1-3 as written. `blaze-ids.workspace = true` moved to parser's `[dependencies]` (sorted, before `blaze-mem`); no cycle (blaze-ids also depends on blaze-mem, which parser already uses, and nothing of it reaches parser). `TokenSet` on `BitArray<3>` (`EMPTY`, `with_bit`, `or`, `has_bit`, all `const`, `Copy` for inline words), the test kept; `scan.rs` `Block` doc added. manifest_lint clean, hot_lint nothing in either file. Nothing built.

## dedup: bits: intern `table.rs` `Live` and the image's live bits onto `blaze_ids::bits`

From `dedup-bits-store` (report `/tmp/blaze-lanes/v2-dedup-bits-store.md`; parent `dedup-bits`).
`dedup-intern`, which was editing this file, is done; `mem-format` names `crates/intern` and is told.
`blaze_ids::bits` (`crates/blaze-ids/src/bits.rs`): `BitSet<u32>` for `Live` (`insert_growing`
grows the `Vec` to the marked word exactly as `resize(word + 1, 0)` did, so its words, `count`
and `PartialEq` are the same; `Debug` prints members); `BitSlice<'static>` for the image's
mapped live words (`contains` is `false` past them where the index panicked; every caller asks
an index below `count`); `BitSet<usize>` for the words `write` emits, written as the same
`u64`s. No layout, schema or byte of the image changes. intern depends on blaze-ids already.

`crates/intern/src/table.rs`:

1. Imports: after `use blaze_format::SchemaHash;` add
   `use blaze_ids::bits::{BitSet, BitSlice};` (before `use blaze_image::...`).

2. `Base`, before: `    live: &'static [u64],` after: `    live: BitSlice<'static>,`
   (`#[derive(Default)]` still holds: an empty slice). `Base::is_live`, before / after:

   ```rust
           self.live[index / 64] >> (index % 64) & 1 == 1
   ```

   ```rust
           self.live.contains(index)
   ```

3. In the restore (~line 419), before / after:

   ```rust
               live: image.slice::<u64>(section, root.live, count.div_ceil(64))?,
   ```

   ```rust
               live: BitSlice::from_words(image.slice::<u64>(section, root.live, count.div_ceil(64))?),
   ```

4. In the check (~line 471), before / after:

   ```rust
           if count % 64 != 0 && base.live[count / 64] >> (count % 64) != 0 {
   ```

   ```rust
           if base.live.first_in(count..base.live.capacity()).is_some() {
   ```

   (`count` is a `usize` there, as `count / 64` indexed with it.)

5. `Live`, before:

   ```rust
   pub struct Live {
       bits: Vec<u64>,
   }

   impl Live {
       pub fn new() -> Live {
           Live::default()
       }

       pub fn mark_id(&mut self, id: u32) {
           let word = id as usize / 64;
           if word >= self.bits.len() {
               self.bits.resize(word + 1, 0);
           }
           self.bits[word] |= 1 << (id % 64);
       }

       pub fn contains(&self, id: u32) -> bool {
           self.bits.get(id as usize / 64).is_some_and(|word| word >> (id % 64) & 1 == 1)
       }

       /// How many IDs are marked.
       pub fn count(&self) -> usize {
           self.bits.iter().map(|word| word.count_ones() as usize).sum()
       }
   }
   ```

   after:

   ```rust
   pub struct Live {
       bits: BitSet<u32>,
   }

   impl Live {
       pub fn new() -> Live {
           Live::default()
       }

       pub fn mark_id(&mut self, id: u32) {
           self.bits.insert_growing(id);
       }

       pub fn contains(&self, id: u32) -> bool {
           self.bits.contains(id)
       }

       /// How many IDs are marked.
       pub fn count(&self) -> usize {
           self.bits.count()
       }
   }
   ```

6. `Table::write`, before / after:

   ```rust
           let mut live_bits = vec![0u64; count.div_ceil(64)];
   ```

   ```rust
           let mut live_bits = BitSet::<usize>::new(count);
   ```

   ```rust
                   live_bits[index / 64] |= 1 << (index % 64);
   ```

   ```rust
                   live_bits.insert(index);
   ```

   ```rust
               live: out.slice(&live_bits),
   ```

   ```rust
               live: out.slice(live_bits.words()),
   ```

Status: open

> forwarded to requests-ids (requests child holding `crates/intern/`) (from requests, pass 17; told by tmux).
> applied by requests (child requests-ids): all six edits in `crates/intern/src/table.rs` as written (`Live` on `BitSet<u32>`, `Base.live` a `BitSlice<static>`, the past-the-end check via `first_in`, `write` emits `BitSet<usize>` words); no layout or schema change.

## frontend-hooks: `BodySyntax::text` is a `syntax::Text` (lane format, `crates/hir-def/src/expr_store/body.rs`; 2026-10-29)

From frontend-hooks, which applied db's side of frontend-text's "the parse tree's source is a
`Text`; `FileText` and `BodySyntax` hold one" (`v2/requests/db.md`): `FileText::text` returns
`&syntax::Text` now (`crates/base-db/src/lib.rs`). `BodySyntax` is format's, so its part is here:

- `body.rs:352`, in `pub struct BodySyntax`: old `text: Arc<str>,` (triomphe's), new
  `text: syntax::Text,`.
- `body.rs:418`: old
  `true => ast::SourceFile::parse_outline(std::sync::Arc::from(self.text()), self.edition),`
  new `true => ast::SourceFile::parse_outline(self.text(), self.edition),` (`parse_outline` takes
  `impl Into<syntax::Text>`, so `Arc::from` no longer infers; a `&str` is a copy into an `Arc`, as
  before).
- `body.rs:635-636`: the fill `text: db.file_text(file).text(db).clone(),` is unchanged; its
  comment `// clone: an \`Arc<str>\` of the file's text (a refcount)` becomes
  `// clone: a count of the file's text`.

`&self.text[self.range]` (L411) compiles as is (`Text: Deref<Target = str>`). Why: frontend.md
item 6, the file's text is the VFS's bytes from the VFS to the parse tree, with no copy; without
the first line `body.rs` doesn't compile against base-db.

> forwarded to mem-format (mem-format-hir-apply, which holds `hir-def/src/expr_store/`; format is DONE) (requests, pass 20; told by tmux).
> applied by mem-format-hir-apply (2026-10-29): all three edits, as written (`body.rs` L352,
> L418 and the comment at L635). `FileText::text` returns `&syntax::Text` as stated, and the
> field's only other use, `&self.text[self.range]`, goes through `Deref<Target = str>`.
> Status: applied.

## dedup: ARITH_OPS is ArithOp::ALL, the hir-def half (from dedup-scan, filed by db-apply-ty 2026-10-29)

From `v2/requests/db.md` `## dedup: ARITH_OPS is ArithOp::ALL`; db-apply-ty applies the hir-ty
half (`crates/hir-ty/src/infer/persist.rs`). `syntax::ast::ArithOp::ALL` is in
(`crates/syntax/src/ast/operators.rs:67`), in the same declaration order as the const below, so
every persisted code decodes as before.

`crates/hir-def/src/hir/columns.rs`:
- delete `const ARITH_OPS: [ArithOp; 10] = [ ArithOp::Add, … ArithOp::BitAnd, ];` (~L520-531);
- L571 `3..=12 => BinaryOp::ArithOp(ARITH_OPS[(word - 3) as usize]),` ->
  `3..=12 => BinaryOp::ArithOp(ArithOp::ALL[(word - 3) as usize]),`
- L578 `20..=29 => BinaryOp::Assignment { op: Some(ARITH_OPS[(word - 20) as usize]) },` ->
  `20..=29 => BinaryOp::Assignment { op: Some(ArithOp::ALL[(word - 20) as usize]) },`
- L2030-2031 `ARITH_OPS.into_iter()` -> `ArithOp::ALL.into_iter()` (both lines).
The `use syntax::ast::{ArithOp, ..}` at L95 stays.
Status: open

> forwarded to mem-format (holds format's hir-def `hir/` files; format is DONE; told by tmux; requests, pass 22).
> applied by mem-format-fwd: all four edits in `crates/hir-def/src/hir/columns.rs` as written: the const is gone, the decode at ~L559 and ~L566 and the test's two lines (~L2018) read `ArithOp::ALL`, whose order (`syntax/src/ast/operators.rs` L67) is the const's, so every code decodes as before. The `ArithOp` import stays. Status: applied.

## dedup: `blaze_format::Placed`, a column written for a later `Slice` (from dedup-sim-front, 2026-10-29)

hir-ty has three identical `struct Placed { at: u64, len: usize }` with `push` and `slice`
(`consteval.rs`, which also has `push_each`, `infer/persist.rs`, `lower/persist.rs`): where a
column of records was written with a `Writer`, read back as a `Slice`. That is blaze-format's
kind of thing, so the one copy is a new file, already written: `crates/blaze-format/src/placed.rs`
(`Placed`, `EMPTY`, `push`, `push_each`, `slice`, with a proptest against the old copy and
`push_each` against `push`). Its module line is the one edit asked here, in
`crates/blaze-format/src/lib.rs`:

- after `pub mod leb;` add `mod placed;`
- after `pub use crate::schema::{Part, SCHEMA_DOMAIN, schema, schema_of};` add
  `pub use crate::placed::Placed;`

The hir-ty side is `v2/requests/db.md` under the same heading. hir-def's
`hir/persist.rs` `Column { at: u64, len: u32 }` stays its own: `u32` lengths, read across the
crate, and it pushes an empty column too.

> applied (requests, pass 30; lane format is DONE): `crates/blaze-format/src/lib.rs` has `mod placed;` after `pub mod leb;` and `pub use crate::placed::Placed;` after the schema re-export. proptest is already a dev-dependency.

## dedup: `intern::order_key` is `blaze_ids::names::order_key` (from dedup-sim-front, 2026-10-29)

`crates/intern/src/symbol.rs` `pub const fn order_key(bytes: &[u8]) -> u64` and
`crates/blaze-ids/src/names.rs` `order_key` are the same function line for line (blaze-ids'
doc says "The same key as `intern::order_key`"). fix-orderkeys kept two because intern was not
to depend on blaze-ids; it does now (`symbol.rs` imports `blaze_ids::{ContentId, Hasher}`). In
`crates/intern/src/symbol.rs`:

- delete the doc comment, `#[inline]` and body of `pub const fn order_key`, and in their place
  write `pub use blaze_ids::names::order_key;` (intern's `lib.rs` re-exports `order_key` from
  this module; that line stays).
- the `order_keys` test stays: it now checks `Symbol::order_key` against the shared function.

Same keys: the function is the same, `const` included, so `Text { key: order_key(..), .. }` in
a `const` context still compiles.

> applied (requests, pass 31; lane format is DONE): `crates/intern/src/symbol.rs` has `pub use blaze_ids::names::order_key;` in place of its copy. blaze-ids' doc says intern re-exports it.

## dedup: one persisted code per binary operator, `BinaryOp::code` (from dedup-sim-front, 2026-10-29)

`crates/hir-def/src/hir/columns.rs` `binary_op_word`/`binary_op` and
`crates/hir-ty/src/infer/persist.rs` `binary_code`/`binary_of` (with `arith_code`) are one table
of the 29 binary operators, hir-def's word being hir-ty's code plus one (0 there is no
operator). The table goes beside the operators, in syntax; each format keeps its words. The
hir-ty half is `v2/requests/db.md` under the same heading.

1. `crates/syntax/src/ast/operators.rs`: in `impl ArithOp` after `ALL`, add

```rust
    /// Its place in [`ArithOp::ALL`]: a persisted code, which never changes.
    pub const fn code(self) -> u32 {
        self as u32
    }
```

   and after `impl ArithOp`, add

```rust
impl BinaryOp {
    /// How many [`BinaryOp::code`]s there are.
    pub const CODES: u32 = 29;

    /// `&&` and `||` at 0 and 1, the ten arithmetic operators from 2 in [`ArithOp::ALL`]'s
    /// order, `==` and `!=` at 12, `<=`, `<`, `>=`, `>` from 14, `=` at 18 and the compound
    /// assignments from 19: persisted codes, which never change.
    pub const fn code(self) -> u32 {
        match self {
            BinaryOp::LogicOp(LogicOp::And) => 0,
            BinaryOp::LogicOp(LogicOp::Or) => 1,
            BinaryOp::ArithOp(op) => 2 + op.code(),
            BinaryOp::CmpOp(CmpOp::Eq { negated }) => 12 + negated as u32,
            BinaryOp::CmpOp(CmpOp::Ord { ordering, strict }) => {
                14 + 2 * ordering as u32 + strict as u32
            }
            BinaryOp::Assignment { op: None } => 18,
            BinaryOp::Assignment { op: Some(op) } => 19 + op.code(),
        }
    }

    /// The operator [`BinaryOp::code`] gave `code`.
    pub const fn from_code(code: u32) -> Option<BinaryOp> {
        Some(match code {
            0 => BinaryOp::LogicOp(LogicOp::And),
            1 => BinaryOp::LogicOp(LogicOp::Or),
            2..12 => BinaryOp::ArithOp(ArithOp::ALL[code as usize - 2]),
            12 | 13 => BinaryOp::CmpOp(CmpOp::Eq { negated: code == 13 }),
            14..18 => BinaryOp::CmpOp(CmpOp::Ord {
                ordering: if code >= 16 { Ordering::Greater } else { Ordering::Less },
                strict: code % 2 == 1,
            }),
            18 => BinaryOp::Assignment { op: None },
            19..29 => BinaryOp::Assignment { op: Some(ArithOp::ALL[code as usize - 19]) },
            _ => return None,
        })
    }
}
```

   and at the end of the file the tests, with both old encoders copied in as references:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// hir-ty's `binary_code`, as it was.
    fn infer_code(op: BinaryOp) -> u32 {
        let arith = |op: ArithOp| ArithOp::ALL.iter().position(|it| *it == op).unwrap() as u32;
        match op {
            BinaryOp::LogicOp(LogicOp::And) => 0,
            BinaryOp::LogicOp(LogicOp::Or) => 1,
            BinaryOp::ArithOp(op) => 2 + arith(op),
            BinaryOp::CmpOp(CmpOp::Eq { negated }) => 12 + u32::from(negated),
            BinaryOp::CmpOp(CmpOp::Ord { ordering, strict }) => {
                14 + 2 * u32::from(ordering == Ordering::Greater) + u32::from(strict)
            }
            BinaryOp::Assignment { op: None } => 18,
            BinaryOp::Assignment { op: Some(op) } => 19 + arith(op),
        }
    }

    /// hir-def's `binary_op_word`, as it was.
    fn column_word(op: Option<BinaryOp>) -> u32 {
        match op {
            None => 0,
            Some(BinaryOp::LogicOp(LogicOp::And)) => 1,
            Some(BinaryOp::LogicOp(LogicOp::Or)) => 2,
            Some(BinaryOp::ArithOp(op)) => 3 + op as u32,
            Some(BinaryOp::CmpOp(CmpOp::Eq { negated })) => 13 + negated as u32,
            Some(BinaryOp::CmpOp(CmpOp::Ord { ordering, strict })) => {
                15 + 2 * ordering as u32 + strict as u32
            }
            Some(BinaryOp::Assignment { op: None }) => 19,
            Some(BinaryOp::Assignment { op: Some(op) }) => 20 + op as u32,
        }
    }

    #[test]
    fn codes_are_the_ones_persisted() {
        for code in 0..BinaryOp::CODES {
            let op = BinaryOp::from_code(code).expect("every code below CODES is an operator");
            assert_eq!(op.code(), code);
            assert_eq!(infer_code(op), code, "hir-ty's code of {op:?}");
            assert_eq!(column_word(Some(op)), code + 1, "hir-def's word of {op:?}");
        }
        assert_eq!(BinaryOp::from_code(BinaryOp::CODES), None);
        for (code, op) in ArithOp::ALL.into_iter().enumerate() {
            assert_eq!(op.code() as usize, code);
        }
    }
}
```

2. `crates/hir-def/src/hir/columns.rs`: `binary_op_word`'s body becomes
   `op.map_or(0, |op| 1 + op.code())`, and `binary_op`'s

```rust
fn binary_op(word: u32) -> Checked<Option<BinaryOp>> {
    match word {
        0 => Ok(None),
        word => BinaryOp::from_code(word - 1).map(Some).ok_or("a binary operator out of range"),
    }
}
```

   (`Ordering`, `LogicOp`, `CmpOp` and `ArithOp` leave this file's imports if `rg` finds no
   other use there). The words written are the same: the test above holds `code + 1` to the old
   `binary_op_word`.

> applied (requests, pass 31; lane format is DONE): `ArithOp::code`, `BinaryOp::{CODES, code, from_code}` and the tests are in `crates/syntax/src/ast/operators.rs`. `hir-def/src/hir/columns.rs`'s `binary_op_word`/`binary_op` are as written. `ArithOp`, `CmpOp`, `LogicOp` and `Ordering` moved from the file's imports into its tests module, which still uses them.
