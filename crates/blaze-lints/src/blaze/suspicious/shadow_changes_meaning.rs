//! `shadow_changes_meaning`: a `let` that reuses the name of a binding still in scope for a
//! value not made from it. `let len = header.len(); .. let len = body.len();` leaves one name for
//! two things in one function: a line below reads the second where the author (or the next
//! edit) meant the first, and the type checker cannot see it when both are the same type. A
//! shadow that refines the old value (`let x = x.trim();`, `let path = path?;`, `let s =
//! &s[1..];`) reads it, and is left alone: it is the same thing, narrowed. Clippy's
//! `shadow_unrelated` says this, but `blaze check` does not run it; this is its blaze form, on
//! the HIR with macro expansions (a read inside `format!` counts).
//!
//! The shadowed binding is the one the new `let`'s initializer would name, found by the
//! resolver at that scope (the scopes of rustc's name resolution, not text); the initializer
//! reads it when a path anywhere in it, closures included, resolves to it.
//!
//! Not checked: names starting with `_`, a binding a macro made (its hygiene is not the
//! file's), a `match` arm's or a closure's binding (a new scope, shown by its own braces), and
//! code in tests. No fix: the right name for either value is the author's; the right form is a
//! name for each (`header_len`, `body_len`), or a block that ends the first one's scope.

use std::alloc::Global;

use hir_def::{
    DefWithBodyId,
    expr_store::{Body, HygieneId, path::Path, scope::ExprScopes},
    hir::{BindingId, Expr, ExprId, Pat, Statement},
    resolver::{ValueNs, resolver_for_scope},
};
use hir_expand::mod_path::{ModPath, PathKind};

use super::super::{Finding, Lint, facts, hash_order_output::local_of};
use crate::check::Cx;

const LINT: Lint = Lint::ShadowChangesMeaning;

/// How many expressions deep an initializer is searched for a read of the shadowed binding.
const DEPTH: u32 = 256;

pub(super) fn check_body(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) || !cx.in_file() || cx.is_in_test(cx.root()) {
        return;
    }
    let Some(analysis) = cx.analysis() else { return };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return };
    let db = cx.db();
    let body = analysis.body();
    let scopes = ExprScopes::of(db, def);
    let mut found: Vec<Finding> = Vec::new();
    for (_, expr) in body.exprs() {
        let Expr::Block { statements, .. } = expr else { continue };
        for statement in statements.iter() {
            let Statement::Let { pat, initializer: Some(init), .. } = statement else { continue };
            let Pat::Bind { id: new, .. } = body.pat(pat) else { continue };
            let binding = &body[new];
            if binding.name.as_str().starts_with('_') || binding.hygiene != HygieneId::ROOT {
                continue;
            }
            // The binding the initializer's scope names by the new one's name: the shadowed.
            let path = ModPath::from_segments(PathKind::Plain, std::iter::once(binding.name.clone()));
            let Some(scope) = scopes.scope_for(init) else { continue };
            let resolver = resolver_for_scope(db, def, Some(scope));
            let Some(ValueNs::LocalBinding(old)) =
                resolver.resolve_path_in_value_ns_fully(db, Path::BarePath(&path), HygieneId::ROOT)
            else {
                continue;
            };
            if old == new || body[old].hygiene != HygieneId::ROOT {
                continue;
            }
            if reads(db, def, body, init, old) {
                continue;
            }
            let Some(range) = analysis.expr_range(init) else { continue };
            let name = binding.name.as_str();
            facts::push(
                &mut found,
                LINT,
                range,
                format!(
                    "`let {name}` shadows a `{name}` still in scope with a value not made from it: \
                     below here `{name}` is another thing under the same name"
                ),
                Some(format!(
                    "give each its own name (`{name}` for one, a name for what the other is), or end \
                     the first one's scope with a block before this `let`"
                )),
            );
        }
    }
    facts::report(cx, found);
}

/// Whether the expression `root` of `def`'s body reads `binding`: a path in it (closures
/// included) resolves to it.
fn reads(db: &base_db::Db, def: DefWithBodyId, body: &Body, root: ExprId, binding: BindingId) -> bool {
    let mut stack: Vec<(ExprId, u32), Global> = Vec::new_in(Global);
    stack.push((root, 0));
    while let Some((expr, depth)) = stack.pop() {
        if local_of(db, def, body, expr) == Some(binding) {
            return true;
        }
        if depth >= DEPTH {
            // Deeper than looked: a read cannot be ruled out, so the shadow is not reported.
            return true;
        }
        body.walk_child_exprs(expr, |child| stack.push((child, depth + 1)));
    }
    false
}
