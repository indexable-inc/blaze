//! Zero-`Pin` sans-IO Machine ABI lowering and verification pass for one-shot lexical `.do`
//! coroutines (`docs/v2/plan/design/sans-io-machines.md`).
//!
//! # Why this exists alongside [`crate::coroutine`]
//!
//! [`crate::coroutine`] lowers Rust `async fn` / `Future` bodies where stack slots whose address
//! was taken (`Op::SlotAddr`) may remain live across `Op::Suspend`, because `Future::poll` pins
//! `Pin<&mut State>` so self-referential addresses into the state remain valid across polls.
//!
//! One-shot lexical `.do` workflows forbid local borrows across suspension points so the compiled
//! machine state is **freely movable with zero `Pin`** (enabling cross-thread `TaskFrame`
//! migration without pinning). This module:
//!
//! 1. Verifies the input LIR body ([`crate::verify::verify_in`]).
//! 2. Enforces the zero-`Pin` invariant via an exact forward may-dataflow analysis over
//!    `Op::SlotAddr` and `Op::SlotDead` across every reachable `Op::Suspend` (plus an SSA
//!    crossing check for any `Op::SlotAddr` value used past a suspend), rejecting any escaping
//!    local slot address with [`MachineError::LocalSlotAddrAcrossSuspend`].
//! 3. Delegates SSA splitting, state layout, unwind poisoning, and output verification to
//!    [`crate::coroutine::split_planted_with`].
//! 4. Extracts an explicit [`MachineGraphSummary`] of state-to-state transitions (`Suspend`,
//!    `Return`, `Cancel`) via a bounded iterative CFG walk with no recursion.

use alloc::alloc::Allocator;
use alloc::vec::Vec;
use core::fmt;

use crate::{
    Block, Func, Op, Ty, View,
    coroutine::{CoroutineError, Panics, Plant, Shape, StateLayout},
    func::{NONE, SlotId},
    op::{Field, Layout, Word, walk},
};

/// ABI metadata for lowering a one-shot lexical `.do` coroutine body into a zero-`Pin` machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MachineShape {
    /// Which entry parameter is the state pointer (`ptr`).
    pub state_param: u32,
    /// Bytes at the front of the state holding captured inputs/upvars.
    pub prefix_size: u32,
    /// Log2 alignment of the prefix.
    pub prefix_align: u8,
    /// Block that cleans up an unresumed machine, ending in `Op::Cancelled`.
    pub unresumed: Block,
    /// Bounded cancellation grace budget (`GraceBudget`) associated with this machine.
    pub grace_max_steps: u32,
}

impl MachineShape {
    /// Converts this machine shape to the underlying [`Shape`] consumed by [`crate::coroutine`].
    pub const fn to_coroutine_shape(self) -> Shape {
        Shape {
            state_param: self.state_param,
            prefix_size: self.prefix_size,
            prefix_align: self.prefix_align,
            unresumed: self.unresumed,
        }
    }
}

/// Planted bug selectors for simulation and oracle controls.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MachinePlant {
    None,
    /// Skip the zero-`Pin` `Op::SlotAddr` across `Op::Suspend` verification check.
    AllowEscapedSlotAddr,
    /// Omit storing the next state tag before returning on `Op::Suspend`.
    SkipTagStore,
    /// Dispatch resume tag `k` to the next suspend point's resume block.
    WrongResumeTarget,
}

/// Why a one-shot `.do` coroutine body cannot be lowered to the zero-`Pin` Machine ABI.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MachineError {
    /// Ordinary LIR verification or coroutine-splitting error.
    Inner(CoroutineError),
    /// A stack slot's address (`Op::SlotAddr`) survived across an `Op::Suspend` without an
    /// intervening `Op::SlotDead`, violating the zero-`Pin` relocation invariant.
    LocalSlotAddrAcrossSuspend { slot: SlotId, point: u32, inst: u32 },
    /// A suspend point exists in the body whose resume block is unreachable from entry.
    UnreachableResume { point: u32 },
}

impl From<CoroutineError> for MachineError {
    fn from(err: CoroutineError) -> Self {
        MachineError::Inner(err)
    }
}

impl fmt::Display for MachineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MachineError::Inner(err) => fmt::Display::fmt(err, f),
            MachineError::LocalSlotAddrAcrossSuspend { slot, point, inst } => write!(
                f,
                "v{inst}: local slot {slot} has its address live across suspend point {point} (violates zero-Pin Machine ABI)"
            ),
            MachineError::UnreachableResume { point } => {
                write!(f, "suspend point {point} has an unreachable resume block")
            }
        }
    }
}

impl core::error::Error for MachineError {}

/// Kind of state transition in a lowered machine's CFG summary.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum MachineEdgeKind {
    Suspend,
    Return,
    Cancel,
}

/// One transition edge in a lowered machine's state graph.
///
/// `from_point == None` denotes the entry (`Unresumed`) state; `Some(k)` denotes suspend point `k`.
/// `to_point == Some(j)` for `Suspend` edges and `None` for terminal `Return` / `Cancel` edges.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MachineEdge {
    pub from_point: Option<u32>,
    pub to_point: Option<u32>,
    pub kind: MachineEdgeKind,
}

/// Exact CFG/transition summary of the lowered zero-`Pin` machine states.
#[derive(Clone, Debug)]
pub struct MachineGraphSummary<A: Allocator> {
    pub points: u32,
    pub state_size: u32,
    pub state_align: u8,
    pub zero_pin_proven: bool,
    pub edges: Vec<MachineEdge, A>,
}

/// Lowered zero-`Pin` machine: its `step` and `on_cancel` functions, state layout, and graph
/// summary.
#[derive(Clone, Debug)]
pub struct MachineSplit<A: Allocator> {
    pub step: Func<A>,
    pub on_cancel: Func<A>,
    pub layout: StateLayout<A>,
    pub graph: MachineGraphSummary<A>,
}

/// Lowers `body` into a verified zero-`Pin` sans-IO machine (`step`, `on_cancel`, `layout`, and
/// `graph`), allocating all scratch and output buffers in `alloc`.
pub fn split_machine<A: Allocator + Clone>(
    body: View<'_>,
    shape: MachineShape,
    alloc: A,
) -> Result<MachineSplit<A>, MachineError> {
    split_machine_with_plant(body, shape, MachinePlant::None, alloc)
}

/// [`split_machine`] with an explicit [`MachinePlant`] bug-injection selector for tests and
/// simulation controls.
pub fn split_machine_with_plant<A: Allocator + Clone>(
    body: View<'_>,
    shape: MachineShape,
    plant: MachinePlant,
    alloc: A,
) -> Result<MachineSplit<A>, MachineError> {
    // 1. Verify the input LIR body before any analysis.
    crate::verify::verify_in(body, &alloc).map_err(|err| MachineError::Inner(CoroutineError::Verify(err)))?;

    let scratch = &alloc;
    let in_poll = reach(&body, &[0], false, scratch);
    let suspends = collect_suspends(&body, &in_poll, scratch);

    // 2. Enforce the zero-Pin invariant: no local stack slot's address may survive across any
    //    Op::Suspend without an intervening Op::SlotDead.
    if plant != MachinePlant::AllowEscapedSlotAddr {
        check_no_slot_addr_across_suspend(&body, shape, &in_poll, &suspends, scratch)?;
    }

    // 3. Reuse coroutine.rs for SSA splitting, slot packing, unwind poisoning, and LIR output
    //    verification.
    let coro_plant = match plant {
        MachinePlant::None | MachinePlant::AllowEscapedSlotAddr => Plant::None,
        MachinePlant::SkipTagStore => Plant::TagAfterReturn,
        MachinePlant::WrongResumeTarget => Plant::ResumeNext,
    };
    let split = crate::coroutine::split_planted_with(
        body,
        shape.to_coroutine_shape(),
        Panics::NONE,
        coro_plant,
        alloc.clone(),
    )?;

    // Reject any dead Op::Suspend whose resume block is unreachable from entry.
    let mut drop_roots = Vec::with_capacity_in(suspends.len() + 1, scratch);
    drop_roots.push(shape.unresumed.0);
    for s in suspends.iter() {
        drop_roots.push(s.cancel);
    }
    let in_drop = reach(&body, &drop_roots, true, scratch);
    for b in 0..body.blocks() {
        if in_poll[b] || in_drop[b] {
            continue;
        }
        let term = body.terminator(Block(b as u32));
        if body.op(term) == Op::Suspend {
            let mut o = body.operands(term);
            let point = o.imm();
            return Err(MachineError::UnreachableResume { point });
        }
    }

    // 4. Extract exact state-to-state transition graph via bounded iterative CFG walks.
    let edges = extract_edges(&body, shape, &suspends, scratch, alloc.clone());
    let graph = MachineGraphSummary {
        points: split.layout.points,
        state_size: split.layout.size,
        state_align: split.layout.align,
        zero_pin_proven: plant != MachinePlant::AllowEscapedSlotAddr,
        edges,
    };

    Ok(MachineSplit {
        step: split.poll,
        on_cancel: split.drop,
        layout: split.layout,
        graph,
    })
}

/// A reachable `Op::Suspend` in the body.
#[derive(Clone, Copy, Debug)]
struct SuspendAt {
    point: u32,
    inst: u32,
    block: u32,
    resume: u32,
    cancel: u32,
}

fn filled<T: Clone, S: Allocator + Copy>(len: usize, item: T, alloc: S) -> Vec<T, S> {
    let mut out = Vec::with_capacity_in(len, alloc);
    out.resize(len, item);
    out
}

/// Visits every successor block of terminator `at`, paired with whether the edge is a suspend's
/// cancel edge.
fn successors(view: &View<'_>, at: usize, mut f: impl FnMut(Block, bool)) {
    let mut o = view.operands(at);
    match view.op(at) {
        Op::Jump => f(o.target().block, false),
        Op::Br => {
            o.value();
            f(o.target().block, false);
            f(o.target().block, false);
        }
        Op::Switch => {
            let scrutinee = o.value();
            f(o.target().block, false);
            for (_, target) in o.cases(scrutinee) {
                f(target.block, false);
            }
        }
        Op::Suspend => {
            o.imm();
            f(o.block(), false);
            f(o.block(), true);
        }
        Op::Invoke | Op::InvokeIndirect => {
            o.word();
            o.sig();
            f(o.block(), false);
            f(o.block(), false);
        }
        Op::AsmBr => {
            o.asm();
            f(o.block(), false);
            for &lbl in o.blocks() {
                f(Block(lbl), false);
            }
        }
        _ => {}
    }
}

/// Visits every SSA value operand referenced by instruction `at`.
fn each_value(view: &View<'_>, at: usize, mut f: impl FnMut(u32)) {
    match view.op(at).info().layout {
        Layout::None => {}
        Layout::Data(field) => {
            if field == Field::Value {
                f(view.data[at]);
            }
        }
        Layout::Extra(fields) => {
            let words = &view.extra[view.data[at] as usize..];
            let mut visit = |word: Word| -> Result<(), ()> {
                if let Word::Value(value) = word {
                    f(value);
                }
                Ok(())
            };
            let walked = walk(words, fields, Ty(view.tys[at]), view.tys, &mut visit);
            debug_assert!(walked.is_ok(), "operands of a verified function walk");
        }
    }
}

/// Iterative reachability over `view`'s blocks from `roots`. Bounded to at most `view.blocks()`
/// worklist pops.
fn reach<S: Allocator + Copy>(view: &View<'_>, roots: &[u32], cancel_edges: bool, alloc: S) -> Vec<bool, S> {
    let blocks = view.blocks();
    let mut seen = filled(blocks, false, alloc);
    let mut work = Vec::with_capacity_in(blocks, alloc);
    for &root in roots {
        let idx = root as usize;
        if idx < blocks && !seen[idx] {
            seen[idx] = true;
            work.push(root);
        }
    }
    while let Some(b) = work.pop() {
        successors(view, view.terminator(Block(b)), |to, cancel| {
            let idx = to.index();
            if (cancel_edges || !cancel) && idx < blocks && !seen[idx] {
                seen[idx] = true;
                work.push(to.0);
            }
        });
    }
    seen
}

/// Collects all `Op::Suspend` terminators in `in_poll`, sorted deterministically by `(point, inst)`.
fn collect_suspends<S: Allocator + Copy>(
    view: &View<'_>,
    in_poll: &[bool],
    alloc: S,
) -> Vec<SuspendAt, S> {
    let mut suspends = Vec::new_in(alloc);
    for (b, &reachable) in in_poll.iter().enumerate() {
        if !reachable {
            continue;
        }
        let term = view.terminator(Block(b as u32));
        if view.op(term) == Op::Suspend {
            let mut o = view.operands(term);
            let point = o.imm();
            let resume = o.block().0;
            let cancel = o.block().0;
            suspends.push(SuspendAt { point, inst: term as u32, block: b as u32, resume, cancel });
        }
    }
    suspends.sort_unstable_by_key(|s| (s.point, s.inst));
    suspends
}

/// Enforces the zero-`Pin` invariant on `view`:
///
/// 1. Exact forward may-dataflow over `in_poll` blocks tracking `Op::SlotAddr` (gen) and
///    `Op::SlotDead` (kill): if any slot bit is set at the end of a suspending block, its address
///    was taken without an intervening `Op::SlotDead` on some path reaching that `Op::Suspend`.
/// 2. Backward SSA-crossing check for `Op::SlotAddr` values: rejects any `Op::SlotAddr` value
///    whose SSA result is referenced in a block past a `Suspend` (even if `Op::SlotDead` was
///    emitted beforehand).
fn check_no_slot_addr_across_suspend<S: Allocator + Copy>(
    view: &View<'_>,
    shape: MachineShape,
    in_poll: &[bool],
    suspends: &[SuspendAt],
    alloc: S,
) -> Result<(), MachineError> {
    let slots = view.slots.len();
    if suspends.is_empty() || slots == 0 {
        return Ok(());
    }
    let blocks = view.blocks();
    let words = slots.div_ceil(64);

    // Per-block transfer functions: last SlotAddr/SlotDead per slot in the block.
    let mut gen_bits = filled(blocks * words, 0u64, alloc);
    let mut kill_bits = filled(blocks * words, 0u64, alloc);
    let mut at_start = filled(blocks * words, 0u64, alloc);
    let mut at_end = filled(blocks * words, 0u64, alloc);

    for (b, &reachable) in in_poll.iter().enumerate() {
        if !reachable {
            continue;
        }
        for at in view.block(Block(b as u32)) {
            let op = view.op(at);
            if op != Op::SlotAddr && op != Op::SlotDead {
                continue;
            }
            let slot = view.data[at] as usize;
            let word = b * words + slot / 64;
            let bit = 1u64 << (slot % 64);
            if op == Op::SlotDead {
                gen_bits[word] &= !bit;
                kill_bits[word] |= bit;
            } else {
                gen_bits[word] |= bit;
                kill_bits[word] &= !bit;
            }
        }
    }

    // Least fixed point via bounded iterative worklist (monotone bitset union).
    let mut queued = filled(blocks, false, alloc);
    let mut work = Vec::with_capacity_in(blocks, alloc);
    for b in (0..blocks).rev() {
        if in_poll[b] {
            queued[b] = true;
            work.push(b as u32);
        }
    }
    while let Some(b) = work.pop() {
        let b_idx = b as usize;
        queued[b_idx] = false;
        let base = b_idx * words;
        for w in 0..words {
            at_end[base + w] = gen_bits[base + w] | (at_start[base + w] & !kill_bits[base + w]);
        }
        successors(view, view.terminator(Block(b)), |to, cancel| {
            if cancel {
                return;
            }
            let to_idx = to.index();
            if !in_poll[to_idx] {
                return;
            }
            let to_base = to_idx * words;
            let mut grew = false;
            for w in 0..words {
                let out = at_end[base + w];
                let dst = &mut at_start[to_base + w];
                grew |= (out & !*dst) != 0;
                *dst |= out;
            }
            if grew && !queued[to_idx] {
                queued[to_idx] = true;
                work.push(to.0);
            }
        });
    }

    // Check every reachable suspend in deterministic `(point, inst)` order.
    for s in suspends.iter() {
        let base = s.block as usize * words;
        for w in 0..words {
            let live = at_end[base + w];
            if live != 0 {
                let slot = w * 64 + live.trailing_zeros() as usize;
                return Err(MachineError::LocalSlotAddrAcrossSuspend {
                    slot: SlotId(slot as u32),
                    point: s.point,
                    inst: s.inst,
                });
            }
        }
    }

    // Also reject any Op::SlotAddr SSA value directly used across a suspend boundary (e.g. a
    // stale pointer value kept across a suspend even after Op::SlotDead).
    check_no_slot_addr_ssa_crossing(view, shape, in_poll, suspends, alloc)
}

/// Checks whether any `Op::SlotAddr` SSA value crosses into a suspend's `resume` or `cancel`
/// region, using a bounded iterative backward walk over CSR predecessors.
fn check_no_slot_addr_ssa_crossing<S: Allocator + Copy>(
    view: &View<'_>,
    shape: MachineShape,
    in_poll: &[bool],
    suspends: &[SuspendAt],
    alloc: S,
) -> Result<(), MachineError> {
    let blocks = view.blocks();
    let mut drop_roots = Vec::with_capacity_in(suspends.len() + 1, alloc);
    if shape.unresumed.index() < blocks {
        drop_roots.push(shape.unresumed.0);
    }
    for s in suspends.iter() {
        drop_roots.push(s.cancel);
    }
    let in_drop = reach(view, &drop_roots, true, alloc);

    let mut block_of = filled(view.len(), NONE, alloc);
    let mut has_slot_addr = false;
    for b in 0..blocks {
        if !(in_poll[b] || in_drop[b]) {
            continue;
        }
        for at in view.block(Block(b as u32)) {
            block_of[at] = b as u32;
            if in_poll[b] && view.op(at) == Op::SlotAddr {
                has_slot_addr = true;
            }
        }
    }
    if !has_slot_addr {
        return Ok(());
    }

    // Mark every suspend's resume and cancel entry blocks with their index in `suspends`.
    let mut after = filled(blocks, NONE, alloc);
    for (idx, s) in suspends.iter().enumerate() {
        let idx = idx as u32;
        if after[s.resume as usize] == NONE {
            after[s.resume as usize] = idx;
        }
        if after[s.cancel as usize] == NONE {
            after[s.cancel as usize] = idx;
        }
    }

    // Build CSR predecessor table over held blocks.
    let mut starts = filled(blocks + 1, 0u32, alloc);
    for b in 0..blocks {
        if !(in_poll[b] || in_drop[b]) {
            continue;
        }
        successors(view, view.terminator(Block(b as u32)), |to, _| {
            starts[to.index() + 1] += 1;
        });
    }
    for b in 0..blocks {
        starts[b + 1] += starts[b];
    }
    let mut preds = filled(starts[blocks] as usize, 0u32, alloc);
    let mut cursor = filled(blocks, 0u32, alloc);
    cursor.copy_from_slice(&starts[..blocks]);
    for b in 0..blocks {
        if !(in_poll[b] || in_drop[b]) {
            continue;
        }
        successors(view, view.terminator(Block(b as u32)), |to, _| {
            let idx = to.index();
            preds[cursor[idx] as usize] = b as u32;
            cursor[idx] += 1;
        });
    }

    // Collect cross-block uses of `Op::SlotAddr` values.
    let mut uses: Vec<(u32, u32), S> = Vec::new_in(alloc);
    for b in 0..blocks {
        if !(in_poll[b] || in_drop[b]) {
            continue;
        }
        for at in view.block(Block(b as u32)) {
            each_value(view, at, |val| {
                let v_idx = val as usize;
                if view.op(v_idx) == Op::SlotAddr && block_of[v_idx] != b as u32 {
                    uses.push((val, b as u32));
                }
            });
        }
    }
    if uses.is_empty() {
        return Ok(());
    }
    uses.sort_unstable();
    uses.dedup();

    let mut passed = filled(blocks, NONE, alloc);
    let mut work = Vec::with_capacity_in(blocks, alloc);
    let mut i = 0;
    while i < uses.len() {
        let val = uses[i].0;
        let made_in = block_of[val as usize];
        work.clear();
        while i < uses.len() && uses[i].0 == val {
            let use_b = uses[i].1;
            i += 1;
            if passed[use_b as usize] != val {
                passed[use_b as usize] = val;
                work.push(use_b);
            }
        }
        while let Some(b) = work.pop() {
            let susp_idx = after[b as usize];
            if susp_idx != NONE {
                let s = suspends[susp_idx as usize];
                return Err(MachineError::LocalSlotAddrAcrossSuspend {
                    slot: SlotId(view.data[val as usize]),
                    point: s.point,
                    inst: s.inst,
                });
            }
            let p_start = starts[b as usize] as usize;
            let p_end = starts[b as usize + 1] as usize;
            for &pred in &preds[p_start..p_end] {
                if pred != made_in && passed[pred as usize] != val {
                    passed[pred as usize] = val;
                    work.push(pred);
                }
            }
        }
    }

    Ok(())
}

/// Extracts all reachable state-to-state edges (`Suspend`, `Return`, `Cancel`) from entry (`None`)
/// and every suspend point (`Some(k)`) using a bounded iterative worklist (zero recursion).
fn extract_edges<S: Allocator + Copy, A: Allocator>(
    view: &View<'_>,
    shape: MachineShape,
    suspends: &[SuspendAt],
    scratch: S,
    out_alloc: A,
) -> Vec<MachineEdge, A> {
    let blocks = view.blocks();
    let mut seen = filled(blocks, false, scratch);
    let mut work = Vec::with_capacity_in(blocks, scratch);
    let mut edges = Vec::new_in(out_alloc);

    // Entry (`None`) step path and unresumed cancel path.
    walk_step_edges(view, 0, None, &mut seen, &mut work, &mut edges);
    walk_cancel_edges(view, shape.unresumed.0, None, &mut seen, &mut work, &mut edges);

    // Each suspend point's resume path and cancel path.
    for s in suspends.iter() {
        walk_step_edges(view, s.resume, Some(s.point), &mut seen, &mut work, &mut edges);
        walk_cancel_edges(view, s.cancel, Some(s.point), &mut seen, &mut work, &mut edges);
    }

    edges.sort_unstable();
    edges.dedup();
    edges
}

fn walk_step_edges<S: Allocator, A: Allocator>(
    view: &View<'_>,
    start_block: u32,
    from_point: Option<u32>,
    seen: &mut [bool],
    work: &mut Vec<u32, S>,
    edges: &mut Vec<MachineEdge, A>,
) {
    seen.fill(false);
    work.clear();
    let start_idx = start_block as usize;
    if start_idx >= view.blocks() {
        return;
    }
    seen[start_idx] = true;
    work.push(start_block);

    while let Some(b) = work.pop() {
        let term = view.terminator(Block(b));
        match view.op(term) {
            Op::Suspend => {
                let mut o = view.operands(term);
                let to_point = o.imm();
                edges.push(MachineEdge {
                    from_point,
                    to_point: Some(to_point),
                    kind: MachineEdgeKind::Suspend,
                });
            }
            Op::Return => {
                edges.push(MachineEdge {
                    from_point,
                    to_point: None,
                    kind: MachineEdgeKind::Return,
                });
            }
            _ => {
                successors(view, term, |to, cancel| {
                    let idx = to.index();
                    if !cancel && idx < seen.len() && !seen[idx] {
                        seen[idx] = true;
                        work.push(to.0);
                    }
                });
            }
        }
    }
}

fn walk_cancel_edges<S: Allocator, A: Allocator>(
    view: &View<'_>,
    start_block: u32,
    from_point: Option<u32>,
    seen: &mut [bool],
    work: &mut Vec<u32, S>,
    edges: &mut Vec<MachineEdge, A>,
) {
    seen.fill(false);
    work.clear();
    let start_idx = start_block as usize;
    if start_idx >= view.blocks() {
        return;
    }
    seen[start_idx] = true;
    work.push(start_block);

    while let Some(b) = work.pop() {
        let term = view.terminator(Block(b));
        match view.op(term) {
            Op::Cancelled => {
                edges.push(MachineEdge {
                    from_point,
                    to_point: None,
                    kind: MachineEdgeKind::Cancel,
                });
            }
            _ => {
                successors(view, term, |to, _| {
                    let idx = to.index();
                    if idx < seen.len() && !seen[idx] {
                        seen[idx] = true;
                        work.push(to.0);
                    }
                });
            }
        }
    }
}

#[cfg(test)]
#[path = "coroutine_machine_tests.rs"]
mod tests;
