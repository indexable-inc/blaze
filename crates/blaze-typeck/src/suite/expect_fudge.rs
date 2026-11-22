//! A call's expected type reaches an argument whose resolved formal still holds one of the
//! callee's OWN inference variables (rules/expect_inputs.rs, `TypeCtx::list_has_infer_since`).
//!
//! The rung s6-async-04 (std census 1, class C2) stopped in `main` with "IncompatibleArms of
//! two `Pin<Box<Future(..)>>`" at
//!
//! ```text
//! let futs: Vec<Pin<Box<dyn Future<Output = String>>>> = vec![Box::pin(async { .. }), Box::pin(async { .. }), ..];
//! ```
//!
//! The pinned library's `vec!` (library/alloc/src/macros.rs) is
//! `box_assume_init_into_vec_unsafe(write_box_via_move(Box::new_uninit(), [$($x),+]))`, with
//! `fn box_assume_init_into_vec_unsafe<T, const N: usize>(b: Box<MaybeUninit<[T; N]>>) -> Vec<T>`
//! (library/alloc/src/boxed.rs) and `fn write_box_via_move<T>(b: Box<MaybeUninit<T>>, x: T) ->
//! Box<MaybeUninit<T>>` (library/alloc/src/intrinsics.rs, whose doc says the call chain is kept
//! "or else this breaks inference of e.g. unsizing coercions"). The expected `Vec<P>` gives
//! `T = P`, so the formal input is `Box<MaybeUninit<[P; ?N]>>`: it still holds `?N`, the
//! callee's own const variable, made BEFORE the probe. The check took no hint from any input
//! holding a variable, so the array was typed with no expected element: its first element fixed
//! the element type to that async block's own future and the second, another block's, was
//! E0308. The Reference, "Type coercions", "Coercion sites": "Array literals, where the array
//! has type `[U; n]`. Each sub-expression in the array literal is a coercion site for coercion
//! to type `U`", and "Function results", "Arguments for function calls": each argument is
//! coerced to its formal parameter's type, which the call's expected type decides here. rustc's
//! `fudge_inference_if_ok` drops only the variables the probe made.
//!
//! The toy world, reduced (a tuple stands for the array: one more element type, `X`, that the
//! expected type does not decide; `&mut u8` to `&u8` for the unsizing coercion):
//!
//! ```text
//! #[lang = "sized"] trait Sized {}
//! enum Opt<T> { Some(T), None }
//! fn mk<T, X>(x: (T, T, X)) -> Opt<T>;
//! fn shared(x: &mut u8, y: &u8) -> Opt<&u8> { mk((x, y, 0u8)) }      // clean
//! fn unique(x: &mut u8, y: &u8) -> Opt<&mut u8> { mk((x, y, 0u8)) }  // E0308 at `y`
//! ```
//!
//! `shared`: `T = &u8` from the expected type, the formal is `(&u8, &u8, ?X)` (`?X` made with
//! `mk`'s instantiation, before the probe), the tuple's elements are coercion sites to it
//! ("Coercion sites": "Function results" and the tuple's fields as the formal's), and
//! `&mut u8` coerces to `&u8` ("Coercion types": "`&mut T` to `&T`"). Without the fix the
//! tuple is typed `(&mut u8, &u8, u8)` first and is then E0308 at the argument against
//! `(?T, ?T, ?X)`. `unique`: the control: no coercion takes `&u8` to `&mut u8`, so the hint
//! still refuses, once, at the element `y` (without the fix, at the whole argument instead).
//!
//! Each test prints `test suite::expect_fudge::<name> ... ok`.

use std::string::String;
use std::vec::Vec;

use super::{Want, check_body, differences};
use crate::body::IntSuffix;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, Mutbl, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;

/// `shared` and `unique`.
fn world() -> (Fx, [ItemKey; 2]) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (u8_, p0, p1) = (fx.u8(), fx.param(0), fx.param(1));
    let (opt, _) = fx.enum_("Opt", 1, &[("Some", &[p0]), ("None", &[])]);
    let three = fx.tuple(&[p0, p0, p1]);
    let opt_p = fx.adt(opt, &[p0]);
    fx.func("mk", 2, &[three], opt_p, Vec::new());
    let r_u8 = fx.r(Mutbl::Not, u8_);
    let m_u8 = fx.r(Mutbl::Mut, u8_);
    let opt_r = fx.adt(opt, &[r_u8]);
    let opt_m = fx.adt(opt, &[m_u8]);
    let shared = fx.func("shared", 0, &[m_u8, r_u8], opt_r, Vec::new());
    let unique = fx.func("unique", 0, &[m_u8, r_u8], opt_m, Vec::new());
    fx.finish();
    (fx, [shared, unique])
}

/// Every difference but the expressions this test writes no expectation for.
fn written(lines: Vec<String>) -> Vec<String> {
    lines.into_iter().filter(|l| !l.contains("no expectation written")).collect()
}

/// The body `mk((x, y, 0u8))` of `owner(x, y)`; the element `y` and the argument.
fn body() -> (Bb, ExprId, ExprId, ExprId) {
    let mut b = Bb::new();
    let px = b.param("x");
    let py = b.param("y");
    let mk = b.path(&["mk"]);
    let x = b.local(px);
    let y = b.local(py);
    let zero = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let arg = b.tuple(&[x, y, zero]);
    let call = b.call(mk, &[arg]);
    let root = b.block(&[], Some(call));
    (b, y, arg, root)
}

/// `shared`: the hint `(&u8, &u8, ?X)` holds `mk`'s own `?X` and is taken; both elements
/// coerce to `&u8`. Without the fix: one E0308 at the argument (`(&mut u8, &u8, u8)` against
/// `(?T, ?T, ?X)`), the census's IncompatibleArms in the array form.
#[test]
fn a_hint_holding_the_callees_own_variable_reaches_the_argument() {
    let (fx, [shared, _]) = world();
    let (b, _, _, root) = body();
    let c = check_body(&fx, &b, shared, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[], pats: &[], diags: &[] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// `unique`: the hint `(&mut u8, &mut u8, ?X)` is taken, and `y: &u8` has no coercion to
/// `&mut u8`: one E0308, at `y`. Without the fix the one E0308 is at the whole argument.
#[test]
fn a_hinted_element_with_no_coercion_is_refused_at_the_element() {
    let (fx, [_, unique]) = world();
    let (b, y, _, root) = body();
    let c = check_body(&fx, &b, unique, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[], pats: &[], diags: &[(DiagCode::Mismatch, y)] };
    let d = written(differences(&fx, &b, &c, &want));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}
