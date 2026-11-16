//! The adapter OUT (coroutines.md, "From a checked body and back", "Back"): what the code
//! builder receives for one coroutine, as flat tables with typed ids. Not blocks to walk: the
//! builder goes on walking the expression tree with its own drop scopes, and reads from a
//! [`Plan`]:
//!
//! - the STATE: its fields in order ([`Plan::fields`]): the upvars, the saved locals in
//!   declaration order (both as [`crate::layout::Layout::slots`] gives them), one FLAG byte per
//!   local some cancel drops, and the tag last. What each field is ([`Origin`]) is the walk's
//!   own id of the binding, temporary or await, so the builder finds its type where it already
//!   has it; the state's bytes are the builder's one layout of that field list (rule 30);
//! - each saved local's HOME ([`Plan::home_of`]): the field every read and write of it goes
//!   to, through the pinned `&mut` state, from its first write (decision D3); a local with no
//!   home is an ordinary local of the poll;
//! - the HEAD SWITCH ([`Plan::resume`]): what a poll does for each tag;
//! - at each `.await` its [`PointRow`]: the tag to store before returning `Poll::Pending`, the
//!   field of the future tmp polled in place, and the resume label's point;
//! - the CANCEL order of each point ([`Plan::cancel`]) for the state's drop glue, and the
//!   upvars dropped before the first poll ([`Plan::unresumed`], decision D5).
//!
//! Flags (decision D2, as built): whether a local is initialised when the coroutine parks is
//! not a static fact (a move on one branch before the suspension). The builder already keeps
//! that fact, per binding, as a drop flag or as its own static knowledge. A flag is never
//! borrowed, so it may be copied (decision D3 forbids a copy only of what a reference may
//! point at): before parking at a point the poll stores, for each entry of the point's cancel
//! order but the future tmp, whether it is initialised, into the entry's flag field; after a
//! resume it reads them back. The drop glue tests the field. The future tmp of the point has
//! no flag: a coroutine parked at a point holds that point's future.
#![blaze::sans_io]

use blaze_own::Vec;
use core::alloc::Allocator;
use core::fmt;

use crate::ir::{LocalId, PointId, Span32};
use crate::layout::{Home, Tag};
use crate::record::{Lowered, Origin};
use crate::transform::{TransformError, transform};

/// A field of the state, in [`Plan::fields`]' order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FieldId(pub u32);

impl FieldId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FieldKind {
    /// A capture, or an `async fn`'s parameter: written when the state is made.
    Upvar,
    /// A local held across some suspension: of the local's type.
    Saved,
    /// One byte: whether `local` is initialised while the coroutine is parked.
    Flag,
    /// The tag, a `u32`: [`Tag`].
    Tag,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Field {
    pub kind: FieldKind,
    /// The local the field holds, or whose flag it is; `None` for the tag.
    pub local: Option<LocalId>,
    /// What that local is to the walk; `Origin::Scratch` for the tag.
    pub origin: Origin,
}

/// One suspension point.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PointRow {
    /// The `Expr::Await`.
    pub expr: u32,
    /// The tag stored before the poll returns `Poll::Pending` here.
    pub tag: Tag,
    /// The field of the future tmp, polled in place.
    pub future: FieldId,
    /// Its cancel order, a range of the plan's cancel table.
    cancel: Span32,
}

/// One drop of a cancel: the field, and the flag field that says whether it is initialised
/// (`None`: always, the point's own future tmp).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CancelDrop {
    pub field: FieldId,
    pub flag: Option<FieldId>,
}

/// What a poll does for a tag: the head switch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resume {
    /// From the body's start.
    Start,
    /// From the head of this point's poll loop (the Reference's step 3).
    Point(PointId),
    /// The library's panic "`async fn` resumed after completion".
    Completed,
    /// The library's panic "`async fn` resumed after panicking".
    Panicked,
    /// Not a tag of this coroutine: a corrupt state.
    NoSuchTag,
}

/// A recording no plan is made of, named (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlanError {
    /// The transform refused the recorded body.
    Transform(TransformError),
    /// A local a cancel at `point` drops has no slot in the state.
    CancelNotSaved { point: PointId, local: LocalId },
    /// The future tmp of `point` has no slot in the state: it could not be polled in place.
    FutureNotSaved(PointId),
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            PlanError::Transform(e) => write!(f, "{e}"),
            PlanError::CancelNotSaved { point, local } => {
                write!(f, "a cancel at {point} drops {local}, which the state does not save")
            }
            PlanError::FutureNotSaved(point) => write!(f, "the future of {point} is not saved in the state"),
        }
    }
}

/// What the builder receives for one coroutine (module doc).
pub struct Plan<A: Allocator> {
    fields: Vec<Field, A>,
    /// Per local: its home in the state.
    home: Vec<Option<FieldId>, A>,
    /// Per local: its flag field.
    flag: Vec<Option<FieldId>, A>,
    points: Vec<PointRow, A>,
    cancels: Vec<CancelDrop, A>,
    unresumed: Vec<FieldId, A>,
    tag: FieldId,
    upvars: u32,
}

impl<A: Allocator> Plan<A> {
    /// The state's fields, in order.
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// The number of upvars: fields `0..upvars`.
    pub fn upvars(&self) -> u32 {
        self.upvars
    }

    /// The tag's field, the last.
    pub fn tag(&self) -> FieldId {
        self.tag
    }

    /// Where a saved local lives; `None`: an ordinary local of the poll.
    pub fn home_of(&self, l: LocalId) -> Option<FieldId> {
        self.home.get(l.index()).copied().flatten()
    }

    /// The flag field of a local some cancel drops.
    pub fn flag_of(&self, l: LocalId) -> Option<FieldId> {
        self.flag.get(l.index()).copied().flatten()
    }

    pub fn points(&self) -> &[PointRow] {
        &self.points
    }

    /// The head switch.
    pub fn resume(&self, tag: Tag) -> Resume {
        if tag == Tag::UNRESUMED {
            return Resume::Start;
        }
        if tag == Tag::RETURNED {
            return Resume::Completed;
        }
        if tag == Tag::PANICKED {
            return Resume::Panicked;
        }
        match tag.as_point(self.points.len() as u32) {
            Some(p) => Resume::Point(p),
            None => Resume::NoSuchTag,
        }
    }

    /// What dropping a coroutine parked at `point` drops, first dropped first.
    pub fn cancel(&self, point: PointId) -> &[CancelDrop] {
        match self.points.get(point.index()) {
            Some(row) => &self.cancels[row.cancel.range()],
            None => &[],
        }
    }

    /// What dropping a coroutine never polled drops: the upvars whose types need drop, in
    /// declaration order (decision D5).
    pub fn unresumed(&self) -> &[FieldId] {
        &self.unresumed
    }

    /// The plan's text: the same plan gives the same text on any machine.
    pub fn print(&self, out: &mut impl fmt::Write) -> fmt::Result {
        for (i, f) in self.fields.iter().enumerate() {
            match (f.kind, f.local) {
                (FieldKind::Upvar | FieldKind::Saved, Some(l)) => writeln!(out, "field {i}: {} ({l})", f.origin)?,
                (FieldKind::Flag, Some(l)) => writeln!(out, "field {i}: flag of {l}")?,
                _ => writeln!(out, "field {i}: tag")?,
            }
        }
        for (k, p) in self.points.iter().enumerate() {
            write!(out, "point s{k}: await {} tag {} future field {} cancel:", p.expr, p.tag.0, p.future.0)?;
            for (j, c) in self.cancels[p.cancel.range()].iter().enumerate() {
                if j > 0 {
                    write!(out, ",")?;
                }
                write!(out, " field {}", c.field.0)?;
                if let Some(flag) = c.flag {
                    write!(out, " if field {}", flag.0)?;
                }
            }
            writeln!(out)?;
        }
        write!(out, "unresumed:")?;
        for f in self.unresumed.iter() {
            write!(out, " field {}", f.0)?;
        }
        writeln!(out)
    }
}

/// The plan of a recorded body: the transform's layout and its checks (decision D7), read back
/// by what each local is.
pub fn plan<A: Allocator, B: Allocator + Clone>(lowered: &Lowered<A>, alloc: B) -> Result<Plan<B>, PlanError> {
    let machine = transform(&lowered.body, alloc.clone()).map_err(PlanError::Transform)?;
    let layout = machine.layout();
    let locals = lowered.body.locals.len();
    let upvars = lowered.body.upvars;
    let mut fields = Vec::new_in(alloc.clone());
    let mut home = Vec::with_capacity_in(locals, alloc.clone());
    let mut flag = Vec::with_capacity_in(locals, alloc.clone());
    for _ in 0..locals {
        home.push(None);
        flag.push(None);
    }
    // The upvars, then the saved locals in declaration order: the layout's slots.
    for &l in layout.slots.iter() {
        let kind = if l.0 < upvars { FieldKind::Upvar } else { FieldKind::Saved };
        home[l.index()] = Some(FieldId(fields.len() as u32));
        fields.push(Field { kind, local: Some(l), origin: lowered.local_of[l.index()] });
    }
    // A flag for every local some cancel drops, but a point's own future tmp. Marked first,
    // then given fields in the locals' order, so the order does not depend on the points'.
    let mut wants = Vec::with_capacity_in(locals, alloc.clone());
    for _ in 0..locals {
        wants.push(false);
    }
    for k in 0..lowered.point_of.len() {
        let point = PointId(k as u32);
        for &l in lowered.cancel(point) {
            if layout.home_of(l) == Home::Frame {
                return Err(PlanError::CancelNotSaved { point, local: l });
            }
            if l != lowered.future_of[k] {
                wants[l.index()] = true;
            }
        }
    }
    for l in 0..locals {
        if wants[l] {
            flag[l] = Some(FieldId(fields.len() as u32));
            let l = LocalId(l as u32);
            fields.push(Field { kind: FieldKind::Flag, local: Some(l), origin: lowered.local_of[l.index()] });
        }
    }
    let tag = FieldId(fields.len() as u32);
    fields.push(Field { kind: FieldKind::Tag, local: None, origin: Origin::Scratch });
    let mut points = Vec::with_capacity_in(lowered.point_of.len(), alloc.clone());
    let mut cancels = Vec::new_in(alloc.clone());
    for k in 0..lowered.point_of.len() {
        let point = PointId(k as u32);
        let future = home[lowered.future_of[k].index()].ok_or(PlanError::FutureNotSaved(point))?;
        let first = cancels.len() as u32;
        for &l in lowered.cancel(point) {
            let field = home[l.index()].ok_or(PlanError::CancelNotSaved { point, local: l })?;
            cancels.push(CancelDrop { field, flag: flag[l.index()] });
        }
        let cancel = Span32 { first, len: cancels.len() as u32 - first };
        points.push(PointRow { expr: lowered.point_of[k], tag: Tag::point(point), future, cancel });
    }
    let mut unresumed = Vec::new_in(alloc);
    for l in 0..upvars as usize {
        if lowered.body.locals[l].needs_drop
            && let Some(f) = home[l]
        {
            unresumed.push(f);
        }
    }
    Ok(Plan { fields, home, flag, points, cancels, unresumed, tag, upvars })
}
