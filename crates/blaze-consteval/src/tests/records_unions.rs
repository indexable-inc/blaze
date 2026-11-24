//! Structural records and error unions in `blaze-consteval`:
//! record literals, field access, functional updates, `Pat::Record` destructuring,
//! `AdjustKind::RecordProject`, `AdjustKind::UnionInject` (direct-tagged, niche-optimized,
//! and single-member), `AdjustKind::UnionWiden`, and `UnionPat::{Variant, Residual}`.

use blaze_ids::ContentId;
use blaze_own::Alloc;
use blaze_typeck::fixture::Fx;
use blaze_typeck::{
    Adt, Arm, BinOp, BindAnn, BodyPath, BodyView, Checked, Edition, Expr, ExprId, Fetch, FieldInit, FieldPat, InferCtx,
    IntSuffix, IntTy, ItemKey, List, Lit, Mutbl, Name, Needs, Outcome, PNode, PTy, PTys, Pat, PatId, PathIdx,
    PortableTys, Range, RecordSchema, ScopeId, Segment, Stmt, StmtId, StructBase, UintTy, UnOp, check,
};

use crate::layout::{LayoutFacts, Layouts, Missing, ReprAttrs, ValidRange};
use crate::machine::{Answer, Codes, value_of};
use crate::record::{ConstRecord, RecordKey};
use crate::{ConstWorld, FnKind, Intrinsic, Limits, Lits, Typed};

fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn empty_body<'b>(owner: ItemKey, types: PTys<'b>, root: ExprId) -> BodyView<'b> {
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
        edition: Edition::E2021,
        expr_editions: &[],
        pat_editions: &[],
        record_expr_schemas: &[],
        closure_static_params: &[],
    }
}

struct OneBodyWorld<'b> {
    item: ItemKey,
    typed: Typed<'b>,
    fx: Option<&'b Fx>,
    intrinsics: &'b [(ItemKey, Intrinsic)],
}

impl LayoutFacts for OneBodyWorld<'_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.fx.and_then(|fx| fx.adt_of(key))
    }

    fn repr(&self, _: ItemKey) -> Result<ReprAttrs, Missing> {
        Ok(ReprAttrs::RUST)
    }

    fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> {
        Ok(None)
    }

    fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> {
        Ok(ValidRange::default())
    }
}

impl ConstWorld for OneBodyWorld<'_> {
    fn body(&self, item: ItemKey) -> Fetch<Option<Typed<'_>>> {
        if item == self.item {
            Ok(Some(self.typed))
        } else {
            Ok(None)
        }
    }

    fn fn_kind(&self, item: ItemKey) -> FnKind {
        self.intrinsics
            .iter()
            .find(|&&(k, _)| k == item)
            .map_or(FnKind::NotConst, |&(_, intr)| FnKind::Intrinsic(intr))
    }

    fn code_id(&self, item: ItemKey) -> ContentId {
        ContentId::of("test-code", &item.0.to_le_bytes())
    }

    fn is_static(&self, _: ItemKey) -> bool {
        false
    }

    fn record(&self, _: RecordKey) -> Fetch<&ConstRecord<Alloc<'_>>> {
        Err(blaze_typeck::Missing)
    }

    fn explicit_discriminant(&self, _: ItemKey, _: u32) -> Option<ItemKey> {
        None
    }
}

fn check_and_eval_u32(fx: &Fx, bv: &BodyView<'_>, owner: ItemKey, lit_text: &str, lit_exprs: (&[ExprId], &[Range])) -> u32 {
    let alloc = heap();
    let mut cx = InferCtx::new_in(alloc);
    let checked: Checked<'static> = match check(bv, fx, &mut cx, &Needs::new_in(alloc), alloc) {
        Ok(Outcome::Done(c)) => {
            assert!(c.diags.is_empty(), "check diagnostics: {:?}", c.diags);
            c
        }
        Ok(Outcome::Need(n)) => panic!("check missed {:?}", n.questions),
        Err(e) => panic!("check error: {e:?}"),
    };
    let lits = Lits { text: lit_text, exprs: lit_exprs, pats: (&[], &[]) };
    let typed = Typed::new(*bv, &checked, lit_text, lits, ContentId::of("test-body", lit_text.as_bytes()));
    let world = OneBodyWorld { item: owner, typed, fx: None, intrinsics: &[] };
    let mut ground = PortableTys::new_in(alloc);
    let mut layouts = Layouts::new_in(alloc);
    let mut codes = Codes::new_in(alloc);
    let rec = match value_of(&world, &mut layouts, &mut codes, owner, List::EMPTY, &mut ground, Limits::DEFAULT, alloc) {
        Ok(Answer::Done(r)) => r,
        Ok(Answer::Need(n)) => panic!("unexpected need: {n:?}"),
        Err(e) => panic!("consteval refused: {:?}", e.refusal),
    };
    let bytes = rec.data.allocs[0].bytes.as_slice();
    assert_eq!(bytes.len(), 4, "expected 4-byte u32 result, got {} bytes", bytes.len());
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn ranges_for(n: usize) -> blaze_own::Vec<Range, Alloc<'static>> {
    let mut out = blaze_own::Vec::with_capacity_in(n, heap());
    for i in 0..n {
        let s = u32::try_from(i).unwrap_or(0);
        out.push(Range { start: s, end: s + 1 });
    }
    out
}

fn bind(name: &str, ty: Option<PTy>) -> Pat {
    Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of(name), sub: None, ty }
}

/// Evaluates:
/// ```text
/// {
///   let r = { b: 10u8, a: 200u32 };   // non-canonical order
///   let x = r.b;                      // field access -> 10u8
///   let { a, .. } = r;                // Pat::Record destructuring -> 200u32
///   let r2 = { a: a + 50u32, ..r };   // Expr::Record update -> { a: 250u32, b: 10u8 }
///   r2.a + (r2.b as u32) + (x as u32) // 250 + 10 + 10 = 270
/// }
/// ```
#[test]
fn record_literal_field_update_and_destructure() {
    let alloc = heap();
    let mut fx = Fx::new();
    let u32_t = fx.u32();
    let owner = fx.func("C", 0, &[], u32_t, Default::default());
    fx.finish();

    let mut body_tys = PortableTys::new_in(alloc);
    let b_u32 = body_tys.intern(PNode::Uint(UintTy::U32)).expect("u32");
    let sorted = [Name::of("a"), Name::of("b")];
    let schema = RecordSchema::of_sorted_names(&sorted);
    body_tys.register_record_schema(schema, &sorted).expect("register schema");

    let pats = [
        bind("r", None),                                              // 0
        bind("x", None),                                              // 1
        bind("a", None),                                              // 2
        Pat::Record { fields: List { start: 0, len: 1 }, rest: true }, // 3: { a, .. }
        bind("r2", None),                                             // 4
    ];
    let field_pats = [FieldPat { name: Name::of("a"), pat: PatId(2) }];
    let pat_ranges = ranges_for(pats.len());

    let lit_text = "10u8200u3250u32";
    let exprs = [
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U8)))),                         // 0: 10u8
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),                        // 1: 200u32
        Expr::Record { fields: List { start: 0, len: 2 }, base: StructBase::None },     // 2: { b: 10u8, a: 200u32 }
        Expr::Local(PatId(0)),                                                          // 3: r
        Expr::Field { base: ExprId(3), name: Name::of("b") },                           // 4: r.b
        Expr::Local(PatId(0)),                                                          // 5: r
        Expr::Local(PatId(2)),                                                          // 6: a
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),                        // 7: 50u32
        Expr::Binary { op: BinOp::Add, lhs: ExprId(6), rhs: ExprId(7) },                // 8: a + 50u32
        Expr::Local(PatId(0)),                                                          // 9: r
        Expr::Record { fields: List { start: 2, len: 1 }, base: StructBase::Expr(ExprId(9)) }, // 10: { a: a + 50, ..r }
        Expr::Local(PatId(4)),                                                          // 11: r2
        Expr::Field { base: ExprId(11), name: Name::of("a") },                          // 12: r2.a
        Expr::Local(PatId(4)),                                                          // 13: r2
        Expr::Field { base: ExprId(13), name: Name::of("b") },                          // 14: r2.b
        Expr::Cast { operand: ExprId(14), ty: b_u32 },                                  // 15: r2.b as u32
        Expr::Binary { op: BinOp::Add, lhs: ExprId(12), rhs: ExprId(15) },              // 16: r2.a + (r2.b as u32)
        Expr::Local(PatId(1)),                                                          // 17: x
        Expr::Cast { operand: ExprId(17), ty: b_u32 },                                  // 18: x as u32
        Expr::Binary { op: BinOp::Add, lhs: ExprId(16), rhs: ExprId(18) },              // 19: + (x as u32)
        Expr::Block { stmts: List { start: 0, len: 4 }, tail: Some(ExprId(19)), scope: ScopeId(0) }, // 20
    ];
    let expr_ranges = ranges_for(exprs.len());
    let field_inits = [
        FieldInit { name: Name::of("b"), expr: ExprId(0) },
        FieldInit { name: Name::of("a"), expr: ExprId(1) },
        FieldInit { name: Name::of("a"), expr: ExprId(8) },
    ];
    let stmts = [
        Stmt::Let { pat: PatId(0), ty: None, init: Some(ExprId(2)), else_: None },
        Stmt::Let { pat: PatId(1), ty: None, init: Some(ExprId(4)), else_: None },
        Stmt::Let { pat: PatId(3), ty: None, init: Some(ExprId(5)), else_: None },
        Stmt::Let { pat: PatId(4), ty: None, init: Some(ExprId(10)), else_: None },
    ];
    let stmt_lists = [StmtId(0), StmtId(1), StmtId(2), StmtId(3)];
    let record_expr_schemas = [(ExprId(2), schema)];

    let mut bv = empty_body(owner, body_tys.view(), ExprId(20));
    bv.pats = &pats;
    bv.pat_ranges = &pat_ranges;
    bv.field_pats = &field_pats;
    bv.exprs = &exprs;
    bv.expr_ranges = &expr_ranges;
    bv.field_inits = &field_inits;
    bv.stmts = &stmts;
    bv.stmt_lists = &stmt_lists;
    bv.record_expr_schemas = &record_expr_schemas;

    let lit_ids = [ExprId(0), ExprId(1), ExprId(7)];
    let lit_ranges = [Range { start: 0, end: 4 }, Range { start: 4, end: 10 }, Range { start: 10, end: 15 }];
    let val = check_and_eval_u32(&fx, &bv, owner, lit_text, (&lit_ids, &lit_ranges));
    assert_eq!(val, 270);
}

/// Evaluates structural record width-subtyping (`AdjustKind::RecordProject`):
/// `{ c: 3u16, b: 200u32, a: 7u8 }` projected to `{ a: u8, c: u16 }`, then `(n.a as u32) + (n.c as u32)` == 10.
#[test]
fn record_width_subtyping_projection() {
    let alloc = heap();
    let mut fx = Fx::new();
    let u32_t = fx.u32();
    let owner = fx.func("C", 0, &[], u32_t, Default::default());
    fx.finish();

    let mut body_tys = PortableTys::new_in(alloc);
    let b_u8 = body_tys.intern(PNode::Uint(UintTy::U8)).expect("u8");
    let b_u16 = body_tys.intern(PNode::Uint(UintTy::U16)).expect("u16");
    let b_u32 = body_tys.intern(PNode::Uint(UintTy::U32)).expect("u32");

    let wide_names = [Name::of("a"), Name::of("b"), Name::of("c")];
    let wide_schema = RecordSchema::of_sorted_names(&wide_names);
    body_tys.register_record_schema(wide_schema, &wide_names).expect("wide schema");

    let narrow_names = [Name::of("a"), Name::of("c")];
    let narrow_schema = RecordSchema::of_sorted_names(&narrow_names);
    body_tys.register_record_schema(narrow_schema, &narrow_names).expect("narrow schema");
    let narrow_list = body_tys.intern_list(&[b_u8, b_u16]).expect("narrow list");
    let narrow_ty = body_tys.intern(PNode::Record(narrow_schema, narrow_list)).expect("narrow ty");

    let pats = [bind("wide", None), bind("narrow", None)];
    let pat_ranges = ranges_for(pats.len());

    let lit_text = "3u16200u327u8";
    let exprs = [
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U16)))),                    // 0: 3u16
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),                    // 1: 200u32
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U8)))),                     // 2: 7u8
        Expr::Record { fields: List { start: 0, len: 3 }, base: StructBase::None }, // 3: { c, b, a }
        Expr::Local(PatId(0)),                                                      // 4: wide (RecordProject -> narrow)
        Expr::Local(PatId(1)),                                                      // 5: narrow
        Expr::Field { base: ExprId(5), name: Name::of("a") },                       // 6: narrow.a
        Expr::Cast { operand: ExprId(6), ty: b_u32 },                               // 7: narrow.a as u32
        Expr::Local(PatId(1)),                                                      // 8: narrow
        Expr::Field { base: ExprId(8), name: Name::of("c") },                       // 9: narrow.c
        Expr::Cast { operand: ExprId(9), ty: b_u32 },                               // 10: narrow.c as u32
        Expr::Binary { op: BinOp::Add, lhs: ExprId(7), rhs: ExprId(10) },           // 11: +
        Expr::Block { stmts: List { start: 0, len: 2 }, tail: Some(ExprId(11)), scope: ScopeId(0) }, // 12
    ];
    let expr_ranges = ranges_for(exprs.len());
    let field_inits = [
        FieldInit { name: Name::of("c"), expr: ExprId(0) },
        FieldInit { name: Name::of("b"), expr: ExprId(1) },
        FieldInit { name: Name::of("a"), expr: ExprId(2) },
    ];
    let stmts = [
        Stmt::Let { pat: PatId(0), ty: None, init: Some(ExprId(3)), else_: None },
        Stmt::Let { pat: PatId(1), ty: Some(narrow_ty), init: Some(ExprId(4)), else_: None },
    ];
    let stmt_lists = [StmtId(0), StmtId(1)];
    let record_expr_schemas = [(ExprId(3), wide_schema)];

    let mut bv = empty_body(owner, body_tys.view(), ExprId(12));
    bv.pats = &pats;
    bv.pat_ranges = &pat_ranges;
    bv.exprs = &exprs;
    bv.expr_ranges = &expr_ranges;
    bv.field_inits = &field_inits;
    bv.stmts = &stmts;
    bv.stmt_lists = &stmt_lists;
    bv.record_expr_schemas = &record_expr_schemas;

    let lit_ids = [ExprId(0), ExprId(1), ExprId(2)];
    let lit_ranges = [Range { start: 0, end: 4 }, Range { start: 4, end: 10 }, Range { start: 10, end: 13 }];
    let val = check_and_eval_u32(&fx, &bv, owner, lit_text, (&lit_ids, &lit_ranges));
    assert_eq!(val, 10);
}

/// Evaluates structural union injection (`UnionInject`), widening (`UnionWiden`), typed variant
/// matching (`UnionPat::Variant`), and residual narrowing (`UnionPat::Residual`) on direct-tagged
/// `u32 | bool -> u32 | bool | u16`.
#[test]
fn union_inject_widen_and_match_direct_tagged() {
    let run = |inject_bool: bool| -> u32 {
        let alloc = heap();
        let mut fx = Fx::new();
        let u32_t = fx.u32();
        let owner = fx.func("C", 0, &[], u32_t, Default::default());
        fx.finish();

        let mut body_tys = PortableTys::new_in(alloc);
        let b_u32 = body_tys.intern(PNode::Uint(UintTy::U32)).expect("u32");
        let b_bool = body_tys.intern(PNode::Bool).expect("bool");
        let b_u16 = body_tys.intern(PNode::Uint(UintTy::U16)).expect("u16");
        let u2_l = body_tys.intern_list(&[b_u32, b_bool]).expect("u2");
        let u2_ty = body_tys.intern(PNode::Union(u2_l)).expect("u32 | bool");
        let u3_l = body_tys.intern_list(&[b_u32, b_bool, b_u16]).expect("u3");
        let u3_ty = body_tys.intern(PNode::Union(u3_l)).expect("u32 | bool | u16");

        let pats = [
            bind("u", None),          // 0
            bind("w", None),          // 1
            bind("n", Some(b_u32)),   // 2: Variant(0)
            bind("rest", None),       // 3: Residual (bool | u16)
            bind("b", Some(b_bool)),  // 4: Variant(0) on bool | u16
            bind("s", Some(b_u16)),   // 5: Variant(1) on bool | u16
        ];
        let pat_ranges = ranges_for(pats.len());

        let lit_text = "42u3277u320u32";
        let init_expr = if inject_bool {
            Expr::Lit(Lit::Bool(true))
        } else {
            Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32))))
        };
        let exprs = [
            init_expr,                                                                   // 0: 42u32 or true (UnionInject -> u2)
            Expr::Local(PatId(0)),                                                       // 1: u (UnionWiden -> u3)
            Expr::Local(PatId(1)),                                                       // 2: w
            Expr::Local(PatId(2)),                                                       // 3: n
            Expr::Local(PatId(3)),                                                       // 4: rest
            Expr::Local(PatId(4)),                                                       // 5: b
            Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),                     // 6: 77u32
            Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),                     // 7: 0u32
            Expr::If { cond: ExprId(5), then: ExprId(6), else_: Some(ExprId(7)) },       // 8: if b { 77 } else { 0 }
            Expr::Local(PatId(5)),                                                       // 9: s
            Expr::Cast { operand: ExprId(9), ty: b_u32 },                                // 10: s as u32
            Expr::Match { scrutinee: ExprId(4), arms: List { start: 0, len: 2 } },       // 11: match rest { b => ..., s => ... }
            Expr::Match { scrutinee: ExprId(2), arms: List { start: 2, len: 2 } },       // 12: match w { n => n, rest => ... }
            Expr::Block { stmts: List { start: 0, len: 2 }, tail: Some(ExprId(12)), scope: ScopeId(0) }, // 13
        ];
        let expr_ranges = ranges_for(exprs.len());
        let arms = [
            Arm { pat: PatId(4), guard: None, body: ExprId(8) },
            Arm { pat: PatId(5), guard: None, body: ExprId(10) },
            Arm { pat: PatId(2), guard: None, body: ExprId(3) },
            Arm { pat: PatId(3), guard: None, body: ExprId(11) },
        ];
        let stmts = [
            Stmt::Let { pat: PatId(0), ty: Some(u2_ty), init: Some(ExprId(0)), else_: None },
            Stmt::Let { pat: PatId(1), ty: Some(u3_ty), init: Some(ExprId(1)), else_: None },
        ];
        let stmt_lists = [StmtId(0), StmtId(1)];

        let mut bv = empty_body(owner, body_tys.view(), ExprId(13));
        bv.pats = &pats;
        bv.pat_ranges = &pat_ranges;
        bv.exprs = &exprs;
        bv.expr_ranges = &expr_ranges;
        bv.arms = &arms;
        bv.stmts = &stmts;
        bv.stmt_lists = &stmt_lists;

        let lit_ids = [ExprId(0), ExprId(6), ExprId(7)];
        let lit_ranges = [Range { start: 0, end: 5 }, Range { start: 5, end: 10 }, Range { start: 10, end: 14 }];
        check_and_eval_u32(&fx, &bv, owner, lit_text, (&lit_ids, &lit_ranges))
    };

    assert_eq!(run(false), 42, "inject u32 -> widen -> match Variant(0)");
    assert_eq!(run(true), 77, "inject bool -> widen -> match Residual(bool | u16) -> match Variant(0)");
}

/// Evaluates niche-optimized `&u32 | ()` (non-null pointer niche) and transparent single-member
/// union `PNode::Union([u32])`.
#[test]
fn union_niche_ref_unit_and_single_member() {
    let run_niche = |take_ref: bool| -> u32 {
        let alloc = heap();
        let mut fx = Fx::new();
        let u32_t = fx.u32();
        let owner = fx.func("C", 0, &[], u32_t, Default::default());
        fx.finish();

        let mut body_tys = PortableTys::new_in(alloc);
        let b_u32 = body_tys.intern(PNode::Uint(UintTy::U32)).expect("u32");
        let b_ref_u32 = body_tys.intern(PNode::Ref(Mutbl::Not, b_u32)).expect("&u32");
        let b_unit = body_tys.intern(PNode::Tuple(List::EMPTY)).expect("()");
        let u_l = body_tys.intern_list(&[b_ref_u32, b_unit]).expect("&u32 | () list");
        let u_ty = body_tys.intern(PNode::Union(u_l)).expect("&u32 | ()");

        let pats = [
            bind("v", None),             // 0: 42u32
            bind("u", None),             // 1: &u32 | ()
            bind("r", Some(b_ref_u32)),  // 2: Variant(0)
            bind("z", Some(b_unit)),     // 3: Variant(1)
        ];
        let pat_ranges = ranges_for(pats.len());

        let lit_text = "42u32999u32";
        if take_ref {
            let exprs = [
                Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),                // 0: 42u32
                Expr::Local(PatId(0)),                                                  // 1: v
                Expr::Ref { mutbl: Mutbl::Not, operand: ExprId(1) },                    // 2: &v (UnionInject(0))
                Expr::Local(PatId(1)),                                                  // 3: u
                Expr::Local(PatId(2)),                                                  // 4: r
                Expr::Unary { op: UnOp::Deref, operand: ExprId(4) },                    // 5: *r
                Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),                // 6: 999u32
                Expr::Match { scrutinee: ExprId(3), arms: List { start: 0, len: 2 } },  // 7
                Expr::Block { stmts: List { start: 0, len: 2 }, tail: Some(ExprId(7)), scope: ScopeId(0) }, // 8
            ];
            let expr_ranges = ranges_for(exprs.len());
            let arms = [
                Arm { pat: PatId(2), guard: None, body: ExprId(5) },
                Arm { pat: PatId(3), guard: None, body: ExprId(6) },
            ];
            let stmts = [
                Stmt::Let { pat: PatId(0), ty: None, init: Some(ExprId(0)), else_: None },
                Stmt::Let { pat: PatId(1), ty: Some(u_ty), init: Some(ExprId(2)), else_: None },
            ];
            let stmt_lists = [StmtId(0), StmtId(1)];

            let mut bv = empty_body(owner, body_tys.view(), ExprId(8));
            bv.pats = &pats;
            bv.pat_ranges = &pat_ranges;
            bv.exprs = &exprs;
            bv.expr_ranges = &expr_ranges;
            bv.arms = &arms;
            bv.stmts = &stmts;
            bv.stmt_lists = &stmt_lists;

            let lit_ids = [ExprId(0), ExprId(6)];
            let lit_ranges = [Range { start: 0, end: 5 }, Range { start: 5, end: 11 }];
            return check_and_eval_u32(&fx, &bv, owner, lit_text, (&lit_ids, &lit_ranges));
        }
        let exprs = [
            Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),                // 0: 42u32
            Expr::Tuple { elems: List::EMPTY },                                     // 1: () (UnionInject(1))
            Expr::Local(PatId(1)),                                                  // 2: u
            Expr::Local(PatId(2)),                                                  // 3: r
            Expr::Unary { op: UnOp::Deref, operand: ExprId(3) },                    // 4: *r
            Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),                // 5: 999u32
            Expr::Match { scrutinee: ExprId(2), arms: List { start: 0, len: 2 } },  // 6
            Expr::Block { stmts: List { start: 0, len: 2 }, tail: Some(ExprId(6)), scope: ScopeId(0) }, // 7
        ];
        let expr_ranges = ranges_for(exprs.len());
        let arms = [
            Arm { pat: PatId(2), guard: None, body: ExprId(4) },
            Arm { pat: PatId(3), guard: None, body: ExprId(5) },
        ];
        let stmts = [
            Stmt::Let { pat: PatId(0), ty: None, init: Some(ExprId(0)), else_: None },
            Stmt::Let { pat: PatId(1), ty: Some(u_ty), init: Some(ExprId(1)), else_: None },
        ];
        let stmt_lists = [StmtId(0), StmtId(1)];

        let mut bv = empty_body(owner, body_tys.view(), ExprId(7));
        bv.pats = &pats;
        bv.pat_ranges = &pat_ranges;
        bv.exprs = &exprs;
        bv.expr_ranges = &expr_ranges;
        bv.arms = &arms;
        bv.stmts = &stmts;
        bv.stmt_lists = &stmt_lists;

        let lit_ids = [ExprId(0), ExprId(5)];
        let lit_ranges = [Range { start: 0, end: 5 }, Range { start: 5, end: 11 }];
        check_and_eval_u32(&fx, &bv, owner, lit_text, (&lit_ids, &lit_ranges))
    };

    assert_eq!(run_niche(true), 42, "niche-optimized `&u32 | ()` holding `&u32`");
    assert_eq!(run_niche(false), 999, "niche-optimized `&u32 | ()` holding `()`");

    // Transparent single-member union `PNode::Union([u32])`.
    let alloc = heap();
    let mut fx = Fx::new();
    let u32_t = fx.u32();
    let owner = fx.func("SINGLE", 0, &[], u32_t, Default::default());
    fx.finish();
    let mut body_tys = PortableTys::new_in(alloc);
    let b_u32 = body_tys.intern(PNode::Uint(UintTy::U32)).expect("u32");
    let u1_l = body_tys.intern_list(&[b_u32]).expect("u1");
    let u1_ty = body_tys.intern(PNode::Union(u1_l)).expect("single-member union");

    let pats = [bind("u", None), bind("x", Some(b_u32))];
    let pat_ranges = ranges_for(pats.len());
    let exprs = [
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),                // 0: 55u32
        Expr::Local(PatId(0)),                                                  // 1: u
        Expr::Local(PatId(1)),                                                  // 2: x
        Expr::Match { scrutinee: ExprId(1), arms: List { start: 0, len: 1 } },  // 3
        Expr::Block { stmts: List { start: 0, len: 1 }, tail: Some(ExprId(3)), scope: ScopeId(0) }, // 4
    ];
    let expr_ranges = ranges_for(exprs.len());
    let arms = [Arm { pat: PatId(1), guard: None, body: ExprId(2) }];
    let stmts = [Stmt::Let { pat: PatId(0), ty: Some(u1_ty), init: Some(ExprId(0)), else_: None }];
    let stmt_lists = [StmtId(0)];
    let mut bv = empty_body(owner, body_tys.view(), ExprId(4));
    bv.pats = &pats;
    bv.pat_ranges = &pat_ranges;
    bv.exprs = &exprs;
    bv.expr_ranges = &expr_ranges;
    bv.arms = &arms;
    bv.stmts = &stmts;
    bv.stmt_lists = &stmt_lists;

    let lit_ids = [ExprId(0)];
    let lit_ranges = [Range { start: 0, end: 5 }];
    assert_eq!(check_and_eval_u32(&fx, &bv, owner, "55u32", (&lit_ids, &lit_ranges)), 55);
}

fn eval_reflect_call(
    fx: &Fx,
    body_tys: PTys<'_>,
    owner: ItemKey,
    intrinsics: &[(ItemKey, Intrinsic)],
    fn_name: &str,
    type_args: &[PTy],
) -> u64 {
    let alloc = heap();
    let paths = [BodyPath { scope: ScopeId(0), segments: List { start: 0, len: 1 }, lang: None, anchor: None }];
    let segments = [Segment {
        name: Name::of(fn_name),
        generic_args: List { start: 0, len: u32::try_from(type_args.len()).unwrap_or(0) },
        has_args: true,
    }];
    let segment_ranges = [Range { start: 0, end: 1 }];
    let type_list_ranges = ranges_for(type_args.len());
    let exprs = [
        Expr::Path(PathIdx(0)),
        Expr::Call { callee: ExprId(0), args: List::EMPTY },
    ];
    let expr_ranges = [Range { start: 0, end: 1 }, Range { start: 1, end: 2 }];

    let mut bv = empty_body(owner, body_tys, ExprId(1));
    bv.paths = &paths;
    bv.segments = &segments;
    bv.segment_ranges = &segment_ranges;
    bv.type_lists = type_args;
    bv.type_list_ranges = &type_list_ranges;
    bv.exprs = &exprs;
    bv.expr_ranges = &expr_ranges;

    let mut cx = InferCtx::new_in(alloc);
    let checked: Checked<'static> = match check(&bv, fx, &mut cx, &Needs::new_in(alloc), alloc) {
        Ok(Outcome::Done(c)) => {
            assert!(c.diags.is_empty(), "check diagnostics for `{fn_name}`: {:?}", c.diags);
            c
        }
        Ok(Outcome::Need(n)) => panic!("check missed {:?}", n.questions),
        Err(e) => panic!("check error: {e:?}"),
    };
    let lits = Lits { text: "", exprs: (&[], &[]), pats: (&[], &[]) };
    let typed = Typed::new(bv, &checked, "", lits, ContentId::of("test-reflect", fn_name.as_bytes()));
    let world = OneBodyWorld { item: owner, typed, fx: Some(fx), intrinsics };
    let mut ground = PortableTys::new_in(alloc);
    let mut layouts = Layouts::new_in(alloc);
    let mut codes = Codes::new_in(alloc);
    let rec = match value_of(&world, &mut layouts, &mut codes, owner, List::EMPTY, &mut ground, Limits::DEFAULT, alloc) {
        Ok(Answer::Done(r)) => r,
        Ok(Answer::Need(n)) => panic!("unexpected need: {n:?}"),
        Err(e) => panic!("consteval refused `{fn_name}`: {:?}", e.refusal),
    };
    let bytes = rec.data.allocs[0].bytes.as_slice();
    match bytes {
        &[b] => u64::from(b),
        &[b0, b1, b2, b3, b4, b5, b6, b7] => u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7]),
        other => panic!("unexpected result width {} for `{fn_name}`", other.len()),
    }
}

/// Evaluates `blaze_field_count::<T>() -> usize`, `blaze_records_disjoint::<A, B>() -> bool`,
/// and `blaze_record_schema_id::<T>() -> u64` in `blaze-consteval` across structural records
/// (empty, overlapping, disjoint, reordered canonical schemas), tuples, structs, enums, and
/// primitives.
#[test]
fn structural_record_reflection_intrinsics_in_consteval() {
    let alloc = heap();
    let mut fx = Fx::new();
    let fx_usize = fx.ty(PNode::Uint(UintTy::Usize));
    let fx_bool = fx.ty(PNode::Bool);
    let fx_u64 = fx.ty(PNode::Uint(UintTy::U64));
    let fx_u32 = fx.u32();
    let fx_i32 = fx.ty(PNode::Int(IntTy::I32));

    let k_field_count = fx.func("blaze_field_count", 1, &[], fx_usize, Default::default());
    let k_disjoint = fx.func("blaze_records_disjoint", 2, &[], fx_bool, Default::default());
    let k_schema_id = fx.func("blaze_record_schema_id", 1, &[], fx_u64, Default::default());

    let c_usize = fx.func("C_USIZE", 0, &[], fx_usize, Default::default());
    let c_bool = fx.func("C_BOOL", 0, &[], fx_bool, Default::default());
    let c_u64 = fx.func("C_U64", 0, &[], fx_u64, Default::default());

    let unit_struct = fx.strukt("UnitS", 0, &[]);
    let braced_struct = fx.strukt("Point", 0, &[("x", fx_i32), ("y", fx_i32)]);
    let tuple_struct = fx.tuple_strukt("Triple", 0, &[fx_u32, fx_u32, fx_u32]);
    let (multi_enum, _) = fx.enum_("Choice", 0, &[("A", &[fx_u32]), ("B", &[fx_i32, fx_u32])]);
    let (single_enum, _) = fx.enum_("Solo", 0, &[("Only", &[fx_u32, fx_i32])]);
    fx.finish();

    let intrinsics = [
        (k_field_count, Intrinsic::BlazeFieldCount),
        (k_disjoint, Intrinsic::BlazeRecordsDisjoint),
        (k_schema_id, Intrinsic::BlazeRecordSchemaId),
    ];

    let mut body_tys = PortableTys::new_in(alloc);
    let b_u8 = body_tys.intern(PNode::Uint(UintTy::U8)).expect("u8");
    let b_u16 = body_tys.intern(PNode::Uint(UintTy::U16)).expect("u16");
    let b_u32 = body_tys.intern(PNode::Uint(UintTy::U32)).expect("u32");
    let b_u64 = body_tys.intern(PNode::Uint(UintTy::U64)).expect("u64");
    let b_i32 = body_tys.intern(PNode::Int(IntTy::I32)).expect("i32");
    let b_bool = body_tys.intern(PNode::Bool).expect("bool");
    let b_unit = body_tys.intern(PNode::Tuple(List::EMPTY)).expect("()");
    let b_pair = {
        let l = body_tys.intern_list(&[b_u8, b_u32]).expect("pair list");
        body_tys.intern(PNode::Tuple(l)).expect("pair")
    };
    let b_quad = {
        let l = body_tys.intern_list(&[b_u8, b_u16, b_u32, b_u64]).expect("quad list");
        body_tys.intern(PNode::Tuple(l)).expect("quad")
    };
    let b_unit_struct = body_tys.intern(PNode::Adt(unit_struct, List::EMPTY)).expect("UnitS");
    let b_braced_struct = body_tys.intern(PNode::Adt(braced_struct, List::EMPTY)).expect("Point");
    let b_tuple_struct = body_tys.intern(PNode::Adt(tuple_struct, List::EMPTY)).expect("Triple");
    let b_multi_enum = body_tys.intern(PNode::Adt(multi_enum, List::EMPTY)).expect("Choice");
    let b_single_enum = body_tys.intern(PNode::Adt(single_enum, List::EMPTY)).expect("Solo");

    // Records:
    // empty = {}
    body_tys.register_record_schema(RecordSchema::EMPTY, &[]).expect("empty schema");
    let rec_empty = body_tys.intern(PNode::Record(RecordSchema::EMPTY, List::EMPTY)).expect("{}");

    // pos = { x: i32, y: i32 }
    let pos_names = [Name::of("x"), Name::of("y")];
    let pos_schema = RecordSchema::of_sorted_names(&pos_names);
    body_tys.register_record_schema(pos_schema, &pos_names).expect("pos schema");
    let pos_fields = body_tys.intern_list(&[b_i32, b_i32]).expect("pos fields");
    let rec_pos = body_tys.intern(PNode::Record(pos_schema, pos_fields)).expect("pos");

    // Reordered-source canonical record `{ y: i32, x: i32 }` -> same canonical sorted names [x, y]
    let reordered_names = [Name::of("x"), Name::of("y")];
    let reordered_schema = RecordSchema::of_sorted_names(&reordered_names);
    let rec_pos_reordered = body_tys.intern(PNode::Record(reordered_schema, pos_fields)).expect("pos reordered");

    // vel = { vx: i32, vy: i32 } (disjoint from pos)
    let vel_names = [Name::of("vx"), Name::of("vy")];
    let vel_schema = RecordSchema::of_sorted_names(&vel_names);
    body_tys.register_record_schema(vel_schema, &vel_names).expect("vel schema");
    let vel_fields = body_tys.intern_list(&[b_i32, b_i32]).expect("vel fields");
    let rec_vel = body_tys.intern(PNode::Record(vel_schema, vel_fields)).expect("vel");

    // hp_a = { hp: u32, x: i32 }, hp_b = { hp: u64, shield: u32 } (overlapping on `hp`)
    let hpa_names = [Name::of("hp"), Name::of("x")];
    let hpa_schema = RecordSchema::of_sorted_names(&hpa_names);
    body_tys.register_record_schema(hpa_schema, &hpa_names).expect("hpa schema");
    let hpa_fields = body_tys.intern_list(&[b_u32, b_i32]).expect("hpa fields");
    let rec_hpa = body_tys.intern(PNode::Record(hpa_schema, hpa_fields)).expect("hpa");

    let hpb_names = [Name::of("hp"), Name::of("shield")];
    let hpb_schema = RecordSchema::of_sorted_names(&hpb_names);
    body_tys.register_record_schema(hpb_schema, &hpb_names).expect("hpb schema");
    let hpb_fields = body_tys.intern_list(&[b_u64, b_u32]).expect("hpb fields");
    let rec_hpb = body_tys.intern(PNode::Record(hpb_schema, hpb_fields)).expect("hpb");

    // tri = { a: u8, b: u16, c: u32 }
    let tri_names = [Name::of("a"), Name::of("b"), Name::of("c")];
    let tri_schema = RecordSchema::of_sorted_names(&tri_names);
    body_tys.register_record_schema(tri_schema, &tri_names).expect("tri schema");
    let tri_fields = body_tys.intern_list(&[b_u8, b_u16, b_u32]).expect("tri fields");
    let rec_tri = body_tys.intern(PNode::Record(tri_schema, tri_fields)).expect("tri");

    let view = body_tys.view();

    // 1. `blaze_field_count::<T>() -> usize`
    let count_of = |t: PTy| eval_reflect_call(&fx, view, c_usize, &intrinsics, "blaze_field_count", &[t]);
    assert_eq!(count_of(rec_empty), 0, "empty record");
    assert_eq!(count_of(rec_pos), 2, "2-field record");
    assert_eq!(count_of(rec_tri), 3, "3-field record");
    assert_eq!(count_of(b_unit), 0, "unit tuple");
    assert_eq!(count_of(b_pair), 2, "2-tuple");
    assert_eq!(count_of(b_quad), 4, "4-tuple");
    assert_eq!(count_of(b_unit_struct), 0, "unit struct");
    assert_eq!(count_of(b_braced_struct), 2, "braced struct");
    assert_eq!(count_of(b_tuple_struct), 3, "tuple struct");
    assert_eq!(count_of(b_multi_enum), 0, "multi-variant enum");
    assert_eq!(count_of(b_single_enum), 0, "single-variant enum");
    assert_eq!(count_of(b_u32), 0, "primitive u32");
    assert_eq!(count_of(b_bool), 0, "primitive bool");

    // 2. `blaze_records_disjoint::<A, B>() -> bool`
    let disjoint = |a: PTy, b: PTy| eval_reflect_call(&fx, view, c_bool, &intrinsics, "blaze_records_disjoint", &[a, b]);
    assert_eq!(disjoint(rec_pos, rec_vel), 1, "{{ x, y }} vs {{ vx, vy }} are disjoint");
    assert_eq!(disjoint(rec_hpa, rec_hpb), 0, "{{ hp, x }} vs {{ hp, shield }} overlap on `hp`");
    assert_eq!(disjoint(rec_pos, rec_hpa), 0, "{{ x, y }} vs {{ hp, x }} overlap on `x`");
    assert_eq!(disjoint(rec_pos, rec_pos), 0, "non-empty record overlaps itself");
    assert_eq!(disjoint(rec_empty, rec_pos), 1, "empty record is disjoint from any record");
    assert_eq!(disjoint(rec_empty, rec_empty), 1, "empty record is disjoint from empty record");
    assert_eq!(disjoint(rec_pos, b_unit), 1, "record vs () is disjoint");
    assert_eq!(disjoint(b_unit, rec_pos), 1, "() vs record is disjoint");
    assert_eq!(disjoint(b_unit, b_unit), 1, "() vs () is disjoint");
    assert_eq!(disjoint(b_u32, b_u32), 0, "same non-record type is not disjoint");
    assert_eq!(disjoint(b_u32, b_i32), 1, "distinct non-record types are disjoint");
    assert_eq!(disjoint(rec_pos, b_u32), 1, "record vs non-record are disjoint");

    // 3. `blaze_record_schema_id::<T>() -> u64`
    let schema_id = |t: PTy| eval_reflect_call(&fx, view, c_u64, &intrinsics, "blaze_record_schema_id", &[t]);
    assert_eq!(schema_id(rec_pos), pos_schema.0, "record returns its canonical schema hash");
    assert_eq!(schema_id(rec_pos_reordered), pos_schema.0, "reordered fields share identical schema ID");
    assert_ne!(schema_id(rec_pos), schema_id(rec_vel), "different field sets have distinct schema IDs");
    assert_ne!(schema_id(rec_pos), 0, "record schema ID is non-zero");
    assert_eq!(schema_id(rec_empty), RecordSchema::EMPTY.0, "empty record returns RecordSchema::EMPTY.0");
    assert_eq!(schema_id(b_u32), 0, "primitive returns 0");
    assert_eq!(schema_id(b_pair), 0, "tuple returns 0");
    assert_eq!(schema_id(b_braced_struct), 0, "nominal struct returns 0");
    assert_eq!(schema_id(b_multi_enum), 0, "enum returns 0");
}
