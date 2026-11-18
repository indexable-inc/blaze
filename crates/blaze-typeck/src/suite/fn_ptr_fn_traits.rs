//! Which fn pointers implement the `Fn*` traits. The rule is the pinned nightly's rustc_type_ir
//! `FnSig::is_fn_trait_compatible`: "!self.c_variadic() && self.safety().is_safe() &&
//! self.abi() == ExternAbi::Rust". Only a safe, non-variadic fn pointer of the Rust ABI is
//! `Fn*`. An `extern "C" fn(u8) -> u16` is not, so passing it where `F: Fn(u8) -> u16` is asked
//! is E0277 (blaze-solve builtin.rs `callable`, the `FnPtr` arm).
//!
//! ```text
//! #[lang = "fn_once"] trait FnOnce<Args> { type Output; extern "rust-call" fn call_once(self, args: Args) -> Self::Output; }
//! #[lang = "fn_mut"] trait FnMut<Args>: FnOnce<Args> { .. }
//! #[lang = "fn"] trait Fn<Args>: FnMut<Args> { .. }
//! fn apply<F: Fn(u8) -> u16>(f: F) -> u16;
//! fn with_rust(f: fn(u8) -> u16);
//! fn with_c(f: extern "C" fn(u8) -> u16);
//! fn with_sysv(f: extern "sysv64" fn(u8) -> u16);
//! ```

use std::vec::Vec;

use super::check_body;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{Abi, FnPtrHeader, ItemKey, LangItem, Mutbl, Safety, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

struct Items {
    with_rust: ItemKey,
    with_c: ItemKey,
    with_sysv: ItemKey,
    main: ItemKey,
}

fn world() -> (Fx, Items) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let rust_call = FnPtrHeader { abi: Abi::RustCall, safety: Safety::Safe, c_variadic: false };

    let fn_once = fx.tr("FnOnce", 2, Vec::new(), &["Output"]);
    fx.lang(LangItem::FnOnce, fn_once);
    let out = fx.alias(fn_once, "Output", &[p0, p1]);
    let call_once = fx.trait_fn(fn_once, "call_once", true, &[p0, p1], out);
    fx.set_header(call_once, rust_call);

    let sup = fx.bound(fn_once, &[p0, p1]);
    let fn_mut = fx.tr("FnMut", 2, std::vec![sup], &[]);
    fx.lang(LangItem::FnMut, fn_mut);
    let m_self = fx.r(Mutbl::Mut, p0);
    let call_mut = fx.trait_fn(fn_mut, "call_mut", true, &[m_self, p1], out);
    fx.set_header(call_mut, rust_call);

    let sup = fx.bound(fn_mut, &[p0, p1]);
    let fn_ = fx.tr("Fn", 2, std::vec![sup], &[]);
    fx.lang(LangItem::Fn, fn_);
    let r_self = fx.r(Mutbl::Not, p0);
    let call = fx.trait_fn(fn_, "call", true, &[r_self, p1], out);
    fx.set_header(call, rust_call);

    let (u8_, u16_) = (fx.ty(PNode::Uint(UintTy::U8)), fx.ty(PNode::Uint(UintTy::U16)));
    let unit = fx.unit();
    let args = fx.tuple(&[u8_]);
    let ret = fx.proj(fn_once, &[p0, args], "Output", u16_);
    let b = fx.bound(fn_, &[p0, args]);
    fx.func("apply", 1, &[p0], u16_, std::vec![b, ret]);

    let io = fx.list(&[u8_, u16_]);
    let rust_ptr = fx.ty(PNode::FnPtr(io, FnPtrHeader::RUST));
    let c_ptr = fx.ty(PNode::FnPtr(io, FnPtrHeader { abi: Abi::C, safety: Safety::Safe, c_variadic: false }));
    let sysv_ptr = fx.ty(PNode::FnPtr(io, FnPtrHeader { abi: Abi::SysV64, safety: Safety::Safe, c_variadic: false }));
    let with_rust = fx.func("with_rust", 0, &[rust_ptr], unit, Vec::new());
    let with_c = fx.func("with_c", 0, &[c_ptr], unit, Vec::new());
    let with_sysv = fx.func("with_sysv", 0, &[sysv_ptr], unit, Vec::new());
    fx.func("k", 0, &[u8_], u16_, Vec::new());
    let ck = fx.func("ck", 0, &[u8_], u16_, Vec::new());
    fx.set_header(ck, FnPtrHeader { abi: Abi::C, safety: Safety::Safe, c_variadic: false });
    let main = fx.func("main", 0, &[], unit, Vec::new());
    fx.finish();
    (fx, Items { with_rust, with_c, with_sysv, main })
}

/// ```text
/// fn with_rust(f: fn(u8) -> u16) { apply(f); }                 // holds
/// fn with_c(f: extern "C" fn(u8) -> u16) { apply(f); }         // E0277
/// fn with_sysv(f: extern "sysv64" fn(u8) -> u16) { apply(f); } // E0277
/// ```
/// `fn(u8) -> u16` is `Fn<(u8,)>` with `Output = u16`. The two foreign-ABI pointers are not
/// (`is_fn_trait_compatible`: `abi == ExternAbi::Rust`), so each call is E0277 and nothing else.
/// Before, only "rust-call" and `unsafe` were refused, so the C pointer held.
#[test]
fn only_rust_abi_fn_pointers_are_fn() {
    let (fx, it) = world();
    for (owner, refused) in [(it.with_rust, false), (it.with_c, true), (it.with_sysv, true)] {
        let mut b = Bb::new();
        let p = b.param("f");
        let a = b.path(&["apply"]);
        let f = b.local(p);
        let call = b.call(a, &[f]);
        let s = b.semi(call);
        let root = b.block(&[s], None);
        let c = check_body(&fx, &b, owner, root, Plant::None, InferPlant::None);
        let unsatisfied = c.diags.iter().filter(|d| d.code == DiagCode::Unsatisfied).count();
        // A refused pointer fails the `Fn` bound and its `Output` projection (E0277 rows at one
        // range, as for a refused fn item in fn_traits.rs); nothing but E0277 is reported.
        assert_eq!((unsatisfied > 0, c.diags.len() == unsatisfied), (refused, true), "apply(f) in {owner:?}: {:?}", c.diags);
    }
}

/// ```text
/// fn k(x: u8) -> u16;   extern "C" fn ck(x: u8) -> u16;
/// fn main() { apply(k); }    // holds
/// fn main() { apply(ck); }   // E0277
/// ```
/// A fn item takes the same rule (`extract_tupled_inputs_and_output_from_callable`, `FnDef`:
/// `sig.is_fn_trait_compatible()`): a safe Rust-ABI item is `Fn`, an `extern "C"` one is not.
#[test]
fn only_rust_abi_fn_items_are_fn() {
    let (fx, it) = world();
    for (name, refused) in [("k", false), ("ck", true)] {
        let mut b = Bb::new();
        let a = b.path(&["apply"]);
        let f = b.path(&[name]);
        let call = b.call(a, &[f]);
        let s = b.semi(call);
        let root = b.block(&[s], None);
        let c = check_body(&fx, &b, it.main, root, Plant::None, InferPlant::None);
        let unsatisfied = c.diags.iter().filter(|d| d.code == DiagCode::Unsatisfied).count();
        assert_eq!((unsatisfied > 0, c.diags.len() == unsatisfied), (refused, true), "apply({name}): {:?}", c.diags);
    }
}

/// The names `Abi::name` prints and `Abi::of_name` reads are one table: every ABI round-trips,
/// and the header byte (`FnPtrHeader::bits`) round-trips for each with both flags.
#[test]
fn every_abi_round_trips_by_name_and_by_bits() {
    for abi in Abi::ALL {
        assert_eq!(Abi::of_name(abi.name()), Some(abi), "{abi:?}");
        for safety in [Safety::Safe, Safety::Unsafe] {
            for c_variadic in [false, true] {
                let h = FnPtrHeader { abi, safety, c_variadic };
                assert_eq!(FnPtrHeader::from_bits(h.bits()), Some(h), "{h:?}");
            }
        }
    }
    assert_eq!(Abi::of_name("stdcall-unwnd"), None);
    assert_eq!(FnPtrHeader::from_bits(40), None);
}
