//! Flecs 4.2-inspired Relational Entity-Component-System (pure Rust, zero dependencies).
//!
//! Core Flecs concepts modeled natively:
//!   - `Entity(u32)` lightweight handles for Threads, Functions (Slots), CpuCores, HeapSites, Builds.
//!   - `Column<T>`: sparse-set backed dense SoA component storage (O(1) lookup, contiguous cache-line iteration).
//!   - `RelStore<T>`: first-class entity relationships `Pair(subject, Rel, target)` indexed both
//!     forward (`subject -> [(target, T)]`) and reverse (`target -> [(subject, T)]`), enabling
//!     O(1) Code Map call-graph traversal (`Calls`), thread spawn trees (`SpawnedBy`),
//!     and lock contention dependency arrows (`BlockedBy`).
//!   - Ordered pipeline phases (`OnIngest` -> `OnAnalyze` -> `OnLayout` -> `OnRender`).

use std::collections::HashMap;

/// Lightweight generational-style entity ID (0 = invalid / null entity).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Entity(pub u32);

impl Entity {
    pub const NULL: Self = Self(0);
    #[inline]
    pub fn is_valid(self) -> bool {
        self.0 != 0
    }
}

/// First-class relationship kinds (Flecs `Pair(Rel, Target)`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Rel {
    /// `caller_fn -(Calls)-> callee_fn` (payload: call count + cumulative ns)
    Calls,
    /// `child_thread -(SpawnedBy)-> parent_thread`
    SpawnedBy,
    /// `waiter_thread -(BlockedBy)-> holder_thread`
    BlockedBy,
    /// `thread -(RunsOn)-> cpu_core`
    RunsOn,
}

/// Dense SoA component column backed by a sparse-set index for O(1) lookup and contiguous iteration.
pub struct Column<T> {
    entities: Vec<Entity>,
    data: Vec<T>,
    sparse: Vec<u32>,
}

const NONE_IDX: u32 = u32::MAX;

impl<T> Default for Column<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Column<T> {
    pub fn new() -> Self {
        Self {
            entities: Vec::new(),
            data: Vec::new(),
            sparse: Vec::new(),
        }
    }

    pub fn clear(&mut self) {
        self.entities.clear();
        self.data.clear();
        self.sparse.fill(NONE_IDX);
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn insert(&mut self, e: Entity, value: T) -> &mut T {
        let id = e.0 as usize;
        if id >= self.sparse.len() {
            self.sparse.resize(id + 1, NONE_IDX);
        }
        let slot = self.sparse[id];
        if slot != NONE_IDX {
            let idx = slot as usize;
            self.data[idx] = value;
            &mut self.data[idx]
        } else {
            let idx = self.data.len();
            self.entities.push(e);
            self.data.push(value);
            self.sparse[id] = idx as u32;
            &mut self.data[idx]
        }
    }

    #[inline]
    pub fn get(&self, e: Entity) -> Option<&T> {
        let id = e.0 as usize;
        let idx = *self.sparse.get(id)?;
        if idx == NONE_IDX {
            None
        } else {
            self.data.get(idx as usize)
        }
    }

    #[inline]
    pub fn get_mut(&mut self, e: Entity) -> Option<&mut T> {
        let id = e.0 as usize;
        let idx = *self.sparse.get(id)?;
        if idx == NONE_IDX {
            None
        } else {
            self.data.get_mut(idx as usize)
        }
    }

    pub fn get_or_insert_with(&mut self, e: Entity, f: impl FnOnce() -> T) -> &mut T {
        if self.get(e).is_some() {
            // SAFETY: checked immediately above.
            return self.get_mut(e).unwrap();
        }
        self.insert(e, f())
    }

    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = (Entity, &T)> {
        self.entities.iter().copied().zip(self.data.iter())
    }

    #[inline]
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (Entity, &mut T)> {
        self.entities.iter().copied().zip(self.data.iter_mut())
    }
}

/// Edge payload for weighted Flecs relationships (e.g., `(Calls, callee)` -> `CallEdge`).
#[derive(Clone, Copy, Default, Debug)]
pub struct RelEdge {
    pub count: u64,
    pub total_ns: u64,
    pub last_ts_ms: f64,
}

/// Bi-directional indexed relationship table (`(subject, rel, target) -> RelEdge`).
#[derive(Default)]
pub struct RelGraph {
    forward: HashMap<(Entity, Rel), Vec<(Entity, RelEdge)>>,
    reverse: HashMap<(Entity, Rel), Vec<(Entity, RelEdge)>>,
}

impl RelGraph {
    pub fn clear(&mut self) {
        self.forward.clear();
        self.reverse.clear();
    }

    pub fn add_edge(&mut self, from: Entity, rel: Rel, to: Entity, count_delta: u64, ns_delta: u64, ts_ms: f64) {
        if !from.is_valid() || !to.is_valid() || from == to {
            return;
        }
        let fwd = self.forward.entry((from, rel)).or_default();
        if let Some((_, edge)) = fwd.iter_mut().find(|(t, _)| *t == to) {
            edge.count = edge.count.saturating_add(count_delta);
            edge.total_ns = edge.total_ns.saturating_add(ns_delta);
            edge.last_ts_ms = ts_ms;
        } else {
            fwd.push((
                to,
                RelEdge {
                    count: count_delta,
                    total_ns: ns_delta,
                    last_ts_ms: ts_ms,
                },
            ));
        }

        let rev = self.reverse.entry((to, rel)).or_default();
        if let Some((_, edge)) = rev.iter_mut().find(|(s, _)| *s == from) {
            edge.count = edge.count.saturating_add(count_delta);
            edge.total_ns = edge.total_ns.saturating_add(ns_delta);
            edge.last_ts_ms = ts_ms;
        } else {
            rev.push((
                from,
                RelEdge {
                    count: count_delta,
                    total_ns: ns_delta,
                    last_ts_ms: ts_ms,
                },
            ));
        }
    }

    /// Flecs-style outgoing query: all `(target, edge)` where `from -(rel)-> target`.
    #[inline]
    pub fn targets(&self, from: Entity, rel: Rel) -> &[(Entity, RelEdge)] {
        self.forward
            .get(&(from, rel))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Flecs-style incoming query: all `(source, edge)` where `source -(rel)-> to`.
    #[inline]
    pub fn sources(&self, rel: Rel, to: Entity) -> &[(Entity, RelEdge)] {
        self.reverse
            .get(&(to, rel))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Iterate all edges of a specific relationship kind.
    pub fn iter_rel(&self, rel: Rel) -> impl Iterator<Item = (Entity, Entity, RelEdge)> + '_ {
        self.forward.iter().filter(move |((_, r), _)| *r == rel).flat_map(|((from, _), list)| {
            list.iter().map(move |(to, edge)| (*from, *to, *edge))
        })
    }
}

/// Entity allocator for the ECS World.
pub struct EntityAlloc {
    next_id: u32,
}

impl Default for EntityAlloc {
    fn default() -> Self {
        Self { next_id: 1 }
    }
}

impl EntityAlloc {
    pub fn spawn(&mut self) -> Entity {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        Entity(id)
    }

    pub fn reset(&mut self) {
        self.next_id = 1;
    }
}
