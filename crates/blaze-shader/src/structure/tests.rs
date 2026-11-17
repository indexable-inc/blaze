//! The structurizer's oracle: for any reducible graph and any sequence of branch choices, the
//! graph, its labelled structure, the simplified structure and naga's shape run the same blocks in
//! the same order and leave from the same block. Each run is bounded (`FUEL` blocks), and the
//! traces are compared up to where it ran out, so a loop that never ends is a finite trace too.
use super::*;

const FUEL: usize = 300;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    Run(BlockId),
    Exit(BlockId),
    /// The run used up its fuel here.
    Fuel,
}

/// Branch choices, consumed one per run of a block with two or more successors, in run order.
struct Choices<'a> {
    seq: &'a [u8],
    at: usize,
}

impl Choices<'_> {
    fn arm(&mut self, n: usize) -> usize {
        let c = if self.seq.is_empty() { 0 } else { self.seq[self.at % self.seq.len()] as usize };
        self.at += 1;
        c % n
    }
}

fn run_cfg(cfg: &Cfg, choices: &[u8]) -> Vec<Event> {
    let mut ch = Choices { seq: choices, at: 0 };
    let mut trace = Vec::new();
    let mut b = 0;
    loop {
        if trace.len() == FUEL {
            trace.push(Event::Fuel);
            return trace;
        }
        trace.push(Event::Run(b));
        let succ = &cfg.succ[b as usize];
        b = match succ.len() {
            0 => {
                trace.push(Event::Exit(b));
                return trace;
            }
            1 => succ[0],
            n => succ[ch.arm(n)],
        };
    }
}

/// How a statement list ended.
#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Normal,
    Br(Target),
    Break,
    Continue,
    Exit,
    Fuel,
}

struct Machine<'a> {
    ch: Choices<'a>,
    trace: Vec<Event>,
    flag: u32,
    /// Loop iterations, so a loop with no block in it can't spin forever either.
    spins: usize,
}

impl<'a> Machine<'a> {
    fn new(choices: &'a [u8]) -> Self {
        Machine { ch: Choices { seq: choices, at: 0 }, trace: Vec::new(), flag: 0, spins: 0 }
    }

    fn run(&mut self, b: BlockId) -> bool {
        if self.trace.len() == FUEL {
            self.trace.push(Event::Fuel);
            return false;
        }
        self.trace.push(Event::Run(b));
        true
    }

    fn spin(&mut self) {
        self.spins += 1;
        assert!(self.spins < 100 * FUEL, "a loop spins without running a block");
    }

    fn stmts(&mut self, list: &[Stmt]) -> Flow {
        for s in list {
            let flow = match s {
                Stmt::Run(b) => {
                    if self.run(*b) {
                        Flow::Normal
                    } else {
                        Flow::Fuel
                    }
                }
                Stmt::Exit(b) => {
                    self.trace.push(Event::Exit(*b));
                    Flow::Exit
                }
                Stmt::Select { arms, .. } => {
                    let i = self.ch.arm(arms.len());
                    self.stmts(&arms[i])
                }
                Stmt::Br(t) => Flow::Br(*t),
                Stmt::Loop { header, body } => loop {
                    self.spin();
                    match self.stmts(body) {
                        Flow::Br(Target::Continue(h)) if h == *header => continue,
                        Flow::Normal => break Flow::Normal,
                        other => break other,
                    }
                },
                Stmt::Block { merge, body } => match self.stmts(body) {
                    Flow::Br(Target::Exit(y)) if y == *merge => Flow::Normal,
                    other => other,
                },
            };
            if flow != Flow::Normal {
                return flow;
            }
        }
        Flow::Normal
    }

    fn shaped(&mut self, list: &[Shaped]) -> Flow {
        for s in list {
            let flow = match s {
                Shaped::Run(b) => {
                    if self.run(*b) {
                        Flow::Normal
                    } else {
                        Flow::Fuel
                    }
                }
                Shaped::Exit(b) => {
                    self.trace.push(Event::Exit(*b));
                    Flow::Exit
                }
                Shaped::Select { arms, .. } => {
                    let i = self.ch.arm(arms.len());
                    self.shaped(&arms[i])
                }
                Shaped::Loop(body) => loop {
                    self.spin();
                    match self.shaped(body) {
                        Flow::Normal | Flow::Continue => continue,
                        Flow::Break => break Flow::Normal,
                        other => break other,
                    }
                },
                Shaped::Break => Flow::Break,
                Shaped::Continue => Flow::Continue,
                Shaped::SetFlag(v) => {
                    self.flag = *v;
                    Flow::Normal
                }
                Shaped::ClearFlag => {
                    self.flag = 0;
                    Flow::Normal
                }
                Shaped::IfFlag { value, then } => {
                    if self.flag == *value {
                        self.shaped(then)
                    } else {
                        Flow::Normal
                    }
                }
                Shaped::IfAnyFlag(then) => {
                    if self.flag != 0 {
                        self.shaped(then)
                    } else {
                        Flow::Normal
                    }
                }
            };
            if flow != Flow::Normal {
                return flow;
            }
        }
        Flow::Normal
    }
}

fn run_stmts(tree: &[Stmt], choices: &[u8]) -> Vec<Event> {
    let mut m = Machine::new(choices);
    let flow = m.stmts(tree);
    assert!(matches!(flow, Flow::Exit | Flow::Fuel), "the structure fell off its end or branched out of it: {flow:?}");
    m.trace
}

fn run_shaped(tree: &[Shaped], choices: &[u8]) -> Vec<Event> {
    let mut m = Machine::new(choices);
    let flow = m.shaped(tree);
    assert!(matches!(flow, Flow::Exit | Flow::Fuel), "the shape fell off its end or broke out of it: {flow:?}");
    assert!(flow == Flow::Fuel || m.flag == 0, "the shape left with the flag still set to {}", m.flag);
    m.trace
}

/// A random reducible graph of up to `max` blocks, drawing each number from `draw(lo, hi)` (both
/// ends included): forward edges only from earlier blocks to later ones (each block gets one from
/// an earlier block, so all are reachable), then edges back from a block to one of its dominators,
/// which keeps the graph reducible and its dominators unchanged.
fn reducible(max: usize, draw: &mut dyn FnMut(usize, usize) -> usize) -> Cfg {
    let n = draw(1, max);
    let mut succ: Vec<Vec<BlockId>> = vec![Vec::new(); n];
    for j in 1..n {
        let p = draw(0, j - 1);
        succ[p].push(j as BlockId);
    }
    for _ in 0..draw(0, 2 * n) {
        let a = draw(0, n - 1);
        let b = draw(a, n - 1);
        if a < b {
            succ[a].push(b as BlockId);
        }
    }
    for s in &mut succ {
        // Shuffle each block's arms by rotating them, so the edge into a block isn't always first.
        if s.len() > 1 {
            let k = draw(0, s.len() - 1);
            s.rotate_left(k);
        }
    }
    let dag = Cfg { succ: succ.clone() };
    let g = Graph::new(&dag).expect("a forward-only graph is reducible");
    for _ in 0..draw(0, n) {
        let u = draw(0, n - 1) as BlockId;
        // One of u's dominators, itself included (a loop of one block).
        let mut doms = vec![u];
        let mut d = u;
        while d != 0 {
            d = g.idom[d as usize];
            doms.push(d);
        }
        let v = doms[draw(0, doms.len() - 1)];
        succ[u as usize].push(v);
    }
    Cfg { succ }
}

/// Whether every form of `cfg` runs as the graph does under `choices`; the first that doesn't, if
/// any, with the two traces.
fn disagreement(cfg: &Cfg, choices: &[u8], plant: Plant) -> Option<String> {
    let want = run_cfg(cfg, choices);
    let tree = structure_with(cfg, plant).expect("a reducible graph structures");
    let got = run_stmts(&tree, choices);
    if got != want {
        return Some(format!("structure: {got:?}\n    graph: {want:?}\n    tree: {tree:?}"));
    }
    let simple = simplify(tree);
    let got = run_stmts(&simple, choices);
    if got != want {
        return Some(format!("simplify: {got:?}\n    graph: {want:?}\n    tree: {simple:?}"));
    }
    let shaped = shape_with(simple, plant);
    let got = run_shaped(&shaped, choices);
    if got != want {
        return Some(format!("shape: {got:?}\n    graph: {want:?}\n    shape: {shaped:?}"));
    }
    None
}

/// The invariant: for any reducible graph and any sequence of branch choices, the graph, its
/// labelled structure, the simplified structure and naga's shape run the same blocks in the same
/// order and leave from the same block, compared up to `FUEL` blocks. The oracle over the tape's
/// graphs and choices.
#[test]
fn structured_control_flow_runs_as_the_graph_does() {
    blaze_sim::check(blaze_sim::Config::cases(500), |tape| {
        let (cfg, choices) = graph_and_choices(tape);
        if let Some(d) = disagreement(&cfg, &choices, Plant::None) {
            panic!("{cfg:?} under {choices:?}:\n    {d}");
        }
    });
}

/// A reducible graph of up to 14 blocks and up to 40 branch choices, drawn from `tape`.
fn graph_and_choices(tape: &blaze_sim::Tape) -> (Cfg, Vec<u8>) {
    let cfg = reducible(14, &mut |lo, hi| tape.int_in(lo..=hi, "graph"));
    let choices = tape.vec(0..=40, "choices", |tape| tape.int::<u8>("choice"));
    (cfg, choices)
}

/// One case of a control: the drawn graph and choices run under `plant`, a disagreement with the
/// graph the oracle's finding. A planted bug may also break the structure's own shape (a branch
/// outside its construct), which panics: that is caught here and counts as the oracle's finding.
fn planted_case(tape: &blaze_sim::Tape, plant: Plant) -> Result<(), blaze_sim::Broke> {
    let (cfg, choices) = graph_and_choices(tape);
    match std::panic::catch_unwind(|| disagreement(&cfg, &choices, plant)) {
        Ok(None) => Ok(()),
        Ok(Some(d)) => Err(blaze_sim::Broke::Oracle(format!(
            "runs differently from the graph: {cfg:?} under {choices:?}:\n    {d}"
        ))),
        Err(_) => Err(blaze_sim::Broke::Oracle(format!(
            "runs differently from the graph: {cfg:?} under {choices:?}: the structure panicked"
        ))),
    }
}

/// The controls: with the merges nested inside out, or the flag's check left out, the oracle
/// finds runs that go wrong. A green oracle that missed these would prove nothing.
#[test]
fn the_oracle_catches_planted_bugs() {
    for plant in [Plant::MergesInsideOut, Plant::NoFlagCheck] {
        blaze_sim::control(
            blaze_sim::Config::cases(300),
            Plant::None,
            plant,
            "runs differently from the graph",
            planted_case,
        );
    }
}

/// A loop entered in its middle as well as at its top has no structured form, and the error names
/// the edge.
#[test]
fn an_irreducible_graph_is_refused_naming_the_edge() {
    // 0 -> 1, 0 -> 2, 1 <-> 2: the loop {1, 2} has two entries.
    let cfg = Cfg { succ: vec![vec![1, 2], vec![2], vec![1]] };
    let err = structure(&cfg).expect_err("irreducible");
    assert!(matches!(err, Error::Irreducible { .. }), "{err:?}");
    assert!(err.to_string().contains("must be reducible"), "{err}");
    assert_eq!(structure(&Cfg { succ: vec![vec![3]] }), Err(Error::NoBlock { from: 0, to: 3 }));
    assert_eq!(structure(&Cfg { succ: vec![] }), Err(Error::Empty));
}

/// A `continue 'outer` from inside an inner loop: naga's `continue` reaches only the innermost
/// loop, so it goes through the flag, and the flag is clear once it lands.
#[test]
fn a_continue_of_the_outer_loop_goes_through_the_flag() {
    // 0: outer header -> 1; 1: inner header -> 2 or 4 (a return from inside both); 2 -> 1 (inner
    // again) or 3; 3 -> 0 (outer again, from inside the inner loop's code).
    let cfg = Cfg { succ: vec![vec![1], vec![2, 4], vec![1, 3], vec![0], vec![]] };
    let shaped = structured(&cfg).expect("reducible");
    fn has_flag(list: &[Shaped]) -> bool {
        list.iter().any(|s| match s {
            Shaped::SetFlag(_) => true,
            Shaped::Loop(b) | Shaped::IfAnyFlag(b) | Shaped::IfFlag { then: b, .. } => has_flag(b),
            Shaped::Select { arms, .. } => arms.iter().any(|a| has_flag(a)),
            _ => false,
        })
    }
    for choices in [&[0u8, 1, 1, 0, 1][..], &[1], &[0, 0, 1, 1, 1, 0, 1, 1]] {
        assert_eq!(run_shaped(&shaped, choices), run_cfg(&cfg, choices), "{shaped:?}");
    }
    assert!(has_flag(&shaped), "the outer continue should set the flag: {shaped:?}");
}
