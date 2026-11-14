//! `skipped_pair`: the first call of a pair (`begin`/`end`, `start`/`stop`, `open`/`close`,
//! `enter`/`exit`, `push`/`pop`, `acquire`/`release`, `lock`/`unlock`, or a function marked
//! `#[blaze::pairs_with(second)]`) whose second, on the same receiver, does not post-dominate it:
//! a `?`, a `return` or a `break` between the two leaves with the first done and the second not.
//! `self.scopes.push(s); let t = self.resolve(e)?; self.scopes.pop();` leaves a scope pushed on
//! every error, and the next walk resolves names in it.
//!
//! Read from the MIR of the body (its closures' and `async` blocks' too): the calls are the
//! `Call` terminators whose callee resolves to a function of that name; the receiver is the
//! place the call's first argument borrows or copies, traced through the reborrows of its
//! block; two calls are a pair when their callees are methods of the same type (or trait, or
//! free functions of one module) with the pair's names, on one place. The second post-dominates
//! the first when every path from the first's return, not through a cleanup block, reaches a
//! block that calls the second before the body's `Return`. A path that ends at a call that never
//! returns (a panic, an exit) is no skip: it ends the thread or the process.
//!
//! Left alone: a first whose second the body never calls (the pair is split across functions
//! on purpose, or the first hands back what ends it); a first that returns a value with drop
//! glue, a guard, which is the right form already; test code.
//!
//! `#[blaze::pairs_with(..)]` is read from the first function's own attributes, from its source
//! wherever in the crate it is written (a mark on a function of another crate is not read).
//!
//! The right form is a type: a guard whose `Drop` does the second (`let _scope =
//! self.scopes.enter(s);`), or a `#[blaze::linear]` value the second consumes, so no exit can
//! leave without it and the compiler, not a reader, checks every path.

use hir::view::HasSourceView as _;
use hir_def::{DefWithBodyId, hir::Expr};
use hir_ty::mir::{
    BasicBlockId, MirBody, MirSpan, OperandKind, ProjectionElem, Rvalue, StatementKind,
    StoredPlace, TerminatorKind,
};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasAttrs as _},
};

use super::super::{Lint, facts, std_mistakes::report};
use crate::{check::Cx, lints::guards};

const LINT: Lint = Lint::SkippedPair;

/// The pairs known by name: the first, then the second that undoes it.
const PAIRS: &[(&str, &str)] = &[
    ("begin", "end"),
    ("start", "stop"),
    ("open", "close"),
    ("enter", "exit"),
    ("push", "pop"),
    ("acquire", "release"),
    ("lock", "unlock"),
];

/// The attribute that names a function's second.
const MARK: &str = "blaze::pairs_with";

/// A call of a body's MIR.
struct Call {
    block: BasicBlockId,
    target: Option<BasicBlockId>,
    function: hir::Function,
    receiver: Option<StoredPlace>,
    /// Whether what it returns has drop glue: a guard that does the second itself.
    guard: bool,
    span: MirSpan,
}

/// Where a function lives, which both halves of a pair share.
#[derive(PartialEq, Eq)]
enum Home {
    Adt(hir::Adt),
    Trait(hir::Trait),
    Module(hir::Module),
}

pub(super) fn check_body(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) || cx.is_in_test(cx.root()) {
        return;
    }
    // alloc: the findings of one body, reported once its analysis is let go
    let mut found: Vec<(TextRange, String, &'static str)> = Vec::new();
    {
        let Some(analysis) = cx.analysis() else { return };
        let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return };
        let db = cx.db();
        for body in guards::bodies(db, analysis, def) {
            let mir = body.mir;
            let calls = calls_of(cx, analysis, mir);
            for first in &calls {
                let Some(receiver) = &first.receiver else { continue };
                if first.guard {
                    continue;
                }
                let Some(second) = partner(cx, first.function) else { continue };
                let first_home = home(cx, first.function);
                // alloc: the blocks that call the second, a few
                let seconds: Vec<BasicBlockId> = calls
                    .iter()
                    .filter(|it| {
                        it.block != first.block
                            && it.receiver.as_ref() == Some(receiver)
                            && it.function.name(db).as_str() == second
                            && home(cx, it.function) == first_home
                    })
                    .map(|it| it.block)
                    .collect();
                if seconds.is_empty() {
                    continue;
                }
                let Some(target) = first.target else { continue };
                let Some(exit) = skipping_exit(analysis, mir, target, &seconds) else { continue };
                let Some(range) = facts::mir_range(analysis, first.span) else { continue };
                let name = first.function.name(db).as_str().to_owned(); // alloc: a message's part
                let exit = match exit {
                    Some((range, word)) => format!("{word} `{}` leaves", short(cx.snippet(range))),
                    None => "the path to the end of the body goes".to_owned(), // alloc: a message's part
                };
                found.push((
                    range,
                    format!("this `{name}` is not undone by its `{second}` on every path: {exit} without it"),
                    "a pair whose second an early exit skips leaves the state of the first behind: the \
                     next caller sees a scope still pushed, a lock still held, a span still open",
                ));
            }
        }
    }
    let root = cx.root();
    found.sort_by_key(|(range, ..)| (range.start(), range.end()));
    found.dedup_by_key(|(range, ..)| *range);
    for (range, message, note) in found {
        report(cx, LINT, root, range, message, note, None);
    }
}

/// The calls of `mir` whose callee is a function, with the place each's first argument names.
fn calls_of(cx: &Cx<'_, '_>, analysis: &hir::BodyAnalysis<'_>, mir: &MirBody<'_>) -> Vec<Call> {
    let db = cx.db();
    // alloc: the calls of one body
    let mut out = Vec::new();
    for (id, block) in mir.basic_blocks.iter() {
        if block.is_cleanup {
            continue;
        }
        let Some(terminator) = &block.terminator else { continue };
        let TerminatorKind::Call { args, destination, target, .. } = &terminator.kind else {
            continue;
        };
        let MirSpan::ExprId(expr) = terminator.span else { continue };
        if !matches!(analysis.body().expr(expr), Expr::MethodCall { .. } | Expr::Call { .. }) {
            continue;
        }
        let Some((function, _)) = analysis.callee(expr) else { continue };
        let function = hir::Function::from(function);
        let receiver = args.first().and_then(|it| match &it.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => Some(borrowed(mir, id, place)),
            _ => None,
        });
        let returned = analysis.ty(mir.locals[destination.local].ty.as_ref());
        let guard = !returned.is_unit()
            && !returned.is_never()
            && matches!(returned.drop_glue(db), hir::DropGlue::HasDropGlue);
        out.push(Call { block: id, target: *target, function, receiver, guard, span: terminator.span });
    }
    out
}

/// The place `place`, an argument of the call ending `block`, stands for: the place it borrows
/// when the block made it by `&`, `&mut` or a reborrow of one, followed back a few steps.
fn borrowed(mir: &MirBody<'_>, block: BasicBlockId, place: &StoredPlace) -> StoredPlace {
    let mut at = place.clone();
    for _ in 0..4 {
        if !at.projection.is_empty() && !matches!(at.projection.as_slice(), [ProjectionElem::Deref]) {
            return at;
        }
        let made = mir.basic_blocks[block].statements.iter().rev().find_map(|it| match &it.kind {
            StatementKind::Assign(to, Rvalue::Ref(_, from)) if to.local == at.local && to.projection.is_empty() => {
                Some(from.clone())
            }
            StatementKind::Assign(to, Rvalue::Use(operand)) if to.local == at.local && to.projection.is_empty() => {
                match &operand.kind {
                    OperandKind::Copy(from) | OperandKind::Move(from) => Some(from.clone()),
                    _ => None,
                }
            }
            _ => None,
        });
        // A reborrow `&mut *x` names what `x` points at: the walk goes on to where `x` came from.
        match made {
            Some(from) => at = from,
            None => return at,
        }
    }
    at
}

/// The second of `f`'s pair, by its name or by its mark.
fn partner(cx: &Cx<'_, '_>, f: hir::Function) -> Option<String> {
    let db = cx.db();
    let name = f.name(db);
    if let Some((_, second)) = PAIRS.iter().find(|(first, _)| *first == name.as_str()) {
        return Some((*second).to_owned()); // alloc: the partner's name
    }
    if f.module(db).krate(db) != cx.krate() {
        return None;
    }
    mark_of(cx, f)
}

/// The second `f` names in its own `#[blaze::pairs_with(second)]`, read from `f`'s own source
/// (wherever in the crate it is written): a body's walk sees only its own item, so the mark is
/// looked up on the callee, not found by scanning the linted file.
fn mark_of(cx: &Cx<'_, '_>, f: hir::Function) -> Option<String> {
    let db = cx.db();
    let cfg = cx.krate().cfg(db);
    let item = f.source_view(db)?.value;
    for (meta, _) in hir_expand::attrs::cfg_attrs_view(item.attrs(), cfg) {
        let ast::Meta::TokenTreeMeta(meta) = meta else { continue };
        if !meta.path().is_some_and(|it| it.syntax().text() == MARK) {
            continue;
        }
        let Some(tt) = meta.token_tree() else { continue };
        let text = tt.syntax().text();
        let second = text.strip_prefix('(').and_then(|it| it.strip_suffix(')')).unwrap_or(text).trim();
        return Some(second.to_owned()); // alloc: the second's name
    }
    None
}

fn home(cx: &Cx<'_, '_>, f: hir::Function) -> Home {
    let db = cx.db();
    match hir::AsAssocItem::as_assoc_item(f, db).map(|it| it.container(db)) {
        Some(hir::AssocItemContainer::Impl(imp)) => match imp.trait_(db) {
            Some(it) => Home::Trait(it),
            None => imp.self_ty(db).as_adt().map_or(Home::Module(f.module(db)), Home::Adt),
        },
        Some(hir::AssocItemContainer::Trait(it)) => Home::Trait(it),
        None => Home::Module(f.module(db)),
    }
}

/// The exit on a path from `start` to the body's `Return` that meets none of `seconds`: the
/// `?`, `return` or `break` on it, by its range and what it is (`Some(None)` when the path is
/// the body's own end), or `None` when every path meets a second.
fn skipping_exit(
    analysis: &hir::BodyAnalysis<'_>,
    mir: &MirBody<'_>,
    start: BasicBlockId,
    seconds: &[BasicBlockId],
) -> Option<Option<(TextRange, &'static str)>> {
    let blocks = mir.basic_blocks.len();
    // alloc: the walk's parent of each block of one body
    let mut parent: Vec<Option<BasicBlockId>> = vec![None; blocks];
    let mut seen = vec![false; blocks]; // alloc: a flag per block of one body
    let mut queue = std::collections::VecDeque::new(); // alloc: the walk's frontier
    seen[guards::block_index(start)] = true;
    queue.push_back(start);
    while let Some(at) = queue.pop_front() {
        if seconds.contains(&at) {
            continue;
        }
        let block = &mir.basic_blocks[at];
        let Some(terminator) = &block.terminator else { continue };
        if matches!(terminator.kind, TerminatorKind::Return) {
            return Some(exit_on(analysis, mir, at, &parent));
        }
        for next in terminator.kind.successors() {
            let index = guards::block_index(next);
            if seen[index] || mir.basic_blocks[next].is_cleanup {
                continue;
            }
            seen[index] = true;
            parent[index] = Some(at);
            queue.push_back(next);
        }
    }
    None
}

/// The last `?`, `return` or `break` on the path the walk took to `end`.
fn exit_on(
    analysis: &hir::BodyAnalysis<'_>,
    mir: &MirBody<'_>,
    end: BasicBlockId,
    parent: &[Option<BasicBlockId>],
) -> Option<(TextRange, &'static str)> {
    let body = analysis.body();
    let mut at = Some(end);
    while let Some(block) = at {
        let data = &mir.basic_blocks[block];
        let spans = data.statements.iter().map(|it| it.span).chain(data.terminator.as_ref().map(|it| it.span));
        for span in spans {
            let MirSpan::ExprId(expr) = span else { continue };
            let word = match body.expr(expr) {
                Expr::Return { .. } | Expr::Become { .. } => "the exit",
                Expr::Break { .. } => "the `break`",
                _ => continue,
            };
            if let Some(range) = analysis.expr_range(expr) {
                return Some((range, word));
            }
        }
        at = parent[guards::block_index(block)];
    }
    None
}

/// The first line of `text`, cut short for a message: how the lints of a body quote code.
pub(super) fn short(text: &str) -> &str {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(60) {
        Some((at, _)) => &line[..at],
        None => line,
    }
}
