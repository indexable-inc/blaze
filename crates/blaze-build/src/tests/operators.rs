//! Operators as the check's callee rows say (walk/operators.rs): the builtin operator on
//! builtin scalar types, a call of the trait's method everywhere else. Built and run natively.
//!
//! The property the unit tests lacked (every comparison in tests/matches.rs dereferences
//! first, so `&u32 == &u32` compared two ADDRESSES unseen): for generated values and every
//! operator of the table, the built program's answer at `T`, at `&T` and at `&&T` is one
//! value, the operator's definition computed here.
//!
//! The Reference, "Operator expressions": [expr.cmp.place] "Unlike the arithmetic and logical
//! operators above, these operators implicitly take shared borrows of their operands,
//! evaluating them in place expression context: `a == b;` is equivalent to
//! `::std::cmp::PartialEq::eq(&a, &b);`"; [expr.cmp.behavior]: `==` is
//! `std::cmp::PartialEq::eq`, `!=` `ne`, `>` `std::cmp::PartialOrd::gt`, `<` `lt`, `>=` `ge`,
//! `<=` `le`; [expr.arith-logic.behavior]: "This table summarizes the behavior of arithmetic
//! and logical binary operators on primitive types and which traits are used to overload these
//! operators for other types" (`+` `std::ops::Add`, `-` `Sub`, `*` `Mul`, `&` `BitAnd`, `|`
//! `BitOr`, `^` `BitXor`; `+=` `AddAssign`); [expr.negate.results]: `-` is `std::ops::Neg`,
//! `!` is `std::ops::Not`; [expr.compound-assign.trait]: "this expression is syntactic sugar
//! for using the corresponding trait for the operator .. and calling its method with the left
//! hand side as the receiver and the right hand side as the next argument."
//!
//! The program's library, written as the pinned `core` writes it (library/core/src/cmp.rs:
//! `impl<A: PointeeSized, B: PointeeSized> PartialEq<&B> for &A where A: PartialEq<B> { fn
//! eq(&self, other: &&B) -> bool { PartialEq::eq(*self, *other) } .. }`, which is `**self ==
//! **other`; library/core/src/internal_macros.rs `forward_ref_binop!`: `impl Add<&u32> for
//! &u32`, `impl Add<&u32> for u32`, `impl Add<u32> for &u32`, each `Add::add(*self, *other)`
//! on the values; `forward_ref_unop!`; `forward_ref_op_assign!`):
//! ```text
//! #[lang = "sized"] trait Sized {}
//! #[lang = "eq"] trait PartialEq<Rhs> { fn eq(&self, other: &Rhs) -> bool; fn ne(&self, other: &Rhs) -> bool; }
//! #[lang = "partial_ord"] trait PartialOrd<Rhs> { fn lt(&self, other: &Rhs) -> bool; .. le, gt, ge }
//! impl PartialEq<i32> for i32 { fn eq(&self, other: &i32) -> bool { *self == *other } .. }
//! impl<A: PartialEq<B>, B> PartialEq<&B> for &A { fn eq(&self, other: &&B) -> bool { **self == **other } .. }
//! (PartialOrd likewise)
//! #[lang = "add"] trait Add<Rhs> { type Output; fn add(self, rhs: Rhs) -> Self::Output; }
//! impl Add<i32> for i32 { type Output = i32; fn add(self, other: i32) -> i32 { self + other } }
//! impl Add<&i32> for &i32 { type Output = i32; fn add(self, other: &i32) -> i32 { *self + *other } }
//! impl Add<&i32> for i32 { .. self + *other }      impl Add<i32> for &i32 { .. *self + other }
//! (Sub, Mul, BitAnd, BitOr, BitXor likewise)
//! #[lang = "neg"] trait Neg { type Output; fn neg(self) -> Self::Output; }
//! impl Neg for i32 { .. -self }                    impl Neg for &i32 { .. -*self }   (Not likewise)
//! #[lang = "add_assign"] trait AddAssign<Rhs> { fn add_assign(&mut self, rhs: Rhs); }
//! impl AddAssign<i32> for i32 { .. *self += other }  impl AddAssign<&i32> for i32 { .. *self += *other }
//! fn main() -> i64;
//! ```
//! Which impl an operator's trait method runs is `blaze_typeck::resolve_instance`'s answer,
//! as in the driver: the forwarding impls are ordinary impls, and the walk knows none of them.

use std::alloc::Global;
use std::format;
use std::string::String;
use std::vec::Vec as StdVec;

use blaze_own::Alloc;
use blaze_sim::{Broke, Config, Tape};
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{
    Adt, BinOp, BodyView, Checked, Expr, ExprId, InferCtx, IntTy, ItemKey, LangItem, List, Mutbl, Needs, Outcome, PNode, PTy, PortableTys,
    UnOp,
};

use super::run_native_with;
use crate::layout::{LayoutFacts, Missing, ReprAttrs, ValidRange};
use crate::{Abi, Adts, Built, Planted, Subst};

/// One impl fn of the library: its item and its written body.
struct ImplFn {
    item: ItemKey,
    b: Bb,
    root: ExprId,
}

struct World {
    fx: Fx,
    main: ItemKey,
    impls: StdVec<ImplFn>,
}

/// What an impl fn's body is, over its parameters `self` and `other`.
#[derive(Clone, Copy)]
enum Shape {
    Binary(BinOp),
    Unary(UnOp),
    /// `*self op= other`.
    Assign(BinOp),
}

/// An expression node (the fixture's `Bb::e` is its crate's own).
fn put(b: &mut Bb, x: Expr) -> ExprId {
    let at = b.int(None);
    b.exprs[at.ix()] = x;
    at
}

/// `e` dereferenced `n` times.
fn derefs(b: &mut Bb, mut e: ExprId, n: u32) -> ExprId {
    for _ in 0..n {
        e = put(b, Expr::Unary { op: UnOp::Deref, operand: e });
    }
    e
}

/// The body of impl fn `item`: `self` dereferenced `dl` times, `other` `dr` times, joined as
/// `shape` says.
fn impl_body(item: ItemKey, shape: Shape, dl: u32, dr: u32) -> ImplFn {
    let mut b = Bb::new();
    let s = b.param("self");
    let root = match shape {
        Shape::Unary(op) => {
            let x = b.local(s);
            let x = derefs(&mut b, x, dl);
            let v = put(&mut b, Expr::Unary { op, operand: x });
            b.block(&[], Some(v))
        }
        Shape::Binary(op) => {
            let o = b.param("other");
            let (l, r) = (b.local(s), b.local(o));
            let (lhs, rhs) = (derefs(&mut b, l, dl), derefs(&mut b, r, dr));
            let v = put(&mut b, Expr::Binary { op, lhs, rhs });
            b.block(&[], Some(v))
        }
        Shape::Assign(op) => {
            let o = b.param("other");
            let (l, r) = (b.local(s), b.local(o));
            let (lhs, rhs) = (derefs(&mut b, l, dl), derefs(&mut b, r, dr));
            let v = put(&mut b, Expr::AssignOp { op, lhs, rhs });
            let st = b.semi(v);
            b.block(&[st], None)
        }
    };
    ImplFn { item, b, root }
}

const EQ: [(BinOp, &str); 2] = [(BinOp::Eq, "eq"), (BinOp::Ne, "ne")];
const ORD: [(BinOp, &str); 4] = [(BinOp::Lt, "lt"), (BinOp::Le, "le"), (BinOp::Gt, "gt"), (BinOp::Ge, "ge")];
const ARITH: [(BinOp, &str, &str, LangItem); 6] = [
    (BinOp::Add, "Add", "add", LangItem::Add),
    (BinOp::Sub, "Sub", "sub", LangItem::Sub),
    (BinOp::Mul, "Mul", "mul", LangItem::Mul),
    (BinOp::BitAnd, "BitAnd", "bitand", LangItem::BitAnd),
    (BinOp::BitOr, "BitOr", "bitor", LangItem::BitOr),
    (BinOp::BitXor, "BitXor", "bitxor", LangItem::BitXor),
];
const UNARY: [(UnOp, &str, &str, LangItem); 2] = [(UnOp::Neg, "Neg", "neg", LangItem::Neg), (UnOp::Not, "Not", "not", LangItem::Not)];

/// The module doc's library.
fn world() -> World {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, StdVec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let t = fx.ty(PNode::Int(IntTy::I32));
    let r = fx.r(Mutbl::Not, t);
    let bool_ = fx.ty(PNode::Bool);
    let unit = fx.unit();
    let i64_ = fx.ty(PNode::Int(IntTy::I64));
    let main = fx.func("main", 0, &[], i64_, StdVec::new());
    let (rp0, rp1) = (fx.r(Mutbl::Not, p0), fx.r(Mutbl::Not, p1));
    let (rrp0, rrp1) = (fx.r(Mutbl::Not, rp0), fx.r(Mutbl::Not, rp1));
    let mut impls = StdVec::new();
    for (name, lang, ops) in [("PartialEq", LangItem::PartialEq, &EQ[..]), ("PartialOrd", LangItem::PartialOrd, &ORD[..])] {
        let tr = fx.tr(name, 2, StdVec::new(), &[]);
        fx.lang(lang, tr);
        for &(_, method) in ops {
            fx.trait_fn(tr, method, true, &[rp0, rp1], bool_);
        }
        let at_t = fx.imp("for i32", 0, t, Some((tr, &[t])), &[], StdVec::new());
        let bound = fx.bound(tr, &[p0, p1]);
        let at_ref = fx.imp("for &A", 2, rp0, Some((tr, &[rp1])), &[], std::vec![bound]);
        for &(op, method) in ops {
            let item = fx.impl_fn(at_t, method, true, &[r, r], bool_);
            impls.push(impl_body(item, Shape::Binary(op), 1, 1));
            let item = fx.impl_fn(at_ref, method, true, &[rrp0, rrp1], bool_);
            impls.push(impl_body(item, Shape::Binary(op), 2, 2));
        }
    }
    for (op, name, method, lang) in ARITH {
        let tr = fx.tr(name, 2, StdVec::new(), &["Output"]);
        fx.lang(lang, tr);
        let out = fx.alias(tr, "Output", &[p0, p1]);
        fx.trait_fn(tr, method, true, &[p0, p1], out);
        for (self_ty, rhs, dl, dr) in [(t, t, 0, 0), (r, r, 1, 1), (t, r, 0, 1), (r, t, 1, 0)] {
            let imp = fx.imp("an arithmetic impl", 0, self_ty, Some((tr, &[rhs])), &[("Output", t)], StdVec::new());
            let item = fx.impl_fn(imp, method, true, &[self_ty, rhs], t);
            impls.push(impl_body(item, Shape::Binary(op), dl, dr));
        }
    }
    for (op, name, method, lang) in UNARY {
        let tr = fx.tr(name, 1, StdVec::new(), &["Output"]);
        fx.lang(lang, tr);
        let out = fx.alias(tr, "Output", &[p0]);
        fx.trait_fn(tr, method, true, &[p0], out);
        for (self_ty, dl) in [(t, 0), (r, 1)] {
            let imp = fx.imp("a unary impl", 0, self_ty, Some((tr, &[])), &[("Output", t)], StdVec::new());
            let item = fx.impl_fn(imp, method, true, &[self_ty], t);
            impls.push(impl_body(item, Shape::Unary(op), dl, 0));
        }
    }
    let tr = fx.tr("AddAssign", 2, StdVec::new(), &[]);
    fx.lang(LangItem::AddAssign, tr);
    let mp0 = fx.r(Mutbl::Mut, p0);
    fx.trait_fn(tr, "add_assign", true, &[mp0, p1], unit);
    let mt = fx.r(Mutbl::Mut, t);
    for (rhs, dr) in [(t, 0), (r, 1)] {
        let imp = fx.imp("an assigning impl", 0, t, Some((tr, &[rhs])), &[], StdVec::new());
        let item = fx.impl_fn(imp, "add_assign", true, &[mt, rhs], unit);
        impls.push(impl_body(item, Shape::Assign(BinOp::Add), 1, dr));
    }
    fx.finish();
    World { fx, main, impls }
}

struct OpAdts<'a> {
    w: &'a World,
    /// `main`'s literals.
    lits: &'a [(ExprId, u128)],
    planted: Planted,
}

impl LayoutFacts for OpAdts<'_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.w.fx.adt_of(key)
    }

    fn repr(&self, _: ItemKey) -> Result<ReprAttrs, Missing> {
        Ok(ReprAttrs::RUST)
    }

    fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> {
        Ok(None)
    }

    fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> {
        Ok(ValidRange::default())
    }
}

impl Adts for OpAdts<'_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.w.fx.adt_of(key)
    }

    /// The operator fixtures were written for wrapping arithmetic: the release setting.
    fn overflow_checks(&self) -> crate::OverflowChecks {
        crate::OverflowChecks::Off
    }

    fn ub_checks(&self) -> crate::UbChecks {
        crate::UbChecks::Off
    }

    fn facts(&self) -> &dyn LayoutFacts {
        self
    }

    fn int_lit(&self, item: ItemKey, expr: ExprId) -> Option<u128> {
        if item != self.w.main {
            return None;
        }
        self.lits.iter().find(|(e, _)| *e == expr).map(|&(_, v)| v)
    }

    fn abi(&self, item: ItemKey) -> Option<Abi> {
        self.w.fx.sig_of(item).map(|_| Abi::Rust)
    }

    fn drop_impl(&self, _: PTy, _: &mut PortableTys<'_>) -> Result<Option<ItemKey>, crate::BuildError> {
        Ok(None)
    }

    fn planted(&self) -> Planted {
        self.planted
    }
}

fn checked_of(w: &World, view: &BodyView<'_>, what: &str) -> Result<Checked<'static>, String> {
    let mut ty = InferCtx::new_in(Alloc::global());
    let c = match blaze_typeck::check(view, &w.fx, &mut ty, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        Ok(Outcome::Need(n)) => return Err(format!("the check of {what} needed {:?}", n.questions)),
        Err(error) => return Err(format!("the check of {what} failed: {error:?}")),
    };
    if !c.diags.is_empty() {
        return Err(format!("the check of {what} gave diagnostics: {:?}", c.diags));
    }
    Ok(c)
}

/// Checks and builds `main` (body `mb`), then by a work list every instance its calls reach:
/// each reference's impl fn as `blaze_typeck::resolve_instance` selects it (the driver's
/// resolver), built at the impl's arguments. Runs `main` natively and returns its value.
fn run_main(w: &World, mb: &Bb, root: ExprId, lits: &[(ExprId, u128)], planted: Planted) -> Result<i64, String> {
    let adts = OpAdts { w, lits, planted };
    let mut ground = PortableTys::new_in(Alloc::global());
    let finish = |built: &Built<Global>, what: &str| -> Result<(), String> {
        blaze_lir::verify(built.func.view()).map_err(|error| format!("{what} does not verify: {error}"))?;
        super::emits(built, &[]);
        Ok(())
    };
    let mview = mb.view(w.main, root);
    let mchecked = checked_of(w, &mview, "main")?;
    let sig = w.fx.sig_of(w.main).ok_or_else(|| String::from("no signature for main"))?;
    let main = {
        let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
        crate::build(&mview, &mchecked, &sig, &mut subst, &adts, Global).map_err(|error| format!("the build of main failed: {error}"))?
    };
    finish(&main, "main")?;
    let views: StdVec<BodyView<'_>> = w.impls.iter().map(|p| p.b.view(p.item, p.root)).collect();
    let mut checked: StdVec<Option<Checked<'static>>> = views.iter().map(|_| None).collect();
    let mut bodies: StdVec<(ItemKey, List, Built<Global>)> = StdVec::new();
    let mut todo: StdVec<(ItemKey, List)> = main.refs.iter().map(|r| (r.item, r.args)).collect();
    while let Some((item, args)) = todo.pop() {
        if bodies.iter().any(|(i, a, _)| *i == item && *a == args) {
            continue;
        }
        let name = w.fx.name_of(item);
        let r = blaze_typeck::resolve_instance(&w.fx, item, args, &mut ground, Alloc::global())
            .map_err(|error| format!("`{name}` has no impl at its arguments: {error:?}"))?;
        let at = w.impls.iter().position(|p| p.item == r.item).ok_or_else(|| format!("`{name}` resolved to a fn with no body here"))?;
        if checked[at].is_none() {
            checked[at] = Some(checked_of(w, &views[at], name)?);
        }
        let Some(c) = checked[at].as_ref() else { return Err(String::from("a body checked and lost")) };
        let gargs = ground.view().list(r.args).map_err(|_| String::from("an instance's arguments past the ground table"))?.to_vec();
        let sig = w.fx.sig_of(r.item).ok_or_else(|| format!("no signature for `{name}`'s impl fn"))?;
        let built = {
            let mut subst = Subst { args: &gargs, ground: &mut ground, scratch: Alloc::global() };
            crate::build(&views[at], c, &sig, &mut subst, &adts, Global).map_err(|error| format!("the build of `{name}`'s impl fn failed: {error}"))?
        };
        finish(&built, name)?;
        todo.extend(built.refs.iter().map(|r| (r.item, r.args)));
        bodies.push((item, args, built));
    }
    // Callees before callers, as tests/closures.rs `run_progs` places them.
    let mut fns: StdVec<(ItemKey, Option<List>, &Built<Global>)> = StdVec::new();
    let mut placed = std::vec![false; bodies.len()];
    for _ in 0..=bodies.len() {
        for (j, (i, a, b)) in bodies.iter().enumerate() {
            let ready = b.refs.iter().all(|r| bodies.iter().zip(&placed).all(|((ci, ca, _), &p)| p || *ci != r.item || *ca != r.args));
            if !placed[j] && ready {
                placed[j] = true;
                fns.push((*i, Some(*a), b));
            }
        }
    }
    if !placed.iter().all(|&p| p) {
        return Err(String::from("a cycle among the built bodies"));
    }
    fns.push((w.main, None, &main));
    Ok(run_native_with(&[], &fns) as i64)
}

/// One operator of the table.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Form {
    Cmp(BinOp),
    Arith(BinOp),
    Unary(UnOp),
    AddAssign,
}

const FORMS: [Form; 15] = [
    Form::Cmp(BinOp::Eq),
    Form::Cmp(BinOp::Ne),
    Form::Cmp(BinOp::Lt),
    Form::Cmp(BinOp::Le),
    Form::Cmp(BinOp::Gt),
    Form::Cmp(BinOp::Ge),
    Form::Arith(BinOp::Add),
    Form::Arith(BinOp::Sub),
    Form::Arith(BinOp::Mul),
    Form::Arith(BinOp::BitAnd),
    Form::Arith(BinOp::BitOr),
    Form::Arith(BinOp::BitXor),
    Form::Unary(UnOp::Neg),
    Form::Unary(UnOp::Not),
    Form::AddAssign,
];

/// How many times each operand is borrowed, per form: every combination Rust defines for it
/// with the module doc's library (`&&a + &&b` has no impl; `&a += b` is no place of `i32`).
fn depths(form: Form) -> &'static [(u32, u32)] {
    match form {
        Form::Cmp(_) => &[(0, 0), (1, 1), (2, 2)],
        Form::Arith(_) => &[(0, 0), (1, 1), (0, 1), (1, 0)],
        Form::Unary(_) => &[(0, 0), (1, 0)],
        Form::AddAssign => &[(0, 0), (0, 1)],
    }
}

/// The operator's definition on `i32` (the Reference's tables; the values are drawn so that
/// no arithmetic overflows).
fn definition(form: Form, a: i32, b: i32) -> i64 {
    match form {
        Form::Cmp(BinOp::Eq) => i64::from(a == b),
        Form::Cmp(BinOp::Ne) => i64::from(a != b),
        Form::Cmp(BinOp::Lt) => i64::from(a < b),
        Form::Cmp(BinOp::Le) => i64::from(a <= b),
        Form::Cmp(BinOp::Gt) => i64::from(a > b),
        Form::Cmp(_) => i64::from(a >= b),
        Form::Arith(BinOp::Add) | Form::AddAssign => i64::from(a) + i64::from(b),
        Form::Arith(BinOp::Sub) => i64::from(a) - i64::from(b),
        Form::Arith(BinOp::Mul) => i64::from(a) * i64::from(b),
        Form::Arith(BinOp::BitAnd) => i64::from(a & b),
        Form::Arith(BinOp::BitOr) => i64::from(a | b),
        Form::Arith(_) => i64::from(a ^ b),
        Form::Unary(UnOp::Neg) => -i64::from(a),
        Form::Unary(_) => i64::from(!a),
    }
}

/// ```text
/// fn main() -> i64 {
///     let a: i32 = <a>;
///     let b: i32 = <b>;
///     [let l = <a borrowed dl times>; let r = <b borrowed dr times>;]    // `named`
///     (<l> op <r>) as i64        // or (op <l>) as i64, or { a += <r>; a as i64 }
/// }
/// ```
fn main_body(form: Form, (dl, dr): (u32, u32), named: bool, a: i32, b: i32) -> (Bb, ExprId, StdVec<(ExprId, u128)>) {
    let mut bb = Bb::new();
    let mut lits = StdVec::new();
    let mut stmts = StdVec::new();
    let i32_ty = bb.ty(PNode::Int(IntTy::I32));
    let i64_ty = bb.ty(PNode::Int(IntTy::I64));
    let mut bound = |bb: &mut Bb, stmts: &mut StdVec<_>, name: &str, v: i32| {
        let e = bb.int(None);
        lits.push((e, u128::from(v.unsigned_abs())));
        let e = if v < 0 { put(bb, Expr::Unary { op: UnOp::Neg, operand: e }) } else { e };
        let p = bb.bind(name);
        stmts.push(bb.let_(p, Some(i32_ty), e));
        p
    };
    let (pa, pb) = (bound(&mut bb, &mut stmts, "a", a), bound(&mut bb, &mut stmts, "b", b));
    let operand = |bb: &mut Bb, stmts: &mut StdVec<_>, name: &str, p, depth: u32| {
        let mut e = bb.local(p);
        for _ in 0..depth {
            e = bb.borrow(Mutbl::Not, e);
        }
        if !named {
            return e;
        }
        let l = bb.bind(name);
        stmts.push(bb.let_(l, None, e));
        bb.local(l)
    };
    let value = match form {
        Form::Cmp(op) | Form::Arith(op) => {
            let lhs = operand(&mut bb, &mut stmts, "l", pa, dl);
            let rhs = operand(&mut bb, &mut stmts, "r", pb, dr);
            put(&mut bb, Expr::Binary { op, lhs, rhs })
        }
        Form::Unary(op) => {
            let x = operand(&mut bb, &mut stmts, "l", pa, dl);
            put(&mut bb, Expr::Unary { op, operand: x })
        }
        Form::AddAssign => {
            let rhs = operand(&mut bb, &mut stmts, "r", pb, dr);
            let lhs = bb.local(pa);
            let set = put(&mut bb, Expr::AssignOp { op: BinOp::Add, lhs, rhs });
            stmts.push(bb.semi(set));
            bb.local(pa)
        }
    };
    let cast = put(&mut bb, Expr::Cast { operand: value, ty: i64_ty });
    let root = bb.block(&stmts, Some(cast));
    (bb, root, lits)
}

/// The words of the oracle's finding (`blaze_sim::control` looks for them).
const SAYS: &str = "where the operator's definition gives";

/// `form` on `a` and `b` at every borrow depth: each built answer is the definition's.
fn agree(w: &World, form: Form, named: bool, a: i32, b: i32, planted: Planted) -> Result<(), Broke> {
    let want = definition(form, a, b);
    for &depth in depths(form) {
        let (mb, root, lits) = main_body(form, depth, named, a, b);
        let got = run_main(w, &mb, root, &lits, planted)
            .map_err(|e| Broke::Oracle(format!("{form:?} on {a} and {b} at borrow depths {depth:?} (named: {named}): {e}")))?;
        if got != want {
            return Err(Broke::Oracle(format!(
                "{form:?} on {a} and {b} at borrow depths {depth:?} (named: {named}) is {got}, {SAYS} {want}"
            )));
        }
    }
    Ok(())
}

/// One drawn case: an operator, two values (equal half the time: two equal values in two
/// places are what tells a comparison of referents from one of addresses), written inline or
/// through locals.
fn operators_agree(w: &World, tape: &Tape, planted: Planted) -> Result<(), Broke> {
    let form = FORMS[tape.index(FORMS.len(), "operator")];
    let a = tape.below(20_001, "a") as i32 - 10_000;
    let b = if tape.coin("equal operands") { a } else { tape.below(20_001, "b") as i32 - 10_000 };
    let named = tape.coin("operands bound to locals first");
    agree(w, form, named, a, b, planted)
}

/// THE PROPERTY: for generated values and every operator of the table, the built program's
/// answer at `T`, `&T` and `&&T` (every depth Rust defines) is the operator's definition.
#[test]
fn an_operator_gives_its_definition_at_every_borrow_depth() {
    let w = world();
    blaze_sim::check(Config::cases(120), |tape: &Tape| operators_agree(&w, tape, Planted::Nothing));
}

/// The control (rule 8): with "compare the addresses" planted (walk.rs `binary` before
/// walk/operators.rs: a reference is held as a pointer, so `&i32 == &i32` was the builtin
/// comparison of two pointers), the same property fails, in the oracle's words.
#[test]
fn the_control_comparing_addresses_is_caught() {
    let w = world();
    blaze_sim::control(Config::cases(120), Planted::Nothing, Planted::CompareAddresses, SAYS, |tape: &Tape, planted: Planted| {
        operators_agree(&w, tape, planted)
    });
}

/// Every operator at every depth, inline and through locals, on equal and on unequal values:
/// no draw decides whether a form is covered. Among them the forms the walk refused or
/// miscompiled before (WRONG-NOTES.md, cause B): `&a == &b` (addresses compared), `&a + &b`
/// and `-&a` (refused by name), `a + &b` (stopped only by LIR verify).
#[test]
fn every_operator_at_every_depth_on_fixed_values() {
    let w = world();
    for form in FORMS {
        for named in [false, true] {
            for (a, b) in [(7, 7), (-3, 5), (9, -9)] {
                agree(&w, form, named, a, b, Planted::Nothing).unwrap_or_else(|e| panic!("{e:?}"));
            }
        }
    }
}

/// An operator the check typed through a trait and recorded no callee for (here: a world
/// whose `PartialEq` declares no `eq`) is refused by name, with the operator and the operand
/// types, never built as the builtin comparison of how references are held.
/// ```text
/// #[lang = "eq"] trait PartialEq<Rhs> {}
/// impl PartialEq<&i32> for &i32 {}
/// fn main() -> i64 { let a: i32 = 4; let b: i32 = 4; (&a == &b) as i64 }
/// ```
#[test]
fn an_operator_with_no_callee_row_and_no_builtin_types_is_refused_by_name() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, StdVec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let t = fx.ty(PNode::Int(IntTy::I32));
    let r = fx.r(Mutbl::Not, t);
    let i64_ = fx.ty(PNode::Int(IntTy::I64));
    let main = fx.func("main", 0, &[], i64_, StdVec::new());
    let tr = fx.tr("PartialEq", 2, StdVec::new(), &[]);
    fx.lang(LangItem::PartialEq, tr);
    fx.imp("for &i32", 0, r, Some((tr, &[r])), &[], StdVec::new());
    fx.finish();
    let w = World { fx, main, impls: StdVec::new() };
    let (mb, root, lits) = main_body(Form::Cmp(BinOp::Eq), (1, 1), false, 4, 4);
    let error = run_main(&w, &mb, root, &lits, Planted::Nothing).expect_err("a comparison of references with no callee is refused");
    assert!(error.contains("operator `==` on Ref("), "{error}");
    assert!(error.contains("recorded no callee"), "{error}");
}
