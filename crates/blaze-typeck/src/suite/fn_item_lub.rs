//! Two different fn items of one signature meet at their fn pointer (rules/coerce_lub.rs; the
//! Reference, "Type coercions", "Least upper bound coercions", and "Function item types"). The
//! shape is std's `RwLock::lock_contended` (library/std/src/sys/sync/rwlock/queue.rs:362):
//! `let update_fn = if write { write_lock } else { read_lock };`, both `fn(State) ->
//! Option<State>`. The fixture has no `if` builder; a `match` with two arms is the same
//! coercion site with several sources (`CoerceMany`), checked by the same `coerce_many_push`.
//!
//! ```text
//! fn write_lock(s: u8) -> u8;
//! fn read_lock(s: u8) -> u8;
//! fn other(s: u8) -> u16;
//! fn main();
//! ```
//!
//! Planted bug: `Plant::FnLubOff` (the lub's fn-item case is skipped): caught by
//! `control_without_the_lub_the_second_arm_is_e0308`, which gives the E0308 the owner's hello
//! world stopped on ("found FnDef(..), expected FnDef(..)", text `{ read_lock }`).

use std::vec::Vec;

use super::{adjusts, check_body};
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx, show};
use crate::ids::{ExprId, ItemKey, PatId, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

fn world() -> (Fx, ItemKey) {
    let mut world = Fx::new();
    let fx = &mut world;
    let u8_ = fx.ty(PNode::Uint(UintTy::U8));
    let u16_ = fx.ty(PNode::Uint(UintTy::U16));
    let unit = fx.unit();
    fx.func("write_lock", 0, &[u8_], u8_, Vec::new());
    fx.func("read_lock", 0, &[u8_], u8_, Vec::new());
    fx.func("other", 0, &[u8_], u16_, Vec::new());
    let main = fx.func("main", 0, &[], unit, Vec::new());
    fx.finish();
    (world, main)
}

/// What one case gave.
struct Got {
    diags: Vec<(DiagCode, u32)>,
    /// The adjustments of the first arm, then of the second.
    first: std::string::String,
    second: std::string::String,
    /// The type of `f`.
    f: std::string::String,
}

/// `fn main() { let f = match true { x => FIRST, y => SECOND }; }`. Expressions: 0 `true`,
/// 1 FIRST, 2 SECOND, 3 the match, 4 the block; bindings 0 `x`, 1 `y`, 2 `f`.
fn case(first: &str, second: &str, plant: Plant) -> Got {
    let (fx, main) = world();
    let mut b = Bb::new();
    let scrutinee = b.bool();
    let a0 = b.path(&[first]);
    let a1 = b.path(&[second]);
    let x = b.bind("x");
    let y = b.bind("y");
    let m = b.match_(scrutinee, &[(x, a0), (y, a1)]);
    let f = b.bind("f");
    let s0 = b.let_(f, None, m);
    let root = b.block(&[s0], None);
    assert_eq!((a0, a1, f), (ExprId(1), ExprId(2), PatId(2)), "the case's numbering");
    let c = check_body(&fx, &b, main, root, plant, InferPlant::None);
    Got {
        diags: c.diags.iter().map(|d| (d.code, d.at.start)).collect(),
        first: adjusts(&fx, &c, a0),
        second: adjusts(&fx, &c, a1),
        f: show(&fx, c.types.view(), c.local_ty[f.0 as usize]),
    }
}

/// `write_lock` and `read_lock` are two types of one signature: both reify, and `f` is the
/// pointer. Without the fix: one E0308 (IncompatibleArms) at expression 2.
#[test]
fn two_fn_items_of_one_signature_meet_at_their_pointer() {
    let g = case("write_lock", "read_lock", Plant::None);
    assert_eq!(g.diags, Vec::new());
    assert_eq!(g.first, "ReifyFnPointer -> fn(u8) -> u8");
    assert_eq!(g.second, "ReifyFnPointer -> fn(u8) -> u8");
    assert_eq!(g.f, "fn(u8) -> u8");
}

/// The control: with the lub's fn-item case planted off, the case above is today's E0308 at
/// the second arm, and nothing reifies.
#[test]
fn control_without_the_lub_the_second_arm_is_e0308() {
    let g = case("write_lock", "read_lock", Plant::FnLubOff);
    assert_eq!(g.diags, std::vec![(DiagCode::IncompatibleArms, 2)]);
    assert_eq!(g.first, "");
    assert_eq!(g.second, "");
}

/// Two signatures have no lub: `fn(u8) -> u8` and `fn(u8) -> u16` stay E0308 (rustc: "`match`
/// arms have incompatible types"), and nothing reifies.
#[test]
fn two_signatures_have_no_lub() {
    let g = case("write_lock", "other", Plant::None);
    assert_eq!(g.diags, std::vec![(DiagCode::IncompatibleArms, 2)]);
    assert_eq!(g.first, "");
    assert_eq!(g.second, "");
}

/// One fn item twice is one type: it stays the fn item, and nothing is reified.
#[test]
fn one_fn_item_twice_stays_itself() {
    let g = case("write_lock", "write_lock", Plant::None);
    assert_eq!(g.diags, Vec::new());
    assert_eq!(g.first, "");
    assert_eq!(g.second, "");
    assert_eq!(g.f, "fn write_lock");
}
