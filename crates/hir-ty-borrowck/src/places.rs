//! The type of every place a body names, found once: each distinct local with a prefix of a
//! projection is a node with its parent, its last element and its `PlaceTy`, filled from the
//! parent's by one projection. A lookup is a hash probe per element where projecting again
//! would intern field types and may normalize.

use hir_ty::mir::{LocalId, MirBody, PlaceElem, PlaceTy};

use crate::mem::{FMap, FVec, Frame, filled, map};

/// A place of the body: its local's id below the number of locals, a projection above.
pub(crate) type PlaceId = u32;

const NONE: u32 = u32::MAX;

pub(crate) struct Places<'f, 'db> {
    ty: FVec<'f, PlaceTy<'db>>,
    parent: FVec<'f, u32>,
    local: FVec<'f, LocalId>,
    /// The element below its parent of each place but a local's, by id less the locals.
    elem: FVec<'f, PlaceElem>,
    roots: usize,
    steps: FMap<'f, (u32, PlaceElem), u32>,
}

impl<'f, 'db> Places<'f, 'db> {
    pub(crate) fn new(frame: &'f Frame, body: &MirBody<'db>) -> Self {
        let roots = body.locals.len();
        let mut ty = Vec::with_capacity_in(roots * 2, frame);
        ty.extend(body.locals.iter().map(|(_, it)| PlaceTy::from_ty(it.ty.as_ref())));
        let mut local = Vec::with_capacity_in(roots * 2, frame);
        local.extend(body.locals.iter().map(|(id, _)| id));
        let mut parent = filled(frame, roots, NONE);
        parent.reserve(roots);
        Places {
            ty,
            parent,
            local,
            elem: Vec::with_capacity_in(roots, frame),
            roots,
            steps: map(frame),
        }
    }

    /// The place `local.projection`, made with each prefix it has not met yet, a new one's type
    /// `project(parent's type, element)`.
    pub(crate) fn intern(
        &mut self,
        local: LocalId,
        projection: &[PlaceElem],
        mut project: impl FnMut(PlaceTy<'db>, &PlaceElem) -> PlaceTy<'db>,
    ) -> PlaceId {
        let mut at = local.into_raw().into_u32();
        for elem in projection {
            at = match self.steps.get(&(at, *elem)) {
                Some(&next) => next,
                None => {
                    let next = self.ty.len() as u32;
                    let ty = project(self.ty[at as usize], elem);
                    self.ty.push(ty);
                    self.parent.push(at);
                    self.local.push(local);
                    self.elem.push(*elem);
                    self.steps.insert((at, *elem), next);
                    next
                }
            };
        }
        at
    }

    pub(crate) fn ty(&self, place: PlaceId) -> PlaceTy<'db> {
        self.ty[place as usize]
    }

    pub(crate) fn local(&self, place: PlaceId) -> LocalId {
        self.local[place as usize]
    }

    pub(crate) fn parent(&self, place: PlaceId) -> Option<PlaceId> {
        let parent = self.parent[place as usize];
        (parent != NONE).then_some(parent)
    }

    /// The last element of `place`'s projection; `None` for a local.
    pub(crate) fn elem(&self, place: PlaceId) -> Option<&PlaceElem> {
        (place as usize).checked_sub(self.roots).map(|at| &self.elem[at])
    }

    /// The places from `place`'s local down to it, root first, into `out` (emptied first).
    pub(crate) fn chain(&self, place: PlaceId, out: &mut FVec<'_, PlaceId>) {
        out.clear();
        let mut at = place;
        loop {
            out.push(at);
            match self.parent(at) {
                Some(up) => at = up,
                None => break,
            }
        }
        out.reverse();
    }

    pub(crate) fn len(&self) -> usize {
        self.ty.len()
    }
}
