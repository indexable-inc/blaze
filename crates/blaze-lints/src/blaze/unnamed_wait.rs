//! `unnamed_wait`: a wait through `blaze_graph`'s API that does not say what it waits for
//! (AGENTS.md rule 12: a thread or fiber that blocks opens a span naming the awaited key, lock,
//! channel, IO or child). The profile test fails on such a wait when it runs; this finds it where
//! it is written. It goes once the typed wait API (%195) makes the name part of the call's type.
//!
//! A finding is:
//!
//! - `Flights::get_or_run` or `hold`, which name the wait only by the key's hash: the `_named`
//!   forms write the query and its key;
//! - `get_or_run_named` or `hold_named` whose naming closure writes nothing, or only a
//!   placeholder (`""`, `"wait"`, `"todo"`, ...);
//! - `wait::wait`, `wait::lock` or `wait::wait_until` named by a placeholder literal;
//! - `sched::wait` or `EventWait::wait`, which take no name, and a `Condvar` wait on a driver (off
//!   the pool: on it, `blocking_on_pool` has it), outside the block of a `wait::wait` that names
//!   them.
//!
//! `blaze_graph` itself, where the API is written, is not linted.

use base_db::Db;
use hir_def::{
    DefWithBodyId, FunctionId, HasModule as _, ModuleDefId,
    expr_store::Body,
    hir::{Expr, ExprId, Literal},
    signatures::FunctionSignature,
};

use super::{
    Finding, Lint,
    alloc::written_callee,
    blocking_on_pool::on_fiber_set,
    facts::{self, Parents},
};
use crate::check::Cx;

/// The names that say nothing of what is awaited, trimmed and lowercased.
const PLACEHOLDERS: &[&str] = &["", "wait", "waiting", "todo", "fixme", "tbd", "xxx", "...", "?"];

/// How many expressions a naming closure's body is looked through at most.
const MOST_EXPRS: usize = 4096;

/// How many parents up a wait looks for the `wait::wait` whose block it is in.
const MOST_PARENTS: usize = 64;

/// `blaze_graph`'s wait API, as the linted crate's dependency has it.
struct Api {
    wait: Option<FunctionId>,
    lock: Option<FunctionId>,
    until: Option<FunctionId>,
    sched_wait: Option<FunctionId>,
    flights: Option<hir::Adt>,
    event_wait: Option<hir::Adt>,
}

impl Api {
    fn of(db: &Db, krate: base_db::Crate) -> Api {
        let f = |path: &[&str]| facts::dep_fn(db, krate, "blaze_graph", path);
        let adt = |path: &[&str]| match facts::dep_type(db, krate, "blaze_graph", path)? {
            ModuleDefId::AdtId(it) => Some(hir::Adt::from(it)),
            _ => None,
        };
        Api {
            wait: f(&["wait", "wait"]),
            lock: f(&["wait", "lock"]),
            until: f(&["wait", "wait_until"]),
            sched_wait: f(&["sched", "wait"]),
            flights: adt(&["pump", "Flights"]),
            event_wait: adt(&["sched", "EventWait"]),
        }
    }
}

/// What a wait says of what it waits for, when it does not name it.
enum Said {
    Nothing,
    Hash,
    Placeholder(String),
}

/// What a finding says to do instead.
#[derive(Clone, Copy)]
enum Instead {
    Named,
    Span,
    Name,
}

impl Instead {
    fn say(self) -> &'static str {
        match self {
            Instead::Named => {
                "call the `_named` form (`get_or_run_named`, `hold_named`) and write the query and its key into the name"
            }
            Instead::Span => {
                "open the wait's span around it, `blaze_graph::wait::wait(WaitOn::Key(name), owner, || ..)`, or wait on a `Condvar` with `wait::wait_until(condvar, held, name, until)`"
            }
            Instead::Name => "write the name of what is awaited: the key, what the lock guards, what the channel carries",
        }
    }
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(Lint::UnnamedWait) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let db = cx.db();
    let krate = cx.krate();
    if facts::crate_is(db, krate, "blaze_graph") {
        return found;
    }
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let api = Api::of(db, krate.base());
    let body = analysis.body();
    let infer = analysis.infer();
    let mut parents: Option<Parents> = None;
    let mut on_fiber: Option<bool> = None;
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::Call { .. } | Expr::MethodCall { .. }) {
            continue;
        }
        let Some((f, _)) = written_callee(infer, body, id) else { continue };
        let name = FunctionSignature::of(db, f).name.as_str();
        let of = facts::impl_adt(db, f);
        let (api_name, said, instead) = if Some(f) == api.wait {
            let Some(on) = arg(body, id, 0, false) else { continue };
            let Expr::Call { args: [named], .. } = body.expr(on) else { continue };
            let Some(it) = placeholder(body, *named) else { continue };
            ("wait::wait".to_owned(), Said::Placeholder(it), Instead::Name) // alloc: a finding's name
        } else if Some(f) == api.lock || Some(f) == api.until {
            let at = if Some(f) == api.lock { 1 } else { 2 };
            let Some(it) = arg(body, id, at, false).and_then(|it| placeholder(body, it)) else { continue };
            (format!("wait::{name}"), Said::Placeholder(it), Instead::Name)
        } else if of.is_some() && of == api.flights {
            let said = match name {
                "get_or_run" | "hold" => Said::Hash,
                "get_or_run_named" | "hold_named" => {
                    let Some(said) = arg(body, id, 1, true).and_then(|it| named_by(body, it)) else { continue };
                    said
                }
                _ => continue,
            };
            let instead = match said {
                Said::Hash | Said::Nothing => Instead::Named,
                Said::Placeholder(_) => Instead::Name,
            };
            (format!("Flights::{name}"), said, instead)
        } else if Some(f) == api.sched_wait || (of.is_some() && of == api.event_wait && name == "wait") {
            let parents = parents.get_or_insert_with(|| Parents::of(body));
            if in_wait_block(&api, analysis, parents, id) {
                continue;
            }
            let said = match of {
                Some(_) => "EventWait::wait".to_owned(), // alloc: a finding's name
                None => "sched::wait".to_owned(),        // alloc: as above
            };
            (said, Said::Nothing, Instead::Span)
        } else if name.starts_with("wait") && of.is_some_and(|it| is_condvar(cx, it)) {
            let DefWithBodyId::FunctionId(owner) = def else { continue };
            if *on_fiber.get_or_insert_with(|| on_fiber_set(db, owner.krate(db)).contains(owner)) {
                continue;
            }
            let parents = parents.get_or_insert_with(|| Parents::of(body));
            if in_wait_block(&api, analysis, parents, id) {
                continue;
            }
            (format!("Condvar::{name}"), Said::Nothing, Instead::Span)
        } else {
            continue;
        };
        let Some(range) = analysis.expr_range(id) else { continue };
        let message = match &said {
            Said::Nothing => format!("`{api_name}` names nothing it waits for"),
            Said::Hash => format!("`{api_name}` names what it waits for only as its key's hash"),
            Said::Placeholder(it) => format!("`{api_name}` names what it waits for only as `{it:?}`"),
        };
        let note = format!(
            "a wait opens a span naming what it waits for, and the profile test fails on one with no name (AGENTS.md \
             rule 12): {}",
            instead.say()
        );
        facts::push(&mut found, Lint::UnnamedWait, range, message, Some(note));
    }
    found
}

/// The argument at `index` of the call `call`, `self` not counted: a method's when `method`, to
/// skip the receiver of a call written as a path.
fn arg(body: &Body, call: ExprId, index: usize, method: bool) -> Option<ExprId> {
    match body.expr(call) {
        Expr::MethodCall { args, .. } => args.get(index).copied(),
        Expr::Call { args, .. } => args.get(index + usize::from(method)).copied(),
        _ => None,
    }
}

/// The literal `expr` is, when it is a placeholder.
fn placeholder(body: &Body, expr: ExprId) -> Option<String> {
    let Expr::Literal(Literal::String(text)) = body.expr(expr) else { return None };
    let said = text.as_str().trim().to_lowercase();
    PLACEHOLDERS.contains(&said.as_str()).then(|| text.as_str().to_owned()) // alloc: a finding's text
}

/// What the naming closure `expr` writes, when it names nothing or only a placeholder; `None`
/// when it names something, or is not a closure written here.
fn named_by(body: &Body, expr: ExprId) -> Option<Said> {
    let Expr::Closure { body: inner, .. } = body.expr(expr) else { return None };
    // alloc: the closure's expressions, a few
    let mut stack = vec![inner];
    let mut seen = 0;
    let mut first = None;
    while let Some(at) = stack.pop() {
        seen += 1;
        if seen > MOST_EXPRS {
            return None;
        }
        match body.expr(at) {
            Expr::MethodCall { method_name, args: [text], .. } if method_name.as_str() == "push_str" => {
                let it = placeholder(body, *text)?;
                first.get_or_insert(it);
            }
            Expr::Call { .. } | Expr::MethodCall { .. } => return None,
            _ => {}
        }
        body.walk_child_exprs(at, |it| stack.push(it));
    }
    Some(first.map_or(Said::Nothing, Said::Placeholder))
}

/// Whether `id` is inside the block closure of a `wait::wait`, which names the wait.
fn in_wait_block(api: &Api, analysis: &hir::BodyAnalysis<'_>, parents: &Parents, id: ExprId) -> bool {
    let Some(wait) = api.wait else { return false };
    let body = analysis.body();
    parents.up(id, MOST_PARENTS).any(|closure| {
        if !matches!(body.expr(closure), Expr::Closure { .. }) {
            return false;
        }
        let Some(call) = parents.of_expr(closure) else { return false };
        matches!(body.expr(call), Expr::Call { args, .. } if args.contains(&closure))
            && written_callee(analysis.infer(), body, call).is_some_and(|(f, _)| f == wait)
    })
}

fn is_condvar(cx: &Cx<'_, '_>, adt: hir::Adt) -> bool {
    cx.std_def(&["std", "sync", "Condvar"]) == Some(hir::ModuleDef::Adt(adt))
        || facts::adt_is(cx.db(), adt, "parking_lot", "Condvar")
}
