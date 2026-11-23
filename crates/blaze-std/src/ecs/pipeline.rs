//! Phased Flecs 4.2 system pipeline and deterministic conflict-free parallel batch scheduler.

use core::alloc::Allocator;

use blaze_own::Vec as OwnVec;

use super::error::EcsError;
use super::fetch::QueryFetch;
use super::id::Id;
use super::world::World;

/// Canonical Flecs 4.2 execution phases in deterministic tick order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Phase {
    OnLoad,
    PostLoad,
    PreUpdate,
    OnUpdate,
    OnValidate,
    PostUpdate,
    PreStore,
    OnStore,
}

impl Phase {
    /// All eight canonical phases in exact execution order.
    pub const ALL: [Self; 8] = [
        Self::OnLoad,
        Self::PostLoad,
        Self::PreUpdate,
        Self::OnUpdate,
        Self::OnValidate,
        Self::PostUpdate,
        Self::PreStore,
        Self::OnStore,
    ];

    /// Human-readable phase name.
    #[inline]
    pub const fn name(self) -> &'static str {
        match self {
            Self::OnLoad => "OnLoad",
            Self::PostLoad => "PostLoad",
            Self::PreUpdate => "PreUpdate",
            Self::OnUpdate => "OnUpdate",
            Self::OnValidate => "OnValidate",
            Self::PostUpdate => "PostUpdate",
            Self::PreStore => "PreStore",
            Self::OnStore => "OnStore",
        }
    }
}

/// System execution function operating on a `World`.
pub type SystemFn<A> = fn(&mut World<A>) -> Result<(), EcsError>;

/// System descriptor declaring its phase, read/write [`Id`] sets, and step function.
pub struct SystemDesc<A: Allocator + Copy + 'static> {
    pub name: &'static str,
    pub phase: Phase,
    pub reads: OwnVec<Id, A>,
    pub writes: OwnVec<Id, A>,
    pub step: SystemFn<A>,
}

impl<A: Allocator + Copy + 'static> SystemDesc<A> {
    /// Construct a new system descriptor in `alloc`.
    #[inline]
    pub fn new_in(name: &'static str, phase: Phase, step: SystemFn<A>, alloc: A) -> Self {
        Self {
            name,
            phase,
            reads: OwnVec::new_in(alloc),
            writes: OwnVec::new_in(alloc),
            step,
        }
    }

    /// Declare that this system reads `id`.
    pub fn read(mut self, id: Id) -> Result<Self, EcsError> {
        self.reads.try_push(id).map_err(|r| EcsError::Alloc(r.why))?;
        Ok(self)
    }

    /// Declare that this system writes `id`.
    pub fn write(mut self, id: Id) -> Result<Self, EcsError> {
        self.writes.try_push(id).map_err(|r| EcsError::Alloc(r.why))?;
        Ok(self)
    }

    /// Validate that this system does not declare self-conflicting access (duplicate write of the
    /// same ID or reading and writing the same ID separately).
    pub fn validate(&self) -> Result<(), EcsError> {
        let writes = self.writes.as_slice();
        for (i, &w) in writes.iter().enumerate() {
            if let Some(rest) = writes.get(i + 1..) {
                if rest.contains(&w) {
                    return Err(EcsError::SystemAccessConflict { system: self.name, id: w });
                }
            }
            if self.reads.as_slice().contains(&w) {
                return Err(EcsError::SystemAccessConflict { system: self.name, id: w });
            }
        }
        Ok(())
    }

    /// Return `true` iff `self` and `other` have zero read/write or write/write conflicts and may
    /// safely execute in parallel within the same batch.
    pub fn access_disjoint(&self, other: &Self) -> bool {
        let self_w = self.writes.as_slice();
        let self_r = self.reads.as_slice();
        let other_w = other.writes.as_slice();
        let other_r = other.reads.as_slice();
        for &w in self_w {
            if other_w.contains(&w) || other_r.contains(&w) {
                return false;
            }
        }
        for &w in other_w {
            if self_r.contains(&w) {
                return false;
            }
        }
        true
    }
}

/// One conflict-free batch of system indices (into `Pipeline::systems`) that can run in parallel.
pub struct Batch<A: Allocator + Copy> {
    pub systems: OwnVec<usize, A>,
}

/// Scheduled batches for a single [`Phase`].
pub struct PhaseSchedule<A: Allocator + Copy> {
    pub phase: Phase,
    pub batches: OwnVec<Batch<A>, A>,
}

/// Complete phased schedule returned by [`Pipeline::batches`].
pub struct Batches<A: Allocator + Copy> {
    pub phases: OwnVec<PhaseSchedule<A>, A>,
}

/// Phased Flecs 4.2 system pipeline with deterministic conflict-free batch partitioning.
pub struct Pipeline<A: Allocator + Copy + 'static> {
    systems: OwnVec<SystemDesc<A>, A>,
    alloc: A,
}

impl<A: Allocator + Copy + 'static> Pipeline<A> {
    /// Create an empty pipeline using `alloc`.
    #[inline]
    pub fn new_in(alloc: A) -> Self {
        Self { systems: OwnVec::new_in(alloc), alloc }
    }

    /// Number of registered systems.
    #[inline]
    pub fn len(&self) -> usize {
        self.systems.len()
    }

    /// Whether the pipeline has zero registered systems.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.systems.is_empty()
    }

    /// Access all registered systems.
    #[inline]
    pub fn systems(&self) -> &[SystemDesc<A>] {
        self.systems.as_slice()
    }

    /// Validate and register a system in the pipeline, returning its index.
    pub fn add_system(&mut self, desc: SystemDesc<A>) -> Result<usize, EcsError> {
        desc.validate()?;
        let idx = self.systems.len();
        self.systems.try_push(desc).map_err(|r| EcsError::Alloc(r.why))?;
        Ok(idx)
    }

    /// Register a typed query system in `phase`, automatically extracting its `reads` and `writes`
    /// sets via [`QueryFetch::register_access`] so disjoint batching requires zero manual access
    /// declarations.
    pub fn add_query<Q: QueryFetch>(
        &mut self,
        world: &mut World<A>,
        phase: Phase,
        name: &'static str,
        step: SystemFn<A>,
    ) -> Result<usize, EcsError> {
        let mut desc = SystemDesc::new_in(name, phase, step, self.alloc);
        Q::register_access(world, &mut desc.reads, &mut desc.writes)?;
        self.add_system(desc)
    }

    /// Partition every [`Phase`] into deterministic, order-preserving, pairwise conflict-free
    /// parallel batches.
    ///
    /// Within each phase, a system is placed into the earliest batch `b` such that:
    /// 1. `b` is strictly after every batch containing an earlier conflicting system (preserving
    ///    write/read dependency order), and
    /// 2. every system already in `b` satisfies [`SystemDesc::access_disjoint`].
    pub fn batches(&self) -> Result<Batches<A>, EcsError> {
        let mut phases = OwnVec::new_in(self.alloc);
        for phase in Phase::ALL {
            let mut phase_batches: OwnVec<Batch<A>, A> = OwnVec::new_in(self.alloc);
            for (sys_idx, sys) in self.systems.as_slice().iter().enumerate() {
                if sys.phase != phase {
                    continue;
                }
                // Find the latest batch containing a system that conflicts with `sys`.
                let mut earliest_allowed = 0usize;
                for (b_idx, batch) in phase_batches.as_slice().iter().enumerate() {
                    let mut conflicts = false;
                    for &other_idx in batch.systems.as_slice() {
                        if let Some(other) = self.systems.get(other_idx) {
                            if !sys.access_disjoint(other) {
                                conflicts = true;
                                break;
                            }
                        }
                    }
                    if conflicts {
                        earliest_allowed = b_idx + 1;
                    }
                }
                if let Some(batch) = phase_batches.get_mut(earliest_allowed) {
                    batch.systems.try_push(sys_idx).map_err(|r| EcsError::Alloc(r.why))?;
                } else {
                    let mut new_batch = OwnVec::new_in(self.alloc);
                    new_batch.try_push(sys_idx).map_err(|r| EcsError::Alloc(r.why))?;
                    phase_batches
                        .try_push(Batch { systems: new_batch })
                        .map_err(|r| EcsError::Alloc(r.why))?;
                }
            }
            if !phase_batches.is_empty() {
                phases
                    .try_push(PhaseSchedule { phase, batches: phase_batches })
                    .map_err(|r| EcsError::Alloc(r.why))?;
            }
        }
        Ok(Batches { phases })
    }

    /// Execute one full tick across all canonical phases and batches in deterministic order.
    pub fn run_tick(&self, world: &mut World<A>) -> Result<(), EcsError> {
        let schedule = self.batches()?;
        for phase_sched in schedule.phases.as_slice() {
            for batch in phase_sched.batches.as_slice() {
                for &sys_idx in batch.systems.as_slice() {
                    if let Some(sys) = self.systems.get(sys_idx) {
                        (sys.step)(world)?;
                    }
                }
            }
        }
        Ok(())
    }
}
