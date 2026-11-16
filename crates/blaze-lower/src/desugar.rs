//! rustc's desugarings, made as the machine closes the node that needs one: the same shapes
//! `rustc_ast_lowering` builds (its `lower_expr_for`, `lower_expr_while_in_loop_scope`,
//! `lower_expr_try`, `lower_expr_range`, `make_lowered_await` and
//! `lower_coroutine_body_with_moved_arguments`), in this crate's columns. A path a desugaring
//! names is a lang item's ([`PathRef::lang`]); the body's set of them is its read list's.
//!
//! Every node made here is written after its children (post-order holds), and every name made
//! here (`iter`, `val`, `residual`, `__awaitee`, `result`, `_task_context`, `__argN`) is
//! hygienic: no path of the source finds it, because no binding made here enters the scope
//! map; the desugared expressions name their bindings by pattern directly.
//!
//! Each expansion, as rustc documents it:
//!
//! ```text
//! for PAT in HEAD BODY
//!   => match IntoIterator::into_iter(HEAD) {
//!          mut iter => loop {
//!              match Iterator::next(&mut iter) {
//!                  None => break,
//!                  Some(PAT) => BODY,
//!              }
//!          },
//!      }
//! while COND BODY
//!   => loop { if COND BODY else { break } }
//! EXPR?
//!   => match Try::branch(EXPR) {
//!          ControlFlow::Break(residual) => return FromResidual::from_residual(residual),
//!          ControlFlow::Continue(val) => val,
//!      }
//!      (inside `try { }`: `break 'try FromResidual::from_residual(residual)`)
//! a..b => Range { start: a, end: b }      a.. => RangeFrom { start: a }
//! ..b  => RangeTo { end: b }              ..  => RangeFull {}
//! a..=b => RangeInclusive::new(a, b)      ..=b => RangeToInclusive { end: b }
//! EXPR.await
//!   => match IntoFuture::into_future(EXPR) {
//!          mut __awaitee => loop {
//!              match unsafe { Future::poll(Pin::new_unchecked(&mut __awaitee),
//!                                          get_context(_task_context)) } {
//!                  Poll::Ready(result) => break result,
//!                  Poll::Pending => {}
//!              }
//!              _task_context = yield ();
//!          },
//!      }
//! async fn f(PAT: T) BODY
//!   => fn f(__arg0: T) { async move { let PAT = __arg0; BODY } }
//! ```

use blaze_surface::Range;

use crate::form::*;
use crate::machine::{Machine, NOT_CORO, idx};

/// A `for` loop's two nodes: the expression it is (the outer `match`) and the loop its
/// `break` and `continue` leave.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ForParts {
    pub(crate) outer: ExprId,
    pub(crate) inner_loop: ExprId,
}

impl Machine<'_, '_, '_> {
    /// A path to a lang item, read by the body.
    fn lang_path(&mut self, lang: LangItem, r: Range) -> PathIdx {
        self.out.langs |= lang.bit();
        let segments = List { start: idx(self.out.rows.len()), len: 0 };
        let p = PathRef { segments, absolute: false, scope: Some(self.scope()), lang: Some(lang) };
        self.put_path(p, r)
    }

    fn lang_call(&mut self, lang: LangItem, args: &[ExprId], r: Range) -> ExprId {
        let path = self.lang_path(lang, r);
        let callee = self.put_expr(Expr::Path(path), r);
        let start = idx(self.out.ids.len());
        self.out.ids.extend(args.iter().map(|a| a.0));
        self.put_expr(Expr::Call { callee, args: List { start, len: idx(args.len()) } }, r)
    }

    /// An expression naming a binding made here: a local with a path of no segments.
    pub(crate) fn hygienic_local(&mut self, pat: PatId, r: Range) -> ExprId {
        let segments = List { start: idx(self.out.rows.len()), len: 0 };
        let path = self.put_path(PathRef { segments, absolute: false, scope: Some(self.scope()), lang: None }, r);
        self.put_expr(Expr::Local { pat, path }, r)
    }

    fn hygienic_bind(&mut self, name: &str, mutable: bool, r: Range) -> PatId {
        let name = self.synth_name(name, r);
        let ann = BindAnn { by_ref: None, mutable };
        self.put_pat(Pat::Bind { ann, name, sub: None, shadows: None, candidate: false }, r)
    }

    /// `Lang(elems..)` as a pattern.
    fn lang_variant_pat(&mut self, lang: LangItem, elems: &[PatId], r: Range) -> PatId {
        let path = self.lang_path(lang, r);
        if elems.is_empty() {
            return self.put_pat(Pat::Path(path), r);
        }
        let start = idx(self.out.ids.len());
        self.out.ids.extend(elems.iter().map(|p| p.0));
        self.put_pat(Pat::TupleStruct { path, elems: List { start, len: idx(elems.len()) }, rest: None }, r)
    }

    fn arms(&mut self, arms: &[Arm], r: Range) -> List {
        let start = idx(self.out.rows.len());
        for a in arms {
            self.out.rows.push(a.encode());
            self.out.row_ranges.push(r);
        }
        List { start, len: idx(arms.len()) }
    }

    fn block_of(&mut self, stmts: &[StmtId], tail: Option<ExprId>, kind: BlockKind, r: Range) -> ExprId {
        let start = idx(self.out.ids.len());
        self.out.ids.extend(stmts.iter().map(|s| s.0));
        let scope = self.scope();
        self.put_expr(Expr::Block { stmts: List { start, len: idx(stmts.len()) }, tail, scope, kind }, r)
    }

    /// `for PAT in HEAD BODY`; the user's `break`s and `continue`s target `inner_loop`.
    pub(crate) fn for_loop(&mut self, pat: PatId, head: ExprId, body: ExprId, r: Range) -> ForParts {
        let head_r = self.out.expr_ranges[head.0 as usize];
        // `None => break`
        let break_ = if plant_for_drops_break() {
            self.put_expr(Expr::Tuple { elems: List { start: 0, len: 0 } }, r)
        } else {
            self.put_expr(Expr::Break { value: None, target: None }, r)
        };
        let none = self.lang_variant_pat(LangItem::OptionNone, &[], r);
        // `Some(PAT) => BODY`
        let pat_r = self.out.pat_ranges[pat.0 as usize];
        let some = self.lang_variant_pat(LangItem::OptionSome, &[pat], pat_r);
        // `mut iter`
        let iter = self.hygienic_bind("iter", true, head_r);
        // `Iterator::next(&mut iter)`
        let iter_ref = self.hygienic_local(iter, head_r);
        let borrow = self.put_expr(Expr::Ref { mutbl: Mutbl::Mut, raw: false, operand: iter_ref }, head_r);
        let next = self.lang_call(LangItem::IteratorNext, &[borrow], head_r);
        let arms = self.arms(&[Arm { pat: none, guard: None, body: break_ }, Arm { pat: some, guard: None, body }], r);
        let inner = self.put_expr(Expr::Match { scrutinee: next, arms, source: MatchSource::ForLoop }, head_r);
        let stmt = self.put_stmt(Stmt::Expr { expr: inner, semi: false });
        let loop_body = self.block_of(&[stmt], None, BlockKind::Plain, r);
        let inner_loop = self.put_expr(Expr::Loop { body: loop_body, source: LoopSource::ForLoop }, r);
        if let Ok(Expr::Break { value, .. }) = Expr::decode(self.out.exprs[break_.0 as usize]) {
            self.out.exprs[break_.0 as usize] = Expr::Break { value, target: Some(inner_loop) }.encode();
        }
        // `match IntoIterator::into_iter(HEAD) { mut iter => loop { .. } }`
        let into_iter = self.lang_call(LangItem::IntoIterIntoIter, &[head], head_r);
        let arms = self.arms(&[Arm { pat: iter, guard: None, body: inner_loop }], r);
        let outer = self.put_expr(Expr::Match { scrutinee: into_iter, arms, source: MatchSource::ForLoop }, r);
        ForParts { outer, inner_loop }
    }

    /// `while COND BODY`: `loop { if COND BODY else { break } }`. Returns the loop.
    pub(crate) fn while_loop(&mut self, cond: ExprId, body: ExprId, r: Range) -> ExprId {
        let break_ = self.put_expr(Expr::Break { value: None, target: None }, r);
        let stmt = self.put_stmt(Stmt::Expr { expr: break_, semi: false });
        let else_ = self.block_of(&[stmt], None, BlockKind::Plain, r);
        let if_ = self.put_expr(Expr::If { cond, then: body, else_: Some(else_) }, r);
        let block = self.block_of(&[], Some(if_), BlockKind::Plain, r);
        let lp = self.put_expr(Expr::Loop { body: block, source: LoopSource::While }, r);
        self.out.exprs[break_.0 as usize] = Expr::Break { value: None, target: Some(lp) }.encode();
        lp
    }

    /// `EXPR?`.
    pub(crate) fn try_op(&mut self, e: ExprId, r: Range) -> ExprId {
        let branch = self.lang_call(LangItem::TryTraitBranch, &[e], r);
        // `ControlFlow::Break(residual) => return FromResidual::from_residual(residual)`
        let residual = self.hygienic_bind("residual", false, r);
        let break_pat = self.lang_variant_pat(LangItem::ControlFlowBreak, &[residual], r);
        let residual_ref = self.hygienic_local(residual, r);
        let from = self.lang_call(LangItem::TryTraitFromResidual, &[residual_ref], r);
        let leave = match self.try_entry() {
            Some(entry) => {
                let b = self.put_expr(Expr::Break { value: Some(from), target: None }, r);
                self.add_jump(entry, b);
                b
            }
            None => self.put_expr(Expr::Return { value: Some(from) }, r),
        };
        // `ControlFlow::Continue(val) => val`
        let val = self.hygienic_bind("val", false, r);
        let continue_pat = self.lang_variant_pat(LangItem::ControlFlowContinue, &[val], r);
        let val_ref = self.hygienic_local(val, r);
        let arms = self.arms(
            &[Arm { pat: break_pat, guard: None, body: leave }, Arm { pat: continue_pat, guard: None, body: val_ref }],
            r,
        );
        self.put_expr(Expr::Match { scrutinee: branch, arms, source: MatchSource::Try }, r)
    }

    /// A range expression, by which ends it has.
    pub(crate) fn range_expr(&mut self, lo: Option<ExprId>, hi: Option<ExprId>, inclusive: bool, r: Range) -> ExprId {
        let lang = match (lo, hi, inclusive) {
            (None, None, false) => LangItem::RangeFull,
            (Some(_), None, false) => LangItem::RangeFrom,
            (None, Some(_), false) => LangItem::RangeTo,
            (Some(_), Some(_), false) => LangItem::Range,
            (None, Some(_), true) => LangItem::RangeToInclusive,
            (Some(lo), Some(hi), true) => return self.lang_call(LangItem::RangeInclusiveNew, &[lo, hi], r),
            (start, None, true) => {
                // E0586; rustc goes on with the half-open range, and so does the lowering.
                self.diag(DiagKind::InclusiveRangeNoEnd, r);
                if start.is_some() { LangItem::RangeFrom } else { LangItem::RangeFull }
            }
        };
        let path = self.lang_path(lang, r);
        let mut inits = [FieldInit { name: NameIdx(0), expr: ExprId(0), shorthand: false }; 2];
        let mut n = 0;
        for (field, e) in [("start", lo), ("end", hi)] {
            if let Some(e) = e {
                let er = self.out.expr_ranges[e.0 as usize];
                inits[n] = FieldInit { name: self.synth_name(field, er), expr: e, shorthand: false };
                n += 1;
            }
        }
        let start = idx(self.out.rows.len());
        for init in &inits[..n] {
            self.out.rows.push(init.encode());
            self.out.row_ranges.push(r);
        }
        self.put_expr(Expr::Struct { path, fields: List { start, len: idx(n) }, base: StructBase::None }, r)
    }

    /// The innermost coroutine's resume argument, `_task_context`, made at its first use.
    fn task_context(&mut self, r: Range) -> Option<PatId> {
        let at = self.coros.len().checked_sub(1)?;
        match self.coros[at] {
            NOT_CORO => None,
            NONE => {
                let p = self.hygienic_bind("_task_context", true, r);
                self.coros[at] = p.0;
                Some(p)
            }
            p => Some(PatId(p)),
        }
    }

    /// `EXPR.await`.
    pub(crate) fn await_op(&mut self, e: ExprId, r: Range) -> ExprId {
        let Some(task) = self.task_context(r) else {
            // E0728: no desugaring outside a coroutine.
            self.diag(DiagKind::AwaitOutsideAsync, r);
            return e;
        };
        let into_future = self.lang_call(LangItem::IntoFutureIntoFuture, &[e], r);
        let awaitee = self.hygienic_bind("__awaitee", true, r);
        // `unsafe { Future::poll(Pin::new_unchecked(&mut __awaitee), get_context(_task_context)) }`
        let awaitee_ref = self.hygienic_local(awaitee, r);
        let borrow = self.put_expr(Expr::Ref { mutbl: Mutbl::Mut, raw: false, operand: awaitee_ref }, r);
        let pinned = self.lang_call(LangItem::PinNewUnchecked, &[borrow], r);
        let task_ref = self.hygienic_local(task, r);
        let cx = self.lang_call(LangItem::GetContext, &[task_ref], r);
        let poll = self.lang_call(LangItem::FuturePoll, &[pinned, cx], r);
        let unsafe_poll = self.block_of(&[], Some(poll), BlockKind::Unsafe, r);
        // `Poll::Ready(result) => break result`
        let result = self.hygienic_bind("result", false, r);
        let ready = self.lang_variant_pat(LangItem::PollReady, &[result], r);
        let result_ref = self.hygienic_local(result, r);
        let break_ = self.put_expr(Expr::Break { value: Some(result_ref), target: None }, r);
        // `Poll::Pending => {}`
        let pending = self.lang_variant_pat(LangItem::PollPending, &[], r);
        let empty = self.block_of(&[], None, BlockKind::Plain, r);
        let arms =
            self.arms(&[Arm { pat: ready, guard: None, body: break_ }, Arm { pat: pending, guard: None, body: empty }], r);
        let poll_match = self.put_expr(Expr::Match { scrutinee: unsafe_poll, arms, source: MatchSource::Await }, r);
        let poll_stmt = self.put_stmt(Stmt::Expr { expr: poll_match, semi: false });
        // `_task_context = yield ();`
        let unit = self.put_expr(Expr::Tuple { elems: List { start: 0, len: 0 } }, r);
        let yield_ = self.put_expr(Expr::Yield { value: Some(unit) }, r);
        let task_place = self.hygienic_local(task, r);
        let assign = self.put_expr(Expr::Assign { lhs: task_place, rhs: yield_ }, r);
        let yield_stmt = self.put_stmt(Stmt::Expr { expr: assign, semi: false });
        let body = self.block_of(&[poll_stmt, yield_stmt], None, BlockKind::Plain, r);
        let lp = self.put_expr(Expr::Loop { body, source: LoopSource::Loop }, r);
        self.out.exprs[break_.0 as usize] = Expr::Break { value: Some(result_ref), target: Some(lp) }.encode();
        let arms = self.arms(&[Arm { pat: awaitee, guard: None, body: lp }], r);
        self.put_expr(Expr::Match { scrutinee: into_future, arms, source: MatchSource::Await }, r)
    }

    /// An `async fn`'s body: its parameters (`pats`, the written patterns) become the coroutine's
    /// moved arguments. Returns the body's root and the fn's parameter patterns, in `ids`.
    pub(crate) fn async_fn(&mut self, pats: &[u32], body: ExprId, r: Range) -> (ExprId, List) {
        let mut stmts: blaze_own::AVec<'_, StmtId> = blaze_own::Vec::new_in(*self.vals.allocator());
        let mut params: blaze_own::AVec<'_, u32> = blaze_own::Vec::new_in(*self.vals.allocator());
        for (n, &orig) in pats.iter().enumerate() {
            let Some(orig) = unopt(orig).map(PatId) else {
                params.push(NONE);
                continue;
            };
            let pr = self.out.pat_ranges[orig.0 as usize];
            let simple = match Pat::decode(self.out.pats[orig.0 as usize]) {
                Ok(Pat::Bind { ann: BindAnn { by_ref: None, .. }, name, sub: None, .. }) => Some(name),
                _ => None,
            };
            match simple {
                // `x: T` => `x: T`, then `let x = x;`, the written pattern kept.
                Some(name) => {
                    let ann = BindAnn { by_ref: None, mutable: false };
                    let param = self.put_pat(Pat::Bind { ann, name, sub: None, shadows: None, candidate: false }, pr);
                    let init = self.hygienic_local(param, pr);
                    stmts.push(self.put_stmt(Stmt::Let { pat: orig, ty: None, init: Some(init), else_: None }));
                    params.push(param.0);
                }
                // `PAT: T` => `__argN: T`, then `let mut __argN = __argN; let PAT = __argN;`.
                None => {
                    let mut text = blaze_own::String::new_in(*self.vals.allocator());
                    let _ = core::fmt::Write::write_fmt(&mut text, format_args!("__arg{n}"));
                    let param = self.hygienic_bind(text.as_str(), false, pr);
                    let moved = self.hygienic_bind(text.as_str(), true, pr);
                    let init = self.hygienic_local(param, pr);
                    stmts.push(self.put_stmt(Stmt::Let { pat: moved, ty: None, init: Some(init), else_: None }));
                    let init = self.hygienic_local(moved, pr);
                    stmts.push(self.put_stmt(Stmt::Let { pat: orig, ty: None, init: Some(init), else_: None }));
                    params.push(param.0);
                }
            }
        }
        let block = self.block_of(&stmts, Some(body), BlockKind::Plain, r);
        let resume = self.coros.last().copied().filter(|c| *c != NOT_CORO).and_then(unopt).map(PatId);
        let c = Closure {
            params: List { start: 0, len: 0 },
            ret: None,
            body: block,
            resume,
            kind: CoroutineKind::Async,
            source: CoroutineSource::Fn,
            capture_by_value: true,
            konst: false,
            immovable: false,
        };
        let row = self.put_row(c.encode(), r);
        let root = self.put_expr(Expr::Closure { row }, r);
        let params = self.put_ids_of(&params);
        (root, params)
    }
}

#[cfg(test)]
fn plant_for_drops_break() -> bool {
    crate::plant::on(crate::plant::Plant::ForDropsBreak)
}

#[cfg(not(test))]
const fn plant_for_drops_break() -> bool {
    false
}
