//! The pointer-stepping intrinsics read their generic arguments by their own signatures
//! (library/core/src/intrinsics/mod.rs in the pinned core):
//!
//! - 1157: `pub const unsafe fn offset<Ptr: bounds::BuiltinDeref, Delta>(dst: Ptr, offset: Delta) -> Ptr;`
//!   "Calculates the offset from a pointer.": the step is the pointee of generic 0.
//! - 1176: `pub const unsafe fn arith_offset<T>(dst: *const T, offset: isize) -> *const T;`
//!   "Calculates the offset from a pointer, potentially wrapping.": the step is generic 0 itself.
//!
//! So `arith_offset::<u32>(p, 2)` is `p + 8`, `arith_offset::<*const u32>(p, 2)` is `p + 16`
//! (the element is the 8-byte pointer, not its pointee), and `offset::<*const u32, isize>(p, 2)`
//! is `p + 8`; `offset::<u32, isize>` is not of the signature and is refused by name.

use blaze_lir::func::SlotKind;
use blaze_lir::{Builder, Flags, Full, Ty};
use blaze_typeck::{Mutbl, PNode, PTy};
use std::alloc::Global;

use super::{Env, ISIZE, U32, eval, func, scalar};
use crate::intrinsics::{Arg, Call, GenericArg, Which};

/// Writes a `u32` through `which::<generics>(base, 2)` and reads it back at `base + byte`.
fn written_at(env: &mut Env, which: Which, generics: &[GenericArg], ret: PTy, byte: u128) -> u128 {
    let u32_ = env.g.n(U32);
    let unit = env.g.tuple(&[]);
    let f = func(&[Ty::I32, Ty::I64], &[Ty::I32], |b, a| {
        let slot = b.slot(32, 2, SlotKind::Temp);
        let base = b.slot_addr(slot);
        let c = Call { which, generics, args: &[Arg::Scalar(base), Arg::Scalar(a[1])], ret, site: None };
        let at = scalar(env.call(b, &c).unwrap());
        let c = Call { which: Which::WriteViaMove, generics: &[GenericArg::Ty(u32_)], args: &[Arg::Scalar(at), Arg::Scalar(a[0])], ret: unit, site: None };
        env.call(b, &c).unwrap();
        let byte = b.iconst(Ty::I64, byte);
        let expected = b.ptr_add(Flags::NONE, base, byte);
        let c = Call { which: Which::ReadViaCopy, generics: &[GenericArg::Ty(u32_)], args: &[Arg::Scalar(expected)], ret: u32_, site: None };
        vec![scalar(env.call(b, &c).unwrap())]
    });
    eval(&f, &[0xdead_beef, 2])[0]
}

/// Passing prints `test intrinsics::tests::offsets::offset_intrinsics_step_by_the_element_their_signature_names ... ok`.
#[test]
fn offset_intrinsics_step_by_the_element_their_signature_names() {
    let mut env = Env::new();
    let u32_ = env.g.n(U32);
    let isize_ = env.g.n(ISIZE);
    let p_u32 = env.g.n(PNode::Ptr(Mutbl::Not, u32_));
    let p_p_u32 = env.g.n(PNode::Ptr(Mutbl::Not, p_u32));

    // `arith_offset<T>`: T = u32, two elements of 4 bytes.
    assert_eq!(written_at(&mut env, Which::ArithOffset, &[GenericArg::Ty(u32_)], p_u32, 8), 0xdead_beef);
    // `arith_offset<T>`: T = *const u32, two elements of 8 bytes (never the pointee's 4).
    assert_eq!(written_at(&mut env, Which::ArithOffset, &[GenericArg::Ty(p_u32)], p_p_u32, 16), 0xdead_beef);
    // `offset<Ptr, Delta>`: Ptr = *const u32, the pointee's 4 bytes twice.
    assert_eq!(written_at(&mut env, Which::Offset, &[GenericArg::Ty(p_u32), GenericArg::Ty(isize_)], p_u32, 8), 0xdead_beef);

    // `offset`'s generic 0 must be a pointer (`Ptr: BuiltinDeref`): a `u32` there is refused.
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let p = b.iconst(Ty::I64, 0);
    let d = b.iconst(Ty::I64, 2);
    let c = Call { which: Which::Offset, generics: &[GenericArg::Ty(u32_), GenericArg::Ty(isize_)], args: &[Arg::Scalar(p), Arg::Scalar(d)], ret: u32_, site: None };
    let error = env.call(&mut b, &c).unwrap_err();
    assert_eq!(error.to_string(), "intrinsic `offset`: a generic argument that should be a pointer is not one");
}
