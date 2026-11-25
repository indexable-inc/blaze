//! Borrowed temporaries whose type needs drop: `f(&make())`, `make().method()` by autoref. The
//! Reference, "Destructors", "Temporary scopes": "the temporary scope of an expression is the
//! smallest scope that contains the expression and is one of: The entire function. A
//! statement. The body of an `if`, `while` or `loop` expression. The `else` block of an `if`
//! expression. The non-pattern matching condition expression of an `if` or `while` expression,
//! or a `match` guard. The body expression for a match arm. Each operand of a lazy boolean
//! expression. ... The entirety of the tail expression of a block"; "Drop scopes": "When
//! control flow leaves a drop scope all variables associated to that scope are dropped in
//! reverse order of declaration"; "Temporary lifetime extension": "The temporary scopes for
//! expressions in `let` statements are sometimes extended to the scope of the block containing
//! the `let` statement", for "the operand of an extending borrow expression" (`let x = &temp;`,
//! through extending tuples, arrays, casts, braced structs and block tails).
//!
//! Built here, every temporary scope of that list (`scan_temp_scopes`): a STATEMENT (an
//! expression statement or a `let` initializer, through calls, method calls, operators, a
//! `match` scrutinee: "The scrutinee of a `match` expression is not a temporary scope, so
//! temporaries in the scrutinee can be dropped after the `match` expression"), and, each its
//! own scope entered at its `Task::Eval` (`temp_enter`) and closed by `Task::TempScopeEnd`
//! right after its value: an `if` or `while` condition ("The non-pattern matching condition
//! expression of an `if` or `while` expression": dropped before the branch runs), an `if` body
//! and `else` block, a loop body (once per iteration, as the body's code runs once per
//! iteration), each operand of `&&` and `||`, a guard, an arm's body, an edition-2024 block's
//! tail ("The entirety of the tail expression of a block", 2024: "temporaries of tail
//! expressions of blocks are dropped immediately after the tail expression is evaluated", so
//! before the block's locals), the function's body ("The entire function": an edition-2021
//! tail's temporaries outlive the body block's locals), and a closure's body (its own
//! function). A temporary joins its scope's entries as a whole value (`Held::Rest`) at that
//! scope's own end of `scope_pats` (before any scope opened inside it: an edition-2021 tail's
//! temporary outlives its block's locals), and the scope's end drops them in reverse order of
//! creation, after the scope's value (and a `let`'s binding); a `return`, `break` or
//! `continue` drops them with the scopes it leaves (`drop_from`). Since every branch of an
//! expression starts a temporary scope of its own, an ordinary temporary never needs a flag.
//!
//! An EXTENDED temporary (`extended_by`) is a hidden local declared just before its `let`'s
//! bindings (`Held::Temp`), dropped at its block's end in reverse order with the locals; one
//! made under an extending `if` branch or `match` arm exists only if that branch ran: its
//! address goes in a slot and a drop flag (`ext_flags_for`, cleared at the `let`'s start)
//! says whether it was made.

use core::alloc::Allocator;

use blaze_lir::builder::MemAttrs;
use blaze_lir::func::{SlotId, SlotKind};
use blaze_lir::{Flags, IntCond, Op, Ty, Value};
use blaze_own::Vec;
use blaze_typeck::{BinOp, CtorKind, Def, Edition, Expr, ExprId, Stmt, UnOp};

use super::lets::scope::LetAnchor;
use super::{Held, Home, LocalOwn, NO_EXPR, Repr, Task, V, Walk, malformed, unsupported};
use crate::BuildError;

/// `Walk::temp_here` per expression: not a temporary scope of its own.
pub(super) const NOT_SCOPE: u8 = 0;
/// A temporary scope of its own (`scan_temp_scopes`), not yet entered.
const SCOPE: u8 = 1;
/// Such a scope, entered: its walk is under way or done (each expression is walked once).
const OPENED: u8 = 2;

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Temporary `e` (its value at `at`, held as `repr`, which needs drop), borrowed: an
    /// extended one joins its `let`'s block (`extended_by`), any other its temporary scope
    /// (`temp_anchor`); each is dropped at that scope's end.
    pub(super) fn hold_temp(&mut self, e: ExprId, at: (Value, i32), repr: Repr) -> Result<(), BuildError> {
        let Repr::Agg(l) = repr else { return Err(malformed("a value that needs drop not held as an aggregate")) };
        // In a laid coroutine state: the temporary is the flow's local of `e` (walk/state_names.rs).
        self.name_at(at, blaze_flow_lower::identity::Identity::Temp(e, blaze_flow_lower::identity::Role::Value));
        if let Some((borrow, init, branch)) = self.extended_by(e)? {
            // "The temporary scopes for expressions in `let` statements are sometimes extended
            // to the scope of the block containing the `let` statement": a hidden local
            // declared just before the `let`'s bindings, so dropped right after them ("in
            // reverse order of declaration").
            let t = self.temp_scope_index(init).ok_or(malformed("an extended temporary whose `let` is not open"))?;
            let flag = if branch {
                // Made under an `if` branch or a `match` arm: its address kept, its flag set.
                let flag = self.ext_flags.iter().find(|&&(b, _)| b == borrow).map(|&(_, s)| s);
                let flag = flag.ok_or(malformed("an extended temporary under a branch without its flag"))?;
                let home = self.b.slot(8, 3, SlotKind::Temp);
                let p = self.address(at);
                let ha = self.b.slot_addr(home);
                self.b.store(Flags::NONE, p, ha, 0, MemAttrs::aligned(3));
                self.store_flag(flag, true);
                Some((flag, home))
            } else {
                None
            };
            let pos = *self.temp_marks.get(t).ok_or(malformed("a temporary scope past `temp_marks`"))? as usize;
            let depth = self.temp_scopes.get(t).ok_or(malformed("a temporary scope past `temp_scopes`"))?.1 as usize;
            self.insert_held(pos, t, depth, Held::Temp { at, l, flag });
            return Ok(());
        }
        let anchor = self.temp_anchor(e)?;
        let t = self.temp_scope_index(anchor).ok_or(malformed("a temporary's scope is not open"))?;
        let depth = self.temp_scopes.get(t).ok_or(malformed("a temporary scope past `temp_scopes`"))?.1 as usize;
        // The end of scope `t`'s own entries: before the first scope opened inside it.
        let mut pos = self.scope_pats.len();
        if let Some(&m) = self.scope_marks.get(depth) {
            pos = pos.min(m as usize);
        }
        if let Some(&m) = self.temp_marks.get(t + 1) {
            pos = pos.min(m as usize);
        }
        self.insert_held(pos, t + 1, depth, Held::Rest { at, l, variant: None, moved: 0 });
        Ok(())
    }

    /// Puts `h` at `pos` of `scope_pats`; the scopes opened after it (the temporary scopes from
    /// `temps` on, the drop scopes from `depth` on), which all start at or past `pos`, move up
    /// by one.
    pub(super) fn insert_held(&mut self, pos: usize, temps: usize, depth: usize, h: Held) {
        self.scope_pats.insert(pos, h);
        for m in self.temp_marks.iter_mut().skip(temps) {
            if *m as usize >= pos {
                *m += 1;
            }
        }
        for m in self.scope_marks.iter_mut().skip(depth) {
            if *m as usize >= pos {
                *m += 1;
            }
        }
    }

    /// The innermost open temporary scope anchored at expression `anchor`.
    pub(super) fn temp_scope_index(&self, anchor: u32) -> Option<usize> {
        self.temp_scopes.iter().rposition(|&(a, _)| a == anchor)
    }

    /// The temporary scope of temporary `e`, not extended: the smallest expression up from it
    /// that is a temporary scope of its own (`temp_here`) or a statement's (an expression
    /// statement's expression, a `let`'s initializer). An edition-2021 block tail is not one:
    /// the walk goes on past its block.
    pub(super) fn temp_anchor(&self, e: ExprId) -> Result<u32, BuildError> {
        let body = self.body;
        let mut c = e.0;
        loop {
            if self.temp_here.get(c as usize).copied().ok_or(malformed("an expression past `exprs`"))? != NOT_SCOPE {
                return Ok(c);
            }
            let (up, cond) = *self.expr_parent.get(c as usize).ok_or(malformed("an expression past `exprs`"))?;
            if up == NO_EXPR {
                return Err(unsupported(e, "a borrowed temporary whose type needs drop outside a statement"));
            }
            let parent = self.expr(ExprId(up))?;
            if let Expr::Let { .. } = parent {
                // "The pattern-matching condition(s) and consequent body of `if`" (edition
                // 2024): the condition's region walk/lets/scope.rs opens. Before 2024 the
                // `if let` was a `match`, whose scrutinee is not a temporary scope: the search
                // goes on from the `if`.
                match self.let_anchor(ExprId(up))? {
                    LetAnchor::Cond(a) => return Ok(a),
                    LetAnchor::Above(iff) => {
                        c = iff.0;
                        continue;
                    }
                }
            }
            if let Expr::Block { stmts, .. } = parent {
                let ss = body.stmt_lists.get(stmts.range()).ok_or(malformed("a block's statements past `stmt_lists`"))?;
                for &s in ss {
                    match body.stmts.get(s.ix()).copied().ok_or(malformed("a statement past `stmts`"))? {
                        Stmt::Expr { expr, .. } if expr.0 == c => return Ok(c),
                        Stmt::Let { init: Some(i), .. } if i.0 == c => return Ok(c),
                        _ => {}
                    }
                }
            }
            if cond {
                // Every branch the walk builds starts a temporary scope (`scan_temp_scopes`);
                // what is left (a `let`-`else` block, an async block) is not built here.
                return Err(unsupported(
                    e,
                    "a borrowed temporary whose type needs drop under a branch that is not a temporary scope (a `let`-`else` block, an async block)",
                ));
            }
            c = up;
        }
    }

    /// Whether temporary `e` is extended, by the Reference's "Extending based on expressions":
    /// "For a `let` statement with an initializer, an extending expression is an expression
    /// which is one of the following: The initializer expression. The operand of an extending
    /// borrow expression. ... The operand(s) of an extending array, cast, braced struct, or
    /// tuple expression. The arguments to an extending tuple struct or tuple enum variant
    /// constructor expression. The final expression of an extending block expression except for
    /// an async block expression. The final expression of an extending `if` expression's
    /// consequent, `else if`, or `else` block. An arm expression of an extending `match`
    /// expression"; "The operand of an extending borrow expression has its temporary scope
    /// extended"; and "If a borrow, dereference, field, or tuple indexing expression has an
    /// extended temporary scope, then so does its operand. If an indexing expression has an
    /// extended temporary scope, then the indexed expression also has an extended temporary
    /// scope." `Some((borrow, init, branch))`: the extending borrow, the `let`'s initializer,
    /// and whether an `if` branch or a `match` arm lies between (its drop needs a flag).
    fn extended_by(&self, e: ExprId) -> Result<Option<(u32, u32, bool)>, BuildError> {
        let up_of = |c: u32| -> Result<u32, BuildError> {
            Ok(self.expr_parent.get(c as usize).ok_or(malformed("an expression past `exprs`"))?.0)
        };
        // Up through the operands that share an extended scope, to the outermost borrow.
        let mut c = e.0;
        let mut borrow = None;
        loop {
            let up = up_of(c)?;
            if up == NO_EXPR {
                return Ok(None);
            }
            match self.expr(ExprId(up))? {
                Expr::Ref { .. } => borrow = Some(up),
                Expr::Paren(_) => {}
                Expr::Unary { op: UnOp::Deref, .. } => borrow = None,
                Expr::Field { base, .. } | Expr::TupleField { base, .. } | Expr::Index { base, .. } if base.0 == c => borrow = None,
                _ => break,
            }
            c = up;
        }
        let Some(borrow) = borrow else { return Ok(None) };
        Ok(self.extending_init(borrow)?.map(|(init, branch)| (borrow, init, branch)))
    }

    /// Up from expression `from` through extending expressions (the Reference's "Extending
    /// based on expressions", as `extended_by` quotes it), to a `let`'s initializer:
    /// `Some((init, branch))`, `branch` when an `if` branch or a `match` arm lies between; `None`
    /// when `from` is not extending. Also the rule of a `super let` binding's scope, with `from`
    /// its block (walk/super_let.rs).
    pub(super) fn extending_init(&self, from: u32) -> Result<Option<(u32, bool)>, BuildError> {
        let body = self.body;
        let up_of = |c: u32| -> Result<u32, BuildError> {
            Ok(self.expr_parent.get(c as usize).ok_or(malformed("an expression past `exprs`"))?.0)
        };
        let mut c = from;
        let mut branch = false;
        loop {
            let up = up_of(c)?;
            if up == NO_EXPR {
                return Ok(None);
            }
            match self.expr(ExprId(up))? {
                Expr::Ref { .. } | Expr::Paren(_) | Expr::Cast { .. } | Expr::Tuple { .. } | Expr::Array { .. } => {}
                Expr::Struct { base, .. } | Expr::Record { base, .. } if base.expr().map(|b| b.0) != Some(c) => {}
                Expr::Call { callee, .. }
                    if callee.0 != c && matches!(self.expr_def(callee), Some(Def::Ctor { kind: CtorKind::Tuple, .. })) => {}
                Expr::Block { stmts, tail, .. } => {
                    if tail.map(|t| t.0) != Some(c) {
                        let ss = body.stmt_lists.get(stmts.range()).ok_or(malformed("a block's statements past `stmt_lists`"))?;
                        for &s in ss {
                            if let Stmt::Let { init: Some(i), else_: None, .. } =
                                body.stmts.get(s.ix()).copied().ok_or(malformed("a statement past `stmts`"))?
                                && i.0 == c
                            {
                                return Ok(Some((c, branch)));
                            }
                        }
                        return Ok(None);
                    }
                }
                Expr::If { cond, .. } if cond.0 != c => branch = true,
                Expr::Match { arms, .. } => {
                    let arms = body.arms.get(arms.range()).ok_or(malformed("a match's arms past `arms`"))?;
                    if !arms.iter().any(|a| a.body.0 == c) {
                        return Ok(None);
                    }
                    branch = true;
                }
                _ => return Ok(None),
            }
            c = up;
        }
    }

    /// At the start of `let` initializer `init`: a cleared drop flag for each extending borrow
    /// under an `if` branch or a `match` arm (the downward walk of `extended_by`'s rules), so
    /// the flag is clear on every path that does not make the temporary.
    fn ext_flags_for(&mut self, init: ExprId) -> Result<(), BuildError> {
        if !self.live {
            return Ok(());
        }
        let body = self.body;
        let mut todo: Vec<(ExprId, bool), A> = Vec::new_in(self.alloc);
        todo.push((init, false));
        while let Some((x, under)) = todo.pop() {
            match self.expr(x)? {
                Expr::Ref { operand, .. } => {
                    if under && !self.ext_flags.iter().any(|&(b, _)| b == x.0) {
                        let slot = self.b.slot(1, 0, SlotKind::Named);
                        self.ext_flags.push((x.0, slot));
                        self.store_flag(slot, false);
                    }
                    todo.push((operand, under));
                }
                Expr::Paren(i) | Expr::Cast { operand: i, .. } => todo.push((i, under)),
                Expr::Tuple { elems } | Expr::Array { elems } => {
                    let l = body.expr_lists.get(elems.range()).ok_or(malformed("a list past `expr_lists`"))?;
                    todo.extend(l.iter().map(|&c| (c, under)));
                }
                Expr::Struct { fields, .. } | Expr::Record { fields, .. } => {
                    let inits = body.field_inits.get(fields.range()).ok_or(malformed("a struct literal's fields past `field_inits`"))?;
                    todo.extend(inits.iter().map(|f| (f.expr, under)));
                }
                Expr::Call { callee, args } if matches!(self.expr_def(callee), Some(Def::Ctor { kind: CtorKind::Tuple, .. })) => {
                    let l = body.expr_lists.get(args.range()).ok_or(malformed("a list past `expr_lists`"))?;
                    todo.extend(l.iter().map(|&c| (c, under)));
                }
                Expr::Block { tail: Some(t), .. } => todo.push((t, under)),
                Expr::If { then, else_, .. } => {
                    todo.push((then, true));
                    todo.extend(else_.map(|x| (x, true)));
                }
                Expr::Match { arms, .. } => {
                    let arms = body.arms.get(arms.range()).ok_or(malformed("a match's arms past `arms`"))?;
                    todo.extend(arms.iter().map(|a| (a.body, true)));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Stores `on` into drop flag `slot`.
    fn store_flag(&mut self, slot: SlotId, on: bool) {
        let at = self.b.slot_addr(slot);
        let v = self.b.iconst(Ty::I8, u128::from(on));
        self.b.store(Flags::NONE, v, at, 0, MemAttrs::aligned(0));
    }

    /// Drops an extended temporary (`Held::Temp`): at `at`, or, made under a branch, at the
    /// address in its home slot if its flag is set (cleared on the way).
    pub(super) fn drop_temp(&mut self, at: (Value, i32), l: u32, flag: Option<(SlotId, SlotId)>) -> Result<(), BuildError> {
        let Some((flag, home)) = flag else { return self.drop_rest(at, l, None, 0) };
        let fa = self.b.slot_addr(flag);
        let f = self.b.load(Ty::I8, Flags::NONE, fa, 0, MemAttrs::aligned(0));
        let zero = self.b.iconst(Ty::I8, 0);
        let set = self.b.icmp(IntCond::Ne, f, zero);
        let yes = self.create();
        let join = self.create();
        self.br(set, (yes, &[]), (join, &[]));
        self.begin(yes);
        let ha = self.b.slot_addr(home);
        let p = self.b.load(Ty::PTR, Flags::NONE, ha, 0, MemAttrs::aligned(3));
        self.drop_rest((p, 0), l, None, 0)?;
        self.store_flag(flag, false);
        self.jump(join, &[]);
        self.begin(join);
        Ok(())
    }

    /// Which expressions are temporary scopes of their own (the Reference's list, quoted at the
    /// top of this file); a statement's are opened by `stmt_temps`. After `scan_flags`.
    pub(super) fn scan_temp_scopes(&mut self) -> Result<(), BuildError> {
        let body = self.body;
        self.scope_at(body.root)?;
        for x in body.exprs.iter() {
            match *x {
                Expr::If { cond, then, else_ } => {
                    self.scope_at(cond)?;
                    self.scope_at(then)?;
                    if let Some(x) = else_ {
                        self.scope_at(x)?;
                    }
                }
                Expr::Loop { body: b } | Expr::Closure { body: b, .. } => self.scope_at(b)?,
                Expr::Binary { op: BinOp::And | BinOp::Or, lhs, rhs } => {
                    self.scope_at(lhs)?;
                    self.scope_at(rhs)?;
                }
                Expr::Match { arms, .. } => {
                    for arm in body.arms.get(arms.range()).ok_or(malformed("a match's arms past `arms`"))? {
                        if let Some(g) = arm.guard {
                            self.scope_at(g)?;
                        }
                        self.scope_at(arm.body)?;
                    }
                }
                Expr::Block { tail: Some(t), .. } if body.edition >= Edition::E2024 => self.scope_at(t)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn scope_at(&mut self, x: ExprId) -> Result<(), BuildError> {
        *self.temp_here.get_mut(x.ix()).ok_or(malformed("a temporary scope past `exprs`"))? = SCOPE;
        Ok(())
    }

    /// `Task::Eval(e)`: when `e` is a temporary scope not yet entered, opens it, queues
    /// `e`'s evaluation inside it and its end after, and says the caller is done.
    pub(super) fn temp_enter(&mut self, e: ExprId) -> Result<bool, BuildError> {
        let s = self.temp_here.get_mut(e.ix()).ok_or(malformed("an expression past `exprs`"))?;
        if *s != SCOPE {
            return Ok(false);
        }
        *s = OPENED;
        self.open_temps(e.0);
        self.work.push(Task::TempScopeEnd(e));
        self.work.push(Task::Eval(e));
        Ok(true)
    }

    fn open_temps(&mut self, anchor: u32) {
        self.temp_marks.push(self.scope_pats.len() as u32);
        self.temp_scopes.push((anchor, self.scope_marks.len() as u32));
    }

    /// The start of statement `stmt`: its temporary scope (ended by `Task::TempEnd`, after its
    /// value and its `let`'s binding), and a `let`'s extended temporaries' flags.
    pub(super) fn stmt_temps(&mut self, stmt: Stmt) -> Result<(), BuildError> {
        let anchor = match stmt {
            Stmt::Expr { expr, .. } => expr.0,
            Stmt::Let { init: Some(i), .. } => i.0,
            _ => NO_EXPR,
        };
        self.open_temps(anchor);
        self.work.push(Task::TempEnd);
        if let Stmt::Let { init: Some(i), else_: None, .. } = stmt {
            self.ext_flags_for(i)?;
        }
        Ok(())
    }

    /// `Task::TempScopeEnd(e)`: temporary scope `e`'s value is made; its temporaries dropped
    /// under it (an aggregate place copied out first, as it may be in one of them).
    pub(super) fn temp_close(&mut self, e: ExprId) -> Result<(), BuildError> {
        if self.temp_scopes.last().map(|&(a, _)| a) != Some(e.0) {
            return Err(malformed("a temporary scope's end out of order"));
        }
        let mark = *self.temp_marks.last().ok_or(malformed("a temporary scope's end out of order"))? as usize;
        if self.scope_pats.len() > mark {
            let v = self.pop()?;
            let repr = self.adjusted_repr(e)?;
            let v = self.settle(v, repr, mark)?;
            self.push_val(v, None);
        }
        self.temp_end()
    }

    /// The end of a temporary scope: its temporaries (`Held::Rest` past its mark) dropped in
    /// reverse order of creation; the bindings its `let` made, and extended temporaries
    /// (`Held::Temp`, its block's), stay.
    pub(super) fn temp_end(&mut self) -> Result<(), BuildError> {
        let depth = self.temp_scopes.len().saturating_sub(1);
        let mark = self.temp_marks.pop().ok_or(malformed("a statement's temporaries out of order"))? as usize;
        self.temp_scopes.pop().ok_or(malformed("a statement's temporaries out of order"))?;
        let mut i = self.scope_pats.len();
        while i > mark {
            i -= 1;
            let h = self.scope_pats[i];
            // `Held::Moved`: a temporary scrutinee a match by tests moves out of (walk/moves.rs).
            if let Held::Rest { at, .. } = h {
                self.drop_held(h, true)?;
                self.scope_pats.remove(i);
                if let Some(slot) = self.whole_slot(at) {
                    self.end_temp_slot(slot);
                }
            } else if let Held::Moved { at, mask, .. } = h {
                self.drop_held(h, true)?;
                self.scope_pats.remove(i);
                if self.live {
                    self.b.slot_dead(mask);
                    self.release_dead_slot(mask);
                }
                if let Some(slot) = self.whole_slot(at) {
                    self.end_temp_slot(slot);
                }
            } else if self.super_let_ends_here(h)? {
                // A `super let` binding of a block in this scope (walk/super_let.rs).
                self.scope_pats.remove(i);
            }
        }
        // End non-dropping temporary slots (and non-dropping `super let` binding slots)
        // registered in this temporary scope.
        let mut j = self.temp_slots.len();
        while j > 0 {
            j -= 1;
            if self.temp_slots.get(j).is_some_and(|&(d, _)| d >= depth) {
                let (_, slot) = self.temp_slots.remove_swap(j);
                if self.is_temp_slot(slot) {
                    self.end_temp_slot(slot);
                } else if self.live && !self.vals_use_slot(slot) && !self.slot_held(slot) {
                    self.b.slot_dead(slot);
                }
            }
        }
        Ok(())
    }

    /// Whether `slot` is a `SlotKind::Temp` in the current LIR function.
    pub(super) fn is_temp_slot(&self, slot: SlotId) -> bool {
        self.b
            .func()
            .view()
            .slots
            .get(slot.index())
            .is_some_and(|s| SlotKind::from_byte(s.kind) == Some(SlotKind::Temp))
    }

    /// Registers a non-dropping temporary slot (`SlotKind::Temp`) created for expression `e`
    /// (a borrowed copy temporary or `format_args!` argument array) so its storage ends when
    /// its temporary (or extended `let` block) scope closes, and names it if the flow saves `e`.
    pub(super) fn hold_copy_temp(&mut self, e: ExprId, at: (Value, i32)) -> Result<(), BuildError> {
        let Some(slot) = self.whole_slot(at) else { return Ok(()) };
        if !self.is_temp_slot(slot) {
            return Ok(());
        }
        self.name_slot(slot, blaze_flow_lower::identity::Identity::Temp(e, blaze_flow_lower::identity::Role::Value));
        if self.co.is_none() {
            return Ok(());
        }
        if let Some((_, init, _)) = self.extended_by(e)? {
            if let Some(t) = self.temp_scope_index(init)
                && let Some(&(_, depth)) = self.temp_scopes.get(t)
                && let Ok(d) = usize::try_from(depth)
            {
                self.scope_slots.push((d, slot));
            }
            return Ok(());
        }
        if let Ok(anchor) = self.temp_anchor(e)
            && let Some(t) = self.temp_scope_index(anchor)
        {
            self.temp_slots.push((t, slot));
        }
        Ok(())
    }

    /// The slot `p` addresses (at any byte offset), if `p` is a `slot_addr`.
    pub(super) fn value_slot(&self, p: Value) -> Option<SlotId> {
        let view = self.b.func().view();
        let ix = p.index();
        (ix < view.len() && view.op(ix) == Op::SlotAddr).then(|| view.data.get(ix).copied().map(SlotId)).flatten()
    }

    /// Whether any value currently on the walk's value stack addresses `slot`.
    fn vals_use_slot(&self, slot: SlotId) -> bool {
        self.vals_slot_count(slot) > 0
    }

    /// How many entries currently on the walk's value stack address `slot`.
    pub(super) fn vals_slot_count(&self, slot: SlotId) -> usize {
        self.vals
            .iter()
            .filter(|&&v| match v {
                V::Place(p, _) | V::Unsized(p, _) | V::Scalar(p) => self.value_slot(p) == Some(slot),
                V::None => false,
            })
            .count()
    }

    /// Whether `slot` is owned by an entry currently in `scope_pats`.
    pub(super) fn slot_held(&self, slot: SlotId) -> bool {
        self.scope_pats.iter().any(|&h| match h {
            Held::Rest { at, .. } => self.value_slot(at.0) == Some(slot),
            Held::Moved { at, mask, .. } => mask == slot || self.value_slot(at.0) == Some(slot),
            Held::Temp { at, flag, .. } => {
                self.value_slot(at.0) == Some(slot)
                    || flag.is_some_and(|(f, home)| f == slot || home == slot)
            }
            Held::Pat(p) => {
                let home_hit = match self.slots.get(p.ix()).copied().flatten() {
                    Some((Home::Slot(s), _)) => s == slot,
                    Some((Home::At(v, _), _)) => self.value_slot(v) == Some(slot),
                    Some((Home::Indirect(..), _)) | None => false,
                };
                let own_hit = match self.owned.get(p.ix()).copied() {
                    Some(LocalOwn::Drop(o)) => o.flag == Some(slot) || o.rest == Some(slot),
                    Some(LocalOwn::KeptFlags(s)) => s == slot,
                    _ => false,
                };
                home_hit || own_hit
            }
            Held::EnvPlace(_) => false,
        })
    }

    /// Whether `slot` is bound as any local's home (even a non-dropping local not in `scope_pats`).
    pub(super) fn slot_is_local(&self, slot: SlotId) -> bool {
        self.slots.iter().any(|s| match *s {
            Some((Home::Slot(s), _)) => s == slot,
            Some((Home::At(v, _), _)) => self.value_slot(v) == Some(slot),
            Some((Home::Indirect(..), _)) | None => false,
        })
    }

    /// Ends `slot` (`slot_dead`) when it is a `SlotKind::Temp` in live code that nothing on the
    /// value stack, in an open drop scope, or in a local binding still holds.
    pub(super) fn end_temp_slot(&mut self, slot: SlotId) {
        if !self.live {
            return;
        }
        if !self.is_temp_slot(slot) {
            return;
        }
        if self.vals_use_slot(slot) || self.slot_held(slot) || self.slot_is_local(slot) {
            return;
        }
        self.b.slot_dead(slot);
        self.release_dead_slot(slot);
    }

    /// Before an `.await`'s suspension loop: ends the storage of:
    /// 1. any statically moved-out dropping local binding (either unflagged with `!self.init[p]`,
    ///    or flagged with path-sensitive `self.definitely_uninit[p]`), plus its drop flag slot when
    ///    the flow does not save `p` (so `drop_local` skips it on the cancel path too),
    /// 2. in a laid coroutine state (`self.names.is_some()`), any non-dropping (`LocalOwn::None`
    ///    or `LocalOwn::Kept`) local binding whose `Identity::Binding(p)` is NOT saved by
    ///    `blaze-flow::saved()` and has no remaining unwalked uses, and
    /// 3. in a laid coroutine state, any unnamed `SlotKind::Temp` slot unreachable from every
    ///    live root of `Walk` (completed pre-await temporaries whose storage never explicitly
    ///    ended before the suspension).
    pub(super) fn end_dead_slots_before_await(&mut self, fut_slot: Option<SlotId>) {
        if !self.live {
            return;
        }
        for i in 0..self.scope_pats.len() {
            let Some(&Held::Pat(p)) = self.scope_pats.get(i) else { continue };
            if self.captured_whole(p) {
                continue;
            }
            let Some(LocalOwn::Drop(o)) = self.owned.get(p.ix()).copied() else { continue };
            if o.rest.is_some() {
                continue;
            }
            let moved_out = match o.flag {
                None => !self.init.get(p.ix()).copied().unwrap_or(false),
                Some(_) => self.definitely_uninit.get(p.ix()).copied().unwrap_or(false),
            };
            if !moved_out {
                continue;
            }
            if let Some((Home::Slot(slot), _)) = self.slots.get(p.ix()).copied().flatten()
                && !self.vals_use_slot(slot)
                && !self.slot_is_named(slot)
            {
                self.b.slot_dead(slot);
            }
            if let Some(flag_slot) = o.flag
                && self.names.is_some()
                && !self.binding_is_saved(p)
                && !self.slot_is_named(flag_slot)
            {
                self.b.slot_dead(flag_slot);
            }
        }
        if self.names.is_some() {
            for ix in 0..self.slots.len() {
                let Some((Home::Slot(slot), _)) = self.slots.get(ix).copied().flatten() else {
                    continue;
                };
                if !matches!(self.owned.get(ix), Some(LocalOwn::None | LocalOwn::Kept)) {
                    continue;
                }
                if self.uses_left.get(ix).copied().unwrap_or(1) != 0 {
                    continue;
                }
                let Ok(ix_u32) = u32::try_from(ix) else {
                    continue;
                };
                let p = blaze_typeck::PatId(ix_u32);
                if self.captured_whole(p)
                    || self.binding_is_saved(p)
                    || self.slot_is_named(slot)
                    || self.vals_use_slot(slot)
                    || self.slot_held(slot)
                {
                    continue;
                }
                self.b.slot_dead(slot);
            }
            // Live-root sweep over unnamed `SlotKind::Temp` slots: any temp slot not referenced
            // by any live walk state or named in `StateNames` cannot be read after this suspend.
            let num_slots = self.b.func().view().slots.len();
            let mut keep: Vec<bool, A> = Vec::with_capacity_in(num_slots, self.alloc);
            keep.resize(num_slots, false);
            let mut mark = |slot: SlotId| {
                if let Some(b) = keep.get_mut(slot.index()) {
                    *b = true;
                }
            };
            if let Some(s) = fut_slot {
                mark(s);
            }
            if let Some(s) = self.co.and_then(|c| c.pin) {
                mark(s);
            }
            for &v in self.vals.iter() {
                if let V::Place(p, _) | V::Unsized(p, _) | V::Scalar(p) = v
                    && let Some(s) = self.value_slot(p)
                {
                    mark(s);
                }
            }
            for held_list in [&self.scope_pats, &self.let_else_held] {
                for &h in held_list.iter() {
                    match h {
                        Held::Rest { at, .. } => {
                            if let Some(s) = self.value_slot(at.0) {
                                mark(s);
                            }
                        }
                        Held::Moved { at, mask, .. } => {
                            mark(mask);
                            if let Some(s) = self.value_slot(at.0) {
                                mark(s);
                            }
                        }
                        Held::Temp { at, flag, .. } => {
                            if let Some(s) = self.value_slot(at.0) {
                                mark(s);
                            }
                            if let Some((f, home)) = flag {
                                mark(f);
                                mark(home);
                            }
                        }
                        Held::Pat(_) | Held::EnvPlace(_) => {}
                    }
                }
            }
            for s in self.slots.iter().flatten() {
                match s.0 {
                    Home::Slot(slot) => mark(slot),
                    Home::At(v, _) => {
                        if let Some(slot) = self.value_slot(v) {
                            mark(slot);
                        }
                    }
                    Home::Indirect(..) => {}
                }
            }
            for &own in self.owned.iter() {
                match own {
                    LocalOwn::Drop(o) => {
                        if let Some(f) = o.flag {
                            mark(f);
                        }
                        if let Some(r) = o.rest {
                            mark(r);
                        }
                    }
                    LocalOwn::KeptFlags(r) => mark(r),
                    _ => {}
                }
            }
            for &(_, s) in self.temp_slots.iter() {
                mark(s);
            }
            for &(_, s) in self.scope_slots.iter() {
                mark(s);
            }
            for &(_, s) in self.ext_flags.iter() {
                mark(s);
            }
            for t in self.targets.iter() {
                if let Some(s) = t.res {
                    mark(s);
                }
            }
            for m in self.matches.iter() {
                if let Some(s) = m.res {
                    mark(s);
                }
                if let Some((at, ..)) = m.of_enum
                    && let Some(s) = self.value_slot(at.0)
                {
                    mark(s);
                }
                if let Some(at) = m.tested
                    && let Some(s) = self.value_slot(at.at.0)
                {
                    mark(s);
                }
            }
            for task in self.work.iter() {
                match *task {
                    Task::IfThen { res: Some(s), .. } | Task::IfElse { res: Some(s), .. } => mark(s),
                    Task::GuardAlts { word, .. } => mark(word),
                    _ => {}
                }
            }
            self.moves.for_each_live_slot(&mut mark, |v| self.value_slot(v));
            for (i, &kept) in keep.iter().enumerate() {
                if kept {
                    continue;
                }
                let Ok(id) = u32::try_from(i) else { continue };
                let slot = SlotId(id);
                if self.is_temp_slot(slot) && !self.slot_is_named(slot) {
                    self.b.slot_dead(slot);
                }
            }
        }
    }

    /// Ends the `SlotKind::Temp` slot underlying `v`, if `v` is a whole-slot place and unused.
    pub(super) fn end_temp_val(&mut self, v: V) {
        if let V::Place(p, off) = v
            && let Some(slot) = self.whole_slot((p, off))
        {
            self.end_temp_slot(slot);
        }
    }

    /// Collects the slots owned by `self.scope_pats[mark..]` into `dead`.
    fn collect_held_slots(&self, mark: usize, dead: &mut Vec<SlotId, A>) {
        let Some(held) = self.scope_pats.get(mark..) else { return };
        for &h in held {
            match h {
                Held::Pat(p) => {
                    match self.slots.get(p.ix()).copied().flatten() {
                        Some((Home::Slot(s), _)) => dead.push(s),
                        Some((Home::At(val, off), _)) => {
                            if let Some(s) = self.whole_slot((val, off)) {
                                dead.push(s);
                            }
                        }
                        Some((Home::Indirect(..), _)) | None => {}
                    }
                    match self.owned.get(p.ix()).copied() {
                        Some(LocalOwn::Drop(o)) => {
                            if let Some(f) = o.flag {
                                dead.push(f);
                            }
                            if let Some(r) = o.rest {
                                dead.push(r);
                            }
                        }
                        Some(LocalOwn::KeptFlags(r)) => dead.push(r),
                        _ => {}
                    }
                }
                Held::Rest { at, .. } => {
                    if let Some(s) = self.whole_slot(at)
                        && self.is_temp_slot(s)
                    {
                        dead.push(s);
                    }
                }
                Held::Moved { at, mask, .. } => {
                    dead.push(mask);
                    if let Some(s) = self.whole_slot(at)
                        && self.is_temp_slot(s)
                    {
                        dead.push(s);
                    }
                }
                Held::Temp { at, flag, .. } => {
                    if let Some(s) = self.whole_slot(at)
                        && self.is_temp_slot(s)
                    {
                        dead.push(s);
                    }
                    if let Some((f, home)) = flag {
                        dead.push(f);
                        dead.push(home);
                    }
                }
                Held::EnvPlace(_) => {}
            }
        }
    }

    /// Emits `slot_dead` on slots owned by `self.scope_pats[mark..]`, `self.scope_slots` at
    /// depth `>= depth`, and `self.temp_slots` whose enclosing drop depth is `>= depth`, WITHOUT
    /// modifying `scope_pats`, `scope_slots`, or `temp_slots` (for a failed `if let` link,
    /// `let`-`else` miss path, `break`, or `continue`).
    pub(super) fn end_scope_slots_peek(&mut self, mark: usize, depth: usize) {
        self.end_scope_slots_peek_except(mark, depth, None);
    }

    pub(super) fn end_scope_slots_peek_except(&mut self, mark: usize, depth: usize, except: Option<SlotId>) {
        if !self.live {
            return;
        }
        let mut dead: Vec<SlotId, A> = Vec::new_in(self.alloc);
        self.collect_held_slots(mark, &mut dead);
        for &(d, slot) in self.scope_slots.iter() {
            if d >= depth {
                dead.push(slot);
            }
        }
        for &(td, slot) in self.temp_slots.iter() {
            if self.temp_scopes.get(td).is_some_and(|&(_, sd)| usize::try_from(sd).is_ok_and(|d| d >= depth)) {
                dead.push(slot);
            }
        }
        for slot in dead {
            if Some(slot) != except && !self.vals_use_slot(slot) {
                self.b.slot_dead(slot);
            }
        }
    }

    /// Ends the storage of slots owned by the drop scope closing at `(mark, depth)`, sparing any
    /// slot backing the scope's settled tail value `v` or still live on the value stack.
    pub(super) fn end_scope_slots(&mut self, mark: usize, depth: usize, v: V) {
        let kept = match v {
            V::Place(p, _) | V::Unsized(p, _) | V::Scalar(p) => self.value_slot(p),
            V::None => None,
        };
        let mut dead: Vec<SlotId, A> = Vec::new_in(self.alloc);
        if self.live {
            self.collect_held_slots(mark, &mut dead);
        }
        let mut j = self.scope_slots.len();
        while j > 0 {
            j -= 1;
            if self.scope_slots.get(j).is_some_and(|&(d, _)| d >= depth) {
                let (_, slot) = self.scope_slots.remove_swap(j);
                if self.live {
                    dead.push(slot);
                }
            }
        }
        for slot in dead {
            if Some(slot) != kept && !self.vals_use_slot(slot) {
                self.b.slot_dead(slot);
            }
        }
    }
}
