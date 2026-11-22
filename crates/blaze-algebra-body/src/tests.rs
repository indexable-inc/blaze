use blaze_algebra::{CostModel, ExprGraph, Interval, SimplifyOpts, eval_dual, hessian};
use blaze_body::{
    BinOp, Block, BlockKind, Body, BodyBuilder, Callee, ConstDecl, ConstKind, LocalDecl, LocalKind,
    Operand, Role, Span, StmtKind,
    body::{ExprId as MirExprId, ItemKey, List, Mutbl, PTy},
    verify,
};
use blaze_own::Alloc;

use crate::{
    BodyAlgebraError, BodyPlant, MathEmitter, MathOp, MathResolver, NoMathCalls, diff_body,
    diff_body_with_plant, eval_pure_body, eval_pure_multi_body, grad_bodies, hvp_body, lift_body,
    simplify_body, simplify_body_with, splice_body, splice_multi_body, value_and_grad_body,
};

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

    let orig_body = splice_body(&g, root, &[vx, vy], PTy(0), &TestMath, heap()).expect("splice transcendental");
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

    let big_mir = splice_body(&g, diff_expr, &[vx, vy], PTy(0), &NoMathCalls, heap()).unwrap();
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

        let mir = splice_body(&g, root, &[vx, vy], PTy(0), &TestMath, heap())
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

    let orig = splice_body(&g, root, &[vx, vy], PTy(0), &TestMath, heap()).unwrap();
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
        splice_multi_body(&g, &[root, xy, sxy], &[vx, vy], PTy(0), PTy(1), &TestMath, heap())
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

    let mir_f = splice_body(&g, root, &[vx, vy], PTy(0), &TestMath, heap()).unwrap();
    let v_f = verify(&mir_f).expect("Switch diamond MIR for f must verify");
    // Two Op::Select nodes => at least 1 + 3 + 3 = 7 basic blocks!
    assert!(v_f.blocks() >= 7, "expected >= 7 blocks for 2 Select diamonds, got {}", v_f.blocks());

    let mir_dx = splice_body(&g, df_dx, &[vx, vy], PTy(0), &TestMath, heap()).unwrap();
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

    let orig = splice_body(&g, root, &[vx, vy], PTy(0), &TestMath, heap()).unwrap();
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
    let mac_mir = splice_body(&g2, mac, &[va, vb, vc], PTy(0), &TestMath, heap()).unwrap();
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
