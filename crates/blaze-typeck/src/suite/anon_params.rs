//! Anonymous type parameters at a call. The Reference, "Impl trait" (Anonymous type
//! parameters): "`impl Trait` in argument position ... is an anonymous type parameter";
//! "explicit generic arguments cannot be given for an anonymous type parameter" (so `f::<..>`
//! gives the declared parameters only, and the anonymous ones are inferred from the
//! arguments); "Generic arguments": a path's generic arguments must match its parameters, and
//! too many or too few is rustc's E0107.
//!
//! ```text
//! trait Show {}   impl Show for u16 {}
//! fn f<T>(a: T, b: impl Show);          // own: T, then the anonymous P1: Show
//! fn main(x: u8, y: u16);
//! ```

use std::vec::Vec;

use super::{Want, check_body, expect};
use crate::diag::{Because, DiagCode};
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, PatId, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

fn world() -> (Fx, ItemKey, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let show = fx.tr("Show", 1, Vec::new(), &[]);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let (u8_, u16_) = (fx.u8(), fx.u16());
    fx.imp("show_u16", 0, u16_, Some((show, &[])), &[], Vec::new());
    let unit = fx.unit();
    let b = fx.bound(show, &[p1]);
    let f = fx.func("f", 2, &[p0, p1], unit, std::vec![b]);
    fx.anon_params(f, 1);
    let main = fx.func("main", 0, &[u8_, u16_], unit, Vec::new());
    fx.finish();
    (fx, f, main)
}

/// `f::<args..>(x, y)` in `main`; ids: callee 0, x 1, y 2, call 3, block 4.
fn call(written: &[UintTy]) -> (Fx, ItemKey, Bb, crate::columns::Checked<'static>) {
    let (fx, f, main) = world();
    let mut b = Bb::new();
    let (x, y) = (b.param("x"), b.param("y"));
    let args: Vec<_> = written.iter().map(|&u| b.ty(PNode::Uint(u))).collect();
    let e0 = if written.is_empty() { b.path(&["f"]) } else { b.path_args("f", &args) };
    let (e1, e2) = (b.local(x), b.local(y));
    let e3 = b.call(e0, &[e1, e2]);
    let s = b.semi(e3);
    let root = b.block(&[s], None);
    let c = check_body(&fx, &b, main, root, Plant::None, InferPlant::None);
    (fx, f, b, c)
}

/// `f::<u8>(x, y)`: `T` is given, the anonymous parameter is inferred as `u16` (and
/// `u16: Show` holds): no diagnostic. `f(x, y)` (none written) infers both.
#[test]
fn turbofish_gives_only_the_declared_parameters() {
    for written in [&[UintTy::U8][..], &[][..]] {
    let (fx, _, b, c) = call(written);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[(ExprId(0), "fn f<u8, u16>"), (ExprId(1), "u8"), (ExprId(2), "u16"), (ExprId(3), "()"), (ExprId(4), "()")],
            pats: &[(PatId(0), "u8"), (PatId(1), "u16")],
            diags: &[],
        },
    );
    }
}

/// `f::<u8, u16>(x, y)`: two written for one declared parameter is E0107 at the path, naming
/// `f` and both counts (1 declared, 2 written), though `f` has two own parameters.
#[test]
fn turbofish_with_a_wrong_count_names_the_fn_and_both_counts() {
    for (written, got) in [(&[UintTy::U8, UintTy::U16][..], 2)] {
        let (_, f, _, c) = call(written);
        let found: Vec<_> = c.diags.iter().map(|d| (d.code, d.because)).collect();
        assert_eq!(
            found,
            [(DiagCode::GenericArgCount, Some(Because::ArgCount { item: f, expected: 1, got }))],
            "f with {got} written: {:?}",
            c.diags
        );
    }
}
