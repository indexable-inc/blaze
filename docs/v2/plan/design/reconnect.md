---
status: plan
summary: How the parked type checker (hir-ty and the hir-ty-* crates) is reconnected to the new item layer without touching its call sites: the names it asks base-db, hir-expand and hir-def for, counted; one task's database over the records; ids interned from item keys; a missing record as a typed stop that names its key; a resolver and narrow def-map accessors over per-module scopes.
date: 2026-11-15
---
# Reconnecting the type checker

The old item layer (the world, the def map, the item tree's persistence, `deps::`/`frozen::`) was
deleted on 2026-11-15 (bottom.md). The type checker was parked, not deleted: 153k lines in
hir-ty and 353k in the hir-ty-* crates, asking hir-def about 1,500 call sites' worth of
questions. The decision: keep every name and signature they call, and answer each from the new
records (identity.md, bottom.md). This is the plan, the census it rests on, and what is built.

## The census

Counted over the 772 `.rs` files of `crates/hir-ty` and `crates/hir-ty-*` at the tree of
2026-11-15 (a word count of each imported name's last segment in each file that imports it, plus
every inline `hir_def::…` path; the script and its output are kept with the lane's notes).

| crate | distinct paths imported | uses |
|---|---:|---:|
| `hir_def` | 277 | 16,288 |
| `base_db` | 22 | 3,471 |
| `hir_expand` | 32 | 1,798 |

By group (uses):

- **Ids and their conversions.** `FunctionId` 1,267; `AdtId` 1,062; `DefWithBodyId` 891;
  `TraitId` 500; `ModuleDefId` 416; `GenericDefId` 379; `VariantId` 367; and the other `*Id`
  types and their `From`/`TryFrom` conversions. `hir::ExprId` 933, `hir::Expr` 641.
- **`Lookup`/`Intern`/`HasModule`.** `.lookup(` 258; of the fields read after it: `.container`
  152, `.parent` 29, `.id` 29, `.source_view` 5, `.def` 2, `.ast_ptr` 2, `.name` 1.
  `.krate(db)` 443, `.module(db)` 136.
- **Signatures.** `FunctionSignature::of` 372, `StructSignature` 80, `TraitSignature` 75,
  `StaticSignature` 45, `EnumSignature` 39, `ConstSignature` 31, `UnionSignature` 25,
  `TypeAliasSignature` 21, `ImplSignature` 21.
- **Generics.** `GenericParams::of` 61.
- **Fields and variants.** `VariantFields::of` 15; `.fields(` 184; `enum_variants` 100.
- **Attributes and features.** `AttrFlags::query` 114 (308 uses of the type);
  `UnstableFeatures::query` 18.
- **Lang items.** `lang_items(` 242; field reads on the struct (`.Sized`, `.FnOnce`,
  `.Fn_call`, `.Deref_deref`, …) 268; `is_lang` 57.
- **The crate graph.** `base_db::Crate` 582; `all_crates` 135 (128 calls); `.data(db)` 225;
  `.extra_data` 60; `.edition` 70; `.cfg_options` 22; `.origin` 30; `transitive_deps` 5;
  `rust_name` 4; `.content` 11; `named` (in coherence and method resolution).
- **`DefMap` / `crate_def_map`.** `crate_def_map` 219 uses (367 references). Chained after it:
  `.modules` 53, `.root_module_id` 22, `.views` 3, `.recursion_limit` 3, `.handles` 2,
  `.pattern_types`/`.features`/`.early_drop` 1 each. On a def-map variable: `.modules` 77,
  `.module` 44, `.crate_root` 9, `.root_module_id` 8, `.krate` 2, and one each of
  `.is_rustc_coherence_is_core`, `.handles`, `.fn_as_proc_macro`, `.features`, `.early_drop`,
  `.diagnostics`. `block_def_map` 13. `.scope(db).X`: `declarations` 101, `impls` 25, `get` 5,
  `entries` 4, `set` 3, `derive_macro_invocs` 3, `builtin_derive_impls` 3, `values` 2, `macros`
  2, and one each of `iter_macro_invoc`, `inherent_impls`, `extern_blocks`, `all_macro_calls`.
- **`Resolver` methods** (calls in hir-ty*): `krate` 553, `module` 251, `generic_def` 82,
  `def_map` 77, `expression_store_owner` 54, `update_to_inner_scope` 29,
  `resolve_path_in_value_ns_fully` 26, `reset_to_guard` 25, `resolve_path_in_type_ns_hygienic`
  8, `resolve_path_in_type_ns_fully` 8, `resolve_path_in_type_ns` 8, `resolver_for_scope` 7,
  `resolve_path_in_value_ns` 6, `top_level_def_map` 3, `is_visible_written` 3,
  `resolve_path_as_macro` 2, `privacy_scope` 2; one each of `traits_in_scope`,
  `resolve_path_in_value_ns_with_prefix_info`, `resolve_module_path_in_items`,
  `resolve_lifetime`, `record_trait_uses`, `record_macro_path_uses`, `generic_params`,
  `for_each_trait_in_scope`, `all_generic_params`. `HasResolver` 25, `resolver(` 58,
  `ValueNs` 162, `TypeNs` 133.
- **Bodies and `ExpressionStore`.** `Body::of` 127 (315 uses of the type),
  `ExpressionStore::of` 46, `with_source_map` 16, `Body::of(..).blocks(db)` 54.
- **`MacroCallId`, hygiene, `SyntaxContext`.** `MacroCallId` and `HirFileId` across the
  diagnostics and the MIR source maps; `hir_expand::name::Name` 202.
- **File ids, `InFile`, `AstId`.** `EditionedFileId` in method resolution, opaques and the MIR
  locations; `InFile` and `AstId` mostly in diagnostics.
- **The database and its tracked queries.** `base_db::Db` 2,431 (the type of every `db`);
  `#[blaze_memo::tracked]` over `&Db` throughout.
- **The old persistence**, gone with the layer: `deps::` 94 uses (`ImageStore` 28 with 59
  references, `PortableIds` 27), `frozen::` 78, `hir_expand::fixture` 1,202 (tests only),
  `base_db::Edit`/`Committed`/`Given`/`Located`/`CrateKey` in the per-crate caches.

## The database

`base_db::Db` keeps its name, and every query keeps taking `&Db`. It is one task's snapshot;
`'db` is that task's life. Every id, every `&'db` borrow of a memo value and every view of a
record is valid until the task ends and is never written anywhere that outlives it: a record
names an item by its key (identity.md), an id is not `blaze_id::Portable`.

It holds, concretely (no `&dyn`):

- the memo (`blaze_memo::Memo`), with a `Crate` input per crate in that crate's arena;
- `Records`: every crate's surface records the task may read, checked once when inserted and
  sorted at seal, implementing `blaze_surface::Items` for every crate and `Source` for the
  crate whose text the task holds (it holds that crate's `HasText`);
- the task's `LocalTable` (an item key to the small number the task knows it by), behind a
  named lock held for one question;
- the crate list in topological order, by key;
- the file table (a span's `FileId` is an index; the content is the file's id), the include
  table (`include!`-family paths to files of the table, bytes of a file with no text) and the
  driver's gifts (a proc-macro crate's loaded expanders, `base_db::Gifts`);
- the front-end settings.

The driver makes one per task (`Db::new`) and drops it when the task answers. No `Edit`, no
commit: a command answers and exits (rule 70), and what the old edit loop moved between
snapshots is now records.

`Crate` is a memo input again: `key: CrateItemKey` (the prefix of every item key of the crate),
`data` (edition, dependencies, origin, target kind, crate types, root file), `extra_data`,
`workspace_data`, `cfg_options`, `env`, `extensions`. The driver writes `data.dependencies`
from the `CrateGraph` record so the two never disagree. `named`, `rust_name`, `content` (its
key's content) and `transitive_deps` keep their signatures.

## Ids

`FunctionId` and the rest stay interned `Copy` ids in the memo, but are interned from what the
item *is*, not where the old tree had it: `ItemLoc { container, key: ItemKey }` and
`AssocItemLoc { container: ItemContainerId, key }`. The container is the record's parent
(`Items::parent`), read once at intern time. `.lookup(db)` still returns a location value of the
same type name, answered from the record. `Db` implements `blaze_id::KeyOf<Id>` and
`blaze_id::Resolve<Id>` for every id type (the orphan rule allows it: the id types are hir-def's,
the traits are blaze-id's and implemented in hir-def), so a record written by the type checker
names an item through the one pair of functions identity.md asks for.

The location loses its `id: AstId<N>`. Where a caller reads `.lookup(db).id` it now reads the
item's place (`Items::place`) and, for the crate being compiled, the syntax node through
`Source`; those callers are listed under "Callers that need edits".

## A missing record is a typed stop

A compiler task never parks and never computes another item's record inline. A facade function
that needs a record the task does not hold, for a crate without text, stops the task: it raises
`base_db::Need { why: SurfaceError }` by unwinding with it as the payload, the one mechanism
`blaze_memo::Cancelled` already uses to carry a typed stop through recursive query code
(`resume_unwind(Box::new_in(stop, Global))`; `Need::catch` downcasts it and resumes any other
panic). `OrNeed::or_need` turns a `Result<T, SurfaceError>` into that stop. Every stop carries
the missing key (`Needed::{Item, File, Crate}` from `SurfaceError::{NoRecord, NoFileRecord,
NoCrateRecord}`), so the set of stops one run of a task raises is its `Need(keys)`. A
`SurfaceError` that names no missing record (`NotBuilt`, `WrongCrate`, `NoItem`, `NoLocal`,
`LocalsFull`) also unwinds as a `Need`, and its `needed()` is `None`: the driver reports it as the
error it is and does not re-enter (rule 3).

Reads that succeed are the task's read list, recorded by the memo engine as today: a record read
is a read of the input that holds it.

**Re-entry.** The driver catches the stop at the task's top, makes the named record (a task of
its own, keyed by the record's key), inserts it into the next `Records`, and enters the task
again from the top in a new `Db`. What the first entry computed is not lost: every record it
read or wrote is in the store under its key, and every record its first entry already made is
found there by key; nothing inside the stopped run is reused, and nothing needs to be (a run
stops at its first missing key, before the costly part of the work for the item it is about).

**Which facade functions can raise `Need`.** Every function that reads a record of another
crate: the id `.lookup(db)` and `HasModule` (the `KeyIndex` page of the item's module),
`crate_def_map`'s accessors (`CrateRoot`, `ModuleScope`, `ModuleDecls`, `KeyIndex`), every
`Resolver` path method (`ModuleScope` per module on the path), `lang_items` (`LangItems`), the
crate graph accessors (`CrateGraph`), the signatures, generics and fields of an item of another
crate (records not yet defined: see below), `Db::file_text` and `EditionedFileId::parse` for a
file of another crate, and `ProcMacros::get_for_crate` never (a gift, not a record).

## The resolver

`hir_def::resolver::Resolver` keeps its method names. It is a stack of scopes: the module scope
(blaze-scope's per-module `ModuleScope` record, read one name at a time through
`ScopeView::get(ns, name)`, with the crate root's prelude and extern prelude from `CrateRoot`),
then the generic scopes of the enclosing item, then the body's local scopes (`ExprScopes`, as
today, made from the body). `resolve_path_in_*_ns*` walk a path segment by segment: each module
step is one record read; an `Undetermined` or `Ambiguous` target is the typed refusal blaze-scope
already gives. A method that cannot be served from the records yet returns
`SurfaceError::NotBuilt { method, item }` as a stop.

## Narrow def-map accessors

No whole-crate map is rebuilt. `crate_def_map(db, krate)` returns a small handle with the old
accessor names, each a record read: `root_module_id` and `crate_root` (`CrateRoot`), `module(id)`
(the module's `KeyEntry`), `[id].scope` (`ModuleScope`, per name), `features` and
`recursion_limit` (the crate root's attributes), `krate`. `block_def_map` is the same over a
block's scope. The prelude and the extern prelude come from `CrateRoot`.

Uses that truly need "all modules of a crate" (`def_map.modules(db)` over every module): the
tests aside, `hir-ty/src/method_resolution.rs` (16: collecting impls and traits per crate and
per block), `hir-ty-codegen/v2/global_asm.rs`, `roots.rs` and `stable.rs`,
`hir-ty/src/lower/batch.rs`, `hir-ty-pure/src/models.rs`, `hir-ty-matchck/src/typo.rs`. These
want a crate's item list, which is the union of its `KeyIndex` pages: `modules` is served from
those pages (one read per module the crate has); method resolution's impl collection should
move to blaze-impls's records, which answer "the impls of this trait or self type" without
listing modules.

## Callers that need edits

These read what the new layer does not keep, so the callers change (each in the lane that owns
the crate):

- `.lookup(db).id` (the `AstId`): hir-ty-codegen `stable.rs:222,239,255`, hir-ty-lean
  `body.rs:1491`, hir-ty `body_store/entries.rs:1215-1222` and its tests, `method_resolution/
  frozen/pages.rs:120-132`, `used_imports/tests.rs`; `.source_view`: hir-ty-mir
  `const_check/stability.rs`, `next_solver/format_proof_tree.rs`; `.ast_ptr`: hir-ty-mir
  `mir/pretty.rs:693`, hir-ty-codegen `v2/roots/harness.rs:133`. They ask the item's key and,
  for text, `Source`.
- `deps::*` and `frozen::*` (`ImageStore`, `PortableIds`, the frozen pages): the old persistence;
  replaced by records keyed by item key.
- `base_db::Edit`, `Committed`, `Located` (`check_cache.rs`, `consteval/deps.rs`,
  `hir-ty-borrowck/budget.rs`, `hir-ty-codegen/v2/roots/kept/frozen.rs`): there is no edit;
  what was given per crate is a gift (`base_db::Gifts`, `Crate::given`) or a record.
- `hir_expand::fixture` (1,202 uses, tests only): the fixture built a world; tests build a
  `Db` from records (blaze-surface's `MemSurface` behind its testing feature).
- `db.file_text(f).text(db)` keeps its shape; `base_db::line_index` keeps its signature.

## What the new-layer crates lack

- A production `Items`/`Source` implementation over the store's packs; `Records` (base-db) is
  the concrete surface until it is in blaze-surface.
- Records for signatures, generic parameters, fields and variants, attributes (`AttrFlags`),
  and expansions: `Items` has no methods for them, so for a crate without text those facade
  functions answer `NotBuilt`.
- A macro definition record readable by hir-expand's declarative expander
  (`MacroDefView` holds the definition's place, not its tokens).
- The include table and the gifts are the driver's facts today; they want a record of their
  own (the crate's surface names its `include!` paths).
- What hir-def stops on today, each a `NotBuilt` naming the item: an `AssocItems` record (the
  `KeyIndex` page stands in); block scopes; `Expansion` records (def-site hygiene of a
  `macro`, a body's macro calls, builtin derive impls, an expanded item's name key); the
  textual `macro_rules!` scope at a position and the `#[macro_use]` prelude; `use T as _`
  bindings and import provenance in `Binding` (the unused-import lint is parked); macro
  attribute flags in `MacroDefHead`; an `Attrs` record and crate attributes of a crate without
  text; `Signature` records for dependencies; extern blocks; which `extern crate` added an
  extern-prelude name; diagnostic items in bodies, on extern blocks and of dependencies;
  command-line crate-attr files. The driver must set `Crate.blaze_edition`.
