//! `f16` and `f128` through the walk and the C convention (walk/halfquad.rs, walk/numbers.rs,
//! cabi.rs). These functions are built, verified and printed, not emitted: the baseline tier
//! holds no `f128` in a register (blaze-emit compose/analysis.rs `class_of`), and LIR's
//! legalize (INTRINSICS.md P3) is the piece that rewrites `f16`/`f128` operations for it.

use super::*;

use crate::cabi::{ArgLoc, Class, Ext, Place, Platform, RetLoc, assign, class_of, ret};
use crate::layout::tests::{F, G, World};
use crate::layout::{LayoutCx, Layouts, ReprAttrs};

const F16: PNode = PNode::Float(blaze_typeck::FloatTy::F16);
const F32: PNode = PNode::Float(blaze_typeck::FloatTy::F32);
const F128: PNode = PNode::Float(blaze_typeck::FloatTy::F128);

/// As tests.rs `build_instance`, without its `emits` (the module's reason): checks `body` of
/// `owner` and builds its one instance, verified.
fn built_unemitted(world: &TestWorld, body: &BodyBuf, owner: ItemKey, root: ExprId) -> Built<Global> {
    let view = body.view(owner, root);
    let mut ty = InferCtx::new_in(Alloc::global());
    let checked = match blaze_typeck::check(&view, world, &mut ty, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        Ok(Outcome::Need(n)) => panic!("the check needed {:?}", n.questions),
        Err(error) => panic!("the check failed: {error:?}"),
    };
    assert!(checked.diags.is_empty(), "diagnostics: {:?}", checked.diags);
    let sig = world.sig(owner).expect("a signature for the owner");
    let mut ground = PortableTys::new_in(Alloc::global());
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
    let built = crate::build(&view, &checked, &sig, &mut subst, &TestAdts { world, body }, Global).unwrap_or_else(|error| panic!("the build failed: {error}"));
    blaze_lir::verify(built.func.view()).unwrap_or_else(|error| panic!("does not verify: {error}\n{}", blaze_lir::print(built.func.view())));
    built
}

/// `fn f(a: f16, b: f128) -> f128 { b }` at `key`.
fn f_of(w: &mut TestWorld, key: u128) -> (BodyBuf, ExprId) {
    w.func(key, "f", &[F16, F128], F128);
    let mut body = BodyBuf::new();
    let a = body.bind("a");
    let b = body.bind("b");
    body.params.extend_from_slice(&[a, b]);
    let root = body.local(b);
    (body, root)
}

/// The pinned library's `primitive_docs.rs`: `f16` is "the "binary16" type defined in IEEE
/// 754-2008" and `f128` "the "binary128" type". AAPCS64 (2024Q3) 5.1.2 lists Half-, Single-,
/// Double- and Quad-precision floats among the fundamental types, each held in one SIMD and
/// floating-point register (`h`, `s`, `d`, `q` views of `v`); 5.9.5.1: an HFA is one to four
/// members of ONE such float type, quad included; 6.8 C.1: a float takes `v[NSRN]`; 6.9: a
/// result comes back where it would go as the first argument. "Type cast expressions": "Casting
/// from an f32 to an f64 is perfect and lossless", so `f32 as f128` widens (`fpext`).
///
/// So: `fn f(a: f16, b: f128) -> f128` has the LIR signature `(f16, f128) -> (f128)` under
/// the Rust ABI and under `extern "C"`; the C convention puts `a` in `h0` (`v0`, 2 bytes),
/// `b` in `q1` (`v1`, 16 bytes) and the result in `q0`; four `f128`s in a `repr(C)` struct are
/// an HFA in `v0..v4`. `1.5f16` is `fconst f16` of 0x3e00 (15872) and `1.5f128` `fconst_wide
/// f128` of 0x3fff8 followed by 27 hex zeros; `x as f128` for an `f32` is `fpext`.
///
/// A wrong fix fails it: `float_repr` still refusing f16/f128 fails the first build; a cabi
/// that still refuses an `f128` fails `class_of`; `float_size` without the 16-byte row makes
/// the struct no HFA (a 64-byte composite); the old `float_cast` makes the cast `fptrunc`.
/// Expected line (lanecheck test blaze-build halfquad): `test tests::halfquad::f16_f128_lits_and_abi ... ok`.
#[test]
fn f16_f128_lits_and_abi() {
    // The Rust ABI and the C ABI: the same LIR signature, float scalars both.
    let mut w = TestWorld::new();
    let (rust_body, rust_root) = f_of(&mut w, 1);
    let rust = built_unemitted(&w, &rust_body, ItemKey(1), rust_root);
    let text = blaze_lir::print(rust.func.view());
    assert_eq!(own_sig(&rust), (vec_of(&[Ty::F16, Ty::F128]), vec_of(&[Ty::F128])), "{text}");
    let mut w = TestWorld::new();
    let (c_body, c_root) = f_of(&mut w, 3);
    w.c_abi.push(ItemKey(3));
    let c = built_unemitted(&w, &c_body, ItemKey(3), c_root);
    let text = blaze_lir::print(c.func.view());
    assert_eq!(own_sig(&c), (vec_of(&[Ty::F16, Ty::F128]), vec_of(&[Ty::F128])), "{text}");

    // Where the C convention puts them.
    let mut lw = World::default();
    let quad4 = lw.strukt(0, &[F::Leaf(F128), F::Leaf(F128), F::Leaf(F128), F::Leaf(F128)], ReprAttrs { c: true, ..ReprAttrs::RUST });
    let mut g = G::new();
    let (h, q) = (g.n(F16), g.n(F128));
    let q4 = g.adt(quad4, &[]);
    let mut class = |t: PTy| {
        let mut ls = Layouts::new_in(Global);
        let mut cx = LayoutCx { ground: &mut g.t, facts: &lw, scratch: Alloc::global() };
        class_of(&mut ls, t, &mut cx, Global).unwrap_or_else(|e| panic!("{t:?}: {e}"))
    };
    let (ch, cq, cq4) = (class(h), class(q), class(q4));
    assert_eq!((ch, cq, cq4), (Class::Float { size: 2 }, Class::Float { size: 16 }, Class::Hfa { count: 4, size: 16 }));
    let mut locs = Vec::new_in(Global);
    assign(&[ch, cq, cq4], None, Platform::Apple, &mut locs).expect("assigns");
    let fpr = |first, count| ArgLoc::Direct { at: Place::Fpr { first, count }, ext: Ext::None };
    // h0, q1, then the HFA in v2..v6.
    assert_eq!(locs.as_slice(), &[fpr(0, 1), fpr(1, 1), fpr(2, 4)]);
    assert_eq!(ret(cq), RetLoc::Direct(Place::Fpr { first: 0, count: 1 }));

    // Literals and a widening cast.
    let mut w = TestWorld::new();
    w.func(1, "main", &[], F128);
    let mut b = BodyBuf::new();
    let e = b.e(Expr::Lit(Lit::Float(Some(blaze_typeck::FloatTy::F128))));
    b.lits.push((e, 0x3fff8 << 108));
    let quad = built_unemitted(&w, &b, MAIN, e);
    let text = blaze_lir::print(quad.func.view());
    assert!(text.lines().any(|l| l.contains("fconst_wide f128 0x3fff8000000000000000000000000000")), "{text}");
    let mut w = TestWorld::new();
    w.func(1, "main", &[], F16);
    let mut b = BodyBuf::new();
    let e = b.e(Expr::Lit(Lit::Float(Some(blaze_typeck::FloatTy::F16))));
    b.lits.push((e, 0x3e00));
    let half = built_unemitted(&w, &b, MAIN, e);
    let text = blaze_lir::print(half.func.view());
    assert!(text.lines().any(|l| l.contains("fconst f16 15872")), "{text}");
    let mut w = TestWorld::new();
    w.func(1, "main", &[F32], F128);
    let mut b = BodyBuf::new();
    let x = b.bind("x");
    b.params.push(x);
    let xe = b.local(x);
    let f128_ty = b.types.intern(F128).expect("a type interns");
    let cast = b.e(Expr::Cast { operand: xe, ty: f128_ty });
    let widened = built_unemitted(&w, &b, MAIN, cast);
    let text = blaze_lir::print(widened.func.view());
    assert!(text.contains("fpext f128") && !text.contains("fptrunc"), "{text}");
}
