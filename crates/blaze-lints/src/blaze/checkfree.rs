//! `checkfree` (`deny` by default; it fires only where the code asks for it): a function
//! marked `#[blaze::checkfree]` whose optimized code still makes a bounds check, an overflow
//! check or a division-by-zero check, or calls one of the toolchain's panicking functions
//! (`panic_bounds_check`, `panic_const_*`, `panic_fmt`, `unwrap_failed`, `expect_failed`, an
//! `unwrap` or `Index::index` kept out of line). Goose's `checkfree` functions are the idea: a
//! hot loop whose author has proved every index in range says so, and the compiler holds them
//! to it. Each check is its own finding, at the source line it comes from (an inlined callee's
//! at the call), naming which check it is.
//!
//! What it reads: the function's MIR after blaze's optimizations at the release level, with
//! inlining (`hir_ty_mir::mir::checks`, which the stencil backend refuses the same checks from).
//! The MIR optimizer does not prove ranges, so a bounds check LLVM drops (a range loop's
//! `v[i]` after `assert!(n <= v.len())`, or with `v.len()` as the bound) is still there: the
//! ones `bounds_check_in_loop` counts as proven are left out, a prediction of LLVM's that the
//! release build's own check (on the optimized IR) confirms or refutes. An overflow check a
//! crate drops without `overflow-checks` (`+`, `-`, `*`, `<<`, `>>`, negation) is left out:
//! `blaze check` reads no profile, and release builds drop them; the build's check sees what
//! its profile keeps. A body whose MIR can't be built is a finding too, never a pass.
//!
//! Left alone: a panic the function writes itself, a call of `core::panicking` (or std's `rt`)
//! that a macro of its own body wrote (`assert!`, `panic!`, `unreachable!`), since an
//! `assert!(n <= v.len())` before a loop is how a checkfree function proves its indexes; an
//! `unwrap()` or `v[i]` it writes is never its own panic, so it stays a finding.
//!
//! The attribute changes diagnostics only: no codegen key reads `AttrFlags::CHECKFREE`, so a
//! function without it builds byte for byte as before.

use hir_def::{
    AttrDefId, DefWithBodyId,
    attrs::AttrFlags,
    hir::{Expr, ExprId},
};
use hir_ty::{InferBodyId, db::HirDatabase as _};
use hir_ty_mir::mir::{
    MirBody, MirOptLevel, MirOptions, MirSpan,
    checks::{self, CheckKind},
};

use super::{Finding, Lint, bounds_check_in_loop, exempt, facts, general_facts, loops};
use crate::check::Cx;

const LINT: Lint = Lint::Checkfree;

/// How many closure bodies of one function are read.
const BODIES: usize = 64;

/// The options the release build's MIR is optimized with: what a checkfree function is held
/// to.
pub(crate) const RELEASE: MirOptions = MirOptions {
    level: MirOptLevel::Release,
    ub_checks: false,
    lifetime_markers: true,
    inline: true,
    heap_to_stack: false,
    world: None,
};

/// Whether `def` is a function marked `#[blaze::checkfree]`.
pub(crate) fn is_checkfree(db: &base_db::Db, def: DefWithBodyId) -> bool {
    let DefWithBodyId::FunctionId(f) = def else { return false };
    AttrFlags::query(db, AttrDefId::FunctionId(f)).contains(AttrFlags::CHECKFREE)
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    if !is_checkfree(db, def) {
        return found;
    }
    let hir::DefWithBody::Function(f) = analysis.def() else { return found };
    let name = f.name(db);
    let name = name.as_str();
    let Some(whole) = cx.range(cx.root()) else { return found };
    let mir = match db.optimized_mir_body(InferBodyId::DefWithBodyId(def), RELEASE) {
        Ok(it) => it,
        Err(err) => {
            let message = format!(
                "`{name}` is `#[blaze::checkfree]`, but its MIR can't be built, so its checks can't be read: {err:?}"
            );
            facts::push(&mut found, LINT, whole, message, None);
            return found;
        }
    };
    // alloc: the body's MIR and its closures', a few
    let mut bodies: Vec<&MirBody<'_>> = vec![mir];
    let mut at = 0;
    while at < bodies.len() && bodies.len() < BODIES {
        for &closure in &bodies[at].closures {
            match db.optimized_mir_body_for_closure(closure, RELEASE) {
                Ok(it) => bodies.push(it),
                Err(err) => {
                    let message = format!(
                        "`{name}` is `#[blaze::checkfree]`, but the MIR of a closure in it can't be built, so its checks \
                         can't be read: {err:?}"
                    );
                    facts::push(&mut found, LINT, whole, message, None);
                }
            }
        }
        at += 1;
    }
    // The range loops' indexes LLVM drops the check of (`bounds_check_in_loop`'s proof).
    // alloc: one body's proven indexes, a few
    let mut proven: Vec<ExprId> = Vec::new();
    bounds_check_in_loop::scan(cx, analysis, |it| {
        if it.proven {
            proven.push(it.at);
        }
    });
    let body = analysis.body();
    for mir in bodies {
        let made = match checks::checks(db, mir) {
            Ok(it) => it,
            Err(error) => {
                let message = format!(
                    "`{name}` is `#[blaze::checkfree]`, but its checks can't be read: {error}"
                );
                facts::push(&mut found, LINT, whole, message, None);
                continue;
            }
        };
        for check in made {
            if check.kind.is_optional() {
                continue;
            }
            let expr = match check.span {
                MirSpan::ExprId(it) | MirSpan::ExprEnd(it) => Some(it),
                _ => None,
            };
            if expr.is_some_and(|it| is_proven(body, &proven, it, &check.kind)) {
                continue;
            }
            if expr.is_some_and(|it| is_explicit(db, analysis, it, &check.kind)) {
                continue;
            }
            let range = facts::mir_range(analysis, check.span).unwrap_or(whole);
            if exempt::exempt(cx.text(), range, LINT) {
                continue;
            }
            let written =
                expr.and_then(|it| analysis.expr_range(it)).map(|it| cx.snippet(it).trim());
            let at = match written {
                Some(it) if it.len() <= 60 && !it.contains('\n') => format!(" at `{it}`"),
                _ => String::new(), // alloc: a message's part
            };
            let from = if check.inlined { ", from a callee inlined there" } else { "" };
            let message = format!(
                "`{name}` is `#[blaze::checkfree]`, but {}{at}{from} stays in its optimized code",
                check.kind.say()
            );
            let note = match check.span {
                MirSpan::ExprId(_) | MirSpan::ExprEnd(_) => None,
                // alloc: a message's part
                _ => Some("no expression of the function's source reaches this check; it is placed at the function".to_owned()),
            };
            facts::push(&mut found, LINT, range, message, note);
        }
    }
    found
}

/// Whether the panic of `kind` at `expr` is one the function states itself: `expr` is a call a
/// macro of the function's own body wrote (`assert!`, `panic!`, `unreachable!`, `todo!`), of a
/// function of `core::panicking` or std's `rt` / `panicking` (its own call, or the call a
/// callee inlined there makes). An `assert!(n <= v.len())` before a loop is how a checkfree
/// function proves its indexes: the one check it keeps is the one it wrote.
fn is_explicit<'db>(
    db: &'db base_db::Db,
    analysis: &hir::BodyAnalysis<'db>,
    expr: ExprId,
    kind: &CheckKind,
) -> bool {
    if !matches!(kind, CheckKind::Panics(_)) || loops::written(analysis, expr) {
        return false;
    }
    if !matches!(analysis.body().expr(expr), Expr::Call { .. }) {
        return false;
    }
    general_facts::callee(db, analysis, expr)
        .is_some_and(|it| it.in_std_module(db, "panicking") || it.in_std_module(db, "rt"))
}

/// Whether the check of `kind` at `expr` is a bounds check of an index `bounds_check_in_loop`
/// counts as proven before its loop (the `v[i]` itself, or the `Index::index` call it is).
fn is_proven(
    body: &hir_def::expr_store::Body,
    proven: &[ExprId],
    expr: ExprId,
    kind: &CheckKind,
) -> bool {
    let bounds = match kind {
        CheckKind::BoundsCheck => true,
        CheckKind::Panics(it) => {
            it.ends_with("::index")
                || it.ends_with("::index_mut")
                || it.ends_with("panic_bounds_check")
        }
        _ => false,
    };
    bounds && matches!(body.expr(expr), Expr::Index { .. }) && proven.contains(&expr)
}
