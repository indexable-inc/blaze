//! Expressions, blocks and statements, with the spans of rustc's `parser/expr.rs` and
//! `parser/stmt.rs`: an expression runs from its first token after its own attributes to its
//! last, except a binary, assignment, cast or range expression, which starts at its left
//! operand's first outer attribute (`mk_expr_sp`).

use syntax::view::{self as ra, AstNode, HasArgList as _, HasGenericArgs as _, HasLoopBody as _};
use syntax::{NodeOrToken, SyntaxKind, T};

use super::Lower;
use crate::ast::token::Delimiter;
use crate::ast::{self, DUMMY_NODE_ID, ThinVec};
use crate::span::{Ident, Span, Spanned, sym};

impl Lower<'_> {
    pub(crate) fn expr(&self, e: ra::Expr<'_>) -> Box<ast::Expr> {
        self.expr_with(e, ThinVec::new())
    }

    pub(crate) fn expr_err(&self, span: Span) -> Box<ast::Expr> {
        self.unsupported(span, "an expression rust-analyzer did not parse");
        self.mk_expr(
            span,
            ast::ExprKind::Err(ast::ErrorGuaranteed::unchecked_error_guaranteed()),
            ThinVec::new(),
        )
    }

    fn mk_expr(&self, span: Span, kind: ast::ExprKind, attrs: ast::AttrVec) -> Box<ast::Expr> {
        Box::new(ast::Expr {
            id: DUMMY_NODE_ID,
            kind,
            span,
            attrs,
            tokens: None,
        })
    }

    fn opt_expr(&self, e: Option<ra::Expr<'_>>, span: Span) -> Box<ast::Expr> {
        match e {
            Some(e) => self.expr(e),
            None => self.expr_err(span),
        }
    }

    /// `inherited` are outer attributes rustc gives this expression although rust-analyzer
    /// hangs them on an enclosing binary expression.
    fn expr_with(&self, e: ra::Expr<'_>, inherited: ast::AttrVec) -> Box<ast::Expr> {
        let mut attrs = inherited;
        let own = self.outer_attrs(e.syntax());
        let span = self.nsp(e.syntax());
        let binary_like = match e {
            ra::Expr::BinExpr(_) | ra::Expr::CastExpr(_) => true,
            ra::Expr::RangeExpr(r) => {
                use syntax::view::RangeItem as _;
                r.start().is_some()
            }
            _ => false,
        };
        if binary_like {
            let mut pass = attrs;
            pass.extend(own);
            return self.binary_like(e, pass, span);
        }
        attrs.extend(own);
        let kind = match e {
            ra::Expr::ArrayExpr(a) => match a.kind() {
                ra::ArrayExprKind::Repeat {
                    initializer,
                    repeat,
                } => {
                    let init = self.opt_expr(initializer, span);
                    let count = match repeat {
                        Some(r) => self.anon_const(r),
                        None => ast::AnonConst {
                            id: DUMMY_NODE_ID,
                            value: self.expr_err(span),
                        },
                    };
                    ast::ExprKind::Repeat(init, count)
                }
                ra::ArrayExprKind::ElementList(elems) => {
                    ast::ExprKind::Array(elems.map(|e| self.expr(e)).collect())
                }
            },
            ra::Expr::AwaitExpr(a) => {
                let kw_span = a.await_token().map(|t| self.tsp(t)).unwrap_or(span);
                ast::ExprKind::Await(self.opt_expr(a.expr(), span), kw_span)
            }
            ra::Expr::BecomeExpr(b) => ast::ExprKind::Become(self.opt_expr(b.expr(), span)),
            ra::Expr::BlockExpr(b) => return self.block_expr(b, attrs),
            ra::Expr::BreakExpr(b) => {
                let label = b.lifetime().map(|lt| ast::Label {
                    ident: self.lifetime(lt).ident,
                });
                ast::ExprKind::Break(label, b.expr().map(|e| self.expr(e)))
            }
            ra::Expr::CallExpr(c) => {
                let args = c
                    .arg_list()
                    .map(|l| l.args().map(|a| self.expr(a)).collect())
                    .unwrap_or_default();
                ast::ExprKind::Call(self.opt_expr(c.expr(), span), args)
            }
            ra::Expr::ClosureExpr(c) => return self.closure(c, attrs, span),
            ra::Expr::ContinueExpr(c) => {
                let label = c.lifetime().map(|lt| ast::Label {
                    ident: self.lifetime(lt).ident,
                });
                ast::ExprKind::Continue(label)
            }
            ra::Expr::FieldExpr(f) => {
                let ident = match f.name_ref() {
                    Some(n) => self.name_ref(n),
                    None => return self.expr_err(span),
                };
                ast::ExprKind::Field(self.opt_expr(f.expr(), span), ident)
            }
            ra::Expr::ForExpr(f) => {
                let label = self.label(f.label());
                let pat = match f.pat() {
                    Some(p) => self.pat(p),
                    None => return self.expr_err(span),
                };
                let iter = self.opt_expr(f.iterable(), span);
                let body = match f.loop_body() {
                    Some(b) => self.block_expr_inner(b),
                    None => return self.expr_err(span),
                };
                if let Some(b) = f.loop_body() {
                    attrs.extend(self.block_inner_attrs(b));
                }
                ast::ExprKind::ForLoop(Box::new(ast::ForLoop {
                    pat,
                    iter,
                    body,
                    label,
                    kind: ast::ForLoopKind::For,
                }))
            }
            ra::Expr::IfExpr(i) => self.if_kind(i, span),
            ra::Expr::IndexExpr(i) => {
                let delim = match (i.l_brack_token(), i.r_brack_token()) {
                    (Some(l), Some(r)) => self.tsp(l).to(self.tsp(r)),
                    _ => return self.expr_err(span),
                };
                ast::ExprKind::Index(
                    self.opt_expr(i.base(), span),
                    self.opt_expr(i.index(), span),
                    delim,
                )
            }
            ra::Expr::LetExpr(l) => {
                let pat = match l.pat() {
                    Some(p) => self.pat(p),
                    None => return self.expr_err(span),
                };
                let init = self.opt_expr(l.expr(), span);
                let let_span = match l.let_token() {
                    Some(t) => self.tsp(t).to(init.span),
                    None => span,
                };
                ast::ExprKind::Let(pat, init, let_span, ast::Recovered::No)
            }
            ra::Expr::Literal(l) => return self.lit_expr_with(l, attrs),
            ra::Expr::LoopExpr(l) => {
                let label = self.label(l.label());
                let kw_span = l.loop_token().map(|t| self.tsp(t)).unwrap_or(span);
                let body = match l.loop_body() {
                    Some(b) => {
                        attrs.extend(self.block_inner_attrs(b));
                        self.block_expr_inner(b)
                    }
                    None => return self.expr_err(span),
                };
                ast::ExprKind::Loop(body, label, kw_span)
            }
            ra::Expr::MacroExpr(m) => match m.macro_call() {
                Some(mc) => ast::ExprKind::MacCall(self.mac_call(mc)),
                None => return self.expr_err(span),
            },
            ra::Expr::MatchExpr(m) => {
                let scrutinee = self.opt_expr(m.expr(), span);
                let mut arms = ThinVec::new();
                if let Some(list) = m.match_arm_list() {
                    attrs.extend(self.inner_attrs(list.syntax()));
                    for arm in list.arms() {
                        arms.push(self.arm(arm));
                    }
                }
                ast::ExprKind::Match(scrutinee, arms, ast::MatchKind::Prefix)
            }
            ra::Expr::MethodCallExpr(m) => {
                let ident = match m.name_ref() {
                    Some(n) => self.name_ref(n),
                    None => return self.expr_err(span),
                };
                let args = m.generic_arg_list().map(|l| Box::new(self.angle_args(l)));
                let seg = ast::PathSegment {
                    ident,
                    id: DUMMY_NODE_ID,
                    args,
                };
                let receiver = self.opt_expr(m.receiver(), span);
                let list = m.arg_list();
                let call_args = list
                    .map(|l| l.args().map(|a| self.expr(a)).collect())
                    .unwrap_or_default();
                let hi = list.map(|l| self.nsp(l.syntax())).unwrap_or(span);
                ast::ExprKind::MethodCall(Box::new(ast::MethodCall {
                    seg,
                    receiver,
                    args: call_args,
                    span: ident.span.to(hi),
                }))
            }
            ra::Expr::ParenExpr(p) => ast::ExprKind::Paren(self.opt_expr(p.expr(), span)),
            ra::Expr::PathExpr(p) => match p.path() {
                Some(path) => {
                    let (qself, path) = self.path(path);
                    ast::ExprKind::Path(qself, path)
                }
                None => return self.expr_err(span),
            },
            ra::Expr::PrefixExpr(p) => {
                let op = match p.op_kind() {
                    Some(ra::UnaryOp::Deref) => ast::UnOp::Deref,
                    Some(ra::UnaryOp::Not) => ast::UnOp::Not,
                    Some(ra::UnaryOp::Neg) => ast::UnOp::Neg,
                    None => return self.expr_err(span),
                };
                ast::ExprKind::Unary(op, self.opt_expr(p.expr(), span))
            }
            ra::Expr::RangeExpr(r) => {
                use syntax::view::RangeItem as _;
                let limits = self.range_limits(r.syntax());
                ast::ExprKind::Range(None, r.end().map(|e| self.expr(e)), limits)
            }
            ra::Expr::RecordExpr(r) => self.struct_expr(r, span),
            ra::Expr::RefExpr(r) => {
                let mutbl = if r.mut_token().is_some() {
                    ast::Mutability::Mut
                } else {
                    ast::Mutability::Not
                };
                let kind = if r.raw_token().is_some() {
                    ast::BorrowKind::Raw
                } else {
                    ast::BorrowKind::Ref
                };
                ast::ExprKind::AddrOf(kind, mutbl, self.opt_expr(r.expr(), span))
            }
            ra::Expr::ReturnExpr(r) => ast::ExprKind::Ret(r.expr().map(|e| self.expr(e))),
            ra::Expr::TryExpr(t) => ast::ExprKind::Try(self.opt_expr(t.expr(), span)),
            ra::Expr::TupleExpr(t) => {
                ast::ExprKind::Tup(t.fields().map(|f| self.expr(f)).collect())
            }
            ra::Expr::UnderscoreExpr(_) => ast::ExprKind::Underscore,
            ra::Expr::UnsafeBinderCastExpr(c) => {
                let kind = if c.is_wrap() {
                    ast::UnsafeBinderCastKind::Wrap
                } else {
                    ast::UnsafeBinderCastKind::Unwrap
                };
                ast::ExprKind::UnsafeBinderCast(kind, self.opt_expr(c.expr(), span), c.ty().map(|t| self.ty(t)))
            }
            ra::Expr::WhileExpr(w) => {
                let label = self.label(w.label());
                let cond = self.opt_expr(w.condition(), span);
                let body = match w.loop_body() {
                    Some(b) => {
                        attrs.extend(self.block_inner_attrs(b));
                        self.block_expr_inner(b)
                    }
                    None => return self.expr_err(span),
                };
                ast::ExprKind::While(cond, body, label)
            }
            ra::Expr::YeetExpr(y) => ast::ExprKind::Yeet(y.expr().map(|e| self.expr(e))),
            ra::Expr::YieldExpr(y) => {
                ast::ExprKind::Yield(ast::YieldKind::Prefix(y.expr().map(|e| self.expr(e))))
            }
            ra::Expr::AsmExpr(_)
            | ra::Expr::CfgPredExpr(_)
            | ra::Expr::FormatArgsExpr(_)
            | ra::Expr::IncludeBytesExpr(_)
            | ra::Expr::OffsetOfExpr(_)
            | ra::Expr::BinExpr(_)
            | ra::Expr::CastExpr(_) => {
                self.unsupported(span, "a builtin expression");
                ast::ExprKind::Err(ast::ErrorGuaranteed::unchecked_error_guaranteed())
            }
        };
        self.mk_expr(span, kind, attrs)
    }

    /// `a op b`, `a = b`, `a op= b`, `a as T`, `a..b`: the attributes rust-analyzer puts on the
    /// whole expression belong to the left operand, and the span starts at the first of them.
    fn binary_like(&self, e: ra::Expr<'_>, attrs: ast::AttrVec, span: Span) -> Box<ast::Expr> {
        let (lhs_node, op_tok) = match e {
            ra::Expr::BinExpr(b) => (b.lhs(), b.op_token()),
            ra::Expr::CastExpr(c) => (c.expr(), c.as_token()),
            ra::Expr::RangeExpr(r) => {
                use syntax::view::RangeItem as _;
                (r.start(), range_op(r.syntax()))
            }
            _ => (None, None),
        };
        let (Some(lhs_node), Some(op_tok)) = (lhs_node, op_tok) else {
            return self.expr_err(span);
        };
        self.check_non_associative(e, span);
        let lhs = self.expr_with(lhs_node, attrs);
        let lhs_lo = lhs
            .attrs
            .iter()
            .find(|a| a.style == ast::AttrStyle::Outer)
            .map_or(lhs.span, |a| a.span);
        let op_span = self.tsp(op_tok);
        let (kind, hi) = match e {
            ra::Expr::BinExpr(b) => {
                let rhs = self.opt_expr(b.rhs(), span);
                let hi = rhs.span;
                let kind = match op_tok.kind() {
                    T![=] => ast::ExprKind::Assign(lhs, rhs, op_span),
                    k => match assign_op(k) {
                        Some(op) => ast::ExprKind::AssignOp(
                            Spanned {
                                node: op,
                                span: op_span,
                            },
                            lhs,
                            rhs,
                        ),
                        None => match bin_op(k) {
                            Some(op) => ast::ExprKind::Binary(
                                Spanned {
                                    node: op,
                                    span: op_span,
                                },
                                lhs,
                                rhs,
                            ),
                            None => return self.expr_err(span),
                        },
                    },
                };
                (kind, hi)
            }
            ra::Expr::CastExpr(c) => {
                let ty = self.opt_ty(c.ty(), span);
                let hi = ty.span;
                (ast::ExprKind::Cast(lhs, ty), hi)
            }
            ra::Expr::RangeExpr(r) => {
                use syntax::view::RangeItem as _;
                let end = r.end().map(|e| self.expr(e));
                let hi = end.as_ref().map_or(op_span, |e| e.span);
                (
                    ast::ExprKind::Range(Some(lhs), end, self.range_limits(r.syntax())),
                    hi,
                )
            }
            _ => return self.expr_err(span),
        };
        self.mk_expr(lhs_lo.to(op_span).to(hi), kind, ThinVec::new())
    }

    /// Ranges and comparisons do not chain in rustc's grammar: `a..b..c` and `a < b < c` are
    /// errors there (and make rustfmt try a macro argument as a type or pattern instead), where
    /// rust-analyzer nests them.
    fn check_non_associative(&self, e: ra::Expr<'_>, span: Span) {
        use syntax::view::RangeItem as _;
        let is_comparison = |e: &ra::Expr<'_>| match e {
            ra::Expr::BinExpr(b) => matches!(b.op_kind(), Some(ra::BinaryOp::CmpOp(_))),
            _ => false,
        };
        match e {
            // The right operand may be a range of its own only when it has no start: rustc
            // reads `a .. ..b` as `a..(..b)`.
            ra::Expr::RangeExpr(r) => {
                let start_is_range = matches!(r.start(), Some(ra::Expr::RangeExpr(_)));
                let end_is_range =
                    matches!(r.end(), Some(ra::Expr::RangeExpr(e)) if e.start().is_some());
                if start_is_range || end_is_range {
                    self.rustc_rejects(span, "ranges cannot be chained");
                }
            }
            ra::Expr::BinExpr(b) if is_comparison(&e) => {
                if [b.lhs(), b.rhs()].iter().flatten().any(is_comparison) {
                    self.rustc_rejects(span, "comparison operators cannot be chained");
                }
            }
            _ => {}
        }
    }

    fn range_limits(&self, node: ra::Node<'_>) -> ast::RangeLimits {
        match range_op(node).map(|t| t.kind()) {
            Some(T![..=]) => ast::RangeLimits::Closed,
            _ => ast::RangeLimits::HalfOpen,
        }
    }

    fn label(&self, label: Option<ra::Label<'_>>) -> Option<ast::Label> {
        label.and_then(|l| l.lifetime()).map(|lt| ast::Label {
            ident: self.lifetime(lt).ident,
        })
    }

    // ---- literals ----

    pub(crate) fn lit_expr(&self, l: ra::Literal<'_>) -> Box<ast::Expr> {
        self.lit_expr_with(l, ThinVec::new())
    }

    fn lit_expr_with(&self, l: ra::Literal<'_>, attrs: ast::AttrVec) -> Box<ast::Expr> {
        let token = l.token();
        let span = self.tsp(token);
        let lit = self.token_lit(token);
        self.mk_expr(span, ast::ExprKind::Lit(lit), attrs)
    }

    // ---- if, match ----

    /// rustc's `parse_expr_if`: an `else if` is an `if` expression spanning from its own `if`,
    /// and an `else { .. }` a block expression spanning its braces.
    fn if_kind(&self, i: ra::IfExpr<'_>, span: Span) -> ast::ExprKind {
        let cond = self.opt_expr(i.condition(), span);
        let then = match i.then_branch() {
            Some(b) => self.block_expr_inner(b),
            None => return self.expr_err(span).kind,
        };
        let els = i.else_branch().map(|e| match e {
            ra::ElseBranch::Block(b) => self.block_expr(b, ThinVec::new()),
            ra::ElseBranch::IfExpr(i) => {
                let span = self.nsp(i.syntax());
                let kind = self.if_kind(i, span);
                self.mk_expr(span, kind, ThinVec::new())
            }
        });
        ast::ExprKind::If(cond, then, els)
    }

    /// `if cond` of an arm or a guard pattern; `lo` stands in for a missing `if` or condition.
    pub(crate) fn guard(&self, g: ra::MatchGuard<'_>, lo: Span) -> Box<ast::Guard> {
        let if_span = g.if_token().map(|t| self.tsp(t)).unwrap_or(lo);
        let cond = self.opt_expr(g.condition(), lo);
        Box::new(ast::Guard {
            span_with_leading_if: if_span.to(cond.span),
            cond: *cond,
        })
    }

    /// rustc's `parse_arm`: from the pattern (after attributes, with a leading `|`) to the end
    /// of the body, leaving out the comma.
    fn arm(&self, arm: ra::MatchArm<'_>) -> ast::Arm {
        let attrs = self.outer_attrs(arm.syntax());
        let lo = self.nsp(arm.syntax());
        let pat = match arm.pat() {
            Some(p) => self.pat(p),
            None => Box::new(ast::Pat {
                id: DUMMY_NODE_ID,
                kind: ast::PatKind::Err(ast::ErrorGuaranteed::unchecked_error_guaranteed()),
                span: lo,
            }),
        };
        let guard = arm.guard().map(|g| self.guard(g, lo));
        let body = arm.expr().map(|e| self.expr(e));
        let hi = match (&body, &guard) {
            (Some(b), _) => b.span,
            (None, Some(g)) => g.span_with_leading_if,
            (None, None) => pat.span,
        };
        ast::Arm {
            attrs,
            span: lo.shrink_to_lo().to(hi),
            pat,
            guard,
            body,
            id: DUMMY_NODE_ID,
            is_placeholder: false,
        }
    }

    // ---- struct literals ----

    fn struct_expr(&self, r: ra::RecordExpr<'_>, span: Span) -> ast::ExprKind {
        let (qself, path) = match r.path() {
            Some(p) => self.path(p),
            None => (None, self.err_path(span)),
        };
        let mut fields = ThinVec::new();
        let mut rest = ast::StructRest::None;
        if let Some(list) = r.record_expr_field_list() {
            for f in list.fields() {
                fields.push(self.expr_field(f));
            }
            if let Some(dots) = list.dotdot_token() {
                rest = match list.spread() {
                    Some(base) => ast::StructRest::Base(self.expr(base)),
                    None => ast::StructRest::Rest(self.tsp(dots)),
                };
            }
        }
        ast::ExprKind::Struct(Box::new(ast::StructExpr {
            qself,
            path,
            fields,
            rest,
        }))
    }

    fn expr_field(&self, f: ra::RecordExprField<'_>) -> ast::ExprField {
        let attrs = self.outer_attrs(f.syntax());
        let span = self.nsp(f.syntax());
        let expr = self.opt_expr(f.expr(), span);
        let (ident, is_shorthand) = match f.name_ref() {
            Some(n) if f.colon_token().is_some() => (self.name_ref(n), false),
            _ => match &expr.kind {
                ast::ExprKind::Path(None, path) if path.segments.len() == 1 => {
                    (path.segments[0].ident, true)
                }
                _ => {
                    self.unsupported(span, "a struct literal field rustc would reject");
                    (Ident::new(sym::empty, span), true)
                }
            },
        };
        ast::ExprField {
            attrs,
            id: DUMMY_NODE_ID,
            span,
            ident,
            expr,
            is_shorthand,
            is_placeholder: false,
        }
    }

    // ---- closures ----

    /// rustc's `parse_expr_closure`: `fn_decl_span` from the start through the return type (or
    /// the closing `|`), `fn_arg_span` the `|..|`, and the closure to the end of its body.
    fn closure(&self, c: ra::ClosureExpr<'_>, attrs: ast::AttrVec, span: Span) -> Box<ast::Expr> {
        let lo = span;
        let binder = match c.for_binder() {
            Some(b) => ast::ClosureBinder::For {
                span: self.nsp(b.syntax()),
                generic_params: b
                    .generic_param_list()
                    .map(|l| l.generic_params().map(|g| self.generic_param(g)).collect())
                    .unwrap_or_default(),
            },
            None => ast::ClosureBinder::NotPresent,
        };
        let constness = match c.const_token() {
            Some(t) => ast::Const::Yes(self.tsp(t)),
            None => ast::Const::No,
        };
        let movability = if c.static_token().is_some() {
            ast::Movability::Static
        } else {
            ast::Movability::Movable
        };
        let coroutine_marker = match (c.async_token(), c.gen_token()) {
            (Some(a), Some(g)) => {
                Some((ast::CoroutineKind::AsyncGen, self.tsp(a).to(self.tsp(g))))
            }
            (Some(a), None) => Some((ast::CoroutineKind::Async, self.tsp(a))),
            (None, Some(g)) => Some((ast::CoroutineKind::Gen, self.tsp(g))),
            (None, None) => None,
        }
        .map(|(kind, span)| ast::CoroutineMarker {
            kind,
            span,
            closure_id: DUMMY_NODE_ID,
            return_impl_trait_id: DUMMY_NODE_ID,
        });
        let capture_clause = match c.move_token() {
            Some(t) => ast::CaptureBy::Value {
                move_kw: self.tsp(t),
            },
            None => match c.syntax().child_token(T![use]) {
                Some(t) => ast::CaptureBy::Use {
                    use_kw: self.tsp(t),
                },
                None => ast::CaptureBy::Ref,
            },
        };
        let Some(params) = c.param_list() else {
            return self.expr_err(span);
        };
        let fn_arg_span = self.nsp(params.syntax());
        let inputs = params.params().map(|p| self.closure_param(p)).collect();
        let output = self.ret_ty(c.ret_type(), fn_arg_span);
        let decl_hi = match &output {
            ast::FnRetTy::Ty(ty) => ty.span,
            ast::FnRetTy::Default(_) => fn_arg_span,
        };
        let body = self.opt_expr(c.body(), span);
        let closure_span = lo.to(body.span);
        let closure = ast::Closure {
            binder,
            capture_clause,
            constness,
            coroutine_marker,
            movability,
            fn_decl: Box::new(ast::FnDecl { inputs, output }),
            body,
            fn_decl_span: lo.to(decl_hi),
            fn_arg_span,
        };
        self.mk_expr(
            closure_span,
            ast::ExprKind::Closure(Box::new(closure)),
            attrs,
        )
    }

    /// rustc's `parse_fn_block_param`: the span starts before attributes; an untyped parameter
    /// gets an inferred type spanning its pattern.
    fn closure_param(&self, p: ra::Param<'_>) -> ast::Param {
        let span = self.nsp_with_attrs(p.syntax());
        let attrs = self.outer_attrs(p.syntax());
        let pat = match p.pat() {
            Some(pat) => self.pat(pat),
            None => Box::new(ast::Pat {
                id: DUMMY_NODE_ID,
                kind: ast::PatKind::Err(ast::ErrorGuaranteed::unchecked_error_guaranteed()),
                span,
            }),
        };
        let ty = match p.ty() {
            Some(ty) => self.ty(ty),
            None => Box::new(ast::Ty {
                id: DUMMY_NODE_ID,
                kind: ast::TyKind::Infer,
                span: pat.span,
            }),
        };
        ast::Param {
            attrs,
            ty,
            pat,
            id: DUMMY_NODE_ID,
            span,
            is_placeholder: false,
            has_const_keyword: false,
        }
    }

    // ---- blocks ----

    /// A block expression: `unsafe { .. }` and `'a: { .. }` blocks span their keyword or
    /// label; `async`, `gen`, `const` and `try` blocks wrap a block spanning its braces.
    fn block_expr(&self, b: ra::BlockExpr<'_>, mut attrs: ast::AttrVec) -> Box<ast::Expr> {
        let span = self.nsp(b.syntax());
        attrs.extend(self.block_inner_attrs(b));
        let label = self.label(b.label());
        let braces = b.stmt_list().map(|s| self.nsp(s.syntax())).unwrap_or(span);
        let kind = if b.async_token().is_some() || b.gen_token().is_some() {
            let kind = match (b.async_token(), b.gen_token()) {
                (Some(_), Some(_)) => ast::CoroutineKind::AsyncGen,
                (Some(_), None) => ast::CoroutineKind::Async,
                _ => ast::CoroutineKind::Gen,
            };
            let capture = match b.move_token() {
                Some(m) => ast::CaptureBy::Value {
                    move_kw: self.tsp(m),
                },
                None => ast::CaptureBy::Ref,
            };
            let decl_span = match (
                b.async_token().or(b.gen_token()),
                b.move_token().or(b.gen_token()),
            ) {
                (Some(first), Some(last)) => self.tsp(first).to(self.tsp(last)),
                (Some(first), None) => self.tsp(first),
                _ => span,
            };
            ast::ExprKind::Gen(
                capture,
                self.block_from(b, braces, ast::BlockCheckMode::Default),
                kind,
                decl_span,
            )
        } else if b.const_token().is_some() {
            let block = self.block_from(b, braces, ast::BlockCheckMode::Default);
            let value = self.mk_expr(braces, ast::ExprKind::Block(block, None), ThinVec::new());
            ast::ExprKind::ConstBlock(ast::AnonConst {
                id: DUMMY_NODE_ID,
                value,
            })
        } else if let Some(m) = b.try_block_modifier() {
            if m.bikeshed_token().is_some() {
                self.unsupported(span, "a typed `try` block");
            }
            ast::ExprKind::TryBlock(
                self.block_from(b, braces, ast::BlockCheckMode::Default),
                None,
            )
        } else {
            let rules = if b.unsafe_token().is_some() {
                ast::BlockCheckMode::Unsafe(ast::UnsafeSource::UserProvided)
            } else {
                ast::BlockCheckMode::Default
            };
            ast::ExprKind::Block(self.block_from(b, span, rules), label)
        };
        self.mk_expr(span, kind, attrs)
    }

    /// The block of a function body, loop or `if` branch: its braces.
    pub(crate) fn block_expr_inner(&self, b: ra::BlockExpr<'_>) -> Box<ast::Block> {
        let span = b
            .stmt_list()
            .map(|s| self.nsp(s.syntax()))
            .unwrap_or_else(|| self.nsp(b.syntax()));
        let rules = if b.unsafe_token().is_some() {
            ast::BlockCheckMode::Unsafe(ast::UnsafeSource::UserProvided)
        } else {
            ast::BlockCheckMode::Default
        };
        if b.label().is_some() || b.async_token().is_some() || b.const_token().is_some() {
            self.unsupported(
                self.nsp(b.syntax()),
                "a block where rustc expects a plain one",
            );
        }
        self.block_from(b, span, rules)
    }

    pub(crate) fn block_inner_attrs(&self, b: ra::BlockExpr<'_>) -> ast::AttrVec {
        match b.stmt_list() {
            Some(s) => self.inner_attrs(s.syntax()),
            None => ThinVec::new(),
        }
    }

    fn block_from(
        &self,
        b: ra::BlockExpr<'_>,
        span: Span,
        rules: ast::BlockCheckMode,
    ) -> Box<ast::Block> {
        let stmts = match b.stmt_list() {
            Some(list) => self.stmts(list),
            None => ThinVec::new(),
        };
        Box::new(ast::Block {
            stmts,
            id: DUMMY_NODE_ID,
            rules,
            span,
        })
    }

    /// rustc's `parse_full_stmt` for each statement, with a stray `;` an empty statement.
    fn stmts(&self, list: ra::StmtList<'_>) -> ThinVec<ast::Stmt> {
        let mut stmts = ThinVec::new();
        let tail = list.tail_expr();
        for child in list.syntax().children_with_tokens() {
            match child {
                NodeOrToken::Token(t) if t.kind() == T![;] => stmts.push(ast::Stmt {
                    id: DUMMY_NODE_ID,
                    kind: ast::StmtKind::Empty,
                    span: self.tsp(t),
                }),
                NodeOrToken::Token(_) => {}
                NodeOrToken::Node(n) => {
                    if let Some(stmt) = ra::Stmt::cast(n) {
                        if let Some(s) = self.stmt(stmt) {
                            stmts.push(s);
                        }
                    } else if let Some(e) = ra::Expr::cast(n)
                        && tail.is_some_and(|t| t.syntax() == n)
                    {
                        stmts.push(self.expr_stmt(e, None));
                    }
                }
            }
        }
        stmts
    }

    fn stmt(&self, stmt: ra::Stmt<'_>) -> Option<ast::Stmt> {
        match stmt {
            ra::Stmt::LetStmt(l) => Some(self.let_stmt(l)),
            ra::Stmt::ExprStmt(s) => {
                let e = s.expr()?;
                Some(self.expr_stmt(e, s.semicolon_token()))
            }
            ra::Stmt::Item(item) => {
                if let ra::Item::MacroCall(mc) = item {
                    let semi = mc.semicolon_token();
                    return Some(self.mac_stmt(mc, self.outer_attrs(mc.syntax()), semi));
                }
                let item = self.item(item)?;
                let span = item.span;
                Some(ast::Stmt {
                    id: DUMMY_NODE_ID,
                    kind: ast::StmtKind::Item(item),
                    span,
                })
            }
        }
    }

    fn expr_stmt(&self, e: ra::Expr<'_>, semi: Option<ra::Token<'_>>) -> ast::Stmt {
        if let ra::Expr::MacroExpr(m) = e
            && let Some(mc) = m.macro_call()
        {
            let attrs = self.outer_attrs(e.syntax());
            let brace = mc.token_tree().and_then(|tt| tt.l_curly_token()).is_some();
            if brace || semi.is_some() {
                return self.mac_stmt(mc, attrs, semi);
            }
        }
        let expr = self.expr(e);
        let lo = expr.span;
        match semi {
            Some(semi) => {
                let span = lo.to(self.tsp(semi));
                ast::Stmt {
                    id: DUMMY_NODE_ID,
                    kind: ast::StmtKind::Semi(expr),
                    span,
                }
            }
            None => ast::Stmt {
                id: DUMMY_NODE_ID,
                kind: ast::StmtKind::Expr(expr),
                span: lo,
            },
        }
    }

    /// rustc's `parse_stmt_mac`: a macro call in statement position, `Braces` for `{..}`,
    /// `Semicolon` once a `;` follows.
    fn mac_stmt(
        &self,
        mc: ra::MacroCall<'_>,
        attrs: ast::AttrVec,
        semi: Option<ra::Token<'_>>,
    ) -> ast::Stmt {
        let mac = self.mac_call(mc);
        let lo = self.nsp(mc.syntax());
        let style = match (semi.is_some(), mac.args.delim) {
            (true, _) => ast::MacStmtStyle::Semicolon,
            (false, Delimiter::Brace) => ast::MacStmtStyle::Braces,
            (false, _) => ast::MacStmtStyle::NoBraces,
        };
        let span = match semi {
            Some(s) => lo.to(self.tsp(s)),
            None => lo,
        };
        ast::Stmt {
            id: DUMMY_NODE_ID,
            kind: ast::StmtKind::MacCall(Box::new(ast::MacCallStmt {
                mac,
                style,
                attrs,
                tokens: None,
            })),
            span,
        }
    }

    /// rustc's `parse_local`: from `let` (or `super`) through the `;`.
    fn let_stmt(&self, l: ra::LetStmt<'_>) -> ast::Stmt {
        let attrs = self.outer_attrs(l.syntax());
        let span = self.nsp(l.syntax());
        let super_ = l.super_token().map(|t| self.tsp(t));
        let pat = match l.pat() {
            Some(p) => self.pat(p),
            None => {
                self.unsupported(span, "a `let` without a pattern");
                Box::new(ast::Pat {
                    id: DUMMY_NODE_ID,
                    kind: ast::PatKind::Wild,
                    span,
                })
            }
        };
        let colon_sp = l.colon_token().map(|t| self.tsp(t));
        let ty = l.ty().map(|t| self.ty(t));
        let kind = match (l.initializer(), l.let_else()) {
            (None, _) => ast::LocalKind::Decl,
            (Some(init), None) => ast::LocalKind::Init(self.expr(init)),
            (Some(init), Some(els)) => {
                let init = self.expr(init);
                match els.block_expr() {
                    Some(b) => ast::LocalKind::InitElse(init, self.block_expr_inner(b)),
                    None => {
                        self.unsupported(span, "a `let .. else` without a block");
                        ast::LocalKind::Init(init)
                    }
                }
            }
        };
        if l.semicolon_token().is_none() {
            self.unsupported(span, "a `let` without a `;`");
        }
        let local = ast::Local {
            id: DUMMY_NODE_ID,
            super_,
            pat,
            ty,
            kind,
            span,
            colon_sp,
            attrs,
            tokens: None,
        };
        ast::Stmt {
            id: DUMMY_NODE_ID,
            kind: ast::StmtKind::Let(Box::new(local)),
            span,
        }
    }
}

fn range_op(node: ra::Node<'_>) -> Option<ra::Token<'_>> {
    node.children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|t| matches!(t.kind(), T![..] | T![..=] | T![...]))
}

fn bin_op(kind: SyntaxKind) -> Option<ast::BinOpKind> {
    use ast::BinOpKind::*;
    Some(match kind {
        T![+] => Add,
        T![-] => Sub,
        T![*] => Mul,
        T![/] => Div,
        T![%] => Rem,
        T![&&] => And,
        T![||] => Or,
        T![^] => BitXor,
        T![&] => BitAnd,
        T![|] => BitOr,
        T![<<] => Shl,
        T![>>] => Shr,
        T![==] => Eq,
        T![<] => Lt,
        T![<=] => Le,
        T![!=] => Ne,
        T![>=] => Ge,
        T![>] => Gt,
        _ => return None,
    })
}

fn assign_op(kind: SyntaxKind) -> Option<ast::AssignOpKind> {
    use ast::AssignOpKind::*;
    Some(match kind {
        T![+=] => AddAssign,
        T![-=] => SubAssign,
        T![*=] => MulAssign,
        T![/=] => DivAssign,
        T![%=] => RemAssign,
        T![^=] => BitXorAssign,
        T![&=] => BitAndAssign,
        T![|=] => BitOrAssign,
        T![<<=] => ShlAssign,
        T![>>=] => ShrAssign,
        _ => return None,
    })
}
