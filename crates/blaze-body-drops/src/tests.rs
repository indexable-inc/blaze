//! Drop elaboration's tests, in blaze-body's text form: a body in, the elaborated body out,
//! compared as text; every output verifies, and a path checker (`first_bad_drop`) walks every
//! path of it to a bounded depth and finds no place dropped twice, dropped while moved out, or
//! dropped uninitialized. A planted bug (a flag not cleared at a move) is caught by that checker.
//!
//! Types are handles: t0 `()`, t1 `bool`, t2 a type with drop glue, t3 a struct of four t2
//! fields, t4 `u32`, t5 a type parameter.

use std::alloc::Global;
use std::format;
use std::println;
use std::string::String;
use std::vec;
use std::vec::Vec;

use blaze_body::body::PTy;
use blaze_body::{
    Block, Body, Callee, ConstKind, LocalKind, Operand, Place, Proj, Stmt, StmtKind, Unwind, Value, Verified,
    verify,
};

use crate::{DropFacts, DropState, Field, NeedsDrop, Part, PartState, Plant, elaborate_planted};

/// A table of answers: the state at each drop row, and the types that need no drop or are
/// generic (every other type needs drop).
struct Facts {
    at: &'static [(u32, DropState<'static>)],
    no_drop: &'static [u32],
    generic: &'static [u32],
}

impl DropFacts for Facts {
    fn at(&self, drop: Stmt) -> DropState<'_> {
        match self.at.iter().find(|(s, _)| *s == drop.0) {
            Some((_, d)) => *d,
            None => panic!("the table has no answer for the drop at {drop}"),
        }
    }

    fn needs_drop(&self, ty: PTy) -> NeedsDrop {
        if self.no_drop.contains(&ty.0) {
            NeedsDrop::No
        } else if self.generic.contains(&ty.0) {
            NeedsDrop::Generic
        } else {
            NeedsDrop::Yes
        }
    }

    fn bool_ty(&self) -> PTy {
        PTy(1)
    }
}

fn print(b: &Body<Global>) -> String {
    let mut out = String::new();
    blaze_body::print(&b.view(), &mut out).expect("a built body prints");
    out
}

fn test_alloc() -> impl core::alloc::Allocator + Copy {
    Global
}

fn try_run<F: DropFacts>(text: &str, facts: &F, plant: Plant) -> Result<Body<Global>, crate::ElabError> {
    let a = Default::default();
    let input = blaze_body::parse(text, a).unwrap_or_else(|e| panic!("{e:?} in:\n{text}"));
    let v = verify(&input).unwrap_or_else(|e| panic!("the input is refused: {e:?}:\n{text}"));
    elaborate_planted(v, facts, a, plant)
}

fn run(text: &str, facts: &Facts, plant: Plant) -> Body<Global> {
    let out = try_run(text, facts, plant).unwrap_or_else(|e| panic!("elaboration failed: {e:?}"));
    if let Err(e) = verify(&out) {
        panic!("the output is refused: {e:?}:\n{}", print(&out));
    }
    out
}

/// Elaborates `text`, which must give `want`, with no bad drop on any path.
fn check(text: &str, facts: &Facts, want: &str) {
    let out = run(text, facts, Plant::None);
    assert_eq!(print(&out), want);
    assert_eq!(first_bad_drop(&out), None, "in:\n{want}");
}

const UNINIT: u8 = 0;
const INIT: u8 = 1;
const MOVED: u8 = 2;
const DROPPED: u8 = 3;
const DEPTH: u32 = 64;

fn name(v: Verified<'_>, p: Place) -> String {
    let row = v.place(p);
    match (row.parent, row.proj) {
        (Some(up), Some(Proj::Field { index, .. })) => format!("{}.{index}", name(v, up)),
        (Some(up), Some(_)) => format!("{}.?", name(v, up)),
        _ => format!("{}", row.local),
    }
}

fn value_operands(v: Verified<'_>, value: Value) -> Vec<Operand> {
    match value {
        Value::Use(o) | Value::Unary { operand: o, .. } | Value::Cast { operand: o, .. } => vec![o],
        Value::Repeat { operand, .. } => vec![operand],
        Value::Binary { lhs, rhs, .. } | Value::CheckedBinary { lhs, rhs, .. } => vec![lhs, rhs],
        Value::Aggregate { operands, .. } => v.ops(operands).collect(),
        _ => vec![],
    }
}

/// Walks every path of `out` from its entry, `DEPTH` blocks deep, tracking each place's state
/// (initialized, moved out, dropped, uninitialized: moves.rs's transfer, with a place covering
/// its parts) and each drop flag's value (a switch on a known flag takes only its case). The
/// first drop of a place some part of which is not initialized is the answer.
fn first_bad_drop(out: &Body<Global>) -> Option<String> {
    let v = verify(out).expect("the output verifies");
    let places = v.places();
    let under = move |p: Place| (0..places).map(Place).filter(move |&q| v.is_prefix(p, q));
    let set = |st: &mut Vec<u8>, p: Place, to: u8| {
        for q in under(p) {
            st[q.index()] = to;
        }
    };
    let moved = |st: &mut Vec<u8>, o: Operand| {
        if let Operand::Move(p) = o {
            set(st, p, MOVED);
        }
    };
    let mut start = vec![UNINIT; places as usize];
    for q in 0..places {
        let l = v.place(Place(q)).local.0;
        if l >= 1 && l <= v.args() {
            start[q as usize] = INIT;
        }
    }
    let mut stack = vec![(Block(0), start, vec![None::<bool>; v.locals() as usize], 0u32)];
    while let Some((block, mut st, mut fl, depth)) = stack.pop() {
        if depth >= DEPTH {
            continue;
        }
        let row = v.block(block);
        for s in row.stmts.start..row.stmts.end - 1 {
            match v.stmt(Stmt(s)).kind {
                StmtKind::Assign { dest, value } => {
                    for o in value_operands(v, value) {
                        moved(&mut st, o);
                    }
                    set(&mut st, dest, INIT);
                    let l = v.place(dest).local;
                    if let Value::Use(Operand::Const(c)) = value
                        && let LocalKind::DropFlag(_) = v.local(l).kind
                        && let ConstKind::Scalar { bits } = v.konst(c).kind
                    {
                        fl[l.index()] = Some(bits != 0);
                    }
                }
                StmtKind::StorageLive(l) | StmtKind::StorageDead(l) => set(&mut st, v.local_place(l), UNINIT),
                _ => {}
            }
        }
        let t = v.terminator(block);
        let next = depth + 1;
        match v.stmt(t).kind {
            StmtKind::Goto(b) => stack.push((b, st, fl, next)),
            StmtKind::Switch { on, cases, otherwise } => {
                let known = match on {
                    Operand::Copy(p) if v.place(p).parent.is_none() => fl[v.place(p).local.index()],
                    _ => None,
                };
                let taken: Vec<Block> = match known {
                    Some(b) => vec![v.cases(cases).find(|c| c.0 == u128::from(b)).map_or(otherwise, |c| c.1)],
                    None => v.cases(cases).map(|c| c.1).chain([otherwise]).collect(),
                };
                for b in taken {
                    stack.push((b, st.clone(), fl.clone(), next));
                }
            }
            StmtKind::Call { callee, args, dest, target, unwind } => {
                if let Callee::Value(o) = v.callee(callee) {
                    moved(&mut st, o);
                }
                for o in v.ops(args) {
                    moved(&mut st, o);
                }
                if let Unwind::Cleanup(c) = unwind {
                    stack.push((c, st.clone(), fl.clone(), next));
                }
                if let (Some(d), Some(b)) = (dest, target) {
                    set(&mut st, d, INIT);
                    stack.push((b, st, fl, next));
                }
            }
            StmtKind::Drop { place, target, unwind } => {
                for q in under(place) {
                    let why = match st[q.index()] {
                        INIT => continue,
                        MOVED => "while moved out",
                        DROPPED => "twice",
                        _ => "uninitialized",
                    };
                    return Some(format!("{} dropped {why} at {t}", name(v, place)));
                }
                set(&mut st, place, DROPPED);
                if let Unwind::Cleanup(c) = unwind {
                    stack.push((c, st.clone(), fl.clone(), next));
                }
                stack.push((target, st, fl, next));
            }
            StmtKind::Assert { target, unwind, .. } => {
                if let Unwind::Cleanup(c) = unwind {
                    stack.push((c, st.clone(), fl.clone(), next));
                }
                stack.push((target, st, fl, next));
            }
            StmtKind::Suspend { resume, cancel } => {
                stack.push((cancel, st.clone(), fl.clone(), next));
                stack.push((resume, st, fl, next));
            }
            StmtKind::InlineAsm { target, unwind, .. } => {
                if let Unwind::Cleanup(c) = unwind {
                    stack.push((c, st.clone(), fl.clone(), next));
                }
                if let Some(b) = target {
                    stack.push((b, st, fl, next));
                }
            }
            _ => {}
        }
    }
    None
}

const ALWAYS: &str = "\
local _0: t0 return
local _1: t2 binding pat0
bb0:
  live _1
  call fn k1<0,0>() -> _1 then bb1 unwind continue
bb1:
  drop _1 then bb2 unwind continue
bb2:
  dead _1
  return
";

/// Prints "drops: a local dropped on every path keeps its one drop".
#[test]
fn a_local_dropped_on_every_path_keeps_its_drop() {
    let facts = Facts { at: &[(2, DropState::Init)], no_drop: &[], generic: &[] };
    check(ALWAYS, &facts, ALWAYS);
    println!("drops: a local dropped on every path keeps its one drop");
}

/// `let x = f(); if c { g(x) }; h();` with `h` unwinding: `x` is moved on one arm only.
const ONE_ARM: &str = "\
local _0: t0 return
local _1: t1 arg 0
local _2: t2 binding pat0
local _3: t0 temp e1 value
bb0:
  live _2
  call fn k1<0,0>() -> _2 then bb1 unwind continue
bb1:
  switch copy _1 [0 -> bb3] else bb2
bb2:
  call fn k2<0,0>(move _2) -> _3 then bb3 unwind continue
bb3:
  call fn k3<0,0>() -> _0 then bb4 unwind bb6
bb4:
  drop _2 then bb5 unwind continue
bb5:
  dead _2
  return
bb6 cleanup:
  drop _2 then bb7 unwind terminate
bb7 cleanup:
  resume
";

const ONE_ARM_FACTS: Facts = Facts { at: &[(5, DropState::Maybe), (8, DropState::Maybe)], no_drop: &[], generic: &[] };

/// Prints "drops: a local moved on one arm gets one flag, tested on both paths".
#[test]
fn a_local_moved_on_one_arm_gets_one_flag() {
    let want = "\
local _0: t0 return
local _1: t1 arg 0
local _2: t2 binding pat0
local _3: t0 temp e1 value
local _4: t1 flag _2 mut
const c0 = scalar 0x0: t1
const c1 = scalar 0x1: t1
bb0:
  _4 = const c0
  live _2
  _4 = const c0
  call fn k1<0,0>() -> _2 then bb8 unwind continue
bb8:
  _4 = const c1
  goto bb1
bb1:
  switch copy _1 [0 -> bb3] else bb2
bb2:
  _4 = const c0
  call fn k2<0,0>(move _2) -> _3 then bb3 unwind continue
bb3:
  call fn k3<0,0>() -> _0 then bb4 unwind bb6
bb4:
  switch copy _4 [0 -> bb5] else bb9
bb9:
  _4 = const c0
  drop _2 then bb5 unwind continue
bb5:
  dead _2
  _4 = const c0
  return
bb6 cleanup:
  switch copy _4 [0 -> bb7] else bb10
bb10 cleanup:
  _4 = const c0
  drop _2 then bb7 unwind terminate
bb7 cleanup:
  resume
";
    check(ONE_ARM, &ONE_ARM_FACTS, want);
    println!("drops: a local moved on one arm gets one flag, tested on both paths");
}

/// Prints "drops: a forgotten clear at the move is caught: _2 dropped while moved out".
#[test]
fn a_flag_kept_at_the_move_is_caught() {
    let out = run(ONE_ARM, &ONE_ARM_FACTS, Plant::KeepFlagAtMove);
    let found = first_bad_drop(&out).expect("the checker catches the planted bug");
    assert!(found.starts_with("_2 dropped while moved out at "), "{found}");
    println!("drops: a forgotten clear at the move is caught: {found}");
}

const ALL_PATHS: &str = "\
local _0: t0 return
local _1: t2 binding pat0
local _2: t0 temp e1 value
bb0:
  live _1
  call fn k1<0,0>() -> _1 then bb1 unwind continue
bb1:
  call fn k2<0,0>(move _1) -> _2 then bb2 unwind continue
bb2:
  drop _1 then bb3 unwind continue
bb3:
  dead _1
  return
";

/// Prints "drops: a local moved on every path loses its drop and has no flag".
#[test]
fn a_local_moved_on_every_path_loses_its_drop() {
    let facts = Facts { at: &[(3, DropState::Uninit)], no_drop: &[], generic: &[] };
    let want = ALL_PATHS.replace("  drop _1 then bb3 unwind continue\n", "  goto bb3\n");
    check(ALL_PATHS, &facts, &want);
    println!("drops: a local moved on every path loses its drop and has no flag");
}

const PARTIAL: &str = "\
local _0: t0 return
local _1: t3 binding pat0
local _2: t0 temp e1 value
bb0:
  live _1
  call fn k1<0,0>() -> _1 then bb1 unwind continue
bb1:
  call fn k2<0,0>(move _1.1:t2) -> _2 then bb2 unwind continue
bb2:
  drop _1 then bb3 unwind continue
bb3:
  dead _1
  return
";

/// Listed out of order: the pass orders them. A `const`, so `&PARTS` is promoted to `'static`.
const PARTS: [Part<'static>; 3] = [
    Part { path: &[Field { index: 3, ty: PTy(2) }], state: PartState::Init },
    Part { path: &[Field { index: 0, ty: PTy(2) }], state: PartState::Init },
    Part { path: &[Field { index: 2, ty: PTy(2) }], state: PartState::Init },
];

/// Prints "drops: a struct with field 1 moved drops fields 0, 2, 3 in order, each landing
/// dropping the rest".
#[test]
fn a_partly_moved_struct_drops_the_rest_in_order_with_a_ladder() {
    let facts = Facts { at: &[(3, DropState::Partial(&PARTS))], no_drop: &[], generic: &[] };
    let want = "\
local _0: t0 return
local _1: t3 binding pat0
local _2: t0 temp e1 value
bb0:
  live _1
  call fn k1<0,0>() -> _1 then bb1 unwind continue
bb1:
  call fn k2<0,0>(move _1.1:t2) -> _2 then bb2 unwind continue
bb2:
  drop _1.0:t2 then bb4 unwind bb6
bb4:
  drop _1.2:t2 then bb5 unwind bb7
bb5:
  drop _1.3:t2 then bb3 unwind continue
bb6 cleanup:
  drop _1.2:t2 then bb7 unwind terminate
bb7 cleanup:
  drop _1.3:t2 then bb8 unwind terminate
bb8 cleanup:
  resume
bb3:
  dead _1
  return
";
    check(PARTIAL, &facts, want);
    println!("drops: a struct with field 1 moved drops fields 0, 2, 3 in order, each landing dropping the rest");
}

/// `let a = f(); let b = g();` with `g` unwinding into the scope's cleanup, which the lowering
/// wrote un-elaborated as "drop b, drop a".
const TWO: &str = "\
local _0: t0 return
local _1: t2 binding pat0
local _2: t2 binding pat1
bb0:
  live _1
  call fn k1<0,0>() -> _1 then bb1 unwind continue
bb1:
  live _2
  call fn k2<0,0>() -> _2 then bb2 unwind bb4
bb2:
  drop _2 then bb3 unwind bb5
bb3:
  drop _1 then bb6 unwind continue
bb4 cleanup:
  drop _2 then bb5 unwind terminate
bb5 cleanup:
  drop _1 then bb7 unwind terminate
bb6:
  dead _2
  dead _1
  return
bb7 cleanup:
  resume
";

/// Prints "drops: the call's unwind edge reaches a cleanup that drops only the first local".
#[test]
fn a_call_between_two_locals_unwinds_dropping_only_the_first() {
    let facts = Facts {
        at: &[(4, DropState::Init), (5, DropState::Init), (6, DropState::Uninit), (7, DropState::Init)],
        no_drop: &[],
        generic: &[],
    };
    let want = TWO.replace("bb4 cleanup:\n  drop _2 then bb5 unwind terminate\n", "bb4 cleanup:\n  goto bb5\n");
    check(TWO, &facts, &want);
    println!("drops: the call's unwind edge reaches a cleanup that drops only the first local");
}

const NO_DROP: &str = "\
local _0: t0 return
local _1: t4 binding pat0
local _2: t5 binding pat1
bb0:
  live _1
  live _2
  call fn k1<0,0>() -> _1 then bb1 unwind continue
bb1:
  call fn k2<0,0>() -> _2 then bb2 unwind continue
bb2:
  drop _2 then bb3 unwind continue
bb3:
  drop _1 then bb4 unwind continue
bb4:
  dead _2
  dead _1
  return
";

/// Prints "drops: a type that needs no drop loses its drop; a generic one keeps it".
#[test]
fn a_type_that_needs_no_drop_loses_its_drop() {
    let facts = Facts { at: &[(4, DropState::Init), (5, DropState::Init)], no_drop: &[4], generic: &[5] };
    let want = NO_DROP.replace("  drop _1 then bb4 unwind continue\n", "  goto bb4\n");
    check(NO_DROP, &facts, &want);
    println!("drops: a type that needs no drop loses its drop; a generic one keeps it");
}

/// Non-field projections (`Deref`, `DerefBox`, `Index`, `ConstIndex`, `Downcast`) resolve their
/// dropped place's type iteratively through `DropFacts::project_ty`, while an unanswered non-field
/// projection (other than `Downcast`) still reports `ElabError::PlaceType`.
#[test]
fn non_field_projections_elaborate_via_project_ty() {
    struct ProjFacts {
        answer: bool,
    }
    impl DropFacts for ProjFacts {
        fn at(&self, _: Stmt) -> DropState<'_> {
            DropState::Init
        }
        fn needs_drop(&self, ty: PTy) -> NeedsDrop {
            // t4 (`u32`) needs no drop; t2 needs drop.
            if ty == PTy(4) { NeedsDrop::No } else { NeedsDrop::Yes }
        }
        fn bool_ty(&self) -> PTy {
            PTy(1)
        }
        fn project_ty(&self, parent: PTy, proj: Proj) -> Option<PTy> {
            if !self.answer {
                return None;
            }
            match (parent, proj) {
                // t6 = Box<u32>, t7 = &mut Droppable, t8 = [u32; 2], t9 = [Droppable; 2]
                (PTy(6), Proj::DerefBox) => Some(PTy(4)),
                (PTy(7), Proj::Deref) => Some(PTy(2)),
                (PTy(8), Proj::Index(_) | Proj::ConstIndex { .. }) => Some(PTy(4)),
                (PTy(9), Proj::Index(_) | Proj::ConstIndex { .. }) => Some(PTy(2)),
                _ => None,
            }
        }
    }

    let text = "\
local _0: t0 return
local _1: t6 arg 0
local _2: t7 arg 1
local _3: t8 arg 2
local _4: t4 arg 3
local _5: t9 arg 4
bb0:
  drop _1.box then bb1 unwind continue
bb1:
  drop _2.* then bb2 unwind continue
bb2:
  drop _3[_4] then bb3 unwind continue
bb3:
  drop _5[0 of 2] then bb4 unwind continue
bb4:
  return
";
    // Without `project_ty`, `DerefBox` still returns `ElabError::PlaceType`.
    let refused = try_run(text, &ProjFacts { answer: false }, Plant::None);
    assert!(matches!(refused, Err(crate::ElabError::PlaceType { stmt, .. }) if stmt == Stmt(0)));

    // With `project_ty`: `_1.box` (u32) and `_3[_4]` (u32) lose their drops (`goto`),
    // while `_2.*` (t2) and `_5[0 of 2]` (t2) keep their drops!
    let out = try_run(text, &ProjFacts { answer: true }, Plant::None).unwrap();
    verify(&out).expect("elaborated body verifies");
    let printed = print(&out);
    assert!(!printed.contains("drop _1.box"), "u32 DerefBox drop must be removed:\n{printed}");
    assert!(printed.contains("drop _2.*"), "droppable Deref drop must stay:\n{printed}");
    assert!(!printed.contains("drop _3[_4]"), "u32 Index drop must be removed:\n{printed}");
    assert!(printed.contains("drop _5[0 of 2]"), "droppable ConstIndex drop must stay:\n{printed}");
}

#[test]
fn enum_partial_drop_switches_on_discriminant() {
    use crate::VariantDrop;
    let v0_fields = [Part { path: &[Field { index: 0, ty: PTy(2) }], state: PartState::Init }];
    let v1_fields = [
        Part { path: &[Field { index: 0, ty: PTy(2) }], state: PartState::Maybe },
        Part { path: &[Field { index: 1, ty: PTy(2) }], state: PartState::Init },
    ];
    let variants = [
        VariantDrop { discr: 0, variant: 0, parts: &v0_fields },
        VariantDrop { discr: 1, variant: 1, parts: &v1_fields },
    ];
    struct EnumFacts<'a>(&'a [VariantDrop<'a>]);
    impl<'a> DropFacts for EnumFacts<'a> {
        fn at(&self, _: Stmt) -> DropState<'_> {
            DropState::Enum { discr_ty: PTy(4), variants: self.0 }
        }
        fn needs_drop(&self, ty: PTy) -> NeedsDrop {
            if ty == PTy(4) || ty == PTy(0) || ty == PTy(1) { NeedsDrop::No } else { NeedsDrop::Yes }
        }
        fn bool_ty(&self) -> PTy {
            PTy(1)
        }
    }
    let text = "\
local _0: t0 return
local _1: t3 arg 0
bb0:
  drop _1 then bb1 unwind continue
bb1:
  return
";
    let out = try_run(text, &EnumFacts(&variants), Plant::None).unwrap();
    verify(&out).expect("enum partial drop body verifies");
    let printed = print(&out);
    assert!(printed.contains("discr _1"), "expected discriminant read:\n{printed}");
    assert!(printed.contains("switch"), "expected discriminant switch:\n{printed}");
    assert!(printed.contains("_1#0.0:t2"), "expected variant 0 field 0 drop:\n{printed}");
    assert!(printed.contains("_1#1.0:t2"), "expected variant 1 field 0 drop:\n{printed}");
    assert!(printed.contains("_1#1.1:t2"), "expected variant 1 field 1 drop:\n{printed}");
    assert!(printed.contains("cleanup:"), "expected unwind cleanup ladder:\n{printed}");
}

#[test]
fn out_of_block_order_loans_replay_without_id_mismatch() {
    use blaze_body::body::{ExprId, Mutbl};
    use blaze_body::{BlockKind, BodyBuilder, BorrowKind, LocalDecl, Role, Span};

    let a = test_alloc();
    let mut b = BodyBuilder::new_in(a);
    let _r0 = b.new_local(LocalDecl { ty: PTy(0), kind: LocalKind::Return, mutbl: Mutbl::Mut });
    let x = b.new_local(LocalDecl { ty: PTy(1), kind: LocalKind::Temp(ExprId(0), Role::Value), mutbl: Mutbl::Not });
    let r1 = b.new_local(LocalDecl { ty: PTy(2), kind: LocalKind::Temp(ExprId(1), Role::Borrow), mutbl: Mutbl::Not });
    let r2 = b.new_local(LocalDecl { ty: PTy(2), kind: LocalKind::Temp(ExprId(2), Role::Borrow), mutbl: Mutbl::Not });
    let xp = b.local_place(x);
    let r1p = b.local_place(r1);
    let r2p = b.local_place(r2);
    let o0 = b.origin(false);
    let o1 = b.origin(false);

    let bb0 = b.new_block(BlockKind::Normal);
    let bb1 = b.new_block(BlockKind::Normal);
    let bb2 = b.new_block(BlockKind::Normal);

    // Fill bb0, then bb2 (emitting L0), then bb1 (emitting L1): loan IDs follow statement order,
    // not block index order.
    b.start(bb0);
    b.terminate(StmtKind::Goto(bb2), Span::default());

    b.start(bb2);
    let _l0 = b.borrow(r1p, BorrowKind::Shared, xp, o0, Span::default());
    b.terminate(StmtKind::Goto(bb1), Span::default());

    b.start(bb1);
    let _l1 = b.borrow(r2p, BorrowKind::Shared, xp, o1, Span::default());
    b.terminate(StmtKind::Return, Span::default());

    let body = b.finish().expect("build body");
    let v = verify(&body).expect("verify input");
    struct NoDrops;
    impl DropFacts for NoDrops {
        fn at(&self, _: Stmt) -> DropState<'_> { DropState::Uninit }
        fn needs_drop(&self, _: PTy) -> NeedsDrop { NeedsDrop::No }
        fn bool_ty(&self) -> PTy { PTy(1) }
    }
    let out = crate::elaborate(v, &NoDrops, a).expect("elaborate out-of-order loans");
    verify(&out).expect("elaborated body verifies");
}

/// A non-`Copy` local conditionally moved before `StmtKind::Suspend` gets one drop flag checked on
/// BOTH the `cancel` path (ending in `StmtKind::Cancelled`) and the `resume` path (ending in
/// `StmtKind::Return`), with zero double-drops and zero leaked drops.
#[test]
fn coroutine_suspend_elaborates_cancel_and_resume_drop_flags() {
    let text = "\
local _0: t0 return mut
local _1: t1 arg 0
local _2: t2 binding pat0 mut
local _3: t2 temp e0 value
const c0 = scalar 0x0: t0
const c1 = scalar 0x1: t2
bb0:
  live _2
  _2 = const c1
  switch copy _1 [0 -> bb2] else bb1
bb1:
  _3 = move _2
  goto bb2
bb2:
  suspend resume bb3 cancel bb4
bb3:
  drop _2 then bb5 unwind continue
bb5:
  dead _2
  _0 = const c0
  return
bb4:
  drop _2 then bb6 unwind continue
bb6:
  dead _2
  cancelled
";
    struct MaybeOnDrops;
    impl DropFacts for MaybeOnDrops {
        fn at(&self, _: Stmt) -> DropState<'_> {
            DropState::Maybe
        }
        fn needs_drop(&self, ty: PTy) -> NeedsDrop {
            if ty == PTy(2) { NeedsDrop::Yes } else { NeedsDrop::No }
        }
        fn bool_ty(&self) -> PTy {
            PTy(1)
        }
    }
    let out = try_run(text, &MaybeOnDrops, Plant::None).expect("elaborates coroutine drops");
    verify(&out).expect("elaborated coroutine body verifies");
    let printed = print(&out);
    assert!(printed.contains("suspend resume"), "suspend preserved:\n{printed}");
    assert!(printed.contains("cancelled"), "cancelled preserved:\n{printed}");
    assert!(printed.contains("flag _2"), "drop flag created for _2:\n{printed}");
    assert_eq!(first_bad_drop(&out), None, "zero double-drops or leaked drops on both resume and cancel:\n{printed}");
}
