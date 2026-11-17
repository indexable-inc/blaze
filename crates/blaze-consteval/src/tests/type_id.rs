//! `type_id::<T>()` in constants: the pinned core, library/core/src/any.rs, `TypeId::of` is
//! `const { intrinsics::type_id::<T>() }`, and library/core/src/intrinsics/reflection.rs: "Gets
//! an identifier which is globally unique to the specified type. This function will return the
//! same value for a type regardless of whichever crate it is invoked in." Its value is the 128
//! bits of blaze-build `instance::type_id_hash`, the one function the builder's row calls too
//! (blaze-build intrinsics/tests.rs `type_id_is_the_one_hash_at_run_time` checks the built code
//! against it), so the constant and the run-time value are one. `TypeId` is 16 bytes here, a
//! struct of two words as core's `data: [*const (); 2]`; `transmute::<_, u128>` reads it back as
//! `TypeId::as_u128` does.
//!
//! Control: with the `Intrinsic::TypeId` arm taken out of machine.rs `intrinsic` (it falls to
//! `other => unsupported(..)`), every constant here is refused with `Unsupported { what:
//! "type_id" }`.

use blaze_own::Alloc;
use blaze_typeck::{PNode, PortableTys, UintTy};

use super::{eval_int, world};
use crate::Intrinsic;

/// The hash of `u8`, `u32` and `(u8, u32)` from a table of their own: the one function's value,
/// independent of the table (and so of the world) the type is interned in.
fn expected() -> [u128; 3] {
    let mut t = PortableTys::new_in(Alloc::global());
    let filler = t.intern(PNode::Bool).unwrap();
    let u32_ = t.intern(PNode::Uint(UintTy::U32)).unwrap();
    let u8_ = t.intern(PNode::Uint(UintTy::U8)).unwrap();
    let l = t.intern_list(&[u8_, u32_]).unwrap();
    let pair = t.intern(PNode::Tuple(l)).unwrap();
    assert_ne!(filler, u8_);
    let h = |x| blaze_build::instance::type_id_hash(x, t.view(), Alloc::global()).unwrap();
    [h(u8_), h(u32_), h(pair)]
}

/// Equal for one type asked twice, different for two types, and equal to the builder's value
/// (the one function, from another table). Prints
/// `test tests::type_id::type_id_is_the_one_hash_in_a_constant ... ok`.
#[test]
fn type_id_is_the_one_hash_in_a_constant() {
    let mut w = world();
    w.strukt("Tid", &[("lo", "u64"), ("hi", "u64")]);
    let out = w.ty("Tid");
    w.intrinsic("type_id", 1, &[], out, Intrinsic::TypeId);
    w.konst("U8", "u128", "transmute::<Tid, u128>(type_id::<u8>())");
    w.konst("U8_AGAIN", "u128", "transmute::<Tid, u128>(type_id::<u8>())");
    w.konst("U32", "u128", "transmute::<Tid, u128>(type_id::<u32>())");
    w.konst("PAIR", "u128", "transmute::<Tid, u128>(type_id::<(u8, u32)>())");
    w.konst("SWAPPED", "u128", "transmute::<Tid, u128>(type_id::<(u32, u8)>())");
    w.build();
    let [u8_, u32_, pair] = expected();
    assert_eq!(eval_int(&mut w, "U8"), Ok(u8_), "the one function's value");
    assert_eq!(eval_int(&mut w, "U8_AGAIN"), Ok(u8_), "one type asked twice");
    assert_eq!(eval_int(&mut w, "U32"), Ok(u32_));
    assert_eq!(eval_int(&mut w, "PAIR"), Ok(pair));
    let swapped = eval_int(&mut w, "SWAPPED").expect("evaluates");
    assert_ne!(swapped, pair, "`(u32, u8)` is another type than `(u8, u32)`");
    assert_ne!(u8_, u32_);
    std::println!("type_id::<u8>() = {u8_:#034x}, type_id::<(u8, u32)>() = {pair:#034x}");
}
