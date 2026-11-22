//! `_` in a const-argument position is an INFERRED CONST, never an expression (the Reference,
//! "Array expressions": "the length operand must be either an inferred const or a constant
//! expression of type `usize`"; "Generic arguments": a const argument may be an inferred const
//! `_`, `generic_arg_infer`). Its value comes from unification, here a declared array length or
//! an argument's type; it is an error only when nothing fixes it (E0282).
//!
//! The case that made this: the pinned library's panic_unwind/src/gcc.rs:67,
//! `private: [core::ptr::null(); _]`. blaze-lower's `to_typeck` kept the repeat count `_` as
//! the count EXPRESSION (`tc::Expr::Repeat { .., count: Some(len) }`), which blaze-typeck's
//! `expr_repeat` checked as a `usize` and `expr_underscore` refused: E0000 `UnderscoreExpr`,
//! on 46 of 49 std-census rungs. The lowering now makes that `_` a hole with no count (the
//! checker's `lower_len` makes it a const variable), as the array type's `[T; _]` and the
//! generic argument's `f::<_>` already were.
//!
//! CONTROL (the coordinator runs it): in crates/blaze-lower/src/typeck.rs, `Conv::expr`'s
//! `Expr::Repeat` arm, change `if matches!(self.v.expr(len)?, Expr::Underscore) {` to
//! `if false && matches!(self.v.expr(len)?, Expr::Underscore) {`. Then
//! `a_repeat_count_underscore_takes_the_fields_length` fails with
//! "unexpected diagnostics [UnderscoreExpr]" (this file's `Holes::repeat_term` makes `_` a hole
//! as front2's does, so the old path reaches the checker), and
//! `an_unresolved_inferred_const_is_e0282` fails with "want only E0282, got [UnderscoreExpr]".
//! The other two do not depend on the change (they pin the type and path positions).

use blaze_typeck::fixture::Fx;
use blaze_typeck::{DiagCode, Mutbl, PNode, PTy, PortableTys, UintTy};

use super::{findings, lower_and_check_with, typed_lines};
use crate::columns::View;
use crate::form::{ExprId, TypeId};
use crate::typeck::{ConstTerm, Refused, Types};

/// The written types and repeat lengths these bodies use, by their text: `_` (a hole, as front2
/// makes it: blaze-ladder front2/types.rs `TypeRef::Infer`), `[u8; _]` (an array whose length is
/// a hole, front2's `Builder::const_term` on `_`), and a repeat length that is an integer
/// literal. A repeat length `_` is a hole as front2's `repeat_term` makes it; with the fix in,
/// `to_typeck` never asks for it.
struct Holes;

impl Types for Holes {
    fn ty(&mut self, v: &View<'_>, t: TypeId, tys: &mut PortableTys<'_>) -> Result<PTy, Refused> {
        let bad = Refused::Form { what: "a type the test world does not name", range: Default::default() };
        let r = v.type_range(t).map_err(|_| bad)?;
        match v.text_at(r).map_err(|_| bad)?.trim() {
            "_" => Ok(tys.intern(PNode::Infer)?),
            "[u8; _]" => {
                let u8_ = tys.intern(PNode::Uint(UintTy::U8))?;
                let hole = tys.intern(PNode::Infer)?;
                Ok(tys.intern(PNode::Array(u8_, hole))?)
            }
            _ => Err(bad),
        }
    }

    fn repeat_len(&mut self, v: &View<'_>, len: ExprId) -> Result<u64, Refused> {
        let bad = Refused::Form { what: "a repeat length that is no integer literal", range: Default::default() };
        let r = v.expr_range(len).map_err(|_| bad)?;
        v.text_at(r).map_err(|_| bad)?.trim().parse::<u64>().map_err(|_| bad)
    }

    fn const_arg(&mut self, view: &View<'_>, c: crate::ConstIdx, _: &mut PortableTys<'_>) -> Result<PTy, Refused> {
        Err(crate::typeck::no_const_arg(view, c))
    }

    fn repeat_term(&mut self, v: &View<'_>, len: ExprId, tys: &mut PortableTys<'_>) -> Result<ConstTerm, Refused> {
        let bad = Refused::Form { what: "a repeat length the test world does not read", range: Default::default() };
        let r = v.expr_range(len).map_err(|_| bad)?;
        if v.text_at(r).map_err(|_| bad)?.trim() == "_" {
            return Ok(ConstTerm::Is(tys.intern(PNode::Infer)?));
        }
        let n = self.repeat_len(v, len)?;
        Ok(ConstTerm::Is(tys.usize_value(n)?))
    }
}

/// The world: `struct Gcc { private: [*const u8; 2] }`, `fn null() -> *const u8`,
/// `fn take<const N: usize>(a: [u8; N])`, and the owners `make() -> Gcc`, `f()`, `g()`, `h()`.
struct World {
    fx: Fx,
    make: blaze_typeck::ItemKey,
    f: blaze_typeck::ItemKey,
    g: blaze_typeck::ItemKey,
    h: blaze_typeck::ItemKey,
}

fn world() -> World {
    let mut fx = Fx::new();
    let (unit, u8_, usize_) = (fx.unit(), fx.u8(), fx.usize());
    let ptr = fx.ty(PNode::Ptr(Mutbl::Not, u8_));
    let arr2 = fx.array(ptr, 2);
    let gcc = fx.strukt("Gcc", 0, &[("private", arr2)]);
    let gcc_ty = fx.adt(gcc, &[]);
    fx.func("null", 0, &[], ptr, std::vec::Vec::new());
    let n0 = fx.ty(PNode::ConstParam(0));
    let arr_n = fx.ty(PNode::Array(u8_, n0));
    let take = fx.func("take", 1, &[arr_n], unit, std::vec::Vec::new());
    fx.const_param(take, 0, usize_);
    let make = fx.func("make", 0, &[], gcc_ty, std::vec::Vec::new());
    let f = fx.func("f", 0, &[], unit, std::vec::Vec::new());
    let g = fx.func("g", 0, &[], unit, std::vec::Vec::new());
    let h = fx.func("h", 0, &[], unit, std::vec::Vec::new());
    fx.finish();
    World { fx, make, f, g, h }
}

/// gcc.rs:67's form: a field declared `[*const u8; 2]` filled by `[null(); _]`. The count is an
/// inferred const fixed by the field's type (the Reference, "Array expressions"): clean, the
/// repeat typed `[*const u8; 2]`. Before the fix: E0000 `UnderscoreExpr` at the `_`.
///
/// Prints: `test typeck_tests::infer_const::a_repeat_count_underscore_takes_the_fields_length ... ok`
#[test]
fn a_repeat_count_underscore_takes_the_fields_length() {
    let w = world();
    let text = "fn make() -> Gcc {\n    Gcc { private: [null(); _] }\n}";
    let (c, ranges, _) = lower_and_check_with(&w.fx, w.make, text, &mut Holes);
    let lines = typed_lines(&w.fx, text, &c, &ranges);
    let codes: std::vec::Vec<DiagCode> = c.diags.iter().map(|d| d.code).collect();
    assert!(codes.is_empty(), "unexpected diagnostics {codes:?}\n{lines}");
    for want in ["[null(); _] => [*const u8; 2]", "Gcc { private: [null(); _] } => Gcc"] {
        assert!(lines.contains(want), "no `{want}` in\n{lines}");
    }
}

/// An array type's length written `_` (the Reference, "Array types": the length is a const
/// argument, which may be inferred): `let a: [u8; _] = [0; 3];` is clean and `a` is `[u8; 3]`.
///
/// Prints: `test typeck_tests::infer_const::an_array_types_length_underscore_is_inferred ... ok`
#[test]
fn an_array_types_length_underscore_is_inferred() {
    let w = world();
    let text = "fn f() {\n    let a: [u8; _] = [0; 3];\n    a;\n}";
    let (c, ranges, _) = lower_and_check_with(&w.fx, w.f, text, &mut Holes);
    let lines = typed_lines(&w.fx, text, &c, &ranges);
    let codes: std::vec::Vec<DiagCode> = c.diags.iter().map(|d| d.code).collect();
    assert!(codes.is_empty(), "unexpected diagnostics {codes:?}\n{lines}");
    for want in ["[0; 3] => [u8; 3]", "a => [u8; 3]"] {
        assert!(lines.contains(want), "no `{want}` in\n{lines}");
    }
}

/// A const generic argument written `_` (the Reference, "Generic arguments", inferred const):
/// `take::<_>([0u8; 2])` takes `N = 2` from the argument's type, never an E0747.
///
/// Prints: `test typeck_tests::infer_const::a_const_generic_argument_underscore_is_inferred ... ok`
#[test]
fn a_const_generic_argument_underscore_is_inferred() {
    let w = world();
    let text = "fn g() {\n    take::<_>([0u8; 2]);\n}";
    let (c, ranges, _) = lower_and_check_with(&w.fx, w.g, text, &mut Holes);
    let lines = typed_lines(&w.fx, text, &c, &ranges);
    let codes: std::vec::Vec<DiagCode> = c.diags.iter().map(|d| d.code).collect();
    assert!(codes.is_empty(), "unexpected diagnostics {codes:?}\n{lines}");
    for want in ["take::<_> => fn take<2>", "[0u8; 2] => [u8; 2]"] {
        assert!(lines.contains(want), "no `{want}` in\n{lines}");
    }
}

/// An inferred const nothing fixes: `let a = [0u8; _];`. The Reference leaves its value to
/// inference; with none, the body is E0282 (type annotations needed), at a place in the body:
/// never E0000 for `_` as an expression.
///
/// Prints: `inferred const left open: [("E0282", "<text at the finding>")]` and
/// `test typeck_tests::infer_const::an_unresolved_inferred_const_is_e0282 ... ok`
#[test]
fn an_unresolved_inferred_const_is_e0282() {
    let w = world();
    let text = "fn h() {\n    let a = [0u8; _];\n    a;\n}";
    let (c, _, _) = lower_and_check_with(&w.fx, w.h, text, &mut Holes);
    let found = findings(text, &c);
    let codes: std::vec::Vec<DiagCode> = found.iter().map(|(code, _)| *code).collect();
    assert!(!codes.is_empty() && codes.iter().all(|&code| code == DiagCode::AnnotationsNeeded), "want only E0282, got {codes:?}");
    for (_, at) in &found {
        assert!(!at.is_empty(), "the E0282 names no place in the body: {found:?}");
    }
    let shown: std::vec::Vec<(&str, &str)> = found.iter().map(|(code, at)| (code.code(), *at)).collect();
    std::println!("inferred const left open: {shown:?}");
}
