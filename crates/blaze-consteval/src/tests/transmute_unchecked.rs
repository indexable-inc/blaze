//! `transmute_unchecked` in constants: evaluated by layout exactly as `transmute`, with the
//! same checks (the sizes agree, the bytes read back are valid for the target type).
//!
//! The pinned core, library/core/src/intrinsics/mod.rs, on `transmute_unchecked`: "Like
//! [`transmute`], but even less checked at compile-time: rather than giving an error for
//! `size_of::<Src>() != size_of::<Dst>()`, it's **Undefined Behavior** at runtime." In a
//! constant that undefined behaviour is refused (`Refusal::TransmuteSize`), never evaluated.
//! core reaches it from `NonZero::new` (library/core/src/num/nonzero.rs: `unsafe {
//! intrinsics::transmute_unchecked(n) }`), so every `NonZero::<T>::MAX`/`MIN` constant calls
//! it (tempo build census 5: 25 fns refused "transmute_unchecked is not evaluated yet").
//!
//! Control: with the `Intrinsic::TransmuteUnchecked` arm taken out of machine.rs `intrinsic`
//! (it falls to `other => unsupported(..)`), `transmute_unchecked_reads_by_layout` fails with
//! left `Err("Unsupported { what: \"transmute_unchecked\" }")`, right `Ok(..)`; the same
//! constants through `transmute` pass either way.

use super::{eval_int, refusal, world};
use crate::{Intrinsic, Refusal};

/// The world of the suite with `transmute_unchecked<Src, Dst>(src: Src) -> Dst` declared.
fn world_unchecked() -> crate::fixture::World {
    let mut w = world();
    let (p0, p1) = (w.fx.param(0), w.fx.param(1));
    w.intrinsic("transmute_unchecked", 2, &[p0], p1, Intrinsic::TransmuteUnchecked);
    w
}

/// Plain data reinterpreted as `transmute` does it, and through a `const fn` as core's
/// `NonZero::new` calls it.
/// Prints `test tests::transmute_unchecked::transmute_unchecked_reads_by_layout ... ok`.
#[test]
fn transmute_unchecked_reads_by_layout() {
    let mut w = world_unchecked();
    w.const_fn("bits", &[("x", "f32")], "u32", "{ transmute_unchecked::<f32, u32>(x) }");
    w.konst("ARR", "u32", "transmute_unchecked::<[u8; 4], u32>([1, 2, 3, 4])");
    w.konst("ARR_CONTROL", "u32", "transmute::<[u8; 4], u32>([1, 2, 3, 4])");
    w.konst("BITS", "u32", "bits(1.0)");
    w.build();
    assert_eq!(eval_int(&mut w, "ARR"), Ok(0x0403_0201), "the bytes read back as the target type");
    assert_eq!(eval_int(&mut w, "ARR_CONTROL"), Ok(0x0403_0201), "the control: `transmute` gives the same");
    assert_eq!(eval_int(&mut w, "BITS"), Ok(u128::from(1f32.to_bits())), "through a `const fn`");
}

/// The checks are `transmute`'s: different sizes, an invalid `bool`, are refused by name.
/// Prints `test tests::transmute_unchecked::transmute_unchecked_checks_as_transmute ... ok`.
#[test]
fn transmute_unchecked_checks_as_transmute() {
    let mut w = world_unchecked();
    w.konst("TS", "u16", "transmute_unchecked::<u8, u16>(1)");
    w.konst("IB", "bool", "transmute_unchecked::<u8, bool>(2)");
    w.build();
    assert_eq!(refusal(&mut w, "TS"), std::format!("{:?}", Refusal::TransmuteSize { from: 1, to: 2 }));
    assert_eq!(refusal(&mut w, "IB"), std::format!("{:?}", Refusal::InvalidBool { value: 2 }));
}
