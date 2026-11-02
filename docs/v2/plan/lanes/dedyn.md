---
status: plan
summary: Lane dedyn's notes: how each hot-path `dyn` in the front end and the hot crates was replaced (concrete type, generic, enum, fn-pointer table) or kept with a reason, and what the lint misses.
date: 2026-10-29
---
# dedyn: hot-path `dyn`

## The rule applied per site

1. One implementation on v2's path: the concrete type.
2. Callers that differ at compile time and a small body: a generic (`impl Fn`/`&mut impl
   FnMut`, `?Sized` where a caller may still hold a `dyn`). Call sites written `&mut |..| ..`
   or `&|| ..` stay as they are: `&mut F` and `&F` implement the closure traits.
3. A closed set behind a large function: an enum with a `match` (a generic would copy the
   function per implementation).
4. A real boundary (debug output, errors, the I/O driver chosen once, rustc's panic payloads)
   or a site where the generic would copy a large function per node type and no concrete type
   is cheaper: kept, with `// dyn: <why>` on the line.

## Front end (the parent's part)

| site | became | why |
|---|---|---|
| `hir-ty/src/traits.rs` `implements_trait_unique_with_infcx`, `could_unify_with_infcx`, `impl_may_apply` | `impl FnOnce` (applied) | called once to seed a fresh inference context; 5-25 line bodies |
| `hir-ty/src/layout.rs` `struct_tail_erasing_lifetimes` | `&F: Fn` (applied) | three callers, a small recursive body; recursion stays in one instance |
| `hir-ty/src/lower.rs` `dyn TyLoweringInferVarsCtx` | enum `InferVars` (inference, a body's `InferCtxt`, the IDE's answers) | closed set of three; `TyLoweringContext` is too large to copy |
| `hir-ty/src/lower.rs:790` and `consteval.rs` `create_anon_const`/`path_to_const` | generics | two or three callers, small bodies |
| `hir-ty/src/consteval.rs` memory-map writer/reader callbacks | generics | one production caller each |
| `hir-ty/src/method_resolution.rs` impl visitors | `&mut impl FnMut` | two closures per visitor, bodies under 20 lines |
| `hir-def/src/nameres.rs` `with_ancestor_maps` | `&mut impl FnMut` | five small instances |
| `hir-def/src/lib.rs` `macro_call_as_call_id` | hir-expand's justified `EagerCallBackFn` | one call per eager-nested macro call; a generic copies the recursive eager expander per caller |
| `hir-def/src/expr_store/lower.rs` `check_cfg`, `may_have_attrs` | `&impl HasAttrs` | per expression; small bodies; the full cfg check behind them keeps its justified `&dyn` (nodes with attributes only) |
| `hir-def/src/attrs/docs.rs` | generics | two callers |
| `hir-def/src/item_tree/lower.rs` span callbacks | `&mut (impl FnMut + ?Sized)`, as `ModPath::from_src` | per path segment |
| `hir-def/src/item_tree/lower.rs` `lower_visibility*` | `&impl HasVisibility` | 4-line bodies |
| item-tree and hir-expand `&dyn ast::HasAttrs` | kept, justified | once per item; a generic copies the attribute lowering per node type; `ast::AnyHasAttrs` costs a refcount, no cheaper |

Test-only: `hir-expand/src/builtin/derive_macro/reference.rs`, `mbe/src/differential.rs`,
`hir-def/src/macro_expansion_tests/` (declared `#[cfg(test)]` in the parent module).

## The other crates

Children, one per crate group, with their own notes in their reports:
`/tmp/blaze-lanes/v2-dedyn-{graph,cas,codegen,diag}.md`.

## Speed of light

An indirect call through a vtable costs a dependent load of the vtable slot and an indirect
branch: about 1-3 ns when predicted, 15-20 ns mispredicted, and it blocks inlining, which is
the larger cost for a closure that is a field read (`span_map.span_for_range(range).ctx`) or a
push. The bound for each site is zero indirect calls on the hot path; every replacement above
reaches it (static dispatch, inlinable), and each kept `dyn` sits at most once per item,
never per token, segment or expression.

## What the lint misses

`tools/v2/hot_lint.py` (requests in `v2/requests/dedyn.md`): its `crates` filter crashes
(`list[str]`); it never lints `hir-ty-codegen` (sources at the crate root) or
`blaze-diagnostics`; `dyn\s+[A-Z]` misses path-qualified traits (`&dyn ast::HasAttrs`, about 25
lines); and it flags modules declared `#[cfg(test)]` in their parent.

## Outcome (2026-10-29 22:30)

Every site is applied in the tree, by dedyn and its children under common.md's dedyn edit
rule. Per site: `/tmp/blaze-lanes/v2-dedyn*.md`. hot_lint's `dyn` findings across the crates
went from 444 to 13, all Rust text inside string literals. The `dyn` left in code (297 lines)
each carries a `// dyn:` reason: crate-cycle edges called once per crate, build or request
(`ImageStore`, `ProcMacroExpander`, `BodyChecks`, `GoalFacts`, blaze-api's `Builder`), panic
payloads, `dyn Error`, the sandbox's `Confine`, CLI writers, cranelift's `TargetIsa`, and v1
codegen's `SymbolResolver`. That last one goes when v1's rustc-symbol linking does
(docs/no-rustc.md).
