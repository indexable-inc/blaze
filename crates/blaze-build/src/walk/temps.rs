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
//! Built here: a temporary whose temporary scope is its STATEMENT (an expression statement or a
//! `let` initializer, through calls, method calls, operators, a `match` scrutinee): it joins the
//! innermost drop scope as a whole value (`Held::Rest`), and `Task::TempEnd` at the
//! statement's end drops the statement's temporaries in reverse order of creation, after the
//! statement's own value and before the next statement; a `return`, `break` or `continue` on
//! the way drops them with the scope (`drop_from`). Refused by name, never guessed: an
//! extended temporary, a temporary under a branch (a lazy operand, an arm, a guard, a closure:
//! its drop needs a flag), in an `if` or `while` condition, or in a block's tail.

use core::alloc::Allocator;

use blaze_lir::Value;
use blaze_typeck::{Expr, ExprId, Stmt};

use super::{Held, NO_EXPR, Repr, Walk, malformed, unsupported};
use crate::BuildError;

impl<A: Allocator + Clone> Walk<'_, '_, '_, A> {
    /// Temporary `e` (its value at `at`, held as `repr`, which needs drop), borrowed: it joins
    /// its statement's temporaries, dropped at the statement's end (`Task::TempEnd`).
    pub(super) fn hold_temp(&mut self, e: ExprId, at: (Value, i32), repr: Repr) -> Result<(), BuildError> {
        let Repr::Agg(l) = repr else { return Err(malformed("a value that needs drop not held as an aggregate")) };
        if self.temp_marks.is_empty() {
            return Err(unsupported(e, "a borrowed temporary whose type needs drop outside a statement"));
        }
        let body = self.body;
        // `e` is the operand of a borrow: extended if every step up to a `let` initializer is
        // an extending expression.
        let (up0, _) = *self.expr_parent.get(e.ix()).ok_or(malformed("an expression past `exprs`"))?;
        let mut extending = up0 != NO_EXPR && matches!(self.expr(ExprId(up0))?, Expr::Ref { .. });
        let mut c = e.0;
        loop {
            let (up, cond) = *self.expr_parent.get(c as usize).ok_or(malformed("an expression past `exprs`"))?;
            if cond {
                return Err(unsupported(
                    e,
                    "a borrowed temporary whose type needs drop under a branch (a lazy operand, an arm, a guard, a closure: its drop flag)",
                ));
            }
            if up == NO_EXPR {
                return Err(unsupported(e, "a borrowed temporary whose type needs drop outside a statement"));
            }
            let parent = self.expr(ExprId(up))?;
            match parent {
                Expr::Block { stmts, tail, .. } => {
                    if tail.map(|t| t.0) == Some(c) {
                        if extending {
                            // An extending block tail: the step continues upward.
                            c = up;
                            continue;
                        }
                        return Err(unsupported(e, "a borrowed temporary whose type needs drop in a block's tail (its temporary scope)"));
                    }
                    let ss = body.stmt_lists.get(stmts.range()).ok_or(malformed("a block's statements past `stmt_lists`"))?;
                    let mut init = false;
                    for &s in ss {
                        match body.stmts.get(s.ix()).copied().ok_or(malformed("a statement past `stmts`"))? {
                            Stmt::Let { init: Some(i), .. } if i.0 == c => init = true,
                            _ => {}
                        }
                    }
                    if init && extending {
                        return Err(unsupported(e, "an extended temporary whose type needs drop (`let x = &temp`: dropped at its block's end)"));
                    }
                    break;
                }
                Expr::If { cond, .. } if cond.0 == c => {
                    return Err(unsupported(e, "a borrowed temporary whose type needs drop in an `if` condition (its temporary scope)"));
                }
                Expr::Let { .. } | Expr::Loop { .. } => {
                    return Err(unsupported(e, "a borrowed temporary whose type needs drop in a condition or a loop's body"));
                }
                Expr::Ref { .. } | Expr::Paren(_) | Expr::Cast { .. } | Expr::Tuple { .. } | Expr::Array { .. } | Expr::Struct { .. } => {}
                _ => extending = false,
            }
            c = up;
        }
        self.scope_pats.push(Held::Rest { at, l, variant: None, moved: 0 });
        Ok(())
    }

    /// The end of a statement: its temporaries (`Held::Rest` past its mark) dropped in reverse
    /// order of creation; the bindings its `let` made stay.
    pub(super) fn temp_end(&mut self) -> Result<(), BuildError> {
        let mark = self.temp_marks.pop().ok_or(malformed("a statement's temporaries out of order"))? as usize;
        let mut i = self.scope_pats.len();
        while i > mark {
            i -= 1;
            let h = self.scope_pats[i];
            if let Held::Rest { .. } = h {
                self.drop_held(h, true)?;
                self.scope_pats.remove(i);
            }
        }
        Ok(())
    }
}
