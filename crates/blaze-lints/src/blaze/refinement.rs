//! `refinement` (`deny`, unwaivable; it fires only where code states an invariant or a
//! contract, docs/v2/plan/design/refined-values.md): a value blaze cannot prove keeps what its
//! type's `#[blaze::invariant(..)]` says where it is built, changed, passed or given back, a
//! call whose callee's `requires` is not shown, a return whose `ensures` is not, and an
//! invariant blaze cannot check (not in the fragment it decides, on an enum, or on a struct
//! with a field that is not private).
//!
//! What it reads: `hir_ty_pure::refined`, which reads the clauses and asks the prover of
//! `hir_ty_pure::reach` of each body. A crate that neither has `#![feature(blaze_verify)]` nor
//! sees a struct with an invariant costs one query and is not walked (`refined::in_play`).
//! No fix: each refusal is a decision about the value.

use hir_def::{AdtId, DefWithBodyId};
use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::{Lint, facts};
use crate::check::Cx;

const LINT: Lint = Lint::Refinement;

/// The sites of the body being walked that do not show a clause.
pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let db = cx.db();
    if !hir_ty_pure::refined::in_play(db, cx.krate().base()) {
        return;
    }
    let Some(analysis) = cx.analysis() else { return };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return };
    let sites = hir_ty_pure::refined::of_body(db, def);
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    for (at, refusal) in sites.found.iter() {
        let at = at.unwrap_or_else(|| analysis.body().root_expr());
        let Some(range) = analysis.expr_range(at) else { continue };
        // clone: the finding's own copies
        facts::push(&mut found, LINT, range, refusal.message.clone(), refusal.note.clone());
    }
    facts::report(cx, found);
}

/// An invariant on `adt` that blaze refuses: not checkable, on an enum or a union, or on a
/// struct with a field that is not private.
pub(super) fn check_adt<'t>(cx: &mut Cx<'_, 't>, adt: ast::Adt<'t>) {
    if !cx.on_blaze(LINT) || cx.expansion().is_some() || !cx.is_written(adt.syntax()) {
        return;
    }
    let Some(def) = cx.adt_def(adt) else { return };
    let db = cx.db();
    let Some(invariant) = hir_ty_pure::refined::invariant_of(db, AdtId::from(def)) else { return };
    let Some(refused) = &invariant.refused else { return };
    let node = adt.name().map_or(adt.syntax(), |it| it.syntax());
    let Some(range) = cx.range(node) else { return };
    let span = cx.span(range);
    let message = refused.message.clone(); // clone: the diagnostic's own copy
    let note = refused.note.clone(); // clone: as above
    let diagnostic = cx.blaze(LINT, adt.syntax(), span, message);
    if let Some(note) = note {
        diagnostic.help(note);
    }
    diagnostic.help(format!("for further information visit {}", LINT.docs_url())); // alloc: a help line
}

/// An attribute that names `blaze::refinement` below `deny` on `node`: a finding at the name,
/// since no level waives it ([`super::UNWAIVABLE`]).
pub(super) fn check_waiver<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if cx.expansion().is_some() {
        return;
    }
    for (range, level) in cx.waivers(node, LINT) {
        let span = cx.span(range);
        // alloc: a message
        let message = format!(
            "`#[{level}(blaze::refinement)]`: this lint can't be waived: a value of a type with an invariant, or a call of a function with a contract, compiles only where blaze proves what it needs"
        );
        cx.blaze(LINT, node, span, message)
            .help("take the attribute off; where a site can't be proved, add the check that shows it, or take the invariant or the clause off")
            .help(format!("for further information visit {}", LINT.docs_url())); // alloc: a help line
    }
}
