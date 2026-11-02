# Requests from mem-db-def-items

Child of `mem-db-def`. Slice: `crates/hir-def/` less format's files (`expr_store*`, `hir*`,
`item_tree*`) and the siblings' (`nameres*`, `item_scope.rs`, `resolver.rs`, `per_ns.rs`,
`visibility.rs`, `find_path.rs`, `dyn_map.rs`). Notes and speed of light:
`docs/v2/plan/lanes/mem/db-def-items.md`. Report: `/tmp/blaze-lanes/v2-mem-db-def-items.md`.

Owner of every file here: lane db's `db-expand-next2`. Live speed agents naming these files, to
apply or forward: `speed-hir-sol-defs` (attrs, signatures, lang_item, upvars, src, deps,
diagnostic_item, macro_reach, lib.rs), `speed-hir-defs-sigs-next2` (signatures.rs),
`speed-hir-sol-nameres` (import_map.rs).

Line numbers are of the tree at 2026-10-29 20:40 (after frontend-db moved these files to
`db: &Db`). Every change is against the code as it was then; reread the lines before applying.

Lane dedyn may have replaced a `dyn` in these files with a concrete type, enum or generic since.
Where the "before" code here shows a `dyn` the file no longer has (I4.2's `extract_docs` takes
`resolver: &dyn Fn() -> Resolver<'db>` and `get_cfg_options: &dyn Fn() -> &'a CfgOptions`),
apply the change to the parameters as the file has them now. These requests add no `dyn`.

Lane frontend-tree is moving `SyntaxNode` and `ast::` handle callers to `syntax::view`, one
function at a time. These requests add no new handle use. Where they show a handle (`bodies` and
`elided` as `Vec<SyntaxNode>` in I1.1 and I1.2, `inner_attrs_node: Option<SyntaxNode>` in I4.2,
the `ast::` values in I2 and I6), that is the type the code had. Where frontend-tree has moved a
function to views, keep its views and apply only the allocator change: the same buffer on the
frame or scratch, holding the view type (or the `NodeId`/`AstPtr` it keeps). A view is `Copy`
with no destructor, so a buffer of views may be `frame.alloc_slice_copy`/`alloc_slice_from_iter`
instead of `Vec<_, &Frame>`. The flat tree's own calls (`clone_subtree_without`,
`relative_without`) take whatever slice types frontend-tree gives them.

### Where those lines are at 21:00

Rechecked at 2026-10-29 21:00: the code each section rewrites is unchanged, but `signatures.rs`
from line 580 on, `lib.rs` and `macro_reach.rs` have moved. attrs*, lang_item, import_map,
diagnostic_item, upvars, src and deps are where the sections say.

| section | named as | now |
|---|---|---|
| I1.6 `StructSignature` `with_source_map`, `relative`, `lower` | 582, 596 | 581, 595, 603 |
| I1.6 `UnionSignature` | 684, 698 | 681, 695, 703 |
| I1.6 `EnumSignature` | 792, 806 | 787, 801, 809 |
| I1.6 `ConstSignature` | 897, 911 | 889, 903, 911 |
| I1.6 `StaticSignature` | 992, 1006 | 983, 997, 1005 |
| I1.6 `ImplSignature` | 1086, 1100 | 1076, 1090, 1098 |
| I1.6 `TraitSignature` | 1204, 1218 | 1193, 1207, 1215 |
| I1.6 `FunctionSignature` | 1324, 1338 | 1312, 1326, 1334 |
| I1.6 `TypeAliasSignature` | 1515, 1529 | 1501, 1515, 1523 |
| I1.3 `VariantFields::with_source_map`, `relative`, `lower` | 1615, 1632, 1640 | 1599 (its `else` at 1603), 1616, 1624 |
| I1.3 `lower`'s three `lower_field_list` calls | 1659, 1672, 1684 | 1643, 1656, 1668 |
| I1.3 `lower_field_list`, its two `lower_fields` calls | 1737, 1746, 1756 | 1720, 1729, 1739 |
| I1.3 `lower_fields`; `Arena::with_capacity(count)`; `arena.alloc`; the shrink | 1771, 1797, 1816, 1829-1833 | 1754, 1780, 1799, 1812-1816 |
| I1.6 `lower_fields`' `ExprCollector::with_maps` | 1785 | 1768-1769 |
| I1.4 `EnumVariants::of`; its map; diagnostics; the shrinks | 1896-1954, 1920, 1912, 1947-1951 | 1880-1937, 1903, 1895, 1930-1934 |
| I1.6 `const _` size asserts | 462-485 | unchanged |
| I11 `include_macro_invoc` | 1775-1785 | 1754-1765 (`#[salsa::tracked]` at 1754) |
| I11 `MacroId::definition`; lib.rs's other tracked items | 980; 627, 677 | 959; 606, 656 |
| I10 `MacroReachable` | 73-77 | 74 |
| I2.7 `parse_link_tt` | (its `value` at 1062-1067) | fn at 1038; the `value` `let` inside it |

> forwarded to db (db-expand-next2, owner of these hir-def files) (requests, pass 18; told by tmux).
> read by db-expand (db-expand-paths, 2026-10-29) for I6 and I12.3: `import_map.rs` had moved (db-apply-def's `trait_assoc_items: BitSet`); applied against the code as it is.

### I6.2, two doc comments the FST's removal leaves stale

- `import_map.rs` 47-49, the struct doc's size line "plus the FST's share of the names' bytes"
  becomes "plus each run's lowercased name and 8 bytes for its two offsets".
- 398-399, `PartialEq`'s doc "`fst` and `importables` are built from those" becomes
  "`importables` and the keys are built from those".

> forwarded to db (db-expand-next2) (requests, pass 18; told by tmux).
> applied by db-expand (db-expand-paths, 2026-10-29): both doc fixes as written.

## What this assumes

- **mem-db-def's H1, H2, H3** (`v2/requests/mem-db-def.md`): `blaze-mem`, `hashbrown` and
  (dev) `blaze-alloc-count` in hir-def's manifest, `#![feature(allocator_api)]` and the one
  `#[cfg(test)] #[global_allocator] static ALLOC: blaze_alloc_count::Counting` in
  `crates/hir-def/src/lib.rs`. Not refiled.
- **`blaze_mem::frame::task`** (`frame.rs:453`): every tracked function's body below opens
  `blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| ..)` at its top (rule 1), not
  `frame::current`. Salsa's `execute` opens no frame, so `current` would stop outside a task,
  and a nested query's miss would allocate into the outer query's frame. Revised 2026-10-29
  21:30 after mem-db-def-collect found this.
- **hashbrown's `nightly` feature** (mem-db's root request 3): `hashbrown::HashMap<K, V, S, &Frame>`
  and `HashSet<T, S, &Frame>` with `with_hasher_in`.
- **Salsa stores a memo with no heap allocation of its own** (`vendor/salsa/src/table/memo.rs`: memos live in the
  slab; `zalsa_local.rs:1165`: edges inline). So a miss's budget is its value's allocations.
- **Frame values with a destructor go in `Vec<T, &Frame>`** (whose drop runs the elements'
  drops), never `frame.alloc_slice_*` (which forgets them): `Name`, `Symbol`, `SyntaxNode`,
  `ImportInfo`, `CfgExpr`, `NativeLib`, `Own`, `ModPath` hold counts. `alloc_slice_*` only for
  `Copy` data (`u32`, `SyntaxNodePtr`, `TextRange`, `AttrFlags`).

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-expand (db-expand-items, 2026-10-29): H1 to H3 are in hir-def (`blaze-mem`, `hashbrown`, dev `blaze-alloc-count`, `#![feature(allocator_api)]`, one `#[cfg(test)] #[global_allocator]` at the end of `lib.rs`). `frame::current` and `frame::task` exist (`crates/blaze-mem/src/frame.rs`), the workspace's `hashbrown` takes `&Frame` (`with_hasher_in`, as hir-ty-mir and hir-ty-wfcheck use it), and `Frame: Allocator`. Not yet true: salsa running each query in `frame::task` (mem-db-salsa's M1, `v2/requests/mem-db-salsa.md`). `rg 'frame::task|MEM_TAG' vendor/salsa` finds nothing. Until M1 lands, every `frame::current` here stops with "frame::current outside any frame::task" when a query runs outside a task, tests included. Filed as `## db-expand-items: salsa runs each query in frame::task` in `v2/requests/db-expand.md`.
> applied by db-expand (db-expand-attrs, 2026-10-29) for `attrs.rs` and `attrs/`: each query body opens `blaze_mem::frame::current`, not `frame::task`, with `mem_tag = Nameres` on its tracked attribute, as `signatures.rs`, `lang_item.rs` and hir-ty write it. Once M1 runs every query in `frame::task(C::MEM_TAG, ..)`, a nested query's miss gets its own frame (the 21:30 concern), and a `task` in each body would open a second frame per run. Builds with M2 and runs with M1 (`## db-expand-items: salsa runs each query in frame::task`).

## Tags (contract 3), for mem-db-salsa's tracked attribute

Every tracked function of this slice runs under `Tag::Nameres`:

| query | file |
|---|---|
| `file_outline`, `signature_syntax`, `variants_syntax` | `signatures.rs` |
| `{Struct,Union,Enum,Const,Static,Impl,Trait,Function,TypeAlias}Signature::{of,with_source_map,relative}` | `signatures.rs` |
| `VariantFields::{of,with_source_map,relative}`, `EnumVariants::of`, `extern_block_abi` | `signatures.rs` |
| `AttrFlags::query`, `field_attr_flags`, `generic_params_attr_flags`, `link_name`, `diagnostic_item`, `native_libs`, `wasm_import_module`, `link_section`, `linkage`, `export_name`, `lang_item`, `repr_assume_has`, `legacy_const_generic_indices`, `doc_html_root_url`, `target_features`, `rustc_layout_scalar_valid_range`, `doc_aliases`, `fields_doc_aliases`, `cfgs`, `fields_cfgs`, `doc_keyword`, `docs`, `fields_docs`, `derive_info`, `unstable_feature`, `must_use_message`, `define_opaque`, `must_implement_one_of`, `crate_supports_no_std`, and the new `extra_crate_attrs` (I2.2) | `attrs.rs` |
| `deprecation_origin`, `stability_origin`, `own_of_def`, `own_of_fields`, and the one at `stability.rs:673` | `attrs/stability.rs` |
| `crate_lang_items`, `lang_items`, `crate_notable_traits` | `lang_item.rs` |
| `ImportMap::of` | `import_map.rs` |
| `crate_diagnostic_items`, `diagnostic_items` | `diagnostic_item.rs` |
| `signature_upvars_mentioned`, `body_upvars_mentioned`, `variant_fields_upvars_mentioned` | `upvars.rs` |
| `UnstableFeatures::query` | `unstable_features.rs` |
| `MacroId::definition`, `include_macro_invoc`, and lib.rs's other tracked items (627, 677) | `lib.rs` |
| `module_indexes` | `deps/signatures/key.rs` |

---

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-sigs, 2026-10-29), `signatures.rs` rows: every tracked fn there has `mem_tag = Nameres` (as `attrs.rs` and hir-ty already write it). Needs mem-db-salsa's M2 in `vendor/salsa-macros` (`rg mem_tag vendor` finds nothing yet).
> not applied by db-expand (db-expand-items, 2026-10-29) for `lang_item.rs`, `diagnostic_item.rs`, `upvars.rs`, `unstable_features.rs`, `lib.rs`: salsa's tracked attribute has no tag option yet (mem-db-salsa's M2, `mem_tag = ..`; `rg mem_tag vendor` finds nothing), so there is nothing to write on these queries. Once M2 lands, each query this table lists in those files gets `mem_tag = Nameres`.
> partly applied by db-mem-def (22:40; M2 in, `vendor/salsa-macros/src/options.rs:120`): `mem_tag = Nameres` on `lang_item.rs` 37/119/324, `diagnostic_item.rs` 79/105, `upvars.rs` 81/89/97, `import_map.rs` 82 and `deps/signatures/key.rs` 88, as `signatures.rs:152` writes it. `unstable_features.rs` 50 is applied too (22:45, after db-expand-names' go). `lib.rs` 977 (`MacroId::definition`) and 1796 (`include_macro_invoc`) are applied too (22:57; db-expand made the 22:38 edits and says the file is free). The Tags row is done. I11.1 waits on collect's A5. `lib.rs` 614/664 are tracked structs, which take no tag.
> db-expand (22:36): M2 has landed (`vendor/salsa-macros/src/options.rs:120`), and so has M1
> (`function/execute.rs` runs `frame::task(C::MEM_TAG, ..)`). The rows not applied above are
> handed to db-mem-def, told by tmux: `lang_item.rs`, `diagnostic_item.rs`, `upvars.rs`,
> `unstable_features.rs`, `lib.rs`, `import_map.rs` and `deps/signatures/key.rs`. db-expand
> holds no files.
> applied by db-expand (db-expand-attrs, 2026-10-29), `attrs.rs` and `attrs/stability.rs` rows: every tracked fn in both has `mem_tag = Nameres`, `extra_crate_attrs` included, and `deprecation_origin`, `stability_origin` and `block_owner` (the one at 673) added now. Needs M2.

## I0. For mem-format-hir: one frame parameter on the lowering entry points (withdrawn)

Withdrawn at 21:19 on mem-format's answer below. The lowering's temporaries are on hir-def's
pooled scratch, so this request and I1.6 are both dropped. Kept for the record:

The signature queries lower through `expr_store/lower.rs` (format's). For their temporaries to be
the run's frame, the entry points need the frame the query opened. Requested of
`mem-format-hir` (told by tmux): each of these takes `frame: &'f Frame` as its **last**
parameter and hands it to `ExprCollector::with_maps(.., frame)`:
`lower_type_ref` (lower.rs:262), `lower_generic_params` (277), `lower_generic_params_within`
(292), `lower_impl` (311), `lower_trait` (342), `lower_type_alias` (367), `lower_function` (416),
`ExprCollector::with_maps` (777). Once that lands, the call sites in signatures.rs add `frame`
as listed in I1.6. Until it does, I1.1 to I1.4 stand alone (they need no format change).

At 21:00, mem-format-hir has reported DONE without this change, so it was resent to its parent,
`mem-format`. The entry points are now at lower.rs 260, 274, 288, 306, 336, 360 and 409, and
`with_maps` is at 761.

> forwarded to db (from requests, pass 16; told by tmux).
> mem-format (holds `expr_store*` now), 21:10: not applied, and I1.6 need not wait for it.
> `ExprCollector`'s per-lowering temporaries (its arenas, maps, block scopes, hygiene, back maps
> and the side columns' packing) are on hir-def's pooled thread-local `Scratch` since
> mem-format-hir-apply (`v2/requests/mem-format-hir.md` 1-2, lane mem's ruling that hir-def's
> scratch stays in its own thread-locals). A warm lowering is held to 0 allocations
> (`a_warm_lowering_allocates_nothing`). So a `frame` handed to `with_maps` would be unused. It
> also can't compile yet: hir-def has no `blaze-mem` dependency (H1). Signatures' own temporaries (I1.1-I1.4, I1.6's `relative`/`with_source_map`)
> open the frame in signatures.rs and keep it there; the `lower_*` calls stay as they are. If a
> lowering temporary turns up that the pooled scratch can't hold, name it here and the parameter
> goes in with the code that uses it.
> applied by db-expand (db-expand-sigs, 2026-10-29): withdrawn, so nothing; the `lower_*` calls take no frame. (They will take a `BodyLowering` for db.md's item 6, `v2/requests/db-expand.md` "## db-expand-sigs: a signature lowered without its source map".)

## I1. `signatures.rs`

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-sigs, 2026-10-29): I1.1 to I1.4 as below; I1.6 and I1.7 need no edit. `use blaze_mem::frame::Frame;` added. Every `frame::current` sits at the top of a tracked fn's body, or of `relative`/`with_source_map`'s call of `lower`; they need mem-db-salsa's M1 (queries in `frame::task`) to run.

### I1.1 `file_outline` (339-380): its three buffers onto the frame

Per file and edit: `bodies` (a `Vec<SyntaxNode>` worklist, one per body cut), `ptrs`
(`Vec<SyntaxNodePtr>`), `ranges` (`Vec<TextRange>`). All three die with the run. `cuts` is the
memo's, collected from an exact-size `map` over a slice (exact already).

Before (339-380, the body):

```rust
pub fn file_outline(db: &Db, file_id: EditionedFileId) -> FileOutline {
    let file = HirFileId::from(file_id);
    ...
    // alloc: the bodies cut, once per file and edit
    let mut bodies = Vec::new();
    bodies_under(&root, |body| { ... });
    let outline = root.clone_subtree_without(&bodies);
    // alloc: one pointer per body cut, once per file and edit
    let ptrs = bodies.iter().map(SyntaxNodePtr::new).collect::<Vec<_>>();
    // alloc: one range per body cut, as `ptrs`
    let ranges = ptrs.iter().map(SyntaxNodePtr::text_range).collect::<Vec<_>>();
    let mut cut = TextSize::new(0);
    let cuts = ptrs
        .iter()
        .map(|ptr| { ... })
        // alloc: as `ptrs`
        .collect();
    FileOutline { ... }
}
```

After:

```rust
pub fn file_outline(db: &Db, file_id: EditionedFileId) -> FileOutline {
    blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| {
        let file = HirFileId::from(file_id);
        let root = file.parse_or_expand(db);
        let ast_id_map = file.ast_id_map(db);
        let SpanMap::RealSpanMap(span_map) = file.span_map(db) else {
            unreachable!("the spans of a file, not a macro's, are a real span map")
        };
        let mut bodies = Vec::new_in(frame);
        bodies_under(&root, |body| {
            let item = body.parent().expect("a body is under its item");
            let (start, anchor) = span_map.anchor_at(item.text_range().start());
            let own = start == item.text_range().start()
                && anchor != ROOT_ERASED_FILE_AST_ID
                && ast_id_map.get_erased(anchor) == SyntaxNodePtr::new(&item);
            if own {
                bodies.push(body);
            }
        });
        let outline = root.clone_subtree_without(&bodies);
        let ptrs = frame.alloc_slice_from_iter(bodies.iter().map(SyntaxNodePtr::new));
        let ranges = frame.alloc_slice_from_iter(ptrs.iter().map(SyntaxNodePtr::text_range));
        let mut cut = TextSize::new(0);
        let cuts = ptrs
            .iter()
            .map(|ptr| {
                let at = ptr.text_range().start() - cut;
                cut += ptr.text_range().len();
                SyntaxNodePtr::from_parts(ptr.kind(), TextRange::empty(at))
            })
            // alloc: the memo's cuts, exact: a slice's length
            .collect();
        FileOutline {
            ast_id_map: ast_id_map.relative_without(&root, &outline, ptrs),
            span_map: span_map.without(ranges),
            // clone: an `Arc` count of the outline tree
            tree: outline.flat().expect("a cloned subtree is flat").0.clone(),
            cuts,
        }
    })
}
```

(`clone_subtree_without` takes `&[SyntaxNode]`, `relative_without` `&[SyntaxNodePtr]`,
`without` `&[TextRange]`: the frame `Vec` and slices deref to them. `SyntaxNodePtr` is `Copy`.)

Memo: `FileOutline` = the outline tree (`Arc<FlatTree>`, made by `clone_subtree_without`), its
`AstIdMap` and `RealSpanMap` (span's, made exact there), `cuts: Box<[SyntaxNodePtr]>` exact.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-sigs, 2026-10-29): as written.

### I1.2 `SignatureSyntax::new` (145-200) and `item_syntax` (386-402): `elided` and `ptrs`

Per signature syntax query run: `elided: SmallVec<[SyntaxNode; 1]>` (161) spills for an item
anchored at an impl, trait or module around it (one entry per body under the anchor); `ptrs`
(191) the same.

- `fn new(source: Source<'_>, item: &SyntaxNode, elide: Elide) -> Option<SignatureSyntax>`
  becomes `fn new(source: Source<'_>, item: &SyntaxNode, elide: Elide, frame: &Frame) -> ...`.
- 161: `let mut elided = SmallVec::<[SyntaxNode; 1]>::new();` becomes
  `let mut elided = Vec::with_capacity_in(1, frame);`
- 190-191: the `// alloc:` line and
  `let ptrs = elided.iter().map(SyntaxNodePtr::new).collect::<SmallVec<[_; 1]>>();` become
  `let ptrs = frame.alloc_slice_from_iter(elided.iter().map(SyntaxNodePtr::new));`
  and 197 `ast_id_map.relative_without(&anchor, &copy, &ptrs)` becomes
  `ast_id_map.relative_without(&anchor, &copy, ptrs)`.
- `item_syntax` (386) gains `frame: &Frame` last and passes it to both `SignatureSyntax::new`
  calls (396, 401).
- `signature_syntax` (406): the body's `match def { .. }` goes inside
  `blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| match def { .. })`, each arm passing `frame` last to
  `item_syntax`.
- `variants_syntax` (427):
  `blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| item_syntax(db, def.lookup(db).id, |_| None, frame))`.
- `use blaze_mem::frame::Frame;` at the top. `SmallVec` stays imported (`VariantBits`).

Memo: `SignatureSyntax` = the item's own tree (`Arc<FlatTree>` from `clone_subtree_without`),
its relative `AstIdMap` and `RealSpanMap` (`within`), two `SyntaxNodePtr`s.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-sigs, 2026-10-29): as written. The test `items_anchored_at_a_fn_are_read_around_its_body` calls `item_syntax` inside `frame::task(Tag::Nameres, ..)`.

### I1.3 `lower_fields` (1771-1834): fields lowered into the frame, the arena made exact

Per `VariantFields` run: `Arena::with_capacity(count)` (1797) sized to every written field, then
`shrink_to_fit` (1830-1832) when cfg leaves some out: a grow-then-shrink of the memo (contract 4).

- `VariantFields::lower` (1640), `lower_field_list` (1737), `lower_fields` (1771) each gain
  `frame: &Frame` last; `lower_field_list` passes it to both `lower_fields` calls (1746, 1756),
  `lower` to the three `lower_field_list` calls (1659, 1672, 1684).
- `relative` (1632): `let syntax = variant_syntax(db, id)?;
  Some(blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| Self::lower(db, id, Some(syntax), frame)))`.
- `with_source_map` (1615): its first line becomes
  `let Some((it, map)) = Self::relative(db, id) else {
      return blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| Self::lower(db, id, None, frame));
  };`
- In `lower_fields`, 1797 `let mut arena = Arena::with_capacity(count);` becomes
  `let mut lowered = Vec::with_capacity_in(count, frame);`; 1816
  `arena.alloc(FieldData { .. });` becomes `lowered.push(FieldData { .. });`; and 1829-1833

  ```rust
      let (store, source_map) = col.store.finish();
      if arena.len() < count {
          arena.shrink_to_fit();
      }
      Some((arena, store, source_map))
  ```

  become

  ```rust
      let (store, source_map) = col.store.finish();
      // alloc: the memo's fields, exact: those the cfg left in
      let mut arena = Arena::with_capacity(lowered.len());
      for field in lowered.drain(..) {
          arena.alloc(field);
      }
      Some((arena, store, source_map))
  ```

Memo: `(Arc<VariantFields>, ExpressionStoreSourceMap)`: one `Arc`, the field arena (one exact
allocation, none for no field), the store and source map (format's).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-sigs, 2026-10-29): as written, the copy into the arena a consuming `for` over the frame `Vec` rather than `drain(..)`. `moved_source_maps_are_the_files` calls `VariantFields::lower` inside `frame::task(Tag::Nameres, ..)`.

### I1.4 `EnumVariants::of` (1896-1954): variants and diagnostics into the frame, results exact

Per enum: `FxIndexMap::with_capacity_and_hasher(count)` (1920) sized to every written variant,
shrunk at 1947 when cfg leaves some out; `diagnostics` (1912) a `ThinVec` grown by `push` and
shrunk (1951).

After (the body from 1901):

```rust
    ) -> (EnumVariants, ThinVec<InactiveEnumVariantCode>) {
        blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| {
            let loc = e.lookup(db);
            // The relative tree's ast ids are the file's: what a variant is interned by.
            let (source, ast_id_map) = match variants_syntax(db, e) {
                Some(syntax) => (syntax.node::<ast::Enum>(), &syntax.ast_id_map),
                None => {
                    let source = loc.source(db);
                    let ast_id_map = source.file_id.ast_id_map(db);
                    (source, ast_id_map)
                }
            };
            let cfg_options = loc.container.krate(db).cfg_options(db);
            let Some(list) = source.value.variant_list() else {
                let variants = FxIndexMap::default();
                let explicit_discriminant = VariantBits::default();
                return (EnumVariants { variants, explicit_discriminant }, ThinVec::new());
            };
            let count = list.variants().count();
            let mut kept = Vec::with_capacity_in(count, frame);
            let mut inactive = Vec::new_in(frame);
            for variant in list.variants() {
                let ast_id = ast_id_map.ast_id(&variant);
                match AttrFlags::is_cfg_enabled_for(&variant, cfg_options) {
                    Ok(()) => {
                        let name = as_name_opt(variant.name());
                        let id = source.with_value(ast_id);
                        // clone: a `Symbol` count: the loc and the map each keep the name
                        let enum_variant =
                            EnumVariantLoc { id, parent: e, name: name.clone() }.intern(db);
                        // The expression the variant's body is lowered from, when it has one.
                        let explicit = variant.const_arg().and_then(|it| it.expr()).is_some();
                        kept.push((name, enum_variant, adt_shape(variant.kind()), explicit));
                    }
                    Err(cfg) => inactive.push(InactiveEnumVariantCode {
                        ast_id,
                        cfg,
                        // clone: error path, a cfg'd-out variant's diagnostic
                        opts: cfg_options.clone(),
                    }),
                }
            }
            // alloc: the memo's map, exact unless a name is written twice (an error)
            let mut variants =
                FxIndexMap::with_capacity_and_hasher(kept.len(), Default::default());
            let kept_len = kept.len();
            let mut explicit_discriminant = VariantBits::with_len(kept_len);
            for (name, enum_variant, shape, explicit) in kept.drain(..) {
                // A name written twice keeps its first place and the last variant's value.
                let (index, _) = variants.insert_full(name, (enum_variant, shape));
                explicit_discriminant.set(index, explicit);
            }
            if variants.len() < kept_len {
                explicit_discriminant.truncate(variants.len());
            }
            // alloc: the memo's diagnostics, exact; none for an enum cfg leaves whole
            let mut diagnostics = ThinVec::with_capacity(inactive.len());
            diagnostics.extend(inactive.drain(..));
            (EnumVariants { variants, explicit_discriminant }, diagnostics)
        })
    }
```

`VariantBits::truncate` keeps its words exact
for equality; its `shrink_to_fit` is inline (no allocation) up to 64 variants.

Memo: `EnumVariants` = an `FxIndexMap` (entries and index table: two exact allocations, none
for an enum with no variant) and `VariantBits` (inline up to 64); the `ThinVec` of
diagnostics (none unless a variant is cfg'd out).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-sigs, 2026-10-29), adapted: the body is now `EnumVariants::lower` (frontend-deps split `of` into the image's value or `lower`), so the frame and the exact results went there. `VariantBits::with_len` does not exist; it is `VariantBits::new(kept_len)`, which the code already used. The bits are truncated and shrunk only when a name is written twice.

### I1.6 The nine signatures' `relative` and `with_source_map`: no change

Withdrawn at 21:19, after mem-format's answer to I0. `StructSignature`, `UnionSignature`,
`EnumSignature`, `ConstSignature`, `StaticSignature`, `ImplSignature`, `TraitSignature`,
`FunctionSignature` and `TypeAliasSignature` each have a `lower` with no temporary of its own
(I1.7). All of their per-run work goes through `expr_store/lower.rs`'s `lower_*` entry points.
`ExprCollector` keeps those temporaries on hir-def's pooled thread-local `Scratch`
(`expr_store.rs` 518, 599), where a warm lowering allocates 0 (`a_warm_lowering_allocates_nothing`).
So these `relative` and `with_source_map` functions open no frame: one would go unused. Their
`lower` signatures and the `lower_*` calls stay as they are, and so does `lower_fields`'
`ExprCollector::with_maps` (1768-1769).

The only frames in signatures.rs are those I1.1 to I1.4 open for their own buffers:
`file_outline`, `signature_syntax`, `variants_syntax`, `VariantFields::relative` and
`with_source_map` (the lowered fields, I1.3), and `EnumVariants::of`. If a temporary of the
nine turns up later, it is named here, and the frame goes in with the code that uses it.

`of` lowers nothing (an `Arc` count of `relative`'s or `with_source_map`'s memo). Memo of each:
`(Arc<XSignature>, ExpressionStoreSourceMap)`: one `Arc` (the signature's fixed fields:
`const _` asserts at 462-485), its `ExpressionStore` and `GenericParams` (format's, exact per
mem-format), `FunctionSignature::params` / `TypeAliasSignature::bounds` (`Box<[_]>` drained from
the store's list mark, format's), `Name`s (a symbol each).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-sigs, 2026-10-29): withdrawn, so no frame here. (db.md's item 6 changes these functions for another reason: `of` lowering without the map.)

### I1.7 Nothing else in signatures.rs allocates per run

`child_tokens` (72) is a fixed array; `flags_from_attrs!` shifts; `extern_block_abi` parses an
ABI string in place; `VariantBits` is inline up to 64 variants; `moved_to_file`/`moved` are
format's (`ExpressionStoreSourceMap::moved`).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-sigs, 2026-10-29): nothing to change; rechecked, still true.

## I2. `attrs.rs`

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-attrs, 2026-10-29): I2.1 to I2.8, each below.

### I2.1 `has_word` (1392-1409): every name a caller asks becomes a flag, no walk on a hit

`AttrFlags::has_word` is a plain function (no memo) called per use: `"marker"` per trait by the
solver (`hir-ty/src/next_solver/interner.rs:2123`), `"automatically_derived"` per impl by
inference (`hir-ty/src/infer.rs:3236`), `"rustc_main"`, `"default"` and
`"rustc_canonical_symbol"` by the dead-code lint (`blaze-diagnostics/.../dead/seed.rs:181,224`,
`mark.rs:386`). Each name not in `has_word`'s three-arm table walks the item's syntax
(`has_word_in_source`: `attrs_source`, which for a crate root also re-parses its extra crate
attributes, I2.2). Bits 78 and up of `AttrFlags` are free.

- In `bitflags! { pub struct AttrFlags: u128 { .. } }`, after `RUSTC_CANONICAL_SYMBOL = 1 << 77;`
  (707):

  ```rust
          /// `#[marker]`.
          const MARKER = 1 << 78;
          /// `#[automatically_derived]`.
          const AUTOMATICALLY_DERIVED = 1 << 79;
          /// `#[rustc_main]`.
          const RUSTC_MAIN = 1 << 80;
          /// `#[default]`, on an enum's variant.
          const DEFAULT = 1 << 81;
  ```

- In `name_set! { enum Word { .. } }` (513-586), in order: `automatically_derived,` after
  `bench,`; `default,` after `completions,`; `marker,` after `macro_style,`;
  `rustc_main,` after `rustc_layout_scalar_valid_range_start,`.
- In `match_attr_flags`'s one-segment `PathMeta` arm, after the `rustc_canonical_symbol` arm
  (348-350):

  ```rust
                      Some(Word::marker) if single => AttrFlags::MARKER,
                      Some(Word::automatically_derived) if single => {
                          AttrFlags::AUTOMATICALLY_DERIVED
                      }
                      Some(Word::rustc_main) if single => AttrFlags::RUSTC_MAIN,
                      Some(Word::default) if single => AttrFlags::DEFAULT,
  ```

  (`if single`: `has_word_in_source` matches `path_is(attr.path(), name)`, one segment, with
  `r#` stripped as `name_text` strips it.)
- `has_word` (1393-1398) becomes:

  ```rust
          let flag = match word(name) {
              Some(Word::rustc_nounwind) => AttrFlags::RUSTC_NOUNWIND,
              Some(Word::rustc_comptime) => AttrFlags::RUSTC_COMPTIME,
              Some(Word::panic_handler) => AttrFlags::PANIC_HANDLER,
              Some(Word::rustc_canonical_symbol) => AttrFlags::RUSTC_CANONICAL_SYMBOL,
              Some(Word::marker) => AttrFlags::MARKER,
              Some(Word::automatically_derived) => AttrFlags::AUTOMATICALLY_DERIVED,
              Some(Word::rustc_main) => AttrFlags::RUSTC_MAIN,
              Some(Word::default) => AttrFlags::DEFAULT,
              _ => return Self::has_word_in_source(db, owner, name),
          };
  ```

- `check_name_set!(Word)` (attrs/tests.rs) covers the four new members. A differential test,
  in `attrs/tests.rs`:

  ```rust
  /// Each word `has_word` answers by a flag gives what the walk of the source gives.
  #[test]
  fn has_word_flags_match_the_source() {
      let (db, file_id) = TestDB::with_single_file(
          r#"
  #[marker] pub trait M {}
  #[automatically_derived] impl M for u8 {}
  #[rustc_main] fn main() {}
  pub enum E { #[default] A, B }
  #[rustc_canonical_symbol] extern "C" { fn f(); }
  #[rust_analyzer::marker] pub trait N {}
  pub struct Plain;
  "#,
      );
      let module = db.module_for_file(file_id.file_id(&db));
      let scope = module.def_map(&db).module(&db, module).scope(&db);
      let mut owners: Vec<AttrDefId> = Vec::new(); // alloc: test
      for def in scope.declarations() {
          match def {
              ModuleDefId::TraitId(it) => owners.push(it.into()),
              ModuleDefId::FunctionId(it) => owners.push(it.into()),
              ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                  owners.push(AdtId::EnumId(it).into());
                  owners.extend(it.enum_variants(&db).variants.values().map(|&(v, _)| v.into()));
              }
              ModuleDefId::AdtId(it) => owners.push(it.into()),
              _ => {}
          }
      }
      owners.extend(scope.impls().map(AttrDefId::from));
      for word in ["marker", "automatically_derived", "rustc_main", "default", "rustc_canonical_symbol"] {
          for &owner in &owners {
              assert_eq!(
                  AttrFlags::has_word(&db, owner, word),
                  AttrFlags::has_word_in_source(&db, owner, word),
                  "{word} on {owner:?}"
              );
          }
      }
  }
  ```

  (`has_word_in_source` is private to `AttrFlags`; the tests module is `attrs::tests`, a child,
  so it sees it. The foreign fn of the `extern` block is reached through its own scope only;
  the block's flag reads the same either way.)

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-attrs, 2026-10-29), adapted, one change with speed-hir-sol-defs §2 (`v2/requests/db-expand.md`) and diagnostics-speed-bodies §2: bit 78 was `HAS_SANITIZE` (db-apply-def), so `MARKER` 79, `AUTOMATICALLY_DERIVED` 80, `RUSTC_MAIN` 81, `DEFAULT` 82, with diagnostics' `NAKED` 83 and db-expand-sigs' `RUSTC_DENY_EXPLICIT_IMPL` 84; each `if single` in `match_attr_flags` and an arm of `has_word`. Test `has_word_flags_match_the_source` (`attrs/tests.rs`: the root module's items, impls and variants, `#[unsafe(naked)]` and `cfg_attr` included, with the count each word should find); `check_name_set!(Word)` takes the new words by itself. The corpus test's gating table now lists every one-segment word flag, which the old string match never set (it failed for `rustc_insignificant_dtor` and the four after it before this).

### I2.2 `parse_extra_crate_attrs` (714-742): memoized, the concat in scratch

Today every call concatenates `crate_attrs` into a new `String` and parses it: on every
attribute walk of a crate-root module (`attrs_source`, 779), per `crate_supports_no_std`,
`doc_html_root_url`, the item tree (`item_tree.rs:239`, `item_tree/items.rs:422`) and
`hir::Semantics` (`hir/src/semantics.rs:291`, per IDE request). After:

```rust
pub fn parse_extra_crate_attrs(db: &Db, krate: Crate) -> Option<SourceFile> {
    extra_crate_attrs(db, krate).as_ref().map(syntax::Parse::tree)
}

/// The crate's `--crate-attr`s as one file, parsed once per change of them; `None` for none,
/// or when one fails to parse (logged).
#[salsa::tracked(returns(ref))]
fn extra_crate_attrs(db: &Db, krate: Crate) -> Option<syntax::Parse<SourceFile>> {
    let crate_data = krate.data(db);
    let crate_attrs = &crate_data.crate_attrs;
    if crate_attrs.is_empty() {
        return None;
    }
    // All attributes are already enclosed in `#![]`.
    let p = blaze_mem::scratch::with_string(|combined| {
        for attr in crate_attrs.iter() {
            combined.push_str(attr);
        }
        SourceFile::parse(combined, crate_data.edition)
    });

    let errs = p.errors();
    if !errs.is_empty() {
        let base_msg = "Failed to parse extra crate-level attribute";
        let crate_name =
            krate.extra_data(db).display_name.as_ref().map_or("{unknown}", |name| name.as_str());
        let mut errs = errs.iter().peekable();
        let mut offset = TextSize::from(0);
        for raw_attr in crate_attrs.iter() {
            let attr_end = offset + TextSize::of(&**raw_attr);
            if errs.peeking_take_while(|e| e.range().start() < attr_end).count() > 0 {
                tracing::error!("{base_msg} {raw_attr} for crate {crate_name}");
            }
            offset = attr_end
        }
        return None;
    }

    Some(p)
}
```

`SourceFile::parse` copies the text it keeps (`syntax::Parse` owns its tree), so the lent
string goes back to the shelf. `syntax::Parse<SourceFile>` is `PartialEq + Eq`
(`syntax/src/lib.rs:82`) and is a memo value already (`EditionedFileId::parse`). Memo: the
parse (its flat tree and errors). `.tree()` is a node over the memo's tree (an `Arc` count).
Add `extra_crate_attrs` to the tag table (`Nameres`).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-attrs, 2026-10-29): as written.

### I2.3 `field_attr_flags` (1160-1173): flags in the frame, the memo exact

`SmallVec::<[AttrFlags; 8]>` spills past eight fields and `into_boxed_slice` then shrinks.

```rust
        #[salsa::tracked(returns(ref))]
        fn field_attr_flags(db: &Db, variant: VariantId) -> Box<[AttrFlags]> {
            blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| {
                let mut result = Vec::new_in(frame);
                for_each_field(db, variant, |_, cfg_options, field| {
                    let mut attr_flags = AttrFlags::empty();
                    expand_cfg_attr(
                        field.value.attrs(),
                        || cfg_options,
                        |attr, _| match_attr_flags(&mut attr_flags, attr),
                    );
                    result.push(attr_flags);
                });
                // alloc: the memo's flags, exact
                Box::from(&result[..])
            })
        }
```

Memo: `Box<[AttrFlags]>`, 16 bytes a field, one exact allocation (none for no field).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-attrs, 2026-10-29): as written, `frame::current` for `frame::task` (see "What this assumes"); the field walk reads the outline (`Tree::Outline`, speed-hir-sol-defs §12).

### I2.4 `generic_params_attr_flags` (1190-1229): the two maps exact

`ArenaMap::new()` twice, grown by `insert`, then `shrink_to_fit`. After: the non-empty flags
gathered in the frame, each map made once at the length its last index needs.

```rust
        #[salsa::tracked(returns(ref))]
        fn generic_params_attr_flags(
            db: &Db,
            def: GenericDefId,
        ) -> (ArenaMap<LocalLifetimeParamId, AttrFlags>, ArenaMap<LocalTypeOrConstParamId, AttrFlags>)
        {
            blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| {
                let mut cfg_options = None;
                let mut cfg_options =
                    || *cfg_options.get_or_insert_with(|| def.krate(db).cfg_options(db));

                let mut lifetimes = Vec::new_in(frame);
                let lifetimes_source = HasChildSource::<LocalLifetimeParamId>::child_source(&def, db);
                for (lifetime_id, lifetime) in lifetimes_source.value.iter() {
                    let mut attr_flags = AttrFlags::empty();
                    expand_cfg_attr(lifetime.attrs(), &mut cfg_options, |attr, _| {
                        match_attr_flags(&mut attr_flags, attr)
                    });
                    if !attr_flags.is_empty() {
                        lifetimes.push((lifetime_id, attr_flags));
                    }
                }

                let mut type_and_consts = Vec::new_in(frame);
                let type_and_consts_source =
                    HasChildSource::<LocalTypeOrConstParamId>::child_source(&def, db);
                for (type_or_const_id, type_or_const) in type_and_consts_source.value.iter() {
                    let mut attr_flags = AttrFlags::empty();
                    expand_cfg_attr(type_or_const.attrs(), &mut cfg_options, |attr, _| {
                        match_attr_flags(&mut attr_flags, attr)
                    });
                    if !attr_flags.is_empty() {
                        type_and_consts.push((type_or_const_id, attr_flags));
                    }
                }

                (exact_arena_map(&lifetimes), exact_arena_map(&type_and_consts))
            })
        }
```

and, beside `collect_field_attrs` (919), one helper both use:

```rust
/// An `ArenaMap` of `entries` (in index order), with room for exactly the last index.
fn exact_arena_map<T, V: Clone>(entries: &[(la_arena::Idx<T>, V)]) -> ArenaMap<la_arena::Idx<T>, V> {
    let len = entries.last().map_or(0, |(idx, _)| u32::from(idx.into_raw()) as usize + 1);
    // alloc: the memo's map, exact
    let mut map = ArenaMap::with_capacity(len);
    for (idx, value) in entries {
        // clone: a value moved out of the frame's buffer: flags are `Copy`
        map.insert(*idx, value.clone());
    }
    map
}
```

(`child_source` iterates its `ArenaMap` in index order, so `entries` are in index order.
`HasChildSource::child_source` is `src.rs`'s, which builds an `InFile<ArenaMap<..>>` per call:
I9.)

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-attrs, 2026-10-29): as written, `frame::current` for `frame::task`; `exact_arena_map` takes `V: Copy`, so it copies and needs no `// clone:`.

### I2.5 `collect_field_attrs` (919-933) and its three users: per-field values in the frame

`ArenaMap::new()` grown by `insert`, then `shrink_to_fit`. Its users: `fields_doc_aliases`
(1700), `fields_cfgs` (1746), `fields_docs` (1819), all tracked.

```rust
fn collect_field_attrs<T>(
    db: &Db,
    variant: VariantId,
    frame: &Frame,
    mut field_attrs: impl FnMut(&CfgOptions, InFile<ast::AnyHasAttrs>) -> T,
) -> ArenaMap<LocalFieldId, T> {
    let mut values = Vec::new_in(frame);
    for_each_field(db, variant, |_, cfg_options, field| {
        values.push(field_attrs(cfg_options, field));
    });
    // alloc: the memo's map, exact: one slot a field the cfg leaves in
    let mut result = ArenaMap::with_capacity(values.len());
    for (idx, value) in values.drain(..).enumerate() {
        result.insert(la_arena::Idx::from_raw(la_arena::RawIdx::from_u32(idx as u32)), value);
    }
    result
}
```

(`for_each_field` numbers the fields `0..` with no gap, so the index is the position.) Each user
wraps its body in `blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| ..)` and passes `frame` third.
`fields_doc_aliases`' per-field `let mut result = Vec::new();` (1705) becomes
`let mut result = Vec::new_in(frame);` and `result.into_boxed_slice()` (1711) becomes
`result.drain(..).collect::<Box<[Symbol]>>()` (an exact-size drain: one exact allocation, none
for a field with no alias); `extract_doc_aliases` (992) takes
`result: &mut Vec<Symbol, &Frame>`. `fields_cfgs`' per-field `Vec::new()` (1751) likewise;
`extract_cfgs` (1133) takes `result: &mut Vec<CfgExpr, &Frame>`; the
`1 => result.into_iter().next()` arms stay; `_ => Some(CfgExpr::All(result.into_boxed_slice()))`
(1740, 1760) become `_ => Some(CfgExpr::All(result.drain(..).collect()))`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-attrs, 2026-10-29): as written; `collect_field_attrs` also takes the `Tree` (§12): `fields_doc_aliases` and `fields_cfgs` read the outline, `fields_docs` the parse (its source map points into the file).

### I2.6 The value queries that build a list: frame, then exact

- `native_libs` (1315-1330): `let mut libs = Vec::new();` becomes, inside
  `blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| { .. })`, `let mut libs = Vec::new_in(frame);`, and
  `libs.into_boxed_slice()` becomes `libs.drain(..).collect()` (`// alloc: the memo's libraries,
  exact`).
- `doc_aliases` (1693-1697): the same, over `extract_doc_aliases` (now taking the frame `Vec`).
- `cfgs` (1734-1742): the same, over `extract_cfgs`.
- `target_features` (1557-1587): `let mut result = FxHashSet::default();` ... `result.extend(..)`
  ... `result.shrink_to_fit(); result` becomes a frame `Vec<Symbol>` gathered the same way, then

  ```rust
              // alloc: the memo's set, once: room for every feature named (a feature named
              // twice is the one case with room to spare)
              let mut set = FxHashSet::with_capacity_and_hasher(found.len(), Default::default());
              set.extend(found.drain(..));
              set
  ```

- `define_opaque` (1948-1985): `let mut result = Vec::new();` and the per-attribute
  `tokens ... .collect_vec()` and per-path `let mut segments = Vec::new();` all become
  `Vec::new_in(frame)` (one `frame::task(Tag::Nameres, ..)` at the body's top);
  `ModPath::from_segments(kind, segments)` becomes `ModPath::from_segments(kind, segments.drain(..))`
  (it takes `impl IntoIterator<Item = Name>`, `mod_path.rs:105`), and
  `result.into_boxed_slice()` becomes `result.drain(..).collect()`.
- `derive_info` (1846-1880): the helpers'
  `.map(|it| Symbol::intern(it.text())).collect::<Box<[_]>>()` (a `filter` chain: not exact, so a
  `Vec` grown then shrunk) becomes, with `frame` from a `frame::task(Tag::Nameres, ..)` at the body's top,
  `{ let mut helpers = Vec::new_in(frame); helpers.extend(<the same chain>); helpers.drain(..).collect::<Box<[_]>>() }`.
- `must_implement_one_of` (1996-2016): the same for `names`
  (`.filter_map(..).map(..).collect::<Box<[_]>>()`).
- `parse_rustc_legacy_const_generics` (2119-2125): its `filter_map(..).collect()` into
  `Box<[u32]>` takes a frame: called from `legacy_const_generic_indices` (1495, tracked), which
  opens `frame::task(Tag::Nameres, ..)` and passes it;
  `let mut found = Vec::new_in(frame); found.extend(<the chain>); Box::from(&found[..])`.

Memo of each: the `Box<[_]>` / set at its exact length (none when empty: `Box<[T]>` of length 0
allocates nothing).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-attrs, 2026-10-29): as written, each under `frame::current`.

### I2.7 `#[link]` parsing: no `String` per key, no `format!` per `cfg`

- `parse_link_tt` (1062-1067):

  ```rust
                  let Some(value) =
                      ast::String::cast(value).and_then(|it| Some(it.value().ok()?.into_owned()))
                  else {
                      continue;
                  };
  ```

  becomes

  ```rust
                  let Some(value) = ast::String::cast(value) else { continue };
                  let Ok(value) = value.value() else { continue };
  ```

  (a `Cow` borrowed from the token unless it has escapes; every use below reads `&value` or
  `value.split(',')`, which a `Cow<str>` derefs to: write `LinkKind::of(&value)` as is, and
  `value.split(',')` as `(*value).split(',')` where the borrow checker asks).
- `link_value` (1104-1119) becomes `link_symbol`, interning in place, its one caller
  (`wasm_import_module`, 1340-1342) taking the symbol:

  ```rust
  /// The string a `link(...)` gives `key`, as `key = "value"`, interned.
  fn link_symbol(tt: &ast::TokenTree, key: &str) -> Option<Symbol> {
      let mut children = TokenTreeChildren::new(tt).peekable();
      while let Some(child) = children.next() {
          let NodeOrToken::Token(name) = child else { continue };
          if name.kind() != SyntaxKind::IDENT || name.text() != key {
              continue;
          }
          let Some(NodeOrToken::Token(eq)) = children.next() else { continue };
          if eq.kind() != T![=] {
              continue;
          }
          let Some(NodeOrToken::Token(value)) = children.next() else { continue };
          return ast::String::cast(value).and_then(|it| Some(Symbol::intern(&it.value().ok()?)));
      }
      None
  }
  ```

  and in `wasm_import_module`: `&& let Some(module) = link_symbol(&tt, "wasm_import_module")`
  `{ return ControlFlow::Break(module); }`.
- `link_cfg` (1123-1131): `let text = format!("#[cfg{}] fn f() {{}}", group.syntax().text());`
  becomes the parse in a lent string:

  ```rust
  fn link_cfg(group: &ast::TokenTree) -> Option<CfgExpr> {
      let file = blaze_mem::scratch::with_string(|text| {
          use std::fmt::Write;
          // A `String` takes every write.
          let _ = write!(text, "#[cfg{}] fn f() {{}}", group.syntax().text());
          SourceFile::parse(text, span::Edition::CURRENT).tree()
      });
      let attr = file.syntax().descendants().find_map(ast::Attr::cast)?;
      match attr.meta()? {
          ast::Meta::CfgMeta(it) => Some(CfgExpr::parse_from_ast(it.cfg_predicate()?)),
          _ => None,
      }
  }
  ```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-attrs, 2026-10-29): as written; `link_cfg` writes into `scratch::with_string`, so speed-hir-sol-defs §2's `format!` justification has nothing left to justify.

### I2.8 Hit paths of the attribute lookups: what is zero after I2.1 and I2.2

Outside any query run, per IDE request or per use in a later query, each of these is a memo
read and a bit test: `AttrFlags::query`, `query_field` (a slice index), `query_lifetime_param`,
`query_type_or_const_param` (an `ArenaMap::get`), `lang_item`/`lang_item_with_attrs` (a flag,
then a memo's `Symbol` count), `link_name`, `diagnostic_item`, `link_section`, `linkage`,
`export_name`, `repr`, `repr_assume_has`, `legacy_const_generic_indices`, `target_features`,
`is_rustc_nounwind`, `rustc_layout_scalar_valid_range`, `doc_aliases`, `cfgs`, `doc_keyword`,
`docs`, `field_docs`, `derive_info`, `unstable_feature`, `must_use_message`, `define_opaque`,
`must_implement_one_of`, `has_word` for any word with a flag. None makes a buffer per call, so
none needs scratch; the budget test I12.1 holds them at zero. Two that still walk the source
per call, named for their owners and left as they are because their callers are cold:
`has_word` of a word with no flag (`has_word_in_source`, no caller in the tree today) and
`target_feature_list` (1593: a fresh `Vec<Symbol>` per call, no caller outside attrs.rs today).
`is_cfg_enabled_for` (1252) parses each `#[cfg]` into a `CfgExpr` (its `Box`es) per call:
called inside query runs only (item and field lowering, `for_each_field`), where the parse is
the run's work; it returns the failing `CfgExpr` as the diagnostic, so it stays owned.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-attrs, 2026-10-29): nothing to change; I12.1's test holds the hit paths at zero.

## I3. `attrs/stability.rs`

- `own_of_fields` (354-374): `let mut fields = Vec::new();` becomes, inside
  `blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| { .. })`, `let mut fields = Vec::new_in(frame);`, and
  `fields.into_boxed_slice()` becomes `fields.drain(..).collect()` (`// alloc: the memo's
  fields, exact`). Memo: `Box<[Own]>`, exact.
- `items` (561-591): `SmallVec<[_; 16]>` of tokens and `Items = SmallVec<[_; 4]>`: inline for any
  attribute list of up to 16 tokens and 4 items (every `#[stable]`/`#[unstable]`/`#[deprecated]`
  in the standard library). Kept.
- `own_of_def` (341-351): no buffer (a `Reader` on the stack). Memo: `Own` (two options of
  symbols).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-attrs, 2026-10-29): `own_of_fields` as written (`fields.drain(..).collect()`, with speed-hir-sol-defs §4's `// alloc:` line); `items` and `own_of_def` unchanged. Both read the parse (`Tree::Live`).

## I4. `attrs/docs.rs`: `docs` and `fields_docs` (IDE only, lru'd)

Per doc string with a newline: `lines: Vec<(&str, TextSize)>` (289) and two
`raw_lines: Vec<&str>` (297, 303), and `get_horizontal_trim` returns a `String` (517, 563). Per
`extract_docs` (824-899): `Docs::docs: String`, `docs_source_map: Vec`, `Indent::lines: Vec`
grown by `push` and shrunk (`shrink_to_fit`, 476-490 and 896).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-attrs, 2026-10-29): I4.1 and I4.2, below.

### I4.1 The trims over `doc.lines()`, nothing collected

`push_doc_lines` (267-324), `get_vertical_trim` (497-511), `get_horizontal_trim` (517-564)
become:

```rust
    fn push_doc_lines(
        &mut self,
        doc: &str,
        ast_offset: Option<TextSize>,
        comment_kind: DocCommentKind,
        indent: &mut Indent,
    ) {
        // Note: this is pushed even if there are only empty lines here, because that's what rustdoc does.
        let shape = match comment_kind {
            DocCommentKind::Sugared(shape) => {
                indent.seen_sugared = true;
                shape
            }
            // rustc uses `Line` for desugared comments.
            DocCommentKind::Desugared => ast::CommentShape::Line,
        };

        if !doc.contains('\n') {
            self.push_doc_line(doc, ast_offset, comment_kind, indent);
            return;
        }

        let (i, j) = get_vertical_trim(doc).unwrap_or((0, doc.lines().count()));
        let lines = || doc.lines().skip(i).take(j - i);
        let horizontal = get_horizontal_trim(lines(), shape);
        for line in lines() {
            let mut line_offset = TextSize::new(doc.substr_range(line).unwrap().start as u32);
            let mut line = line;
            // Strip `"[ \t]*\*"` from each line where present, exactly like rustc.
            if let Some(horizontal) = horizontal
                && let Some(rest) = line.strip_prefix(horizontal)
            {
                line = rest;
                line_offset += TextSize::of(horizontal);
                if shape == ast::CommentShape::Block
                    && (line == "*" || line.starts_with("* ") || line.starts_with("**"))
                {
                    line = &line[1..];
                    line_offset += TextSize::of("*");
                }
            }
            self.push_doc_line(line, ast_offset.map(|it| it + line_offset), comment_kind, indent);
        }
    }
```

```rust
/// rustc's [`beautify_doc_string`]'s vertical trim, modulo `CommentKind`/`CommentShape`
/// renaming, over the lines of `doc`.
///
/// [`beautify_doc_string`]: https://github.com/rust-lang/rust/blob/16a623ad672a92409b5c04beb303583c6cf72a7e/compiler/rustc_ast/src/util/comments.rs#L38
fn get_vertical_trim(doc: &str) -> Option<(usize, usize)> {
    let len = doc.lines().count();
    let mut i = 0;
    let mut j = len;
    // first line of all-stars should be omitted
    if doc.lines().next().is_some_and(|line| line.chars().all(|c| c == '*')) {
        i += 1;
    }

    // like the first, a last line of all stars should be omitted
    if j > i
        && let Some(last) = doc.lines().next_back()
        && !last.is_empty()
        && last.chars().all(|c| c == '*')
    {
        j -= 1;
    }

    if i != 0 || j != len { Some((i, j)) } else { None }
}

/// rustc's [`beautify_doc_string`]'s horizontal trim, modulo `CommentKind`/`CommentShape`
/// renaming, over `lines`, returning the prefix borrowed from the first line rather than
/// interning it.
///
/// [`beautify_doc_string`]: https://github.com/rust-lang/rust/blob/16a623ad672a92409b5c04beb303583c6cf72a7e/compiler/rustc_ast/src/util/comments.rs#L54
fn get_horizontal_trim<'a>(
    lines: impl Iterator<Item = &'a str> + Clone,
    kind: ast::CommentShape,
) -> Option<&'a str> {
    let mut i = usize::MAX;
    let mut first = true;

    // In case we have doc comments like `/**` or `/*!`, we want to remove stars if they are
    // present. However, we first need to strip the empty lines so they don't get in the middle
    // when we try to compute the "horizontal trim".
    let len = lines.clone().count();
    let (start, end) = match kind {
        ast::CommentShape::Block => {
            // Whatever happens, we skip the first line.
            let skip = lines
                .clone()
                .next()
                .map(|l| if l.trim_start().starts_with('*') { 0 } else { 1 })
                .unwrap_or(0);
            // The first and one past the last line from `skip` on that is not blank.
            let (mut start, mut end) = (len, len);
            for (at, line) in lines.clone().enumerate().skip(skip) {
                if !line.trim().is_empty() {
                    if start == len {
                        start = at;
                    }
                    end = at + 1;
                }
            }
            (start, end)
        }
        ast::CommentShape::Line => (0, len),
    };
    let lines = lines.skip(start).take(end - start);

    for line in lines.clone() {
        for (j, c) in line.chars().enumerate() {
            if j > i || !"* \t".contains(c) {
                return None;
            }
            if c == '*' {
                if first {
                    i = j;
                    first = false;
                } else if i != j {
                    return None;
                }
                break;
            }
        }
        if i >= line.len() {
            return None;
        }
    }
    let first = lines.clone().next()?;
    Some(&first[..i])
}
```

(The block case: rustc skips leading blank lines from `skip` and trailing ones down to the
first kept; when every line from `skip` on is blank both meet at the end, an empty slice, as
here with `(len, len)`.) The old three functions move, unchanged, into `docs.rs`'s tests as
`reference_push_lines` (the `lines`/`raw_lines` body returning the `(line, offset)` pairs it
would push), `reference_vertical_trim`, `reference_horizontal_trim`, and a test compares:

```rust
    /// The trims over `doc.lines()` give the lines and offsets the collected ones gave, over
    /// seeded doc strings of stars, blanks, tabs and text, in both shapes.
    #[test]
    fn trims_match_reference() {
        let pieces = ["", "*", "**", " * a", "\t* b", "  c", "* ", "   ", "*/", "d *"];
        let mut state = 0x646f_6373u64;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) as usize
        };
        for _ in 0..20_000 {
            let lines = next() % 7;
            // alloc: test
            let doc: String =
                (0..lines).map(|_| pieces[next() % pieces.len()]).collect::<Vec<_>>().join("\n");
            if !doc.contains('\n') {
                continue;
            }
            for shape in [ast::CommentShape::Line, ast::CommentShape::Block] {
                let (i, j) = get_vertical_trim(&doc).unwrap_or((0, doc.lines().count()));
                let kept: Vec<&str> = doc.lines().skip(i).take(j - i).collect();
                assert_eq!(get_vertical_trim(&doc), reference_vertical_trim(&doc.lines().collect::<Vec<_>>()), "{doc:?}");
                assert_eq!(
                    get_horizontal_trim(kept.iter().copied(), shape).map(str::to_owned),
                    reference_horizontal_trim(&kept, shape),
                    "{doc:?} {shape:?}"
                );
            }
        }
    }
```

(`ast::CommentShape` derives `Debug` and `Copy`; check, else print with `{:?}` of a `match`.)

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-attrs, 2026-10-29): as written. The old functions are `reference_vertical_trim`, `reference_horizontal_trim` and `reference_push_lines` in `docs.rs`'s tests; `trims_match_reference` compares the trims, and also the text and source map `push_doc_lines` builds against pushing the reference lines, over 20 000 seeded strings joined by `\n` and `\r\n` (no `Debug` of `CommentShape` needed). The iterator copies carry `// clone:`.

### I4.2 `extract_docs`' buffers from the worker, the memo copied out exact

Beside `Indent` (75):

```rust
/// The buffers a run of [`extract_docs`] builds in, kept by the worker: the text, its source
/// map and the indent of each line grow to the largest docs seen and are reused.
#[derive(Default)]
struct DocBuffers {
    docs: String,
    source_map: Vec<DocsSourceMapLine>,
    indent: Vec<Option<(usize, DocCommentKind)>>,
}

thread_local! {
    static SPARE_DOC_BUFFERS: std::cell::RefCell<Option<DocBuffers>> =
        const { std::cell::RefCell::new(None) };
}

impl blaze_mem::scratch::Shelved for DocBuffers {
    fn empty(&mut self) {
        self.docs.clear();
        self.source_map.clear();
        self.indent.clear();
    }

    fn held(&self) -> usize {
        self.docs.capacity()
            + self.source_map.capacity() * size_of::<DocsSourceMapLine>()
            + self.indent.capacity() * size_of::<Option<(usize, DocCommentKind)>>()
    }

    fn unshelve() -> Option<Self> {
        SPARE_DOC_BUFFERS.try_with(|spare| spare.borrow_mut().take()).ok().flatten()
    }

    fn shelve(self) {
        // A thread that is ending, or a nested run's smaller set: freed.
        let _ = SPARE_DOC_BUFFERS.try_with(|spare| {
            let mut spare = spare.borrow_mut();
            if spare.as_ref().is_none_or(|it| it.held() < self.held()) {
                *spare = Some(self);
            }
        });
    }
}
```

`extract_docs` (824-899) becomes:

```rust
pub(crate) fn extract_docs<'a, 'db>(
    db: &'db Db,
    krate: Crate,
    resolver: &dyn Fn() -> Resolver<'db>,
    get_cfg_options: &dyn Fn() -> &'a CfgOptions,
    source: InFile<ast::AnyHasAttrs>,
    outer_mod_decl: Option<InFile<ast::Module>>,
    inner_attrs_node: Option<SyntaxNode>,
) -> Option<Box<Docs>> {
    blaze_mem::scratch::with::<DocBuffers, _>(|buffers| {
        let mut result = Docs {
            docs: std::mem::take(&mut buffers.docs),
            docs_source_map: std::mem::take(&mut buffers.source_map),
            outline_mod: None,
            inline_file: source.file_id,
            prefix_len: TextSize::new(0),
            inline_inner_docs_start: None,
            outline_inner_docs_start: None,
            macro_calls: ThinVec::new(),
        };
        let mut indent =
            Indent { lines: std::mem::take(&mut buffers.indent), seen_sugared: false };
        let mut cfg_options = None;

        <the three `extend_with_attrs` calls and the `outline_mod` and
         `inline_inner_docs_start` lines of 847-891, unchanged>

        result.remove_indent(&indent);
        result.remove_last_newline();

        // alloc: the memo's text and source map, exact; nothing for empty docs
        let docs = String::from(result.docs.as_str());
        let source_map = Vec::from(result.docs_source_map.as_slice());
        buffers.docs = std::mem::replace(&mut result.docs, docs);
        buffers.source_map = std::mem::replace(&mut result.docs_source_map, source_map);
        buffers.indent = indent.lines;
        // A `#[doc = macro!()]` is rare: its calls keep the `ThinVec` they were pushed to.
        result.macro_calls.shrink_to_fit();

        // alloc: the memo's box
        if result.is_empty() { None } else { Some(Box::new(result)) }
    })
}
```

`remove_indent` moves `self.docs` out as bytes and back (`String::from_utf8`), keeping the
buffer, so the worker's buffer is what returns. A nested `extract_docs` (a doc macro whose
expansion asks for docs) gets a default `DocBuffers` (`scratch::with` makes one when the shelf
is empty): correct, and warm after once. `Docs::shrink_to_fit` (476-490) has no other caller
and goes. Memo: `Option<Box<Docs>>`: the box, the text and source map each exact, the
`ThinVec` of macro calls (none without a doc macro). The tests that build a `Docs` by hand
(947, 1124, 1193, 1228, 1312) are unchanged: `Indent` keeps `Default`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-attrs, 2026-10-29): as written, the body in `extract_docs_into(buffers, ..)`, which `extract_docs` calls inside `scratch::with::<DocBuffers, _>`; the parameters are dedyn's `&impl Fn`. `Docs::shrink_to_fit` deleted (no other caller).

## I5. `lang_item.rs`

- `crate_notable_traits` (324-340):

  ```rust
  pub fn crate_notable_traits(db: &Db, krate: Crate) -> Option<Box<[TraitId]>> {
      blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| {
          let mut traits = Vec::new_in(frame);
          let crate_def_map = crate_def_map(db, krate);
          for (_, module_data) in crate_def_map.modules(db) {
              for def in module_data.scope(db).declarations() {
                  if let ModuleDefId::TraitId(trait_) = def
                      && AttrFlags::query(db, trait_.into()).contains(AttrFlags::IS_DOC_NOTABLE_TRAIT)
                  {
                      traits.push(trait_);
                  }
              }
          }
          // alloc: the memo's traits, exact
          if traits.is_empty() { None } else { Some(Box::from(&traits[..])) }
      })
  }
  ```

- `crate_lang_items` (38-114): its collector is `LangItems` on the stack (a fixed struct of
  `Option` ids: no heap); the memo is one `Box<LangItems>`, exact. No buffer.
- `lang_items` (119-141): `LangItems` by value, merged field by field: no heap. Memo:
  `LangItems` in the memo slot.
- The lookup (`lang_items(db, krate).Foo`, `LangItemEnum::from_lang_items`): a memo read and a
  field load: zero (test I12.2).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-items, 2026-10-29): `crate_notable_traits` as written (frame `Vec`, `Box::from(&traits[..])`). `crate_lang_items` and `lang_items` were already as described, so nothing changed there apart from speed-hir-sol-defs §5's `// alloc:` on the box.

## I6. `import_map.rs`: the collection into the frame, the search over sorted keys

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-paths, 2026-10-29): I6.1, I6.2 and I6.3; see each below. The `fst` manifest move is asked in `v2/requests/db-expand.md` ("db-expand-paths: ...").

### I6.1 The collection (`Collector`, 197-394) and the build (98-166) onto the frame

Per crate, once: the `FxIndexSet` of rows (grown by doubling and then *kept as the memo*), the
`trait_assoc_items: Vec<bool>`, `infos: Vec<(u32, ImportInfo)>` (one per import found), the
`worklist: Vec<ModuleId>` and `visited: FxHashSet<ModuleId>` of the BFS, the counting sort's
`order: Vec<u32>`, the sort of `importables` (a stable `sort_by`: a merge buffer of `n / 2`
from the global allocator), the FST builder's `Vec<u8>` (grown by doubling and kept with its
spare capacity as the memo) and `key: Vec<u8>`.

`Collector` becomes

```rust
/// What the walk over the public modules finds, before it is laid out as columns: the run's.
struct Collector<'f> {
    /// The row of each item found, in the order first met.
    rows_of: hashbrown::HashMap<ItemInNs, u32, FxBuildHasher, &'f Frame>,
    rows: Vec<ItemInNs, &'f Frame>,
    /// Per row, whether the item is an associated item of a trait (as the first info found
    /// for it says).
    trait_assoc_items: Vec<bool, &'f Frame>,
    /// `(row, info)` in the order found.
    infos: Vec<(u32, ImportInfo), &'f Frame>,
}

impl<'f> Collector<'f> {
    fn add(&mut self, item: ItemInNs, is_trait_assoc_item: bool, info: ImportInfo) {
        let next = self.rows.len() as u32;
        let row = *self.rows_of.entry(item).or_insert(next);
        if row == next {
            self.rows.push(item);
            self.trait_assoc_items.push(is_trait_assoc_item);
        }
        self.infos.push((row, info));
    }

    fn collect(db: &Db, krate: Crate, frame: &'f Frame) -> Collector<'f> {
        let _p = tracing::info_span!("collect_import_map").entered();

        let def_map = crate_def_map(db, krate);
        let mut found = Collector {
            rows_of: hashbrown::HashMap::with_hasher_in(FxBuildHasher, frame),
            rows: Vec::new_in(frame),
            trait_assoc_items: Vec::new_in(frame),
            infos: Vec::new_in(frame),
        };

        // We look only into modules that are public(ly reexported), starting with the crate root.
        let root = def_map.root_module_id();
        let mut worklist = Vec::new_in(frame);
        worklist.push(root);
        let mut visited = hashbrown::HashSet::with_hasher_in(FxBuildHasher, frame);

        <the `while let Some(module) = worklist.pop() { .. }` loop of 227-257, unchanged>
        found
    }

    fn visit(
        &mut self,
        db: &Db,
        worklist: &mut Vec<ModuleId, &'f Frame>,
        <the rest unchanged>
```

(`collect_enum_variants` and `collect_trait_assoc_items` move into `impl<'f> Collector<'f>`
unchanged.) `import_map_query_impl` (98) becomes:

```rust
    fn import_map_query_impl(db: &Db, krate: Crate) -> Self {
        let _p = tracing::info_span!("import_map_query").entered();
        blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| Self::build(db, krate, frame))
    }

    fn build(db: &Db, krate: Crate, frame: &Frame) -> Self {
        let found = Collector::collect(db, krate, frame);
        let rows = found.rows.len();

        // Group the infos by row with a counting sort, which keeps the order they were found in
        // within a row: count, prefix-sum to the starts, then place with the starts as cursors
        // (which leaves each at its row's end) and shift them back.
        // alloc: the memo's offsets, exact
        let mut info_start = vec![0u32; rows + 1].into_boxed_slice();
        for &(row, _) in &found.infos {
            info_start[row as usize + 1] += 1;
        }
        for row in 0..rows {
            info_start[row + 1] += info_start[row];
        }
        let order = frame.alloc_slice_fill_with(found.infos.len(), |_| 0u32);
        for (at, &(row, _)) in found.infos.iter().enumerate() {
            let cursor = &mut info_start[row as usize];
            order[*cursor as usize] = at as u32;
            *cursor += 1;
        }
        info_start.copy_within(0..rows, 1);
        info_start[0] = 0;
        let infos: Box<[ImportInfo]> =
            // alloc: the memo's infos, exact; clone: a `Symbol` count each
            order.iter().map(|&at| found.infos[at as usize].1.clone()).collect();

        // alloc: the memo's bits, exact
        let mut trait_assoc_items = vec![0u64; rows.div_ceil(64)].into_boxed_slice();
        for (row, &is_trait_assoc_item) in found.trait_assoc_items.iter().enumerate() {
            if is_trait_assoc_item {
                trait_assoc_items[row / 64] |= 1 << (row % 64);
            }
        }

        // Every info, in row order, sorted by its lowercased name, ties by index: the stable
        // order, with no merge buffer. The order of UTF-8 bytes is the order of chars, and
        // lowercasing ASCII bytes leaves the others alone, so this is the keys' byte order.
        // alloc: the memo's order, exact
        let mut importables: Box<[u32]> = (0..infos.len() as u32).collect();
        importables.sort_unstable_by(|&l, &r| {
            cmp_ascii_lowercase(infos[l as usize].name.as_str(), infos[r as usize].name.as_str())
                .then(l.cmp(&r))
        });

        // One key per run of names equal up to ASCII case, lowercased, end to end.
        let mut keys = Vec::new_in(frame);
        let mut key_start = Vec::new_in(frame);
        let mut run_start = Vec::new_in(frame);
        let mut start = 0;
        while start < importables.len() {
            let name = infos[importables[start] as usize].name.as_str();
            let mut end = start + 1;
            while end < importables.len()
                && infos[importables[end] as usize].name.as_str().eq_ignore_ascii_case(name)
            {
                end += 1;
            }
            key_start.push(keys.len() as u32);
            run_start.push(start as u32);
            keys.extend(name.bytes().map(|b| b.to_ascii_lowercase()));
            start = end;
        }
        key_start.push(keys.len() as u32);
        run_start.push(importables.len() as u32);

        // alloc: the memo's set, exact
        let mut items = FxIndexSet::with_capacity_and_hasher(rows, FxBuildHasher);
        items.extend(found.rows.iter().copied());
        ImportMap {
            items,
            info_start,
            infos,
            trait_assoc_items,
            importables,
            // alloc: the memo's keys and their offsets, exact
            keys: Box::from(&keys[..]),
            key_start: Box::from(&key_start[..]),
            run_start: Box::from(&run_start[..]),
        }
    }
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-paths, 2026-10-29), adapted to the code as it is: `Collector<'f>` on the frame as written (rows map, rows, flags, infos, worklist, visited). In `build`, the counting sort is speed-hir's `Csr::build` (db-expand.md "speed-hir: four edits" item 1), over `(row, ImportInfo)` so it yields `info_start` and `infos` at their length (no `order` column); `trait_assoc_items` stays db-apply-def's `BitSet::new(rows)`. The three key columns are `Vec::with_capacity_in(.., frame)` (runs at most the infos, key bytes at most the names'), so they never regrow. Memo: nine exact allocations plus `Csr`'s cursors.

### I6.2 The FST goes: sorted keys, binary search, a scan for fuzzy

`search_dependencies` (564-602) allocates per call, on every completion request: the
`import_maps: Vec<&ImportMap>` (571), fst's `OpBuilder` (a `Vec` of boxed streams, one per
dependency), each `Stream` (two `Vec`s: its state stack and key buffer), `union()`'s heap, and
the result set. With fst's API none of these can be reused. The FST's job here is finding the
runs whose key an automaton takes (`Str`, `Str::starts_with`, `Subsequence`, all over the
lowercased query's bytes): over keys already sorted, exact and prefix are a binary search and a
scan from it, and `Subsequence` visits every key in fst too (no prefix of a key can rule out a
subsequence match, so the automaton never prunes). The keys end to end with `u32` offsets cost
their bytes plus 8 per run, against the FST's shared prefixes; the search then allocates
nothing.

The struct's `fst` field (68) goes, and in its place:

```rust
    /// The name of each run of [`Self::importables`] equal up to ASCII case, lowercased, end to
    /// end in byte order: run `r`'s is `keys[key_start[r]..key_start[r + 1]]`. Strictly
    /// increasing, as the names are sorted.
    keys: Box<[u8]>,
    key_start: Box<[u32]>,
    /// Run `r` is `importables[run_start[r]..run_start[r + 1]]`.
    run_start: Box<[u32]>,
```

The doc of `importables` (61-66) becomes "Indices into [`Self::infos`], sorted by their names
lowercased (ASCII), ties in row order, in runs of one name ([`Self::run_start`])." The
`#[derive(Default)]` stays (every field is `Default`; `run_start` empty means no run: `runs`
below saturates). `use fst::{Automaton, Streamer, raw::IndexedValue};` (6) goes.

Beside `row_of_info` (177):

```rust
    /// How many runs of one name there are.
    fn runs(&self) -> usize {
        self.run_start.len().saturating_sub(1)
    }

    fn key(&self, run: usize) -> &[u8] {
        &self.keys[self.key_start[run] as usize..self.key_start[run + 1] as usize]
    }

    fn run(&self, run: usize) -> &[u32] {
        &self.importables[self.run_start[run] as usize..self.run_start[run + 1] as usize]
    }

    /// The first run whose key is not below `query`.
    fn lower_bound(&self, query: &[u8]) -> usize {
        let (mut lo, mut hi) = (0, self.runs());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.key(mid) < query {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Each run whose key `mode` takes for `query`, lowercased: the key itself for
    /// [`SearchMode::Exact`], a key it starts for [`SearchMode::Prefix`], a key it is a
    /// subsequence of (by bytes) for [`SearchMode::Fuzzy`]; the runs fst's `Str`,
    /// `Str::starts_with` and `Subsequence` automata took over the same keys, in key order.
    fn search_runs(&self, mode: SearchMode, query: &[u8], mut each: impl FnMut(usize)) {
        match mode {
            SearchMode::Exact => {
                let at = self.lower_bound(query);
                if at < self.runs() && self.key(at) == query {
                    each(at);
                }
            }
            SearchMode::Prefix => {
                for run in self.lower_bound(query)..self.runs() {
                    if !self.key(run).starts_with(query) {
                        break;
                    }
                    each(run);
                }
            }
            SearchMode::Fuzzy => {
                for run in 0..self.runs() {
                    if is_subsequence(query, self.key(run)) {
                        each(run);
                    }
                }
            }
        }
    }
```

and beside `cmp_ascii_lowercase` (190):

```rust
/// Whether the bytes of `needle` appear in `hay` in order: fst's `Subsequence` automaton.
fn is_subsequence(needle: &[u8], hay: &[u8]) -> bool {
    let mut rest = hay;
    needle.iter().all(|&byte| match rest.iter().position(|&it| it == byte) {
        Some(at) => {
            rest = &rest[at + 1..];
            true
        }
        None => false,
    })
}
```

`search_dependencies` (561-602) and `search_maps` (604-631) become:

```rust
/// Searches dependencies of `krate` for an importable name matching `query`.
///
/// This returns a list of items that could be imported from dependencies of `krate`.
pub fn search_dependencies(db: &Db, krate: Crate, query: &Query) -> FxHashSet<(ItemInNs, Complete)> {
    // alloc: the caller's result, at the edge
    let mut found = FxHashSet::default();
    search_dependencies_into(db, krate, query, &mut found);
    found
}

/// [`search_dependencies`] into `found`, which it adds to: with a set the caller keeps, a
/// search allocates nothing once the set has grown to its results.
pub fn search_dependencies_into(
    db: &Db,
    krate: Crate,
    query: &Query,
    found: &mut FxHashSet<(ItemInNs, Complete)>,
) {
    search_dependencies_each(db, krate, query, |item, complete| {
        found.insert((item, complete));
    });
}

/// Calls `f` on each importable name of `krate`'s dependencies that matches `query`, in the
/// order the search finds them (dependencies in order, then each map's runs in key order, each
/// run's infos in order); an item can come more than once.
pub fn search_dependencies_each(
    db: &Db,
    krate: Crate,
    query: &Query,
    mut f: impl FnMut(ItemInNs, Complete),
) {
    let _p = tracing::info_span!("search_dependencies", ?query).entered();
    let lowercased = query.lowercased.as_bytes();
    for dep in krate.data(db).dependencies.iter() {
        let map = ImportMap::of(db, dep.crate_id);
        map.search_runs(query.search_mode, lowercased, |run| {
            for &info_idx in map.run(run) {
                let info = &map.infos[info_idx as usize];
                if !query.search_mode.check(&query.query, query.case_sensitive, info.name.as_str())
                {
                    continue;
                }
                let row = map.row_of_info(info_idx);
                if query.matches_assoc_mode(map.is_trait_assoc_item(row)) {
                    f(map.items[row], info.complete);
                }
            }
        });
    }
}
```

The `hir` wrapper (`hir/src/lib.rs:376`, `query_external_importables`) calls
`search_dependencies_each`. It hands items out in the order found, deduplicated by a per-worker
seen-set, so the completion order never depends on how far a reused set's table has grown on
earlier requests (`v2/requests/mem-db-def-hir.md` R10b, which asked for `_each`). Visiting an
`ItemInNs` and a `Complete`, both `Copy`, allocates nothing. `Query::new` (518)
lowercases into a `String` once per request: the edge.

Manifest (owner's): `fst` moves from `[dependencies]` to `[dev-dependencies]` of
`crates/hir-def/Cargo.toml` (line 24, the same spec), used only by the tests' reference model.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-paths, 2026-10-29): as written: the `fst` field and `use fst::..` gone, `keys`/`key_start`/`run_start`, `runs`, `key`, `run`, `lower_bound`, `search_runs`, `is_subsequence`, `search_dependencies` on `search_dependencies_into` on `search_dependencies_each` (R10b's), `search_maps` gone, `importables`' doc. Not done here: the manifest move (hir-def's `Cargo.toml` is not db-expand-paths'; asked in `v2/requests/db-expand.md`) and hir's `query_external_importables` onto `_each` (`crates/hir/`, mem-db-def-hir R10b).

### I6.3 The differential test (1563-1645): keys against the reference FST

1593-1597 (`assert_eq!(new.fst.as_fst().as_bytes(), old.fst.as_fst().as_bytes(), ..)`)
become the keys and run bounds read off the old FST's stream:

```rust
                {
                    use fst::Streamer;
                    let mut stream = old.fst.stream();
                    let mut run = 0;
                    while let Some((key, value)) = stream.next() {
                        assert_eq!(new.key(run), key, "seed {seed}, run {run}\n{fixture}");
                        let bounds =
                            (u64::from(new.run_start[run]) << 32) | u64::from(new.run_start[run + 1]);
                        assert_eq!(bounds, value, "seed {seed}, run {run}\n{fixture}");
                        run += 1;
                    }
                    assert_eq!(run, new.runs(), "seed {seed}\n{fixture}");
                }
```

and 1598 (`ImportMap::import_map_query_impl(&db, krate)`, outside any query run) becomes
`blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| ImportMap::build(&db, krate, frame))`.
The `search_dependencies(&db, main, &query) == reference::search(..)` comparison (1636-1640)
now checks the sorted-key search against the FST's union, every mode, both cases, every assoc
mode. The reference module keeps fst (dev-dependency).

Memo: `ImportMap` = `items` (an `FxIndexSet`: entries and index table, exact), `info_start`
(`rows + 1` u32s), `infos` (24 bytes each), `trait_assoc_items` (a bit a row), `importables`
(a u32 an info), `keys` (the runs' names' bytes), `key_start` and `run_start` (a u32 a run, plus
one): nine exact allocations, built once.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-paths, 2026-10-29): the stream comparison and the `frame::task` rebuild as written. Added, for "the same order": the differential test also compares `search_dependencies_each`'s visits, in order, with a new `reference::search_each` (each dependency's own FST stream in key order, each key's importables in order), for every query and mode; and `is_subsequence_against_fst` checks `is_subsequence` against fst's `Subsequence` over random keys and every query of up to two chars.

## I7. `diagnostic_item.rs`

- `crate_diagnostic_items` (62-76): `Collect.items.by_name` grows by `insert` and is shrunk
  (75); `duplicates` grows by `push`. After: `Collect<'db, 'f>` gathers into the frame and the
  memo is made once:

  ```rust
  struct Collect<'db, 'f> {
      db: &'db Db,
      by_name: hashbrown::HashMap<Symbol, AttrDefId, FxBuildHasher, &'f Frame>,
      duplicates: Vec<(Symbol, AttrDefId, AttrDefId), &'f Frame>,
  }
  ```

  `Collect::item` inserts as `DiagnosticItems::insert` does, into these (the same `entry`
  match on `hashbrown::hash_map::Entry`); `module` and `assoc` are unchanged. The query:

  ```rust
  pub fn crate_diagnostic_items(db: &Db, krate: Crate) -> DiagnosticItems {
      let _p = tracing::info_span!("crate_diagnostic_items_query").entered();
      blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| {
          let mut collect = Collect {
              db,
              by_name: hashbrown::HashMap::with_hasher_in(FxBuildHasher, frame),
              duplicates: Vec::new_in(frame),
          };
          for (module, data) in crate_def_map(db, krate).modules(db) {
              collect.module(module, data.scope(db));
          }
          demand::walk_bodies_saying(db, krate, Look::DiagnosticItems, |_, blocks| {
              for &(_, def_map) in &blocks.blocks {
                  for (module, data) in def_map.modules(db) {
                      collect.module(module, data.scope(db));
                  }
              }
          });
          // alloc: the memo's map and duplicates, exact
          let mut by_name =
              FxHashMap::with_capacity_and_hasher(collect.by_name.len(), FxBuildHasher);
          by_name.extend(collect.by_name.drain());
          let duplicates = collect.duplicates.drain(..).collect();
          DiagnosticItems { by_name, duplicates }
      })
  }
  ```

  (`duplicates`' order is the order found, kept by the `Vec`; `by_name` is a map: its iteration
  order is not part of its value, `PartialEq` compares contents.)
- `diagnostic_items` (83-108): `own.clone()` then `insert` per dependency's item (growing the
  clone), the BFS `seen: FxHashSet<Crate>` and `queue: Vec<Crate>`. After: `seen` and `queue`
  in the frame, the merge into a frame map, the memo made once:

  ```rust
  pub fn diagnostic_items(db: &Db, krate: Crate) -> DiagnosticItems {
      let _p = tracing::info_span!("diagnostic_items_query").entered();
      blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| {
          let own = crate_diagnostic_items(db, krate);
          // `CrateData::dependencies` lists the sysroot crates even for `#![no_std]` and
          // `#![no_core]` crates, which rustc does not load; the extern prelude is what name
          // resolution settled on.
          let mut seen = hashbrown::HashSet::with_hasher_in(FxBuildHasher, frame);
          seen.insert(krate);
          let mut queue = Vec::new_in(frame);
          queue.push(krate);
          let mut at = 0;
          while let Some(&from) = queue.get(at) {
              at += 1;
              for (_, (module, _)) in crate_local_def_map(db, from).local(db).extern_prelude() {
                  let dep = module.krate(db);
                  if seen.insert(dep) {
                      queue.push(dep);
                  }
              }
          }
          let mut by_name = hashbrown::HashMap::with_hasher_in(FxBuildHasher, frame);
          for (name, &owner) in &own.by_name {
              // clone: a `Symbol` count
              by_name.insert(name.clone(), owner);
          }
          let mut duplicates = Vec::new_in(frame);
          duplicates.extend(own.duplicates.iter().cloned()); // clone: `Symbol` counts
          for &dep in &queue[1..] {
              for (name, &owner) in &crate_diagnostic_items(db, dep).by_name {
                  match by_name.entry(name.clone()) { // clone: a `Symbol` count
                      hashbrown::hash_map::Entry::Vacant(it) => {
                          it.insert(owner);
                      }
                      hashbrown::hash_map::Entry::Occupied(it) => {
                          if *it.get() != owner {
                              duplicates.push((it.key().clone(), *it.get(), owner));
                          }
                      }
                  }
              }
          }
          // alloc: the memo's map and duplicates, exact
          let mut result = FxHashMap::with_capacity_and_hasher(by_name.len(), FxBuildHasher);
          result.extend(by_name.drain());
          DiagnosticItems { by_name: result, duplicates: duplicates.drain(..).collect() }
      })
  }
  ```

  `DiagnosticItems::insert` (42-53) has no other caller and goes.
- `use blaze_mem::frame::Frame; use rustc_hash::FxBuildHasher;` at the top; `FxHashSet` and
  `std::collections::hash_map::Entry` imports go.
- `DiagnosticItems::get` (38): a memo read and one probe: zero (test I12.1 covers it).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-items, 2026-10-29), with one change of form: the `entry` match is one free `fn insert(by_name, duplicates, name, owner)` shared by `Collect::item` and `diagnostic_items`' merge, and the memo is built once by `fn settle(by_name, duplicates)` (`FxHashMap::with_capacity_and_hasher` + `drain`, `duplicates.drain(..).collect()`). Both fns are used by both queries. The value is the same: the same entries, and duplicates in the order found. `Symbol` is `Copy`, so the merge copies (`(&name, &owner)`, `.copied()`) where the text cloned. `DiagnosticItems::insert` is gone; imports as asked (`hashbrown::hash_map::Entry`, `FxBuildHasher`, `Frame`).

## I8. `upvars.rs`

`upvars_mentioned_impl` (105-133): per body with a closure, `binding_names: FxHashSet<Symbol>`
(120), `mentioned: Vec<BindingId>` (123), `closures_map: FxHashMap` grown by `insert` (121)
then `shrink_to_fit` (130) and boxed as the memo.

- `struct UpvarsMentionedVisitor<'db>` (135) becomes `UpvarsMentionedVisitor<'db, 'f>` with
  `binding_names: hashbrown::HashSet<Symbol, FxBuildHasher, &'f Frame>`,
  `closures: Vec<(ExprId, Upvars), &'f Frame>` (renamed from `closures_map`: the closures in
  the order walked, each once) and `mentioned: Vec<BindingId, &'f Frame>`; the two impls (150,
  202) gain `'_`.
- 229 `self.closures_map.insert(expr, Upvars(self.mentioned[start..].into()));` becomes
  `self.closures.push((expr, Upvars(self.mentioned[start..].into())));` (each closure is walked
  once: `on_expr` of a `Closure` is its one visit).
- 105-133 become:

  ```rust
  pub fn upvars_mentioned_impl(
      db: &Db,
      owner: ExpressionStoreOwnerId,
  ) -> Option<Box<FxHashMap<ExprId, Upvars>>> {
      let store = ExpressionStore::of(db, owner);
      store.expr_roots().next()?;
      // Most bodies have no closure: they need no resolver and no walk.
      if !store.exprs().any(|(_, expr)| matches!(expr, Expr::Closure { .. })) {
          return None;
      }
      blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| {
          let mut binding_names = hashbrown::HashSet::with_hasher_in(FxBuildHasher, frame);
          binding_names.extend(store.bindings().map(|(_, binding)| *binding.name.symbol()));
          let mut visitor = UpvarsMentionedVisitor {
              db,
              resolver: owner.resolver(db),
              owner,
              store,
              binding_names,
              closures: Vec::new_in(frame),
              current_closure: None,
              mentioned: Vec::new_in(frame),
          };
          visitor.on_exprs(store.expr_roots());
          if visitor.closures.is_empty() {
              return None;
          }
          // alloc: the memo's map, exact
          let mut result =
              FxHashMap::with_capacity_and_hasher(visitor.closures.len(), FxBuildHasher);
          result.extend(visitor.closures.drain(..));
          // alloc: the memo's box
          Some(Box::new(result))
      })
  }
  ```

  `upvars_mentioned_impl` is `pub` but its only callers are the three tracked functions
  (85, 93, 101); make it `fn` (no `pub`), so no caller outside a run reaches it.
- `use blaze_mem::frame::Frame; use rustc_hash::{FxBuildHasher, FxHashMap};`.

Memo: `Option<Box<FxHashMap<ExprId, Upvars>>>`: the box, the map (exact), each closure's
`Upvars(Box<[BindingId]>)` exact (`[..].into()` from a slice). The `Resolver` the visitor walks
with is mem-db-def-resolve's.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-items, 2026-10-29) as written: `UpvarsMentionedVisitor<'db, 'f>` with frame `binding_names`, `closures` and `mentioned`, `closures.push`, the memo exact, and `upvars_mentioned_impl` private. The early return uses `store.has_closures()` (speed-hir's item 3, `v2/requests/db-expand.md`) instead of the `exprs().any(..)` shown.

## I9. `src.rs` (IDE and attribute lowering; owned returns)

`HasChildSource::child_source` returns an owned `InFile<ArenaMap<..>>` by its public signature,
so its map is its caller's. The one change: `VariantId::child_source` (136-187)'s
`let mut map = ArenaMap::new();` (152), grown by `insert`, becomes sized by the written fields
before the loop in each arm: in the `Tuple` arm
`let mut map = ArenaMap::with_capacity(fl.fields().count());`, in the `Record` arm the same,
in `Unit` `ArenaMap::new()` (the `let` moves into the arms, each returning
`InFile::new(src.file_id, map)`). Room for every written field: cfg'd-out ones leave room
unused, never a regrow. `use_tree_source_map` (50) and `UseId::child_source` (66) are the
IDE's (source-to-def of a `use` tree), at the edge.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-items, 2026-10-29) as written: the map is made in each arm, `ArenaMap::with_capacity(fl.fields().count())` for `Tuple` and `Record` and `ArenaMap::new()` for `Unit`, and each arm returns `InFile::new(src.file_id, map)`.

## I10. `macro_reach.rs`: the walk's set and stack in the caller's frame

`MacroReachable` (73-77) is kept across one fixpoint of its caller (one query run) and has no
caller outside its tests yet. It becomes:

```rust
/// rustc's `macro_reachable`: the pairs of a module walked and the macro's module it was walked
/// for, kept across the macros and the passes of one fixpoint, in the frame of the run that
/// holds the fixpoint.
#[derive(Debug)]
pub struct MacroReachable<'f> {
    walked: hashbrown::HashSet<(ModuleId, ModuleId), FxBuildHasher, &'f Frame>,
    stack: Vec<ModuleId, &'f Frame>,
}

impl<'f> MacroReachable<'f> {
    pub fn new_in(frame: &'f Frame) -> MacroReachable<'f> {
        MacroReachable {
            walked: hashbrown::HashSet::with_hasher_in(FxBuildHasher, frame),
            stack: Vec::new_in(frame),
        }
    }
```

(the two methods unchanged, in `impl<'f> MacroReachable<'f>`; `#[derive(Default)]` goes;
`use rustc_hash::FxBuildHasher; use blaze_mem::frame::Frame;`). The tests (`macro_reach/tests.rs`
165, 190): `let mut reachable = MacroReachable::default();` and what uses it go inside
`blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| { let mut reachable =
MacroReachable::new_in(frame); .. })`. `symbol_attr` (38) is attrs.rs's (no buffer).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-items, 2026-10-29) as written: `MacroReachable<'f>` with `new_in`, `Default` gone, both tests inside `frame::task(Tag::Nameres, ..)`. It still has no caller outside its tests (`rg MacroReachable crates` outside `macro_reach*`: none).

## I11. `lib.rs`, `unstable_features.rs`, `deps/`, the rest

- `include_macro_invoc` (1775-1785): a `filter_map` chain `collect`ed into `Box<[_]>` (a `Vec`
  grown then shrunk). After:

  ```rust
  pub fn include_macro_invoc(db: &Db, krate: Crate) -> Box<[(MacroCallId, EditionedFileId)]> {
      blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| {
          let mut found = Vec::new_in(frame);
          found.extend(
              crate_def_map(db, krate)
                  .modules(db)
                  .flat_map(|(_, m)| m.scope(db).iter_macro_invoc())
                  .filter_map(|(_, &invoc)| {
                      invoc.loc(db).include_file_id(db, invoc).map(|x| (invoc, x))
                  }),
          );
          // alloc: the memo's calls, exact
          Box::from(&found[..])
      })
  }
  ```

  Read per crate by `hir-ty`'s method resolution (`method_resolution.rs:698`), `blaze-engine`
  and `hir`'s source-to-def: a memo read each, zero.
- `UnstableFeatures::query` (51): a clone of the def map's set (exact capacity, one per crate).
  No temporary.
- `MacroId::definition` (980) and lib.rs's other tracked items: no buffer.
- `deps/serve.rs`, `deps/key.rs`, `deps/image.rs`, `deps/signatures.rs` (`write`, 120-146):
  session setup and image writing, run once per session before any warm loop: the edge.
  `deps/signatures/codes.rs` (the decode of a signature from an image, a query's run): every
  list is made at the length the record gives (`Arena::with_capacity(len)` 81,
  `with_capacity_and_hasher(len)` 751, `ThinVec::with_capacity(len)` 762), one `Arc` per
  signature: exact already. `deps/signatures/key.rs`'s `module_indexes` (88, tracked):
  collected from an `enumerate` over the def map's modules, sized by its length. `key`'s
  `bytes: SmallVec` (101): inline.
- `builtin_derive.rs`, `builtin_type.rs`, `test_db.rs`: no allocation on a query or hit path.

### I11.1 `unstable_features.rs`: built at its size (mem-db-def-collect's H9 ask 4)

The collector's H4 (`v2/requests/mem-db-def-collect.md`) counts the crate's `#![feature]`s and
reserves the set once before enabling them (`crate_data.unstable_features.reserve(features.len())`),
so the set is never regrown or shrunk. In `impl UnstableFeatures` (28-42), 39-41

```rust
    pub(crate) fn shrink_to_fit(&mut self) {
        self.all.shrink_to_fit();
    }
```

become

```rust
    /// Room for `additional` more features: the collector reserves the crate's count once,
    /// before it enables them.
    pub(crate) fn reserve(&mut self, additional: usize) {
        // alloc: the def map's feature set, once per crate, at the count written
        self.all.reserve(additional);
    }

    /// The heap buffers this owns, for the def map's budget tests: the set's table, if it has one.
    #[cfg(test)]
    pub(crate) fn owned_allocations(&self) -> u64 {
        u64::from(self.all.capacity() > 0)
    }
```

Order: `shrink_to_fit`'s one caller, `DefMapCrateData::shrink_to_fit` (`nameres.rs:349`), goes in
mem-db-def-collect's A5 (which makes it `settle`). Apply this together with A5 or after it, never
before, or `nameres.rs` stops building. A feature written twice leaves one slot spare: still one
table, never a regrow. `query` (51) clones the def map's set: `FxHashSet::clone` allocates one
table at the source's bucket count, so the memo is the same size as the def map's.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-items, 2026-10-29), I11: `include_macro_invoc` as written (frame `Vec`, `Box::from(&found[..])`). The rest of I11 needs no change. `UnstableFeatures::query`'s clone carries speed-hir-sol-defs §10's `// clone:`.
> not applied (I11.1): mem-db-def-collect's A5 has not landed. `nameres.rs:352` (`DefMapCrateData::shrink_to_fit`) still calls `unstable_features.shrink_to_fit()`, and nothing calls `reserve` (`rg 'unstable_features\.reserve' crates/hir-def` finds nothing). Replacing `shrink_to_fit` now would stop `nameres.rs` building. Apply I11.1 when A5 makes that function `settle` and the collector's H4 reserves. Filed under `## db-expand-items: I11.1 waits on A5` in `v2/requests/db-expand.md`.

## I12. Budget tests (written in full)

Each needs H3's counting allocator in hir-def's test binary. Counts are per thread; salsa runs
each query on the calling thread. "Warm" means asked once before the measure. The misses
assume salsa stores a memo with no heap allocation of its own and interns into regions
(mem-db-salsa's contract 5): what is left is the value's own allocations.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-attrs, 2026-10-29): I12.1, below (I12.2 to I12.4 are the siblings').

### I12.1 `attrs/tests.rs`: attribute lookups on a warm item are zero

```rust
/// Every attribute lookup a later query or an IDE request makes, on an item asked once: a
/// memo read and a bit test, no allocation.
#[test]
fn warm_attribute_lookups_allocate_nothing() {
    let (db, file_id) = TestDB::with_single_file(
        r#"
#![feature(lang_items)]
#[repr(C)]
#[must_use = "use it"]
#[doc(alias = "Ess")]
pub struct S {
    #[doc(hidden)]
    pub a: u8,
    #[cfg(any())]
    pub b: u8,
}
#[lang = "sized"]
#[marker]
pub trait Sized {}
#[rustc_diagnostic_item = "the_fn"]
#[export_name = "named"]
pub fn f() {}
"#,
    );
    let module = db.module_for_file(file_id.file_id(&db));
    let scope = module.def_map(&db).module(&db, module).scope(&db);
    let mut strukt = None;
    let mut trait_ = None;
    let mut func = None;
    for def in scope.declarations() {
        match def {
            ModuleDefId::AdtId(AdtId::StructId(it)) => strukt = Some(it),
            ModuleDefId::TraitId(it) => trait_ = Some(it),
            ModuleDefId::FunctionId(it) => func = Some(it),
            _ => {}
        }
    }
    let (strukt, trait_, func) = (strukt.unwrap(), trait_.unwrap(), func.unwrap());
    let field = FieldId {
        parent: VariantId::StructId(strukt),
        local_id: Idx::from_raw(RawIdx::from_u32(0)),
    };
    let krate = db.fetch_test_crate();
    let lookups = || {
        let s = AttrFlags::query(&db, strukt.into());
        assert!(s.contains(AttrFlags::HAS_REPR | AttrFlags::IS_MUST_USE));
        assert!(AttrFlags::query_field(&db, field).contains(AttrFlags::IS_DOC_HIDDEN));
        assert!(AttrFlags::repr(&db, AdtId::StructId(strukt)).is_some());
        assert_eq!(AttrFlags::must_use_message(&db, strukt.into()), Some(Some("use it")));
        assert_eq!(s.doc_aliases(&db, Either::Left(strukt.into())).len(), 1);
        assert!(AttrFlags::lang_item(&db, trait_.into()).is_some());
        assert!(AttrFlags::has_word(&db, trait_.into(), "marker"));
        assert!(AttrFlags::diagnostic_item(&db, func.into()).is_some());
        assert!(AttrFlags::export_name(&db, func.into()).is_some());
        assert!(AttrFlags::docs(&db, strukt.into()).is_none());
        assert!(crate::diagnostic_item::diagnostic_items(&db, krate).get(&Symbol::intern("the_fn")).is_some());
    };
    lookups();
    blaze_alloc_count::assert_at_most(0, "attribute lookups on a warm item", lookups);
}
```

(`Symbol::intern("the_fn")` of a name already interned is a probe, no allocation. Imports to add
to the test module: `either::Either`, `crate::{AdtId, VariantId}` are there; add `ModuleDefId`
if not.)

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-attrs, 2026-10-29): as written in `attrs/tests.rs`, the items found through a `root_owners` helper (also used by the I2.1 and §12 tests) and `"the_fn"` interned before the measure. Runs once M1 does (the queries' `frame::current`).

### I12.2 `lang_item.rs` tests: a lang-item lookup is zero; a notable-traits miss is its box

```rust
    /// A lang-item lookup is a memo read and a field load.
    #[test]
    fn lang_item_lookup_allocates_nothing() {
        let db = TestDB::with_files(
            r#"
#![feature(lang_items)]
#[lang = "sized"]
pub trait Sized {}
#[lang = "global_alloc_ty"]
pub struct Global;
"#,
        );
        let krate = db.fetch_test_crate();
        let _ = super::lang_items(&db, krate);
        let (sized, global) = blaze_alloc_count::assert_at_most(0, "a warm lang-item lookup", || {
            let items = super::lang_items(&db, krate);
            (items.Sized, LangItemEnum::GlobalAlloc.from_lang_items(items))
        });
        assert!(sized.is_some() && global.is_some());
    }

    /// A miss of `crate_notable_traits`, its inputs warm, allocates its box and nothing else.
    #[test]
    fn notable_traits_miss_is_its_result() {
        let db = TestDB::with_files(
            r#"
#[doc(notable_trait)]
pub trait Read {}
pub trait Other {}
"#,
        );
        let krate = db.fetch_test_crate();
        let def_map = crate::nameres::crate_def_map(&db, krate);
        for (_, module) in def_map.modules(&db) {
            for def in module.scope(&db).declarations() {
                if let crate::ModuleDefId::TraitId(it) = def {
                    let _ = crate::attrs::AttrFlags::query(&db, it.into());
                }
            }
        }
        let traits = blaze_alloc_count::assert_at_most(1, "a notable-traits miss", || {
            super::crate_notable_traits(&db, krate).map(<[TraitId]>::len)
        });
        assert_eq!(traits, Some(1));
    }
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-items, 2026-10-29): both tests as written, at the end of `lang_item.rs`'s `mod tests`. Of I12's other parts, I12.1 (`attrs/tests.rs`) is db-expand-attrs', and I12.3 (`import_map.rs`) and I12.4 (`signatures.rs`) are other children's files. The upvars and lang-item read budgets are also in `alloc_budgets.rs` (now compiled).

### I12.3 `import_map.rs` tests: a search after its buffers have grown is zero; a build is its nine

```rust
    /// With the caller's set grown once, an exact, prefix or fuzzy search of the dependencies
    /// allocates nothing.
    #[test]
    fn warm_search_allocates_nothing() {
        let db = TestDB::with_files(
            r#"
//- /main.rs crate:main deps:dep
//- /dep.rs crate:dep
pub struct Fmt;
pub mod fmt {
    pub trait Display { fn fmt(&self); }
    pub fn format() {}
}
"#,
        );
        let main = *all_crates(&db)
            .iter()
            .find(|krate| {
                krate.extra_data(&db).display_name.as_ref().is_some_and(|it| it.crate_name().as_str() == "main")
            })
            .unwrap();
        let queries = [
            Query::new("fmt".to_owned()),
            Query::new("fo".to_owned()).prefix(),
            Query::new("fmt".to_owned()).fuzzy(),
            Query::new("FMT".to_owned()).fuzzy().case_sensitive(),
        ];
        let mut found = FxHashSet::default();
        for query in &queries {
            search_dependencies_into(&db, main, query, &mut found);
        }
        for query in &queries {
            found.clear();
            blaze_alloc_count::assert_at_most(0, "a warm import-map search", || {
                search_dependencies_into(&db, main, query, &mut found)
            });
            assert_eq!(found, search_dependencies(&db, main, query), "{query:?}");
        }
    }

    /// `search_dependencies_each` allocates nothing of its own, visits what
    /// `search_dependencies` returns, and visits it in the same order every time, whatever
    /// was searched before.
    #[test]
    fn each_is_free_and_in_a_fixed_order() {
        let db = TestDB::with_files(
            r#"
//- /main.rs crate:main deps:dep,other
//- /dep.rs crate:dep
pub struct Fmt;
pub mod fmt { pub fn format() {} pub use super::Fmt as Formatter; }
//- /other.rs crate:other
pub fn fmt() {}
"#,
        );
        let main = *all_crates(&db)
            .iter()
            .find(|krate| {
                krate.extra_data(&db).display_name.as_ref().is_some_and(|it| it.crate_name().as_str() == "main")
            })
            .unwrap();
        let query = Query::new("f".to_owned()).prefix();
        // alloc: test, room for every visit, so the measure counts only the search
        let mut first = Vec::with_capacity(64);
        search_dependencies_each(&db, main, &query, |item, complete| first.push((item, complete)));
        assert!(first.len() < 64);
        let _ = search_dependencies(&db, main, &Query::new("fmt".to_owned()).fuzzy());
        // alloc: test, as `first`
        let mut again = Vec::with_capacity(64);
        blaze_alloc_count::assert_at_most(0, "a visiting import-map search", || {
            search_dependencies_each(&db, main, &query, |item, complete| {
                again.push((item, complete))
            })
        });
        assert_eq!(first, again);
        let visited: FxHashSet<_> = first.iter().copied().collect();
        assert_eq!(visited, search_dependencies(&db, main, &query));
    }

    /// A build of a crate's import map, its inputs warm, allocates its nine columns and nothing
    /// else: the collection, the sort and the keys are the frame's.
    #[test]
    fn build_is_its_columns() {
        let db = TestDB::with_files(
            r#"
//- /main.rs crate:main
pub struct S;
pub enum E { A }
pub trait T { fn m(); }
pub mod m { pub fn f() {} pub use super::S as R; }
"#,
        );
        let krate = db.fetch_test_crate();
        let memo = ImportMap::of(&db, krate);
        let built = blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| {
            blaze_alloc_count::assert_at_most(9, "an import-map build", || {
                ImportMap::build(&db, krate, frame)
            })
        });
        assert!(built == *memo);
    }
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-paths, 2026-10-29): the three tests at the end of `import_map.rs`'s `mod tests` (`TestDBExt` imported for `fetch_test_crate`). One change: `build_is_its_columns` allows 10, as `Csr::build` (speed-hir's item 1) allocates its cursors once per build. Not run.

### I12.4 `signatures.rs` tests: every signature query's warm re-request is zero; a miss is its value

```rust
    /// A second request of each signature query is a memo hit: no allocation.
    #[test]
    fn warm_signature_requests_allocate_nothing() {
        let (db, file_id) = TestDB::with_single_file(
            r#"
pub struct S<T: Clone>(T) where T: Copy;
pub union U { a: u8 }
pub enum E { A, B(u8) }
pub const C: u8 = 0;
pub static ST: u8 = 0;
pub trait Tr<T> { type A; fn m(&self); }
impl<T: Copy + Clone> Tr<T> for S<T> { type A = (); fn m(&self) {} }
pub fn f<'a, T: ?Sized>(x: &'a T) -> &'a T { x }
pub type A<T> = (T, T);
"#,
        );
        let module = db.module_for_file(file_id.file_id(&db));
        let scope = module.def_map(&db).module(&db, module).scope(&db);
        let impl_ = scope.impls().next().unwrap();
        let ask = || {
            for def in scope.declarations() {
                match def {
                    ModuleDefId::AdtId(AdtId::StructId(it)) => {
                        let _ = StructSignature::of(&db, it);
                        let _ = StructSignature::with_source_map(&db, it);
                        let _ = VariantFields::of(&db, it.into());
                    }
                    ModuleDefId::AdtId(AdtId::UnionId(it)) => {
                        let _ = UnionSignature::of(&db, it);
                        let _ = VariantFields::of(&db, it.into());
                    }
                    ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                        let _ = EnumSignature::of(&db, it);
                        let _ = EnumVariants::of(&db, it);
                    }
                    ModuleDefId::ConstId(it) => _ = ConstSignature::of(&db, it),
                    ModuleDefId::StaticId(it) => _ = StaticSignature::of(&db, it),
                    ModuleDefId::TraitId(it) => _ = TraitSignature::of(&db, it),
                    ModuleDefId::FunctionId(it) => {
                        let _ = FunctionSignature::of(&db, it);
                        let _ = FunctionSignature::with_source_map(&db, it);
                    }
                    ModuleDefId::TypeAliasId(it) => _ = TypeAliasSignature::of(&db, it),
                    _ => {}
                }
            }
            let _ = ImplSignature::of(&db, impl_);
            let _ = ImplSignature::with_source_map(&db, impl_);
        };
        ask();
        blaze_alloc_count::assert_at_most(0, "warm signature requests", ask);
    }

    /// A miss of `EnumVariants::of`, its syntax warm, allocates its map's two tables and
    /// nothing else: the variants gathered in the frame, no diagnostics, the bits inline.
    #[test]
    fn enum_variants_miss_is_its_map() {
        let (db, file_id) = TestDB::with_single_file("pub enum E { Some, None }");
        let module = db.module_for_file(file_id.file_id(&db));
        let scope = module.def_map(&db).module(&db, module).scope(&db);
        let e = scope
            .declarations()
            .find_map(|it| match it {
                ModuleDefId::AdtId(AdtId::EnumId(it)) => Some(it),
                _ => None,
            })
            .unwrap();
        let _ = variants_syntax(&db, e);
        let _ = AttrFlags::query(&db, e.into());
        let len = blaze_alloc_count::assert_at_most(2, "an enum-variants miss", || {
            EnumVariants::of(&db, e).0.variants.len()
        });
        assert_eq!(len, 2);
    }
```

(`Some`/`None` are predefined symbols, so naming them interns nothing; the `EnumVariantLoc`
interned per variant goes to salsa's intern tables, on regions. The test module imports
`crate::{AdtId, ModuleDefId, test_db::TestDB}`, `test_fixture::WithFixture`, and the signature
types from `super::*`.)

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand (db-expand-sigs, 2026-10-29): both tests in the new `crates/hir-def/src/signatures/budgets.rs` (`#[cfg(test)] mod budgets;` in signatures.rs). The items are gathered with `test_items::{declarations, impls}` before measuring, in place of `module_for_file`/`def_map().module().scope()`. The miss budget of 2 also assumes mem-db-salsa's M5 (intern tables on regions), since the `EnumVariantLoc` interns are global allocations until then.

> not applied by db-mem-def (21:39): db-expand-next2 holds these hir-def files and its children (db-expand-sigs, -items, -paths, -attrs) are applying them; it has not handed any over (asked by tmux 21:39). The `vendor/salsa` edit the tags need is M2 in `v2/requests/mem-db-salsa.md`, which speed-salsa-next3 has been given to apply after its S5.
