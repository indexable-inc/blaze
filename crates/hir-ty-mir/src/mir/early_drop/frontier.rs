//! Where early drops go, decided from facts alone (no MIR here: `super` reads a body into these
//! facts and writes the answer back, and the property test below makes them from generated
//! programs).
//!
//! A local is *held* where it is live or a local that may hold a borrow of it is. Which locals
//! may hold a borrow of which is [`Owners`], the least relation the [`Tie`]s of the body
//! allow, flow-insensitive. An early local is dropped on each edge `from -> to` where it was
//! held or touched in `from`, is not held on entry to `to`, has storage, and was not last moved
//! out ([`edges`]). Several on one edge go in the order of their ranks, then by descending
//! index: never by an order a hash or an address gives.

use blaze_mem::Frame;

use crate::mir::framed::{Bits, bits_in};

/// No slot: the local is not one early drops are placed for.
const NONE: u32 = u32::MAX;

/// One way a local comes to hold what another holds. The locals are indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tie {
    /// `into = &of` (or `&of.f`, `&*of`): `into` may hold a borrow of `of` and of everything
    /// `of` may hold a borrow of.
    Borrow { into: u32, of: u32 },
    /// `into = from`, where `into`'s type cannot tie lifetimes back (a reference to a type with
    /// no lifetime in it, a raw pointer): `into` may hold what `from` does.
    Flow { into: u32, from: u32 },
    /// The two meet in one statement or call and both have a lifetime in their type: either may
    /// come to hold what the other does (an argument stored through another, a lifetime two
    /// types share).
    Meet(u32, u32),
    /// `of` is a reference to `is`, which has a lifetime in its type: what `of` may hold may be
    /// stored through it into `is`.
    Referent { of: u32, is: u32 },
}

/// A bug planted for the controls of the property test: the safeguard it removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(not(test), expect(dead_code, reason = "only the tests plant a bug"))]
pub(super) enum Plant {
    #[default]
    None,
    /// The ties are ignored: a local is held only where it is live itself.
    OwnLivenessOnly,
    /// Scoped locals are placed as early ones are.
    ScopedToo,
    /// The locals of one edge are ordered by a hash of their index.
    HashOrder,
}

/// What a block says of the locals.
pub(super) struct Block<'f> {
    /// Live on entry, where a drop of an early local is no use and a drop of a scoped one is.
    pub(super) live_in: Bits<'f>,
    /// Used or assigned in the block (a drop of the whole local and its storage aside).
    pub(super) touched: Bits<'f>,
    /// Whose last mention in the block moves the whole local out: nothing is left to drop.
    pub(super) moved_last: Bits<'f>,
    /// Have storage where the block ends.
    pub(super) storage_out: Bits<'f>,
    /// Whose drop at the end of their scope is what the block starts with, storage ends and
    /// other such drops aside: a drop on an edge into it would be that drop again.
    pub(super) ends: Bits<'f>,
    /// The blocks an edge from here may carry a drop to, by index.
    pub(super) successors: Vec<u32, &'f Frame>,
    /// A cleanup block, or one nothing reaches: no drop goes into or out of it.
    pub(super) closed: bool,
}

/// The facts of a body.
pub(super) struct Input<'a, 'f> {
    pub(super) locals: usize,
    /// The locals to drop early.
    pub(super) early: &'a Bits<'f>,
    /// The locals with drop glue that keep their scope's drop.
    pub(super) scoped: &'a Bits<'f>,
    /// Per local, where its scope's end comes among the scopes' ends: the lower is dropped
    /// first. `u32::MAX` for a local no scope holds.
    pub(super) rank: &'a [u32],
    pub(super) blocks: &'a [Block<'f>],
}

/// The drops of one edge, in the order they run.
#[derive(Debug)]
pub(super) struct EdgeDrops<'f> {
    pub(super) from: u32,
    pub(super) to: u32,
    pub(super) locals: Vec<u32, &'f Frame>,
}

/// Which locals may hold a borrow of which candidate: a row of slots per local.
pub(super) struct Owners<'f> {
    /// Per local, its slot among the candidates, or [`NONE`].
    slot_of: Vec<u32, &'f Frame>,
    /// Per slot, its local.
    local_of: Vec<u32, &'f Frame>,
    /// Per local, the slots of the candidates it may hold a borrow of.
    rows: Vec<Bits<'f>, &'f Frame>,
}

impl<'f> Owners<'f> {
    /// The least relation `ties` allow over `candidates`, the locals that may be dropped
    /// early (`scoped` too, under [`Plant::ScopedToo`]).
    pub(super) fn new(
        frame: &'f Frame,
        locals: usize,
        candidates: &Bits<'_>,
        scoped: &Bits<'_>,
        ties: &[Tie],
        plant: Plant,
    ) -> Owners<'f> {
        let mut slot_of = Vec::with_capacity_in(locals, frame);
        let mut local_of = Vec::new_in(frame);
        for local in 0..locals {
            let placed = candidates.contains(local)
                || (plant == Plant::ScopedToo && scoped.contains(local));
            slot_of.push(match placed {
                true => {
                    local_of.push(local as u32);
                    local_of.len() as u32 - 1
                }
                false => NONE,
            });
        }
        let slots = local_of.len();
        let mut rows = Vec::with_capacity_in(locals, frame);
        rows.extend((0..locals).map(|_| bits_in(slots, frame)));
        let mut owners = Owners { slot_of, local_of, rows };
        if plant == Plant::OwnLivenessOnly || slots == 0 {
            return owners;
        }
        let mut scratch: Bits<'f> = bits_in(slots, frame);
        // Each round but the last adds a bit, of `locals * slots` in all.
        for _ in 0..=locals.saturating_mul(slots) {
            let mut changed = false;
            for &tie in ties {
                changed |= owners.apply(tie, &mut scratch);
            }
            if !changed {
                break;
            }
        }
        owners
    }

    /// Adds what `tie` allows; whether a row grew.
    fn apply(&mut self, tie: Tie, scratch: &mut Bits<'f>) -> bool {
        match tie {
            Tie::Borrow { into, of } => {
                scratch.copy_from(&self.rows[of as usize]);
                if let Some(slot) = self.slot(of) {
                    scratch.insert(slot);
                }
                self.rows[into as usize].union_with(scratch)
            }
            Tie::Flow { into, from } => {
                scratch.copy_from(&self.rows[from as usize]);
                self.rows[into as usize].union_with(scratch)
            }
            Tie::Meet(a, b) => {
                scratch.copy_from(&self.rows[a as usize]);
                let mut changed = self.rows[b as usize].union_with(scratch);
                scratch.copy_from(&self.rows[b as usize]);
                changed |= self.rows[a as usize].union_with(scratch);
                changed
            }
            Tie::Referent { of, is } => {
                scratch.copy_from(&self.rows[of as usize]);
                if let Some(slot) = self.slot(is) {
                    scratch.remove(slot);
                }
                self.rows[is as usize].union_with(scratch)
            }
        }
    }

    fn slot(&self, local: u32) -> Option<usize> {
        match self.slot_of[local as usize] {
            NONE => None,
            slot => Some(slot as usize),
        }
    }

    fn slots(&self) -> usize {
        self.local_of.len()
    }

    /// Calls `f` with each candidate `holder` may hold a borrow of, in the order of the locals.
    pub(super) fn held_by(&self, holder: usize, mut f: impl FnMut(usize)) {
        for slot in self.rows[holder].iter() {
            f(self.local_of[slot] as usize);
        }
    }

    /// The slots held where the locals of `set` are live or touched: their own and those they
    /// may hold a borrow of.
    fn held_into(&self, set: &Bits<'_>, out: &mut Bits<'f>) {
        for local in set.iter() {
            if let Some(slot) = self.slot(local as u32) {
                out.insert(slot);
            }
            out.union_with(&self.rows[local]);
        }
    }
}

/// The early drops of each edge, edges in the order of their blocks and then of the block's
/// successors, each edge once.
pub(super) fn edges<'f>(
    frame: &'f Frame,
    input: &Input<'_, 'f>,
    owners: &Owners<'f>,
    plant: Plant,
) -> Vec<EdgeDrops<'f>, &'f Frame> {
    let slots = owners.slots();
    let mut found = Vec::new_in(frame);
    if slots == 0 {
        return found;
    }
    let mut held_in: Vec<Bits<'f>, &Frame> = Vec::with_capacity_in(input.blocks.len(), frame);
    for block in input.blocks {
        let mut held: Bits<'f> = bits_in(slots, frame);
        owners.held_into(&block.live_in, &mut held);
        held_in.push(held);
    }
    let mut active: Bits<'f> = bits_in(slots, frame);
    for (from, block) in input.blocks.iter().enumerate() {
        if block.closed || block.successors.is_empty() {
            continue;
        }
        active.copy_from(&held_in[from]);
        owners.held_into(&block.touched, &mut active);
        for (at, &to) in block.successors.iter().enumerate() {
            let target = &input.blocks[to as usize];
            if target.closed || block.successors[..at].contains(&to) {
                continue;
            }
            let mut locals = Vec::new_in(frame);
            for slot in active.iter() {
                let local = owners.local_of[slot] as usize;
                let placed = input.early.contains(local)
                    || (plant == Plant::ScopedToo && input.scoped.contains(local));
                if placed
                    && !held_in[to as usize].contains(slot)
                    && block.storage_out.contains(local)
                    && !block.moved_last.contains(local)
                    && !target.ends.contains(local)
                {
                    locals.push(local as u32);
                }
            }
            if locals.is_empty() {
                continue;
            }
            match plant {
                Plant::HashOrder => locals.sort_unstable_by_key(|&it| scrambled(it)),
                _ => locals.sort_unstable_by_key(|&it| order_key(input.rank, it)),
            }
            found.push(EdgeDrops { from: from as u32, to, locals });
        }
    }
    found
}

/// What the locals of one edge are ordered by: the scope that ends first, then the local
/// declared last. Distinct per local, so the unstable sort has one answer.
fn order_key(rank: &[u32], local: u32) -> (u32, std::cmp::Reverse<u32>) {
    (rank[local as usize], std::cmp::Reverse(local))
}

/// The order a hash table would give: what [`Plant::HashOrder`] plants.
fn scrambled(local: u32) -> u32 {
    local.wrapping_mul(0x9E37_79B1).rotate_left(7) ^ 0x5bd1_e995
}

#[cfg(test)]
mod tests {
    //! Generated programs over a few owners and references, read into the facts, placed, and
    //! run on a path with the placed drops in them.

    use blaze_mem::{Tag, frame};
    use hegel::{TestCase, generators as gs};

    use super::*;

    /// One step of a generated program. Owners and references are locals: owners first.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Op {
        /// The owner gets a value.
        Init(u32),
        /// The owner's value is read.
        Use(u32),
        /// `r = &owner`.
        Borrow { r: u32, owner: u32 },
        /// `to = from`, both references.
        CopyRef { to: u32, from: u32 },
        /// The reference is read through.
        UseRef(u32),
        /// The owner's value is moved out.
        Move(u32),
        /// The owner's drop where its scope ends.
        ScopeDrop(u32),
    }

    #[derive(Debug, Clone)]
    enum Node {
        Op(Op),
        If(Vec<Node>, Vec<Node>),
        Loop(Vec<Node>),
    }

    /// A block of the lowered program: at most one step, then its successors.
    #[derive(Debug, Clone, Default)]
    struct Step {
        op: Option<Op>,
        successors: Vec<u32>,
    }

    #[derive(Debug, Clone)]
    struct Program {
        /// Per owner, whether it is dropped early; the references follow the owners.
        early: Vec<bool>,
        refs: usize,
        steps: Vec<Step>,
    }

    impl Program {
        fn owners(&self) -> usize {
            self.early.len()
        }

        fn locals(&self) -> usize {
            self.early.len() + self.refs
        }

        /// Lowers `nodes`, then the scopes' ends: each owner dropped, the last declared first.
        fn new(early: Vec<bool>, refs: usize, nodes: &[Node]) -> Program {
            let mut program = Program { early, refs, steps: vec![Step::default()] };
            let mut at = program.lower(nodes, 0);
            for owner in (0..program.owners() as u32).rev() {
                at = program.then(at, Some(Op::ScopeDrop(owner)));
            }
            program.then(at, None);
            program
        }

        /// A new step after `at`.
        fn then(&mut self, at: u32, op: Option<Op>) -> u32 {
            let next = self.steps.len() as u32;
            self.steps.push(Step { op, successors: Vec::new() });
            self.steps[at as usize].successors.push(next);
            next
        }

        /// Lowers `nodes` after the step `at`; the step they end in.
        fn lower(&mut self, nodes: &[Node], mut at: u32) -> u32 {
            for node in nodes {
                match node {
                    Node::Op(op) => at = self.then(at, Some(*op)),
                    Node::If(then, otherwise) => {
                        let fork = self.then(at, None);
                        let then_start = self.then(fork, None);
                        let then_end = self.lower(then, then_start);
                        let else_start = self.then(fork, None);
                        let else_end = self.lower(otherwise, else_start);
                        let join = self.then(then_end, None);
                        self.steps[else_end as usize].successors.push(join);
                        at = join;
                    }
                    Node::Loop(body) => {
                        let head = self.then(at, None);
                        let start = self.then(head, None);
                        let end = self.lower(body, start);
                        self.steps[end as usize].successors.push(head);
                        // The way out is the head's second successor.
                        at = self.then(head, None);
                    }
                }
            }
            at
        }

        /// What the step at `at` reads, in `uses`, and writes whole, in `defs`.
        fn mentions(&self, at: usize, plant: Plant, uses: &mut Vec<u32>, defs: &mut Vec<u32>) {
            let owners = self.owners() as u32;
            match self.steps[at].op {
                None => {}
                Some(Op::Init(owner)) => defs.push(owner),
                Some(Op::Use(owner) | Op::Move(owner)) => uses.push(owner),
                Some(Op::Borrow { r, owner }) => {
                    uses.push(owner);
                    defs.push(owners + r);
                }
                Some(Op::CopyRef { to, from }) => {
                    uses.push(owners + from);
                    defs.push(owners + to);
                }
                Some(Op::UseRef(r)) => uses.push(owners + r),
                // A drop of an early local is no use; of a scoped one it is, which is what
                // keeps a guard to the end of its scope (and what `Plant::ScopedToo` removes).
                Some(Op::ScopeDrop(owner)) => {
                    if !self.early[owner as usize] && plant != Plant::ScopedToo {
                        uses.push(owner);
                    }
                }
            }
        }

        /// The reference model of liveness: per step, the locals live on entry.
        fn live_in(&self, plant: Plant) -> Vec<Vec<bool>> {
            let mut live = vec![vec![false; self.locals()]; self.steps.len()];
            let (mut uses, mut defs) = (Vec::new(), Vec::new());
            let mut changed = true;
            while changed {
                changed = false;
                for at in (0..self.steps.len()).rev() {
                    let mut now = vec![false; self.locals()];
                    for &next in &self.steps[at].successors {
                        for (local, &on) in live[next as usize].iter().enumerate() {
                            now[local] |= on;
                        }
                    }
                    uses.clear();
                    defs.clear();
                    self.mentions(at, plant, &mut uses, &mut defs);
                    for &local in &defs {
                        now[local as usize] = false;
                    }
                    for &local in &uses {
                        now[local as usize] = true;
                    }
                    if now != live[at] {
                        live[at] = now;
                        changed = true;
                    }
                }
            }
            live
        }

        /// The early drops [`edges`] places for the program, as `(from, to, locals)`.
        fn placed(&self, plant: Plant) -> Vec<(u32, u32, Vec<u32>)> {
            frame::task(Tag::Mir, |frame| {
                let locals = self.locals();
                let owners = self.owners();
                let live = self.live_in(plant);
                let mut early: Bits<'_> = bits_in(locals, frame);
                let mut scoped: Bits<'_> = bits_in(locals, frame);
                for (owner, &is_early) in self.early.iter().enumerate() {
                    match is_early {
                        true => early.insert(owner),
                        false => scoped.insert(owner),
                    };
                }
                // The scopes end in the order `Program::new` drops the owners.
                let rank: Vec<u32> = (0..locals)
                    .map(|local| match local < owners {
                        true => (owners - 1 - local) as u32,
                        false => u32::MAX,
                    })
                    .collect();
                let mut ties = Vec::new();
                let mut blocks = Vec::new();
                // The owners a chain of scope drops from each step on drops.
                let mut ends = vec![Vec::new(); self.steps.len()];
                for at in (0..self.steps.len()).rev() {
                    if let Some(Op::ScopeDrop(owner)) = self.steps[at].op {
                        let mut all = vec![owner];
                        if let Some(&next) = self.steps[at].successors.first() {
                            all.extend(ends[next as usize].iter().copied());
                        }
                        ends[at] = all;
                    }
                }
                for (at, step) in self.steps.iter().enumerate() {
                    let mut block = Block {
                        live_in: bits_in(locals, frame),
                        touched: bits_in(locals, frame),
                        moved_last: bits_in(locals, frame),
                        storage_out: bits_in(locals, frame),
                        ends: bits_in(locals, frame),
                        successors: Vec::new_in(frame),
                        closed: false,
                    };
                    for (local, &on) in live[at].iter().enumerate() {
                        if on {
                            block.live_in.insert(local);
                        }
                    }
                    block.storage_out.insert_range(0..locals);
                    for &owner in &ends[at] {
                        block.ends.insert(owner as usize);
                    }
                    block.successors.extend(step.successors.iter().copied());
                    let refs = owners as u32;
                    match step.op {
                        None | Some(Op::ScopeDrop(_)) => {}
                        Some(Op::Init(owner) | Op::Use(owner)) => {
                            block.touched.insert(owner as usize);
                        }
                        Some(Op::Move(owner)) => {
                            block.touched.insert(owner as usize);
                            block.moved_last.insert(owner as usize);
                        }
                        Some(Op::Borrow { r, owner }) => {
                            block.touched.insert(owner as usize);
                            block.touched.insert((refs + r) as usize);
                            ties.push(Tie::Borrow { into: refs + r, of: owner });
                        }
                        Some(Op::CopyRef { to, from }) => {
                            block.touched.insert((refs + to) as usize);
                            block.touched.insert((refs + from) as usize);
                            ties.push(Tie::Flow { into: refs + to, from: refs + from });
                        }
                        Some(Op::UseRef(r)) => {
                            block.touched.insert((refs + r) as usize);
                        }
                    }
                    blocks.push(block);
                }
                let owned = Owners::new(frame, locals, &early, &scoped, &ties, plant);
                let input =
                    Input { locals, early: &early, scoped: &scoped, rank: &rank, blocks: &blocks };
                edges(frame, &input, &owned, plant)
                    .iter()
                    .map(|it| (it.from, it.to, it.locals.to_vec()))
                    .collect()
            })
        }
    }

    /// What a run shows: the reads and the drops of scoped owners, in order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Seen {
        Read(u32),
        ReadThrough(u32),
        ScopedDropped(u32),
    }

    /// Runs the program on the path `turns` picks (at a fork or a loop's head, the next of
    /// them: `true` the first successor, and the last successor once they run out), with
    /// `placed` drops on its edges. `flags` is drop elaboration: a drop of a value that is gone
    /// does nothing; without it that drop is a second drop. An error is a broken invariant.
    fn run(
        program: &Program,
        placed: &[(u32, u32, Vec<u32>)],
        turns: &[bool],
        flags: bool,
    ) -> Result<Vec<Seen>, String> {
        let owners = program.owners();
        let mut holds = vec![false; owners];
        let mut points_to: Vec<Option<u32>> = vec![None; program.refs];
        let mut seen = Vec::new();
        let mut turn = 0;
        let mut at = 0u32;
        let drop = |owner: u32,
                        holds: &mut Vec<bool>,
                        seen: &mut Vec<Seen>,
                        what: &str|
         -> Result<(), String> {
            if !holds[owner as usize] {
                return match flags {
                    true => Ok(()),
                    false => Err(format!("owner {owner} is dropped twice ({what})")),
                };
            }
            holds[owner as usize] = false;
            if !program.early[owner as usize] {
                seen.push(Seen::ScopedDropped(owner));
            }
            Ok(())
        };
        // Every loop turn takes a turn, and each turn is taken once: the run ends.
        for _ in 0..(turns.len() + 2) * (program.steps.len() + 1) {
            let step = &program.steps[at as usize];
            match step.op {
                None => {}
                Some(Op::Init(owner)) => holds[owner as usize] = true,
                Some(Op::Use(owner)) => {
                    if !holds[owner as usize] {
                        return Err(format!("owner {owner} is used after its drop"));
                    }
                    seen.push(Seen::Read(owner));
                }
                Some(Op::Move(owner)) => {
                    if !holds[owner as usize] {
                        return Err(format!("owner {owner} is moved after its drop"));
                    }
                    holds[owner as usize] = false;
                }
                Some(Op::Borrow { r, owner }) => {
                    if !holds[owner as usize] {
                        return Err(format!("owner {owner} is borrowed after its drop"));
                    }
                    points_to[r as usize] = Some(owner);
                }
                Some(Op::CopyRef { to, from }) => points_to[to as usize] = points_to[from as usize],
                Some(Op::UseRef(r)) => {
                    let Some(owner) = points_to[r as usize] else {
                        return Err(format!("reference {r} is read before it is made"));
                    };
                    if !holds[owner as usize] {
                        return Err(format!(
                            "owner {owner} was dropped while reference {r}, a loan of it, is live"
                        ));
                    }
                    seen.push(Seen::ReadThrough(r));
                }
                Some(Op::ScopeDrop(owner)) => drop(owner, &mut holds, &mut seen, "its scope ends")?,
            }
            let next = match step.successors.as_slice() {
                [] => {
                    return match holds.iter().position(|&it| it) {
                        Some(owner) => Err(format!("owner {owner} is never dropped")),
                        None => Ok(seen),
                    };
                }
                [only] => *only,
                [first, .., last] => {
                    let taken = turns.get(turn).copied().unwrap_or(false);
                    turn += 1;
                    if taken { *first } else { *last }
                }
            };
            if let Some((_, _, locals)) = placed.iter().find(|it| it.0 == at && it.1 == next) {
                for &local in locals {
                    if local as usize >= owners {
                        return Err(format!("reference {local} is given a drop"));
                    }
                    drop(local, &mut holds, &mut seen, "an early drop")?;
                }
            }
            at = next;
        }
        Err("the run did not end".to_owned())
    }

    /// The oracle. On the path `turns` picks, with the drops `plant`ed placement gives: no
    /// local is dropped while a loan of it is live, none is used after its drop, every value
    /// is dropped once, the drops of one edge are in scope order, and the run shows what the
    /// run without early drops shows (so each scoped local drops where Rust drops it).
    fn holds(program: &Program, turns: &[bool], plant: Plant, flags: bool) -> Result<(), String> {
        let rust = run(program, &[], turns, true)
            .map_err(|why| format!("the generated program is not a valid one: {why}"))?;
        let placed = program.placed(plant);
        let owners = program.owners() as u32;
        for (from, to, locals) in &placed {
            let keys: Vec<u32> = locals.iter().map(|&it| owners - 1 - it).collect();
            if !keys.is_sorted() {
                return Err(format!(
                    "the drops of edge {from} -> {to} are not in scope order: {locals:?}"
                ));
            }
        }
        let early = run(program, &placed, turns, flags)?;
        match early == rust {
            true => Ok(()),
            false => Err(format!("a scoped local moved: {rust:?} without, {early:?} with")),
        }
    }

    /// What the generator knows of a local on every path so far.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Known {
        Yes,
        No,
        Maybe,
    }

    #[derive(Debug, Clone)]
    struct State {
        holds: Vec<Known>,
        /// Per reference, the owner it points to on every path.
        points_to: Vec<Option<u32>>,
    }

    impl State {
        fn join(&mut self, other: &State) {
            for (mine, theirs) in self.holds.iter_mut().zip(&other.holds) {
                if mine != theirs {
                    *mine = Known::Maybe;
                }
            }
            for (mine, theirs) in self.points_to.iter_mut().zip(&other.points_to) {
                if mine != theirs {
                    *mine = None;
                }
            }
        }

        /// The reference `r` reads a value that is there on every path.
        fn readable(&self, r: u32) -> bool {
            self.points_to[r as usize].is_some_and(|it| self.holds[it as usize] == Known::Yes)
        }
    }

    /// Up to `len` nodes, each valid on every path given `state`, which it updates. In a loop's
    /// body (`reads_only`) only reads: the body is valid however often it runs.
    fn nodes(tc: &TestCase, state: &mut State, depth: u8, reads_only: bool) -> Vec<Node> {
        let len = tc.draw(gs::integers::<u8>().max_value(6));
        let owners = state.holds.len() as u32;
        let refs = state.points_to.len() as u32;
        let mut out = Vec::new();
        for _ in 0..len {
            let owner = tc.draw(gs::integers::<u32>().max_value(owners - 1));
            let r = tc.draw(gs::integers::<u32>().max_value(refs - 1));
            let other = tc.draw(gs::integers::<u32>().max_value(refs - 1));
            let there = state.holds[owner as usize];
            match tc.draw(gs::integers::<u8>().max_value(7)) {
                0 if !reads_only && there == Known::No => {
                    state.holds[owner as usize] = Known::Yes;
                    out.push(Node::Op(Op::Init(owner)));
                }
                1 if there == Known::Yes => out.push(Node::Op(Op::Use(owner))),
                2 if !reads_only && there == Known::Yes => {
                    state.points_to[r as usize] = Some(owner);
                    out.push(Node::Op(Op::Borrow { r, owner }));
                }
                3 if state.readable(r) => out.push(Node::Op(Op::UseRef(r))),
                4 if !reads_only && state.points_to[other as usize].is_some() && other != r => {
                    state.points_to[r as usize] = state.points_to[other as usize];
                    out.push(Node::Op(Op::CopyRef { to: r, from: other }));
                }
                5 if !reads_only && there == Known::Yes => {
                    // A moved value's borrows are dead: nothing reads through them again.
                    state.holds[owner as usize] = Known::No;
                    for target in state.points_to.iter_mut() {
                        if *target == Some(owner) {
                            *target = None;
                        }
                    }
                    out.push(Node::Op(Op::Move(owner)));
                }
                6 if !reads_only && depth > 0 => {
                    let mut other_arm = state.clone(); // clone: a test's state per arm
                    let then = nodes(tc, state, depth - 1, false);
                    let otherwise = nodes(tc, &mut other_arm, depth - 1, false);
                    state.join(&other_arm);
                    out.push(Node::If(then, otherwise));
                }
                7 if depth > 0 => out.push(Node::Loop(nodes(tc, state, depth - 1, true))),
                _ => {}
            }
        }
        out
    }

    fn program(tc: &TestCase) -> Program {
        let early: Vec<bool> = tc.draw(gs::vecs(gs::booleans()).min_size(1).max_size(4));
        let refs = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
        let mut state = State { holds: vec![Known::No; early.len()], points_to: vec![None; refs] };
        let nodes = nodes(tc, &mut state, 2, false);
        Program::new(early, refs, &nodes)
    }

    /// The invariant: under early drop no local is dropped while a loan of it is live, no local
    /// is used after its drop, every initialized local is dropped exactly once on every path,
    /// the locals of one edge drop in scope order, and a scoped local drops where Rust drops
    /// it.
    #[hegel::test(test_cases = 400)]
    fn early_drops_keep_every_program_as_rust_runs_it(tc: TestCase) {
        let program = program(&tc);
        let turns: Vec<bool> = tc.draw(gs::vecs(gs::booleans()).max_size(12));
        if let Err(why) = holds(&program, &turns, Plant::None, true) {
            panic!("{why}\n{program:#?}\nturns {turns:?}");
        }
    }

    /// Two runs of one program place the same drops in the same order.
    #[hegel::test(test_cases = 100)]
    fn placement_is_the_same_on_every_run(tc: TestCase) {
        let program = program(&tc);
        assert_eq!(program.placed(Plant::None), program.placed(Plant::None));
    }

    /// `a` is borrowed into a reference that is read after `a`'s own last use.
    fn reborrowed() -> Program {
        let nodes = [
            Node::Op(Op::Init(0)),
            Node::Op(Op::Borrow { r: 0, owner: 0 }),
            Node::Op(Op::CopyRef { to: 1, from: 0 }),
            Node::Op(Op::Init(1)),
            Node::Op(Op::Use(1)),
            Node::Op(Op::UseRef(1)),
        ];
        Program::new(vec![true, true], 2, &nodes)
    }

    #[test]
    fn the_oracle_catches_a_drop_before_the_last_use_of_a_reborrow() {
        assert_eq!(holds(&reborrowed(), &[], Plant::None, true), Ok(()));
        let why = holds(&reborrowed(), &[], Plant::OwnLivenessOnly, true)
            .expect_err("the oracle missed a drop placed while a reborrow is live");
        assert!(why.contains("owner 0 was dropped while reference 1, a loan of it, is live"), "{why}");
    }

    /// `a` is moved out on one arm only, then its scope ends.
    fn moved_on_one_arm() -> Program {
        let nodes = [
            Node::Op(Op::Init(0)),
            Node::Op(Op::Init(1)),
            Node::If(vec![Node::Op(Op::Move(0))], vec![Node::Op(Op::Use(0))]),
            Node::Op(Op::Use(1)),
        ];
        Program::new(vec![true, true], 1, &nodes)
    }

    #[test]
    fn the_oracle_catches_a_conditional_move_with_no_drop_flag() {
        for turns in [[true], [false]] {
            assert_eq!(holds(&moved_on_one_arm(), &turns, Plant::None, true), Ok(()));
        }
        // Without drop elaboration's flag the drop where the scope ends drops what the early
        // drop on the arm that kept the value already dropped.
        let why = holds(&moved_on_one_arm(), &[false], Plant::None, false)
            .expect_err("the oracle missed a second drop of a conditionally moved value");
        assert!(why.contains("owner 0 is dropped twice"), "{why}");
    }

    /// A scoped guard whose last use comes before another local's read.
    fn guarded() -> Program {
        let nodes = [
            Node::Op(Op::Init(0)),
            Node::Op(Op::Init(1)),
            Node::Op(Op::Use(0)),
            Node::Op(Op::Use(1)),
        ];
        Program::new(vec![false, true], 1, &nodes)
    }

    #[test]
    fn the_oracle_catches_a_scoped_guard_dropped_early() {
        assert_eq!(holds(&guarded(), &[], Plant::None, true), Ok(()));
        let why = holds(&guarded(), &[], Plant::ScopedToo, true)
            .expect_err("the oracle missed a guard dropped before its scope ends");
        assert!(why.contains("a scoped local moved"), "{why}");
    }

    /// Four early locals that all die on one edge, the one into the arm that does not use them.
    fn four_at_once() -> Program {
        let mut nodes: Vec<Node> = (0..4).map(|it| Node::Op(Op::Init(it))).collect();
        // Used on one arm only: on the edge into the other all four are dropped.
        nodes.push(Node::If((0..4).map(|it| Node::Op(Op::Use(it))).collect(), Vec::new()));
        Program::new(vec![true; 4], 1, &nodes)
    }

    #[test]
    fn the_oracle_catches_two_locals_dropped_in_hash_order() {
        for turns in [[true], [false]] {
            assert_eq!(holds(&four_at_once(), &turns, Plant::None, true), Ok(()));
        }
        let why = holds(&four_at_once(), &[false], Plant::HashOrder, true)
            .expect_err("the oracle missed drops ordered by a hash");
        assert!(why.contains("are not in scope order"), "{why}");
    }

    /// A body whose locals are all used until their scopes end gets no drop.
    #[test]
    fn nothing_is_placed_where_the_scope_ends_next() {
        let nodes = [Node::Op(Op::Init(0)), Node::Op(Op::Init(1)), Node::Op(Op::Use(0))];
        let program = Program::new(vec![true, true], 1, &nodes);
        // `1` is never used: it is dropped at once. `0` is used last, and its scope ends next.
        assert_eq!(program.placed(Plant::None), vec![(2, 3, vec![1])]);
    }
}
