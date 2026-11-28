//! Unified sans-IO machine state-graph verifier and Lean 4 transition-system emitter
//! (`docs/v2/plan/design/sans-io-machines.md`).
//!
//! Pure `#![no_std]`, zero global heap, `#![forbid(unsafe_code)]`, strictly bounded iterative
//! passes (rule 18 `bounded-progress`, no recursion):
//!
//! 1. **Reachability:** valid `entry` and edge endpoints; every declared state is reachable from
//!    `entry`.
//! 2. **Progress (dead ends & unmarked cycles):** every reachable state has an outgoing edge and
//!    can reach a terminal (`Done`, `Fail`, `Abandon`) or an explicit `loop_state: true` state.
//! 3. **Path-sensitive linear handle discipline:** tracks up to 32 linear handles in a `u32`
//!    bitmask via bounded forward worklist, refusing `DoubleOpen`, `DoubleClose`, inconsistent
//!    handle masks at state merges, and non-zero live masks on `Done` or `Fail` (while allowing
//!    `Abandon` with live handles for driver table reclamation on `Stopped`).
//! 4. **Bounded cancellation cleanup:** every `CancelCleanup` chain from any state terminates in
//!    `Done`/`Fail`/`Abandon` within `graph.grace_max_steps` steps (no unbounded cleanup loops).
//! 5. **Lean 4 emission:** [`emit_lean`] emits a self-contained Lean 4 transition system
//!    (`State`, `Step`, `legalTerminal`, `verified_machine_no_leak`) for formal refinement proofs.

use core::fmt;

use blaze_own::FixedVec;
use blaze_own::planted::{NoPlant, Planted, Plants};

/// Hard upper bound on states in one verified [`MachineGraph`] (rule 18 `bounded-progress`).
pub const MAX_GRAPH_STATES: usize = 256;

/// Dense or sparse identifier of a state node in a [`MachineGraph`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct StateId(pub u16);

/// Slot index (`0..32`) of one tracked linear handle in a machine's `u32` live-handle bitmask.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct HandleSlot(pub u8);

impl HandleSlot {
    /// Maximum number of distinct linear handle slots tracked per machine (`32`).
    pub const MAX_SLOTS: u8 = 32;

    /// Single-bit mask `1u32 << self.0`, or `None` when `self.0 >= 32`.
    #[inline]
    pub const fn bit(self) -> Option<u32> {
        if self.0 < Self::MAX_SLOTS {
            Some(1u32 << self.0)
        } else {
            None
        }
    }
}

/// Linear handle effect performed along one transition edge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HandleOp {
    /// Transition neither opens nor consumes a tracked linear handle.
    None,
    /// Transition opens/acquires the linear handle in `slot` (must not already be live).
    Open(HandleSlot),
    /// Transition closes the linear handle in `slot` (must currently be live).
    Close(HandleSlot),
    /// Transition transfers ownership of the linear handle in `slot` out to the caller (must
    /// currently be live; clears the bit before terminal leak checks).
    TransferOut(HandleSlot),
}

/// Target/outcome classification of one [`GraphEdge`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EdgeKind {
    /// Normal request/resume edge moving to waiting state `to`.
    Ask { to: StateId },
    /// Normal successful completion (`Turn::Done`).
    Done,
    /// Normal typed error exit (`Turn::Fail`).
    Fail,
    /// Graceful cancellation cleanup step (`CancelAction::Cleanup`) moving to state `to`.
    CancelCleanup { to: StateId },
    /// Immediate abandonment on external `Stopped` (`CancelAction::Abandon`); live handles are
    /// reclaimed by the driver's handle table.
    Abandon,
}

/// One directed transition edge in a [`MachineGraph`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GraphEdge {
    pub from: StateId,
    pub kind: EdgeKind,
    pub handle_op: HandleOp,
    pub label: &'static str,
}

/// One state node in a [`MachineGraph`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StateNode {
    pub id: StateId,
    pub name: &'static str,
    pub loop_state: bool,
    pub initial_handles: u32,
}

/// Reflected state graph of a sans-IO [`crate::Machine`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MachineGraph<'a> {
    pub name: &'static str,
    pub entry: StateId,
    pub grace_max_steps: u32,
    pub states: &'a [StateNode],
    pub edges: &'a [GraphEdge],
}

/// Typed verification failure naming the exact state, slot, mask, or bound violated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GraphError {
    TooManyStates { count: usize, max: usize },
    DuplicateStateId(StateId),
    InvalidEntry(StateId),
    InvalidEdgeSource(StateId),
    InvalidEdgeTarget { from: StateId, to: StateId },
    InvalidHandleSlot { from: StateId, slot: HandleSlot },
    UnreachableState(StateId),
    DeadEnd(StateId),
    UnmarkedCycle(StateId),
    HandleLeakedOnDone { from: StateId, live_mask: u32 },
    HandleLeakedOnFail { from: StateId, live_mask: u32 },
    DoubleClose { from: StateId, slot: HandleSlot },
    DoubleOpen { from: StateId, slot: HandleSlot },
    HandleMaskConflict { state: StateId, a: u32, b: u32 },
    UnboundedCancelCleanup { start: StateId, steps: u32, max_steps: u32 },
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            GraphError::TooManyStates { count, max } => {
                write!(f, "machine graph has {count} states, exceeding cap {max}")
            }
            GraphError::DuplicateStateId(id) => {
                write!(f, "machine graph declares duplicate state id {}", id.0)
            }
            GraphError::InvalidEntry(id) => {
                write!(f, "machine graph entry state {} is not declared", id.0)
            }
            GraphError::InvalidEdgeSource(id) => {
                write!(f, "machine graph edge source state {} is not declared", id.0)
            }
            GraphError::InvalidEdgeTarget { from, to } => {
                write!(f, "edge from state {} targets undeclared state {}", from.0, to.0)
            }
            GraphError::InvalidHandleSlot { from, slot } => {
                write!(
                    f,
                    "edge from state {} names handle slot {} >= {}",
                    from.0,
                    slot.0,
                    HandleSlot::MAX_SLOTS
                )
            }
            GraphError::UnreachableState(id) => {
                write!(f, "state {} is unreachable from entry", id.0)
            }
            GraphError::DeadEnd(id) => {
                write!(f, "reachable state {} has no outgoing edges", id.0)
            }
            GraphError::UnmarkedCycle(id) => {
                write!(
                    f,
                    "state {} lies on an unmarked cycle that cannot reach a terminal or loop_state",
                    id.0
                )
            }
            GraphError::HandleLeakedOnDone { from, live_mask } => {
                write!(f, "Done edge from state {} leaks linear handle mask {live_mask:#x}", from.0)
            }
            GraphError::HandleLeakedOnFail { from, live_mask } => {
                write!(f, "Fail edge from state {} leaks linear handle mask {live_mask:#x}", from.0)
            }
            GraphError::DoubleClose { from, slot } => {
                write!(
                    f,
                    "edge from state {} closes/transfers handle slot {} which is not live",
                    from.0, slot.0
                )
            }
            GraphError::DoubleOpen { from, slot } => {
                write!(
                    f,
                    "edge from state {} opens handle slot {} which is already live",
                    from.0, slot.0
                )
            }
            GraphError::HandleMaskConflict { state, a, b } => {
                write!(
                    f,
                    "state {} reached with conflicting live handle masks {a:#x} vs {b:#x}",
                    state.0
                )
            }
            GraphError::UnboundedCancelCleanup { start, steps, max_steps } => {
                write!(
                    f,
                    "CancelCleanup from state {} takes {steps} step(s), exceeding grace_max_steps {max_steps}",
                    start.0
                )
            }
        }
    }
}

/// Planted-bug seam for [`verify_graph_with`] controls (rules 43 & 53).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GraphPlant {
    /// Planted bug: skip the unmarked-cycle / reachability-to-terminal check.
    SkipCycleCheck,
    /// Planted bug: skip checking `live_mask == 0` on `Done` and `Fail` edges.
    SkipLeakCheck,
    /// Planted bug: skip checking that `CancelCleanup` chains terminate within `grace_max_steps`.
    SkipCancelBoundCheck,
}

impl Plants for GraphPlant {}

#[inline]
fn find_state_idx(states: &[StateNode], id: StateId) -> Option<usize> {
    let mut i = 0;
    while i < states.len() {
        if states[i].id == id {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[inline]
fn apply_handle_op(from: StateId, mask: u32, op: HandleOp) -> Result<u32, GraphError> {
    match op {
        HandleOp::None => Ok(mask),
        HandleOp::Open(slot) => {
            let Some(bit) = slot.bit() else {
                return Err(GraphError::InvalidHandleSlot { from, slot });
            };
            if (mask & bit) != 0 {
                return Err(GraphError::DoubleOpen { from, slot });
            }
            Ok(mask | bit)
        }
        HandleOp::Close(slot) | HandleOp::TransferOut(slot) => {
            let Some(bit) = slot.bit() else {
                return Err(GraphError::InvalidHandleSlot { from, slot });
            };
            if (mask & bit) == 0 {
                return Err(GraphError::DoubleClose { from, slot });
            }
            Ok(mask & !bit)
        }
    }
}

/// Verifies `graph` against all structural, progress, linear-handle, and bounded-cleanup
/// invariants with zero planted bugs.
#[inline]
pub fn verify_graph(graph: &MachineGraph<'_>) -> Result<(), GraphError> {
    verify_graph_with(graph, NoPlant)
}

/// Verifies `graph` through the [`Planted`] seam for planted-bug controls.
pub fn verify_graph_with<P: Planted<GraphPlant>>(
    graph: &MachineGraph<'_>,
    plant: P,
) -> Result<(), GraphError> {
    let n = graph.states.len();
    if n > MAX_GRAPH_STATES {
        return Err(GraphError::TooManyStates { count: n, max: MAX_GRAPH_STATES });
    }

    // Check duplicate state ids.
    for i in 0..n {
        let id = graph.states[i].id;
        for j in (i + 1)..n {
            if graph.states[j].id == id {
                return Err(GraphError::DuplicateStateId(id));
            }
        }
    }

    let Some(entry_idx) = find_state_idx(graph.states, graph.entry) else {
        return Err(GraphError::InvalidEntry(graph.entry));
    };

    // Validate all edge sources, targets, and handle slot bounds upfront.
    for edge in graph.edges {
        if find_state_idx(graph.states, edge.from).is_none() {
            return Err(GraphError::InvalidEdgeSource(edge.from));
        }
        match edge.kind {
            EdgeKind::Ask { to } | EdgeKind::CancelCleanup { to } => {
                if find_state_idx(graph.states, to).is_none() {
                    return Err(GraphError::InvalidEdgeTarget { from: edge.from, to });
                }
            }
            EdgeKind::Done | EdgeKind::Fail | EdgeKind::Abandon => {}
        }
        match edge.handle_op {
            HandleOp::None => {}
            HandleOp::Open(slot) | HandleOp::Close(slot) | HandleOp::TransferOut(slot) => {
                if slot.bit().is_none() {
                    return Err(GraphError::InvalidHandleSlot { from: edge.from, slot });
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Pass 1: Reachability from `entry` (bounded iterative DFS/BFS worklist).
    // -----------------------------------------------------------------------
    let mut reachable = [false; MAX_GRAPH_STATES];
    let mut worklist: FixedVec<usize, MAX_GRAPH_STATES> = FixedVec::new();
    reachable[entry_idx] = true;
    if worklist.push(entry_idx).is_err() {
        unreachable!("n <= MAX_GRAPH_STATES and entry_idx is pushed once");
    }

    while let Some(u) = worklist.pop() {
        let u_id = graph.states[u].id;
        for edge in graph.edges {
            if edge.from != u_id {
                continue;
            }
            match edge.kind {
                EdgeKind::Ask { to } | EdgeKind::CancelCleanup { to } => {
                    let Some(v) = find_state_idx(graph.states, to) else {
                        unreachable!("edge targets validated above");
                    };
                    if !reachable[v] {
                        reachable[v] = true;
                        if worklist.push(v).is_err() {
                            unreachable!("each state index is pushed at most once");
                        }
                    }
                }
                EdgeKind::Done | EdgeKind::Fail | EdgeKind::Abandon => {}
            }
        }
    }

    for (idx, state) in graph.states.iter().enumerate() {
        if !reachable[idx] {
            return Err(GraphError::UnreachableState(state.id));
        }
    }

    // -----------------------------------------------------------------------
    // Pass 2: Dead-end and unmarked-cycle check.
    // Every reachable state must have at least one outgoing edge, and must be
    // able to reach a terminal (`Done`/`Fail`/`Abandon`) or `loop_state: true`.
    // -----------------------------------------------------------------------
    let mut has_outgoing = [false; MAX_GRAPH_STATES];
    let mut can_escape = [false; MAX_GRAPH_STATES];

    for (idx, state) in graph.states.iter().enumerate() {
        if state.loop_state {
            can_escape[idx] = true;
        }
    }

    for edge in graph.edges {
        let Some(u) = find_state_idx(graph.states, edge.from) else {
            unreachable!("edge sources validated above");
        };
        has_outgoing[u] = true;
        match edge.kind {
            EdgeKind::Done | EdgeKind::Fail | EdgeKind::Abandon => {
                can_escape[u] = true;
            }
            EdgeKind::Ask { .. } | EdgeKind::CancelCleanup { .. } => {}
        }
    }

    for (idx, state) in graph.states.iter().enumerate() {
        if !has_outgoing[idx] {
            return Err(GraphError::DeadEnd(state.id));
        }
    }

    if !plant.has(GraphPlant::SkipCycleCheck) {
        // Bounded fixed-point backward reachability (at most `n` passes).
        for _ in 0..n {
            let mut changed = false;
            for edge in graph.edges {
                match edge.kind {
                    EdgeKind::Ask { to } | EdgeKind::CancelCleanup { to } => {
                        let Some(u) = find_state_idx(graph.states, edge.from) else {
                            unreachable!("edge sources validated above");
                        };
                        let Some(v) = find_state_idx(graph.states, to) else {
                            unreachable!("edge targets validated above");
                        };
                        if !can_escape[u] && can_escape[v] {
                            can_escape[u] = true;
                            changed = true;
                        }
                    }
                    EdgeKind::Done | EdgeKind::Fail | EdgeKind::Abandon => {}
                }
            }
            if !changed {
                break;
            }
        }

        // If any state cannot reach a terminal or explicit loop_state, walk forward
        // at most `n` steps to pinpoint a state actually lying on the unmarked cycle.
        for start in 0..n {
            if !can_escape[start] {
                let mut seen = [false; MAX_GRAPH_STATES];
                let mut cur = start;
                for _ in 0..=n {
                    if seen[cur] {
                        return Err(GraphError::UnmarkedCycle(graph.states[cur].id));
                    }
                    seen[cur] = true;
                    let cur_id = graph.states[cur].id;
                    let mut next = None;
                    for edge in graph.edges {
                        if edge.from != cur_id {
                            continue;
                        }
                        match edge.kind {
                            EdgeKind::Ask { to } | EdgeKind::CancelCleanup { to } => {
                                next = find_state_idx(graph.states, to);
                                break;
                            }
                            EdgeKind::Done | EdgeKind::Fail | EdgeKind::Abandon => {}
                        }
                    }
                    let Some(next_idx) = next else {
                        unreachable!("non-escaping state has outgoing non-terminal edge");
                    };
                    cur = next_idx;
                }
                return Err(GraphError::UnmarkedCycle(graph.states[start].id));
            }
        }
    }

    // -----------------------------------------------------------------------
    // Pass 3: Forward path-sensitive handle-bitmask check (bounded worklist).
    // Each state acquires at most one canonical `u32` live-handle bitmask;
    // conflicting merges fail immediately, so the worklist pops <= `n` times.
    // -----------------------------------------------------------------------
    let mut known_mask: [Option<u32>; MAX_GRAPH_STATES] = [None; MAX_GRAPH_STATES];
    worklist.clear();
    known_mask[entry_idx] = Some(graph.states[entry_idx].initial_handles);
    if worklist.push(entry_idx).is_err() {
        unreachable!("n <= MAX_GRAPH_STATES and entry_idx is pushed once");
    }

    while let Some(u) = worklist.pop() {
        let u_id = graph.states[u].id;
        let Some(mask) = known_mask[u] else {
            unreachable!("every state in worklist has a known handle mask");
        };
        for edge in graph.edges {
            if edge.from != u_id {
                continue;
            }
            let next_mask = apply_handle_op(u_id, mask, edge.handle_op)?;
            match edge.kind {
                EdgeKind::Ask { to } | EdgeKind::CancelCleanup { to } => {
                    let Some(v) = find_state_idx(graph.states, to) else {
                        unreachable!("edge targets validated above");
                    };
                    match known_mask[v] {
                        None => {
                            known_mask[v] = Some(next_mask);
                            if worklist.push(v).is_err() {
                                unreachable!("each state index is pushed at most once");
                            }
                        }
                        Some(prev) => {
                            if prev != next_mask {
                                return Err(GraphError::HandleMaskConflict {
                                    state: to,
                                    a: prev,
                                    b: next_mask,
                                });
                            }
                        }
                    }
                }
                EdgeKind::Done => {
                    if !plant.has(GraphPlant::SkipLeakCheck) && next_mask != 0 {
                        return Err(GraphError::HandleLeakedOnDone {
                            from: u_id,
                            live_mask: next_mask,
                        });
                    }
                }
                EdgeKind::Fail => {
                    if !plant.has(GraphPlant::SkipLeakCheck) && next_mask != 0 {
                        return Err(GraphError::HandleLeakedOnFail {
                            from: u_id,
                            live_mask: next_mask,
                        });
                    }
                }
                EdgeKind::Abandon => {
                    // Abandoning with non-zero live handles is permitted: the handler's
                    // `HandleTable` reclaims task-owned handles on external `Stopped`.
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Pass 4: Bounded `CancelCleanup` chain termination check.
    // Compute longest non-terminal path length `max_depth[u]` over `n`
    // Bellman-Ford passes: any node that can reach a non-terminal cycle has
    // `max_depth[u] >= n`; every acyclic node has its exact longest path
    // length `< n`.
    // -----------------------------------------------------------------------
    if !plant.has(GraphPlant::SkipCancelBoundCheck) {
        let mut max_depth = [0u32; MAX_GRAPH_STATES];
        for _ in 0..n {
            let mut changed = false;
            for edge in graph.edges {
                match edge.kind {
                    EdgeKind::Ask { to } | EdgeKind::CancelCleanup { to } => {
                        let Some(u) = find_state_idx(graph.states, edge.from) else {
                            unreachable!("edge sources validated above");
                        };
                        let Some(v) = find_state_idx(graph.states, to) else {
                            unreachable!("edge targets validated above");
                        };
                        let cand = max_depth[v].saturating_add(1);
                        if cand > max_depth[u] {
                            max_depth[u] = cand;
                            changed = true;
                        }
                    }
                    EdgeKind::Done | EdgeKind::Fail | EdgeKind::Abandon => {}
                }
            }
            if !changed {
                break;
            }
        }

        let n_u32 = n as u32;
        for edge in graph.edges {
            if let EdgeKind::CancelCleanup { to } = edge.kind {
                let Some(v) = find_state_idx(graph.states, to) else {
                    unreachable!("edge targets validated above");
                };
                let steps = if max_depth[v] >= n_u32 {
                    // Target can reach a non-terminal cycle -> unbounded cleanup loop.
                    graph.grace_max_steps.saturating_add(1)
                } else {
                    max_depth[v].saturating_add(1)
                };
                if steps > graph.grace_max_steps {
                    return Err(GraphError::UnboundedCancelCleanup {
                        start: edge.from,
                        steps,
                        max_steps: graph.grace_max_steps,
                    });
                }
            }
        }
    }

    Ok(())
}

fn write_lean_ident(out: &mut impl fmt::Write, raw: &str) -> fmt::Result {
    if raw.is_empty() {
        return out.write_str("unnamed");
    }
    for (idx, ch) in raw.chars().enumerate() {
        let valid = ch.is_ascii_alphanumeric() || ch == '_';
        if idx == 0 && ch.is_ascii_digit() {
            out.write_char('_')?;
        }
        if valid {
            out.write_char(ch)?;
        } else {
            out.write_char('_')?;
        }
    }
    Ok(())
}

fn state_name(graph: &MachineGraph<'_>, id: StateId) -> Result<&'static str, fmt::Error> {
    let idx = find_state_idx(graph.states, id).ok_or(fmt::Error)?;
    Ok(graph.states[idx].name)
}

fn write_next_mask(out: &mut impl fmt::Write, op: HandleOp) -> fmt::Result {
    match op {
        HandleOp::None => write!(out, "m"),
        HandleOp::Open(slot) => write!(out, "(m ||| (1 <<< {}))", slot.0),
        HandleOp::Close(slot) | HandleOp::TransferOut(slot) => {
            write!(out, "(m - (1 <<< {}))", slot.0)
        }
    }
}

/// Emits a Lean 4 transition system (`namespace`, `inductive State`, `inductive Step`,
/// `def legalTerminal`, and `theorem verified_machine_no_leak`) for formal refinement proofs.
pub fn emit_lean(graph: &MachineGraph<'_>, out: &mut impl fmt::Write) -> fmt::Result {
    let entry_name = state_name(graph, graph.entry)?;

    writeln!(out, "-- Generated by blaze-machine::graph::emit_lean")?;
    write!(out, "-- Machine: ")?;
    write_lean_ident(out, graph.name)?;
    write!(out, " (entry: ")?;
    write_lean_ident(out, entry_name)?;
    writeln!(out, ", grace_max_steps: {})", graph.grace_max_steps)?;

    write!(out, "namespace ")?;
    write_lean_ident(out, graph.name)?;
    writeln!(out)?;
    writeln!(out)?;

    writeln!(out, "inductive State where")?;
    for state in graph.states {
        write!(out, "  | ")?;
        write_lean_ident(out, state.name)?;
        writeln!(out)?;
    }
    writeln!(out, "  | terminalDone")?;
    writeln!(out, "  | terminalFail")?;
    writeln!(out, "  | terminalAbandoned")?;
    writeln!(out, "  deriving DecidableEq, Repr")?;
    writeln!(out)?;

    writeln!(out, "inductive Step : State -> Nat -> State -> Nat -> Prop where")?;
    for (idx, edge) in graph.edges.iter().enumerate() {
        let from_name = state_name(graph, edge.from)?;
        write!(out, "  | step_{idx}_")?;
        write_lean_ident(out, edge.label)?;
        write!(out, " (m : Nat)")?;

        match edge.handle_op {
            HandleOp::None => {}
            HandleOp::Open(slot) => {
                write!(out, " (hopen : (m &&& (1 <<< {})) = 0)", slot.0)?;
            }
            HandleOp::Close(slot) | HandleOp::TransferOut(slot) => {
                write!(out, " (hclose : (m &&& (1 <<< {})) != 0)", slot.0)?;
            }
        }

        match edge.kind {
            EdgeKind::Ask { to } | EdgeKind::CancelCleanup { to } => {
                let to_name = state_name(graph, to)?;
                write!(out, " : Step .")?;
                write_lean_ident(out, from_name)?;
                write!(out, " m .")?;
                write_lean_ident(out, to_name)?;
                write!(out, " ")?;
                write_next_mask(out, edge.handle_op)?;
                writeln!(out)?;
            }
            EdgeKind::Done => {
                write!(out, " (hzero : ")?;
                write_next_mask(out, edge.handle_op)?;
                write!(out, " = 0) : Step .")?;
                write_lean_ident(out, from_name)?;
                writeln!(out, " m .terminalDone 0")?;
            }
            EdgeKind::Fail => {
                write!(out, " (hzero : ")?;
                write_next_mask(out, edge.handle_op)?;
                write!(out, " = 0) : Step .")?;
                write_lean_ident(out, from_name)?;
                writeln!(out, " m .terminalFail 0")?;
            }
            EdgeKind::Abandon => {
                write!(out, " : Step .")?;
                write_lean_ident(out, from_name)?;
                writeln!(out, " m .terminalAbandoned 0")?;
            }
        }
    }
    writeln!(out)?;

    writeln!(out, "def legalTerminal (s : State) (m : Nat) : Prop :=")?;
    writeln!(out, "  match s with")?;
    writeln!(out, "  | .terminalDone => m = 0")?;
    writeln!(out, "  | .terminalFail => m = 0")?;
    writeln!(out, "  | .terminalAbandoned => True")?;
    writeln!(out, "  | _ => True")?;
    writeln!(out)?;

    writeln!(out, "theorem verified_machine_no_leak")?;
    writeln!(out, "    (s s' : State) (m m' : Nat) (h : Step s m s' m') :")?;
    writeln!(out, "    legalTerminal s' m' := by")?;
    writeln!(out, "  cases h <;> simp [legalTerminal]")?;
    writeln!(out)?;

    write!(out, "end ")?;
    write_lean_ident(out, graph.name)?;
    writeln!(out)?;

    Ok(())
}
