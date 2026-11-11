//! Structured control flow from a control-flow graph: step 1 of the pipeline in
//! docs/v2/plan/design/formal-verification.md, the way Charon makes LLBC from ULLBC.
//!
//! A [`Cfg`] is blocks of statements ending in a jump, a switch, a return or an end (a panic,
//! or code nothing reaches). [`reconstruct`] gives the same program as a tree of [`Node`]s:
//! `Branch` (an `if` or a `match`), `Loop`, `Block` (a labelled block), `Break`, `Continue`
//! and `Return`. Both are generic in the statement `S` and in what a switch tests, `C`: the
//! reconstruction reads neither, so it is tested on programs of a small test language and used
//! on MIR ([`crate::body`]).
//!
//! The algorithm is Ramsey's ("Beyond Relooper", ICFP 2022) over the dominator tree, for
//! reducible graphs only: a block with two or more forward edges into it follows a `Block` its
//! predecessors `Break` out of; a loop header opens a `Loop` its back edges `Continue`; every
//! other block is put where its one predecessor jumps to it. On top of it, the code a loop
//! exits to is put after the loop, not inside it ([`Analysis::exit_of`]), so a `while` reads
//! `Block { Loop { if c { body } else { break } } }; after` as it was written. A graph with a
//! retreating edge to a block that does not dominate its source is irreducible, and refused
//! ([`StructureError::Irreducible`]): nothing is duplicated to make it reducible.
//!
//! Labels are counted from the inside: `Break(0)` leaves the nearest enclosing `Block` or
//! `Loop`. The normal form [`reconstruct`] returns has no `Break` at the end of the `Block` it
//! leaves, no `Continue` at the end of the `Loop` it repeats, and no `Block` nothing breaks
//! out of. Reaching the end of a `Loop`'s body repeats it, as in Rust.

use std::fmt;

/// A control-flow graph. Blocks are named by their index; blocks nothing reaches are ignored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cfg<S, C> {
    pub blocks: Vec<CfgBlock<S, C>>,
    pub entry: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CfgBlock<S, C> {
    pub statements: Vec<S>,
    pub exit: Exit<C>,
}

/// How a block ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exit<C> {
    Goto(usize),
    /// To the target of the first value `on` equals, else to `otherwise`.
    Switch { on: C, targets: Vec<(u128, usize)>, otherwise: usize },
    Return,
    End(End),
}

/// Control stops without returning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum End {
    /// A panic: v0 models it as `Result.fail`, whatever the panic strategy (the plan's
    /// "Panics and overflow").
    Panic,
    /// MIR's `Unreachable`: reaching it is undefined behaviour, which safe code never does.
    Unreachable,
}

/// A node of structured control flow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node<S, C> {
    Do(S),
    /// The arm of the first value `on` equals, else `otherwise`. No label.
    Branch { on: C, arms: Vec<(u128, Vec<Node<S, C>>)>, otherwise: Vec<Node<S, C>> },
    /// A label: `Break` to it goes on after it.
    Block(Vec<Node<S, C>>),
    /// A label: `Continue` to it, or reaching the end of the body, runs the body again.
    Loop(Vec<Node<S, C>>),
    /// Leaves the `n`th enclosing label, counted from 0 at the innermost; a `Block` in what
    /// [`reconstruct`] returns.
    Break(u32),
    /// Repeats the `n`th enclosing label, a `Loop`.
    Continue(u32),
    Return,
    End(End),
}

/// Why a graph has no structured form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StructureError {
    /// The edge `from -> to` goes back to a block that does not dominate its source: the loop
    /// has two entries.
    Irreducible { from: usize, to: usize },
    /// A block jumps to a block the graph does not have.
    NoSuchBlock { from: usize, to: usize },
}

impl fmt::Display for StructureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StructureError::Irreducible { from, to } => write!(
                f,
                "irreducible control flow: block {from} jumps back to block {to}, which does not \
                 dominate it (a loop with two entries)"
            ),
            StructureError::NoSuchBlock { from, to } => {
                write!(f, "block {from} jumps to block {to}, which the body does not have")
            }
        }
    }
}

/// A bug planted in the reconstruction, for the controls of its refinement test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    None,
    /// A back edge becomes a `Break` of its loop.
    ContinueAsBreak,
    /// Where both arms of a two-way branch end at one join, the second arm's own code is
    /// dropped.
    ElseDropped,
}

impl<C> Exit<C> {
    fn successors(&self) -> Vec<usize> {
        match self {
            Exit::Goto(to) => vec![*to],
            Exit::Switch { targets, otherwise, .. } => {
                targets.iter().map(|(_, to)| *to).chain([*otherwise]).collect()
            }
            Exit::Return | Exit::End(_) => Vec::new(),
        }
    }
}

/// What the reconstruction knows of a graph's shape.
struct Analysis {
    /// A reachable block's place in reverse postorder.
    rpo: Vec<Option<usize>>,
    idom: Vec<usize>,
    /// The blocks a block immediately dominates, in reverse postorder.
    children: Vec<Vec<usize>>,
    header: Vec<bool>,
    /// Two or more forward edges come into the block.
    merge: Vec<bool>,
    /// The header of the outermost loop a block is the exit of: its immediate dominator is in
    /// the loop and it is not. Its code goes after that loop.
    exit_of: Vec<Option<usize>>,
    /// The exits of each header, latest in reverse postorder first.
    exits: Vec<Vec<usize>>,
}

fn analyse<S, C>(cfg: &Cfg<S, C>) -> Result<Analysis, StructureError> {
    let n = cfg.blocks.len();
    let successors: Vec<Vec<usize>> = cfg.blocks.iter().map(|it| it.exit.successors()).collect();
    for (from, tos) in successors.iter().enumerate() {
        if let Some(&to) = tos.iter().find(|&&to| to >= n) {
            return Err(StructureError::NoSuchBlock { from, to });
        }
    }
    if cfg.entry >= n {
        return Err(StructureError::NoSuchBlock { from: cfg.entry, to: cfg.entry });
    }
    // Postorder by an explicit stack: (block, next successor to visit).
    let mut seen = vec![false; n];
    let mut post = Vec::with_capacity(n);
    let mut stack = vec![(cfg.entry, 0usize)];
    seen[cfg.entry] = true;
    while let Some(top) = stack.last_mut() {
        let (block, next) = *top;
        match successors[block].get(next) {
            Some(&to) => {
                top.1 += 1;
                if !seen[to] {
                    seen[to] = true;
                    stack.push((to, 0));
                }
            }
            None => {
                post.push(block);
                stack.pop();
            }
        }
    }
    let order: Vec<usize> = post.iter().rev().copied().collect();
    let mut rpo = vec![None; n];
    for (at, &block) in order.iter().enumerate() {
        rpo[block] = Some(at);
    }
    let mut predecessors = vec![Vec::new(); n];
    for &from in &order {
        for &to in &successors[from] {
            predecessors[to].push(from);
        }
    }
    // Cooper, Harvey and Kennedy's dominators.
    let mut idom: Vec<Option<usize>> = vec![None; n];
    idom[cfg.entry] = Some(cfg.entry);
    let at = |block: usize| rpo[block].expect("a reachable block");
    let mut changed = true;
    while changed {
        changed = false;
        for &block in order.iter().skip(1) {
            let mut new: Option<usize> = None;
            for &pred in &predecessors[block] {
                if idom[pred].is_none() {
                    continue;
                }
                new = Some(match new {
                    None => pred,
                    Some(mut other) => {
                        let mut pred = pred;
                        while pred != other {
                            while at(pred) > at(other) {
                                pred = idom[pred].expect("processed");
                            }
                            while at(other) > at(pred) {
                                other = idom[other].expect("processed");
                            }
                        }
                        pred
                    }
                });
            }
            if new.is_some() && idom[block] != new {
                idom[block] = new;
                changed = true;
            }
        }
    }
    let idom: Vec<usize> = (0..n).map(|block| idom[block].unwrap_or(block)).collect();
    let dominates = |a: usize, mut b: usize| loop {
        if a == b {
            return true;
        }
        if b == cfg.entry {
            return false;
        }
        b = idom[b];
    };
    let mut header = vec![false; n];
    let mut latches = vec![Vec::new(); n];
    let mut forward = vec![0u32; n];
    for &from in &order {
        for &to in &successors[from] {
            if at(to) <= at(from) {
                if !dominates(to, from) {
                    return Err(StructureError::Irreducible { from, to });
                }
                header[to] = true;
                latches[to].push(from);
            } else {
                forward[to] += 1;
            }
        }
    }
    let merge: Vec<bool> = forward.iter().map(|&count| count >= 2).collect();
    // Each header's natural loop: the header and what reaches a latch without passing it.
    let mut in_loop: Vec<Option<Vec<bool>>> = vec![None; n];
    for &head in &order {
        if !header[head] {
            continue;
        }
        let mut members = vec![false; n];
        members[head] = true;
        let mut work: Vec<usize> = latches[head].clone();
        while let Some(block) = work.pop() {
            if members[block] {
                continue;
            }
            members[block] = true;
            work.extend(predecessors[block].iter().copied());
        }
        in_loop[head] = Some(members);
    }
    let mut exit_of = vec![None; n];
    let mut exits = vec![Vec::new(); n];
    // Latest first, so each header's list is.
    for &block in order.iter().rev() {
        if block == cfg.entry {
            continue;
        }
        let dom = idom[block];
        // The outermost such loop is the first in reverse postorder.
        let found = order.iter().copied().find(|&head| {
            in_loop[head].as_ref().is_some_and(|members| members[dom] && !members[block])
        });
        if let Some(head) = found {
            exit_of[block] = Some(head);
            exits[head].push(block);
        }
    }
    let mut children = vec![Vec::new(); n];
    for &block in order.iter().skip(1) {
        children[idom[block]].push(block);
    }
    Ok(Analysis { rpo, idom, children, header, merge, exit_of, exits })
}

/// A label in scope while emitting.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Label {
    Loop(usize),
    /// The `Block` the code of this block follows.
    Before(usize),
}

struct Emit<'a, S, C> {
    cfg: &'a Cfg<S, C>,
    shape: Analysis,
    plant: Plant,
    labels: Vec<Label>,
}

impl<'a, S: Clone, C: Clone> Emit<'a, S, C> {
    fn at(&self, block: usize) -> usize {
        self.shape.rpo[block].expect("a reachable block")
    }

    fn depth(&self, label: Label) -> Option<u32> {
        self.labels.iter().rev().position(|it| *it == label).map(|it| it as u32)
    }

    /// The code of `block` and of everything it dominates.
    fn tree(&mut self, block: usize) -> Vec<Node<S, C>> {
        if !self.shape.header[block] {
            return self.within(block);
        }
        let exits = self.shape.exits[block].clone();
        self.followed_by(&exits, &mut |this: &mut Self| {
            this.labels.push(Label::Loop(block));
            let body = this.within(block);
            this.labels.pop();
            vec![Node::Loop(body)]
        })
    }

    /// `inner`, in a `Block` for each of `followers` (latest first) with that block's code
    /// after it.
    fn followed_by(
        &mut self,
        followers: &[usize],
        inner: &mut dyn FnMut(&mut Self) -> Vec<Node<S, C>>,
    ) -> Vec<Node<S, C>> {
        let Some((&last, earlier)) = followers.split_first() else { return inner(self) };
        self.labels.push(Label::Before(last));
        let body = self.followed_by(earlier, inner);
        self.labels.pop();
        let mut out = vec![Node::Block(body)];
        out.extend(self.tree(last));
        out
    }

    /// `block`'s own code, then the merge blocks it immediately dominates.
    fn within(&mut self, block: usize) -> Vec<Node<S, C>> {
        let mut merges: Vec<usize> = self.shape.children[block]
            .iter()
            .copied()
            .filter(|&child| self.shape.merge[child] && self.shape.exit_of[child].is_none())
            .collect();
        merges.reverse();
        self.followed_by(&merges, &mut |this: &mut Self| this.own(block))
    }

    fn own(&mut self, block: usize) -> Vec<Node<S, C>> {
        let cfg: &'a Cfg<S, C> = self.cfg;
        let data = &cfg.blocks[block];
        let mut out: Vec<Node<S, C>> = data.statements.iter().cloned().map(Node::Do).collect();
        match &data.exit {
            Exit::Goto(to) => out.extend(self.jump(block, *to)),
            Exit::Switch { on, targets, otherwise } => {
                let arms: Vec<(u128, Vec<Node<S, C>>)> =
                    targets.iter().map(|&(value, to)| (value, self.jump(block, to))).collect();
                let mut otherwise = self.jump(block, *otherwise);
                let join = match (arms.as_slice(), otherwise.last()) {
                    ([(_, first)], Some(Node::Break(to))) => match first.last() {
                        Some(Node::Break(other)) if other == to => Some(*to),
                        _ => None,
                    },
                    _ => None,
                };
                if let (Plant::ElseDropped, Some(to)) = (self.plant, join) {
                    otherwise = vec![Node::Break(to)];
                }
                out.push(Node::Branch { on: on.clone(), arms, otherwise });
            }
            Exit::Return => out.push(Node::Return),
            Exit::End(end) => out.push(Node::End(*end)),
        }
        out
    }

    fn jump(&mut self, from: usize, to: usize) -> Vec<Node<S, C>> {
        if self.at(to) <= self.at(from) {
            let depth = self.depth(Label::Loop(to)).expect("a back edge is inside its loop");
            return match self.plant {
                Plant::ContinueAsBreak => vec![Node::Break(depth)],
                _ => vec![Node::Continue(depth)],
            };
        }
        if let Some(depth) = self.depth(Label::Before(to)) {
            return vec![Node::Break(depth)];
        }
        debug_assert!(
            !self.shape.merge[to] && self.shape.exit_of[to].is_none(),
            "block {to} follows a `Block` that is not in scope at block {from} (its dominator \
             is {})",
            self.shape.idom[to]
        );
        self.tree(to)
    }
}

/// `cfg` as structured control flow, in the normal form the module describes.
pub fn reconstruct<S: Clone, C: Clone>(
    cfg: &Cfg<S, C>,
) -> Result<Vec<Node<S, C>>, StructureError> {
    reconstruct_planted(cfg, Plant::None)
}

pub(crate) fn reconstruct_planted<S: Clone, C: Clone>(
    cfg: &Cfg<S, C>,
    plant: Plant,
) -> Result<Vec<Node<S, C>>, StructureError> {
    let shape = analyse(cfg)?;
    let mut emit = Emit { cfg, shape, plant, labels: Vec::new() };
    let nodes = emit.tree(cfg.entry);
    Ok(normalize(nodes))
}

/// Removes the `Break(depth)` (`breaks`) or `Continue(depth)` that ends `nodes`: there control
/// reaches the end of the label's body anyway.
fn drop_tail_jump<S, C>(nodes: &mut Vec<Node<S, C>>, depth: u32, breaks: bool) {
    match nodes.last_mut() {
        Some(Node::Break(to)) if breaks && *to == depth => {
            nodes.pop();
        }
        Some(Node::Continue(to)) if !breaks && *to == depth => {
            nodes.pop();
        }
        Some(Node::Branch { arms, otherwise, .. }) => {
            for (_, arm) in arms {
                drop_tail_jump(arm, depth, breaks);
            }
            drop_tail_jump(otherwise, depth, breaks);
        }
        // The end of an inner block's body is the end of this one's.
        Some(Node::Block(body)) => drop_tail_jump(body, depth + 1, breaks),
        _ => {}
    }
}

/// Whether a jump in `nodes` goes to the label `depth` levels out of them.
fn jumps_to<S, C>(nodes: &[Node<S, C>], depth: u32) -> bool {
    nodes.iter().any(|node| match node {
        Node::Break(to) | Node::Continue(to) => *to == depth,
        Node::Branch { arms, otherwise, .. } => {
            arms.iter().any(|(_, arm)| jumps_to(arm, depth)) || jumps_to(otherwise, depth)
        }
        Node::Block(body) | Node::Loop(body) => jumps_to(body, depth + 1),
        Node::Do(_) | Node::Return | Node::End(_) => false,
    })
}

/// The label `depth` levels out of `nodes` is gone: every jump past it counts one less.
fn unlabel<S, C>(nodes: &mut [Node<S, C>], depth: u32) {
    for node in nodes {
        match node {
            Node::Break(to) | Node::Continue(to) => {
                if *to > depth {
                    *to -= 1;
                }
            }
            Node::Branch { arms, otherwise, .. } => {
                for (_, arm) in arms {
                    unlabel(arm, depth);
                }
                unlabel(otherwise, depth);
            }
            Node::Block(body) | Node::Loop(body) => unlabel(body, depth + 1),
            Node::Do(_) | Node::Return | Node::End(_) => {}
        }
    }
}

/// The normal form: see the module.
pub fn normalize<S, C>(nodes: Vec<Node<S, C>>) -> Vec<Node<S, C>> {
    let mut out = Vec::with_capacity(nodes.len());
    for node in nodes {
        match node {
            Node::Branch { on, arms, otherwise } => out.push(Node::Branch {
                on,
                arms: arms.into_iter().map(|(value, arm)| (value, normalize(arm))).collect(),
                otherwise: normalize(otherwise),
            }),
            Node::Block(body) => {
                let mut body = normalize(body);
                drop_tail_jump(&mut body, 0, true);
                if jumps_to(&body, 0) {
                    out.push(Node::Block(body));
                } else {
                    unlabel(&mut body, 0);
                    out.extend(body);
                }
            }
            Node::Loop(body) => {
                let mut body = normalize(body);
                drop_tail_jump(&mut body, 0, false);
                out.push(Node::Loop(body));
            }
            other => out.push(other),
        }
    }
    out
}

/// What is wrong with the labels of `nodes`, if anything: a jump past the outermost label, a
/// `Break` of a `Loop` or a `Continue` of a `Block`.
pub fn ill_formed<S, C>(nodes: &[Node<S, C>]) -> Option<String> {
    fn walk<S, C>(nodes: &[Node<S, C>], labels: &mut Vec<bool>) -> Option<String> {
        for node in nodes {
            let found = match node {
                Node::Break(to) | Node::Continue(to) => {
                    let is_continue = matches!(node, Node::Continue(_));
                    match labels.iter().rev().nth(*to as usize) {
                        None => Some(format!("a jump {to} labels out, past the outermost")),
                        Some(&is_loop) if is_loop != is_continue => Some(format!(
                            "a `{}` of a `{}`",
                            if is_continue { "Continue" } else { "Break" },
                            if is_loop { "Loop" } else { "Block" }
                        )),
                        Some(_) => None,
                    }
                }
                Node::Branch { arms, otherwise, .. } => arms
                    .iter()
                    .find_map(|(_, arm)| walk(arm, labels))
                    .or_else(|| walk(otherwise, labels)),
                Node::Block(body) | Node::Loop(body) => {
                    labels.push(matches!(node, Node::Loop(_)));
                    let found = walk(body, labels);
                    labels.pop();
                    found
                }
                Node::Do(_) | Node::Return | Node::End(_) => None,
            };
            if found.is_some() {
                return found;
            }
        }
        None
    }
    walk(nodes, &mut Vec::new())
}

/// Structured code as a graph: the inverse of [`reconstruct`] up to the normal form. Reaching
/// the end of `nodes` returns.
pub fn lower<S: Clone, C: Clone>(nodes: &[Node<S, C>]) -> Cfg<S, C> {
    struct Lower<S, C> {
        blocks: Vec<(Vec<S>, Option<Exit<C>>)>,
        /// Where a jump to each label in scope goes, innermost last.
        labels: Vec<usize>,
    }
    impl<S: Clone, C: Clone> Lower<S, C> {
        fn block(&mut self) -> usize {
            self.blocks.push((Vec::new(), None));
            self.blocks.len() - 1
        }
        /// Lowers `nodes` from the open block `at`; the block left open at their end, if
        /// control reaches it.
        fn seq(&mut self, nodes: &[Node<S, C>], mut at: usize) -> Option<usize> {
            for node in nodes {
                match node {
                    Node::Do(statement) => self.blocks[at].0.push(statement.clone()),
                    Node::Branch { on, arms, otherwise } => {
                        let mut ends = Vec::new();
                        let mut targets = Vec::with_capacity(arms.len());
                        for (value, arm) in arms {
                            let start = self.block();
                            targets.push((*value, start));
                            ends.extend(self.seq(arm, start));
                        }
                        let start = self.block();
                        ends.extend(self.seq(otherwise, start));
                        self.blocks[at].1 =
                            Some(Exit::Switch { on: on.clone(), targets, otherwise: start });
                        if ends.is_empty() {
                            return None;
                        }
                        at = self.block();
                        for end in ends {
                            self.blocks[end].1 = Some(Exit::Goto(at));
                        }
                    }
                    Node::Block(body) => {
                        let after = self.block();
                        self.labels.push(after);
                        let end = self.seq(body, at);
                        self.labels.pop();
                        if let Some(end) = end {
                            self.blocks[end].1 = Some(Exit::Goto(after));
                        }
                        at = after;
                    }
                    Node::Loop(body) => {
                        let head = self.block();
                        self.blocks[at].1 = Some(Exit::Goto(head));
                        self.labels.push(head);
                        let end = self.seq(body, head);
                        self.labels.pop();
                        if let Some(end) = end {
                            self.blocks[end].1 = Some(Exit::Goto(head));
                        }
                        return None;
                    }
                    Node::Break(to) | Node::Continue(to) => {
                        let target = self.labels[self.labels.len() - 1 - *to as usize];
                        self.blocks[at].1 = Some(Exit::Goto(target));
                        return None;
                    }
                    Node::Return => {
                        self.blocks[at].1 = Some(Exit::Return);
                        return None;
                    }
                    Node::End(end) => {
                        self.blocks[at].1 = Some(Exit::End(*end));
                        return None;
                    }
                }
            }
            Some(at)
        }
    }
    let mut lower = Lower { blocks: Vec::new(), labels: Vec::new() };
    let entry = lower.block();
    if let Some(end) = lower.seq(nodes, entry) {
        lower.blocks[end].1 = Some(Exit::Return);
    }
    Cfg {
        blocks: lower
            .blocks
            .into_iter()
            .map(|(statements, exit)| CfgBlock {
                statements,
                // A block nothing jumps to (the join of arms that all leave).
                exit: exit.unwrap_or(Exit::End(End::Unreachable)),
            })
            .collect(),
        entry,
    }
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    Returned,
    Stopped(End),
    /// The step budget ran out.
    OutOfSteps,
}

/// What runs statements and evaluates what a switch tests: the semantics both forms share.
pub trait Machine<S, C> {
    /// Runs `statement`; `Err` when it panics.
    fn step(&mut self, statement: &S) -> Result<(), End>;
    fn test(&mut self, on: &C) -> u128;
}

/// Runs the graph for at most `steps` statements and jumps.
pub fn run_cfg<S, C>(cfg: &Cfg<S, C>, machine: &mut impl Machine<S, C>, mut steps: u64) -> Ended {
    let mut at = cfg.entry;
    loop {
        let block = &cfg.blocks[at];
        for statement in &block.statements {
            if steps == 0 {
                return Ended::OutOfSteps;
            }
            steps -= 1;
            if let Err(end) = machine.step(statement) {
                return Ended::Stopped(end);
            }
        }
        if steps == 0 {
            return Ended::OutOfSteps;
        }
        steps -= 1;
        at = match &block.exit {
            Exit::Goto(to) => *to,
            Exit::Switch { on, targets, otherwise } => {
                let value = machine.test(on);
                targets.iter().find(|(it, _)| *it == value).map_or(*otherwise, |(_, to)| *to)
            }
            Exit::Return => return Ended::Returned,
            Exit::End(end) => return Ended::Stopped(*end),
        };
    }
}

enum Flow {
    Next,
    /// A jump to the label this many levels out; whether it is a `Continue`.
    Jump(u32, bool),
    Ended(Ended),
}

/// Runs structured code for at most `steps` statements, branches and loop turns. A `Break` of
/// a `Loop` leaves the loop (what a planted bug makes).
pub fn run_structured<S, C>(
    nodes: &[Node<S, C>],
    machine: &mut impl Machine<S, C>,
    steps: u64,
) -> Ended {
    fn seq<S, C>(
        nodes: &[Node<S, C>],
        machine: &mut impl Machine<S, C>,
        steps: &mut u64,
    ) -> Flow {
        for node in nodes {
            if *steps == 0 {
                return Flow::Ended(Ended::OutOfSteps);
            }
            *steps -= 1;
            match node {
                Node::Do(statement) => {
                    if let Err(end) = machine.step(statement) {
                        return Flow::Ended(Ended::Stopped(end));
                    }
                }
                Node::Branch { on, arms, otherwise } => {
                    let value = machine.test(on);
                    let arm =
                        arms.iter().find(|(it, _)| *it == value).map_or(otherwise, |(_, arm)| arm);
                    match seq(arm, &mut *machine, steps) {
                        Flow::Next => {}
                        flow => return flow,
                    }
                }
                Node::Block(body) => match seq(body, &mut *machine, steps) {
                    Flow::Next | Flow::Jump(0, _) => {}
                    Flow::Jump(to, is_continue) => return Flow::Jump(to - 1, is_continue),
                    flow => return flow,
                },
                Node::Loop(body) => loop {
                    if *steps == 0 {
                        return Flow::Ended(Ended::OutOfSteps);
                    }
                    *steps -= 1;
                    match seq(body, &mut *machine, steps) {
                        Flow::Next | Flow::Jump(0, true) => {}
                        Flow::Jump(0, false) => break,
                        Flow::Jump(to, is_continue) => return Flow::Jump(to - 1, is_continue),
                        flow => return flow,
                    }
                },
                Node::Break(to) => return Flow::Jump(*to, false),
                Node::Continue(to) => return Flow::Jump(*to, true),
                Node::Return => return Flow::Ended(Ended::Returned),
                Node::End(end) => return Flow::Ended(Ended::Stopped(*end)),
            }
        }
        Flow::Next
    }
    let mut steps = steps;
    match seq(nodes, machine, &mut steps) {
        // Reaching the end returns, as in `lower`; a jump past the outermost label has
        // nowhere to go but out.
        Flow::Next | Flow::Jump(..) => Ended::Returned,
        Flow::Ended(ended) => ended,
    }
}
