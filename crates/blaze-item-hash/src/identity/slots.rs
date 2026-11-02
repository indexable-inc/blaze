//! The literals the identity writes as slots (section 3.3): what `line!()`, `column!()`,
//! `file!()`, `module_path!()` or a `concat!` expanded to, whose value moves with the code.
//! A MIR record keeps a slot for each of `hir_ty::body_store::location_literals` and fills it
//! from where the body is now, so the identity leaves their values out and a moved body hits.
//!
//! Only those an ordinary expression of the body reaches are slots: none under an array repeat
//! count, a `const { .. }` block, a const generic argument, a type (an array length), a pattern
//! or an `asm!`. There the value becomes part of a type or a `SwitchInt`, which the MIR slot
//! does not fill, so it is hashed by value and moving such a body misses.

use base_db::Db;
use hir_def::{
    DefWithBodyId,
    expr_store::Body,
    hir::{Array, Expr, ExprId},
};
use hir_ty::body_store::location_literals;
use smallvec::SmallVec;

/// The slots of `def`'s body, sorted; empty for a body that calls no location macro.
pub(crate) fn of(db: &Db, def: DefWithBodyId) -> SmallVec<[ExprId; 4]> {
    let located = location_literals(db, def);
    if located.is_empty() {
        return SmallVec::new();
    }
    let body = Body::of(db, def);
    // alloc: the slots and the walk's stack, of a body that calls a location macro
    let mut slots = SmallVec::new();
    let mut stack: SmallVec<[ExprId; 32]> = SmallVec::new();
    stack.push(body.root_expr());
    while let Some(expr) = stack.pop() {
        if located.binary_search(&expr).is_ok() {
            slots.push(expr);
        }
        match body.expr(expr) {
            Expr::Array(Array::Repeat { initializer, .. }) => stack.push(initializer),
            // An inline const's value can reach a type, and an `asm!` const operand is in the
            // assembly; its other operands are left by value with it.
            Expr::Const(_) | Expr::InlineAsm(_) => {}
            _ => body.walk_child_exprs_without_pats(expr, |child| stack.push(child)),
        }
    }
    slots.sort_unstable();
    slots
}
