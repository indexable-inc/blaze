//! `todo` (`warn`; the merge gate and a build for a release deny it,
//! docs/v2/plan/design/proved-unreachable.md "`blaze::todo`: typed holes"): a `todo!()` in
//! code that is not a test. A hole, as in Agda and Haskell: the finding names the function,
//! the type the expression must have there (what inference coerces the `!` to) and the
//! bindings in scope that have that type.
//!
//! The macro is found by what it resolves to (std's `todo`), never by the text. A `todo!()`
//! is never discharged by the prover: it is unfinished, not dead.

use hir::HirDisplay as _;
use hir_def::{
    DefWithBodyId,
    expr_store::scope::ExprScopes,
    hir::{Expr, ExprId},
};
use hir_ty::Adjust;
use syntax::view::{self as ast, AstNode as _};

use super::{Finding, Lint, facts};
use crate::check::Cx;

const LINT: Lint = Lint::Todo;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find<'t>(cx: &Cx<'_, 't>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let db = cx.db();
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let name = match def {
        // alloc: a message's part
        DefWithBodyId::FunctionId(func) => format!(
            "`{}`",
            hir_def::signatures::FunctionSignature::of(db, func).name.symbol().as_str()
        ),
        _ => "this body".to_owned(), // alloc: a message's part
    };
    let target = cx.krate().to_display_target(db);
    for call in cx.root().descendants().filter_map(ast::MacroCall::cast) {
        if cx.std_macro(call).is_none_or(|it| it.as_str() != "todo") {
            continue;
        }
        if cx.is_in_test(call.syntax()) {
            continue;
        }
        let Some(range) = cx.range(call.syntax()) else { continue };
        // The hole: the outermost expression the call wrote that never returns, and the type
        // inference coerces it to.
        let infer = analysis.infer();
        let hole = analysis
            .body()
            .exprs()
            .filter(|&(id, _)| analysis.expr_range(id) == Some(range))
            .filter_map(|(id, _)| {
                let adjustments = infer.expr_adjustment(id)?;
                let never = adjustments.iter().rev().find(|it| it.kind == Adjust::NeverToAny)?;
                Some((id, never.target.as_ref()))
            })
            .last();
        let needs = match hole {
            Some((id, ty)) => {
                let wanted = analysis.ty(ty).display(db, target).to_string();
                let fits = in_scope(db, def, analysis, id, ty);
                match fits.is_empty() {
                    true => format!("needs `{wanted}`"), // alloc: a message's part
                    // alloc: a message's part
                    false => format!("needs `{wanted}`; in scope: {}", fits.join(", ")),
                }
            }
            // A statement, or a place nothing constrains: any type does.
            None => "its value is not used".to_owned(), // alloc: a message's part
        };
        let message = format!("`todo!()` in {name}: {needs}"); // alloc: a message
        let note = "a `todo!()` is a hole to fill: it warns in every local build and is an error in the merge gate and in a build for a release; test code may keep one";
        facts::push(&mut found, LINT, range, message, Some(note.to_owned())); // alloc: a finding's note
    }
    found
}

/// The bindings in scope at `hole` whose type is `wanted`, innermost first, as source names
/// them: "`cached`".
fn in_scope<'db>(
    db: &'db base_db::Db,
    def: DefWithBodyId,
    analysis: &hir::BodyAnalysis<'db>,
    hole: ExprId,
    wanted: hir_ty::next_solver::Ty<'db>,
) -> Vec<String> {
    // alloc: the few bindings that fit, a message's part
    let mut fits: Vec<String> = Vec::new();
    let scopes = ExprScopes::of(db, def);
    let infer = analysis.infer();
    // The scope of the expression, or of the nearest one around it that has one.
    let scope = scopes.scope_for(hole).or_else(|| {
        analysis.body().exprs().filter(|(_, expr)| matches!(expr, Expr::Block { .. })).find_map(
            |(id, _)| {
                let mut inside = false;
                analysis.body().walk_child_exprs(id, |child| inside |= child == hole);
                inside.then(|| scopes.scope_for(id)).flatten()
            },
        )
    });
    for scope in scopes.scope_chain(scope) {
        for entry in scopes.entries(scope) {
            if infer.binding_ty(entry.binding()) != wanted {
                continue;
            }
            let said = format!("`{}`", entry.name().as_str()); // alloc: a message's part
            if !fits.contains(&said) && fits.len() < 4 {
                fits.push(said);
            }
        }
    }
    fits
}
