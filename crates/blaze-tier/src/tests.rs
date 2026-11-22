//! Piece 0 holds types; these tests pin the data in it (the arm64 description) and the two small
//! helpers a later piece would otherwise get wrong silently.

extern crate std;

use std::format;

use blaze_own::Alloc;

use crate::{ARM64, Emitted, Live, LoopHeader, Place, Reg, RegSet, TierError, TierName};

/// The arm64 description's sets do not overlap where they must not: nothing allocatable is
/// scratch, an argument register, x18 (Apple's platform register), x29, x30 or 31 (sp/xzr).
/// Expected: passes. Fails if a mask is mistyped (each bit is named here).
#[test]
fn arm64_sets_are_disjoint() {
    assert!(ARM64.allocatable.0 & ARM64.scratch.0 == 0, "allocatable and scratch overlap");
    for reg in [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 18, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39] {
        assert!(!ARM64.allocatable.contains(Reg(reg)), "register {reg} must not be allocatable");
    }
    for reg in [9u8, 15, 19, 27, 28, 40, 47, 63] {
        assert!(ARM64.allocatable.contains(Reg(reg)), "register {reg} must be allocatable");
    }
    assert!(ARM64.scratch == RegSet::EMPTY.with(Reg(16)).with(Reg(17)));
    assert!(ARM64.run_context.is_none(), "D100 point 3: no reserved register");
    for &(a, b) in ARM64.save_pairs {
        assert!(ARM64.callee_saved.contains(a) && ARM64.callee_saved.contains(b), "pair {a:?} {b:?}");
    }
    assert_eq!(ARM64.allocatable.of(crate::RegClass::Gpr).len(), 17);
    assert_eq!(ARM64.allocatable.of(crate::RegClass::Fpr).len(), 24);
}

/// A loop header whose range runs past `live` gives `None`, never a shorter slice.
/// Expected: passes. Fails if `live_of` clamps.
#[test]
fn live_of_refuses_a_range_past_the_list() {
    let mut out = Emitted::new_in(Alloc::global());
    out.live.push(Live { value: 3, place: Place::Reg(Reg(19)) });
    assert_eq!(out.live_of(LoopHeader { at: 0, first: 0, count: 1 }).map(<[Live]>::len), Some(1));
    assert!(out.live_of(LoopHeader { at: 0, first: 0, count: 2 }).is_none());
    assert!(out.live_of(LoopHeader { at: 0, first: u32::MAX, count: u32::MAX }).is_none());
}

/// Every error names its tier. Expected: "the fast tier refused instruction 7: a vector shuffle".
#[test]
fn errors_name_the_tier() {
    let error = TierError::Refused { tier: TierName::Fast, inst: Some(7), what: "a vector shuffle" };
    assert_eq!(format!("{error}"), "the fast tier refused instruction 7: a vector shuffle");
}
