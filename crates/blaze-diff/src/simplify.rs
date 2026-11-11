//! Simplification by equality saturation (docs/future/autodiff.md, layer 3's minimum).
//!
//! The terms go into an e-graph (classes of equal terms: a union-find over classes, a hash-cons
//! of e-nodes whose arguments are classes, and a rebuild that restores congruence), the
//! rewrites of [`REWRITES`] are applied until nothing changes or the [`Fuel`] is spent, and the
//! cheapest term of each root's class under a [`CostModel`] is taken out.
//!
//! [`REWRITES`] is the one table of rewrites, each with the IEEE 754 condition it holds under
//! ([`Ieee`]). By default only the exact ones run: for every input, NaN, infinities, signed
//! zeros and subnormals included, the rewritten term has the same value bit for bit (a NaN's
//! sign and payload aside, which Rust leaves unspecified). So `x + 0.0 → x` is not among them
//! (`-0.0 + 0.0` is `0.0`), `x + -0.0 → x` is; `x - x → 0` is not (NaN, infinity). Constants are
//! folded only through operations IEEE 754 rounds correctly ([`crate::eval::folds_exactly`]).
//! The algebraic rewrites (`fast_math`) run only when asked, and [`Simplified::ran`] names
//! every rewrite that changed something.
//!
//! Every run is bounded ([`Fuel`]): hitting a bound is [`Stop::Iterations`] or [`Stop::Nodes`],
//! not an error, and the result is still the cheapest term found, which is valid: each union
//! made is of equal terms, whenever the run stops.

use std::alloc::Global;

use blaze_hash::HashMap;

use crate::{
    eval::{self, Konst},
    expr::{Graph, Id, Node, Op, Sort, exponent_payload, payload_of},
};

/// How many pattern variables a rewrite may use.
pub const MAX_VARS: usize = 4;

/// A pattern over terms.
#[derive(Clone, Copy, Debug)]
pub enum Pat {
    /// A pattern variable: any term, the same one at each mention.
    V(u8),
    /// The float constant with exactly this value's bits: `0.0` is not `-0.0`.
    K(f64),
    /// `true` or `false`.
    B(bool),
    /// An operation of patterns.
    N(Op, &'static [Pat]),
    /// A `powi` with this exponent.
    Powi(&'static Pat, i32),
}

/// The IEEE 754 condition a rewrite holds under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ieee {
    /// Both sides have the same value for every input, bit for bit (any NaN is any NaN).
    Exact,
    /// Equal in real arithmetic under the condition named, not in floats: rounding, a NaN, an
    /// infinity or a signed zero can differ. Runs only under `fast_math`.
    FastMath(&'static str),
}

/// One rewrite: wherever `lhs` matches, the term equals `rhs`.
#[derive(Clone, Copy, Debug)]
pub struct Rewrite {
    pub name: &'static str,
    pub lhs: Pat,
    pub rhs: Pat,
    pub ieee: Ieee,
}

const A: Pat = Pat::V(0);
const B: Pat = Pat::V(1);
const C: Pat = Pat::V(2);
const ZERO: Pat = Pat::K(0.0);
const MINUS_ZERO: Pat = Pat::K(-0.0);
const ONE: Pat = Pat::K(1.0);
const MINUS_ONE: Pat = Pat::K(-1.0);
const TWO: Pat = Pat::K(2.0);
const REAL: &str = "finite values, no rounding";
const NONZERO: &str = "finite values, a divisor that is not 0";

const fn exact(name: &'static str, lhs: Pat, rhs: Pat) -> Rewrite {
    Rewrite { name, lhs, rhs, ieee: Ieee::Exact }
}

const fn fast(name: &'static str, lhs: Pat, rhs: Pat, holds: &'static str) -> Rewrite {
    Rewrite { name, lhs, rhs, ieee: Ieee::FastMath(holds) }
}

/// The rewrites: the exact ones first, then `fast_math`'s. The one owner: the simplifier, the
/// tests that check each rule numerically and `blaze explain` read this table.
pub const REWRITES: &[Rewrite] = &[
    exact("mul-one", Pat::N(Op::Mul, &[A, ONE]), A),
    exact("div-one", Pat::N(Op::Div, &[A, ONE]), A),
    exact("add-minus-zero", Pat::N(Op::Add, &[A, MINUS_ZERO]), A),
    exact("sub-zero", Pat::N(Op::Sub, &[A, ZERO]), A),
    exact("neg-neg", Pat::N(Op::Neg, &[Pat::N(Op::Neg, &[A])]), A),
    exact("mul-minus-one", Pat::N(Op::Mul, &[A, MINUS_ONE]), Pat::N(Op::Neg, &[A])),
    exact("div-minus-one", Pat::N(Op::Div, &[A, MINUS_ONE]), Pat::N(Op::Neg, &[A])),
    exact("sub-as-add", Pat::N(Op::Sub, &[A, B]), Pat::N(Op::Add, &[A, Pat::N(Op::Neg, &[B])])),
    exact("add-neg-as-sub", Pat::N(Op::Add, &[A, Pat::N(Op::Neg, &[B])]), Pat::N(Op::Sub, &[A, B])),
    exact(
        "neg-out-of-mul",
        Pat::N(Op::Mul, &[Pat::N(Op::Neg, &[A]), B]),
        Pat::N(Op::Neg, &[Pat::N(Op::Mul, &[A, B])]),
    ),
    exact(
        "neg-out-of-dividend",
        Pat::N(Op::Div, &[Pat::N(Op::Neg, &[A]), B]),
        Pat::N(Op::Neg, &[Pat::N(Op::Div, &[A, B])]),
    ),
    exact(
        "neg-out-of-divisor",
        Pat::N(Op::Div, &[A, Pat::N(Op::Neg, &[B])]),
        Pat::N(Op::Neg, &[Pat::N(Op::Div, &[A, B])]),
    ),
    exact("commute-add", Pat::N(Op::Add, &[A, B]), Pat::N(Op::Add, &[B, A])),
    exact("commute-mul", Pat::N(Op::Mul, &[A, B]), Pat::N(Op::Mul, &[B, A])),
    exact("double", Pat::N(Op::Add, &[A, A]), Pat::N(Op::Mul, &[A, TWO])),
    exact("abs-neg", Pat::N(Op::Abs, &[Pat::N(Op::Neg, &[A])]), Pat::N(Op::Abs, &[A])),
    exact("abs-abs", Pat::N(Op::Abs, &[Pat::N(Op::Abs, &[A])]), Pat::N(Op::Abs, &[A])),
    exact("select-true", Pat::N(Op::Select, &[Pat::B(true), A, B]), A),
    exact("select-false", Pat::N(Op::Select, &[Pat::B(false), A, B]), B),
    exact("select-same", Pat::N(Op::Select, &[C, A, A]), A),
    exact("not-not", Pat::N(Op::Not, &[Pat::N(Op::Not, &[A])]), A),
    // `fast_math` from here on.
    fast("add-zero", Pat::N(Op::Add, &[A, ZERO]), A, "a value that is not -0.0"),
    fast("mul-zero", Pat::N(Op::Mul, &[A, ZERO]), ZERO, "a finite value"),
    fast("zero-div", Pat::N(Op::Div, &[ZERO, A]), ZERO, NONZERO),
    fast("sub-self", Pat::N(Op::Sub, &[A, A]), ZERO, "a finite value"),
    fast("div-self", Pat::N(Op::Div, &[A, A]), ONE, NONZERO),
    fast("mul-div-cancel", Pat::N(Op::Div, &[Pat::N(Op::Mul, &[A, B]), B]), A, NONZERO),
    fast("div-mul-cancel", Pat::N(Op::Mul, &[Pat::N(Op::Div, &[A, B]), B]), A, NONZERO),
    fast(
        "assoc-add",
        Pat::N(Op::Add, &[Pat::N(Op::Add, &[A, B]), C]),
        Pat::N(Op::Add, &[A, Pat::N(Op::Add, &[B, C])]),
        REAL,
    ),
    fast(
        "assoc-add-back",
        Pat::N(Op::Add, &[A, Pat::N(Op::Add, &[B, C])]),
        Pat::N(Op::Add, &[Pat::N(Op::Add, &[A, B]), C]),
        REAL,
    ),
    fast(
        "assoc-mul",
        Pat::N(Op::Mul, &[Pat::N(Op::Mul, &[A, B]), C]),
        Pat::N(Op::Mul, &[A, Pat::N(Op::Mul, &[B, C])]),
        REAL,
    ),
    fast(
        "assoc-mul-back",
        Pat::N(Op::Mul, &[A, Pat::N(Op::Mul, &[B, C])]),
        Pat::N(Op::Mul, &[Pat::N(Op::Mul, &[A, B]), C]),
        REAL,
    ),
    fast(
        "factor",
        Pat::N(Op::Add, &[Pat::N(Op::Mul, &[A, C]), Pat::N(Op::Mul, &[B, C])]),
        Pat::N(Op::Mul, &[Pat::N(Op::Add, &[A, B]), C]),
        REAL,
    ),
    fast(
        "factor-sub",
        Pat::N(Op::Sub, &[Pat::N(Op::Mul, &[A, C]), Pat::N(Op::Mul, &[B, C])]),
        Pat::N(Op::Mul, &[Pat::N(Op::Sub, &[A, B]), C]),
        REAL,
    ),
    fast(
        "common-divisor",
        Pat::N(Op::Add, &[Pat::N(Op::Div, &[A, C]), Pat::N(Op::Div, &[B, C])]),
        Pat::N(Op::Div, &[Pat::N(Op::Add, &[A, B]), C]),
        NONZERO,
    ),
    fast(
        "common-divisor-sub",
        Pat::N(Op::Sub, &[Pat::N(Op::Div, &[A, C]), Pat::N(Op::Div, &[B, C])]),
        Pat::N(Op::Div, &[Pat::N(Op::Sub, &[A, B]), C]),
        NONZERO,
    ),
    fast(
        "mul-of-quotient",
        Pat::N(Op::Mul, &[A, Pat::N(Op::Div, &[B, C])]),
        Pat::N(Op::Div, &[Pat::N(Op::Mul, &[A, B]), C]),
        NONZERO,
    ),
    fast(
        "quotient-of-quotient",
        Pat::N(Op::Div, &[Pat::N(Op::Div, &[A, B]), C]),
        Pat::N(Op::Div, &[A, Pat::N(Op::Mul, &[B, C])]),
        NONZERO,
    ),
    fast(
        "sub-of-quotient",
        Pat::N(Op::Sub, &[A, Pat::N(Op::Div, &[B, C])]),
        Pat::N(Op::Div, &[Pat::N(Op::Sub, &[Pat::N(Op::Mul, &[A, C]), B]), C]),
        NONZERO,
    ),
    fast(
        "mul-reciprocal",
        Pat::N(Op::Mul, &[A, Pat::N(Op::Div, &[ONE, B])]),
        Pat::N(Op::Div, &[A, B]),
        NONZERO,
    ),
    fast(
        "pythagoras",
        Pat::N(
            Op::Add,
            &[
                Pat::N(Op::Mul, &[Pat::N(Op::Sin, &[A]), Pat::N(Op::Sin, &[A])]),
                Pat::N(Op::Mul, &[Pat::N(Op::Cos, &[A]), Pat::N(Op::Cos, &[A])]),
            ],
        ),
        ONE,
        "a finite value",
    ),
    fast(
        "ln-exp",
        Pat::N(Op::Ln, &[Pat::N(Op::Exp, &[A])]),
        A,
        "a finite value whose `exp` is finite",
    ),
    fast(
        "exp-sum",
        Pat::N(Op::Mul, &[Pat::N(Op::Exp, &[A]), Pat::N(Op::Exp, &[B])]),
        Pat::N(Op::Exp, &[Pat::N(Op::Add, &[A, B])]),
        REAL,
    ),
    // One way only: a `powi` is a call, and a rewrite that made one would have the lowered
    // code call a function the program never named. A square prints as `x²` either way.
    fast("powi-two", Pat::Powi(&A, 2), Pat::N(Op::Mul, &[A, A]), REAL),
    fast("powi-one", Pat::Powi(&A, 1), A, REAL),
    fast("powi-zero", Pat::Powi(&A, 0), ONE, "a value that is not NaN"),
    fast(
        "fma",
        Pat::N(Op::Add, &[Pat::N(Op::Mul, &[A, B]), C]),
        Pat::N(Op::MulAdd, &[A, B, C]),
        "finite values: the product is no longer rounded before the sum",
    ),
    fast(
        "neg-of-difference",
        Pat::N(Op::Neg, &[Pat::N(Op::Sub, &[A, B])]),
        Pat::N(Op::Sub, &[B, A]),
        "values that differ, or a zero whose sign does not matter",
    ),
    fast(
        "sqrt-squared",
        Pat::N(Op::Mul, &[Pat::N(Op::Sqrt, &[A]), Pat::N(Op::Sqrt, &[A])]),
        A,
        "a finite value that is not below 0",
    ),
];

/// The rewrites of [`REWRITES`] that run: the exact ones, and with `fast_math` the rest.
pub fn rewrites(fast_math: bool) -> Vec<Rewrite> {
    // alloc: the selected rules, a few dozen `Copy` rows, once per run
    REWRITES.iter().copied().filter(|it| fast_math || it.ieee == Ieee::Exact).collect()
}

impl Pat {
    /// The term this pattern is with `vars[i]` for variable `i`, added to `graph`.
    pub fn build(&self, graph: &mut Graph, vars: &[Id]) -> Id {
        match *self {
            Pat::V(at) => vars[usize::from(at)],
            Pat::K(value) => graph.constant(value),
            Pat::B(value) => graph.bool(value),
            Pat::Powi(base, n) => {
                let base = base.build(graph, vars);
                graph.powi(base, n)
            }
            Pat::N(op, pats) => {
                let mut args = [Id::NONE; 3];
                for (at, pat) in pats.iter().enumerate() {
                    args[at] = pat.build(graph, vars);
                }
                graph.add(Node { op, args, payload: 0 })
            }
        }
    }

    /// The sort each variable must have for the pattern, of sort `expected`, to be a term:
    /// `sorts[i]` for variable `i`, `None` for one it does not mention.
    pub fn sorts(&self, expected: Sort, sorts: &mut [Option<Sort>; MAX_VARS]) {
        match *self {
            Pat::V(at) => sorts[usize::from(at)] = Some(expected),
            Pat::K(_) | Pat::B(_) => {}
            Pat::Powi(base, _) => base.sorts(Sort::Float, sorts),
            Pat::N(op, pats) => {
                for (at, pat) in pats.iter().enumerate() {
                    pat.sorts(op.arg_sort(at), sorts);
                }
            }
        }
    }

    /// The sort of a term this pattern matches; `None` for a lone variable.
    pub fn sort(&self) -> Option<Sort> {
        match *self {
            Pat::V(_) => None,
            Pat::K(_) | Pat::Powi(..) => Some(Sort::Float),
            Pat::B(_) => Some(Sort::Bool),
            Pat::N(op, pats) => match op {
                // A `select` is of its arms' sort, which only its arms say.
                Op::Select => pats.get(1).and_then(Pat::sort).or(Some(Sort::Float)),
                _ => Some(op.sort()),
            },
        }
    }
}

/// What extraction counts a term as costing: the sum over its nodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CostModel {
    /// The target fuses `a * b + c`: a `mul_add` is one cheap instruction, not a library call.
    pub fma: bool,
    /// A call into the math library: `sin`, `cos`, `tan`, `tanh`, `exp`, `ln`, `powf`.
    pub call: u64,
}

impl CostModel {
    /// A CPU with fused multiply-add.
    pub const CPU_FMA: CostModel = CostModel { fma: true, call: 40 };
    /// A GPU: transcendental calls dominate.
    pub const GPU: CostModel = CostModel { fma: true, call: 100 };

    pub const fn cost(&self, op: Op) -> u64 {
        match op {
            Op::Const | Op::Var | Op::Bool | Op::Neg | Op::Abs | Op::Not | Op::And | Op::Or => 1,
            Op::Add | Op::Sub | Op::Mul | Op::Min | Op::Max | Op::Select => 2,
            Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::Eq | Op::Ne => 2,
            Op::Powi => 6,
            Op::Div | Op::Sqrt => 8,
            Op::MulAdd => {
                if self.fma {
                    3
                } else {
                    self.call
                }
            }
            Op::Sin | Op::Cos | Op::Tan | Op::Tanh | Op::Exp | Op::Ln | Op::Powf => self.call,
        }
    }
}

impl Default for CostModel {
    /// A CPU without fused multiply-add.
    fn default() -> Self {
        CostModel { fma: false, call: 40 }
    }
}

/// The bounds of one run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fuel {
    /// The e-graph stops growing at this many e-nodes.
    pub nodes: usize,
    /// At most this many rounds of matching every rule and applying every match.
    pub iterations: usize,
}

impl Default for Fuel {
    fn default() -> Self {
        Fuel { nodes: 4000, iterations: 12 }
    }
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// No rewrite changes anything any more: the term is the cheapest the rules reach.
    Saturated,
    /// The iteration bound stopped it, after this many rounds.
    Iterations(usize),
    /// The node bound stopped it, at this many e-nodes.
    Nodes(usize),
}

/// What [`simplify`] gives.
#[derive(Debug)]
pub struct Simplified {
    /// The cheapest terms found, in a graph of their own.
    pub graph: Graph,
    /// The roots, in the order given.
    pub roots: Vec<Id>,
    pub stop: Stop,
    /// The rewrites that changed something, by name, in [`REWRITES`]' order.
    pub ran: Vec<&'static str>,
    /// The rounds run.
    pub iterations: usize,
    /// The e-nodes the e-graph ended with.
    pub enodes: usize,
}

/// What to simplify with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Also run the algebraic rewrites, which are not exact in IEEE 754.
    pub fast_math: bool,
    pub fuel: Fuel,
    pub cost: CostModel,
}

/// The cheapest terms equal to `roots` under [`REWRITES`]: the exact rewrites, and with
/// `options.fast_math` the algebraic ones too.
pub fn simplify(graph: &Graph, roots: &[Id], options: &Options) -> Simplified {
    simplify_with(graph, roots, &rewrites(options.fast_math), options.fuel, options.cost)
}

/// [`simplify`] with the rewrites given: the table's, or a test's own.
pub fn simplify_with(
    graph: &Graph,
    roots: &[Id],
    rules: &[Rewrite],
    fuel: Fuel,
    cost: CostModel,
) -> Simplified {
    let mut egraph = EGraph::new(graph.width());
    let reached = graph.reached(roots);
    // alloc: each node's class, scratch of this run
    let mut class_of = vec![NO_CLASS; graph.len()];
    for (at, node) in graph.nodes().iter().enumerate() {
        if !reached[at] {
            continue;
        }
        let mut node = *node;
        for arg in &mut node.args[..node.op.arity()] {
            *arg = Id(class_of[arg.index()]);
        }
        class_of[at] = egraph.add(node);
    }
    // alloc: which rules fired, and the matches of one round
    let mut fired = vec![false; rules.len()];
    let mut matches: Vec<(usize, u32, Subst)> = Vec::new();
    let mut stop = Stop::Iterations(fuel.iterations);
    let mut iterations = 0;
    'rounds: while iterations < fuel.iterations {
        iterations += 1;
        egraph.rebuild();
        matches.clear();
        for (at, rule) in rules.iter().enumerate() {
            egraph.matches(&rule.lhs, &mut |class, subst| {
                if matches.len() < MATCHES_PER_ROUND {
                    matches.push((at, class, subst));
                }
            });
        }
        let unions = egraph.unions;
        for &(at, class, subst) in &matches {
            if egraph.nodes.len() >= fuel.nodes {
                stop = Stop::Nodes(egraph.nodes.len());
                break 'rounds;
            }
            let rhs = egraph.instantiate(&rules[at].rhs, &subst);
            if egraph.union(class, rhs) {
                fired[at] = true;
            }
        }
        if egraph.unions == unions {
            // A round cut short at its match bound has not shown that nothing is left.
            if matches.len() < MATCHES_PER_ROUND {
                stop = Stop::Saturated;
            } else {
                stop = Stop::Iterations(iterations);
            }
            break;
        }
    }
    egraph.rebuild();
    let root_classes: Vec<u32> = roots.iter().map(|it| egraph.find(class_of[it.index()])).collect(); // alloc: one class per root
    let (out, roots) = egraph.extract(&root_classes, cost);
    // alloc: the names of the rules that ran, the caller's result
    let ran =
        rules.iter().zip(&fired).filter(|(_, fired)| **fired).map(|(it, _)| it.name).collect();
    Simplified { graph: out, roots, stop, ran, iterations, enodes: egraph.nodes.len() }
}

/// The most matches one round applies: a bound on a round's work besides the node bound.
const MATCHES_PER_ROUND: usize = 20_000;
const NO_CLASS: u32 = u32::MAX;
/// The cost of a class no term of which is known yet.
const NO_COST: u64 = u64::MAX;

/// A binding of pattern variables to classes; [`NO_CLASS`] for one not bound.
type Subst = [u32; MAX_VARS];

/// An e-graph over [`Node`]s whose arguments are classes.
///
/// A class is named by the index of the e-node that made it; `parent` is the union-find over
/// those names, and the smaller name is always the root, so no order comes from a hash map.
pub(crate) struct EGraph {
    width: crate::expr::Width,
    /// The e-nodes in the order added. After [`EGraph::rebuild`] every argument is a root.
    pub(crate) nodes: Vec<Node>,
    /// The union-find: `parent[c] == c` for a root.
    parent: Vec<u32>,
    /// The constant a class is known to be, at its root.
    konst: Vec<Option<Konst>>,
    /// The class of each e-node, as of the last rebuild: looked up, never iterated.
    memo: HashMap<Node, u32, Global>,
    /// After a rebuild: whether the e-node is the first of its shape (a later equal one is a
    /// duplicate matching skips).
    live: Vec<bool>,
    /// After a rebuild: the live e-nodes of each root class, ascending.
    members: Vec<Vec<u32>>,
    /// How many unions joined two different classes.
    pub(crate) unions: u64,
}

impl EGraph {
    pub(crate) fn new(width: crate::expr::Width) -> EGraph {
        // alloc: the e-graph of one run owns its tables
        EGraph {
            width,
            nodes: Vec::new(),
            parent: Vec::new(),
            konst: Vec::new(),
            memo: HashMap::new_in(Global),
            live: Vec::new(),
            members: Vec::new(),
            unions: 0,
        }
    }

    /// The root of `class`.
    pub(crate) fn find(&self, mut class: u32) -> u32 {
        // Bounded by the classes: a parent's name is below its child's.
        while self.parent[class as usize] != class {
            class = self.parent[class as usize];
        }
        class
    }

    fn canonical(&self, mut node: Node) -> Node {
        for arg in &mut node.args[..node.op.arity()] {
            *arg = Id(self.find(arg.0));
        }
        node
    }

    /// The constant `node` is, from its arguments' classes.
    fn konst_of(&self, node: &Node) -> Option<Konst> {
        match node.op {
            Op::Const => Some(Konst::Float(node.payload)),
            Op::Bool => Some(Konst::Bool(node.payload != 0)),
            Op::Var => None,
            _ => {
                let mut args = [Konst::Bool(false); 3];
                let arity = node.op.arity();
                for at in 0..arity {
                    args[at] = self.konst[self.find(node.args[at].0) as usize]?;
                }
                eval::fold(self.width, node, &args[..arity])
            }
        }
    }

    fn konst_node(konst: Konst) -> Node {
        match konst {
            Konst::Float(bits) => Node { op: Op::Const, args: [Id::NONE; 3], payload: bits },
            Konst::Bool(it) => Node { op: Op::Bool, args: [Id::NONE; 3], payload: u64::from(it) },
        }
    }

    /// The class of `node` (its arguments are classes), made when the e-node is new. A new
    /// e-node that folds to a constant joins that constant's class.
    pub(crate) fn add(&mut self, node: Node) -> u32 {
        let node = self.canonical(node);
        if let Some(&class) = self.memo.get(&node) {
            return self.find(class);
        }
        let class = u32::try_from(self.nodes.len()).expect("fewer than 2^32 e-nodes");
        let konst = self.konst_of(&node);
        self.nodes.push(node);
        self.parent.push(class);
        self.konst.push(konst);
        self.memo.insert(node, class);
        if let Some(konst) = konst
            && !matches!(node.op, Op::Const | Op::Bool)
        {
            let constant = self.add(Self::konst_node(konst));
            self.union(class, constant);
            return self.find(class);
        }
        class
    }

    /// Says the classes `a` and `b` are equal. Whether they were two classes.
    pub(crate) fn union(&mut self, a: u32, b: u32) -> bool {
        let (a, b) = (self.find(a), self.find(b));
        if a == b {
            return false;
        }
        let (root, child) = (a.min(b), a.max(b));
        self.parent[child as usize] = root;
        // Two constants in one class are one value under the exact rules (two NaNs at most);
        // under `fast_math` a rewrite outside its condition can join two: the first stays.
        if self.konst[root as usize].is_none() {
            self.konst[root as usize] = self.konst[child as usize];
        }
        self.unions += 1;
        true
    }

    /// Restores the invariants unions break: every e-node's arguments are roots, two e-nodes
    /// of one shape are in one class (congruence), and a class whose e-node folds to a
    /// constant holds that constant. Repeats until a pass changes nothing; each pass that
    /// changes something joins two classes or gives one its constant, so it ends.
    pub(crate) fn rebuild(&mut self) {
        loop {
            for class in 0..self.parent.len() {
                self.parent[class] = self.find(class as u32);
            }
            self.memo.clear();
            self.live.clear();
            self.live.resize(self.nodes.len(), false);
            let mut changed = false;
            // alloc: the constants found this pass, usually none
            let mut folded: Vec<(u32, Konst)> = Vec::new();
            for at in 0..self.nodes.len() {
                let node = self.canonical(self.nodes[at]);
                self.nodes[at] = node;
                let class = self.find(at as u32);
                match self.memo.get(&node).copied() {
                    Some(other) => {
                        if self.union(other, class) {
                            changed = true;
                        }
                    }
                    None => {
                        self.memo.insert(node, class);
                        self.live[at] = true;
                    }
                }
                let class = self.find(class);
                if self.konst[class as usize].is_none()
                    && let Some(konst) = self.konst_of(&node)
                {
                    self.konst[class as usize] = Some(konst);
                    folded.push((class, konst));
                    changed = true;
                }
            }
            for (class, konst) in folded {
                let constant = self.add(Self::konst_node(konst));
                self.union(class, constant);
            }
            if !changed {
                break;
            }
        }
        for members in &mut self.members {
            members.clear();
        }
        // alloc: one list per class, kept across rebuilds
        self.members.resize_with(self.nodes.len(), Vec::new);
        for at in 0..self.nodes.len() {
            if self.live[at] {
                let class = self.find(at as u32);
                self.members[class as usize].push(at as u32);
            }
        }
    }

    /// Every match of `pattern`, as its class and the classes its variables are bound to, in
    /// class order. Call after [`EGraph::rebuild`].
    fn matches(&self, pattern: &Pat, found: &mut dyn FnMut(u32, Subst)) {
        for class in 0..self.parent.len() as u32 {
            if self.parent[class as usize] != class {
                continue;
            }
            self.matches_in(pattern, class, [NO_CLASS; MAX_VARS], &mut |subst| found(class, subst));
        }
    }

    fn matches_in(
        &self,
        pattern: &Pat,
        class: u32,
        mut subst: Subst,
        found: &mut dyn FnMut(Subst),
    ) {
        match *pattern {
            Pat::V(at) => {
                let bound = &mut subst[usize::from(at)];
                if *bound == NO_CLASS {
                    *bound = class;
                    found(subst);
                } else if *bound == class {
                    found(subst);
                }
            }
            Pat::K(value) => {
                let bits = payload_of(self.width, value);
                if self.konst[class as usize] == Some(Konst::Float(bits)) {
                    found(subst);
                }
            }
            Pat::B(value) => {
                if self.konst[class as usize] == Some(Konst::Bool(value)) {
                    found(subst);
                }
            }
            Pat::Powi(base, n) => {
                for &at in &self.members[class as usize] {
                    let node = self.nodes[at as usize];
                    if node.op == Op::Powi && node.exponent() == n {
                        self.matches_in(base, node.args[0].0, subst, found);
                    }
                }
            }
            Pat::N(op, pats) => {
                for &at in &self.members[class as usize] {
                    let node = self.nodes[at as usize];
                    if node.op == op && op.arity() == pats.len() {
                        self.matches_args(&node, pats, 0, subst, found);
                    }
                }
            }
        }
    }

    fn matches_args(
        &self,
        node: &Node,
        pats: &[Pat],
        at: usize,
        subst: Subst,
        found: &mut dyn FnMut(Subst),
    ) {
        if at == pats.len() {
            found(subst);
            return;
        }
        self.matches_in(&pats[at], node.args[at].0, subst, &mut |subst| {
            self.matches_args(node, pats, at + 1, subst, &mut *found);
        });
    }

    /// The class of `pattern` under `subst`, its e-nodes added.
    fn instantiate(&mut self, pattern: &Pat, subst: &Subst) -> u32 {
        match *pattern {
            Pat::V(at) => self.find(subst[usize::from(at)]),
            Pat::K(value) => {
                self.add(Self::konst_node(Konst::Float(payload_of(self.width, value))))
            }
            Pat::B(value) => self.add(Self::konst_node(Konst::Bool(value))),
            Pat::Powi(base, n) => {
                let base = self.instantiate(base, subst);
                self.add(Node {
                    op: Op::Powi,
                    args: [Id(base), Id::NONE, Id::NONE],
                    payload: exponent_payload(n),
                })
            }
            Pat::N(op, pats) => {
                let mut args = [Id::NONE; 3];
                for (at, pat) in pats.iter().enumerate() {
                    args[at] = Id(self.instantiate(pat, subst));
                }
                self.add(Node { op, args, payload: 0 })
            }
        }
    }

    /// The cheapest term of each root class of `roots` under `cost`, in a new graph. A term's
    /// cost is the sum over its nodes as a tree; of two e-nodes of one cost the one added
    /// first wins. Call after [`EGraph::rebuild`].
    pub(crate) fn extract(&self, roots: &[u32], cost: CostModel) -> (Graph, Vec<Id>) {
        let count = self.nodes.len();
        // alloc: each class's best cost and e-node, scratch of this extraction
        let mut best: Vec<(u64, u32)> = vec![(NO_COST, NO_CLASS); count];
        // Each pass that changes something lowers some class's cost to its final value, so
        // there are at most as many passes as classes, and one more that changes nothing.
        for _ in 0..=count {
            let mut changed = false;
            for at in 0..count {
                if !self.live[at] {
                    continue;
                }
                let node = &self.nodes[at];
                let mut total = cost.cost(node.op);
                for arg in node.args() {
                    total = total.saturating_add(best[arg.index()].0);
                }
                let class = self.find(at as u32) as usize;
                if total < best[class].0 {
                    best[class] = (total, at as u32);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let mut graph = Graph::new(self.width);
        // alloc: each class's node in the new graph, and the walk's stack
        let mut built: Vec<Option<Id>> = vec![None; count];
        let mut stack: Vec<u32> = roots.to_vec();
        while let Some(&class) = stack.last() {
            if built[class as usize].is_some() {
                stack.pop();
                continue;
            }
            let (found, at) = best[class as usize];
            assert!(found != NO_COST, "blaze-diff: a class reached from a root has no term");
            let mut node = self.nodes[at as usize];
            let mut waits = false;
            for arg in &mut node.args[..node.op.arity()] {
                match built[arg.index()] {
                    Some(id) => *arg = id,
                    None => {
                        stack.push(arg.0);
                        waits = true;
                    }
                }
            }
            if waits {
                continue;
            }
            built[class as usize] = Some(graph.add(node));
            stack.pop();
        }
        // alloc: one id per root, the caller's result
        let roots =
            roots.iter().map(|&class| built[class as usize].expect("built above")).collect();
        (graph, roots)
    }

    /// Whether `a` and `b` are one class.
    #[cfg(test)]
    pub(crate) fn same(&self, a: u32, b: u32) -> bool {
        self.find(a) == self.find(b)
    }
}
