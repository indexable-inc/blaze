//! Lowering of nightly's contracts (`#[core::contracts::requires]`, `#[core::contracts::ensures]`),
//! as rustc's `rustc_ast_lowering/src/contract.rs` does it.
//!
//! hir-expand's builtin attributes leave each clause as a marker statement at the start of the
//! function's body (`#[rustc_contract_requires] { DECLS; PRECOND };`, then
//! `#[rustc_contract_ensures] { ENSURES };`), in place of the clauses rustc's expander injects
//! into the signature. A body with both becomes
//!
//! ```ignore (pseudo-rust)
//! {
//!     let __ensures_checker = if CC {
//!         DECLS;
//!         contract_check_requires(|| PRECOND);
//!         Some(contract_build_check_ensures({ ENSURES }))
//!     } else {
//!         None
//!     };
//!     { let __ret = { BODY }; if CC { contract_check_ensures(__ensures_checker, __ret) } else { __ret } }
//! }
//! ```
//!
//! with `CC` the literal `cfg(contract_checks)` (rustc's `sess.contract_checks()`,
//! `-Zcontract-checks`), so the clauses are type-checked even when they are not run. Every
//! `return e` and every `?` that returns from the function (not from a closure, an async block or
//! a const) returns `e` through the same check. With only a `requires`, the body is
//! `{ if CC { DECLS; contract_check_requires(|| PRECOND); } BODY }`.

use cfg::CfgAtom;
use intern::sym;
use syntax::view::{self as ast, HasAttrs};

use crate::{
    expr_store::{ExprPtr, HygieneId, lower::ExprCollector},
    hir::{
        BindingAnnotation, CaptureBy, ClosureKind, Expr, ExprId, Literal, Pat, Statement, Unsafe,
    },
};

use super::Awaitable;

/// The ensures check `return`s go through while the function's own body is lowered: the name
/// of the `__ensures_checker` binding and the crate's `CC`.
#[derive(Clone)]
pub(super) struct Ensures {
    checker: hir_expand::name::Name,
    checked: bool,
}

/// The marker statements of a function's body.
struct Markers<'t> {
    requires: Option<ast::BlockExpr<'t>>,
    ensures: Option<ast::BlockExpr<'t>>,
    /// How many of the body's leading statements they are.
    count: usize,
}

/// The marker block of `stmt`, if it is one, and whether it is an ensures.
fn marker(stmt: ast::Stmt<'_>) -> Option<(ast::BlockExpr<'_>, bool)> {
    let ast::Stmt::ExprStmt(stmt) = stmt else { return None };
    let Some(ast::Expr::BlockExpr(block)) = stmt.expr() else { return None };
    block.attrs().find_map(|attr| match attr.simple_name()? {
        "rustc_contract_requires" => Some((block, false)),
        "rustc_contract_ensures" => Some((block, true)),
        _ => None,
    })
}

fn markers(body: ast::BlockExpr<'_>) -> Option<Markers<'_>> {
    let mut found = Markers { requires: None, ensures: None, count: 0 };
    for stmt in body.statements() {
        // The expander puts at most one requires, then at most one ensures.
        match marker(stmt) {
            Some((block, false)) if found.requires.is_none() && found.ensures.is_none() => {
                found.requires = Some(block)
            }
            Some((block, true)) if found.ensures.is_none() => found.ensures = Some(block),
            _ => break,
        }
        found.count += 1;
    }
    (found.count > 0).then_some(found)
}

impl<'db> ExprCollector<'db> {
    /// The body `expr` of a function, its contract clauses desugared.
    pub(super) fn collect_fn_body(&mut self, expr: Option<ast::Expr<'_>>) -> ExprId {
        let Some(ast::Expr::BlockExpr(body)) = expr else { return self.collect_expr_opt(expr) };
        let Some(markers) = markers(body) else { return self.collect_expr_opt(expr) };
        let checked = self.cfg_options.check_atom(&CfgAtom::Flag(sym::contract_checks));
        let ptr = ExprPtr::of(ast::Expr::BlockExpr(body));

        let Some(ensures) = markers.ensures else {
            // `{ if CC { DECLS; contract_check_requires(|| PRECOND); } BODY }`
            let requires = markers.requires.expect("a marker was found");
            let then_branch = self.contract_requires_block(requires, None);
            let condition = self.store.alloc_literal_expr(Literal::Bool(checked));
            let check = self.alloc_expr_desugared_with_ptr(
                Expr::If { condition, then_branch, else_branch: None },
                ptr,
            );
            let inner = self.collect_contract_body(body, markers.count);
            let mark = self.store.list_mark();
            self.store.push_list_statement(Statement::Expr { expr: check, has_semi: true });
            return self.with_lists(mark, |this, lists| {
                this.alloc_expr_desugared_with_ptr(
                    Expr::Block {
                        id: None,
                        statements: lists.statements_since(mark),
                        tail: Some(inner),
                        label: None,
                        unsafe_: Unsafe::No,
                    },
                    ptr,
                )
            });
        };

        // `let __ensures_checker = if CC { DECLS; check_requires(|| P); Some(build(ENS)) } else { None };`
        let then_branch = match markers.requires {
            Some(requires) => self.contract_requires_block(requires, Some(ensures)),
            None => {
                let some = self.contract_some_checker(ensures);
                self.alloc_expr_desugared_with_ptr(
                    Expr::Block {
                        id: None,
                        statements: crate::hir::Statements::EMPTY,
                        tail: Some(some),
                        label: None,
                        unsafe_: Unsafe::No,
                    },
                    ptr,
                )
            }
        };
        let none = self.lang_path(self.lang_items().OptionNone);
        let none = self.alloc_opt_path_expr(none);
        let else_branch = self.alloc_expr_desugared_with_ptr(
            Expr::Block {
                id: None,
                statements: crate::hir::Statements::EMPTY,
                tail: Some(none),
                label: None,
                unsafe_: Unsafe::No,
            },
            ptr,
        );
        let condition = self.store.alloc_literal_expr(Literal::Bool(checked));
        let checker_value = self.alloc_expr_desugared_with_ptr(
            Expr::If { condition, then_branch, else_branch: Some(else_branch) },
            ptr,
        );
        let checker = self.generate_new_name();
        // clone: a name, its symbol's count
        let checker_binding =
            self.alloc_binding(checker.clone(), BindingAnnotation::Unannotated, HygieneId::ROOT);
        let checker_pat = self.alloc_pat_desugared(Pat::Bind { id: checker_binding, subpat: None });
        self.add_definition_to_binding(checker_binding, checker_pat);

        // The body, its returns checked, then its value.
        let previous = self.contract_ensures.replace(Ensures { checker, checked });
        let inner = self.collect_contract_body(body, markers.count);
        let inner = self.contract_checked_return(inner, ptr);
        self.contract_ensures = previous;

        let mark = self.store.list_mark();
        self.store.push_list_statement(Statement::Let {
            pat: checker_pat,
            type_ref: None,
            initializer: Some(checker_value),
            else_branch: None,
            is_super: false,
        });
        self.with_lists(mark, |this, lists| {
            this.alloc_expr_desugared_with_ptr(
                Expr::Block {
                    id: None,
                    statements: lists.statements_since(mark),
                    tail: Some(inner),
                    label: None,
                    unsafe_: Unsafe::No,
                },
                ptr,
            )
        })
    }

    /// The body block with its first `count` statements, the markers, left out.
    fn collect_contract_body(&mut self, body: ast::BlockExpr<'_>, count: usize) -> ExprId {
        self.contract_markers = count;
        let body = self.collect_block(body);
        debug_assert_eq!(self.contract_markers, 0, "the body's block took its markers");
        body
    }

    /// `{ DECLS; contract_check_requires(|| PRECOND); }`, ending in
    /// `Some(contract_build_check_ensures(ENSURES))` when there is an `ensures`: the requires
    /// block's statements in its own scope (its items are its block's).
    fn contract_requires_block(
        &mut self,
        requires: ast::BlockExpr<'_>,
        ensures: Option<ast::BlockExpr<'_>>,
    ) -> ExprId {
        let ptr = ExprPtr::of(ast::Expr::BlockExpr(requires));
        self.with_block_scope(requires, |this, id| {
            let mark = this.store.list_mark();
            let precond = requires.tail_expr();
            // rustc's parser takes the block's last statement as the precondition: a tail
            // expression is it, anything else is dropped and `()` checked in its place.
            let statements = requires.statements().count();
            let decls = if precond.is_some() { statements } else { statements.saturating_sub(1) };
            requires.statements().take(decls).for_each(|stmt| this.collect_stmt(stmt));
            let closure = this.contract_precond_closure(precond, ptr);
            let check = this.lang_path(this.lang_items().ContractCheckRequires);
            let check = this.alloc_opt_path_expr(check);
            let call = this
                .alloc_expr_desugared_with_ptr(Expr::Call { callee: check, args: &[closure] }, ptr);
            this.store.push_list_statement(Statement::Expr { expr: call, has_semi: true });
            let tail = ensures.map(|ensures| this.contract_some_checker(ensures));
            this.with_lists(mark, |this, lists| {
                this.alloc_expr_desugared_with_ptr(
                    Expr::Block {
                        id,
                        statements: lists.statements_since(mark),
                        tail,
                        label: None,
                        unsafe_: Unsafe::No,
                    },
                    ptr,
                )
            })
        })
    }

    /// `|| PRECOND`, a closure as rustc's parser makes it (`mk_closure_expr`): no parameters,
    /// captured by reference, `()` when there is no precondition.
    fn contract_precond_closure(&mut self, precond: Option<ast::Expr<'_>>, ptr: ExprPtr) -> ExprId {
        self.with_label_rib(super::RibKind::Closure, |this| {
            this.with_binding_owner(|this| {
                let prev_is_lowering_coroutine = std::mem::take(&mut this.is_lowering_coroutine);
                let prev_try_block = this.current_try_block.take();
                let body = this.with_awaitable_block(Awaitable::No("non-async closure"), |this| {
                    match precond {
                        Some(precond) => this.collect_expr(precond),
                        None => this.alloc_expr_desugared_with_ptr(Expr::Tuple { exprs: &[] }, ptr),
                    }
                });
                this.is_lowering_coroutine = prev_is_lowering_coroutine;
                this.current_try_block = prev_try_block;
                this.alloc_expr_desugared_with_ptr(
                    Expr::Closure {
                        args: &[],
                        arg_types: &[],
                        ret_type: None,
                        body,
                        closure_kind: ClosureKind::Closure,
                        capture_by: CaptureBy::Ref,
                        binder: crate::expr_store::path::Names::EMPTY,
                    },
                    ptr,
                )
            })
        })
    }

    /// `Some(contract_build_check_ensures({ ENSURES }))`.
    fn contract_some_checker(&mut self, ensures: ast::BlockExpr<'_>) -> ExprId {
        let ptr = ExprPtr::of(ast::Expr::BlockExpr(ensures));
        let ensures = self.collect_block(ensures);
        let build = self.lang_path(self.lang_items().ContractBuildCheckEnsures);
        let build = self.alloc_opt_path_expr(build);
        let built =
            self.alloc_expr_desugared_with_ptr(Expr::Call { callee: build, args: &[ensures] }, ptr);
        let some = self.lang_path(self.lang_items().OptionSome);
        let some = self.alloc_opt_path_expr(some);
        self.alloc_expr_desugared_with_ptr(Expr::Call { callee: some, args: &[built] }, ptr)
    }

    /// `value` returned through the ensures check, when the function's own body is being lowered
    /// and has one (rustc's `checked_return`): `return e` and a `?` that returns. `None` is a
    /// `return` without a value, which returns `()` through it.
    pub(super) fn contract_return_value(
        &mut self,
        value: Option<ExprId>,
        ptr: ExprPtr,
    ) -> Option<ExprId> {
        if self.contract_ensures.is_none() {
            return value;
        }
        let value = value
            .unwrap_or_else(|| self.alloc_expr_desugared_with_ptr(Expr::Tuple { exprs: &[] }, ptr));
        Some(self.contract_checked_return(value, ptr))
    }

    /// `{ let __ret = value; if CC { contract_check_ensures(__ensures_checker, __ret) } else { __ret } }`
    /// (rustc's `inject_ensures_check`).
    fn contract_checked_return(&mut self, value: ExprId, ptr: ExprPtr) -> ExprId {
        // clone: the checker's name and a bool, once per return
        let Some(Ensures { checker, checked }) = self.contract_ensures.clone() else {
            return value;
        };
        let ret = self.generate_new_name();
        // clone: a name, its symbol's count
        let ret_binding =
            self.alloc_binding(ret.clone(), BindingAnnotation::Unannotated, HygieneId::ROOT);
        let ret_pat = self.alloc_pat_desugared(Pat::Bind { id: ret_binding, subpat: None });
        self.add_definition_to_binding(ret_binding, ret_pat);

        let check = self.lang_path(self.lang_items().ContractCheckEnsures);
        let check = self.alloc_opt_path_expr(check);
        let checker = self.store.alloc_name_path_expr(checker);
        let checker = self.map_expr_desugared(checker, ptr);
        // clone: a name, its symbol's count
        let ret_value = self.store.alloc_name_path_expr(ret.clone());
        let ret_value = self.map_expr_desugared(ret_value, ptr);
        let call = self.alloc_expr_desugared_with_ptr(
            Expr::Call { callee: check, args: &[checker, ret_value] },
            ptr,
        );
        let then_branch = self.contract_tail_block(call, ptr);
        let ret_value = self.store.alloc_name_path_expr(ret);
        let ret_value = self.map_expr_desugared(ret_value, ptr);
        let else_branch = self.contract_tail_block(ret_value, ptr);
        let condition = self.store.alloc_literal_expr(Literal::Bool(checked));
        let check = self.alloc_expr_desugared_with_ptr(
            Expr::If { condition, then_branch, else_branch: Some(else_branch) },
            ptr,
        );
        let mark = self.store.list_mark();
        self.store.push_list_statement(Statement::Let {
            pat: ret_pat,
            type_ref: None,
            initializer: Some(value),
            else_branch: None,
            is_super: false,
        });
        self.with_lists(mark, |this, lists| {
            this.alloc_expr_desugared_with_ptr(
                Expr::Block {
                    id: None,
                    statements: lists.statements_since(mark),
                    tail: Some(check),
                    label: None,
                    unsafe_: Unsafe::No,
                },
                ptr,
            )
        })
    }

    /// `{ tail }` (rustc's `block_expr_block`).
    fn contract_tail_block(&mut self, tail: ExprId, ptr: ExprPtr) -> ExprId {
        self.alloc_expr_desugared_with_ptr(
            Expr::Block {
                id: None,
                statements: crate::hir::Statements::EMPTY,
                tail: Some(tail),
                label: None,
                unsafe_: Unsafe::No,
            },
            ptr,
        )
    }
}
