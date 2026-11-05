---
status: plan
summary: dedup-names (2026-10-29): blaze-engine's and blaze-wasm's resolvers of rustc's symbols share one spelling of paths, the crate rustc-symbols, which no-rustc step G deletes.
date: 2026-10-29
---
# dedup: one rustc-symbol resolver

## The copies

- `crates/blaze-engine/src/lib.rs`, `Resolver`: `path`, `item_path`, `scope_path`, `type_path`,
  `function_at`, the bodies of `closure`, `static_`, `declared_in`, `impl_place` and `item`, and
  the closure and static counter (`place_in_definition`, `place_counted`, `Count`,
  `LateImports`, `Places`, `name_in_path`, `is_closure`, `is_definition`, `closure_number`).
- `crates/blaze-wasm/src/names.rs`, whose doc said it was the first copied "without what a
  checkout on disk needs". It had drifted: no path for what a trait provides, a static in a
  `const _` left unnamed, `function_at` comparing ranges where the engine compares AST ids, and
  the old count of closures, which went into every macro in turn and guessed where the engine's
  says it can't tell (late imports, attribute macros, macros expanded twice in a body).

## What was picked

The engine's copy, which is the newer in every place they differ, became
`crates/rustc-symbols` (trait `rustc_symbols::Paths`). A resolver gives it four things:
the database, how its table of rustc's symbols spells a crate (`crate_path`, a `Cow` so the
engine can lend the spelling its per-build memo holds and blaze-wasm can build one), its
`Semantics` (the engine's sees each file as a module of the program's crates), and its `Places`,
the per-definition counts shared by a build's threads. It gets back the path of a function, an
item, a type, a closure, and where a static in a body is (`StaticPath`), which each side looks up
in its table. What only one side has stays there: the checkout, the prebuilt standard library,
`ResolverParts`, the engine's crate ids and own names, blaze-wasm's FNV crate id.

Why a crate and not a module of `hir-ty-codegen`, where `SymbolResolver` is: nothing would cycle,
but the code needs `hir`, which `hir-ty-codegen` has only as a dev-dependency. As a normal one, the
code generator could not start building until all of `hir` had. The code uses nothing of
`hir-ty-codegen`, `rustc-symbols` is not a hot crate, and the no-rustc deletion is one directory.

## On views

The crate reads syntax through views (`syntax::view`, `hir::view::Semantics`, frontend.md item
16), as blaze-wasm's `names.rs` does since frontend-tree-move-tools moved it: no `SyntaxNode`
and no handle `Semantics`. `Paths::sema` gives a `hir::view::Semantics` (the engine's
`new_within` its programs' crates). `function_at` keeps the engine's match by AST id, through
`AstIdMap::ast_id_of_view`; a block module is found by `Module::definition_source_range` and
`Node::descendant_at_range`; `impl_place` reads `HasSourceView::source_view`. The counter's
nodes are `Node<'db>`, `Copy`, so `along`, `target` and the waiting macro calls are copied
views, and `name_in_path` lends the name from the tree (`Option<&str>`) where it built a
`String`. The view `Semantics` methods it calls past those in `view.rs` today
(`ancestors_with_macros`, `expand_macro_call`, `scope`, `resolve_path`, `resolve_macro_call`,
`resolve_attr_macro_call`, `to_def`, `Module::declaration_source_view`) are the ones
docs/v2/plan/lanes/frontend/tree-move-hir.md declares.

## No rustc

This is the bridge to code rustc built, which the no-rustc rule removes: step G of
docs/no-rustc.md, [#87](https://github.com/indexable-inc/blaze/issues/87), links nothing of
rustc's, and the crate goes with it. Nothing was widened: blaze-wasm still names no function a
trait declares, and a closure or static the count can't place is no symbol, where the old copy
guessed.

## Speed of light

The work per question is a walk up the syntax from the item to its module or the function around
it, one string the length of the path, and for a closure or a static in a body one walk of the
definition it is in. The bound is that walk and one write of the path: tens of nodes and under
200 bytes, about a microsecond. On views the walk itself allocates and counts nothing per node.
What makes it slower than that is the count of a definition's
closures and statics, which is linear in the definition and, done once per closure, quadratic.
`Places` keeps each definition's count for every thread of a build (#259), so it is done once
per definition and the rest are lookups. The engine memoizes the answers per build
(`mem-codegen-engine` 3-5), so each is worked out once and a unit's questions afterwards are
lock-free probes. The strings it builds are the paths the symbol table is keyed by, so building
them is the work itself. That is within 10% of the bound for a cold path that goes away with G.
