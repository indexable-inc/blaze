//! A function's control-flow graph, as naga's structured control flow.
//!
//! MIR is a graph of basic blocks; naga's IR has only `if`, `loop` (which `break` leaves and
//! `continue` restarts, both to the innermost loop), and `return`. Two steps get from one to the
//! other:
//!
//! 1. [`structure`]: Ramsey's "Beyond Relooper" (ICFP 2022) over the dominator tree. Each node
//!    becomes code where its immediate dominator's code is; a loop header wraps its subtree in a
//!    [`Stmt::Loop`]; a node with two or more forward edges into it (a merge) follows a
//!    [`Stmt::Block`] that the branches to it leave. A branch is a [`Stmt::Br`]: back to a loop
//!    header (`continue`) or out of a block to its merge (`break` out of it). The result is exact:
//!    it runs the blocks the graph runs, in the same order, for every choice of branches. It needs a
//!    reducible graph (every loop entered only through its header), which MIR from Rust source is,
//!    having no `goto`; an irreducible one is [`Error::Irreducible`], naming the edge, never a
//!    dispatch loop (which would put every block in non-uniform control flow, where a derivative or
//!    an implicit-LOD sample is undefined).
//! 2. [`shape`]: naga's form. Branches that leave from the tail of the block they leave become
//!    falling through, and a block nothing leaves any more is spliced in place ([`simplify`]). Each
//!    remaining block and loop is a naga `loop` ending in `break`; a branch out of the innermost
//!    one is `break` or `continue`, and a branch out of more than one sets the flag to its target
//!    and breaks, and after each loop it crossed, a check of the flag breaks or continues on.
//!
//! The statements name blocks ([`Stmt::Run`]: run block b's statements), so the MIR lowering emits
//! each block's code at its `Run` and its terminator's condition at its [`Stmt::Select`].
use std::fmt;

/// A basic block's index; block 0 is the entry.
pub type BlockId = u32;

/// A control-flow graph: each block's successors, in the order of its terminator's arms (a
/// `SwitchInt`'s targets, then its otherwise). A block with none leaves the function (a return).
/// A successor may repeat (two arms to one block).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cfg {
    pub succ: Vec<Vec<BlockId>>,
}

/// Where a [`Stmt::Br`] goes.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// To the top of the [`Stmt::Loop`] headed by this block.
    Continue(BlockId),
    /// Out of the [`Stmt::Block`] followed by this block.
    Exit(BlockId),
}

/// Structured control flow with labelled exits, as [`structure`] makes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stmt {
    /// Run the block's statements.
    Run(BlockId),
    /// The block's terminator leaves the function.
    Exit(BlockId),
    /// The block's terminator picks one arm, arm `i` for its successor `i`.
    Select { block: BlockId, arms: Vec<Vec<Stmt>> },
    /// Run `body`; a `Br(Continue(header))` in it runs it again. Falling off its end leaves it.
    Loop { header: BlockId, body: Vec<Stmt> },
    /// Run `body`; a `Br(Exit(merge))` in it lands just after it, where `merge`'s code is.
    Block { merge: BlockId, body: Vec<Stmt> },
    Br(Target),
}

/// naga's structured control flow: loops whose `break` and `continue` reach only the innermost
/// one, and one flag (a `u32` local, 0 when no branch is under way) carrying a branch out of more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shaped {
    Run(BlockId),
    Exit(BlockId),
    Select { block: BlockId, arms: Vec<Vec<Shaped>> },
    /// naga's `loop`: runs `body` again when it falls off the end, until a `Break`.
    Loop(Vec<Shaped>),
    Break,
    Continue,
    /// `flag = value` (never 0).
    SetFlag(u32),
    /// `flag = 0`.
    ClearFlag,
    /// `if flag == value { then }`.
    IfFlag { value: u32, then: Vec<Shaped> },
    /// `if flag != 0 { then }`.
    IfAnyFlag(Vec<Shaped>),
}

/// Why a graph has no structured form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The graph has no blocks.
    Empty,
    /// A successor that is not a block.
    NoBlock { from: BlockId, to: BlockId },
    /// The edge `from -> to` goes back to `to`, which doesn't dominate `from`: a loop entered other
    /// than through its header.
    Irreducible { from: BlockId, to: BlockId },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Empty => f.write_str("structuring control flow: the function has no blocks"),
            Error::NoBlock { from, to } => {
                write!(f, "structuring control flow: bb{from} branches to bb{to}, which is not a block")
            }
            Error::Irreducible { from, to } => write!(
                f,
                "structuring control flow: the edge bb{from} -> bb{to} enters a loop at bb{to}, which does not \
                 dominate bb{from}; a shader's control flow must be reducible (docs/future/gpu/shaders.md)"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// A deliberate bug for the tests' planted-bug controls (`testing` skill): the oracle must catch
/// each.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum Plant {
    None,
    /// Nest a node's merge blocks the wrong way round (the earliest outermost).
    MergesInsideOut,
    /// Leave out the flag check after a loop a branch crossed.
    NoFlagCheck,
}

/// The graph's structured form with labelled exits.
pub fn structure(cfg: &Cfg) -> Result<Vec<Stmt>, Error> {
    structure_with(cfg, Plant::None)
}

/// The graph's structured form in naga's shape: [`structure`], [`simplify`], [`shape`].
pub fn structured(cfg: &Cfg) -> Result<Vec<Shaped>, Error> {
    Ok(shape(simplify(structure(cfg)?)))
}

pub(crate) fn structure_with(cfg: &Cfg, plant: Plant) -> Result<Vec<Stmt>, Error> {
    let g = Graph::new(cfg)?;
    Ok(g.tree(0, plant))
}

/// What [`structure`] needs to know of the graph.
struct Graph<'a> {
    cfg: &'a Cfg,
    /// Each block's place in reverse postorder; `u32::MAX` for an unreachable block.
    rpo: Vec<u32>,
    /// Each reachable block's immediate dominator (the entry's is itself).
    idom: Vec<BlockId>,
    /// Blocks some edge goes back to.
    header: Vec<bool>,
    /// Blocks two or more forward edges go to.
    merge: Vec<bool>,
    /// Each block's children in the dominator tree, in reverse postorder.
    children: Vec<Vec<BlockId>>,
}

const UNREACHED: u32 = u32::MAX;

impl<'a> Graph<'a> {
    fn new(cfg: &'a Cfg) -> Result<Self, Error> {
        let n = cfg.succ.len();
        if n == 0 {
            return Err(Error::Empty);
        }
        for (from, succ) in cfg.succ.iter().enumerate() {
            if let Some(&to) = succ.iter().find(|&&to| to as usize >= n) {
                return Err(Error::NoBlock { from: from as BlockId, to });
            }
        }

        // Reverse postorder of a depth-first walk from the entry, without recursion.
        // alloc: per function structured, dropped with the graph
        let mut seen = vec![false; n];
        // alloc: as above
        let mut post: Vec<BlockId> = Vec::with_capacity(n);
        // alloc: as above
        let mut stack: Vec<(BlockId, usize)> = vec![(0, 0)];
        seen[0] = true;
        while let Some(top) = stack.last_mut() {
            let (b, i) = *top;
            match cfg.succ[b as usize].get(i) {
                Some(&s) => {
                    top.1 += 1;
                    if !seen[s as usize] {
                        seen[s as usize] = true;
                        stack.push((s, 0));
                    }
                }
                None => {
                    post.push(b);
                    stack.pop();
                }
            }
        }
        post.reverse();
        let order = post;
        // alloc: as above
        let mut rpo = vec![UNREACHED; n];
        for (i, &b) in order.iter().enumerate() {
            rpo[b as usize] = i as u32;
        }

        // alloc: as above
        let mut preds: Vec<Vec<BlockId>> = vec![Vec::new(); n];
        for &b in &order {
            for &s in &cfg.succ[b as usize] {
                preds[s as usize].push(b);
            }
        }

        // Dominators: Cooper, Harvey and Kennedy's iteration over reverse postorder. On a reducible
        // graph it settles in two passes; on any graph within one pass per block.
        // alloc: as above
        let mut idom = vec![UNREACHED; n];
        idom[0] = 0;
        let intersect = |idom: &[BlockId], mut a: BlockId, mut b: BlockId| {
            while a != b {
                while rpo[a as usize] > rpo[b as usize] {
                    a = idom[a as usize];
                }
                while rpo[b as usize] > rpo[a as usize] {
                    b = idom[b as usize];
                }
            }
            a
        };
        let mut settled = false;
        for _round in 0..=n {
            let mut changed = false;
            for &b in order.iter().skip(1) {
                let mut new = UNREACHED;
                for &p in &preds[b as usize] {
                    if idom[p as usize] != UNREACHED {
                        new = if new == UNREACHED { p } else { intersect(&idom, p, new) };
                    }
                }
                if idom[b as usize] != new {
                    idom[b as usize] = new;
                    changed = true;
                }
            }
            if !changed {
                settled = true;
                break;
            }
        }
        assert!(settled, "dominators of a {n}-block graph did not settle in {} passes", n + 1);

        let dominates = |a: BlockId, mut b: BlockId| loop {
            if a == b {
                return true;
            }
            if b == 0 {
                return false;
            }
            b = idom[b as usize];
        };

        // alloc: as above
        let mut header = vec![false; n];
        // alloc: as above
        let mut forward_in = vec![0u32; n];
        for &u in &order {
            for &s in &cfg.succ[u as usize] {
                if rpo[s as usize] <= rpo[u as usize] {
                    if !dominates(s, u) {
                        return Err(Error::Irreducible { from: u, to: s });
                    }
                    header[s as usize] = true;
                } else {
                    forward_in[s as usize] += 1;
                }
            }
        }
        let merge = forward_in.iter().map(|&k| k >= 2).collect();

        // alloc: as above
        let mut children: Vec<Vec<BlockId>> = vec![Vec::new(); n];
        for &b in order.iter().skip(1) {
            children[idom[b as usize] as usize].push(b);
        }
        Ok(Graph { cfg, rpo, idom, header, merge, children })
    }

    /// The code of `x` and everything it dominates (Ramsey's `doTree`).
    fn tree(&self, x: BlockId, plant: Plant) -> Vec<Stmt> {
        // alloc: one function's statements, kept until it is lowered
        let mut merges: Vec<BlockId> = self.children[x as usize].iter().copied().filter(|&c| self.merge[c as usize]).collect();
        if plant == Plant::MergesInsideOut {
            merges.reverse();
        }
        let code = self.node(x, &merges, plant);
        if self.header[x as usize] {
            // alloc: as above
            vec![Stmt::Loop { header: x, body: code }]
        } else {
            code
        }
    }

    /// `x`'s own code inside the blocks of its merge children `merges` (in reverse postorder: the
    /// last one's block is outermost, since its code comes last), each followed by its code.
    fn node(&self, x: BlockId, merges: &[BlockId], plant: Plant) -> Vec<Stmt> {
        if let Some((&y, inner)) = merges.split_last() {
            // alloc: as above
            let mut out = vec![Stmt::Block { merge: y, body: self.node(x, inner, plant) }];
            out.extend(self.tree(y, plant));
            return out;
        }
        // alloc: as above
        let mut out = vec![Stmt::Run(x)];
        let succ = &self.cfg.succ[x as usize];
        match succ.as_slice() {
            [] => out.push(Stmt::Exit(x)),
            [s] => out.extend(self.branch(x, *s, plant)),
            arms => out.push(Stmt::Select { block: x, arms: arms.iter().map(|&s| self.branch(x, s, plant)).collect() }),
        }
        out
    }

    /// The edge `x -> y` (Ramsey's `doBranch`): back to a loop, out to a merge, or `y`'s own code
    /// in place when this is its one way in.
    fn branch(&self, x: BlockId, y: BlockId, plant: Plant) -> Vec<Stmt> {
        if self.rpo[y as usize] <= self.rpo[x as usize] {
            // alloc: as above
            vec![Stmt::Br(Target::Continue(y))]
        } else if self.merge[y as usize] {
            // alloc: as above
            vec![Stmt::Br(Target::Exit(y))]
        } else {
            debug_assert_eq!(self.idom[y as usize], x, "bb{y}'s one way in is from bb{x}, which must dominate it");
            self.tree(y, plant)
        }
    }
}

/// Branches out of a block from its tail become falling through, and a block no branch leaves any
/// more is spliced in place. Runs the same blocks in the same order as its input.
pub fn simplify(stmts: Vec<Stmt>) -> Vec<Stmt> {
    // alloc: one function's statements, kept until it is lowered
    let mut out = Vec::with_capacity(stmts.len());
    for s in stmts {
        match s {
            Stmt::Block { merge, body } => {
                let mut body = simplify(body);
                drop_tail_exit(&mut body, merge);
                if leaves(&body, Target::Exit(merge)) {
                    out.push(Stmt::Block { merge, body });
                } else {
                    out.extend(body);
                }
            }
            Stmt::Loop { header, body } => out.push(Stmt::Loop { header, body: simplify(body) }),
            Stmt::Select { block, arms } => out.push(Stmt::Select { block, arms: arms.into_iter().map(simplify).collect() }),
            s @ (Stmt::Run(_) | Stmt::Exit(_) | Stmt::Br(_)) => out.push(s),
        }
    }
    out
}

/// Removes `Br(Exit(merge))` wherever it is the last thing run before falling off the end of
/// `list`, which then falls through to the same place.
fn drop_tail_exit(list: &mut Vec<Stmt>, merge: BlockId) {
    match list.last_mut() {
        Some(Stmt::Br(Target::Exit(y))) if *y == merge => {
            list.pop();
        }
        Some(Stmt::Select { arms, .. }) => {
            for arm in arms {
                drop_tail_exit(arm, merge);
            }
        }
        // Falling off a block's or a loop's body falls off it, so their tails are this list's tail.
        Some(Stmt::Block { body, .. } | Stmt::Loop { body, .. }) => drop_tail_exit(body, merge),
        _ => {}
    }
}

/// Whether a `Br(target)` is anywhere in `list`.
fn leaves(list: &[Stmt], target: Target) -> bool {
    list.iter().any(|s| match s {
        Stmt::Br(t) => *t == target,
        Stmt::Select { arms, .. } => arms.iter().any(|a| leaves(a, target)),
        Stmt::Block { body, .. } | Stmt::Loop { body, .. } => leaves(body, target),
        Stmt::Run(_) | Stmt::Exit(_) => false,
    })
}

/// The flag's value for a branch to `t`: never 0, distinct for every target.
pub fn flag(t: Target) -> u32 {
    match t {
        Target::Exit(y) => 2 * y + 1,
        Target::Continue(h) => 2 * h + 2,
    }
}

/// naga's shape of labelled structured control flow: every block and loop a naga loop ending in a
/// `Break`, and branches out of more than one through the flag.
pub fn shape(stmts: Vec<Stmt>) -> Vec<Shaped> {
    shape_with(stmts, Plant::None)
}

pub(crate) fn shape_with(stmts: Vec<Stmt>, plant: Plant) -> Vec<Shaped> {
    // alloc: one function's shaping, dropped with it
    let mut s = Shaper { frames: Vec::new(), plant };
    s.list(stmts)
}

/// One labelled construct being shaped, as a naga loop.
struct Frame {
    /// What a `Br` lands on by leaving it with `break`.
    exit: Option<BlockId>,
    /// What a `Br` lands on by restarting it with `continue`.
    cont: Option<BlockId>,
    /// Whether a branch to something outside it goes through it, so it needs the flag check after.
    crossed: bool,
}

struct Shaper {
    frames: Vec<Frame>,
    plant: Plant,
}

impl Shaper {
    fn list(&mut self, stmts: Vec<Stmt>) -> Vec<Shaped> {
        // alloc: one function's statements, kept until it is lowered
        let mut out = Vec::with_capacity(stmts.len());
        for s in stmts {
            self.stmt(s, &mut out);
        }
        out
    }

    fn stmt(&mut self, s: Stmt, out: &mut Vec<Shaped>) {
        match s {
            Stmt::Run(b) => out.push(Shaped::Run(b)),
            Stmt::Exit(b) => out.push(Shaped::Exit(b)),
            Stmt::Select { block, arms } => {
                let arms = arms.into_iter().map(|a| self.list(a)).collect();
                out.push(Shaped::Select { block, arms });
            }
            Stmt::Br(t) => {
                let at = self.frames.iter().rposition(|f| match t {
                    Target::Exit(y) => f.exit == Some(y),
                    Target::Continue(h) => f.cont == Some(h),
                });
                let Some(at) = at else {
                    panic!("structuring control flow: a branch to {t:?} outside the construct it names")
                };
                if at + 1 == self.frames.len() {
                    out.push(match t {
                        Target::Exit(_) => Shaped::Break,
                        Target::Continue(_) => Shaped::Continue,
                    });
                } else {
                    for f in &mut self.frames[at + 1..] {
                        f.crossed = true;
                    }
                    out.push(Shaped::SetFlag(flag(t)));
                    out.push(Shaped::Break);
                }
            }
            Stmt::Loop { header, body } => self.construct(Frame { exit: None, cont: Some(header), crossed: false }, body, out),
            Stmt::Block { merge, body } => self.construct(Frame { exit: Some(merge), cont: None, crossed: false }, body, out),
        }
    }

    fn construct(&mut self, frame: Frame, body: Vec<Stmt>, out: &mut Vec<Shaped>) {
        self.frames.push(frame);
        let mut body = self.list(body);
        // Falling off a block or a loop leaves it; a naga loop would run again.
        body.push(Shaped::Break);
        let frame = self.frames.pop().expect("the frame pushed above");
        out.push(Shaped::Loop(body));
        if !frame.crossed || self.plant == Plant::NoFlagCheck {
            return;
        }
        let parent = self.frames.last().expect("a branch crossed this loop to reach a construct outside it");
        // alloc: as above
        let mut check = Vec::with_capacity(3);
        if let Some(y) = parent.exit {
            // alloc: as above
            check.push(Shaped::IfFlag { value: flag(Target::Exit(y)), then: vec![Shaped::ClearFlag, Shaped::Break] });
        }
        if let Some(h) = parent.cont {
            // alloc: as above
            check.push(Shaped::IfFlag { value: flag(Target::Continue(h)), then: vec![Shaped::ClearFlag, Shaped::Continue] });
        }
        check.push(Shaped::Break);
        out.push(Shaped::IfAnyFlag(check));
    }
}

#[cfg(test)]
#[path = "structure/tests.rs"]
mod tests;
