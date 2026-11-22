//! An operator's callee row (rules/op.rs): `Checked::callees` holds one row on every operator
//! the check resolves to a trait's method, decided on the FINAL, normalized operand types, and
//! none on a builtin operator. The build reads the row and decides nothing itself
//! (blaze-build walk/operators.rs): a reference is held as a pointer there, and a build that
//! guessed from that compared two ADDRESSES for `&u32 == &u32`.
//!
//! The Reference, "Operator expressions": [expr.cmp.place] "Unlike the arithmetic and logical
//! operators above, these operators implicitly take shared borrows of their operands,
//! evaluating them in place expression context: `a == b;` is equivalent to
//! `::std::cmp::PartialEq::eq(&a, &b);`"; [expr.arith-logic.behavior] "This table summarizes
//! the behavior of arithmetic and logical binary operators on primitive types and which traits
//! are used to overload these operators for other types."

use std::vec::Vec;

use super::{callee, check_body};
use crate::body::{BinOp, Expr, IntSuffix, UnOp};
use crate::fixture::{Bb, Fx, show};
use crate::ids::{ExprId, IntTy, ItemKey, LangItem, Mutbl, PatId, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::{PNode, PTy};

/// ```text
/// #[lang = "sized"] trait Sized {}
/// #[lang = "eq"] trait PartialEq<Rhs> { fn eq(&self, other: &Rhs) -> bool; }
/// impl PartialEq<i32> for i32 {}
/// impl<A: PartialEq<B>, B> PartialEq<&B> for &A {}
/// #[lang = "add"] trait Add<Rhs> { type Output; fn add(self, rhs: Rhs) -> Self::Output; }
/// impl Add<i32> for i32 { type Output = i32; }
/// impl Add<&i32> for i32 { type Output = i32; }
/// #[lang = "neg"] trait Neg { type Output; fn neg(self) -> Self::Output; }
/// impl Neg for &i32 { type Output = i32; }
/// fn mk<T>() -> T;
/// ```
struct Ops {
    fx: Fx,
    i32_: PTy,
    r_i32: PTy,
    bool_: PTy,
    partial_eq: ItemKey,
}

fn ops() -> Ops {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let i32_ = fx.ty(PNode::Int(IntTy::I32));
    let r_i32 = fx.r(Mutbl::Not, i32_);
    let bool_ = fx.ty(PNode::Bool);
    let (rp0, rp1) = (fx.r(Mutbl::Not, p0), fx.r(Mutbl::Not, p1));
    let partial_eq = fx.tr("PartialEq", 2, Vec::new(), &[]);
    fx.lang(LangItem::PartialEq, partial_eq);
    fx.trait_fn(partial_eq, "eq", true, &[rp0, rp1], bool_);
    fx.imp("eq for i32", 0, i32_, Some((partial_eq, &[i32_])), &[], Vec::new());
    let bound = fx.bound(partial_eq, &[p0, p1]);
    fx.imp("eq for &A", 2, rp0, Some((partial_eq, &[rp1])), &[], std::vec![bound]);
    let add = fx.tr("Add", 2, Vec::new(), &["Output"]);
    fx.lang(LangItem::Add, add);
    let out = fx.alias(add, "Output", &[p0, p1]);
    fx.trait_fn(add, "add", true, &[p0, p1], out);
    fx.imp("add for i32", 0, i32_, Some((add, &[i32_])), &[("Output", i32_)], Vec::new());
    fx.imp("add &i32 for i32", 0, i32_, Some((add, &[r_i32])), &[("Output", i32_)], Vec::new());
    let neg = fx.tr("Neg", 1, Vec::new(), &["Output"]);
    fx.lang(LangItem::Neg, neg);
    let out = fx.alias(neg, "Output", &[p0]);
    fx.trait_fn(neg, "neg", true, &[p0], out);
    fx.imp("neg for &i32", 0, r_i32, Some((neg, &[])), &[("Output", i32_)], Vec::new());
    fx.func("mk", 1, &[], p0, Vec::new());
    Ops { fx, i32_, r_i32, bool_, partial_eq }
}

/// The record of `fn <name>(a: <a>, b: <b>) -> <out> { <body(a, b)> }` and its root's tail.
fn checked(o: &mut Ops, generics: u32, clauses: Vec<crate::world::PClause>, a: PTy, b: PTy, out: PTy, body: impl Fn(&mut Bb, PatId, PatId) -> ExprId) -> (crate::Checked<'static>, ExprId) {
    let owner = o.fx.func("f", generics, &[a, b], out, clauses);
    o.fx.finish();
    let mut bb = Bb::new();
    let (pa, pb) = (bb.param("a"), bb.param("b"));
    let tail = body(&mut bb, pa, pb);
    let root = bb.block(&[], Some(tail));
    let c = check_body(&o.fx, &bb, owner, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "diagnostics: {:?}", c.diags);
    (c, tail)
}

/// `fn f(a: &i32, b: &i32) -> bool { a == b }` is `PartialEq::eq` at `[&i32, &i32]`: a row,
/// never the builtin comparison. `fn f(a: i32, b: i32) -> bool { a == b }` is the builtin
/// one: no row. `fn f<T: PartialEq<T>>(a: T, b: T) -> bool { a == b }` is the trait's method
/// at `[T, T]`, whatever `T` is at an instance (the Reference's "prior to monomorphization",
/// [expr.compound-assign.primitives], is the same rule for evaluation order).
#[test]
fn a_comparison_of_references_or_of_a_parameter_records_eq_and_one_of_scalars_nothing() {
    let eq = |bb: &mut Bb, a: PatId, b: PatId| {
        let (lhs, rhs) = (bb.local(a), bb.local(b));
        bb.e(Expr::Binary { op: BinOp::Eq, lhs, rhs })
    };
    let mut o = ops();
    let (r, bool_) = (o.r_i32, o.bool_);
    let (c, e) = checked(&mut o, 0, Vec::new(), r, r, bool_, eq);
    assert_eq!(callee(&o.fx, &c, e), "eq<&i32, &i32>");

    let mut o = ops();
    let (t, bool_) = (o.i32_, o.bool_);
    let (c, e) = checked(&mut o, 0, Vec::new(), t, t, bool_, eq);
    assert_eq!(callee(&o.fx, &c, e), "none");

    let mut o = ops();
    let p0 = o.fx.param(0);
    let bound = o.fx.bound(o.partial_eq, &[p0, p0]);
    let bool_ = o.bool_;
    let (c, e) = checked(&mut o, 1, std::vec![bound], p0, p0, bool_, eq);
    let i = c.callees.column::<0>().iter().position(|&x| x == e).expect("a row on `a == b` at a type parameter");
    assert_eq!(o.fx.name_of(c.callees.column::<1>()[i]), "eq");
    let args = c.types.view().list(c.callees.column::<2>()[i]).expect("the callee's arguments in the body's table");
    assert!(matches!(args, [x, y] if x == y && matches!(c.types.view().get(*x), Some(PNode::Param(0)))), "{args:?}");
}

/// `a + b` on `i32` and `&i32` is `Add::add` at `[i32, &i32]`; `-b` on `&i32` is `Neg::neg`
/// at `[&i32]`; `a + a` on `i32` and `-a` are builtin.
#[test]
fn arithmetic_and_negation_on_a_reference_record_their_methods() {
    let mut o = ops();
    let (t, r) = (o.i32_, o.r_i32);
    let (c, e) = checked(&mut o, 0, Vec::new(), t, r, t, |bb, a, b| {
        let (lhs, rhs) = (bb.local(a), bb.local(b));
        bb.e(Expr::Binary { op: BinOp::Add, lhs, rhs })
    });
    assert_eq!(callee(&o.fx, &c, e), "add<i32, &i32>");
    assert_eq!(show(&o.fx, c.types.view(), c.expr_ty[e.ix()]), "i32");

    let mut o = ops();
    let (c, e) = checked(&mut o, 0, Vec::new(), t, r, t, |bb, _, b| {
        let operand = bb.local(b);
        bb.e(Expr::Unary { op: UnOp::Neg, operand })
    });
    assert_eq!(callee(&o.fx, &c, e), "neg<&i32>");

    // `a + -a`.
    let mut o = ops();
    let (c, e) = checked(&mut o, 0, Vec::new(), t, r, t, |bb, a, _| {
        let (lhs, operand) = (bb.local(a), bb.local(a));
        let neg = bb.e(Expr::Unary { op: UnOp::Neg, operand });
        bb.e(Expr::Binary { op: BinOp::Add, lhs, rhs: neg })
    });
    assert_eq!(callee(&o.fx, &c, e), "none");
    assert_eq!(c.callees.len(), 0, "a builtin operator left a row");
}

/// The decision is on the FINAL types (rustc's writeback `fix_scalar_builtin_expr`):
/// ```text
/// fn f(a: i32, b: i32) -> i32 { let x = mk(); let y = x + a; let z: i32 = x; y }
/// ```
/// `x`'s type is a variable when `x + a` is checked, so the operator goes through `Add`
/// (`?x: Add<i32>`, its type the projection); the `let z: i32 = x` after it makes `x` an
/// `i32`, and `i32 + i32` is the builtin operator: no row is left, and the type is `i32`.
#[test]
fn an_operand_known_only_after_the_operator_is_builtin_at_the_end() {
    let mut o = ops();
    let t = o.i32_;
    let owner = o.fx.func("f", 0, &[t, t], t, Vec::new());
    o.fx.finish();
    let mut bb = Bb::new();
    let (pa, _pb) = (bb.param("a"), bb.param("b"));
    let (x, y, z) = (bb.bind("x"), bb.bind("y"), bb.bind("z"));
    let mk = bb.path(&["mk"]);
    let made = bb.call(mk, &[]);
    let s1 = bb.let_(x, None, made);
    let (lhs, rhs) = (bb.local(x), bb.local(pa));
    let sum = bb.e(Expr::Binary { op: BinOp::Add, lhs, rhs });
    let s2 = bb.let_(y, None, sum);
    let i32_ty = bb.ty(PNode::Int(IntTy::I32));
    let xe = bb.local(x);
    let s3 = bb.let_(z, Some(i32_ty), xe);
    let tail = bb.local(y);
    let root = bb.block(&[s1, s2, s3], Some(tail));
    let c = check_body(&o.fx, &bb, owner, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "diagnostics: {:?}", c.diags);
    assert_eq!(show(&o.fx, c.types.view(), c.expr_ty[sum.ix()]), "i32");
    assert_eq!(callee(&o.fx, &c, sum), "none", "`i32 + i32` kept the row it had while an operand was unknown");
}

/// Builtin is decided on NORMALIZED operand types: in a crate that declares no `add`, `neg`
/// or `eq` lang trait (a no_core program: only the builtin operators exist),
/// ```text
/// #[lang = "index"] trait Index<Idx> { type Output; fn index(&self, i: Idx) -> &Self::Output; }
/// struct Table { cells: [i32; 3] }
/// impl Index<usize> for Table { type Output = i32; }
/// fn f(t: Table, count: i32) -> i32 { let n = -t[1]; let q = t[1usize] == 12; count + t[1usize] }
/// ```
/// `t[1]`'s type is the projection `<Table as Index<usize>>::Output`, `i32` once normalized
/// (the Reference, "Associated Items", [items.associated.type.alias]: "If a type `Item` has an
/// associated type `Assoc` from a trait `Trait`, then `<Item as Trait>::Assoc` is a type that
/// is an alias of the type specified in the associated type definition."). So `count + t[1]`
/// is the builtin `i32 + i32`
/// ([expr.arith-logic.behavior]: "the behavior of arithmetic and logical binary operators on
/// primitive types"), with no finding that the `add` lang item is missing.
#[test]
fn an_operand_of_projection_type_is_normalized_before_builtin_is_decided() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let i32_ = fx.ty(PNode::Int(IntTy::I32));
    let usize_ = fx.usize();
    let index = fx.tr("Index", 2, Vec::new(), &["Output"]);
    fx.lang(LangItem::Index, index);
    let out = fx.alias(index, "Output", &[p0, p1]);
    let (rp0, rout) = (fx.r(Mutbl::Not, p0), fx.r(Mutbl::Not, out));
    fx.trait_fn(index, "index", true, &[rp0, p1], rout);
    let cells = fx.array(i32_, 3);
    let table = fx.strukt("Table", 0, &[("cells", cells)]);
    let table_ty = fx.adt(table, &[]);
    fx.imp("index for Table", 0, table_ty, Some((index, &[usize_])), &[("Output", i32_)], Vec::new());
    let owner = fx.func("f", 0, &[table_ty, i32_], i32_, Vec::new());
    fx.finish();

    let mut bb = Bb::new();
    let (pt, pc) = (bb.param("t"), bb.param("count"));
    let at = |bb: &mut Bb| {
        let base = bb.local(pt);
        let index = bb.int(Some(IntSuffix::Uint(UintTy::Usize)));
        bb.e(Expr::Index { base, index })
    };
    let (n, q) = (bb.bind("n"), bb.bind("q"));
    let cell = at(&mut bb);
    let neg = bb.e(Expr::Unary { op: UnOp::Neg, operand: cell });
    let s1 = bb.let_(n, None, neg);
    let (cell, twelve) = (at(&mut bb), bb.int(None));
    let same = bb.e(Expr::Binary { op: BinOp::Eq, lhs: cell, rhs: twelve });
    let s2 = bb.let_(q, None, same);
    let (count, cell) = (bb.local(pc), at(&mut bb));
    let sum = bb.e(Expr::Binary { op: BinOp::Add, lhs: count, rhs: cell });
    let root = bb.block(&[s1, s2], Some(sum));
    let c = check_body(&fx, &bb, owner, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "diagnostics: {:?}", c.diags);
    for (e, want) in [(neg, "i32"), (same, "bool"), (sum, "i32")] {
        assert_eq!(show(&fx, c.types.view(), c.expr_ty[e.ix()]), want);
        assert_eq!(callee(&fx, &c, e), "none");
    }
}
