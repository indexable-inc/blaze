//! The derivative against central finite differences, with the planted bugs that show the
//! oracle sees a wrong one; and the plan's directed cases.

use super::{float_term, pick, plan_example};
use crate::{
    diff::{
        self, Choice, DiffError, Differentiator, P, Planted, Policy, RULES, Rule, hessian, rule_of,
    },
    eval::{Value, eval, eval_all, eval_one},
    expr::{Graph, Id, Op, Sort, Width},
    print,
    simplify::{Options, Stop, simplify},
};

/// A point is kept this far from where a rule stops holding and from a comparison's tie.
const MARGIN: f64 = 1e-3;
/// Values past this are left out: the difference quotient of a huge value is rounding noise.
const LARGEST: f64 = 1e4;

/// What decides which side of every kink `point` is on, for the nodes `roots` reach: each
/// condition's value, and for `abs`, `min` and `max` the sign that picks their branch. `None`
/// when the point is not one to check at: a value is not finite or is huge, or the point is
/// within [`MARGIN`] of where some operation's rule stops holding or a comparison ties.
fn sides(graph: &Graph, roots: &[Id], point: &[f64]) -> Option<Vec<bool>> {
    let values = eval_all::<f64>(graph, roots, point).ok()?;
    let float = |id: Id| values[id.index()].expect("an argument of a reached node").float();
    let mut sides = Vec::new();
    for (at, node) in graph.nodes().iter().enumerate() {
        let Some(value) = values[at] else { continue };
        let mut args = [0.0; 3];
        for (slot, arg) in node.args().iter().enumerate() {
            if graph.sort(*arg) == Sort::Float {
                args[slot] = float(*arg);
            }
        }
        match value {
            Value::Bool(side) => {
                let compares =
                    matches!(node.op, Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::Eq | Op::Ne);
                if compares && (args[0] - args[1]).abs() < MARGIN {
                    return None;
                }
                sides.push(side);
            }
            Value::Float(value) => {
                if !value.is_finite() || value.abs() > LARGEST {
                    return None;
                }
                if let Some(rule) = rule_of(RULES, node.op) {
                    if rule.domain.distance(node, &args) < MARGIN {
                        return None;
                    }
                    match node.op {
                        Op::Abs => sides.push(args[0] > 0.0),
                        Op::Min | Op::Max => sides.push(args[0] > args[1]),
                        _ => {}
                    }
                }
            }
        }
    }
    Some(sides)
}

/// The oracle: whether `df` is the derivative of `f` by variable `var` at `point`, by central
/// differences at two step sizes. `Ok(false)` when the point is not one a difference quotient
/// can judge (a kink, a pole or a huge value nearby, or the two step sizes disagree, which is
/// what rounding noise and a function that turns within the step look like); `Err` says what
/// disagreed. The tolerance is relative to the derivative's size, at least 1.
fn agrees(graph: &Graph, f: Id, df: Id, var: usize, point: &[f64]) -> Result<bool, String> {
    let h = 1e-5 * point[var].abs().max(1.0);
    let at = |offset: f64| {
        let mut moved = point.to_vec();
        moved[var] += offset;
        moved
    };
    let Some(here) = sides(graph, &[f, df], point) else { return Ok(false) };
    let mut quotients = [0.0; 2];
    for (slot, step) in [h, h / 2.0].into_iter().enumerate() {
        let (below, above) = (at(-step), at(step));
        // The same side of every kink across the step: the function is one smooth piece.
        if sides(graph, &[f, df], &below).as_ref() != Some(&here)
            || sides(graph, &[f, df], &above).as_ref() != Some(&here)
        {
            return Ok(false);
        }
        let f_below = eval_one::<f64>(graph, f, &below).map_err(|it| it.to_string())?;
        let f_above = eval_one::<f64>(graph, f, &above).map_err(|it| it.to_string())?;
        quotients[slot] = (f_above - f_below) / (above[var] - below[var]);
    }
    let symbolic = eval_one::<f64>(graph, df, point).map_err(|it| it.to_string())?;
    let [coarse, fine] = quotients;
    let scale = symbolic.abs().max(fine.abs()).max(1.0);
    if (coarse - fine).abs() > 1e-7 * scale {
        return Ok(false);
    }
    if (symbolic - fine).abs() > 1e-4 * scale {
        return Err(format!(
            "at {point:?} the symbolic derivative by variable {var} is {symbolic:e}, and the \
             central difference is {fine:e} (at twice the step {coarse:e})\n{graph:?}"
        ));
    }
    Ok(true)
}

/// The invariant: the symbolic derivative of an expression is its derivative: for a drawn
/// expression over every operation and a drawn point inside each operation's domain and away
/// from its kinks, the value of `d(e)/dx` agrees with the central finite difference of `e`,
/// within a tolerance relative to the derivative's size; and so does its exact simplification.
#[hegel::test]
fn the_symbolic_derivative_agrees_with_finite_differences(tc: hegel::TestCase) {
    let mut graph = Graph::new(Width::F64);
    let f = float_term(&tc, &mut graph, 2, 3, true);
    // Coordinates in [0.3, 1.7], in steps of 1/1000.
    let coordinate = |tc: &hegel::TestCase| 0.3 + f64::from(pick(tc, 1400)) / 1000.0;
    let point = [coordinate(&tc), coordinate(&tc)];
    let var = pick(&tc, 1);
    let policy = Policy { kinks: Choice::Subgradient(0.5) };
    let df = diff::derivative(&mut graph, f, var, policy).expect("a subgradient is chosen");
    if let Err(message) = agrees(&graph, f, df, var as usize, &point) {
        panic!("{message}");
    }
    // The same derivative after the exact rewrites, in one graph with the function.
    let simplified = simplify(&graph, &[f, df], &Options::default());
    let [f, df] = simplified.roots[..] else { panic!("two roots in, two out") };
    if let Err(message) = agrees(&simplified.graph, f, df, var as usize, &point) {
        panic!("after simplification: {message}");
    }
}

/// `term`'s derivative by `x` at `point` as `differentiator` takes it, judged by the oracle.
fn judged(
    differentiator: &Differentiator<'_>,
    term: impl Fn(&mut Graph, Id) -> Id,
    point: f64,
) -> Result<bool, String> {
    let mut graph = Graph::new(Width::F64);
    let x = graph.var(0);
    let f = term(&mut graph, x);
    let df = differentiator.derivatives(&mut graph, &[f], 0).expect("no kink in the term")[0];
    agrees(&graph, f, df, 0, &[point])
}

/// [`RULES`] with `op`'s tangent replaced.
fn rules_with(op: Op, tangent: P) -> Vec<Rule> {
    RULES.iter().map(|rule| if rule.op == op { Rule { tangent, ..*rule } } else { *rule }).collect()
}

/// A control passes when the oracle accepts the right derivative of the term and refuses the
/// planted one, at the same point.
#[track_caller]
fn caught(planted: &Differentiator<'_>, term: impl Fn(&mut Graph, Id) -> Id + Copy, point: f64) {
    let right = Differentiator::new(Policy::default());
    assert_eq!(judged(&right, term, point), Ok(true), "the oracle refuses the right derivative");
    let verdict = judged(planted, term, point);
    assert!(verdict.is_err(), "the oracle let the planted bug through: {verdict:?}");
}

#[test]
fn the_oracle_catches_a_wrong_quotient_rule_sign() {
    // (a / b)' = (a' + (a / b) * b') / b: the sign of the second term is wrong.
    const WRONG: P = P::Div(&P::Add(&P::T(0), &P::Mul(&P::Out, &P::T(1))), &P::A(1));
    let rules = rules_with(Op::Div, WRONG);
    let planted =
        Differentiator { rules: &rules, policy: Policy::default(), planted: Planted::None };
    // x / (1 + x * x)
    let term = |graph: &mut Graph, x: Id| {
        let one = graph.constant(1.0);
        let xx = graph.binary(Op::Mul, x, x);
        let denominator = graph.binary(Op::Add, one, xx);
        graph.binary(Op::Div, x, denominator)
    };
    caught(&planted, term, 0.7);
}

#[test]
fn the_oracle_catches_a_missing_chain_rule_factor_for_tanh() {
    // tanh(u)' = 1 - tanh(u)², without u'.
    const WRONG: P = P::Sub(&P::Lit(1.0), &P::Mul(&P::Out, &P::Out));
    let rules = rules_with(Op::Tanh, WRONG);
    let planted =
        Differentiator { rules: &rules, policy: Policy::default(), planted: Planted::None };
    // tanh(3 * x)
    let term = |graph: &mut Graph, x: Id| {
        let three = graph.constant(3.0);
        let inner = graph.binary(Op::Mul, three, x);
        graph.unary(Op::Tanh, inner)
    };
    caught(&planted, term, 0.3);
}

#[test]
fn the_oracle_catches_a_derivative_that_drops_a_shared_nodes_second_use() {
    let planted = Differentiator {
        rules: RULES,
        policy: Policy::default(),
        planted: Planted::DropsSecondUse,
    };
    // sin(x) * sin(x): the shared `sin(x)` is used twice.
    let term = |graph: &mut Graph, x: Id| {
        let sin = graph.unary(Op::Sin, x);
        graph.binary(Op::Mul, sin, sin)
    };
    caught(&planted, term, 0.7);
}

#[test]
fn every_float_operation_has_one_rule() {
    for &op in Op::ALL {
        let rules = RULES.iter().filter(|it| it.op == op).count();
        let differentiable = op.sort() == Sort::Float && !matches!(op, Op::Const | Op::Var);
        assert_eq!(rules, usize::from(differentiable), "`{op}`");
    }
}

/// The plan's closed form of `d/dx (x * y).sin() / (1.0 + x * x)`.
fn plan_closed_form(x: f64, y: f64) -> f64 {
    let d = 1.0 + x * x;
    (y * (x * y).cos() * d - 2.0 * x * (x * y).sin()) / (d * d)
}

const POINTS: &[[f64; 2]] = &[[0.5, 0.75], [1.25, -0.5], [-2.0, 1.5], [0.1, 3.0], [1.7, 0.3]];

#[track_caller]
fn close(found: f64, expected: f64, tolerance: f64) {
    let scale = expected.abs().max(1.0);
    assert!(
        (found - expected).abs() <= tolerance * scale,
        "{found:e} is not within {tolerance:e} of {expected:e}"
    );
}

#[test]
fn the_plans_example_differentiates_to_its_closed_form() {
    let mut graph = Graph::new(Width::F64);
    let f = plan_example(&mut graph);
    let df = diff::derivative(&mut graph, f, 0, Policy::default()).expect("no kink");
    // As the rules build it: the quotient rule with the quotient shared.
    assert_eq!(
        print::math(&graph, df, &["x", "y"]),
        "(y * cos(x * y) - sin(x * y) / (1 + x²) * (x + x)) / (1 + x²)"
    );
    assert_eq!(
        print::rust(&graph, &[df], "d_f_dx", &["x", "y"]),
        "fn d_f_dx(x: f64, y: f64) -> f64 {\n    \
             let t0: f64 = x * y;\n    \
             let t1: f64 = 1.0 + x * x;\n    \
             (y * t0.cos() - t0.sin() / t1 * (x + x)) / t1\n\
         }\n"
    );
    for &[x, y] in POINTS {
        close(eval_one(&graph, df, &[x, y]).unwrap(), plan_closed_form(x, y), 1e-12);
    }

    // The exact rewrites keep its value bit for bit.
    let exact = simplify(&graph, &[df], &Options::default());
    for &[x, y] in POINTS {
        let before: f64 = eval_one(&graph, df, &[x, y]).unwrap();
        let after: f64 = eval_one(&exact.graph, exact.roots[0], &[x, y]).unwrap();
        assert_eq!(before.to_bits(), after.to_bits(), "at ({x}, {y})");
    }

    // With `fast_math` it is still the closed form, up to rounding, and still one `sin` and
    // one `cos` of the one `x * y`.
    let fast = simplify(&graph, &[df], &Options { fast_math: true, ..Options::default() });
    for &[x, y] in POINTS {
        close(eval_one(&fast.graph, fast.roots[0], &[x, y]).unwrap(), plan_closed_form(x, y), 1e-9);
    }
    let reached = fast.graph.reached(&fast.roots);
    let count = |op: Op| {
        fast.graph.nodes().iter().enumerate().filter(|(at, it)| reached[*at] && it.op == op).count()
    };
    assert_eq!((count(Op::Sin), count(Op::Cos)), (1, 1), "{:?}", fast.graph);
    assert!(!fast.ran.is_empty() || fast.stop == Stop::Saturated);
    // The closed form itself, written as the plan writes it, is the same function.
    let mut plan = Graph::new(Width::F64);
    let (x, y) = (plan.var(0), plan.var(1));
    let xy = plan.binary(Op::Mul, x, y);
    let (sin, cos) = (plan.unary(Op::Sin, xy), plan.unary(Op::Cos, xy));
    let one = plan.constant(1.0);
    let xx = plan.binary(Op::Mul, x, x);
    let d = plan.binary(Op::Add, one, xx);
    let y_cos = plan.binary(Op::Mul, y, cos);
    let left = plan.binary(Op::Mul, y_cos, d);
    let two = plan.constant(2.0);
    let two_x = plan.binary(Op::Mul, two, x);
    let right = plan.binary(Op::Mul, two_x, sin);
    let numerator = plan.binary(Op::Sub, left, right);
    let dd = plan.binary(Op::Mul, d, d);
    let closed = plan.binary(Op::Div, numerator, dd);
    assert_eq!(
        print::math(&plan, closed, &["x", "y"]),
        "(y * cos(x * y) * (1 + x²) - 2x * sin(x * y)) / (1 + x²)²"
    );
}

#[test]
fn the_hessian_of_a_quadratic_form_is_constant() {
    // 3x² + 2xy + 5y²
    let mut graph = Graph::new(Width::F64);
    let (x, y) = (graph.var(0), graph.var(1));
    let (three, two, five) = (graph.constant(3.0), graph.constant(2.0), graph.constant(5.0));
    let three_x = graph.binary(Op::Mul, three, x);
    let xx = graph.binary(Op::Mul, three_x, x);
    let two_x = graph.binary(Op::Mul, two, x);
    let xy = graph.binary(Op::Mul, two_x, y);
    let five_y = graph.binary(Op::Mul, five, y);
    let yy = graph.binary(Op::Mul, five_y, y);
    let sum = graph.binary(Op::Add, xx, xy);
    let f = graph.binary(Op::Add, sum, yy);

    let hessian = hessian(&mut graph, f, &[0, 1], Policy::default()).expect("no kink");
    let entries: Vec<Id> = hessian.iter().flatten().copied().collect();
    let simplified = simplify(&graph, &entries, &Options::default());
    let constants: Vec<Option<f64>> =
        simplified.roots.iter().map(|&it| simplified.graph.as_const(it)).collect();
    assert_eq!(constants, [Some(6.0), Some(2.0), Some(2.0), Some(10.0)], "{:?}", simplified.graph);
}

#[test]
fn abs_at_zero_is_a_typed_error_by_default_and_a_choice_when_asked() {
    let mut graph = Graph::new(Width::F64);
    let (x, y) = (graph.var(0), graph.var(1));
    let abs = graph.unary(Op::Abs, x);
    assert_eq!(
        diff::derivative(&mut graph, abs, 0, Policy::default()),
        Err(DiffError::NonDifferentiable { op: Op::Abs, point: "0", node: abs })
    );
    // The variable does not reach the kink: nothing to choose.
    let abs_y = graph.unary(Op::Abs, y);
    let zero = diff::derivative(&mut graph, abs_y, 0, Policy::default()).expect("no `x` in it");
    assert_eq!(graph.as_const(zero), Some(0.0));

    for choice in [0.0, 0.25, -1.0] {
        let policy = Policy { kinks: Choice::Subgradient(choice) };
        let d = diff::derivative(&mut graph, abs, 0, policy).expect("a subgradient is chosen");
        let at = |x: f64| eval_one::<f64>(&graph, d, &[x, 0.0]).unwrap();
        assert_eq!((at(2.0), at(-3.0), at(0.0), at(-0.0)), (1.0, -1.0, choice, choice));
    }

    // The other kinks, each named.
    let named =
        |graph: &mut Graph, root: Id| match diff::derivative(graph, root, 0, Policy::default()) {
            Err(DiffError::NonDifferentiable { op, point, .. }) => (op, point),
            other => panic!("expected a kink, got {other:?}"),
        };
    let min = graph.binary(Op::Min, x, y);
    assert_eq!(named(&mut graph, min), (Op::Min, "a tie of its operands"));
    let sqrt = graph.unary(Op::Sqrt, x);
    assert_eq!(named(&mut graph, sqrt), (Op::Sqrt, "0"));
    let cond = graph.binary(Op::Lt, x, y);
    let select = graph.select(cond, x, y);
    assert_eq!(named(&mut graph, select), (Op::Select, "the boundary of its condition"));
    // A `select` on a condition without the variable is differentiable arm by arm.
    let cond = graph.binary(Op::Lt, y, y);
    let select = graph.select(cond, x, y);
    let d = diff::derivative(&mut graph, select, 0, Policy::default()).expect("no kink");
    assert_eq!(eval::<f64>(&graph, &[d], &[1.0, 2.0]).unwrap(), [0.0]);
}

#[test]
fn a_shared_node_is_differentiated_once() {
    // e = sin(e) * sin(e), twelve times: a tree of 2^12 leaves, a DAG of a few dozen nodes.
    let mut graph = Graph::new(Width::F64);
    let mut e = graph.var(0);
    for _ in 0..12 {
        let sin = graph.unary(Op::Sin, e);
        e = graph.binary(Op::Mul, sin, sin);
    }
    let before = graph.len();
    diff::derivative(&mut graph, e, 0, Policy::default()).expect("no kink");
    assert!(graph.len() <= before * 6, "{} nodes became {}", before, graph.len());
}

#[hegel::test]
fn a_gradient_is_the_derivative_by_each_variable(tc: hegel::TestCase) {
    let mut graph = Graph::new(Width::F64);
    let f = float_term(&tc, &mut graph, 3, 2, true);
    let policy = Policy { kinks: Choice::Subgradient(0.0) };
    let gradient = diff::gradient(&mut graph, f, &[0, 1, 2], policy).unwrap();
    let jacobian = diff::jacobian(&mut graph, &[f, f], &[0, 1, 2], policy).unwrap();
    for (var, &entry) in gradient.iter().enumerate() {
        let alone = diff::derivative(&mut graph, f, var as u32, policy).unwrap();
        // Hash-consing: the same derivative is the same node.
        assert_eq!(entry, alone);
        assert_eq!(jacobian[0][var], alone);
        assert_eq!(jacobian[1][var], alone);
    }
}
