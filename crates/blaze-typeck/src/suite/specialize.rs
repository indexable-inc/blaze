//! Specialization at an instance: the most specific holding impl wins (walk list-AJ, P9).
//!
//! ```text
//! trait Display {}                 impl Display for str {}     struct D;  impl Display for D {}
//! trait ToString { fn to_string(&self) -> u8; }
//! impl<T: Display + ?Sized> ToString for T { default fn to_string(&self) -> u8 { .. } }
//! impl ToString for str { fn to_string(&self) -> u8 { .. } }
//! ```
//!
//! The pinned library: alloc/src/string.rs writes exactly this pair (`impl<T: fmt::Display +
//! ?Sized> ToString for T` with `default fn to_string`, beside `impl ToString for str`), under
//! `#![feature(min_specialization)]` in alloc/src/lib.rs. RFC 1210: impl A specializes impl B
//! when A's trait reference is an instance of B's under A's where-clauses; the most specific
//! impl that applies is the one used. So `<str as ToString>::to_string` is the str impl's, and
//! `<D as ToString>::to_string` (only the blanket applies) is the blanket's at `T = D`.
//! Before P9, `instance::select_in` returned `Overlap` at the second holding impl, and every
//! `"x".to_string()` of a hello run was refused.
//!
//! The control: two blanket impls neither of which specializes the other (`impl<T: A> Show for
//! T`, `impl<T: B> Show for T`, with `u8: A + B`) stay an `Overlap`, so the choice is not
//! "take the last" or "take the concrete one".
//!
//! The fixture registers no `Sized` lang item, so no implied `Sized` clause is filed: every
//! parameter here is `?Sized`, as the library's blanket impl's is.

use crate::fixture::Fx;
use crate::ids::{ItemKey, Mutbl};
use crate::instance::ResolveError;
use crate::portable::{PNode, PortableTys};

struct World {
    fx: Fx,
    to_string: ItemKey,
    blanket_fn: ItemKey,
    str_fn: ItemKey,
    d: ItemKey,
}

fn world() -> World {
    let mut fx = Fx::new();
    let u8_ = fx.u8();
    let p0 = fx.param(0);
    let r_self = fx.r(Mutbl::Not, p0);
    let str_ = fx.ty(PNode::Str);
    let display = fx.tr("Display", 1, std::vec::Vec::new(), &[]);
    fx.imp("impl Display for str", 0, str_, Some((display, &[])), &[], std::vec::Vec::new());
    let d = fx.strukt("D", 0, &[]);
    let d_ty = fx.adt(d, &[]);
    fx.imp("impl Display for D", 0, d_ty, Some((display, &[])), &[], std::vec::Vec::new());
    let tr = fx.tr("ToString", 1, std::vec::Vec::new(), &[]);
    let to_string = fx.trait_fn(tr, "to_string", true, &[r_self], u8_);
    let t_display = fx.bound(display, &[p0]);
    let blanket = fx.imp("impl<T: Display + ?Sized> ToString for T", 1, p0, Some((tr, &[])), &[], std::vec![t_display]);
    let blanket_fn = fx.impl_fn(blanket, "to_string", true, &[r_self], u8_);
    let str_imp = fx.imp("impl ToString for str", 0, str_, Some((tr, &[])), &[], std::vec::Vec::new());
    let r_str = fx.r(Mutbl::Not, str_);
    let str_fn = fx.impl_fn(str_imp, "to_string", true, &[r_str], u8_);
    fx.use_trait(tr);
    fx.finish();
    World { fx, to_string, blanket_fn, str_fn, d }
}

/// `<str as ToString>::to_string` is the str impl's `to_string`, with no arguments. Prints
/// nothing of its own; passes:
/// `test suite::specialize::the_most_specific_impl_wins ... ok`.
/// Without P9 it panics "resolving an instance: selecting an impl of .. both .. and .. hold
/// (overlapping impls)".
#[test]
fn the_most_specific_impl_wins() {
    let w = world();
    let alloc = blaze_own::testing::heap().alloc();
    let mut ground = PortableTys::new_in(alloc);
    let str_g = ground.intern(PNode::Str).expect("intern");
    let args = ground.intern_list(&[str_g]).expect("intern");
    let r = crate::resolve_instance(&w.fx, w.to_string, args, &mut ground, alloc)
        .unwrap_or_else(|e| panic!("resolving an instance: {e}"));
    assert_eq!(r.item, w.str_fn, "`<str as ToString>::to_string` is the str impl's");
    assert!(ground.view().list(r.args).expect("the impl's arguments").is_empty(), "the str impl has no parameters");
}

/// `<D as ToString>::to_string`: only the blanket impl holds, so it is chosen at `T = D`.
/// Passes: `test suite::specialize::the_blanket_impl_serves_a_display_type ... ok`.
#[test]
fn the_blanket_impl_serves_a_display_type() {
    let w = world();
    let alloc = blaze_own::testing::heap().alloc();
    let mut ground = PortableTys::new_in(alloc);
    let no_args = ground.intern_list(&[]).expect("intern");
    let d_g = ground.intern(PNode::Adt(w.d, no_args)).expect("intern");
    let args = ground.intern_list(&[d_g]).expect("intern");
    let r = crate::resolve_instance(&w.fx, w.to_string, args, &mut ground, alloc)
        .unwrap_or_else(|e| panic!("resolving an instance: {e}"));
    assert_eq!(r.item, w.blanket_fn, "`<D as ToString>::to_string` is the blanket impl's");
    let got = ground.view().list(r.args).expect("the impl's arguments");
    assert_eq!(got.len(), 1, "the blanket impl's arguments are `[T]`");
    assert_eq!(got[0], d_g, "T = D");
}

/// The control: `impl<T: A> Show for T` and `impl<T: B> Show for T` both hold at `u8` and
/// neither specializes the other (A's `T: A` does not prove B's `T: B`, nor the reverse), so
/// the instance is still an `Overlap`. Passes:
/// `test suite::specialize::unrelated_holding_impls_stay_an_overlap ... ok`.
#[test]
fn unrelated_holding_impls_stay_an_overlap() {
    let mut fx = Fx::new();
    let u8_ = fx.u8();
    let p0 = fx.param(0);
    let r_self = fx.r(Mutbl::Not, p0);
    let a = fx.tr("A", 1, std::vec::Vec::new(), &[]);
    let b = fx.tr("B", 1, std::vec::Vec::new(), &[]);
    fx.imp("impl A for u8", 0, u8_, Some((a, &[])), &[], std::vec::Vec::new());
    fx.imp("impl B for u8", 0, u8_, Some((b, &[])), &[], std::vec::Vec::new());
    let show = fx.tr("Show", 1, std::vec::Vec::new(), &[]);
    let show_fn = fx.trait_fn(show, "show", true, &[r_self], u8_);
    let t_a = fx.bound(a, &[p0]);
    let t_b = fx.bound(b, &[p0]);
    let via_a = fx.imp("impl<T: A> Show for T", 1, p0, Some((show, &[])), &[], std::vec![t_a]);
    fx.impl_fn(via_a, "show", true, &[r_self], u8_);
    let via_b = fx.imp("impl<T: B> Show for T", 1, p0, Some((show, &[])), &[], std::vec![t_b]);
    fx.impl_fn(via_b, "show", true, &[r_self], u8_);
    fx.use_trait(show);
    fx.finish();

    let alloc = blaze_own::testing::heap().alloc();
    let mut ground = PortableTys::new_in(alloc);
    let u8_g = ground.intern(PNode::Uint(crate::ids::UintTy::U8)).expect("intern");
    let args = ground.intern_list(&[u8_g]).expect("intern");
    match crate::resolve_instance(&fx, show_fn, args, &mut ground, alloc) {
        Err(ResolveError::Overlap { trait_, first, second }) => {
            assert_eq!(trait_, show);
            assert_eq!([first, second], [via_a, via_b], "the two holding impls, in selection order");
        }
        other => panic!("want an Overlap of the two blanket impls, got {other:?}"),
    }
}
