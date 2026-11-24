//! Blocks, statements, `if`, `loop`, `break`, `continue` and `return`.
//!
//! Scopes and temporaries (the Reference, "Destructors"):
//! - a block is a drop scope for its bindings; each statement is a temporary scope;
//! - a block's tail expression: from edition 2024 its own temporary scope, its temporaries
//!   dropped before the block's bindings; before 2024 its temporaries live on in the
//!   enclosing temporary scope, dropped after the block's bindings;
//! - an `if` condition is a temporary scope, its temporaries dropped before either branch;
//! - an `if let` scrutinee's temporaries: from edition 2024 dropped before the `else` branch,
//!   before 2024 at the end of the whole `if let`.
//!
//! `while` and `for` arrive desugared to `loop` and `match` (the tree's lowering hands them so),
//! and `?` to a `match` on `Try::branch` with a `return`; both are lowered as what they are.

use core::alloc::Allocator;

use blaze_body::{BlockKind, Place, Role, StmtKind};
use blaze_own::Vec;
use blaze_typeck::{BinOp, Edition, Expr, ExprId, List, PNode, PatId, Stmt, StmtId};

use crate::scope::{Binding, Entry, Target};
use crate::{At, BodyFacts, Construct, Lower, LowerError, MOST_POSITIONS};

impl<F: BodyFacts, A: Allocator + Copy> Lower<'_, '_, '_, F, A> {
    fn edition_2024(&self) -> bool {
        matches!(self.body.edition, Edition::E2024)
    }

    /// `{ stmts; tail }` into `dest`. A block a `break` names is a target: its exit is after it.
    pub(crate) fn block_into(&mut self, dest: Place, e: ExprId, stmts: List, tail: Option<ExprId>) -> Result<(), LowerError> {
        let span = self.span(e);
        let mark = self.open();
        let exit = if self.is_break_target(e) {
            let x = self.b.new_block(BlockKind::Normal);
            self.targets.push(Target { expr: e, exit: x, head: None, dest, mark });
            Some(x)
        } else {
            None
        };
        for &s in self.stmts_in(stmts, e)? {
            self.stmt(s, e)?;
        }
        match tail {
            Some(t) if self.edition_2024() => {
                self.open();
                self.lower_into(dest, t)?;
                self.close(span)?;
                self.close(span)?;
            }
            Some(t) => {
                let from = self.sched.len();
                self.lower_into(dest, t)?;
                self.exit_keeping_tail(mark, from, span)?;
                self.marks.pop();
            }
            None => {
                let ty = self.expr_ty(e)?;
                self.unit(dest, ty, At::Expr(e), span)?;
                self.close(span)?;
            }
        }
        if let Some(x) = exit {
            self.goto(x, span);
            self.b.start(x);
            self.targets.pop();
        }
        Ok(())
    }

    fn is_super_let(&self, s: StmtId) -> bool {
        self.body.super_lets.binary_search_by(|q| q.0.cmp(&s.0)).is_ok()
    }

    /// One statement of block `block`: a temporary scope around it.
    fn stmt(&mut self, s: StmtId, block: ExprId) -> Result<(), LowerError> {
        match self.stmt_at(s, block)? {
            Stmt::Item => Ok(()),
            Stmt::Expr { expr, .. } => {
                let span = self.span(expr);
                self.open();
                self.discard(expr)?;
                self.close(span)
            }
            Stmt::Let { pat, init, else_, .. } => {
                if self.is_super_let(s) {
                    if else_.is_some() {
                        return self.refuse_pat(Construct::SuperLet, pat);
                    }
                    return self.super_let_stmt(pat, init, block);
                }
                match else_ {
                    Some(diverge) => self.let_else_stmt(pat, init, diverge),
                    None => self.let_stmt(pat, init),
                }
            }
        }
    }

    /// `e`'s value made and dropped with its statement (`e;`): a place expression is moved
    /// (or copied) out, as in any value context.
    pub(crate) fn discard(&mut self, e: ExprId) -> Result<(), LowerError> {
        let ty = self.adjusted_ty(e)?;
        let t = if let PNode::Never = self.node(ty, At::Expr(e))? {
            self.scratch(e, ty)
        } else {
            self.temp(e, ty, Role::Value)?
        };
        self.lower_into(t, e)
    }

    /// `let pat = init;`: the bindings' storage begins, the initializer is lowered into them in
    /// the statement's temporary scope, its temporaries are dropped, then the bindings join the
    /// block's scope (so a call in the initializer unwinds without dropping them). `let pat;`
    /// schedules its bindings uninitialized.
    fn let_stmt(&mut self, pat: PatId, init: Option<ExprId>) -> Result<(), LowerError> {
        let span = self.pat_span(pat);
        let mut out: Vec<Binding, A> = Vec::new_in(self.alloc);
        let Some(init) = init else {
            self.declare_uninit(pat, &mut out)?;
            self.schedule(&out);
            return Ok(());
        };
        self.open();
        let prev = core::mem::replace(&mut self.extended, Vec::new_in(self.alloc));
        let prev_blocks = core::mem::replace(&mut self.extended_blocks, Vec::new_in(self.alloc));
        let prev_mark = self.ext_mark.take();
        self.mark_extending(init)?;
        self.ext_mark = Some(self.marks.len() - 1);
        if self.simple_bind(pat)? {
            let ty = self.local_ty(pat)?;
            let (local, place) = self.binding_local(pat, ty)?;
            self.lower_into(place, init)?;
            let drops = self.needs_drop(ty, At::Pat(pat))?;
            out.push(Binding { local, place, ty, drops });
            self.bind(pat, place);
        } else {
            let ty = self.adjusted_ty(init)?;
            let p = self.place(init)?;
            self.bind_irrefutable(pat, p, ty, &mut out)?;
        }
        self.extended = prev;
        self.extended_blocks = prev_blocks;
        self.ext_mark = prev_mark;
        self.close(span)?;
        self.schedule(&out);
        Ok(())
    }

    /// `super let pat = init;`: bindings outlive `block`, placed in the scope a temporary of
    /// `block`'s value would live in (the extending `let`'s block scope if `block` is an
    /// extending expression of a `let`, else the scope immediately enclosing `block`).
    fn super_let_stmt(&mut self, pat: PatId, init: Option<ExprId>, block: ExprId) -> Result<(), LowerError> {
        let span = self.pat_span(pat);
        let mut out: Vec<Binding, A> = Vec::new_in(self.alloc);
        match init {
            None => self.declare_uninit(pat, &mut out)?,
            Some(init) => {
                self.open();
                let prev = core::mem::replace(&mut self.extended, Vec::new_in(self.alloc));
                let prev_blocks = core::mem::replace(&mut self.extended_blocks, Vec::new_in(self.alloc));
                let prev_mark = self.ext_mark.take();
                self.mark_extending(init)?;
                self.ext_mark = Some(self.marks.len() - 1);
                if self.simple_bind(pat)? {
                    let ty = self.local_ty(pat)?;
                    let (local, place) = self.binding_local(pat, ty)?;
                    self.lower_into(place, init)?;
                    let drops = self.needs_drop(ty, At::Pat(pat))?;
                    out.push(Binding { local, place, ty, drops });
                    self.bind(pat, place);
                } else {
                    let ty = self.adjusted_ty(init)?;
                    let p = self.place(init)?;
                    self.bind_irrefutable(pat, p, ty, &mut out)?;
                }
                self.extended = prev;
                self.extended_blocks = prev_blocks;
                self.ext_mark = prev_mark;
                self.close(span)?;
            }
        }
        let target_d = match self.ext_mark {
            Some(d) if self.extended_blocks.scan_for(&block).is_some() => d,
            _ => match self.marks.len().checked_sub(1) {
                Some(d) => d,
                None => {
                    self.schedule(&out);
                    return Ok(());
                }
            },
        };
        let mut first_at: Option<usize> = None;
        for b in out.iter() {
            let Some(&mark_u32) = self.marks.get(target_d) else {
                return self.refuse_pat(Construct::Malformed, pat);
            };
            let Ok(at) = usize::try_from(mark_u32) else {
                return self.refuse_pat(Construct::Malformed, pat);
            };
            if first_at.is_none() {
                first_at = Some(at);
            }
            self.sched.insert_shift(at, Entry { local: b.local, place: b.place, drops: b.drops, storage: true, cleanup: None });
            for m in self.marks.iter_mut().skip(target_d) {
                *m += 1;
            }
        }
        if let Some(at) = first_at {
            self.invalidate_from(at);
        }
        Ok(())
    }

    /// `let pat = init else { diverge };` (the Reference, "Let statements"): `init` is evaluated
    /// in its statement temporary scope; if `pat` fails to match, the statement's temporaries
    /// are dropped BEFORE `diverge` runs (and `diverge` cannot return); if `pat` matches, its
    /// bindings are made, the statement's temporaries are dropped, and the bindings join the
    /// enclosing block's scope.
    fn let_else_stmt(&mut self, pat: PatId, init: Option<ExprId>, diverge: ExprId) -> Result<(), LowerError> {
        let Some(init) = init else {
            return self.refuse_pat(Construct::Malformed, pat);
        };
        let span = self.pat_span(pat);
        let tm = self.open();
        let ty = self.adjusted_ty(init)?;
        let p = self.place(init)?;
        let eb = self.b.new_block(BlockKind::Normal);
        let after = self.b.new_block(BlockKind::Normal);
        let mut out: Vec<Binding, A> = Vec::new_in(self.alloc);
        self.match_and_bind(pat, p, ty, eb, init, &mut out)?;
        self.emit_exit(tm, span)?;
        self.goto(after, span);
        self.b.start(eb);
        self.emit_exit(tm, span)?;
        self.pop_scope();
        let else_ty = self.adjusted_ty(diverge)?;
        let dead = self.scratch(diverge, else_ty);
        self.lower_into(dead, diverge)?;
        self.b.terminate(StmtKind::Unreachable, self.span(diverge));
        self.b.start(after);
        self.schedule(&out);
        Ok(())
    }

    /// The operands of the extending borrows of initializer `init` (the Reference, "Extending
    /// based on expressions"): the initializer is extending; so are the operand of an extending
    /// borrow, the operands of an extending array, cast, braced struct or tuple expression, the
    /// arguments of an extending tuple struct or variant constructor call, and the tail of an
    /// extending block.
    fn mark_extending(&mut self, init: ExprId) -> Result<(), LowerError> {
        let mut work: Vec<ExprId, A> = Vec::new_in(self.alloc);
        work.push(init);
        let mut steps = 0u32;
        while let Some(x) = work.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Construct::Bound, init);
            }
            match self.expr(x)? {
                Expr::Ref { operand, .. } => {
                    self.extended.push(operand);
                    work.push(operand);
                }
                Expr::Paren(y) | Expr::Cast { operand: y, .. } => work.push(y),
                Expr::Tuple { elems } | Expr::Array { elems } => work.extend_from_slice(self.exprs_in(elems, x)?),
                Expr::Struct { fields, .. } | Expr::Record { fields, .. } => {
                    for fi in self.field_inits_in(fields, x)? {
                        work.push(fi.expr);
                    }
                }
                Expr::Call { callee, args } if matches!(self.expr_def(callee), Some(blaze_typeck::Def::Ctor { .. })) => {
                    work.extend_from_slice(self.exprs_in(args, x)?);
                }
                Expr::Block { tail: Some(t), .. } => {
                    self.extended_blocks.push(x);
                    work.push(t);
                }
                Expr::Block { tail: None, .. } => {
                    self.extended_blocks.push(x);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Whether condition expression `cond` contains a `let` (alone or inside an `&&` chain).
    pub(crate) fn cond_has_let(&self, cond: ExprId) -> Result<bool, LowerError> {
        let mut work: Vec<ExprId, A> = Vec::new_in(self.alloc);
        work.push(cond);
        let mut steps = 0u32;
        while let Some(x) = work.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Construct::Bound, cond);
            }
            match self.expr(x)? {
                Expr::Let { .. } => return Ok(true),
                Expr::Paren(y) => work.push(y),
                Expr::Binary { op: BinOp::And, lhs, rhs } => {
                    work.push(lhs);
                    work.push(rhs);
                }
                _ => {}
            }
        }
        Ok(false)
    }

    /// Flattens `a && b && c` left to right into its condition links.
    pub(crate) fn links_of(&self, cond: ExprId) -> Result<Vec<ExprId, A>, LowerError> {
        let mut links: Vec<ExprId, A> = Vec::new_in(self.alloc);
        let mut work: Vec<ExprId, A> = Vec::new_in(self.alloc);
        work.push(cond);
        let mut steps = 0u32;
        while let Some(x) = work.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Construct::Bound, cond);
            }
            match self.expr(x)? {
                Expr::Paren(y) => work.push(y),
                Expr::Binary { op: BinOp::And, lhs, rhs } => {
                    work.push(rhs);
                    work.push(lhs);
                }
                _ => links.push(x),
            }
        }
        Ok(links)
    }

    /// `if cond { then } else { else_ }` into `dest`.
    pub(crate) fn if_into(
        &mut self,
        dest: Place,
        e: ExprId,
        cond: ExprId,
        then: ExprId,
        else_: Option<ExprId>,
    ) -> Result<(), LowerError> {
        if self.cond_has_let(cond)? {
            let links = self.links_of(cond)?;
            if !self.edition_2024()
                && let [only] = links.as_slice()
                && let Expr::Let { pat, init } = self.expr(*only)?
            {
                return self.if_let_into(dest, e, pat, init, then, else_);
            }
            return self.if_let_chain_into(dest, e, &links, then, else_);
        }
        let span = self.span(e);
        let tm = self.open();
        let c = self.operand(cond)?;
        let tb = self.b.new_block(BlockKind::Normal);
        let eb = self.b.new_block(BlockKind::Normal);
        let join = self.b.new_block(BlockKind::Normal);
        let cases = self.b.cases(&[(0, eb)]);
        self.b.terminate(StmtKind::Switch { on: c, cases, otherwise: tb }, span);
        self.b.start(tb);
        self.emit_exit(tm, span)?;
        self.lower_into(dest, then)?;
        self.goto(join, span);
        self.b.start(eb);
        self.emit_exit(tm, span)?;
        self.else_into(dest, e, else_)?;
        self.goto(join, span);
        self.pop_scope();
        self.b.start(join);
        Ok(())
    }

    fn else_into(&mut self, dest: Place, e: ExprId, else_: Option<ExprId>) -> Result<(), LowerError> {
        match else_ {
            Some(x) => self.lower_into(dest, x),
            None => {
                let ty = self.expr_ty(e)?;
                let span = self.span(e);
                self.unit(dest, ty, At::Expr(e), span)
            }
        }
    }

    /// `if let pat = init { then } else { else_ }` (pre-2024 temporary drop order: scrutinee
    /// temporaries drop after `else`).
    fn if_let_into(
        &mut self,
        dest: Place,
        e: ExprId,
        pat: PatId,
        init: ExprId,
        then: ExprId,
        else_: Option<ExprId>,
    ) -> Result<(), LowerError> {
        let span = self.span(e);
        let tm = self.open();
        let ty = self.adjusted_ty(init)?;
        let p = self.place(init)?;
        let eb = self.b.new_block(BlockKind::Normal);
        let join = self.b.new_block(BlockKind::Normal);
        self.open();
        let mut out: Vec<Binding, A> = Vec::new_in(self.alloc);
        self.match_and_bind(pat, p, ty, eb, e, &mut out)?;
        self.schedule(&out);
        self.lower_into(dest, then)?;
        self.close(span)?;
        self.emit_exit(tm, span)?;
        self.goto(join, span);
        self.b.start(eb);
        if self.edition_2024() {
            self.emit_exit(tm, span)?;
            self.else_into(dest, e, else_)?;
        } else {
            self.else_into(dest, e, else_)?;
            self.emit_exit(tm, span)?;
        }
        self.goto(join, span);
        self.pop_scope();
        self.b.start(join);
        Ok(())
    }

    /// `if let P1 = e1 && cond && let P2 = e2 { then } else { else_ }` (the Reference,
    /// "Chains of conditions"): each link evaluates left to right; any failed `let` or `false`
    /// link drops bindings and scrutinee temporaries accumulated so far in reverse order before
    /// jumping to `else_`; on full match, `then` runs with all bindings in scope and drops them
    /// in reverse order afterward.
    fn if_let_chain_into(
        &mut self,
        dest: Place,
        e: ExprId,
        links: &[ExprId],
        then: ExprId,
        else_: Option<ExprId>,
    ) -> Result<(), LowerError> {
        let span = self.span(e);
        let tm = self.open();
        let eb = self.b.new_block(BlockKind::Normal);
        let join = self.b.new_block(BlockKind::Normal);
        for &link in links {
            match self.expr(link)? {
                Expr::Let { pat, init } => {
                    let ty = self.adjusted_ty(init)?;
                    let p = self.place(init)?;
                    let link_fail = self.b.new_block(BlockKind::Normal);
                    let link_ok = self.b.new_block(BlockKind::Normal);
                    let mut out: Vec<Binding, A> = Vec::new_in(self.alloc);
                    self.match_and_bind(pat, p, ty, link_fail, e, &mut out)?;
                    self.goto(link_ok, span);
                    self.b.start(link_fail);
                    self.emit_exit(tm, span)?;
                    self.goto(eb, span);
                    self.b.start(link_ok);
                    self.schedule(&out);
                }
                _ => {
                    let c = self.operand(link)?;
                    let link_ok = self.b.new_block(BlockKind::Normal);
                    let link_fail = self.b.new_block(BlockKind::Normal);
                    let cases = self.b.cases(&[(0, link_fail)]);
                    self.b.terminate(StmtKind::Switch { on: c, cases, otherwise: link_ok }, span);
                    self.b.start(link_fail);
                    self.emit_exit(tm, span)?;
                    self.goto(eb, span);
                    self.b.start(link_ok);
                }
            }
        }
        self.lower_into(dest, then)?;
        self.emit_exit(tm, span)?;
        self.goto(join, span);
        self.pop_scope();
        self.b.start(eb);
        self.else_into(dest, e, else_)?;
        self.goto(join, span);
        self.b.start(join);
        Ok(())
    }

    /// `loop { body }` into `dest` (what its `break`s write).
    pub(crate) fn loop_into(&mut self, dest: Place, e: ExprId, body: ExprId) -> Result<(), LowerError> {
        let span = self.span(e);
        let head = self.b.new_block(BlockKind::Normal);
        self.goto(head, span);
        self.b.start(head);
        let exit = self.b.new_block(BlockKind::Normal);
        let mark = self.sched.len();
        self.targets.push(Target { expr: e, exit, head: Some(head), dest, mark });
        self.open();
        self.discard(body)?;
        self.close(span)?;
        self.goto(head, span);
        self.targets.pop();
        self.b.start(exit);
        Ok(())
    }

    fn target(&self, e: ExprId, target: ExprId) -> Result<Target, LowerError> {
        match self.targets.iter().rev().find(|t| t.expr == target) {
            Some(&t) => Ok(t),
            None => self.refuse(Construct::Target, e),
        }
    }

    /// `break 'l value`: the value into the target's place, everything the target's scope
    /// holds dropped, then to its exit.
    pub(crate) fn break_(&mut self, e: ExprId, value: Option<ExprId>, target: ExprId) -> Result<(), LowerError> {
        let span = self.span(e);
        let t = self.target(e, target)?;
        match value {
            Some(v) => self.lower_into(t.dest, v)?,
            None => {
                let ty = self.expr_ty(target)?;
                self.unit(t.dest, ty, At::Expr(target), span)?;
            }
        }
        self.emit_exit(t.mark, span)?;
        self.goto(t.exit, span);
        self.dead_block();
        Ok(())
    }

    /// `continue 'l`: the loop body's scope dropped, then to the loop's head.
    pub(crate) fn continue_(&mut self, e: ExprId, target: ExprId) -> Result<(), LowerError> {
        let span = self.span(e);
        let t = self.target(e, target)?;
        let Some(head) = t.head else { return self.refuse(Construct::Target, e) };
        self.emit_exit(t.mark, span)?;
        self.goto(head, span);
        self.dead_block();
        Ok(())
    }

    /// `return value`: the value into `_0`, everything live dropped (parameters last), then
    /// `return`.
    pub(crate) fn return_(&mut self, e: ExprId, value: Option<ExprId>) -> Result<(), LowerError> {
        let span = self.span(e);
        let ret = self.ret;
        match value {
            Some(v) => self.lower_into(ret, v)?,
            None => {
                let ty = self.ret_ty;
                self.unit(ret, ty, At::Expr(e), span)?;
            }
        }
        self.emit_exit(0, span)?;
        self.b.terminate(StmtKind::Return, span);
        self.dead_block();
        Ok(())
    }
}
