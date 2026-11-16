//! The state (coroutines.md, "The state"): which locals have a slot in it, and the tags.
//!
//! The Reference, Async blocks: "The future type that rustc generates is roughly equivalent to
//! an enum with one variant per await point, where each variant stores the data needed to resume
//! from its corresponding point." Here it is one struct: the upvars first, then one slot per
//! saved local in declaration order, and a tag. No two slots overlap (decision D8). A local's
//! home is fixed for its whole life (decision D3): a saved local is read and written in the
//! state from its first write, never copied in or out at a suspension.
#![blaze::sans_io]

use blaze_flow::Bound;
use blaze_own::Vec;
use core::alloc::Allocator;

use crate::Plant;
use crate::ir::{Body, LocalId, PointId, SlotId};
use crate::live::{Across, across};

/// Where a local lives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Home {
    /// An ordinary local of `resume`: new and uninitialised at every poll.
    Frame,
    /// A slot of the state: it keeps its value and its address between polls.
    State(SlotId),
}

/// The state's tag: where the coroutine is parked.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Tag(pub u32);

impl Tag {
    /// Made, never polled: only the upvars are initialised.
    pub const UNRESUMED: Tag = Tag(0);
    /// Finished: nothing is initialised.
    pub const RETURNED: Tag = Tag(1);
    /// A poll unwound. Reserved: the rebuilt path aborts on a panic (decision D6).
    pub const PANICKED: Tag = Tag(2);
    const FIRST_POINT: u32 = 3;

    /// The tag of a coroutine parked at `point`.
    pub fn point(point: PointId) -> Tag {
        Tag(Tag::FIRST_POINT + point.0)
    }

    /// The point a tag parks at, among `points` points.
    pub fn as_point(self, points: u32) -> Option<PointId> {
        let k = self.0.checked_sub(Tag::FIRST_POINT)?;
        (k < points).then_some(PointId(k))
    }
}

/// The state of one coroutine body.
pub struct Layout<A: Allocator> {
    /// Each local's home.
    pub home: Vec<Home, A>,
    /// Each slot's local: the upvars, then the saved locals in declaration order.
    pub slots: Vec<LocalId, A>,
    /// The locals held across each point: the input of slot overlap, and the future's exact
    /// interior (decision D9).
    pub across: Across<A>,
    pub points: u32,
}

impl<A: Allocator> Layout<A> {
    pub fn home_of(&self, l: LocalId) -> Home {
        self.home[l.index()]
    }

    pub fn is_saved(&self, l: LocalId) -> bool {
        matches!(self.home[l.index()], Home::State(_))
    }
}

/// Lays out the state of a validated body: every upvar, and every local held across at least
/// one suspension, gets a slot; every other local stays in the frame.
pub fn layout<A: Allocator, B: Allocator + Clone>(body: &Body<A>, plant: Plant, alloc: B) -> Result<Layout<B>, Bound> {
    let across = across(body, plant, alloc.clone())?;
    let m = body.locals.len();
    let upvars = body.upvars as usize;
    let points = body.points as usize;
    let held = |l: usize| l < upvars || (0..points).any(|p| across.at.get(p, l));
    // Planted (test builds only): the last saved local that is not an upvar is left out.
    let mut left_out = None;
    if cfg!(test) && plant == Plant::LiveLeftOut {
        left_out = (upvars..m).rev().find(|&l| held(l));
    }
    let mut home = Vec::with_capacity_in(m, alloc.clone());
    let mut slots = Vec::new_in(alloc);
    for l in 0..m {
        if held(l) && left_out != Some(l) {
            home.push(Home::State(SlotId(slots.len() as u32)));
            slots.push(LocalId(l as u32));
        } else {
            home.push(Home::Frame);
        }
    }
    Ok(Layout { home, slots, across, points: body.points })
}
