---
status: plan
summary: speed-hir-defs-sigs: item signatures, VariantFields and EnumVariants in hir-def's signatures.rs brought to the fast-code bar (flags by shift and mask, one keyword pass per item, exact-size field and variant tables, variant bitset), with before and after numbers against rustc's collect queries.
date: 2026-10-29
---
# speed-hir-defs-sigs

Path: `crates/hir-def/src/signatures.rs`. The store it lowers into (`ExpressionStore`,
`ExprCollector`, `lower_*`) belongs to speed-hir-store. The flags it reads (`AttrFlags`) belong to
[speed-hir-defs](speed-hir-defs.md).

## Hot path

On a cold build, each item's signature is lowered once: fn, struct, union, enum, const, static,
trait, impl and type alias, plus every variant's `VariantFields` and every enum's
`EnumVariants`. hir-ty then reads the signature for every use of the item. A read is
`X::of(db, id)`, which is `returns(deref)`: one salsa memo lookup giving `&X`, with no `Arc`
clone and no lock. `of` clones the `Arc` out of `X::relative` once, when the memo is filled.

## Lowered from the item, not the file

Every `with_source_map` used to read its item from the file (`loc.source(db)`: the file's tree,
span map and ast id map, all absolute). So any edit of a file, a body edit included, ran every
signature lowering of that file again. The results then backdated, but the work was wasted.

Now each signature is lowered from a `SignatureSyntax`, as bodies are from `BodySyntax`. The
`signature_syntax(db, GenericDefId)` query makes it. It is the item as a flat tree of its own,
its text starting at the item. The part no signature reads is replaced by an empty node of its
kind: the body of a fn, const or static; the item list of an impl or trait; the variant list of
an enum. It also holds the tree's span map (`RealSpanMap::within`) and ast id map
(`AstIdMap::relative_without`, from frontend-text), and it compares by the tree's content. So it
backdates whenever the item's outline text is unchanged, wherever the item moved.

- `X::relative(db, id)` lowers from it with those maps (`ExprCollector::with_maps`, from
  speed-hir-store). This is the only query that lowers.
- `X::of` reads `relative`.
- `X::with_source_map` (`lru = 1024`) moves `relative`'s source map to the file. A pointer at or
  after the empty node moves on by the length left out, the empty node becomes what it replaced,
  and the item gets its file range. This re-runs on every edit of the file, but it only
  rewrites pointers. It reads the item's file range off its ast id pointer, with no walk of the
  tree to the node.
- `variants_syntax(db, EnumId)` is the whole enum, with nothing left out. `EnumVariants::of`
  and the `VariantFields` of an enum's variants read it. A struct's or union's `VariantFields`
  reads its `signature_syntax`.
- An item a macro writes has no syntax of its own (`None`, as with bodies). It is lowered from
  its file, as before.

| edit | before | after |
|---|---|---|
| a body (fn, const, static, assoc fn, trait default) | every signature, `VariantFields` and `EnumVariants` of the file lowered again | none |
| another item's signature, moving this one | all of the file's | only the edited item's |
| an enum discriminant | all of the file's | that enum's `EnumVariants` and variant fields, not its `EnumSignature` |

The limit: an item whose spans are counted from an item around it is lowered from that whole
item, with nothing left out. That covers an item in a body, or one in an `impl`, trait or module
that has attributes (`RealSpanMap::anchor_at`). Any edit inside the enclosing item lowers it
again, because its spans hold offsets from the enclosing item's start. Bodies have the same
limit.

Memory: a `SignatureSyntax` keeps its own copy of the outline tree, not the file's. This differs
from `BodySyntax`, which keeps the file's text and parses again. The copy is the outline only,
so it is small. The exception is an item anchored at an enclosing one, which keeps a copy of
that whole enclosing item.

## Benchmark: rustc's `rustc_hir_analysis::collect`

rustc's `generics_of`, `fn_sig`, `adt_def` and `trait_def` read arena HIR. Each result is
arena-allocated (`&'tcx Generics`, `&'tcx TraitDef`) or interned (`AdtDef`, `PolyFnSig`).
Callers get a `Copy` handle of 8 bytes, and the global allocator is called 0 times per
signature. rustc reads the keywords of an item (`unsafe`, `async`, `const`, ...) from HIR fields
the parser already set. It reads attribute flags from parsed attributes, for example
`find_attr!(.., Fundamental)`.

## Before and after

Sizes come from the `const` assertions in the file. The store and generic params are 72 bytes
each, as of 2026-10-29, before speed-hir-store's type columns. The assertions are written as
"store + generic params + own bytes", so they still hold when those sizes change.

| | before | after | rustc |
|---|---|---|---|
| handle a reader gets | `&X` from the memo | same | `Copy` 8-byte arena or interned pointer |
| bytes per signature (8-byte `Arc` header not counted) | struct, union, enum, trait, impl 160; fn, type alias 184; const, static 88 (unchanged, now asserted) | same | arena bytes only |
| bytes of flags | 1–2 per signature | same, asserted per flag type | bit fields of the HIR node |
| flags from `AttrFlags` | one `if contains` and `|=` per flag: 3 for a struct, 6 for a trait, 5 for a fn | a branch-free shift and mask per flag, constants checked at compile time (`single_bit`) | `find_attr!` scan per attribute kind |
| struct lang items | a chain of up to 8 `_ if lang == sym::..` guards | one table of 8 `(Symbol, StructFlags)` pairs; only runs when `LANG_ITEM` is set | `LangItem` enum compare |
| keyword scans of the syntax | fn 6 child walks (`unsafe_token()` .. `safe_token()`); static 3; impl 4; trait 4; a field 1 walk through `NodeOrToken` cursors | 1 walk per item over the flat tree's kind column (`child_tokens`), no cursor built | 0 (HIR fields) |
| `VariantFields` field arena, n fields | grown by doubling (about log2 n allocations), then `shrink_to_fit` (one more reallocation) | `Arena::with_capacity(n)` from the syntax's count: 1 allocation; shrinks only when a field is cfg'd out | 1 arena bump |
| `VariantFields` of a unit or empty variant | builds and finishes an empty `ExpressionStoreBuilder` (takes and returns a pooled scratch set) | `ExpressionStore::empty().clone()`, no list setup at all: 0 allocations | 0 |
| `EnumVariants`, n variants | `FxIndexMap` grown by doubling (entries and index table each about log2 n reallocations), `Vec<bool>` grown and then boxed, both shrunk: about 11 allocations for 10 variants | map preallocated from the syntax's count (2 allocations: entries and index); `explicit_discriminant` is a bitset inline up to 64 variants (`VariantBits`, 24 bytes): 0 allocations; shrinks only when a variant is cfg'd out | 1 arena bump (`AdtDef` variants) |
| `explicit_discriminant` bytes | 16-byte `Box<[bool]>` + n bytes on the heap | 24 bytes inline for up to 64 variants, n/8 bytes past that | — |

## Semantics fixed on the way

An `unsafe fn` with `#[rustc_deprecated_safe_2024]` (std's `env::set_var`) now keeps `UNSAFE` as
well as `DEPRECATED_SAFE_2024`. rustc's `fn_sig` of it is unsafe. Before, only the second flag
was set, so its body was checked as a safe function's body and it coerced to a safe `fn` pointer.
db-ty-lower moved the deprecated-safe test first in `is_fn_unsafe_to_call`
(`crates/hir-ty/src/utils.rs`), so a pre-2024 call outside `unsafe` still only lints, as in
rustc. This was diagnostics' request 6 (`v2/requests/diagnostics.md`).

## Where it still doesn't match rustc, and what it would take

- **One `Arc`, a store and generic params per signature.** rustc bump-allocates all of a crate's
  signatures in one arena. To match, signatures would be rows in a per-crate column table,
  lowered with the item tree, and `X::of` would return an index into it. That is the salsa memo
  layout (lane db) and the item tree's columns (format), not this file.
- **The source map is built for every signature.** A second lowering without it can't be
  shared. `collect_block_` makes a tracked `BlockId` for a block in a const argument
  (`[u8; { .. }]`), so two lowerings give unequal stores. `of` is therefore a projection of
  `relative`, the one lowering, and `with_source_map` only moves `relative`'s map. The source
  map costs what speed-hir-store's sorted tables cost.
- **Keywords are read from syntax.** rustc's parser sets them on HIR. Matching that means the
  item tree storing a per-item keyword byte (format/items). `child_tokens` is already one pass
  over a `u16` column.
- **`InactiveCode { opts: CfgOptions }`** clones the crate's whole cfg set for each cfg'd-out
  field. The options are always the crate's, so the diagnostic can read them when it is
  reported. That edit is in speed-hir-store's types; it was filed with them as a finding.

## Tests (in `signatures.rs`)

- `flags_from_attrs_as_one_if_per_flag`: every signature's `from_attrs`, and the const and
  static macro uses, against the old one-`if`-per-flag code kept as the model. Inputs are all
  128 single bits of `AttrFlags`, none, all, and 4096 seeded random words.
- `struct_lang_items_as_a_match`: the table against the old guard chain, over every entry,
  other lang items, and freshly interned names.
- `child_tokens_as_the_accessors`: the one-pass scan against each `*_token()` accessor, for every
  fn, static, impl, trait and field of a fixture. The fixture covers each keyword, and the test
  asserts that each keyword is actually seen, so it can't pass vacuously.
- `variant_bits_as_bools`: the bitset against a `Vec<bool>` model across the 64-bit word
  boundaries (0, 1, 63, 64, 65, 127–129, 200, 1000), including `truncate`, and inline storage
  up to 64 variants.
- `relative_tests` (`TestDB::log_executed`), over a fixture with every kind of signature, a
  `macro_rules!` type in signatures, and an attributed impl:
  - `a_body_edit_lowers_no_signature`: every body is edited at once. No `relative` or
    `EnumVariants::of` runs, and every signature is the same memo as before.
  - `an_edit_of_another_item_lowers_none_of_this_one`: a field is added to the first struct,
    moving every item after it. Exactly that struct's signature and fields are lowered again.
  - `a_discriminant_edit_lowers_the_variants_not_the_enum`.
  - `moved_source_maps_are_the_files`: before and after the body edits, each signature and
    `VariantFields`, moved from the item's tree, equals the lowering from the whole file (store
    and source map).
- The `signatures_lower_no_body` test in `nameres/demand_tests.rs` (db-expand-split) still holds:
  no signature query reads a body. `HAS_BODY` and `explicit_discriminant` are read off the syntax.

## Speed of light

The bounds, the arithmetic and the gaps are in
[speed-of-light.md](speed-of-light.md#hir-def-signatures-signaturesrs-speed-hir-defs-sigs).
Applied: the tree is compared by content instead of two SipHash passes (≈ 285 ns per item per
edit), and the source map's move reads the item's range off its ast id.

Also applied, over frontend-text's list forms of `clone_subtree_without`,
`AstIdMap::relative_without` and `RealSpanMap::without`:
- **`file_outline(db, EditionedFileId)`**: one query per file. It holds the file's tree with
  the body of every fn, const and static that is its own span anchor left out, each replaced
  by an empty node. It also holds that tree's ast id and span maps, and where each body was
  cut.
  - It is made in one pass over the file's items and one copy that never reads a cut body.
  - It compares equal after any body edit. Nothing after a body moves, because a cut body has
    no length in the outline.
  - `signature_syntax` and `variants_syntax` read it and not the parse. So on a body edit, the
    file's one outline query runs and backdates, and no per-item syntax query runs. This is
    rustc's per-owner HIR hash, taken per file.
- **`SignatureSyntax::new`** copies the item with `clone_subtree_without`, one pass. Before, it
  cloned the whole item, then repacked it with `replace_subtree` and copied its text.
- **An item anchored at one around it** is copied without the outline's empty bodies in that
  anchor. The ast id map would otherwise take an empty fn body for one it holds. Those bodies
  all come after the item, so nothing in it moves.
- **An item in a cut body** is not in the outline, and is read off the file as before, the same
  as the body it is in.
- **What still re-runs on a body edit:**
  - the bodies of items anchored at an enclosing item, which stay in the outline: items of an
    impl, trait or module with attributes, or of a module nested in a module;
  - items inside cut bodies.

  For both, an edit runs the file's signature syntax queries again, as every edit did before.
- **Tests** (`relative_tests`):
  - `the_outline_leaves_out_each_body_of_its_own_anchor`: the outline's text, and that each cut
    is an empty body of one of its items.
  - `a_body_edit_runs_no_signature_syntax`: `file_outline` runs and nothing per item does.
  - `items_anchored_at_a_fn_are_read_around_its_body`: covers an item in a cut body, read off
    the file, and an item in a fn's parameter type, read off the outline without the fn's body.
    Each keeps its ast id.
