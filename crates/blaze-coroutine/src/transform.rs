//! The rewrite (coroutines.md, "The rewrite" and "Drop on cancel"): a body with suspensions
//! becomes a [`Machine`]: the same blocks with every `Suspend` turned into `Park`, the state's
//! layout, and two entry tables read by the tag: where `resume` goes on, and what the state's
//! drop glue runs.
//!
//! The Reference, Await expressions, step 5: "If the call to poll returns Poll::Pending, then
//! the future returns Poll::Pending, suspending its state so that, when the surrounding async
//! context is re-polled, execution returns to step 3". `Park(k)` is "returns Poll::Pending,
//! suspending its state"; `Machine::entry(Tag::point(k))` is "execution returns to step 3".
#![blaze::sans_io]

use blaze_flow::Bound;
use blaze_own::Vec;
use core::alloc::Allocator;
use core::fmt;

use crate::Plant;
use crate::ir::{BlockId, Body, BodyBuilder, IrError, LocalId, Operand, Stmt, Term, validate};
use crate::layout::{Home, Layout, Tag, layout};

/// What `resume` does for a tag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Entry {
    /// Run from this block: the body's entry, or the resume block of the point parked at.
    Run(BlockId),
    /// The library's panic "`async fn` resumed after completion".
    Completed,
    /// The library's panic "`async fn` resumed after panicking".
    Panicked,
    /// Not a tag of this machine: a corrupt state.
    NoSuchTag,
}

/// What the state's drop glue does for a tag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DropEntry {
    /// Run this block to its `Cancelled`.
    Run(BlockId),
    /// Nothing is initialised: finished, or unwound.
    Nothing,
    NoSuchTag,
}

/// A body the transform refuses, named (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransformError {
    /// A dataflow went past its stated bound.
    Bound(Bound),
    /// The body is malformed.
    Ir(IrError),
    /// The input holds a `Park`: it is a machine's terminator.
    ParkInInput(BlockId),
    /// A cancel path reaches a suspension: a destructor cannot await (decision D7).
    SuspendOnCancelPath(BlockId),
    /// A cancel path reaches a `Return`: a dropped coroutine has no result (decision D7).
    ReturnOnCancelPath(BlockId),
    /// A resume path reaches `Cancelled`, the end of a cancel path (decision D7).
    CancelledOnResumePath(BlockId),
}

impl fmt::Display for TransformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            TransformError::Bound(b) => write!(f, "{} went past its bound of {}", b.what, b.limit),
            TransformError::Ir(e) => write!(f, "the body is malformed: {e}"),
            TransformError::ParkInInput(b) => write!(f, "{b} ends in `Park`, which only a machine holds"),
            TransformError::SuspendOnCancelPath(b) => {
                write!(f, "{b} suspends on a cancel path: a dropped coroutine cannot await")
            }
            TransformError::ReturnOnCancelPath(b) => {
                write!(f, "{b} returns on a cancel path: a dropped coroutine has no result")
            }
            TransformError::CancelledOnResumePath(b) => {
                write!(f, "{b} ends in `Cancelled` on a resume path: only a cancel path ends so")
            }
        }
    }
}

/// A transformed body: only [`transform`] makes one.
pub struct Machine<A: Allocator> {
    body: Body<A>,
    layout: Layout<A>,
    resume_at: Vec<BlockId, A>,
    drop_at: Vec<BlockId, A>,
    drop_unresumed: BlockId,
}

impl<A: Allocator> Machine<A> {
    /// The blocks: no `Suspend` among them.
    pub fn body(&self) -> &Body<A> {
        &self.body
    }

    pub fn layout(&self) -> &Layout<A> {
        &self.layout
    }

    /// The `match state.tag` at the head of `resume`.
    pub fn entry(&self, tag: Tag) -> Entry {
        if tag == Tag::UNRESUMED {
            return Entry::Run(self.body.entry());
        }
        if tag == Tag::RETURNED {
            return Entry::Completed;
        }
        if tag == Tag::PANICKED {
            return Entry::Panicked;
        }
        match tag.as_point(self.layout.points) {
            Some(p) => Entry::Run(self.resume_at[p.index()]),
            None => Entry::NoSuchTag,
        }
    }

    /// The `match state.tag` of the state's drop glue.
    pub fn drop_entry(&self, tag: Tag) -> DropEntry {
        if tag == Tag::UNRESUMED {
            return DropEntry::Run(self.drop_unresumed);
        }
        if tag == Tag::RETURNED || tag == Tag::PANICKED {
            return DropEntry::Nothing;
        }
        match tag.as_point(self.layout.points) {
            Some(p) => DropEntry::Run(self.drop_at[p.index()]),
            None => DropEntry::NoSuchTag,
        }
    }
}

/// Turns a body with suspensions into a machine.
pub fn transform<A: Allocator, B: Allocator + Clone>(body: &Body<A>, alloc: B) -> Result<Machine<B>, TransformError> {
    transform_planted(body, Plant::None, alloc)
}

/// [`transform`], with a planted bug in test builds (`plant` is ignored in any other build).
pub fn transform_planted<A: Allocator, B: Allocator + Clone>(
    body: &Body<A>,
    plant: Plant,
    alloc: B,
) -> Result<Machine<B>, TransformError> {
    validate(body, alloc.clone()).map_err(TransformError::Ir)?;
    check_paths(body, alloc.clone())?;
    let mut layout = layout(body, plant, alloc.clone()).map_err(TransformError::Bound)?;

    let n = body.blocks.len();
    let upvars = body.upvars as usize;
    let mut b = BodyBuilder::new_in(alloc.clone());
    for (i, row) in body.locals.iter().enumerate() {
        if i < upvars {
            b.upvar(row.needs_drop);
        } else {
            b.local(row.needs_drop);
        }
    }
    for _ in 0..n {
        b.block();
    }
    for _ in 0..body.points {
        b.point();
    }

    // Planted (test builds only): the first saved local something borrows lives in the frame,
    // and is copied to a slot at each suspension and back at each resume.
    let mut copied: Option<(LocalId, LocalId)> = None;
    if cfg!(test) && plant == Plant::SavedByCopy {
        let victim = (upvars..body.locals.len()).map(|l| LocalId(l as u32)).find(|&l| {
            layout.is_saved(l) && body.stmts.iter().any(|s| matches!(*s, Stmt::Borrow { of, .. } if of == l))
        });
        if let Some(victim) = victim {
            let spill = b.local(false);
            let slot = layout.home[victim.index()];
            if let Home::State(s) = slot {
                layout.slots[s.index()] = spill;
            }
            layout.home[victim.index()] = Home::Frame;
            layout.home.push(slot);
            copied = Some((victim, spill));
        }
    }

    let points = body.points as usize;
    let mut resume_at = Vec::with_capacity_in(points, alloc.clone());
    resume_at.resize(points, BlockId(0));
    let mut drop_at = Vec::with_capacity_in(points, alloc);
    drop_at.resize(points, BlockId(0));

    for i in 0..n {
        let id = BlockId(i as u32);
        b.begin(id);
        for &s in body.stmts_of(id) {
            match s {
                Stmt::Call { dest, func, args } => b.call(dest, func, body.args(args)),
                other => b.push(other),
            }
        }
        match body.term(id) {
            Term::Suspend { point, resume, drop } => {
                resume_at[point.index()] = resume;
                drop_at[point.index()] = drop;
                if let Some((victim, spill)) = copied {
                    b.push(Stmt::Use { dest: spill, src: Operand::Move(victim) });
                }
                b.end(Term::Park(point));
            }
            Term::Switch { on, targets } => b.switch(on, body.targets_of(targets)),
            Term::Park(_) => return Err(TransformError::ParkInInput(id)),
            term => b.end(term),
        }
    }

    // The glue of a coroutine never polled: its upvars, in declaration order (decision D5).
    let drop_unresumed = b.block();
    b.begin(drop_unresumed);
    for (i, row) in body.locals.iter().enumerate().take(upvars) {
        if row.needs_drop {
            b.push(Stmt::Drop(LocalId(i as u32)));
        }
    }
    b.end(Term::Cancelled);

    if let Some((victim, spill)) = copied {
        for p in 0..points {
            let back = b.block();
            b.begin(back);
            b.push(Stmt::Use { dest: victim, src: Operand::Move(spill) });
            b.end(Term::Goto(resume_at[p]));
            resume_at[p] = back;
        }
    }
    // Planted (test builds only): tag `k` resumes at point `k + 1`.
    if cfg!(test) && plant == Plant::ResumeNext && points > 1 {
        let first = resume_at[0];
        for p in 0..points - 1 {
            resume_at[p] = resume_at[p + 1];
        }
        resume_at[points - 1] = first;
    }
    // Planted (test builds only): cancelling drops every saved local that needs drop, whether
    // or not it is initialised at the point.
    if cfg!(test) && plant == Plant::CancelDropsAll {
        for p in 0..points {
            let all = b.block();
            b.begin(all);
            for &l in layout.slots.iter().rev() {
                if body.locals.get(l.index()).is_some_and(|row| row.needs_drop) {
                    b.push(Stmt::Drop(l));
                }
            }
            b.end(Term::Cancelled);
            drop_at[p] = all;
        }
    }

    let body = b.finish().map_err(TransformError::Ir)?;
    Ok(Machine { body, layout, resume_at, drop_at, drop_unresumed })
}

/// Decision D7, by two walks with a work list: a resume path (from the entry, through every
/// edge but a suspension's cancel edge) never ends in `Cancelled`; a cancel path (from every
/// suspension's cancel edge) never suspends and never returns.
fn check_paths<A: Allocator, B: Allocator + Clone>(body: &Body<A>, alloc: B) -> Result<(), TransformError> {
    let n = body.blocks.len();
    let mut seen = Vec::with_capacity_in(n, alloc.clone());
    seen.resize(n, false);
    let mut todo = Vec::with_capacity_in(n, alloc.clone());

    seen[0] = true;
    todo.push(body.entry());
    while let Some(b) = todo.pop() {
        let mut next = |t: BlockId| {
            if !seen[t.index()] {
                seen[t.index()] = true;
                todo.push(t);
            }
        };
        match body.term(b) {
            Term::Cancelled => return Err(TransformError::CancelledOnResumePath(b)),
            Term::Park(_) => return Err(TransformError::ParkInInput(b)),
            Term::Suspend { resume, .. } => next(resume),
            _ => body.for_each_succ(b, next),
        }
    }

    let mut seen = Vec::with_capacity_in(n, alloc);
    seen.resize(n, false);
    for i in 0..n {
        if let Term::Suspend { drop, .. } = body.term(BlockId(i as u32)) {
            if !seen[drop.index()] {
                seen[drop.index()] = true;
                todo.push(drop);
            }
        }
    }
    while let Some(b) = todo.pop() {
        match body.term(b) {
            Term::Suspend { .. } => return Err(TransformError::SuspendOnCancelPath(b)),
            Term::Return(_) => return Err(TransformError::ReturnOnCancelPath(b)),
            Term::Park(_) => return Err(TransformError::ParkInInput(b)),
            _ => body.for_each_succ(b, |t| {
                if !seen[t.index()] {
                    seen[t.index()] = true;
                    todo.push(t);
                }
            }),
        }
    }
    Ok(())
}
