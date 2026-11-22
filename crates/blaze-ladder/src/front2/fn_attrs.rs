//! A fn item's attributes that change typing ([`TypeAttrs`]), as the checker's
//! `World::attrs` asks them of a FN (not only of an ADT).
//!
//! Who asks: blaze-typeck's constant promotion (rules/promote.rs `promotable_callee`) asks the
//! attributes of every callee of a call under a shared borrow, `&f(..)`, to see whether it
//! carries `#[rustc_promotable]` ([`TypeAttrs::PROMOTABLE`]). The Reference, "Destructors",
//! "Constant promotion": "Promotion of a value expression to a `'static` slot occurs when the
//! expression could be written in a constant and borrowed ..". The pinned library marks
//! (library/core/src/task/wake.rs, nightly-2026-10-27, lines 197-207):
//! ```text
//!     #[rustc_promotable]
//!     #[stable(feature = "futures_api", since = "1.36.0")]
//!     #[rustc_const_stable(feature = "futures_api", since = "1.36.0")]
//!     pub const fn new(
//!         clone: unsafe fn(*const ()) -> RawWaker,
//!         ..
//!     ) -> Self {
//! ```
//! and the same on `RawWaker::new` (lines 52-56).
//!
//! STD-CENSUS-1 class 1 (48 of 57 std rungs, 2026-11-16): std's `panic_with_hook`
//! (library/std/src/panicking.rs line 773) writes
//! `default_hook(&PanicHookInfo::new(location, payload.get(), can_unwind, force_no_backtrace))`:
//! a shared borrow of a call of an INHERENT IMPL's fn (`impl<'a> PanicHookInfo<'a> { fn new }`,
//! library/std/src/panic.rs line 49). The world answered `attrs` for ADTs only: the fn's key is a
//! member key (keys.rs `member_key`, std's crate prefix), `Keys::library_in` answers `None` for a
//! member, and the answer was `Gap::Fill(FillError::UnknownKey(..))` ("the checker key .. is in
//! no row of the front end's key map"). Class 5 (s2-gen-01-dyn-traits, `println!("{}",
//! describe(&Sq(3.0)))`: `format_args!` borrows `describe(..)`) is the same question of a
//! MODULE-LEVEL fn, answered by wanting the fn as an ADT ("a fn (asked for as an ADT)").
//!
//! The answer is read once while the fn head is lowered and kept in [`FnHead::qualifiers`], so
//! a `#[cfg_attr(.., rustc_promotable)]` is resolved under its crate's cfg before the body asks
//! for it. No other attribute of a fn changes typing today; one that does is added here, beside
//! this one.

use blaze_typeck::TypeAttrs;

use super::heads::FnHead;

/// The attribute that marks a fn whose call may be promoted.
pub const PROMOTABLE: &str = "rustc_promotable";

/// The attribute that marks a compiler intrinsic ([`TypeAttrs::INTRINSIC`]): blaze-typeck's
/// rules/reify.rs refuses it as a fn pointer by name (core/src/intrinsics/mod.rs: "The actual
/// implementations live in the compiler."). `rustc_intrinsic_must_be_overridden` contains the
/// word, and the head's lowered qualifiers carry the answer.
pub const INTRINSIC: &str = "rustc_intrinsic";

/// The attribute that gates a fn on CPU features ([`TypeAttrs::TARGET_FEATURE`]): blaze-typeck's
/// rules/target_feature.rs makes a safe such fn's pointer `unsafe` outside its features (the
/// Reference, "The `target_feature` attribute"; library/stdarch/crates/core_arch/src/arm_shared/
/// neon/generated.rs line 2041, `#[target_feature(enable = "neon")] pub fn vaddq_u8`). A
/// `cfg_attr(target_feature = ..)` also contains the word; the head's lowered qualifiers decide.
pub const TARGET_FEATURE: &str = "target_feature";

/// The typing attributes of the fn whose head is `f`, already read while that head was made.
pub fn fn_type_attrs(f: &FnHead<'_>) -> TypeAttrs {
    f.qualifiers.typing()
}
