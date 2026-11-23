//! Operators: which are the builtin ones and which are calls. THE WALK NEVER DECIDES THIS
//! FROM HOW A VALUE IS HELD (`Repr`): a reference and a raw pointer are both held as a
//! pointer, and `a == b` on two `&u32` compared the two ADDRESSES (rung
//! x0-match-05-nested-enums, 2026-11-16) where Rust compares what they point to.
//!
//! The decision is the check's, in one home: a row of `Checked::callees` on the operator
//! expression (blaze-typeck rules/op.rs `binop`, `expr_unary`, `operator_settled_builtin`).
//! A row says: this operator is a call of the trait's method at these arguments (`[Self,
//! Rhs]`, or `[Self]` for a unary operator). No row says: the builtin operator, and then the
//! operand TYPES at this instance must be that operator's builtin scalar types, which
//! `builtin_binary` and `builtin_unary` assert before walk.rs picks an instruction by the
//! held form; anything else is [`BuildError::Operator`] naming the operator and the types.
//!
//! The Reference, "Operator expressions":
//! - "Comparison Operators", [expr.cmp.place]: "Unlike the arithmetic and logical operators
//!   above, these operators implicitly take shared borrows of their operands, evaluating them
//!   in place expression context: `a == b;` is equivalent to `::std::cmp::PartialEq::eq(&a,
//!   &b);` This means that the operands don't have to be moved out of." [expr.cmp.behavior]:
//!   `==` is `std::cmp::PartialEq::eq`, `!=` `ne`, `>` `std::cmp::PartialOrd::gt`, `<` `lt`,
//!   `>=` `ge`, `<=` `le`.
//! - "Arithmetic and Logical Binary Operators", [expr.arith-logic.behavior]: "This table
//!   summarizes the behavior of arithmetic and logical binary operators on primitive types and
//!   which traits are used to overload these operators for other types. .. The operands of all
//!   of these operators are evaluated in value expression context so are moved or copied."
//!   (`+`: Integer, Floating Point, `std::ops::Add`; `&`: Integer, bool, `std::ops::BitAnd`;
//!   `<<`: Integer, `std::ops::Shl`; ..)
//! - "Negation operators", [expr.negate.results]: `-` is negation on signed integers ("Only
//!   for signed integer types") and floating point, else `std::ops::Neg`; `!` is bitwise NOT
//!   on integers and logical NOT on `bool`, else `std::ops::Not`; "The operands of all of
//!   these operators are evaluated in value expression context so are moved or copied."
//! - "Compound assignment expressions", [expr.compound-assign.primitives]: "If the types of
//!   both operands are known, prior to monomorphization, to be primitive, the right hand side
//!   is evaluated first, the left hand side is evaluated next, and the place given by the
//!   evaluation of the left hand side is mutated by applying the operator to the values of
//!   both sides." [expr.compound-assign.trait]: "Otherwise, this expression is syntactic sugar
//!   for using the corresponding trait for the operator (see expr.arith-logic.behavior) and
//!   calling its method with the left hand side as the receiver and the right hand side as the
//!   next argument." "Prior to monomorphization" is the row: it is recorded on the generic
//!   body, so a type parameter instantiated at `u8` is still the call (the Reference's `f3`).
//!
//! So a call's operands: a comparison's are places, each borrowed shared (`Task::Ref` over
//! `Task::Place`, exactly what `&operand` pushes: a value operand is spilled to a temporary
//! dropped at its statement's end, an unsized place gives its wide pointer); an arithmetic or
//! unary operator's are values, in order; a compound assignment's are the left place borrowed
//! mutably, THEN the right value. Which impl the trait method's instance runs is the
//! driver's question, as for any trait method call (`blaze_typeck::resolve_instance`): the
//! forwarding impls (`impl PartialEq<&B> for &A`, `impl Add<&u32> for &u32`) are ordinary
//! impls there, and nothing here knows them.
//!
//! Not built, refused by name: an operator whose trait method is called through a trait
//! object's vtable (`*a == 5` with `a: &dyn PartialEq<i32>`).

use core::alloc::Allocator;

use blaze_code::record::TrapKind;
use blaze_own::Vec;
use blaze_typeck::{BinOp, Expr, ExprId, Mutbl, PNode, UnOp};

use super::{Repr, Task, V, Walk, malformed};
use crate::BuildError;

/// The operator as written, for an error.
pub(super) fn sign(op: BinOp, assign: bool) -> &'static str {
    match (op, assign) {
        (BinOp::Add, false) => "+",
        (BinOp::Sub, false) => "-",
        (BinOp::Mul, false) => "*",
        (BinOp::Div, false) => "/",
        (BinOp::Rem, false) => "%",
        (BinOp::BitAnd, false) => "&",
        (BinOp::BitOr, false) => "|",
        (BinOp::BitXor, false) => "^",
        (BinOp::Shl, false) => "<<",
        (BinOp::Shr, false) => ">>",
        (BinOp::Add, true) => "+=",
        (BinOp::Sub, true) => "-=",
        (BinOp::Mul, true) => "*=",
        (BinOp::Div, true) => "/=",
        (BinOp::Rem, true) => "%=",
        (BinOp::BitAnd, true) => "&=",
        (BinOp::BitOr, true) => "|=",
        (BinOp::BitXor, true) => "^=",
        (BinOp::Shl, true) => "<<=",
        (BinOp::Shr, true) => ">>=",
        (BinOp::Eq, _) => "==",
        (BinOp::Ne, _) => "!=",
        (BinOp::Lt, _) => "<",
        (BinOp::Le, _) => "<=",
        (BinOp::Gt, _) => ">",
        (BinOp::Ge, _) => ">=",
        (BinOp::And, _) => "&&",
        (BinOp::Or, _) => "||",
    }
}

/// A comparison operator ([expr.cmp]): its operands are places, borrowed.
pub(super) fn comparison(op: BinOp) -> bool {
    matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)
}

fn integral(n: PNode) -> bool {
    matches!(n, PNode::Int(_) | PNode::Uint(_))
}

/// The types a builtin comparison takes (blaze-typeck rules/op.rs `is_scalar`): references
/// are NOT among them.
fn scalar(n: PNode) -> bool {
    integral(n) || matches!(n, PNode::Float(_) | PNode::Bool | PNode::Char | PNode::Ptr(..) | PNode::FnPtr(..))
}

/// What an operator's call passes for one operand: the value, or a borrow of its place.
#[derive(Clone, Copy)]
enum Operand {
    Value(ExprId),
    Borrowed(ExprId, Mutbl),
}

const NO_ROW: &str = "the check recorded no callee for it (`Checked::callees`) and the operand types are not this \
                      operator's builtin scalar types, so it is neither the builtin operator nor a call the build can name";

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// The check recorded a callee on `e`.
    fn has_callee(&self, e: ExprId) -> bool {
        self.checked.callees.column::<0>().contains(&e)
    }

    /// The type of operand `e` at this instance, after its adjustments.
    fn operand_node(&mut self, e: ExprId) -> Result<PNode, BuildError> {
        let t = self.adjusted_ty(e)?;
        self.node_at(e, t)
    }

    /// Pushes the tasks of operator expression `e` (binary, unary `-` or `!`, compound
    /// assignment) when the check recorded a callee for it: its operands as the module doc
    /// says, then the call. False: no row, the builtin operator (walk.rs pushes it).
    pub(super) fn push_overloaded(&mut self, e: ExprId) -> Result<bool, BuildError> {
        if !self.has_callee(e) {
            return Ok(false);
        }
        match self.expr(e)? {
            Expr::Binary { op: BinOp::And | BinOp::Or, .. } => return Err(malformed("a callee recorded on `&&` or `||`")),
            Expr::Binary { op, lhs, rhs } if comparison(op) => {
                #[cfg(test)]
                if self.adts.planted() == crate::Planted::CompareAddresses
                    && matches!(self.operand_node(lhs)?, PNode::Ref(..))
                    && matches!(self.operand_node(rhs)?, PNode::Ref(..))
                {
                    return Ok(false);
                }
                self.work.push(Task::OpCall(e));
                self.work.push(Task::Ref);
                self.work.push(Task::Place(rhs));
                self.work.push(Task::Ref);
                self.work.push(Task::Place(lhs));
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.work.push(Task::OpCall(e));
                self.push_operand_value(rhs)?;
                self.push_operand_value(lhs)?;
            }
            Expr::Unary { op: UnOp::Neg | UnOp::Not, operand } => {
                self.work.push(Task::OpCall(e));
                self.push_operand_value(operand)?;
            }
            // The left place first, then the right value ([expr.compound-assign.trait]).
            Expr::AssignOp { lhs, rhs, .. } => {
                self.work.push(Task::OpCall(e));
                self.push_operand_value(rhs)?;
                self.work.push(Task::Ref);
                self.work.push(Task::Place(lhs));
            }
            _ => return Err(malformed("a callee's operator tasks on an expression that is no operator")),
        }
        Ok(true)
    }

    /// Pushes the tasks of operand `a` passed by value: as a call's argument (walk.rs `raw`,
    /// `Expr::MethodCall`): an aggregate is copied before a later operand can write it.
    fn push_operand_value(&mut self, a: ExprId) -> Result<(), BuildError> {
        if let Repr::Agg(_) = self.adjusted_repr(a)? {
            self.work.push(Task::ArgCopy(a));
        }
        self.work.push(Task::Eval(a));
        Ok(())
    }

    /// The call of operator expression `e`, its operands on the value stack as
    /// `push_overloaded` pushed them: the trait method the check recorded, at the row's
    /// arguments grounded at this instance. The row's types must be the operands' (the
    /// values on the stack are of the operands' types): another shape is refused by name.
    pub(super) fn operator_call(&mut self, e: ExprId) -> Result<(), BuildError> {
        let (op, operands): (&'static str, [Option<Operand>; 2]) = match self.expr(e)? {
            Expr::Binary { op, lhs, rhs } if comparison(op) => {
                (sign(op, false), [Some(Operand::Borrowed(lhs, Mutbl::Not)), Some(Operand::Borrowed(rhs, Mutbl::Not))])
            }
            Expr::Binary { op, lhs, rhs } => (sign(op, false), [Some(Operand::Value(lhs)), Some(Operand::Value(rhs))]),
            Expr::AssignOp { op, lhs, rhs } => (sign(op, true), [Some(Operand::Borrowed(lhs, Mutbl::Mut)), Some(Operand::Value(rhs))]),
            Expr::Unary { op: UnOp::Neg, operand } => ("-", [Some(Operand::Value(operand)), None]),
            Expr::Unary { op: UnOp::Not, operand } => ("!", [Some(Operand::Value(operand)), None]),
            _ => return Err(malformed("an operator's call on an expression that is no operator")),
        };
        let instance = self.method_callee(e)?;
        let item = instance.item;
        let mut row = Vec::new_in(self.alloc);
        row.extend_from_slice(self.ground.view().list(instance.args).map_err(|_| malformed("a callee's arguments past the ground table"))?);
        let abi = self.adts.abi(item).ok_or(BuildError::NoAbi { item })?;
        let mut reprs = Vec::with_capacity_in(2, self.alloc);
        let mut passes = Vec::with_capacity_in(2, self.alloc);
        let mut nodes = [None, None];
        let mut mismatch = operands.iter().flatten().count() != row.len();
        for (i, operand) in operands.iter().flatten().enumerate() {
            let (a, borrow) = match *operand {
                Operand::Value(a) => (a, None),
                Operand::Borrowed(a, m) => (a, Some(m)),
            };
            // A place operand (a compound assignment's left side) has no adjustments.
            let t = if borrow == Some(Mutbl::Mut) { self.expr_ty(a)? } else { self.adjusted_ty(a)? };
            let g = self.ground_ty(t).map_err(|error| Self::ty_err(a, error))?;
            nodes[i] = Some(self.ground_node(g).map_err(|error| Self::ty_err(a, error))?);
            mismatch |= row.get(i) != Some(&g);
            let g = match borrow {
                Some(m) => self.ground.intern(PNode::Ref(m, g)).map_err(|_| malformed("the ground table is full"))?,
                None => g,
            };
            let repr = self.repr(g).map_err(|error| Self::ty_err(a, error))?;
            reprs.push(repr);
            passes.push(self.pass(g, repr, abi, Some(item))?);
        }
        let refuse = |why| BuildError::Operator { expr: e, op, lhs: nodes[0], rhs: nodes[1], why };
        if mismatch {
            return Err(refuse("its callee row (`Checked::callees`) does not hold exactly its operands' types"));
        }
        if self.dyn_slot(e, instance)?.is_some() {
            return Err(refuse("its trait method is called through a trait object's vtable, which is not built for an operator"));
        }
        let ty = self.expr_ty(e)?;
        let g = self.ground_ty(ty).map_err(|error| Self::ty_err(e, error))?;
        let rrepr = self.repr_at(e, ty)?;
        let rpass = self.pass(g, rrepr, abi, Some(item))?;
        self.call_instance(Some(e), instance, &reprs, &passes, None, rrepr, rpass)?;
        if let PNode::Never = self.node_at(e, ty)? {
            self.pop_val();
            self.unreachable_of(TrapKind::AfterDivergingCall, Some(e));
            self.dead_block();
            self.push_val(V::None, None);
        }
        Ok(())
    }

    /// Asserts that binary operator or compound assignment `e` (`lhs op rhs`), which has no
    /// callee row, is on its builtin scalar types at this instance (the module doc; the
    /// categories are blaze-typeck rules/op.rs `is_builtin_binop`'s, minus bitwise operators
    /// on floats, which no table of the Reference lists). `lhs_place`: the left side is a
    /// compound assignment's place, read at its own type.
    pub(super) fn builtin_binary(&mut self, e: ExprId, op: BinOp, lhs: ExprId, rhs: ExprId, lhs_place: bool) -> Result<(), BuildError> {
        let l = if lhs_place {
            let t = self.expr_ty(lhs)?;
            self.node_at(lhs, t)?
        } else {
            self.operand_node(lhs)?
        };
        let r = self.operand_node(rhs)?;
        let float = |n| matches!(n, PNode::Float(_));
        let ok = match op {
            BinOp::Shl | BinOp::Shr => integral(l) && integral(r),
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => (integral(l) && integral(r)) || (float(l) && float(r)),
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => (integral(l) && integral(r)) || (l == PNode::Bool && r == PNode::Bool),
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => scalar(l) && scalar(r),
            BinOp::And | BinOp::Or => l == PNode::Bool && r == PNode::Bool,
        };
        #[cfg(test)]
        let ok = ok
            || (self.adts.planted() == crate::Planted::CompareAddresses && comparison(op) && matches!((l, r), (PNode::Ref(..), PNode::Ref(..))));
        if ok {
            return Ok(());
        }
        Err(BuildError::Operator { expr: e, op: sign(op, lhs_place), lhs: Some(l), rhs: Some(r), why: NO_ROW })
    }

    /// Asserts that unary operator `e` (`-operand`, `!operand`), which has no callee row, is
    /// on its builtin scalar type at this instance ([expr.negate.results]).
    pub(super) fn builtin_unary(&mut self, e: ExprId, op: UnOp, operand: ExprId) -> Result<(), BuildError> {
        let n = self.operand_node(operand)?;
        let (ok, op) = match op {
            UnOp::Neg => (matches!(n, PNode::Int(_) | PNode::Float(_)), "-"),
            UnOp::Not => (integral(n) || n == PNode::Bool, "!"),
            UnOp::Deref => return Err(malformed("a dereference among the negation operators")),
        };
        if ok {
            return Ok(());
        }
        Err(BuildError::Operator { expr: e, op, lhs: Some(n), rhs: None, why: NO_ROW })
    }
}
