//! Bodies written in blaze-body's text form, projected: the flow each becomes, and what the
//! move and borrow checks find in it, reported at body rows. These need no lowering (P2).

use std::alloc::Global;
use std::string::String;

use blaze_body as bd;
use blaze_body::{Body, Proj, verify};
use blaze_flow::{BorrowFinding, DropKind, MoveFindingKind};

use crate::{ProjectError, Projected, canon, project};

fn parse(text: &str) -> Body<Global> {
    bd::parse(text, Global).unwrap_or_else(|e| panic!("{e:?} in:\n{text}"))
}

fn projected(body: &Body<Global>) -> Projected<Global> {
    let v = verify(body).unwrap_or_else(|e| panic!("verify refused {e:?}"));
    project(v, Global).unwrap_or_else(|e| panic!("project refused: {e}"))
}

fn flow_text(p: &Projected<Global>) -> String {
    let mut out = String::new();
    p.flow.print(&mut out).expect("a String takes every write");
    out
}

fn canon_text(p: &Projected<Global>) -> String {
    let mut out = String::new();
    canon(&p.flow, &mut out, Global).expect("a String takes every write");
    out
}

/// Straight-line code with a constant, a borrow, its origin and a subset to a placeholder (so
/// it does NOT check clean: [`CLEAN`] says why).
const STRAIGHT: &str = "\
local _0: t0 return
local _1: t0 arg 0
local _2: t0 temp e3 value mut
local _3: t1 temp e4 borrow
const c0 = scalar 0x2: t0
origin 'o0 placeholder
origin 'o1 placeholder
origin 'o2
known 'o0: 'o1
mentions _3 'o2
bb0:
  live _2 @0..5
  _2 = add(copy _1, const c0) @0..5
  _3 = borrow shared _2 L0 'o2 @6..8 ; 'o2: 'o0
  _0 = move _2
  dead _2
  return
";

/// Prints the flow `STRAIGHT` becomes: the addition an aggregate of its operands, the constant
/// without its value, the borrow with its loan, origin and subset.
#[test]
fn straight_line_projects_row_for_row() {
    let p = projected(&parse(STRAIGHT));
    let want = "\
bb0:
  s0 live _2
  s1 _2 = (copy _1, const)
  s2 _3 = &_2 [L0 in 'o2]; 'o2: 'o0
  s3 _0 = move _2
  s4 dead _2
  s5 return
";
    assert_eq!(flow_text(&p), want);
    assert_eq!(p.flow.placeholder.as_slice(), &[true, true, false]);
    assert_eq!(p.flow.known.as_slice(), &[(blaze_flow::Origin(0), blaze_flow::Origin(1))]);
    for s in 0..6 {
        assert_eq!(p.flow_stmt(bd::Stmt(s)), Some(blaze_flow::Stmt(s)));
        assert_eq!(p.body_stmt(blaze_flow::Stmt(s)), Some(bd::Stmt(s)));
    }
    assert_eq!(p.flow_loan(bd::Loan(0)), Some(blaze_flow::Loan(0)));
}

/// `STRAIGHT` without the subset `'o2: 'o0`. With it the body is NOT clean: `'o0` is a
/// placeholder (a universal region, live at every point and past the body), so the loan `L0`
/// of `_2` is live from its borrow on, and `_0 = move _2` and `dead _2` invalidate it (E0505
/// and E0597 under RFC 2094's rules; the checker found exactly those two, run of 2026-11-17).
/// Without the subset `'o2` is live only where `_3` is, and `_3` is never used.
const CLEAN: &str = "\
local _0: t0 return
local _1: t0 arg 0
local _2: t0 temp e3 value mut
local _3: t1 temp e4 borrow
const c0 = scalar 0x2: t0
origin 'o0 placeholder
origin 'o1 placeholder
origin 'o2
known 'o0: 'o1
mentions _3 'o2
bb0:
  live _2 @0..5
  _2 = add(copy _1, const c0) @0..5
  _3 = borrow shared _2 L0 'o2 @6..8
  _0 = move _2
  dead _2
  return
";

/// Prints nothing: a body with no error has no move or borrow finding, and the record carries
/// the proof.
#[test]
fn clean_body_has_no_findings() {
    let p = projected(&parse(CLEAN));
    let r = blaze_flow::check(&p.flow, Global).expect("within bounds");
    assert!(r.moves.is_empty(), "{:?}", r.moves);
    assert!(r.borrows.is_empty(), "{:?}", r.borrows);
    assert!(r.proof.is_some());
}

/// Prints `UseOfMoved at s1`: the second move of `_1`, found in the flow and named at its body
/// row. A projection that dropped the first move (or mapped rows wrongly) finds nothing or
/// names another row. `_0` is written before `return` (a `return` reads `_0`: a body that
/// never writes it is a use of an uninitialized place, a second finding, as rustc's MIR
/// borrowck would report).
#[test]
fn use_after_move_is_found_at_its_body_row() {
    let p = projected(&parse(
        "\
local _0: t0 return
local _1: t1 arg 0
local _2: t1 temp e0 value
local _3: t1 temp e1 value
const c0 = scalar 0x2: t0
bb0:
  _2 = move _1
  _3 = move _1
  _0 = const c0
  return
",
    ));
    let r = blaze_flow::check(&p.flow, Global).expect("within bounds");
    let [f] = r.moves.as_slice() else { panic!("one finding wanted: {:?}", r.moves) };
    assert_eq!(f.kind, MoveFindingKind::UseOfMoved);
    assert_eq!(p.body_stmt(f.at.stmt()), Some(bd::Stmt(1)));
    assert_eq!(p.body_local(p.flow.place(f.place).local), Some(bd::Local(1)));
}

/// Prints `Invalidated L0 at s1`: a second `&mut _1` while the first is used after it.
#[test]
fn conflicting_borrow_is_found_at_its_body_row() {
    let p = projected(&parse(
        "\
local _0: t0 return
local _1: t0 arg 0
local _2: t1 temp e0 borrow
local _3: t1 temp e1 borrow
origin 'o0
origin 'o1
mentions _2 'o0
mentions _3 'o1
bb0:
  _2 = borrow mut _1 L0 'o0
  _3 = borrow mut _1 L1 'o1
  _0 = copy _2.*
  return
",
    ));
    let r = blaze_flow::check(&p.flow, Global).expect("within bounds");
    let found = r.borrows.iter().any(|f| match *f {
        BorrowFinding::Invalidated { at, loan, .. } => {
            p.body_loan(loan) == Some(bd::Loan(0)) && p.body_stmt(at.stmt()) == Some(bd::Stmt(1))
        }
        BorrowFinding::UnprovenOutlives { .. } => false,
    });
    assert!(found, "L0 invalidated at s1 wanted: {:?}", r.borrows);
}

/// A call whose unwind runs a cleanup path (blaze-body's own `UNWIND`).
const UNWIND: &str = "\
local _0: t0 return
local _1: t1 arg 0
local _2: t2 temp e4 value
bb0:
  live _2
  _2 = copy _1.0:t2
  call fn k7<0,0>(move _2) -> _0 then bb1 unwind bb2 @3..9
bb1:
  dead _2
  drop _1 then bb3 unwind continue
bb2 cleanup:
  drop _1 then bb4 unwind terminate
bb3:
  return
bb4 cleanup:
  resume
";

/// Prints the canonical text below: the call and the drop each a statement and a `goto` (joined
/// away by `canon`), both cleanup blocks left out; the drop of `_1` is `Static`.
#[test]
fn cleanup_blocks_are_left_out_and_drops_read_back() {
    let p = projected(&parse(UNWIND));
    let want = "\
args 1
bb0:
  live _2
  _2 = copy _1.0
  _0 = call(move _2)
  dead _2
  drop _1
  return
placeholders
mentions
known
";
    assert_eq!(canon_text(&p), want);
    assert_eq!(p.left_out(), &[bd::Block(2), bd::Block(4)]);
    assert_eq!(p.flow_block(bd::Block(3)), Some(blaze_flow::Block(2)));
    assert_eq!(p.flow_block(bd::Block(2)), None);
    assert_eq!(p.flow_stmt(bd::Stmt(5)), None);
    let kinds = p.drop_kinds(Global).expect("within bounds");
    assert_eq!(kinds.as_slice(), &[(bd::Stmt(4), DropKind::Static)]);
}

/// Prints `switch _1 -> bb1 bb2`: a discriminant read and the switch on it are one flow row,
/// the switch on the matched place, as blaze-flow-lower makes it.
#[test]
fn discriminant_and_switch_are_one_flow_row() {
    let p = projected(&parse(
        "\
local _0: t0 return
local _1: t1 arg 0
local _2: t0 temp e0 value
bb0:
  _2 = discr _1
  switch copy _2 [0 -> bb1] else bb2
bb1:
  return
bb2:
  return
",
    ));
    let want = "\
args 1
bb0:
  switch _1 -> bb1 bb2
bb1:
  return
bb2:
  return
placeholders
mentions
known
";
    assert_eq!(canon_text(&p), want);
    assert_eq!(p.flow_stmt(bd::Stmt(0)), p.flow_stmt(bd::Stmt(1)));
}

fn refused(text: &str) -> ProjectError {
    let body = parse(text);
    let v = verify(&body).unwrap_or_else(|e| panic!("verify refused {e:?}"));
    match project(v, Global) {
        Ok(p) => panic!("projected:\n{}", flow_text(&p)),
        Err(e) => e,
    }
}

/// Prints `s0: an `asm!` has no flow form`.
#[test]
fn asm_is_refused_by_name() {
    let e = refused("local _0: t0 return\nbb0:\n  asm 0() unwind continue\n");
    assert_eq!(e, ProjectError::InlineAsm { stmt: bd::Stmt(0) });
}

/// Prints `pN: a constant index or subslice of a slice pattern has no flow form`.
#[test]
fn subslice_is_refused_by_name() {
    let text = "\
local _0: t0 return
local _1: t1 arg 0
local _2: t1 temp e0 value
bb0:
  _2 = copy _1[0..1]
  return
";
    let body = parse(text);
    let v = verify(&body).expect("verifies");
    let ProjectError::SlicePlace { place } = refused(text) else { panic!("SlicePlace wanted") };
    assert!(matches!(v.place(place).proj, Some(Proj::Subslice { .. })));
}

/// Prints `s0: a discriminant read that the next row does not switch on`.
#[test]
fn lone_discriminant_is_refused_by_name() {
    let e = refused(
        "\
local _0: t0 return
local _1: t1 arg 0
bb0:
  _0 = discr _1
  return
",
    );
    assert_eq!(e, ProjectError::LoneDiscriminant { stmt: bd::Stmt(0) });
}

/// Prints `UseOfMoved at s1`: `deinit _1` is a move out of `_1` into the scratch local `_2`, so
/// the read after it uses a moved value.
#[test]
fn deinit_moves_the_place_out() {
    let p = projected(&parse(
        "\
local _0: t0 return
local _1: t0 arg 0
bb0:
  deinit _1
  _0 = move _1
  return
",
    ));
    let want = "\
bb0:
  s0 _2 = move _1
  s1 _0 = move _1
  s2 return
";
    assert_eq!(flow_text(&p), want);
    let r = blaze_flow::check(&p.flow, Global).expect("within bounds");
    let [f] = r.moves.as_slice() else { panic!("one finding wanted: {:?}", r.moves) };
    assert_eq!(f.kind, MoveFindingKind::UseOfMoved);
    assert_eq!(p.body_stmt(f.at.stmt()), Some(bd::Stmt(1)));
}

/// Prints no finding: `setdiscr _1 variant 1` is a write to `_1`, so the move after it reads an
/// initialized place.
#[test]
fn set_discriminant_writes_the_place() {
    let p = projected(&parse(
        "\
local _0: t0 return
local _1: t0 temp e0 value mut
bb0:
  setdiscr _1 variant 1
  _0 = move _1
  return
",
    ));
    let want = "\
bb0:
  s0 _1 = const
  s1 _0 = move _1
  s2 return
";
    assert_eq!(flow_text(&p), want);
    let r = blaze_flow::check(&p.flow, Global).expect("within bounds");
    assert!(r.moves.is_empty(), "{:?}", r.moves);
}
