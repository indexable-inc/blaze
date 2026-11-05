//! `unbounded_from_input`: a loop that reads input it doesn't control (a socket, a `Read` or
//! `BufRead`, stdin, a listener's `incoming()`) and grows a collection or an unbounded channel
//! once per round with no cap on it in the loop. Whoever writes the input then decides how much
//! memory the program takes: a peer that sends faster than the receiver drains, or a line that
//! never ends, grows it until the process is killed.
//!
//! A loop reads input when it is a `for` over std's `io::Lines`, `io::Bytes`, `io::Split` or
//! `net::Incoming`, or when its body calls a read of std's `io` or `net` (`Read::read*`,
//! `BufRead::read_line` and `read_until`, `Stdin::read_line`, `TcpListener::accept`,
//! `UdpSocket::recv*`), the callee resolved. What grows: `push`, `push_back`, `push_front`,
//! `insert`, `extend` and `extend_from_slice` on a `Vec`, a `VecDeque` or a hash map or set; a
//! `send` on std's `mpsc::Sender` (`channel()`'s, never bounded), tokio's `UnboundedSender`, or
//! a crossbeam-channel `Sender` that the same body took from `crossbeam_channel::unbounded()`.
//! The collection must live past one round: a field, or a local declared outside the loop.
//!
//! A cap is any call on the same collection in the loop that reads or limits its size: `len`,
//! `capacity`, `is_full`, `is_empty`, `clear`, `truncate`, `pop*`, `remove`, `drain`, `retain`,
//! `try_reserve`. Narrowed: input read by a function the loop calls (not std's) is not seen, and
//! a cap checked outside the loop (or by a helper) is not either.

use base_db::Db;
use blaze_hash::HashSet;
use hir_def::{
    DefWithBodyId, FieldId,
    expr_store::Body,
    hir::{BindingId, Expr, ExprId, LoopSource, Pat, Statement, UnaryOp},
    resolver::ValueNs,
};
use std::alloc::Global;

use super::{
    Finding, Lint, facts,
    general_facts::{self, Walked},
};
use crate::check::Cx;

const LINT: Lint = Lint::UnboundedFromInput;

/// The calls that grow a collection or queue a message.
const GROW: &[&str] =
    &["push", "push_back", "push_front", "insert", "extend", "extend_from_slice", "send"];

/// The calls on a collection that read or limit its size.
const CAP: &[&str] = &[
    "len",
    "capacity",
    "is_full",
    "is_empty",
    "clear",
    "truncate",
    "pop",
    "pop_front",
    "pop_back",
    "remove",
    "drain",
    "retain",
    "try_reserve",
    "shrink_to",
];

/// A collection, by where it lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    Field(FieldId),
    Local(BindingId),
}

/// What grows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Grows {
    Collection,
    Channel,
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
    let body = analysis.body();
    let mut parents = None;
    for (loop_, expr) in body.exprs() {
        let Expr::Loop { body: inner, source, .. } = expr else { continue };
        let parents = parents.get_or_insert_with(|| facts::Parents::of(body));
        let for_input =
            source == LoopSource::ForLoop && for_over_input(db, analysis, parents, loop_);
        // alloc: the calls of one loop, a few dozen
        let mut calls = Vec::new();
        let walked = general_facts::walk(body, inner, |id| {
            if matches!(body.expr(id), Expr::MethodCall { .. } | Expr::Call { .. }) {
                calls.push(id);
            }
        });
        if walked == Walked::OutOfFuel {
            continue;
        }
        let reads = for_input || calls.iter().any(|&id| reads_input(db, analysis, id));
        if !reads {
            continue;
        }
        let inside = declared_in(body, inner);
        for &call in &calls {
            let Expr::MethodCall { receiver, method_name, .. } = body.expr(call) else { continue };
            if !GROW.contains(&method_name.as_str()) {
                continue;
            }
            let Some(grows) = grows(db, def, analysis, call) else { continue };
            let Some(place) = place_of(db, def, analysis, receiver) else { continue };
            if matches!(place, Place::Local(it) if inside.contains(&it)) {
                continue;
            }
            let capped = calls.iter().any(|&it| match body.expr(it) {
                Expr::MethodCall { receiver, method_name, .. } => {
                    CAP.contains(&method_name.as_str())
                        && place_of(db, def, analysis, receiver) == Some(place)
                }
                _ => false,
            });
            if capped {
                continue;
            }
            let Some(range) = analysis.expr_range(call) else { continue };
            let (message, note) = match grows {
                Grows::Collection => (
                    format!(
                        "this `{}` grows a collection once per round of a loop that reads input, with no cap on its size in the loop",
                        method_name.as_str()
                    ),
                    "whoever writes the input decides how much memory the collection takes",
                ),
                Grows::Channel => (
                    "this `send` queues a message on an unbounded channel once per round of a loop that reads input".to_owned(), // alloc: a message's part
                    "an unbounded channel keeps every message until the receiver takes it: a reader faster than the \
                     receiver grows the queue without limit",
                ),
            };
            facts::push(&mut found, LINT, range, message, Some(note.to_owned())); // alloc: a finding's note
        }
    }
    found
}

/// Whether the `for` loop `loop_` (its desugared `loop`) iterates over std's lines, bytes or
/// splits of a reader, or a listener's incoming connections.
fn for_over_input<'db>(
    db: &'db Db,
    analysis: &hir::BodyAnalysis<'db>,
    parents: &facts::Parents,
    loop_: ExprId,
) -> bool {
    let body = analysis.body();
    let Some(parent) = parents.of_expr(loop_) else { return false };
    let Expr::Match { expr: iterator, .. } = body.expr(parent) else { return false };
    let Expr::Call { args, .. } = body.expr(iterator) else { return false };
    let Some(&head) = args.first() else { return false };
    let Some(adt) = analysis.ty(analysis.infer().expr_ty(head)).strip_references().as_adt() else {
        return false;
    };
    general_facts::std_adt(db, adt, "io", &["Lines", "Bytes", "Split"])
        || general_facts::std_adt(db, adt, "net", &["Incoming"])
}

/// Whether the call `call` reads input: a read of std's `io` or `net`.
fn reads_input<'db>(db: &'db Db, analysis: &hir::BodyAnalysis<'db>, call: ExprId) -> bool {
    let Some(callee) = general_facts::callee(db, analysis, call) else { return false };
    let named = callee.name.starts_with("read")
        || callee.name.starts_with("recv")
        || callee.name == "accept";
    named && (callee.in_std_module(db, "io") || callee.in_std_module(db, "net"))
}

/// What the grow call `call` grows, if it is one this lint counts.
fn grows<'db>(
    db: &'db Db,
    def: DefWithBodyId,
    analysis: &hir::BodyAnalysis<'db>,
    call: ExprId,
) -> Option<Grows> {
    let callee = general_facts::callee(db, analysis, call)?;
    let adt = callee.self_adt?;
    if callee.name == "send" {
        if general_facts::std_adt(db, adt, "sync", &["Sender"])
            || general_facts::crate_adt(db, adt, "tokio", &["UnboundedSender"])
        {
            return Some(Grows::Channel);
        }
        if general_facts::crate_adt(db, adt, "crossbeam_channel", &["Sender"]) {
            let Expr::MethodCall { receiver, .. } = analysis.body().expr(call) else { return None };
            let Place::Local(local) = place_of(db, def, analysis, receiver)? else { return None };
            return from_unbounded(db, analysis, local).then_some(Grows::Channel);
        }
        return None;
    }
    let vec =
        analysis.def().module(db).krate(db).diagnostic_item_adt(db, &intern::Symbol::intern("Vec"))
            == Some(adt);
    let hash = matches!(adt, hir::Adt::Struct(_)) && {
        let names = ["HashMap", "HashSet"];
        general_facts::std_adt(db, adt, "collections", &names)
            || general_facts::crate_adt(db, adt, "hashbrown", &names)
    };
    (vec || hash || general_facts::std_adt(db, adt, "collections", &["VecDeque"]))
        .then_some(Grows::Collection)
}

/// Whether the local `local` was bound, in the body, as the sender of a
/// `crossbeam_channel::unbounded()` call: `let (tx, rx) = unbounded();`.
fn from_unbounded<'db>(db: &'db Db, analysis: &hir::BodyAnalysis<'db>, local: BindingId) -> bool {
    let body = analysis.body();
    body.exprs().any(|(_, expr)| {
        let Expr::Block { statements, .. } = expr else { return false };
        statements.iter().any(|it| {
            let Statement::Let { pat, initializer: Some(init), .. } = it else { return false };
            let Pat::Tuple { args, .. } = body.pat(pat) else { return false };
            let binds = args.first().is_some_and(
                |&first| matches!(body.pat(first), Pat::Bind { id, .. } if id == local),
            );
            binds
                && general_facts::callee(db, analysis, init).is_some_and(|it| {
                    it.name == "unbounded"
                        && facts::crate_is(
                            db,
                            hir::Function::from(it.f).module(db).krate(db),
                            "crossbeam_channel",
                        )
                })
        })
    })
}

/// The collection the receiver `expr` is, under references and derefs: a field or a local.
fn place_of(
    db: &Db,
    def: DefWithBodyId,
    analysis: &hir::BodyAnalysis<'_>,
    expr: ExprId,
) -> Option<Place> {
    let body = analysis.body();
    let mut at = expr;
    for _ in 0..8 {
        match body.expr(at) {
            Expr::Ref { expr, .. } | Expr::UnaryOp { expr, op: UnaryOp::Deref } => at = expr,
            Expr::Field { .. } => {
                return match analysis.infer().field_resolution(at)? {
                    either::Either::Left(it) => Some(Place::Field(it)),
                    either::Either::Right(_) => None,
                };
            }
            Expr::Path(_) => {
                return match general_facts::value_of(db, def, body, at)? {
                    ValueNs::LocalBinding(it) => Some(Place::Local(it)),
                    _ => None,
                };
            }
            _ => return None,
        }
    }
    None
}

/// The locals declared inside `root`: by a `let`, a `match` arm, a closure's parameters, or a
/// `for` loop's pattern.
fn declared_in(body: &Body, root: ExprId) -> HashSet<BindingId, Global> {
    // alloc: the locals of one loop
    let mut out = HashSet::new_in(Global);
    let mut add = |pat| {
        body.walk_bindings_in_pat(pat, |it| {
            out.insert(it);
        })
    };
    // The caller walked the same subtree to the end with the same fuel, so this walk ends too.
    let _ = general_facts::walk(body, root, |id| match body.expr(id) {
        Expr::Block { statements, .. } => {
            for statement in statements.iter() {
                if let Statement::Let { pat, .. } = statement {
                    add(pat);
                }
            }
        }
        Expr::Match { arms, .. } => {
            for arm in arms {
                add(arm.pat);
            }
        }
        Expr::Closure { args, .. } => {
            for &arg in args {
                add(arg);
            }
        }
        _ => {}
    });
    out
}
