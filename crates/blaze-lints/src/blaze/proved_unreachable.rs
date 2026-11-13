//! `unreachable` (`deny` by default; it fires only where the code wrote
//! `blaze::unreachable!()`, docs/v2/plan/design/proved-unreachable.md): a
//! `blaze::unreachable!()` that blaze cannot prove no path reaches. One finding per site, at
//! the site, naming the function and what could not be shown.
//!
//! What it reads: the body's macro calls, to find the sites by what they resolve to (the
//! builtin `core::blaze::unreachable`, never the text), and `hir_ty_pure::reach`'s proof of
//! the body's runtime MIR: a site is proved when every block lowered for it is one no feasible
//! path reaches, or lowering made none (code after an expression that never returns).
//! There is no fallback: a body that does not lower, or whose proof does not settle, is a
//! finding saying so.

use hir_def::{DefWithBodyId, hir::Expr};
use hir_expand::{HirFileId, MacroCallId, MacroDefKind, builtin::BuiltinFnLikeExpander};

use super::{Finding, Lint, facts};
use crate::check::Cx;

const LINT: Lint = Lint::Unreachable;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

/// Whether `call` is a call of `blaze::unreachable!()`.
fn ours(db: &base_db::Db, call: MacroCallId) -> bool {
    matches!(
        call.loc(db).def.kind,
        MacroDefKind::BuiltIn(_, BuiltinFnLikeExpander::BlazeUnreachable)
    )
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let db = cx.db();
    let body = analysis.body();
    if !body.macro_calls().iter().any(|&call| ours(db, call)) {
        return found;
    }
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    // The expansion is one call of core's panic: the site is that call.
    // alloc: the sites of one body, few
    let sites: Vec<_> = body
        .exprs()
        .filter(|(_, expr)| matches!(expr, Expr::Call { .. }))
        .filter(|&(id, _)| {
            analysis.source_map().store.expr_syntax(id).is_ok_and(
                |at| matches!(at.file_id, HirFileId::MacroFile(call) if ours(db, call)),
            )
        })
        .map(|(id, _)| id)
        .collect();
    let name = match def {
        DefWithBodyId::FunctionId(func) => hir_def::signatures::FunctionSignature::of(db, func)
            .name
            .symbol()
            .as_str()
            .to_owned(), // alloc: a message's part
        _ => "this body".to_owned(), // alloc: a message's part
    };
    let reach = hir_ty_pure::reach::of_body(db, def);
    // alloc: the sites not proved, few
    let unproved: Vec<_> = sites.iter().copied().filter(|&site| !reach.unreached(site)).collect();
    if unproved.is_empty() {
        return found;
    }
    // Why each is reached: asked only now, of the blocks of the sites that are refused.
    // alloc: those blocks, few
    let blocks: Vec<u32> = unproved.iter().flat_map(|&site| reach.blocks_at(site)).collect();
    let explained = hir_ty_pure::reach::explain(db, def, &blocks);
    for site in unproved {
        let Some(range) = analysis.expr_range(site) else { continue };
        let why = match reach.refused {
            // alloc: a message's part
            Some(why) => format!("`{name}` can't be checked: {why}"),
            None => reach
                .blocks_at(site)
                .find_map(|block| {
                    explained.explained.iter().find(|(at, _)| *at == block).map(|(_, why)| why)
                })
                .cloned() // clone: a message's part
                // alloc: a message's part
                .unwrap_or_else(|| "nothing before it rules out the path to it".to_owned()),
        };
        // alloc: a message
        let message = format!(
            "`{name}`: can't prove this `blaze::unreachable!()` is never reached: {why}"
        );
        let note = "`blaze::unreachable!()` compiles only where blaze proves no path leads to it, from the checks that dominate it (a `match`, `if let`, `is_some`, a comparison with a length or with zero); add the check, or use `unreachable!()` for a promise blaze does not check";
        facts::push(&mut found, LINT, range, message, Some(note.to_owned())); // alloc: a finding's note
    }
    found
}
