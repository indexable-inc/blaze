//! The composer's analysis pass (docs/v2/plan/design/backend-next.md D97 point 2, D98, and D100
//! point 5, "homes and eviction"): one function's emission order, loops, liveness and the HOME of
//! every value, before the emission pass (compose/emit.rs) copies a single template.
//!
//! What it decides, and the emission pass takes as given:
//!
//! - `order`: the blocks the emission pass lays out, in reverse postorder of a depth-first walk
//!   from the entry along each terminator's targets in operand order. Unreachable blocks are not
//!   in it (no code is made for them). Every POSITION below is an index into the instructions of
//!   `order`'s blocks laid end to end.
//! - `home`, per instruction: a value read in a block other than its own, or a block parameter
//!   written by an edge (any parameter of a block something branches to, once it has a use), is
//!   live across a block boundary and gets its home here: a register for its WHOLE interval or its
//!   cell. Every other instruction is [`Home::Local`]: defined and last used in one block (or with
//!   no result, or never used, or unreachable): the emission pass gives it any free register and
//!   may evict it to its cell, since no other path reaches that code (D100 point 5).
//! - An interval is `[start, end]` in positions: from the definition (for a parameter, from the
//!   earliest terminator of a predecessor, which writes it) to the last use (for a parameter, at
//!   least to the last predecessor's terminator), extended to the last instruction of every loop
//!   whose header the value is live into. Registers are given header parameters first, deepest
//!   loop first, then by `start`, each the lowest free register of its class in
//!   `regs.allocatable` less the registers reserve.rs keeps for one instruction's operands and
//!   result (`reserve::kept`: never a home, so a block-local value always finds a register or
//!   a local to evict) (free: no interval given it overlaps). None free: its cell.
//! - `last_use`, per instruction: the instruction at the END of its interval for a value with a
//!   home (its register is held through it), the last use in its block for a `Local` value, its
//!   own index for one with no use. `next_use`, per instruction: its first use after its
//!   definition (by position), or [`NO_USE`].
//! - `back_edges`: every retreating edge `(from, header)` of the walk (in this order, an edge
//!   to a block at or before its source), sorted by the header's position, then the source's.
//!   Each back edge gets the poll. `loops`: one [`LoopHeader`] per distinct header, in the same
//!   order (so sorted by `at` once the emission pass fills it; [`Analysis::loop_index`] finds a
//!   header's), with `at` left 0. `live`: each header's rows, sorted by value: every value live
//!   into it (a header parameter with a use included) at the place it has at the header, and
//!   every stack slot whose address is taken in a block that reaches the header by one or more
//!   edges, as [`Place::Slot`] keyed after every instruction (the function's instruction count
//!   plus its slot index: one key space, so each header lists each key once; loop_map.rs). A
//!   [`Place::Cell`] or [`Place::Slot`] row's `sp_offset` is left 0: the emission pass lays out
//!   the frame.
//! - `saved`: the callee-saved registers a home holds. THE EMISSION PASS'S CONTRACT: a `Local`
//!   value takes a register outside `regs.callee_saved` or inside `saved` (the prologue saves
//!   only `saved`); a home in a caller-saved register live across a call is saved in its cell
//!   around the call by the emission pass; an Fpr home holding more than 64 bits is treated as
//!   caller-saved (a call keeps the low 64 bits only).
//! - A value's class: integers up to 64 bits and pointers are `Gpr`; `f16`, `f32`, `f64` and
//!   fixed vectors of at most 128 bits are `Fpr`; `i128`, `f128`, wider or scalable vectors and
//!   compounds have no register class, so a cross-block one is a `Cell`.
//! - Irreducible control flow (a retreating edge whose header does not dominate its source):
//!   every cross-block value is a `Cell`, `irreducible` is set, and the loop rows say `Cell`.
//!
//! Work lists throughout, no recursion. Every list is in the caller's allocator. The same view
//! gives the same analysis byte for byte: every order here is the view's own or a sort on keys.

use core::array;

use blaze_lir::op::{WalkError, Word, walk};
use blaze_lir::ty::Form;
use blaze_lir::{Block, Field, Layout, Op, Scalar, Ty, Value, View};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::{Live, LoopHeader, Place, Reg, RegClass, RegDesc, RegSet, TierError, TierName};

use super::loop_map::LoopMap;
use super::reserve;

/// "No use": a [`Analysis::next_use`] entry of a value nothing reads.
pub(crate) const NO_USE: u32 = u32::MAX;

/// "Not reached" / "none" in the pass's own per-block and per-instruction tables.
const NONE: u32 = u32::MAX;

/// Where a value lives (D100 point 5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Home {
    /// Its 16-byte cell, for its whole life.
    Cell,
    /// This register for its whole interval.
    Reg(Reg),
    /// Defined and last used in one block: the emission pass's to allocate.
    Local,
}

/// One function's analysis, in the caller's allocator. See the module's documentation for what
/// each field holds.
pub(crate) struct Analysis<'a> {
    pub(crate) order: AVec<'a, Block>,
    pub(crate) home: AVec<'a, Home>,
    pub(crate) last_use: AVec<'a, u32>,
    pub(crate) next_use: AVec<'a, u32>,
    pub(crate) loops: AVec<'a, LoopHeader>,
    pub(crate) live: AVec<'a, Live>,
    pub(crate) back_edges: AVec<'a, (Block, Block)>,
    pub(crate) saved: RegSet,
    pub(crate) irreducible: bool,
}

impl Analysis<'_> {
    /// The index into `loops` of the loop whose header is `header`, or `None` when no back edge
    /// enters it.
    pub(crate) fn loop_index(&self, header: Block) -> Option<u32> {
        let mut index = 0u32;
        let mut previous: Option<Block> = None;
        for &(_, to) in self.back_edges.iter() {
            if previous.is_some_and(|it| it != to) {
                index += 1;
            }
            if to == header {
                return Some(index);
            }
            previous = Some(to);
        }
        None
    }
}

/// A planted bug for the analysis test's control (rule dst-not-review).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// A value live into a loop header keeps the interval its uses give, not extended to the
    /// loop's last instruction: a later value can take its register inside the loop.
    NoLoopExtension,
}

/// The analysis of `view` for the registers `regs` describes, its lists in `alloc`.
///
/// Refuses, naming the instruction where there is one: a function past `u32` instructions, one
/// with no blocks, an empty block, a block whose last instruction is not a terminator, a branch
/// to a block the function lacks, a use of a value it lacks or of an instruction with no result,
/// a reached use of a value defined in unreached code, a use before its definition in one block,
/// and operands that do not fit their layout.
pub(crate) fn analyse<'a>(view: View<'_>, regs: &RegDesc, alloc: Alloc<'a>) -> Result<Analysis<'a>, TierError> {
    analyse_planted(view, regs, alloc, Plant::None)
}

/// [`analyse`] with `plant` planted.
pub(crate) fn analyse_planted<'a>(
    view: View<'_>,
    regs: &RegDesc,
    alloc: Alloc<'a>,
    plant: Plant,
) -> Result<Analysis<'a>, TierError> {
    let n = view.len();
    if n >= NONE as usize || view.blocks() >= NONE as usize {
        return Err(TierError::TooLarge { tier: TierName::Fast, what: "the function's instruction count" });
    }
    if view.blocks() == 0 {
        return Err(refused(None, "a function with no blocks"));
    }
    let graph = graph(view, alloc)?;
    let walked = walk_blocks(view.blocks(), &graph, alloc);
    let preds = preds(view.blocks(), &graph, &walked, alloc);
    let idom = dominators(&walked, &preds, alloc)?;
    let irreducible = walked
        .retreating
        .iter()
        .any(|&(from, to)| !dominates(&idom, walked.rpo[to as usize], walked.rpo[from as usize]));
    let headers = headers(view.blocks(), &walked, alloc);
    let positions = positions(view, &walked, alloc);
    let bodies = match irreducible {
        true => None,
        false => Some(loop_bodies(view, &walked, &preds, &headers, &positions, alloc)),
    };
    let uses = uses(view, &walked, alloc)?;
    let mut lives = liveness(view, &graph, &preds, &headers, &positions, &uses, alloc)?;
    if let (Some(bodies), Plant::None) = (&bodies, plant) {
        for &(header, value) in lives.header_live.iter() {
            let end = &mut lives.end[value as usize];
            *end = (*end).max(bodies.end[header as usize]);
        }
    }
    let (home, saved) = homes(view, regs, &graph, &headers, bodies.as_ref(), &lives, irreducible, alloc);
    let mut last_use = lives.last_use;
    for value in 0..n {
        if matches!(home[value], Home::Reg(_) | Home::Cell) {
            last_use[value] = positions.inst_at[lives.end[value] as usize];
        }
    }
    let (loops, live) = loop_rows(view, &graph, &walked, &headers, &home, &lives.header_live, alloc)?;
    let mut back_edges = Vec::with_capacity_in(walked.retreating.len(), alloc);
    for &(from, to) in walked.retreating.iter() {
        back_edges.push((Block(from), Block(to)));
    }
    let mut order = Vec::with_capacity_in(walked.order.len(), alloc);
    for &block in walked.order.iter() {
        order.push(Block(block));
    }
    Ok(Analysis {
        order,
        home,
        last_use,
        next_use: lives.next_use,
        loops,
        live,
        back_edges,
        saved,
        irreducible,
    })
}

fn refused(inst: Option<u32>, what: &'static str) -> TierError {
    TierError::Refused { tier: TierName::Fast, inst, what }
}

/// Calls `visit` with each operand word of instruction `inst` (values, blocks, the rest), as
/// LIR's own walk reads them.
fn operands(
    view: View<'_>,
    inst: usize,
    visit: &mut impl FnMut(Word) -> Result<(), TierError>,
) -> Result<(), TierError> {
    let at = Some(inst as u32);
    let layout = view.op(inst).info().layout;
    let words: &[u32] = match layout {
        Layout::None => &[],
        Layout::Data(_) => core::slice::from_ref(&view.data[inst]),
        Layout::Extra(_) => view
            .extra
            .get(view.data[inst] as usize..)
            .ok_or(refused(at, "operands past the function's operand column"))?,
    };
    match walk(words, layout.fields(), Ty(view.tys[inst]), view.tys, visit) {
        Ok(_) => Ok(()),
        Err(WalkError::Visit(error)) => Err(error),
        Err(WalkError::Short | WalkError::NoScrutinee) => Err(refused(at, "operands that do not fit their layout")),
    }
}

/// Per instruction its block; per block its successors (`succ[succ_first[b]..succ_first[b + 1]]`,
/// in operand order, repeats kept).
struct Graph<'a> {
    block_of: AVec<'a, u32>,
    succ_first: AVec<'a, u32>,
    succ: AVec<'a, u32>,
}

impl Graph<'_> {
    fn succs(&self, block: u32) -> &[u32] {
        &self.succ[self.succ_first[block as usize] as usize..self.succ_first[block as usize + 1] as usize]
    }
}

fn graph<'a>(view: View<'_>, alloc: Alloc<'a>) -> Result<Graph<'a>, TierError> {
    let blocks = view.blocks();
    let mut block_of = Vec::from_elem_in(NONE, view.len(), alloc);
    let mut succ_first = Vec::with_capacity_in(blocks + 1, alloc);
    let mut succ = Vec::new_in(alloc);
    for block in 0..blocks {
        let range = view.block(Block(block as u32));
        if range.is_empty() {
            return Err(refused(None, "a block with no instructions"));
        }
        for inst in range.clone() {
            block_of[inst] = block as u32;
        }
        succ_first.push(succ.len() as u32);
        let term = range.end - 1;
        if !view.op(term).is_terminator() {
            return Err(refused(Some(term as u32), "a block whose last instruction is not a terminator"));
        }
        operands(view, term, &mut |word| {
            if let Word::Block(to) = word {
                if to as usize >= blocks {
                    return Err(refused(Some(term as u32), "a branch to a block the function does not have"));
                }
                succ.push(to);
            }
            Ok(())
        })?;
    }
    succ_first.push(succ.len() as u32);
    Ok(Graph { block_of, succ_first, succ })
}

/// The depth-first walk from the entry: the reverse postorder, each block's place in it
/// ([`NONE`] for an unreached block), and the retreating edges `(from, to)` sorted by the
/// header's place, then the source's, without repeats.
struct Walked<'a> {
    order: AVec<'a, u32>,
    rpo: AVec<'a, u32>,
    retreating: AVec<'a, (u32, u32)>,
}

fn walk_blocks<'a>(blocks: usize, graph: &Graph<'_>, alloc: Alloc<'a>) -> Walked<'a> {
    const UNSEEN: u8 = 0;
    const ON_STACK: u8 = 1;
    let mut state = Vec::from_elem_in(UNSEEN, blocks, alloc);
    let mut post = Vec::with_capacity_in(blocks, alloc);
    let mut retreating = Vec::new_in(alloc);
    // (block, the next of its successors' places in `succ`).
    let mut stack: AVec<'_, (u32, u32)> = Vec::new_in(alloc);
    state[0] = ON_STACK;
    stack.push((0, graph.succ_first[0]));
    while let Some(&(block, next)) = stack.last() {
        if next < graph.succ_first[block as usize + 1] {
            if let Some(top) = stack.last_mut() {
                top.1 = next + 1;
            }
            let to = graph.succ[next as usize];
            match state[to as usize] {
                UNSEEN => {
                    state[to as usize] = ON_STACK;
                    stack.push((to, graph.succ_first[to as usize]));
                }
                ON_STACK => retreating.push((block, to)),
                _ => {}
            }
        } else {
            state[block as usize] = ON_STACK + 1;
            post.push(block);
            stack.pop();
        }
    }
    let mut order = Vec::with_capacity_in(post.len(), alloc);
    let mut rpo = Vec::from_elem_in(NONE, blocks, alloc);
    for &block in post.iter().rev() {
        rpo[block as usize] = order.len() as u32;
        order.push(block);
    }
    retreating.sort_unstable_by_key(|&(from, to)| (rpo[to as usize], rpo[from as usize]));
    retreating.dedup();
    Walked { order, rpo, retreating }
}

/// Per reached block its reached predecessors: `pred[pred_first[b]..pred_first[b + 1]]`.
struct Preds<'a> {
    pred_first: AVec<'a, u32>,
    pred: AVec<'a, u32>,
}

impl Preds<'_> {
    fn of(&self, block: u32) -> &[u32] {
        &self.pred[self.pred_first[block as usize] as usize..self.pred_first[block as usize + 1] as usize]
    }
}

fn preds<'a>(blocks: usize, graph: &Graph<'_>, walked: &Walked<'_>, alloc: Alloc<'a>) -> Preds<'a> {
    let mut pred_first = Vec::from_elem_in(0u32, blocks + 1, alloc);
    for &block in walked.order.iter() {
        for &to in graph.succs(block) {
            pred_first[to as usize + 1] += 1;
        }
    }
    for block in 0..blocks {
        pred_first[block + 1] += pred_first[block];
    }
    let mut fill = Vec::from_slice_in(&pred_first[..blocks], alloc);
    let mut pred = Vec::from_elem_in(0u32, pred_first[blocks] as usize, alloc);
    for &block in walked.order.iter() {
        for &to in graph.succs(block) {
            pred[fill[to as usize] as usize] = block;
            fill[to as usize] += 1;
        }
    }
    Preds { pred_first, pred }
}

/// Immediate dominators by place in the reverse postorder (Cooper, Harvey and Kennedy's
/// iteration); the entry is its own.
fn dominators<'a>(walked: &Walked<'_>, preds: &Preds<'_>, alloc: Alloc<'a>) -> Result<AVec<'a, u32>, TierError> {
    let reached = walked.order.len();
    let mut idom = Vec::from_elem_in(NONE, reached, alloc);
    idom[0] = 0;
    // Each round settles at least one more place; a reducible graph needs two or three.
    for _round in 0..reached + 2 {
        let mut changed = false;
        for place in 1..reached {
            let mut new = NONE;
            for &pred in preds.of(walked.order[place]) {
                let at = walked.rpo[pred as usize];
                if idom[at as usize] == NONE {
                    continue;
                }
                new = match new {
                    NONE => at,
                    _ => intersect(&idom, at, new),
                };
            }
            if idom[place] != new {
                idom[place] = new;
                changed = true;
            }
        }
        if !changed {
            return Ok(idom);
        }
    }
    Err(refused(None, "dominators that do not settle"))
}

fn intersect(idom: &[u32], mut a: u32, mut b: u32) -> u32 {
    while a != b {
        while a > b {
            a = idom[a as usize];
        }
        while b > a {
            b = idom[b as usize];
        }
    }
    a
}

/// Whether the block at place `a` dominates the block at place `b`.
fn dominates(idom: &[u32], a: u32, mut b: u32) -> bool {
    loop {
        if a == b {
            return true;
        }
        if b == 0 {
            return false;
        }
        b = idom[b as usize];
    }
}

/// The loop headers: the distinct targets of retreating edges in the walk's order, and per
/// block its loop's index ([`NONE`] for a block no back edge enters).
struct Headers<'a> {
    blocks: AVec<'a, u32>,
    loop_of: AVec<'a, u32>,
}

fn headers<'a>(blocks: usize, walked: &Walked<'_>, alloc: Alloc<'a>) -> Headers<'a> {
    let mut list = Vec::new_in(alloc);
    let mut loop_of = Vec::from_elem_in(NONE, blocks, alloc);
    for &(_, to) in walked.retreating.iter() {
        if loop_of[to as usize] == NONE {
            loop_of[to as usize] = list.len() as u32;
            list.push(to);
        }
    }
    Headers { blocks: list, loop_of }
}

/// Positions: per instruction its place in the laid-out order ([`NONE`] when unreached), and
/// per position its instruction.
struct Positions<'a> {
    pos: AVec<'a, u32>,
    inst_at: AVec<'a, u32>,
}

fn positions<'a>(view: View<'_>, walked: &Walked<'_>, alloc: Alloc<'a>) -> Positions<'a> {
    let mut pos = Vec::from_elem_in(NONE, view.len(), alloc);
    let mut inst_at = Vec::with_capacity_in(view.len(), alloc);
    for &block in walked.order.iter() {
        for inst in view.block(Block(block)) {
            pos[inst] = inst_at.len() as u32;
            inst_at.push(inst as u32);
        }
    }
    Positions { pos, inst_at }
}

/// Per loop (reducible functions only): the position of its body's last instruction, and its
/// header's depth (how many loops' bodies hold the header, its own included).
struct Bodies<'a> {
    end: AVec<'a, u32>,
    depth: AVec<'a, u32>,
}

fn loop_bodies<'a>(
    view: View<'_>,
    walked: &Walked<'_>,
    preds: &Preds<'_>,
    headers: &Headers<'_>,
    positions: &Positions<'_>,
    alloc: Alloc<'a>,
) -> Bodies<'a> {
    let count = headers.blocks.len();
    let mut end = Vec::from_elem_in(0u32, count, alloc);
    let mut depth = Vec::from_elem_in(0u32, count, alloc);
    let mut mark = Vec::from_elem_in(NONE, view.blocks(), alloc);
    let mut work: AVec<'_, u32> = Vec::new_in(alloc);
    let last = |block: u32| positions.pos[view.terminator(Block(block))];
    for (index, &header) in headers.blocks.iter().enumerate() {
        let id = index as u32;
        mark[header as usize] = id;
        end[index] = last(header);
        depth[index] += 1;
        for &(from, to) in walked.retreating.iter() {
            if to == header && mark[from as usize] != id {
                mark[from as usize] = id;
                work.push(from);
            }
        }
        // The natural loop: every block that reaches a latch without passing the header (in a
        // reducible function the header dominates them all, so the walk stays inside).
        while let Some(block) = work.pop() {
            end[index] = end[index].max(last(block));
            let inner = headers.loop_of[block as usize];
            if inner != NONE {
                depth[inner as usize] += 1;
            }
            for &pred in preds.of(block) {
                if mark[pred as usize] != id {
                    mark[pred as usize] = id;
                    work.push(pred);
                }
            }
        }
    }
    Bodies { end, depth }
}

/// Per value its uses' instructions: `uses[first[v]..first[v + 1]]`, reached uses only.
struct Uses<'a> {
    first: AVec<'a, u32>,
    uses: AVec<'a, u32>,
}

impl Uses<'_> {
    fn of(&self, value: usize) -> &[u32] {
        &self.uses[self.first[value] as usize..self.first[value + 1] as usize]
    }
}

fn uses<'a>(view: View<'_>, walked: &Walked<'_>, alloc: Alloc<'a>) -> Result<Uses<'a>, TierError> {
    let n = view.len();
    let mut first = Vec::from_elem_in(0u32, n + 1, alloc);
    for &block in walked.order.iter() {
        for inst in view.block(Block(block)) {
            operands(view, inst, &mut |word| {
                if let Word::Value(value) = word {
                    if value as usize >= n {
                        return Err(refused(Some(inst as u32), "a use of a value the function does not have"));
                    }
                    if view.ty(Value(value)).is_none() {
                        return Err(refused(Some(inst as u32), "a use of an instruction that makes no value"));
                    }
                    first[value as usize + 1] += 1;
                }
                Ok(())
            })?;
        }
    }
    for value in 0..n {
        first[value + 1] += first[value];
    }
    let mut fill = Vec::from_slice_in(&first[..n], alloc);
    let mut uses = Vec::from_elem_in(0u32, first[n] as usize, alloc);
    for &block in walked.order.iter() {
        for inst in view.block(Block(block)) {
            operands(view, inst, &mut |word| {
                if let Word::Value(value) = word {
                    uses[fill[value as usize] as usize] = inst as u32;
                    fill[value as usize] += 1;
                }
                Ok(())
            })?;
        }
    }
    Ok(Uses { first, uses })
}

/// What liveness gives: per value whether it lives across a block boundary, its interval
/// `[start, end]` in positions, its last use and first use (instructions), and the pairs
/// `(loop, value)` of values live into each header (header parameters with a use included).
struct Lives<'a> {
    cross: AVec<'a, bool>,
    start: AVec<'a, u32>,
    end: AVec<'a, u32>,
    last_use: AVec<'a, u32>,
    next_use: AVec<'a, u32>,
    header_live: AVec<'a, (u32, u32)>,
}

/// SSA liveness by walking back from each use to the definition (each block entered once per
/// value), so the work is the size of the live sets, not blocks times values.
fn liveness<'a>(
    view: View<'_>,
    graph: &Graph<'_>,
    preds: &Preds<'_>,
    headers: &Headers<'_>,
    positions: &Positions<'_>,
    uses: &Uses<'_>,
    alloc: Alloc<'a>,
) -> Result<Lives<'a>, TierError> {
    let n = view.len();
    let mut cross = Vec::from_elem_in(false, n, alloc);
    let mut start = Vec::from_elem_in(0u32, n, alloc);
    let mut end = Vec::from_elem_in(0u32, n, alloc);
    let mut last_use = Vec::with_capacity_in(n, alloc);
    let mut next_use = Vec::from_elem_in(NO_USE, n, alloc);
    let mut header_live = Vec::new_in(alloc);
    let mut stamp = Vec::from_elem_in(NONE, view.blocks(), alloc);
    let mut work: AVec<'_, u32> = Vec::new_in(alloc);
    for value in 0..n {
        last_use.push(value as u32);
        let def_pos = positions.pos[value];
        let def_block = graph.block_of[value];
        if def_pos == NONE {
            if let Some(&inst) = uses.of(value).first() {
                return Err(refused(Some(inst), "a reached use of a value defined in unreached code"));
            }
            continue;
        }
        let tag = value as u32;
        let (mut first_pos, mut last_pos) = (NONE, def_pos);
        for &inst in uses.of(value) {
            let at = positions.pos[inst as usize];
            if at < first_pos {
                first_pos = at;
                next_use[value] = inst;
            }
            if at > last_pos {
                last_pos = at;
                last_use[value] = inst;
            }
            let block = graph.block_of[inst as usize];
            if block == def_block {
                if inst as usize <= value {
                    return Err(refused(Some(inst), "a use before its definition in one block"));
                }
                continue;
            }
            if stamp[block as usize] != tag {
                stamp[block as usize] = tag;
                work.push(block);
            }
        }
        while let Some(block) = work.pop() {
            cross[value] = true;
            let inner = headers.loop_of[block as usize];
            if inner != NONE {
                header_live.push((inner, tag));
            }
            for &pred in preds.of(block) {
                if pred != def_block && stamp[pred as usize] != tag {
                    stamp[pred as usize] = tag;
                    work.push(pred);
                }
            }
        }
        let (mut from, mut to) = (def_pos, last_pos);
        // A parameter is written by each edge into its block: at the predecessor's terminator.
        let edge_written = view.op(value) == Op::Param && !preds.of(def_block).is_empty();
        if edge_written && !uses.of(value).is_empty() {
            cross[value] = true;
            for &pred in preds.of(def_block) {
                let at = positions.pos[view.terminator(Block(pred))];
                from = from.min(at);
                to = to.max(at);
            }
            let inner = headers.loop_of[def_block as usize];
            if inner != NONE {
                header_live.push((inner, tag));
            }
        }
        start[value] = from;
        end[value] = to;
    }
    Ok(Lives { cross, start, end, last_use, next_use, header_live })
}

/// A value's register class, or `None` where no register holds it whole.
fn class_of(ty: Ty, ptr_bits: u32) -> Option<RegClass> {
    match ty.form() {
        Form::Scalar => match ty.elem() {
            Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32 | Scalar::I64 | Scalar::Ptr => Some(RegClass::Gpr),
            Scalar::F16 | Scalar::F32 | Scalar::F64 => Some(RegClass::Fpr),
            Scalar::I128 | Scalar::F128 | Scalar::None => None,
        },
        Form::Vector if ty.bits(ptr_bits) <= 128 => Some(RegClass::Fpr),
        Form::Vector | Form::Scalable | Form::Compound => None,
    }
}

/// Every value's home, and the callee-saved registers the homes hold.
#[expect(clippy::too_many_arguments, reason = "one call site; each input is a distinct table of the pass")]
fn homes<'a>(
    view: View<'_>,
    regs: &RegDesc,
    graph: &Graph<'_>,
    headers: &Headers<'_>,
    bodies: Option<&Bodies<'_>>,
    lives: &Lives<'_>,
    irreducible: bool,
    alloc: Alloc<'a>,
) -> (AVec<'a, Home>, RegSet) {
    let n = view.len();
    let mut home = Vec::from_elem_in(Home::Local, n, alloc);
    // (group: 0 for a header parameter, 1 for the rest; the deepest loop first; start; value).
    let mut wanted: AVec<'_, (u32, u32, u32, u32)> = Vec::new_in(alloc);
    for value in 0..n {
        if !lives.cross[value] {
            continue;
        }
        if irreducible || class_of(view.ty(Value(value as u32)), view.head.ptr_bits).is_none() {
            home[value] = Home::Cell;
            continue;
        }
        let inner = headers.loop_of[graph.block_of[value] as usize];
        let key = match (view.op(value) == Op::Param, inner, bodies) {
            (true, inner, Some(bodies)) if inner != NONE => (0, u32::MAX - bodies.depth[inner as usize], lives.start[value], value as u32),
            _ => (1, 0, lives.start[value], value as u32),
        };
        wanted.push(key);
    }
    wanted.sort_unstable();
    let mut taken: [AVec<'_, (u32, u32)>; 64] = array::from_fn(|_| Vec::new_in(alloc));
    // D100 point 5: some registers of each class are never a home (reserve.rs), so the emission
    // pass can always place one instruction's operands and result beside the homes, evicting a
    // local where it must; a kept callee-saved register (none on arm64) is saved for the locals.
    let homes_may_take = [
        regs.allocatable.of(RegClass::Gpr).minus(reserve::kept(regs, RegClass::Gpr)),
        regs.allocatable.of(RegClass::Fpr).minus(reserve::kept(regs, RegClass::Fpr)),
    ];
    let mut saved = reserve::kept_callee_saved(regs);
    for &(_, _, from, value) in wanted.iter() {
        let to = lives.end[value as usize];
        let Some(class) = class_of(view.ty(Value(value)), view.head.ptr_bits) else { continue };
        let mut free = match class {
            RegClass::Gpr => homes_may_take[0],
            RegClass::Fpr => homes_may_take[1],
        };
        home[value as usize] = Home::Cell;
        while let Some(reg) = free.first() {
            let held = &mut taken[reg.0 as usize];
            if held.iter().all(|&(a, b)| b < from || to < a) {
                held.push((from, to));
                home[value as usize] = Home::Reg(reg);
                if regs.callee_saved.contains(reg) {
                    saved = saved.with(reg);
                }
                break;
            }
            free = free.without(reg);
        }
    }
    (home, saved)
}

/// The loop map: one header row per loop (`at` 0) and its live rows, sorted by key, each key once
/// ([`LoopMap`]: values by index, address-taken slots after every instruction).
fn loop_rows<'a>(
    view: View<'_>,
    graph: &Graph<'_>,
    walked: &Walked<'_>,
    headers: &Headers<'_>,
    home: &[Home],
    header_live: &[(u32, u32)],
    alloc: Alloc<'a>,
) -> Result<(AVec<'a, LoopHeader>, AVec<'a, Live>), TierError> {
    let mut map = LoopMap::new(view.len(), view.slots.len(), headers.blocks.len(), alloc)?;
    for &(inner, value) in header_live {
        let place = match home[value as usize] {
            Home::Reg(reg) => Place::Reg(reg),
            Home::Cell => Place::Cell { sp_offset: 0 },
            Home::Local => return Err(refused(Some(value), "a value live into a loop header without a home")),
        };
        map.value(inner, value, place)?;
    }
    // Slots whose address is taken: live into every header reached from a block taking it.
    let mut taken: AVec<'_, (u32, u32)> = Vec::new_in(alloc);
    for &block in walked.order.iter() {
        for inst in view.block(Block(block)) {
            if view.op(inst) != Op::SlotAddr {
                continue;
            }
            operands(view, inst, &mut |word| {
                if let Word::Of(Field::Slot, slot) = word {
                    taken.push((slot, block));
                }
                Ok(())
            })?;
        }
    }
    taken.sort_unstable();
    taken.dedup();
    let mut stamp = Vec::from_elem_in(NONE, view.blocks(), alloc);
    let mut work: AVec<'_, u32> = Vec::new_in(alloc);
    let mut at = 0;
    while at < taken.len() {
        let slot = taken[at].0;
        while at < taken.len() && taken[at].0 == slot {
            for &to in graph.succs(taken[at].1) {
                if stamp[to as usize] != slot {
                    stamp[to as usize] = slot;
                    work.push(to);
                }
            }
            at += 1;
        }
        while let Some(block) = work.pop() {
            let inner = headers.loop_of[block as usize];
            if inner != NONE {
                map.slot(inner, slot)?;
            }
            for &to in graph.succs(block) {
                if stamp[to as usize] != slot {
                    stamp[to as usize] = slot;
                    work.push(to);
                }
            }
        }
    }
    map.finish(alloc)
}
