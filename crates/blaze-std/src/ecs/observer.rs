//! Reactive Flecs-style observers (`OnAdd`, `OnSet`, `OnRemove`) supporting exact IDs and
//! wildcard pairs `(Rel, WILDCARD)` / `(WILDCARD, Target)`.
//!
//! Observers record events into a bounded ring/log (and optionally invoke a non-reentrant callback
//! taking only [`Event`]), guaranteeing zero recursion by construction.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{Box as OwnBox, Vec as OwnVec};

use super::error::EcsError;
use super::id::{Entity, Id};

/// Default per-observer bounded event buffer capacity (`bounded-progress`).
pub const DEFAULT_OBSERVER_CAP: usize = 4096;

/// Lifecycle event kind observed by an [`Observer`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EventKind {
    /// Fired when a component, tag, or pair is newly added to an entity's archetype.
    OnAdd,
    /// Fired when a component's value is written (both on initial `set` and in-place updates).
    OnSet,
    /// Fired when a component, tag, or pair is removed from an entity (or on `despawn`).
    OnRemove,
}

/// One reactive lifecycle notification emitted by a `World`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Event {
    pub kind: EventKind,
    pub entity: Entity,
    pub id: Id,
}

/// Typed observer handle returned by `World::observe*`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct ObserverId(pub u32);

impl fmt::Debug for ObserverId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Observer#{}", self.0)
    }
}

/// Non-reentrant observer callback invoked synchronously with the matching [`Event`].
pub type ObserverCallback<A> = OwnBox<dyn FnMut(Event), A>;

/// Registered observer matching a specific `(EventKind, Id)` pattern (with wildcard pair support).
pub(crate) struct Observer<A: Allocator + Copy> {
    pub(crate) kind: EventKind,
    pub(crate) filter: Id,
    pub(crate) max_events: usize,
    pub(crate) dropped_events: usize,
    pub(crate) records: OwnVec<Event, A>,
    pub(crate) callback: Option<ObserverCallback<A>>,
}

impl<A: Allocator + Copy> Observer<A> {
    pub(crate) fn new_in(
        kind: EventKind,
        filter: Id,
        max_events: usize,
        callback: Option<ObserverCallback<A>>,
        alloc: A,
    ) -> Self {
        Self {
            kind,
            filter,
            max_events,
            dropped_events: 0,
            records: OwnVec::new_in(alloc),
            callback,
        }
    }

    pub(crate) fn dispatch(&mut self, event: Event, wildcard: Entity) -> Result<(), EcsError> {
        if self.kind != event.kind || !event.id.matches_pattern(self.filter, wildcard) {
            return Ok(());
        }
        if self.records.len() < self.max_events {
            self.records.try_push(event).map_err(|r| EcsError::Alloc(r.why))?;
        } else {
            self.dropped_events = self.dropped_events.saturating_add(1);
        }
        if let Some(cb) = self.callback.as_mut() {
            cb(event);
        }
        Ok(())
    }
}
