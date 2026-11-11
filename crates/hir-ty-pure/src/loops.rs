//! Which loops of a body are known to end (docs/v2/plan/design/effects.md, "`diverge`"): what
//! [`Rule::Loop`](crate::effects::Rule::Loop) is recorded from.
//!
//! A loop is a strongly connected component of the body's control flow graph (a block that goes
//! to itself counts). A component ends when some block of it, a *bounding block*, can run only
//! finitely often while control stays in the component, and what is left of the component
//! without that block ends too ([`unbounded`]): a path that stays in the component for ever
//! passes the bounding block finitely often, so from some point on it stays in what is left,
//! and there in one of its components, which ends by the same argument. A component with no
//! bounding block is refused, naming its line; nothing is guessed.
//!
//! The bounding rules ([`Bounding`]) are data for that one algorithm. This slice has one,
//! provable from the MIR alone ([`Counted`]): a counted `while i < n`. A `for` over a range or a
//! slice (the iterator's `next` is finite and its `None` edge leaves the component) and a
//! `decreases` clause the verifier proves are the next rules: each is one more [`Bounding`]
//! tried on a component, and neither changes the algorithm or its test.

use hir_ty::next_solver::TyKind;
use hir_ty_mir::mir::{
    BasicBlock, BinOp, FieldIndex, LocalId, MirBody, MirSpan, Operand, OperandKind,
    ProjectionElem, Rvalue, Statement, StatementKind, StoredPlace, TerminatorKind,
};
use rustc_type_ir::{IntTy, inherent::IntoKind as _};
use smallvec::SmallVec;

use crate::effect_attrs::constant_bits;

/// A control flow graph: blocks `0..len`, each with the blocks it may go to.
pub(crate) struct Graph {
    // alloc: one list per block, once per body with a loop (and per test graph)
    successors: Vec<SmallVec<[u32; 2]>>,
    #[cfg(test)]
    plant: Option<Plant>,
}

/// A bug planted for the controls of the invariant's test.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    /// A block that goes to itself is not seen as a cycle.
    SelfLoop,
    /// A bounding block's exit is taken to leave the component wherever it goes.
    ExitInside,
}

impl Graph {
    pub(crate) fn new(blocks: usize) -> Graph {
        Graph {
            successors: vec![SmallVec::new(); blocks], // alloc: as the field says
            #[cfg(test)]
            plant: None,
        }
    }

    pub(crate) fn edge(&mut self, from: u32, to: u32) {
        let list = &mut self.successors[from as usize];
        if !list.contains(&to) {
            list.push(to);
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.successors.len()
    }

    pub(crate) fn successors(&self, block: u32) -> &[u32] {
        &self.successors[block as usize]
    }

    /// Whether a block that goes to itself is a cycle (it is; a control plants that it is not).
    fn self_loops_count(&self) -> bool {
        #[cfg(test)]
        {
            if self.plant == Some(Plant::SelfLoop) {
                return false;
            }
        }
        true
    }

    /// The strongly connected components with a cycle among the blocks `inside` marks, edges
    /// between them only: more than one block, or one that goes to itself. Each sorted, in the
    /// order of their smallest blocks (Tarjan's algorithm, with its own stack: a body's blocks
    /// can be many).
    pub(crate) fn components(&self, inside: &[bool]) -> Vec<Vec<u32>> {
        const UNSEEN: u32 = u32::MAX;
        let count = self.len();
        let mut index = vec![UNSEEN; count]; // alloc: once per component split
        let mut low = vec![0u32; count]; // alloc: as above
        let mut on_stack = vec![false; count]; // alloc: as above
        let mut stack: Vec<u32> = Vec::new(); // alloc: as above
        let mut calls: Vec<(u32, u32)> = Vec::new(); // alloc: as above
        let mut found: Vec<Vec<u32>> = Vec::new(); // alloc: the components, few
        let mut next = 0u32;
        for root in 0..count as u32 {
            if !inside[root as usize] || index[root as usize] != UNSEEN {
                continue;
            }
            index[root as usize] = next;
            low[root as usize] = next;
            next += 1;
            stack.push(root);
            on_stack[root as usize] = true;
            calls.push((root, 0));
            // Each turn takes one edge or ends one block: at most edges plus blocks turns.
            while let Some(&(block, at)) = calls.last() {
                let top = calls.len() - 1;
                if let Some(&to) = self.successors[block as usize].get(at as usize) {
                    calls[top].1 += 1;
                    if !inside[to as usize] {
                        continue;
                    }
                    if index[to as usize] == UNSEEN {
                        index[to as usize] = next;
                        low[to as usize] = next;
                        next += 1;
                        stack.push(to);
                        on_stack[to as usize] = true;
                        calls.push((to, 0));
                    } else if on_stack[to as usize] {
                        low[block as usize] = low[block as usize].min(index[to as usize]);
                    }
                    continue;
                }
                calls.pop();
                if let Some(&(parent, _)) = calls.last() {
                    low[parent as usize] = low[parent as usize].min(low[block as usize]);
                }
                if low[block as usize] != index[block as usize] {
                    continue;
                }
                let mut component: Vec<u32> = Vec::new(); // alloc: one component
                while let Some(member) = stack.pop() {
                    on_stack[member as usize] = false;
                    component.push(member);
                    if member == block {
                        break;
                    }
                }
                let to_itself =
                    self.self_loops_count() && self.successors[block as usize].contains(&block);
                if component.len() > 1 || to_itself {
                    component.sort_unstable();
                    found.push(component);
                }
            }
        }
        found.sort_by_key(|it| it[0]);
        found
    }

    /// Whether `block` is on a cycle among the blocks `inside` marks.
    pub(crate) fn on_cycle(&self, inside: &[bool], block: u32) -> bool {
        self.components(inside).iter().any(|it| it.binary_search(&block).is_ok())
    }
}

/// One component a rule is asked about.
pub(crate) struct Component<'a> {
    /// Its blocks, sorted.
    pub(crate) blocks: &'a [u32],
    /// Per block of the graph, whether it is in the component.
    pub(crate) inside: &'a [bool],
    #[cfg(test)]
    plant: Option<Plant>,
}

impl Component<'_> {
    pub(crate) fn has(&self, block: u32) -> bool {
        self.inside[block as usize]
    }

    /// Whether going to `target` leaves the component: what a rule asks of the edge its
    /// bounding block takes once its bound is used up.
    pub(crate) fn exits(&self, target: u32) -> bool {
        #[cfg(test)]
        {
            if self.plant == Some(Plant::ExitInside) {
                return true;
            }
        }
        !self.has(target)
    }
}

/// A rule that knows some loops end.
pub(crate) trait Bounding {
    /// A block of `component` that runs only finitely often while control stays in the
    /// component, whatever the rest of the component does; none when the rule knows of none.
    fn bounding(&self, graph: &Graph, component: &Component<'_>) -> Option<u32>;
}

/// The blocks of the first component of `graph` (by its smallest block) that `rule` does not
/// bound, at any depth; none when every cycle is bounded. Each step takes one block off a
/// component, so it ends after at most as many steps as the graph has blocks.
pub(crate) fn unbounded(graph: &Graph, rule: &impl Bounding) -> Option<Vec<u32>> {
    let mut first: Option<Vec<u32>> = None;
    // alloc: the sets of blocks still to split, once per body with a loop
    let mut work: Vec<Vec<bool>> = vec![vec![true; graph.len()]];
    while let Some(inside) = work.pop() {
        for blocks in graph.components(&inside) {
            let mut within = vec![false; graph.len()]; // alloc: one component's blocks
            for &block in &blocks {
                within[block as usize] = true;
            }
            let component = Component {
                blocks: &blocks,
                inside: &within,
                #[cfg(test)]
                plant: graph.plant,
            };
            match rule.bounding(graph, &component) {
                // A block outside the component bounds nothing in it, and taking it off would
                // make no progress.
                Some(block) if within[block as usize] => {
                    within[block as usize] = false;
                    work.push(within);
                }
                _ => {
                    if first.as_ref().is_none_or(|it| blocks[0] < it[0]) {
                        first = Some(blocks);
                    }
                }
            }
        }
    }
    first
}

/// Where the first loop of `body` not known to end is written; none when every loop ends (or
/// the body has none, which costs one pass over its blocks).
pub(crate) fn unbounded_loop(body: &MirBody<'_>) -> Option<MirSpan> {
    // alloc: the body's blocks by index, once per body
    let blocks: Vec<&BasicBlock> = body.basic_blocks.iter().map(|(_, block)| block).collect();
    let mut graph = Graph::new(blocks.len());
    for (id, block) in body.basic_blocks.iter() {
        let Some(terminator) = &block.terminator else { continue };
        for target in terminator.kind.all_successors() {
            graph.edge(id.into_raw().into_u32(), target.into_raw().into_u32());
        }
    }
    let all = vec![true; blocks.len()]; // alloc: once per body
    if graph.components(&all).is_empty() {
        return None;
    }
    let rule = Counted::new(body, &blocks, &graph);
    let component = unbounded(&graph, &rule)?;
    Some(loop_span(&blocks, &rule.predecessors, &component))
}

/// The span that names the loop `component` is: of the jump into its head from outside (the
/// loop expression's, as lowering makes it), else of a jump back to its head, else of the
/// head's own terminator. The head is its first block entered from outside.
fn loop_span(
    blocks: &[&BasicBlock],
    predecessors: &[SmallVec<[u32; 2]>],
    component: &[u32],
) -> MirSpan {
    let inside = |block: u32| component.binary_search(&block).is_ok();
    let head = component
        .iter()
        .copied()
        .find(|&block| predecessors[block as usize].iter().any(|&from| !inside(from)))
        .unwrap_or(component[0]);
    let jump = |from: u32| match blocks[from as usize].terminator.as_ref() {
        Some(it) if matches!(it.kind, TerminatorKind::Goto { .. }) => Some(it.span),
        _ => None,
    };
    let froms = &predecessors[head as usize];
    let entry = froms.iter().copied().filter(|&from| !inside(from)).find_map(jump);
    let back = || froms.iter().copied().filter(|&from| inside(from)).find_map(jump);
    let own = || blocks[head as usize].terminator.as_ref().map(|it| it.span);
    entry.or_else(back).or_else(own).unwrap_or(MirSpan::Unknown)
}

/// The rule of a counted loop, `while i < n { ..; i += c; }` and its mirror with `>` and `-=`,
/// read from the MIR.
///
/// A block `H` of the component is bounding when:
/// - it ends in a switch on a comparison it makes of a local integer `i` (the counter) with a
///   limit, and the switch stays in the component only while `i < limit` (or `<=`; `>`, `>=`
///   for a counter that falls), so every other outcome leaves it;
/// - the limit is a constant, or a local nothing in the component writes;
/// - every write of `i` in the component is `i = i + c` (`i - c` for one that falls) with `c`
///   a positive constant, and none is in `H`;
/// - the steps cannot wrap: each is an overflow check that runs (`AddWithOverflow`, an `Assert`
///   on its flag, then the sum: what a crate built with overflow checks has, and a wrap there
///   is a panic, which ends the function), and every cycle through `H` passes one; or, where
///   the crate drops the check (release without `overflow-checks`, or a body that inherits the
///   setting), the comparison is strict, the step is by one, it is the only write of `i` in
///   the component and it runs at most once between two runs of `H`: `i < n` then gives
///   `i + 1 <= n`, which fits.
///
/// Then the values of `i` at `H` rise strictly and stay under a fixed limit while control is
/// in the component, so `H` runs finitely often. No local involved may have its address taken
/// anywhere in the body (a write through a pointer is not seen), and a body with inline
/// assembly or of a coroutine has no counted loop.
struct Counted<'a, 'db> {
    body: &'a MirBody<'db>,
    blocks: &'a [&'a BasicBlock],
    // alloc: one list per block, once per body with a loop
    predecessors: Vec<SmallVec<[u32; 2]>>,
    /// Per local, whether some `&` of it (or of a part of it) is taken in the body.
    // alloc: one flag per local, once per body with a loop
    borrowed: Vec<bool>,
    /// The body has inline assembly or is a coroutine's: nothing is known of its locals.
    opaque: bool,
}

/// Which way a counter goes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Way {
    Up,
    Down,
}

/// One write of the counter: `i = i ± amount`.
struct Step {
    way: Way,
    amount: u128,
    /// The step is an overflow check that runs: a wrap is a panic.
    checked: bool,
}

/// What an operand is, as of a statement of a block.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Value {
    /// The local, as it is there.
    Local(LocalId),
    Constant(u128),
    Unknown,
}

fn index(local: LocalId) -> usize {
    local.into_raw().into_u32() as usize
}

/// The local a place is, when it is one whole.
fn whole(place: &StoredPlace) -> Option<LocalId> {
    place.as_ref().as_local()
}

/// Whether the place is `local` or a part of it (not what a pointer in it points to).
fn part_of(place: &StoredPlace, local: LocalId) -> bool {
    place.local == local
        && !place.projection.as_slice().iter().any(|it| matches!(it, ProjectionElem::Deref))
}

/// Whether `statement` writes `local`, ends its storage or starts it anew.
fn writes(statement: &Statement, local: LocalId) -> bool {
    match &statement.kind {
        StatementKind::Assign(to, _) | StatementKind::Deinit(to) => part_of(to, local),
        StatementKind::StorageLive(it) | StatementKind::StorageDead(it) => *it == local,
        StatementKind::FakeRead(_) | StatementKind::InlineAsm(_) | StatementKind::Nop => false,
    }
}

/// Whether the terminator of `block` writes `local`.
fn terminator_writes(block: &BasicBlock, local: LocalId) -> bool {
    match block.terminator.as_ref().map(|it| &it.kind) {
        Some(TerminatorKind::Call { destination: to, .. })
        | Some(TerminatorKind::Drop { place: to, .. })
        | Some(TerminatorKind::DropAndReplace { place: to, .. })
        | Some(TerminatorKind::Yield { resume_arg: to, .. }) => part_of(to, local),
        _ => false,
    }
}

impl<'a, 'db> Counted<'a, 'db> {
    fn new(body: &'a MirBody<'db>, blocks: &'a [&'a BasicBlock], graph: &Graph) -> Self {
        let mut predecessors = vec![SmallVec::new(); blocks.len()]; // alloc: as the field says
        for from in 0..blocks.len() as u32 {
            for &to in graph.successors(from) {
                predecessors[to as usize].push(from);
            }
        }
        let mut borrowed = vec![false; body.locals.len()]; // alloc: as the field says
        let mut opaque = body.coroutine.is_some();
        for block in blocks {
            for statement in &block.statements {
                match &statement.kind {
                    StatementKind::Assign(_, Rvalue::Ref(_, of)) => {
                        // A borrow through a pointer the local holds takes no address of it.
                        if part_of(of, of.local) {
                            borrowed[index(of.local)] = true;
                        }
                    }
                    StatementKind::InlineAsm(_) => opaque = true,
                    _ => {}
                }
            }
        }
        Counted { body, blocks, predecessors, borrowed, opaque }
    }

    /// The last statement of `block` before `before` that writes `local`.
    fn last_write(&self, block: u32, before: usize, local: LocalId) -> Option<usize> {
        self.blocks[block as usize].statements[..before].iter().rposition(|it| writes(it, local))
    }

    /// Whether a statement of `block` in `from..to` writes `local`.
    fn written_between(&self, block: u32, from: usize, to: usize, local: LocalId) -> bool {
        self.blocks[block as usize].statements[from..to].iter().any(|it| writes(it, local))
    }

    /// What `operand` is at statement `at` of `block`: a constant, or the local whose value it
    /// has there, through the copies the block makes (`_4 = copy _1; .. move _4`). A local
    /// whose address is taken is unknown, as is one written between a copy of it and `at`.
    fn value(&self, block: u32, at: usize, operand: &Operand) -> Value {
        if let Some(bits) = constant_bits(operand) {
            return Value::Constant(bits);
        }
        let (OperandKind::Copy(place) | OperandKind::Move(place)) = &operand.kind else {
            return Value::Unknown;
        };
        let Some(mut local) = whole(place) else { return Value::Unknown };
        let statements = &self.blocks[block as usize].statements;
        let mut upto = at;
        // Each turn goes to an earlier statement of the block.
        loop {
            if self.borrowed[index(local)] {
                return Value::Unknown;
            }
            let Some(written) = self.last_write(block, upto, local) else { break };
            let StatementKind::Assign(to, Rvalue::Use(from)) = &statements[written].kind else {
                break;
            };
            if whole(to) != Some(local) {
                return Value::Unknown;
            }
            if let Some(bits) = constant_bits(from) {
                return Value::Constant(bits);
            }
            let (OperandKind::Copy(source) | OperandKind::Move(source)) = &from.kind else {
                return Value::Unknown;
            };
            let Some(source) = whole(source) else { return Value::Unknown };
            (local, upto) = (source, written);
        }
        // The value was read at `upto`: it is the local's at `at` only if nothing wrote the
        // local since.
        match self.written_between(block, upto, at, local) {
            true => Value::Unknown,
            false => Value::Local(local),
        }
    }

    /// A positive constant step of a counter of `counter`'s type, from its bits.
    fn positive(&self, counter: LocalId, bits: u128) -> Option<u128> {
        let width: u32 = match self.body.locals[counter].ty.as_ref().kind() {
            TyKind::Uint(_) => return (bits != 0).then_some(bits),
            TyKind::Int(IntTy::I8) => 8,
            // The narrowest `isize` a target has.
            TyKind::Int(IntTy::I16 | IntTy::Isize) => 16,
            TyKind::Int(IntTy::I32) => 32,
            TyKind::Int(IntTy::I64) => 64,
            TyKind::Int(IntTy::I128) => 128,
            _ => return None,
        };
        (bits != 0 && bits < 1u128 << (width - 1)).then_some(bits)
    }

    /// `a op b` as a step of `counter`, read at statement `at` of `block`.
    fn sum(
        &self,
        block: u32,
        at: usize,
        counter: LocalId,
        way: Way,
        a: &Operand,
        b: &Operand,
    ) -> Option<(Way, u128)> {
        let (a, b) = (self.value(block, at, a), self.value(block, at, b));
        let amount = match (way, a, b) {
            (_, Value::Local(it), Value::Constant(bits)) if it == counter => bits,
            // `c + i` is `i + c`; `c - i` is no step.
            (Way::Up, Value::Constant(bits), Value::Local(it)) if it == counter => bits,
            _ => return None,
        };
        Some((way, self.positive(counter, amount)?))
    }

    /// The step the write of `counter` at statement `at` of `block` is, if it is one.
    fn step(&self, block: u32, at: usize, counter: LocalId) -> Option<Step> {
        let statements = &self.blocks[block as usize].statements;
        let StatementKind::Assign(to, rvalue) = &statements[at].kind else { return None };
        if whole(to) != Some(counter) {
            return None;
        }
        self.stepped(block, at, counter, rvalue)
    }

    /// What `rvalue`, at statement `at` of `block`, is as a step of `counter`: nothing wrote
    /// the counter between where its old value was read and `at`.
    fn stepped(&self, block: u32, at: usize, counter: LocalId, rvalue: &Rvalue) -> Option<Step> {
        let statements = &self.blocks[block as usize].statements;
        match rvalue {
            // The crate drops the overflow check: the sum wraps.
            Rvalue::BinaryOp(op @ (BinOp::Add | BinOp::Sub), a, b) => {
                let way = if matches!(op, BinOp::Add) { Way::Up } else { Way::Down };
                let (way, amount) = self.sum(block, at, counter, way, a, b)?;
                Some(Step { way, amount, checked: false })
            }
            Rvalue::Use(Operand { kind: OperandKind::Copy(from) | OperandKind::Move(from), .. }) => {
                let made = from.local;
                if made == counter || self.borrowed[index(made)] {
                    return None;
                }
                if let Some(temporary) = whole(from) {
                    // `tmp = i + c; ..; i = move tmp`.
                    let written = self.last_write(block, at, temporary)?;
                    let StatementKind::Assign(to, earlier) = &statements[written].kind else {
                        return None;
                    };
                    if whole(to) != Some(temporary)
                        || self.written_between(block, written, at, counter)
                    {
                        return None;
                    }
                    return self.stepped(block, written, counter, earlier);
                }
                // `(t.0)` of `t = AddWithOverflow(i, c)`: the sum of a checked step.
                if !matches!(from.projection.as_slice(), [ProjectionElem::Field(FieldIndex(0))]) {
                    return None;
                }
                let (home, written, checked) = match self.last_write(block, at, made) {
                    Some(written) => (block, written, false),
                    None => {
                        // The pair is made in the one block that comes here, which ends in
                        // the check of its flag: `assert(!move (t.1)) -> here`.
                        let &[from_block] = self.predecessors[block as usize].as_slice() else {
                            return None;
                        };
                        let before = self.blocks[from_block as usize];
                        let Some(TerminatorKind::Assert { cond, expected: false, msg, .. }) =
                            before.terminator.as_ref().map(|it| &it.kind)
                        else {
                            return None;
                        };
                        let (OperandKind::Copy(flag) | OperandKind::Move(flag)) = &cond.kind
                        else {
                            return None;
                        };
                        let of_pair = flag.local == made
                            && matches!(
                                flag.projection.as_slice(),
                                [ProjectionElem::Field(FieldIndex(1))]
                            );
                        if !of_pair || !msg.is_optional_overflow_check() {
                            return None;
                        }
                        let written = self.last_write(from_block, before.statements.len(), made)?;
                        // A body that inherits the setting may be built without the check.
                        (from_block, written, !self.body.inherits_overflow_checks)
                    }
                };
                let pair = &self.blocks[home as usize].statements;
                let StatementKind::Assign(to, Rvalue::BinaryOp(op, a, b)) = &pair[written].kind
                else {
                    return None;
                };
                let way = match op {
                    BinOp::AddWithOverflow => Way::Up,
                    BinOp::SubWithOverflow => Way::Down,
                    _ => return None,
                };
                // Nothing wrote the counter between the pair and the sum's use.
                let quiet = match home == block {
                    true => !self.written_between(block, written, at, counter),
                    false => {
                        !self.written_between(home, written, pair.len(), counter)
                            && !self.written_between(block, 0, at, counter)
                    }
                };
                if whole(to) != Some(made) || !quiet {
                    return None;
                }
                let (way, amount) = self.sum(home, written, counter, way, a, b)?;
                Some(Step { way, amount, checked })
            }
            _ => None,
        }
    }

    /// Whether `head` is a counted loop's test that bounds `component`.
    fn bounds(&self, graph: &Graph, component: &Component<'_>, head: u32) -> bool {
        let block = self.blocks[head as usize];
        let Some(TerminatorKind::SwitchInt { discr, targets }) =
            block.terminator.as_ref().map(|it| &it.kind)
        else {
            return false;
        };
        let (OperandKind::Copy(tested) | OperandKind::Move(tested)) = &discr.kind else {
            return false;
        };
        let Some(tested) = whole(tested) else { return false };
        if self.borrowed[index(tested)] {
            return false;
        }
        let Some(at) = self.last_write(head, block.statements.len(), tested) else { return false };
        let StatementKind::Assign(to, Rvalue::BinaryOp(op, a, b)) = &block.statements[at].kind
        else {
            return false;
        };
        if whole(to) != Some(tested) {
            return false;
        }
        // The outcome the switch stays in the component on; every other one leaves it.
        let (on_false, on_true) = (targets.target_for_value(0), targets.target_for_value(1));
        let (on_false, on_true) = (on_false.into_raw().into_u32(), on_true.into_raw().into_u32());
        let stays_while = match (component.exits(on_false), component.exits(on_true)) {
            (true, false) => true,
            (false, true) => false,
            _ => return false,
        };
        // The comparison as it holds while control stays, the counter on its left.
        let (a, b) = (self.value(head, at, a), self.value(head, at, b));
        for (counter, limit, flipped) in [(a, b, false), (b, a, true)] {
            let Value::Local(counter) = counter else { continue };
            let op = match (op, flipped) {
                (BinOp::Lt, false) | (BinOp::Gt, true) => BinOp::Lt,
                (BinOp::Le, false) | (BinOp::Ge, true) => BinOp::Le,
                (BinOp::Gt, false) | (BinOp::Lt, true) => BinOp::Gt,
                (BinOp::Ge, false) | (BinOp::Le, true) => BinOp::Ge,
                _ => return false,
            };
            let (way, strict) = match (op, stays_while) {
                (BinOp::Lt, true) | (BinOp::Ge, false) => (Way::Up, true),
                (BinOp::Le, true) | (BinOp::Gt, false) => (Way::Up, false),
                (BinOp::Gt, true) | (BinOp::Le, false) => (Way::Down, true),
                _ => (Way::Down, false),
            };
            if self.counts(graph, component, head, counter, limit, way, strict) {
                return true;
            }
        }
        false
    }

    /// Whether `counter` goes `way` towards `limit` on every cycle of `component` through
    /// `head`, without wrapping.
    #[expect(clippy::too_many_arguments, reason = "one loop's facts, read in one place")]
    fn counts(
        &self,
        graph: &Graph,
        component: &Component<'_>,
        head: u32,
        counter: LocalId,
        limit: Value,
        way: Way,
        strict: bool,
    ) -> bool {
        let written_in = |local: LocalId| {
            component.blocks.iter().any(|&block| {
                let block = self.blocks[block as usize];
                block.statements.iter().any(|it| writes(it, local))
                    || terminator_writes(block, local)
            })
        };
        match limit {
            Value::Constant(_) => {}
            Value::Local(limit) if limit != counter && !written_in(limit) => {}
            _ => return false,
        }
        // Every write of the counter in the component is a step the right way, outside `head`.
        let mut steps: SmallVec<[(u32, Step); 2]> = SmallVec::new();
        for &block in component.blocks {
            let data = self.blocks[block as usize];
            if terminator_writes(data, counter) {
                return false;
            }
            for (at, statement) in data.statements.iter().enumerate() {
                if !writes(statement, counter) {
                    continue;
                }
                match self.step(block, at, counter) {
                    Some(step) if step.way == way && block != head => steps.push((block, step)),
                    _ => return false,
                }
            }
        }
        if steps.is_empty() {
            return false;
        }
        // Every cycle through `head` passes a step.
        let mut without_steps: Vec<bool> = component.inside.to_vec(); // alloc: once per loop
        for (block, _) in &steps {
            without_steps[*block as usize] = false;
        }
        if graph.on_cycle(&without_steps, head) {
            return false;
        }
        if steps.iter().all(|(_, step)| step.checked) {
            return true;
        }
        // A step that can wrap: by one, under a strict test, the only one, and at most once
        // between two tests.
        let [(block, step)] = steps.as_slice() else { return false };
        let mut without_head: Vec<bool> = component.inside.to_vec(); // alloc: once per loop
        without_head[head as usize] = false;
        strict && step.amount == 1 && !graph.on_cycle(&without_head, *block)
    }
}

impl Bounding for Counted<'_, '_> {
    fn bounding(&self, graph: &Graph, component: &Component<'_>) -> Option<u32> {
        if self.opaque {
            return None;
        }
        component.blocks.iter().copied().find(|&head| self.bounds(graph, component, head))
    }
}

#[cfg(test)]
mod tests {
    use hegel::{TestCase, generators as gs};

    use super::*;

    /// A fact the model's rule knows of a block: once it has left by another edge `bound`
    /// times, it takes `exit`; running a block of `resets` gives it its `bound` back.
    #[derive(Clone, Debug)]
    struct Fact {
        block: u32,
        exit: u32,
        resets: Vec<u32>,
        bound: u32,
    }

    /// The model's rule: what a fact gives, shaped as the real rules are. A fact
    /// bounds a component its exit leaves and none of its resets is in.
    struct Facts(Vec<Fact>);

    impl Bounding for Facts {
        fn bounding(&self, _: &Graph, component: &Component<'_>) -> Option<u32> {
            self.0
                .iter()
                .find(|fact| {
                    component.has(fact.block)
                        && component.exits(fact.exit)
                        && fact.resets.iter().all(|&it| !component.has(it))
                })
                .map(|fact| fact.block)
        }
    }

    /// Whether some path of `graph` never ends, each fact's block made to take its exit once
    /// its bound is used up: a cycle among the states (a block and each fact's count) reached
    /// from any block with every count at zero. Brute force, every state.
    fn runs_for_ever(graph: &Graph, facts: &[Fact]) -> bool {
        type State = (u32, Vec<u32>);
        let next = |(block, counts): &State| -> Vec<State> {
            let mut counts = counts.clone();
            for (at, fact) in facts.iter().enumerate() {
                if fact.resets.contains(block) {
                    counts[at] = 0;
                }
            }
            let own = facts.iter().position(|fact| fact.block == *block);
            match own {
                Some(at) if counts[at] >= facts[at].bound => vec![(facts[at].exit, counts)],
                Some(at) => graph
                    .successors(*block)
                    .iter()
                    .map(|&to| {
                        let mut counts = counts.clone();
                        if to != facts[at].exit {
                            counts[at] += 1;
                        }
                        (to, counts)
                    })
                    .collect(),
                None => graph.successors(*block).iter().map(|&to| (to, counts.clone())).collect(),
            }
        };
        // Depth first, a state on the path met again is a cycle.
        let mut done: Vec<State> = Vec::new();
        for start in 0..graph.len() as u32 {
            let mut path: Vec<(State, Vec<State>)> = Vec::new();
            let first = (start, vec![0; facts.len()]);
            if done.contains(&first) {
                continue;
            }
            let after = next(&first);
            path.push((first, after));
            while let Some((state, rest)) = path.last_mut() {
                let Some(to) = rest.pop() else {
                    done.push(state.clone());
                    path.pop();
                    continue;
                };
                if path.iter().any(|(on, _)| *on == to) {
                    return true;
                }
                if !done.contains(&to) {
                    let after = next(&to);
                    path.push((to, after));
                }
            }
        }
        false
    }

    /// The oracle: a graph the analysis accepts has no path that runs for ever.
    fn holds(graph: &Graph, facts: &[Fact]) -> bool {
        unbounded(graph, &Facts(facts.to_vec())).is_some() || !runs_for_ever(graph, facts)
    }

    fn below(tc: &TestCase, n: u32) -> u32 {
        tc.draw(gs::integers::<u32>().max_value(n - 1))
    }

    fn drawn(tc: &TestCase) -> (Graph, Vec<Fact>) {
        let blocks = 1 + below(tc, 6);
        let mut graph = Graph::new(blocks as usize);
        for from in 0..blocks {
            for _ in 0..below(tc, 3) {
                graph.edge(from, below(tc, blocks));
            }
        }
        let mut facts: Vec<Fact> = Vec::new();
        for _ in 0..below(tc, 4) {
            let block = below(tc, blocks);
            let targets = graph.successors(block);
            if targets.is_empty() || facts.iter().any(|it| it.block == block) {
                continue;
            }
            let exit = targets[below(tc, targets.len() as u32) as usize];
            let resets = (0..blocks).filter(|&it| it != block && below(tc, 4) == 0).collect();
            facts.push(Fact { block, exit, resets, bound: 1 + below(tc, 2) });
        }
        (graph, facts)
    }

    /// The invariant: a control flow graph the analysis accepts has no infinite path. For random
    /// small graphs with random bounding facts, every accepted graph, each fact's block forced
    /// through its exit once its bound is used up, has only finite paths (checked by brute
    /// force over every state).
    #[hegel::test]
    fn an_accepted_graph_has_no_infinite_path(tc: TestCase) {
        let (graph, facts) = drawn(&tc);
        assert!(holds(&graph, &facts), "accepted, but runs for ever: {facts:?}");
    }

    /// The analysis accepts what ends and refuses what does not, on the shapes the rule is
    /// for: a loop with a fact, the same nested, and a loop whose fact is reset inside it.
    #[test]
    fn nested_bounded_loops_are_accepted_and_a_reset_one_is_not() {
        // 0 -> 1; 1 -> 2 | 5; 2 -> 3 | 1; 3 -> 2; outer head 1 (exit 5), inner head 2 (exit 1).
        let mut graph = Graph::new(6);
        for (from, to) in [(0, 1), (1, 2), (1, 5), (2, 3), (2, 1), (3, 2)] {
            graph.edge(from, to);
        }
        let outer = Fact { block: 1, exit: 5, resets: vec![0], bound: 2 };
        let inner = Fact { block: 2, exit: 1, resets: vec![1], bound: 2 };
        let both = Facts(vec![outer.clone(), inner.clone()]);
        assert_eq!(unbounded(&graph, &both), None);
        assert_eq!(unbounded(&graph, &Facts(vec![outer.clone()])), Some(vec![2, 3]));
        assert_eq!(unbounded(&graph, &Facts(vec![inner.clone()])), Some(vec![1, 2, 3]));
        // The inner loop gives the outer one its bound back: the outer one is not bounded.
        let reset = Fact { resets: vec![3], ..outer };
        assert_eq!(unbounded(&graph, &Facts(vec![reset, inner])), Some(vec![1, 2, 3]));
        assert_eq!(unbounded(&Graph::new(3), &Facts(Vec::new())), None);
    }

    /// Control: with a block that goes to itself not seen as a cycle, the analysis accepts
    /// `0 -> 0`, and the oracle finds its infinite path.
    #[test]
    fn the_oracle_catches_a_self_loop_not_seen_as_a_cycle() {
        let mut graph = Graph::new(1);
        graph.edge(0, 0);
        assert!(holds(&graph, &[]), "refused as it is");
        graph.plant = Some(Plant::SelfLoop);
        assert_eq!(unbounded(&graph, &Facts(Vec::new())), None, "the plant accepts it");
        assert!(!holds(&graph, &[]), "the oracle catches the plant");
    }

    /// Control: with a bounding block taken off a component its exit stays in, the analysis
    /// accepts `0 <-> 1` on a fact of `0` whose exit is `1`, and the oracle finds its infinite
    /// path.
    #[test]
    fn the_oracle_catches_a_bounding_block_whose_exit_stays_inside() {
        let mut graph = Graph::new(2);
        graph.edge(0, 1);
        graph.edge(1, 0);
        let facts = [Fact { block: 0, exit: 1, resets: Vec::new(), bound: 1 }];
        assert!(holds(&graph, &facts), "refused as it is");
        assert!(unbounded(&graph, &Facts(facts.to_vec())).is_some());
        graph.plant = Some(Plant::ExitInside);
        assert_eq!(unbounded(&graph, &Facts(facts.to_vec())), None, "the plant accepts it");
        assert!(!holds(&graph, &facts), "the oracle catches the plant");
    }
}
