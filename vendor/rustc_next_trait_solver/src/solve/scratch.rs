//! Buffers the solver uses inside one goal, kept in the search graph between goals and by the
//! delegate between root goals ([`SolverDelegate::obtain_search_storage`]): each is taken
//! empty and given back emptied, so a warm worker allocates none of them.
//!
//! [`SolverDelegate::obtain_search_storage`]: crate::delegate::SolverDelegate::obtain_search_storage

use derive_where::derive_where;
use rustc_type_ir::data_structures::HashSet;
use rustc_type_ir::search_graph::SearchGraphStorage;
use rustc_type_ir::{self as ty, Interner};

use crate::solve::assembly::Candidate;
use crate::solve::{Goal, GoalSource, GoalStalledOn, VisibleForLeakCheck};

type NestedGoal<I> =
    (GoalSource, Goal<I, <I as Interner>::Predicate>, Option<GoalStalledOn<I>>);
type RegionConstraint<I> = (ty::RegionConstraint<I>, VisibleForLeakCheck);
type OpaqueType<I> = (ty::OpaqueTypeKey<I>, <I as Interner>::Ty);
type NormalizationGoal<I> = (GoalSource, Goal<I, <I as Interner>::Predicate>);

/// A pool of emptied buffers per kind, as deep as goals nest (at most the recursion limit).
#[derive_where(Default; I: Interner)]
pub struct SolverScratch<I: Interner> {
    nested_goals: Vec<Vec<NestedGoal<I>>>,
    candidates: Vec<Vec<Candidate<I>>>,
    args: Vec<Vec<I::GenericArg>>,
    opt_args: Vec<Vec<Option<I::GenericArg>>>,
    universes: Vec<Vec<Option<ty::UniverseIndex>>>,
    seen_outlives: Vec<HashSet<ty::RegionConstraint<I>>>,
    region_vars: Vec<HashSet<ty::RegionVid>>,
    region_constraints: Vec<Vec<RegionConstraint<I>>>,
    opaque_types: Vec<Vec<OpaqueType<I>>>,
    normalization_goals: Vec<Vec<NormalizationGoal<I>>>,
}

macro_rules! pool {
    ($take:ident, $give:ident, $field:ident: $ty:ty) => {
        pub(super) fn $take(&mut self) -> $ty {
            self.$field.pop().unwrap_or_default()
        }

        /// Takes back a buffer, emptied and grown to the largest its pool holds: the pool hands
        /// buffers out last in, first out, so without it a goal may get one smaller than a
        /// sibling outgrew and grow it again each run. One that never grew is kept too once
        /// its pool holds room: a step swaps the list it evaluates for a spare, so a grown
        /// list goes back from a shallower level than took it, and dropping the spare would
        /// leave the pool a list short at the depth that grew one, one level more each run.
        pub(super) fn $give(&mut self, mut buffer: $ty) {
            buffer.clear();
            let largest = self.$field.iter().map(|it| it.capacity()).max().unwrap_or(0);
            // alloc: once per buffer below its pool's largest, while warming.
            buffer.reserve(largest);
            if buffer.capacity() != 0 {
                self.$field.push(buffer);
            }
        }
    };
}

/// Bytes a pool of buffers holds room for, the pool's own slots included.
fn held_in<B>(pool: &Vec<B>, capacity: impl Fn(&B) -> usize, element: usize) -> usize {
    pool.capacity() * size_of::<B>() + pool.iter().map(capacity).sum::<usize>() * element
}

impl<I: Interner> SolverScratch<I> {
    pool!(take_nested_goals, give_nested_goals, nested_goals: Vec<NestedGoal<I>>);
    pool!(take_candidates, give_candidates, candidates: Vec<Candidate<I>>);
    pool!(take_args, give_args, args: Vec<I::GenericArg>);
    pool!(take_opt_args, give_opt_args, opt_args: Vec<Option<I::GenericArg>>);
    pool!(take_universes, give_universes, universes: Vec<Option<ty::UniverseIndex>>);
    pool!(take_seen_outlives, give_seen_outlives, seen_outlives: HashSet<ty::RegionConstraint<I>>);
    pool!(take_region_vars, give_region_vars, region_vars: HashSet<ty::RegionVid>);
    pool!(
        take_region_constraints,
        give_region_constraints,
        region_constraints: Vec<RegionConstraint<I>>
    );
    pool!(take_opaque_types, give_opaque_types, opaque_types: Vec<OpaqueType<I>>);
    pool!(
        take_normalization_goals,
        give_normalization_goals,
        normalization_goals: Vec<NormalizationGoal<I>>
    );

    /// Bytes it holds room for: what a shelf keeps the largest of. A set's slot also has its
    /// control byte.
    pub fn held(&self) -> usize {
        held_in(&self.nested_goals, Vec::capacity, size_of::<NestedGoal<I>>())
            + held_in(&self.candidates, Vec::capacity, size_of::<Candidate<I>>())
            + held_in(&self.args, Vec::capacity, size_of::<I::GenericArg>())
            + held_in(&self.opt_args, Vec::capacity, size_of::<Option<I::GenericArg>>())
            + held_in(&self.universes, Vec::capacity, size_of::<Option<ty::UniverseIndex>>())
            + held_in(
                &self.seen_outlives,
                HashSet::capacity,
                size_of::<ty::RegionConstraint<I>>() + 1,
            )
            + held_in(&self.region_vars, HashSet::capacity, size_of::<ty::RegionVid>() + 1)
            + held_in(&self.region_constraints, Vec::capacity, size_of::<RegionConstraint<I>>())
            + held_in(&self.opaque_types, Vec::capacity, size_of::<OpaqueType<I>>())
            + held_in(
                &self.normalization_goals,
                Vec::capacity,
                size_of::<NormalizationGoal<I>>(),
            )
    }
}

/// The search graph's storage for one root goal and the goals nested in it.
pub type SearchStorage<I> = SearchGraphStorage<I, SolverScratch<I>>;

#[cfg(test)]
mod tests {
    use super::held_in;

    /// The pools' discipline over a plain buffer type: `SolverScratch` needs an `Interner`,
    /// which this crate has no test double of, and each of its pools is this macro.
    #[derive(Default)]
    struct Pool {
        buffers: Vec<Vec<u64>>,
    }

    impl Pool {
        pool!(take, give, buffers: Vec<u64>);
    }

    #[test]
    fn a_given_buffer_comes_back_with_its_capacity() {
        let mut pool = Pool::default();
        let mut buffer = pool.take();
        buffer.extend(0..100);
        let capacity = buffer.capacity();
        pool.give(buffer);
        let buffer = pool.take();
        assert!(buffer.is_empty());
        assert_eq!(buffer.capacity(), capacity);
        assert!(pool.buffers.is_empty());
    }

    #[test]
    fn a_buffer_that_never_grew_is_not_kept() {
        let mut pool = Pool::default();
        let buffer = pool.take();
        pool.give(buffer);
        assert!(pool.buffers.is_empty());
    }

    #[test]
    fn a_warm_take_push_give_allocates_nothing() {
        let mut pool = Pool::default();
        let mut buffer = pool.take();
        buffer.extend(0..64);
        pool.give(buffer);
        blaze_alloc_count::assert_at_most(0, "a warm scratch buffer", || {
            for _ in 0..10 {
                let mut buffer = pool.take();
                buffer.extend(0..64);
                pool.give(buffer);
            }
        });
    }

    /// Two buffers of different sizes, taken in either order once warm, never grow: each given
    /// back is raised to the larger.
    #[test]
    fn warm_buffers_hold_the_largest_any_needed() {
        let mut pool = Pool::default();
        let (mut big, mut small) = (pool.take(), pool.take());
        big.extend(0..100);
        small.extend(0..10);
        pool.give(big);
        pool.give(small);
        let (first, second) = (pool.take(), pool.take());
        pool.give(first);
        pool.give(second);
        blaze_alloc_count::assert_at_most(0, "buffers raised to the largest", || {
            let (mut first, mut second) = (pool.take(), pool.take());
            first.extend(0..100);
            second.extend(0..100);
            pool.give(second);
            pool.give(first);
        });
    }

    /// The solver's shape: an outer list, an inner one that grows, and a step that evaluates
    /// the inner list after swapping it for a spare, so the grown list goes back first.
    fn swapped_run(pool: &mut Pool) {
        let outer = pool.take();
        let mut inner = pool.take();
        inner.extend(0..64);
        let spare = pool.take();
        let evaluated = std::mem::replace(&mut inner, spare);
        pool.give(evaluated);
        pool.give(inner);
        pool.give(outer);
    }

    #[test]
    fn lists_swapped_between_levels_are_warm_after_one_run() {
        let mut pool = Pool::default();
        swapped_run(&mut pool);
        blaze_alloc_count::assert_at_most(0, "lists swapped between levels", || {
            swapped_run(&mut pool);
            swapped_run(&mut pool);
        });
        assert_eq!(pool.buffers.len(), 3);
    }

    #[test]
    fn held_counts_slots_and_room() {
        let pool = vec![Vec::<u64>::with_capacity(10), Vec::with_capacity(6)];
        let room: usize = pool.iter().map(Vec::capacity).sum();
        assert!(room >= 16);
        assert_eq!(
            held_in(&pool, Vec::capacity, size_of::<u64>()),
            pool.capacity() * size_of::<Vec<u64>>() + room * size_of::<u64>()
        );
    }
}
