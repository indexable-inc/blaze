//! The tests: the derivative against finite differences ([`derivative`]), simplification
//! against evaluation and the e-graph against a reference model ([`simplify`]), and printing
//! ([`print`]). What they share is here: drawing a term.

mod derivative;
mod print;
mod simplify;

use hegel::generators as gs;

use crate::expr::{Graph, Id, Op};

/// A number in `0..=max`.
fn pick(tc: &hegel::TestCase, max: u32) -> u32 {
    tc.draw(gs::integers::<u32>().max_value(max))
}

/// The plan's example, `f(x, y) = (x * y).sin() / (1.0 + x * x)`, built in its order.
fn plan_example(graph: &mut Graph) -> Id {
    let x = graph.var(0);
    let y = graph.var(1);
    let xy = graph.binary(Op::Mul, x, y);
    let sin = graph.unary(Op::Sin, xy);
    let one = graph.constant(1.0);
    let xx = graph.binary(Op::Mul, x, x);
    let denominator = graph.binary(Op::Add, one, xx);
    graph.binary(Op::Div, sin, denominator)
}

/// The constants a drawn term may hold: the ones the rewrites name, and others.
const CONSTANTS: &[f64] = &[1.0, 2.0, 0.5, -1.0, 3.0, -1.5, 0.0, -0.0];

/// A drawn float term over `vars` variables, at most `depth` operations deep, over every
/// operation. With `tame`, an operation with a pole or a domain gets an argument inside it
/// (`ln(e * e + 0.5)`), and the constants are not zeros: a term to differentiate.
fn float_term(tc: &hegel::TestCase, graph: &mut Graph, vars: u32, depth: u32, tame: bool) -> Id {
    if depth == 0 || pick(tc, 5) == 0 {
        return if pick(tc, 2) == 0 {
            let constants = if tame { &CONSTANTS[..6] } else { CONSTANTS };
            graph.constant(constants[pick(tc, constants.len() as u32 - 1) as usize])
        } else {
            graph.var(pick(tc, vars - 1))
        };
    }
    let sub = |graph: &mut Graph| float_term(tc, graph, vars, depth - 1, tame);
    // `e * e + 0.5`: above 0, and `e` is used twice.
    let positive = |graph: &mut Graph, e: Id| {
        let square = graph.binary(Op::Mul, e, e);
        let half = graph.constant(0.5);
        graph.binary(Op::Add, square, half)
    };
    match pick(tc, 19) {
        0 => {
            let (a, b) = (sub(graph), sub(graph));
            graph.binary(Op::Add, a, b)
        }
        1 => {
            let (a, b) = (sub(graph), sub(graph));
            graph.binary(Op::Sub, a, b)
        }
        2 => {
            let (a, b) = (sub(graph), sub(graph));
            graph.binary(Op::Mul, a, b)
        }
        3 => {
            let (a, b) = (sub(graph), sub(graph));
            let b = if tame { positive(graph, b) } else { b };
            graph.binary(Op::Div, a, b)
        }
        4 => {
            let a = sub(graph);
            graph.unary(Op::Neg, a)
        }
        5 => {
            let a = sub(graph);
            graph.powi(a, 2 + pick(tc, 1) as i32)
        }
        6 => {
            let (a, b) = (sub(graph), sub(graph));
            let a = if tame { positive(graph, a) } else { a };
            graph.binary(Op::Powf, a, b)
        }
        7 => {
            let a = sub(graph);
            let a = if tame { positive(graph, a) } else { a };
            graph.unary(Op::Sqrt, a)
        }
        8 => {
            let a = sub(graph);
            graph.unary(Op::Exp, a)
        }
        9 => {
            let a = sub(graph);
            let a = if tame { positive(graph, a) } else { a };
            graph.unary(Op::Ln, a)
        }
        10 => {
            let a = sub(graph);
            graph.unary(Op::Sin, a)
        }
        11 => {
            let a = sub(graph);
            graph.unary(Op::Cos, a)
        }
        12 => {
            let a = sub(graph);
            // `tanh` is within (-1, 1), away from the poles of `tan`.
            let a = if tame { graph.unary(Op::Tanh, a) } else { a };
            graph.unary(Op::Tan, a)
        }
        13 => {
            let a = sub(graph);
            graph.unary(Op::Tanh, a)
        }
        14 => {
            let a = sub(graph);
            graph.unary(Op::Abs, a)
        }
        15 => {
            let (a, b) = (sub(graph), sub(graph));
            graph.binary(Op::Min, a, b)
        }
        16 => {
            let (a, b) = (sub(graph), sub(graph));
            graph.binary(Op::Max, a, b)
        }
        17 => {
            let (a, b, c) = (sub(graph), sub(graph), sub(graph));
            graph.mul_add(a, b, c)
        }
        _ => {
            let (l, r) = (sub(graph), sub(graph));
            let compare = [Op::Lt, Op::Le, Op::Gt, Op::Ge, Op::Eq, Op::Ne][pick(tc, 5) as usize];
            let mut cond = graph.binary(compare, l, r);
            if !tame && pick(tc, 3) == 0 {
                cond = graph.unary(Op::Not, cond);
            }
            let (a, b) = (sub(graph), sub(graph));
            graph.select(cond, a, b)
        }
    }
}
