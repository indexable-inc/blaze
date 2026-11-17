//! The body's tests, written in the text form: a body with an unwind path verifies, each
//! [`VerifyError`] has a body that triggers exactly it, and the text and the record round trip.

use std::alloc::Global;
use std::string::String;

use blaze_typeck::{PNode, PTys, UintTy};

use crate::body::{List, Mutbl, PTy};
use crate::code::encode_stmt;
use crate::{
    Block, BlockKind, Body, BodyBuilder, ConstDecl, ConstKind, Local, LocalDecl, LocalKind, Place, Proj,
    Span, Stmt, StmtKind, Table, Unwind, VerifyError, encode, open, try_encode, verify, verify_ref,
};

fn parse(text: &str) -> Body<Global> {
    crate::parse(text, Global).unwrap_or_else(|e| panic!("{e:?} in:\n{text}"))
}

fn print(b: crate::BodyRef<'_>) -> String {
    let mut out = String::new();
    crate::print(&b, &mut out).expect("a parsed body prints");
    out
}

fn refused(b: &Body<Global>) -> VerifyError {
    match verify(b) {
        Ok(_) => panic!("verify accepted:\n{}", print(b.view())),
        Err(e) => e,
    }
}

fn accepted(b: &Body<Global>) {
    if let Err(e) = verify(b) {
        panic!("verify refused {e:?}:\n{}", print(b.view()));
    }
}

/// A call whose unwind runs a cleanup path that drops the argument and resumes unwinding.
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

/// Straight-line code with a constant, a string, a borrow, its origin and a subset.
const STRAIGHT: &str = "\
local _0: t0 return
local _1: t0 arg 0
local _2: t0 temp e3 value mut
local _3: t1 temp e4 borrow
const c0 = scalar 0x2: t0
const c1 = bytes \"6869\": t5
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

/// A switch with three cases.
const SWITCH: &str = "\
local _0: t0 return
local _1: t0 arg 0
bb0:
  switch copy _1 [0 -> bb1, 1 -> bb2, 7 -> bb3] else bb4
bb1:
  goto bb4
bb2:
  goto bb4
bb3:
  goto bb4
bb4:
  return
";

/// An `.await`'s suspension: resumed it returns, cancelled it drops the awaitee.
const SUSPEND: &str = "\
local _0: t0 return
local _1: t5 temp e1 awaitee
bb0:
  suspend resume bb1 cancel bb2
bb1:
  return
bb2:
  drop _1 then bb3 unwind continue
bb3:
  return
";

/// Two blocks of plain rows: the base the corruptions below start from.
const BASE: &str = "\
local _0: t0 return
local _1: t0 arg 0
local _2: t1 temp e0 value
const c0 = scalar 0x1: t0
bb0:
  _0 = const c0
  _2 = copy _1.0:t1
  goto bb1
bb1:
  return
";

#[test]
fn unwind_to_cleanup_verifies() {
    let b = parse(UNWIND);
    accepted(&b);
    let v = verify(&b).expect("accepted above");
    assert_eq!(v.block(Block(2)).kind, BlockKind::Cleanup);
    let drop = v.stmt(v.terminator(Block(2))).kind;
    let one = v.local_place(Local(1));
    assert_eq!(drop, StmtKind::Drop { place: one, target: Block(4), unwind: Unwind::Terminate });
    assert_eq!(v.stmt(v.terminator(Block(4))).kind, StmtKind::Resume);
    assert_eq!(print(b.view()), UNWIND);
}

#[test]
fn text_round_trips() {
    for text in [STRAIGHT, SWITCH, SUSPEND, UNWIND] {
        let b = parse(text);
        accepted(&b);
        assert_eq!(print(b.view()), text);
    }
}

#[test]
fn record_round_trips() {
    for text in [STRAIGHT, SWITCH, SUSPEND, UNWIND] {
        let b = parse(text);
        let bytes = encode(&b.view(), Global);
        let read = open(&bytes).expect("a written record opens");
        assert!(verify_ref(read).is_ok(), "the record of\n{text}is refused: {:?}", verify_ref(read).err());
        assert_eq!(print(read), text);
        assert_eq!(encode(&read, Global).as_slice(), bytes.as_slice());
    }
}

#[test]
fn open_refuses_a_buffer_that_is_no_record() {
    assert!(open(&[0u8; 8]).is_err());
}

#[test]
fn bad_row() {
    let mut b = parse(BASE);
    b.stmts[0].tag = 0;
    let want = crate::BadRow { table: Table::Stmts, row: 0, code: 0 };
    assert_eq!(refused(&b), VerifyError::BadRow(want));
}

#[test]
fn column_length() {
    let mut b = parse(BASE);
    b.spans.truncate(3);
    assert_eq!(refused(&b), VerifyError::ColumnLength { table: Table::Spans, len: 3, want: 4 });
}

#[test]
fn out_of_range() {
    let mut b = parse(BASE);
    b.stmts[2] = encode_stmt(StmtKind::Goto(Block(9)));
    let at = crate::Site { table: Table::Stmts, row: 2 };
    assert_eq!(refused(&b), VerifyError::OutOfRange { at, table: Table::Blocks, id: 9 });
}

#[test]
fn local_place() {
    let mut b = parse(BASE);
    b.local_places[1] = 0;
    assert_eq!(refused(&b), VerifyError::LocalPlace { local: Local(1) });
}

#[test]
fn place_shape() {
    let mut b = parse(BASE);
    b.places[3].depth = 2;
    assert_eq!(refused(&b), VerifyError::PlaceShape { place: crate::Place(3) });
}

#[test]
fn no_blocks() {
    let mut b = parse("local _0: t0 return\nbb0:\n  return\n");
    b.blocks.clear();
    assert_eq!(refused(&b), VerifyError::NoBlocks);
}

#[test]
fn empty_block() {
    let mut b = parse(BASE);
    b.blocks[1].end = b.blocks[1].start;
    assert_eq!(refused(&b), VerifyError::EmptyBlock { block: Block(1) });
}

#[test]
fn block_range() {
    let mut b = parse(BASE);
    b.blocks[1].end = 5;
    assert_eq!(refused(&b), VerifyError::BlockRange { block: Block(1) });
}

#[test]
fn stmt_block() {
    let mut b = parse(BASE);
    b.stmt_block[0] = 1;
    assert_eq!(refused(&b), VerifyError::StmtBlock { stmt: Stmt(0) });
}

#[test]
fn entry_is_cleanup() {
    let b = parse("local _0: t0 return\nbb0 cleanup:\n  resume\n");
    assert_eq!(refused(&b), VerifyError::EntryIsCleanup);
}

#[test]
fn no_terminator() {
    let mut b = parse(BASE);
    b.stmts[3] = encode_stmt(StmtKind::StorageDead(Local(2)));
    assert_eq!(refused(&b), VerifyError::NoTerminator { block: Block(1), last: Stmt(3) });
}

#[test]
fn terminator_inside() {
    let mut b = parse(BASE);
    b.stmts[0] = encode_stmt(StmtKind::Return);
    assert_eq!(refused(&b), VerifyError::TerminatorInside { block: Block(0), stmt: Stmt(0) });
}

#[test]
fn unwind_into_normal() {
    let b = parse(
        "\
local _0: t0 return
bb0:
  call fn k1<0,0>() -> _0 then bb1 unwind bb1
bb1:
  return
",
    );
    assert_eq!(refused(&b), VerifyError::UnwindIntoNormal { stmt: Stmt(0), block: Block(1) });
}

#[test]
fn unwind_from_cleanup() {
    let b = parse(
        "\
local _0: t0 return
local _1: t0 arg 0
bb0:
  call fn k1<0,0>() -> _0 then bb1 unwind bb2
bb1:
  return
bb2 cleanup:
  drop _1 then bb3 unwind bb3
bb3 cleanup:
  resume
",
    );
    assert_eq!(refused(&b), VerifyError::UnwindFromCleanup { stmt: Stmt(2), block: Block(3) });
}

#[test]
fn normal_into_cleanup() {
    let b = parse("local _0: t0 return\nbb0:\n  goto bb1\nbb1 cleanup:\n  resume\n");
    assert_eq!(refused(&b), VerifyError::NormalIntoCleanup { stmt: Stmt(0), block: Block(1) });
}

#[test]
fn cleanup_into_normal() {
    let b = parse(
        "\
local _0: t0 return
bb0:
  call fn k1<0,0>() -> _0 then bb2 unwind bb1
bb1 cleanup:
  goto bb2
bb2:
  return
",
    );
    assert_eq!(refused(&b), VerifyError::CleanupIntoNormal { stmt: Stmt(1), block: Block(2) });
}

#[test]
fn resume_outside_cleanup() {
    let b = parse("local _0: t0 return\nbb0:\n  resume\n");
    assert_eq!(refused(&b), VerifyError::ResumeOutsideCleanup { stmt: Stmt(0) });
}

#[test]
fn duplicate_case() {
    let b = parse(
        "\
local _0: t0 return
local _1: t0 arg 0
bb0:
  switch copy _1 [1 -> bb1, 1 -> bb1] else bb1
bb1:
  return
",
    );
    assert_eq!(refused(&b), VerifyError::DuplicateCase { stmt: Stmt(0), value: 1 });
}

#[test]
fn cases_unsorted() {
    let mut b = parse(SWITCH);
    b.cases.swap(0, 2);
    assert_eq!(refused(&b), VerifyError::CasesUnsorted { stmt: Stmt(0) });
}

#[test]
fn half_destination() {
    let b = parse("local _0: t0 return\nbb0:\n  call fn k1<0,0>() -> _0 unwind continue\n");
    assert_eq!(refused(&b), VerifyError::HalfDestination { stmt: Stmt(0) });
}

/// A record table without `(u32, bool)`: `t0` is `bool`, `t1` is `u32`.
const LACKS: [PNode; 2] = [PNode::Bool, PNode::Uint(UintTy::U32)];

/// The same table with `(u32, bool)` as `t2`, its list the record's `<0,2>`.
const HAS: [PNode; 3] = [PNode::Bool, PNode::Uint(UintTy::U32), PNode::Tuple(List { start: 0, len: 2 })];
const HAS_LISTS: [PTy; 2] = [PTy(1), PTy(0)];

/// A body whose return place is the overlay's `(u32, bool)`, as [`overlay_built`] prints it.
const OVERLAY: &str = "\
types t2 l0
tylists t1 t0
ty t2 = tuple <0,2>
local _0: t2 return
bb0:
  return
";

/// Builds a body against [`LACKS`] whose return type is `(u32, bool)`.
fn overlay_built() -> Body<Global> {
    let mut b = BodyBuilder::new_with_types_in(PTys::flat(&LACKS, &[]), Global);
    let l = b.intern_list(&[PTy(1), PTy(0)]).expect("both entries are the record's");
    let pair = b.intern_ty(PNode::Tuple(l)).expect("a tuple of the record's types");
    b.new_local(LocalDecl { ty: pair, kind: LocalKind::Return, mutbl: Mutbl::Not });
    let entry = b.new_block(BlockKind::Normal);
    b.start(entry);
    b.terminate(StmtKind::Return, Span::default());
    b.finish().expect("a whole body")
}

/// `(u32, bool)` against a record without it is the overlay's `t2`, the same `t2` asked
/// again; `bool` is the record's `t0`; against a record with it, the record's `t2` and no
/// overlay. Prints [`OVERLAY`]'s `ty t2 = tuple <0,2>`.
#[test]
fn intern_ty_overlays_what_the_record_lacks() {
    let mut b = BodyBuilder::new_with_types_in(PTys::flat(&LACKS, &[]), Global);
    let l = b.intern_list(&[PTy(1), PTy(0)]).expect("in range");
    assert_eq!(l, List { start: 0, len: 2 });
    let pair = b.intern_ty(PNode::Tuple(l)).expect("in range");
    assert_eq!(pair, PTy(2));
    assert_eq!(b.intern_list(&[PTy(1), PTy(0)]), Ok(l));
    assert_eq!(b.intern_ty(PNode::Tuple(l)), Ok(PTy(2)));
    assert_eq!(b.intern_ty(PNode::Bool), Ok(PTy(0)));
    assert_eq!(b.body().view().own_tys(), 1);

    let mut b = BodyBuilder::new_with_types_in(PTys::flat(&HAS, &HAS_LISTS), Global);
    let l = b.intern_list(&[PTy(1), PTy(0)]).expect("in range");
    assert_eq!(b.intern_ty(PNode::Tuple(l)), Ok(PTy(2)));
    assert_eq!(b.body().view().ty_base(), None);

    let built = overlay_built();
    accepted(&built);
    assert_eq!(print(built.view()), OVERLAY);
    let v = verify(&built).expect("accepted above");
    let record = PTys::flat(&LACKS, &[]);
    assert_eq!(v.ty(&record, PTy(2)), Ok(PNode::Tuple(List { start: 0, len: 2 })));
    assert_eq!(v.ty(&record, PTy(1)), Ok(PNode::Uint(UintTy::U32)));
    let entries = v.ty_list(&record, List { start: 0, len: 2 }).expect("the overlay's list");
    assert_eq!(entries.iter().collect::<std::vec::Vec<_>>(), [PTy(1), PTy(0)]);
}

/// `intern_ty` with no record table is refused by name, and the refusal is kept.
#[test]
fn intern_ty_needs_the_record() {
    let mut b = BodyBuilder::new_in(Global);
    assert_eq!(b.intern_ty(PNode::Bool), Err(crate::BuildError::NoTypeTable));
    assert_eq!(b.error(), Some(crate::BuildError::NoTypeTable));
}

/// The overlay round trips through the text and the record: prints [`OVERLAY`] both times.
#[test]
fn overlay_round_trips() {
    let b = parse(OVERLAY);
    accepted(&b);
    assert_eq!(print(b.view()), OVERLAY);
    let bytes = encode(&b.view(), Global);
    let read = open(&bytes).expect("a written record opens");
    assert!(verify_ref(read).is_ok());
    assert_eq!(print(read), OVERLAY);
    assert_eq!(encode(&read, Global).as_slice(), bytes.as_slice());
    let tried = try_encode(&read, Global).expect("far under 2 GiB");
    assert_eq!(tried.as_slice(), bytes.as_slice());
    assert_eq!(encode(&overlay_built().view(), Global).as_slice(), bytes.as_slice());
}

/// An overlay type naming a type after itself: `TypeChild { ty: PTy(2), child: PTy(5) }`.
#[test]
fn type_child() {
    let b = parse("types t2 l0\nty t2 = ref t5\nlocal _0: t0 return\nbb0:\n  return\n");
    assert_eq!(refused(&b), VerifyError::TypeChild { ty: PTy(2), child: PTy(5) });
}

/// Two overlay types of one node: `DuplicateType { ty: PTy(3), first: PTy(2) }`.
#[test]
fn duplicate_type() {
    let b = parse("types t2 l0\nty t2 = slice t0\nty t3 = slice t0\nlocal _0: t0 return\nbb0:\n  return\n");
    assert_eq!(refused(&b), VerifyError::DuplicateType { ty: PTy(3), first: PTy(2) });
}

/// `deinit`, `setdiscr` and a local's drop-type mark: prints `  setdiscr _1 variant 2` and
/// `local _1: t3 temp e2 value mut !` back, from the text and from the record.
const SET_DISCR: &str = "\
local _0: t0 return
local _1: t3 temp e2 value mut !
bb0:
  deinit _1
  setdiscr _1 variant 2
  return
";

#[test]
fn set_discriminant_and_deinit_round_trip() {
    let b = parse(SET_DISCR);
    accepted(&b);
    assert_eq!(print(b.view()), SET_DISCR);
    let v = verify(&b).expect("accepted above");
    let one = v.local_place(Local(1));
    assert!(v.place(one).of_drop_type);
    assert_eq!(v.stmt(Stmt(0)).kind, StmtKind::Deinit(one));
    assert_eq!(v.stmt(Stmt(1)).kind, StmtKind::SetDiscriminant { place: one, variant: 2 });
    let bytes = encode(&b.view(), Global);
    let read = open(&bytes).expect("a written record opens");
    assert!(verify_ref(read).is_ok());
    assert_eq!(print(read), SET_DISCR);
}

/// A `deinit` or `setdiscr` of a place past the table is refused naming the row and the id:
/// `OutOfRange { at: Stmts row 0, table: Places, id: 9 }`.
#[test]
fn set_discriminant_and_deinit_bad_place() {
    let at = crate::Site { table: Table::Stmts, row: 0 };
    let want = VerifyError::OutOfRange { at, table: Table::Places, id: 9 };
    let mut b = parse(SET_DISCR);
    b.stmts[0] = encode_stmt(StmtKind::Deinit(Place(9)));
    assert_eq!(refused(&b), want);
    let mut b = parse(SET_DISCR);
    b.stmts[0] = encode_stmt(StmtKind::SetDiscriminant { place: Place(9), variant: 0 });
    assert_eq!(refused(&b), want);
}

/// Places and constants intern by hash: the same projection, constant or string twice is
/// one id; a string of another type is another.
#[test]
fn places_and_constants_intern() {
    let mut b = BodyBuilder::new_in(Global);
    let l = b.new_local(LocalDecl { ty: PTy(0), kind: LocalKind::Return, mutbl: Mutbl::Not });
    let root = b.local_place(l);
    let field = Proj::Field { index: 0, ty: PTy(1) };
    let a = b.project(root, field);
    assert_eq!(b.project(root, field), a);
    assert_ne!(b.project_marked(root, field, true), a);
    let one = ConstDecl { kind: ConstKind::Scalar { bits: 1 }, ty: PTy(0) };
    let c = b.new_const(one);
    assert_eq!(b.new_const(one), c);
    let s = b.new_bytes(b"hi", PTy(5));
    assert_eq!(b.new_bytes(b"hi", PTy(5)), s);
    assert_ne!(b.new_bytes(b"hi", PTy(6)), s);
    assert_eq!(b.body().view().consts(), 3);
}
