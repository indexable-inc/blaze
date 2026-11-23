//! A panic inside a poll (ASYNC.md D-A5; a child of [`super`], the coroutine pass, so it
//! reads the pass's own tables).
//!
//! The Reference, "Panic": with `unwind`, "live objects in those frames that implement Drop
//! will have their drop methods called". The builder writes those drops as it writes them in
//! any function: a call that may unwind with something to drop is an `invoke` whose landing
//! block (a `cleanup` block) drops it and ends in `resume`. The split takes such bodies:
//!
//! - **Cleanup blocks.** An `invoke`'s landing block and every block reached from it is a
//!   cleanup block. It is copied into whichever output holds it like any block; a kept value
//!   used there is loaded from the state exactly as on a cancel path (the landing blocks are
//!   counted where the suspends' cancel blocks are: a value live into one is kept). A cleanup
//!   path may not suspend, as a cancel path may not: refused by name, "a suspension in a
//!   cleanup path".
//! - **Poisoning.** Every `resume` is preceded by a store of [`PANICKED`] at the tag, so a
//!   state whose poll unwound is `Panicked`: the split's `drop` at `Panicked` returns at once
//!   (the cleanup already dropped what was live), and `poll` at `Panicked` goes to
//!   [`Panics::panicked`].
//! - **An invoke's value** is defined at the head of its normal block (the verifier's rule),
//!   so it is kept, when it is kept, by a store at the head of that block.
//! - **Finished states.** `poll` at `Returned` goes to [`Panics::returned`] and at `Panicked`
//!   to [`Panics::panicked`]: blocks of the body that call the library's
//!   `panic_const_async_fn_resumed` ("`async fn` resumed after completion",
//!   core/src/panicking.rs:199) and `panic_const_async_fn_resumed_panic` ("`async fn` resumed
//!   after panicking", :203). A program without those items (`no_core`) passes none, and
//!   `poll` traps [`super::RESUMED_TRAP`] there instead.
//!
//! Refused by name: a `catch` landing block (`catch_unwind` inside an `async` body, which the
//! builder refuses too: a caught unwind goes on in ordinary code, which nothing here has
//! been shown to split right).

use alloc::vec::Vec;
use alloc::alloc::Allocator;

use super::{CoroutineError, DIRECT, Emit, PANICKED, Plant, REMADE, Shape, Split, filled, split_planted_with};
use crate::{Block, Op, Ty, View, func::BlockKind, func::NONE};

/// The two finished tags `poll` may go to a block of [`Panics`] on, by rising tag.
pub(super) const FINISHED: [u32; 2] = [super::RETURNED, PANICKED];

/// Where `poll` goes on a state that is already finished: blocks of the body that no edge of
/// it leads to, each calling the library's panic for that case (and never returning). `None`
/// keeps the trap [`super::RESUMED_TRAP`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Panics {
    /// `poll` of a `Returned` state: "`async fn` resumed after completion".
    pub returned: Option<Block>,
    /// `poll` of a `Panicked` state: "`async fn` resumed after panicking".
    pub panicked: Option<Block>,
}

/// [`super::split`] of a body that may unwind, `poll` going to `panics`' blocks on a finished
/// state.
pub fn split_with<A: Allocator + Clone>(
    view: View<'_>,
    shape: Shape,
    panics: Panics,
    alloc: A,
) -> Result<Split<A>, CoroutineError> {
    split_planted_with(view, shape, panics, Plant::None, alloc)
}

impl Panics {
    /// No blocks: `poll` traps on a finished state.
    pub const NONE: Panics = Panics { returned: None, panicked: None };

    /// The blocks by [`FINISHED`]'s order.
    pub(super) fn by_tag(self) -> [(u32, Option<Block>); 2] {
        [(FINISHED[0], self.returned), (FINISHED[1], self.panicked)]
    }
}

/// What [`super::read`] learns of a body's unwind edges.
pub(super) struct Unwinds<A: Allocator> {
    /// The landing blocks of the body's `invoke`s, rising, each once: the cleanup paths'
    /// entries.
    pub(super) landings: Vec<u32, A>,
    /// Per block: the `invoke` whose normal block it is, [`NONE`] for another.
    pub(super) invoked: Vec<u32, A>,
}

/// Reads the body's `invoke`s: their landing blocks, and per normal block its `invoke`. An
/// invoke's value is defined at the head of its normal block, so `defined_in` (per
/// instruction, the block its value is defined in) names that block for it.
pub(super) fn unwinds<A: Allocator + Copy>(
    view: &View<'_>,
    defined_in: &mut [u32],
    alloc: &A,
) -> Result<Unwinds<A>, CoroutineError> {
    let blocks = view.blocks();
    let mut landings = Vec::new_in(*alloc);
    let mut invoked = filled(blocks, NONE, alloc);
    for b in 0..blocks {
        let at = view.terminator(Block(b as u32));
        let op = view.op(at);
        if !matches!(op, Op::Invoke | Op::InvokeIndirect) {
            continue;
        }
        let mut o = view.operands(at);
        o.word();
        o.sig();
        let (normal, unwind) = (o.block(), o.block());
        if view.block_kind(unwind) == BlockKind::Catch {
            return Err(CoroutineError::Refused {
                inst: at as u32,
                op,
                why: "an unwind edge to a `catch` block: `catch_unwind` in a coroutine body is not built",
            });
        }
        defined_in[at] = normal.0;
        invoked[normal.index()] = at as u32;
        landings.push(unwind.0);
    }
    landings.sort_unstable();
    landings.dedup();
    Ok(Unwinds { landings, invoked })
}

/// Checks [`Panics`]' blocks: each a block of the body, not the entry, not a landing block.
pub(super) fn check_panics(view: &View<'_>, panics: Panics) -> Result<(), CoroutineError> {
    for block in [panics.returned, panics.panicked].into_iter().flatten() {
        if block.index() >= view.blocks() || block.0 == 0 || view.block_kind(block).is_landing() {
            return Err(CoroutineError::Panics { block: block.0 });
        }
    }
    Ok(())
}

/// The roots `poll` holds the blocks reached from: the entry, then [`Panics`]' blocks.
pub(super) fn poll_roots(panics: Panics) -> ([u32; 3], usize) {
    let mut roots = [0u32; 3];
    let mut n = 1;
    for block in [panics.returned, panics.panicked].into_iter().flatten() {
        roots[n] = block.0;
        n += 1;
    }
    (roots, n)
}

/// Refuses a suspend in a cleanup block (`cleanup` per block).
pub(super) fn no_suspend_in_cleanup(view: &View<'_>, cleanup: &[bool]) -> Result<(), CoroutineError> {
    for (b, _) in cleanup.iter().enumerate().filter(|it| *it.1) {
        let at = view.terminator(Block(b as u32));
        if view.op(at) == Op::Suspend {
            return Err(CoroutineError::Refused { inst: at as u32, op: Op::Suspend, why: "a suspension in a cleanup path" });
        }
    }
    Ok(())
}

impl<S: Allocator + Copy, A: Allocator> Emit<'_, '_, S, A> {
    /// Stores the body's `invoke` value `invoke` to its field, if it has one, at the head of
    /// the invoke's normal block (the current one). The stored word names the body's value
    /// and is fixed up once every block is copied: the normal block may come before the
    /// invoke's own in the body's order.
    pub(super) fn spill_invoked(&mut self, invoke: usize) {
        let home = self.plan.home[invoke];
        if home == DIRECT || home == REMADE {
            return;
        }
        let view = self.body.view;
        let (loc, state) = (view.locs[invoke], self.state);
        if Ty(view.tys[invoke]) == Ty::I1 {
            // `zext`'s one operand is its `data` word.
            let byte = self.push(Op::Zext, Ty::I8, &[invoke as u32], loc);
            self.fixups.push((byte, true));
            self.push(Op::Store, Ty::NONE, &[byte, state, home], loc);
        } else {
            // `store`'s first operand is the first word it puts in `extra`.
            let first = self.func.extra.len() as u32;
            self.push(Op::Store, Ty::NONE, &[invoke as u32, state, home], loc);
            self.fixups.push((first, false));
        }
    }
}
