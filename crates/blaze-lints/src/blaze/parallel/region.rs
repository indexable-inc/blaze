//! The control flow of a MIR body: which blocks are in a cycle, the blocks of one loop, and
//! which locals are live where.
//!
//! Only the edges control takes when nothing unwinds count: a cleanup block runs once, on the
//! way out of a panic, and what it reads does not carry from one iteration to the next.

use hir_ty::mir::{
    BasicBlockId, LocalId, MirBody, Operand, OperandKind, ProjectionElem, Rvalue, StatementKind,
    StoredPlace, TerminatorKind,
};
use super::effects::index_of;

pub(crate) fn block_index(id: BasicBlockId) -> usize {
    id.into_raw().into_u32() as usize
}

/// Each block's successors when nothing unwinds; none for a cleanup block.
pub(crate) fn successors(body: &MirBody<'_>) -> Vec<Vec<usize>> {
    body.basic_blocks
        .iter()
        .map(|(_, block)| match (&block.terminator, block.is_cleanup) {
            (Some(terminator), false) => {
                terminator.kind.successors().into_iter().map(block_index).collect()
            }
            _ => Vec::new(),
        })
        .collect()
}

/// The nodes of a graph that are in a cycle of it.
pub(crate) struct Cycles {
    in_cycle: Vec<bool>,
}

impl Cycles {
    /// The cycles of `body` through the blocks `keep` holds for.
    pub(crate) fn of(body: &MirBody<'_>, keep: impl Fn(BasicBlockId) -> bool) -> Cycles {
        let successors = successors(body);
        let kept: Vec<bool> = body.basic_blocks.iter().map(|(id, _)| keep(id)).collect();
        Cycles::of_graph(&successors, &kept, None)
    }

    /// The cycles of the graph `successors` through the nodes `kept` holds for, leaving out the
    /// edges into `cut`.
    pub(crate) fn of_graph(successors: &[Vec<usize>], kept: &[bool], cut: Option<usize>) -> Cycles {
        let n = successors.len();
        let edge = |node: usize, at: usize| {
            successors[node].iter().copied().filter(|&it| kept[it] && Some(it) != cut).nth(at)
        };
        // Tarjan's strongly connected components, with an explicit stack.
        let unseen = usize::MAX;
        let mut index = vec![unseen; n];
        let mut low = vec![0; n];
        let mut on_stack = vec![false; n];
        let mut stack: Vec<usize> = Vec::new();
        let mut in_cycle = vec![false; n];
        let mut next = 0;
        let mut work: Vec<(usize, usize)> = Vec::new();
        for root in 0..n {
            if !kept[root] || index[root] != unseen {
                continue;
            }
            index[root] = next;
            low[root] = next;
            next += 1;
            stack.push(root);
            on_stack[root] = true;
            work.push((root, 0));
            while let Some(&(node, at)) = work.last() {
                if let Some(child) = edge(node, at) {
                    if let Some(top) = work.last_mut() {
                        top.1 += 1;
                    }
                    if index[child] == unseen {
                        index[child] = next;
                        low[child] = next;
                        next += 1;
                        stack.push(child);
                        on_stack[child] = true;
                        work.push((child, 0));
                    } else if on_stack[child] {
                        low[node] = low[node].min(index[child]);
                    }
                    continue;
                }
                work.pop();
                if let Some(&(parent, _)) = work.last() {
                    low[parent] = low[parent].min(low[node]);
                }
                if low[node] != index[node] {
                    continue;
                }
                let start = stack.iter().rposition(|&it| it == node).unwrap_or(stack.len());
                let looped = stack.len() - start > 1
                    || (0..).map_while(|at| edge(node, at)).any(|it| it == node);
                for member in stack.drain(start..) {
                    on_stack[member] = false;
                    in_cycle[member] = looped;
                }
            }
        }
        Cycles { in_cycle }
    }

    pub(crate) fn contains(&self, block: BasicBlockId) -> bool {
        self.contains_index(block_index(block))
    }

    pub(crate) fn contains_index(&self, block: usize) -> bool {
        self.in_cycle.get(block).copied().unwrap_or(false)
    }
}

/// The blocks of the loop whose header is `header`: those reachable from it that reach it
/// back.
pub(crate) fn natural_loop(successors: &[Vec<usize>], header: usize) -> Vec<bool> {
    let n = successors.len();
    let mut forward = vec![false; n];
    let mut work = vec![header];
    forward[header] = true;
    while let Some(node) = work.pop() {
        for &next in &successors[node] {
            if !forward[next] {
                forward[next] = true;
                work.push(next);
            }
        }
    }
    let mut predecessors = vec![Vec::new(); n];
    for (node, next) in successors.iter().enumerate() {
        for &it in next {
            predecessors[it].push(node);
        }
    }
    let mut back = vec![false; n];
    let mut work = vec![header];
    back[header] = true;
    while let Some(node) = work.pop() {
        for &previous in &predecessors[node] {
            if forward[previous] && !back[previous] {
                back[previous] = true;
                work.push(previous);
            }
        }
    }
    back
}

/// A set of locals.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Locals(Vec<u64>);

impl Locals {
    pub(crate) fn new(n: usize) -> Locals {
        Locals(vec![0; n.div_ceil(64)])
    }

    pub(crate) fn insert(&mut self, local: usize) {
        self.0[local / 64] |= 1 << (local % 64);
    }

    pub(crate) fn remove(&mut self, local: usize) {
        self.0[local / 64] &= !(1 << (local % 64));
    }

    pub(crate) fn contains(&self, local: usize) -> bool {
        self.0[local / 64] & (1 << (local % 64)) != 0
    }

    fn union(&mut self, other: &Locals) {
        for (it, other) in self.0.iter_mut().zip(&other.0) {
            *it |= other;
        }
    }
}

/// Which locals are live where a block starts: read later on some path before they are
/// written whole.
pub(crate) struct Liveness {
    live_in: Vec<Locals>,
}

impl Liveness {
    pub(crate) fn of(body: &MirBody<'_>, successors: &[Vec<usize>]) -> Liveness {
        let n = body.locals.len();
        let blocks: Vec<_> = body.basic_blocks.iter().map(|(_, it)| it).collect();
        let mut live_in = vec![Locals::new(n); blocks.len()];
        loop {
            let mut changed = false;
            for index in (0..blocks.len()).rev() {
                let block = blocks[index];
                if block.is_cleanup {
                    continue;
                }
                let mut live = Locals::new(n);
                for &next in &successors[index] {
                    live.union(&live_in[next]);
                }
                if let Some(terminator) = &block.terminator {
                    terminator_transfer(&terminator.kind, &mut live);
                }
                for statement in block.statements.iter().rev() {
                    statement_transfer(&statement.kind, &mut live);
                }
                if live != live_in[index] {
                    live_in[index] = live;
                    changed = true;
                }
            }
            if !changed {
                return Liveness { live_in };
            }
        }
    }

    /// Whether `local` is live where `block` starts.
    pub(crate) fn live_in(&self, block: usize, local: LocalId) -> bool {
        self.live_in[block].contains(index_of(local))
    }
}

/// The locals a place reads when it is read: its base, and the locals it is indexed by.
fn use_place(place: &StoredPlace, live: &mut Locals) {
    live.insert(index_of(place.local));
    use_indices(place, live);
}

fn use_indices(place: &StoredPlace, live: &mut Locals) {
    for elem in place.projection.as_slice() {
        if let ProjectionElem::Index(local) = elem {
            live.insert(index_of(*local));
        }
    }
}

/// A write to `place`: a write of a whole local ends its life; a write into part of one, or
/// through a pointer, reads the pointer and the indices.
fn def_place(place: &StoredPlace, live: &mut Locals) {
    let projection = place.projection.as_slice();
    if projection.is_empty() {
        live.remove(index_of(place.local));
        return;
    }
    if projection.iter().any(|it| matches!(it, ProjectionElem::Deref)) {
        live.insert(index_of(place.local));
    }
    use_indices(place, live);
}

fn use_operand(operand: &Operand, live: &mut Locals) {
    if let OperandKind::Copy(place) | OperandKind::Move(place) = &operand.kind {
        use_place(place, live);
    }
}

fn use_rvalue(rvalue: &Rvalue, live: &mut Locals) {
    match rvalue {
        Rvalue::Ref(_, place)
        | Rvalue::Len(place)
        | Rvalue::Discriminant(place)
        | Rvalue::CopyForDeref(place) => use_place(place, live),
        _ => rvalue.for_each_operand(&mut |it| use_operand(it, live)),
    }
}

fn statement_transfer(kind: &StatementKind, live: &mut Locals) {
    match kind {
        StatementKind::Assign(place, rvalue) => {
            def_place(place, live);
            use_rvalue(rvalue, live);
        }
        StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
            live.remove(index_of(*local));
        }
        StatementKind::FakeRead(place) => use_place(place, live),
        StatementKind::Deinit(place) => def_place(place, live),
        StatementKind::InlineAsm(_) => kind.for_each_operand(&mut |it| use_operand(it, live)),
        StatementKind::Nop => {}
    }
}

fn terminator_transfer(kind: &TerminatorKind, live: &mut Locals) {
    match kind {
        TerminatorKind::Call { destination, .. } => def_place(destination, live),
        TerminatorKind::Drop { place, .. } => use_place(place, live),
        TerminatorKind::DropAndReplace { place, .. } => {
            def_place(place, live);
            use_place(place, live);
        }
        TerminatorKind::Return => live.insert(0),
        TerminatorKind::Yield { resume_arg, .. } => def_place(resume_arg, live),
        _ => {}
    }
    kind.for_each_operand(&mut |it| use_operand(it, live));
}
