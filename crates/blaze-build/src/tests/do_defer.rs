//! Tests for lexical `defer { ... }` blocks (`walk.rs`) and one-shot `.do` suspension
//! lowering to zero-`Pin` sans-IO machines (`coroutine.rs` -> `blaze_lir::coroutine_machine`).

use std::alloc::Global;
use std::vec::Vec as StdVec;

use blaze_lir::coroutine::Saved;
use blaze_lir::coroutine_machine::{MachineEdge, MachineEdgeKind};
use blaze_lir::eval::{Bodies, BodyId, Outcome as EvalOutcome, Val, run_calls_in};
use blaze_lir::func::RefId;
use blaze_lir::{Op, View};
use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{
    Adt, BinOp, BodyView, Expr, ExprId, InferCtx, IntSuffix, ItemKey, LangItem, Mutbl, Name, Needs, Outcome, PNode,
    PTy, PortableTys, Stmt, UintTy, UnOp,
};

use super::{BodyBuf, Built, MAIN, TestWorld, build, one};
use crate::layout::{LayoutFacts, Missing, ReprAttrs, ValidRange};
use crate::{Abi, Adts, BuildError, Subst, build_do_machine_body, split_do_machine_body};

const U64: PNode = PNode::Uint(UintTy::U64);

/// Helper: emits `log = log * 10 + digit;` as a statement on `BodyBuf`.
fn push_digit_stmt(b: &mut BodyBuf, log: blaze_typeck::PatId, digit: u128) -> Stmt {
    let lhs = b.local(log);
    let cur = b.local(log);
    let ten = b.int(10, Some(IntSuffix::Uint(UintTy::U64)));
    let mul = b.bin(BinOp::Mul, cur, ten);
    let d = b.int(digit, Some(IntSuffix::Uint(UintTy::U64)));
    let add = b.bin(BinOp::Add, mul, d);
    let assign = b.e(Expr::Assign { lhs, rhs: add });
    Stmt::Expr { expr: assign, semi: true }
}

/// Helper: emits `*ptr = *ptr * 10 + digit;` as a statement on `BodyBuf`.
fn push_deref_digit_stmt(b: &mut BodyBuf, ptr: blaze_typeck::PatId, digit: u128) -> Stmt {
    let p_lhs = b.local(ptr);
    let deref_lhs = b.e(Expr::Unary { op: UnOp::Deref, operand: p_lhs });
    let p_rhs = b.local(ptr);
    let deref_rhs = b.e(Expr::Unary { op: UnOp::Deref, operand: p_rhs });
    let ten = b.int(10, Some(IntSuffix::Uint(UintTy::U64)));
    let mul = b.bin(BinOp::Mul, deref_rhs, ten);
    let d = b.int(digit, Some(IntSuffix::Uint(UintTy::U64)));
    let add = b.bin(BinOp::Add, mul, d);
    let assign = b.e(Expr::Assign { lhs: deref_lhs, rhs: add });
    Stmt::Expr { expr: assign, semi: true }
}

/// Helper: builds `defer { stmt };` as a statement on `BodyBuf`.
fn defer_stmt(b: &mut BodyBuf, stmt: Stmt) -> Stmt {
    let body = b.block(&[stmt], None);
    let def = b.e(Expr::Defer(body));
    Stmt::Expr { expr: def, semi: true }
}

fn defer_push_digit(b: &mut BodyBuf, log: blaze_typeck::PatId, digit: u128) -> Stmt {
    let s = push_digit_stmt(b, log, digit);
    defer_stmt(b, s)
}

fn defer_push_deref_digit(b: &mut BodyBuf, ptr: blaze_typeck::PatId, digit: u128) -> Stmt {
    let s = push_deref_digit_stmt(b, ptr, digit);
    defer_stmt(b, s)
}

fn plain_world() -> TestWorld {
    let mut w = TestWorld::new();
    w.overflow = crate::OverflowChecks::Off;
    w.ub = crate::UbChecks::Off;
    w.panic = crate::Panic::Abort;
    w
}

/// `defer { ... }` does not execute eagerly and runs in strict LIFO order on normal block exit:
/// ```text
/// let mut log: u64 = 0;
/// {
///     defer { log = log * 10 + 1; };
///     defer { log = log * 10 + 2; };
///     log = log * 10 + 9;
/// }
/// log // == 921
/// ```
#[test]
fn defer_executes_in_strict_lifo_order_on_normal_block_exit() {
    let mut w = plain_world();
    w.func(1, "main", &[], U64);
    let mut b = BodyBuf::new();
    let log = b.bind("log");
    let zero = b.int(0, Some(IntSuffix::Uint(UintTy::U64)));
    let d1 = defer_push_digit(&mut b, log, 1);
    let d2 = defer_push_digit(&mut b, log, 2);
    let body_op = push_digit_stmt(&mut b, log, 9);
    let inner = b.block(&[d1, d2, body_op], None);
    let ret_log = b.local(log);
    let root = b.block(
        &[
            Stmt::Let { pat: log, ty: None, init: Some(zero), else_: None },
            Stmt::Expr { expr: inner, semi: true },
        ],
        Some(ret_log),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(one(&built, &[]), 921);
}

/// Nested `defer` scopes execute inner defers before outer defers, and a `defer` registered inside
/// a `defer` body runs cleanly in its own inner scope before outer defers continue:
/// ```text
/// let mut log: u64 = 0;
/// {
///     defer {
///         defer { log = log * 10 + 1; };
///         log = log * 10 + 7;
///     };
///     {
///         defer { log = log * 10 + 2; };
///         {
///             defer { log = log * 10 + 3; };
///             log = 5;
///         }
///         log = log * 10 + 4;
///     }
///     log = log * 10 + 6;
/// }
/// log // 5 -> 53 -> 534 -> 5342 -> 53426 -> 534267 -> 5342671
/// ```
#[test]
fn nested_defer_scopes_execute_inner_before_outer() {
    let mut w = plain_world();
    w.func(1, "main", &[], U64);
    let mut b = BodyBuf::new();
    let log = b.bind("log");
    let zero = b.int(0, Some(IntSuffix::Uint(UintTy::U64)));

    // Outer defer containing an inner defer: writes 7, then on its block exit writes 1.
    let nested_d1 = defer_push_digit(&mut b, log, 1);
    let write_7 = push_digit_stmt(&mut b, log, 7);
    let outer_defer_body = b.block(&[nested_d1, write_7], None);
    let outer_defer = Stmt::Expr { expr: b.e(Expr::Defer(outer_defer_body)), semi: true };

    let d2 = defer_push_digit(&mut b, log, 2);
    let d3 = defer_push_digit(&mut b, log, 3);
    let write_5 = push_digit_stmt(&mut b, log, 5);
    let innermost = b.block(&[d3, write_5], None);

    let write_4 = push_digit_stmt(&mut b, log, 4);
    let mid = b.block(&[d2, Stmt::Expr { expr: innermost, semi: true }, write_4], None);

    let write_6 = push_digit_stmt(&mut b, log, 6);
    let outer = b.block(&[outer_defer, Stmt::Expr { expr: mid, semi: true }, write_6], None);

    let ret_log = b.local(log);
    let root = b.block(
        &[
            Stmt::Let { pat: log, ty: None, init: Some(zero), else_: None },
            Stmt::Expr { expr: outer, semi: true },
        ],
        Some(ret_log),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(one(&built, &[]), 5_342_671);
}

/// `defer` executes on `break` out of a labeled block without firing outer scopes' defers early:
/// ```text
/// let mut log: u64 = 0;
/// {
///     defer { log = log * 10 + 1; };
///     'b: {
///         defer { log = log * 10 + 2; };
///         defer { log = log * 10 + 3; };
///         log = 4;
///         break 'b;
///     }
///     log = log * 10 + 5;
/// }
/// log // 4 -> 43 -> 432 -> 4325 -> 43251
/// ```
#[test]
fn defer_runs_in_lifo_order_on_break_from_labeled_block() {
    let mut w = plain_world();
    w.func(1, "main", &[], U64);
    let mut b = BodyBuf::new();
    let log = b.bind("log");
    let zero = b.int(0, Some(IntSuffix::Uint(UintTy::U64)));

    let d1 = defer_push_digit(&mut b, log, 1);
    let labeled = b.e(Expr::Missing);
    let d2 = defer_push_digit(&mut b, log, 2);
    let d3 = defer_push_digit(&mut b, log, 3);
    let write_4 = push_digit_stmt(&mut b, log, 4);
    let brk = b.e(Expr::Break { value: None, target: labeled });
    let start = b.stmt_lists.len() as u32;
    for s in [d2, d3, write_4, Stmt::Expr { expr: brk, semi: true }] {
        let id = blaze_typeck::StmtId(b.stmts.len() as u32);
        b.stmts.push(s);
        b.stmt_lists.push(id);
    }
    let stmts = blaze_typeck::List { start, len: b.stmt_lists.len() as u32 - start };
    b.set(labeled, Expr::Block { stmts, tail: None, scope: blaze_typeck::ScopeId(0) });

    let write_5 = push_digit_stmt(&mut b, log, 5);
    let outer = b.block(&[d1, Stmt::Expr { expr: labeled, semi: true }, write_5], None);
    let ret_log = b.local(log);
    let root = b.block(
        &[
            Stmt::Let { pat: log, ty: None, init: Some(zero), else_: None },
            Stmt::Expr { expr: outer, semi: true },
        ],
        Some(ret_log),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(one(&built, &[]), 43_251);
}

/// Two-function `Bodies` table for evaluating `main` calling `helper` via `run_calls_in`.
struct TwoBodies<'a> {
    main_key: ItemKey,
    helper_key: ItemKey,
    main: &'a Built<Global>,
    helper: &'a Built<Global>,
}

impl Bodies for TwoBodies<'_> {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
        let caller = if from == BodyId([0u8; 32]) { self.main } else { self.helper };
        let inst = caller.refs.get(reference.index())?;
        if inst.item == self.helper_key {
            Some((BodyId([1u8; 32]), self.helper.func.view()))
        } else if inst.item == self.main_key {
            Some((BodyId([0u8; 32]), self.main.func.view()))
        } else {
            None
        }
    }
}

/// `defer` executes in strict LIFO order on early `return` AND leaves the non-returning path's
/// registered defers intact when the same function takes the normal exit path:
/// ```text
/// fn helper(out: &mut u64, early: u64) -> u64 {
///     defer { *out = *out * 10 + 1; };
///     defer { *out = *out * 10 + 2; };
///     if early == 1 {
///         *out = *out * 10 + 7;
///         return 99;
///     }
///     *out = *out * 10 + 8;
///     44
/// }
/// fn main() -> u64 {
///     let mut log: u64 = 0;
///     let r1 = helper(&mut log, 1); // log: 7 -> 72 -> 721, r1 = 99
///     let r2 = helper(&mut log, 0); // log: 7218 -> 72182 -> 721821, r2 = 44
///     log * 1000 + r1 + r2 // == 721_821_143
/// }
/// ```
#[test]
fn defer_runs_in_lifo_order_on_early_return() {
    let mut w = plain_world();
    let ref_mut_u64 = w.ref_(Mutbl::Mut, U64);
    let helper_key = ItemKey(2);
    w.func(1, "main", &[], U64);
    w.func(2, "helper", &[ref_mut_u64, U64], U64);

    // Build `helper(out: &mut u64, early: u64) -> u64`.
    let mut hb = BodyBuf::new();
    let out_p = hb.bind("out");
    let early_p = hb.bind("early");
    hb.params.push(out_p);
    hb.params.push(early_p);
    let hd1 = defer_push_deref_digit(&mut hb, out_p, 1);
    let hd2 = defer_push_deref_digit(&mut hb, out_p, 2);
    let early_use = hb.local(early_p);
    let one_lit = hb.int(1, Some(IntSuffix::Uint(UintTy::U64)));
    let cond = hb.bin(BinOp::Eq, early_use, one_lit);
    let write_7 = push_deref_digit_stmt(&mut hb, out_p, 7);
    let v99 = hb.int(99, Some(IntSuffix::Uint(UintTy::U64)));
    let ret_99 = hb.e(Expr::Return { value: Some(v99) });
    let then_blk = hb.block(&[write_7, Stmt::Expr { expr: ret_99, semi: true }], None);
    let if_expr = hb.e(Expr::If { cond, then: then_blk, else_: None });
    let write_8 = push_deref_digit_stmt(&mut hb, out_p, 8);
    let v44 = hb.int(44, Some(IntSuffix::Uint(UintTy::U64)));
    let hroot = hb.block(&[hd1, hd2, Stmt::Expr { expr: if_expr, semi: true }, write_8], Some(v44));

    let mut ground = PortableTys::new_in(blaze_own::testing::heap().alloc());
    let helper_built = super::build_instance(&w, &hb, helper_key, hroot, &[], &mut ground)
        .unwrap_or_else(|e| panic!("helper build failed: {e}"));

    // Build `main() -> u64`.
    let mut mb = BodyBuf::new();
    let log = mb.bind("log");
    let r1 = mb.bind("r1");
    let r2 = mb.bind("r2");
    let zero = mb.int(0, Some(IntSuffix::Uint(UintTy::U64)));
    let log_ref1 = {
        let l = mb.local(log);
        mb.e(Expr::Ref { mutbl: Mutbl::Mut, operand: l })
    };
    let arg_early = mb.int(1, Some(IntSuffix::Uint(UintTy::U64)));
    let call1 = mb.call("helper", &[log_ref1, arg_early]);
    let log_ref2 = {
        let l = mb.local(log);
        mb.e(Expr::Ref { mutbl: Mutbl::Mut, operand: l })
    };
    let arg_normal = mb.int(0, Some(IntSuffix::Uint(UintTy::U64)));
    let call2 = mb.call("helper", &[log_ref2, arg_normal]);
    let log_use = mb.local(log);
    let k1000 = mb.int(1000, Some(IntSuffix::Uint(UintTy::U64)));
    let scaled = mb.bin(BinOp::Mul, log_use, k1000);
    let r1_use = mb.local(r1);
    let r2_use = mb.local(r2);
    let sum_r = mb.bin(BinOp::Add, r1_use, r2_use);
    let total = mb.bin(BinOp::Add, scaled, sum_r);
    let mroot = mb.block(
        &[
            Stmt::Let { pat: log, ty: None, init: Some(zero), else_: None },
            Stmt::Let { pat: r1, ty: None, init: Some(call1), else_: None },
            Stmt::Let { pat: r2, ty: None, init: Some(call2), else_: None },
        ],
        Some(total),
    );
    let main_built = super::build_instance(&w, &mb, MAIN, mroot, &[], &mut ground)
        .unwrap_or_else(|e| panic!("main build failed: {e}"));

    let bodies = TwoBodies { main_key: MAIN, helper_key, main: &main_built, helper: &helper_built };
    let outcome = run_calls_in(main_built.func.view(), BodyId([0u8; 32]), &[], 10_000_000, &bodies, Global)
        .unwrap_or_else(|e| panic!("eval failed: {e:?}"));
    match outcome {
        EvalOutcome::Returned(vals) => assert_eq!(vals.as_slice(), &[Val::Int(721_821_143)]),
        other => panic!("unexpected eval outcome: {other:?}"),
    }
}

/// Fixture world for `.do` suspension tests using `blaze_typeck::fixture::Fx`.
struct DoWorld {
    fx: Fx,
    request_tr: ItemKey,
    workflow: ItemKey,
    ask_agg_fn: ItemKey,
}

fn do_world() -> DoWorld {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, StdVec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let copy = fx.tr("Copy", 1, StdVec::new(), &[]);
    fx.lang(LangItem::Copy, copy);
    let u64_ = fx.ty(PNode::Uint(UintTy::U64));
    fx.imp("copy_u64", 0, u64_, Some((copy, &[])), &[], StdVec::new());

    let request_tr = fx.tr("Request", 1, StdVec::new(), &["Answer"]);
    fx.lang(LangItem::Request, request_tr);

    // Zero-sized `Ping` request with `Answer = u64`.
    let ping_adt = fx.strukt("Ping", 0, &[]);
    let ping_t = fx.adt(ping_adt, &[]);
    fx.imp("copy_ping", 0, ping_t, Some((copy, &[])), &[], StdVec::new());
    fx.imp("req_ping", 0, ping_t, Some((request_tr, &[])), &[("Answer", u64_)], StdVec::new());

    // Non-ZST aggregate `AskNum { tag: u64 }` request with `Answer = u64`.
    let ask_num_adt = fx.strukt("AskNum", 0, &[("tag", u64_)]);
    let ask_num_t = fx.adt(ask_num_adt, &[]);
    fx.imp("copy_ask_num", 0, ask_num_t, Some((copy, &[])), &[], StdVec::new());
    fx.imp("req_ask_num", 0, ask_num_t, Some((request_tr, &[])), &[("Answer", u64_)], StdVec::new());

    let workflow = fx.func("workflow", 0, &[u64_, ping_t], u64_, StdVec::new());
    let ask_agg_fn = fx.func("ask_agg", 0, &[u64_], u64_, StdVec::new());
    fx.finish();
    DoWorld { fx, request_tr, workflow, ask_agg_fn }
}

struct DoAdts<'a> {
    w: &'a DoWorld,
    lits: &'a [(ExprId, u128)],
}

impl LayoutFacts for DoAdts<'_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.w.fx.adt_of(key)
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

    fn normalize(&self, trait_: ItemKey, name: Name, _: &[PTy], ground: &mut PortableTys<'_>) -> Result<PTy, BuildError> {
        if trait_ == self.w.request_tr && name == Name::of("Answer") {
            return ground
                .intern(PNode::Uint(UintTy::U64))
                .map_err(|_| BuildError::Malformed { what: "the ground table is full" });
        }
        Err(BuildError::Projection { trait_, name })
    }
}

impl Adts for DoAdts<'_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.w.fx.adt_of(key)
    }

    fn facts(&self) -> &dyn LayoutFacts {
        self
    }

    fn overflow_checks(&self) -> crate::OverflowChecks {
        crate::OverflowChecks::Off
    }

    fn ub_checks(&self) -> crate::UbChecks {
        crate::UbChecks::Off
    }

    fn panic(&self) -> crate::Panic {
        crate::Panic::Abort
    }

    fn int_lit(&self, _: ItemKey, expr: ExprId) -> Option<u128> {
        self.lits.iter().find(|(e, _)| *e == expr).map(|&(_, v)| v)
    }

    fn drop_impl(&self, _: PTy, _: &mut PortableTys<'_>) -> Result<Option<ItemKey>, BuildError> {
        Ok(None)
    }

    fn abi(&self, _: ItemKey) -> Option<Abi> {
        Some(Abi::Rust)
    }
}

fn bb_bin(b: &mut Bb, op: BinOp, lhs: ExprId, rhs: ExprId) -> ExprId {
    let at = b.int(None);
    b.exprs[at.ix()] = Expr::Binary { op, lhs, rhs };
    at
}

fn check_fx(fx: &Fx, view: &BodyView<'_>) -> blaze_typeck::Checked<'static> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut ty = InferCtx::new_in(alloc);
    match blaze_typeck::check(view, fx, &mut ty, &Needs::new_in(alloc), alloc) {
        Ok(Outcome::Done(c)) => {
            assert!(c.diags.is_empty(), "unexpected diagnostics: {:?}", c.diags);
            c
        }
        Ok(Outcome::Need(n)) => panic!("typecheck needed {:?}", n.questions),
        Err(error) => panic!("typecheck failed: {error:?}"),
    }
}

/// `.do` inside a plain non-coroutine `crate::build` (no suspend harness attached) fails with a
/// typed `BuildError::Unsupported` explicitly naming `.do`.
#[test]
fn do_suspension_outside_machine_harness_returns_typed_error() {
    let w = do_world();
    let mut b = Bb::new();
    let _x = b.param("x");
    let req = b.param("req");
    let req_e = b.local(req);
    let do_e = b.do_expr(req_e);
    let root = b.block(&[], Some(do_e));
    let view = b.view(w.workflow, root);
    let checked = check_fx(&w.fx, &view);
    assert!(checked.suspends, "`.do` body must mark checked.suspends");

    let sig = w.fx.sig_of(w.workflow).expect("workflow sig");
    let mut ground = PortableTys::new_in(Alloc::global());
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
    let adts = DoAdts { w: &w, lits: &[] };
    let err = crate::build(&view, &checked, &sig, &mut subst, &adts, Global)
        .err()
        .expect("plain build without machine harness must reject `.do`");
    match err {
        BuildError::Unsupported { expr, what } => {
            assert_eq!(expr, do_e);
            assert!(what.contains(".do"), "error must name `.do`, got: {what}");
        }
        other => panic!("expected BuildError::Unsupported naming `.do`, got: {other:?}"),
    }
}

/// A two-suspend `.do` workflow with live locals across suspends lowers to verified LIR with
/// `Op::Suspend` and passes `blaze_lir::coroutine_machine::split_machine` with
/// `zero_pin_proven == true` and zero pinned stack slots:
/// ```text
/// fn workflow(x: u64, req: Ping) -> u64 {
///     let y: u64 = x + 10;
///     let a: u64 = req.do;
///     let b: u64 = req.do;
///     y + a + b
/// }
/// ```
#[test]
fn do_suspension_lowers_to_lir_suspend_and_splits_with_zero_pin_proven() {
    let w = do_world();
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let x = b.param("x");
    let req = b.param("req");

    let ten = b.int(Some(IntSuffix::Uint(UintTy::U64)));
    lits.push((ten, 10));
    let x_e = b.local(x);
    let x_plus_10 = bb_bin(&mut b, BinOp::Add, x_e, ten);
    let y = b.bind("y");
    let s_y = b.let_(y, None, x_plus_10);

    let r1 = b.local(req);
    let do1 = b.do_expr(r1);
    let a = b.bind("a");
    let s_a = b.let_(a, None, do1);

    let r2 = b.local(req);
    let do2 = b.do_expr(r2);
    let bb = b.bind("b");
    let s_b = b.let_(bb, None, do2);

    let y_e = b.local(y);
    let a_e = b.local(a);
    let b_e = b.local(bb);
    let ya = bb_bin(&mut b, BinOp::Add, y_e, a_e);
    let sum = bb_bin(&mut b, BinOp::Add, ya, b_e);
    let root = b.block(&[s_y, s_a, s_b], Some(sum));

    let view = b.view(w.workflow, root);
    let checked = check_fx(&w.fx, &view);
    assert!(checked.suspends, "`.do` workflow must set checked.suspends");
    let sig = w.fx.sig_of(w.workflow).expect("workflow sig");
    let adts = DoAdts { w: &w, lits: &lits };

    // 1. Pre-split LIR contains 2 `Op::Suspend` instructions and passes `blaze_lir::verify`.
    let mut ground = PortableTys::new_in(Alloc::global());
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
    let (pre_func, shape) = build_do_machine_body(&view, &checked, &sig, &mut subst, &adts, 2, Global)
        .unwrap_or_else(|e| panic!("build_do_machine_body failed: {e}"));
    blaze_lir::verify(pre_func.view()).expect("pre-split `.do` body must verify");
    let suspend_count = (0..pre_func.view().len()).filter(|&i| pre_func.view().op(i) == Op::Suspend).count();
    assert_eq!(suspend_count, 2, "expected 2 Op::Suspend ops in pre-split LIR");
    assert_eq!(shape.state_param, 0);
    assert_eq!(shape.grace_max_steps, 2);

    // 2. Full `split_do_machine_body` pipeline proves zero-`Pin` and emits verified `step`/`on_cancel`.
    let mut ground2 = PortableTys::new_in(Alloc::global());
    let mut subst2 = Subst { args: &[], ground: &mut ground2, scratch: Alloc::global() };
    let split = split_do_machine_body(&view, &checked, &sig, &mut subst2, &adts, 2, Global)
        .unwrap_or_else(|e| panic!("split_do_machine_body failed: {e}"));
    assert!(split.graph.zero_pin_proven, "`.do` machine must prove zero_pin_proven");
    assert_eq!(split.graph.points, 2);
    assert!(
        split.layout.fields.iter().all(|f| matches!(f.what, Saved::Value(_))),
        "zero-Pin machine must not pin stack slots in state: {:?}",
        split.layout.fields
    );
    blaze_lir::verify(split.step.view()).expect("split step function must verify");
    blaze_lir::verify(split.on_cancel.view()).expect("split on_cancel function must verify");
    let expected_edges = [
        MachineEdge { from_point: None, to_point: None, kind: MachineEdgeKind::Cancel },
        MachineEdge { from_point: None, to_point: Some(0), kind: MachineEdgeKind::Suspend },
        MachineEdge { from_point: Some(0), to_point: None, kind: MachineEdgeKind::Cancel },
        MachineEdge { from_point: Some(0), to_point: Some(1), kind: MachineEdgeKind::Suspend },
        MachineEdge { from_point: Some(1), to_point: None, kind: MachineEdgeKind::Return },
        MachineEdge { from_point: Some(1), to_point: None, kind: MachineEdgeKind::Cancel },
    ];
    assert_eq!(split.graph.edges.as_slice(), &expected_edges);
}

/// A non-ZST aggregate request temporary `AskNum { tag: x }.do` copies its request into `out` and
/// ends its temporary stack slot (`Op::SlotDead`) before `Op::Suspend`, so `split_do_machine_body`
/// still proves `zero_pin_proven == true`.
#[test]
fn do_suspension_with_aggregate_request_kills_temp_slot_for_zero_pin() {
    let w = do_world();
    let mut b = Bb::new();
    let x = b.param("x");
    let x_e = b.local(x);
    let req_lit = b.struct_lit(&["AskNum"], &[("tag", x_e)], None);
    let ans = b.do_expr(req_lit);
    let x_after = b.local(x);
    let sum = bb_bin(&mut b, BinOp::Add, ans, x_after);
    let root = b.block(&[], Some(sum));

    let view = b.view(w.ask_agg_fn, root);
    let checked = check_fx(&w.fx, &view);
    assert!(checked.suspends);
    let sig = w.fx.sig_of(w.ask_agg_fn).expect("ask_agg sig");
    let adts = DoAdts { w: &w, lits: &[] };

    let mut ground = PortableTys::new_in(Alloc::global());
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
    let split = split_do_machine_body(&view, &checked, &sig, &mut subst, &adts, 1, Global)
        .unwrap_or_else(|e| panic!("aggregate `.do` machine split failed: {e}"));
    assert!(split.graph.zero_pin_proven, "aggregate request temporary must be killed before suspend");
    assert_eq!(split.graph.points, 1);
    blaze_lir::verify(split.step.view()).expect("step must verify");
    blaze_lir::verify(split.on_cancel.view()).expect("on_cancel must verify");
}
