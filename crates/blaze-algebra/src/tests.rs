use blaze_own::Alloc;

use crate::{
    AlgebraPlant, ConstExpr, ConstGraph, CostModel, DERIV_RULES, ExprGraph, ExprId, Interval,
    NewtonOpts, Op, PatNode, Poly, PolyPlant, Rat, RewriteMode, RewriteRule,
    STANDARD_REWRITE_RULES, SimplifyOpts, Solution, SymScope, active_vars, diff,
    diff::diff_with_plant, emit_rust_fn, emit_rust_multi_fn, eval_dual, eval_interval, eval_newton,
    export_lean_theorems, expr_cost, expr_cost_with, expr_to_poly, format_expr, grad, grad_reverse,
    grad_reverse_with_plant, hessian, hvp, jacobian, jvp, newton_step, poly_to_expr,
    prune_branches, simplify, simplify_poly_expr, simplify_with, simplify_with_rules, solve_for,
    solve_linear_system, standard_rewrite_rules, taylor,
};

fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn close(a: f64, b: f64, tol: f64) -> bool {
    let scale = 1.0_f64.max(blaze_math::abs(a)).max(blaze_math::abs(b));
    blaze_math::abs(a - b) <= tol * scale
}

/// True compile-time symbolic differentiation + compile-time evaluation in `const`!
const COMPTIME_DERIV: (ConstGraph<64>, ConstExpr) = {
    let mut g = ConstGraph::new();
    let x = g.var(0);
    // f(x) = x^3 + 2*x - 5  =>  f'(x) = 3*x^2 + 2
    let x3 = g.pow_int(x, 3);
    let c2 = g.lit(2.0);
    let two_x = g.mul(c2, x);
    let sum = g.add(x3, two_x);
    let c5 = g.lit(5.0);
    let f = g.sub(sum, c5);
    let df = g.diff(f, 0);
    (g, df)
};
const COMPTIME_VAL_AT_3: f64 = COMPTIME_DERIV.0.eval_const(COMPTIME_DERIV.1, &[3.0]);

#[test]
fn comptime_const_symbolic_diff_evaluates_at_compile_time() {
    // f'(3) = 3 * 9 + 2 = 29.0 — computed by rustc/blaze at compile time!
    assert_eq!(COMPTIME_VAL_AT_3, 29.0);
    // Also verify transcendental comptime differentiation (d/dx sin(x^2) = 2x cos(x^2)):
    const SIN_DERIV: (ConstGraph<64>, ConstExpr) = {
        let mut g = ConstGraph::new();
        let x = g.var(0);
        let x2 = g.mul(x, x);
        let f = g.sin(x2);
        let df = g.diff(f, 0);
        (g, df)
    };
    let x = 1.3;
    let expected = 2.0 * x * blaze_math::cos(x * x);
    let actual = SIN_DERIV.0.eval(SIN_DERIV.1, &[x]);
    assert!(close(actual, expected, 1e-12), "actual={actual}, expected={expected}");
}

#[test]
fn symbolic_diff_matches_dual_oracle_and_catches_planted_bugs() {
    let s = SymScope::new_in(heap());
    let (vx, x) = s.var("x");
    let (vy, y) = s.var("y");
    // f(x, y) = sin(x * y) / (1 + x^2) + exp(x - y) + ln(x + 2) + sqrt(y + 3) + tanh(x)
    let f = (x * y).sin() / (s.lit(1.0) + x * x)
        + (x - y).exp()
        + (x + s.lit(2.0)).ln()
        + (y + s.lit(3.0)).sqrt()
        + x.tanh();
    let f_id = f.id();
    let mut g = s.into_graph();

    let df_dx = diff(&mut g, f_id, vx).expect("diff wrt x");
    let df_dx_simp = simplify(&mut g, df_dx).expect("simplify df_dx");
    let df_dy = diff(&mut g, f_id, vy).expect("diff wrt y");
    let df_dy_simp = simplify(&mut g, df_dy).expect("simplify df_dy");

    for &(xv, yv) in &[(0.5, 1.2), (1.7, -0.4), (-0.8, 2.1), (2.3, 0.9)] {
        let env = [xv, yv];
        let oracle_x = eval_dual(&g, f_id, &env, vx).expect("dual x");
        let sym_x = g.eval(df_dx, &env).expect("eval df_dx");
        let simp_x = g.eval(df_dx_simp, &env).expect("eval simplified df_dx");
        assert!(close(sym_x, oracle_x.deriv, 1e-11), "sym_x={sym_x} vs dual={}", oracle_x.deriv);
        assert!(close(simp_x, oracle_x.deriv, 1e-11), "simp_x={simp_x} vs dual={}", oracle_x.deriv);

        let oracle_y = eval_dual(&g, f_id, &env, vy).expect("dual y");
        let sym_y = g.eval(df_dy, &env).expect("eval df_dy");
        let simp_y = g.eval(df_dy_simp, &env).expect("eval simplified df_dy");
        assert!(close(sym_y, oracle_y.deriv, 1e-11), "sym_y={sym_y} vs dual={}", oracle_y.deriv);
        assert!(close(simp_y, oracle_y.deriv, 1e-11), "simp_y={simp_y} vs dual={}", oracle_y.deriv);
    }

    // Planted-bug oracle controls (rule 43 `dst-not-review`): both planted bugs MUST be caught!
    for plant in [AlgebraPlant::WrongSinDeriv, AlgebraPlant::WrongQuotientSign] {
        let buggy = diff_with_plant(&mut g, f_id, vx, plant).expect("buggy diff");
        let env = [0.7, 1.1];
        let oracle = eval_dual(&g, f_id, &env, vx).expect("dual");
        let got = g.eval(buggy, &env).expect("eval buggy");
        assert!(
            !close(got, oracle.deriv, 1e-6),
            "oracle failed to catch planted bug {plant:?}: got={got}, oracle={}",
            oracle.deriv
        );
    }
}

#[test]
fn blaze_sim_property_random_exprs_match_dual_oracle_and_egraph_preserves_value() {
    let config = blaze_sim::Config { first: 2026_1122, ..blaze_sim::Config::cases(64) };
    blaze_sim::check(config, |t| {
        let mut g = ExprGraph::new_in(heap());
        let (vx, ex) = g.var("x");
        let (vy, ey) = g.var("y");
        let mut pool: [ExprId; 12] = [ex; 12];
        pool[0] = ex;
        pool[1] = ey;
        pool[2] = g.lit(2.0);
        pool[3] = g.lit(3.0);
        let mut count = 4usize;

        let depth = t.range(2..7, "steps");
        for _ in 0..depth {
            let kind = t.range(0..11, "op");
            let ia = t.range(0..count as u64, "a") as usize;
            let ib = t.range(0..count as u64, "b") as usize;
            let a = pool[ia];
            let b = pool[ib];
            let next = match kind {
                0 => g.add(a, b),
                1 => g.sub(a, b),
                2 => g.mul(a, b),
                3 => {
                    // Safe positive denominator: 1 + b^2
                    let one = g.lit(1.0);
                    let b2 = g.mul(b, b);
                    let den = g.add(one, b2);
                    g.div(a, den)
                }
                4 => g.sin(a),
                5 => g.cos(a),
                6 => g.tanh(a),
                7 => {
                    // Safe positive argument for ln/sqrt: 1 + a^2
                    let one = g.lit(1.0);
                    let a2 = g.mul(a, a);
                    let pos = g.add(one, a2);
                    g.ln(pos)
                }
                8 => {
                    let one = g.lit(1.0);
                    let a2 = g.mul(a, a);
                    let pos = g.add(one, a2);
                    g.sqrt(pos)
                }
                9 => {
                    let c = g.lit(0.5);
                    let ta = g.tanh(a);
                    let scaled = g.mul(c, ta);
                    g.exp(scaled)
                }
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
        let env = [0.63, -0.41];

        let df = diff(&mut g, root, vx).map_err(|_| blaze_sim::Broke::Oracle("diff failed".into()))?;
        let simp = simplify(&mut g, df).map_err(|_| blaze_sim::Broke::Oracle("simplify failed".into()))?;
        let rev = grad_reverse(&mut g, root, &[vx, vy])
            .map_err(|_| blaze_sim::Broke::Oracle("grad_reverse failed".into()))?;

        let dual_x = eval_dual(&g, root, &env, vx).map_err(|_| blaze_sim::Broke::Oracle("dual x failed".into()))?;
        let dual_y = eval_dual(&g, root, &env, vy).map_err(|_| blaze_sim::Broke::Oracle("dual y failed".into()))?;
        let sym_v = g.eval(df, &env).map_err(|_| blaze_sim::Broke::Oracle("eval df failed".into()))?;
        let simp_v = g.eval(simp, &env).map_err(|_| blaze_sim::Broke::Oracle("eval simp failed".into()))?;
        let rev_x = g.eval(rev[0], &env).map_err(|_| blaze_sim::Broke::Oracle("eval rev_x failed".into()))?;
        let rev_y = g.eval(rev[1], &env).map_err(|_| blaze_sim::Broke::Oracle("eval rev_y failed".into()))?;

        if dual_x.deriv.is_finite() {
            if !close(sym_v, dual_x.deriv, 1e-8) {
                return Err(blaze_sim::Broke::Oracle("symbolic diff != dual tangent".into()));
            }
            if !close(simp_v, dual_x.deriv, 1e-8) {
                return Err(blaze_sim::Broke::Oracle("e-graph simplified diff != dual tangent".into()));
            }
            if !close(rev_x, dual_x.deriv, 1e-8) {
                return Err(blaze_sim::Broke::Oracle("reverse-mode symbolic dx != dual tangent".into()));
            }
            let c_before = expr_cost(&g, df).unwrap_or(u32::MAX);
            let c_after = expr_cost(&g, simp).unwrap_or(u32::MAX);
            if c_after > c_before {
                return Err(blaze_sim::Broke::Oracle("e-graph increased cost".into()));
            }
        }
        if dual_y.deriv.is_finite() && !close(rev_y, dual_y.deriv, 1e-8) {
            return Err(blaze_sim::Broke::Oracle("reverse-mode symbolic dy != dual tangent".into()));
        }
        Ok(())
    });
}

#[test]
fn egraph_simplifies_trig_log_and_factoring_identities() {
    let s = SymScope::new_in(heap());
    let (_, x) = s.var("x");
    let (_, a) = s.var("a");
    let (_, b) = s.var("b");
    let (_, c) = s.var("c");

    // 1. sin(x)^2 + cos(x)^2 -> 1
    let trig = x.sin().powf(2.0) + x.cos().powf(2.0);
    // 2. ln(exp(x)) -> x
    let logexp = x.exp().ln();
    // 3. a * b + a * c -> a * (b + c)
    let factored = a * b + a * c;
    let (trig_id, logexp_id, fac_id, x_id) = (trig.id(), logexp.id(), factored.id(), x.id());
    let mut g = s.into_graph();

    let trig_s = simplify(&mut g, trig_id).expect("simplify trig");
    assert_eq!(g.node(trig_s).unwrap().as_const(), Some(1.0));

    let logexp_s = simplify(&mut g, logexp_id).expect("simplify ln(exp)");
    assert_eq!(logexp_s, x_id);

    let fac_before = expr_cost(&g, fac_id).unwrap();
    let fac_s = simplify(&mut g, fac_id).expect("simplify factor");
    let fac_after = expr_cost(&g, fac_s).unwrap();
    assert!(fac_after < fac_before, "expected factoring to lower cost: {fac_before} -> {fac_after}");
    let env = [0.0, 2.5, 3.0, 4.0];
    assert!(close(g.eval(fac_id, &env).unwrap(), g.eval(fac_s, &env).unwrap(), 1e-12));
}

#[test]
fn symbolic_solver_solves_linear_quadratic_and_transcendental_equations() {
    let s = SymScope::new_in(heap());
    let (vx, x) = s.var("x");
    let (_, a) = s.var("a");
    let (_, b) = s.var("b");
    let (_, y) = s.var("y");

    // 1. Linear: a * x + b = y  =>  x = (y - b) / a
    let lin_lhs = a * x + b;
    // 2. Quadratic: x^2 - 5x + 6 = 0  =>  x = 3, x = 2
    let quad_lhs = x.powf(2.0) - s.lit(5.0) * x + s.lit(6.0);
    let zero = s.lit(0.0);
    // 3. Transcendental: exp(2*x + 1) = y  =>  x = (ln(y) - 1) / 2
    let exp_lhs = (s.lit(2.0) * x + s.lit(1.0)).exp();

    let (lin_l, quad_l, z_id, exp_l, y_id) = (lin_lhs.id(), quad_lhs.id(), zero.id(), exp_lhs.id(), y.id());
    let mut g = s.into_graph();

    let Solution::Exact(lin_sol) = solve_for(&mut g, lin_l, y_id, vx).expect("linear") else {
        panic!("expected exact linear solution");
    };
    let env = [0.0, 4.0, 3.0, 19.0]; // a=4, b=3, y=19 -> x=4
    assert!(close(g.eval(lin_sol, &env).unwrap(), 4.0, 1e-12));

    let Solution::Two(r1, r2) = solve_for(&mut g, quad_l, z_id, vx).expect("quadratic") else {
        panic!("expected two quadratic roots");
    };
    let v1 = g.eval(r1, &env).unwrap();
    let v2 = g.eval(r2, &env).unwrap();
    let (lo, hi) = if v1 < v2 { (v1, v2) } else { (v2, v1) };
    assert!(close(lo, 2.0, 1e-12) && close(hi, 3.0, 1e-12), "roots=({v1}, {v2})");

    let Solution::Exact(exp_sol) = solve_for(&mut g, exp_l, y_id, vx).expect("transcendental") else {
        panic!("expected exact transcendental solution");
    };
    let x_sol = g.eval(exp_sol, &[0.0, 0.0, 0.0, 20.0]).unwrap();
    let check_lhs = g.eval(exp_l, &[x_sol, 0.0, 0.0, 20.0]).unwrap();
    assert!(close(check_lhs, 20.0, 1e-11), "exp(2*x+1)={check_lhs} != 20.0");
}

#[test]
fn grad_hessian_taylor_and_rust_ssa_emitter() {
    let s = SymScope::new_in(heap());
    let (vx, x) = s.var("x");
    let (vy, y) = s.var("y");
    let f = x * x * y + y.sin();
    let f_id = f.id();
    let sin_x = x.sin().id();
    let mut g = s.into_graph();

    let gr = grad(&mut g, f_id, &[vx, vy]).unwrap();
    let h = hessian(&mut g, f_id, &[vx, vy]).unwrap();
    assert_eq!(gr.len(), 2);
    assert_eq!(h.len(), 4);
    // H_00 = d²f/dx² = 2y; H_01 = H_10 = 2x
    let env = [3.0, 5.0];
    assert!(close(g.eval(h[0], &env).unwrap(), 10.0, 1e-12));
    assert!(close(g.eval(h[1], &env).unwrap(), 6.0, 1e-12));
    assert!(close(g.eval(h[2], &env).unwrap(), 6.0, 1e-12));

    // Taylor of sin(x) around 0 up to degree 5: x - x^3/6 + x^5/120
    let zero = g.lit(0.0);
    let t5 = taylor(&mut g, sin_x, vx, zero, 5).unwrap();
    let x_small = 0.25;
    assert!(close(g.eval(t5, &[x_small, 0.0]).unwrap(), blaze_math::sin(x_small), 1e-7));

    let pretty = format_expr(&g, gr[0]).unwrap();
    assert!(!pretty.as_str().is_empty());
    let rust_code = emit_rust_fn(&g, "d_f_dx", gr[0]).unwrap();
    assert!(rust_code.as_str().contains("pub fn d_f_dx(x: f64, y: f64) -> f64"));
    assert!(rust_code.as_str().contains("let t"));
}

#[test]
fn reverse_mode_symbolic_grad_jacobian_and_multi_output_ssa_with_planted_bug_control() {
    let s = SymScope::new_in(heap());
    let (vx, x) = s.var("x");
    let (vy, y) = s.var("y");
    // Diamond DAG: shared subexpression `u = sin(x * y)` feeds multiple downstream paths.
    let u = (x * y).sin();
    let f = u * u + u.exp() + x / (s.lit(1.0) + y * y);
    let g_fn = x * y + y.cos();
    let (f_id, g_id) = (f.id(), g_fn.id());
    let mut graph = s.into_graph();

    let rev = grad_reverse(&mut graph, f_id, &[vx, vy]).expect("grad_reverse");
    let jac = jacobian(&mut graph, &[f_id, g_id], &[vx, vy]).expect("jacobian");
    assert_eq!(jac.len(), 4);

    let env = [0.8, 1.3];
    let dual_fx = eval_dual(&graph, f_id, &env, vx).unwrap();
    let dual_fy = eval_dual(&graph, f_id, &env, vy).unwrap();
    let dual_gx = eval_dual(&graph, g_id, &env, vx).unwrap();
    let dual_gy = eval_dual(&graph, g_id, &env, vy).unwrap();

    assert!(close(graph.eval(rev[0], &env).unwrap(), dual_fx.deriv, 1e-11));
    assert!(close(graph.eval(rev[1], &env).unwrap(), dual_fy.deriv, 1e-11));
    assert!(close(graph.eval(jac[0], &env).unwrap(), dual_fx.deriv, 1e-11));
    assert!(close(graph.eval(jac[1], &env).unwrap(), dual_fy.deriv, 1e-11));
    assert!(close(graph.eval(jac[2], &env).unwrap(), dual_gx.deriv, 1e-11));
    assert!(close(graph.eval(jac[3], &env).unwrap(), dual_gy.deriv, 1e-11));

    // Planted-bug control: overwriting instead of accumulating fan-out adjoints MUST fail the oracle!
    let buggy = grad_reverse_with_plant(
        &mut graph,
        f_id,
        &[vx, vy],
        AlgebraPlant::WrongReverseAdjointAccum,
    )
    .expect("buggy reverse");
    let buggy_dx = graph.eval(buggy[0], &env).unwrap();
    assert!(
        !close(buggy_dx, dual_fx.deriv, 1e-6),
        "oracle failed to catch WrongReverseAdjointAccum: got={buggy_dx}, oracle={}",
        dual_fx.deriv
    );

    // Multi-output CSE SSA emission shares forward + reverse subexpressions in one function!
    let multi = emit_rust_multi_fn(&graph, "f_and_grad", &[f_id, rev[0], rev[1]]).unwrap();
    assert!(multi.as_str().contains("pub fn f_and_grad(x: f64, y: f64) -> [f64; 3]"));
    assert!(multi.as_str().contains("[t"));
}

#[test]
fn exact_poly_cancellation_gcd_rational_reduction_and_solver() {
    // 1. Exact rational arithmetic (1/3 + 1/6 == 1/2 with zero float drift).
    let one_third = Rat::new(1, 3).unwrap();
    let one_sixth = Rat::new(1, 6).unwrap();
    assert_eq!(one_third.checked_add(one_sixth).unwrap(), Rat::new(1, 2).unwrap());

    // 2. Multivariate binomial identity: (x + y)^3 - (x^3 + 3*x^2*y + 3*x*y^2 + y^3) == 0 exactly!
    let s = SymScope::new_in(heap());
    let (vx, x) = s.var("x");
    let (_, y) = s.var("y");
    let expanded = x.powf(3.0)
        + s.lit(3.0) * x.powf(2.0) * y
        + s.lit(3.0) * x * y.powf(2.0)
        + y.powf(3.0);
    let diff_id = ((x + y).powf(3.0) - expanded).id();

    // 3. Exact univariate rational-function GCD cancellation:
    //    (x^3 - 6x^2 + 11x - 6) / (x - 2) == x^2 - 4x + 3
    let num = (x - s.lit(1.0)) * (x - s.lit(2.0)) * (x - s.lit(3.0));
    let den = x - s.lit(2.0);
    let rat_fn_id = (num / den).id();

    // 4. Nested polynomial equation: (x - 2)*(x - 3) + (x + 1)^2 = x^2 + 2*x + 1  =>  roots {2, 3}!
    let eq_lhs = ((x - s.lit(2.0)) * (x - s.lit(3.0)) + (x + s.lit(1.0)).powf(2.0)).id();
    let eq_rhs = (x.powf(2.0) + s.lit(2.0) * x + s.lit(1.0)).id();

    let mut g = s.into_graph();

    let cancelled = simplify_poly_expr(&mut g, diff_id).expect("binomial cancel");
    assert_eq!(g.node(cancelled).unwrap().as_const(), Some(0.0));

    let reduced = simplify_poly_expr(&mut g, rat_fn_id).expect("rational gcd cancel");
    // Must no longer have a Div node at the root, and must agree with x^2 - 4x + 3 everywhere,
    // including at the removable singularity x = 2 (value = 4 - 8 + 3 = -1)!
    assert!(!matches!(g.node(reduced).unwrap(), crate::Op::Div(_, _)));
    assert!(close(g.eval(reduced, &[2.0, 0.0]).unwrap(), -1.0, 1e-12));
    assert!(close(g.eval(reduced, &[5.0, 0.0]).unwrap(), 8.0, 1e-12));

    let Solution::Two(r1, r2) = solve_for(&mut g, eq_lhs, eq_rhs, vx).expect("solve nested poly") else {
        panic!("expected two roots for nested polynomial");
    };
    let v1 = g.eval(r1, &[0.0, 0.0]).unwrap();
    let v2 = g.eval(r2, &[0.0, 0.0]).unwrap();
    let (lo, hi) = if v1 < v2 { (v1, v2) } else { (v2, v1) };
    assert!(close(lo, 2.0, 1e-12) && close(hi, 3.0, 1e-12), "roots=({v1}, {v2})");
}

#[test]
fn blaze_sim_property_exact_poly_div_gcd_diff_and_planted_bugs() {
    let config = blaze_sim::Config { first: 2026_1122, ..blaze_sim::Config::cases(48) };
    blaze_sim::check(config, |t| {
        let mut g = ExprGraph::new_in(heap());
        let (vx, _) = g.var("x");

        // Build random univariate polynomials `p`, `q`, `r` in Q[x] with small integer coefficients.
        let draw_poly = |deg: u32, label: &'static str| -> Poly<_> {
            let mut acc = Poly::zero_in(heap());
            for d in 0..=deg {
                let c = (t.range(0..7, label) as i64) - 3;
                let coeff = if d == deg && c == 0 { 1 } else { c };
                let x_pow = Poly::var_in(vx, heap()).pow_u32(d).unwrap();
                let term = x_pow.scale(Rat::from_i64(coeff)).unwrap();
                acc = acc.add(&term).unwrap();
            }
            acc
        };

        let p = draw_poly(t.range(1..3, "deg_p") as u32, "cp");
        let q = draw_poly(t.range(1..3, "deg_q") as u32, "cq");
        let r = draw_poly(t.range(1..3, "deg_r") as u32, "cr");

        // 1. Exact division round-trip: (p * r) / r == (p, 0).
        let pr = p.mul(&r).map_err(|_| blaze_sim::Broke::Oracle("mul failed".into()))?;
        let (quot, rem) = pr
            .div_rem_univariate(&r, vx)
            .map_err(|_| blaze_sim::Broke::Oracle("div_rem failed".into()))?;
        if quot != p || !rem.is_zero() {
            return Err(blaze_sim::Broke::Oracle("(p * r) / r != (p, 0)".into()));
        }

        // 2. Euclidean GCD: gcd(p * r, q * r) must divide BOTH p*r and q*r and be divisible by r.
        let qr = q.mul(&r).map_err(|_| blaze_sim::Broke::Oracle("mul qr failed".into()))?;
        let common = pr
            .gcd_univariate(&qr, vx)
            .map_err(|_| blaze_sim::Broke::Oracle("gcd failed".into()))?;
        let (_, r1) = pr
            .div_rem_univariate(&common, vx)
            .map_err(|_| blaze_sim::Broke::Oracle("pr % gcd failed".into()))?;
        let (_, r2) = qr
            .div_rem_univariate(&common, vx)
            .map_err(|_| blaze_sim::Broke::Oracle("qr % gcd failed".into()))?;
        let (_, r3) = common
            .div_rem_univariate(&r, vx)
            .map_err(|_| blaze_sim::Broke::Oracle("gcd % r failed".into()))?;
        if !r1.is_zero() || !r2.is_zero() || !r3.is_zero() {
            return Err(blaze_sim::Broke::Oracle("gcd divisibility invariant broke".into()));
        }

        // 3. Exact poly_to_expr -> expr_to_poly round-trip + exact Poly::diff == eval_dual!
        let pr_expr = poly_to_expr(&mut g, &pr);
        let round = expr_to_poly(&g, pr_expr)
            .map_err(|_| blaze_sim::Broke::Oracle("expr_to_poly failed".into()))?
            .ok_or_else(|| blaze_sim::Broke::Oracle("expr_to_poly returned None".into()))?;
        if round != pr {
            return Err(blaze_sim::Broke::Oracle("poly -> expr -> poly mismatch".into()));
        }
        let d_pr = pr.diff(vx).map_err(|_| blaze_sim::Broke::Oracle("poly diff failed".into()))?;
        let dual = eval_dual(&g, pr_expr, &[1.5], vx)
            .map_err(|_| blaze_sim::Broke::Oracle("eval_dual failed".into()))?;
        let exact_d = d_pr
            .eval_rat(&[Rat::new(3, 2).unwrap()])
            .map_err(|_| blaze_sim::Broke::Oracle("eval_rat failed".into()))?
            .to_f64();
        if !close(exact_d, dual.deriv, 1e-10) {
            return Err(blaze_sim::Broke::Oracle("Poly::diff != Dual tangent".into()));
        }
        Ok(())
    });

    // Planted-bug controls (rule 43 `dst-not-review`): both PolyPlant bugs MUST be caught!
    let mut g = ExprGraph::new_in(heap());
    let (vx, _) = g.var("x");
    let x = Poly::var_in(vx, heap());
    let p1 = x.mul(&x).unwrap().sub(&Poly::constant_in(Rat::ONE, heap())).unwrap(); // x^2 - 1
    let p2 = x.sub(&Poly::constant_in(Rat::ONE, heap())).unwrap(); // x - 1
    let buggy_gcd = p1.gcd_univariate_with_plant(&p2, vx, PolyPlant::WrongGcdRemainder).unwrap();
    let (_, rem) = p2.div_rem_univariate(&buggy_gcd, vx).unwrap();
    assert!(!rem.is_zero(), "oracle failed to catch WrongGcdRemainder");

    let x2_expr = poly_to_expr(&mut g, &p1);
    let buggy_diff = p1.diff_with_plant(vx, PolyPlant::WrongPolyDiffCoeff).unwrap();
    let dual = eval_dual(&g, x2_expr, &[3.0], vx).unwrap();
    let got = buggy_diff.eval_f64(&[3.0]).unwrap();
    assert!(!close(got, dual.deriv, 1e-6), "oracle failed to catch WrongPolyDiffCoeff");
}

#[test]
fn select_relu_max_min_clamp_forward_reverse_and_comptime_match_dual_oracle() {
    // 1. Compile-time `ConstGraph::relu` + `max_expr` + compile-time symbolic diff!
    const COMPTIME_RELU_DIFF: (ConstGraph<64>, ConstExpr) = {
        let mut g = ConstGraph::new();
        let x = g.var(0);
        let c2 = g.lit(2.0);
        let two_x = g.mul(c2, x);
        let r = g.relu(two_x);
        let df = g.diff(r, 0);
        (g, df)
    };
    const RELU_DF_POS: f64 = COMPTIME_RELU_DIFF.0.eval_const(COMPTIME_RELU_DIFF.1, &[3.0]);
    const RELU_DF_NEG: f64 = COMPTIME_RELU_DIFF.0.eval_const(COMPTIME_RELU_DIFF.1, &[-3.0]);
    assert_eq!(RELU_DF_POS, 2.0);
    assert_eq!(RELU_DF_NEG, 0.0);

    // 2. Runtime `Sym` piecewise graph: f(x, y) = relu(x * y) + max(x, y) - min(x, 2*y) + clamp(x, -1, 1)
    let s = SymScope::new_in(heap());
    let (vx, x) = s.var("x");
    let (vy, y) = s.var("y");
    let f = (x * y).relu() + x.max(y) - x.min(s.lit(2.0) * y) + x.clamp(s.lit(-1.0), s.lit(1.0));
    let fid = f.id();
    // Degenerate select(cond, x, x) must fold to x immediately.
    let deg = x.select(y, y).id();
    let yid = y.id();
    let mut g = s.into_graph();
    assert_eq!(deg, yid);

    // Forward diff + reverse-mode grad + e-graph simplification must all agree with Dual oracle
    // on points across multiple branches!
    let df_dx = diff(&mut g, fid, vx).unwrap();
    let df_dy = diff(&mut g, fid, vy).unwrap();
    let rev = grad_reverse(&mut g, fid, &[vx, vy]).unwrap();
    let simp_f = simplify(&mut g, fid).unwrap();
    let simp_dx = simplify(&mut g, df_dx).unwrap();

    for env in [[0.4, 1.5], [-0.5, 1.2], [2.5, -0.7], [-2.0, -0.3]] {
        let dual_x = eval_dual(&g, fid, &env, vx).unwrap();
        let dual_y = eval_dual(&g, fid, &env, vy).unwrap();
        assert!(close(g.eval(simp_f, &env).unwrap(), dual_x.val, 1e-12));
        assert!(close(g.eval(df_dx, &env).unwrap(), dual_x.deriv, 1e-12));
        assert!(close(g.eval(df_dy, &env).unwrap(), dual_y.deriv, 1e-12));
        assert!(close(g.eval(rev[0], &env).unwrap(), dual_x.deriv, 1e-12));
        assert!(close(g.eval(rev[1], &env).unwrap(), dual_y.deriv, 1e-12));
        assert!(close(g.eval(simp_dx, &env).unwrap(), dual_x.deriv, 1e-12));
    }

    // Rust SSA emitter produces real `if ... > 0.0_f64 { ... } else { ... }`.
    let code = emit_rust_fn(&g, "piecewise", fid).unwrap();
    assert!(code.as_str().contains("> 0.0_f64"), "expected select branch in:\n{}", code.as_str());
}

#[test]
fn declarative_custom_rewrite_rules_simplify_domain_identities() {
    // Declarative `const` user rewrite rules (zero heap, postorder PatNode slices: root is last):
    //   1. exp(?0) * exp(?1)  =>  exp(?0 + ?1)
    //   2. ln(?0) + ln(?1)    =>  ln(?0 * ?1)
    const EXP_MUL_LHS: &[PatNode] = &[
        PatNode::Var(0),
        PatNode::Var(1),
        PatNode::Exp(0),
        PatNode::Exp(1),
        PatNode::Mul(2, 3),
    ];
    const EXP_MUL_RHS: &[PatNode] = &[
        PatNode::Var(0),
        PatNode::Var(1),
        PatNode::Add(0, 1),
        PatNode::Exp(2),
    ];
    const LN_ADD_LHS: &[PatNode] = &[
        PatNode::Var(0),
        PatNode::Var(1),
        PatNode::Ln(0),
        PatNode::Ln(1),
        PatNode::Add(2, 3),
    ];
    const LN_ADD_RHS: &[PatNode] = &[
        PatNode::Var(0),
        PatNode::Var(1),
        PatNode::Mul(0, 1),
        PatNode::Ln(2),
    ];
    const RULES: &[RewriteRule<'static>] = &[
        RewriteRule { name: "exp_mul", lhs: EXP_MUL_LHS, rhs: EXP_MUL_RHS },
        RewriteRule { name: "ln_add", lhs: LN_ADD_LHS, rhs: LN_ADD_RHS },
    ];

    let s = SymScope::new_in(heap());
    let (_, x) = s.var("x");
    let (_, y) = s.var("y");
    let e = (x.exp() * y.exp()) + (x.ln() + y.ln());
    let eid = e.id();
    let mut g = s.into_graph();

    let cost_before = expr_cost(&g, eid).unwrap();
    let (simp, _stats) = simplify_with_rules(&mut g, eid, SimplifyOpts::default(), RULES).unwrap();
    let cost_after = expr_cost(&g, simp).unwrap();
    assert!(
        cost_after < cost_before,
        "expected custom rules to reduce cost ({cost_before} -> {cost_after}): {}",
        format_expr(&g, simp).unwrap().as_str()
    );
    // Semantic equality across sample points!
    for env in [[1.2, 2.3], [0.5, 4.1], [3.0, 0.7]] {
        assert!(close(g.eval(eid, &env).unwrap(), g.eval(simp, &env).unwrap(), 1e-12));
    }
}

#[test]
fn symbolic_linear_system_and_exact_newton_raphson_solver() {
    // 1. Symbolic 2x2 parametric linear system:
    //    a*x + b*y = u
    //    c*x + d*y = v
    //    Solution: x = (u*d - b*v) / (a*d - b*c), y = (a*v - u*c) / (a*d - b*c)
    let mut g = ExprGraph::new_in(heap());
    let (_, a) = g.var("a");
    let (_, b) = g.var("b");
    let (_, c) = g.var("c");
    let (_, d) = g.var("d");
    let (_, u) = g.var("u");
    let (_, v) = g.var("v");

    let sol = solve_linear_system(&mut g, &[a, b, c, d], &[u, v], 2).unwrap().expect("non-singular");
    assert_eq!(sol.len(), 2);
    // Test with a=2, b=1, c=5, d=3 (det = 1), u=4, v=11 => x=1, y=2
    let env = [2.0, 1.0, 5.0, 3.0, 4.0, 11.0];
    let x_val = g.eval(sol[0], &env).unwrap();
    let y_val = g.eval(sol[1], &env).unwrap();
    assert!(close(x_val, 1.0, 1e-12), "x={x_val}");
    assert!(close(y_val, 2.0, 1e-12), "y={y_val}");

    // 2. Numeric 3x3 linear system:
    //    2x + y - z = 8
    //   -3x - y + 2z = -11
    //   -2x + y + 2z = -3
    //    Exact solution: x = 2, y = 3, z = -1
    let mat3 = [
        g.lit(2.0), g.lit(1.0), g.lit(-1.0),
        g.lit(-3.0), g.lit(-1.0), g.lit(2.0),
        g.lit(-2.0), g.lit(1.0), g.lit(2.0),
    ];
    let rhs3 = [g.lit(8.0), g.lit(-11.0), g.lit(-3.0)];
    let sol3 = solve_linear_system(&mut g, &mat3, &rhs3, 3).unwrap().expect("non-singular 3x3");
    assert!(close(g.eval(sol3[0], &env).unwrap(), 2.0, 1e-12));
    assert!(close(g.eval(sol3[1], &env).unwrap(), 3.0, 1e-12));
    assert!(close(g.eval(sol3[2], &env).unwrap(), -1.0, 1e-12));

    // 3. Exact symbolic Newton-Raphson:
    //    (a) x^3 - 2 = 0  =>  2^(1/3)
    let mut g2 = ExprGraph::new_in(heap());
    let (vx, ex) = g2.var("x");
    let c3 = g2.lit(3.0);
    let c2 = g2.lit(2.0);
    let x3 = g2.pow(ex, c3);
    let f_cbrt = g2.sub(x3, c2);
    let step_expr = newton_step(&mut g2, f_cbrt, vx).unwrap();
    // One Newton step from x0 = 1.0 for x^3 - 2 is 1 - (-1)/3 = 4/3.
    assert!(close(g2.eval(step_expr, &[1.0]).unwrap(), 4.0 / 3.0, 1e-12));

    let res = eval_newton(&mut g2, f_cbrt, vx, &[0.0], 1.5, NewtonOpts::default()).unwrap();
    assert!(res.converged);
    assert!(close(res.root * res.root * res.root, 2.0, 1e-12), "root={}", res.root);

    //    (b) Transcendental Dottie number: cos(x) - x = 0  =>  0.7390851332151607...
    let cos_x = g2.cos(ex);
    let f_dottie = g2.sub(cos_x, ex);
    let dottie = eval_newton(&mut g2, f_dottie, vx, &[0.0], 0.5, NewtonOpts::default()).unwrap();
    assert!(dottie.converged);
    assert!(close(blaze_math::cos(dottie.root), dottie.root, 1e-12), "dottie={}", dottie.root);
}

#[test]
fn interval_dead_branch_pruning_ieee_exact_mode_and_cpu_fma_fusion() {
    // 1. Sound interval enclosure + dead-branch elimination on `Op::Select` and `abs`:
    let s = SymScope::new_in(heap());
    let (_, x) = s.var("x");
    let (_, y) = s.var("y");
    let expr = (x.relu() + y.relu() + x.abs() + (x / x)).id();
    let mut g = s.into_graph();

    // x in [2, 5] (> 0), y in [-4, -1] (< 0):
    //   relu(x) -> x, relu(y) -> 0, abs(x) -> x, x / x -> 1  =>  2*x + 1
    let bounds = [Interval::new(2.0, 5.0).unwrap(), Interval::new(-4.0, -1.0).unwrap()];
    let iv = eval_interval(&g, expr, &bounds).unwrap();
    for env in [[2.0, -4.0], [3.5, -2.5], [5.0, -1.0]] {
        let v = g.eval(expr, &env).unwrap();
        assert!(iv.contains(v), "interval {iv:?} failed to enclose {v}");
    }
    let pruned = prune_branches(&mut g, expr, &bounds).unwrap();
    let simp = simplify(&mut g, pruned).unwrap();
    let txt = format_expr(&g, simp).unwrap();
    assert!(!txt.as_str().contains("select"), "dead select remained: {}", txt.as_str());
    assert!(!txt.as_str().contains("abs"), "signed abs remained: {}", txt.as_str());
    for env in [[2.0, -4.0], [3.5, -2.5], [5.0, -1.0]] {
        assert!(close(g.eval(expr, &env).unwrap(), g.eval(simp, &env).unwrap(), 1e-12));
    }

    // 2. RewriteMode::IeeeExact must NOT apply `x / x -> 1`, while still simplifying `x * 1 -> x`:
    let mut g2 = ExprGraph::new_in(heap());
    let (_, ex) = g2.var("x");
    let x_div_x = g2.div(ex, ex);
    let ieee_opts = SimplifyOpts { mode: RewriteMode::IeeeExact, ..SimplifyOpts::default() };
    let (ieee_out, _) = simplify_with(&mut g2, x_div_x, ieee_opts).unwrap();
    assert!(matches!(g2.node(ieee_out).unwrap(), Op::Div(_, _)), "IeeeExact must preserve x / x");

    let (fast_out, _) = simplify_with(&mut g2, x_div_x, SimplifyOpts::default()).unwrap();
    assert_eq!(g2.node(fast_out).unwrap().as_const(), Some(1.0));

    // 3. CostModel::CpuFma fuses `a * b + c` into `Op::Fma(a, b, c)`!
    let mut g3 = ExprGraph::new_in(heap());
    let (va, ea) = g3.var("a");
    let (_, eb) = g3.var("b");
    let (_, ec) = g3.var("c");
    let ab = g3.mul(ea, eb);
    let ab_c = g3.add(ab, ec);
    let fma_opts = SimplifyOpts { cost_model: CostModel::CpuFma, ..SimplifyOpts::default() };
    let (fused, _) = simplify_with(&mut g3, ab_c, fma_opts).unwrap();
    assert!(matches!(g3.node(fused).unwrap(), Op::Fma(_, _, _)), "expected Fma fusion");
    assert!(
        expr_cost_with(&g3, fused, CostModel::CpuFma).unwrap()
            < expr_cost_with(&g3, ab_c, CostModel::CpuFma).unwrap()
    );
    let code = emit_rust_fn(&g3, "fused_mac", fused).unwrap();
    assert!(code.as_str().contains("blaze_math::mul_add"), "expected mul_add in:\n{}", code.as_str());
    // Differentiate Fma and check against Dual oracle:
    let df_da = diff(&mut g3, fused, va).unwrap();
    let dual = eval_dual(&g3, fused, &[2.0, 3.5, -1.2], va).unwrap();
    assert!(close(g3.eval(df_da, &[2.0, 3.5, -1.2]).unwrap(), dual.deriv, 1e-12));
}

#[test]
fn jvp_hvp_and_active_vars_sparsity_match_full_hessian_oracle() {
    // f(x, y, z) = sin(x * y) + exp(x) * y^2  (z is inactive!)
    let s = SymScope::new_in(heap());
    let (vx, x) = s.var("x");
    let (vy, y) = s.var("y");
    let (vz, _z) = s.var("z");
    let (_, dx) = s.var("vx");
    let (_, dy) = s.var("vy");
    let (_, dz) = s.var("vz");
    let f = ((x * y).sin() + x.exp() * y * y).id();
    let dir = [dx.id(), dy.id(), dz.id()];
    let mut g = s.into_graph();

    let active = active_vars(&g, f, 3).unwrap();
    assert_eq!(active.as_slice(), &[true, true, false]);

    let directional = jvp(&mut g, f, &[vx, vy, vz], &dir).unwrap();
    let hv = hvp(&mut g, f, &[vx, vy, vz], &dir).unwrap();
    let h = hessian(&mut g, f, &[vx, vy, vz]).unwrap();
    let grad_f = grad_reverse(&mut g, f, &[vx, vy, vz]).unwrap();

    let env = [0.7, -1.3, 42.0, 0.4, -0.9, 1.5];
    let v = [env[3], env[4], env[5]];

    // Verify jvp == grad(f) · v
    let mut expected_jvp = 0.0;
    for i in 0..3 {
        expected_jvp += g.eval(grad_f[i], &env).unwrap() * v[i];
    }
    assert!(close(g.eval(directional, &env).unwrap(), expected_jvp, 1e-11));

    // Verify hvp[i] == sum_j H[i, j] * v[j]
    for i in 0..3 {
        let mut expected_hi = 0.0;
        for j in 0..3 {
            expected_hi += g.eval(h[i * 3 + j], &env).unwrap() * v[j];
        }
        let got_hi = g.eval(hv[i], &env).unwrap();
        assert!(close(got_hi, expected_hi, 1e-11), "hvp[{i}]: {got_hi} vs {expected_hi}");
    }
    assert_eq!(g.eval(hv[2], &env).unwrap(), 0.0);
}

#[test]
fn declarative_rules_table_drives_egraph_and_exports_lean4_obligations() {
    // 1. Same STANDARD_REWRITE_RULES drives Rust e-graph saturation:
    let s = SymScope::new_in(heap());
    let (_, x) = s.var("x");
    let (_, y) = s.var("y");
    let e = (x.exp() * y.exp() + x.ln() + y.ln()).id();
    let mut g = s.into_graph();
    let rules = standard_rewrite_rules(RewriteMode::FastMath, heap());
    let (simp, stats) = simplify_with_rules(&mut g, e, SimplifyOpts::default(), rules.as_slice()).unwrap();
    assert!(stats.cost_after < stats.cost_before);
    assert!(close(g.eval(e, &[1.3, 2.7]).unwrap(), g.eval(simp, &[1.3, 2.7]).unwrap(), 1e-12));

    // 2. And exports a complete Lean 4 proof-obligation module with AllSound witness fields:
    let lean = export_lean_theorems(STANDARD_REWRITE_RULES, DERIV_RULES, heap());
    let src = lean.as_str();
    assert!(src.contains("namespace BlazeAlgebra.Verified"));
    assert!(src.contains("structure AllSound : Prop where"));
    let mut pat = blaze_own::String::new_in(heap());
    for r in STANDARD_REWRITE_RULES {
        pat.clear();
        let _ = core::fmt::Write::write_fmt(&mut pat, format_args!("def rule_{} : Prop :=", r.name));
        assert!(src.contains(pat.as_str()), "missing {pat} in Lean export");
        pat.clear();
        let _ = core::fmt::Write::write_fmt(&mut pat, format_args!("rule_{0} : rule_{0}", r.name));
        assert!(src.contains(pat.as_str()), "missing {pat} in AllSound");
    }
    for d in DERIV_RULES {
        pat.clear();
        let _ = core::fmt::Write::write_fmt(&mut pat, format_args!("def deriv_{} : Prop :=", d.op));
        assert!(src.contains(pat.as_str()), "missing {pat} in Lean export");
        pat.clear();
        let _ = core::fmt::Write::write_fmt(&mut pat, format_args!("deriv_{0} : deriv_{0}", d.op));
        assert!(src.contains(pat.as_str()), "missing {pat} in AllSound");
    }

    // 3. Zero-drift gate: checked-in `lean/analysis/BlazeDiff/Generated.lean` must match byte-for-byte!
    let checked_in = include_str!("../../../lean/analysis/BlazeDiff/Generated.lean");
    assert_eq!(
        src, checked_in,
        "lean/analysis/BlazeDiff/Generated.lean drifted from blaze_algebra::rules::export_lean_theorems"
    );
}
