---
status: plan
summary: frontend-nameres-diff — the proof that the per-module resolver (`facts::collect_crate`, `facts::collect_block`) builds the old collector's def maps, on every name resolution and macro expansion fixture and on generated crates, and that an edit reruns only the queries whose facts changed.
date: 2026-10-29
---
# frontend-nameres-diff: old resolver against new, and what an edit reruns

Under [nameres.md](nameres.md) sections 8 and 10. Code: `crates/hir-def/src/nameres/facts/tests.rs`
and `facts/tests/{differential,generated,trace}.rs`.

## The two sides

- **Old**: `old_crate_local_def_map(db, krate)`, a test-only `#[salsa::tracked]` fn whose body is
  `crate_local_def_map`'s before the hook (`DefMap::empty`, `collector::collect_defs`,
  `hand_over_scopes`), without `frozen`, which serves and checks images the same way for both.
  `old_block_def_map(db, block)` is `block_def_map`'s old body. Each is a query of its own, so
  the tracked structs it makes (module ids, `ModuleScope`s) live in one.
- **New**: `new_crate_local_def_map` calls `facts::collect_crate`; `new_block_def_map` calls
  `facts::collect_block`. Both start from the same empty block map (`empty_block_def_map`) and
  read the same `crate_local_def_map` for the enclosing crate, so the only thing that differs
  is the collector.

## The canonical projection (`project`, `compare`)

The two sides' `ModuleId`s differ (tracked structs made by different queries), and so does
every item id whose loc names a module. `project` maps a `(DefMap, LocalDefMap)` to `CDefMap`:

- a module of the map is a `ModKey`: its names from the map's root and its `ModuleOrigin`
  (which tells apart a `mod` declared twice); a module of another map (a dependency's, an
  enclosing block's) stays its id, `CModule::Foreign`, since both sides share it;
- an item is its kind, its container's canonical form and its `AstId`; a macro keeps its
  expander, flags and edition; a use tree is its `use` item and tree index;
- every field of `DefMap`, `ModuleData`, `ItemScope`, `DefMapCrateData`, `LocalDefMap` and
  `DeriveMacroInvocation` is destructured with no `..`, so a new field stops the build until
  the projection says what to do with it. `ItemScope::{names, sorted, by_def}` are indexes over
  the columns, derived and left out; `DefMap::modules` is the table walked;
  `diagnostics_by_module` is checked against `diagnostics` (same diagnostics per module, same
  order) and then each module's diagnostics are compared;
- reorderable per §8.1: the three namespaces sorted by name, every other column and side map
  sorted by its canonical form (`Debug` text, which holds no side-specific id), modules by path
  then origin. Kept in order: the macros of one name in `legacy_macros` (definition order picks
  which a call names) and the extern prelude (`FxIndexMap` order is observable).

`compare` names the module and the field of each difference and lists entries only on one side.
`assert_handed_over` checks the new map's modules and scopes are `Collected`, as the frozen
interface says `collect_crate` returns them.

## Fixtures (`differential.rs`)

The fixtures are read out of the test files themselves, not copied: `include_str!` of each of
`nameres/tests.rs`, `nameres/tests/*.rs`, `macro_expansion_tests/**/*.rs`, and a small lexer
(`fixtures`) that returns every raw string literal outside an `expect![..]`, named
`file::item#k` by the `fn`/`const`/`static` it is in, skipping comments, plain strings, char
literals and lifetimes. A literal `ChangeFixture` can't parse is not a fixture and is listed on
stderr; every other one is a crate compared old against new, with the builtin-derive fast path
on and off (`with_fast_path`, a fresh database each, since the setting is read per collection
and memos would carry it), for the crate maps of every crate in the fixture and the block maps
of every block of every body (functions, consts, statics, enum variants, impl and trait items,
unnamed consts), recursing into blocks of blocks. `every_test_file_is_compared` fails when a
new test file appears that the lists miss. The fixture database offers the
`identity_when_valid` attribute macro the macro expansion tests use.

§8.2's order-dependent programs are `ORDER_DEPENDENT`, each with the error rustc gives it and the
names (or unresolved-import diagnostics) left out of the comparison; everything else in them is
compared exactly. `two_globs_keep_the_first_by_position` checks the new resolver's
deterministic choice (§8: component order, module order, position). §8.3 (a within-round write
seen one round later) has the same final map, so its fixture is compared exactly. `OWN` adds
fixtures for what the test files leave out: a three-module glob cycle with shadowing,
restricted visibilities, `extern crate self as` with `$crate`, `#[macro_use]` and
`#[macro_export]` through modules, cfg'd items and imports, nested blocks with imports.

## Generated crates (`generated.rs`)

A model (`Model`: a module tree of inline and file modules up to depth 3, a module now and then
declared twice; structs and fns of every visibility including `pub(in crate::..)`; plain,
aliased, `as _` and glob imports by absolute, `self`, `super` and 2018 relative paths, which
make cycles and shadowing; `macro_rules!` exported or not under `#[macro_use]` modules, whose
bodies make items, modules with `$crate` globs, and modules calling other exported macros;
`extern crate self as`; `#[cfg(on)]`/`#[cfg(not(on))]` wrappers; editions 2015 to 2021) drawn
from a xorshift seed, so a seed is a case and a failure replays. A failing case is shrunk on the
model, not the text: drop one item or every call of one macro, keep the change while the
difference stays, until no smaller model differs; the panic prints the smallest fixture. This
does what proptest does, with a shrinker that knows the program's structure: dropping a `mod`
drops its file and everything in it, and a macro's calls go together. 256 cases per run.

## What an edit reruns (`trace.rs`)

Events come from `TestDB::log`; each `WillExecute` gives the ingredient's name (the query's fn
name, matched exactly) and the key's `salsa::Id`. Every resolver query takes one interned key
(`facts/queries.rs`: `DeclKey`, `FactKey`, `SccKey`, `RoundKey`), so the test reads the key back
with `FromId` and names the module (`DeclKey::decl().module`, `FactKey::module`,
`SccKey::anchor`) by its path. The fixture: `b` reads `a::foo` and `a::N` (both missing), `c`
globs `a`, `d` reads `e::E`, `y` globs `e`, the root only declares.

| edit | expected | test |
|---|---|---|
| `pub fn foo` added to `a` | `module_decls` of `a` only; `scc_state` of `a`, `b`, `c` only; no `partition` | `adding_an_item_reruns_its_module_and_its_readers` |
| `use crate::e::E` added to `a` | `import_shape` and `partition` rerun; `scc_state` of `a` and `c` only | `adding_an_import_reruns_the_partition_and_the_components_whose_facts_changed` |
| a name added to `e` | `scc_state` of `e` and `y` (its glob) only | `a_name_a_glob_makes_visible_reruns_only_that_globs_component` |
| `pub struct N` added to `a` | `b` reruns and `use crate::a::N` resolves | `a_negative_fact_flips_when_the_name_appears`, `a_negative_fact_holds_before_the_name_appears` |
| `foo` removed again | `a`, `b`, `c` rerun | `removing_a_read_name_reruns_its_reader` |
| `b`'s imports changed | `module_decls` and `scc_state` of `b` only | `editing_a_reader_reruns_only_the_reader` |
| a fn body edited | no resolver query | `an_edit_inside_a_body_reruns_no_resolver_query` |

After each edit the edited database's old and new maps are compared too, so a trace test also
proves the incremental answer equals a from-scratch old collection.

## State of the art

- rustc proves incremental reuse with `#[rustc_clean]`/`#[rustc_dirty]` and `-Z query-dep-graph`
  per query node; it has no differential oracle for resolution, which it never reimplemented.
  The trace tests here are that, per module and per fact, plus the equality check after the
  edit, which rustc's tests don't have.
- Compiler differential testing (Csmith, YARPGen, rustc's own `tests/ui` run against
  `-Zincremental-verify-ich`) compares two implementations on generated programs; the generator
  here shrinks on the program model, as C-Reduce does on syntax, without leaving the space of
  programs the generator can make.

## Speed of light

Tests, not a hot path: the cost is two collections and two projections per fixture and
setting. A projection is one pass over the map plus a sort per column, `O(n log n)` in the
map's entries; formatting for the sort key (`sort_by_cached_key`) is once per entry. The fixture
lexer is one pass over each test file. No part runs in a build.
