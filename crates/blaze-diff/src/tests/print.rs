//! Printing: the same graph prints the same bytes, node order is the order of insertion, and
//! the two forms read as written.

use super::{float_term, plan_example};
use crate::{
    diff::{self, Choice, Policy},
    expr::{Graph, Id, Op, Width},
    print,
    simplify::{Options, simplify},
};

/// The plan's example, its derivative by `x` and the derivative simplified with `fast_math`,
/// each printed both ways.
fn printed() -> Vec<String> {
    let mut graph = Graph::new(Width::F64);
    let f = plan_example(&mut graph);
    let df = diff::derivative(&mut graph, f, 0, Policy::default()).expect("no kink");
    let fast = simplify(&graph, &[df], &Options { fast_math: true, ..Options::default() });
    let params = ["x", "y"];
    vec![
        print::math(&graph, f, &params),
        print::rust(&graph, &[f], "f", &params),
        print::math(&graph, df, &params),
        print::rust(&graph, &[df], "d_f_dx", &params),
        print::math(&fast.graph, fast.roots[0], &params),
        print::rust(&fast.graph, &fast.roots, "d_f_dx", &params),
        format!("{:?} {:?} {:?}", fast.stop, fast.ran, fast.graph),
    ]
}

#[test]
fn printing_the_same_derivative_twice_gives_the_same_bytes() {
    // Two runs, each with graphs, hash maps and an e-graph of its own.
    let (first, second) = (printed(), printed());
    assert_eq!(first, second);
    assert_eq!(first[0], "sin(x * y) / (1 + x²)");
    assert_eq!(first[1], "fn f(x: f64, y: f64) -> f64 {\n    (x * y).sin() / (1.0 + x * x)\n}\n");
}

#[hegel::test]
fn node_order_is_insertion_order_whatever_was_added_before(tc: hegel::TestCase) {
    // The same term built in an empty graph and in one that already holds other terms: the
    // new nodes come in the same order, and the term prints the same.
    let mut scratch = Graph::new(Width::F64);
    let term = float_term(&tc, &mut scratch, 2, 3, false);
    let reached = scratch.reached(&[term]);

    let mut crowded = Graph::new(Width::F64);
    float_term(&tc, &mut crowded, 2, 3, false);
    let offset = crowded.len();
    let mut alone = Graph::new(Width::F64);
    let copy = |into: &mut Graph| {
        let mut map = vec![Id::NONE; scratch.len()];
        for (at, node) in scratch.nodes().iter().enumerate() {
            if reached[at] {
                let mut node = *node;
                for arg in &mut node.args[..node.op.arity()] {
                    *arg = map[arg.index()];
                }
                map[at] = into.add(node);
            }
        }
        map[term.index()]
    };
    let (in_alone, in_crowded) = (copy(&mut alone), copy(&mut crowded));
    // Ids only grow, and a node met again keeps its id.
    for (at, node) in alone.nodes().iter().enumerate() {
        for arg in node.args() {
            assert!(arg.index() < at);
        }
    }
    assert!(crowded.len() <= offset + alone.len());
    let params = ["x", "y"];
    // The math form is of the term alone: the ids it happens to have do not show.
    assert_eq!(print::math(&alone, in_alone, &params), print::math(&crowded, in_crowded, &params));
    // The Rust form names shared nodes by id, so it is a function of the graph: the same graph
    // built again prints the same.
    let mut again = Graph::new(Width::F64);
    let in_again = copy(&mut again);
    assert_eq!(in_again, in_alone);
    assert_eq!(
        print::rust(&alone, &[in_alone], "f", &params),
        print::rust(&again, &[in_again], "f", &params)
    );
}

#[test]
fn the_rust_form_is_typed_and_parenthesized() {
    let mut graph = Graph::new(Width::F64);
    let x = graph.var(0);
    let (two, three, minus_two) = (graph.constant(2.0), graph.constant(3.0), graph.constant(-2.0));
    // A literal receiver says its type; a negative one is parenthesized.
    let six = graph.binary(Op::Mul, two, three);
    let sin = graph.unary(Op::Sin, six);
    let sqrt = graph.unary(Op::Sqrt, minus_two);
    let sum = graph.binary(Op::Add, sin, sqrt);
    assert_eq!(
        print::rust(&graph, &[sum], "f", &[]),
        "fn f() -> f64 {\n    (2.0_f64 * 3.0).sin() + (-2.0_f64).sqrt()\n}\n"
    );
    // Subtraction and division keep their grouping; a shared node is bound once.
    let x_minus = graph.binary(Op::Sub, x, two);
    let grouped = graph.binary(Op::Sub, three, x_minus);
    let quotient = graph.binary(Op::Div, grouped, x_minus);
    let cond = graph.binary(Op::Lt, x, two);
    let neg = graph.unary(Op::Neg, quotient);
    let select = graph.select(cond, neg, x);
    let power = graph.powi(select, 3);
    assert_eq!(
        print::rust(&graph, &[power, cond], "g", &["x"]),
        "fn g(x: f64) -> (f64, bool) {\n    \
             let t0: f64 = x - 2.0;\n    \
             let t1: bool = x < 2.0;\n    \
             ((if t1 { -((3.0 - t0) / t0) } else { x }).powi(3), t1)\n\
         }\n"
    );
    assert_eq!(print::math(&graph, power, &["x"]), "(-((3 - (x - 2)) / (x - 2)) if x < 2 else x)³");

    // A subgradient prints as the choice it is.
    let abs = graph.unary(Op::Abs, x);
    let policy = Policy { kinks: Choice::Subgradient(0.0) };
    let d = diff::derivative(&mut graph, abs, 0, policy).unwrap();
    assert_eq!(print::math(&graph, d, &["x"]), "(1 if x > 0 else (-1 if x < 0 else 0))");
    assert_eq!(
        print::rust(&graph, &[d], "d_abs", &["x"]),
        "fn d_abs(x: f64) -> f64 {\n    if x > 0.0 { 1.0 } else { if x < 0.0 { -1.0 } else { 0.0 } }\n}\n"
    );

    // An `f32` graph prints `f32`.
    let mut narrow = Graph::new(Width::F32);
    let x = narrow.var(0);
    let tenth = narrow.constant(0.1);
    let product = narrow.binary(Op::Mul, tenth, x);
    assert_eq!(
        print::rust(&narrow, &[product], "h", &["x"]),
        "fn h(x: f32) -> f32 {\n    0.1 * x\n}\n"
    );
    assert_eq!(print::math(&narrow, product, &["x"]), "0.1x");
}
