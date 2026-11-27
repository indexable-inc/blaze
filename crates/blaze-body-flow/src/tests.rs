//! Bodies written in blaze-body's text form, projected: the flow each becomes, and what the
//! move and borrow checks find in it, reported at body rows. These need no lowering (P2).

use std::alloc::Global;
use std::string::String;

use blaze_body as bd;
use blaze_body::{Body, verify};
use blaze_flow::{BorrowFinding, DropKind, MoveFindingKind};

use crate::{ProjectError, Projected, canon, project};

fn parse(text: &str) -> Body<Global> {
    bd::parse(text, Global).unwrap_or_else(|e| panic!("{e:?} in:\n{text}"))
}

fn projected(body: &Body<Global>) -> Projected<Global> {
    let v = verify(body).unwrap_or_else(|e| panic!("verify refused {e:?}"));
    project(v, Global).unwrap_or_else(|e| panic!("project refused: {e}"))
}

fn flow_text<A: core::alloc::Allocator + Copy>(p: &Projected<A>) -> String {
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

/// `ConstIndex` and `Subslice` places project directly to `blaze_flow::Proj::ConstantIndex`
/// and `Subslice`, so disjoint slice pattern borrows pass borrowck cleanly while whole-slice
/// conflicts are caught.
#[test]
fn const_index_and_subslice_project_cleanly() {
    let text = "\
local _0: t0 return
local _1: t1 arg 0
local _2: t1 temp e0 value
local _3: t2 temp e1 value
bb0:
  _2 = copy _1[0..1]
  _3 = copy _1[0 of 2]
  _0 = move _2
  return
";
    let a = blaze_own::testing::heap().alloc();
    let body = bd::parse(text, a).unwrap_or_else(|e| panic!("{e:?}"));
    let p = project(bd::verify(&body).unwrap(), a).unwrap_or_else(|e| panic!("project refused: {e}"));
    let r = blaze_flow::check(&p.flow, a).expect("within bounds");
    assert!(r.moves.is_empty(), "{:?}", r.moves);
    assert!(r.borrows.is_empty(), "{:?}", r.borrows);
    let has_subslice = (0..p.flow.places.len()).any(|i| matches!(p.flow.place(blaze_flow::Place(i as u32)).proj, Some(blaze_flow::Proj::Subslice { .. })));
    let has_const_idx = (0..p.flow.places.len()).any(|i| matches!(p.flow.place(blaze_flow::Place(i as u32)).proj, Some(blaze_flow::Proj::ConstantIndex { .. })));
    assert!(has_subslice && has_const_idx, "expected Subslice and ConstantIndex in flow:\n{}", flow_text(&p));
}

/// A lone discriminant read (such as an enum-to-integer cast `_0 = discr _1; return`) projects
/// as a shared read of `_1` writing `_0`, passing borrowck and catching use-after-move.
#[test]
fn lone_discriminant_projects_as_shared_read() {
    let a = blaze_own::testing::heap().alloc();
    let body = bd::parse(
        "\
local _0: t0 return
local _1: t1 arg 0
bb0:
  _0 = discr _1
  return
",
        a,
    )
    .unwrap();
    let p = project(bd::verify(&body).unwrap(), a).unwrap_or_else(|e| panic!("project refused: {e}"));
    let want = "\
bb0:
  s0 _0 = (copy _1)
  s1 return
";
    assert_eq!(flow_text(&p), want);
    let r = blaze_flow::check(&p.flow, a).expect("within bounds");
    assert!(r.moves.is_empty(), "{:?}", r.moves);
    assert!(r.borrows.is_empty(), "{:?}", r.borrows);
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

/// D119 End-to-End Oracle Parity Suite (rule 78): runs the SAME real type-checked Rust functions
/// through BOTH the old AST walk (`blaze_build::build`) AND the D119 unified MIR pipeline
/// (`blaze_body_lower::lower` -> `blaze_body::verify` -> `blaze_body_build::build`), executing
/// both resulting verified LIR functions in `blaze_lir::eval` and proving identical runtime
/// outputs across every input.
mod d119_oracle_parity {
    use blaze_body_lower::BodyFacts;
    use blaze_build::layout::{LayoutFacts, Missing, ReprAttrs, ValidRange};
    use blaze_build::{Abi, Adts, OverflowChecks, Panic, Subst};
    use blaze_flow_lower::{AdtFacts, CalleeFacts, Facts};
    use blaze_lir::eval::{Outcome, Val, run_in};
    use blaze_own::Alloc;
    use blaze_typeck::fixture::Fx;
    use blaze_typeck::{
        Adt, AdtKind, Arm, BinOp, BindAnn, BodyPath, BodyView, Checked, CtorKind, Def, Edition, Expr, ExprId, Field,
        FieldInit, FnPtrHeader, FnSig, InferCtx, IntSuffix, IntTy, ItemKey, List, Lit, Name, Needs,
        Outcome as CheckOutcome, PNode, PTy, PTys, Pat, PatId, PathIdx, PortableTys, Range, ScopeId, Segment, Stmt,
        StmtId, StructBase, UintTy, Variant,
    };

    fn heap() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    fn make_ranges(n: usize) -> blaze_own::Vec<Range, Alloc<'static>> {
        let mut out = blaze_own::Vec::with_capacity_in(n, heap());
        for i in 0..n {
            let s = u32::try_from(i).unwrap_or(0);
            out.push(Range { start: s, end: s + 1 });
        }
        out
    }

    type PromotedRecord = blaze_build::data::DataRecord<super::Global>;

    struct OracleWorld<'a> {
        overflow: OverflowChecks,
        ints: &'a [(ExprId, u128)],
        floats: &'a [(ExprId, u128)],
        strs: &'a [(ExprId, &'a str)],
        int_pats: &'a [(PatId, u128)],
        float_pats: &'a [(PatId, u128)],
        str_pats: &'a [(PatId, &'a [u8])],
        enum_key: Option<ItemKey>,
        enum_tys: Option<PortableTys<'static>>,
        enum_variants: &'a [Variant],
        enum_fields: &'a [Field],
        enum_discrs: &'a [i128],
        struct_key: Option<ItemKey>,
        struct_tys: Option<PortableTys<'static>>,
        struct_variants: &'a [Variant],
        struct_fields: &'a [Field],
        deref_fn_item: Option<ItemKey>,
        index_fn_item: Option<ItemKey>,
        fn_call_item: Option<ItemKey>,
        fn_once_item: Option<ItemKey>,
        closure_tys: Option<PortableTys<'static>>,
        closures: &'a [(ItemKey, u32, PTy, PTy, blaze_build::FnTrait)],
        promoted: &'a [(ExprId, PromotedRecord)],
    }

    impl<'a> OracleWorld<'a> {
        fn new(overflow: OverflowChecks, ints: &'a [(ExprId, u128)], int_pats: &'a [(PatId, u128)]) -> Self {
            Self {
                overflow,
                ints,
                floats: &[],
                strs: &[],
                int_pats,
                float_pats: &[],
                str_pats: &[],
                enum_key: None,
                enum_tys: None,
                enum_variants: &[],
                enum_fields: &[],
                enum_discrs: &[10, 20, 30],
                struct_key: None,
                struct_tys: None,
                struct_variants: &[],
                struct_fields: &[],
                deref_fn_item: None,
                index_fn_item: None,
                fn_call_item: None,
                fn_once_item: None,
                closure_tys: None,
                closures: &[],
                promoted: &[],
            }
        }
    }

    impl Facts for OracleWorld<'_> {
        fn regions(&self) -> &dyn blaze_flow_lower::RegionFacts {
            &blaze_flow_lower::NoRegions
        }
        fn region_tys(&self) -> Option<PTys<'_>> {
            None
        }
        fn adt_variances(&self, _: ItemKey) -> Option<&[blaze_flow_lower::Variance]> {
            None
        }
        fn implied_bounds(&self) -> Option<(&[(u32, blaze_flow_lower::Region)], &[(blaze_flow_lower::Region, blaze_flow_lower::Region)])> {
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
        fn closure_requirements(&self, _: ExprId) -> &[blaze_flow_lower::Requirement] {
            &[]
        }
        fn async_bodies_checked(&self) -> bool {
            false
        }
        fn is_future(&self, _: PTy) -> Option<bool> {
            None
        }
        fn shape(&self, _: ItemKey) -> Option<blaze_flow_lower::Shape<'_>> {
            None
        }
        fn object_trait_lifetimes(&self, _: ItemKey) -> Option<bool> {
            None
        }
        fn call_bounds(&self, _: ItemKey) -> Option<blaze_flow_lower::CallBounds<'_>> {
            None
        }
        fn trait_lifetimes(&self, _: ItemKey) -> Option<u32> {
            None
        }
        fn alias_lifetimes(&self, _: ItemKey, _: Name) -> Option<u32> {
            None
        }
        fn ties(&self, _: ItemKey) -> Option<blaze_flow_lower::Ties<'_>> {
            None
        }
        fn field_param(&self, _: ItemKey, _: u32, _: u32) -> Option<u32> {
            None
        }
        fn field_lead(&self, _: ItemKey, _: u32, _: u32) -> Option<blaze_flow_lower::FieldLead> {
            None
        }
        fn field_regions(&self, _: ItemKey, _: u32, _: u32) -> Option<&[blaze_flow_lower::FieldRegion]> {
            None
        }
        fn copy_args(&self, _: ItemKey) -> Option<&[bool]> {
            None
        }
        fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
            if self.enum_key == Some(k) {
                return Some(AdtFacts { copy: true, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum: true, is_box: false });
            }
            Some(AdtFacts { copy: true, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum: false, is_box: false })
        }
        fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
            Some(CalleeFacts { writes_lifetimes: false, self_ref: false, c_variadic: false })
        }
        fn field(&self, k: ItemKey, _: u32, name: Name) -> Option<u32> {
            if self.struct_key == Some(k) {
                return self.struct_fields.iter().position(|f| f.name == name).and_then(|i| u32::try_from(i).ok());
            }
            None
        }
        fn field_count(&self, k: ItemKey, v: u32) -> Option<u32> {
            if self.struct_key == Some(k) {
                return u32::try_from(self.struct_fields.len()).ok();
            }
            if self.enum_key == Some(k) {
                return self.enum_variants.get(v as usize).map(|var| var.fields.len);
            }
            None
        }
        fn param_copy(&self, _: u32) -> bool {
            false
        }
    }

    impl BodyFacts for OracleWorld<'_> {
        fn int_lit(&self, e: ExprId) -> Option<u128> {
            self.ints.iter().find(|(id, _)| *id == e).map(|(_, v)| *v)
        }
        fn float_lit(&self, e: ExprId) -> Option<u128> {
            self.floats.iter().find(|(id, _)| *id == e).map(|(_, v)| *v)
        }
        fn str_lit(&self, e: ExprId) -> Option<&[u8]> {
            self.strs.iter().find(|(id, _)| *id == e).map(|(_, s)| s.as_bytes())
        }
        fn int_pat(&self, p: PatId) -> Option<u128> {
            self.int_pats.iter().find(|(id, _)| *id == p).map(|(_, v)| *v)
        }
        fn float_pat(&self, p: PatId) -> Option<u128> {
            self.float_pats.iter().find(|(id, _)| *id == p).map(|(_, v)| *v)
        }
        fn str_pat(&self, p: PatId) -> Option<&[u8]> {
            self.str_pats.iter().find(|(id, _)| *id == p).map(|(_, b)| *b)
        }
        fn discr(&self, adt: ItemKey, variant: u32) -> Option<blaze_body_lower::Discr> {
            if self.enum_key == Some(adt) {
                let d = self.enum_discrs.get(variant as usize).copied().unwrap_or(i128::from(variant));
                return Some(blaze_body_lower::Discr {
                    int: blaze_body_lower::DiscrInt::Int(IntTy::Isize),
                    bits: (d as i64) as u64 as u128,
                });
            }
            None
        }
        fn pointer_bits(&self) -> Option<u32> {
            Some(64)
        }
        fn trait_item(&self, _: ItemKey) -> Option<bool> {
            Some(false)
        }
        fn deref_fn(&self, _: blaze_typeck::Mutbl) -> Option<ItemKey> {
            self.deref_fn_item
        }
        fn index_fn(&self, _: blaze_typeck::Mutbl) -> Option<ItemKey> {
            self.index_fn_item
        }
        fn fn_trait_method(&self, kind: blaze_typeck::ClosureKind) -> Option<ItemKey> {
            match kind {
                blaze_typeck::ClosureKind::FnOnce => self.fn_once_item.or(self.fn_call_item),
                _ => self.fn_call_item,
            }
        }
    }

    impl LayoutFacts for OracleWorld<'_> {
        fn adt(&self, k: ItemKey) -> Option<Adt<'_>> {
            if self.enum_key == Some(k) {
                let tys = self.enum_tys.as_ref()?;
                return Some(Adt { kind: AdtKind::Enum, generics: 0, types: tys.view(), variants: self.enum_variants, fields: self.enum_fields });
            }
            if self.struct_key == Some(k) {
                let tys = self.struct_tys.as_ref()?;
                return Some(Adt { kind: AdtKind::Struct, generics: 0, types: tys.view(), variants: self.struct_variants, fields: self.struct_fields });
            }
            None
        }
        fn repr(&self, _: ItemKey) -> Result<ReprAttrs, Missing> {
            Ok(ReprAttrs::RUST)
        }
        fn discriminant(&self, k: ItemKey, v: u32) -> Result<Option<i128>, Missing> {
            if self.enum_key == Some(k) {
                return Ok(Some(self.enum_discrs.get(v as usize).copied().unwrap_or(i128::from(v))));
            }
            Ok(None)
        }
        fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> {
            Ok(ValidRange::default())
        }
        fn closure(&self, owner: ItemKey, ordinal: u32) -> Option<blaze_build::layout::ClosureFacts<'_>> {
            let ctys = self.closure_tys.as_ref()?;
            let &(_, _, sig, env, kind) = self.closures.iter().find(|&&(o, ord, ..)| o == owner && ord == ordinal)?;
            Some(blaze_build::layout::ClosureFacts { types: ctys.view(), sig, env, kind })
        }
    }

    impl Adts for OracleWorld<'_> {
        fn adt(&self, k: ItemKey) -> Option<Adt<'_>> {
            LayoutFacts::adt(self, k)
        }
        fn facts(&self) -> &dyn LayoutFacts {
            self
        }
        fn int_lit(&self, _: ItemKey, e: ExprId) -> Option<u128> {
            self.ints.iter().find(|(id, _)| *id == e).map(|(_, v)| *v)
        }
        fn float_lit(&self, _: ItemKey, e: ExprId) -> Option<u128> {
            self.floats.iter().find(|(id, _)| *id == e).map(|(_, v)| *v)
        }
        fn str_lit(&self, _: ItemKey, e: ExprId) -> Option<&str> {
            self.strs.iter().find(|(id, _)| *id == e).map(|(_, s)| *s)
        }
        fn int_pat(&self, _: ItemKey, p: PatId) -> Option<u128> {
            self.int_pats.iter().find(|(id, _)| *id == p).map(|(_, v)| *v)
        }
        fn float_pat(&self, _: ItemKey, p: PatId) -> Option<u128> {
            self.float_pats.iter().find(|(id, _)| *id == p).map(|(_, v)| *v)
        }
        fn str_pat(&self, _: ItemKey, p: PatId) -> Option<&[u8]> {
            self.str_pats.iter().find(|(id, _)| *id == p).map(|(_, b)| *b)
        }
        fn panic(&self) -> Panic {
            Panic::Abort
        }
        fn overflow_checks(&self) -> OverflowChecks {
            self.overflow
        }
        fn ub_checks(&self) -> blaze_build::UbChecks {
            blaze_build::UbChecks::Off
        }
        fn drop_impl(&self, _: PTy, _: &mut PortableTys<'_>) -> Result<Option<ItemKey>, blaze_build::BuildError> {
            Ok(None)
        }
        fn abi(&self, k: ItemKey) -> Option<Abi> {
            if self.fn_call_item == Some(k) || self.fn_once_item == Some(k) {
                Some(Abi::RustCall)
            } else {
                Some(Abi::Rust)
            }
        }
        fn deref_fn(&self, _: blaze_typeck::Mutbl) -> Option<ItemKey> {
            self.deref_fn_item
        }
        fn index_fn(&self, _: blaze_typeck::Mutbl) -> Option<ItemKey> {
            self.index_fn_item
        }
        fn fn_trait_method(&self, which: blaze_build::FnTrait) -> Option<ItemKey> {
            match which {
                blaze_build::FnTrait::FnOnce => self.fn_once_item.or(self.fn_call_item),
                _ => self.fn_call_item,
            }
        }
        fn promoted_value(&self, _: ItemKey, borrow: ExprId, _: &[PTy]) -> Option<&PromotedRecord> {
            self.promoted.iter().find(|(e, _)| *e == borrow).map(|(_, r)| r)
        }
    }

    fn base_view<'b>(owner: ItemKey, types: PTys<'b>, root: ExprId) -> BodyView<'b> {
        BodyView {
            owner,
            types,
            root,
            params: &[],
            exprs: &[],
            expr_ranges: &[],
            pats: &[],
            pat_ranges: &[],
            stmts: &[],
            expr_lists: &[],
            pat_lists: &[],
            stmt_lists: &[],
            paths: &[],
            segments: &[],
            opt_types: &[],
            type_lists: &[],
            type_list_ranges: &[],
            segment_ranges: &[],
            method_name_ranges: &[],
            arms: &[],
            field_inits: &[],
            field_pats: &[],
            fmt_uses: &[],
            coroutine: None,
            asm_pieces: &[],
            asm_operands: &[],
            asm_clobbers: &[],
            asm_text: "",
            call_arg_ints: &[],
            legacy_args: &[],
            super_lets: &[],
            unsafe_blocks: &[],
            edition: Edition::E2024,
            expr_editions: &[],
            pat_editions: &[],
            record_expr_schemas: &[],
            closure_param_regions: &[],
            loop_marks: &[],
        }
    }

    fn check_clean(fx: &Fx, bv: &BodyView<'_>) -> Checked<'static> {
        let mut cx = InferCtx::new_in(heap());
        match blaze_typeck::check(bv, fx, &mut cx, &Needs::new_in(heap()), heap()) {
            Ok(CheckOutcome::Done(c)) => {
                assert!(c.diags.is_empty(), "fixture type-checks clean: {:?}", c.diags);
                c
            }
            Ok(CheckOutcome::Need(n)) => panic!("world missed {:?}", n.questions),
            Err(e) => panic!("{e:?}"),
        }
    }

    fn assert_oracle_parity(
        name: &str,
        bv: &BodyView<'_>,
        ck: &Checked<'_>,
        sig: &FnSig<'_>,
        world: &OracleWorld<'_>,
        cases: &[(&[Val], Val)],
    ) {
        let mut g_old = PortableTys::new_in(heap());
        let mut subst_old = Subst { args: &[], ground: &mut g_old, scratch: heap() };
        let old_built = blaze_build::build(bv, ck, sig, &mut subst_old, world, heap())
            .unwrap_or_else(|e| panic!("{name}: old walk failed: {e:?}"));

        let body = blaze_body_lower::lower(bv, ck, world, heap())
            .unwrap_or_else(|e| panic!("{name}: blaze-body-lower failed: {e:?}"));
        let verified = blaze_body::verify(&body)
            .unwrap_or_else(|e| panic!("{name}: blaze-body verify failed: {e:?}"));
        let mut g_new = PortableTys::new_in(heap());
        let mut subst_new = Subst { args: &[], ground: &mut g_new, scratch: heap() };
        let new_built = blaze_body_build::build(verified, ck.types.view(), bv.owner, &mut subst_new, world, heap())
            .unwrap_or_else(|e| panic!("{name}: blaze-body-build failed: {e:?}"));

        for &(args, expected) in cases {
            let old_out = run_in(old_built.func.view(), args, 20_000, heap()).expect("old run");
            let new_out = run_in(new_built.func.view(), args, 20_000, heap()).expect("new run");
            let Outcome::Returned(old_vals) = old_out else { panic!("{name}: old walk did not return for {args:?}") };
            let Outcome::Returned(new_vals) = new_out else { panic!("{name}: D119 build did not return for {args:?}") };
            assert_eq!(old_vals.as_slice(), &[expected], "{name}: old walk mismatch for {args:?}");
            assert_eq!(new_vals.as_slice(), old_vals.as_slice(), "{name}: D119 vs old walk divergence for {args:?}");
        }
    }

    fn plain_sig<'t>(inputs: &'t [PTy], output: PTy, types: PTys<'t>) -> FnSig<'t> {
        FnSig {
            parent_count: 0,
            own_count: 0,
            declared_count: 0,
            self_param: false,
            header: FnPtrHeader::RUST,
            c_variadic: false,
            legacy_const_generics: &[],
            inputs,
            output,
            types,
        }
    }

    /// `let 10 = (match x { 1..=5 => 10, 6 | 7 => 20, _ => 30 }) else { return x + 100; };`
    /// `let [first, .., last] = [x, 100, 200]; first + last`
    /// Verified identical between old walk (`blaze-build`) and D119 (`blaze-body-lower` -> `blaze-body-build`).
    #[test]
    fn oracle_parity_let_else_range_or_and_slice_pattern() {
        let mut fx = Fx::new();
        let u32_t = fx.u32();
        let owner = fx.func("classify", 0, &[u32_t], u32_t, Default::default());
        fx.finish();
        let mut sig_tys = PortableTys::new_in(heap());
        let s_u32 = sig_tys.intern(PNode::Uint(UintTy::U32)).unwrap();
        let sig_in = [s_u32];
        let sig = plain_sig(&sig_in, s_u32, sig_tys.view());

        let body_tys = PortableTys::new_in(heap());
        let mk_bind = |name: &str| Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of(name), sub: None, ty: None };
        let lit_u32 = || Pat::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32))));
        let pats = [
            mk_bind("x"),                                                   // 0
            lit_u32(),                                                      // 1: 1
            lit_u32(),                                                      // 2: 5
            Pat::Range { lo: Some(PatId(1)), hi: Some(PatId(2)), inclusive: true }, // 3: 1..=5
            lit_u32(),                                                      // 4: 6
            lit_u32(),                                                      // 5: 7
            Pat::Or { alts: List { start: 0, len: 2 } },                    // 6: 6 | 7
            Pat::Wild,                                                      // 7: _
            lit_u32(),                                                      // 8: 10
            mk_bind("first"),                                               // 9
            Pat::Wild,                                                      // 10: ..
            mk_bind("last"),                                                // 11
            Pat::Slice { elems: List { start: 2, len: 3 }, rest: Some(1) }, // 12: [first, .., last]
        ];
        let pat_lists = [PatId(4), PatId(5), PatId(9), PatId(10), PatId(11)];
        let pat_ranges = make_ranges(pats.len());

        let exprs = [
            Expr::Local(PatId(0)),                                          // 0: x
            Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),        // 1: 10
            Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),        // 2: 20
            Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),        // 3: 30
            Expr::Match { scrutinee: ExprId(0), arms: List { start: 0, len: 3 } }, // 4
            Expr::Local(PatId(0)),                                          // 5: x
            Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),        // 6: 100
            Expr::Binary { op: BinOp::Add, lhs: ExprId(5), rhs: ExprId(6) },// 7: x + 100
            Expr::Return { value: Some(ExprId(7)) },                        // 8: return x + 100
            Expr::Local(PatId(0)),                                          // 9: x
            Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),        // 10: 100
            Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),        // 11: 200
            Expr::Array { elems: List { start: 0, len: 3 } },               // 12: [x, 100, 200]
            Expr::Local(PatId(9)),                                          // 13: first
            Expr::Local(PatId(11)),                                         // 14: last
            Expr::Binary { op: BinOp::Add, lhs: ExprId(13), rhs: ExprId(14) }, // 15: first + last
            Expr::Block { stmts: List { start: 0, len: 2 }, tail: Some(ExprId(15)), scope: ScopeId(0) }, // 16
        ];
        let expr_lists = [ExprId(9), ExprId(10), ExprId(11)];
        let expr_ranges = make_ranges(exprs.len());
        let arms = [
            Arm { pat: PatId(3), guard: None, body: ExprId(1) },
            Arm { pat: PatId(6), guard: None, body: ExprId(2) },
            Arm { pat: PatId(7), guard: None, body: ExprId(3) },
        ];
        let stmts = [
            Stmt::Let { pat: PatId(8), ty: None, init: Some(ExprId(4)), else_: Some(ExprId(8)) },
            Stmt::Let { pat: PatId(12), ty: None, init: Some(ExprId(12)), else_: None },
        ];
        let stmt_lists = [StmtId(0), StmtId(1)];
        let ints = [
            (ExprId(1), 10),
            (ExprId(2), 20),
            (ExprId(3), 30),
            (ExprId(6), 100),
            (ExprId(10), 100),
            (ExprId(11), 200),
        ];
        let int_pats = [(PatId(1), 1), (PatId(2), 5), (PatId(4), 6), (PatId(5), 7), (PatId(8), 10)];
        let params = [PatId(0)];
        let mut bv = base_view(owner, body_tys.view(), ExprId(16));
        bv.params = &params;
        bv.pats = &pats;
        bv.pat_lists = &pat_lists;
        bv.pat_ranges = &pat_ranges;
        bv.exprs = &exprs;
        bv.expr_lists = &expr_lists;
        bv.expr_ranges = &expr_ranges;
        bv.arms = &arms;
        bv.stmts = &stmts;
        bv.stmt_lists = &stmt_lists;
        bv.call_arg_ints = &ints;

        let ck = check_clean(&fx, &bv);
        let world = OracleWorld::new(OverflowChecks::Off, &ints, &int_pats);
        assert_oracle_parity(
            "let_else_range_or_slice",
            &bv,
            &ck,
            &sig,
            &world,
            &[
                (&[Val::Int(0)], Val::Int(100)),
                (&[Val::Int(1)], Val::Int(201)),
                (&[Val::Int(3)], Val::Int(203)),
                (&[Val::Int(5)], Val::Int(205)),
                (&[Val::Int(6)], Val::Int(106)),
                (&[Val::Int(7)], Val::Int(107)),
                (&[Val::Int(42)], Val::Int(142)),
            ],
        );
    }

    /// Counted loop with `break acc;` and `OverflowChecks::Off`: sums `0..n`, verified identical
    /// between old walk (`blaze-build`) and D119 (`blaze-body-lower` -> `blaze-body-build`).
    #[test]
    fn oracle_parity_loop_break_value_and_wrapping_off() {
        let mut fx = Fx::new();
        let i64_t = fx.ty(PNode::Int(IntTy::I64));
        let owner = fx.func("sum_up", 0, &[i64_t], i64_t, Default::default());
        fx.finish();
        let mut sig_tys = PortableTys::new_in(heap());
        let s_i64 = sig_tys.intern(PNode::Int(IntTy::I64)).unwrap();
        let sig_in = [s_i64];
        let sig = plain_sig(&sig_in, s_i64, sig_tys.view());

        let body_tys = PortableTys::new_in(heap());
        let mk_bind = |name: &str, mutable: bool| Pat::Bind {
            ann: BindAnn { by_ref: None, mutable },
            name: Name::of(name),
            sub: None,
            ty: None,
        };
        let pats = [mk_bind("n", false), mk_bind("acc", true), mk_bind("i", true)];
        let pat_ranges = make_ranges(pats.len());
        let exprs = [
            Expr::Lit(Lit::Int(Some(IntSuffix::Int(IntTy::I64)))),          // 0: 0i64
            Expr::Lit(Lit::Int(Some(IntSuffix::Int(IntTy::I64)))),          // 1: 0i64
            Expr::Local(PatId(2)),                                          // 2: i
            Expr::Local(PatId(0)),                                          // 3: n
            Expr::Binary { op: BinOp::Ge, lhs: ExprId(2), rhs: ExprId(3) }, // 4: i >= n
            Expr::Local(PatId(1)),                                          // 5: acc
            Expr::Break { value: Some(ExprId(5)), target: ExprId(17) },     // 6: break acc
            Expr::Block { stmts: List { start: 0, len: 1 }, tail: None, scope: ScopeId(1) }, // 7: { break acc; }
            Expr::If { cond: ExprId(4), then: ExprId(7), else_: None },     // 8: if i >= n { break acc; }
            Expr::Local(PatId(1)),                                          // 9: acc
            Expr::Local(PatId(1)),                                          // 10: acc
            Expr::Local(PatId(2)),                                          // 11: i
            Expr::Binary { op: BinOp::Add, lhs: ExprId(10), rhs: ExprId(11) }, // 12: acc + i
            Expr::Assign { lhs: ExprId(9), rhs: ExprId(12) },               // 13: acc = acc + i
            Expr::Local(PatId(2)),                                          // 14: i
            Expr::Lit(Lit::Int(Some(IntSuffix::Int(IntTy::I64)))),          // 15: 1i64
            Expr::AssignOp { op: BinOp::Add, lhs: ExprId(14), rhs: ExprId(15) }, // 16: i += 1
            Expr::Loop { body: ExprId(18) },                                // 17: loop { .. }
            Expr::Block { stmts: List { start: 1, len: 3 }, tail: None, scope: ScopeId(2) }, // 18: loop body
            Expr::Block { stmts: List { start: 4, len: 2 }, tail: Some(ExprId(17)), scope: ScopeId(0) }, // 19: root
        ];
        let expr_ranges = make_ranges(exprs.len());
        let stmts = [
            Stmt::Expr { expr: ExprId(6), semi: true },                     // 0: break acc;
            Stmt::Expr { expr: ExprId(8), semi: false },                    // 1: if i >= n { break acc; }
            Stmt::Expr { expr: ExprId(13), semi: true },                    // 2: acc = acc + i;
            Stmt::Expr { expr: ExprId(16), semi: true },                    // 3: i += 1;
            Stmt::Let { pat: PatId(1), ty: None, init: Some(ExprId(0)), else_: None }, // 4: let mut acc = 0;
            Stmt::Let { pat: PatId(2), ty: None, init: Some(ExprId(1)), else_: None }, // 5: let mut i = 0;
        ];
        let stmt_lists = [StmtId(0), StmtId(1), StmtId(2), StmtId(3), StmtId(4), StmtId(5)];
        let ints = [(ExprId(0), 0), (ExprId(1), 0), (ExprId(15), 1)];
        let params = [PatId(0)];
        let mut bv = base_view(owner, body_tys.view(), ExprId(19));
        bv.params = &params;
        bv.pats = &pats;
        bv.pat_ranges = &pat_ranges;
        bv.exprs = &exprs;
        bv.expr_ranges = &expr_ranges;
        bv.stmts = &stmts;
        bv.stmt_lists = &stmt_lists;
        bv.call_arg_ints = &ints;

        let ck = check_clean(&fx, &bv);
        let world = OracleWorld::new(OverflowChecks::Off, &ints, &[]);
        assert_oracle_parity(
            "loop_break_value",
            &bv,
            &ck,
            &sig,
            &world,
            &[
                (&[Val::Int(0)], Val::Int(0)),
                (&[Val::Int(1)], Val::Int(0)),
                (&[Val::Int(5)], Val::Int(10)),
                (&[Val::Int(10)], Val::Int(45)),
                (&[Val::Int(100)], Val::Int(4950)),
            ],
        );
    }

    /// Enum-to-integer cast (`E::B as i64`) + struct literal & field read (`S { a: x, b: 10 }.a`),
    /// verified identical between old walk (`blaze-build`) and D119 (`blaze-body-lower` -> `blaze-body-build`).
    #[test]
    fn oracle_parity_enum_cast_and_struct_fields() {
        let mut fx = Fx::new();
        let i64_t = fx.ty(PNode::Int(IntTy::I64));
        let (ek, evs) = fx.enum_("E", 0, &[("A", &[]), ("B", &[])]);
        let sk = fx.strukt("S", 0, &[("a", i64_t), ("b", i64_t)]);
        let owner = fx.func("mix", 0, &[i64_t], i64_t, Default::default());
        fx.finish();
        let mut sig_tys = PortableTys::new_in(heap());
        let s_i64 = sig_tys.intern(PNode::Int(IntTy::I64)).unwrap();
        let sig_in = [s_i64];
        let sig = plain_sig(&sig_in, s_i64, sig_tys.view());

        let mut body_tys = PortableTys::new_in(heap());
        let b_i64 = body_tys.intern(PNode::Int(IntTy::I64)).unwrap();
        let mk_bind = |name: &str| Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of(name), sub: None, ty: None };
        let pats = [mk_bind("x"), mk_bind("s")];
        let pat_ranges = make_ranges(pats.len());
        let segments = [
            Segment { name: Name::of("E"), generic_args: List::EMPTY, has_args: false },
            Segment { name: Name::of("B"), generic_args: List::EMPTY, has_args: false },
            Segment { name: Name::of("S"), generic_args: List::EMPTY, has_args: false },
        ];
        let paths = [
            BodyPath { scope: ScopeId(0), segments: List { start: 0, len: 2 }, lang: None, anchor: None }, // 0: E::B
            BodyPath { scope: ScopeId(0), segments: List { start: 2, len: 1 }, lang: None, anchor: None }, // 1: S
        ];
        let exprs = [
            Expr::Local(PatId(0)),                                          // 0: x
            Expr::Lit(Lit::Int(Some(IntSuffix::Int(IntTy::I64)))),          // 1: 10i64
            Expr::Struct { path: PathIdx(1), fields: List { start: 0, len: 2 }, base: StructBase::None }, // 2: S { a: x, b: 10 }
            Expr::Path(PathIdx(0)),                                         // 3: E::B
            Expr::Cast { operand: ExprId(3), ty: b_i64 },                   // 4: E::B as i64 (20)
            Expr::Local(PatId(1)),                                          // 5: s
            Expr::Field { base: ExprId(5), name: Name::of("a") },           // 6: s.a
            Expr::Local(PatId(1)),                                          // 7: s
            Expr::Field { base: ExprId(7), name: Name::of("b") },           // 8: s.b
            Expr::Binary { op: BinOp::Add, lhs: ExprId(4), rhs: ExprId(6) },// 9: (E::B as i64) + s.a
            Expr::Binary { op: BinOp::Add, lhs: ExprId(9), rhs: ExprId(8) },// 10: ((E::B as i64) + s.a) + s.b
            Expr::Block { stmts: List { start: 0, len: 1 }, tail: Some(ExprId(10)), scope: ScopeId(0) }, // 11
        ];
        let field_inits = [
            FieldInit { name: Name::of("a"), expr: ExprId(0) },
            FieldInit { name: Name::of("b"), expr: ExprId(1) },
        ];
        let expr_ranges = make_ranges(exprs.len());
        let stmts = [Stmt::Let { pat: PatId(1), ty: None, init: Some(ExprId(2)), else_: None }];
        let stmt_lists = [StmtId(0)];
        let ints = [(ExprId(1), 10)];
        let params = [PatId(0)];
        let mut bv = base_view(owner, body_tys.view(), ExprId(11));
        bv.params = &params;
        bv.pats = &pats;
        bv.pat_ranges = &pat_ranges;
        bv.segments = &segments;
        bv.paths = &paths;
        bv.exprs = &exprs;
        bv.field_inits = &field_inits;
        bv.expr_ranges = &expr_ranges;
        bv.stmts = &stmts;
        bv.stmt_lists = &stmt_lists;
        bv.call_arg_ints = &ints;

        let ck = check_clean(&fx, &bv);
        let enum_variants = [
            Variant { key: evs[0], name: Name::of("A"), ctor: CtorKind::Unit, fields: List { start: 0, len: 0 } },
            Variant { key: evs[1], name: Name::of("B"), ctor: CtorKind::Unit, fields: List { start: 0, len: 0 } },
        ];
        let mut st = PortableTys::new_in(heap());
        let si64 = st.intern(PNode::Int(IntTy::I64)).unwrap();
        let struct_fields = [
            Field { name: Name::of("a"), ty: si64 },
            Field { name: Name::of("b"), ty: si64 },
        ];
        let struct_variants = [Variant { key: sk, name: Name::of("S"), ctor: CtorKind::Braces, fields: List { start: 0, len: 2 } }];
        let mut world = OracleWorld::new(OverflowChecks::Off, &ints, &[]);
        let et = PortableTys::new_in(heap());
        world.enum_key = Some(ek);
        world.enum_tys = Some(et);
        world.enum_variants = &enum_variants;
        world.struct_key = Some(sk);
        world.struct_tys = Some(st);
        world.struct_fields = &struct_fields;
        world.struct_variants = &struct_variants;
        let _ = Def::Adt(ek);

        assert_oracle_parity(
            "enum_cast_and_struct",
            &bv,
            &ck,
            &sig,
            &world,
            &[
                (&[Val::Int(12)], Val::Int(42)),
                (&[Val::Int(0)], Val::Int(30)),
                (&[Val::Int(u128::from((-5i64).cast_unsigned()))], Val::Int(25)),
            ],
        );
    }

    /// Binding `OrPattern` (`E::A(v) | E::B(v)`) + `LetChain` (`if let ... && v > 0 && let ...`)
    /// + float & string `ValuePattern`: verified byte-identical between old walk (`blaze-build`)
    /// and D119 (`blaze-body-lower` -> `blaze-body-build`), AND clean through `blaze-body-flow`!
    #[test]
    fn oracle_parity_binding_or_let_chain_and_float_str_value_patterns() {
        use blaze_typeck::FloatTy;
        let mut fx = Fx::new();
        let i64_t = fx.ty(PNode::Int(IntTy::I64));
        let f64_t = fx.ty(PNode::Float(FloatTy::F64));
        let (ek, evs) = fx.enum_("E", 0, &[("A", &[i64_t]), ("B", &[i64_t]), ("C", &[])]);
        let owner = fx.func("chain_val", 0, &[i64_t, i64_t, f64_t], i64_t, Default::default());
        fx.finish();
        let mut sig_tys = PortableTys::new_in(heap());
        let s_i64 = sig_tys.intern(PNode::Int(IntTy::I64)).unwrap();
        let s_f64 = sig_tys.intern(PNode::Float(FloatTy::F64)).unwrap();
        let sig_in = [s_i64, s_i64, s_f64];
        let sig = plain_sig(&sig_in, s_i64, sig_tys.view());

        let body_tys = PortableTys::new_in(heap());
        let mk_bind = |name: &str| Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of(name), sub: None, ty: None };
        let segments = [
            Segment { name: Name::of("E"), generic_args: List::EMPTY, has_args: false },
            Segment { name: Name::of("A"), generic_args: List::EMPTY, has_args: false },
            Segment { name: Name::of("E"), generic_args: List::EMPTY, has_args: false },
            Segment { name: Name::of("B"), generic_args: List::EMPTY, has_args: false },
            Segment { name: Name::of("E"), generic_args: List::EMPTY, has_args: false },
            Segment { name: Name::of("C"), generic_args: List::EMPTY, has_args: false },
        ];
        let paths = [
            BodyPath { scope: ScopeId(0), segments: List { start: 0, len: 2 }, lang: None, anchor: None }, // 0: E::A
            BodyPath { scope: ScopeId(0), segments: List { start: 2, len: 2 }, lang: None, anchor: None }, // 1: E::B
            BodyPath { scope: ScopeId(0), segments: List { start: 4, len: 2 }, lang: None, anchor: None }, // 2: E::C
        ];
        let pats = [
            mk_bind("kind"),                                                 // 0: param kind (0->A, 1->B, else C)
            mk_bind("val"),                                                  // 1: param val
            mk_bind("fl"),                                                   // 2: param fl
            mk_bind("e"),                                                    // 3: let e = ...
            mk_bind("v"),                                                    // 4: v in E::A(v)
            Pat::TupleStruct { path: PathIdx(0), elems: List { start: 0, len: 1 }, rest: None }, // 5: E::A(v)
            mk_bind("v"),                                                    // 6: v in E::B(v)
            Pat::TupleStruct { path: PathIdx(1), elems: List { start: 1, len: 1 }, rest: None }, // 7: E::B(v)
            Pat::Or { alts: List { start: 2, len: 2 } },                     // 8: E::A(v) | E::B(v)
            Pat::NegLit(Lit::Float(Some(FloatTy::F64))),                     // 9: -1.5f64
            Pat::Lit(Lit::Str),                                              // 10: "hi"
        ];
        let pat_lists = [PatId(4), PatId(6), PatId(5), PatId(7)];
        let pat_ranges = make_ranges(pats.len());
        let exprs = [
            Expr::Local(PatId(0)),                                           // 0: kind
            Expr::Lit(Lit::Int(Some(IntSuffix::Int(IntTy::I64)))),           // 1: 0
            Expr::Binary { op: BinOp::Eq, lhs: ExprId(0), rhs: ExprId(1) },  // 2: kind == 0
            Expr::Path(PathIdx(0)),                                          // 3: E::A
            Expr::Local(PatId(1)),                                           // 4: val
            Expr::Call { callee: ExprId(3), args: List { start: 0, len: 1 } }, // 5: E::A(val)
            Expr::Local(PatId(0)),                                           // 6: kind
            Expr::Lit(Lit::Int(Some(IntSuffix::Int(IntTy::I64)))),           // 7: 1
            Expr::Binary { op: BinOp::Eq, lhs: ExprId(6), rhs: ExprId(7) },  // 8: kind == 1
            Expr::Path(PathIdx(1)),                                          // 9: E::B
            Expr::Local(PatId(1)),                                           // 10: val
            Expr::Call { callee: ExprId(9), args: List { start: 1, len: 1 } }, // 11: E::B(val)
            Expr::Path(PathIdx(2)),                                          // 12: E::C
            Expr::If { cond: ExprId(8), then: ExprId(11), else_: Some(ExprId(12)) }, // 13
            Expr::If { cond: ExprId(2), then: ExprId(5), else_: Some(ExprId(13)) },  // 14: e init
            // LetChain: `if let E::A(v) | E::B(v) = e && v > 0 && let -1.5 = fl && let "hi" = "hi" { v + 100 } else { 7 }`
            Expr::Local(PatId(3)),                                           // 15: e
            Expr::Let { pat: PatId(8), init: ExprId(15) },                   // 16: let E::A(v) | E::B(v) = e
            Expr::Local(PatId(4)),                                           // 17: v
            Expr::Lit(Lit::Int(Some(IntSuffix::Int(IntTy::I64)))),           // 18: 0
            Expr::Binary { op: BinOp::Gt, lhs: ExprId(17), rhs: ExprId(18) },// 19: v > 0
            Expr::Binary { op: BinOp::And, lhs: ExprId(16), rhs: ExprId(19) }, // 20
            Expr::Local(PatId(2)),                                           // 21: fl
            Expr::Let { pat: PatId(9), init: ExprId(21) },                   // 22: let -1.5 = fl
            Expr::Binary { op: BinOp::And, lhs: ExprId(20), rhs: ExprId(22) }, // 23
            Expr::Lit(Lit::Str),                                             // 24: "hi"
            Expr::Let { pat: PatId(10), init: ExprId(24) },                  // 25: let "hi" = "hi"
            Expr::Binary { op: BinOp::And, lhs: ExprId(23), rhs: ExprId(25) }, // 26
            Expr::Local(PatId(4)),                                           // 27: v
            Expr::Lit(Lit::Int(Some(IntSuffix::Int(IntTy::I64)))),           // 28: 100
            Expr::Binary { op: BinOp::Add, lhs: ExprId(27), rhs: ExprId(28) }, // 29: v + 100
            Expr::Lit(Lit::Int(Some(IntSuffix::Int(IntTy::I64)))),           // 30: 7
            Expr::If { cond: ExprId(26), then: ExprId(29), else_: Some(ExprId(30)) }, // 31
            Expr::Block { stmts: List { start: 0, len: 1 }, tail: Some(ExprId(31)), scope: ScopeId(0) }, // 32
        ];
        let expr_lists = [ExprId(4), ExprId(10)];
        let expr_ranges = make_ranges(exprs.len());
        let stmts = [Stmt::Let { pat: PatId(3), ty: None, init: Some(ExprId(14)), else_: None }];
        let stmt_lists = [StmtId(0)];
        let ints = [(ExprId(1), 0), (ExprId(7), 1), (ExprId(18), 0), (ExprId(28), 100), (ExprId(30), 7)];
        let strs = [(ExprId(24), "hi")];
        let float_pats = [(PatId(9), u128::from(1.5f64.to_bits()))];
        let str_pats: &[(PatId, &[u8])] = &[(PatId(10), b"hi")];
        let params = [PatId(0), PatId(1), PatId(2)];
        let mut bv = base_view(owner, body_tys.view(), ExprId(32));
        bv.params = &params;
        bv.segments = &segments;
        bv.paths = &paths;
        bv.pats = &pats;
        bv.pat_lists = &pat_lists;
        bv.pat_ranges = &pat_ranges;
        bv.exprs = &exprs;
        bv.expr_lists = &expr_lists;
        bv.expr_ranges = &expr_ranges;
        bv.stmts = &stmts;
        bv.stmt_lists = &stmt_lists;
        bv.call_arg_ints = &ints;

        let ck = check_clean(&fx, &bv);
        let mut et = PortableTys::new_in(heap());
        let ei64 = et.intern(PNode::Int(IntTy::I64)).unwrap();
        let enum_fields = [
            Field { name: Name::of("0"), ty: ei64 },
            Field { name: Name::of("0"), ty: ei64 },
        ];
        let enum_variants = [
            Variant { key: evs[0], name: Name::of("A"), ctor: CtorKind::Tuple, fields: List { start: 0, len: 1 } },
            Variant { key: evs[1], name: Name::of("B"), ctor: CtorKind::Tuple, fields: List { start: 1, len: 1 } },
            Variant { key: evs[2], name: Name::of("C"), ctor: CtorKind::Unit, fields: List { start: 2, len: 0 } },
        ];
        let mut world = OracleWorld::new(OverflowChecks::Off, &ints, &[]);
        world.strs = &strs;
        world.float_pats = &float_pats;
        world.str_pats = str_pats;
        world.enum_key = Some(ek);
        world.enum_tys = Some(et);
        world.enum_variants = &enum_variants;
        world.enum_fields = &enum_fields;
        world.enum_discrs = &[0, 1, 2];

        // Also prove the lowered body projects cleanly through `blaze-body-flow`!
        let lowered = blaze_body_lower::lower(&bv, &ck, &world, heap()).expect("lowers");
        let verified = blaze_body::verify(&lowered).expect("verifies");
        let proj = crate::project(verified, heap()).expect("projects through blaze-body-flow");
        let check_res = blaze_flow::check(&proj.flow, heap()).expect("borrowck within bounds");
        assert!(check_res.moves.is_empty(), "{:?}", check_res.moves);

        let neg15 = Val::Int(u128::from((-1.5f64).to_bits()));
        let pos15 = Val::Int(u128::from((1.5f64).to_bits()));
        let nan = Val::Int(u128::from(f64::NAN.to_bits()));
        assert_oracle_parity(
            "binding_or_let_chain_value_pats",
            &bv,
            &ck,
            &sig,
            &world,
            &[
                (&[Val::Int(0), Val::Int(5), neg15], Val::Int(105)),
                (&[Val::Int(1), Val::Int(9), neg15], Val::Int(109)),
                (&[Val::Int(0), Val::Int(0), neg15], Val::Int(7)),
                (&[Val::Int(2), Val::Int(5), neg15], Val::Int(7)),
                (&[Val::Int(0), Val::Int(5), pos15], Val::Int(7)),
                (&[Val::Int(0), Val::Int(5), nan], Val::Int(7)),
            ],
        );
    }

    /// Promoted borrow `&42i64` (`Checked::promoted` -> `ConstKind::Promoted` -> `Adts::promoted_value`):
    /// verified byte-identical between old walk (`blaze-build`) and D119 (`blaze-body-lower` -> `blaze-body-build`).
    #[test]
    fn oracle_parity_promoted_borrow() {
        let mut fx = Fx::new();
        let i64_t = fx.ty(PNode::Int(IntTy::I64));
        let owner = fx.func("prom", 0, &[i64_t], i64_t, Default::default());
        fx.finish();
        let mut sig_tys = PortableTys::new_in(heap());
        let s_i64 = sig_tys.intern(PNode::Int(IntTy::I64)).unwrap();
        let sig_in = [s_i64];
        let sig = plain_sig(&sig_in, s_i64, sig_tys.view());

        let body_tys = PortableTys::new_in(heap());
        let mk_bind = |name: &str| Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of(name), sub: None, ty: None };
        let pats = [mk_bind("x")];
        let pat_ranges = make_ranges(pats.len());
        let exprs = [
            Expr::Lit(Lit::Int(Some(IntSuffix::Int(IntTy::I64)))),           // 0: 42i64
            Expr::Ref { mutbl: blaze_typeck::Mutbl::Not, operand: ExprId(0) }, // 1: &42i64 (promoted!)
            Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: ExprId(1) }, // 2: *(&42i64)
            Expr::Local(PatId(0)),                                           // 3: x
            Expr::Binary { op: BinOp::Add, lhs: ExprId(2), rhs: ExprId(3) }, // 4: *(&42) + x
        ];
        let expr_ranges = make_ranges(exprs.len());
        let ints = [(ExprId(0), 42)];
        let params = [PatId(0)];
        let mut bv = base_view(owner, body_tys.view(), ExprId(4));
        bv.params = &params;
        bv.pats = &pats;
        bv.pat_ranges = &pat_ranges;
        bv.exprs = &exprs;
        bv.expr_ranges = &expr_ranges;
        bv.call_arg_ints = &ints;

        let ck = check_clean(&fx, &bv);
        assert!(ck.promoted.scan_for(&ExprId(1)).is_some(), "expected ExprId(1) in Checked::promoted: {:?}", ck.promoted);
        let bytes = 42i64.to_le_bytes().to_vec();
        let allocs = core::iter::once(blaze_build::data::DataAlloc { bytes, align: 3, mutable: false, relocs: Default::default() }).collect();
        let promoted = [(ExprId(1), blaze_build::data::DataRecord { allocs })];
        let mut world = OracleWorld::new(OverflowChecks::Off, &ints, &[]);
        world.promoted = &promoted;

        assert_oracle_parity(
            "promoted_borrow",
            &bv,
            &ck,
            &sig,
            &world,
            &[
                (&[Val::Int(0)], Val::Int(42)),
                (&[Val::Int(8)], Val::Int(50)),
                (&[Val::Int(u128::from((-42i64).cast_unsigned()))], Val::Int(0)),
            ],
        );
    }

    /// Closures (`lower_closure` + `CallThroughFnTrait`), `ClosureFnPointer` coercion,
    /// `OverloadedDeref`, and `OverloadedIndex`: built through BOTH the old walk (`blaze-build`)
    /// and D119 (`blaze-body-lower` -> `blaze-body-build`), linked with real callee bodies, and
    /// executed in `blaze_lir::eval::run_calls_in` to prove identical runtime outputs!
    #[test]
    fn oracle_parity_closures_fn_trait_deref_index_and_closure_fn_ptr() {
        use blaze_lir::builder::Variadic;
        use blaze_lir::eval::{Bodies, BodyId, run_calls_in};
        use blaze_lir::func::{BlockKind, Conv, RefId};
        use blaze_lir::{Builder, Full, View};
        use blaze_typeck::{LangItem, Mutbl, UnOp};

        const DEREF_ITEM: ItemKey = ItemKey(501);
        const INDEX_ITEM: ItemKey = ItemKey(502);
        const FN_CALL_ITEM: ItemKey = ItemKey(503);
        const FN_ONCE_ITEM: ItemKey = ItemKey(504);

        let mut fx = Fx::new();
        let i64_t = fx.ty(PNode::Int(IntTy::I64));
        let usize_t = fx.usize();
        let copy_tr = fx.tr("Copy", 1, Default::default(), &[]);
        fx.lang(LangItem::Copy, copy_tr);
        fx.imp("impl Copy for i64", 0, i64_t, Some((copy_tr, &[])), &[], Default::default());
        let deref_tr = fx.tr("Deref", 1, Default::default(), &["Target"]);
        fx.lang(LangItem::Deref, deref_tr);
        let index_tr = fx.tr("Index", 2, Default::default(), &["Output"]);
        fx.lang(LangItem::Index, index_tr);
        let wrap_k = fx.strukt("Wrap", 0, &[("v", i64_t)]);
        let wrap_t = fx.adt(wrap_k, &[]);
        fx.imp("impl Deref for Wrap", 0, wrap_t, Some((deref_tr, &[])), &[("Target", i64_t)], Default::default());
        fx.imp("impl Index<usize> for Wrap", 0, wrap_t, Some((index_tr, &[usize_t])), &[("Output", i64_t)], Default::default());
        let owner = fx.func("run_all", 0, &[i64_t], i64_t, Default::default());
        fx.finish();

        let mut sig_tys = PortableTys::new_in(heap());
        let s_i64 = sig_tys.intern(PNode::Int(IntTy::I64)).unwrap();
        let sig_in = [s_i64];
        let sig = plain_sig(&sig_in, s_i64, sig_tys.view());

        let mut body_tys = PortableTys::new_in(heap());
        let b_i64 = body_tys.intern(PNode::Int(IntTy::I64)).unwrap();
        let b_fpl = body_tys.intern_list(&[b_i64, b_i64]).unwrap();
        let b_fnptr = body_tys.intern(PNode::FnPtr(b_fpl, FnPtrHeader::RUST)).unwrap();

        let mk_bind = |name: &str| Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of(name), sub: None, ty: None };
        let segments = [Segment { name: Name::of("Wrap"), generic_args: List::EMPTY, has_args: false }];
        let paths = [BodyPath { scope: ScopeId(0), segments: List { start: 0, len: 1 }, lang: None, anchor: None }];
        let pats = [
            mk_bind("x"),   // 0: param x
            mk_bind("w"),   // 1: let w = Wrap { v: x }
            mk_bind("fp"),  // 2: let fp: fn(i64) -> i64 = |a| a + 10
            mk_bind("a"),   // 3: closure0 param a
            mk_bind("c"),   // 4: let c = |b| b + x
            mk_bind("b"),   // 5: closure1 param b
        ];
        let pat_lists = [PatId(3), PatId(5)];
        let pat_ranges = make_ranges(pats.len());
        let opt_types = [Some(b_i64)];
        let exprs = [
            Expr::Local(PatId(0)),                                           // 0: x
            Expr::Struct { path: PathIdx(0), fields: List { start: 0, len: 1 }, base: StructBase::None }, // 1: Wrap { v: x }
            Expr::Local(PatId(3)),                                           // 2: a
            Expr::Lit(Lit::Int(Some(IntSuffix::Int(IntTy::I64)))),           // 3: 10
            Expr::Binary { op: BinOp::Add, lhs: ExprId(2), rhs: ExprId(3) }, // 4: a + 10
            Expr::Closure { params: List { start: 0, len: 1 }, param_tys: List::EMPTY, ret: None, body: ExprId(4), is_move: false, is_async: false }, // 5: |a| a + 10
            Expr::Local(PatId(5)),                                           // 6: b
            Expr::Local(PatId(0)),                                           // 7: x (captured by ref)
            Expr::Binary { op: BinOp::Add, lhs: ExprId(6), rhs: ExprId(7) }, // 8: b + x
            Expr::Closure { params: List { start: 1, len: 1 }, param_tys: List { start: 0, len: 1 }, ret: None, body: ExprId(8), is_move: false, is_async: false }, // 9: |b: i64| b + x
            // Tail: `fp(*w) + c(w[2usize])`
            Expr::Local(PatId(2)),                                           // 10: fp
            Expr::Local(PatId(1)),                                           // 11: w
            Expr::Unary { op: UnOp::Deref, operand: ExprId(11) },            // 12: *w
            Expr::Call { callee: ExprId(10), args: List { start: 0, len: 1 } }, // 13: fp(*w)
            Expr::Local(PatId(4)),                                           // 14: c
            Expr::Local(PatId(1)),                                           // 15: w
            Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::Usize)))),       // 16: 2usize
            Expr::Index { base: ExprId(15), index: ExprId(16) },             // 17: w[2usize]
            Expr::Call { callee: ExprId(14), args: List { start: 1, len: 1 } }, // 18: c(w[2usize])
            Expr::Binary { op: BinOp::Add, lhs: ExprId(13), rhs: ExprId(18) }, // 19: fp(*w) + c(w[2])
            Expr::Block { stmts: List { start: 0, len: 3 }, tail: Some(ExprId(19)), scope: ScopeId(0) }, // 20
        ];
        let expr_lists = [ExprId(12), ExprId(17)];
        let expr_ranges = make_ranges(exprs.len());
        let field_inits = [FieldInit { name: Name::of("v"), expr: ExprId(0) }];
        let stmts = [
            Stmt::Let { pat: PatId(1), ty: None, init: Some(ExprId(1)), else_: None },
            Stmt::Let { pat: PatId(2), ty: Some(b_fnptr), init: Some(ExprId(5)), else_: None },
            Stmt::Let { pat: PatId(4), ty: None, init: Some(ExprId(9)), else_: None },
        ];
        let stmt_lists = [StmtId(0), StmtId(1), StmtId(2)];
        let ints = [(ExprId(3), 10), (ExprId(16), 2)];
        let params = [PatId(0)];
        let mut bv = base_view(owner, body_tys.view(), ExprId(20));
        bv.params = &params;
        bv.segments = &segments;
        bv.paths = &paths;
        bv.opt_types = &opt_types;
        bv.pats = &pats;
        bv.pat_lists = &pat_lists;
        bv.pat_ranges = &pat_ranges;
        bv.exprs = &exprs;
        bv.expr_lists = &expr_lists;
        bv.expr_ranges = &expr_ranges;
        bv.field_inits = &field_inits;
        bv.stmts = &stmts;
        bv.stmt_lists = &stmt_lists;
        bv.call_arg_ints = &ints;

        let ck = check_clean(&fx, &bv);

        let mut st = PortableTys::new_in(heap());
        let si64 = st.intern(PNode::Int(IntTy::I64)).unwrap();
        let struct_fields = [Field { name: Name::of("v"), ty: si64 }];
        let struct_variants = [Variant { key: wrap_k, name: Name::of("Wrap"), ctor: CtorKind::Braces, fields: List { start: 0, len: 1 } }];

        let mut ctys = PortableTys::new_in(heap());
        let ci64 = ctys.intern(PNode::Int(IntTy::I64)).unwrap();
        let cref_i64 = ctys.intern(PNode::Ref(Mutbl::Not, ci64)).unwrap();
        let csig_l = ctys.intern_list(&[ci64, ci64]).unwrap();
        let csig = ctys.intern(PNode::FnPtr(csig_l, FnPtrHeader::RUST)).unwrap();
        let empty_l = ctys.intern_list(&[]).unwrap();
        let cenv0 = ctys.intern(PNode::Tuple(empty_l)).unwrap();
        let cap1_l = ctys.intern_list(&[cref_i64]).unwrap();
        let cenv1 = ctys.intern(PNode::Tuple(cap1_l)).unwrap();
        let closures = [
            (owner, 0u32, csig, cenv0, blaze_build::FnTrait::Fn),
            (owner, 1u32, csig, cenv1, blaze_build::FnTrait::Fn),
        ];

        let mut world = OracleWorld::new(OverflowChecks::Off, &ints, &[]);
        world.struct_key = Some(wrap_k);
        world.struct_tys = Some(st);
        world.struct_variants = &struct_variants;
        world.struct_fields = &struct_fields;
        world.deref_fn_item = Some(DEREF_ITEM);
        world.index_fn_item = Some(INDEX_ITEM);
        world.fn_call_item = Some(FN_CALL_ITEM);
        world.fn_once_item = Some(FN_ONCE_ITEM);
        world.closure_tys = Some(ctys);
        world.closures = &closures;

        let mut g_old = PortableTys::new_in(heap());
        let mut subst_old = Subst { args: &[], ground: &mut g_old, scratch: heap() };
        let old_built = blaze_build::build(&bv, &ck, &sig, &mut subst_old, &world, heap()).expect("old walk builds");

        let body = blaze_body_lower::lower(&bv, &ck, &world, heap()).expect("D119 lowers");
        let verified = blaze_body::verify(&body).expect("D119 verifies");
        let mut g_new = PortableTys::new_in(heap());
        let mut subst_new = Subst { args: &[], ground: &mut g_new, scratch: heap() };
        let new_built = blaze_body_build::build(verified, ck.types.view(), bv.owner, &mut subst_new, &world, heap()).expect("D119 builds");

        // Build both closure bodies through D119 (`lower_closure` -> `blaze_body_build`) too!
        let c0_body = blaze_body_lower::lower_closure(&bv, &ck, &world, ExprId(5), heap()).expect("closure 0 lowers");
        let c0_v = blaze_body::verify(&c0_body).expect("closure 0 verifies");
        let mut g_c0 = PortableTys::new_in(heap());
        let mut s_c0 = Subst { args: &[], ground: &mut g_c0, scratch: heap() };
        let c0_built = blaze_body_build::build(c0_v, ck.types.view(), owner, &mut s_c0, &world, heap()).expect("closure 0 builds");

        let c1_body = blaze_body_lower::lower_closure(&bv, &ck, &world, ExprId(9), heap()).expect("closure 1 lowers");
        let c1_v = blaze_body::verify(&c1_body).expect("closure 1 verifies");
        let mut g_c1 = PortableTys::new_in(heap());
        let mut s_c1 = Subst { args: &[], ground: &mut g_c1, scratch: heap() };
        let c1_built = blaze_body_build::build(c1_v, ck.types.view(), owner, &mut s_c1, &world, heap()).expect("closure 1 builds");

        // `deref(&Wrap) -> &i64` returns `self` ptr directly (field 0 at offset 0).
        let deref_fn = {
            let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, heap());
            let p = blaze_lir::builder::Param::of(blaze_lir::Ty::PTR);
            let s = b.sig(Conv::C, Variadic::No, &[p], &[p]);
            b.set_signature(s);
            let ent = b.create_block(BlockKind::Normal);
            b.begin(ent);
            let ptr = b.param(blaze_lir::Ty::PTR);
            b.ret(&[ptr]);
            b.finish_checked().unwrap()
        };
        // `index(&Wrap, usize) -> &i64` returns `self` ptr directly (reads `w.v`).
        let index_fn = {
            let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, heap());
            let p = blaze_lir::builder::Param::of(blaze_lir::Ty::PTR);
            let u = blaze_lir::builder::Param::of(blaze_lir::Ty::I64);
            let s = b.sig(Conv::C, Variadic::No, &[p, u], &[p]);
            b.set_signature(s);
            let ent = b.create_block(BlockKind::Normal);
            b.begin(ent);
            let ptr = b.param(blaze_lir::Ty::PTR);
            let _idx = b.param(blaze_lir::Ty::I64);
            b.ret(&[ptr]);
            b.finish_checked().unwrap()
        };
        // Verify `c0_built` (`(&C, a) -> a + 10`) directly, and provide its `call_once(a) -> a + 10` entry.
        let c0_res = run_in(c0_built.func.view(), &[Val::Ptr { slot: Val::NO_SLOT, offset: 0 }, Val::Int(5)], 1000, heap()).unwrap();
        let Outcome::Returned(c0_vals) = c0_res else { panic!("c0_built did not return") };
        assert_eq!(c0_vals.as_slice(), &[Val::Int(15)]);
        let c0_fn_ptr = {
            let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, heap());
            let i = blaze_lir::builder::Param::of(blaze_lir::Ty::I64);
            let s = b.sig(Conv::C, Variadic::No, &[i], &[i]);
            b.set_signature(s);
            let ent = b.create_block(BlockKind::Normal);
            b.begin(ent);
            let a = b.param(blaze_lir::Ty::I64);
            let ten = b.iconst(blaze_lir::Ty::I64, 10);
            let sum = b.binary(blaze_lir::Op::Add, blaze_lir::Flags::NONE, a, ten);
            b.ret(&[sum]);
            b.finish_checked().unwrap()
        };

        struct Table<'a> {
            refs: &'a [blaze_build::Instance],
            deref_v: View<'a>,
            index_v: View<'a>,
            c0_v: View<'a>,
            c1_v: View<'a>,
        }
        impl Bodies for Table<'_> {
            fn callee(&self, _: BodyId, r: RefId) -> Option<(BodyId, View<'_>)> {
                let ix = usize::try_from(r.0).ok()?;
                let inst = self.refs.get(ix)?;
                match inst.item {
                    DEREF_ITEM => Some((BodyId([1; 32]), self.deref_v)),
                    INDEX_ITEM => Some((BodyId([2; 32]), self.index_v)),
                    FN_ONCE_ITEM => Some((BodyId([3; 32]), self.c0_v)),
                    FN_CALL_ITEM => Some((BodyId([4; 32]), self.c1_v)),
                    _ => None,
                }
            }
            fn target(&self, from: BodyId, r: RefId) -> Option<blaze_lir::body::Target> {
                self.callee(from, r).map(|(id, _)| blaze_lir::body::Target::Func(id))
            }
            fn body(&self, id: BodyId) -> Option<View<'_>> {
                match id.0[0] {
                    1 => Some(self.deref_v),
                    2 => Some(self.index_v),
                    3 => Some(self.c0_v),
                    4 => Some(self.c1_v),
                    _ => None,
                }
            }
        }
        let old_tbl = Table { refs: &old_built.refs, deref_v: deref_fn.view(), index_v: index_fn.view(), c0_v: c0_fn_ptr.view(), c1_v: c1_built.func.view() };
        let new_tbl = Table { refs: &new_built.refs, deref_v: deref_fn.view(), index_v: index_fn.view(), c0_v: c0_fn_ptr.view(), c1_v: c1_built.func.view() };

        // For input `x`: `fp(*w) + c(w[2]) = (x + 10) + (x + x) = 3*x + 10`!
        for x in [0i64, 5, 11, -3] {
            let arg = Val::Int(u128::from(x.cast_unsigned()));
            let expected = Val::Int(u128::from((3 * x + 10).cast_unsigned()));
            let old_out = run_calls_in(old_built.func.view(), BodyId([0; 32]), &[arg], 20_000, &old_tbl, heap()).expect("old run");
            let new_out = run_calls_in(new_built.func.view(), BodyId([0; 32]), &[arg], 20_000, &new_tbl, heap()).expect("new run");
            let Outcome::Returned(old_vals) = old_out else { panic!("old walk did not return for x={x}") };
            let Outcome::Returned(new_vals) = new_out else { panic!("D119 build did not return for x={x}") };
            assert_eq!(old_vals.as_slice(), &[expected], "old walk mismatch for x={x}");
            assert_eq!(new_vals.as_slice(), old_vals.as_slice(), "D119 vs old walk divergence for x={x}");
        }
    }
}
