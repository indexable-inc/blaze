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

    struct OracleWorld<'a> {
        overflow: OverflowChecks,
        ints: &'a [(ExprId, u128)],
        int_pats: &'a [(PatId, u128)],
        enum_key: Option<ItemKey>,
        enum_tys: Option<PortableTys<'static>>,
        enum_variants: &'a [Variant],
        struct_key: Option<ItemKey>,
        struct_tys: Option<PortableTys<'static>>,
        struct_variants: &'a [Variant],
        struct_fields: &'a [Field],
    }

    impl<'a> OracleWorld<'a> {
        fn new(overflow: OverflowChecks, ints: &'a [(ExprId, u128)], int_pats: &'a [(PatId, u128)]) -> Self {
            Self {
                overflow,
                ints,
                int_pats,
                enum_key: None,
                enum_tys: None,
                enum_variants: &[],
                struct_key: None,
                struct_tys: None,
                struct_variants: &[],
                struct_fields: &[],
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
        fn field_count(&self, k: ItemKey, _: u32) -> Option<u32> {
            if self.struct_key == Some(k) { u32::try_from(self.struct_fields.len()).ok() } else { None }
        }
        fn param_copy(&self, _: u32) -> bool {
            false
        }
    }

    impl BodyFacts for OracleWorld<'_> {
        fn int_lit(&self, e: ExprId) -> Option<u128> {
            self.ints.iter().find(|(id, _)| *id == e).map(|(_, v)| *v)
        }
        fn int_pat(&self, p: PatId) -> Option<u128> {
            self.int_pats.iter().find(|(id, _)| *id == p).map(|(_, v)| *v)
        }
        fn pointer_bits(&self) -> Option<u32> {
            Some(64)
        }
        fn trait_item(&self, _: ItemKey) -> Option<bool> {
            Some(false)
        }
    }

    impl LayoutFacts for OracleWorld<'_> {
        fn adt(&self, k: ItemKey) -> Option<Adt<'_>> {
            if self.enum_key == Some(k) {
                let tys = self.enum_tys.as_ref()?;
                return Some(Adt { kind: AdtKind::Enum, generics: 0, types: tys.view(), variants: self.enum_variants, fields: &[] });
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
                return Ok(Some(match v {
                    0 => 10,
                    1 => 20,
                    _ => 30,
                }));
            }
            Ok(None)
        }
        fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> {
            Ok(ValidRange::default())
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
        fn int_pat(&self, _: ItemKey, p: PatId) -> Option<u128> {
            self.int_pats.iter().find(|(id, _)| *id == p).map(|(_, v)| *v)
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
        fn abi(&self, _: ItemKey) -> Option<Abi> {
            Some(Abi::Rust)
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
}
