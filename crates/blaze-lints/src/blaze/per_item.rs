//! What `syscall_in_loop` and `query_in_loop` ask of a call: whether it runs once per element
//! of a collection, and which locals hold an element there.
//!
//! A call runs once per element when it is in the body of a `for` loop, or in a closure given
//! to a method of an iterator trait: core's `Iterator` (`map`, `filter`, `for_each`, ..) or
//! rayon's `ParallelIterator` and `IndexedParallelIterator`, the method resolved, never matched
//! by name. A closure given to anything else (a callback, `Option::map`, a lazy cell) may run
//! once, and ends the walk up: the call is not per element. A `while` or `loop` is not counted:
//! it waits or serves, it does not walk a collection.

use base_db::Db;
use hir::AsAssocItem as _;
use hir_def::{
    DefWithBodyId, FunctionId,
    expr_store::{Body, scope::ExprScopes},
    hir::{BindingId, Expr, ExprId, LoopSource, Statement},
    resolver::{ValueNs, resolver_for_scope},
    signatures::FunctionSignature,
};
use syntax::SyntaxKind;

use super::{
    alloc::written_callee,
    facts::{self, Parents},
};
use crate::check::Cx;

/// How many expressions up a call is followed to its loop or closure.
const UP: usize = 64;

/// The traits of crates other than core whose methods run a closure once per element.
const PER_ELEMENT_TRAITS: &[(&str, &str)] =
    &[("rayon", "ParallelIterator"), ("rayon", "IndexedParallelIterator")];

/// What a call runs once per element of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Over {
    /// The body of the `for` loop `loop_` (its desugared `loop`).
    For { loop_: ExprId },
    /// The closure `closure`, given to the iterator method `method`.
    Closure { closure: ExprId, method: FunctionId },
}

impl Over {
    /// How a message names it: "this `for` loop", "the closure given to `map`".
    pub(super) fn say(self, db: &Db) -> String {
        match self {
            Over::For { .. } => "this `for` loop".to_owned(), // alloc: a message's part
            Over::Closure { method, .. } => {
                format!(
                    "the closure given to `{}`",
                    FunctionSignature::of(db, method).name.as_str()
                )
            }
        }
    }

    /// The expression whose evaluation is per element: the loop, or the closure.
    pub(super) fn expr(self) -> ExprId {
        match self {
            Over::For { loop_ } => loop_,
            Over::Closure { closure, .. } => closure,
        }
    }
}

/// The innermost `for` loop or iterator closure that `call` runs once per element of.
pub(super) fn per_item(
    cx: &Cx<'_, '_>,
    analysis: &hir::BodyAnalysis<'_>,
    parents: &Parents,
    call: ExprId,
) -> Option<Over> {
    let body = analysis.body();
    for parent in parents.up(call, UP) {
        match body.expr(parent) {
            Expr::Loop { source: LoopSource::ForLoop, .. } => {
                return Some(Over::For { loop_: parent });
            }
            Expr::Closure { .. } => {
                let method = parents
                    .of_expr(parent)
                    .and_then(|at| iterator_method(cx, analysis, at, parent))?;
                return Some(Over::Closure { closure: parent, method });
            }
            _ => {}
        }
    }
    None
}

/// The iterator method the call `at` is, when `closure` is one of its arguments.
fn iterator_method(
    cx: &Cx<'_, '_>,
    analysis: &hir::BodyAnalysis<'_>,
    at: ExprId,
    closure: ExprId,
) -> Option<FunctionId> {
    let body = analysis.body();
    let args = match body.expr(at) {
        Expr::MethodCall { args, .. } | Expr::Call { args, .. } => args,
        _ => return None,
    };
    if !args.contains(&closure) {
        return None;
    }
    let (f, _) = written_callee(analysis.infer(), body, at)?;
    let db = cx.db();
    let hir::AssocItemContainer::Trait(trait_) =
        hir::Function::from(f).as_assoc_item(db)?.container(db)
    else {
        return None;
    };
    let iterator = cx.lang_trait(hir::LangItem::Iterator) == Some(trait_);
    let krate = trait_.module(db).krate(db);
    let name = trait_.name(db);
    let parallel = PER_ELEMENT_TRAITS
        .iter()
        .any(|&(of, it)| name.as_str() == it && facts::crate_is(db, krate, of));
    (iterator || parallel).then_some(f)
}

/// The locals that hold an element, or something made from one, inside `over`: the closure's
/// parameters, and every binding of a pattern inside it (the `for` loop's `Some(pat)` arm,
/// `let`s, `match` arms, inner closures' parameters).
pub(super) fn element_bindings(body: &Body, over: Over) -> Vec<BindingId> {
    // alloc: the bindings of one loop body, a few
    let mut out = Vec::new();
    let mut stack = vec![over.expr()]; // alloc: the expressions of one loop body
    while let Some(id) = stack.pop() {
        match body.expr(id) {
            Expr::Closure { args, .. } => {
                for &pat in args {
                    body.walk_bindings_in_pat(pat, |it| out.push(it));
                }
            }
            Expr::Match { arms, .. } => {
                for arm in arms {
                    body.walk_bindings_in_pat(arm.pat, |it| out.push(it));
                }
            }
            Expr::Let { pat, .. } => body.walk_bindings_in_pat(pat, |it| out.push(it)),
            Expr::Block { statements, .. } => {
                for statement in statements {
                    if let Statement::Let { pat, .. } = statement {
                        body.walk_bindings_in_pat(pat, |it| out.push(it));
                    }
                }
            }
            _ => {}
        }
        body.walk_child_exprs(id, |child| stack.push(child));
    }
    out
}

/// Whether `expr`, or an expression inside it, reads one of `bindings`.
pub(super) fn mentions(
    db: &Db,
    def: DefWithBodyId,
    body: &Body,
    expr: ExprId,
    bindings: &[BindingId],
) -> bool {
    let mut stack = vec![expr]; // alloc: the expressions of one argument
    while let Some(id) = stack.pop() {
        if matches!(body.expr(id), Expr::Path(_))
            && local_of(db, def, body, id).is_some_and(|it| bindings.contains(&it))
        {
            return true;
        }
        body.walk_child_exprs(id, |child| stack.push(child));
    }
    false
}

/// The local the path expression `expr` of `def`'s body names, if it names one.
pub(super) fn local_of(
    db: &Db,
    def: DefWithBodyId,
    body: &Body,
    expr: ExprId,
) -> Option<BindingId> {
    match value_of(db, def, body, expr)? {
        ValueNs::LocalBinding(it) => Some(it),
        _ => None,
    }
}

/// The value the path expression `expr` of `def`'s body names, if it is one.
pub(super) fn value_of(db: &Db, def: DefWithBodyId, body: &Body, expr: ExprId) -> Option<ValueNs> {
    let Expr::Path(path) = body.expr(expr) else { return None };
    let scope = ExprScopes::of(db, def).scope_for(expr);
    resolver_for_scope(db, def, scope).resolve_path_in_value_ns_fully(
        db,
        path,
        body.expr_path_hygiene(expr),
    )
}

/// Whether `id` is a call written as one in the body's own file: not a `for` loop's desugared
/// `next`, not a call a macro wrote.
pub(super) fn written_call(analysis: &hir::BodyAnalysis<'_>, id: ExprId) -> bool {
    let file = analysis.tree().1;
    analysis.source_map().expr_syntax(id).is_ok_and(|it| {
        it.file_id == file
            && matches!(it.value.kind(), SyntaxKind::CALL_EXPR | SyntaxKind::METHOD_CALL_EXPR)
    })
}
