//! `thread_local_in_waiting_fn`: a thread-local read in a function that may park the fiber, or
//! in a function that can be inlined into one (AGENTS.md rule 27). LLVM treats a thread-local's
//! address as constant inside one function, so a function that reads `CACHES.with(..)` and also
//! waits takes the address once and goes on using it after the fiber resumed on another worker:
//! the 2026-11-02 "RefCell already borrowed" in `blaze_graph`'s pool, where `CACHES.with` and
//! `LOCAL.get()` were inlined into `blaze_codegen::driver::Build::walk` across its
//! `sched::host::wait`. The fix is a small `#[inline(never)]` reader that does not wait
//! (`pool::with_caches`, `pool::local`, `sched::host::job_begins`).
//!
//! A read is a call of a method of `std::thread::LocalKey` that reaches the slot ([`READS`]),
//! resolved through inference, never by text. Which bodies may park the fiber is the may-suspend
//! check's ([`hir_ty_suspend::suspend_points`]): a body with a point may. In such a body this
//! finds:
//!
//! 1. each read in the body itself (its closures included), `#[inline(never)]` or not: the
//!    body's own wait splits it the same way;
//! 2. each call of a function that reads a thread-local in its own body and can be inlined
//!    here: not `#[inline(never)]`, and of this crate, or `#[inline]`, or generic (built in the
//!    crate that names its arguments). A function of std, core or alloc is not followed: its
//!    reads are std's, and a call of `LocalKey`'s own methods is (1).
//!
//! Not seen: a read two calls down (a reader inlined into a function itself inlined here), a
//! trait method called through a type parameter, and inlining across crates by LTO.

use base_db::Db;
use hir_def::{
    AttrDefId, DefWithBodyId, FunctionId, HasModule as _,
    attrs::AttrFlags,
    expr_store::Body,
    hir::{Expr, ExprId, UnaryOp},
    resolver::ValueNs,
    signatures::FunctionSignature,
};
use hir_ty::InferenceResult;

use super::{Finding, Lint, alloc::written_callee, facts, general_facts};
use crate::check::Cx;

/// The methods of `LocalKey` that reach the thread's slot, with how a finding names each.
const READS: &[(&str, &str)] = &[
    ("with", "LocalKey::with"),
    ("try_with", "LocalKey::try_with"),
    ("with_borrow", "LocalKey::with_borrow"),
    ("with_borrow_mut", "LocalKey::with_borrow_mut"),
    ("get", "LocalKey::get"),
    ("set", "LocalKey::set"),
    ("take", "LocalKey::take"),
    ("replace", "LocalKey::replace"),
];

/// How many references and derefs a read's receiver is looked through for the item it names.
const PEEL: usize = 4;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(Lint::ThreadLocalInWaitingFn) || !cx.suspend_checked() {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

/// A read of a thread-local: what it reads, as a finding names it, and the method.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Read {
    key: String,
    method: &'static str,
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    let krate = def.krate(db);
    let points = hir_ty_suspend::suspend_points(db, krate, def);
    // The call that parks first in the text, for the finding to name.
    let Some(first) = points.iter().min_by_key(|it| {
        facts::mir_range(analysis, it.call.span).map_or(u32::MAX, |range| range.start().into())
    }) else {
        return found;
    };
    let parks = first.path_said(db);
    let owner = owner_said(db, def);
    let body = analysis.body();
    let infer = analysis.infer();
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::Call { .. } | Expr::MethodCall { .. }) {
            continue;
        }
        let Some(range) = analysis.expr_range(id) else { continue };
        if let Some(read) = read_at(db, def, body, infer, id) {
            let message = format!(
                "{} is read through `{}` in {owner}, which may park the fiber at {parks}",
                read.key, read.method
            );
            let note = format!(
                "LLVM takes a thread-local's address once per function, so after a wait resumes the fiber on another \
                 worker, {owner} can go on using the first thread's slot (AGENTS.md rule 27); `#[inline(never)]` on \
                 {owner} does not help, its own wait splits it the same way"
            );
            facts::push(&mut found, Lint::ThreadLocalInWaitingFn, range, message, Some(note));
            continue;
        }
        let Some((callee, args)) = written_callee(infer, body, id) else { continue };
        if def == DefWithBodyId::FunctionId(callee) {
            continue;
        }
        let generic = args.types().next().is_some() || args.consts().next().is_some();
        let Some(why) = inlinable(db, callee, krate, generic) else { continue };
        let Some(read) = first_read(db, callee) else { continue };
        let callee = fn_said(db, callee);
        let message = format!(
            "{callee} reads {} through `{}` and can be inlined into {owner}, which may park the fiber at {parks}",
            read.key, read.method
        );
        let note = format!(
            "{callee} is {why}: inlined, its thread-local's address is taken once for all of {owner} and can be used \
             after a wait resumes the fiber on another worker (AGENTS.md rule 27)"
        );
        facts::push(&mut found, Lint::ThreadLocalInWaitingFn, range, message, Some(note));
    }
    found
}

/// Why `callee`, called from a body of `krate` (with `generic` arguments when its call names
/// types or consts), can be inlined there, as a note says it; `None` when it can't, or when it
/// is std's, core's or alloc's.
fn inlinable(
    db: &Db,
    callee: FunctionId,
    krate: base_db::Crate,
    generic: bool,
) -> Option<&'static str> {
    let flags = AttrFlags::query(db, AttrDefId::FunctionId(callee));
    let home = callee.krate(db);
    if flags.contains(AttrFlags::INLINE_NEVER) || home.data(db).origin.is_lang() {
        return None;
    }
    if flags.contains(AttrFlags::IS_INLINE) {
        Some("`#[inline]`")
    } else if home == krate {
        Some("of this crate and not `#[inline(never)]`")
    } else if generic {
        Some("generic, so built in this crate")
    } else {
        None
    }
}

/// The first thread-local `f` reads in its own body (its closures included), not in what it
/// calls; `None` for a function of `LocalKey`'s own, whose call is the read.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn first_read(db: &Db, f: FunctionId) -> Option<Read> {
    if local_key_method(db, f).is_some() || !FunctionSignature::of(db, f).has_body() {
        return None;
    }
    let def = DefWithBodyId::FunctionId(f);
    let body = Body::of(db, def);
    let infer = InferenceResult::of(db, def);
    body.exprs().find_map(|(id, _)| read_at(db, def, body, infer, id))
}

/// The read the call `id` of `def`'s body is, when it calls a method of [`READS`].
fn read_at<'db>(
    db: &'db Db,
    def: DefWithBodyId,
    body: &Body,
    infer: &InferenceResult<'db>,
    id: ExprId,
) -> Option<Read> {
    let (f, _) = written_callee(infer, body, id)?;
    let method = local_key_method(db, f)?;
    let receiver = match body.expr(id) {
        Expr::MethodCall { receiver, .. } => Some(receiver),
        Expr::Call { args, .. } => args.first().copied(),
        _ => None,
    };
    let key = receiver.and_then(|it| key_said(db, def, body, it));
    // alloc: a finding's name of what is read
    let key = key.unwrap_or_else(|| "a thread-local reached through a reference".to_owned());
    Some(Read { key, method })
}

/// How a finding names `f`, when it is a method of [`READS`] on `LocalKey`.
fn local_key_method(db: &Db, f: FunctionId) -> Option<&'static str> {
    let adt = facts::impl_adt(db, f)?;
    if !facts::adt_is(db, adt, "std", "LocalKey") {
        return None;
    }
    let name = FunctionSignature::of(db, f).name.as_str();
    READS.iter().find(|(it, _)| *it == name).map(|(_, said)| *said)
}

/// The thread-local the receiver `expr` names, under references and derefs: the path of its
/// `static` or `const` (`thread_local!` makes a `const`), or the field it is held in.
fn key_said(db: &Db, def: DefWithBodyId, body: &Body, expr: ExprId) -> Option<String> {
    let mut at = expr;
    for _ in 0..PEEL {
        match body.expr(at) {
            Expr::Ref { expr, .. } | Expr::UnaryOp { expr, op: UnaryOp::Deref } => at = expr,
            Expr::Field { name, .. } => {
                // alloc: a finding's name of what is read
                return Some(format!("the thread-local in the field `{}`", name.as_str()));
            }
            Expr::Path(_) => {
                let (module, name) = match general_facts::value_of(db, def, body, at)? {
                    ValueNs::StaticId(it) => {
                        (hir::Static::from(it).module(db), hir::Static::from(it).name(db))
                    }
                    ValueNs::ConstId(it) => {
                        (hir::Const::from(it).module(db), hir::Const::from(it).name(db)?)
                    }
                    _ => return None,
                };
                // alloc: as above
                return Some(format!(
                    "the thread-local `{}`",
                    item_path(db, module, name.as_str())
                ));
            }
            _ => return None,
        }
    }
    None
}

/// `name` in `module`, from its crate's name: `blaze_graph::pool::CACHES`.
fn item_path(db: &Db, module: hir::Module, name: &str) -> String {
    // alloc: a finding's path of an item
    let mut path = String::new();
    if let Some(krate) = module.krate(db).display_name(db) {
        path.push_str(krate.crate_name().as_str());
        path.push_str("::");
    }
    for segment in module.path_segments(db) {
        path.push_str(segment.as_str());
        path.push_str("::");
    }
    path.push_str(name);
    path
}

/// How a finding names the body `def`: `` `fn Build::walk` ``.
fn owner_said(db: &Db, def: DefWithBodyId) -> String {
    match def {
        DefWithBodyId::FunctionId(f) => fn_said(db, f),
        _ => {
            let name = hir::DefWithBody::from(def).name(db);
            // alloc: a finding's name of a body
            format!("`{}`", name.as_ref().map_or("_", |it| it.as_str()))
        }
    }
}

/// How a finding names the function `f`: `` `fn Build::walk` ``, `` `fn reader` ``.
fn fn_said(db: &Db, f: FunctionId) -> String {
    let name = FunctionSignature::of(db, f).name.as_str();
    match facts::impl_adt(db, f) {
        // alloc: a finding's name of a function
        Some(adt) => format!("`fn {}::{name}`", adt.name(db).as_str()),
        None => format!("`fn {name}`"), // alloc: as above
    }
}
