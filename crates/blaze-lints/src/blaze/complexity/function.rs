//! The measures of one function: its weight and nesting ([`super::weight`]), its lines and MIR
//! statements, its parameters, its paths ([`super::paths`]), its live mutable state, its
//! exits, the kinds of effect its calls bring and its fan-out. Each is measured only when its
//! lint is on; the findings are collected, then reported ([`super::report`]).

use std::alloc::Global;

use hir::HirDisplay as _;
use hir_def::{
    DefWithBodyId, FunctionId, HasModule as _,
    hir::{BindingAnnotation, Expr, ExprId},
};
use hir_ty::{InferBodyId, db::HirDatabase as _};
use hir_ty_mir::mir::{MirBody, TerminatorKind};
use hir_ty_pure::effects::Rule;
use syntax::{
    SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, HasName as _},
};

use super::{
    Item, Measure, Over, Pending, code_lines, fingerprint_of, judge, line_from, paths, percent,
    report, slice, weight,
};
use crate::{
    blaze::{
        facts,
        sans_io_core::{fn_name, root_path},
    },
    check::Cx,
};

/// The crates whose types' drops only free memory: holding one is not state the reader has to
/// see finished at each exit.
const FREES_ONLY: [&str; 5] = ["core", "alloc", "smallvec", "thin_vec", "arrayvec"];

/// The kinds of effect `effect_breadth` counts, as a message names them, and the rules that
/// make each.
const KINDS: [(&str, &[Rule]); 6] = [
    ("IO", &[Rule::Io]),
    ("blocking", &[Rule::Block]),
    ("syncing to disk", &[Rule::Sync]),
    ("locking", &[Rule::Lock]),
    ("allocating", &[Rule::Alloc, Rule::AllocIn]),
    ("panicking", &[Rule::Panic]),
];

/// How many names a `live_state` message lists before it says how many more.
const MOST_NAMED: usize = 8;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, f: ast::Fn<'t>) {
    if cx.is_in_test(f.syntax()) {
        return;
    }
    let pending = measure(cx, f);
    report(cx, pending);
}

/// What one finding of the function shares: where it is, what names it, how lines are read.
struct Of<'a> {
    at: TextRange,
    item: Item,
    shown: String,
    text: &'a str,
    /// Where the function starts, for lines counted from it.
    start: syntax::TextSize,
}

impl Of<'_> {
    fn lines(&self, range: TextRange) -> (usize, usize) {
        (line_from(self.text, self.start, range.start()), line_from(self.text, self.start, range.end()))
    }

    fn pending(&self, over: Over, said: String) -> Pending {
        Pending { at: self.at, item: self.item.clone(), over, said, way: None, help: None }
    }
}

fn measure<'t>(cx: &Cx<'_, 't>, f: ast::Fn<'t>) -> Vec<Pending, Global> {
    let mut out = Vec::new_in(Global);
    let (Some(block), Some(name)) = (f.body(), f.name()) else { return out };
    let Some(analysis) = cx.analysis() else { return out };
    let Ok(DefWithBodyId::FunctionId(func)) = DefWithBodyId::try_from(analysis.def()) else {
        return out;
    };
    let Some(at) = cx.range(name.syntax()) else { return out };
    let db = cx.db();
    let whole = f.syntax().text_range();
    let start = f.fn_token().map_or(whole.start(), |it| it.text_range().start());
    let of = Of {
        at,
        item: Item {
            path: root_path(db, func),
            fingerprint: fingerprint_of(f.syntax(), Some(name.syntax().text_range())),
        },
        shown: fn_name(db, func),
        text: cx.text(),
        start,
    };
    let callees = callees(analysis);
    let on = |measure: Measure| cx.on_blaze(measure.lint());

    // Weight and nesting, on the syntax.
    let weighed = weight::weigh(block.syntax());
    if on(Measure::Weight) {
        let recursive = callees.iter().any(|&(it, _)| it == func);
        let total = weighed.total + u64::from(recursive);
        if let Some(over) = judge(cx, Measure::Weight, &of.item, total) {
            let mut parts = Vec::with_capacity_in(weighed.parts.len(), Global);
            parts.extend(weighed.parts.iter().map(|it| (it.range, it.weight)));
            let mut it = of.pending(over, format!("`{}` weighs {total} by cognitive complexity", of.shown));
            it.way = weight::way_down(total, over.bound, &parts).map(|at| {
                let part = weighed.parts[at];
                let (from, to) = of.lines(part.range);
                let text = format!(
                    "move the {} at lines {from}-{to} of the function into its own function: it carries {} of the {total} ({}%)",
                    part.kind,
                    part.weight,
                    percent(part.weight, total)
                );
                (part.range, text)
            });
            if recursive {
                it.help = Some("1 of it is for calling itself".to_owned());
            }
            out.push(it);
        }
    }
    if on(Measure::Nesting)
        && let Some(over) = judge(cx, Measure::Nesting, &of.item, u64::from(weighed.depth))
    {
        let depth = weighed.depth;
        let mut it = of.pending(over, format!("`{}` nests control flow {depth} deep", of.shown));
        it.way = weighed.deepest.map(|(range, kind)| {
            let (from, _) = of.lines(range);
            let text = format!(
                "the {kind} body at line {from} of the function is {depth} deep: return early from the conditions around it, or move it into its own function"
            );
            (range, text)
        });
        out.push(it);
    }

    // Lines, and the parts that carry them.
    if on(Measure::FunctionLines) {
        let lines = code_lines(slice(of.text, TextRange::new(start, whole.end())));
        if let Some(over) = judge(cx, Measure::FunctionLines, &of.item, lines) {
            let mut sized = Vec::with_capacity_in(weighed.parts.len(), Global);
            sized.extend(weighed.parts.iter().map(|it| (it.range, code_lines(slice(of.text, it.range)))));
            let mut it = of.pending(over, format!("`{}` is {lines} lines of code", of.shown));
            it.way = weight::way_down(lines, over.bound, &sized).map(|at| {
                let (range, part) = sized[at];
                let kind = weighed.parts[at].kind;
                let (from, to) = of.lines(range);
                let text = format!(
                    "move the {kind} at lines {from}-{to} of the function into its own function: {part} of the {lines} lines ({}%)",
                    percent(part, lines)
                );
                (range, text)
            });
            if it.way.is_none() {
                it.help = Some(
                    "it is straight-line code: split it where it moves from one step to the next, each step a function"
                        .to_owned(),
                );
            }
            out.push(it);
        }
    }

    // Parameters, `self` not counted.
    if on(Measure::Params)
        && let Some(list) = f.param_list()
    {
        let params = list.params().count() as u64;
        if let Some(over) = judge(cx, Measure::Params, &of.item, params) {
            let mut it = of.pending(over, format!("`{}` takes {params} parameters", of.shown));
            let help = "group the parameters that travel together into one struct, or make it a method of the type most of them describe";
            it.way = cx.range(list.syntax()).map(|range| (range, help.to_owned()));
            out.push(it);
        }
    }

    // The built MIR: statements and paths.
    let def = DefWithBodyId::FunctionId(func);
    if (on(Measure::FunctionStatements) || on(Measure::Paths))
        && let Ok(mir) = db.mir_body(InferBodyId::DefWithBodyId(def))
    {
        if on(Measure::FunctionStatements) {
            let statements = statements(mir);
            if let Some(over) = judge(cx, Measure::FunctionStatements, &of.item, statements) {
                let mut it =
                    of.pending(over, format!("`{}` lowers to {statements} MIR statements", of.shown));
                it.help = Some(
                    "what macros and generic calls expand to counts here: move a part that expands large into its own function"
                        .to_owned(),
                );
                out.push(it);
            }
        }
        if on(Measure::Paths) {
            let count = paths::count(&cfg_of(mir));
            if let Some(over) = judge(cx, Measure::Paths, &of.item, count) {
                let mut it = of.pending(
                    over,
                    format!("`{}` has {count} paths through it, loops taken once: as many tests to cover each", of.shown),
                );
                let branching = weighed
                    .parts
                    .iter()
                    .filter(|it| matches!(it.kind, "if" | "match" | "loop"))
                    .max_by(|a, b| a.weight.cmp(&b.weight).then(b.range.start().cmp(&a.range.start())));
                it.way = branching.map(|part| {
                    let (from, to) = of.lines(part.range);
                    let text = format!(
                        "each branch multiplies the paths after it: move the {} at lines {from}-{to} of the function, the most branching part, into its own function",
                        part.kind
                    );
                    (part.range, text)
                });
                out.push(it);
            }
        }
    }

    // The runtime MIR: mutable state live across a call.
    if on(Measure::Live)
        && let Some((live, call, names)) = live_state(cx, def)
        && let Some(over) = judge(cx, Measure::Live, &of.item, live)
    {
        let mut it = of.pending(
            over,
            format!("`{}` holds {live} mutable places live across one call ({names})", of.shown),
        );
        it.way = call.map(|range| {
            let text = "at this call: finish the ones done before it, or move each, with the code that changes it, into a function that returns what it computed";
            (range, text.to_owned())
        });
        out.push(it);
    }

    // Exits of a function that holds state its drop finishes.
    if on(Measure::Exits)
        && let Some((binding, ty)) = finished_by_drop(cx)
    {
        let (exits, first) = exits(cx, block.syntax());
        if let Some(over) = judge(cx, Measure::Exits, &of.item, exits) {
            let mut it = of.pending(
                over,
                format!("`{}` has {exits} early exits while it holds `{binding}`, a `{ty}` its drop finishes", of.shown),
            );
            it.way = first.map(|range| {
                let text = "each exit finishes it by its drop: return from one place (the work after it in one function that returns once), or finish it explicitly in one place";
                (range, text.to_owned())
            });
            out.push(it);
        }
    }

    // Effects of the calls into other crates, and fan-out.
    let own = func.krate(db);
    if on(Measure::Effects) {
        let effects = effects(cx, own, &callees);
        let kinds = effects.iter().filter(|it| it.is_some()).count() as u64;
        if let Some(over) = judge(cx, Measure::Effects, &of.item, kinds) {
            let mut said: Vec<String, Global> = Vec::with_capacity_in(KINDS.len(), Global);
            for (&(kind, _), it) in KINDS.iter().zip(effects.iter()) {
                if let Some((callee, _, _)) = it {
                    said.push(format!("{kind} through `{}`", fn_name(db, *callee)));
                }
            }
            let mut it = of.pending(
                over,
                format!("`{}` does {kinds} kinds of effect through its own calls: {}", of.shown, said.join(", ")),
            );
            // The kind fewest calls bring is the cheapest to move out.
            let fewest = KINDS
                .iter()
                .zip(effects.iter())
                .filter_map(|(&(kind, _), it)| it.map(|(_, at, calls)| (kind, at, calls)))
                .min_by_key(|&(_, _, calls)| calls);
            it.way = fewest.and_then(|(kind, at, _)| {
                let range = analysis.expr_range(at)?;
                let text = format!(
                    "move the calls that bring {kind}, like this one, into a function of this crate of their own, so this one does fewer kinds of thing"
                );
                Some((range, text))
            });
            out.push(it);
        }
    }
    if on(Measure::FanOut) {
        let mut called: Vec<FunctionId, Global> = Vec::with_capacity_in(callees.len(), Global);
        for &(callee, _) in &callees {
            if callee != func && !called.contains(&callee) && !callee.krate(db).data(db).origin.is_lang() {
                called.push(callee);
            }
        }
        let fan_out = called.len() as u64;
        if let Some(over) = judge(cx, Measure::FanOut, &of.item, fan_out) {
            // The lint's own help says the way out; a second one here would repeat it.
            out.push(of.pending(
                over,
                format!("`{}` calls {fan_out} distinct functions, the standard library's not counted", of.shown),
            ));
        }
    }
    out
}

/// The calls of the body, each with the function it resolves to, in the body's order.
fn callees(analysis: &hir::BodyAnalysis<'_>) -> Vec<(FunctionId, ExprId), Global> {
    let mut out = Vec::new_in(Global);
    for (id, expr) in analysis.body().exprs() {
        if !matches!(expr, Expr::Call { .. } | Expr::MethodCall { .. }) {
            continue;
        }
        if let Some((callee, _)) = analysis.callee(id) {
            out.push((callee, id));
        }
    }
    out
}

/// For each of [`KINDS`], the first call into another crate that brings it, its expression and
/// how many calls bring it; `None` for a kind no call brings.
fn effects(
    cx: &Cx<'_, '_>,
    own: base_db::Crate,
    callees: &[(FunctionId, ExprId)],
) -> [Option<(FunctionId, ExprId, u32)>; 6] {
    let mut out = [None; 6];
    let Some(analysis) = cx.analysis() else { return out };
    let db = cx.db();
    for &(callee, at) in callees {
        if callee.krate(db) == own {
            continue;
        }
        let Some((_, args)) = analysis.callee(at) else { continue };
        let set = hir_ty_pure::call_effects(db, own, callee, args.store());
        for (slot, &(_, rules)) in out.iter_mut().zip(KINDS.iter()) {
            if !rules.iter().any(|&rule| set.has(rule)) {
                continue;
            }
            match slot {
                Some((_, _, calls)) => *calls = calls.saturating_add(1),
                None => *slot = Some((callee, at, 1)),
            }
        }
    }
    out
}

/// The statements of the body's blocks, its unwind blocks left out.
fn statements(mir: &MirBody<'_>) -> u64 {
    mir.basic_blocks
        .iter()
        .filter(|(_, block)| !block.is_cleanup)
        .map(|(_, block)| block.statements.len() as u64)
        .sum()
}

/// The body's control-flow graph for [`paths::count`]: an unwind block, or one that ends in
/// `Unreachable`, is dead (no path goes through it).
fn cfg_of(mir: &MirBody<'_>) -> paths::Cfg {
    let count = mir.basic_blocks.len();
    let mut successors = Vec::with_capacity_in(count, Global);
    let mut dead = Vec::with_capacity_in(count, Global);
    for (_, block) in mir.basic_blocks.iter() {
        let mut next = Vec::new_in(Global);
        let unreachable = match &block.terminator {
            Some(terminator) => {
                next.extend(terminator.kind.successors().map(|it| it.into_raw().into_u32()));
                matches!(terminator.kind, TerminatorKind::Unreachable)
            }
            None => false,
        };
        successors.push(next);
        dead.push(block.is_cleanup || unreachable);
    }
    paths::Cfg { successors, dead, start: mir.start_block.into_raw().into_u32() }
}

/// The most mutable places (a `mut` binding, a `ref mut` one, a `&mut` local) the runtime MIR
/// holds live across one call, where that call is, and their names as a message lists them.
fn live_state(cx: &Cx<'_, '_>, def: DefWithBodyId) -> Option<(u64, Option<TextRange>, String)> {
    let analysis = cx.analysis()?;
    let db = cx.db();
    let mir = db.runtime_mir_body(InferBodyId::DefWithBodyId(def)).ok()?;
    let body = analysis.body();
    let mut calls = Vec::new_in(Global);
    for (block, data) in mir.basic_blocks.iter() {
        if data.is_cleanup {
            continue;
        }
        if let Some(terminator) = &data.terminator
            && let TerminatorKind::Call { .. } = terminator.kind
        {
            calls.push((block, terminator.span));
        }
    }
    if calls.is_empty() {
        return None;
    }
    let mut blocks = Vec::with_capacity_in(calls.len(), Global);
    blocks.extend(calls.iter().map(|&(block, _)| block));
    blaze_mem::frame::task(blaze_mem::Tag::Diagnostics, |frame| {
        let across = mir.held_across_calls_in(frame, &blocks);
        let mut best: Option<(u64, Option<TextRange>, Vec<String, Global>)> = None;
        for (&(_, span), locals) in calls.iter().zip(across.iter()) {
            let mut names = Vec::new_in(Global);
            for &local in locals.iter() {
                let binding =
                    mir.binding_locals.iter().find_map(|(binding, &it)| (it == local).then_some(binding));
                let annotated = binding.is_some_and(|it| {
                    matches!(body[it].mode, BindingAnnotation::Mutable | BindingAnnotation::RefMut)
                });
                if !annotated && !analysis.ty(mir.locals[local].ty.as_ref()).is_mutable_reference() {
                    continue;
                }
                names.push(match binding {
                    Some(it) => format!("`{}`", body[it].name.as_str()),
                    None => "a temporary `&mut`".to_owned(),
                });
            }
            let count = names.len() as u64;
            if best.as_ref().is_none_or(|(most, ..)| count > *most) {
                best = Some((count, facts::mir_range(analysis, span), names));
            }
        }
        let (count, at, mut names) = best?;
        let more = names.len().saturating_sub(MOST_NAMED);
        names.truncate(MOST_NAMED);
        let mut said = names.join(", ");
        if more > 0 {
            said.push_str(&format!(" and {more} more"));
        }
        Some((count, at, said))
    })
}

/// The first binding of the body that owns a value whose type's own drop finishes something
/// (it implements `Drop`, and its crate's drops do more than free memory), with its type.
fn finished_by_drop(cx: &Cx<'_, '_>) -> Option<(String, String)> {
    let analysis = cx.analysis()?;
    let drop = cx.lang_trait(hir::LangItem::Drop)?;
    let db = cx.db();
    let body = analysis.body();
    let target = cx.krate().to_display_target(db);
    for (binding, ty) in analysis.infer().binding_types() {
        let ty = analysis.ty(ty);
        if ty.is_reference() {
            continue;
        }
        let Some(adt) = ty.as_adt() else { continue };
        let home = adt.module(db).krate(db);
        if FREES_ONLY.iter().any(|it| facts::crate_is(db, home, it)) || !ty.impls_trait(db, drop, &[]) {
            continue;
        }
        return Some((body[binding].name.as_str().to_owned(), ty.display(db, target).to_string()));
    }
    None
}

/// The early exits of `block` (each `return`, `?`, and call or macro that does not return), and
/// where the first is; closures, `async` blocks and nested items are not the function's.
fn exits<'t>(cx: &Cx<'_, 't>, block: ast::Node<'t>) -> (u64, Option<TextRange>) {
    let mut count = 0u64;
    let mut first = None;
    let mut stack = Vec::new_in(Global);
    stack.push(block);
    while let Some(node) = stack.pop() {
        let exit = match node.kind() {
            SyntaxKind::CLOSURE_EXPR | SyntaxKind::FN | SyntaxKind::IMPL | SyntaxKind::MODULE => continue,
            SyntaxKind::BLOCK_EXPR if node.child_token(SyntaxKind::ASYNC_KW).is_some() => continue,
            SyntaxKind::RETURN_EXPR | SyntaxKind::TRY_EXPR => true,
            SyntaxKind::CALL_EXPR | SyntaxKind::METHOD_CALL_EXPR | SyntaxKind::MACRO_EXPR => {
                ast::Expr::cast(node).and_then(|it| cx.ty(it)).is_some_and(|it| it.is_never())
            }
            _ => false,
        };
        if exit {
            count += 1;
            let range = node.text_range();
            if first.is_none_or(|it: TextRange| range.start() < it.start()) {
                first = Some(range);
            }
        }
        // Children last to first, so the walk is in source order.
        let mut children = Vec::new_in(Global);
        children.extend(node.children());
        stack.extend(children.into_iter().rev());
    }
    (count, first)
}
