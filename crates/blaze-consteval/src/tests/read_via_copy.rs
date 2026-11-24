//! `read_via_copy` in constants (`core::intrinsics::read_via_copy::<T>(ptr: *const T) -> T`):
//! the implementation detail of `core::ptr::read`, reading a bitwise copy of `*ptr` of type `T`.
//!
//! Covers scalars, wide pointers, aggregates (with pointer provenance preserved), zero-sized
//! types (including non-null aligned integer pointers like `NonNull::dangling()`), lazy statics,
//! and undefined-behaviour refusals (null/unaligned integer pointers, dangling allocations,
//! out-of-bounds reads, uninitialized scalars, and invalid `bool` bit patterns).
//!
//! Control: with the `Intrinsic::ReadViaCopy` arm removed from `machine.rs::intrinsic`, every
//! constant here is refused with `Unsupported { what: "read_via_copy" }`.

use blaze_typeck::{Mutbl, PNode};

use super::{eval_int, world};
use crate::{Intrinsic, Refusal};

/// The suite's world with `read_via_copy<T>(ptr: *const T) -> T` declared.
fn world_read_via_copy() -> crate::fixture::World {
    let mut w = world();
    let p0 = w.fx.param(0);
    let ptr = w.fx.ty(PNode::Ptr(Mutbl::Not, p0));
    w.intrinsic("read_via_copy", 1, &[ptr], p0, Intrinsic::ReadViaCopy);
    w
}

/// Scalars, aggregates, wide pointers, lazy statics, and ZSTs read through `read_via_copy`.
#[test]
fn read_via_copy_reads_values_and_preserves_provenance() {
    let mut w = world_read_via_copy();
    w.strukt("Pair", &[("a", "u32"), ("b", "u32")]);
    w.strukt("Holder", &[("p", "&u32"), ("tag", "u32")]);
    w.strukt("Unit", &[]);
    w.const_fn("ptr_read_u32", &[("p", "*const u32")], "u32", "{ unsafe { read_via_copy::<u32>(p) } }");
    w.stat("TARGET", "u32", "0xdead_beef");
    w.konst("SCALAR", "u32", "{ let x = 0x1234_5678u32; ptr_read_u32(&raw const x) }");
    w.konst("FROM_STATIC", "u32", "unsafe { read_via_copy::<u32>(&raw const TARGET) }");
    w.konst("AGG", "u32", "{ let p = Pair { a: 10, b: 32 }; let q = unsafe { read_via_copy::<Pair>(&raw const p) }; q.a + q.b }");
    w.konst("WIDE", "u8", "{ let a = [10u8, 20, 30]; let s: &[u8] = &a; let s2 = unsafe { read_via_copy::<&[u8]>(&raw const s) }; s2[1] }");
    w.konst(
        "PROV",
        "u32",
        "{ let x = 77u32; let h = Holder { p: &x, tag: 5 }; let h2 = unsafe { read_via_copy::<Holder>(&raw const h) }; *h2.p + h2.tag }",
    );
    w.konst("ZST_LIVE", "u32", "{ let u = Unit {}; let _u2 = unsafe { read_via_copy::<Unit>(&raw const u) }; 9 }");
    w.konst("ZST_DANGLING_INT", "u32", "{ let _u = unsafe { read_via_copy::<Unit>(1usize as *const Unit) }; 11 }");
    w.build();

    assert_eq!(eval_int(&mut w, "SCALAR"), Ok(0x1234_5678));
    assert_eq!(eval_int(&mut w, "FROM_STATIC"), Ok(0xdead_beef));
    assert_eq!(eval_int(&mut w, "AGG"), Ok(42));
    assert_eq!(eval_int(&mut w, "WIDE"), Ok(20));
    assert_eq!(eval_int(&mut w, "PROV"), Ok(82));
    assert_eq!(eval_int(&mut w, "ZST_LIVE"), Ok(9));
    assert_eq!(eval_int(&mut w, "ZST_DANGLING_INT"), Ok(11));
}

/// Invalid pointer reads are refused by name: null, dangling, out-of-bounds, uninit scalar,
/// invalid bool bit pattern, and null ZST pointer.
#[test]
fn read_via_copy_refuses_ub() {
    let mut w = world_read_via_copy();
    w.strukt("Unit", &[]);
    w.const_fn("dangle", &[], "*const u32", "{ let x = 5u32; &raw const x }");
    w.konst("NULL_U32", "u32", "unsafe { read_via_copy::<u32>(0usize as *const u32) }");
    w.konst("NULL_ZST", "u32", "{ let _ = unsafe { read_via_copy::<Unit>(0usize as *const Unit) }; 1 }");
    w.konst("DANGLE", "u32", "unsafe { read_via_copy::<u32>(dangle()) }");
    w.konst("OOB", "u32", "{ let b = 1u8; unsafe { read_via_copy::<u32>(&raw const b as *const u32) } }");
    w.konst("UNINIT", "u32", "{ let x: u32; unsafe { read_via_copy::<u32>(&raw const x) } }");
    w.konst("BAD_BOOL", "bool", "{ let b = 2u8; unsafe { read_via_copy::<bool>(&raw const b as *const bool) } }");
    w.build();

    assert_eq!(w.eval("NULL_U32").expect_err("NULL_U32").refusal, Refusal::IntAsPointer);
    assert_eq!(w.eval("NULL_ZST").expect_err("NULL_ZST").refusal, Refusal::IntAsPointer);
    assert_eq!(w.eval("DANGLE").expect_err("DANGLE").refusal, Refusal::Dangling);
    assert_eq!(w.eval("OOB").expect_err("OOB").refusal, Refusal::OutOfBounds { offset: 0, size: 4, len: 1 });
    assert_eq!(w.eval("UNINIT").expect_err("UNINIT").refusal, Refusal::Uninit { offset: 0 });
    assert_eq!(w.eval("BAD_BOOL").expect_err("BAD_BOOL").refusal, Refusal::InvalidBool { value: 2 });
}

/// Default-binding-mode destructuring of a reference in a `const fn` parameter or `let`
/// (metrics' `const fn hash_label(Label(key, value): &Label) -> u64`): `typeck` records
/// `pat_ty` at the peeled type `Label`, while the argument/initializer arrives as `&Label`.
#[test]
fn peeled_ref_pattern_in_const_fn_param_and_let() {
    let mut w = world();
    w.strukt("Pair", &[("a", "u32"), ("b", "u32")]);
    w.const_fn("sum_ref", &[("Pair { a, b }", "&Pair")], "u32", "{ *a + *b }");
    w.konst("PARAM", "u32", "{ let p = Pair { a: 19, b: 23 }; sum_ref(&p) }");
    w.konst("LET", "u32", "{ let p = Pair { a: 30, b: 12 }; let Pair { a, b } = &p; *a + *b }");
    w.build();

    assert_eq!(eval_int(&mut w, "PARAM"), Ok(42));
    assert_eq!(eval_int(&mut w, "LET"), Ok(42));
}
