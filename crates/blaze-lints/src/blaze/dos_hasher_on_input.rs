//! `dos_hasher_on_input`: in a library crate, a hash map or set whose hasher has a fixed seed
//! (FxHash, foldhash's `FixedState`, FNV, `BuildHasherDefault` of any hasher but std's SipHash)
//! keyed by text or bytes a caller hands to a public function. Whoever picks the keys can pick
//! many that land in one bucket, since the hash function is known and the same in every
//! process: each insert then walks the whole chain, and a few thousand keys cost seconds
//! (HashDoS). std's `RandomState` seeds each map at random; so do foldhash's and hashbrown's
//! default `RandomState`.
//!
//! Checked: the insert, `entry` and `get_or_insert*` calls of std's and hashbrown's `HashMap`
//! and `HashSet` in a `pub fn` of a crate built as a library, when the key comes from one of the
//! function's parameters that is a `str`, `String`, `[u8]`, `Vec<u8>` or `Cow` of them: the
//! parameter itself, a method or call made on it (`name.to_owned()`, `String::from(name)`), or
//! a local bound from those by a `let` or a `for` loop over them (`for word in text.split(' ')`),
//! followed four rounds deep in the one body.
//!
//! The hasher is the map's type argument resolved, never the text written, so `FxHashMap<K, V>`
//! and a workspace alias are both found. Not counted: ahash's `RandomState` (its seed is fixed
//! only by `with_seeds`, which the type doesn't say), keys from input the function reads itself
//! (a socket, a file), and keys from a parameter of another type.
//!
//! A workspace that turns on `blaze::internal` (blaze itself, which keys maps by a fixed-seed
//! hasher for deterministic output, AGENTS.md rule 25) has this lint allowed by default: it is
//! in the general lints that yield to `blaze::internal` (`blaze::YIELDS_TO_INTERNAL`), decided
//! by the group's level, not by the lint's name.

use base_db::{Db, TargetKind};
use blaze_hash::HashSet;
use hir::{HasVisibility as _, HirDisplay as _};
use hir_def::{
    DefWithBodyId,
    hir::{BindingId, Expr, ExprId, LoopSource, Statement, UnaryOp},
    resolver::ValueNs,
};
use std::alloc::Global;

use super::{Finding, Lint, facts, general_facts};
use crate::check::Cx;

const LINT: Lint = Lint::DosHasherOnInput;

/// How many rounds a key's taint is followed through `let`s and `for` loops.
const ROUNDS: usize = 4;

/// How many steps a key's expression is followed down to the local it is made from.
const DEPTH: usize = 16;

/// The hashers of crates other than std with a seed fixed for every map, by crate.
const FIXED: &[(&str, &[&str])] = &[
    ("rustc_hash", &["FxBuildHasher", "FxSeededState"]),
    ("fxhash", &["FxBuildHasher"]),
    ("foldhash", &["FixedState"]),
    ("fnv", &["FnvBuildHasher"]),
    ("blaze_hash", &["FixedState", "KeyHasher"]),
];

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
    let db = cx.db();
    if !matches!(cx.krate().target_kind(db), TargetKind::Lib { is_proc_macro: false }) {
        return found;
    }
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def @ DefWithBodyId::FunctionId(f)) = DefWithBodyId::try_from(analysis.def()) else {
        return found;
    };
    let function = hir::Function::from(f);
    if !matches!(function.visibility(db), hir::Visibility::Public) {
        return found;
    }
    let body = analysis.body();
    let tainted = tainted(db, def, analysis);
    if tainted.is_empty() {
        return found;
    }
    let target = cx.krate().to_display_target(db);
    for (id, expr) in body.exprs() {
        let Expr::MethodCall { receiver, args, .. } = expr else { continue };
        let Some(callee) = general_facts::callee(db, analysis, id) else { continue };
        if !(callee.name == "insert"
            || callee.name == "entry"
            || callee.name.starts_with("get_or_insert"))
        {
            continue;
        }
        let Some(adt) = callee.self_adt else { continue };
        let at = match adt.name(db).as_str() {
            "HashMap" => 2,
            "HashSet" => 1,
            _ => continue,
        };
        if !(general_facts::std_adt(db, adt, "collections", &["HashMap", "HashSet"])
            || general_facts::crate_adt(db, adt, "hashbrown", &["HashMap", "HashSet"]))
        {
            continue;
        }
        let map = analysis.ty(analysis.infer().expr_ty(receiver)).strip_references();
        let Some(hasher) = map.type_arguments().nth(at) else { continue };
        if !fixed(db, &hasher) {
            continue;
        }
        let Some(&key) = args.first() else { continue };
        let Some(param) = root_local(db, def, analysis, key).filter(|it| tainted.contains(it))
        else {
            continue;
        };
        let Some(range) = analysis.expr_range(id) else { continue };
        let param = body[param].name.as_str().to_owned(); // alloc: a message's part
        let message = format!(
            "this `{}` keys a `{}` with a fixed-seed hasher `{}` by text from `{param}`, a parameter of `pub fn {}`",
            callee.name,
            adt.name(db).as_str(),
            hasher.display(db, target),
            function.name(db).as_str()
        );
        let note = "a caller that picks the keys can pick many that collide, since the hash function is the same in \
                    every process: each insert then walks one long chain (HashDoS)";
        facts::push(&mut found, LINT, range, message, Some(note.to_owned())); // alloc: a finding's note
    }
    found
}

/// Whether the hasher `s` has a seed fixed for every map.
fn fixed(db: &Db, s: &hir::Type<'_>) -> bool {
    let Some(adt) = s.as_adt() else { return false };
    if general_facts::std_adt(db, adt, "hash", &["BuildHasherDefault"]) {
        let Some(inner) = s.type_arguments().next().and_then(|it| it.as_adt()) else {
            return false;
        };
        let sip = general_facts::std_adt(
            db,
            inner,
            "hash",
            &["DefaultHasher", "SipHasher", "SipHasher13"],
        ) || general_facts::std_adt(db, inner, "collections", &["DefaultHasher"]);
        return !sip;
    }
    FIXED.iter().any(|(krate, names)| general_facts::crate_adt(db, adt, krate, names))
}

/// Whether `ty`, references peeled, is text or bytes: `str`, `String`, `[u8]`, `Vec<u8>`, or a
/// `Cow` of one of them.
fn is_text(db: &Db, krate: hir::Crate, ty: &hir::Type<'_>) -> bool {
    let ty = ty.strip_references();
    if ty.is_str() || ty.as_slice().is_some_and(|it| it.is_int_or_uint()) {
        return true;
    }
    let Some(adt) = ty.as_adt() else { return false };
    let item =
        |name: &str| krate.diagnostic_item_adt(db, &intern::Symbol::intern(name)) == Some(adt);
    if item("String") {
        return true;
    }
    let first = ty.type_arguments().next();
    if item("Vec") {
        return first.is_some_and(|it| it.is_int_or_uint());
    }
    if general_facts::std_adt(db, adt, "borrow", &["Cow"]) {
        return first.is_some_and(|it| is_text(db, krate, &it));
    }
    false
}

/// The locals of the body of `def` that hold text from a text parameter, as the module docs say.
fn tainted<'db>(
    db: &'db Db,
    def: DefWithBodyId,
    analysis: &hir::BodyAnalysis<'db>,
) -> HashSet<BindingId, Global> {
    // alloc: the tainted locals of one body
    let mut out = HashSet::new_in(Global);
    let body = analysis.body();
    let infer = analysis.infer();
    let krate = analysis.def().module(db).krate(db);
    for param in &body.params {
        for pat in [param.formal, param.user_written] {
            body.walk_bindings_in_pat(pat, |it| {
                if is_text(db, krate, &analysis.ty(infer.binding_ty(it))) {
                    out.insert(it);
                }
            });
        }
    }
    if out.is_empty() {
        return out;
    }
    for _ in 0..ROUNDS {
        // alloc: one round's new locals
        let mut new = Vec::new();
        for (id, expr) in body.exprs() {
            match expr {
                Expr::Block { statements, .. } => {
                    for statement in statements.iter() {
                        let Statement::Let { pat, initializer: Some(init), .. } = statement else {
                            continue;
                        };
                        if root_local(db, def, analysis, init).is_some_and(|it| out.contains(&it)) {
                            body.walk_bindings_in_pat(pat, |it| new.push(it));
                        }
                    }
                }
                Expr::Loop { body: inner, source: LoopSource::ForLoop, .. } => {
                    if let Some((head, pat)) = for_parts(analysis, id, inner)
                        && root_local(db, def, analysis, head).is_some_and(|it| out.contains(&it))
                    {
                        body.walk_bindings_in_pat(pat, |it| new.push(it));
                    }
                }
                _ => {}
            }
        }
        let before = out.len();
        out.extend(new);
        if out.len() == before {
            break;
        }
    }
    out
}

/// The iterated expression and the element pattern of the `for` loop whose desugared `loop` is
/// `loop_` with body `inner`.
fn for_parts(
    analysis: &hir::BodyAnalysis<'_>,
    loop_: ExprId,
    inner: ExprId,
) -> Option<(ExprId, hir_def::hir::PatId)> {
    let body = analysis.body();
    let Expr::Block { tail: Some(tail), .. } = body.expr(inner) else { return None };
    let Expr::Match { arms, .. } = body.expr(tail) else { return None };
    let pat = arms.get(1)?.pat;
    // The `match IntoIterator::into_iter(head) { iter => loop_ }` around the loop.
    let (outer, _) = body.exprs().find(|(_, it)| matches!(it, Expr::Match { arms, .. } if arms.first().is_some_and(|arm| arm.expr == loop_)))?;
    let Expr::Match { expr: iterator, .. } = body.expr(outer) else { return None };
    let Expr::Call { args, .. } = body.expr(iterator) else { return None };
    Some((*args.first()?, pat))
}

/// The local `expr` is made from: itself when it is a path to a local, else the receiver of a
/// method call, the first argument of a call, or what a reference, a dereference, a field or an
/// index is of, followed down at most [`DEPTH`] steps.
fn root_local(
    db: &Db,
    def: DefWithBodyId,
    analysis: &hir::BodyAnalysis<'_>,
    expr: ExprId,
) -> Option<BindingId> {
    let body = analysis.body();
    let mut at = expr;
    for _ in 0..DEPTH {
        at = match body.expr(at) {
            Expr::Path(_) => {
                return match general_facts::value_of(db, def, body, at)? {
                    ValueNs::LocalBinding(it) => Some(it),
                    _ => None,
                };
            }
            Expr::MethodCall { receiver, .. } => receiver,
            Expr::Call { args, .. } => *args.first()?,
            Expr::Ref { expr, .. }
            | Expr::UnaryOp { expr, op: UnaryOp::Deref }
            | Expr::Field { expr, .. } => expr,
            Expr::Index { base, .. } => base,
            _ => return None,
        };
    }
    None
}
