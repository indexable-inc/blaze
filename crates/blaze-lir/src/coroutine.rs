//! The coroutine pass (decision D73; docs/v2/plan/design/coroutines.md): one body with
//! `suspend`s in, the state's layout and two ordinary functions out.
//!
//! # In
//!
//! The builder builds an `async` body as an ordinary function. Its entry parameters are what
//! every poll passes (the return pointer if there is one, the pinned state pointer, the task
//! context), its returns are `Poll<T>`'s, and each `.await` ends a block in
//! [`Op::Suspend`]: the point's number, the block the next poll goes on at, the block the
//! state's drop goes on at, and the `Poll::Pending` this poll returns. A cancel path ends in
//! [`Op::Cancelled`]. The upvars are not slots: they are the first [`Shape::prefix_size`] bytes
//! of the state, read through the state pointer as a closure reads its environment, and
//! [`Shape::unresumed`] is the block that drops them (the cancel path of a state never
//! polled).
//!
//! # What is kept in the state
//!
//! - **A stack slot** is kept when, at some `suspend`, its address may have been taken and its
//!   storage not ended: a forward may-analysis whose facts are made by `slot_addr` and
//!   `slot_live` and ended by `slot_dead`. It is sound because a slot's bytes are reached only
//!   through an address made by `slot_addr`: if no such address was made since the slot's last
//!   `slot_dead` on any path to a suspend, nothing made before the suspend can read or write
//!   the slot after it without touching dead storage, and an address made after it is of the
//!   new poll's frame, whose bytes that poll writes before it reads. It is coarse (an address
//!   taken once keeps the slot to its `slot_dead`, and the builder writes none today); exact
//!   liveness of the bytes can only keep fewer.
//! - **A kept slot's home never changes** (the design's D3): every `slot_addr` of it is the
//!   state pointer plus the slot's offset, from its first write, so a pointer into it made
//!   before a suspend is good after it. That needs the state not to move once polled: what
//!   `Pin<&mut State>` promises.
//! - **A value** is kept when it is live into the resume or the cancel block of some
//!   `suspend`. It is stored to its field once, where it is made, and loaded where it is used
//!   in any block but the one that makes it. So no value is carried in a register across a
//!   poll, no block needs a new parameter, and the ordinary dominance rule holds in both
//!   outputs. A constant, a function's or a datum's address and the address of a kept slot are
//!   made again at the use instead of stored. A value of the entry block's parameters is never
//!   kept: each poll passes its own (the task context is "the current task context" of each
//!   poll).
//!
//! # Out
//!
//! The state ([`StateLayout`]): the upvars' bytes first, then the kept slots, two of them
//! sharing bytes when no point of the body has both storage-live (coroutine_overlap.rs; the
//! design's D8), then the kept values, one field each, by falling alignment (each by its id),
//! then the tag: `Unresumed` 0, `Returned` 1, `Panicked` 2, point `k` is `3 + k`, in the
//! narrowest of `i8`, `i16`, `i32`.
//!
//! `poll`, of the body's signature: its entry block loads the tag and switches: 0 to the body's
//! start, `3 + k` to point `k`'s resume block, anything else to a trap (the library's "`async
//! fn` resumed after completion"). A `suspend` is "store the tag, return its values"; a
//! `return` is "store `Returned`, return".
//!
//! `drop`, taking the state pointer alone: a switch on the tag: 0 to [`Shape::unresumed`],
//! `3 + k` to point `k`'s cancel block, 1 and 2 to a return, anything else to a trap. A
//! `cancelled` is its return.
//!
//! Both are checked by the ordinary verifier: there is no "pass done" flag.
//!
//! # Refused by name
//!
//! A `catch` landing block (`catch_unwind`), `asm_br` and tail calls in a coroutine body; a
//! suspend in a cleanup path (coroutine_unwind.rs: unwinding through a poll, the `Panicked`
//! tag stored before each `resume`); a cancel path that suspends, returns, or uses a poll's
//! parameter other than the
//! state; a resume path that reaches `cancelled`; a compound, scalable or bit-vector value
//! live across a suspend.
//!
//! Owed: debug rows of kept slots (a `VarRec` names a slot; a kept local needs "the state
//! plus an offset"), so the outputs drop them.

use alloc::vec::Vec;
use alloc::alloc::Allocator;
use core::{convert::Infallible, fmt};

use crate::{
    Block, Func, Op, Ty, Value, View,
    func::{BlockKind, Ext, NONE, ParamKind, ParamRec, SigRec, SlotId},
    op::{Field, Layout, RewriteSink, Word, rewrite_fields, walk},
    ty::Form,
    verify::{VerifyError, verify},
};

#[path = "coroutine_unwind.rs"]
mod unwind;
pub use unwind::{Panics, split_with};

#[path = "coroutine_overlap.rs"]
mod overlap;

#[path = "coroutine_region.rs"]
mod region;
pub use region::{Region, SlotName, split_in};

/// The tag of a state never polled.
pub const UNRESUMED: u32 = 0;
/// The tag of a state whose body returned.
pub const RETURNED: u32 = 1;
/// The tag of a state whose poll unwound: stored before every `resume` (coroutine_unwind.rs).
pub const PANICKED: u32 = 2;
/// The tag of point 0; point `k` is `FIRST_POINT + k`.
pub const FIRST_POINT: u32 = 3;

/// `poll`'s trap code for a state that is `Returned` or `Panicked`: the library's
/// `panic_const_async_fn_resumed`, "`async fn` resumed after completion", until the builder
/// passes the panic's block in.
pub const RESUMED_TRAP: u32 = 0xfe10;
/// The trap code of a tag that names no state, in `poll` and in `drop`.
pub const TAG_TRAP: u32 = 0xfe11;

/// What the builder says of a body beside its LIR.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Shape {
    /// Which entry parameter is the state pointer, a `ptr`.
    pub state_param: u32,
    /// The bytes the state starts with: the upvars, laid out by the builder.
    pub prefix_size: u32,
    /// The log2 of their alignment.
    pub prefix_align: u8,
    /// The block that drops the upvars, ending in `cancelled`: where the state's drop goes
    /// when the state was never polled. No edge of the body leads to it.
    pub unresumed: Block,
}

/// What a field of the state holds, by its name in the body.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Saved {
    Slot(SlotId),
    Value(Value),
}

/// A field of the state past the upvars.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StateField {
    pub what: Saved,
    pub offset: u32,
    pub size: u32,
    /// The log2 of its alignment.
    pub align: u8,
}

/// The state's layout: what the seam's `LayoutFacts::coroutine` answers from.
#[derive(Clone, Debug)]
pub struct StateLayout<A: Allocator> {
    pub size: u32,
    /// The log2 of its alignment.
    pub align: u8,
    pub tag_offset: u32,
    /// `i8`, `i16` or `i32`.
    pub tag_ty: Ty,
    /// One past the highest point's number.
    pub points: u32,
    /// The kept slots and values, in offset order.
    pub fields: Vec<StateField, A>,
}

/// What [`split`] makes of a body.
#[derive(Clone, Debug)]
pub struct Split<A: Allocator> {
    pub poll: Func<A>,
    pub drop: Func<A>,
    pub layout: StateLayout<A>,
}

/// Which of the two functions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Which {
    Poll,
    Drop,
}

/// A planted bug, for the simulation's controls. Only test builds act on one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// The first kept slot stays on the stack.
    SlotLeftOut,
    /// On a cancel path a kept value is not loaded from the state.
    NoReloadOnCancel,
    /// A suspend returns without its tag stored (the store put after the return).
    TagAfterReturn,
    /// The tag of a point resumes at the next point's block.
    ResumeNext,
    /// A `resume` without `Panicked` stored at the tag first.
    NoPoisonOnUnwind,
    /// Every pair of kept slots taken as conflicting: no two share bytes (overlap off).
    NoOverlap,
    /// No pair of kept slots taken as conflicting: every one at the first offset.
    OverlapConflicting,
}

/// Why a body is not split.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CoroutineError {
    /// The body is not LIR.
    Verify(VerifyError),
    /// An output is not LIR: a bug of the pass, found in debug builds.
    Output(Which, VerifyError),
    /// [`Shape::state_param`] is no entry parameter, or not a `ptr`.
    State { param: u32 },
    /// [`Shape::unresumed`] is no block, or the entry block; or the prefix's alignment is
    /// no alignment.
    Shape { unresumed: u32 },
    /// An operation or block a coroutine body may not hold.
    Refused { inst: u32, op: Op, why: &'static str },
    /// Two suspends of one point.
    PointTwice { point: u32, inst: u32 },
    /// A point whose tag does not fit 32 bits.
    PointRange { point: u32, inst: u32 },
    /// A resume path reaches `cancelled` (the design's D7).
    CancelledOnResume { block: u32 },
    /// A cancel path suspends (D7: no async drop).
    SuspendOnCancel { block: u32 },
    /// A cancel path returns a value (D7).
    ReturnOnCancel { block: u32 },
    /// A value live across a suspend of a type the state cannot hold.
    Spill { value: u32, ty: Ty },
    /// A value used in `function` that nothing there makes and no suspend carries: a cancel
    /// path using a poll's parameter other than the state, or the unresumed block using a
    /// value of the body.
    NotCarried { value: u32, function: Which },
    /// The state, or an output, is past what a 32-bit offset or row names.
    TooLarge,
    /// In a state laid out from types ([`Region`]): the kept slots end at `needed`, past the
    /// region's `end`.
    PastRegion { needed: u32, end: u32 },
    /// In a state laid out from types: a value live across a suspend (or into a cleanup path)
    /// that is not made again after it. Only flow-level locals cross a suspension; the builder
    /// writes each into its slot (main's ruling, 2026-11-16; coroutine_region.rs).
    ValueAcrossSuspend { value: u32 },
    /// In a state laid out from types: its tag is not the width `points` suspend points need.
    RegionTag { points: u32 },
    /// In a state laid out from types: a kept slot asks alignment `2^needed`, past its `2^align`.
    RegionAlign { needed: u8, align: u8 },
    /// In a state laid out from types: slot `slot` is kept across a suspend by the pass's rule,
    /// code after a suspension reaches it, and the builder named it as no flow local
    /// ([`SlotName`]). The builder maps the slot to the expression that made it.
    UnnamedAcrossSuspend { slot: u32 },
    /// A [`SlotName`] of no slot of the body, or a slot named twice.
    SlotName { slot: u32 },
    /// A [`SlotName`] outside the region, or at an offset misaligned for its slot.
    NameOutside { slot: u32, offset: u32 },
    /// Two named slots sharing bytes.
    NamesOverlap { a: u32, b: u32 },
    /// A block of [`Panics`] that is no block, the entry, or a landing block.
    Panics { block: u32 },
}

impl fmt::Display for CoroutineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoroutineError::Verify(error) => write!(f, "the coroutine body is not LIR: {error}"),
            CoroutineError::Output(which, error) => {
                write!(f, "the coroutine pass's {which:?} output is not LIR: {error}")
            }
            CoroutineError::State { param } => {
                write!(f, "entry parameter {param} is not the state pointer: no such parameter, or not a `ptr`")
            }
            CoroutineError::Shape { unresumed } => write!(
                f,
                "block{unresumed} cannot be the unresumed block (no such block, or the entry), or the prefix's alignment is none"
            ),
            CoroutineError::Refused { inst, op, why } => {
                write!(f, "v{inst} = {} in a coroutine body: {why}", op.name())
            }
            CoroutineError::PointTwice { point, inst } => write!(f, "v{inst}: a second suspend of point {point}"),
            CoroutineError::PointRange { point, inst } => {
                write!(f, "v{inst}: point {point} has no tag in 32 bits")
            }
            CoroutineError::CancelledOnResume { block } => {
                write!(f, "block{block}: a resume path that reaches `cancelled`")
            }
            CoroutineError::SuspendOnCancel { block } => write!(f, "block{block}: a cancel path that suspends"),
            CoroutineError::ReturnOnCancel { block } => write!(f, "block{block}: a cancel path that returns"),
            CoroutineError::Spill { value, ty } => {
                write!(f, "v{value}: a value of type {ty} live across a suspend, which the state cannot hold")
            }
            CoroutineError::NotCarried { value, function } => {
                write!(f, "v{value} is used in {function:?}, where nothing makes it and no suspend carries it")
            }
            CoroutineError::TooLarge => f.write_str("the coroutine's state or an output is past 32-bit offsets"),
            CoroutineError::PastRegion { needed, end } => write!(
                f,
                "the kept slots end at byte {needed}, past the {end} the state laid from types gives them"
            ),
            CoroutineError::ValueAcrossSuspend { value } => write!(
                f,
                "v{value} is live across a suspend and is no saved local's slot: only flow-level locals cross a suspension (the builder writes each into its slot)"
            ),
            CoroutineError::RegionTag { points } => {
                write!(f, "the state laid from types has a tag of another width than {points} suspend points need")
            }
            CoroutineError::RegionAlign { needed, align } => {
                write!(f, "a kept slot asks alignment 2^{needed}, past the 2^{align} of the state laid from types")
            }
            CoroutineError::UnnamedAcrossSuspend { slot } => write!(
                f,
                "slot {slot} is read after a suspension and names no saved flow local: only flow locals cross a suspension"
            ),
            CoroutineError::SlotName { slot } => write!(f, "a state name of slot {slot}, which the body has not, or names twice"),
            CoroutineError::NameOutside { slot, offset } => write!(
                f,
                "slot {slot} named at state byte {offset}: outside the saved region, or misaligned for the slot"
            ),
            CoroutineError::NamesOverlap { a, b } => write!(f, "named slots {a} and {b} share bytes of the state"),
            CoroutineError::Panics { block } => write!(
                f,
                "block{block} cannot be where `poll` goes on a finished state: no such block, the entry, or a landing block"
            ),
        }
    }
}

impl core::error::Error for CoroutineError {}

/// Splits the coroutine body `view` into its `poll`, its state's `drop` and the state's
/// layout, every buffer in `alloc`.
pub fn split<A: Allocator + Clone>(view: View<'_>, shape: Shape, alloc: A) -> Result<Split<A>, CoroutineError> {
    split_with(view, shape, Panics::NONE, alloc)
}

/// [`split`] with a planted bug, which only test builds act on.
pub fn split_planted<A: Allocator + Clone>(
    view: View<'_>,
    shape: Shape,
    plant: Plant,
    alloc: A,
) -> Result<Split<A>, CoroutineError> {
    split_planted_with(view, shape, Panics::NONE, plant, alloc)
}

/// [`split_with`] with a planted bug, which only test builds act on.
pub fn split_planted_with<A: Allocator + Clone>(
    view: View<'_>,
    shape: Shape,
    panics: Panics,
    plant: Plant,
    alloc: A,
) -> Result<Split<A>, CoroutineError> {
    split_inner(view, shape, panics, plant, None, alloc)
}

/// The one split: [`split_planted_with`] (the pass lays the state), and with `region`
/// [`split_in`] (the caller laid it from types and named the slots that are saved locals;
/// coroutine_region.rs).
fn split_inner<A: Allocator + Clone>(
    view: View<'_>,
    shape: Shape,
    panics: Panics,
    plant: Plant,
    region: Option<(Region, &[SlotName])>,
    alloc: A,
) -> Result<Split<A>, CoroutineError> {
    verify(view).map_err(CoroutineError::Verify)?;
    let scratch = &alloc;
    let body = read(view, shape, panics, &scratch)?;
    let live = overlap::addressed(&body, &scratch);
    let lir_kept = kept_slots(&body, &live, plant, &scratch);
    // In a laid state the names decide what is kept, and an unnamed slot the pass would keep
    // that code after a suspension reaches is refused (coroutine_region.rs `kept_by_name`).
    let (kept, at) = match region {
        Some((r, names)) => {
            let (kept, at) = region::kept_by_name(&body, &lir_kept, r, names, &scratch)?;
            (kept, Some((r, at)))
        }
        None => (lir_kept, None),
    };
    let conflicts = overlap::conflicts(&body, &live, &kept, plant, &scratch);
    let crossing = crossing_values(&body, &scratch);
    let plan = plan(&body, &kept, &conflicts, &crossing, at.as_ref().map(|(r, at)| (*r, &at[..])), &scratch, alloc.clone())?;
    let poll = emit(&body, &plan, Which::Poll, plant, &scratch, alloc.clone())?;
    let drop = emit(&body, &plan, Which::Drop, plant, &scratch, alloc.clone())?;
    Ok(Split { poll, drop, layout: plan.layout })
}

/// A reachable suspend.
#[derive(Clone, Copy, Debug)]
struct SuspendAt {
    point: u32,
    inst: u32,
    block: u32,
    resume: u32,
    cancel: u32,
}

/// A body, read: which blocks each output takes and where it suspends.
struct Body<'a, A: Allocator> {
    view: View<'a>,
    shape: Shape,
    /// How many entry parameters: the values `0..entry_params`.
    entry_params: usize,
    /// Per instruction, the block its value is defined in: its own, or for an `invoke` its
    /// normal block (the verifier's rule).
    block_of: Vec<u32, A>,
    /// Per block, whether `poll` holds it: reached from the entry by any edge but a
    /// suspend's cancel edge.
    in_poll: Vec<bool, A>,
    /// Per block, whether `drop` holds it: reached from the unresumed block or a cancel
    /// block.
    in_drop: Vec<bool, A>,
    /// The suspends `poll` holds, by point.
    suspends: Vec<SuspendAt, A>,
    /// Where `poll` goes on a finished state.
    panics: Panics,
    /// The landing blocks of the body's `invoke`s, rising: the cleanup paths' entries.
    landings: Vec<u32, A>,
    /// Per block: the `invoke` whose normal block it is, [`NONE`] for another.
    invoked: Vec<u32, A>,
}

fn filled<T: Clone, A: Allocator + Copy>(len: usize, item: T, alloc: &A) -> Vec<T, A> {
    let mut all = Vec::with_capacity_in(len, *alloc);
    all.resize(len, item);
    all
}

/// Calls `f` with each block the terminator `at` leads to, and whether the edge is a
/// suspend's cancel edge. A body that [`read`] took has no other terminator with an edge.
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
        // The normal edge, then the unwind edge.
        Op::Invoke | Op::InvokeIndirect => {
            o.word();
            o.sig();
            f(o.block(), false);
            f(o.block(), false);
        }
        // `return`, `trap`, `unreachable`, `cancelled`, `resume`: no edge. The one terminator
        // with other edges (`asm_br`) is refused by `read` before anything asks.
        _ => {}
    }
}

/// Calls `f` with each value instruction `at` uses.
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
            debug_assert!(walked.is_ok(), "the operands of a verified function walk");
        }
    }
}

/// The blocks reached from `roots`, with or without suspends' cancel edges.
fn reach<A: Allocator + Copy>(view: &View<'_>, roots: &[u32], cancel_edges: bool, alloc: &A) -> Vec<bool, A> {
    let mut seen = filled(view.blocks(), false, alloc);
    let mut work = Vec::new_in(*alloc);
    for &root in roots {
        if !seen[root as usize] {
            seen[root as usize] = true;
            work.push(root);
        }
    }
    while let Some(b) = work.pop() {
        successors(view, view.terminator(Block(b)), |to, cancel| {
            if (cancel_edges || !cancel) && !seen[to.index()] {
                seen[to.index()] = true;
                work.push(to.0);
            }
        });
    }
    seen
}

fn read<'a, A: Allocator + Copy>(
    view: View<'a>,
    shape: Shape,
    panics: Panics,
    alloc: &A,
) -> Result<Body<'a, A>, CoroutineError> {
    let blocks = view.blocks();
    let entry_params = view.params(Block(0)).len();
    if shape.state_param as usize >= entry_params || view.ty(Value(shape.state_param)) != Ty::PTR {
        return Err(CoroutineError::State { param: shape.state_param });
    }
    if shape.unresumed.index() >= blocks || shape.unresumed.0 == 0 || shape.prefix_align >= 32 {
        return Err(CoroutineError::Shape { unresumed: shape.unresumed.0 });
    }
    unwind::check_panics(&view, panics)?;
    let mut block_of = filled(view.len(), 0u32, alloc);
    for b in 0..blocks {
        for at in view.block(Block(b as u32)) {
            block_of[at] = b as u32;
            let op = view.op(at);
            let why = match op {
                Op::AsmBr => "an `asm!` block with labels is not built",
                Op::TailCall | Op::TailCallIndirect => "a tail call would return without the `Returned` tag stored",
                _ => continue,
            };
            return Err(CoroutineError::Refused { inst: at as u32, op, why });
        }
    }
    // After every instruction's block is set: an invoke's value is defined in its normal
    // block. Every block reached from a landing block is a cleanup block, which may not
    // suspend.
    let unwind::Unwinds { landings, invoked } = unwind::unwinds(&view, &mut block_of, alloc)?;
    let cleanup = reach(&view, &landings, true, alloc);
    unwind::no_suspend_in_cleanup(&view, &cleanup)?;
    // `poll` holds what the entry and the finished-state blocks reach.
    let (roots, n) = unwind::poll_roots(panics);
    let in_poll = reach(&view, &roots[..n], false, alloc);
    let mut suspends = Vec::new_in(*alloc);
    for b in 0..blocks {
        if !in_poll[b] {
            continue;
        }
        let at = view.terminator(Block(b as u32));
        match view.op(at) {
            Op::Suspend => {
                let mut o = view.operands(at);
                let point = o.imm();
                if point > u32::MAX - FIRST_POINT {
                    return Err(CoroutineError::PointRange { point, inst: at as u32 });
                }
                let (resume, cancel) = (o.block().0, o.block().0);
                suspends.push(SuspendAt { point, inst: at as u32, block: b as u32, resume, cancel });
            }
            Op::Cancelled => return Err(CoroutineError::CancelledOnResume { block: b as u32 }),
            _ => {}
        }
    }
    suspends.sort_unstable_by_key(|it| (it.point, it.inst));
    if let Some(pair) = suspends.windows(2).find(|pair| pair[0].point == pair[1].point) {
        return Err(CoroutineError::PointTwice { point: pair[1].point, inst: pair[1].inst });
    }
    let mut roots = Vec::with_capacity_in(suspends.len() + 1, *alloc);
    roots.push(shape.unresumed.0);
    roots.extend(suspends.iter().map(|it| it.cancel));
    let in_drop = reach(&view, &roots, true, alloc);
    for b in 0..blocks {
        if !in_drop[b] {
            continue;
        }
        match view.op(view.terminator(Block(b as u32))) {
            Op::Suspend => return Err(CoroutineError::SuspendOnCancel { block: b as u32 }),
            Op::Return => return Err(CoroutineError::ReturnOnCancel { block: b as u32 }),
            _ => {}
        }
    }
    Ok(Body { view, shape, entry_params, block_of, in_poll, in_drop, suspends, panics, landings, invoked })
}

/// Per slot, whether the state keeps it: whether at some suspend its address may have been
/// taken and its storage not ended (the module's first rule; `live` is that analysis,
/// coroutine_overlap.rs `addressed`).
fn kept_slots<A: Allocator + Copy>(
    body: &Body<'_, A>,
    live: &overlap::Addressed<A>,
    plant: Plant,
    alloc: &A,
) -> Vec<bool, A> {
    let (slots, words) = (body.view.slots.len(), live.words);
    let mut kept = filled(slots, false, alloc);
    for suspend in body.suspends.iter() {
        for (slot, keep) in kept.iter_mut().enumerate() {
            *keep |= live.at_end[suspend.block as usize * words + slot / 64] & (1u64 << (slot % 64)) != 0;
        }
    }
    // Planted (test builds only): the first kept slot stays on the stack.
    if cfg!(test) && plant == Plant::SlotLeftOut {
        if let Some(first) = kept.iter_mut().find(|it| **it) {
            *first = false;
        }
    }
    kept
}

/// Per value, whether it is live into the resume or the cancel block of some suspend. Each
/// value is walked back from the blocks that use it to the block that makes it; the blocks
/// passed are those it is live into.
fn crossing_values<A: Allocator + Copy>(body: &Body<'_, A>, alloc: &A) -> Vec<bool, A> {
    let view = &body.view;
    let blocks = view.blocks();
    let held = |b: usize| body.in_poll[b] || body.in_drop[b];
    let mut after = filled(blocks, false, alloc);
    for suspend in body.suspends.iter() {
        after[suspend.resume as usize] = true;
        after[suspend.cancel as usize] = true;
    }
    // A cleanup path's entry is counted as a cancel block is (ASYNC.md P5): a value live into
    // it is kept, and loaded there from the state.
    for &landing in body.landings.iter() {
        after[landing as usize] = true;
    }
    // Predecessors, in CSR.
    let mut starts = filled(blocks + 1, 0u32, alloc);
    for b in (0..blocks).filter(|&b| held(b)) {
        successors(view, view.terminator(Block(b as u32)), |to, _| starts[to.index() + 1] += 1);
    }
    for b in 0..blocks {
        starts[b + 1] += starts[b];
    }
    let mut preds = filled(starts[blocks] as usize, 0u32, alloc);
    let mut next = filled(blocks, 0u32, alloc);
    next.copy_from_slice(&starts[..blocks]);
    for b in (0..blocks).filter(|&b| held(b)) {
        successors(view, view.terminator(Block(b as u32)), |to, _| {
            preds[next[to.index()] as usize] = b as u32;
            next[to.index()] += 1;
        });
    }
    // Each use of a value in another block than the one that makes it, once per block. An
    // entry parameter is each poll's own and is never kept.
    let mut uses: Vec<(u32, u32), A> = Vec::new_in(*alloc);
    for b in (0..blocks).filter(|&b| held(b)) {
        for at in view.block(Block(b as u32)) {
            each_value(view, at, |value| {
                if value as usize >= body.entry_params && body.block_of[value as usize] != b as u32 {
                    uses.push((value, b as u32));
                }
            });
        }
    }
    uses.sort_unstable();
    uses.dedup();
    let mut crossing = filled(view.len(), false, alloc);
    // Per block, the last value whose walk passed it.
    let mut passed = filled(blocks, NONE, alloc);
    let mut work = Vec::new_in(*alloc);
    let mut i = 0;
    while i < uses.len() {
        let value = uses[i].0;
        let made_in = body.block_of[value as usize];
        work.clear();
        while i < uses.len() && uses[i].0 == value {
            let b = uses[i].1;
            i += 1;
            if passed[b as usize] != value {
                passed[b as usize] = value;
                work.push(b);
            }
        }
        while let Some(b) = work.pop() {
            if after[b as usize] {
                crossing[value as usize] = true;
                break;
            }
            for &pred in &preds[starts[b as usize] as usize..starts[b as usize + 1] as usize] {
                if pred != made_in && passed[pred as usize] != value {
                    passed[pred as usize] = value;
                    work.push(pred);
                }
            }
        }
    }
    crossing
}

/// A value's home: used as it is made.
const DIRECT: u32 = NONE;
/// A value's home: made again where it is used (a constant, an address).
const REMADE: u32 = NONE - 1;

/// Where everything lives.
struct Plan<S: Allocator, A: Allocator> {
    /// Per slot of the body: its offset in the state, [`NONE`] for a slot that stays.
    slot_offset: Vec<u32, S>,
    /// Per slot of the body: its row in the outputs' `slots`, [`NONE`] for a kept one.
    slot_row: Vec<u32, S>,
    /// Per value of the body: [`DIRECT`], [`REMADE`], or its field's offset.
    home: Vec<u32, S>,
    layout: StateLayout<A>,
}

/// The bytes a kept value of `ty` takes, if the state can hold one. An `i1` is kept as a
/// byte.
fn spill_bytes(ty: Ty, ptr_bits: u32) -> Option<u32> {
    if ty == Ty::I1 {
        return Some(1);
    }
    match ty.form() {
        Form::Scalar | Form::Vector => {
            let bits = ty.bits(ptr_bits);
            (bits != 0 && bits % 8 == 0).then_some(bits / 8)
        }
        Form::Scalable | Form::Compound => None,
    }
}

fn align_up(at: u64, align: u8) -> u64 {
    let mask = (1u64 << align) - 1;
    (at + mask) & !mask
}

fn plan<S: Allocator + Copy, A: Allocator>(
    body: &Body<'_, S>,
    kept: &[bool],
    conflicts: &[u64],
    crossing: &[bool],
    region: Option<(Region, &[u32])>,
    alloc: &S,
    out_alloc: A,
) -> Result<Plan<S, A>, CoroutineError> {
    let view = &body.view;
    let ptr_bits = view.head.ptr_bits;
    let mut slot_offset = filled(kept.len(), NONE, alloc);
    let mut slot_row = filled(kept.len(), NONE, alloc);
    let mut home = filled(view.len(), DIRECT, alloc);
    let mut fields = Vec::new_in(out_alloc);
    let mut sizes = Vec::new_in(*alloc);
    let mut rows = 0u32;
    for (slot, &keep) in kept.iter().enumerate() {
        let rec = view.slots[slot];
        if keep {
            if rec.align >= 32 {
                return Err(CoroutineError::TooLarge);
            }
            sizes.push((rec.size, rec.align));
            fields.push(StateField { what: Saved::Slot(SlotId(slot as u32)), offset: 0, size: rec.size, align: rec.align });
        } else {
            slot_row[slot] = rows;
            rows += 1;
        }
    }
    // The kept slots, in slot order as `conflicts` numbers them: two share bytes when no
    // point has both storage-live (coroutine_overlap.rs; the design's D8).
    // In a laid state each kept slot is at its name's offset (coroutine_region.rs).
    let placed = match region {
        Some((_, at)) => region::placed(kept, at, alloc),
        None => overlap::offsets(&sizes, conflicts, body.shape.prefix_size, alloc)?,
    };
    let mut at = u64::from(body.shape.prefix_size);
    let mut align = body.shape.prefix_align;
    for (field, &offset) in fields.iter_mut().zip(placed.iter()) {
        field.offset = offset;
        at = at.max(u64::from(offset) + u64::from(field.size));
        align = align.max(field.align);
        if let Saved::Slot(slot) = field.what {
            slot_offset[slot.index()] = offset;
        }
    }
    let slots = fields.len();
    // In a state laid from types the kept slots end inside its region.
    if let Some((r, _)) = region {
        if at > u64::from(r.end) {
            return Err(CoroutineError::PastRegion { needed: u32::try_from(at).unwrap_or(u32::MAX), end: r.end });
        }
        if align > r.align {
            return Err(CoroutineError::RegionAlign { needed: align, align: r.align });
        }
    }
    for (value, _) in crossing.iter().enumerate().filter(|it| *it.1) {
        let remade = match view.op(value) {
            Op::SlotAddr => kept[view.data[value] as usize],
            Op::Iconst | Op::Null | Op::FuncAddr | Op::DataAddr => true,
            _ => false,
        };
        if remade {
            home[value] = REMADE;
            continue;
        }
        // Only flow-level locals cross a suspension (main's ruling): in a laid state a value
        // crossing that is not made again has no field.
        if region.is_some() {
            return Err(CoroutineError::ValueAcrossSuspend { value: value as u32 });
        }
        let ty = Ty(view.tys[value]);
        let Some(size) = spill_bytes(ty, ptr_bits) else {
            return Err(CoroutineError::Spill { value: value as u32, ty });
        };
        // A scalar is aligned to its size; a vector to at most 16, since the state's own
        // accesses claim no alignment.
        let align = (size.next_power_of_two().trailing_zeros() as u8).min(4);
        fields.push(StateField { what: Saved::Value(Value(value as u32)), offset: 0, size, align });
    }
    // The values past the slots, one field each, by falling alignment then value. The key is
    // total, so the order is the same on any machine.
    fields[slots..].sort_unstable_by_key(|it| {
        let id = match it.what {
            Saved::Slot(slot) => slot.0,
            Saved::Value(value) => value.0,
        };
        (u8::MAX - it.align, id)
    });
    for field in fields[slots..].iter_mut() {
        at = align_up(at, field.align);
        field.offset = u32::try_from(at).map_err(|_| CoroutineError::TooLarge)?;
        at += u64::from(field.size);
        align = align.max(field.align);
        if let Saved::Value(value) = field.what {
            home[value.index()] = field.offset;
        }
    }
    // `StateLayout::fields` is in offset order; slots sharing an offset, then a slot before a
    // value, each by its id: a total key.
    fields.sort_unstable_by_key(|it| {
        let (kind, id) = match it.what {
            Saved::Slot(slot) => (0u8, slot.0),
            Saved::Value(value) => (1u8, value.0),
        };
        (it.offset, kind, id)
    });
    let points = body.suspends.last().map_or(0, |it| it.point + 1);
    // The highest tag stored or switched on.
    let top = u64::from(FIRST_POINT) + u64::from(points);
    let (tag_ty, tag_align) = if top <= 1 << 8 {
        (Ty::I8, 0u8)
    } else if top <= 1 << 16 {
        (Ty::I16, 1)
    } else {
        (Ty::I32, 2)
    };
    let (size, align, tag_offset) = match region {
        // The caller's layout: the tag where it says, of the width these points need.
        Some((r, _)) => {
            if r.tag_ty != tag_ty {
                return Err(CoroutineError::RegionTag { points });
            }
            (u64::from(r.size), r.align, u64::from(r.tag_offset))
        }
        None => {
            let tag_offset = align_up(at, tag_align);
            let align = align.max(tag_align);
            (align_up(tag_offset + (1u64 << tag_align), align), align, tag_offset)
        }
    };
    // Every offset is an `i32` of a load or store.
    if size > i32::MAX as u64 {
        return Err(CoroutineError::TooLarge);
    }
    let layout = StateLayout { size: size as u32, align, tag_offset: tag_offset as u32, tag_ty, points, fields };
    Ok(Plan { slot_offset, slot_row, home, layout })
}

/// One output being made.
struct Emit<'p, 'a, S: Allocator + Copy, A: Allocator> {
    body: &'p Body<'a, S>,
    plan: &'p Plan<S, A>,
    which: Which,
    plant: Plant,
    func: Func<A>,
    /// Per value of the body: its value here, [`NONE`] if nothing here makes it.
    map: Vec<u32, S>,
    /// Per block of the body: its block here, [`NONE`] if this output has it not.
    block_map: Vec<u32, S>,
    /// The state pointer here.
    state: u32,
    /// The body's block being copied.
    current: u32,
    /// Per value of the body: what stands for it in the block `fetched_in` names (a load
    /// from the state, or the value made again).
    fetched: Vec<u32, S>,
    fetched_in: Vec<u32, S>,
    /// The places that hold a value of the body, to be its value here once every block is
    /// copied (a use may come before its value in layout order): an index of `data` if the
    /// flag is set, else of `extra`.
    fixups: Vec<(u32, bool), S>,
    ptr_int: Ty,
}

impl<S: Allocator + Copy, A: Allocator> Emit<'_, '_, S, A> {
    fn begin(&mut self, kind: BlockKind) {
        self.func.block_starts.push(self.func.ops.len() as u32);
        self.func.block_kinds.push(kind as u8);
    }

    /// Appends an instruction with the columns given.
    fn raw(&mut self, op: Op, flags: u8, ty: Ty, data: u32, attrs: u32, loc: u32) -> u32 {
        let at = self.func.ops.len() as u32;
        self.func.ops.push(op as u8);
        self.func.flags.push(flags);
        self.func.tys.push(ty.0);
        self.func.data.push(data);
        self.func.attrs.push(attrs);
        self.func.locs.push(loc);
        at
    }

    /// Appends an instruction of the pass's own: no flags, no memory promise (an access of
    /// alignment 1), its operands `words` as its layout has them.
    fn push(&mut self, op: Op, ty: Ty, words: &[u32], loc: u32) -> u32 {
        let data = match op.info().layout {
            Layout::None => 0,
            Layout::Data(_) => words[0],
            Layout::Extra(_) => {
                let start = self.func.extra.len() as u32;
                self.func.extra.extend_from_slice(words);
                start
            }
        };
        self.raw(op, 0, ty, data, 0, loc)
    }

    /// The address `offset` bytes into the state.
    fn state_ptr(&mut self, offset: u32, loc: u32) -> u32 {
        if offset == 0 {
            return self.state;
        }
        let by = self.push(Op::Iconst, self.ptr_int, &[offset], loc);
        let state = self.state;
        self.push(Op::PtrAdd, Ty::PTR, &[state, by], loc)
    }

    fn store_tag(&mut self, tag: u32, loc: u32) {
        let (ty, offset, state) = (self.plan.layout.tag_ty, self.plan.layout.tag_offset, self.state);
        let tag = self.push(Op::Iconst, ty, &[tag], loc);
        self.push(Op::Store, Ty::NONE, &[tag, state, offset], loc);
    }

    /// Makes the body's `value` usable in the current block: nothing for a value used as it
    /// is made, else a load from its field or the value made again, once per block.
    fn fetch(&mut self, value: u32, loc: u32) {
        let at = value as usize;
        if at < self.body.entry_params {
            return;
        }
        let home = self.plan.home[at];
        if home == DIRECT || self.body.block_of[at] == self.current || self.fetched_in[at] == self.current {
            return;
        }
        let view = self.body.view;
        let ty = Ty(view.tys[at]);
        let state = self.state;
        let new = if home == REMADE {
            match view.op(at) {
                Op::SlotAddr => self.state_ptr(self.plan.slot_offset[view.data[at] as usize], loc),
                // `iconst`, `null`, `func_addr`, `data_addr`: one word, or none.
                op => self.push(op, ty, &[view.data[at]], loc),
            }
        } else if cfg!(test) && self.plant == Plant::NoReloadOnCancel && self.which == Which::Drop {
            // Planted (test builds only): a cancel path's value is not loaded.
            self.push(Op::Uninit, ty, &[], loc)
        } else if ty == Ty::I1 {
            let byte = self.push(Op::Load, Ty::I8, &[state, home], loc);
            self.push(Op::Trunc, Ty::I1, &[byte], loc)
        } else {
            self.push(Op::Load, ty, &[state, home], loc)
        };
        self.fetched[at] = new;
        self.fetched_in[at] = self.current;
    }

    /// Stores the body's value `at`, just made here, to its field, if it has one.
    fn spill(&mut self, at: usize) {
        let home = self.plan.home[at];
        if home == DIRECT || home == REMADE {
            return;
        }
        let view = self.body.view;
        let (loc, state, made) = (view.locs[at], self.state, self.map[at]);
        let stored = match Ty(view.tys[at]) == Ty::I1 {
            true => self.push(Op::Zext, Ty::I8, &[made], loc),
            false => made,
        };
        self.push(Op::Store, Ty::NONE, &[stored, state, home], loc);
    }

    /// Appends the operands `fields` at `words` to `extra`, each value, block and slot as
    /// this output names it. The mirror of [`walk`], on a verified function.
    fn rewrite(&mut self, words: &[u32], fields: &[Field], ty: Ty) {
        let tys = self.body.view.tys;
        let Ok(()) = rewrite_fields(words, fields, ty, tys, self);
    }

    /// Copies the body's instruction `at`, its operands as this output names them.
    fn copy(&mut self, at: usize) -> u32 {
        let view = self.body.view;
        let op = view.op(at);
        let ty = Ty(view.tys[at]);
        let new = self.func.ops.len() as u32;
        let data = match op.info().layout {
            Layout::None => 0,
            Layout::Data(field) => {
                let word = view.data[at];
                match field {
                    Field::Value if self.fetched_in[word as usize] == self.current => self.fetched[word as usize],
                    Field::Value => {
                        self.fixups.push((new, true));
                        word
                    }
                    Field::Slot => self.plan.slot_row[word as usize],
                    _ => word,
                }
            }
            Layout::Extra(fields) => {
                let start = self.func.extra.len() as u32;
                let words: &[u32] = &view.extra[view.data[at] as usize..];
                self.rewrite(words, fields, ty);
                start
            }
        };
        self.raw(op, view.flags[at], ty, data, view.attrs[at], view.locs[at])
    }

    /// The body's instruction `at`, not a parameter, as this output has it.
    fn inst(&mut self, at: usize) {
        let view = self.body.view;
        let op = view.op(at);
        let loc = view.locs[at];
        match op {
            Op::SlotAddr | Op::SlotLive | Op::SlotDead => {
                let offset = self.plan.slot_offset[view.data[at] as usize];
                if offset != NONE {
                    // A kept slot is a field: its address is the state's plus its offset,
                    // and its storage is the state's.
                    if op == Op::SlotAddr {
                        self.map[at] = self.state_ptr(offset, loc);
                    }
                    return;
                }
            }
            Op::Suspend => {
                let mut o = view.operands(at);
                let point = o.imm();
                o.block();
                o.block();
                let pending = o.values();
                for value in pending.iter() {
                    self.fetch(value.0, loc);
                }
                // Planted (test builds only): the tag is not stored before the return.
                if !(cfg!(test) && self.plant == Plant::TagAfterReturn) {
                    self.store_tag(FIRST_POINT + point, loc);
                }
                let start = self.func.extra.len() as u32;
                self.func.extra.push(pending.len() as u32);
                for value in pending.iter() {
                    self.put_value(value.0);
                }
                self.raw(Op::Return, 0, Ty::NONE, start, 0, loc);
                return;
            }
            Op::Cancelled => {
                self.push(Op::Return, Ty::NONE, &[0], loc);
                return;
            }
            _ => {}
        }
        each_value(&view, at, |value| self.fetch(value, loc));
        if op == Op::Return {
            self.store_tag(RETURNED, loc);
        }
        // An unwind out of a poll leaves the state `Panicked` (coroutine_unwind.rs). Planted
        // (test builds only): the store left out.
        if op == Op::Resume && !(cfg!(test) && self.plant == Plant::NoPoisonOnUnwind) {
            self.store_tag(PANICKED, loc);
        }
        self.map[at] = self.copy(at);
        // An invoke's value is defined in its normal block and stored there
        // (`spill_invoked`): a store here would follow the terminator.
        if !matches!(op, Op::Invoke | Op::InvokeIndirect) {
            self.spill(at);
        }
    }
}

impl<S: Allocator + Copy, A: Allocator> RewriteSink for Emit<'_, '_, S, A> {
    type Error = Infallible;

    /// Appends, to `extra`, the word that stands for the body's `value` in the current block.
    fn put_value(&mut self, value: u32) {
        if self.fetched_in[value as usize] == self.current {
            self.func.extra.push(self.fetched[value as usize]);
        } else {
            self.fixups.push((self.func.extra.len() as u32, false));
            self.func.extra.push(value);
        }
    }

    fn put_block(&mut self, block: u32) {
        let to = self.block_map[block as usize];
        debug_assert!(to != NONE, "an edge of a held block leads to a held block");
        self.func.extra.push(to);
    }

    fn put_slot(&mut self, slot: u32) -> Result<(), Infallible> {
        self.func.extra.push(self.plan.slot_row[slot as usize]);
        Ok(())
    }

    fn push_word(&mut self, word: u32) {
        self.func.extra.push(word);
    }

    fn push_words(&mut self, words: &[u32]) {
        self.func.extra.extend_from_slice(words);
    }
}

/// Makes `which` of the two functions.
fn emit<S: Allocator + Copy, A: Allocator + Clone>(
    body: &Body<'_, S>,
    plan: &Plan<S, A>,
    which: Which,
    plant: Plant,
    alloc: &S,
    out_alloc: A,
) -> Result<Func<A>, CoroutineError> {
    let view = body.view;
    let blocks = view.blocks();
    // The body's tables; the instructions and blocks are made below.
    let mut func = view.to_func_in(out_alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();
    func.slots.clear();
    func.variables.clear();
    for (slot, rec) in view.slots.iter().enumerate() {
        if plan.slot_row[slot] != NONE {
            func.slots.push(*rec);
        }
    }
    // Owed: a kept local's debug row (the module's "Owed").
    for var in view.variables {
        let row = plan.slot_row[var.slot as usize];
        if row != NONE {
            let mut var = *var;
            var.slot = row;
            func.variables.push(var);
        }
    }
    if which == Which::Drop {
        // `(ptr)`, of the body's convention: the body's own row if it has one.
        let own = view.sigs[view.head.sig as usize];
        let param = ParamRec { facts: 0, ty: Ty::PTR.0, ext: Ext::None as u8, kind: ParamKind::Value as u8 };
        let found = view.sigs.iter().position(|rec| {
            rec.params == 1 && rec.rets == 0 && rec.conv == own.conv && rec.bits == 0 && view.sig_params[rec.first as usize] == param
        });
        func.head.sig = match found {
            Some(row) => row as u32,
            None => {
                let row = u32::try_from(func.sigs.len()).map_err(|_| CoroutineError::TooLarge)?;
                let first = u32::try_from(func.sig_params.len()).map_err(|_| CoroutineError::TooLarge)?;
                func.sigs.push(SigRec { first, params: 1, rets: 0, conv: own.conv, bits: 0, pad: [0; 2] });
                func.sig_params.push(param);
                row
            }
        };
    }
    // Block 0 is the head; then the body's blocks this output holds, in the body's order;
    // then the blocks of the pass's own.
    let held = match which {
        Which::Poll => &body.in_poll,
        Which::Drop => &body.in_drop,
    };
    let mut block_map = filled(blocks, NONE, alloc);
    let mut tail = 1u32;
    for b in (0..blocks).filter(|&b| held[b]) {
        block_map[b] = tail;
        tail += 1;
    }
    let ptr_int = Ty::int(view.head.ptr_bits).unwrap_or(Ty::I64);
    let mut e = Emit {
        body,
        plan,
        which,
        plant,
        func,
        map: filled(view.len(), NONE, alloc),
        block_map,
        state: NONE,
        current: NONE,
        fetched: filled(view.len(), NONE, alloc),
        fetched_in: filled(view.len(), NONE, alloc),
        fixups: Vec::new_in(*alloc),
        ptr_int,
    };
    // The head: the parameters, the tag, the switch.
    e.begin(BlockKind::Normal);
    let state_in_body = body.shape.state_param as usize;
    match which {
        Which::Poll => {
            for at in view.params(Block(0)) {
                e.map[at] = e.raw(Op::Param, view.flags[at], Ty(view.tys[at]), 0, view.attrs[at], view.locs[at]);
            }
        }
        Which::Drop => e.map[state_in_body] = e.push(Op::Param, Ty::PTR, &[], 0),
    }
    e.state = e.map[state_in_body];
    let (tag_ty, tag_offset, state) = (plan.layout.tag_ty, plan.layout.tag_offset, e.state);
    let tag = e.push(Op::Load, tag_ty, &[state, tag_offset], 0);
    // The switch's words: the scrutinee, the default target, the cases by rising tag.
    let mut words = Vec::new_in(*alloc);
    let case = |words: &mut Vec<u32, S>, tag: u32, to: u32| words.extend_from_slice(&[tag, to, 0]);
    match which {
        Which::Poll => {
            // A finished state goes to its block of `Panics` when the builder passed one; it
            // without one, and any tag that names no state, to the trap.
            let finished = body.panics.by_tag();
            let given = finished.iter().filter(|it| it.1.is_some()).count() as u32;
            words.extend_from_slice(&[tag, tail, 0, 1 + given + body.suspends.len() as u32]);
            case(&mut words, UNRESUMED, e.block_map[0]);
            for (code, block) in finished {
                if let Some(block) = block {
                    case(&mut words, code, e.block_map[block.index()]);
                }
            }
            for (i, suspend) in body.suspends.iter().enumerate() {
                // Planted (test builds only): tag `k` resumes at the next point's block.
                let from = match cfg!(test) && plant == Plant::ResumeNext {
                    true => &body.suspends[(i + 1) % body.suspends.len()],
                    false => suspend,
                };
                case(&mut words, FIRST_POINT + suspend.point, e.block_map[from.resume as usize]);
            }
        }
        Which::Drop => {
            // `tail` returns; `tail + 1` traps on a tag that names no state.
            words.extend_from_slice(&[tag, tail + 1, 0, 3 + body.suspends.len() as u32]);
            case(&mut words, UNRESUMED, e.block_map[body.shape.unresumed.index()]);
            case(&mut words, RETURNED, tail);
            case(&mut words, PANICKED, tail);
            for suspend in body.suspends.iter() {
                case(&mut words, FIRST_POINT + suspend.point, e.block_map[suspend.cancel as usize]);
            }
        }
    }
    e.push(Op::Switch, Ty::NONE, &words, 0);
    // The body's blocks.
    for b in (0..blocks).filter(|&b| held[b]) {
        e.begin(view.block_kind(Block(b as u32)));
        e.current = b as u32;
        let params = view.params(Block(b as u32));
        // The entry's parameters are the head's.
        if b != 0 {
            for at in params.start..params.end {
                e.map[at] = e.raw(Op::Param, view.flags[at], Ty(view.tys[at]), 0, view.attrs[at], view.locs[at]);
            }
            for at in params.start..params.end {
                e.spill(at);
            }
        }
        if body.invoked[b] != NONE {
            e.spill_invoked(body.invoked[b] as usize);
        }
        for at in params.end..view.block(Block(b as u32)).end {
            e.inst(at);
        }
    }
    // The pass's own blocks.
    match which {
        Which::Poll => {
            e.begin(BlockKind::Cold);
            e.push(Op::Trap, Ty::NONE, &[RESUMED_TRAP], 0);
        }
        Which::Drop => {
            e.begin(BlockKind::Normal);
            e.push(Op::Return, Ty::NONE, &[0], 0);
            e.begin(BlockKind::Cold);
            e.push(Op::Trap, Ty::NONE, &[TAG_TRAP], 0);
        }
    }
    // Every value of the body named in a copied operand becomes its value here.
    let Emit { mut func, map, fixups, .. } = e;
    for &(place, in_data) in fixups.iter() {
        let word = match in_data {
            true => &mut func.data[place as usize],
            false => &mut func.extra[place as usize],
        };
        let to = map[*word as usize];
        if to == NONE {
            return Err(CoroutineError::NotCarried { value: *word, function: which });
        }
        *word = to;
    }
    if u32::try_from(func.ops.len()).is_err() || u32::try_from(func.extra.len()).is_err() {
        return Err(CoroutineError::TooLarge);
    }
    // As the builder does: the output is checked in debug builds.
    if cfg!(debug_assertions) {
        verify(func.view()).map_err(|error| CoroutineError::Output(which, error))?;
    }
    Ok(func)
}
