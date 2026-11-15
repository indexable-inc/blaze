//! `half_done`: a function that writes through a `&mut` parameter (`&mut self` among them),
//! then can leave by a `?` or a `return`, then writes the same value again. On that exit the
//! caller gets the value half changed: `self.len += 1; let slot = self.alloc()?; self.slots
//! .push(slot);` returns an error with `len` counting a slot that is not there.
//!
//! Read from the MIR of the function's own body: a write is an assignment through the
//! parameter (`(*_1).len = ..`) or a `&mut` borrow of a place behind it (`&mut (*_1).slots`,
//! what a `&mut self` method call of a field takes); an exit is a `return` expression of the
//! body, which a `?` lowers to. A finding is a first write, an exit the first write reaches
//! and that is written after it, and a later write the first reaches that is written after the
//! exit, all through one parameter, in a function that returns `Result` or `Option`.
//!
//! Left alone: a function that takes its value by value (a builder consumed and handed back
//! whole on success); closures and `async` blocks; test code; a function whose every write
//! comes after its last exit, or before its first.
//!
//! The right form is compute-then-commit: do everything that can fail into locals, then write
//! the value in one step after the last `?`; or change a copy and swap it in at the end. The
//! type that prevents it is a value passed by value and returned (`fn f(self) -> Result<Self,
//! E>`), so an error drops the half-changed one. A write before an exit is often meant (a
//! counter of attempts, a cache filled before a lookup that may fail), so the lint is a warning
//! in blaze's workspace, held by a ratchet.

use hir_def::{DefWithBodyId, hir::Expr};
use hir_ty::{
    InferBodyId,
    db::HirDatabase as _,
    mir::{
        BasicBlockId, BorrowKind, LocalId, MirBody, MirSpan, PlaceElem, ProjectionElem, Rvalue,
        StatementKind,
    },
};
use syntax::TextRange;

use super::{
    super::{Lint, facts, std_mistakes::report},
    skipped_pair::short,
};
use crate::{check::Cx, lints::guards};

const LINT: Lint = Lint::HalfDone;

/// A write through a parameter or an exit, where it is in the MIR and in the file.
#[derive(Clone, Copy)]
struct At {
    block: BasicBlockId,
    range: TextRange,
}

pub(super) fn check_body(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) || cx.is_in_test(cx.root()) {
        return;
    }
    // alloc: the findings of one body, reported once its analysis is let go
    let mut found: Vec<(TextRange, String)> = Vec::new();
    {
        let Some(analysis) = cx.analysis() else { return };
        let hir::DefWithBody::Function(f) = analysis.def() else { return };
        let db = cx.db();
        let famous = cx.famous();
        let ret = f.ret_type(db).as_adt();
        let fallible = [famous.core_option_Option(), famous.core_result_Result()]
            .into_iter()
            .flatten()
            .any(|it| ret == Some(hir::Adt::Enum(it)));
        if !fallible {
            return;
        }
        let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return };
        let Ok(mir) = db.runtime_mir_body(InferBodyId::DefWithBodyId(def)) else { return };
        let exits = exits(analysis, mir);
        if exits.is_empty() {
            return;
        }
        for &param in &mir.param_locals {
            if !analysis.ty(mir.locals[param].ty.as_ref()).is_mutable_reference() {
                continue;
            }
            let writes = writes(analysis, mir, param);
            let Some(finding) = half_done(mir, &writes, &exits) else { continue };
            let (first, exit, later) = finding;
            let name = guards::say_local(cx, analysis, mir, param);
            found.push((
                exit.range,
                format!(
                    "this exit leaves {name} half changed: `{}` wrote it before, `{}` writes it after",
                    short(cx.snippet(first.range)),
                    short(cx.snippet(later.range)),
                ),
            ));
        }
    }
    let root = cx.root();
    for (range, message) in found {
        report(
            cx,
            LINT,
            root,
            range,
            message,
            "the caller gets the error and a value with some of the change made: compute what can fail \
             into locals first and write the value after the last exit, or change a copy and swap it in",
            None,
        );
    }
}

/// The `return`s of `mir` (a `?`'s among them), each at the first block that has its span.
fn exits(analysis: &hir::BodyAnalysis<'_>, mir: &MirBody<'_>) -> Vec<At> {
    let body = analysis.body();
    // alloc: the exits of one body
    let mut out: Vec<At> = Vec::new();
    for (id, block) in mir.basic_blocks.iter() {
        if block.is_cleanup {
            continue;
        }
        let spans = block.statements.iter().map(|it| it.span).chain(block.terminator.as_ref().map(|it| it.span));
        for span in spans {
            let MirSpan::ExprId(expr) = span else { continue };
            if !matches!(body.expr(expr), Expr::Return { .. }) {
                continue;
            }
            let Some(range) = analysis.expr_range(expr) else { continue };
            if !out.iter().any(|it| it.range == range) {
                out.push(At { block: id, range });
            }
        }
    }
    out
}

/// The writes through `param` in `mir`: an assignment of a place behind it, or a `&mut` borrow
/// of one.
fn writes(analysis: &hir::BodyAnalysis<'_>, mir: &MirBody<'_>, param: LocalId) -> Vec<At> {
    let behind = |local: LocalId, projection: &[PlaceElem]| {
        local == param && matches!(projection.first(), Some(ProjectionElem::Deref))
    };
    // alloc: the writes of one parameter
    let mut out = Vec::new();
    for (id, block) in mir.basic_blocks.iter() {
        if block.is_cleanup {
            continue;
        }
        for statement in &block.statements {
            let StatementKind::Assign(place, rvalue) = &statement.kind else { continue };
            let written = behind(place.local, place.projection.as_slice())
                || matches!(rvalue, Rvalue::Ref(BorrowKind::Mut { .. }, from) if behind(from.local, from.projection.as_slice()));
            if !written {
                continue;
            }
            if let Some(range) = facts::mir_range(analysis, statement.span) {
                out.push(At { block: id, range });
            }
        }
    }
    out.sort_by_key(|it| (it.range.start(), it.range.end()));
    out
}

/// A first write, an exit and a later write: the exit and the later write both reached from
/// the first, written in that order.
fn half_done(mir: &MirBody<'_>, writes: &[At], exits: &[At]) -> Option<(At, At, At)> {
    for first in writes {
        let reached = reached_from(mir, first.block);
        let Some(exit) = exits
            .iter()
            .filter(|it| it.range.start() >= first.range.end() && reached[guards::block_index(it.block)])
            .min_by_key(|it| it.range.start())
        else {
            continue;
        };
        let later = writes.iter().find(|it| {
            it.range.start() >= exit.range.end()
                && !exit.range.contains_range(it.range)
                && reached[guards::block_index(it.block)]
        });
        if let Some(later) = later {
            return Some((*first, *exit, *later));
        }
    }
    None
}

/// Which blocks a path from `start` (itself included) reaches, not through a cleanup block.
fn reached_from(mir: &MirBody<'_>, start: BasicBlockId) -> Vec<bool> {
    let mut seen = vec![false; mir.basic_blocks.len()]; // alloc: a flag per block of one body
    let mut stack = vec![start]; // alloc: the walk's frontier
    seen[guards::block_index(start)] = true;
    while let Some(at) = stack.pop() {
        let Some(terminator) = &mir.basic_blocks[at].terminator else { continue };
        for next in terminator.kind.successors() {
            let index = guards::block_index(next);
            if !seen[index] && !mir.basic_blocks[next].is_cleanup {
                seen[index] = true;
                stack.push(next);
            }
        }
    }
    seen
}
