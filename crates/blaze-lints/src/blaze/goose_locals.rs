//! What the body lints drawn from Goose (`push_without_reserve`, `scratch_in_loop`,
//! `deep_drop_at_exit`) ask of a local: which `let`s of a block bind a plain local to a new
//! collection, what a call makes, how the body uses the local (only borrowed, grown), and the
//! parts of a desugared `for` loop.

use base_db::Db;
use hir_def::{
    DefWithBodyId,
    expr_store::Body,
    hir::{BindingId, Expr, ExprId, LoopSource, Pat, Statement},
    signatures::FunctionSignature,
};
use hir_ty::Adjust;

use super::{alloc::written_callee, facts, general_facts, hash_order_output::local_of};
use crate::check::Cx;

/// A collection a lint knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Coll {
    Vec,
    String,
    /// std's `HashMap` or `HashSet`, or hashbrown's.
    Map,
}

impl Coll {
    pub(super) fn say(self) -> &'static str {
        match self {
            Coll::Vec => "Vec",
            Coll::String => "String",
            Coll::Map => "HashMap",
        }
    }
}

/// The collection `adt` is, when it is one of [`Coll`]'s.
pub(super) fn coll(cx: &Cx<'_, '_>, adt: hir::Adt) -> Option<Coll> {
    let db = cx.db();
    let item =
        |name: &str| cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern(name)) == Some(adt);
    if item("Vec") {
        return Some(Coll::Vec);
    }
    if item("String") {
        return Some(Coll::String);
    }
    let std = |path: &[&str]| cx.std_def(path) == Some(hir::ModuleDef::Adt(adt));
    if std(&["std", "collections", "HashMap"])
        || std(&["std", "collections", "HashSet"])
        || general_facts::crate_adt(db, adt, "hashbrown", &["HashMap", "HashSet"])
    {
        return Some(Coll::Map);
    }
    None
}

/// What the call `init` makes: the collection and the constructor's name, when it is a
/// [`Coll`]'s `new`, `default` or `with_capacity` (also what `vec![]` expands to).
pub(super) fn made(
    cx: &Cx<'_, '_>,
    analysis: &hir::BodyAnalysis<'_>,
    init: ExprId,
) -> Option<(Coll, &'static str)> {
    let db = cx.db();
    let body = analysis.body();
    if !matches!(body.expr(init), Expr::Call { .. }) {
        return None;
    }
    let (f, _) = written_callee(analysis.infer(), body, init)?;
    let name = match FunctionSignature::of(db, f).name.as_str() {
        "new" => "new",
        "default" => "default",
        "with_capacity" => "with_capacity",
        _ => return None,
    };
    let adt = analysis.ty(analysis.infer().expr_ty(init)).as_adt()?;
    Some((coll(cx, adt)?, name))
}

/// A `let` of a block that binds a plain local (`x` or `mut x`, no subpattern) to an
/// initializer, with no `else`.
pub(super) struct Let {
    pub(super) local: BindingId,
    pub(super) init: ExprId,
    /// Its index among the block's statements.
    pub(super) at: usize,
}

/// The plain `let`s of the block `block`.
pub(super) fn lets(body: &Body, block: ExprId) -> Vec<Let> {
    // alloc: the lets of one block, a few
    let mut out = Vec::new();
    let Expr::Block { statements, .. } = body.expr(block) else { return out };
    for (at, statement) in statements.iter().enumerate() {
        let Statement::Let { pat, initializer: Some(init), else_branch: None, .. } = statement
        else {
            continue;
        };
        let Pat::Bind { id, subpat: None } = body.pat(pat) else { continue };
        out.push(Let { local: id, init, at });
    }
    out
}

/// How a body uses one local.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LocalUses {
    /// Every use borrows it: a method called on it through an autoref, `&x`, `&mut x`, `x[i]`.
    pub(super) only_borrowed: bool,
    /// Some use grows it: `push`, `push_str`, `insert`, `extend`, `extend_from_slice`,
    /// `append`, `entry`, `resize`.
    pub(super) grown: bool,
    /// How many uses there are.
    pub(super) count: u32,
}

/// How the body of `def` uses `local`.
pub(super) fn uses(
    db: &Db,
    def: DefWithBodyId,
    analysis: &hir::BodyAnalysis<'_>,
    parents: &facts::Parents,
    local: BindingId,
) -> LocalUses {
    let body = analysis.body();
    let infer = analysis.infer();
    let mut out = LocalUses { only_borrowed: true, grown: false, count: 0 };
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::Path(_)) || local_of(db, def, body, id) != Some(local) {
            continue;
        }
        out.count += 1;
        let borrowed = match parents.of_expr(id).map(|p| (p, body.expr(p))) {
            Some((call, Expr::MethodCall { receiver, .. })) if receiver == id => {
                if let Some((f, _)) = written_callee(infer, body, call)
                    && matches!(
                        FunctionSignature::of(db, f).name.as_str(),
                        "push"
                            | "push_str"
                            | "insert"
                            | "extend"
                            | "extend_from_slice"
                            | "append"
                            | "entry"
                            | "resize"
                    )
                {
                    out.grown = true;
                }
                infer
                    .expr_adjustment(id)
                    .is_some_and(|it| it.iter().any(|a| matches!(a.kind, Adjust::Borrow(_))))
            }
            Some((_, Expr::Ref { .. })) => true,
            Some((_, Expr::Index { base, .. })) => base == id,
            _ => false,
        };
        out.only_borrowed &= borrowed;
    }
    out
}

/// Whether some expression under `root` names `local`.
pub(super) fn mentions(
    db: &Db,
    def: DefWithBodyId,
    body: &Body,
    root: ExprId,
    local: BindingId,
) -> bool {
    let mut found = false;
    let walked = general_facts::walk(body, root, |it| {
        found |=
            matches!(body.expr(it), Expr::Path(_)) && local_of(db, def, body, it) == Some(local);
    });
    // Out of fuel: taken as naming it, so the lint asking stays quiet.
    found || walked == general_facts::Walked::OutOfFuel
}

/// A desugared `for` loop: `match IntoIterator::into_iter(head) { mut iter => loop { match
/// Iterator::next(&mut iter) { None => break, Some(pat) => body } } }`.
pub(super) struct For {
    /// The `match` the loop is, as a statement writes it.
    pub(super) at: ExprId,
    /// The `into_iter` call, whose type is the iterator.
    pub(super) iterator: ExprId,
    /// What is written after `in`.
    pub(super) head: ExprId,
    /// The loop's own body (the `Some` arm's).
    pub(super) body: ExprId,
}

/// The `for` loop `expr` is, when it is one.
pub(super) fn for_loop(body: &Body, expr: ExprId) -> Option<For> {
    let Expr::Match { expr: iterator, arms } = body.expr(expr) else { return None };
    let [arm] = arms else { return None };
    let Expr::Loop { body: inner, source: LoopSource::ForLoop, .. } = body.expr(arm.expr) else {
        return None;
    };
    let Expr::Call { args: [head], .. } = body.expr(iterator) else { return None };
    let Expr::Block { tail: Some(tail), .. } = body.expr(inner) else { return None };
    let Expr::Match { arms, .. } = body.expr(tail) else { return None };
    let some = arms.iter().find(|it| matches!(body.pat(it.pat), Pat::TupleStruct { .. }))?;
    Some(For { at: expr, iterator, head: *head, body: some.expr })
}
