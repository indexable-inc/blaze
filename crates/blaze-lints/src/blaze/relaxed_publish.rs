//! `relaxed_publish`: an `AtomicBool` or `AtomicPtr` that one place sets with a `Relaxed`
//! `store` or `swap`, and another place of the crate reads with a `load` that guards other data.
//! `Relaxed` orders nothing around the atomic, so a thread that sees the flag set (or the new
//! pointer) is not promised to see the writes made before it: the data it then reads may be
//! stale or half-written. Publishing takes `Release` on the store and `Acquire` on the load.
//!
//! The atomic is a field (`self.ready`, any path to it through `Arc` or references) or a
//! `static`, the same one by its resolved field or static. A guarding load is one that is:
//!
//! - the condition of an `if` (or a `while`), alone, under `!`, or in `&&` / `||`, whose branch
//!   taken reads other data: a field (not an atomic's own operation), a dereference or an index;
//! - for an `AtomicPtr`, dereferenced (`*p.load(..)`), or bound by a `let` whose local is
//!   dereferenced or turned into a reference (`as_ref`, `as_mut`, `read`) in the body.
//!
//! The crate's bodies are read once ([`atomic_sites`]); a finding is reported at each side, the
//! store and the guarding load, each naming the other's function. A flag that guards no data
//! (`if stop.load(Relaxed) { return; }`) and counters are not found. Kept with `// relaxed:
//! <why>` on the field's or the static's declaration, or on the line of either side.
//!
//! Narrowed: the other atomics (a `usize` used as a flag) are not counted, a load that guards by
//! an early return (`if !ready.load(..) { return; } use(data)`) is not seen, and an atomic
//! reached through a local reference (`let f = &self.flag;`) is not followed.

use base_db::Db;
use blaze_hash::HashMap;
use hir::view::HasSourceView as _;
use hir_def::{
    DefWithBodyId, FieldId, StaticId,
    expr_store::Body,
    hir::{BinaryOp, Expr, ExprId, Pat, Statement, UnaryOp},
    nameres::crate_def_map,
    resolver::ValueNs,
};
use std::alloc::Global;
use syntax::view::AstNode as _;

use super::{
    Finding, Lint, exempt, facts,
    general_facts::{self, Walked},
};
use crate::check::{self, Cx};

const LINT: Lint = Lint::RelaxedPublish;

/// The comment that keeps an atomic's `Relaxed` ordering.
const TAG: &str = "relaxed";

/// An atomic, by where it lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Atomic {
    Field(FieldId),
    Static(StaticId),
}

/// What a site does with its atomic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    /// A `Relaxed` `store` or `swap` of a flag or a pointer.
    Publish,
    /// A `load` that guards other data.
    Guard,
}

/// One use of an atomic in a body.
struct Site {
    atomic: Atomic,
    role: Role,
    expr: ExprId,
}

/// The bodies of a crate that publish an atomic with `Relaxed`, and those that guard data by
/// it, each in the crate's walk order.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Uses {
    publish: Vec<DefWithBodyId>,
    guard: Vec<DefWithBodyId>,
}

/// [`Uses`] by atomic, for a crate.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Sites(HashMap<Atomic, Uses, Global>);

/// Every body of `krate` read for [`Site`]s, once per crate: its functions, consts and statics,
/// those of its traits and impls, and those of items nested in their blocks.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn atomic_sites(db: &Db, krate: base_db::Crate) -> Sites {
    let mut sites = Sites::default();
    // alloc: the crate's bodies, once per crate and edit
    let mut defs = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        check::bodies_of(db, module.scope(db), &mut defs);
    }
    let mut next = 0;
    while let Some(&def) = defs.get(next) {
        next += 1;
        for (_, block) in Body::of(db, def).blocks(db) {
            for (_, module) in block.modules(db) {
                check::bodies_of(db, module.scope(db), &mut defs);
            }
        }
        let analysis = hir::BodyAnalysis::new(db, hir::DefWithBody::from(def));
        for site in sites_of(db, def, &analysis) {
            if sites.0.get(&site.atomic).is_none() {
                sites.0.insert(site.atomic, Uses::default());
            }
            let Some(uses) = sites.0.get_mut(&site.atomic) else { continue };
            let list = match site.role {
                Role::Publish => &mut uses.publish,
                Role::Guard => &mut uses.guard,
            };
            if !list.contains(&def) {
                list.push(def);
            }
        }
    }
    sites
}

/// The publishing stores and guarding loads of flags and pointers in the body of `def`.
fn sites_of<'db>(db: &'db Db, def: DefWithBodyId, analysis: &hir::BodyAnalysis<'db>) -> Vec<Site> {
    // alloc: the few atomic sites of one body
    let mut out = Vec::new();
    let body = analysis.body();
    let mut parents = None;
    for (id, expr) in body.exprs() {
        let Expr::MethodCall { receiver, args, .. } = expr else { continue };
        let Some(callee) = general_facts::callee(db, analysis, id) else { continue };
        let Some(adt) = callee.self_adt else { continue };
        let pointer = general_facts::std_adt(db, adt, "sync", &["AtomicPtr"]);
        if !pointer && !general_facts::std_adt(db, adt, "sync", &["AtomicBool"]) {
            continue;
        }
        let Some(atomic) = atomic_of(db, def, analysis, receiver) else { continue };
        let role = match callee.name {
            "store" | "swap" => {
                let relaxed =
                    args.get(1).and_then(|&it| general_facts::ordering(db, def, analysis, it))
                        == Some("Relaxed");
                if !relaxed {
                    continue;
                }
                Role::Publish
            }
            "load" => {
                let parents = parents.get_or_insert_with(|| facts::Parents::of(body));
                if !guards(db, def, analysis, parents, id, pointer) {
                    continue;
                }
                Role::Guard
            }
            _ => continue,
        };
        out.push(Site { atomic, role, expr: id });
    }
    out
}

/// The atomic the receiver `expr` is: a field, or a `static`, under references and derefs.
fn atomic_of(
    db: &Db,
    def: DefWithBodyId,
    analysis: &hir::BodyAnalysis<'_>,
    expr: ExprId,
) -> Option<Atomic> {
    let body = analysis.body();
    let mut at = expr;
    for _ in 0..8 {
        match body.expr(at) {
            Expr::Ref { expr, .. } | Expr::UnaryOp { expr, op: UnaryOp::Deref } => at = expr,
            Expr::Field { .. } => {
                return match analysis.infer().field_resolution(at)? {
                    either::Either::Left(it) => Some(Atomic::Field(it)),
                    either::Either::Right(_) => None,
                };
            }
            Expr::Path(_) => {
                return match general_facts::value_of(db, def, body, at)? {
                    ValueNs::StaticId(it) => Some(Atomic::Static(it)),
                    _ => None,
                };
            }
            _ => return None,
        }
    }
    None
}

/// Whether the `load` call `load` guards other data, as the module docs say.
fn guards<'db>(
    db: &'db Db,
    def: DefWithBodyId,
    analysis: &hir::BodyAnalysis<'db>,
    parents: &facts::Parents,
    load: ExprId,
    pointer: bool,
) -> bool {
    let body = analysis.body();
    let mut at = load;
    for parent in parents.up(load, 8) {
        match body.expr(parent) {
            Expr::UnaryOp { op: UnaryOp::Not, .. }
            | Expr::BinaryOp { op: Some(BinaryOp::LogicOp(_)), .. } => at = parent,
            Expr::If { condition, then_branch, .. } if condition == at => {
                return reads_data(db, analysis, parents, then_branch);
            }
            Expr::UnaryOp { op: UnaryOp::Deref, .. } if pointer && at == load => return true,
            Expr::Block { statements, .. } if pointer && at == load => {
                let pat = statements.iter().find_map(|it| match it {
                    Statement::Let { pat, initializer: Some(init), .. } if init == load => {
                        Some(pat)
                    }
                    _ => None,
                });
                let Some(pat) = pat else { return false };
                let Pat::Bind { id: local, .. } = body.pat(pat) else { return false };
                return dereferenced(db, def, body, local);
            }
            _ => return false,
        }
    }
    false
}

/// Whether the branch `branch` reads data other than an atomic: a field that is not the
/// receiver of an atomic's own method, a dereference or an index. A walk that runs out says no.
fn reads_data<'db>(
    db: &'db Db,
    analysis: &hir::BodyAnalysis<'db>,
    parents: &facts::Parents,
    branch: ExprId,
) -> bool {
    let body = analysis.body();
    let mut reads = false;
    let walked = general_facts::walk(body, branch, |id| {
        reads |= match body.expr(id) {
            Expr::UnaryOp { op: UnaryOp::Deref, .. } | Expr::Index { .. } => true,
            Expr::Field { .. } => !atomic_receiver(db, analysis, parents, id),
            _ => false,
        };
    });
    reads && walked == Walked::Done
}

/// Whether the field expression `field` is the receiver of a method of one of core's atomics.
fn atomic_receiver<'db>(
    db: &'db Db,
    analysis: &hir::BodyAnalysis<'db>,
    parents: &facts::Parents,
    field: ExprId,
) -> bool {
    let Some(parent) = parents.of_expr(field) else { return false };
    let Expr::MethodCall { receiver, .. } = analysis.body().expr(parent) else { return false };
    receiver == field
        && general_facts::callee(db, analysis, parent)
            .and_then(|it| it.self_adt)
            .is_some_and(|adt| general_facts::is_atomic(db, adt))
}

/// Whether the local `local` is dereferenced, or made a reference by `as_ref`, `as_mut` or
/// `read`, anywhere in `body`.
fn dereferenced(db: &Db, def: DefWithBodyId, body: &Body, local: hir_def::hir::BindingId) -> bool {
    let is_local = |expr: ExprId| matches!(general_facts::value_of(db, def, body, expr), Some(ValueNs::LocalBinding(it)) if it == local);
    body.exprs().any(|(_, expr)| match expr {
        Expr::UnaryOp { expr, op: UnaryOp::Deref } => is_local(expr),
        Expr::MethodCall { receiver, method_name, .. } => {
            matches!(method_name.as_str(), "as_ref" | "as_mut" | "read") && is_local(receiver)
        }
        _ => false,
    })
}

/// Whether the declaration of `atomic` carries `// relaxed: <why>`, on its line or the line above.
fn documented(db: &Db, atomic: Atomic) -> bool {
    let node = match atomic {
        Atomic::Field(it) => hir::Field::from(it).source_view(db).map(|it| it.value.syntax()),
        Atomic::Static(it) => hir::Static::from(it).source_view(db).map(|it| it.value.syntax()),
    };
    let Some(node) = node else { return false };
    let Some(root) = node.ancestors().last() else { return false };
    exempt::commented(root.text(), node.text_range(), TAG)
}

/// How a message names `atomic`: `Type::field` or the static's name.
fn name_of(db: &Db, atomic: Atomic) -> String {
    match atomic {
        Atomic::Field(it) => {
            let field = hir::Field::from(it);
            format!("`{}::{}`", field.parent_def(db).name(db).as_str(), field.name(db).as_str())
        }
        Atomic::Static(it) => format!("`{}`", hir::Static::from(it).name(db).as_str()),
    }
}

/// How a message names the body `def`.
fn body_name(db: &Db, def: DefWithBodyId) -> String {
    match hir::DefWithBody::from(def).name(db) {
        Some(name) => format!("`{}`", name.as_str()),
        None => "another body".to_owned(), // alloc: a message's part
    }
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
    let here = sites_of(db, def, analysis);
    if here.is_empty() {
        return found;
    }
    let all = atomic_sites(db, cx.krate().base());
    for site in here {
        let Some(uses) = all.0.get(&site.atomic) else { continue };
        let others = match site.role {
            Role::Publish => &uses.guard,
            Role::Guard => &uses.publish,
        };
        let Some(&other) = others.iter().find(|&&it| it != def).or(others.first()) else {
            continue;
        };
        let Some(range) = analysis.expr_range(site.expr) else { continue };
        if exempt::commented(cx.text(), range, TAG) || documented(db, site.atomic) {
            continue;
        }
        let atomic = name_of(db, site.atomic);
        let other = body_name(db, other);
        let more = match others.len() {
            0 | 1 => String::new(),
            n => format!(" (and {} more)", n - 1),
        };
        let (message, note) = match site.role {
            Role::Publish => (
                format!(
                    "this `Relaxed` store of {atomic} publishes it, and {other}{more} reads it with a load that guards other data"
                ),
                "a thread whose load sees this value is not promised to see the writes made before the store; \
                 store with `Ordering::Release` (a `swap` with `AcqRel`) and load with `Ordering::Acquire`",
            ),
            Role::Guard => (
                format!(
                    "this load of {atomic} guards other data, and {other}{more} sets it with a `Relaxed` store"
                ),
                "a `Relaxed` store orders nothing before it, so the data this branch reads may not be written yet \
                 when the load sees the value; load with `Ordering::Acquire` and store with `Ordering::Release`",
            ),
        };
        facts::push(&mut found, LINT, range, message, Some(note.to_owned())); // alloc: a finding's note
    }
    found
}
