//! Systems and observers: a typed query run once per `progress` (a system) or on a change to what it matches (an observer),
//! its closure boxed into the flecs entity and freed by flecs when that entity goes. The closure runs inside hyperion-ecs's
//! trampoline, which catches a panic at the FFI edge (the host raises it again after `progress`; a module image is
//! quarantined). Everything a system touches through its views goes through the stage, so its writes land at the end of the
//! system, as in flecs-rust.
//!
//! Both builders take filter terms (`with`, `without`, `with_up`, `without_up`; see [`crate::query`]) after the fetched ones.
use crate::query::{Filters, QueryTuple, filter_methods};
use crate::{EntityView, IntoId, WorldRef};
use core::ffi::c_void;
use core::marker::PhantomData;
use hyperion_ecs::untyped::UntypedWorld as _;
use hyperion_ecs::{Id, Iter, sys};

/// The iterator a system's `each_iter` closure gets: the matched table's entities, the world and the frame's time step.
pub struct TableIter<'a, 'i> {
    it: &'a Iter<'i>,
}

impl<'a> TableIter<'a, '_> {
    #[must_use]
    pub fn entity(&self, row: usize) -> EntityView<'a> {
        EntityView::new(self.it.stage(), self.it.entities()[row])
    }

    #[must_use]
    pub fn world(&self) -> WorldRef<'a> {
        WorldRef::new(self.it.stage())
    }

    #[must_use]
    pub fn delta_time(&self) -> f32 {
        self.it.delta_time()
    }

    #[must_use]
    pub fn count(&self) -> usize {
        self.it.count()
    }

    /// In an observer, the event that fired it (compare with [`Event::id`]).
    #[must_use]
    pub fn event(&self) -> Id {
        self.it.raw.event
    }

    /// The target of the pair term `index` matched (fetched terms first, in `Q`'s order, then filters in the order they were
    /// added): which `(Rel, *)` an observer on a wildcard pair fired for. `None` when that term is not a pair.
    #[must_use]
    pub fn pair_target(&self, index: i8) -> Option<EntityView<'a>> {
        let id = self.it.field_id(index);
        // SAFETY: `ecs_id_is_pair` reads only the id; `ecs_get_alive` the world's entity index (flecs's `ecs_pair_second`).
        unsafe {
            if !sys::ecs_id_is_pair(id) {
                return None;
            }
            let t = sys::ecs_get_alive(self.it.raw.world, id & 0xFFFF_FFFF);
            (t != 0).then(|| EntityView::new(self.it.stage(), t))
        }
    }
}

/// What fires an observer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A term starts to match: a component, tag or pair was added.
    OnAdd,
    /// A component's value was set (after `OnAdd` for a new one).
    OnSet,
    /// A term stops matching: removed, or the entity deleted (also a pair removed by an `OnDeleteTarget Remove` cleanup).
    OnRemove,
}

impl Event {
    #[must_use]
    pub fn id(self) -> Id {
        // SAFETY: immutable flecs globals.
        unsafe {
            match self {
                Event::OnAdd => sys::EcsOnAdd,
                Event::OnSet => sys::EcsOnSet,
                Event::OnRemove => sys::EcsOnRemove,
            }
        }
    }
}

/// The pipeline phase a system runs in. flecs orders the systems of one phase by entity id, which is not the order they
/// were made in once ids are recycled (a scene load deletes entities), so a system that must see another's results in the
/// same `progress` goes in a later phase, and declares what it [`reads`](SystemBuilder::reads) so flecs merges first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// flecs's OnUpdate, the default: the model, scripts and views writing what layout reads.
    Update,
    /// OnValidate: the layout pass, after everything that sizes nodes.
    Layout,
    /// PostUpdate: what reads this pass's places (hit testing, matched geometry).
    AfterLayout,
    /// PreStore: what reads those (the pointer's topmost hit, from this pass's hits).
    Resolve,
    /// OnStore: the draw pass, last.
    Draw,
}

pub struct SystemBuilder<'a, Q> {
    w: hyperion_ecs::World,
    name: String,
    manual: bool,
    phase: Phase,
    filters: Filters,
    _q: crate::Marker<'a, Q>,
}

type EachIter<Q> = dyn FnMut(&TableIter<'_, '_>, usize, <Q as QueryTuple>::Out<'_>);

impl<'a, Q: QueryTuple + 'static> SystemBuilder<'a, Q> {
    pub(crate) fn new(w: hyperion_ecs::World, name: &str) -> Self {
        Self { w, name: name.to_owned(), manual: false, phase: Phase::Update, filters: Filters::default(), _q: PhantomData }
    }

    filter_methods!();

    /// Run in `phase` instead of the default ([`Phase::Update`]).
    #[must_use]
    pub fn phase(mut self, phase: Phase) -> Self {
        self.phase = phase;
        self
    }

    /// In no phase: `progress` never runs it, its owner does with [`WorldRef::run_system`](crate::WorldRef::run_system),
    /// at a rate of its own (den-sound's control rate, which goes on while the window draws nothing).
    #[must_use]
    pub fn manual(mut self) -> Self {
        self.manual = true;
        self
    }

    /// Tell flecs's scheduler this system writes `id` on entities it does not match (through the stage, landing at its
    /// end): a later system in the same `progress` that [`reads`](Self::reads) it, or matches it on its own entities, runs
    /// after a merge and sees this pass's values, not the last pass's. A no-source `[out] T()` term: it matches nothing.
    #[must_use]
    pub fn writes(mut self, id: impl IntoId) -> Self {
        let id = id.into_id(self.w);
        self.filters.must(crate::query::Term::from(id, sys::EcsIsEntity).write());
        self
    }

    /// Tell flecs's scheduler this system reads `id` from entities it does not match (`try_cloned` on a parent, an owner,
    /// a node found by id): it runs after a merge of any earlier system's [`writes`](Self::writes) of it in the same
    /// `progress`. A no-source `[in] T()` term: it matches nothing.
    #[must_use]
    pub fn reads(mut self, id: impl IntoId) -> Self {
        let id = id.into_id(self.w);
        self.filters.must(crate::query::Term::from(id, sys::EcsIsEntity).read());
        self
    }

    /// Run `f` for every matched entity.
    pub fn each_entity(self, mut f: impl FnMut(EntityView<'_>, Q::Out<'_>) + 'static) -> Id {
        self.each_iter(move |it, row, v| f(it.entity(row), v))
    }

    /// Run `f` for every matched row, with the iterator (its world, time step and entities).
    pub fn each_iter(self, f: impl FnMut(&TableIter<'_, '_>, usize, Q::Out<'_>) + 'static) -> Id {
        let terms = match Q::terms(self.w) {
            Some(t) if !self.filters.missing => t,
            _ => panic!("system {}: a component of its query is not registered", self.name),
        };
        let boxed: Box<Box<EachIter<Q>>> = Box::new(Box::new(f));
        let b = self.w.system(&self.name);
        // SAFETY: immutable flecs globals.
        let phase = unsafe {
            match self.phase {
                Phase::Update => sys::EcsOnUpdate,
                Phase::Layout => sys::EcsOnValidate,
                Phase::AfterLayout => sys::EcsPostUpdate,
                Phase::Resolve => sys::EcsPreStore,
                Phase::Draw => sys::EcsOnStore,
            }
        };
        let b = b.phase(if self.manual { 0 } else { phase });
        let b = terms.into_iter().chain(self.filters.terms).fold(b, |b, t| b.term(t));
        b.user_owned(Box::into_raw(boxed).cast(), free::<Q>).build(run::<Q>).unwrap_or_else(|e| panic!("{e}"))
    }
}

/// An observer: `f` runs when an [`Event`] happens to an entity its terms match, inside the change (not once per frame).
/// Made in a module's install, it lives in the module's scope and goes with it; [`yield_existing`](Self::yield_existing)
/// makes a swapped-in observer catch up with what already matches.
pub struct ObserverBuilder<'a, Q> {
    w: hyperion_ecs::World,
    name: String,
    events: Vec<Event>,
    yield_existing: bool,
    filters: Filters,
    _q: crate::Marker<'a, Q>,
}

impl<'a, Q: QueryTuple + 'static> ObserverBuilder<'a, Q> {
    pub(crate) fn new(w: hyperion_ecs::World, name: &str) -> Self {
        Self { w, name: name.to_owned(), events: Vec::new(), yield_existing: false, filters: Filters::default(), _q: PhantomData }
    }

    filter_methods!();

    /// Match `id` without being fired by it: only the other terms' events fire the observer.
    #[must_use]
    pub fn filter(mut self, id: impl IntoId) -> Self {
        let id = id.into_id(self.w);
        self.filters.must(crate::query::Term::new(id).filter());
        self
    }

    /// Fire on `event` (several calls, several events).
    #[must_use]
    pub fn on(mut self, event: Event) -> Self {
        self.events.push(event);
        self
    }

    #[must_use]
    pub fn on_add(self) -> Self {
        self.on(Event::OnAdd)
    }

    #[must_use]
    pub fn on_set(self) -> Self {
        self.on(Event::OnSet)
    }

    #[must_use]
    pub fn on_remove(self) -> Self {
        self.on(Event::OnRemove)
    }

    /// Run once at creation for every entity that already matches, as if the event had just happened to it.
    #[must_use]
    pub fn yield_existing(mut self) -> Self {
        self.yield_existing = true;
        self
    }

    /// Run `f` for every entity the event happened to.
    pub fn each_entity(self, mut f: impl FnMut(EntityView<'_>, Q::Out<'_>) + 'static) -> Id {
        self.each_iter(move |it, row, v| f(it.entity(row), v))
    }

    /// Run `f` for every row the event happened to, with the iterator (its world, [`TableIter::event`] and
    /// [`TableIter::pair_target`]).
    pub fn each_iter(self, f: impl FnMut(&TableIter<'_, '_>, usize, Q::Out<'_>) + 'static) -> Id {
        let terms = match Q::terms(self.w) {
            Some(t) if !self.filters.missing => t,
            _ => panic!("observer {}: a component of its query is not registered", self.name),
        };
        assert!(!terms.is_empty() || !self.filters.terms.is_empty(), "observer {}: no terms", self.name);
        let boxed: Box<Box<EachIter<Q>>> = Box::new(Box::new(f));
        let b = self.events.iter().fold(self.w.observer(&self.name), |b, e| b.event(e.id()));
        let b = if self.yield_existing { b.yield_existing() } else { b };
        let b = terms.into_iter().chain(self.filters.terms).fold(b, |b, t| b.term(t));
        b.user_owned(Box::into_raw(boxed).cast(), free::<Q>).build(run::<Q>).unwrap_or_else(|e| panic!("{e}"))
    }
}

fn run<Q: QueryTuple + 'static>(it: &mut Iter) {
    let raw: *const sys::ecs_iter_t = &raw const *it.raw;
    // SAFETY: `user` is the `Box<Box<EachIter<Q>>>` this system owns (set in `each_iter`, freed by `free`), used on one thread.
    let f = unsafe { &mut *it.user().cast::<Box<EachIter<Q>>>() };
    let view = TableIter { it };
    for row in 0..view.count() {
        // SAFETY: `row` is in the table; the borrows end with this call.
        f(&view, row, unsafe { Q::fetch(raw, row) });
    }
}

/// # Safety
/// `p` is the system's boxed closure, freed once.
unsafe fn free<Q: QueryTuple + 'static>(p: *mut c_void) {
    // SAFETY: the caller's contract.
    drop(unsafe { Box::from_raw(p.cast::<Box<EachIter<Q>>>()) });
}
