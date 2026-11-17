//! A fn pointer's header: its ABI and safety are part of its type (rustc's `FnSig::abi` and
//! `safety`, compared by `relate` in `super_relate_tys`' `FnPtr` arm). A fn item reifies to a
//! pointer with its own header (`coerce_from_fn_item`); a safe pointer coerces to the `unsafe`
//! one of the same ABI (`coerce_from_safe_fn`, `Adjust::Pointer(UnsafeFnPointer)`). Nothing
//! else changes a header: no ABI to another, no `unsafe` to safe.
//!
//! ```text
//! fn f(x: u8) -> u8;
//! unsafe fn g(x: u8) -> u8;
//! extern "C" fn h(x: u8) -> u8;
//! ```

use std::vec::Vec;

use super::{Want, adjusts, check_body, expect};
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{Abi, ExprId, FnPtrHeader, ItemKey, LangItem, PatId, Safety, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::{PNode, PTy};

const UNSAFE: FnPtrHeader = FnPtrHeader { abi: Abi::Rust, safety: Safety::Unsafe, c_variadic: false };
const C: FnPtrHeader = FnPtrHeader { abi: Abi::C, safety: Safety::Safe, c_variadic: false };
const UNSAFE_C: FnPtrHeader = FnPtrHeader { abi: Abi::C, safety: Safety::Unsafe, c_variadic: false };

/// The world: `f`, `g`, `h`, and the owner `fn main(p: fn(u8) -> u8)`.
fn world() -> (Fx, ItemKey) {
    let mut world = Fx::new();
    let fx = &mut world;
    let u8_ = fx.ty(PNode::Uint(UintTy::U8));
    // `Copy` and core's `impl Copy for u8`: a capture used by value is a copy, not a move.
    let copy = fx.tr("Copy", 1, Vec::new(), &[]);
    fx.lang(LangItem::Copy, copy);
    fx.imp("impl Copy for u8", 0, u8_, Some((copy, &[])), &[], Vec::new());
    let unit = fx.unit();
    let io = fx.list(&[u8_, u8_]);
    let fn_ptr = fx.ty(PNode::FnPtr(io, FnPtrHeader::RUST));
    fx.func("f", 0, &[u8_], u8_, Vec::new());
    let g = fx.func("g", 0, &[u8_], u8_, Vec::new());
    fx.set_header(g, UNSAFE);
    let h = fx.func("h", 0, &[u8_], u8_, Vec::new());
    fx.set_header(h, C);
    let main = fx.func("main", 0, &[fn_ptr], unit, Vec::new());
    fx.finish();
    (world, main)
}

/// `fn(u8) -> u8` with header `h`, in the body's table.
fn fn_ty(b: &mut Bb, h: FnPtrHeader) -> PTy {
    let u8_ = b.ty(PNode::Uint(UintTy::U8));
    let io = b.t.intern_list(&[u8_, u8_]).expect("intern");
    b.ty(PNode::FnPtr(io, h))
}

/// ```text
/// fn main(p: fn(u8) -> u8) {
///     let a: unsafe fn(u8) -> u8 = f;
///     let b: unsafe fn(u8) -> u8 = p;
///     let c: extern "C" fn(u8) -> u8 = h;
///     let d: unsafe extern "C" fn(u8) -> u8 = h;
/// }
/// ```
/// `f` reifies to `fn(u8) -> u8`, then becomes `unsafe`; `p` only becomes `unsafe`; `h`
/// reifies to its own `extern "C"` header, and to the `unsafe` one after it.
#[test]
fn reify_keeps_the_header_and_safe_coerces_to_unsafe() {
    let (fx, main) = world();
    let mut b = Bb::new();
    let p = b.param("p");
    let e0 = b.path(&["f"]);
    let a = b.bind("a");
    let t_unsafe = fn_ty(&mut b, UNSAFE);
    let s0 = b.let_(a, Some(t_unsafe), e0);
    let e1 = b.local(p);
    let pb = b.bind("b");
    let s1 = b.let_(pb, Some(t_unsafe), e1);
    let e2 = b.path(&["h"]);
    let pc = b.bind("c");
    let t_c = fn_ty(&mut b, C);
    let s2 = b.let_(pc, Some(t_c), e2);
    let e3 = b.path(&["h"]);
    let pd = b.bind("d");
    let t_unsafe_c = fn_ty(&mut b, UNSAFE_C);
    let s3 = b.let_(pd, Some(t_unsafe_c), e3);
    let root = b.block(&[s0, s1, s2, s3], None);
    let c = check_body(&fx, &b, main, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[
                (ExprId(0), "fn f"),
                (ExprId(1), "fn(u8) -> u8"),
                (ExprId(2), "fn h"),
                (ExprId(3), "fn h"),
                (ExprId(4), "()"),
            ],
            pats: &[
                (PatId(0), "fn(u8) -> u8"),
                (PatId(1), "unsafe fn(u8) -> u8"),
                (PatId(2), "unsafe fn(u8) -> u8"),
                (PatId(3), "extern \"C\" fn(u8) -> u8"),
                (PatId(4), "unsafe extern \"C\" fn(u8) -> u8"),
            ],
            diags: &[],
        },
    );
    assert_eq!(
        adjusts(&fx, &c, ExprId(0)),
        "ReifyFnPointer -> fn(u8) -> u8, UnsafeFnPointer -> unsafe fn(u8) -> u8"
    );
    assert_eq!(adjusts(&fx, &c, ExprId(1)), "UnsafeFnPointer -> unsafe fn(u8) -> u8");
    assert_eq!(adjusts(&fx, &c, ExprId(2)), "ReifyFnPointer -> extern \"C\" fn(u8) -> u8");
    assert_eq!(
        adjusts(&fx, &c, ExprId(3)),
        "ReifyFnPointer -> extern \"C\" fn(u8) -> u8, UnsafeFnPointer -> unsafe extern \"C\" fn(u8) -> u8"
    );
}

/// ```text
/// fn main(p: fn(u8) -> u8) {
///     let x: extern "C" fn(u8) -> u8 = f;
///     let y: fn(u8) -> u8 = g;
///     let z: extern "C" fn(u8) -> u8 = p;
/// }
/// ```
/// The control: each is the case above with the header changed the other way, and each is
/// E0308 (rustc: "expected `extern "C" fn(u8) -> u8`, found fn item"; "expected normal fn,
/// found unsafe fn"; "expected "C" fn, found "Rust" fn").
#[test]
fn no_coercion_across_abis_or_from_unsafe() {
    let (fx, main) = world();
    let mut b = Bb::new();
    let p = b.param("p");
    let e0 = b.path(&["f"]);
    let x = b.bind("x");
    let t_c = fn_ty(&mut b, C);
    let s0 = b.let_(x, Some(t_c), e0);
    let e1 = b.path(&["g"]);
    let y = b.bind("y");
    let t_rust = fn_ty(&mut b, FnPtrHeader::RUST);
    let s1 = b.let_(y, Some(t_rust), e1);
    let e2 = b.local(p);
    let z = b.bind("z");
    let s2 = b.let_(z, Some(t_c), e2);
    let root = b.block(&[s0, s1, s2], None);
    let c = check_body(&fx, &b, main, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[(ExprId(0), "fn f"), (ExprId(1), "fn g"), (ExprId(2), "fn(u8) -> u8"), (ExprId(3), "()")],
            pats: &[
                (PatId(1), "extern \"C\" fn(u8) -> u8"),
                (PatId(2), "fn(u8) -> u8"),
                (PatId(3), "extern \"C\" fn(u8) -> u8"),
            ],
            diags: &[(DiagCode::Mismatch, ExprId(0)), (DiagCode::Mismatch, ExprId(1)), (DiagCode::Mismatch, ExprId(2))],
        },
    );
    assert_eq!(adjusts(&fx, &c, ExprId(0)), "");
    assert_eq!(adjusts(&fx, &c, ExprId(1)), "");
    assert_eq!(adjusts(&fx, &c, ExprId(2)), "");
}

/// `{ let y: u8 = ..; let f: H fn(u8) -> u8 = |x| BODY; }`: the closure's diagnostics and its
/// adjustments. `capture`: BODY is `y` (a local of the enclosing body), else `x`.
fn closure_to(h: FnPtrHeader, capture: bool) -> (Vec<DiagCode>, std::string::String) {
    let (fx, main) = world();
    let mut b = Bb::new();
    let u8_ = b.ty(PNode::Uint(UintTy::U8));
    let y = b.bind("y");
    let one = b.int(None);
    let s0 = b.let_(y, Some(u8_), one);
    let x = b.bind("x");
    let body = if capture { b.local(y) } else { b.local(x) };
    let cl = b.closure(&[x], body);
    let t = fn_ty(&mut b, h);
    let f = b.bind("f");
    let s1 = b.let_(f, Some(t), cl);
    let root = b.block(&[s0, s1], None);
    let c = check_body(&fx, &b, main, root, Plant::None, InferPlant::None);
    (c.diags.iter().map(|d| d.code).collect(), adjusts(&fx, &c, cl))
}

/// The Reference, "Type coercions", "Coercion types": "Non capturing closures to `fn`
/// pointers". `|x| x` coerces to `fn(u8) -> u8` and to `unsafe fn(u8) -> u8`; `|x| y` captures
/// `y` and does not (E0308). Before the fix every closure was E0308 here ("found
/// Closure(..), expected FnPtr(..)": bstd::fs::rename_unsynced).
#[test]
fn a_closure_that_captures_nothing_is_a_fn_pointer() {
    let (d, adj) = closure_to(FnPtrHeader::RUST, false);
    assert_eq!(d, Vec::new());
    assert_eq!(adj, "ClosureFnPointer -> fn(u8) -> u8");
    let (d, _) = closure_to(UNSAFE, false);
    assert_eq!(d, Vec::new());
    let (d, _) = closure_to(FnPtrHeader::RUST, true);
    assert_eq!(d, std::vec![DiagCode::Mismatch]);
    let (d, _) = closure_to(C, false);
    assert_eq!(d, std::vec![DiagCode::Mismatch]);
}
