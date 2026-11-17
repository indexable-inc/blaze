//! `format_args!` and `.await`, as nodes of their own (not desugared to calls).
//!
//! `format_args!`: rustc_ast_lowering's `expand_format_args` builds `Arguments::new_v1(..)` with
//! one `Argument::new_display(&arg)` (`new_debug`, `new_lower_hex`, ..) per use, each
//! `fn new_display<T: Display + ?Sized>(x: &T)`, and `Argument::from_usize(&arg)` for a width or
//! precision (`x: &usize`). Checked here to the same effect: each argument once, a shared
//! borrow recorded on it (codegen takes `&arg`), `T: Trait` per use with `T` the argument's type,
//! and an argument used as a count coerced to `usize`. The node is `core::fmt::Arguments`.
//!
//! `.await`: rustc's `lower_expr_await` calls `IntoFuture::into_future(e)` and loops on
//! `Future::poll`, so `e.await` proves `T: IntoFuture` and has type
//! `<<T as IntoFuture>::IntoFuture as Future>::Output`, which `IntoFuture`'s bound
//! `type IntoFuture: Future<Output = Self::Output>` makes `<T as IntoFuture>::Output`. The
//! `into_future` callee is recorded. Outside an async fn (a closure inside one included) it is
//! E0728.

use crate::body::FmtTrait;
use crate::columns::AdjustKind;
use crate::diag::{At, DiagCode};
use crate::error::{CheckError, Malformed, Stop};
use crate::ids::{ExprId, LangItem, List, Mutbl, Name};
use crate::machine::{Expectation, Flow, Frame, Machine, Then};
use crate::ty::{Cause, Ty, TyView, TypeCtx};
use crate::world::{Def, World};

impl<W: World, C: TypeCtx> Machine<'_, '_, '_, '_, W, C> {
    /// `format_args!`: the arguments in order (`f.a` the next), each followed by its uses'
    /// obligations and borrow.
    pub(crate) fn expr_format_args(&mut self, mut f: Frame, e: ExprId, args: List, uses: List) -> Result<Flow, Stop> {
        let arg_ids = self.body.expr_list(args)?;
        let use_list = self.body.fmt_use_list(uses)?;
        if f.state == 0 {
            if use_list.iter().any(|u| u.arg as usize >= arg_ids.len()) {
                return Err(CheckError::Malformed(Malformed::List {
                    column: "fmt_uses",
                    start: uses.start,
                    len: uses.len,
                })
                .into());
            }
        } else {
            let i = f.a - 1;
            let t = self.ret;
            self.fmt_uses_of(arg_ids[i as usize], i, t, uses)?;
        }
        if (f.a as usize) < arg_ids.len() {
            let i = f.a;
            f.a += 1;
            let arg = arg_ids[i as usize];
            let count = use_list.iter().any(|u| u.arg == i && u.trait_ == FmtTrait::Count);
            self.resume(f, 1)?;
            if count {
                let to = self.common.usize_;
                self.push_expr(arg, Expectation::HasType(to), Then::Coerce { to, two_phase: false })?;
            } else {
                self.push_expr(arg, Expectation::None, Then::Nothing)?;
            }
            return Ok(Flow::Wait);
        }
        match self.lang(LangItem::FormatArguments)? {
            Some(k) => {
                let none = self.mk_list(&[])?;
                Ok(Flow::Done(self.mk(TyView::Adt(k, none))?))
            }
            None => {
                self.missing_lang(LangItem::FormatArguments, At::Expr(e));
                Ok(Flow::Done(self.common.err))
            }
        }
    }

    /// Argument `i` (expression `arg`, type `t`): `t: Trait` for each of its uses, then the
    /// shared borrow every use takes.
    fn fmt_uses_of(&mut self, arg: ExprId, i: u32, t: Ty, uses: List) -> Result<(), Stop> {
        let use_list = self.body.fmt_use_list(uses)?;
        if self.is_err(t) {
            return Ok(());
        }
        for u in use_list.iter().filter(|u| u.arg == i) {
            let Some(l) = u.trait_.lang() else { continue };
            match self.lang(l)? {
                Some(tr) => {
                    let args = self.mk_list(&[t])?;
                    self.register_trait(tr, args, Cause::Expr(arg))?;
                }
                None => self.missing_lang(l, At::Expr(arg)),
            }
        }
        let borrowed = self.mk(TyView::Ref(Mutbl::Not, t))?;
        self.adjust.push((arg, AdjustKind::Borrow { mutbl: Mutbl::Not, two_phase: false, raw: false }, borrowed));
        Ok(())
    }

    /// `operand.await`.
    pub(crate) fn expr_await(&mut self, f: Frame, e: ExprId, operand: ExprId) -> Result<Flow, Stop> {
        if f.state == 0 {
            self.resume(f, 1)?;
            self.push_expr(operand, Expectation::None, Then::Nothing)?;
            return Ok(Flow::Wait);
        }
        let t = self.ret;
        if self.async_sites.last() != Some(&true) {
            // The Reference, "Await expressions", [expr.await.allowed-positions].
            self.diag(
                DiagCode::AwaitOutsideAsync,
                At::Expr(e),
                crate::diag::About::Rule {
                    rule: "expr.await.allowed-positions",
                    says: "Await expressions are legal only within an async context, like an async fn, async closure, or async block.",
                },
            );
            return Ok(Flow::Done(self.common.err));
        }
        if self.is_err(t) {
            return Ok(Flow::Done(t));
        }
        let Some(tr) = self.need_lang(LangItem::IntoFuture, At::Expr(e))? else {
            return Ok(Flow::Done(self.common.err));
        };
        let Some(Def::Fn(into_future)) = self.reads.lang_path(LangItem::IntoFutureIntoFuture)? else {
            self.missing_lang(LangItem::IntoFutureIntoFuture, At::Expr(e));
            return Ok(Flow::Done(self.common.err));
        };
        let args = self.mk_list(&[t])?;
        self.register_trait(tr, args, Cause::Expr(e))?;
        self.callees.push((e, into_future, args));
        // The future held while suspended here is part of the enclosing future's interior.
        let held = self.project(tr, args, Name::of("IntoFuture"), Cause::Expr(e))?;
        self.awaited.push((e, held));
        Ok(Flow::Done(self.project(tr, args, Name::of("Output"), Cause::Expr(e))?))
    }
}
