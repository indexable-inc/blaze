//! `let` in a condition and `let`-`else`: `if let`, `while let` (the lowering's `loop { if
//! COND BODY else { break } }`, blaze-lower desugar.rs `while_loop`, so it is an `if let`
//! here), let chains, and `let P = e else { .. };`. Each `let` is one arm of walk/matches.rs's
//! match by tests: its pattern's tests on the scrutinee's place (`test_pat`), a failure to the
//! `else`, then its bindings from the place (`bind_tested`). Rules from the Reference:
//! - "If expressions", "if let": "If the value of the scrutinee matches the pattern, the
//!   corresponding block will execute. Otherwise, flow proceeds to the following `else` block
//!   if it exists."
//! - "If expressions", chains of conditions (edition 2024): "Multiple condition operands can
//!   be separated with `&&`"; "each operand is evaluated from left-to-right until an operand
//!   evaluates as `false` or a `let` match fails, in which case the subsequent operands are
//!   not evaluated"; "The bindings of each pattern are put into scope to be available for the
//!   next condition operand and the consequent block." A chain is flattened into its links in
//!   order; every failed link jumps to the one `else` block.
//! - "let statements", let-else: "If the pattern does not match (this requires it to be
//!   refutable), the `else` block is executed. The `else` block must always diverge (evaluate
//!   to the never type)"; its bindings are the enclosing block's, made where the pattern
//!   matched.
//! - "Temporary scopes": "The pattern-matching condition(s) and consequent body of `if`", "The
//!   pattern-matching condition and loop body of `while`".
//!
//! A `let` link owns its scrutinee as a match does (`owned_place`: a place is tested where it
//! is; a temporary that needs drop is held by walk/temps.rs `hold_temp` with a word, and a
//! by-value binding moves its part out). Where they drop is the condition's region
//! (lets/scope.rs): its bindings and, in edition 2024, its scrutinee temporaries, dropped after
//! the consequent in reverse order of creation, or by each failed link before the `else`
//! block; nothing needs a drop flag but a scrutinee's word. A `let`-`else` drops the
//! statement's temporaries before its `else` block (`let_else_test`).

use core::alloc::Allocator;

use blaze_code::record::TrapKind;
use blaze_lir::Block;
use blaze_own::Vec;
use blaze_typeck::{BinOp, Expr, ExprId, Stmt, StmtId};

use super::{Held, Task, Walk, malformed, unsupported};
use crate::BuildError;

pub(super) mod scope;

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Whether `if` condition `cond` holds a `let`: alone or as a link of an `&&` chain.
    pub(super) fn cond_has_let(&self, cond: ExprId) -> Result<bool, BuildError> {
        let mut todo: Vec<ExprId, A> = Vec::new_in(self.alloc);
        todo.push(cond);
        while let Some(x) = todo.pop() {
            match self.expr(x)? {
                Expr::Let { .. } => return Ok(true),
                Expr::Binary { op: BinOp::And, lhs, rhs } => {
                    todo.push(lhs);
                    todo.push(rhs);
                }
                _ => {}
            }
        }
        Ok(false)
    }

    /// `if` expression `e` whose condition holds a `let`: its links in order (a `let`'s
    /// scrutinee, then its tests and bindings; a `bool`, then its branch), each failure to
    /// the `else` block; then the consequent and the `else` as walk.rs `Task::IfThen` builds
    /// them.
    pub(super) fn if_let(&mut self, e: ExprId, cond: ExprId, then: ExprId, else_: Option<ExprId>) -> Result<(), BuildError> {
        let links = self.links_of(cond)?;
        let else_block = self.create();
        let join = self.create();
        // The condition's region (lets/scope.rs): its bindings and, in edition 2024, its
        // scrutinee temporaries; dropped after the consequent (the region's `ScopeEnd`, then
        // the temporary scope's end), or by each failed link before the `else` block.
        let temps = self.if_let_open(e, cond, links.len())?;
        self.work.push(Task::IfThen { expr: e, else_, else_block, join });
        if temps {
            self.work.push(Task::TempScopeEnd(cond));
        }
        self.work.push(Task::ScopeEnd(then));
        self.work.push(Task::Eval(then));
        self.push_links(&links, else_block)
    }

    /// The links of condition `cond` left to right: `a && b && c` is `(a && b) && c`.
    fn links_of(&self, cond: ExprId) -> Result<Vec<ExprId, A>, BuildError> {
        let mut links: Vec<ExprId, A> = Vec::new_in(self.alloc);
        let mut todo: Vec<ExprId, A> = Vec::new_in(self.alloc);
        todo.push(cond);
        while let Some(x) = todo.pop() {
            match self.expr(x)? {
                Expr::Binary { op: BinOp::And, lhs, rhs } => {
                    todo.push(rhs);
                    todo.push(lhs);
                }
                _ => links.push(x),
            }
        }
        Ok(links)
    }

    /// The tasks of `links` (a condition's, from `links_of`), each failure to `fail`: a
    /// `let`'s scrutinee, then its tests and bindings; a `bool`, then its branch.
    fn push_links(&mut self, links: &[ExprId], fail: Block) -> Result<(), BuildError> {
        for &x in links.iter().rev() {
            match self.expr(x)? {
                Expr::Let { init, .. } => {
                    self.work.push(Task::LetLink { expr: x, fail });
                    self.push_scrutinee(init)?;
                }
                _ => {
                    self.work.push(Task::CondLink(fail));
                    self.work.push(Task::Eval(x));
                }
            }
        }
        Ok(())
    }

    /// Match arm guard `g` holding a `let` (an `if let` guard, core's `dec2flt`: `None if let
    /// Some(value) = parse_inf_nan(s, negative) => return Ok(value),`; or a chain with `&&`),
    /// then the arm's `body`; every failed link to `next`, the next arm's block. The Reference,
    /// "Match guards": the guard "may be ... a chain of conditions" whose `let` bindings are in
    /// scope in the arm's body, and when it is false (a `let` fails) "control moves on to the
    /// next arm". Built as an `if let` condition's links (`push_links`) in a region of its own
    /// (lets/scope.rs) opened inside the arm's drop scope: a failed link drops what the region
    /// holds then (`link_fail`), the matched path drops it after the body (a `Task::ScopeEnd`
    /// of the body, which settles the body's value), before the arm's own bindings. A scrutinee temporary that needs drop is refused by name
    /// (lets/scope.rs `let_anchor`: its temporary scope is not built for a guard).
    pub(super) fn let_guard(&mut self, g: ExprId, body: ExprId, next: Block) -> Result<(), BuildError> {
        let links = self.links_of(g)?;
        if links.len() > 1 && !self.if_let_2024() {
            return Err(unsupported(g, "a `let` chain before edition 2024 (chains are an edition-2024 form)"));
        }
        self.scope_marks.push(self.scope_pats.len() as u32);
        self.work.push(Task::ScopeEnd(body));
        self.work.push(Task::Eval(body));
        self.push_links(&links, next)
    }

    /// A `let`'s scrutinee as walk.rs's `match` pushes one: a local that needs drop is a
    /// place expression, matched in place, not moved. Also walk.rs `stmt`'s, for a `let` that
    /// owns its initializer's parts (matches/irrefutable.rs `let_owns_parts`).
    pub(super) fn push_scrutinee(&mut self, init: ExprId) -> Result<(), BuildError> {
        let mut s = init;
        while let Expr::Paren(inner) = self.expr(s)? {
            s = inner;
        }
        match self.expr(s)? {
            Expr::Local(p) if self.local_drops(p) && self.adjust_rows(init).is_empty() => {
                self.work.push(Task::Place(s));
            }
            // A place behind a pointer that needs drop (`if let Reach::Hosted = self.reach`):
            // matched in place, nothing moved (walk/moves.rs `scrutinee_in_place`).
            _ if self.scrutinee_in_place(init)? => self.work.push(Task::Place(init)),
            // A field of an owned local that needs drop (`if let Some(e) = self.failed`):
            // matched in place (walk/moves/field_scrutinee.rs).
            _ if self.owned_field_scrutinee(init)?.is_some() => self.work.push(Task::Place(init)),
            _ => self.work.push(Task::Eval(init)),
        }
        Ok(())
    }

    /// `Task::LetLink`: `let` expression `l`'s scrutinee on top; its tests, a failure to
    /// `fail`; its bindings in the block where it matched, for the later links and the body.
    pub(super) fn let_link(&mut self, l: ExprId, fail: Block) -> Result<(), BuildError> {
        let Expr::Let { pat, init } = self.expr(l)? else { return Err(malformed("a `let` link's task on another expression")) };
        let s = self.pop()?;
        let srepr = self.adjusted_repr(init)?;
        // Owned as a `match`'s scrutinee (`owned_place`, as `let_else_test`): a place is tested
        // where it is (a local by its own scope, a place behind a pointer by no one here); a
        // temporary that needs drop joins the condition's region (`hold_temp`, anchored by
        // lets/scope.rs `let_anchor`) with a word, and a by-value binding moves its part out.
        // A whole local keeps the place it has and is the root's owner: a by-value binding
        // moves its part out by the local's field flags (`if let Some(frame) = frame`; walk.rs
        // `scan_flags` gave them, walk/moves.rs `scan_local_scrutinee`), as a `match` arm does.
        let m = self.matches.len() as u32;
        let at = self.owned_place(m, init, init, s, srepr)?;
        // A failed test drops what the region holds at this link, then goes to the `else`.
        let (to, undo) = self.link_fail(fail)?;
        let pass = self.create();
        // Set before the tests: an or-alternative binds (and moves) on its own pass.
        self.move_arm(m, pat, false);
        if let Err(error) = self.test_pat(pat, at, pass, to) {
            self.move_arm_end();
            return Err(error);
        }
        self.move_arm_end();
        if undo {
            self.link_fail_end(to, fail)?;
        }
        self.begin(pass);
        self.move_arm(m, pat, false);
        let bound = self.bind_tested(pat, at);
        self.move_arm_end();
        bound
    }

    /// `Task::CondLink`: a `bool` link's value on top; false to `fail`, through a block that
    /// drops what the condition's region holds at this link (lets/scope.rs `link_fail`).
    pub(super) fn cond_link(&mut self, fail: Block) -> Result<(), BuildError> {
        let v = self.pop()?;
        let c = self.cond(v)?;
        let next = self.create();
        let (to, undo) = self.link_fail(fail)?;
        self.br(c, (next, &[]), (to, &[]));
        if undo {
            self.link_fail_end(to, fail)?;
        }
        self.begin(next);
        Ok(())
    }

    /// `let P = init else { .. };` (statement `s`): its scrutinee, then `Task::LetElse`.
    pub(super) fn let_else(&mut self, s: StmtId, init: ExprId) -> Result<(), BuildError> {
        self.work.push(Task::LetElse(s));
        self.push_scrutinee(init)
    }

    /// `Task::LetElse`: statement `s`'s scrutinee on top. The tests; on a match the bindings,
    /// then on to a block the rest of the enclosing block continues in; on a failure the
    /// `else` block, which diverges (`Task::LetElseEnd`).
    pub(super) fn let_else_test(&mut self, s: StmtId) -> Result<(), BuildError> {
        let stmt = self.body.stmts.get(s.ix()).copied().ok_or(malformed("a statement past `stmts`"))?;
        let Stmt::Let { pat, init: Some(init), else_: Some(els), .. } = stmt else {
            return Err(malformed("a `let`-`else` task on another statement"));
        };
        let mark = *self.temp_marks.last().ok_or(malformed("a `let`-`else` outside a statement"))? as usize;
        if self.scope_marks.last().is_some_and(|&m| m as usize > mark) {
            return Err(malformed("a drop scope left open inside a `let`-`else` initializer"));
        }
        let v = self.pop()?;
        let srepr = self.adjusted_repr(init)?;
        // The scrutinee is owned like a match's (`owned_place`): a temporary that needs drop
        // joins the statement's temporaries with a word, and a binding by value moves its part
        // out of it. A local scrutinee keeps the place it has; a binding by value moves its
        // part out by the local's field flags (walk/moves.rs `scan_local_scrutinee`).
        let m = self.matches.len() as u32;
        let at = self.owned_place(m, init, init, v, srepr)?;
        let pass = self.create();
        let fail = self.create();
        // Set before the tests: an or-alternative binds (and moves) on its own pass.
        self.move_arm(m, pat, false);
        if let Err(error) = self.test_pat(pat, at, pass, fail) {
            self.move_arm_end();
            return Err(error);
        }
        self.move_arm_end();
        self.begin(pass);
        self.move_arm(m, pat, false);
        let bound = self.bind_tested(pat, at);
        self.move_arm_end();
        bound?;
        let cont = self.create();
        self.jump(cont, &[]);
        // The pattern failed: the statement's temporaries (the initializer's borrowed ones and
        // its own value, whole: nothing was moved out on this path) drop now, last created
        // first, BEFORE the `else` block runs. The Reference's copy on this machine
        // (nightly-2026-10-27, "Destructors") does not state it for `let`-`else`; the rule is
        // rustc's own test, tests/ui/let-else/let-else-temporary-lifetime.rs: `let Foo(0) =
        // Foo(&mut foo) else { *&mut foo = 1; todo!() };` compiles only if the temporary
        // `Foo` (its borrow of `foo`) is gone when the `else` block starts, and `let 0 =
        // Droppy::default().inner else { return };` asserts the drop has run by the next
        // statement. The `else` block is then built with the statement's own entries (its
        // temporaries, now dropped, and its bindings, never made on this path) set aside, so
        // a `return` or `break` in it drops neither again; `let_else_end` puts them back for
        // the matched path, where they drop at the statement's end (`temp_end`).
        self.begin(fail);
        let mut i = self.scope_pats.len();
        while i > mark {
            i -= 1;
            match self.scope_pats[i] {
                h @ (Held::Rest { .. } | Held::Moved { .. }) => self.drop_held(h, false)?,
                // An owned environment field is the body's outermost scope's, made before any
                // statement: never this statement's entry.
                Held::Pat(_) | Held::EnvPlace(_) => {}
                Held::Temp { .. } => {
                    return Err(unsupported(init, "an extended temporary in a `let`-`else` initializer"));
                }
            }
        }
        let from = self.let_else_held.len() as u32;
        self.let_else_held.extend_from_slice(&self.scope_pats[mark..]);
        self.scope_pats.truncate(mark);
        self.work.push(Task::LetElseEnd(cont, from, els));
        self.work.push(Task::Eval(els));
        Ok(())
    }

    /// `Task::LetElseEnd`: the `else` block's value (of type `!`) on top; nothing follows it,
    /// and the statement goes on in `cont` with its entries (`let_else_held` from `from`) back.
    /// `els` is the `else` block, the trap's site if its end is ever reached.
    pub(super) fn let_else_end(&mut self, cont: Block, from: u32, els: ExprId) -> Result<(), BuildError> {
        self.pop()?;
        self.unreachable_of(TrapKind::LetElse, Some(els));
        self.begin(cont);
        let mark = *self.temp_marks.last().ok_or(malformed("a `let`-`else` end outside its statement"))? as usize;
        if self.scope_pats.len() != mark {
            return Err(malformed("a `let`-`else`'s `else` block left drop entries behind"));
        }
        let from = from as usize;
        if from > self.let_else_held.len() {
            return Err(malformed("a `let`-`else`'s entries set aside out of order"));
        }
        self.scope_pats.extend_from_slice(&self.let_else_held[from..]);
        self.let_else_held.truncate(from);
        Ok(())
    }
}
