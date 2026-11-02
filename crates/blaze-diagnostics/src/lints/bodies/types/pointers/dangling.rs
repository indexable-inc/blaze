//! `dangling_pointers_from_temporaries`: a method marked `#[rustc_as_ptr]` (`as_ptr`,
//! `as_mut_ptr`) called on a temporary that owns its allocation (`String`, `Vec`, `CString`,
//! an array, `Box<[T]>`, ..) outside a call's arguments, so the pointer outlives it.
//! `dangling_pointers_from_locals`: a function or closure returning a raw pointer (to a type
//! that is not a 1-ZST) that returns `&local`, through casts and the `let`s it was bound by.
//!
//! Each function and closure is searched as rustc's `check_fn` searches it: a closure's body
//! not as part of the function's but on its own, when the walk reaches it.
//!
//! Follows rustc's `compiler/rustc_lint/src/dangling.rs` and `compiler/rustc_lint/src/lints.rs`
//! (`DanglingPointersFromTemporaries`, `DanglingPointersFromLocals`) (MIT or Apache-2.0).

use blaze_hash::HashMap;
use hir::InFile;
use hir_def::{
    AdtId, AttrDefId, CallableDefId, DefWithBodyId, FunctionId, HasModule, ItemContainerId, Lookup,
    hir::{BinaryOp, BindingId, ClosureKind, Expr, ExprId, Statement, type_ref::TypeRef},
    resolver::ValueNs,
    signatures::{FnFlags, FunctionSignature},
    src::HasSource,
};
use hir_ty::db::HirDatabase;
use hir_ty::next_solver::{DbInterner, Ty, TyKind};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind as _};
use syntax::view::{self as ast, AstNode};

use super::{Inits, has_attr, layout, peel_casts, span, ty_text};
use crate::{
    lints::{
        bodies::{BodyCx, ExprKind, ExprKinds, Hooks, LatePass, LintSet, lint_set, types::util},
        registry::LintId,
    },
    lower::{Found, Phase},
    model::{Diag, Span},
};

const DESTRUCTORS: &str = "for more information, see <https://doc.rust-lang.org/reference/destructors.html>";
const SAFE: &str = "a dangling pointer is safe, but dereferencing one is undefined behavior";

const TEMPORARIES: &str = "dangling_pointers_from_temporaries";
const LOCALS: &str = "dangling_pointers_from_locals";

/// The searcher is a walk of its own, as rustc's `check_fn` makes one: the lints of a function
/// come before those of the nodes in it, and the shared walk reaches them after. Each half runs
/// only when its lint may fire in the crate.
#[derive(Default)]
pub(crate) struct DanglingPointers {
    inits: Inits,
    /// Whether `TEMPORARIES` and `LOCALS` may fire, found at the crate's first body.
    runs: Option<(bool, bool)>,
}

impl DanglingPointers {
    fn runs(&mut self, b: &BodyCx<'_, '_, '_>) -> (bool, bool) {
        *self.runs.get_or_insert_with(|| {
            (b.cx.lint_may_fire(LintId::named(TEMPORARIES)), b.cx.lint_may_fire(LintId::named(LOCALS)))
        })
    }
}

impl LatePass for DanglingPointers {
    const LINTS: LintSet = lint_set([TEMPORARIES, LOCALS]);
    const HOOKS: Hooks = Hooks::BODY;
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Closure]);

    fn check_body(&mut self, b: &BodyCx<'_, '_, '_>, out: &mut Vec<Found>) {
        let DefWithBodyId::FunctionId(function) = b.owner else { return };
        let (temporaries, locals) = self.runs(b);
        let root = b.body.root_expr();
        if temporaries {
            Searcher { b, inside_call_args: false, out }.expr(root);
        }
        if !locals {
            return;
        }
        let sig = FunctionSignature::of(b.db(), function);
        if sig.is_async() || sig.flags.contains(FnFlags::GEN) {
            return;
        }
        let Some(fn_ret) = function_return(b, function) else { return };
        let inits = self.inits.of(b);
        Locals { b, inits, fn_ret: &fn_ret, owner: None }.check(root, out);
    }

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        let Expr::Closure { body, closure_kind, .. } = b.store().expr(expr) else { return };
        let (temporaries, locals) = self.runs(b);
        if temporaries {
            Searcher { b, inside_call_args: false, out }.expr(body);
        }
        if !locals || !matches!(closure_kind, ClosureKind::Closure) {
            return;
        }
        let Some(fn_ret) = closure_return(b, expr) else { return };
        let inits = self.inits.of(b);
        Locals { b, inits, fn_ret: &fn_ret, owner: Some(expr) }.check(body, out);
    }
}

/// rustc's `DanglingPointerSearcher`: every expression of a body but those in a call's
/// arguments (where the temporary lives until the call returns), but a block's statements
/// even there; a closure's or an inline const's body is not of it.
struct Searcher<'s, 'a, 'db> {
    b: &'s BodyCx<'s, 'a, 'db>,
    inside_call_args: bool,
    out: &'s mut Vec<Found>,
}

impl Searcher<'_, '_, '_> {
    fn expr(&mut self, expr: ExprId) {
        if !self.inside_call_args {
            temporary(self.b, expr, self.out);
        }
        let store = self.b.store();
        match store.expr(expr) {
            Expr::Call { callee: lhs, args } | Expr::MethodCall { receiver: lhs, args, .. } => {
                self.expr(lhs);
                self.inside(true, |this| args.iter().for_each(|it| this.expr(*it)));
            }
            Expr::Block { statements, tail, .. } => {
                self.inside(false, |this| {
                    for statement in statements {
                        match statement {
                            Statement::Let { initializer, else_branch, .. } => {
                                initializer.into_iter().chain(else_branch).for_each(|it| this.expr(it));
                            }
                            Statement::Expr { expr, .. } => this.expr(expr),
                            Statement::Item(_) => {}
                        }
                    }
                });
                if let Some(tail) = tail {
                    self.expr(tail);
                }
            }
            Expr::Closure { .. } | Expr::Const(_) => {}
            Expr::Assignment { target, value } => {
                self.expr(value);
                store.walk_exprs_in_pat(target, |it| self.expr(it));
            }
            Expr::BinaryOp { lhs, rhs, op: Some(BinaryOp::Assignment { .. }) } => {
                self.expr(rhs);
                self.expr(lhs);
            }
            _ => store.walk_child_exprs_without_pats(expr, |it| stdx::ensure_sufficient_stack(|| self.expr(it))),
        }
    }

    fn inside(&mut self, inside_call_args: bool, f: impl FnOnce(&mut Self)) {
        let old = std::mem::replace(&mut self.inside_call_args, inside_call_args);
        f(self);
        self.inside_call_args = old;
    }
}

/// rustc's `lint_expr`: `expr` calls a `#[rustc_as_ptr]` method on a temporary that owns an
/// allocation.
fn temporary(b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
    let Expr::MethodCall { receiver, .. } = b.store().expr(expr) else { return };
    if !is_temporary_rvalue(b, receiver) || !owns_allocation(b, util::ty(b, receiver)) {
        return;
    }
    let Some(method) = util::method_fn(b, expr) else { return };
    if !has_attr(b.db(), method, "rustc_as_ptr") {
        return;
    }
    let Some(call) = util::syntax(b, expr) else { return };
    // clone: a node handle, a reference count, once the call is of a `#[rustc_as_ptr]` method on a temporary
    let Some(name) = ast::MethodCallExpr::cast(call.value).and_then(|it| it.name_ref()) else { return };
    let ptr_span = b.cx.span(call.with_value(name.syntax().text_range()));
    let (Some(at), Some(temporary_span), Some(ty)) = (b.expr_node(expr), span(b, receiver), ty_text(b, receiver))
    else {
        return;
    };
    // alloc: the lint's text, for a lint found
    let callee = name.text().to_string();
    let mut diag = Diag::lint(
        TEMPORARIES,
        // alloc: the lint's text, for a lint found
        format!("this creates a dangling pointer because temporary `{ty}` is dropped at end of statement"),
        // clone: a lint's span, for a lint found
        ptr_span.clone(),
    );
    diag.span_label(ptr_span, "pointer created here");
    // alloc: the lint's text, for a lint found
    diag.span_label(temporary_span, format!("this `{ty}` is dropped at end of statement"));
    // alloc: the lint's text, for a lint found
    diag.help(format!("bind the `{ty}` to a variable such that it outlives the pointer returned by `{callee}`"));
    diag.note(SAFE);
    diag.note("returning a pointer to a local variable will always result in a dangling pointer");
    diag.note(DESTRUCTORS);
    out.push(b.lint(diag, Phase::Late, at));
}

/// rustc's `is_temporary_rvalue`, of the expressions rustc's HIR has: a call, a binary
/// operation, a block, `if`, `match`, a loop, `.await` (a `match` in rustc's HIR) or a struct
/// expression.
fn is_temporary_rvalue(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> bool {
    match b.store().expr(expr) {
        Expr::Call { .. } | Expr::MethodCall { .. } | Expr::RecordLit { .. } => true,
        Expr::BinaryOp { op, .. } => !matches!(op, None | Some(BinaryOp::Assignment { .. })),
        Expr::If { .. } | Expr::Loop { .. } | Expr::Match { .. } | Expr::Block { .. } | Expr::Await { .. } => true,
        Expr::Missing
        | Expr::Path(_)
        | Expr::Let { .. }
        | Expr::Const(_)
        | Expr::Continue { .. }
        | Expr::Break { .. }
        | Expr::Return { .. }
        | Expr::Become { .. }
        | Expr::Yield { .. }
        | Expr::Yeet { .. }
        | Expr::Field { .. }
        | Expr::Cast { .. }
        | Expr::Ref { .. }
        | Expr::UnaryOp { .. }
        | Expr::Assignment { .. }
        | Expr::Index { .. }
        | Expr::Closure { .. }
        | Expr::Tuple { .. }
        | Expr::Array(_)
        | Expr::Literal(_)
        | Expr::Underscore
        | Expr::OffsetOf(_)
        | Expr::InlineAsm(_)
        | Expr::IncludeBytes
        | Expr::UnsafeBinderCast { .. } => false,
    }
}

/// rustc's `owns_allocation`: an array, `String`, `Vec`, `CString`, `MaybeUninit`, `Cell`,
/// `UnsafeCell`, `SyncUnsafeCell`, or a `Box` of a slice, `str`, `CStr` or one of those.
fn owns_allocation<'db>(b: &BodyCx<'_, '_, 'db>, ty: Ty<'db>) -> bool {
    if ty.is_array() {
        return true;
    }
    let lang = DbInterner::new_with(b.db(), b.owner.krate(b.db())).lang_items();
    if let Some(inner) = ty.boxed_ty() {
        let is_cstr = inner.as_adt().is_some_and(|(adt, _)| lang.CStr.is_some_and(|it| adt == AdtId::StructId(it)));
        return inner.is_slice() || inner.is_str() || is_cstr || owns_allocation(b, inner);
    }
    let Some((adt, _)) = ty.as_adt() else { return false };
    let lang_owned = [lang.String.map(AdtId::StructId), lang.MaybeUninit.map(AdtId::UnionId), lang.UnsafeCell.map(AdtId::StructId)];
    if lang_owned.contains(&Some(adt)) {
        return true;
    }
    let name = util::diagnostic_item(b.db(), AttrDefId::AdtId(adt));
    let items = util::items();
    name.is_some_and(|it| [items.cstring_type, items.Vec, items.Cell, items.SyncUnsafeCell].contains(&it))
}

/// What a function or closure returning a raw pointer is linted with: rustc's
/// `DanglingPointerLocalContext`.
struct FnRet {
    ret_ty: String,
    ret_ty_span: Span,
    fn_kind: &'static str,
}

/// The return of `function` when it is written as a raw pointer to a type that is not a 1-ZST.
fn function_return(b: &BodyCx<'_, '_, '_>, function: FunctionId) -> Option<FnRet> {
    let db = b.db();
    let sig = FunctionSignature::of(db, function);
    let ret = sig.ret_type?;
    if !matches!(sig.store.type_ref(ret), TypeRef::RawPtr(..)) {
        return None;
    }
    let interner = DbInterner::new_with(db, function.krate(db));
    let poly = db.callable_item_signature(CallableDefId::FunctionId(function)).instantiate_identity().skip_norm_wip();
    let output = interner.instantiate_bound_regions_with_erased(poly).output();
    pointee_not_1zst(b, output)?;
    let source = function.lookup(db).source_view(db);
    let ret_ty_node = source.value.ret_type()?.ty()?;
    let ret_ty_span = b.cx.span(InFile::new(source.file_id, ret_ty_node.syntax().text_range()));
    let ret_ty = b.cx.ty(&hir::Function::from(function).ret_type(db));
    let fn_kind = match function.lookup(db).container {
        ItemContainerId::ImplId(_) | ItemContainerId::TraitId(_) => "method",
        ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => "function",
    };
    Some(FnRet { ret_ty, ret_ty_span, fn_kind })
}

/// The return of the closure `closure` when it is written as a raw pointer to a type that is
/// not a 1-ZST.
fn closure_return(b: &BodyCx<'_, '_, '_>, closure: ExprId) -> Option<FnRet> {
    let Expr::Closure { ret_type: Some(ret), .. } = b.store().expr(closure) else { return None };
    if !matches!(b.store().type_ref(ret), TypeRef::RawPtr(..)) {
        return None;
    }
    let ty = super::type_info(b, closure)?.original;
    let ret = ty.as_callable(b.db())?.return_type();
    let TyKind::Closure(_, args) = util::ty(b, closure).kind() else { return None };
    let interner = DbInterner::new_with(b.db(), b.owner.krate(b.db()));
    let output = interner.instantiate_bound_regions_with_erased(args.as_closure().sig()).output();
    pointee_not_1zst(b, output)?;
    let node = util::syntax(b, closure)?;
    // clone: a node handle, a reference count, once the closure returns a raw pointer
    let ret_ty_node = ast::ClosureExpr::cast(node.value)?.ret_type()?.ty()?;
    let ret_ty_span = b.cx.span(node.with_value(ret_ty_node.syntax().text_range()));
    Some(FnRet { ret_ty: b.cx.ty(&ret), ret_ty_span, fn_kind: "closure" })
}

/// `Some` when `ty` is a raw pointer whose pointee's layout is known and not a 1-ZST.
fn pointee_not_1zst<'db>(b: &BodyCx<'_, '_, 'db>, ty: Ty<'db>) -> Option<()> {
    let TyKind::RawPtr(pointee, _) = ty.kind() else { return None };
    (!layout(b, pointee)?.is_1zst).then_some(())
}

/// The returns of one function or closure that `lint_addr_of_local` checks: each `return`'s
/// value, then the tail of its body's innermost block.
struct Locals<'s, 'a, 'db> {
    b: &'s BodyCx<'s, 'a, 'db>,
    inits: &'s HashMap<BindingId, ExprId>,
    fn_ret: &'s FnRet,
    /// The closure whose body it is, or `None` for the function's.
    owner: Option<ExprId>,
}

impl Locals<'_, '_, '_> {
    fn check(&self, body: ExprId, out: &mut Vec<Found>) {
        self.returns(body, out);
        let store = self.b.store();
        let Expr::Block { label: None, .. } = store.expr(body) else { return };
        let mut tail = body;
        while let Expr::Block { tail: inner, .. } = store.expr(tail) {
            let Some(inner) = inner else { return };
            tail = inner;
        }
        self.addr_of_local(tail, out);
    }

    /// rustc's `DanglingPointerReturnSearcher`: the `return`s in `expr`, not in a closure's or
    /// an inline const's body.
    fn returns(&self, expr: ExprId, out: &mut Vec<Found>) {
        let store = self.b.store();
        match store.expr(expr) {
            Expr::Return { expr: Some(value) } => self.addr_of_local(value, out),
            Expr::Closure { .. } | Expr::Const(_) => return,
            _ => {}
        }
        match store.expr(expr) {
            Expr::Assignment { target, value } => {
                self.returns(value, out);
                store.walk_exprs_in_pat(target, |it| self.returns(it, out));
            }
            Expr::BinaryOp { lhs, rhs, op: Some(BinaryOp::Assignment { .. }) } => {
                self.returns(rhs, out);
                self.returns(lhs, out);
            }
            _ => store.walk_child_exprs_without_pats(expr, |it| stdx::ensure_sufficient_stack(|| self.returns(it, out))),
        }
    }

    /// rustc's `lint_addr_of_local`: `expr` is, through casts and `let`s, `&local` of a local
    /// of this function or closure.
    fn addr_of_local(&self, expr: ExprId, out: &mut Vec<Found>) {
        let b = self.b;
        let store = b.store();
        let inner = peel_casts(b, self.inits, expr);
        let Expr::Ref { expr: inner_of, .. } = store.expr(inner) else { return };
        let local = util::peel_blocks(b, inner_of);
        let Expr::Path(_) = store.expr(local) else { return };
        let Some(ValueNs::LocalBinding(from)) = util::path_value(b, local) else { return };
        if store.binding_owner(from) != self.owner {
            return;
        }
        let (Some(at), Some(expr_span), Some(local_var)) = (b.expr_node(expr), span(b, expr), b.binding_node(from)) else {
            return;
        };
        // alloc: the lint's text, for a lint found
        let name = store[from].name.as_str().to_owned();
        let FnRet { ret_ty, ret_ty_span, fn_kind } = self.fn_ret;
        let mut diag = Diag::lint(
            LOCALS,
            // alloc: the lint's text, for a lint found
            format!("{fn_kind} returns a dangling pointer to dropped local variable `{name}`"),
            expr_span,
        );
        // alloc: the lint's text, for a lint found
        diag.span_label(ret_ty_span.clone(), format!("return type is `{ret_ty}`"));
        diag.span_label(
            b.cx.span_of(local_var),
            // alloc: the lint's text, for a lint found
            format!("local variable `{name}` is dropped at the end of the {fn_kind}"),
        );
        if inner != expr
            && let Some(created_at) = span(b, inner)
        {
            diag.span_label(created_at, "dangling pointer created here");
        }
        diag.note(SAFE);
        diag.note(DESTRUCTORS);
        out.push(b.lint(diag, Phase::Late, at));
    }
}
