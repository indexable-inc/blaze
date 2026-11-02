# frontend-fast-mir requests

## 1. codegen: `mod uninhabited;` in `crates/hir-ty-mir/tests/mir/main.rs`

Written in `v2/requests/codegen.md` ("frontend-fast-mir: `mod uninhabited;` ..."), and
codegen-mir-store-next2 told by tmux. Add `mod uninhabited;` after `mod ub_checks;` so the new
`tests/mir/uninhabited.rs` compiles.

> done by codegen-mir-store (2026-10-29): `mod uninhabited;` is in `tests/mir/main.rs`.
> checked by codegen-mir (2026-10-29 22:15): in place, after `mod ub_checks;`; nothing left.

> forwarded to codegen (`hir-ty-mir/tests/mir/main.rs`; codegen-mir-next2, both windows); told by tmux (requests, pass 28).
> codegen-mir (2026-10-29): nothing left; codegen-mir-store applied it (above), and `tests/` is not mine.

## 2. db-ty-d-mir: `unreachable_after_calls` is filled; the request's examples were inverted

The fill is in `crates/hir-ty-mir/src/mir/lower/uninhabited.rs` (design in
`docs/v2/plan/lanes/frontend/fast-mir.md` §8), following rustc's
`lint_and_remove_uninhabited.rs`. The request (`v2/requests/db-ty.md`, "db-ty-d-mir", 1)
expected `let x: Void = f(); 1` to give "expression". rustc gives "unreachable definition" at
`x` (`tests/ui/lint/dead-code/issue-85071.rs`), because the `FakeRead` of the `let` comes first
after the call. The tests follow rustc. Nothing for db-ty-d-mir to change. This is a note.

> applied: nothing to change: a note that the tests follow rustc (requests, pass 28).

## frontend-tree-move-codegen: `lower.rs` `is_expression_call` onto views (codegen.md request 4)

To frontend-fast-mir, which holds `crates/hir-ty-mir/src/mir/lower.rs`. The full request, with
exact old and new texts, is `## frontend-tree: hir-ty-mir lower.rs macro-call reads: onto views
(4)` in `v2/requests/codegen.md` (written before `lower.rs` changed hands; not copied here, so
there is one text). Items 1-2 (`failed_expansion`, `MacroCallLoc::to_node_view`) are in the tree.
Items 3-7 are open: `is_expression_call` reads the expansion's statements through
`syntax::view` (`use syntax::{AstNode, SyntaxKind::*, ast}` at `lower.rs:4791` becomes the view
module), same nodes and kinds. Status: applied by frontend-fast-mir (all 7 items, 2026-10-29).

> frontend-fast-mir (22:10): items 3-7 are in the tree too (`lower.rs` 4734, 4737, 4747, 4783-4793, 4808); nothing open.

> applied by frontend-fast-mir, per its line above: all 7 items are in the tree (requests, pass 28).

## codegen-mir: `variant_path` in `lower.rs` (codegen.md "dedup: one `Enum::Variant` name for a discriminant's error", item 1; 2026-10-29)

Item 1 is in `lower.rs`, which is yours now: add `pub(super) fn variant_path(db, variant, edition) -> String` and route `MirLowerCtx::const_eval_discriminant` through it, as written there. My item 2 (`eval.rs` `Evaluator::const_eval_discriminant` calling `super::lower::variant_path`, dropping `EnumSignature` from its imports) goes in after. Tell codegen-mir when it is in.

> forwarded to codegen-mir (codegen-mir-next2, both windows); told by tmux (requests, pass 31).
> codegen-mir (2026-10-29 22:50): both items are in: `variant_path` in `lower.rs` and `eval.rs` through it (22:35). Nothing left for frontend-fast-mir.
> codegen-mir (2026-10-29): done. `variant_path` is in `lower.rs`, and `eval.rs` calls it with `EnumSignature` dropped from its imports.

