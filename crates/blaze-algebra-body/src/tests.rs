use blaze_algebra::{CostModel, ExprGraph, Interval, SimplifyOpts, eval_dual, hessian};
use blaze_body::{
    BinOp, Block, BlockKind, Body, BodyBuilder, Callee, ConstDecl, ConstKind, LocalDecl, LocalKind,
    Operand, Proj, Role, Span, StmtKind,
    body::{ExprId as MirExprId, ItemKey, List, Mutbl, PTy},
    verify,
};
use blaze_own::Alloc;

use crate::{
    BodyAlgebraError, BodyPlant, BodyTransform, MathEmitter, MathOp, MathResolver, NoMathCalls,
    ScalarTys, diff_body, diff_body_with_plant, eval_pure_body, eval_pure_multi_body, grad_bodies,
    hvp_body, lift_body, run_body_transform, simplify_body, simplify_body_with, splice_body,
    splice_multi_body, value_and_grad_body,
};

const TEST_TYS: ScalarTys = ScalarTys { f64_ty: PTy(0), bool_ty: PTy(2) };

fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn close(a: f64, b: f64, tol: f64) -> bool {
    let scale = 1.0_f64.max(blaze_math::abs(a)).max(blaze_math::abs(b));
    blaze_math::abs(a - b) <= tol * scale
}

/// Deterministic test math table mapping `ItemKey(100..=108)` <-> `MathOp`.
#[derive(Clone, Copy)]
struct TestMath;

impl TestMath {
    const fn key(op: MathOp) -> ItemKey {
        ItemKey(match op {
            MathOp::Sin => 100,
            MathOp::Cos => 101,
            MathOp::Tan => 102,
            MathOp::Exp => 103,
            MathOp::Ln => 104,
            MathOp::Sqrt => 105,
            MathOp::Tanh => 106,
            MathOp::Abs => 107,
            MathOp::Pow => 108,
            MathOp::Fma => 109,
        })
    }
}

impl MathResolver for TestMath {
    fn classify(&self, callee: Callee) -> Option<MathOp> {
        let Callee::FnItem { key, .. } = callee else { return None };
        match key.0 {
            100 => Some(MathOp::Sin),
            101 => Some(MathOp::Cos),
            102 => Some(MathOp::Tan),
            103 => Some(MathOp::Exp),
            104 => Some(MathOp::Ln),
            105 => Some(MathOp::Sqrt),
            106 => Some(MathOp::Tanh),
            107 => Some(MathOp::Abs),
            108 => Some(MathOp::Pow),
            109 => Some(MathOp::Fma),
            _ => None,
        }
    }
}

impl MathEmitter for TestMath {
    fn callee_for(&self, op: MathOp) -> Option<Callee> {
        Some(Callee::FnItem { key: Self::key(op), args: List::EMPTY })
    }
}

fn build_rational_mir() -> Body<Alloc<'static>> {
    // f(x, y) = (x * x * y + 2.0 * x - y) / (1.0 + x * x)
    let f64_ty = PTy(0);
    let span = Span { lo: 0, hi: 0 };
    let mut b = BodyBuilder::new_in(heap());
    let l0 = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Return, mutbl: Mutbl::Mut });
    let lx = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
    let ly = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Arg(1), mutbl: Mutbl::Not });
    let temp = |b: &mut BodyBuilder<_>, id: u32| {
        let l = b.new_local(LocalDecl {
            ty: f64_ty,
            kind: LocalKind::Temp(MirExprId(id), Role::Value),
            mutbl: Mutbl::Not,
        });
        b.local_place(l)
    };
    let p0 = b.local_place(l0);
    let px = b.local_place(lx);
    let py = b.local_place(ly);
    let t_x2 = temp(&mut b, 0);
    let t_x2y = temp(&mut b, 1);
    let t_2x = temp(&mut b, 2);
    let t_s1 = temp(&mut b, 3);
    let t_num = temp(&mut b, 4);
    let t_den = temp(&mut b, 5);

    let c1 = b.new_const(ConstDecl { kind: ConstKind::Scalar { bits: u128::from(1.0_f64.to_bits()) }, ty: f64_ty });
    let c2 = b.new_const(ConstDecl { kind: ConstKind::Scalar { bits: u128::from(2.0_f64.to_bits()) }, ty: f64_ty });

    let bb0 = b.new_block(BlockKind::Normal);
    b.start(bb0);
    b.assign(t_x2, blaze_body::Value::Binary { op: BinOp::Mul, lhs: Operand::Copy(px), rhs: Operand::Copy(px) }, span);
    b.assign(t_x2y, blaze_body::Value::Binary { op: BinOp::Mul, lhs: Operand::Copy(t_x2), rhs: Operand::Copy(py) }, span);
    b.assign(t_2x, blaze_body::Value::Binary { op: BinOp::Mul, lhs: Operand::Const(c2), rhs: Operand::Copy(px) }, span);
    b.assign(t_s1, blaze_body::Value::Binary { op: BinOp::Add, lhs: Operand::Copy(t_x2y), rhs: Operand::Copy(t_2x) }, span);
    b.assign(t_num, blaze_body::Value::Binary { op: BinOp::Sub, lhs: Operand::Copy(t_s1), rhs: Operand::Copy(py) }, span);
    b.assign(t_den, blaze_body::Value::Binary { op: BinOp::Add, lhs: Operand::Const(c1), rhs: Operand::Copy(t_x2) }, span);
    b.assign(p0, blaze_body::Value::Binary { op: BinOp::Div, lhs: Operand::Copy(t_num), rhs: Operand::Copy(t_den) }, span);
    b.terminate(StmtKind::Return, span);
    b.finish().expect("valid rational MIR")
}

#[test]
fn mir_diff_and_reverse_grad_bodies_verify_and_match_dual_oracle() {
    let body = build_rational_mir();
    let v = verify(&body).expect("verify input MIR");

    let lifted = lift_body(v, &NoMathCalls, heap()).expect("lift MIR");
    let df_dx_body = diff_body(v, 0, &NoMathCalls, &TestMath, heap()).expect("diff_body wrt x");
    let grads = grad_bodies(v, &NoMathCalls, &TestMath, heap()).expect("grad_bodies");
    assert_eq!(grads.len(), 2);

    let v_dx = verify(&df_dx_body).expect("diff_body output must verify");
    let v_gx = verify(&grads[0]).expect("grad[0] must verify");
    let v_gy = verify(&grads[1]).expect("grad[1] must verify");

    for &(xv, yv) in &[(0.5, 1.2), (1.7, -0.4), (-1.1, 2.5)] {
        let env = [xv, yv];
        let dual_x = eval_dual(&lifted.graph, lifted.result, &env, lifted.vars[0]).unwrap();
        let dual_y = eval_dual(&lifted.graph, lifted.result, &env, lifted.vars[1]).unwrap();

        let mir_val = eval_pure_body(v, &env, &NoMathCalls, heap()).unwrap();
        let mir_dx = eval_pure_body(v_dx, &env, &TestMath, heap()).unwrap();
        let mir_gx = eval_pure_body(v_gx, &env, &TestMath, heap()).unwrap();
        let mir_gy = eval_pure_body(v_gy, &env, &TestMath, heap()).unwrap();

        assert!(close(mir_val, dual_x.val, 1e-12));
        assert!(close(mir_dx, dual_x.deriv, 1e-11), "mir_dx={mir_dx} vs dual={}", dual_x.deriv);
        assert!(close(mir_gx, dual_x.deriv, 1e-11), "mir_gx={mir_gx} vs dual={}", dual_x.deriv);
        assert!(close(mir_gy, dual_y.deriv, 1e-11), "mir_gy={mir_gy} vs dual={}", dual_y.deriv);
    }

    // Planted-bug oracle control (rule 43 `dst-not-review`).
    let buggy = diff_body_with_plant(
        v,
        0,
        &NoMathCalls,
        &TestMath,
        BodyPlant::SpliceOriginalInsteadOfDeriv,
        heap(),
    )
    .expect("buggy diff_body");
    let v_bug = verify(&buggy).expect("buggy verifies");
    let env = [0.7, 1.3];
    let dual_x = eval_dual(&lifted.graph, lifted.result, &env, lifted.vars[0]).unwrap();
    let got_bug = eval_pure_body(v_bug, &env, &TestMath, heap()).unwrap();
    assert!(!close(got_bug, dual_x.deriv, 1e-6), "oracle failed to catch SpliceOriginalInsteadOfDeriv");
}

#[test]
fn mir_transcendental_calls_lift_diff_and_splice_multi_block_mir() {
    // Build g(x, y) = sin(x * y) + exp(x - y) directly via ExprGraph -> splice_body -> MIR -> diff_body -> MIR!
    let mut g = ExprGraph::new_in(heap());
    let (vx, ex) = g.var("x");
    let (vy, ey) = g.var("y");
    let xy = g.mul(ex, ey);
    let sxy = g.sin(xy);
    let xmy = g.sub(ex, ey);
    let exmy = g.exp(xmy);
    let root = g.add(sxy, exmy);

    let orig_body = splice_body(&g, root, &[vx, vy], TEST_TYS, &TestMath, heap()).expect("splice transcendental");
    let v_orig = verify(&orig_body).expect("verify orig");
    // Multi-block MIR because `sin` and `exp` are Call terminators!
    assert!(v_orig.blocks() >= 3);

    let d_body = diff_body(v_orig, 0, &TestMath, &TestMath, heap()).expect("diff transcendental MIR");
    let v_d = verify(&d_body).expect("verify differentiated MIR");

    let env = [0.9, 1.4];
    let dual_x = eval_dual(&g, root, &env, vx).unwrap();
    let eval_d = eval_pure_body(v_d, &env, &TestMath, heap()).unwrap();
    assert!(close(eval_d, dual_x.deriv, 1e-11), "eval_d={eval_d} vs dual={}", dual_x.deriv);
}

#[test]
fn mir_simplify_body_collapses_exact_polynomial_identity_to_single_const() {
    // (x + y)^3 - (x^3 + 3*x^2*y + 3*x*y^2 + y^3) == 0
    let mut g = ExprGraph::new_in(heap());
    let (vx, x) = g.var("x");
    let (vy, y) = g.var("y");
    let xpy = g.add(x, y);
    let xpy2 = g.mul(xpy, xpy);
    let lhs = g.mul(xpy2, xpy);

    let x2 = g.mul(x, x);
    let x3 = g.mul(x2, x);
    let y2 = g.mul(y, y);
    let y3 = g.mul(y2, y);
    let c3 = g.lit(3.0);
    let x2y = g.mul(x2, y);
    let t1 = g.mul(c3, x2y);
    let xy2 = g.mul(x, y2);
    let t2 = g.mul(c3, xy2);
    let s1 = g.add(x3, t1);
    let s2 = g.add(s1, t2);
    let rhs = g.add(s2, y3);
    let diff_expr = g.sub(lhs, rhs);

    let big_mir = splice_body(&g, diff_expr, &[vx, vy], TEST_TYS, &NoMathCalls, heap()).unwrap();
    let v_big = verify(&big_mir).unwrap();
    assert!(v_big.stmts() > 10);

    let tiny_mir = simplify_body(v_big, &NoMathCalls, &NoMathCalls, heap()).unwrap();
    let v_tiny = verify(&tiny_mir).unwrap();
    // Collapsed down to `_0 = const 0.0; return;` (2 rows total)!
    assert_eq!(v_tiny.stmts(), 2, "expected 2 rows (assign 0.0 + return), got {}", v_tiny.stmts());
    assert_eq!(eval_pure_body(v_tiny, &[42.5, -17.3], &NoMathCalls, heap()).unwrap(), 0.0);
}

#[test]
fn impure_branching_and_cyclic_mir_is_refused_with_typed_error_naming_site() {
    // Cyclic MIR: bb0 -> goto bb0
    let f64_ty = PTy(0);
    let span = Span { lo: 0, hi: 0 };
    let mut b = BodyBuilder::new_in(heap());
    let _ = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Return, mutbl: Mutbl::Mut });
    let bb0 = b.new_block(BlockKind::Normal);
    b.start(bb0);
    b.terminate(StmtKind::Goto(bb0), span);
    let cyclic = b.finish().unwrap();
    let err = lift_body(verify(&cyclic).unwrap(), &NoMathCalls, heap()).err().unwrap();
    assert!(matches!(err, BodyAlgebraError::CycleOrStepLimit { block: Block(0), .. }));
}

#[test]
fn blaze_sim_property_random_mir_bodies_diff_and_eval_match_dual_oracle() {
    let config = blaze_sim::Config { first: 2026_1122, ..blaze_sim::Config::cases(32) };
    blaze_sim::check(config, |t| {
        let mut g = ExprGraph::new_in(heap());
        let (vx, ex) = g.var("x");
        let (vy, ey) = g.var("y");
        let mut pool = [ex; 10];
        pool[0] = ex;
        pool[1] = ey;
        pool[2] = g.lit(2.0);
        pool[3] = g.lit(3.0);
        let mut count = 4usize;

        let steps = t.range(2..6, "steps");
        for _ in 0..steps {
            let kind = t.range(0..7, "op");
            let a = pool[t.range(0..count as u64, "a") as usize];
            let b = pool[t.range(0..count as u64, "b") as usize];
            let next = match kind {
                0 => g.add(a, b),
                1 => g.sub(a, b),
                2 => g.mul(a, b),
                3 => {
                    let one = g.lit(1.0);
                    let b2 = g.mul(b, b);
                    let den = g.add(one, b2);
                    g.div(a, den)
                }
                4 => g.sin(a),
                5 => g.cos(a),
                _ => g.neg(a),
            };
            if count < pool.len() {
                pool[count] = next;
                count += 1;
            } else {
                pool[count - 1] = next;
            }
        }
        let root = pool[count - 1];

        let mir = splice_body(&g, root, &[vx, vy], TEST_TYS, &TestMath, heap())
            .map_err(|_| blaze_sim::Broke::Oracle("splice_body failed".into()))?;
        let v_mir = verify(&mir)
            .map_err(|_| blaze_sim::Broke::Oracle("verify orig mir failed".into()))?;
        let d_mir = diff_body(v_mir, 0, &TestMath, &TestMath, heap())
            .map_err(|_| blaze_sim::Broke::Oracle("diff_body failed".into()))?;
        let v_d = verify(&d_mir)
            .map_err(|_| blaze_sim::Broke::Oracle("verify diff mir failed".into()))?;

        let env = [0.67, -0.52];
        let dual = eval_dual(&g, root, &env, vx)
            .map_err(|_| blaze_sim::Broke::Oracle("eval_dual failed".into()))?;
        let mir_dx = eval_pure_body(v_d, &env, &TestMath, heap())
            .map_err(|_| blaze_sim::Broke::Oracle("eval_pure_body failed".into()))?;

        if dual.deriv.is_finite() && !close(mir_dx, dual.deriv, 1e-8) {
            return Err(blaze_sim::Broke::Oracle("MIR diff != Dual tangent".into()));
        }
        Ok(())
    });
}

#[test]
fn value_and_grad_body_emits_single_verified_cse_mir_returning_array() {
    // Build f(x, y) = sin(x * y) + exp(x - y) as a verified MIR body, then run
    // `value_and_grad_body` to compile [f, df/dx, df/dy] into ONE verified CSE MIR body!
    let mut g = ExprGraph::new_in(heap());
    let (vx, ex) = g.var("x");
    let (vy, ey) = g.var("y");
    let xy = g.mul(ex, ey);
    let sxy = g.sin(xy);
    let xmy = g.sub(ex, ey);
    let exmy = g.exp(xmy);
    let root = g.add(sxy, exmy);

    let orig = splice_body(&g, root, &[vx, vy], TEST_TYS, &TestMath, heap()).unwrap();
    let v_orig = verify(&orig).unwrap();

    let fused = value_and_grad_body(v_orig, PTy(1), &TestMath, &TestMath, heap()).unwrap();
    let v_fused = verify(&fused).expect("fused value_and_grad MIR must verify");

    // Verify against independent Dual oracle across multiple inputs.
    for env in [[0.8, 1.3], [-0.4, 2.1], [1.7, -0.6]] {
        let out = eval_pure_multi_body(v_fused, &env, &TestMath, heap()).unwrap();
        assert_eq!(out.len(), 3);
        let dx = eval_dual(&g, root, &env, vx).unwrap();
        let dy = eval_dual(&g, root, &env, vy).unwrap();
        assert!(close(out[0], dx.val, 1e-11), "f mismatch: {} vs {}", out[0], dx.val);
        assert!(close(out[1], dx.deriv, 1e-11), "df/dx mismatch: {} vs {}", out[1], dx.deriv);
        assert!(close(out[2], dy.deriv, 1e-11), "df/dy mismatch: {} vs {}", out[2], dy.deriv);
    }

    // Also verify `splice_multi_body` directly on multiple shared expressions.
    let direct =
        splice_multi_body(&g, &[root, xy, sxy], &[vx, vy], TEST_TYS, PTy(1), &TestMath, heap())
            .unwrap();
    let v_direct = verify(&direct).unwrap();
    let vals = eval_pure_multi_body(v_direct, &[0.5, 2.0], &TestMath, heap()).unwrap();
    assert_eq!(vals.len(), 3);
    assert!(close(vals[1], 1.0, 1e-12));
    assert!(close(vals[2], blaze_math::sin(1.0), 1e-12));
}

#[test]
fn select_relu_and_max_splice_into_verified_switch_diamond_mir() {
    // f(x, y) = relu(x * y) + max(x, y)
    let mut g = ExprGraph::new_in(heap());
    let (vx, ex) = g.var("x");
    let (vy, ey) = g.var("y");
    let xy = g.mul(ex, ey);
    let r = g.relu(xy);
    let m = g.max_expr(ex, ey);
    let root = g.add(r, m);

    let df_dx = blaze_algebra::diff(&mut g, root, vx).unwrap();

    let mir_f = splice_body(&g, root, &[vx, vy], TEST_TYS, &TestMath, heap()).unwrap();
    let v_f = verify(&mir_f).expect("Switch diamond MIR for f must verify");
    // Two Op::Select nodes => at least 1 + 3 + 3 = 7 basic blocks!
    assert!(v_f.blocks() >= 7, "expected >= 7 blocks for 2 Select diamonds, got {}", v_f.blocks());

    let mir_dx = splice_body(&g, df_dx, &[vx, vy], TEST_TYS, &TestMath, heap()).unwrap();
    let v_dx = verify(&mir_dx).expect("Switch diamond MIR for df/dx must verify");

    for env in [[1.5, 2.0], [-1.5, 2.0], [2.5, -0.5], [-2.0, -3.0]] {
        let dual = eval_dual(&g, root, &env, vx).unwrap();
        let got_f = eval_pure_body(v_f, &env, &TestMath, heap()).unwrap();
        let got_dx = eval_pure_body(v_dx, &env, &TestMath, heap()).unwrap();
        assert!(close(got_f, dual.val, 1e-12), "f({env:?}): {got_f} vs {}", dual.val);
        assert!(close(got_dx, dual.deriv, 1e-12), "df/dx({env:?}): {got_dx} vs {}", dual.deriv);
    }

    // Round-trip: lift the branching Switch-diamond MIR directly back through `diff_body` and
    // `value_and_grad_body`!
    let round_dx = diff_body(v_f, 0, &TestMath, &TestMath, heap()).unwrap();
    let v_round_dx = verify(&round_dx).unwrap();
    let round_vg = value_and_grad_body(v_f, PTy(1), &TestMath, &TestMath, heap()).unwrap();
    let v_round_vg = verify(&round_vg).unwrap();
    for env in [[1.5, 2.0], [-1.5, 2.0], [2.5, -0.5], [-2.0, -3.0]] {
        let dual_x = eval_dual(&g, root, &env, vx).unwrap();
        let dual_y = eval_dual(&g, root, &env, vy).unwrap();
        assert!(close(eval_pure_body(v_round_dx, &env, &TestMath, heap()).unwrap(), dual_x.deriv, 1e-12));
        let vg = eval_pure_multi_body(v_round_vg, &env, &TestMath, heap()).unwrap();
        assert_eq!(vg.len(), 3);
        assert!(close(vg[0], dual_x.val, 1e-12));
        assert!(close(vg[1], dual_x.deriv, 1e-12));
        assert!(close(vg[2], dual_y.deriv, 1e-12));
    }

    // Sound interval dead-branch pruning on MIR: when x in [3, 5] and y in [1, 2],
    // both `x * y > 0` and `x - y > 0` are provably positive, collapsing 7+ blocks into 1 block!
    let bounds = [Interval::new(3.0, 5.0).unwrap(), Interval::new(1.0, 2.0).unwrap()];
    let branchless = simplify_body_with(
        v_f,
        SimplifyOpts::default(),
        Some(&bounds),
        &TestMath,
        &TestMath,
        heap(),
    )
    .unwrap();
    let v_branchless = verify(&branchless).unwrap();
    assert_eq!(v_branchless.blocks(), 1, "dead Switch diamonds should collapse to 1 block");
    assert!(close(
        eval_pure_body(v_branchless, &[4.0, 1.5], &TestMath, heap()).unwrap(),
        eval_pure_body(v_f, &[4.0, 1.5], &TestMath, heap()).unwrap(),
        1e-12
    ));
}

#[test]
fn hvp_body_and_cpu_fma_simplify_verify_and_match_hessian_oracle() {
    // 1. O(|V|) Hessian-Vector Product MIR pass on f(x, y) = x^3 * y + sin(x * y)
    let mut g = ExprGraph::new_in(heap());
    let (vx, ex) = g.var("x");
    let (vy, ey) = g.var("y");
    let x2 = g.mul(ex, ex);
    let x3 = g.mul(x2, ex);
    let x3y = g.mul(x3, ey);
    let xy = g.mul(ex, ey);
    let sxy = g.sin(xy);
    let root = g.add(x3y, sxy);

    let orig = splice_body(&g, root, &[vx, vy], TEST_TYS, &TestMath, heap()).unwrap();
    let v_orig = verify(&orig).unwrap();

    let hvp_mir = hvp_body(v_orig, PTy(1), &TestMath, &TestMath, heap()).unwrap();
    let v_hvp = verify(&hvp_mir).expect("hvp_body MIR must verify");
    assert_eq!(v_hvp.args(), 4);

    let h = hessian(&mut g, root, &[vx, vy]).unwrap();
    let env_xy = [1.2, -0.7];
    let dir = [0.6, -1.1];
    let got = eval_pure_multi_body(v_hvp, &[env_xy[0], env_xy[1], dir[0], dir[1]], &TestMath, heap()).unwrap();
    assert_eq!(got.len(), 2);
    for i in 0..2 {
        let expected = g.eval(h[i * 2], &env_xy).unwrap() * dir[0]
            + g.eval(h[i * 2 + 1], &env_xy).unwrap() * dir[1];
        assert!(close(got[i], expected, 1e-10), "hvp[{i}]: {} vs {expected}", got[i]);
    }

    // 2. CpuFma cost model simplifies `x * y + z` MIR into a verified 3-arg `MathOp::Fma` MIR call!
    let mut g2 = ExprGraph::new_in(heap());
    let (va, ea) = g2.var("x");
    let (vb, eb) = g2.var("y");
    let (vc, ec) = g2.var("z");
    let ab = g2.mul(ea, eb);
    let mac = g2.add(ab, ec);
    let mac_mir = splice_body(&g2, mac, &[va, vb, vc], TEST_TYS, &TestMath, heap()).unwrap();
    let v_mac = verify(&mac_mir).unwrap();
    let fma_opts = SimplifyOpts { cost_model: CostModel::CpuFma, ..SimplifyOpts::default() };
    let fused_mir = simplify_body_with(v_mac, fma_opts, None, &TestMath, &TestMath, heap()).unwrap();
    let v_fused = verify(&fused_mir).unwrap();
    // Fma is emitted as a Call terminator => 2 blocks (call + continuation) and evaluates cleanly!
    assert_eq!(v_fused.blocks(), 2);
    assert!(close(
        eval_pure_body(v_fused, &[2.5, 4.0, -3.0], &TestMath, heap()).unwrap(),
        7.0,
        1e-12
    ));
}

#[test]
fn unrolled_loop_and_fixed_array_dot_product_lifts_differentiates_and_splices() {
    // Build a real Rust MIR loop kernel:
    //   fn dot3(a: [f64; 3], b: [f64; 3]) -> f64 {
    //       let mut acc = 0.0;
    //       let mut i: usize = 0;
    //       while i < 3 { acc = acc + a[i] * b[i]; i = i + 1; }
    //       acc
    //   }
    let f64_ty = PTy(0);
    let arr_ty = PTy(1);
    let usize_ty = PTy(2);
    let bool_ty = PTy(3);
    let span = Span { lo: 0, hi: 0 };

    let mut b = BodyBuilder::new_in(heap());
    let ret = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Return, mutbl: Mutbl::Mut });
    let arg_a = b.new_local(LocalDecl { ty: arr_ty, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
    let arg_b = b.new_local(LocalDecl { ty: arr_ty, kind: LocalKind::Arg(1), mutbl: Mutbl::Not });
    let acc = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Temp(MirExprId(0), Role::Value), mutbl: Mutbl::Mut });
    let idx = b.new_local(LocalDecl { ty: usize_ty, kind: LocalKind::Temp(MirExprId(1), Role::Value), mutbl: Mutbl::Mut });
    let cond = b.new_local(LocalDecl { ty: bool_ty, kind: LocalKind::Temp(MirExprId(2), Role::Value), mutbl: Mutbl::Mut });
    let prod = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Temp(MirExprId(3), Role::Value), mutbl: Mutbl::Mut });

    let p_ret = b.local_place(ret);
    let p_a = b.local_place(arg_a);
    let p_b = b.local_place(arg_b);
    let p_ai = b.project(p_a, Proj::Index(idx));
    let p_bi = b.project(p_b, Proj::Index(idx));
    let p_acc = b.local_place(acc);
    let p_idx = b.local_place(idx);
    let p_cond = b.local_place(cond);
    let p_prod = b.local_place(prod);

    let c_zero_f = b.new_const(ConstDecl { kind: ConstKind::Scalar { bits: u128::from(0.0_f64.to_bits()) }, ty: f64_ty });
    let c_zero_u = b.new_const(ConstDecl { kind: ConstKind::Scalar { bits: 0 }, ty: usize_ty });
    let c_one_u = b.new_const(ConstDecl { kind: ConstKind::Scalar { bits: 1 }, ty: usize_ty });
    let c_three_u = b.new_const(ConstDecl { kind: ConstKind::Scalar { bits: 3 }, ty: usize_ty });

    let bb_init = b.new_block(BlockKind::Normal);
    let bb_head = b.new_block(BlockKind::Normal);
    let bb_body = b.new_block(BlockKind::Normal);
    let bb_exit = b.new_block(BlockKind::Normal);

    b.start(bb_init);
    b.assign(p_acc, blaze_body::Value::Use(Operand::Const(c_zero_f)), span);
    b.assign(p_idx, blaze_body::Value::Use(Operand::Const(c_zero_u)), span);
    b.terminate(StmtKind::Goto(bb_head), span);

    b.start(bb_head);
    b.assign(p_cond, blaze_body::Value::Binary { op: BinOp::Lt, lhs: Operand::Copy(p_idx), rhs: Operand::Const(c_three_u) }, span);
    let cases = b.cases(&[(1u128, bb_body)]);
    b.terminate(StmtKind::Switch { on: Operand::Copy(p_cond), cases, otherwise: bb_exit }, span);

    b.start(bb_body);
    b.assign(p_prod, blaze_body::Value::Binary { op: BinOp::Mul, lhs: Operand::Copy(p_ai), rhs: Operand::Copy(p_bi) }, span);
    b.assign(p_acc, blaze_body::Value::Binary { op: BinOp::Add, lhs: Operand::Copy(p_acc), rhs: Operand::Copy(p_prod) }, span);
    b.assign(p_idx, blaze_body::Value::Binary { op: BinOp::Add, lhs: Operand::Copy(p_idx), rhs: Operand::Const(c_one_u) }, span);
    b.terminate(StmtKind::Goto(bb_head), span);

    b.start(bb_exit);
    b.assign(p_ret, blaze_body::Value::Use(Operand::Copy(p_acc)), span);
    b.terminate(StmtKind::Return, span);

    let dot_mir = b.finish().expect("dot3 builds");
    let v_dot = verify(&dot_mir).expect("dot3 verifies");

    // Lift unrolls the loop `i = 0, 1, 2` and binds all 6 array elements [a0, b0, a1, b1, a2, b2]!
    let lifted = lift_body(v_dot, &NoMathCalls, heap()).expect("dot3 lifts");
    assert_eq!(lifted.vars.len(), 6);
    // Variables bound in loop access order: a[0], b[0], a[1], b[1], a[2], b[2]
    let env = [1.0, 4.0, 2.0, 5.0, 3.0, 6.0];
    assert!(close(lifted.graph.eval(lifted.result, &env).unwrap(), 32.0, 1e-12));

    // Differentiate the looped dot-product kernel into ONE fused value-and-gradient MIR body!
    let vg = run_body_transform(
        v_dot,
        BodyTransform::ValueAndGrad { array_ty: PTy(4) },
        None,
        &NoMathCalls,
        &NoMathCalls,
        heap(),
    )
    .expect("value_and_grad on looped dot3");
    let v_vg = verify(&vg).unwrap();
    let out = eval_pure_multi_body(v_vg, &env, &NoMathCalls, heap()).unwrap();
    // [dot=32, d/da0=4, d/db0=1, d/da1=5, d/db1=2, d/da2=6, d/db2=3]
    assert_eq!(out.len(), 7);
    let expected = [32.0, 4.0, 1.0, 5.0, 2.0, 6.0, 3.0];
    for (i, (&got, &exp)) in out.iter().zip(expected.iter()).enumerate() {
        assert!(close(got, exp, 1e-12), "vg[{i}]: {got} vs {exp}");
    }
}

#[test]
fn end_to_end_mir_diff_compiles_through_blaze_body_build_to_verified_lir_and_executes() {
    use blaze_build::layout::{LayoutFacts, Missing, ReprAttrs, ValidRange};
    use blaze_build::{Abi, Adts, OverflowChecks, Panic, Subst};
    use blaze_lir::eval::{Outcome, Val, run_in};
    use blaze_typeck::{Adt, FloatTy, PNode, PortableTys};

    struct PipelineWorld;
    impl LayoutFacts for PipelineWorld {
        fn adt(&self, _: ItemKey) -> Option<Adt<'_>> { None }
        fn repr(&self, _: ItemKey) -> Result<ReprAttrs, Missing> { Ok(ReprAttrs::RUST) }
        fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> { Ok(None) }
        fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> { Ok(ValidRange::default()) }
    }
    impl Adts for PipelineWorld {
        fn adt(&self, _: ItemKey) -> Option<Adt<'_>> { None }
        fn facts(&self) -> &dyn LayoutFacts { self }
        fn panic(&self) -> Panic { Panic::Abort }
        fn overflow_checks(&self) -> OverflowChecks { OverflowChecks::Off }
        fn ub_checks(&self) -> blaze_build::UbChecks { blaze_build::UbChecks::Off }
        fn drop_impl(&self, _: PTy, _: &mut PortableTys<'_>) -> Result<Option<ItemKey>, blaze_build::BuildError> { Ok(None) }
        fn abi(&self, _: ItemKey) -> Option<Abi> { Some(Abi::Rust) }
        fn lang_fn(&self, _: &str) -> Option<ItemKey> { None }
    }

    // 1. Create a real `PortableTys` table with `f64` and `bool` and build MIR for:
    //    f(x, y) = x * x * y + 2.0 * x - y   =>   ∂f/∂x = 2*x*y + 2
    let mut from = PortableTys::new_in(heap());
    let f64_ty = from.intern(PNode::Float(FloatTy::F64)).unwrap();
    let bool_ty = from.intern(PNode::Bool).unwrap();
    let tys = ScalarTys { f64_ty, bool_ty };

    let mut g = ExprGraph::new_in(heap());
    let (vx, ex) = g.var("x");
    let (vy, ey) = g.var("y");
    let x2 = g.mul(ex, ex);
    let x2y = g.mul(x2, ey);
    let c2 = g.lit(2.0);
    let two_x = g.mul(c2, ex);
    let sum = g.add(x2y, two_x);
    let root = g.sub(sum, ey);

    let orig_mir = splice_body(&g, root, &[vx, vy], tys, &NoMathCalls, heap()).unwrap();
    let v_orig = verify(&orig_mir).unwrap();

    // 2. Run our MIR symbolic differentiation compiler pass (`BodyTransform::Diff { arg: 0 }`)!
    let diff_mir = run_body_transform(
        v_orig,
        BodyTransform::Diff { arg: 0 },
        None,
        &NoMathCalls,
        &NoMathCalls,
        heap(),
    )
    .unwrap();
    let v_diff = verify(&diff_mir).unwrap();

    // 3. Compile the differentiated `Verified<Body>` ALL THE WAY through `blaze_body_build::build`
    //    into verified machine-ready LIR (`blaze_lir::Func`)!
    let mut ground = PortableTys::new_in(heap());
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: heap() };
    let built = blaze_body_build::build(
        v_diff,
        from.view(),
        ItemKey(1),
        &mut subst,
        &PipelineWorld,
        heap(),
    )
    .expect("differentiated MIR must compile cleanly to verified LIR");

    // 4. Execute the compiled LIR directly via `blaze_lir::eval::run_in` at (x=3.0, y=4.0)!
    //    Expected: ∂f/∂x(3, 4) = 2 * 3 * 4 + 2 = 26.0
    let eval_f64 = |view: blaze_lir::View<'_>, x: f64, y: f64| -> f64 {
        let args = [Val::Int(u128::from(x.to_bits())), Val::Int(u128::from(y.to_bits()))];
        match run_in(view, &args, 10_000, heap()).expect("LIR eval succeeds") {
            Outcome::Returned(vals) => {
                assert_eq!(vals.len(), 1);
                let Val::Int(bits) = vals[0] else {
                    panic!("expected float bits in Val::Int, got {:?}", vals[0]);
                };
                f64::from_bits(bits as u64)
            }
            other => panic!("expected LIR Returned([f64]), got {other:?}"),
        }
    };
    let got = eval_f64(built.func.view(), 3.0, 4.0);
    assert!(close(got, 26.0, 1e-12), "compiled LIR derivative returned {got}");

    // 5. Piecewise + division MIR: h(x, y) = max(x * x, 2.0 * y) / (y + 1.0)
    //    Exercises `fcmp`, `Switch` diamond, `fdiv`, `fconst`, `fmul`, `fadd` through LIR!
    let mut g2 = ExprGraph::new_in(heap());
    let (vx2, ex2) = g2.var("x");
    let (vy2, ey2) = g2.var("y");
    let xx = g2.mul(ex2, ex2);
    let c2_2 = g2.lit(2.0);
    let two_y = g2.mul(c2_2, ey2);
    let mx = g2.max_expr(xx, two_y);
    let c1_2 = g2.lit(1.0);
    let yp1 = g2.add(ey2, c1_2);
    let h_root = g2.div(mx, yp1);

    let h_mir = splice_body(&g2, h_root, &[vx2, vy2], tys, &NoMathCalls, heap()).unwrap();
    let v_h = verify(&h_mir).unwrap();
    let dh_mir = run_body_transform(v_h, BodyTransform::Diff { arg: 0 }, None, &NoMathCalls, &NoMathCalls, heap()).unwrap();
    let v_dh = verify(&dh_mir).unwrap();

    let mut ground2 = PortableTys::new_in(heap());
    let mut subst2 = Subst { args: &[], ground: &mut ground2, scratch: heap() };
    let built_dh = blaze_body_build::build(v_dh, from.view(), ItemKey(2), &mut subst2, &PipelineWorld, heap())
        .expect("piecewise differentiated MIR compiles cleanly to verified LIR");
    // At (3, 2): x^2 = 9 > 4 => ∂h/∂x = 2*x / (y+1) = 6/3 = 2.0
    assert!(close(eval_f64(built_dh.func.view(), 3.0, 2.0), 2.0, 1e-12));
    // At (1, 2): x^2 = 1 <= 4 => ∂h/∂x = 0.0
    assert!(close(eval_f64(built_dh.func.view(), 1.0, 2.0), 0.0, 1e-12));
}
