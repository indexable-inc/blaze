//! Simplification against evaluation: the exact rewrites keep every value bit for bit, each
//! rewrite holds on its own, and the e-graph is closed under congruence and finds a term no
//! dearer than naive rewriting does.

use hegel::generators as gs;

use super::{float_term, pick};
use crate::{
    eval::{Value, eval_all, same},
    expr::{Graph, Id, Node, Op, Sort, Width, payload_of},
    simplify::{
        CostModel, EGraph, Fuel, Ieee, MAX_VARS, Options, Pat, REWRITES, Rewrite, Stop, rewrites,
        simplify, simplify_with,
    },
};

/// The values the exact rewrites have to survive: both zeros, both infinities, a NaN, the
/// smallest subnormals, the smallest normal, the largest finite, and ordinary values.
const SPECIAL: &[f64] = &[
    0.0,
    -0.0,
    1.0,
    -1.0,
    f64::INFINITY,
    f64::NEG_INFINITY,
    f64::NAN,
    5e-324,
    -5e-324,
    1e-310,
    f64::MIN_POSITIVE,
    f64::MAX,
    2.5,
    -3.75,
];

/// A drawn input: one of [`SPECIAL`], or any bits at all.
fn input(tc: &hegel::TestCase) -> f64 {
    if pick(tc, 3) == 0 {
        f64::from_bits(tc.draw(gs::integers::<u64>()))
    } else {
        SPECIAL[pick(tc, SPECIAL.len() as u32 - 1) as usize]
    }
}

/// The oracle: whether `after` computes at `point` exactly what `before` does, for each pair
/// of roots: the same bits, any NaN being any NaN, and the same condition.
fn refines(before: (&Graph, &[Id]), after: (&Graph, &[Id]), point: &[f64]) -> Result<(), String> {
    let was = eval_all::<f64>(before.0, before.1, point).map_err(|it| it.to_string())?;
    let is = eval_all::<f64>(after.0, after.1, point).map_err(|it| it.to_string())?;
    for (&old, &new) in before.1.iter().zip(after.1) {
        let (old, new) = (was[old.index()].expect("a root"), is[new.index()].expect("a root"));
        let equal = match (old, new) {
            (Value::Float(old), Value::Float(new)) => same(old, new),
            (Value::Bool(old), Value::Bool(new)) => old == new,
            _ => false,
        };
        if !equal {
            return Err(format!(
                "at {point:?} the term is {old:?} and its simplification is {new:?}\n{:?}{:?}",
                before.0, after.0
            ));
        }
    }
    Ok(())
}

/// The refinement: simplifying by the exact rewrites preserves the value: for a drawn term
/// over every operation and drawn inputs, signed zeros, infinities, NaN and subnormals
/// included, the simplified term evaluates to the same bits as the term (any NaN is any NaN),
/// whether or not the fuel stopped the run.
#[hegel::test]
fn exact_simplification_preserves_every_value(tc: hegel::TestCase) {
    let mut graph = Graph::new(Width::F64);
    let roots =
        [float_term(&tc, &mut graph, 3, 3, false), float_term(&tc, &mut graph, 3, 2, false)];
    // Sometimes too little fuel to finish: what is found so far is still the same value.
    let fuel = if pick(&tc, 3) == 0 { Fuel { nodes: 40, iterations: 2 } } else { Fuel::default() };
    let simplified = simplify(&graph, &roots, &Options { fuel, ..Options::default() });
    for _ in 0..8 {
        let point = [input(&tc), input(&tc), input(&tc)];
        if let Err(message) =
            refines((&graph, &roots), (&simplified.graph, &simplified.roots), &point)
        {
            panic!("{message}");
        }
    }
}

#[test]
fn the_oracle_catches_x_minus_x_rewritten_to_zero() {
    // Not exact: NaN - NaN and inf - inf are NaN.
    const PLANTED: Rewrite = Rewrite {
        name: "planted-sub-self",
        lhs: Pat::N(Op::Sub, &[Pat::V(0), Pat::V(0)]),
        rhs: Pat::K(0.0),
        ieee: Ieee::Exact,
    };
    let mut graph = Graph::new(Width::F64);
    let x = graph.var(0);
    let root = graph.binary(Op::Sub, x, x);
    let verdicts = |rules: &[Rewrite]| -> Vec<bool> {
        let out = simplify_with(&graph, &[root], rules, Fuel::default(), CostModel::default());
        SPECIAL
            .iter()
            .map(|&value| refines((&graph, &[root]), (&out.graph, &out.roots), &[value]).is_ok())
            .collect()
    };
    let exact = rewrites(false);
    assert!(verdicts(&exact).iter().all(|&ok| ok), "the oracle refuses the exact rewrites");
    let mut planted = exact;
    planted.push(PLANTED);
    let verdicts = verdicts(&planted);
    for (&value, &ok) in SPECIAL.iter().zip(&verdicts) {
        // `x - x` is 0 for every finite `x`, and NaN for a NaN and an infinity.
        assert_eq!(ok, value.is_finite(), "at {value}");
    }
}

/// A rewrite's two sides as terms over variables: a float variable `i` is `x_i`, a condition
/// variable `i` is `x_i < x_(MAX_VARS + i)`.
fn sides(rule: &Rewrite) -> (Graph, [Id; 2], usize) {
    let mut sorts = [None; MAX_VARS];
    rule.lhs
        .sorts(rule.lhs.sort().expect("a rewrite's left side is not a lone variable"), &mut sorts);
    let mut graph = Graph::new(Width::F64);
    let mut vars = [Id::NONE; MAX_VARS];
    let mut used = 0;
    for (at, sort) in sorts.iter().enumerate() {
        match sort {
            None => {}
            Some(Sort::Float) => {
                vars[at] = graph.var(at as u32);
                used = at + 1;
            }
            Some(Sort::Bool) => {
                let (a, b) = (graph.var(at as u32), graph.var((MAX_VARS + at) as u32));
                vars[at] = graph.binary(Op::Lt, a, b);
                used = at + 1;
            }
        }
    }
    let lhs = rule.lhs.build(&mut graph, &vars);
    let rhs = rule.rhs.build(&mut graph, &vars);
    (graph, [lhs, rhs], used)
}

/// Every point whose first `used` coordinates are each one of `values`, the rest 1.
fn grid(values: &[f64], used: usize, mut each: impl FnMut(&[f64; 2 * MAX_VARS])) {
    let mut at = [0usize; MAX_VARS];
    loop {
        let mut point = [1.0; 2 * MAX_VARS];
        for slot in 0..used {
            point[slot] = values[at[slot]];
        }
        each(&point);
        // The next index tuple, as an odometer; done when it wraps.
        let mut slot = 0;
        loop {
            if slot == used {
                return;
            }
            at[slot] += 1;
            if at[slot] < values.len() {
                break;
            }
            at[slot] = 0;
            slot += 1;
        }
    }
}

#[test]
fn each_exact_rewrite_holds_bit_for_bit_on_every_special_value() {
    for rule in REWRITES.iter().filter(|it| it.ieee == Ieee::Exact) {
        let (graph, [lhs, rhs], used) = sides(rule);
        grid(SPECIAL, used, |point| {
            if let Err(message) = refines((&graph, &[lhs]), (&graph, &[rhs]), point) {
                panic!("`{}` is not exact: {message}", rule.name);
            }
        });
    }
}

#[hegel::test]
fn each_exact_rewrite_holds_bit_for_bit_on_any_bits(tc: hegel::TestCase) {
    let exact = rewrites(false);
    let rule = &exact[pick(&tc, exact.len() as u32 - 1) as usize];
    let (graph, [lhs, rhs], _) = sides(rule);
    let mut point = [0.0; 2 * MAX_VARS];
    for coordinate in &mut point {
        *coordinate = input(&tc);
    }
    if let Err(message) = refines((&graph, &[lhs]), (&graph, &[rhs]), &point) {
        panic!("`{}` is not exact: {message}", rule.name);
    }
}

#[test]
fn each_fast_math_rewrite_holds_up_to_rounding_on_its_domain() {
    // Positive and finite: inside every `fast_math` condition.
    const DOMAIN: &[f64] = &[0.5, 0.8125, 1.25, 2.0];
    for rule in REWRITES.iter().filter(|it| it.ieee != Ieee::Exact) {
        let (graph, [lhs, rhs], used) = sides(rule);
        grid(DOMAIN, used, |point| {
            let values = eval_all::<f64>(&graph, &[lhs, rhs], point).unwrap();
            let (l, r) =
                (values[lhs.index()].unwrap().float(), values[rhs.index()].unwrap().float());
            let scale = l.abs().max(r.abs()).max(1.0);
            assert!((l - r).abs() <= 1e-9 * scale, "`{}` at {point:?}: {l:e} and {r:e}", rule.name);
        });
    }
}

#[test]
fn every_rewrite_is_well_formed() {
    let mut names: Vec<&str> = REWRITES.iter().map(|it| it.name).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), REWRITES.len(), "a rewrite's name is used twice");
    for rule in REWRITES {
        let (mut lhs, mut rhs) = ([None; MAX_VARS], [None; MAX_VARS]);
        let sort = rule.lhs.sort().unwrap_or_else(|| panic!("`{}` matches everything", rule.name));
        rule.lhs.sorts(sort, &mut lhs);
        rule.rhs.sorts(sort, &mut rhs);
        for at in 0..MAX_VARS {
            // A variable of the right side is bound by the left, at the same sort.
            assert!(rhs[at].is_none() || rhs[at] == lhs[at], "`{}`, variable {at}", rule.name);
        }
    }
    // The exact rewrites come first.
    let first_fast = REWRITES.iter().position(|it| it.ieee != Ieee::Exact).unwrap();
    assert!(REWRITES[first_fast..].iter().all(|it| it.ieee != Ieee::Exact));
}

#[test]
fn the_exact_rewrites_simplify_what_they_say() {
    let mut graph = Graph::new(Width::F64);
    let x = graph.var(0);
    let (one, minus_zero, zero) = (graph.constant(1.0), graph.constant(-0.0), graph.constant(0.0));
    // (x * 1) + -0.0 is x.
    let times_one = graph.binary(Op::Mul, x, one);
    let a = graph.binary(Op::Add, times_one, minus_zero);
    // x + 0.0 is not x (-0.0 + 0.0 is 0.0), and x - x is not 0.
    let b = graph.binary(Op::Add, x, zero);
    let c = graph.binary(Op::Sub, x, x);
    // -(-x) * -1 is -x; 2 * 3 folds; sin(2) does not (the math library's, not IEEE's).
    let neg = graph.unary(Op::Neg, x);
    let neg_neg = graph.unary(Op::Neg, neg);
    let minus_one = graph.constant(-1.0);
    let d = graph.binary(Op::Mul, neg_neg, minus_one);
    let (two, three) = (graph.constant(2.0), graph.constant(3.0));
    let e = graph.binary(Op::Mul, two, three);
    let f = graph.unary(Op::Sin, two);
    let out = simplify(&graph, &[a, b, c, d, e, f], &Options::default());
    let g = &out.graph;
    let [a, b, c, d, e, f] = out.roots[..] else { panic!("six roots") };
    assert_eq!(g.as_var(a), Some(0));
    assert_eq!(g.node(b).op, Op::Add);
    assert_eq!(g.node(c).op, Op::Sub);
    assert_eq!((g.node(d).op, g.as_var(g.node(d).args[0])), (Op::Neg, Some(0)));
    assert_eq!(g.as_const(e), Some(6.0));
    assert_eq!(g.node(f).op, Op::Sin);
    for name in ["mul-one", "add-minus-zero", "neg-neg", "mul-minus-one"] {
        assert!(out.ran.contains(&name), "`{name}` did not run: {:?}", out.ran);
    }

    // One rewrite and its mirror image: nothing is left to do after the second round.
    let alone = simplify(&graph, &[times_one], &Options::default());
    assert_eq!((alone.stop, alone.iterations), (Stop::Saturated, 2));
    assert_eq!(alone.graph.as_var(alone.roots[0]), Some(0));

    // With `fast_math` the two that were refused go through, and `ran` says so.
    let fast = simplify(&graph, &[b, c], &Options { fast_math: true, ..Options::default() });
    assert_eq!(fast.graph.as_var(fast.roots[0]), Some(0));
    assert_eq!(fast.graph.as_const(fast.roots[1]), Some(0.0));
    assert!(fast.ran.contains(&"add-zero") && fast.ran.contains(&"sub-self"), "{:?}", fast.ran);
}

#[test]
fn spent_fuel_is_a_typed_outcome_with_a_valid_term() {
    // ((((x + 1) + 2) + 3) + ...) under `fast_math`: associativity and commutativity grow the
    // e-graph past any small bound.
    let mut graph = Graph::new(Width::F64);
    let mut sum = graph.var(0);
    for at in 1..=8 {
        let term = graph.var(at);
        sum = graph.binary(Op::Add, sum, term);
    }
    let options = |fuel| Options { fast_math: true, fuel, ..Options::default() };
    let by_nodes = simplify(&graph, &[sum], &options(Fuel { nodes: 60, iterations: 50 }));
    assert!(matches!(by_nodes.stop, Stop::Nodes(nodes) if nodes >= 60), "{:?}", by_nodes.stop);
    let by_rounds = simplify(&graph, &[sum], &options(Fuel { nodes: 1_000_000, iterations: 1 }));
    assert_eq!(by_rounds.stop, Stop::Iterations(1));
    let point: Vec<f64> = (0..9).map(|at| 0.25 * f64::from(at) + 1.0).collect();
    for out in [&by_nodes, &by_rounds] {
        let value = crate::eval::eval_one::<f64>(&out.graph, out.roots[0], &point).unwrap();
        // Sums of quarters: exact in any order.
        assert_eq!(value, point.iter().sum::<f64>());
    }
}

#[test]
fn congruent_terms_are_one_class_after_a_rebuild() {
    let mut egraph = EGraph::new(Width::F64);
    let leaf = |egraph: &mut EGraph, index: u64| {
        egraph.add(Node { op: Op::Var, args: [Id::NONE; 3], payload: index })
    };
    let unary = |egraph: &mut EGraph, op: Op, a: u32| {
        egraph.add(Node { op, args: [Id(a), Id::NONE, Id::NONE], payload: 0 })
    };
    let (a, b) = (leaf(&mut egraph, 0), leaf(&mut egraph, 1));
    let (sin_a, sin_b) = (unary(&mut egraph, Op::Sin, a), unary(&mut egraph, Op::Sin, b));
    let (exp_sin_a, exp_sin_b) =
        (unary(&mut egraph, Op::Exp, sin_a), unary(&mut egraph, Op::Exp, sin_b));
    let cos_a = unary(&mut egraph, Op::Cos, a);
    egraph.rebuild();
    assert!(!egraph.same(sin_a, sin_b) && !egraph.same(exp_sin_a, exp_sin_b));
    // a = b, so sin(a) = sin(b), so exp(sin(a)) = exp(sin(b)): two levels up.
    assert!(egraph.union(a, b));
    egraph.rebuild();
    assert!(egraph.same(sin_a, sin_b), "congruence one level up");
    assert!(egraph.same(exp_sin_a, exp_sin_b), "congruence two levels up");
    assert!(!egraph.same(sin_a, cos_a) && !egraph.same(sin_a, exp_sin_a));
    // Adding a term that is there gives its class.
    let again = unary(&mut egraph, Op::Sin, b);
    assert!(egraph.same(again, sin_a));
}

/// The invariant: after a rebuild the e-graph is closed under congruence: every e-node's
/// arguments are class roots, and two e-nodes of the same operation, payload and argument
/// classes are in one class, whatever unions came before.
#[hegel::test]
fn a_rebuilt_egraph_is_closed_under_congruence(tc: hegel::TestCase) {
    let mut egraph = EGraph::new(Width::F64);
    let mut classes: Vec<u32> = (0..4)
        .map(|index| egraph.add(Node { op: Op::Var, args: [Id::NONE; 3], payload: index }))
        .collect();
    for _ in 0..pick(&tc, 12) {
        let class = |tc: &hegel::TestCase| Id(classes[pick(tc, classes.len() as u32 - 1) as usize]);
        let node = match pick(&tc, 3) {
            0 => Node { op: Op::Sin, args: [class(&tc), Id::NONE, Id::NONE], payload: 0 },
            1 => Node { op: Op::Neg, args: [class(&tc), Id::NONE, Id::NONE], payload: 0 },
            2 => Node { op: Op::Add, args: [class(&tc), class(&tc), Id::NONE], payload: 0 },
            _ => Node { op: Op::Mul, args: [class(&tc), class(&tc), Id::NONE], payload: 0 },
        };
        let added = egraph.add(node);
        classes.push(added);
    }
    for _ in 0..pick(&tc, 6) {
        let a = classes[pick(&tc, classes.len() as u32 - 1) as usize];
        let b = classes[pick(&tc, classes.len() as u32 - 1) as usize];
        egraph.union(a, b);
    }
    if let Err(message) = closed(&mut egraph, false) {
        panic!("{message}");
    }
}

/// The oracle of [`a_rebuilt_egraph_is_closed_under_congruence`]: rebuilds (unless `skip`, the
/// planted bug: unions without the rebuild) and checks the closure by comparing every pair.
fn closed(egraph: &mut EGraph, skip: bool) -> Result<(), String> {
    if !skip {
        egraph.rebuild();
    }
    let canonical = |node: &Node| {
        let mut node = *node;
        for arg in &mut node.args[..node.op.arity()] {
            *arg = Id(egraph.find(arg.0));
        }
        node
    };
    for (at, node) in egraph.nodes.iter().enumerate() {
        if canonical(node) != *node {
            return Err(format!("e-node {at} has an argument that is not a root: {node:?}"));
        }
        for (other_at, other) in egraph.nodes.iter().enumerate().skip(at + 1) {
            if canonical(node) == canonical(other) && !egraph.same(at as u32, other_at as u32) {
                return Err(format!(
                    "e-nodes {at} and {other_at} are congruent and in two classes"
                ));
            }
        }
    }
    Ok(())
}

#[test]
fn the_oracle_catches_a_union_without_a_rebuild() {
    let build = || {
        let mut egraph = EGraph::new(Width::F64);
        let var = |egraph: &mut EGraph, index| {
            egraph.add(Node { op: Op::Var, args: [Id::NONE; 3], payload: index })
        };
        let (a, b) = (var(&mut egraph, 0), var(&mut egraph, 1));
        for class in [a, b] {
            egraph.add(Node { op: Op::Sin, args: [Id(class), Id::NONE, Id::NONE], payload: 0 });
        }
        egraph.rebuild();
        egraph.union(a, b);
        egraph
    };
    assert_eq!(closed(&mut build(), false), Ok(()));
    assert!(
        closed(&mut build(), true).is_err(),
        "sin(a) and sin(b) are apart, and the oracle passed"
    );
}

/// A term's cost as a tree: what extraction minimizes.
fn tree_cost(graph: &Graph, id: Id, cost: CostModel) -> u64 {
    let node = graph.node(id);
    node.args()
        .iter()
        .fold(cost.cost(node.op), |sum, &arg| sum.saturating_add(tree_cost(graph, arg, cost)))
}

/// Whether `pattern` is the term `id`, syntactically; binds `vars`.
fn is_instance(graph: &Graph, pattern: &Pat, id: Id, vars: &mut [Id; MAX_VARS]) -> bool {
    let node = graph.node(id);
    match *pattern {
        Pat::V(at) => {
            let bound = &mut vars[usize::from(at)];
            if *bound == Id::NONE {
                *bound = id;
            }
            *bound == id
        }
        Pat::K(value) => node.op == Op::Const && node.payload == payload_of(graph.width(), value),
        Pat::B(value) => node.op == Op::Bool && node.payload == u64::from(value),
        Pat::Powi(base, n) => {
            node.op == Op::Powi
                && node.exponent() == n
                && is_instance(graph, base, node.args[0], vars)
        }
        Pat::N(op, pats) => {
            node.op == op
                && pats
                    .iter()
                    .zip(node.args())
                    .all(|(pat, &arg)| is_instance(graph, pat, arg, vars))
        }
    }
}

/// Rewriting a term the naive way, the model saturation is held against: its arguments first, then at the root
/// any rule whose left side is the term and whose right side is cheaper, until none is.
fn naive(graph: &mut Graph, id: Id, rules: &[Rewrite], cost: CostModel) -> Id {
    let mut node = graph.node(id);
    for at in 0..node.op.arity() {
        node.args[at] = naive(graph, node.args[at], rules, cost);
    }
    let mut term = graph.add(node);
    // Each step lowers the cost, which is at least 1: bounded by the term's cost.
    'steps: loop {
        for rule in rules {
            let mut vars = [Id::NONE; MAX_VARS];
            if is_instance(graph, &rule.lhs, term, &mut vars) {
                let candidate = rule.rhs.build(graph, &vars);
                if tree_cost(graph, candidate, cost) < tree_cost(graph, term, cost) {
                    term = candidate;
                    continue 'steps;
                }
            }
        }
        return term;
    }
}

/// The reference model: equality saturation that ran to the end finds a term at most as dear
/// as naive rewriting to a fixpoint does, since every term naive rewriting reaches is in the
/// root's class, and extraction takes the cheapest of the class.
#[hegel::test]
fn saturation_is_no_dearer_than_naive_rewriting(tc: hegel::TestCase) {
    let mut graph = Graph::new(Width::F64);
    let root = float_term(&tc, &mut graph, 2, 3, false);
    let rules = rewrites(false);
    let cost = CostModel::default();
    let out = simplify_with(&graph, &[root], &rules, Fuel { nodes: 20_000, iterations: 40 }, cost);
    if out.stop != Stop::Saturated {
        return;
    }
    let rewritten = naive(&mut graph, root, &rules, cost);
    let (saturated, naive) =
        (tree_cost(&out.graph, out.roots[0], cost), tree_cost(&graph, rewritten, cost));
    assert!(
        saturated <= naive,
        "saturation found cost {saturated}, naive rewriting {naive}\n{graph:?}"
    );
    assert!(naive <= tree_cost(&graph, root, cost));
}
