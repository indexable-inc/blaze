//! Evaluating an expression at a point, in `f32` or `f64`: what each [`Op`] means.
//!
//! [`apply`] is the one place an operation's value is defined: Rust's operator or the float
//! type's method of the same name, on the host. The evaluator, the simplifier's constant folding
//! ([`fold`]) and the tests all go through it, so they cannot disagree about an operation.

use crate::expr::{Graph, Id, Node, Op, Sort, Width};

mod sealed {
    pub trait Sealed {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}
}

/// `f32` or `f64`: the type a graph of its [`Width`] is evaluated in.
pub trait Scalar:
    sealed::Sealed
    + Copy
    + PartialOrd
    + std::fmt::Debug
    + std::ops::Add<Output = Self>
    + std::ops::Sub<Output = Self>
    + std::ops::Mul<Output = Self>
    + std::ops::Div<Output = Self>
    + std::ops::Neg<Output = Self>
{
    const WIDTH: Width;
    /// The value of a constant's payload.
    fn from_payload(bits: u64) -> Self;
    /// The payload of a constant of this value.
    fn to_payload(self) -> u64;
    fn is_nan(self) -> bool;
    fn powi(self, n: i32) -> Self;
    fn powf(self, e: Self) -> Self;
    fn sqrt(self) -> Self;
    fn exp(self) -> Self;
    fn ln(self) -> Self;
    fn sin(self) -> Self;
    fn cos(self) -> Self;
    fn tan(self) -> Self;
    fn tanh(self) -> Self;
    fn abs(self) -> Self;
    fn min(self, other: Self) -> Self;
    fn max(self, other: Self) -> Self;
    fn mul_add(self, a: Self, b: Self) -> Self;
}

macro_rules! scalar {
    ($ty:ident, $bits:ident, $width:expr) => {
        impl Scalar for $ty {
            const WIDTH: Width = $width;
            #[inline]
            fn from_payload(bits: u64) -> Self {
                $ty::from_bits(bits as $bits)
            }
            #[inline]
            fn to_payload(self) -> u64 {
                u64::from(self.to_bits())
            }
            #[inline]
            fn is_nan(self) -> bool {
                $ty::is_nan(self)
            }
            #[inline]
            fn powi(self, n: i32) -> Self {
                $ty::powi(self, n)
            }
            #[inline]
            fn powf(self, e: Self) -> Self {
                $ty::powf(self, e)
            }
            #[inline]
            fn sqrt(self) -> Self {
                $ty::sqrt(self)
            }
            #[inline]
            fn exp(self) -> Self {
                $ty::exp(self)
            }
            #[inline]
            fn ln(self) -> Self {
                $ty::ln(self)
            }
            #[inline]
            fn sin(self) -> Self {
                $ty::sin(self)
            }
            #[inline]
            fn cos(self) -> Self {
                $ty::cos(self)
            }
            #[inline]
            fn tan(self) -> Self {
                $ty::tan(self)
            }
            #[inline]
            fn tanh(self) -> Self {
                $ty::tanh(self)
            }
            #[inline]
            fn abs(self) -> Self {
                $ty::abs(self)
            }
            #[inline]
            fn min(self, other: Self) -> Self {
                $ty::min(self, other)
            }
            #[inline]
            fn max(self, other: Self) -> Self {
                $ty::max(self, other)
            }
            #[inline]
            fn mul_add(self, a: Self, b: Self) -> Self {
                $ty::mul_add(self, a, b)
            }
        }
    };
}

scalar!(f32, u32, Width::F32);
scalar!(f64, u64, Width::F64);

/// A node's value: a float or a condition.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Value<F> {
    Float(F),
    Bool(bool),
}

impl<F: Scalar> Value<F> {
    /// The float this is. Panics on a condition: a graph's sorts are checked when a node is
    /// added, so an operation is never handed the wrong one.
    #[inline]
    #[track_caller]
    pub fn float(self) -> F {
        match self {
            Value::Float(it) => it,
            Value::Bool(_) => panic!("blaze-diff: a condition where a float is taken"),
        }
    }

    #[inline]
    #[track_caller]
    pub fn bool(self) -> bool {
        match self {
            Value::Bool(it) => it,
            Value::Float(_) => panic!("blaze-diff: a float where a condition is taken"),
        }
    }
}

/// The value of `node` given its arguments' values, `args[i]` for argument `i`: what the
/// operation means. A [`Op::Var`] has no value here (`None`): the caller has the point.
pub fn apply<F: Scalar>(node: &Node, args: &[Value<F>]) -> Option<Value<F>> {
    let f = |at: usize| args[at].float();
    let b = |at: usize| args[at].bool();
    Some(match node.op {
        Op::Var => return None,
        Op::Const => Value::Float(F::from_payload(node.payload)),
        Op::Bool => Value::Bool(node.payload != 0),
        Op::Add => Value::Float(f(0) + f(1)),
        Op::Sub => Value::Float(f(0) - f(1)),
        Op::Mul => Value::Float(f(0) * f(1)),
        Op::Div => Value::Float(f(0) / f(1)),
        Op::Neg => Value::Float(-f(0)),
        Op::Powi => Value::Float(f(0).powi(node.exponent())),
        Op::Powf => Value::Float(f(0).powf(f(1))),
        Op::Sqrt => Value::Float(f(0).sqrt()),
        Op::Exp => Value::Float(f(0).exp()),
        Op::Ln => Value::Float(f(0).ln()),
        Op::Sin => Value::Float(f(0).sin()),
        Op::Cos => Value::Float(f(0).cos()),
        Op::Tan => Value::Float(f(0).tan()),
        Op::Tanh => Value::Float(f(0).tanh()),
        Op::Abs => Value::Float(f(0).abs()),
        Op::Min => Value::Float(f(0).min(f(1))),
        Op::Max => Value::Float(f(0).max(f(1))),
        Op::MulAdd => Value::Float(f(0).mul_add(f(1), f(2))),
        Op::Lt => Value::Bool(f(0) < f(1)),
        Op::Le => Value::Bool(f(0) <= f(1)),
        Op::Gt => Value::Bool(f(0) > f(1)),
        Op::Ge => Value::Bool(f(0) >= f(1)),
        Op::Eq => Value::Bool(f(0) == f(1)),
        Op::Ne => Value::Bool(f(0) != f(1)),
        Op::And => Value::Bool(b(0) & b(1)),
        Op::Or => Value::Bool(b(0) | b(1)),
        Op::Not => Value::Bool(!b(0)),
        Op::Select => {
            if b(0) {
                args[1]
            } else {
                args[2]
            }
        }
    })
}

/// A constant the simplifier knows a term to be.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Konst {
    /// A float, by its bits (a constant node's payload).
    Float(u64),
    Bool(bool),
}

/// Whether `op` of constants is the same constant on every machine: IEEE 754 rounds it
/// correctly (`+ - * /`, `sqrt`, `mul_add`), or it is exact (`neg`, `abs`, comparisons, the
/// conditions). The transcendental functions and `powi` are whatever the target's math library
/// gives, so folding one on the host could change a program; `min` and `max` of two zeros of
/// different sign are unspecified.
pub const fn folds_exactly(op: Op) -> bool {
    matches!(
        op,
        Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Neg
            | Op::Sqrt
            | Op::Abs
            | Op::MulAdd
            | Op::Lt
            | Op::Le
            | Op::Gt
            | Op::Ge
            | Op::Eq
            | Op::Ne
            | Op::And
            | Op::Or
            | Op::Not
    )
}

/// The constant `node` is when its arguments are the constants `args`, for the operations
/// [`folds_exactly`] names; `None` for the others. A NaN result is the width's one canonical
/// NaN: Rust leaves a NaN's sign and payload unspecified, so every NaN is the same value here.
pub fn fold(width: Width, node: &Node, args: &[Konst]) -> Option<Konst> {
    if !folds_exactly(node.op) {
        return None;
    }
    fn go<F: Scalar>(node: &Node, args: &[Konst]) -> Option<Konst> {
        let mut values = [Value::Bool(false); 3];
        for (at, arg) in args.iter().enumerate() {
            values[at] = match *arg {
                Konst::Float(bits) => Value::Float(F::from_payload(bits)),
                Konst::Bool(it) => Value::Bool(it),
            };
        }
        Some(match apply::<F>(node, &values[..args.len()])? {
            Value::Float(it) => Konst::Float(canonical(it).to_payload()),
            Value::Bool(it) => Konst::Bool(it),
        })
    }
    match width {
        Width::F32 => go::<f32>(node, args),
        Width::F64 => go::<f64>(node, args),
    }
}

/// `value`, a NaN as the width's canonical one.
#[inline]
pub fn canonical<F: Scalar>(value: F) -> F {
    if value.is_nan() { F::from_payload(nan_payload(F::WIDTH)) } else { value }
}

/// The payload of the width's canonical NaN.
pub const fn nan_payload(width: Width) -> u64 {
    match width {
        Width::F32 => f32::NAN.to_bits() as u64,
        Width::F64 => f64::NAN.to_bits(),
    }
}

/// An expression could not be evaluated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EvalError {
    /// The graph is over `graph`, and the point is of the other width.
    Width { graph: Width },
    /// The expression names variable `index`, and the point has `given` coordinates.
    NoSuchVar { index: u32, given: usize },
    /// The root `root` is a condition, not a float.
    NotFloat { root: Id },
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalError::Width { graph } => {
                write!(f, "the expression is over {}, and the point is not", graph.name())
            }
            EvalError::NoSuchVar { index, given } => {
                write!(
                    f,
                    "the expression names variable {index}, and the point has {given} coordinates"
                )
            }
            EvalError::NotFloat { root } => {
                write!(f, "node {} is a condition, and a float was asked", root.0)
            }
        }
    }
}

impl std::error::Error for EvalError {}

/// The value of every node `roots` reach at `point` (`point[i]` is variable `i`), by id; `None`
/// for a node they do not reach. Each shared node is computed once.
pub fn eval_all<F: Scalar>(
    graph: &Graph,
    roots: &[Id],
    point: &[F],
) -> Result<Vec<Option<Value<F>>>, EvalError> {
    if graph.width() != F::WIDTH {
        return Err(EvalError::Width { graph: graph.width() });
    }
    let reached = graph.reached(roots);
    // alloc: one value per node, the caller's result
    let mut values: Vec<Option<Value<F>>> = vec![None; graph.len()];
    for (at, node) in graph.nodes().iter().enumerate() {
        if !reached[at] {
            continue;
        }
        let mut args = [Value::Bool(false); 3];
        for (slot, arg) in node.args().iter().enumerate() {
            args[slot] = values[arg.index()].expect("an argument's id is below its user's");
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
    }
    Ok(values)
}

/// The values of the float `roots` at `point`, in their order.
pub fn eval<F: Scalar>(graph: &Graph, roots: &[Id], point: &[F]) -> Result<Vec<F>, EvalError> {
    let values = eval_all(graph, roots, point)?;
    // alloc: one value per root, the caller's result
    let mut out = Vec::with_capacity(roots.len());
    for &root in roots {
        if graph.sort(root) != Sort::Float {
            return Err(EvalError::NotFloat { root });
        }
        out.push(values[root.index()].expect("a root is reached").float());
    }
    Ok(out)
}

/// The value of the float `root` at `point`.
pub fn eval_one<F: Scalar>(graph: &Graph, root: Id, point: &[F]) -> Result<F, EvalError> {
    Ok(eval(graph, &[root], point)?[0])
}

/// Whether two results are the same value: the same bits, or both NaN. Rust does not specify a
/// NaN's sign or payload (an operation may give any), so no rewrite can be held to them.
#[inline]
pub fn same<F: Scalar>(a: F, b: F) -> bool {
    (a.is_nan() && b.is_nan()) || a.to_payload() == b.to_payload()
}
