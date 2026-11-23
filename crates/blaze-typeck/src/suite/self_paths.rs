//! Type-relative paths through `Self`, as the front end answers them: `Self` in an impl of an
//! ADT resolves to `Def::Adt` consuming one segment (blaze-ladder front2/world_paths.rs), and
//! the next segment is the checker's to find. tempo's "611 diag E0599" (alloy's `sol!` writes
//! `Self::Variant(..)` and `Self::abi_decode_raw_with_config(..)` in `impl SolInterface for
//! XCalls`), reduced; the real-front-end form is blaze-ladder tests/front2_selfpaths.rs.
//!
//! ```text
//! enum E { A(u8), B }
//! impl E { fn b2() -> E { Self::A(3) } }            // the variant, not an associated item
//! trait Tb { fn g() -> u8; }                         // NOT imported where the impl is
//! impl Tb for E { fn g() -> u8 { 1 } fn b4() -> u8 { Self::g() } }
//! ```
//!
//! rustc's `associated_path_to_ty` looks for a variant of the enum before any associated item;
//! rustc_resolve counts the trait of the impl a body is in among the traits in scope
//! (`traits_in_scope` adds `current_trait_ref`), so `Self::g()` needs no `use`.

use std::string::String;
use std::vec::Vec;

use super::{Want, check_body, differences};
use crate::body::IntSuffix;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::world::Def;

fn world() -> (Fx, ItemKey, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let u8_ = fx.u8();
    let (e, _) = fx.enum_("E", 0, &[("A", &[u8_]), ("B", &[])]);
    let e_t = fx.adt(e, &[]);
    fx.use_path("Self", Def::Adt(e));
    let inherent = fx.imp("impl E", 0, e_t, None, &[], Vec::new());
    let b2 = fx.impl_fn(inherent, "b2", false, &[], e_t);
    let tb = fx.tr("Tb", 1, Vec::new(), &[]);
    fx.trait_fn(tb, "g", false, &[], u8_);
    let tb_e = fx.imp("impl Tb for E", 0, e_t, Some((tb, &[])), &[], Vec::new());
    fx.impl_fn(tb_e, "g", false, &[], u8_);
    let b4 = fx.impl_fn(tb_e, "b4", false, &[], u8_);
    fx.finish();
    (fx, b2, b4)
}

/// Every difference but the expressions this test writes no expectation for.
fn written(lines: Vec<String>) -> Vec<String> {
    lines.into_iter().filter(|l| !l.contains("no expectation written")).collect()
}

/// `Self::A(3)` is the variant's ctor: without the variant step it is E0599 (`A` is no
/// associated item of `E`).
#[test]
fn a_variant_through_self_is_the_variants_ctor() {
    let (fx, b2, _) = world();
    let mut b = Bb::new();
    let ctor = b.path(&["Self", "A"]);
    let three = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let call = b.call(ctor, &[three]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, b2, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[(ExprId(2), "E"), (ExprId(3), "E")], pats: &[], diags: &[] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// `Self::g()` in `impl Tb for E` with `Tb` not in scope: the impl's own trait is a candidate.
/// Without it: E0599.
#[test]
fn self_reaches_the_impls_own_trait_without_an_import() {
    let (fx, _, b4) = world();
    let mut b = Bb::new();
    let g = b.path(&["Self", "g"]);
    let call = b.call(g, &[]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, b4, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[(ExprId(1), "u8"), (ExprId(2), "u8")], pats: &[], diags: &[] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// The world of `Q::NAME` on a type parameter (the real-front-end form is blaze-ladder
/// tests/front2_params.rs):
///
/// ```text
/// trait A1 { const N: u8; }   trait A2 { const N: u8; }
/// trait Zst { const Z: bool; }   impl<T> Zst for T {}      // core's SizedTypeProperties
/// trait Zb { const Z: bool; }
/// fn one<Q: A1>() -> u8 { Q::N }          // A2 is in scope and does not hold: A1
/// fn both<Q: A1 + A2>() -> u8 { Q::N }    // E0034
/// fn first<Q: Zb>() -> bool { Q::Z }      // the bound's, before the blanket impl's
/// fn blanket<Q>() -> bool { Q::Z }        // the blanket impl's (`T::IS_ZST`)
/// ```
///
/// The Reference, "Method call expressions": "If `T` is a type parameter, methods provided by
/// trait bounds on `T` are looked up first. Then all remaining methods in scope are looked
/// up." and "If this results in multiple possible candidates, then it is an error". For a
/// path the Reference is silent; rustc keeps the same order (rustc_hir_typeck `probe_op` in
/// `Mode::Path`: `assemble_inherent_candidates_from_param` first).
fn param_world() -> (Fx, [ItemKey; 4]) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let u8_ = fx.u8();
    let bool_ = fx.ty(crate::portable::PNode::Bool);
    let q = fx.param(0);
    fx.use_path("Q", Def::TyParam(0));
    let a1 = fx.tr("A1", 1, Vec::new(), &[]);
    fx.trait_const(a1, "N", u8_);
    let a2 = fx.tr("A2", 1, Vec::new(), &[]);
    fx.trait_const(a2, "N", u8_);
    let zst = fx.tr("Zst", 1, Vec::new(), &[]);
    fx.trait_const(zst, "Z", bool_);
    fx.imp("impl<T> Zst for T", 1, q, Some((zst, &[])), &[], Vec::new());
    let zb = fx.tr("Zb", 1, Vec::new(), &[]);
    fx.trait_const(zb, "Z", bool_);
    // All four are in scope, as items of the module the fns are written in.
    for t in [a1, a2, zst, zb] {
        fx.use_trait(t);
    }
    let c_a1 = fx.bound(a1, &[q]);
    let one = fx.func("one", 1, &[], u8_, std::vec![c_a1]);
    let (c_a1, c_a2) = (fx.bound(a1, &[q]), fx.bound(a2, &[q]));
    let both = fx.func("both", 1, &[], u8_, std::vec![c_a1, c_a2]);
    let c_zb = fx.bound(zb, &[q]);
    let first = fx.func("first", 1, &[], bool_, std::vec![c_zb]);
    let blanket = fx.func("blanket", 1, &[], bool_, Vec::new());
    fx.finish();
    (fx, [one, both, first, blanket])
}

/// The body `{ Q::name }` of `owner`, checked: its differences from `want`.
fn param_path(fx: &Fx, owner: ItemKey, name: &str, want: &Want<'_>) -> Vec<String> {
    let mut b = Bb::new();
    let p = b.path(&["Q", name]);
    let root = b.block(&[], Some(p));
    let c = check_body(fx, &b, owner, root, Plant::None, InferPlant::None);
    written(differences(fx, &b, &c, want))
}

/// A constant through a bound where another trait in scope has the name and does not hold.
#[test]
fn a_parameters_constant_comes_from_its_bound() {
    let (fx, [one, _, _, _]) = param_world();
    let d = param_path(&fx, one, "N", &Want { exprs: &[(ExprId(0), "u8")], pats: &[], diags: &[] });
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// A constant through a blanket impl of a trait in scope where no bound has the name (core's
/// `T::IS_ZST`): the solver holds `Q: Zst` by the blanket impl.
#[test]
fn a_parameters_constant_comes_from_a_blanket_impl() {
    let (fx, [_, _, _, blanket]) = param_world();
    let d = param_path(&fx, blanket, "Z", &Want { exprs: &[(ExprId(0), "bool")], pats: &[], diags: &[] });
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// `Q: Zb` with the blanket `Zst` in scope, both with `Z`: the bound's is picked. Without the
/// two tiers both are candidates and the path is E0034, which rustc does not give.
#[test]
fn a_parameters_bound_comes_before_the_traits_in_scope() {
    let (fx, [_, _, first, _]) = param_world();
    let d = param_path(&fx, first, "Z", &Want { exprs: &[(ExprId(0), "bool")], pats: &[], diags: &[] });
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// Two bounds with the name: E0034.
#[test]
fn two_bounds_with_one_name_are_ambiguous() {
    let (fx, [_, both, _, _]) = param_world();
    let d = param_path(&fx, both, "N", &Want { exprs: &[], pats: &[], diags: &[(DiagCode::AmbiguousMethod, ExprId(0))] });
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// `Self` as a VALUE in an impl: tempo's "347 diag E0423" (alloy's `sol!` writes
/// `fn from(tuple: UnderlyingRustTuple<'_>) -> Self { Self }` for an error with no fields,
/// `Self(tuple.0, ..)` for a tuple-shaped one). The Reference, "Paths", "`Self`": "In an
/// implementation, it refers to the type being implemented. When implementing a tuple or unit
/// struct, it also refers to the constructor in the value namespace." A braced struct has no
/// such constructor (E0423 kept), and a `type` alias gets no such rule (the Reference, "Type
/// aliases": "A type alias to a tuple-struct or unit-struct cannot be used to qualify that
/// type's constructor"). The world answers `Self` with `Def::Adt` consuming one segment, as
/// blaze-ladder front2/world_paths.rs does; the real-front-end form is blaze-ladder
/// tests/front2_selfpaths.rs (`d1`).
///
/// ```text
/// struct U;          impl U  { fn mk() -> Self { Self } }
/// struct T2(u8, u8); impl T2 { fn mk() -> Self { Self(1u8, 2u8) } fn f() -> fn(u8, u8) -> T2 { Self } }
/// struct W<T>(T);    impl<T> W<T> { fn new(t: T) -> Self { Self(t) } fn g() -> W<u8> { Self(1u8) } }
/// struct B { a: u8 } impl B  { fn bad() -> Self { Self } }      // E0423
/// ```
///
/// Each test fails without `Machine::self_ctor_value` (rules/lit_path.rs): `Self` is then
/// E0423 at expression 0; `g` fails if the ctor took fresh arguments instead of the impl's.
/// The world of one struct `name` and an inherent impl of it: `fields` gives a unit or tuple
/// struct's field types in the world's table (`None`: braced, `{ a: u8 }`).
fn self_ctor_world(name: &str, generics: u32, fields: Option<fn(&mut Fx) -> Vec<crate::portable::PTy>>) -> (Fx, ItemKey, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let u8_ = fx.u8();
    let k = match fields {
        Some(make) => {
            let tys = make(&mut fx);
            fx.tuple_strukt(name, generics, &tys)
        }
        None => fx.strukt(name, generics, &[("a", u8_)]),
    };
    let params: Vec<crate::portable::PTy> = (0..generics).map(|i| fx.param(i)).collect();
    let self_t = fx.adt(k, &params);
    fx.use_path("Self", Def::Adt(k));
    let imp = fx.imp("impl", generics, self_t, None, &[], Vec::new());
    (fx, k, imp)
}

/// `struct U; impl U { fn mk() -> Self { Self } }`: `Self` is the unit value `U`.
#[test]
fn self_is_a_unit_structs_value() {
    let (mut fx, k, imp) = self_ctor_world("U", 0, Some(|_| Vec::new()));
    let u_t = fx.adt(k, &[]);
    let mk = fx.impl_fn(imp, "mk", false, &[], u_t);
    fx.finish();
    let mut b = Bb::new();
    let s = b.path(&["Self"]);
    let root = b.block(&[], Some(s));
    let c = check_body(&fx, &b, mk, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[(ExprId(0), "U"), (ExprId(1), "U")], pats: &[], diags: &[] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// `struct T2(u8, u8)`: `Self(1u8, 2u8)` calls the ctor; bare `Self` is the ctor fn, which
/// coerces to `fn(u8, u8) -> T2`.
#[test]
fn self_is_a_tuple_structs_constructor() {
    let (mut fx, k, imp) = self_ctor_world("T2", 0, Some(|fx| {
        let u8_ = fx.u8();
        std::vec![u8_, u8_]
    }));
    let u8_ = fx.u8();
    let t_t = fx.adt(k, &[]);
    let mk = fx.impl_fn(imp, "mk", false, &[], t_t);
    let io = fx.list(&[u8_, u8_, t_t]);
    let ptr = fx.ty(crate::portable::PNode::FnPtr(io, crate::ids::FnPtrHeader::RUST));
    let f = fx.impl_fn(imp, "f", false, &[], ptr);
    fx.finish();

    let mut b = Bb::new();
    let s = b.path(&["Self"]);
    let one = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let two = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let call = b.call(s, &[one, two]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, mk, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[(ExprId(0), "fn T2"), (ExprId(1), "u8"), (ExprId(2), "u8"), (ExprId(3), "T2"), (ExprId(4), "T2")], pats: &[], diags: &[] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "mk:\n{}", d.join("\n"));

    let mut b = Bb::new();
    let s = b.path(&["Self"]);
    let root = b.block(&[], Some(s));
    let c = check_body(&fx, &b, f, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[(ExprId(0), "fn T2")], pats: &[], diags: &[] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "f:\n{}", d.join("\n"));
}

/// `impl<T> W<T> { fn new(t: T) -> Self { Self(t) } }` is clean; `fn g() -> W<u8> { Self(1u8) }`
/// is E0308: `Self` is `W<T>`'s ctor at the impl's `T`, not `W` at fresh arguments.
#[test]
fn self_constructor_takes_the_impls_arguments() {
    let (mut fx, k, imp) = self_ctor_world("W", 1, Some(|fx| std::vec![fx.param(0)]));
    let p0 = fx.param(0);
    let u8_ = fx.u8();
    let w_t = fx.adt(k, &[p0]);
    let w_u8 = fx.adt(k, &[u8_]);
    let new = fx.impl_fn(imp, "new", false, &[p0], w_t);
    let g = fx.impl_fn(imp, "g", false, &[], w_u8);
    fx.finish();

    let mut b = Bb::new();
    let t = b.param("t");
    let s = b.path(&["Self"]);
    let arg = b.local(t);
    let call = b.call(s, &[arg]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, new, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[(ExprId(1), "P0"), (ExprId(2), "W<P0>"), (ExprId(3), "W<P0>")], pats: &[], diags: &[] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "new:\n{}", d.join("\n"));

    let mut b = Bb::new();
    let s = b.path(&["Self"]);
    let one = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let call = b.call(s, &[one]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, g, root, Plant::None, InferPlant::None);
    assert!(c.diags.iter().any(|d| d.code == DiagCode::Mismatch), "g: want E0308, got {:?}", c.diags.iter().map(|d| d.code).collect::<Vec<_>>());
}

/// `struct B { a: u8 } impl B { fn bad() -> Self { Self } }`: E0423 (a braced struct has no
/// ctor in the value namespace), at `Self`.
#[test]
fn self_of_a_braced_struct_is_not_a_value() {
    let (mut fx, k, imp) = self_ctor_world("B", 0, None);
    let b_t = fx.adt(k, &[]);
    let bad = fx.impl_fn(imp, "bad", false, &[], b_t);
    fx.finish();
    let mut b = Bb::new();
    let s = b.path(&["Self"]);
    let root = b.block(&[], Some(s));
    let c = check_body(&fx, &b, bad, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[], pats: &[], diags: &[(DiagCode::NotAValue, ExprId(0))] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// A variant named through `Self` or a type alias in a struct expression or pattern: tempo's
/// "56 diag E0574" (reth_state_root_antithesis model.rs `Self::Account { address, account }`,
/// precompiles test_util.rs `Self::Simple { admin, .. }`, zone_checker l2/events.rs
/// `Self::TokenBurn { .. }`), reduced. The Reference, "Struct expressions": "A struct
/// expression creates a struct, enum, or union value. It consists of a path to a struct, enum
/// variant, or union item followed by the values for the fields of the item", with a
/// type-relative variant among its own examples (`<Enum as Tr>::T::Tuple(); // OK`); "The
/// field names can be decimal integer values to specify indices for constructing tuple
/// structs", which these fixtures use (`Self::A { 0: x }`: the fixture's variants are
/// tuple-shaped; the path step under test does not look at the variant's shape). "Paths",
/// "`Self`": "In an implementation, it refers to the type being implemented."
///
/// ```text
/// enum E { A(u8), B }
/// impl E {
///     fn f(e: E) -> u8 { match e { Self::A { 0: x } => x, Self::B { .. } => 0u8 } }
///     fn g() -> E { Self::A { 0: 1u8 } }
///     fn bad() -> E { Self::C { 0: 1u8 } }              // E0574: `C` is no variant of `E`
/// }
/// type Al = E;  fn h() -> E { Al::A { 0: 1u8 } }
/// enum G<T> { A(T) }
/// impl<T> G<T> { fn k() -> G<u8> { Self::A { 0: 1u8 } } }  // E0308: `Self` is `G<T>`
/// ```
///
/// The world answers `Self` and `Al` with `Def::Adt` consuming one segment, as blaze-ladder
/// front2/world_paths.rs does. Without the variant step in `Machine::struct_path`
/// (rules/adt.rs) `f`, `g` and `h` are E0574; `bad` must stay E0574 (no variant named `C`);
/// `k` must stay E0308 (a fix giving `Self` fresh arguments accepts it). Prints
/// "self_struct_variant: clean".
///
/// The fixture's body builder has no struct expressions or patterns: these bodies take their
/// paths, field lists and struct patterns from [`StructBody`], set on the view.
struct StructBody {
    paths: Vec<crate::body::BodyPath>,
    segments: Vec<crate::body::Segment>,
}

impl StructBody {
    fn new() -> StructBody {
        StructBody { paths: Vec::new(), segments: Vec::new() }
    }

    /// A path `segments[0]::segments[1]..` with no generic arguments.
    fn path(&mut self, segments: &[&str]) -> crate::ids::PathIdx {
        let start = self.segments.len() as u32;
        for s in segments {
            self.segments.push(crate::body::Segment { name: crate::ids::Name::of(s), generic_args: crate::ids::List::EMPTY, has_args: false });
        }
        let at = self.paths.len() as u32;
        let segs = crate::ids::List { start, len: segments.len() as u32 };
        self.paths.push(crate::body::BodyPath { scope: crate::ids::ScopeId(0), segments: segs, lang: None, anchor: None });
        crate::ids::PathIdx(at)
    }
}

/// [`check_body`] over a view the caller completed.
fn check_view(fx: &Fx, v: &crate::body::BodyView<'_>) -> crate::columns::Checked<'static> {
    let mut cx = crate::infer::InferCtx::new_in(blaze_own::Alloc::global());
    let hints = crate::world::Needs::new_in(blaze_own::Alloc::global());
    match crate::check_with(v, fx, &mut cx, &hints, Plant::None, blaze_own::Alloc::global()) {
        Ok(crate::Outcome::Done(c)) => c,
        Ok(crate::Outcome::Need(n)) => panic!("`{}`: the world missed {:?}", fx.name_of(v.owner), n.questions),
        Err(e) => panic!("`{}`: {e:?}", fx.name_of(v.owner)),
    }
}

/// `Self::A { 0: <one> }` (or `<prefix>::<variant>`) as the body's struct expression, its
/// field list the view's `field_inits[0..1]`.
fn struct_expr(b: &mut Bb, sp: &mut StructBody, path: &[&str], one: ExprId) -> (ExprId, [crate::body::FieldInit; 1]) {
    let p = sp.path(path);
    let fields = crate::ids::List { start: 0, len: 1 };
    let s = b.e(crate::body::Expr::Struct { path: p, fields, base: crate::body::StructBase::None });
    (s, [crate::body::FieldInit { name: crate::ids::Name::of_index(0), expr: one }])
}

#[test]
fn self_struct_variant() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let u8_ = fx.u8();
    let (e, _) = fx.enum_("E", 0, &[("A", &[u8_]), ("B", &[])]);
    let e_t = fx.adt(e, &[]);
    fx.use_path("Self", Def::Adt(e));
    fx.use_path("Al", Def::Adt(e));
    let inherent = fx.imp("impl E", 0, e_t, None, &[], Vec::new());
    let f = fx.impl_fn(inherent, "f", false, &[e_t], u8_);
    let g = fx.impl_fn(inherent, "g", false, &[], e_t);
    let bad = fx.impl_fn(inherent, "bad", false, &[], e_t);
    let h = fx.func("h", 0, &[], e_t, Vec::new());
    fx.finish();

    // `fn g() -> E { Self::A { 0: 1u8 } }` and `fn h() -> E { Al::A { 0: 1u8 } }`: clean.
    for (owner, prefix) in [(g, "Self"), (h, "Al")] {
        let mut sp = StructBody::new();
        let mut b = Bb::new();
        let one = b.int(Some(IntSuffix::Uint(UintTy::U8)));
        let (s, inits) = struct_expr(&mut b, &mut sp, &[prefix, "A"], one);
        let root = b.block(&[], Some(s));
        let mut v = b.view(owner, root);
        v.paths = &sp.paths;
        v.segments = &sp.segments;
        v.field_inits = &inits;
        let c = check_view(&fx, &v);
        let want = Want { exprs: &[(ExprId(0), "u8"), (ExprId(1), "E"), (ExprId(2), "E")], pats: &[], diags: &[] };
        let d = differences(&fx, &b, &c, &want);
        assert!(d.is_empty(), "{prefix}::A {{ 0: 1u8 }}:\n{}", d.join("\n"));
    }

    // `fn f(e: E) -> u8 { match e { Self::A { 0: x } => x, Self::B { .. } => 0u8 } }`: clean.
    // Patterns 2 and 3 are placeholders here, replaced by the struct patterns on the view.
    let mut sp = StructBody::new();
    let mut b = Bb::new();
    let e_p = b.param("e");
    let x = b.bind("x");
    let arm_a = b.bind("arm_a");
    let arm_b = b.bind("arm_b");
    let scrutinee = b.local(e_p);
    let x_use = b.local(x);
    let zero = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let m = b.match_(scrutinee, &[(arm_a, x_use), (arm_b, zero)]);
    let root = b.block(&[], Some(m));
    let path_a = sp.path(&["Self", "A"]);
    let path_b = sp.path(&["Self", "B"]);
    let bind = |name: &str| crate::body::Pat::Bind {
        ann: crate::body::BindAnn { by_ref: None, mutable: false },
        name: crate::ids::Name::of(name),
        sub: None,
        ty: None,
    };
    let pats = [
        bind("e"),
        bind("x"),
        crate::body::Pat::Struct { path: path_a, fields: crate::ids::List { start: 0, len: 1 }, rest: false },
        crate::body::Pat::Struct { path: path_b, fields: crate::ids::List { start: 1, len: 0 }, rest: true },
    ];
    assert_eq!((arm_a.0, arm_b.0), (2, 3), "the placeholders are patterns 2 and 3");
    let field_pats = [crate::body::FieldPat { name: crate::ids::Name::of_index(0), pat: x }];
    let mut v = b.view(f, root);
    v.pats = &pats;
    v.paths = &sp.paths;
    v.segments = &sp.segments;
    v.field_pats = &field_pats;
    let c = check_view(&fx, &v);
    let want = Want {
        exprs: &[(ExprId(0), "E"), (ExprId(1), "u8"), (ExprId(2), "u8"), (ExprId(3), "u8"), (ExprId(4), "u8")],
        pats: &[(e_p, "E"), (x, "u8")],
        diags: &[],
    };
    let d = differences(&fx, &b, &c, &want);
    assert!(d.is_empty(), "match e {{ Self::A {{ 0: x }} => x, Self::B {{ .. }} => 0u8 }}:\n{}", d.join("\n"));

    // `fn bad() -> E { Self::C { 0: 1u8 } }`: E0574 at the struct expression, naming `C`.
    let mut sp = StructBody::new();
    let mut b = Bb::new();
    let one = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let (s, inits) = struct_expr(&mut b, &mut sp, &["Self", "C"], one);
    let root = b.block(&[], Some(s));
    let mut v = b.view(bad, root);
    v.paths = &sp.paths;
    v.segments = &sp.segments;
    v.field_inits = &inits;
    let c = check_view(&fx, &v);
    let want = Want { exprs: &[], pats: &[], diags: &[(DiagCode::NotAStruct, s)] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "Self::C {{ 0: 1u8 }}:\n{}", d.join("\n"));
    let named = c.diags.iter().any(|d| d.code == DiagCode::NotAStruct && d.name == Some(crate::ids::Name::of("C")));
    assert!(named, "E0574 must name `C`: {:?}", c.diags.iter().map(|d| (d.code, d.name)).collect::<Vec<_>>());

    // `impl<T> G<T> { fn k() -> G<u8> { Self::A { 0: 1u8 } } }`: E0308, `Self` being `G<T>`.
    let mut gx = Fx::new();
    let sized = gx.tr("Sized", 1, Vec::new(), &[]);
    gx.lang(LangItem::Sized, sized);
    let u8_ = gx.u8();
    let p0 = gx.param(0);
    let (gk, _) = gx.enum_("G", 1, &[("A", &[p0])]);
    let g_t = gx.adt(gk, &[p0]);
    let g_u8 = gx.adt(gk, &[u8_]);
    gx.use_path("Self", Def::Adt(gk));
    let imp = gx.imp("impl<T> G<T>", 1, g_t, None, &[], Vec::new());
    let k = gx.impl_fn(imp, "k", false, &[], g_u8);
    gx.finish();
    let mut sp = StructBody::new();
    let mut b = Bb::new();
    let one = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let (s, inits) = struct_expr(&mut b, &mut sp, &["Self", "A"], one);
    let root = b.block(&[], Some(s));
    let mut v = b.view(k, root);
    v.paths = &sp.paths;
    v.segments = &sp.segments;
    v.field_inits = &inits;
    let c = check_view(&gx, &v);
    let codes: Vec<DiagCode> = c.diags.iter().map(|d| d.code).collect();
    assert!(codes.contains(&DiagCode::Mismatch), "k: want E0308 (`Self` is `G<T>`), got {codes:?}");
    assert!(!codes.contains(&DiagCode::NotAStruct), "k: the variant step must find `A`, got {codes:?}");

    std::println!("self_struct_variant: clean");
}
