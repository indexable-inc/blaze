//! Which binary operators are the BUILT-IN ones by their operand types alone, integer and float
//! literal variables included: then the check unifies (or not) and states the result with no
//! question to the trait solver. One home for the decision (rule 30); rules/op.rs `binop` calls
//! [`classify`] and applies its answer.
//!
//! The Reference, "Operator expressions":
//! - [expr.arith-logic.behavior]: "This table summarizes the behavior of arithmetic and logical
//!   binary operators on primitive types and which traits are used to overload these operators
//!   for other types." Its rows: `+ - * / %` on Integer and Floating Point; `&`, `|`, `^` on
//!   Bool and Integer (not Floating Point); `<<`, `>>` on Integer only.
//! - Shifts: the Reference states no operand-type rule in a sentence; the pinned library does
//!   (core/src/ops/bit.rs, `shl_impl_all!`: `const impl Shl<$f> for $t { type Output = $t;
//!   .. fn shl(self, other: $f) -> $t`, for every pair of integer types): the right operand is
//!   any integer type, NOT unified with the left, and the value has the left operand's type.
//! - [expr.cmp]: "Comparison operators are also defined both for primitive types and many types
//!   in the standard library." On two operands of one primitive scalar type the comparison is
//!   the built-in one and its value is `bool`.
//! - [expr.compound-assign.primitives]: "If the types of both operands are known, prior to
//!   monomorphization, to be primitive, the right hand side is evaluated first, the left hand
//!   side is evaluated next, and the place given by the evaluation of the left hand side is
//!   mutated by applying the operator to the values of both sides." and "As with normal
//!   assignment expressions, compound assignment expressions always produce the unit value."
//! - "Literal expressions", for integer literals: "If the token has no suffix, the expression's
//!   type is determined by type inference:" .. "If the program context under-constrains the
//!   type, it defaults to the signed 32-bit integer i32."; for floating-point literals the same
//!   first sentence and "If the program context under-constrains the type, it defaults to
//!   f64." An integer
//!   variable is therefore INTEGRAL whatever it becomes, a float variable FLOATING: when both
//!   operands are of one class the operator is the built-in one, and unifying an integer
//!   variable with a concrete integer type resolves it exactly as the solver's answer would.
//!
//! Why no answer can differ from asking the trait: the orphan rule
//! ([items.impl.trait.orphan-rule.general]: "At least one of the types `T0..=Tn` must be a local
//! type") leaves the primitive impls to the crate declaring the operator traits, and the pinned
//! library's impls there ARE the built-in operator (core/src/ops/arith.rs `add_impl!`: `fn
//! add(self, other: $t) -> $t { self + other }`). Every other pair (a reference operand, an
//! ADT, a type parameter, a general variable, an integer against a float) is [`Decision::Ask`]:
//! no table of the library's impls lives here.

use crate::body::BinOp;
use crate::ty::{TyView, Var, VarKind};

/// An operand type's class for the built-in operators, as far as it is resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    /// A concrete integer type or an integer variable (an unsuffixed integer literal's).
    Integral,
    /// A concrete float type or a float variable (an unsuffixed float literal's).
    Floating,
    Bool,
    Char,
    /// Anything else: a reference, a raw pointer, an ADT, a parameter, a projection, a general
    /// variable, `!`, the error type.
    Other,
}

/// The four classes an operand can be built in on, for [`defers`]' search.
const SCALARS: [Shape; 4] = [Shape::Integral, Shape::Floating, Shape::Bool, Shape::Char];

/// What a built-in operator's value is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Yields {
    /// The left operand's type (arithmetic, bit operators, shifts).
    Left,
    /// `bool` (comparisons).
    Bool,
    /// `()` (every compound assignment).
    Unit,
}

/// The decision for one operator on two operand shapes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    /// The built-in operator: unify the two operand types when `unify`, and the value is
    /// `yields`. No trait question.
    Builtin { unify: bool, yields: Yields },
    /// Ask the operator's trait, as for any other type.
    Ask,
    /// `&&` or `||`: no operator trait at all (rules/op.rs `expr_lazy` checks them).
    Lazy,
}

/// The shape of a resolved type's top level.
pub(crate) fn shape(v: TyView) -> Shape {
    match v {
        TyView::Int(_) | TyView::Uint(_) | TyView::Infer(Var { kind: VarKind::Int, .. }) => Shape::Integral,
        TyView::Float(_) | TyView::Infer(Var { kind: VarKind::Float, .. }) => Shape::Floating,
        TyView::Bool => Shape::Bool,
        TyView::Char => Shape::Char,
        _ => Shape::Other,
    }
}

/// Whether `lhs op rhs` (`assign`: the compound form `lhs op= rhs`) is the built-in operator
/// by the operand shapes alone, and what it does.
pub(crate) fn classify(op: BinOp, assign: bool, lhs: Shape, rhs: Shape) -> Decision {
    let same = lhs == rhs && lhs != Shape::Other;
    let value = if assign { Yields::Unit } else { Yields::Left };
    match op {
        BinOp::And | BinOp::Or => Decision::Lazy,
        // (a) `+ - * / %`: both integral or both floating, unified.
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
            if same && matches!(lhs, Shape::Integral | Shape::Floating) {
                Decision::Builtin { unify: true, yields: value }
            } else {
                Decision::Ask
            }
        }
        // (b) `& | ^`: both integral or both `bool`, unified (no float row in the table).
        BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
            if same && matches!(lhs, Shape::Integral | Shape::Bool) {
                Decision::Builtin { unify: true, yields: value }
            } else {
                Decision::Ask
            }
        }
        // (c) `<< >>`: both integral, NOT unified, the left operand's type.
        BinOp::Shl | BinOp::Shr => {
            if lhs == Shape::Integral && rhs == Shape::Integral {
                Decision::Builtin { unify: false, yields: value }
            } else {
                Decision::Ask
            }
        }
        // (d) comparisons: both of one scalar class, unified, `bool`. There is no compound
        // comparison; a caller passing `assign` here is refused to the solver by name.
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            if same && !assign {
                Decision::Builtin { unify: true, yields: Yields::Bool }
            } else {
                Decision::Ask
            }
        }
    }
}

/// Whether, with the left operand of shape `lhs`, SOME right operand makes `op` built in: then
/// the trait question waits for the right operand (rules/op.rs `binop` state 1, `f.b == 3`),
/// and is asked then only if [`classify`] says [`Decision::Ask`].
pub(crate) fn defers(op: BinOp, assign: bool, lhs: Shape) -> bool {
    SCALARS.iter().any(|&rhs| matches!(classify(op, assign, lhs, rhs), Decision::Builtin { .. }))
}

#[cfg(test)]
mod tests {
    use super::{Decision, Shape, Yields, classify, defers};
    use crate::body::BinOp;

    const B_UNIFY_LEFT: Decision = Decision::Builtin { unify: true, yields: Yields::Left };

    /// The table of the module doc, each class once. A wrong table prints
    /// "assertion `left == right` failed: <the row's label>".
    #[test]
    fn the_reference_table_by_shape() {
        use Shape::{Bool, Char, Floating, Integral, Other};
        // (a) arithmetic
        assert_eq!(classify(BinOp::Add, false, Integral, Integral), B_UNIFY_LEFT, "{{integer}} + u8");
        assert_eq!(classify(BinOp::Mul, false, Floating, Floating), B_UNIFY_LEFT, "1.0 * x, x: f32");
        assert_eq!(classify(BinOp::Add, false, Integral, Floating), Decision::Ask, "1 + 1.0 is not built in");
        assert_eq!(classify(BinOp::Add, false, Bool, Bool), Decision::Ask, "bool + bool is not built in");
        assert_eq!(classify(BinOp::Add, false, Integral, Other), Decision::Ask, "1 + &u32 asks the solver");
        // (b) bit operators
        assert_eq!(classify(BinOp::BitAnd, false, Integral, Integral), B_UNIFY_LEFT, "1 & 2");
        assert_eq!(classify(BinOp::BitXor, false, Bool, Bool), B_UNIFY_LEFT, "true ^ b");
        assert_eq!(classify(BinOp::BitOr, false, Floating, Floating), Decision::Ask, "f32 | f32: no float row");
        // (c) shifts
        assert_eq!(classify(BinOp::Shl, false, Integral, Integral), Decision::Builtin { unify: false, yields: Yields::Left }, "1 << 2u8");
        assert_eq!(classify(BinOp::Shr, false, Floating, Integral), Decision::Ask, "1.0 >> 2");
        // (d) comparisons
        assert_eq!(classify(BinOp::Eq, false, Integral, Integral), Decision::Builtin { unify: true, yields: Yields::Bool }, "a == 0");
        assert_eq!(classify(BinOp::Lt, false, Char, Char), Decision::Builtin { unify: true, yields: Yields::Bool }, "'a' < c");
        assert_eq!(classify(BinOp::Eq, false, Integral, Char), Decision::Ask, "1 == 'c'");
        assert_eq!(classify(BinOp::Eq, false, Other, Other), Decision::Ask, "&u8 == &u8 asks the solver");
        // (e) compound assignment
        assert_eq!(classify(BinOp::Add, true, Integral, Integral), Decision::Builtin { unify: true, yields: Yields::Unit }, "a += 1");
        assert_eq!(classify(BinOp::Shl, true, Integral, Integral), Decision::Builtin { unify: false, yields: Yields::Unit }, "a <<= 1");
        // (f) lazy
        assert_eq!(classify(BinOp::And, false, Bool, Bool), Decision::Lazy, "&& is no operator trait");
        // deferral
        assert!(defers(BinOp::Add, false, Integral), "{{integer}} + ? may be built in");
        assert!(!defers(BinOp::Shl, false, Floating), "1.0 << ? is never built in");
        assert!(!defers(BinOp::Add, false, Other), "&u32 + ? asks at once");
    }
}
