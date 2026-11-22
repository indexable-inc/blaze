//! An impl with a const parameter, selected at an instance (walk list-AJ, P1).
//!
//! ```text
//! trait Kind { fn kind(&self) -> u8; }
//! impl<T, const N: usize> Kind for [T; N] { fn kind(&self) -> u8 { .. } }
//! ```
//!
//! `<[u8; 4] as Kind>::kind` resolves to the impl's `kind`, with the impl's arguments `[u8, 4]`.
//!
//! Rules:
//! - The Reference, "Generic parameters": a const parameter's argument is a const ("Const
//!   parameters may be used anywhere a const item may be used ... as a const argument"), and
//!   "Array types": `[T; N]` takes a const `N`. So the variable that instantiates `N` when the
//!   impl is matched is a const variable, never a type variable.
//! - The Reference, "Implementations", "Generic Implementations": "Generic parameters
//!   constrain an implementation if the parameter appears at least once in one of: the
//!   implemented trait, if it has one; the implementing type". `N` appears in the implementing
//!   type `[T; N]`, so the header alone says it is a const.
//! - The pinned library has this form for arrays throughout (core/src/array/mod.rs:
//!   `impl<T: Clone, const N: usize> Clone for [T; N]`, `impl<T, const N: usize> TryFrom<&[T]>
//!   for [T; N]`; core/src/array/equality.rs: `impl<T, U, const N: usize> PartialEq<[U; N]> for
//!   [T; N]`), each reached from a hello world.
//!
//! Before the fix, `instance::unify_header` made a TYPE variable for `N`, and building
//! `[?T; ?N]` refused it ("resolving an instance: .. Sort"), as 519 instances of the walk did.

use blaze_own::Alloc;

use crate::fixture::Fx;
use crate::ids::Mutbl;
use crate::portable::{PNode, PortableTys};

/// Prints nothing of its own; passes:
/// `test suite::impl_const_params::const_param_impl_resolves_at_an_instance ... ok`
#[test]
fn const_param_impl_resolves_at_an_instance() {
    let mut fx = Fx::new();
    let u8_ = fx.u8();
    let usize_ = fx.usize();
    let p0 = fx.param(0);
    let r_self = fx.r(Mutbl::Not, p0);
    let kind = fx.tr("Kind", 1, std::vec::Vec::new(), &[]);
    let kind_fn = fx.trait_fn(kind, "kind", true, &[r_self], u8_);
    let n1 = fx.ty(PNode::ConstParam(1));
    let arr = fx.ty(PNode::Array(p0, n1));
    let imp = fx.imp("impl<T, const N: usize> Kind for [T; N]", 2, arr, Some((kind, &[])), &[], std::vec::Vec::new());
    fx.const_param(imp, 1, usize_);
    let r_arr = fx.r(Mutbl::Not, arr);
    let impl_kind = fx.impl_fn(imp, "kind", true, &[r_arr], u8_);
    fx.const_param(impl_kind, 1, usize_);
    fx.use_trait(kind);
    fx.finish();

    let mut ground = PortableTys::new_in(Alloc::global());
    let u8_g = ground.intern(PNode::Uint(crate::ids::UintTy::U8)).expect("intern");
    let arr4 = ground.array_n(u8_g, 4).expect("intern");
    let args = ground.intern_list(&[arr4]).expect("intern");
    let r = crate::resolve_instance(&fx, kind_fn, args, &mut ground, Alloc::global())
        .unwrap_or_else(|e| panic!("resolving an instance: {e}"));
    assert_eq!(r.item, impl_kind, "`<[u8; 4] as Kind>::kind` is the impl's `kind`");
    let want_n = ground.usize_value(4).expect("intern");
    let got = ground.view().list(r.args).expect("the impl's arguments");
    assert_eq!(got.len(), 2, "the impl's arguments are `[T, N]`");
    assert_eq!(ground.view().node(got[0]).expect("T"), PNode::Uint(crate::ids::UintTy::U8), "T = u8");
    assert_eq!(got[1], want_n, "N = 4usize, a const");
}
