//! `lock_order` and `lock_across_wait` (`deny` by default, docs/v2/plan/design/effect-summary.md),
//! over the locks of the one effect summary (`hir_ty_pure::locks`): each acquire's class is read
//! from its receiver (`Type.field`, a static's path, else the lock's type), and a guard held
//! across a call is the MIR's.
//!
//! - `lock_order`: the crate's graph of edges "holds A while taking B", made of the summaries
//!   of its functions (a callee's edges included, so an order fixed in a dependency counts). A
//!   cycle means two threads can each hold one lock and wait for the other: a deadlock. It is
//!   one finding, at the function whose own body adds the least edge of the cycle, printing
//!   both chains: where A is held while B is taken, and where B is held while A is.
//! - `lock_across_wait`: a guard live across a call that blocks the thread (a condvar it is not
//!   handed, a channel, a sleep, a join, a blocking syscall): every thread that wants the lock
//!   waits as long. A fiber's park is `held_across_wait`'s, IO and allocation
//!   `lock_held_across_slow`'s.
//!
//! Each reports only what it can show: a cycle with both chains, a guard across a named call.
//! A summary with more classes or edges than it keeps, or that reaches a body that does not
//! lower, is `wide`: what it misses is unknown, never taken as clean, and refused only at a
//! function that asked for its locks to be proved ([`asked`]): one marked
//! `#[blaze::lock_checked]` (itself or a module around it, `#![blaze::lock_checked]`) or with a
//! role (`#[blaze::bounded_turn]`, `#[blaze::answers_request]`). Elsewhere an unknown is silent:
//! most code reaches a body that does not lower, and a refusal of each would bury the findings.

use base_db::Db;
use hir_def::{
    AttrDefId, DefWithBodyId, FunctionId, HasModule as _,
    attrs::{AttrFlags, PureAttr, RoleWord},
};
use hir_ty_pure::{
    Step,
    locks::{held_across, held_while_taking, lock_facts},
};

use super::{Finding, Lint, effects::StepNote, facts};
use crate::{check::Cx, diag};

/// How many modules one crate's graph reads at most.
const MODULES: usize = 16_384;

/// The rule the messages end with.
const RULE: &str = "docs/v2/plan/design/effect-summary.md";

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    let order = cx.on_blaze(Lint::LockOrder);
    let across = cx.on_blaze(Lint::LockAcrossWait);
    if !order && !across {
        return;
    }
    let Some(analysis) = cx.analysis() else { return };
    let Ok(DefWithBodyId::FunctionId(func)) = DefWithBodyId::try_from(analysis.def()) else {
        return;
    };
    let db = cx.db();
    if !hir_def::signatures::FunctionSignature::of(db, func).has_body() {
        return;
    }
    let Some(whole) = cx.range(cx.root()) else { return };
    // alloc: the findings of one function, reported at the edge
    let mut found: Vec<(Finding, Vec<StepNote>)> = Vec::new();
    if order {
        find_order(db, func, whole, &mut found);
    }
    if across {
        find_across(db, func, whole, &mut found);
    }
    for (finding, steps) in found {
        let diagnostic = facts::report_one(cx, finding);
        for (file, range, text) in steps {
            diagnostic.span_note(diag::of_file(file, range), text);
        }
    }
}

/// Whether `func` asked for its locks to be proved: `#[blaze::lock_checked]` on it or
/// `#![blaze::lock_checked]` on a module around it, or a checked role attribute on it (a
/// `trusted` one asks for nothing).
fn asked(db: &Db, func: FunctionId) -> bool {
    if AttrFlags::query(db, AttrDefId::FunctionId(func)).contains(AttrFlags::LOCK_CHECKED) {
        return true;
    }
    if RoleWord::ALL.into_iter().any(|role| {
        matches!(AttrFlags::role_attr(db, AttrDefId::FunctionId(func), role), Some(PureAttr::Checked))
    }) {
        return true;
    }
    let mut module = Some(hir::Function::from(func).module(db));
    while let Some(it) = module {
        if AttrFlags::query(db, AttrDefId::ModuleId(it.into())).contains(AttrFlags::LOCK_CHECKED) {
            return true;
        }
        module = it.parent(db);
    }
    false
}

/// An edge of the crate's graph: the classes, the function of the crate whose summary has it,
/// and whether its own body adds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CrateEdge {
    held: Box<str>,
    taken: Box<str>,
    func: FunctionId,
    /// The function's path, what ties are broken by (an id's order is the interner's).
    name: Box<str>,
    own: bool,
}

/// Every edge of the summaries of `krate`'s functions with bodies, sorted, each pair once (an
/// own edge before an inherited one), once per crate.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn crate_edges(db: &Db, krate: base_db::Crate) -> Box<[CrateEdge]> {
    // alloc: once per crate, the edges of its graph
    let mut edges: Vec<CrateEdge> = Vec::new();
    // alloc: the worklist of one crate's modules
    let mut modules = vec![hir::Crate::from(krate).root_module(db)];
    let mut seen = 0;
    // alloc: the functions of one module, a few
    let mut funcs: Vec<hir::Function> = Vec::new();
    while let Some(module) = modules.pop() {
        seen += 1;
        if seen > MODULES {
            break;
        }
        modules.extend(module.children(db));
        funcs.clear();
        for def in module.declarations(db) {
            if let hir::ModuleDef::Function(f) = def {
                funcs.push(f);
            }
        }
        for imp in module.impl_defs(db) {
            for item in imp.items(db) {
                if let hir::AssocItem::Function(f) = item {
                    funcs.push(f);
                }
            }
        }
        for &f in &funcs {
            let Ok(DefWithBodyId::FunctionId(func)) = DefWithBodyId::try_from(hir::DefWithBody::from(f))
            else {
                continue;
            };
            if !hir_def::signatures::FunctionSignature::of(db, func).has_body() {
                continue;
            }
            let facts = lock_facts(db, func);
            for edge in facts.order.iter() {
                edges.push(CrateEdge {
                    held: edge.held.clone(),       // clone: a class
                    taken: edge.taken.clone(),     // clone: a class
                    func,
                    name: facts.function.clone(), // clone: a path, once per edge
                    own: edge.own,
                });
            }
        }
    }
    // The own edge of a pair first, then the least function by path, so the graph is the same
    // however the modules were walked.
    edges.sort_by(|a, b| {
        (&a.held, &a.taken, !a.own, &a.name).cmp(&(&b.held, &b.taken, !b.own, &b.name))
    });
    edges.dedup_by(|b, a| a.held == b.held && a.taken == b.taken);
    edges.into_boxed_slice()
}

/// The shortest path of edges from `from` to `to` in `edges` (sorted by `held`), by
/// breadth-first search over the classes: each edge's index.
fn path(edges: &[CrateEdge], from: &str, to: &str) -> Option<Vec<usize>> {
    // alloc: the search's frontier and parents, once per edge of a function that holds a lock
    let mut parent: Vec<Option<usize>> = vec![None; edges.len()];
    let mut queue: std::collections::VecDeque<usize> = std::collections::VecDeque::new(); // alloc: as above
    /// The edges out of `class`: a run of the sorted edges.
    fn out<'e>(edges: &'e [CrateEdge], class: &'e str) -> impl Iterator<Item = usize> + 'e {
        let start = edges.partition_point(|it| &*it.held < class);
        (start..edges.len()).take_while(move |&at| &*edges[at].held == class)
    }
    let mut reached = vec![false; edges.len()]; // alloc: as above
    for at in out(edges, from) {
        reached[at] = true;
        queue.push_back(at);
    }
    while let Some(at) = queue.pop_front() {
        if &*edges[at].taken == to {
            let mut chain = vec![at]; // alloc: the cycle, once
            let mut back = at;
            while let Some(before) = parent[back] {
                chain.push(before);
                back = before;
            }
            chain.reverse();
            return Some(chain);
        }
        for next in out(edges, &edges[at].taken) {
            if !reached[next] {
                reached[next] = true;
                parent[next] = Some(at);
                queue.push_back(next);
            }
        }
    }
    None
}

/// A chain as a message says it: "`a` (src/lib.rs:4) → `b` (src/lib.rs:9)".
fn said(chain: &[Step]) -> String {
    chain
        .iter()
        .map(|step| match &step.at {
            Some(at) if !at.path.is_empty() => format!("`{}` ({}:{})", step.name, at.path, at.line),
            _ => format!("`{}`", step.name),
        })
        .collect::<Vec<_>>() // alloc: a message's parts
        .join(" → ")
}

/// Where a lock is taken, as a message says it: " (src/lib.rs:4)", or nothing.
fn at(place: Option<&hir_ty_pure::Place>) -> String {
    match place {
        Some(at) if !at.path.is_empty() => format!(" ({}:{})", at.path, at.line), // alloc: a message's part
        _ => String::new(), // alloc: empty, allocates nothing
    }
}

/// The notes of `chain`'s steps placed in `krate`'s own files.
fn notes(db: &Db, krate: base_db::Crate, chain: &[Step], last: &str) -> Vec<StepNote> {
    let own = db.file_source_root(krate.data(db).root_file_id).source_root_id(db);
    chain
        .iter()
        .enumerate()
        .filter_map(|(at, step)| {
            let place = step.at.as_ref()?;
            if db.file_source_root(place.file).source_root_id(db) != own {
                return None;
            }
            let text = match chain.get(at + 1) {
                Some(next) => format!("`{}` calls `{}` here", step.name, next.name), // alloc: a note
                None => format!("`{}` {last} here", step.name), // alloc: a note
            };
            Some((place.file, place.range, text))
        })
        .collect() // alloc: the notes of one finding
}

fn find_order(
    db: &Db,
    func: FunctionId,
    whole: syntax::TextRange,
    found: &mut Vec<(Finding, Vec<StepNote>)>,
) {
    let facts = lock_facts(db, func);
    let krate = func.krate(db);
    if facts.wide && asked(db, func) {
        let message = format!(
            "`{}` asked for its locks to be proved, but the check cannot show the absence of a lock-order cycle through it: it reaches more lock classes or lock-order edges than a summary keeps, or a body that does not lower: {RULE}",
            facts.function
        ); // alloc: a message
        found.push((Finding { lint: Lint::LockOrder, range: whole, message, note: None }, Vec::new()));
    }
    if !facts.order.iter().any(|it| it.own) {
        return;
    }
    let edges = crate_edges(db, krate);
    for edge in held_while_taking(db, func).iter() {
        // A cycle back from `taken` to `held` through the crate's graph.
        let Some(back) = path(edges, &edge.taken, &edge.held) else { continue };
        // Said once, at the function whose own edge is the least of the cycle's own edges.
        let least = back
            .iter()
            .map(|&at| &edges[at])
            .filter(|it| it.own)
            .all(|it| (&*edge.held, &*edge.taken) <= (&*it.held, &*it.taken));
        if !least {
            continue;
        }
        let first = &edges[back[0]];
        let reverse = held_while_taking(db, first.func)
            .iter()
            .find(|it| it.held == first.held && it.taken == first.taken)
            .cloned(); // clone: one edge's chain, once per finding
        let classes = std::iter::once(&*edge.held)
            .chain(std::iter::once(&*edge.taken))
            .chain(back.iter().map(|&at| &*edges[at].taken))
            .map(|it| format!("`{it}`"))
            .collect::<Vec<_>>() // alloc: a message's parts
            .join(" → ");
        let other = match &reverse {
            Some(it) => format!(
                "and {} takes `{}` while holding `{}`{}",
                said(&it.chain),
                it.taken,
                it.held,
                at(it.held_at.as_ref())
            ),
            None => format!(
                "and `{}` takes `{}` while holding `{}` through a callee",
                lock_facts(db, first.func).function,
                first.taken,
                first.held
            ),
        }; // alloc: a message's part
        let message = format!(
            "the locks are taken in a cycle, {classes}: {} takes `{}` while holding `{}`{}, {other}; two threads each holding one wait for the other for ever: take them in one order everywhere: {RULE}",
            said(&edge.chain),
            edge.taken,
            edge.held,
            at(edge.held_at.as_ref())
        ); // alloc: a message
        let steps = notes(db, krate, &edge.chain, "takes the lock");
        found.push((Finding { lint: Lint::LockOrder, range: whole, message, note: None }, steps));
    }
}

fn find_across(
    db: &Db,
    func: FunctionId,
    whole: syntax::TextRange,
    found: &mut Vec<(Finding, Vec<StepNote>)>,
) {
    let krate = func.krate(db);
    let facts = lock_facts(db, func);
    if facts.wide && asked(db, func) {
        let message = format!(
            "`{}` asked for its locks to be proved, but the check cannot show that no guard is held across a call that blocks under it: it reaches more locks or holds than a summary keeps, or a body that does not lower: {RULE}",
            facts.function
        ); // alloc: a message
        found.push((Finding { lint: Lint::LockAcrossWait, range: whole, message, note: None }, Vec::new()));
    }
    for hold in held_across(db, func).iter() {
        let message = format!(
            "the lock `{}`{} is held across {}, which {}: every thread that wants the lock waits as long; drop the guard before the call: {RULE}",
            hold.class,
            at(hold.held_at.as_ref()),
            said(&hold.chain),
            hold.what
        ); // alloc: a message
        let steps = notes(db, krate, &hold.chain, &hold.what);
        found.push((Finding { lint: Lint::LockAcrossWait, range: whole, message, note: None }, steps));
    }
}
