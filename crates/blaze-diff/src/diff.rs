//! Symbolic derivatives: `d(expr)/d(variable)` by one rule per operation, with sharing.
//!
//! [`RULES`] is the one table of derivative rules: for each operation, its tangent (what the
//! node's derivative is, given its arguments, itself and its arguments' derivatives) as a
//! template ([`P`]), the points where it has no derivative ([`Kink`]) and the domain the rule
//! holds on ([`Domain`]). The differentiator walks the graph once in id order (arguments
//! first) and keeps each node's derivative, so a node shared by many users is differentiated
//! once and the result is a DAG the size of the input times a constant, not a tree.
//!
//! A derivative that is zero because the term does not mention the variable is never built: a
//! tangent is `Zero`, `One` or a node ([`Tan`]), and `0 * e`, `0 + e`, `1 * e` are resolved
//! while instantiating a rule. That is the definition of the derivative (the partial of a term
//! without `x` is 0), not a rewrite of float code: no value the program computes is changed.
//!
//! A point without a derivative is a typed choice ([`Choice`]): by default differentiating
//! through `abs`, `min`, `max`, `sqrt` or a `select` whose condition depends on the variable is
//! [`DiffError::NonDifferentiable`], naming the operation and the point; with
//! [`Choice::Subgradient`] the derivative there is the value given.

use crate::expr::{Graph, Id, Node, Op, Sort};

/// A template over one node: an expression of its arguments, itself and their tangents.
#[derive(Clone, Copy, Debug)]
pub enum P {
    /// Argument `i` of the node.
    A(u8),
    /// The derivative of argument `i`.
    T(u8),
    /// The node itself: `exp`'s derivative is `exp`, shared.
    Out,
    Lit(f64),
    /// The derivative chosen at the rule's kink ([`Choice::Subgradient`]).
    Choice,
    /// A `powi` node's exponent, as a float.
    Exponent,
    /// A `powi` node's argument to the exponent less one.
    PowiDown,
    Add(&'static P, &'static P),
    Sub(&'static P, &'static P),
    Mul(&'static P, &'static P),
    Div(&'static P, &'static P),
    Neg(&'static P),
    /// A function of one argument: `sin`, `cos`, `ln`.
    Call(Op, &'static P),
    Powf(&'static P, &'static P),
    Select(C, &'static P, &'static P),
}

/// A condition in a template.
#[derive(Clone, Copy, Debug)]
pub enum C {
    /// The node's own condition: a `select`'s first argument.
    Cond,
    Lt(&'static P, &'static P),
    Gt(&'static P, &'static P),
    Eq(&'static P, &'static P),
}

/// Where a rule's operation has no derivative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kink {
    /// The point, as the error names it: "0", "a tie of its operands".
    pub point: &'static str,
    pub at: KinkAt,
}

/// When a kink is on the way of the variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KinkAt {
    /// Whenever the node's value depends on the variable.
    Value,
    /// When the node's condition depends on the variable: a `select` whose condition does not
    /// is differentiable arm by arm.
    Condition,
}

/// Where a rule holds: the operation is differentiable there and the tangent is its
/// derivative, in real arithmetic. In IEEE 754 the tangent then agrees with the derivative up
/// to rounding wherever the values involved are finite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Domain {
    Everywhere,
    /// The first argument is above 0: `ln`, `sqrt`, `powf`'s base.
    ArgPositive,
    /// The first argument is not 0: `abs`.
    ArgNonZero,
    /// The second argument is not 0: `/`.
    DivisorNonZero,
    /// The two arguments differ: `min`, `max`.
    ArgsDiffer,
    /// `cos` of the argument is not 0: `tan`.
    AwayFromPoles,
    /// A `powi`: everywhere for an exponent of 1 or more, the argument not 0 below that.
    PowiBase,
}

impl Domain {
    /// How far `node`'s arguments `args` are from where the rule stops holding: infinite when
    /// it holds everywhere, 0 or below when it does not hold. What a test keeps its points
    /// away from.
    pub fn distance(self, node: &Node, args: &[f64]) -> f64 {
        match self {
            Domain::Everywhere => f64::INFINITY,
            Domain::ArgPositive => args[0],
            Domain::ArgNonZero => args[0].abs(),
            Domain::DivisorNonZero => args[1].abs(),
            Domain::ArgsDiffer => (args[0] - args[1]).abs(),
            Domain::AwayFromPoles => args[0].cos().abs(),
            Domain::PowiBase => {
                if node.exponent() >= 1 {
                    f64::INFINITY
                } else {
                    args[0].abs()
                }
            }
        }
    }
}

/// One operation's derivative rule.
#[derive(Clone, Copy, Debug)]
pub struct Rule {
    pub op: Op,
    /// The node's derivative.
    pub tangent: P,
    pub kink: Option<Kink>,
    pub domain: Domain,
    /// The IEEE 754 conditions the rule holds under, said for people (`blaze explain`).
    pub ieee: &'static str,
}

const A0: P = P::A(0);
const A1: P = P::A(1);
const T0: P = P::T(0);
const T1: P = P::T(1);
const T2: P = P::T(2);
const OUT: P = P::Out;
const ONE: P = P::Lit(1.0);
const TWO: P = P::Lit(2.0);
const ZERO: P = P::Lit(0.0);
const MINUS_ONE: P = P::Lit(-1.0);
/// `out * out`: `tan²`, `tanh²`.
const OUT_SQUARED: P = P::Mul(&OUT, &OUT);
/// At a tie, `choice` of the first operand's derivative and the rest of the second's.
const TIE: P = P::Add(&P::Mul(&P::Choice, &T0), &P::Mul(&P::Sub(&ONE, &P::Choice), &T1));

const FINITE: &str = "finite operands and a finite result";

/// The derivative rules: one per float operation, in [`Op`]'s order. The one owner: the
/// differentiator, the tests' generator and `blaze explain` read this table and nothing else
/// says what a derivative is.
pub const RULES: &[Rule] = &[
    Rule {
        op: Op::Add,
        tangent: P::Add(&T0, &T1),
        kink: None,
        domain: Domain::Everywhere,
        ieee: FINITE,
    },
    Rule {
        op: Op::Sub,
        tangent: P::Sub(&T0, &T1),
        kink: None,
        domain: Domain::Everywhere,
        ieee: FINITE,
    },
    Rule {
        op: Op::Mul,
        tangent: P::Add(&P::Mul(&T0, &A1), &P::Mul(&A0, &T1)),
        kink: None,
        domain: Domain::Everywhere,
        ieee: FINITE,
    },
    // (a / b)' = (a' - (a / b) * b') / b: the quotient itself is shared.
    Rule {
        op: Op::Div,
        tangent: P::Div(&P::Sub(&T0, &P::Mul(&OUT, &T1)), &A1),
        kink: None,
        domain: Domain::DivisorNonZero,
        ieee: "finite operands, a divisor that is not 0",
    },
    Rule {
        op: Op::Neg,
        tangent: P::Neg(&T0),
        kink: None,
        domain: Domain::Everywhere,
        ieee: FINITE,
    },
    Rule {
        op: Op::Powi,
        tangent: P::Mul(&T0, &P::Mul(&P::Exponent, &P::PowiDown)),
        kink: None,
        domain: Domain::PowiBase,
        ieee: "a finite base, not 0 when the exponent is below 1",
    },
    // The exponent's term is dropped when the exponent does not depend on the variable, so a
    // constant exponent needs no positive base.
    Rule {
        op: Op::Powf,
        tangent: P::Add(
            &P::Mul(&T0, &P::Mul(&A1, &P::Powf(&A0, &P::Sub(&A1, &ONE)))),
            &P::Mul(&T1, &P::Mul(&OUT, &P::Call(Op::Ln, &A0))),
        ),
        kink: None,
        domain: Domain::ArgPositive,
        ieee: "a finite base above 0 (any base that `powf` takes when the exponent is constant)",
    },
    Rule {
        op: Op::Sqrt,
        tangent: P::Select(
            C::Eq(&A0, &ZERO),
            &P::Mul(&P::Choice, &T0),
            &P::Div(&T0, &P::Mul(&TWO, &OUT)),
        ),
        kink: Some(Kink { point: "0", at: KinkAt::Value }),
        domain: Domain::ArgPositive,
        ieee: "a finite argument above 0",
    },
    Rule {
        op: Op::Exp,
        tangent: P::Mul(&T0, &OUT),
        kink: None,
        domain: Domain::Everywhere,
        ieee: FINITE,
    },
    Rule {
        op: Op::Ln,
        tangent: P::Div(&T0, &A0),
        kink: None,
        domain: Domain::ArgPositive,
        ieee: "a finite argument above 0",
    },
    Rule {
        op: Op::Sin,
        tangent: P::Mul(&T0, &P::Call(Op::Cos, &A0)),
        kink: None,
        domain: Domain::Everywhere,
        ieee: FINITE,
    },
    Rule {
        op: Op::Cos,
        tangent: P::Neg(&P::Mul(&T0, &P::Call(Op::Sin, &A0))),
        kink: None,
        domain: Domain::Everywhere,
        ieee: FINITE,
    },
    Rule {
        op: Op::Tan,
        tangent: P::Mul(&T0, &P::Add(&ONE, &OUT_SQUARED)),
        kink: None,
        domain: Domain::AwayFromPoles,
        ieee: "a finite argument away from the poles of `tan`",
    },
    Rule {
        op: Op::Tanh,
        tangent: P::Mul(&T0, &P::Sub(&ONE, &OUT_SQUARED)),
        kink: None,
        domain: Domain::Everywhere,
        ieee: FINITE,
    },
    Rule {
        op: Op::Abs,
        tangent: P::Mul(
            &T0,
            &P::Select(
                C::Gt(&A0, &ZERO),
                &ONE,
                &P::Select(C::Lt(&A0, &ZERO), &MINUS_ONE, &P::Choice),
            ),
        ),
        kink: Some(Kink { point: "0", at: KinkAt::Value }),
        domain: Domain::ArgNonZero,
        ieee: "a finite argument that is not 0 or NaN",
    },
    Rule {
        op: Op::Min,
        tangent: P::Select(C::Lt(&A0, &A1), &T0, &P::Select(C::Lt(&A1, &A0), &T1, &TIE)),
        kink: Some(Kink { point: "a tie of its operands", at: KinkAt::Value }),
        domain: Domain::ArgsDiffer,
        ieee: "finite operands that differ, neither NaN",
    },
    Rule {
        op: Op::Max,
        tangent: P::Select(C::Gt(&A0, &A1), &T0, &P::Select(C::Gt(&A1, &A0), &T1, &TIE)),
        kink: Some(Kink { point: "a tie of its operands", at: KinkAt::Value }),
        domain: Domain::ArgsDiffer,
        ieee: "finite operands that differ, neither NaN",
    },
    Rule {
        op: Op::MulAdd,
        tangent: P::Add(&P::Add(&P::Mul(&T0, &A1), &P::Mul(&A0, &T1)), &T2),
        kink: None,
        domain: Domain::Everywhere,
        ieee: FINITE,
    },
    // The taken arm's derivative: never a product with an untaken arm's, which may be NaN.
    Rule {
        op: Op::Select,
        tangent: P::Select(C::Cond, &T1, &T2),
        kink: Some(Kink { point: "the boundary of its condition", at: KinkAt::Condition }),
        domain: Domain::Everywhere,
        ieee: "a condition that does not change near the point",
    },
];

/// The rule of `op` in `rules`.
pub fn rule_of(rules: &[Rule], op: Op) -> Option<&Rule> {
    rules.iter().find(|it| it.op == op)
}

/// What to do at a point without a derivative.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum Choice {
    /// Refuse: [`DiffError::NonDifferentiable`] names the operation and the point.
    #[default]
    Error,
    /// The derivative there is this value: for `abs` at 0 a number in `[-1, 1]`; for `min` and
    /// `max` at a tie the share of the first operand's derivative, in `[0, 1]`; for `sqrt` at
    /// 0 the factor of the argument's derivative; for a `select` the taken arm's derivative,
    /// whatever the value.
    Subgradient(f64),
}

/// How a derivative is taken.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Policy {
    pub kinks: Choice,
}

/// A derivative could not be taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffError {
    /// `op` (node `node`) has no derivative at `point`, the variable reaches it, and the policy
    /// is [`Choice::Error`].
    NonDifferentiable { op: Op, point: &'static str, node: Id },
    /// The rule table has no rule for `op`.
    NoRule { op: Op },
    /// The root `root` is a condition: only a float has a derivative.
    NotFloat { root: Id },
}

impl std::fmt::Display for DiffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiffError::NonDifferentiable { op, point, node } => write!(
                f,
                "`{op}` (node {}) has no derivative at {point}, and the variable reaches it: \
                 choose the derivative there with `Choice::Subgradient(value)`",
                node.0
            ),
            DiffError::NoRule { op } => {
                write!(f, "the rule table has no derivative rule for `{op}`")
            }
            DiffError::NotFloat { root } => {
                write!(f, "node {} is a condition, which has no derivative", root.0)
            }
        }
    }
}

impl std::error::Error for DiffError {}

/// A node's derivative: structurally zero, structurally one, or a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tan {
    Zero,
    One,
    Node(Id),
}

/// A bug planted in the differentiator, for the controls of the tests' oracle: each is a
/// mistake a differentiator can make that no rule table shows.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Planted {
    #[default]
    None,
    /// A shared node's derivative reaches only its first user.
    DropsSecondUse,
}

/// Takes derivatives by a rule table under a policy.
#[derive(Clone, Copy, Debug)]
pub struct Differentiator<'r> {
    pub rules: &'r [Rule],
    pub policy: Policy,
    #[doc(hidden)]
    pub planted: Planted,
}

impl Differentiator<'static> {
    /// By [`RULES`].
    pub fn new(policy: Policy) -> Differentiator<'static> {
        Differentiator { rules: RULES, policy, planted: Planted::None }
    }
}

struct Cx {
    id: Id,
    node: Node,
    tangents: [Tan; 3],
    choice: f64,
}

impl Differentiator<'_> {
    /// The derivatives of the float `roots` by variable `var`, added to `graph`, in the order
    /// of `roots`. Nodes the roots share are differentiated once.
    pub fn derivatives(
        &self,
        graph: &mut Graph,
        roots: &[Id],
        var: u32,
    ) -> Result<Vec<Id>, DiffError> {
        for &root in roots {
            if graph.sort(root) != Sort::Float {
                return Err(DiffError::NotFloat { root });
            }
        }
        let count = graph.len();
        let reached = graph.reached(roots);
        // alloc: one tangent and two flags per node, scratch of this call
        let mut tangents = vec![Tan::Zero; count];
        let mut depends = vec![false; count];
        let mut used = vec![false; count];
        for at in 0..count {
            if !reached[at] {
                continue;
            }
            let id = Id(at as u32);
            let node = graph.node(id);
            depends[at] = node.args().iter().any(|arg| depends[arg.index()]);
            if node.op.sort() == Sort::Bool {
                continue;
            }
            match node.op {
                Op::Const => continue,
                Op::Var => {
                    if node.payload == u64::from(var) {
                        depends[at] = true;
                        tangents[at] = Tan::One;
                    }
                    continue;
                }
                _ => {}
            }
            let mut of_args = [Tan::Zero; 3];
            for (slot, arg) in node.args().iter().enumerate() {
                if graph.sort(*arg) != Sort::Float {
                    continue;
                }
                let first_use = !std::mem::replace(&mut used[arg.index()], true);
                if self.planted == Planted::DropsSecondUse && !first_use {
                    continue;
                }
                of_args[slot] = tangents[arg.index()];
            }
            let rule = rule_of(self.rules, node.op).ok_or(DiffError::NoRule { op: node.op })?;
            let on_condition = node.op == Op::Select && depends[node.args[0].index()];
            let moves = of_args.iter().any(|it| *it != Tan::Zero);
            if !moves && !on_condition {
                continue;
            }
            let mut choice = 0.0;
            if let Some(kink) = rule.kink {
                let on_the_way = match kink.at {
                    KinkAt::Value => moves,
                    KinkAt::Condition => on_condition,
                };
                if on_the_way {
                    match self.policy.kinks {
                        Choice::Error => {
                            return Err(DiffError::NonDifferentiable {
                                op: node.op,
                                point: kink.point,
                                node: id,
                            });
                        }
                        Choice::Subgradient(value) => choice = value,
                    }
                }
            }
            let cx = Cx { id, node, tangents: of_args, choice };
            tangents[at] = instantiate(graph, &rule.tangent, &cx);
        }
        // alloc: one id per root, the caller's result
        let mut out = Vec::with_capacity(roots.len());
        for root in roots {
            out.push(materialize(graph, tangents[root.index()]));
        }
        Ok(out)
    }
}

fn literal(graph: &mut Graph, value: f64) -> Tan {
    if value == 0.0 {
        Tan::Zero
    } else if value == 1.0 {
        Tan::One
    } else {
        Tan::Node(graph.constant(value))
    }
}

fn materialize(graph: &mut Graph, tangent: Tan) -> Id {
    match tangent {
        Tan::Zero => graph.constant(0.0),
        Tan::One => graph.constant(1.0),
        Tan::Node(id) => id,
    }
}

fn instantiate(graph: &mut Graph, template: &P, cx: &Cx) -> Tan {
    match *template {
        P::A(at) => Tan::Node(cx.node.args[usize::from(at)]),
        P::T(at) => cx.tangents[usize::from(at)],
        P::Out => Tan::Node(cx.id),
        P::Lit(value) => literal(graph, value),
        P::Choice => literal(graph, cx.choice),
        P::Exponent => literal(graph, f64::from(cx.node.exponent())),
        P::PowiDown => match cx.node.exponent().wrapping_sub(1) {
            0 => Tan::One,
            1 => Tan::Node(cx.node.args[0]),
            down => Tan::Node(graph.powi(cx.node.args[0], down)),
        },
        P::Add(a, b) => {
            let (a, b) = (instantiate(graph, a, cx), instantiate(graph, b, cx));
            match (a, b) {
                (Tan::Zero, other) | (other, Tan::Zero) => other,
                _ => binary(graph, Op::Add, a, b),
            }
        }
        P::Sub(a, b) => {
            let (a, b) = (instantiate(graph, a, cx), instantiate(graph, b, cx));
            match (a, b) {
                (other, Tan::Zero) => other,
                (Tan::Zero, other) => negate(graph, other),
                _ => binary(graph, Op::Sub, a, b),
            }
        }
        P::Mul(a, b) => {
            let (a, b) = (instantiate(graph, a, cx), instantiate(graph, b, cx));
            match (a, b) {
                (Tan::Zero, _) | (_, Tan::Zero) => Tan::Zero,
                (Tan::One, other) | (other, Tan::One) => other,
                _ => binary(graph, Op::Mul, a, b),
            }
        }
        P::Div(a, b) => {
            let (a, b) = (instantiate(graph, a, cx), instantiate(graph, b, cx));
            match (a, b) {
                (Tan::Zero, _) => Tan::Zero,
                (other, Tan::One) => other,
                _ => binary(graph, Op::Div, a, b),
            }
        }
        P::Neg(a) => {
            let a = instantiate(graph, a, cx);
            negate(graph, a)
        }
        P::Call(op, a) => {
            let a = instantiate(graph, a, cx);
            let a = materialize(graph, a);
            Tan::Node(graph.unary(op, a))
        }
        P::Powf(a, b) => {
            let (a, b) = (instantiate(graph, a, cx), instantiate(graph, b, cx));
            binary(graph, Op::Powf, a, b)
        }
        P::Select(cond, a, b) => {
            let (a, b) = (instantiate(graph, a, cx), instantiate(graph, b, cx));
            if a == b {
                return a;
            }
            let cond = condition(graph, &cond, cx);
            let (a, b) = (materialize(graph, a), materialize(graph, b));
            Tan::Node(graph.select(cond, a, b))
        }
    }
}

fn condition(graph: &mut Graph, cond: &C, cx: &Cx) -> Id {
    let (op, a, b) = match *cond {
        C::Cond => return cx.node.args[0],
        C::Lt(a, b) => (Op::Lt, a, b),
        C::Gt(a, b) => (Op::Gt, a, b),
        C::Eq(a, b) => (Op::Eq, a, b),
    };
    let (a, b) = (instantiate(graph, a, cx), instantiate(graph, b, cx));
    let (a, b) = (materialize(graph, a), materialize(graph, b));
    graph.binary(op, a, b)
}

fn binary(graph: &mut Graph, op: Op, a: Tan, b: Tan) -> Tan {
    let (a, b) = (materialize(graph, a), materialize(graph, b));
    Tan::Node(graph.binary(op, a, b))
}

fn negate(graph: &mut Graph, a: Tan) -> Tan {
    match a {
        Tan::Zero => Tan::Zero,
        Tan::One => Tan::Node(graph.constant(-1.0)),
        Tan::Node(id) => Tan::Node(graph.unary(Op::Neg, id)),
    }
}

/// `d(root)/d(var)`, added to `graph`, by [`RULES`].
pub fn derivative(graph: &mut Graph, root: Id, var: u32, policy: Policy) -> Result<Id, DiffError> {
    Ok(Differentiator::new(policy).derivatives(graph, &[root], var)?[0])
}

/// The gradient of `root`: its derivative by each of `vars`, in their order.
pub fn gradient(
    graph: &mut Graph,
    root: Id,
    vars: &[u32],
    policy: Policy,
) -> Result<Vec<Id>, DiffError> {
    // alloc: one id per variable, the caller's result
    let mut out = Vec::with_capacity(vars.len());
    for &var in vars {
        out.push(derivative(graph, root, var, policy)?);
    }
    Ok(out)
}

/// The Jacobian of `roots`: row `i` is the gradient of `roots[i]` by `vars`. The roots'
/// shared nodes are differentiated once per variable.
pub fn jacobian(
    graph: &mut Graph,
    roots: &[Id],
    vars: &[u32],
    policy: Policy,
) -> Result<Vec<Vec<Id>>, DiffError> {
    // alloc: the matrix, the caller's result
    let mut rows = vec![Vec::with_capacity(vars.len()); roots.len()];
    let differentiator = Differentiator::new(policy);
    for &var in vars {
        let column = differentiator.derivatives(graph, roots, var)?;
        for (row, entry) in rows.iter_mut().zip(column) {
            row.push(entry);
        }
    }
    Ok(rows)
}

/// The Hessian of `root`: entry `[i][j]` is its second derivative by `vars[i]` then
/// `vars[j]`, the Jacobian of its gradient.
pub fn hessian(
    graph: &mut Graph,
    root: Id,
    vars: &[u32],
    policy: Policy,
) -> Result<Vec<Vec<Id>>, DiffError> {
    let gradient = gradient(graph, root, vars, policy)?;
    jacobian(graph, &gradient, vars, policy)
}
