//! Forward-mode differentiation by dual numbers: a value and its derivative by one variable,
//! carried through the same [`Graph`] in one pass.
//!
//! This is the oracle the symbolic differentiator is held to (`tests/dual.rs`), so it must not
//! share the thing it checks: it does not read [`crate::diff::RULES`] and no template is
//! instantiated here. Each operation's derivative is written out below from the operation's
//! definition, and where there are two textbook forms, in the one the rule table does not use
//! (the quotient rule over `b²`, `tan' = 1 / cos²`, `tanh' = 1 / cosh²`, `powf` through
//! `exp(b ln a)`). That the derivatives are written twice in this crate is deliberate: one table
//! says what a derivative is (`rules.json`), and this is the independent second method that can
//! disagree with it. It is not a second home for the rules: nothing but the tests' oracle may
//! take a derivative from here.
//!
//! Beside the tangent it carries a [`Dual::bound`]: the same sum with every term's absolute
//! value, the scale rounding errors in a tangent are relative to (two correct ways of computing
//! a derivative agree to a few units in the last place of this, not of the tangent, which may
//! be a small difference of large terms).

use crate::{
    eval::{EvalError, Scalar, Value, apply},
    expr::{Graph, Id, Op, Sort, payload_of},
};

/// A float node's value, its derivative by the variable, and the scale of the derivative's
/// terms.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Dual<F> {
    pub value: F,
    /// The derivative. NaN at a point without one: `abs` at 0 of an argument that moves, a tie
    /// of `min` or `max` whose operands move differently, and an infinite slope (`sqrt` or
    /// `ln` at 0, a pole) even under an argument that does not move, where the product is
    /// `0 * inf`.
    pub tangent: F,
    /// The sum the tangent is, with each term's absolute value: at least `|tangent|`.
    pub bound: F,
}

fn lit<F: Scalar>(value: f64) -> F {
    F::from_payload(payload_of(F::WIDTH, value))
}

/// The tangent and bound of a function of one argument whose derivative at the argument is
/// `slope`, with `slope_bound` the sum of the absolute values of the terms `slope` is computed
/// from (`|slope|` when it is one term).
fn chain<F: Scalar>(a: Dual<F>, slope: F, slope_bound: F) -> (F, F) {
    (a.tangent * slope, a.bound * slope_bound)
}

/// The tangent and bound of one float node from its arguments' duals and its own `value`.
fn tangent<F: Scalar>(op: Op, exponent: i32, args: &[Dual<F>; 3], value: F) -> (F, F) {
    let zero = lit::<F>(0.0);
    let one = lit::<F>(1.0);
    let two = lit::<F>(2.0);
    let nan = F::from_payload(super::nan_payload(F::WIDTH));
    let [a, b, c] = *args;
    match op {
        Op::Add => (a.tangent + b.tangent, a.bound + b.bound),
        Op::Sub => (a.tangent - b.tangent, a.bound + b.bound),
        Op::Mul => (
            a.tangent * b.value + a.value * b.tangent,
            a.bound * b.value.abs() + a.value.abs() * b.bound,
        ),
        // (a / b)' = (a' b - a b') / b²
        Op::Div => {
            let square = b.value * b.value;
            (
                (a.tangent * b.value - a.value * b.tangent) / square,
                (a.bound * b.value.abs() + a.value.abs() * b.bound) / square,
            )
        }
        Op::Neg => (-a.tangent, a.bound),
        // (a^n)' = n a^(n - 1) a'; one below `i32::MIN` is no `i32`, and a^(n - 1) is a^n / a.
        Op::Powi => {
            let down = match exponent.checked_sub(1) {
                Some(down) => a.value.powi(down),
                None => value / a.value,
            };
            let slope = lit::<F>(f64::from(exponent)) * down;
            chain(a, slope, slope.abs())
        }
        // a^b = exp(b ln a), so (a^b)' = a^b (b' ln a + b a' / a). An exponent that does not
        // move leaves the power rule, which needs no logarithm (and so no positive base).
        Op::Powf => {
            if b.tangent == zero {
                let slope = b.value * a.value.powf(b.value - one);
                chain(a, slope, slope.abs())
            } else {
                let ln = a.value.ln();
                (
                    value * (b.tangent * ln + b.value * a.tangent / a.value),
                    value.abs() * (b.bound * ln.abs() + (b.value * a.bound / a.value).abs()),
                )
            }
        }
        Op::Sqrt => {
            let slope = one / (two * value);
            chain(a, slope, slope.abs())
        }
        Op::Exp => chain(a, value, value.abs()),
        Op::Ln => {
            let slope = one / a.value;
            chain(a, slope, slope.abs())
        }
        Op::Sin => {
            let slope = a.value.cos();
            chain(a, slope, slope.abs())
        }
        Op::Cos => {
            let slope = -a.value.sin();
            chain(a, slope, slope.abs())
        }
        // tan' = 1 / cos²
        Op::Tan => {
            let cos = a.value.cos();
            let slope = one / (cos * cos);
            chain(a, slope, slope)
        }
        // tanh' = 1 / cosh², with cosh from `exp`. The other form, 1 - tanh², is a difference
        // that cancels as tanh nears 1: the bound is that form's terms, 1 + tanh².
        Op::Tanh => {
            let cosh = (a.value.exp() + (-a.value).exp()) / two;
            chain(a, one / (cosh * cosh), one + value * value)
        }
        Op::Abs => {
            if a.value > zero {
                (a.tangent, a.bound)
            } else if a.value < zero {
                (-a.tangent, a.bound)
            } else if a.tangent == zero {
                // The argument does not move: neither does its absolute value, kink or not.
                (zero, a.bound)
            } else {
                (nan, a.bound)
            }
        }
        Op::Min | Op::Max => {
            let first = if op == Op::Min { a.value < b.value } else { a.value > b.value };
            let second = if op == Op::Min { b.value < a.value } else { b.value > a.value };
            if first {
                (a.tangent, a.bound)
            } else if second {
                (b.tangent, b.bound)
            } else if a.tangent == b.tangent {
                // A tie of operands that move alike: either one's derivative.
                (a.tangent, a.bound + b.bound)
            } else {
                (nan, a.bound + b.bound)
            }
        }
        Op::MulAdd => (
            a.tangent * b.value + a.value * b.tangent + c.tangent,
            a.bound * b.value.abs() + a.value.abs() * b.bound + c.bound,
        ),
        // Handled by the caller (a leaf, a condition, or a choice of an argument's dual).
        Op::Const
        | Op::Var
        | Op::Bool
        | Op::Lt
        | Op::Le
        | Op::Gt
        | Op::Ge
        | Op::Eq
        | Op::Ne
        | Op::And
        | Op::Or
        | Op::Not
        | Op::Select => (zero, zero),
    }
}

/// The dual of every float node `roots` reach at `point`, by variable `var`, by id; `None` for
/// a node they do not reach and for a condition. A node's value is [`apply`]'s, the same bits
/// [`crate::eval::eval_all`] gives. A `select` has its taken arm's dual: at the boundary of its
/// condition that is not a derivative, and nothing here says so.
pub fn dual_all<F: Scalar>(
    graph: &Graph,
    roots: &[Id],
    point: &[F],
    var: u32,
) -> Result<Vec<Option<Dual<F>>>, EvalError> {
    if graph.width() != F::WIDTH {
        return Err(EvalError::Width { graph: graph.width() });
    }
    let zero = lit::<F>(0.0);
    let still = |value: F| Dual { value, tangent: zero, bound: zero };
    let reached = graph.reached(roots);
    // alloc: one value and one dual per node, the caller's result and this call's scratch
    let mut values: Vec<Option<Value<F>>> = vec![None; graph.len()];
    let mut duals: Vec<Option<Dual<F>>> = vec![None; graph.len()];
    for (at, node) in graph.nodes().iter().enumerate() {
        if !reached[at] {
            continue;
        }
        let mut args = [Value::Bool(false); 3];
        let mut of_args = [still(zero); 3];
        for (slot, arg) in node.args().iter().enumerate() {
            args[slot] = values[arg.index()].expect("an argument's id is below its user's");
            if let Some(dual) = duals[arg.index()] {
                of_args[slot] = dual;
            }
        }
        let value = match node.op {
            Op::Var => {
                let index = node.payload as u32;
                match point.get(index as usize) {
                    Some(&it) => Value::Float(it),
                    None => return Err(EvalError::NoSuchVar { index, given: point.len() }),
                }
            }
            _ => apply::<F>(node, &args[..node.op.arity()]).expect("not a variable"),
        };
        values[at] = Some(value);
        let Value::Float(value) = value else { continue };
        duals[at] = Some(match node.op {
            Op::Const => still(value),
            Op::Var => {
                let moves = node.payload == u64::from(var);
                let one = lit::<F>(1.0);
                Dual { value, tangent: if moves { one } else { zero }, bound: if moves { one } else { zero } }
            }
            Op::Select => {
                let taken = if args[0].bool() { of_args[1] } else { of_args[2] };
                Dual { value, ..taken }
            }
            op => {
                let (tangent, bound) = tangent(op, node.exponent(), &of_args, value);
                Dual { value, tangent, bound }
            }
        });
    }
    Ok(duals)
}

/// The duals of the float `roots` at `point` by variable `var`, in their order.
pub fn dual<F: Scalar>(
    graph: &Graph,
    roots: &[Id],
    point: &[F],
    var: u32,
) -> Result<Vec<Dual<F>>, EvalError> {
    let duals = dual_all(graph, roots, point, var)?;
    // alloc: one dual per root, the caller's result
    let mut out = Vec::with_capacity(roots.len());
    for &root in roots {
        if graph.sort(root) != Sort::Float {
            return Err(EvalError::NotFloat { root });
        }
        out.push(duals[root.index()].expect("a float root is reached"));
    }
    Ok(out)
}
