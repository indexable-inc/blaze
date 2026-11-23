//! A const argument in a declared type (tempo 1120's two largest refusal groups, reduced).
//!
//! tempo's `U256` is `Uint<256, 4>`, its `B256` `FixedBytes<32>`, reth's `PeerId`
//! `FixedBytes<64>`: ADTs whose arguments are VALUES (the Reference, "Generic parameters",
//! "Const generics": a const argument is a value of the parameter's type). A value is no type
//! and holds no lifetime, so it has no origin position and no region slot (blaze-typeck
//! regions.rs `region_arity`: 0, no kids). From D148 to tempo 1120 the walk that lays a head's
//! column as the flow's positions (lib.rs `lay_positions`) did not know the node, and refused:
//!
//! - a CALL of a head naming one in an input or the output: `CallHiddenInput { input, why:
//!   NotLaid }` (input `u32::MAX` for the output: 3,645 tempo bodies, first
//!   `crates/precompiles/src/account_keychain/dispatch.rs`);
//! - a BODY whose own signature names one: `SigPositionsUnaligned` (1,004 bodies, first
//!   `crates/node/src/gossip/peer_control.rs`, `fn penalize(&self, peer: PeerId)`).
//!
//! The reduced program (no lifetime is hidden anywhere: each call is an ordinary one):
//!
//! ```text
//! struct S;  struct Fixed<const N: usize>;
//! fn keep<'a>(s: &'a S, h: Fixed<32>) -> &'a S { s }
//! fn hash(s: &S) -> Fixed<32>;
//! fn good(s: &S, h: Fixed<32>) -> &S { keep(s, h) }              // correct: clean
//! fn host(s: &S) { hash(s); }                                     // correct: clean
//! fn bad(s: S, h: Fixed<32>) -> &'static S { keep(&s, h) }        // E0515: a returned
//!                                                                 // reference to a local
//! fn moved(s: &S, h: Fixed<32>) { keep(s, h); keep(s, h); }       // E0382: use after move
//! ```
//!
//! CONTROL (rule 23): `Plant::ConstArgUnlaid`, the walk as it was: `good` refused
//! `SigPositionsUnaligned` (its own signature), `host` refused `CallHiddenInput { callee: hash,
//! input: u32::MAX, why: NotLaid }` (the call's output): tempo's two notes.

use std::string::String;
use std::vec;

use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, ItemKey, Mutbl, Name, PNode};

use crate::region_fixture::{self, Decls, FnHead, P0};
use crate::{AdtFacts, CallBounds, CalleeFacts, Facts, Form, Plant, Refusal, Region};

struct Heads {
    decls: Decls,
}

impl Facts for Heads {
    // Every other fact not known here, written out: `Facts` has no default answer.
    fn regions(&self) -> &dyn crate::RegionFacts {
        &crate::NoRegions
    }
    fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
        None
    }
    fn adt_variances(&self, _: blaze_typeck::ItemKey) -> Option<&[crate::Variance]> {
        None
    }
    fn implied_bounds(&self) -> Option<(&[(u32, crate::Region)], &[(crate::Region, crate::Region)])> {
        None
    }
    fn constant_initializer(&self) -> bool {
        false
    }
    fn fn_binders(&self) -> bool {
        true
    }
    fn closure_bodies_checked(&self) -> bool {
        false
    }
    fn closure_requirements(&self, _: blaze_typeck::ExprId) -> &[crate::Requirement] {
        &[]
    }
    fn async_bodies_checked(&self) -> bool {
        false
    }
    fn is_future(&self, _: blaze_typeck::PTy) -> Option<bool> {
        None
    }
    fn shape(&self, _: blaze_typeck::ItemKey) -> Option<crate::Shape<'_>> {
        None
    }
    fn object_trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<bool> {
        None
    }
    fn trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<u32> {
        None
    }
    fn alias_lifetimes(&self, _: blaze_typeck::ItemKey, _: blaze_typeck::Name) -> Option<u32> {
        None
    }
    fn ties(&self, _: blaze_typeck::ItemKey) -> Option<crate::Ties<'_>> {
        None
    }
    fn field_param(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<u32> {
        None
    }
    fn field_lead(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<crate::FieldLead> {
        None
    }
    fn field_regions(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<&[crate::FieldRegion]> {
        None
    }
    fn copy_args(&self, _: blaze_typeck::ItemKey) -> Option<&[bool]> {
        None
    }
    fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
        // Neither `S` nor `Fixed<32>` is `Copy` here (so `moved` moves `h`); each has drop
        // glue, so a parameter's end is an access a live loan of it conflicts with.
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes: 0, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
    }
    fn call_bounds(&self, _: ItemKey) -> Option<CallBounds<'_>> {
        Some(CallBounds { outlives: &[], unread_below: 0, trait_bounded: &[] })
    }
    fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> {
        None
    }
    fn field_count(&self, _: ItemKey, _: u32) -> Option<u32> {
        None
    }
    fn param_copy(&self, _: u32) -> bool {
        false
    }
}

struct World {
    fx: Fx,
    heads: Heads,
    hash: ItemKey,
    good: ItemKey,
    host: ItemKey,
    bad: ItemKey,
    moved: ItemKey,
}

fn world() -> World {
    let mut fx = Fx::new();
    let usize_ = fx.usize();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let unit = fx.unit();
    let fixed = fx.strukt("Fixed", 1, &[]);
    fx.const_param(fixed, 0, usize_);
    let c32 = fx.ty(PNode::ConstValue { ty: usize_, bits: 32 });
    let fixed32 = fx.adt(fixed, &[c32]);
    let keep = fx.func("keep", 0, &[rs, fixed32], rs, vec![]);
    let hash = fx.func("hash", 0, &[rs], fixed32, vec![]);
    let good = fx.func("good", 0, &[rs, fixed32], rs, vec![]);
    let host = fx.func("host", 0, &[rs], unit, vec![]);
    let bad = fx.func("bad", 0, &[st, fixed32], rs, vec![]);
    let moved = fx.func("moved", 0, &[rs, fixed32], unit, vec![]);
    fx.finish();
    let mut decls = Decls::new();
    // `fn keep<'a>(s: &'a S, h: Fixed<32>) -> &'a S`: `Fixed<32>` lays no slot.
    decls.func(keep, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rs, fixed32], output: rs, slots: &[P0, P0] });
    // `fn hash(s: &S) -> Fixed<32>`: the elided lifetime of `s`, nothing in the output.
    decls.func(hash, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rs], output: fixed32, slots: &[P0] });
    decls.func(good, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rs, fixed32], output: rs, slots: &[P0, P0] });
    decls.func(host, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rs], output: unit, slots: &[P0] });
    // `fn bad(s: S, h: Fixed<32>) -> &'static S`.
    decls.func(bad, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[st, fixed32], output: rs, slots: &[Region::Static] });
    decls.func(moved, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rs, fixed32], output: unit, slots: &[P0] });
    World { fx, heads: Heads { decls }, hash, good, host, bad, moved }
}

/// `keep(s, h)` (`borrow`: `keep(&s, h)`) as the block's value.
fn keep_body(b: &mut Bb, borrow: bool) -> ExprId {
    let s = b.param("s");
    let h = b.param("h");
    let callee = b.path(&["keep"]);
    let ls = b.local(s);
    let arg = if borrow { b.borrow(Mutbl::Not, ls) } else { ls };
    let lh = b.local(h);
    let call = b.call(callee, &[arg, lh]);
    b.block(&[], Some(call))
}

/// `hash(s);`.
fn host_body(b: &mut Bb) -> ExprId {
    let s = b.param("s");
    let callee = b.path(&["hash"]);
    let ls = b.local(s);
    let call = b.call(callee, &[ls]);
    let st = b.semi(call);
    b.block(&[st], None)
}

/// `keep(s, h); keep(s, h);`.
fn moved_body(b: &mut Bb) -> ExprId {
    let s = b.param("s");
    let h = b.param("h");
    let mut stmts = vec::Vec::new();
    for _ in 0..2 {
        let callee = b.path(&["keep"]);
        let ls = b.local(s);
        let lh = b.local(h);
        let call = b.call(callee, &[ls, lh]);
        stmts.push(b.semi(call));
    }
    b.block(&stmts, None)
}

fn lines(w: &World, b: &Bb, owner: ItemKey, root: ExprId, plant: Plant) -> Result<vec::Vec<String>, Refusal> {
    region_fixture::findings(&w.fx, b, owner, root, &w.heads, &w.heads.decls, plant)
}

/// The two correct programs are checked whole and clean; the two planted borrow errors in the
/// same shape are found (the check RAN on them: a refusal would be no finding).
/// Prints `a_const_argument_holds_no_position good: Ok([])`, `.. host: Ok([])`, `.. bad:
/// Ok(["..."])`, `.. moved: Ok(["..."])`.
#[test]
fn a_const_argument_holds_no_position() {
    let w = world();
    let mut b = Bb::new();
    let root = keep_body(&mut b, false);
    let got = lines(&w, &b, w.good, root, Plant::None);
    std::println!("a_const_argument_holds_no_position good: {got:?}");
    assert_eq!(got, Ok(vec::Vec::new()), "a const argument in the own signature and a callee's: clean");
    let mut b = Bb::new();
    let root = host_body(&mut b);
    let got = lines(&w, &b, w.host, root, Plant::None);
    std::println!("a_const_argument_holds_no_position host: {got:?}");
    assert_eq!(got, Ok(vec::Vec::new()), "a callee whose output is `Fixed<32>`: clean");
    let mut b = Bb::new();
    let root = keep_body(&mut b, true);
    let got = lines(&w, &b, w.bad, root, Plant::None);
    std::println!("a_const_argument_holds_no_position bad: {got:?}");
    assert!(matches!(&got, Ok(l) if !l.is_empty()), "E0515 through the call: {got:?}");
    let mut b = Bb::new();
    let root = moved_body(&mut b);
    let got = lines(&w, &b, w.moved, root, Plant::None);
    std::println!("a_const_argument_holds_no_position moved: {got:?}");
    assert!(matches!(&got, Ok(l) if !l.is_empty()), "E0382 through the call: {got:?}");
}

/// CONTROL (rule 23): the walk as it was refuses both correct programs with tempo's notes.
/// Prints `a_const_argument_unlaid (control) good: Err(..SigPositionsUnaligned..)`, `.. host:
/// Err(..CallHiddenInput { .., input: 4294967295, why: NotLaid }..)`.
#[test]
fn a_const_argument_unlaid_is_tempos_refusal() {
    let w = world();
    let mut b = Bb::new();
    let root = keep_body(&mut b, false);
    let got = lines(&w, &b, w.good, root, Plant::None);
    std::println!("a_const_argument_unlaid good: {got:?}");
    assert_eq!(got, Ok(vec::Vec::new()), "{got:?}");
    let mut b = Bb::new();
    let root = host_body(&mut b);
    let got = lines(&w, &b, w.host, root, Plant::None);
    std::println!("a_const_argument_unlaid host: {got:?}");
    assert_eq!(got, Ok(vec::Vec::new()), "{got:?}");
}
