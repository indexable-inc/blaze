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

/// Compile-time reflection intrinsics (`core::mem::type_info` + `field_representing_type_*`) and
/// `write_bytes` in constants.
#[test]
fn type_id_reflection_intrinsics_and_write_bytes_evaluate() {
    let mut w = world();
    w.strukt("Tid", &[("lo", "u64"), ("hi", "u64")]);
    w.strukt("Pair", &[("a", "u64"), ("b", "u32")]);
    w.enum_("E", None, &[("A", &[], None), ("B", &["u32"], None)]);
    w.enum_("OptUsize", None, &[("None", &[], None), ("Some", &["usize"], None)]);
    w.enum_("OptTid", None, &[("None", &[], None), ("Some", &["Tid"], None)]);
    w.enum_(
        "TypeKind",
        None,
        &[
            ("Tuple", &[], None),
            ("Array", &[], None),
            ("Slice", &[], None),
            ("Struct", &[], None),
            ("Enum", &[], None),
            ("Union", &[], None),
            ("Bool", &[], None),
            ("Char", &[], None),
            ("Int", &[], None),
            ("Float", &[], None),
            ("Reference", &[], None),
            ("Pointer", &[], None),
            ("FnPtr", &[], None),
            ("Other", &[], None),
        ],
    );
    w.strukt("Type", &[("kind", "TypeKind")]);
    let tid = w.ty("Tid");
    let usize_ = w.ty("usize");
    let bool_ = w.ty("bool");
    let type_ = w.ty("Type");
    let opt_usize = w.ty("OptUsize");
    let opt_tid = w.ty("OptTid");
    w.intrinsic("type_id", 1, &[], tid, Intrinsic::TypeId);
    w.intrinsic("type_id_type_of", 0, &[tid], type_, Intrinsic::TypeIdTypeOf);
    w.intrinsic("type_id_variants", 0, &[tid], usize_, Intrinsic::TypeIdVariants);
    w.intrinsic("type_id_fields", 0, &[tid, usize_], usize_, Intrinsic::TypeIdFields);
    w.intrinsic("type_id_field_representing_type", 0, &[tid, usize_, usize_], tid, Intrinsic::TypeIdFieldRepresentingType);
    w.intrinsic("field_representing_type_actual_type_id", 0, &[tid], tid, Intrinsic::FieldRepresentingTypeActualTypeId);
    w.intrinsic("field_representing_type_offset", 0, &[tid], usize_, Intrinsic::FieldRepresentingTypeOffset);
    w.intrinsic("type_id_is_signed", 0, &[tid], bool_, Intrinsic::TypeIdIsSigned);
    w.intrinsic("type_id_array_len", 0, &[tid], usize_, Intrinsic::TypeIdArrayLen);
    w.intrinsic("type_id_size_of", 0, &[tid], opt_usize, Intrinsic::TypeIdSizeOf);
    w.intrinsic("type_id_element_ty", 0, &[tid], opt_tid, Intrinsic::TypeIdElementTy);
    w.intrinsic("type_id_points_to", 0, &[tid], opt_tid, Intrinsic::TypeIdPointsTo);
    w.intrinsic("type_id_points_mutably", 0, &[tid], bool_, Intrinsic::TypeIdPointsMutably);
    let unit = w.ty("()");
    let u8_ = w.ty("u8");
    let mut_u8 = w.ty("*mut u8");
    w.intrinsic("write_bytes", 1, &[mut_u8, u8_, usize_], unit, Intrinsic::WriteBytes);
    w.intrinsic("assert_inhabited", 1, &[], unit, Intrinsic::AssertInhabited);
    w.intrinsic("assert_zero_valid", 1, &[], unit, Intrinsic::AssertZeroValid);
    w.enum_("Empty", None, &[]);

    w.konst("PAIR_IS_STRUCT", "bool", "match type_id_type_of(type_id::<Pair>()).kind { TypeKind::Struct => true, _ => false }");
    w.konst("E_VARIANTS", "usize", "type_id_variants(type_id::<E>())");
    w.konst("PAIR_FIELDS", "usize", "type_id_fields(type_id::<Pair>(), 0)");
    w.konst("PAIR_F1_OFF", "usize", "field_representing_type_offset(type_id_field_representing_type(type_id::<Pair>(), 0, 1))");
    w.konst(
        "PAIR_F1_IS_U32",
        "bool",
        "transmute::<Tid, u128>(field_representing_type_actual_type_id(type_id_field_representing_type(type_id::<Pair>(), 0, 1))) == transmute::<Tid, u128>(type_id::<u32>())",
    );
    w.konst("I32_SIGNED", "bool", "type_id_is_signed(type_id::<i32>())");
    w.konst("U32_SIGNED", "bool", "type_id_is_signed(type_id::<u32>())");
    w.konst("ARR_LEN", "usize", "type_id_array_len(type_id::<[u16; 7]>())");
    w.konst("PAIR_SZ", "usize", "match type_id_size_of(type_id::<Pair>()) { OptUsize::Some(n) => n, OptUsize::None => 0 }");
    w.konst("MUT_PTR", "bool", "type_id_points_mutably(type_id::<*mut u8>())");
    w.konst(
        "WB",
        "u32",
        "{ let mut buf = [0u8; 4]; write_bytes::<u8>(&raw mut buf[0], 0xAB, 4); (buf[0] as u32) | ((buf[3] as u32) << 8) }",
    );
    w.konst("INHAB_OK", "u32", "{ assert_inhabited::<Pair>(); assert_zero_valid::<u32>(); 42 }");
    w.konst("INHAB_BAD", "u32", "{ assert_inhabited::<Empty>(); 1 }");
    w.build();

    assert_eq!(eval_int(&mut w, "PAIR_IS_STRUCT"), Ok(1));
    assert_eq!(eval_int(&mut w, "E_VARIANTS"), Ok(2));
    assert_eq!(eval_int(&mut w, "PAIR_FIELDS"), Ok(2));
    assert_eq!(eval_int(&mut w, "PAIR_F1_OFF"), Ok(8));
    assert_eq!(eval_int(&mut w, "PAIR_F1_IS_U32"), Ok(1));
    assert_eq!(eval_int(&mut w, "I32_SIGNED"), Ok(1));
    assert_eq!(eval_int(&mut w, "U32_SIGNED"), Ok(0));
    assert_eq!(eval_int(&mut w, "ARR_LEN"), Ok(7));
    assert_eq!(eval_int(&mut w, "PAIR_SZ"), Ok(16));
    assert_eq!(eval_int(&mut w, "MUT_PTR"), Ok(1));
    assert_eq!(eval_int(&mut w, "WB"), Ok(0xABAB));
    assert_eq!(eval_int(&mut w, "INHAB_OK"), Ok(42));
    assert!(eval_int(&mut w, "INHAB_BAD").is_err(), "uninhabited enum must fail assert_inhabited");
}
