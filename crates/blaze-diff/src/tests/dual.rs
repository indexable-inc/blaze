//! The symbolic derivative against forward-mode dual numbers ([`crate::eval::dual`]): an
//! independent method that takes no step, so it judges to rounding, where finite differences
//! judge to four digits. With the planted bugs that show the oracle sees a wrong derivative,
//! one of them too small for finite differences.
//!
//! This is also the link between the Rust differentiator and the Lean one. The rule table is
//! proved (`lean/analysis/BlazeDiff/Correct.lean`, `diff_correct`: the table both read is
//! generated from `rules.json`), but of a differentiator over terms that builds every zero. That
//! the Rust one, over a hash-consed DAG and never building a structural zero or one, computes
//! the same derivative is what the refinement below holds.

use super::{
    derivative::{agrees, rules_with, sides},
    float_term, pick,
};
use crate::{
    diff::{self, Choice, Differentiator, P, Planted, Policy, RULES},
    eval::{dual::dual, eval_one},
    expr::{Graph, Id, Op, Width},
};

/// The two tangents agree within this many times the dual's bound (the sum of the absolute
/// values of the terms the derivative is): about 450,000 units in the last place of that
/// scale. Each operation of either method is within a few units of it and a term of depth 3
/// chains a few dozen, so a correct pair is orders inside; it is kept wide because the rule
/// table's own forms (`1 - tanh²`, the quotient through the quotient) lose digits the bound
/// only estimates. A wrong rule is off by the size of a term, eight orders outside.
const TOLERANCE: f64 = 1e-10;

/// The oracle: whether `df` at `point` is the dual tangent of `f` by variable `var`. `Ok(false)`
/// when the point is not one to judge at (within the margin of a kink, a pole or a tie, a huge
/// value, or no finite tangent); `Err` says what disagreed. Where the bound is 0 (no term at
/// all) the two are equal exactly.
fn tangent_agrees(graph: &Graph, f: Id, df: Id, var: u32, point: &[f64]) -> Result<bool, String> {
    if sides(graph, &[f, df], point).is_none() {
        return Ok(false);
    }
    let forward = dual::<f64>(graph, &[f], point, var).map_err(|it| it.to_string())?[0];
    let symbolic = eval_one::<f64>(graph, df, point).map_err(|it| it.to_string())?;
    if !forward.tangent.is_finite() || !forward.bound.is_finite() {
        return Ok(false);
    }
    if (symbolic - forward.tangent).abs() > TOLERANCE * forward.bound {
        return Err(format!(
            "at {point:?} the symbolic derivative by variable {var} is {symbolic:e}, and the \
             dual tangent is {:e} (its terms sum to {:e})\n{graph:?}",
            forward.tangent, forward.bound
        ));
    }
    Ok(true)
}

/// The refinement: the symbolic differentiator computes what forward-mode dual numbers
/// compute: for a drawn expression over every operation, a drawn variable and a drawn point
/// inside each operation's domain and away from its kinks, the value of `d(e)/dx` built by the
/// rule table equals the tangent dual arithmetic carries through `e`, within `TOLERANCE` of
/// the sum of the tangent's terms (exactly where there are none); and central finite
/// differences, a third method, do not contradict it. (The rule table itself is proved over
/// the reals in Lean, `BlazeDiff.diff_correct`.)
#[test]
fn the_symbolic_derivative_is_the_dual_tangent() {
    blaze_sim::check(blaze_sim::Config::DEFAULT, |tape| {
        let mut graph = Graph::new(Width::F64);
        let f = float_term(tape, &mut graph, 2, 3, true);
        // Coordinates in [0.3, 1.7], in steps of 1/1000.
        let coordinate = |tape: &blaze_sim::Tape| 0.3 + f64::from(pick(tape, 1400, "coordinate")) / 1000.0;
        let point = [coordinate(tape), coordinate(tape)];
        let var = pick(tape, 1, "by var");
        let policy = Policy { kinks: Choice::Subgradient(0.5) };
        let df = diff::derivative(&mut graph, f, var, policy).expect("a subgradient is chosen");
        if let Err(message) = tangent_agrees(&graph, f, df, var, &point) {
            panic!("{message}");
        }
        if let Err(message) = agrees(&graph, f, df, var as usize, &point) {
            panic!("{message}");
        }
    });
}

/// `term`'s derivative by `x` at `point` as `differentiator` takes it, judged by the dual
/// oracle.
fn judged(
    differentiator: &Differentiator<'_>,
    term: impl Fn(&mut Graph, Id) -> Id,
    point: f64,
) -> (Graph, Id, Id, Result<bool, String>) {
    let mut graph = Graph::new(Width::F64);
    let x = graph.var(0);
    let f = term(&mut graph, x);
    let df = differentiator.derivatives(&mut graph, &[f], 0).expect("no kink in the term")[0];
    let verdict = tangent_agrees(&graph, f, df, 0, &[point]);
    (graph, f, df, verdict)
}

/// A control passes when the oracle accepts the right derivative of the term and refuses the
/// planted one, at the same point.
#[track_caller]
fn caught(planted: &Differentiator<'_>, term: impl Fn(&mut Graph, Id) -> Id + Copy, point: f64) {
    let right = Differentiator::new(Policy::default());
    assert_eq!(judged(&right, term, point).3, Ok(true), "the oracle refuses the right derivative");
    let verdict = judged(planted, term, point).3;
    assert!(verdict.is_err(), "the oracle let the planted bug through: {verdict:?}");
}

fn with_rules(rules: &[diff::Rule]) -> Differentiator<'_> {
    Differentiator { rules, policy: Policy::default(), planted: Planted::None }
}

#[test]
fn the_oracle_catches_a_sign_flipped_in_one_rule() {
    // cos(u)' = u' * sin(u): the minus is gone.
    const WRONG: P = P::Mul(&P::T(0), &P::Call(Op::Sin, &P::A(0)));
    let rules = rules_with(Op::Cos, WRONG);
    // cos(x * x)
    let term = |graph: &mut Graph, x: Id| {
        let xx = graph.binary(Op::Mul, x, x);
        graph.unary(Op::Cos, xx)
    };
    caught(&with_rules(&rules), term, 0.7);
}

#[test]
fn the_oracle_catches_a_chain_rule_factor_dropped() {
    // exp(u)' = exp(u), without u'.
    const WRONG: P = P::Out;
    let rules = rules_with(Op::Exp, WRONG);
    // exp(3 * x)
    let term = |graph: &mut Graph, x: Id| {
        let three = graph.constant(3.0);
        let inner = graph.binary(Op::Mul, three, x);
        graph.unary(Op::Exp, inner)
    };
    caught(&with_rules(&rules), term, 0.3);
}

#[test]
fn the_oracle_catches_a_shared_nodes_second_use_dropped() {
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
fn the_oracle_catches_an_error_finite_differences_let_through() {
    // sin(u)' = u' * cos(u) * 1.000001: one part in a million, inside the finite-difference
    // oracle's tolerance and far outside this one's.
    const WRONG: P = P::Mul(&P::Mul(&P::T(0), &P::Call(Op::Cos, &P::A(0))), &P::Lit(1.000_001));
    let rules = rules_with(Op::Sin, WRONG);
    // sin(x * x)
    let term = |graph: &mut Graph, x: Id| {
        let xx = graph.binary(Op::Mul, x, x);
        graph.unary(Op::Sin, xx)
    };
    let planted = with_rules(&rules);
    caught(&planted, term, 0.7);
    let (graph, f, df, _) = judged(&planted, term, 0.7);
    assert_eq!(agrees(&graph, f, df, 0, &[0.7]), Ok(true), "finite differences see it after all");
}

#[test]
fn a_dual_has_the_value_evaluation_gives_and_a_tangent_by_the_asked_variable() {
    // x * y + sin(x), at (0.5, 3)
    let mut graph = Graph::new(Width::F64);
    let (x, y) = (graph.var(0), graph.var(1));
    let xy = graph.binary(Op::Mul, x, y);
    let sin = graph.unary(Op::Sin, x);
    let f = graph.binary(Op::Add, xy, sin);
    let point = [0.5, 3.0];
    let by_x = dual::<f64>(&graph, &[f], &point, 0).unwrap()[0];
    let by_y = dual::<f64>(&graph, &[f], &point, 1).unwrap()[0];
    let value = eval_one::<f64>(&graph, f, &point).unwrap();
    assert_eq!((by_x.value.to_bits(), by_y.value.to_bits()), (value.to_bits(), value.to_bits()));
    assert_eq!(by_x.tangent, 3.0 + 0.5f64.cos());
    assert_eq!(by_y.tangent, 0.5);
    assert!(by_x.bound >= by_x.tangent.abs() && by_y.bound >= by_y.tangent.abs());
    // No derivative at a kink the variable reaches; none needed where it does not.
    let abs = graph.unary(Op::Abs, x);
    assert!(dual::<f64>(&graph, &[abs], &[0.0, 1.0], 0).unwrap()[0].tangent.is_nan());
    assert_eq!(dual::<f64>(&graph, &[abs], &[0.0, 1.0], 1).unwrap()[0].tangent, 0.0);
}
