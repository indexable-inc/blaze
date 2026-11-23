//! An operand matched against `_` and kept by nothing: `let _ = e;` and the assignment `_ = e`,
//! one method for both (`discard`). Rules from the Reference:
//! - "Destructuring assignments" (`[expr.assign.destructure]`): "a destructuring assignment
//!   expression ... is desugared to a pattern match followed by sequential assignment"; its
//!   example writes `(a, b) = (3, 4);` as `{ let (_a, _b) = (3, 4); a = _a; b = _b; }`. With the
//!   assignee `_` there is nothing to assign: `_ = e` is `{ let _ = e; }`, and its value is `()`
//!   ("Evaluating assignment expressions always results in the unit value").
//! - "Wildcard pattern": "Unlike identifier patterns, it does not copy, move or borrow the value
//!   it matches." So `let _ = <place>` reads nothing and moves nothing: the place keeps its value
//!   and is dropped where its owner is.
//! - "Temporary scopes": a value expression's temporary lives to the end of its statement, the
//!   smallest enclosing temporary scope; matched against `_` nothing holds it, so `let _ =
//!   <value>` drops it at once.
//!
//! blaze-typeck types `_ = rhs` as `let _ = rhs;` (rules/op.rs `expr_assign`: only a bare `_`,
//! not `(_)`, which is the `UnderscoreExpr` error); the build follows the same test.

use core::alloc::Allocator;

use blaze_typeck::{Expr, ExprId};

use super::{Task, Walk};
use crate::BuildError;

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Whether assignment `lhs = ..` is `_ = ..`: its left side a bare `_`, as the check
    /// decides it (rules/op.rs `expr_assign`).
    pub(super) fn assigns_to_underscore(&self, lhs: ExprId) -> Result<bool, BuildError> {
        Ok(matches!(self.expr(lhs)?, Expr::Underscore))
    }

    /// Pushes the tasks of `init` matched against `_` (`let _ = init;`, `_ = init`): a place
    /// with no adjustment is built as a place and left (not read, not moved); anything else is
    /// evaluated and its value dropped at once. Leaves nothing on the value stack.
    pub(super) fn discard(&mut self, init: ExprId) -> Result<(), BuildError> {
        if self.adjust_rows(init).is_empty() && self.is_place_expr(init)? {
            self.work.push(Task::Discard);
            self.work.push(Task::Place(init));
        } else {
            self.work.push(Task::DropValue(init));
            self.work.push(Task::Eval(init));
        }
        Ok(())
    }
}
