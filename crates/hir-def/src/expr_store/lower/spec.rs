//! Lowering of blaze_verify's native specification clauses (docs/v2/plan/design/verus.md):
//! `requires`, `recommends`, `ensures` and `decreases` on a function, `invariant`, `ensures` and
//! `decreases` on a `loop` or `while`, and a named return type `-> (r: T)`.
//!
//! Each clause becomes one clause group, an expression statement at the head of the block it is
//! about (the function's body, or the loop's body), recorded in the store's `spec_clauses` with
//! its kind:
//!
//! ```ignore (pseudo-rust)
//! fn f(n: u32) -> (r: u32) requires n < 10, n > 0 ensures r == n { BODY }
//! // lowers as
//! fn f(n: u32) -> u32 { (n < 10, n > 0); |r: u32| (r == n,); BODY }
//! ```
//!
//! so the clauses are in the scope of the parameters (and, for a loop, of the bindings around
//! it), name resolution and inference type them like any expression, and a function's `ensures`
//! sees its named return as the closure's parameter. The groups are ghost: MIR building leaves
//! out every statement whose expression is a group (`ExpressionStore::spec_clause`), so they
//! never run and never borrow; `hir-ty-verus` reads them from here.
//!
//! A crate without `#![feature(blaze_verify)]` gets `VerifyRefused` for each clause and named
//! return type, as does a function whose body also has nightly's `#[core::contracts::*]`
//! markers or that has no body: one function takes one spec form.

use syntax::view::{self as ast, AstNode as _, HasLoopBody as _};

use crate::{
    expr_store::{
        ExprPtr, ExpressionStoreDiagnostics, SpecClauseKind, VerifyRefusal, lower::ExprCollector,
    },
    hir::{CaptureBy, ClosureKind, Expr, ExprId, Statement},
};

use super::{Awaitable, RibKind};

/// The kind of the clause `clause` is, on a function or on a loop. `None` for a keyword the
/// parser does not put there (`invariant` on a function).
fn clause_kind(clause: ast::SpecClause<'_>, on_loop: bool) -> Option<SpecClauseKind> {
    Some(match on_loop {
        false if clause.requires_token().is_some() => SpecClauseKind::Requires,
        false if clause.recommends_token().is_some() => SpecClauseKind::Recommends,
        false if clause.ensures_token().is_some() => SpecClauseKind::Ensures,
        false if clause.decreases_token().is_some() => SpecClauseKind::Decreases,
        true if clause.invariant_token().is_some() => SpecClauseKind::Invariant,
        true if clause.ensures_token().is_some() => SpecClauseKind::LoopEnsures,
        true if clause.decreases_token().is_some() => SpecClauseKind::LoopDecreases,
        _ => return None,
    })
}

/// The named return type of `function`, `-> (r: T)`.
fn named_return(function: ast::Fn<'_>) -> Option<ast::RetType<'_>> {
    function.ret_type().filter(|it| it.pat().is_some())
}

impl<'db> ExprCollector<'db> {
    fn refuse_spec(&mut self, node: ast::Node<'_>, why: VerifyRefusal) {
        let node = self.expander.in_file(ast::ptr(node));
        self.store.diagnostics.push(ExpressionStoreDiagnostics::VerifyRefused { node, why });
    }

    /// Refuses each clause and the named return type of `function`, for `why`.
    fn refuse_fn_spec(&mut self, function: ast::Fn<'_>, why: VerifyRefusal) {
        for clause in function.spec_clauses() {
            self.refuse_spec(clause.syntax(), why);
        }
        if let Some(ret) = named_return(function) {
            self.refuse_spec(ret.syntax(), why);
        }
    }

    /// `function` has no body to put its clauses in: each is refused.
    pub(super) fn refuse_bodyless_spec(&mut self, function: ast::Fn<'_>) {
        let why = match self.features().blaze_verify {
            true => VerifyRefusal::WithoutBody,
            false => VerifyRefusal::WithoutFeature,
        };
        self.refuse_fn_spec(function, why);
    }

    /// Lowers the clause groups of what `block` is the body of (a function, a `loop` or a
    /// `while`), onto the statements of the block being lowered, ahead of its own.
    /// `has_contract_markers` is whether the block is a function body with nightly contract
    /// markers.
    pub(super) fn collect_spec_head(
        &mut self,
        block: ast::BlockExpr<'_>,
        has_contract_markers: bool,
    ) {
        let Some(parent) = block.syntax().parent() else { return };
        let ptr = ExprPtr::of(ast::Expr::BlockExpr(block));
        if let Some(function) = ast::Fn::cast(parent)
            && function.body() == Some(block)
        {
            let named = named_return(function);
            if function.spec_clauses().next().is_none() && named.is_none() {
                return;
            }
            if !self.features().blaze_verify {
                return self.refuse_fn_spec(function, VerifyRefusal::WithoutFeature);
            }
            if has_contract_markers {
                return self.refuse_fn_spec(function, VerifyRefusal::WithNightlyContracts);
            }
            for clause in function.spec_clauses() {
                // The parser puts only a function's four clauses here.
                let Some(kind) = clause_kind(clause, false) else { continue };
                let named = named.filter(|_| kind == SpecClauseKind::Ensures);
                self.collect_spec_group(clause, kind, named, ptr);
            }
            return;
        }
        let clauses = if let Some(it) = ast::LoopExpr::cast(parent)
            && it.loop_body() == Some(block)
        {
            it.spec_clauses()
        } else if let Some(it) = ast::WhileExpr::cast(parent)
            && it.loop_body() == Some(block)
        {
            it.spec_clauses()
        } else {
            return;
        };
        let feature = self.features().blaze_verify;
        for clause in clauses {
            if !feature {
                self.refuse_spec(clause.syntax(), VerifyRefusal::WithoutFeature);
                continue;
            }
            // The parser puts only a loop's three clauses here.
            let Some(kind) = clause_kind(clause, true) else { continue };
            self.collect_spec_group(clause, kind, None, ptr);
        }
    }

    /// One clause group, `(e1, e2, ..)`, or `|r: T| (e1, e2, ..)` for an `ensures` with the
    /// named return `named`, as a statement of the block being lowered.
    fn collect_spec_group(
        &mut self,
        clause: ast::SpecClause<'_>,
        kind: SpecClauseKind,
        named: Option<ast::RetType<'_>>,
        ptr: ExprPtr,
    ) {
        let group = match named {
            None => self.collect_spec_tuple(clause, ptr),
            Some(ret) => self.with_label_rib(RibKind::Closure, |this| {
                this.with_binding_owner(|this| {
                    let mark = this.store.list_mark();
                    let pat = this.collect_pat_top(ret.pat());
                    let ty = ret.ty().map(|it| this.lower_type_ref_disallow_impl_trait(it));
                    this.store.push_list_pat(pat);
                    this.store.push_list_opt_type(ty);
                    let prev_is_lowering_coroutine =
                        std::mem::take(&mut this.is_lowering_coroutine);
                    let prev_try_block = this.current_try_block.take();
                    let body = this
                        .with_awaitable_block(Awaitable::No("specification clause"), |this| {
                            this.collect_spec_tuple(clause, ptr)
                        });
                    this.is_lowering_coroutine = prev_is_lowering_coroutine;
                    this.current_try_block = prev_try_block;
                    this.with_lists(mark, |this, lists| {
                        this.alloc_expr_desugared_with_ptr(
                            Expr::Closure {
                                args: lists.pats_since(mark),
                                arg_types: lists.opt_types_since(mark),
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
            }),
        };
        self.store.push_list_statement(Statement::Expr { expr: group, has_semi: true });
        self.store.spec_clauses.push((group, kind));
    }

    /// `(e1, e2, ..)`: the clause's expressions.
    fn collect_spec_tuple(&mut self, clause: ast::SpecClause<'_>, ptr: ExprPtr) -> ExprId {
        let mark = self.collect_exprs(clause.exprs());
        self.with_lists(mark, |this, lists| {
            this.alloc_expr_desugared_with_ptr(Expr::Tuple { exprs: lists.exprs_since(mark) }, ptr)
        })
    }
}
