//! The bounds written on a trait's associated type hold of its projection on a type parameter.
//! tempo's "759 diag E0277: Try" (every `serde::Serialize` derive's body), reduced:
//!
//! ```text
//! trait SerializeStruct {
//!     type Ok; type Error;
//!     fn serialize_field(&mut self, key: &'static str, value: &u8) -> Result<(), Self::Error>;
//!     fn end(self) -> Result<Self::Ok, Self::Error>;
//! }
//! trait Serializer: Sized {
//!     type Ok; type Error;
//!     type SerializeStruct: SerializeStruct<Ok = Self::Ok, Error = Self::Error>;
//!     fn serialize_struct(self, name: &'static str, len: usize) -> Result<Self::SerializeStruct, Self::Error>;
//! }
//! fn serialize<S: Serializer>(from: &u8, s: S) -> Result<S::Ok, S::Error> {
//!     let state = Serializer::serialize_struct(s, "Burn", 1usize)?;
//!     SerializeStruct::serialize_field(&mut state, "from", from)?;
//!     SerializeStruct::end(state)
//! }
//! ```
//!
//! (serde's `serialize_field` is generic over the value's type; `&u8` here: the class does not
//! turn on it.) The rules relied on, from the Reference:
//!
//! - "Call expressions", "Disambiguating function calls": "All function calls are sugar for a
//!   more explicit fully-qualified syntax." `Serializer::serialize_struct(s, ..)` names the
//!   trait's fn with `Self` to be inferred; the first argument makes it `S`, so the call's type
//!   is `Result<<S as Serializer>::SerializeStruct, <S as Serializer>::Error>`.
//! - "Operator expressions", "The question mark operator": "When applied to values of the
//!   `Result<T, E>` type, it propagates errors. If the value is `Err(e)`, then it will return
//!   `Err(From::from(e))` from the enclosing function or closure. If applied to `Ok(x)`, then
//!   it will unwrap the value to evaluate to `x`." Through core's `impl<T, E> Try for
//!   Result<T, E>` (library/core/src/result.rs), whose parameters are `Sized` as every type
//!   parameter is.
//! - "Associated Items", "Associated Types": "The optional trait bounds must be fulfilled by
//!   the implementations of the type alias. There is an implicit `Sized` bound on associated
//!   types that can be relaxed using the special `?Sized` bound." So for any `S: Serializer`,
//!   `<S as Serializer>::SerializeStruct` is `Sized` and is a `SerializeStruct` whose `Ok` and
//!   `Error` are `S`'s: nothing in the function's where clauses says so; the trait does.
//!
//! The cause as found (2026-11-16): the checker gave the solver no item bounds
//! (infer/facts.rs `fill_trait` wrote `bounds_len: 0`, `trait_def` wrote `item_bounds: &[]`),
//! so `Result<<S as Serializer>::SerializeStruct, _>: Try` failed under the impl's implied
//! `T: Sized`: the alias is rigid and no candidate said it is `Sized`. The bounds are now the
//! predicates of the associated type's own key ([`Fx::set_assoc_bounds`] here; blaze-ladder
//! front2/heads.rs `trait_members` in the front end).
//!
//! The world answers each trait path whole with the fn it names (`Def::Fn`, `Self` fresh), as
//! the front end does for `Serializer::serialize_struct`: here by the fn's name alone.

use std::format;
use std::string::String;
use std::vec::Vec;

use super::check_body;
use crate::body::IntSuffix;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, Mutbl, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;
use crate::world::{CtorKind, Def};

/// The program above over core's `?` items as rung 7c has them (`fixture::rung_7c`): `Sized`,
/// `Infallible`, `ControlFlow`, `Result`, `Try`, `FromResidual`, `From`, and
///
/// ```text
/// impl<T, E> Try for Result<T, E> { type Output = T; type Residual = Result<Infallible, E>; }
/// impl<T, E, F: From<E>> FromResidual<Result<Infallible, E>> for Result<T, F> {}
/// impl<T> From<T> for T {}
/// ```
///
/// `with_bounds: false` leaves every associated type's bounds unsaid (the world before the
/// fix, and the control: the body must then fail).
fn world(with_bounds: bool) -> (Fx, ItemKey) {
    let mut world = Fx::new();
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1, p2) = (fx.param(0), fx.param(1), fx.param(2));
    let (infallible, _) = fx.enum_("Infallible", 0, &[]);
    let infallible_t = fx.adt(infallible, &[]);
    let (control_flow, _) = fx.enum_("ControlFlow", 2, &[("Continue", &[p1]), ("Break", &[p0])]);
    let (result, _) = fx.enum_("Result", 2, &[("Ok", &[p0]), ("Err", &[p1])]);

    let from_residual_tr = fx.tr("FromResidual", 2, Vec::new(), &[]);
    let from_residual = fx.trait_fn(from_residual_tr, "from_residual", false, &[p1], p0);
    let try_tr = fx.tr("Try", 1, Vec::new(), &["Output", "Residual"]);
    let residual = fx.alias(try_tr, "Residual", &[p0]);
    let output = fx.alias(try_tr, "Output", &[p0]);
    let sup = fx.bound(from_residual_tr, &[p0, residual]);
    fx.set_supers(try_tr, std::vec![sup]);
    let cf_ret = fx.adt(control_flow, &[residual, output]);
    let branch = fx.trait_fn(try_tr, "branch", true, &[p0], cf_ret);
    let from_sized = fx.bound(sized, &[p0]);
    let from_tr = fx.tr("From", 2, std::vec![from_sized], &[]);
    fx.trait_fn(from_tr, "from", false, &[p1], p0);

    let res_p0_p1 = fx.adt(result, &[p0, p1]);
    let res_inf_p1 = fx.adt(result, &[infallible_t, p1]);
    fx.imp("impl<T, E> Try for Result<T, E>", 2, res_p0_p1, Some((try_tr, &[])), &[("Output", p0), ("Residual", res_inf_p1)], Vec::new());
    let res_p0_p2 = fx.adt(result, &[p0, p2]);
    let from_e = fx.bound(from_tr, &[p2, p1]);
    fx.imp(
        "impl<T, E, F: From<E>> FromResidual<Result<Infallible, E>> for Result<T, F>",
        3,
        res_p0_p2,
        Some((from_residual_tr, &[res_inf_p1])),
        &[],
        std::vec![from_e],
    );
    fx.imp("impl<T> From<T> for T", 1, p0, Some((from_tr, &[p0])), &[], Vec::new());
    for (l, def) in [
        (LangItem::TryTraitBranch, Def::Fn(branch)),
        (LangItem::TryTraitFromResidual, Def::Fn(from_residual)),
        (LangItem::ControlFlowContinue, Def::Ctor { adt: control_flow, variant: 0, kind: CtorKind::Tuple }),
        (LangItem::ControlFlowBreak, Def::Ctor { adt: control_flow, variant: 1, kind: CtorKind::Tuple }),
    ] {
        fx.lang_def(l, def);
    }

    let str_ = fx.ty(PNode::Str);
    let r_str = fx.r(Mutbl::Not, str_);
    let u8_ = fx.u8();
    let r_u8 = fx.r(Mutbl::Not, u8_);
    let unit = fx.unit();
    let usize_ = fx.usize();

    // trait SerializeStruct { type Ok; type Error; fn serialize_field(..); fn end(self); }
    let ss = fx.tr("SerializeStruct", 1, Vec::new(), &["Ok", "Error"]);
    let ss_ok = fx.alias(ss, "Ok", &[p0]);
    let ss_err = fx.alias(ss, "Error", &[p0]);
    let r_mut_self = fx.r(Mutbl::Mut, p0);
    let res_unit_ss_err = fx.adt(result, &[unit, ss_err]);
    let serialize_field = fx.trait_fn(ss, "serialize_field", true, &[r_mut_self, r_str, r_u8], res_unit_ss_err);
    let res_ss_ok_err = fx.adt(result, &[ss_ok, ss_err]);
    let end = fx.trait_fn(ss, "end", true, &[p0], res_ss_ok_err);

    // trait Serializer: Sized { type Ok; type Error; type SerializeStruct: ..; fn serialize_struct(..); }
    let ser_sized = fx.bound(sized, &[p0]);
    let ser = fx.tr("Serializer", 1, std::vec![ser_sized], &["Ok", "Error", "SerializeStruct"]);
    let ser_ok = fx.alias(ser, "Ok", &[p0]);
    let ser_err = fx.alias(ser, "Error", &[p0]);
    let ser_state = fx.alias(ser, "SerializeStruct", &[p0]);
    let res_state_err = fx.adt(result, &[ser_state, ser_err]);
    let serialize_struct = fx.trait_fn(ser, "serialize_struct", true, &[p0, r_str, usize_], res_state_err);

    if with_bounds {
        // Each associated type's implied `Sized`, and `SerializeStruct`'s written bound with
        // its two `Name = Type` parts, `Self` the alias over the trait's `Self`.
        for (tr, name, alias) in [(ss, "Ok", ss_ok), (ss, "Error", ss_err), (ser, "Ok", ser_ok), (ser, "Error", ser_err)] {
            let is_sized = fx.bound(sized, &[alias]);
            fx.set_assoc_bounds(tr, name, std::vec![is_sized]);
        }
        let is_ss = fx.bound(ss, &[ser_state]);
        let same_ok = fx.proj(ss, &[ser_state], "Ok", ser_ok);
        let same_err = fx.proj(ss, &[ser_state], "Error", ser_err);
        let is_sized = fx.bound(sized, &[ser_state]);
        fx.set_assoc_bounds(ser, "SerializeStruct", std::vec![is_ss, same_ok, same_err, is_sized]);
    }

    fx.use_path("serialize_struct", Def::Fn(serialize_struct));
    fx.use_path("serialize_field", Def::Fn(serialize_field));
    fx.use_path("end", Def::Fn(end));

    // fn serialize<S: Serializer>(from: &u8, s: S) -> Result<S::Ok, S::Error>
    let is_ser = fx.bound(ser, &[p0]);
    let res_ok_err = fx.adt(result, &[ser_ok, ser_err]);
    let serialize = fx.func("serialize", 1, &[r_u8, p0], res_ok_err, std::vec![is_ser]);
    fx.finish();
    (world, serialize)
}

/// The function's body; with `int_try`, `1u8?;` is its first statement. Returns the body, its
/// root, and for `1u8?` the literal and the last expression of its desugaring (the `match`).
fn body(int_try: bool) -> (Bb, ExprId, Option<(ExprId, ExprId)>) {
    let mut b = Bb::new();
    let from = b.param("from");
    let s = b.param("s");
    let mut stmts = Vec::new();
    let mut int = None;
    if int_try {
        let one = b.int(Some(IntSuffix::Uint(UintTy::U8)));
        let t = b.try_op(one);
        stmts.push(b.semi(t));
        int = Some((one, t));
    }
    // let state = Serializer::serialize_struct(s, "Burn", 1usize)?;
    let f0 = b.path(&["serialize_struct"]);
    let a0 = b.local(s);
    let a1 = b.str_();
    let a2 = b.int(Some(IntSuffix::Uint(UintTy::Usize)));
    let call0 = b.call(f0, &[a0, a1, a2]);
    let t0 = b.try_op(call0);
    let state = b.bind("state");
    stmts.push(b.let_(state, None, t0));
    // SerializeStruct::serialize_field(&mut state, "from", from)?;
    let f1 = b.path(&["serialize_field"]);
    let st1 = b.local(state);
    let m1 = b.borrow(Mutbl::Mut, st1);
    let k1 = b.str_();
    let v1 = b.local(from);
    let call1 = b.call(f1, &[m1, k1, v1]);
    let t1 = b.try_op(call1);
    stmts.push(b.semi(t1));
    // SerializeStruct::end(state)
    let f2 = b.path(&["end"]);
    let st2 = b.local(state);
    let call2 = b.call(f2, &[st2]);
    let root = b.block(&stmts, Some(call2));
    (b, root, int)
}

/// Each diagnostic as `code at <expression> found <type>`.
fn diags_shown(fx: &Fx, c: &crate::columns::Checked<'_>) -> Vec<String> {
    c.diags
        .iter()
        .map(|d| format!("{} at {} found {:?}", d.code.code(), d.at.start, d.found.map(|t| super::show(fx, c.types.view(), t))))
        .collect()
}

/// The derive's body checks: no diagnostic. Without the associated types' bounds reaching the
/// solver this is E0277 at the first `?` (`<P0 as Serializer>::SerializeStruct: Sized` under
/// `Result<..>: Try`), the tempo class.
///
/// Prints: `test suite::assoc_item_bounds::question_mark_on_a_trait_path_call_through_a_parameter ... ok`
#[test]
fn question_mark_on_a_trait_path_call_through_a_parameter() {
    let (fx, serialize) = world(true);
    let (b, root, _) = body(false);
    let c = check_body(&fx, &b, serialize, root, Plant::None, InferPlant::None);
    let d = diags_shown(&fx, &c);
    assert!(d.is_empty(), "the derive's body has diagnostics:\n{}", d.join("\n"));
}

/// The same body with `1u8?;` first: `u8` has no `Try` impl, so E0277 on it (at the `?`'s
/// `Try::branch` path, the expression after the literal), and every diagnostic is inside that
/// one `?`: the three serde statements after it still check. A fix that stops asking `Try`
/// fails here.
///
/// Prints: `test suite::assoc_item_bounds::question_mark_on_an_integer_is_still_refused ... ok`
#[test]
fn question_mark_on_an_integer_is_still_refused() {
    let (fx, serialize) = world(true);
    let (b, root, int) = body(true);
    let (one, last) = int.expect("the body was built with `1u8?`");
    let c = check_body(&fx, &b, serialize, root, Plant::None, InferPlant::None);
    let d = diags_shown(&fx, &c);
    let on_u8 = c.diags.iter().any(|x| {
        x.code == DiagCode::Unsatisfied
            && x.at.start == one.0 + 1
            && x.found.map(|t| super::show(&fx, c.types.view(), t)).as_deref() == Some("u8")
    });
    assert!(on_u8, "no E0277 for `u8: Try` at expression {}:\n{}", one.0 + 1, d.join("\n"));
    let outside: Vec<&String> = c.diags.iter().zip(d.iter()).filter(|(x, _)| x.at.start > last.0).map(|(_, s)| s).collect();
    assert!(outside.is_empty(), "diagnostics outside `1u8?` (expressions {}..={}):\n{outside:?}", one.0, last.0);
}

/// The control (rule 23: remove the cause, the effect goes): the same world with no associated
/// type bounds said is the world before the fix, and the derive's body is then refused with
/// E0277. If this body passed without them, the test above would prove nothing.
///
/// Prints: `test suite::assoc_item_bounds::control_without_the_bounds_the_body_is_refused ... ok`
#[test]
fn control_without_the_bounds_the_body_is_refused() {
    let (fx, serialize) = world(false);
    let (b, root, _) = body(false);
    let c = check_body(&fx, &b, serialize, root, Plant::None, InferPlant::None);
    let d = diags_shown(&fx, &c);
    assert!(c.diags.iter().any(|x| x.code == DiagCode::Unsatisfied), "no E0277 without the bounds:\n{}", d.join("\n"));
}
