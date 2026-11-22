//! Trait objects (typeck.md "Trait objects"): the type's one form, method calls on an object,
//! the unsized coercions to and between objects, and dyn compatibility.
//!
//! ```text
//! trait A {}   auto trait Send {}   auto trait Sync {}   trait Iterator { type Item; }
//! ```

use std::string::String;
use std::vec::Vec;

use super::{Want, adjusts, callee, check_body, differences};
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, List, Mutbl, Name, PatId, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::{PNode, PTy};

/// The objects' rows: `A`, `Send`, `Sync`, and `Iterator` with its `Item` bound to `u8` or `u16`.
struct Rows {
    a: PTy,
    send: PTy,
    sync: PTy,
    iter: PTy,
    item_u8: PTy,
    item_u16: PTy,
}

fn rows() -> (Fx, Rows) {
    let mut fx = Fx::new();
    let a = fx.tr("A", 1, Vec::new(), &[]);
    let send = fx.tr("Send", 1, Vec::new(), &[]);
    fx.auto(send);
    let sync = fx.tr("Sync", 1, Vec::new(), &[]);
    fx.auto(sync);
    let iterator = fx.tr("Iterator", 1, Vec::new(), &["Item"]);
    fx.finish();
    let (u8_, u16_) = (fx.ty(PNode::Uint(UintTy::U8)), fx.ty(PNode::Uint(UintTy::U16)));
    let item = |fx: &mut Fx, term| fx.ty(PNode::ExProj { trait_: iterator, name: Name::of("Item"), args: List::EMPTY, term });
    let r = Rows {
        a: fx.ty(PNode::ExTrait(a, List::EMPTY)),
        send: fx.ty(PNode::ExAuto(send)),
        sync: fx.ty(PNode::ExAuto(sync)),
        iter: fx.ty(PNode::ExTrait(iterator, List::EMPTY)),
        item_u8: item(&mut fx, u8_),
        item_u16: item(&mut fx, u16_),
    };
    (fx, r)
}

/// The objects of `object_order_and_repeats_make_no_new_type`, made by `make` (the constructor
/// or the planted bug's): the differences from the Reference's answer, one line each.
fn order_differences(make: fn(&mut Fx, &[PTy]) -> PTy) -> Vec<std::string::String> {
    let (mut fx, r) = rows();
    let one = make(&mut fx, &[r.a, r.send, r.sync]);
    let mut out = Vec::new();
    for (written, preds) in [
        ("dyn Sync + A + Send", std::vec![r.sync, r.a, r.send]),
        ("dyn A + Send + Send + Sync", std::vec![r.a, r.send, r.send, r.sync]),
        ("dyn Send + Sync + A", std::vec![r.send, r.sync, r.a]),
    ] {
        if make(&mut fx, &preds) != one {
            out.push(std::format!("{written} is not `dyn A + Send + Sync`"));
        }
    }
    let (it_u8, it_u8_swapped) = (make(&mut fx, &[r.iter, r.item_u8]), make(&mut fx, &[r.item_u8, r.iter]));
    if it_u8 != it_u8_swapped {
        out.push(std::string::String::from("the binding written first made another type"));
    }
    if make(&mut fx, &[r.iter, r.item_u16]) == it_u8 {
        out.push(std::string::String::from("dyn Iterator<Item = u16> is dyn Iterator<Item = u8>"));
    }
    out
}

/// The Reference, "Trait object types": "Two trait object types alias each other if the base
/// traits alias each other and if the sets of auto traits are the same and the lifetime bounds
/// are the same": `dyn A + Send + Sync`, `dyn Sync + A + Send` and `dyn A + Send + Send + Sync`
/// are one handle; `dyn Iterator<Item = u8>` and `dyn Iterator<Item = u16>` are two.
#[test]
fn object_order_and_repeats_make_no_new_type() {
    let got = order_differences(|fx, p| fx.t.dynamic(p).expect("dynamic"));
    assert!(got.is_empty(), "{got:?}");
}

/// The planted bug: the constructor keeps the rows as written.
#[test]
fn control_object_order() {
    let got = order_differences(|fx, p| fx.t.dynamic_unsorted(p).expect("dynamic"));
    std::println!("with the bug: {} failed: {got:?}", got.len());
    assert!(!got.is_empty(), "the unsorted constructor was not caught");
}

/// The calls' and coercions' world:
/// ```text
/// #[lang = "sized"] trait Sized {}   #[lang = "unsize"] trait Unsize<U: ?Sized> {}
/// #[lang = "coerce_unsized"] trait CoerceUnsized<U> {}   auto trait Send {}
/// trait Tr { fn m(&self) -> u8; fn s(&self) -> u16 where Self: Sized; }
/// trait Sub: Tr {}
/// struct Foo;  impl Tr for Foo {}  impl Sub for Foo {}
/// struct Box<T: ?Sized>;  impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<Box<U>> for Box<T> {}
/// ```
/// and one fn per case, `(name, its one parameter's type, its return type)`.
fn objects_world(fns: &[(&str, fn(&mut Fx, &Objs) -> PTy, fn(&mut Fx, &Objs) -> PTy)]) -> Fx {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let unsize = fx.tr("Unsize", 2, Vec::new(), &[]);
    fx.lang(LangItem::Unsize, unsize);
    let coerce = fx.tr("CoerceUnsized", 2, Vec::new(), &[]);
    fx.lang(LangItem::CoerceUnsized, coerce);
    let send = fx.tr("Send", 1, Vec::new(), &[]);
    fx.auto(send);
    let tr = fx.tr("Tr", 1, Vec::new(), &[]);
    let p0 = fx.param(0);
    let r_self = fx.r(Mutbl::Not, p0);
    let (u8_, u16_) = (fx.ty(PNode::Uint(UintTy::U8)), fx.ty(PNode::Uint(UintTy::U16)));
    fx.trait_fn(tr, "m", true, &[r_self], u8_);
    let self_sized = fx.bound(sized, &[p0]);
    fx.trait_fn_own(tr, "s", true, 0, &[r_self], u16_, std::vec![self_sized]);
    let sup = fx.bound(tr, &[p0]);
    let sub = fx.tr("Sub", 1, std::vec![sup], &[]);
    let foo_k = fx.strukt("Foo", 0, &[]);
    let foo = fx.adt(foo_k, &[]);
    fx.imp("tr_foo", 0, foo, Some((tr, &[])), &[], Vec::new());
    fx.imp("sub_foo", 0, foo, Some((sub, &[])), &[], Vec::new());
    let box_k = fx.strukt("Box", 1, &[]);
    let (box_t, p1) = (fx.adt(box_k, &[p0]), fx.param(1));
    let box_u = fx.adt(box_k, &[p1]);
    let t_unsize_u = fx.bound(unsize, &[p0, p1]);
    let cu = fx.imp("cu_box", 2, box_t, Some((coerce, &[box_u])), &[], std::vec![t_unsize_u]);
    fx.unsized_param(cu, 0);
    fx.unsized_param(cu, 1);
    let ex = |fx: &mut Fx, k| fx.ty(PNode::ExTrait(k, List::EMPTY));
    let (ex_tr, ex_sub, ex_send) = (ex(&mut fx, tr), ex(&mut fx, sub), fx.ty(PNode::ExAuto(send)));
    // `fn mk_opq() -> impl Tr`: the opaque with the bounds `Tr` and `Sized`, as a caller sees it.
    let opq_k = fx.opaque_key("mk_opq::{opaque#0}");
    let opq = fx.ty(PNode::Opaque(opq_k, List::EMPTY));
    let (opq_tr, opq_sized) = (fx.bound(tr, &[opq]), fx.bound(sized, &[opq]));
    fx.set_opaque_bounds(opq_k, std::vec![opq_tr, opq_sized]);
    let o = Objs {
        opq,
        foo,
        box_k,
        dyn_tr: fx.t.dynamic(&[ex_tr]).expect("dyn"),
        dyn_tr_send: fx.t.dynamic(&[ex_send, ex_tr]).expect("dyn"),
        dyn_sub: fx.t.dynamic(&[ex_sub]).expect("dyn"),
    };
    for &(name, param, ret) in fns {
        let (p, r) = (param(&mut fx, &o), ret(&mut fx, &o));
        fx.func(name, 0, &[p], r, Vec::new());
    }
    fx.finish();
    fx
}

/// The types the cases' signatures are made of.
struct Objs {
    /// A return-position `impl Tr` of another fn (bounds `Tr`, `Sized`).
    opq: PTy,
    foo: PTy,
    box_k: ItemKey,
    dyn_tr: PTy,
    dyn_tr_send: PTy,
    dyn_sub: PTy,
}

/// `x.<method>()` in `fn <owner>(x: ..)`: the expression ids are the local 0, the call 1, the
/// block 2.
fn method_on_x(fx: &Fx, owner: &str, method: &str, plant: Plant) -> (Bb, crate::columns::Checked<'static>) {
    let mut b = Bb::new();
    let x = b.param("x");
    let e0 = b.local(x);
    let e1 = b.method(e0, method, &[]);
    let root = b.block(&[], Some(e1));
    let c = check_body(fx, &b, fx.key_of(owner), root, plant, InferPlant::None);
    (b, c)
}

/// The method cases' differences from the Reference's answer (empty when all agree).
fn method_differences(plant: Plant) -> Vec<String> {
    let fx = objects_world(&[
        ("c_sub", |fx, o| fx.r(Mutbl::Not, o.dyn_sub), |fx, _| fx.u8()),
        ("c_sized", |fx, o| fx.r(Mutbl::Not, o.dyn_tr), |fx, _| fx.u16()),
    ]);
    let mut out = Vec::new();
    let (b, c) = method_on_x(&fx, "c_sub", "m", plant);
    let want = Want {
        exprs: &[(ExprId(0), "&dyn Sub"), (ExprId(1), "u8"), (ExprId(2), "u8")],
        pats: &[(PatId(0), "&dyn Sub")],
        diags: &[],
    };
    out.extend(differences(&fx, &b, &c, &want));
    let got = callee(&fx, &c, ExprId(1));
    if got != "m<dyn Sub>" {
        out.push(std::format!("x.m() on &dyn Sub: callee {got}, want m<dyn Sub> (a dynamic call)"));
    }
    let (b, c) = method_on_x(&fx, "c_sized", "s", plant);
    // `where Self: Sized` names `Self`, which input 0 (`&self`) mentions: filed at the receiver.
    let want = Want {
        exprs: &[(ExprId(0), "&dyn Tr"), (ExprId(1), "u16"), (ExprId(2), "u16")],
        pats: &[(PatId(0), "&dyn Tr")],
        diags: &[(DiagCode::Unsatisfied, ExprId(0))],
    };
    out.extend(differences(&fx, &b, &c, &want));
    out
}

/// The Reference, "Trait object types": "the trait object implements the base trait, its auto
/// traits, and any supertraits of the base trait"; "Method call expressions": the candidates
/// are the "methods provided by a visible trait implemented by T"; "Dyn compatibility": a method
/// with `where Self: Sized` is "explicitly non-dispatchable".
/// ```text
/// fn c_sub(x: &dyn Sub) -> u8 { x.m() }      // Tr's method through the supertrait, dynamic
/// fn c_sized(x: &dyn Tr) -> u16 { x.s() }    // refused: `dyn Tr: Sized` does not hold
/// ```
/// The call is recorded as `Tr::m` with `Self` the object (`m<dyn Sub>`): what makes it dynamic
/// for the code builder (typeck.md "Trait objects" (g)).
#[test]
fn methods_of_an_object() {
    let got = method_differences(Plant::None);
    assert!(got.is_empty(), "\n{}", got.join("\n"));
}

/// The planted bug: a call registers none of its method's where-clauses, so `x.s()` is accepted.
#[test]
fn control_sized_method_callable() {
    let got = method_differences(Plant::MethodClausesDropped);
    std::println!("with the bug: {} failed: {got:?}", got.len());
    assert!(!got.is_empty(), "a `where Self: Sized` method on an object was not caught");
}

/// One coercion case: `fn <name>(x: <param>) -> <ret> { x }`, the adjustments wanted on `x`
/// (empty: it does not coerce, a mismatch at the tail).
type Coercion = (&'static str, fn(&mut Fx, &Objs) -> PTy, fn(&mut Fx, &Objs) -> PTy, &'static str);

const COERCIONS: &[Coercion] = &[
    ("d_ref", |fx, o| fx.r(Mutbl::Not, o.foo), |fx, o| fx.r(Mutbl::Not, o.dyn_tr), "* -> Foo, & -> &Foo, unsize -> &dyn Tr"),
    ("d_mut", |fx, o| fx.r(Mutbl::Mut, o.foo), |fx, o| fx.r(Mutbl::Mut, o.dyn_tr), "* -> Foo, &mut -> &mut Foo, unsize -> &mut dyn Tr"),
    ("d_raw", |fx, o| fx.ty(PNode::Ptr(Mutbl::Not, o.foo)), |fx, o| fx.ty(PNode::Ptr(Mutbl::Not, o.dyn_tr)), "* -> Foo, & -> *const Foo, unsize -> *const dyn Tr"),
    ("d_box", |fx, o| fx.adt(o.box_k, &[o.foo]), |fx, o| fx.adt(o.box_k, &[o.dyn_tr]), "unsize -> Box<dyn Tr>"),
    ("d_upcast", |fx, o| fx.r(Mutbl::Not, o.dyn_sub), |fx, o| fx.r(Mutbl::Not, o.dyn_tr), "* -> dyn Sub, & -> &dyn Sub, unsize -> &dyn Tr"),
    ("d_drop_auto", |fx, o| fx.r(Mutbl::Not, o.dyn_tr_send), |fx, o| fx.r(Mutbl::Not, o.dyn_tr), "* -> dyn Tr + Send, & -> &dyn Tr + Send, unsize -> &dyn Tr"),
    ("d_add_auto", |fx, o| fx.r(Mutbl::Not, o.dyn_tr), |fx, o| fx.r(Mutbl::Not, o.dyn_tr_send), ""),
];

fn coercion_differences(plant: Plant) -> Vec<String> {
    let sigs: Vec<_> = COERCIONS.iter().map(|&(n, p, r, _)| (n, p, r)).collect();
    let fx = objects_world(&sigs);
    let mut out = Vec::new();
    for &(name, _, _, want) in COERCIONS {
        let mut b = Bb::new();
        let x = b.param("x");
        let e0 = b.local(x);
        let root = b.block(&[], Some(e0));
        let c = check_body(&fx, &b, fx.key_of(name), root, plant, InferPlant::None);
        let got = adjusts(&fx, &c, ExprId(0));
        let refused = c.diags.iter().any(|d| d.code == DiagCode::Mismatch);
        if got != want || refused != want.is_empty() {
            out.push(std::format!("{name}: adjustments `{got}` (mismatch: {refused}), want `{want}`"));
        }
    }
    out
}

/// The Reference, "Coercion types": "TyCtor(`T`) to TyCtor(`U`), where TyCtor(`T`) is one of
/// `&T`, `&mut T`, `*const T`, `*mut T`, `Box<T>`, and where `U` can be obtained from `T` by
/// unsized coercion"; "Unsized Coercions": "`T` to `dyn U`, when `T` implements `U + Sized`, and
/// `U` is dyn compatible", "`dyn T` to `dyn U`, when `U` is one of `T`'s supertraits". An auto
/// trait of the source may be dropped, never added (`&dyn Tr` to `&(dyn Tr + Send)` fails).
#[test]
fn unsized_coercions_to_and_between_objects() {
    let got = coercion_differences(Plant::None);
    assert!(got.is_empty(), "\n{}", got.join("\n"));
}

/// The Reference, "Unsized Coercions": "`T` to `dyn U`, when `T` implements `U + Sized`, and `U`
/// is dyn compatible"; "Impl trait", "Abstract return types": "the caller can only use the
/// methods declared by the trait bound": so a caller knows `impl Tr: Tr` (and `Sized`) from the
/// opaque's bounds, and `fn d_opq(x: &mut impl-Tr-of-mk_opq) -> &mut dyn Tr { x }` coerces:
/// no E0308, the last adjustment an unsize to `&mut dyn Tr`. The reduction of std's
/// `default_hook` (`if let Some(mut out) = panic_output() { write(&mut out) }`, `panic_output()
/// -> Option<impl io::Write>`, `write: |&mut dyn io::Write|`), which the real front end refused
/// as "E0308 .. found &mut Opaque(..), expected &mut Dynamic(..)" (ladder_front2, 14:52).
/// Should print: "test suite::dyn_objects::a_reference_to_an_opaque_unsizes_to_its_bounds_object ... ok".
#[test]
fn a_reference_to_an_opaque_unsizes_to_its_bounds_object() {
    let fx = objects_world(&[("d_opq", |fx, o| fx.r(Mutbl::Mut, o.opq), |fx, o| fx.r(Mutbl::Mut, o.dyn_tr))]);
    let mut b = Bb::new();
    let x = b.param("x");
    let e0 = b.local(x);
    let root = b.block(&[], Some(e0));
    let c = check_body(&fx, &b, fx.key_of("d_opq"), root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    let got = adjusts(&fx, &c, ExprId(0));
    assert!(got.ends_with("unsize -> &mut dyn Tr"), "adjustments `{got}`");
}

/// The same two sentences, at the site std's `default_hook` has: a BORROW EXPRESSION as a call's
/// argument (`write(&mut out)`), where the borrow's operand is typed with the object type as
/// its hint: `fn take(x: &mut dyn Tr)`, `fn d_arg(x: impl-Tr-of-mk_opq) { let out = x;
/// take(&mut out) }`: no finding, and the argument's last adjustment is the unsize.
/// Should print: "test suite::dyn_objects::a_borrow_of_an_opaque_local_unsizes_at_a_call ... ok".
#[test]
fn a_borrow_of_an_opaque_local_unsizes_at_a_call() {
    let fx = objects_world(&[
        ("take", |fx, o| fx.r(Mutbl::Mut, o.dyn_tr), |fx, _| fx.unit()),
        ("d_arg", |_, o| o.opq, |fx, _| fx.unit()),
    ]);
    let mut b = Bb::new();
    let x = b.param("x");
    let init = b.local(x);
    let out = b.bind("out");
    let s = b.let_(out, None, init);
    let take = b.path(&["take"]);
    let operand = b.local(out);
    let arg = b.borrow(Mutbl::Mut, operand);
    let call = b.call(take, &[arg]);
    let root = b.block(&[s], Some(call));
    let c = check_body(&fx, &b, fx.key_of("d_arg"), root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    let got = adjusts(&fx, &c, arg);
    assert!(got.ends_with("unsize -> &mut dyn Tr"), "adjustments `{got}`");
}

/// The planted bug: the coercion does not ask the solver, so it invents `+ Send`.
#[test]
fn control_coercion_invents_auto() {
    let got = coercion_differences(Plant::UnsizeUnasked);
    std::println!("with the bug: {} failed: {got:?}", got.len());
    assert!(got.iter().any(|d| d.starts_with("d_add_auto")), "the invented auto trait was not caught: {got:?}");
}

/// The Reference, "Trait object types": "There may not be more than one non-auto trait"; and
/// one associated type bound twice is E0719: both are refused, never one row kept.
#[test]
fn two_principals_or_two_bindings_are_refused() {
    let (mut fx, r) = rows();
    assert!(fx.t.dynamic(&[r.a, r.iter]).is_err(), "dyn A + Iterator");
    assert!(fx.t.dynamic(&[r.iter, r.item_u8, r.item_u16]).is_err(), "Item = u8 and Item = u16");
}

/// `fn f(x: &dyn Bad, y: &dyn Bad, z: &dyn Tr, w: &dyn Ok) {}` over `trait Bad { fn new() ->
/// u8; }`, `trait Tr { const N: u8; }`, `trait Ok { fn m(&self) -> u8; }`: the codes, names
/// and rules of the body's diagnostics.
fn dyn_incompatible_diags() -> Vec<(&'static str, Option<Name>, Option<crate::dyn_compat::DynRule>)> {
    use crate::dyn_compat::{MemberFacts, MemberShape};
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let bad = fx.tr("Bad", 1, Vec::new(), &[]);
    let u8_ = fx.u8();
    fx.trait_fn(bad, "new", false, &[], u8_);
    let tr = fx.tr("Tr", 1, Vec::new(), &[]);
    fx.trait_member(tr, MemberFacts { name: Name::of("N"), shape: MemberShape::Const });
    let ok = fx.tr("Ok", 1, Vec::new(), &[]);
    let p0 = fx.param(0);
    let r_self = fx.r(Mutbl::Not, p0);
    fx.trait_fn(ok, "m", true, &[r_self], u8_);
    let obj = |fx: &mut Fx, k: ItemKey| {
        let ex = fx.ty(PNode::ExTrait(k, List::EMPTY));
        let d = fx.t.dynamic(&[ex]).expect("dyn");
        fx.r(Mutbl::Not, d)
    };
    let (d_bad, d_tr, d_ok) = (obj(&mut fx, bad), obj(&mut fx, tr), obj(&mut fx, ok));
    let unit = fx.unit();
    fx.func("f", 0, &[d_bad, d_bad, d_tr, d_ok], unit, Vec::new());
    fx.finish();
    let mut b = Bb::new();
    for p in ["x", "y", "z", "w"] {
        let _ = b.param(p);
    }
    let root = b.block(&[], None);
    let c = check_body(&fx, &b, fx.key_of("f"), root, Plant::None, InferPlant::None);
    let rule = |d: &crate::diag::Diag| match d.because {
        Some(crate::diag::Because::Dyn { rule, .. }) => Some(rule),
        _ => None,
    };
    c.diags.iter().map(|d| (d.code.code(), d.name, rule(d))).collect()
}

/// What the Reference wants of [`dyn_incompatible_diags`]'s body.
fn dyn_incompatible_want() -> Vec<(&'static str, Option<Name>, Option<crate::dyn_compat::DynRule>)> {
    use crate::dyn_compat::DynRule;
    std::vec![("E0038", Some(Name::of("new")), Some(DynRule::NoReceiver)), ("E0038", Some(Name::of("N")), Some(DynRule::AssocConst))]
}

/// The Reference, "Traits", "Dyn compatibility": "Dispatchable functions must: [..] Be a
/// method" (an associated function without `self` and without `where Self: Sized` is neither
/// dispatchable nor explicitly non-dispatchable), and "It must not have any associated
/// constants". E0038 is reported where the trait is used in `dyn`, once per trait in a body
/// however often it is written.
/// ```text
/// trait Bad { fn new() -> u8; }   trait Tr { const N: u8; }   trait Ok { fn m(&self) -> u8; }
/// fn f(x: &dyn Bad, y: &dyn Bad,   // E0038 once, naming `new`
///      z: &dyn Tr,                 // E0038 naming `N`
///      w: &dyn Ok) {}              // accepted
/// ```
#[test]
fn dyn_incompatible_trait_is_e0038_once() {
    assert_eq!(dyn_incompatible_diags(), dyn_incompatible_want());
}

/// Control: with every member taken as compatible (`PLANT_ALL_COMPATIBLE`, the state before
/// the rule was wired) the case above must miss.
#[test]
fn control_dyn_incompatible_unreported() {
    crate::dyn_compat::PLANT_ALL_COMPATIBLE.with(|p| p.set(true));
    let got = dyn_incompatible_diags();
    crate::dyn_compat::PLANT_ALL_COMPATIBLE.with(|p| p.set(false));
    assert!(got.is_empty() && got != dyn_incompatible_want(), "the planted bug was not caught: {got:?}");
}

/// The Reference, "Trait objects" ("Trait objects are written as ... the trait must be dyn
/// compatible") holds wherever the type is written, not only in a body: a struct's field and a
/// bodiless fn's signature (a trait method declaration) are checked per item by
/// `dyn_compat::item_dyn_findings`, once per trait per item, with an empty range (the finding
/// covers the item).
/// ```text
/// struct S<'a> { a: &'a dyn Bad, b: &'a dyn Bad, c: &'a dyn Tr, d: &'a dyn Ok }
/// trait Uses { fn take(x: &dyn Bad, y: &dyn Tr, z: &dyn Ok); }   // no body
/// ```
#[test]
fn dyn_in_a_field_or_a_bodiless_signature_is_e0038() {
    use crate::dyn_compat::{DynRule, MemberFacts, MemberShape, item_dyn_findings};
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let bad = fx.tr("Bad", 1, Vec::new(), &[]);
    let u8_ = fx.u8();
    fx.trait_fn(bad, "new", false, &[], u8_);
    let tr = fx.tr("Tr", 1, Vec::new(), &[]);
    fx.trait_member(tr, MemberFacts { name: Name::of("N"), shape: MemberShape::Const });
    let ok = fx.tr("Ok", 1, Vec::new(), &[]);
    let p0 = fx.param(0);
    let r_self = fx.r(Mutbl::Not, p0);
    fx.trait_fn(ok, "m", true, &[r_self], u8_);
    let obj = |fx: &mut Fx, k: ItemKey| {
        let ex = fx.ty(PNode::ExTrait(k, List::EMPTY));
        let d = fx.t.dynamic(&[ex]).expect("dyn");
        fx.r(Mutbl::Not, d)
    };
    let (d_bad, d_tr, d_ok) = (obj(&mut fx, bad), obj(&mut fx, tr), obj(&mut fx, ok));
    let s = fx.strukt("S", 0, &[("a", d_bad), ("b", d_bad), ("c", d_tr), ("d", d_ok)]);
    let uses = fx.tr("Uses", 1, Vec::new(), &[]);
    let unit = fx.unit();
    let take = fx.trait_fn(uses, "take", false, &[d_bad, d_tr, d_ok], unit);
    fx.finish();
    let want = std::vec![
        (DiagCode::DynIncompatible, 0, 0, Some(Name::of("new")), Some((bad, DynRule::NoReceiver))),
        (DiagCode::DynIncompatible, 0, 0, Some(Name::of("N")), Some((tr, DynRule::AssocConst))),
    ];
    for item in [s, take] {
        let mut out = blaze_own::Vec::new_in(blaze_own::Alloc::global());
        item_dyn_findings(&fx, item, &mut out).expect("the fixture answers");
        let got: Vec<_> = out
            .iter()
            .map(|d| {
                let because = match d.because {
                    Some(crate::diag::Because::Dyn { trait_, rule, .. }) => Some((trait_, rule)),
                    _ => None,
                };
                (d.code, d.at.start, d.at.end, d.name, because)
            })
            .collect();
        assert_eq!(got, want, "{}", fx.name_of(item));
    }
}
