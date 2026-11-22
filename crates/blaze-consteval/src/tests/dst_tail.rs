//! A borrow of a dynamically sized struct's unsized tail field keeps the struct's length. The
//! Reference, "Dynamically sized types": "Structs may contain a DST as the last field; this
//! makes the struct itself a DST", and a pointer to it is wide, its metadata the tail's. The
//! shape is crypto-bigint 0.7's `UintRef(pub [Limb])`, made by
//! `&*(limbs as *const [Limb] as *const UintRef)` and read by `self.0.len()`, which p256's
//! `FieldParams::PARAMS.modulus().as_ref().bits()` and k256's `FRAC_MODULUS_2` reach.

use super::{eval_int, world};
use crate::Intrinsic;

/// `&r.tail` of a `&R` cast from a three-byte slice has length 3 and reads the slice's bytes;
/// the sized field `head` of a sized struct beside it borrows as before. Without the field
/// projection keeping the place's length both tail constants were refused with "an unsized
/// place with no length".
/// Prints `test tests::dst_tail::a_borrowed_unsized_tail_keeps_its_length ... ok`.
#[test]
fn a_borrowed_unsized_tail_keeps_its_length() {
    let mut w = world();
    let usize_ = w.fx.usize();
    let bytes_ptr = w.ty("*const [u8]");
    w.intrinsic("ptr_metadata", 0, &[bytes_ptr], usize_, Intrinsic::PtrMetadata);
    w.strukt("R", &[("tail", "[u8]")]);
    w.strukt("S", &[("head", "u16"), ("next", "u8")]);
    w.konst("A", "[u8; 3]", "[5, 6, 7]");
    w.const_fn("len", &[("s", "&[u8]")], "usize", "{ ptr_metadata(s as *const [u8]) }");
    w.const_fn("view", &[("s", "&[u8]")], "&R", "{ unsafe { &*(s as *const [u8] as *const R) } }");
    w.konst("LEN", "usize", "len(&view(&A).tail)");
    w.konst("LAST", "u8", "{ let r = view(&A); let t = &r.tail; t[2] }");
    w.konst("HEAD", "u16", "{ let s = S { head: 9, next: 1 }; let h = &s.head; *h }");
    w.build();
    assert_eq!(eval_int(&mut w, "LEN"), Ok(3));
    assert_eq!(eval_int(&mut w, "LAST"), Ok(7));
    assert_eq!(eval_int(&mut w, "HEAD"), Ok(9));
}
