//! The `needs_drop` intrinsic in a constant: the pinned core, library/core/src/mem/mod.rs,
//! `pub const fn needs_drop<T: ?Sized>() -> bool { const { intrinsics::needs_drop::<T>() } }`
//! (every `Vec`, `RawVec` and `Box` path of std reaches it, the owner's hello world among them),
//! and library/core/src/intrinsics/mod.rs: "Returns `true` if the actual type given as `T`
//! requires drop glue; returns `false` if the actual type provided for `T` implements `Copy`."
//! The Reference, "Destructors": "If `T: Drop`, calling `<T as core::ops::Drop>::drop`", then
//! "If `T` is a struct, drop each field", "If `T` is a tuple, drop each element", "If `T` is
//! an array, drop each element"; core's `ManuallyDrop<T>` (`#[lang = "manually_drop"]`)
//! "inhibits the compiler from automatically calling `T`'s destructor". An array of length 0
//! holds no element to drop.

use blaze_typeck::PNode;

use super::{eval_int, world};
use crate::Intrinsic;

/// `u8` and `[D; 0]` need no drop; `D` (a `Drop` impl), `(u8, D)` and `W { d: D }` (a part
/// that needs drop) and `[D; 2]` do; the `ManuallyDrop` ADT holding a `D` does not. Without
/// the intrinsic's arm every constant here is refused with "needs_drop is not evaluated yet";
/// a fix that ignored the parts would answer `PAIR` and `WRAP` `0`, one that ignored
/// `ManuallyDrop` would answer `MD` `1`.
/// Prints `test tests::needs_drop::needs_drop_follows_the_destructor_rule ... ok`.
#[test]
fn needs_drop_follows_the_destructor_rule() {
    let mut w = world();
    let b = w.fx.ty(PNode::Bool);
    w.intrinsic("needs_drop", 1, &[], b, Intrinsic::NeedsDrop);
    let d = w.strukt("D", &[("x", "u8")]);
    w.drop_impl_on(d);
    w.strukt("W", &[("a", "u8"), ("d", "D")]);
    let md = w.strukt("Md", &[("value", "D")]);
    w.manually_drop_is(md);
    w.konst("U8", "bool", "needs_drop::<u8>()");
    w.konst("DD", "bool", "needs_drop::<D>()");
    w.konst("PAIR", "bool", "needs_drop::<(u8, D)>()");
    w.konst("WRAP", "bool", "needs_drop::<W>()");
    w.konst("MD", "bool", "needs_drop::<Md>()");
    w.konst("ARR2", "bool", "needs_drop::<[D; 2]>()");
    w.konst("ARR0", "bool", "needs_drop::<[D; 0]>()");
    w.build();
    assert_eq!(eval_int(&mut w, "U8"), Ok(0), "`u8` has no destructor");
    assert_eq!(eval_int(&mut w, "DD"), Ok(1), "`D: Drop`");
    assert_eq!(eval_int(&mut w, "PAIR"), Ok(1), "a tuple drops each element: `D` needs drop");
    assert_eq!(eval_int(&mut w, "WRAP"), Ok(1), "a struct drops each field: `d: D` needs drop");
    assert_eq!(eval_int(&mut w, "MD"), Ok(0), "`ManuallyDrop` inhibits its value's destructor");
    assert_eq!(eval_int(&mut w, "ARR2"), Ok(1), "an array drops each element");
    assert_eq!(eval_int(&mut w, "ARR0"), Ok(0), "`[D; 0]` holds no element");
}
