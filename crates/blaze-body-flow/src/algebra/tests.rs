use blaze_algebra::{CostModel, ExprGraph, Interval, SimplifyOpts, eval_dual, hessian};
use blaze_body::{
    BinOp, Block, BlockKind, Body, BodyBuilder, Callee, ConstDecl, ConstKind, LocalDecl, LocalKind,
    Operand, Proj, Role, Span, StmtKind,
    body::{ExprId as MirExprId, ItemKey, List, Mutbl, PTy},
    verify,
};
use blaze_own::Alloc;

use super::{
    BodyAlgebraError, BodyPlant, BodyTransform, CalleeBodyResolver, MathEmitter, MathOp,
    MathResolver, NoMathCalls, ScalarTys, diff_body, diff_body_with_plant, eval_pure_body,
    eval_pure_multi_body, grad_bodies, hvp_body, lift_body, run_body_transform,
    run_body_transform_with_helpers, simplify_body, simplify_body_with, splice_body,
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
fn iterative_lift_region_handles_nested_direct_return_and_nested_diamond_switches() {
    let f64_ty = PTy(0);
    let span = Span { lo: 0, hi: 0 };

    // 1. Nested Switch with direct returns (no join block: join = None at both levels):
    //    if x > 0 { if y > 0 { return x * y; } else { return x - y; } } else { return y * y; }
    let nested_ret = {
        let mut b = BodyBuilder::new_in(heap());
        let l0 = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Return, mutbl: Mutbl::Mut });
        let lx = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
        let ly = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Arg(1), mutbl: Mutbl::Not });
        let c1 = b.new_local(LocalDecl {
            ty: f64_ty,
            kind: LocalKind::Temp(MirExprId(0), Role::Value),
            mutbl: Mutbl::Not,
        });
        let c2 = b.new_local(LocalDecl {
            ty: f64_ty,
            kind: LocalKind::Temp(MirExprId(1), Role::Value),
            mutbl: Mutbl::Not,
        });
        let p0 = b.local_place(l0);
        let px = b.local_place(lx);
        let py = b.local_place(ly);
        let pc1 = b.local_place(c1);
        let pc2 = b.local_place(c2);
        let zero = b.new_const(ConstDecl {
            kind: ConstKind::Scalar { bits: u128::from(0.0_f64.to_bits()) },
            ty: f64_ty,
        });

        let bb0 = b.new_block(BlockKind::Normal);
        let bb_x_pos = b.new_block(BlockKind::Normal);
        let bb_x_neg = b.new_block(BlockKind::Normal);
        let bb_xy_pos = b.new_block(BlockKind::Normal);
        let bb_y_neg = b.new_block(BlockKind::Normal);

        b.start(bb0);
        b.assign(
            pc1,
            blaze_body::Value::Binary {
                op: BinOp::Gt,
                lhs: Operand::Copy(px),
                rhs: Operand::Const(zero),
            },
            span,
        );
        let cases1 = b.cases(&[(1u128, bb_x_pos)]);
        b.terminate(
            StmtKind::Switch { on: Operand::Copy(pc1), cases: cases1, otherwise: bb_x_neg },
            span,
        );

        b.start(bb_x_pos);
        b.assign(
            pc2,
            blaze_body::Value::Binary {
                op: BinOp::Gt,
                lhs: Operand::Copy(py),
                rhs: Operand::Const(zero),
            },
            span,
        );
        let cases2 = b.cases(&[(1u128, bb_xy_pos)]);
        b.terminate(
            StmtKind::Switch { on: Operand::Copy(pc2), cases: cases2, otherwise: bb_y_neg },
            span,
        );

        b.start(bb_xy_pos);
        b.assign(
            p0,
            blaze_body::Value::Binary {
                op: BinOp::Mul,
                lhs: Operand::Copy(px),
                rhs: Operand::Copy(py),
            },
            span,
        );
        b.terminate(StmtKind::Return, span);

        b.start(bb_y_neg);
        b.assign(
            p0,
            blaze_body::Value::Binary {
                op: BinOp::Sub,
                lhs: Operand::Copy(px),
                rhs: Operand::Copy(py),
            },
            span,
        );
        b.terminate(StmtKind::Return, span);

        b.start(bb_x_neg);
        b.assign(
            p0,
            blaze_body::Value::Binary {
                op: BinOp::Mul,
                lhs: Operand::Copy(py),
                rhs: Operand::Copy(py),
            },
            span,
        );
        b.terminate(StmtKind::Return, span);

        b.finish().expect("valid nested-return switch MIR")
    };

    let v_ret = verify(&nested_ret).expect("verify nested-return MIR");
    let dx_ret = diff_body(v_ret, 0, &NoMathCalls, &TestMath, heap()).expect("diff wrt x");
    let dy_ret = diff_body(v_ret, 1, &NoMathCalls, &TestMath, heap()).expect("diff wrt y");
    let v_dx_ret = verify(&dx_ret).expect("verify dx");
    let v_dy_ret = verify(&dy_ret).expect("verify dy");

    // (x, y, expected_val, expected_dx, expected_dy)
    for (x, y, exp_v, exp_dx, exp_dy) in [
        (2.0, 3.0, 6.0, 3.0, 2.0),
        (2.0, -3.0, 5.0, 1.0, -1.0),
        (-2.0, 3.0, 9.0, 0.0, 6.0),
        (-2.0, -3.0, 9.0, 0.0, -6.0),
    ] {
        let env = [x, y];
        assert!(close(eval_pure_body(v_ret, &env, &NoMathCalls, heap()).unwrap(), exp_v, 1e-12));
        assert!(close(eval_pure_body(v_dx_ret, &env, &TestMath, heap()).unwrap(), exp_dx, 1e-12));
        assert!(close(eval_pure_body(v_dy_ret, &env, &TestMath, heap()).unwrap(), exp_dy, 1e-12));
    }

    // 2. Nested Switch diamond inside a Switch diamond (join = Some(...) at both levels):
    //    let out = if x > 0 { if y > 0 { x * y } else { x + y } } else { -x };
    //    return out * 2.0;
    let nested_diamond = {
        let mut b = BodyBuilder::new_in(heap());
        let l0 = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Return, mutbl: Mutbl::Mut });
        let lx = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
        let ly = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Arg(1), mutbl: Mutbl::Not });
        let l_out = b.new_local(LocalDecl {
            ty: f64_ty,
            kind: LocalKind::Temp(MirExprId(0), Role::Value),
            mutbl: Mutbl::Mut,
        });
        let c1 = b.new_local(LocalDecl {
            ty: f64_ty,
            kind: LocalKind::Temp(MirExprId(1), Role::Value),
            mutbl: Mutbl::Not,
        });
        let c2 = b.new_local(LocalDecl {
            ty: f64_ty,
            kind: LocalKind::Temp(MirExprId(2), Role::Value),
            mutbl: Mutbl::Not,
        });
        let p0 = b.local_place(l0);
        let px = b.local_place(lx);
        let py = b.local_place(ly);
        let pout = b.local_place(l_out);
        let pc1 = b.local_place(c1);
        let pc2 = b.local_place(c2);
        let zero = b.new_const(ConstDecl {
            kind: ConstKind::Scalar { bits: u128::from(0.0_f64.to_bits()) },
            ty: f64_ty,
        });
        let two = b.new_const(ConstDecl {
            kind: ConstKind::Scalar { bits: u128::from(2.0_f64.to_bits()) },
            ty: f64_ty,
        });

        let bb0 = b.new_block(BlockKind::Normal);
        let bb_x_pos = b.new_block(BlockKind::Normal);
        let bb_x_neg = b.new_block(BlockKind::Normal);
        let bb_xy_pos = b.new_block(BlockKind::Normal);
        let bb_y_neg = b.new_block(BlockKind::Normal);
        let bb_inner_join = b.new_block(BlockKind::Normal);
        let bb_outer_join = b.new_block(BlockKind::Normal);

        b.start(bb0);
        b.assign(
            pc1,
            blaze_body::Value::Binary {
                op: BinOp::Gt,
                lhs: Operand::Copy(px),
                rhs: Operand::Const(zero),
            },
            span,
        );
        let cases1 = b.cases(&[(1u128, bb_x_pos)]);
        b.terminate(
            StmtKind::Switch { on: Operand::Copy(pc1), cases: cases1, otherwise: bb_x_neg },
            span,
        );

        b.start(bb_x_pos);
        b.assign(
            pc2,
            blaze_body::Value::Binary {
                op: BinOp::Gt,
                lhs: Operand::Copy(py),
                rhs: Operand::Const(zero),
            },
            span,
        );
        let cases2 = b.cases(&[(1u128, bb_xy_pos)]);
        b.terminate(
            StmtKind::Switch { on: Operand::Copy(pc2), cases: cases2, otherwise: bb_y_neg },
            span,
        );

        b.start(bb_xy_pos);
        b.assign(
            pout,
            blaze_body::Value::Binary {
                op: BinOp::Mul,
                lhs: Operand::Copy(px),
                rhs: Operand::Copy(py),
            },
            span,
        );
        b.terminate(StmtKind::Goto(bb_inner_join), span);

        b.start(bb_y_neg);
        b.assign(
            pout,
            blaze_body::Value::Binary {
                op: BinOp::Add,
                lhs: Operand::Copy(px),
                rhs: Operand::Copy(py),
            },
            span,
        );
        b.terminate(StmtKind::Goto(bb_inner_join), span);

        b.start(bb_inner_join);
        b.terminate(StmtKind::Goto(bb_outer_join), span);

        b.start(bb_x_neg);
        b.assign(
            pout,
            blaze_body::Value::Unary { op: blaze_body::UnOp::Neg, operand: Operand::Copy(px) },
            span,
        );
        b.terminate(StmtKind::Goto(bb_outer_join), span);

        b.start(bb_outer_join);
        b.assign(
            p0,
            blaze_body::Value::Binary {
                op: BinOp::Mul,
                lhs: Operand::Copy(pout),
                rhs: Operand::Const(two),
            },
            span,
        );
        b.terminate(StmtKind::Return, span);

        b.finish().expect("valid nested-diamond switch MIR")
    };

    let v_dia = verify(&nested_diamond).expect("verify nested-diamond MIR");
    let dx_dia = diff_body(v_dia, 0, &NoMathCalls, &TestMath, heap()).expect("diff diamond wrt x");
    let dy_dia = diff_body(v_dia, 1, &NoMathCalls, &TestMath, heap()).expect("diff diamond wrt y");
    let v_dx_dia = verify(&dx_dia).expect("verify diamond dx");
    let v_dy_dia = verify(&dy_dia).expect("verify diamond dy");

    for (x, y, exp_v, exp_dx, exp_dy) in [
        (2.0, 3.0, 12.0, 6.0, 4.0),
        (2.0, -3.0, -2.0, 2.0, 2.0),
        (-2.0, 3.0, 4.0, -2.0, 0.0),
        (-2.0, -3.0, 4.0, -2.0, 0.0),
    ] {
        let env = [x, y];
        assert!(close(eval_pure_body(v_dia, &env, &NoMathCalls, heap()).unwrap(), exp_v, 1e-12));
        assert!(close(eval_pure_body(v_dx_dia, &env, &TestMath, heap()).unwrap(), exp_dx, 1e-12));
        assert!(close(eval_pure_body(v_dy_dia, &env, &TestMath, heap()).unwrap(), exp_dy, 1e-12));
    }
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

#[test]
fn interprocedural_helper_inlining_differentiates_modular_functions_and_enforces_depth_limit() {
    use blaze_body::Value;

    let spring_key = ItemKey(500);
    // Helper: fn spring(d: f64, k: f64) -> f64 { 0.5 * k * d * d }
    let spring_body = {
        let mut g = ExprGraph::new_in(heap());
        let (vd, ed) = g.var("d");
        let (vk, ek) = g.var("k");
        let half = g.lit(0.5);
        let d2 = g.mul(ed, ed);
        let kd2 = g.mul(ek, d2);
        let root = g.mul(half, kd2);
        splice_body(&g, root, &[vd, vk], TEST_TYS, &NoMathCalls, heap()).unwrap()
    };
    let v_spring = verify(&spring_body).unwrap();

    // Caller: fn energy(x: f64, y: f64) -> f64 { spring(x - y, 4.0) + 0.5 * x * x }
    let energy_body = {
        let mut b = BodyBuilder::new_in(heap());
        let ret = b.new_local(LocalDecl { ty: PTy(0), kind: LocalKind::Return, mutbl: Mutbl::Mut });
        let lx = b.new_local(LocalDecl { ty: PTy(0), kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
        let ly = b.new_local(LocalDecl { ty: PTy(0), kind: LocalKind::Arg(1), mutbl: Mutbl::Not });
        let ld = b.new_local(LocalDecl { ty: PTy(0), kind: LocalKind::Temp(MirExprId(0), Role::Value), mutbl: Mutbl::Not });
        let ls = b.new_local(LocalDecl { ty: PTy(0), kind: LocalKind::Temp(MirExprId(1), Role::Value), mutbl: Mutbl::Not });
        let lx2 = b.new_local(LocalDecl { ty: PTy(0), kind: LocalKind::Temp(MirExprId(2), Role::Value), mutbl: Mutbl::Not });
        let lreg = b.new_local(LocalDecl { ty: PTy(0), kind: LocalKind::Temp(MirExprId(3), Role::Value), mutbl: Mutbl::Not });
        let bb0 = b.new_block(BlockKind::Normal);
        let bb1 = b.new_block(BlockKind::Normal);
        let span = Span { lo: 0, hi: 0 };

        b.start(bb0);
        let px = b.local_place(lx);
        let py = b.local_place(ly);
        let pd = b.local_place(ld);
        let ps = b.local_place(ls);
        let px2 = b.local_place(lx2);
        let preg = b.local_place(lreg);
        let pret = b.local_place(ret);
        b.assign(pd, Value::Binary { op: BinOp::Sub, lhs: Operand::Copy(px), rhs: Operand::Copy(py) }, span);
        let c4 = b.new_const(ConstDecl { kind: ConstKind::Scalar { bits: u128::from(4.0_f64.to_bits()) }, ty: PTy(0) });
        let callee = b.callee(Callee::FnItem { key: spring_key, args: List::EMPTY });
        let args = b.operands(&[Operand::Copy(pd), Operand::Const(c4)]);
        b.terminate(
            StmtKind::Call { callee, args, dest: Some(ps), target: Some(bb1), unwind: blaze_body::Unwind::Continue },
            span,
        );

        b.start(bb1);
        b.assign(px2, Value::Binary { op: BinOp::Mul, lhs: Operand::Copy(px), rhs: Operand::Copy(px) }, span);
        let c_half = b.new_const(ConstDecl { kind: ConstKind::Scalar { bits: u128::from(0.5_f64.to_bits()) }, ty: PTy(0) });
        b.assign(preg, Value::Binary { op: BinOp::Mul, lhs: Operand::Const(c_half), rhs: Operand::Copy(px2) }, span);
        b.assign(pret, Value::Binary { op: BinOp::Add, lhs: Operand::Copy(ps), rhs: Operand::Copy(preg) }, span);
        b.terminate(StmtKind::Return, span);
        b.finish().unwrap()
    };
    let v_energy = verify(&energy_body).unwrap();

    struct OneHelper<'a> {
        key: ItemKey,
        body: blaze_body::Verified<'a>,
    }
    impl CalleeBodyResolver for OneHelper<'_> {
        fn resolve_body(&self, callee: Callee) -> Option<blaze_body::Verified<'_>> {
            match callee {
                Callee::FnItem { key, .. } if key == self.key => Some(self.body),
                _ => None,
            }
        }
    }

    let helpers = OneHelper { key: spring_key, body: v_spring };
    let vg = run_body_transform_with_helpers(
        v_energy,
        BodyTransform::ValueAndGrad { array_ty: PTy(1) },
        None,
        &NoMathCalls,
        &helpers,
        &NoMathCalls,
        heap(),
    )
    .expect("inlined value_and_grad succeeds");
    let v_vg = verify(&vg).unwrap();

    // At (x = 5.0, y = 2.0):
    //   energy = 2*(5-2)^2 + 0.5*25 = 18 + 12.5 = 30.5
    //   ∂/∂x   = 4*(5-2) + 5        = 17.0
    //   ∂/∂y   = -4*(5-2)           = -12.0
    let out = eval_pure_multi_body(v_vg, &[5.0, 2.0], &NoMathCalls, heap()).unwrap();
    assert_eq!(out.len(), 3);
    assert!(close(out[0], 30.5, 1e-12), "energy={}", out[0]);
    assert!(close(out[1], 17.0, 1e-12), "d/dx={}", out[1]);
    assert!(close(out[2], -12.0, 1e-12), "d/dy={}", out[2]);

    // Self-recursive helper must hit bounded `InlineDepthLimit` (rule 18 `bounded-progress`).
    let rec_key = ItemKey(501);
    let rec_body = {
        let mut b = BodyBuilder::new_in(heap());
        let ret = b.new_local(LocalDecl { ty: PTy(0), kind: LocalKind::Return, mutbl: Mutbl::Mut });
        let lx = b.new_local(LocalDecl { ty: PTy(0), kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
        let bb0 = b.new_block(BlockKind::Normal);
        let bb1 = b.new_block(BlockKind::Normal);
        let span = Span { lo: 0, hi: 0 };
        b.start(bb0);
        let callee = b.callee(Callee::FnItem { key: rec_key, args: List::EMPTY });
        let px = b.local_place(lx);
        let pret = b.local_place(ret);
        let args = b.operands(&[Operand::Copy(px)]);
        b.terminate(
            StmtKind::Call { callee, args, dest: Some(pret), target: Some(bb1), unwind: blaze_body::Unwind::Continue },
            span,
        );
        b.start(bb1);
        b.terminate(StmtKind::Return, span);
        b.finish().unwrap()
    };
    let v_rec = verify(&rec_body).unwrap();
    let rec_helpers = OneHelper { key: rec_key, body: v_rec };
    let err = run_body_transform_with_helpers(
        v_rec,
        BodyTransform::Diff { arg: 0 },
        None,
        &NoMathCalls,
        &rec_helpers,
        &NoMathCalls,
        heap(),
    )
    .err()
    .expect("self-recursive helper must be refused by InlineDepthLimit");
    assert!(matches!(err, BodyAlgebraError::InlineDepthLimit { depth: 8, .. }), "got {err:?}");
}

#[test]
fn real_typeck_body_lower_algebra_fma_body_build_native_lir_pipeline() {
    use blaze_body_lower::{BodyFacts, lower};
    use blaze_build::intrinsics::target::{Target as IntrTarget, float::{FloatCall, FloatFn, Shape}};
    use blaze_build::layout::{LayoutFacts, Missing, ReprAttrs, ValidRange};
    use blaze_build::{Abi, Adts, Intrinsic, OverflowChecks, Panic, Subst};
    use blaze_flow_lower::{AdtFacts, CalleeFacts, Facts};
    use blaze_lir::eval::{Outcome, Val, run_in};
    use blaze_typeck::fixture::{Bb, Fx};
    use blaze_typeck::{
        Adt, BinOp as TyBinOp, Checked, Expr, ExprId as TyExprId, FloatTy, InferCtx, Name, Needs,
        Outcome as TyOutcome, PNode, PortableTys,
    };

    // 1. Real typeck world with `fn spring(x: f64, y: f64) -> f64` and `fn loss(x: f64, y: f64) -> f64`.
    let mut fx = Fx::new();
    let f64_fx = fx.ty(PNode::Float(FloatTy::F64));
    let spring_key = fx.func("spring", 0, &[f64_fx, f64_fx], f64_fx, Default::default());
    let loss_key = fx.func("loss", 0, &[f64_fx, f64_fx], f64_fx, Default::default());
    fx.finish();

    // Helper for building typeck expressions on `Bb`.
    let mk_bin = |b: &mut Bb, op: TyBinOp, lhs: TyExprId, rhs: TyExprId| -> TyExprId {
        let id = b.bool();
        b.exprs[id.ix()] = Expr::Binary { op, lhs, rhs };
        id
    };
    let mk_if = |b: &mut Bb, cond: TyExprId, then: TyExprId, else_: TyExprId| -> TyExprId {
        let id = b.bool();
        b.exprs[id.ix()] = Expr::If { cond, then, else_: Some(else_) };
        id
    };

    struct FloatFacts<'a> {
        floats: &'a [(TyExprId, f64)],
    }
    impl Facts for FloatFacts<'_> {
        fn regions(&self) -> &dyn blaze_flow_lower::RegionFacts { &blaze_flow_lower::NoRegions }
        fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> { None }
        fn adt_variances(&self, _: ItemKey) -> Option<&[blaze_flow_lower::Variance]> { None }
        fn implied_bounds(&self) -> Option<(&[(u32, blaze_flow_lower::Region)], &[(blaze_flow_lower::Region, blaze_flow_lower::Region)])> { None }
        fn constant_initializer(&self) -> bool { false }
        fn fn_binders(&self) -> bool { true }
        fn closure_bodies_checked(&self) -> bool { false }
        fn closure_requirements(&self, _: TyExprId) -> &[blaze_flow_lower::Requirement] { &[] }
        fn async_bodies_checked(&self) -> bool { false }
        fn is_future(&self, _: PTy) -> Option<bool> { None }
        fn shape(&self, _: ItemKey) -> Option<blaze_flow_lower::Shape<'_>> { None }
        fn object_trait_lifetimes(&self, _: ItemKey) -> Option<bool> { None }
        fn call_bounds(&self, _: ItemKey) -> Option<blaze_flow_lower::CallBounds<'_>> { None }
        fn trait_lifetimes(&self, _: ItemKey) -> Option<u32> { None }
        fn alias_lifetimes(&self, _: ItemKey, _: Name) -> Option<u32> { None }
        fn ties(&self, _: ItemKey) -> Option<blaze_flow_lower::Ties<'_>> { None }
        fn field_param(&self, _: ItemKey, _: u32, _: u32) -> Option<u32> { None }
        fn field_lead(&self, _: ItemKey, _: u32, _: u32) -> Option<blaze_flow_lower::FieldLead> { None }
        fn field_regions(&self, _: ItemKey, _: u32, _: u32) -> Option<&[blaze_flow_lower::FieldRegion]> { None }
        fn copy_args(&self, _: ItemKey) -> Option<&[bool]> { None }
        fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
            Some(AdtFacts { copy: true, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum: false, is_box: false })
        }
        fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
            Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
        }
        fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> { None }
        fn field_count(&self, _: ItemKey, _: u32) -> Option<u32> { None }
        fn param_copy(&self, _: u32) -> bool { true }
    }
    impl BodyFacts for FloatFacts<'_> {
        fn float_lit(&self, e: TyExprId) -> Option<u128> {
            self.floats.iter().find(|&&(id, _)| id == e).map(|&(_, v)| u128::from(v.to_bits()))
        }
        fn trait_item(&self, _: ItemKey) -> Option<bool> { Some(false) }
    }

    let check_and_lower = |fx: &Fx, b: &Bb, owner: ItemKey, root: TyExprId, facts: &FloatFacts<'_>| -> (Checked<'static>, Body<Alloc<'static>>) {
        let mut cx = InferCtx::new_in(heap());
        let view = b.view(owner, root);
        let ck = match blaze_typeck::check(&view, fx, &mut cx, &Needs::new_in(heap()), heap()) {
            Ok(TyOutcome::Done(c)) => {
                assert!(c.diags.is_empty(), "typeck diagnostics: {:?}", c.diags);
                c
            }
            Ok(TyOutcome::Need(n)) => panic!("typeck needed: {:?}", n.questions),
            Err(e) => panic!("typeck error: {e:?}"),
        };
        let mir = lower(&view, &ck, facts, heap()).expect("blaze-body-lower succeeds");
        (ck, mir)
    };

    // 2. Build & lower REAL `fn spring(x: f64, y: f64) -> f64 { x * x * y + 2.0 * x }`.
    let mut bs = Bb::new();
    let sx = bs.param("x");
    let sy = bs.param("y");
    let sx1 = bs.local(sx);
    let sx2 = bs.local(sx);
    let s_x2 = mk_bin(&mut bs, TyBinOp::Mul, sx1, sx2);
    let sy1 = bs.local(sy);
    let s_x2y = mk_bin(&mut bs, TyBinOp::Mul, s_x2, sy1);
    let lit2 = bs.float(Some(FloatTy::F64));
    let sx3 = bs.local(sx);
    let s_2x = mk_bin(&mut bs, TyBinOp::Mul, lit2, sx3);
    let spring_root = mk_bin(&mut bs, TyBinOp::Add, s_x2y, s_2x);
    let spring_facts = FloatFacts { floats: &[(lit2, 2.0)] };
    let (_ck_spring, spring_mir) = check_and_lower(&fx, &bs, spring_key, spring_root, &spring_facts);
    let v_spring = verify(&spring_mir).expect("lowered spring MIR verifies");

    // 3. Build & lower REAL `fn loss(x: f64, y: f64) -> f64 { let s = spring(x, y); if s > 10.0 { s + 3.0 * y } else { y * y + x } }`.
    let mut bl = Bb::new();
    let lx = bl.param("x");
    let ly = bl.param("y");
    let sp_fn = bl.path(&["spring"]);
    let lx1 = bl.local(lx);
    let ly1 = bl.local(ly);
    let sp_call = bl.call(sp_fn, &[lx1, ly1]);
    let s_pat = bl.bind("s");
    let let_s = bl.let_(s_pat, None, sp_call);

    let s_ref1 = bl.local(s_pat);
    let lit10 = bl.float(Some(FloatTy::F64));
    let cond = mk_bin(&mut bl, TyBinOp::Gt, s_ref1, lit10);

    let s_ref2 = bl.local(s_pat);
    let lit3 = bl.float(Some(FloatTy::F64));
    let ly2 = bl.local(ly);
    let three_y = mk_bin(&mut bl, TyBinOp::Mul, lit3, ly2);
    let then_e = mk_bin(&mut bl, TyBinOp::Add, s_ref2, three_y);

    let ly3 = bl.local(ly);
    let ly4 = bl.local(ly);
    let y2 = mk_bin(&mut bl, TyBinOp::Mul, ly3, ly4);
    let lx2 = bl.local(lx);
    let else_e = mk_bin(&mut bl, TyBinOp::Add, y2, lx2);

    let if_e = mk_if(&mut bl, cond, then_e, else_e);
    let loss_root = bl.block(&[let_s], Some(if_e));
    let loss_facts = FloatFacts { floats: &[(lit10, 10.0), (lit3, 3.0)] };
    let (ck_loss, loss_mir) = check_and_lower(&fx, &bl, loss_key, loss_root, &loss_facts);
    let v_loss = verify(&loss_mir).expect("lowered loss MIR verifies");

    // 4. Run `run_body_transform_with_helpers`: inline `spring` into `loss`, differentiate w.r.t. `x`,
    //    and fuse `2*x*y + 2` into native `Op::Fma` using `CostModel::CpuFma`!
    struct HelperMap<'a> {
        key: ItemKey,
        body: blaze_body::Verified<'a>,
    }
    impl CalleeBodyResolver for HelperMap<'_> {
        fn resolve_body(&self, callee: Callee) -> Option<blaze_body::Verified<'_>> {
            match callee {
                Callee::FnItem { key, .. } if key == self.key => Some(self.body),
                _ => None,
            }
        }
    }
    const FMA_KEY: ItemKey = ItemKey(900);
    const SQRT_KEY: ItemKey = ItemKey(901);
    const FABS_KEY: ItemKey = ItemKey(902);
    struct IntrMath;
    impl MathResolver for IntrMath {
        fn classify(&self, callee: Callee) -> Option<MathOp> {
            match callee {
                Callee::Intrinsic { key, .. } | Callee::FnItem { key, .. } => match key {
                    k if k == FMA_KEY => Some(MathOp::Fma),
                    k if k == SQRT_KEY => Some(MathOp::Sqrt),
                    k if k == FABS_KEY => Some(MathOp::Abs),
                    _ => None,
                },
                _ => None,
            }
        }
    }
    impl MathEmitter for IntrMath {
        fn callee_for(&self, op: MathOp) -> Option<Callee> {
            let key = match op {
                MathOp::Fma => FMA_KEY,
                MathOp::Sqrt => SQRT_KEY,
                MathOp::Abs => FABS_KEY,
                _ => return None,
            };
            Some(Callee::Intrinsic { key, args: List::EMPTY })
        }
    }

    let helpers = HelperMap { key: spring_key, body: v_spring };
    let fma_opts = SimplifyOpts { cost_model: CostModel::CpuFma, ..SimplifyOpts::default() };
    let diff_mir = run_body_transform_with_helpers(
        v_loss,
        BodyTransform::DiffWith { arg: 0, opts: fma_opts },
        None,
        &IntrMath,
        &helpers,
        &IntrMath,
        heap(),
    )
    .expect("inlined + FMA-fused MIR diff succeeds");
    let v_diff = verify(&diff_mir).expect("transformed MIR verifies");

    // 5. Compile the FMA-fused differentiated MIR through `blaze_body_build::build` into verified LIR!
    struct NativeMathWorld;
    impl LayoutFacts for NativeMathWorld {
        fn adt(&self, _: ItemKey) -> Option<Adt<'_>> { None }
        fn repr(&self, _: ItemKey) -> Result<ReprAttrs, Missing> { Ok(ReprAttrs::RUST) }
        fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> { Ok(None) }
        fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> { Ok(ValidRange::default()) }
    }
    impl Adts for NativeMathWorld {
        fn adt(&self, _: ItemKey) -> Option<Adt<'_>> { None }
        fn facts(&self) -> &dyn LayoutFacts { self }
        fn panic(&self) -> Panic { Panic::Abort }
        fn overflow_checks(&self) -> OverflowChecks { OverflowChecks::Off }
        fn ub_checks(&self) -> blaze_build::UbChecks { blaze_build::UbChecks::Off }
        fn drop_impl(&self, _: PTy, _: &mut PortableTys<'_>) -> Result<Option<ItemKey>, blaze_build::BuildError> { Ok(None) }
        fn abi(&self, _: ItemKey) -> Option<Abi> { Some(Abi::Rust) }
        fn lang_fn(&self, _: &str) -> Option<ItemKey> { None }
        fn intrinsic(&self, item: ItemKey) -> Option<Intrinsic> {
            let func = match item {
                k if k == FMA_KEY => FloatFn::Fma,
                k if k == SQRT_KEY => FloatFn::Sqrt,
                k if k == FABS_KEY => FloatFn::Fabs,
                _ => return None,
            };
            Some(Intrinsic::Target(IntrTarget::Float(FloatCall { func, shape: Shape::F64 })))
        }
    }

    let mut ground = PortableTys::new_in(heap());
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: heap() };
    let built = blaze_body_build::build(
        v_diff,
        ck_loss.types.view(),
        loss_key,
        &mut subst,
        &NativeMathWorld,
        heap(),
    )
    .expect("real lowered + inlined + FMA-fused diff MIR compiles to verified LIR");

    // 6. Prove the compiled LIR uses a real native hardware `Op::Fma` instruction and ZERO calls!
    let view = built.func.view();
    let has_fma = (0..view.ops.len()).any(|i| view.ops[i] == blaze_lir::Op::Fma as u8);
    let has_call = (0..view.ops.len()).any(|i| view.ops[i] == blaze_lir::Op::Call as u8);
    assert!(has_fma, "compiled LIR must contain native hardware Op::Fma instruction");
    assert!(!has_call, "compiled LIR must have zero Op::Call instructions (pure inlined + native FMA)");

    // 7. Execute the compiled LIR directly via `blaze_lir::eval::run_in` on BOTH branches!
    let eval_lir = |v: blaze_lir::View<'_>, x: f64, y: f64| -> f64 {
        let args = [Val::Int(u128::from(x.to_bits())), Val::Int(u128::from(y.to_bits()))];
        match run_in(v, &args, 10_000, heap()).expect("LIR eval succeeds") {
            Outcome::Returned(vals) => {
                let Val::Int(bits) = vals[0] else { panic!("expected float bits") };
                f64::from_bits(bits as u64)
            }
            other => panic!("expected Returned([f64]), got {other:?}"),
        }
    };
    // Branch 1: (3, 2) => spring(3,2) = 24 > 10 => ∂loss/∂x = 2*3*2 + 2 = 14.0
    assert!(close(eval_lir(view, 3.0, 2.0), 14.0, 1e-12));
    // Branch 2: (1, 1) => spring(1,1) = 3 <= 10 => ∂loss/∂x = 1.0
    assert!(close(eval_lir(view, 1.0, 1.0), 1.0, 1e-12));

    // 8. Also verify `sqrt` and `fabs` intrinsics lower to native `Op::Sqrt` and `Op::Fabs` LIR!
    let mut from = PortableTys::new_in(heap());
    let f64_t = from.intern(PNode::Float(FloatTy::F64)).unwrap();
    let bool_t = from.intern(PNode::Bool).unwrap();
    let mut gs = ExprGraph::new_in(heap());
    let (vx, ex) = gs.var("x");
    let (vy, ey) = gs.var("y");
    let ax = gs.abs(ex);
    let y2 = gs.mul(ey, ey);
    let sum_sa = gs.add(ax, y2);
    let root_sa = gs.sqrt(sum_sa);
    let sa_mir = splice_body(&gs, root_sa, &[vx, vy], ScalarTys { f64_ty: f64_t, bool_ty: bool_t }, &IntrMath, heap()).unwrap();
    let v_sa = verify(&sa_mir).unwrap();
    let mut ground2 = PortableTys::new_in(heap());
    let mut subst2 = Subst { args: &[], ground: &mut ground2, scratch: heap() };
    let built_sa = blaze_body_build::build(v_sa, from.view(), ItemKey(3), &mut subst2, &NativeMathWorld, heap())
        .expect("sqrt + fabs intrinsics compile cleanly to verified LIR");
    let vsa = built_sa.func.view();
    assert!((0..vsa.ops.len()).any(|i| vsa.ops[i] == blaze_lir::Op::Sqrt as u8));
    assert!((0..vsa.ops.len()).any(|i| vsa.ops[i] == blaze_lir::Op::Fabs as u8));
    // sqrt(|-7.0| + 3.0^2) = sqrt(7 + 9) = 4.0
    assert!(close(eval_lir(vsa, -7.0, 3.0), 4.0, 1e-12));
}

#[test]
fn struct_vec3_ad_aegraph_and_native_aarch64_x86_64_machine_code_emission() {
    use blaze_body::{AggregateKind, Value};
    use blaze_build::intrinsics::target::{
        Target as IntrTarget,
        float::{FloatCall, FloatFn, Shape},
    };
    use blaze_build::layout::{LayoutFacts, Missing, ReprAttrs, ValidRange};
    use blaze_build::{Abi, Adts, Intrinsic, OverflowChecks, Panic, Subst};
    use blaze_emit::{Code, Isa, Os, emit};
    use blaze_lir::eval::{Outcome, Val, run_in};
    use blaze_lir::simplify::aegraph_optimize;
    use blaze_typeck::{Adt, FloatTy, PNode, PortableTys};

    // 1. Build a MIR body operating on a plain Rust struct `Vec3 { x: f64, y: f64, z: f64 }`:
    //    fn energy(v: Vec3) -> f64 {
    //        let w = Vec3 { x: v.x + 1.0, y: v.y, z: v.z };
    //        0.5 * (w.x * w.x + w.y * w.y + w.z * w.z) + w.x * w.y
    //    }
    //    At v = (2.0, 3.0, 4.0) => w = (3.0, 3.0, 4.0):
    //      energy = 0.5 * (9 + 9 + 16) + 9 = 17 + 9 = 26.0
    //      ∂/∂x   = w.x + w.y = 6.0
    //      ∂/∂y   = w.y + w.x = 6.0
    //      ∂/∂z   = w.z       = 4.0
    let mut from = PortableTys::new_in(heap());
    let f64_ty = from.intern(PNode::Float(FloatTy::F64)).unwrap();
    let vec3_ty = PTy(10); // opaque struct handle in MIR lifting test
    let arr4_ty = PTy(11);

    let mir = {
        let mut b = BodyBuilder::new_in(heap());
        let ret = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Return, mutbl: Mutbl::Mut });
        let lv = b.new_local(LocalDecl { ty: vec3_ty, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
        let lw = b.new_local(LocalDecl { ty: vec3_ty, kind: LocalKind::Temp(MirExprId(0), Role::Value), mutbl: Mutbl::Not });
        let lx1 = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Temp(MirExprId(1), Role::Value), mutbl: Mutbl::Not });
        let lwx2 = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Temp(MirExprId(2), Role::Value), mutbl: Mutbl::Not });
        let lwy2 = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Temp(MirExprId(3), Role::Value), mutbl: Mutbl::Not });
        let lwz2 = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Temp(MirExprId(4), Role::Value), mutbl: Mutbl::Not });
        let ls1 = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Temp(MirExprId(5), Role::Value), mutbl: Mutbl::Not });
        let ls2 = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Temp(MirExprId(6), Role::Value), mutbl: Mutbl::Not });
        let lhalf = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Temp(MirExprId(7), Role::Value), mutbl: Mutbl::Not });
        let lxy = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Temp(MirExprId(8), Role::Value), mutbl: Mutbl::Not });
        let bb0 = b.new_block(BlockKind::Normal);
        let span = Span { lo: 0, hi: 0 };
        b.start(bb0);

        let pv = b.local_place(lv);
        let pv_x = b.project(pv, Proj::Field { index: 0, ty: f64_ty });
        let pv_y = b.project(pv, Proj::Field { index: 1, ty: f64_ty });
        let pv_z = b.project(pv, Proj::Field { index: 2, ty: f64_ty });
        let c1 = b.new_const(ConstDecl { kind: ConstKind::Scalar { bits: u128::from(1.0_f64.to_bits()) }, ty: f64_ty });
        let px1 = b.local_place(lx1);
        b.assign(px1, Value::Binary { op: BinOp::Add, lhs: Operand::Copy(pv_x), rhs: Operand::Const(c1) }, span);

        // Construct `w = Vec3 { x: v.x + 1.0, y: v.y, z: v.z }` via `AggregateKind::Adt(0)`.
        let pw = b.local_place(lw);
        let w_ops = b.operands(&[Operand::Copy(px1), Operand::Copy(pv_y), Operand::Copy(pv_z)]);
        b.assign(pw, Value::Aggregate { kind: AggregateKind::Adt(0), ty: vec3_ty, operands: w_ops }, span);

        let pw_x = b.project(pw, Proj::Field { index: 0, ty: f64_ty });
        let pw_y = b.project(pw, Proj::Field { index: 1, ty: f64_ty });
        let pw_z = b.project(pw, Proj::Field { index: 2, ty: f64_ty });
        let pwx2 = b.local_place(lwx2);
        let pwy2 = b.local_place(lwy2);
        let pwz2 = b.local_place(lwz2);
        let ps1 = b.local_place(ls1);
        let ps2 = b.local_place(ls2);
        let phalf = b.local_place(lhalf);
        let pxy = b.local_place(lxy);
        let pret = b.local_place(ret);

        b.assign(pwx2, Value::Binary { op: BinOp::Mul, lhs: Operand::Copy(pw_x), rhs: Operand::Copy(pw_x) }, span);
        b.assign(pwy2, Value::Binary { op: BinOp::Mul, lhs: Operand::Copy(pw_y), rhs: Operand::Copy(pw_y) }, span);
        b.assign(pwz2, Value::Binary { op: BinOp::Mul, lhs: Operand::Copy(pw_z), rhs: Operand::Copy(pw_z) }, span);
        b.assign(ps1, Value::Binary { op: BinOp::Add, lhs: Operand::Copy(pwx2), rhs: Operand::Copy(pwy2) }, span);
        b.assign(ps2, Value::Binary { op: BinOp::Add, lhs: Operand::Copy(ps1), rhs: Operand::Copy(pwz2) }, span);
        let chalf = b.new_const(ConstDecl { kind: ConstKind::Scalar { bits: u128::from(0.5_f64.to_bits()) }, ty: f64_ty });
        b.assign(phalf, Value::Binary { op: BinOp::Mul, lhs: Operand::Const(chalf), rhs: Operand::Copy(ps2) }, span);
        b.assign(pxy, Value::Binary { op: BinOp::Mul, lhs: Operand::Copy(pw_x), rhs: Operand::Copy(pw_y) }, span);
        b.assign(pret, Value::Binary { op: BinOp::Add, lhs: Operand::Copy(phalf), rhs: Operand::Copy(pxy) }, span);
        b.terminate(StmtKind::Return, span);
        b.finish().unwrap()
    };
    let v_mir = verify(&mir).unwrap();

    // 2. Differentiate `Vec3` struct body via `BodyTransform::ValueAndGrad`!
    let vg = run_body_transform(
        v_mir,
        BodyTransform::ValueAndGrad { array_ty: arr4_ty },
        None,
        &NoMathCalls,
        &NoMathCalls,
        heap(),
    )
    .expect("struct Vec3 ValueAndGrad succeeds");
    let v_vg = verify(&vg).unwrap();
    let out = eval_pure_multi_body(v_vg, &[2.0, 3.0, 4.0], &NoMathCalls, heap()).unwrap();
    assert_eq!(out.len(), 4);
    assert!(close(out[0], 26.0, 1e-12), "energy={}", out[0]);
    assert!(close(out[1], 6.0, 1e-12), "d/dx={}", out[1]);
    assert!(close(out[2], 6.0, 1e-12), "d/dy={}", out[2]);
    assert!(close(out[3], 4.0, 1e-12), "d/dz={}", out[3]);

    // 3. Compile ∂energy/∂x (which is `(x + 1) + y`) through:
    //    `blaze_body_build` -> `blaze_lir::simplify::aegraph_optimize` -> `blaze_lir::eval`
    //    -> `blaze_emit::emit` for BOTH AArch64 AND x86-64 native machine code!
    const FMA_KEY: ItemKey = ItemKey(950);
    struct FmaMath;
    impl MathResolver for FmaMath {
        fn classify(&self, _: Callee) -> Option<MathOp> { None }
    }
    impl MathEmitter for FmaMath {
        fn callee_for(&self, op: MathOp) -> Option<Callee> {
            (op == MathOp::Fma).then_some(Callee::Intrinsic { key: FMA_KEY, args: List::EMPTY })
        }
    }
    struct EmitWorld;
    impl LayoutFacts for EmitWorld {
        fn adt(&self, _: ItemKey) -> Option<Adt<'_>> { None }
        fn repr(&self, _: ItemKey) -> Result<ReprAttrs, Missing> { Ok(ReprAttrs::RUST) }
        fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> { Ok(None) }
        fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> { Ok(ValidRange::default()) }
    }
    impl Adts for EmitWorld {
        fn adt(&self, _: ItemKey) -> Option<Adt<'_>> { None }
        fn facts(&self) -> &dyn LayoutFacts { self }
        fn panic(&self) -> Panic { Panic::Abort }
        fn overflow_checks(&self) -> OverflowChecks { OverflowChecks::Off }
        fn ub_checks(&self) -> blaze_build::UbChecks { blaze_build::UbChecks::Off }
        fn drop_impl(&self, _: PTy, _: &mut PortableTys<'_>) -> Result<Option<ItemKey>, blaze_build::BuildError> { Ok(None) }
        fn abi(&self, _: ItemKey) -> Option<Abi> { Some(Abi::Rust) }
        fn lang_fn(&self, _: &str) -> Option<ItemKey> { None }
        fn intrinsic(&self, item: ItemKey) -> Option<Intrinsic> {
            (item == FMA_KEY).then_some(Intrinsic::Target(IntrTarget::Float(FloatCall {
                func: FloatFn::Fma,
                shape: Shape::F64,
            })))
        }
    }

    let dx_mir = run_body_transform(
        v_mir,
        BodyTransform::DiffWith {
            arg: 0,
            opts: SimplifyOpts { cost_model: CostModel::CpuFma, ..SimplifyOpts::default() },
        },
        None,
        &FmaMath,
        &FmaMath,
        heap(),
    )
    .expect("struct Vec3 DiffWith CpuFma succeeds");
    let v_dx = verify(&dx_mir).unwrap();

    let mut ground = PortableTys::new_in(heap());
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: heap() };
    let built = blaze_body_build::build(v_dx, from.view(), ItemKey(40), &mut subst, &EmitWorld, heap())
        .expect("differentiated Vec3 MIR compiles to verified LIR");
    let opt = aegraph_optimize(built.func.view(), heap()).expect("LIR aegraph optimizes cleanly");
    let verified = blaze_lir::verify(opt.func.view()).expect("optimized LIR verifies");

    // Verify LIR evaluation: ∂energy/∂x(2, 3, 4) = (2 + 1) + 3 = 6.0
    let args = [
        Val::Int(u128::from(2.0_f64.to_bits())),
        Val::Int(u128::from(3.0_f64.to_bits())),
        Val::Int(u128::from(4.0_f64.to_bits())),
    ];
    match run_in(opt.func.view(), &args, 10_000, heap()).expect("eval succeeds") {
        Outcome::Returned(vals) => {
            let Val::Int(bits) = vals[0] else { panic!("expected f64 bits") };
            assert!(close(f64::from_bits(bits as u64), 6.0, 1e-12));
        }
        other => panic!("expected Returned([6.0]), got {other:?}"),
    }

    // 4. Emit REAL native AArch64 machine code for BOTH macOS (Apple Silicon) AND Linux ARM64!
    let mut arm64_mac = Code::new_in(heap());
    emit(verified, &[], Isa { lse: true, os: Os::MacOs }, &mut arm64_mac)
        .expect("AArch64 macOS machine code emission succeeds");
    assert!(!arm64_mac.bytes.is_empty(), "AArch64 macOS machine code must be non-empty");
    assert_eq!(arm64_mac.bytes.len() % 4, 0, "AArch64 instructions are 4-byte aligned");

    let mut arm64_linux = Code::new_in(heap());
    emit(verified, &[], Isa { lse: true, os: Os::Linux }, &mut arm64_linux)
        .expect("AArch64 Linux machine code emission succeeds");
    assert!(!arm64_linux.bytes.is_empty(), "AArch64 Linux machine code must be non-empty");
    assert_eq!(arm64_linux.bytes.len() % 4, 0, "AArch64 instructions are 4-byte aligned");

    // 5. Prove `blaze-body-build` grounds types interned into a body's own overlay table!
    let empty_table = blaze_typeck::PTys::flat(&[], &[]);
    let mut ob = BodyBuilder::new_with_types_in(empty_table, heap());
    let ov_f64 = ob.intern_ty(PNode::Float(FloatTy::F64)).expect("intern f64 in overlay");
    let o_ret = ob.new_local(LocalDecl { ty: ov_f64, kind: LocalKind::Return, mutbl: Mutbl::Mut });
    let o_x = ob.new_local(LocalDecl { ty: ov_f64, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
    let o_bb = ob.new_block(BlockKind::Normal);
    ob.start(o_bb);
    let po_x = ob.local_place(o_x);
    let po_ret = ob.local_place(o_ret);
    ob.assign(po_ret, Value::Binary { op: BinOp::Add, lhs: Operand::Copy(po_x), rhs: Operand::Copy(po_x) }, Span { lo: 0, hi: 0 });
    ob.terminate(StmtKind::Return, Span { lo: 0, hi: 0 });
    let ob_body = ob.finish().expect("overlay body builds");
    let v_ob = verify(&ob_body).expect("overlay body verifies");

    let mut ground_ov = PortableTys::new_in(heap());
    let mut subst_ov = Subst { args: &[], ground: &mut ground_ov, scratch: heap() };
    let built_ov = blaze_body_build::build(v_ob, empty_table, ItemKey(41), &mut subst_ov, &EmitWorld, heap())
        .expect("blaze-body-build grounds overlay-interned f64 cleanly");
    let ov_eval = run_in(built_ov.func.view(), &[Val::Int(u128::from(4.5_f64.to_bits()))], 10_000, heap()).unwrap();
    match ov_eval {
        Outcome::Returned(vals) => {
            let Val::Int(bits) = vals[0] else { panic!("expected f64 bits") };
            assert!(close(f64::from_bits(bits as u64), 9.0, 1e-12));
        }
        other => panic!("expected Returned([9.0]), got {other:?}"),
    }
}
