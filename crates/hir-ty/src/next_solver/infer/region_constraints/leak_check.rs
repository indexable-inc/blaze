//! rustc's leak check (`rustc_infer::infer::region_constraints::leak_check`), on the constraint
//! columns, with Tarjan's algorithm for the strongly connected components.

use blaze_ids::radix;
use rustc_type_ir::error::TypeError;
use rustc_type_ir::inherent::IntoKind;
use rustc_type_ir::solve::VisibleForLeakCheck;
use rustc_type_ir::{RegionKind, UniverseIndex};
use tracing::{debug, instrument};

use super::RegionConstraintCollector;
use crate::next_solver::infer::relate::RelateResult;
use crate::next_solver::infer::snapshot::CombinedSnapshot;
use crate::next_solver::{DbInterner, FxIndexMap, PlaceholderRegion, Region};

impl<'db> RegionConstraintCollector<'db, '_> {
    /// Searches new universes created during `snapshot`, looking for
    /// placeholders that may "leak" out from the universes they are contained
    /// in. If any leaking placeholders are found, then an `Err` is returned
    /// (typically leading to the snapshot being reversed). This algorithm
    /// only looks at placeholders which cannot be named by `outer_universe`,
    /// as this is the universe we're currently checking for a leak.
    ///
    /// The leak checker works by searching for the following error patterns:
    ///
    /// * P1: P2, where P1 != P2
    /// * P1: R, where R is in some universe that cannot name P1
    ///
    /// First, we construct a graph where `R2: R1` implies `R2 -> R1`, and we compute the SCCs.
    ///
    /// For each SCC S, we compute:
    ///
    /// * what placeholder P it must be equal to, if any
    ///   * if there are multiple placeholders that must be equal, report an error because `P1: P2`
    /// * the minimum universe of its constituents
    ///
    /// Then we walk the SCCs in dependency order and compute
    ///
    /// * what placeholder they must outlive transitively
    ///   * if they must also be equal to a placeholder, report an error because `P1: P2`
    /// * minimum universe U of all SCCs they must outlive
    ///   * if they must also be equal to a placeholder P, and U cannot name P, report an error, as
    ///     that indicates `P: R` and `R` is in an incompatible universe
    ///
    /// With `only_consider_snapshot`, only the constraints added in that snapshot are looked at,
    /// so the result does not depend on constraints from outside an evaluation.
    #[instrument(level = "debug", skip(self, interner, only_consider_snapshot), ret)]
    pub fn leak_check(
        self,
        interner: DbInterner<'db>,
        outer_universe: UniverseIndex,
        max_universe: UniverseIndex,
        only_consider_snapshot: Option<&CombinedSnapshot>,
    ) -> RelateResult<'db, ()> {
        if outer_universe == max_universe {
            return Ok(());
        }

        let mini_graph = MiniGraph::new(interner, &self, only_consider_snapshot);

        let mut leak_check =
            LeakCheck::new(interner, outer_universe, max_universe, mini_graph, self);
        leak_check.assign_placeholder_values()?;
        leak_check.propagate_scc_value()?;
        Ok(())
    }
}

struct LeakCheck<'a, 'db> {
    interner: DbInterner<'db>,
    outer_universe: UniverseIndex,
    mini_graph: MiniGraph<'db>,
    rcc: RegionConstraintCollector<'db, 'a>,

    // Initially, for each SCC S, stores a placeholder `P` such that `S = P`
    // must hold.
    //
    // Later, during the [`LeakCheck::propagate_scc_value`] function, this array
    // is repurposed to store some placeholder `P` such that the weaker
    // condition `S: P` must hold. (This is true if `S: S1` transitively and `S1
    // = P`.)
    scc_placeholders: Vec<Option<PlaceholderRegion<'db>>>,

    // For each SCC S, track the minimum universe that flows into it. Note that
    // this is both the minimum of the universes for every region that is a
    // member of the SCC, but also if you have `R1: R2`, then the universe of
    // `R2` must be less than the universe of `R1` (i.e., `R1` flows `R2`). To
    // see that, imagine that you have `P1: R` -- in that case, `R` must be
    // either the placeholder `P1` or the empty region in that same universe.
    //
    // To detect errors, we look for an SCC S where the values in
    // `scc_placeholders[S]` (if any) cannot be stored into `scc_universes[S]`.
    scc_universes: Vec<SccUniverse<'db>>,
}

impl<'a, 'db> LeakCheck<'a, 'db> {
    fn new(
        interner: DbInterner<'db>,
        outer_universe: UniverseIndex,
        max_universe: UniverseIndex,
        mini_graph: MiniGraph<'db>,
        rcc: RegionConstraintCollector<'db, 'a>,
    ) -> Self {
        let dummy_scc_universe = SccUniverse { universe: max_universe, region: None };
        let num_sccs = mini_graph.sccs.num_sccs();
        Self {
            interner,
            outer_universe,
            mini_graph,
            rcc,
            // alloc: per leak check past a binder, sized to its region graph.
            scc_placeholders: vec![None; num_sccs],
            // alloc: per leak check past a binder, sized to its region graph.
            scc_universes: vec![dummy_scc_universe; num_sccs],
        }
    }

    /// Compute what placeholders (if any) each SCC must be equal to.
    /// Also compute the minimum universe of all the regions in each SCC.
    fn assign_placeholder_values(&mut self) -> RelateResult<'db, ()> {
        // First walk: find each placeholder that is from a newly created universe.
        for (&region, &leak_check_node) in &self.mini_graph.nodes {
            let scc = self.mini_graph.sccs.scc(leak_check_node);

            // Set the universe of each SCC to be the minimum of its constituent universes
            let universe = self.rcc.universe(region);
            debug!(
                "assign_placeholder_values: scc={:?} universe={:?} region={:?}",
                scc, universe, region
            );
            self.scc_universes[scc].take_min(universe, region);

            // Detect those SCCs that directly contain a placeholder
            if let RegionKind::RePlaceholder(placeholder) = region.kind()
                && self.outer_universe.cannot_name(placeholder.universe)
            {
                // Update `scc_placeholders` to account for the fact that `P: S` must hold.
                match self.scc_placeholders[scc] {
                    Some(p) => {
                        assert_ne!(p, placeholder);
                        return Err(self.placeholder_error(p, placeholder));
                    }
                    None => {
                        self.scc_placeholders[scc] = Some(placeholder);
                    }
                }
            }
        }

        Ok(())
    }

    /// Iterate over each SCC `S` and ensure that, for each `S1` where `S1: S`,
    /// `universe(S) <= universe(S1)`. This executes after
    /// `assign_placeholder_values`, so `universe(S)` is already the minimum
    /// universe of any of its direct constituents.
    fn propagate_scc_value(&mut self) -> RelateResult<'db, ()> {
        // Loop invariants:
        //
        // On start of the loop iteration for `scc1`:
        //
        // * `scc_universes[scc1]` contains the minimum universe of the
        //   constituents of `scc1`
        // * `scc_placeholder[scc1]` stores the placeholder that `scc1` must
        //   be equal to (if any)
        //
        // For each successor `scc2` where `scc1: scc2`:
        //
        // * `scc_placeholder[scc2]` stores some placeholder `P` where
        //   `scc2: P` (if any)
        // * `scc_universes[scc2]` contains the minimum universe of the
        //   constituents of `scc2` and any of its successors
        for scc1 in 0..self.mini_graph.sccs.num_sccs() {
            debug!(
                "propagate_scc_value: scc={:?} with universe {:?}",
                scc1, self.scc_universes[scc1]
            );

            // Walk over each `scc2` such that `scc1: scc2` and compute:
            //
            // * `scc1_universe`: the minimum universe of `scc2` and the constituents of `scc1`
            // * `succ_bound`: placeholder `P` that the successors must outlive, if any (if there
            //   are multiple, we pick one arbitrarily)
            let mut scc1_universe = self.scc_universes[scc1];
            let mut succ_bound = None;
            for &scc2 in self.mini_graph.sccs.successors(scc1) {
                let scc2 = scc2 as usize;
                let SccUniverse { universe: scc2_universe, region: scc2_region } =
                    self.scc_universes[scc2];

                scc1_universe.take_min(scc2_universe, scc2_region.unwrap());

                if let Some(b) = self.scc_placeholders[scc2] {
                    succ_bound = Some(b);
                }
            }

            // Update minimum universe of scc1.
            self.scc_universes[scc1] = scc1_universe;

            // At this point, `scc_placeholders[scc1]` stores the placeholder that
            // `scc1` must be equal to, if any.
            if let Some(scc1_placeholder) = self.scc_placeholders[scc1] {
                debug!(
                    "propagate_scc_value: scc1={:?} placeholder={:?} scc1_universe={:?}",
                    scc1, scc1_placeholder, scc1_universe
                );

                // Check if `P1: R` for some `R` in a universe that cannot name
                // P1. That's an error.
                if scc1_universe.universe.cannot_name(scc1_placeholder.universe) {
                    return Err(self.error(scc1_placeholder, scc1_universe.region.unwrap()));
                }

                // Check if we have some placeholder where `S: P2`
                // (transitively). In that case, since `S = P1`, that implies
                // `P1: P2`, which is an error condition.
                if let Some(scc2_placeholder) = succ_bound {
                    assert_ne!(scc1_placeholder, scc2_placeholder);
                    return Err(self.placeholder_error(scc1_placeholder, scc2_placeholder));
                }
            } else {
                // Otherwise, we can reach a placeholder if some successor can.
                self.scc_placeholders[scc1] = succ_bound;
            }

            // At this point, `scc_placeholder[scc1]` stores some placeholder that `scc1` must
            // outlive (if any).
        }
        Ok(())
    }

    fn placeholder_error(
        &self,
        placeholder1: PlaceholderRegion<'db>,
        placeholder2: PlaceholderRegion<'db>,
    ) -> TypeError<DbInterner<'db>> {
        self.error(placeholder1, Region::new_placeholder(self.interner, placeholder2))
    }

    fn error(
        &self,
        placeholder: PlaceholderRegion<'db>,
        other_region: Region<'db>,
    ) -> TypeError<DbInterner<'db>> {
        debug!("error: placeholder={:?}, other_region={:?}", placeholder, other_region);
        TypeError::RegionsInsufficientlyPolymorphic(placeholder.bound, other_region)
    }
}

/// Tracks the "minimum universe" for each SCC, along with some region that
/// caused it to change.
#[derive(Copy, Clone, Debug)]
struct SccUniverse<'db> {
    /// For some SCC S, the minimum universe of:
    ///
    /// * each region R in S
    /// * each SCC S1 such that S: S1
    universe: UniverseIndex,

    /// Some region that caused `universe` to be what it is.
    region: Option<Region<'db>>,
}

impl<'db> SccUniverse<'db> {
    /// If `universe` is less than our current universe, then update
    /// `self.universe` and `self.region`.
    fn take_min(&mut self, universe: UniverseIndex, region: Region<'db>) {
        if universe < self.universe || self.region.is_none() {
            self.universe = universe;
            self.region = Some(region);
        }
    }
}

/// Represents the graph of constraints. For each `R1: R2` constraint we create
/// an edge `R1 -> R2` in the graph.
struct MiniGraph<'db> {
    /// Map from a region to the index of the node in the graph.
    nodes: FxIndexMap<Region<'db>, u32>,

    /// Map from node index to SCC, and stores the successors of each SCC. All
    /// the regions in the same SCC are equal to one another, and if `S1 -> S2`,
    /// then `S1: S2`.
    sccs: Sccs,
}

impl<'db> MiniGraph<'db> {
    fn new(
        interner: DbInterner<'db>,
        region_constraints: &RegionConstraintCollector<'db, '_>,
        only_consider_snapshot: Option<&CombinedSnapshot>,
    ) -> Self {
        let mut nodes = FxIndexMap::default();
        let mut edges = Vec::with_capacity(region_constraints.legacy_data().constraints.len());

        // Note that if `R2: R1`, we get a callback `r1, r2`, so `target` is first parameter.
        Self::iterate_region_constraints(
            interner,
            region_constraints,
            only_consider_snapshot,
            |target, source| {
                let source_node = Self::add_node(&mut nodes, source);
                let target_node = Self::add_node(&mut nodes, target);
                edges.push((source_node, target_node));
            },
        );
        let sccs = Sccs::new(nodes.len(), edges);
        Self { nodes, sccs }
    }

    /// Invokes `each_edge(R1, R2)` for each edge where `R2: R1`
    fn iterate_region_constraints(
        interner: DbInterner<'db>,
        region_constraints: &RegionConstraintCollector<'db, '_>,
        only_consider_snapshot: Option<&CombinedSnapshot>,
        mut each_edge: impl FnMut(Region<'db>, Region<'db>),
    ) {
        let data = region_constraints.legacy_data();
        let start = match only_consider_snapshot {
            Some(snapshot) => {
                let mark = &snapshot.undo_snapshot.region_constraints;
                assert!(
                    data.verifys.len() <= mark.verifys as usize,
                    "we never add verifications while doing higher-ranked things",
                );
                mark.constraints as usize
            }
            None => 0,
        };
        let constraints = &data.constraints;
        for index in start..constraints.len() {
            match constraints.visible_for_leak_check(index) {
                VisibleForLeakCheck::Yes => {
                    let (sub, sup) = constraints.get(index).regions(interner);
                    each_edge(sub, sup)
                }
                VisibleForLeakCheck::No => {}
                VisibleForLeakCheck::Unreachable => unreachable!(),
            }
        }
    }

    fn add_node(nodes: &mut FxIndexMap<Region<'db>, u32>, r: Region<'db>) -> u32 {
        let l = nodes.len() as u32;
        *nodes.entry(r).or_insert(l)
    }
}

/// The strongly connected components of a graph, numbered in post order: an SCC's
/// successors have smaller numbers, as `rustc_data_structures::graph::scc::Sccs` numbers them.
struct Sccs {
    /// For each node, its SCC.
    scc_of: Vec<u32>,
    /// For each SCC, its successors as a range into `successors` (CSR).
    successor_starts: Vec<u32>,
    successors: Vec<u32>,
}

impl Sccs {
    /// Tarjan's algorithm, iterative, visiting nodes in index order and each node's successors
    /// in target order, as rustc's `VecGraph` sorts them.
    /// `O(nodes + edges)`: the edges go to CSR by two stable counting passes (by target, then
    /// by source), which is the `(source, target)` order of rustc's sort without comparing.
    fn new(num_nodes: usize, edges: Vec<(u32, u32)>) -> Sccs {
        // alloc: per leak check past a binder, sized to its region graph.
        let (starts, targets) = radix::Csr::graph(num_nodes, edges.iter().copied()).into_parts();
        drop(edges);

        const UNVISITED: u32 = u32::MAX;
        // alloc: per leak check past a binder, sized to its region graph.
        let mut index_of = vec![UNVISITED; num_nodes];
        // alloc: per leak check past a binder, sized to its region graph.
        let mut low = vec![0u32; num_nodes];
        // A node is on the Tarjan stack exactly when it is visited and has no SCC yet.
        // alloc: per leak check past a binder, sized to its region graph.
        let mut scc_of = vec![UNVISITED; num_nodes];
        let mut stack: Vec<u32> = Vec::with_capacity(num_nodes);
        // The DFS: each frame is a node and the position of the next successor to visit.
        // alloc: per leak check past a binder, sized to its region graph.
        let mut frames: Vec<(u32, u32)> = Vec::new();
        let mut next_index = 0u32;
        let mut num_sccs = 0u32;
        // The members of SCC `s` are `members[member_starts[s]..member_starts[s + 1]]`.
        let mut members: Vec<u32> = Vec::with_capacity(num_nodes);
        // alloc: per leak check past a binder, sized to its region graph.
        let mut member_starts: Vec<u32> = vec![0];

        for root in 0..num_nodes as u32 {
            if index_of[root as usize] != UNVISITED {
                continue;
            }
            index_of[root as usize] = next_index;
            low[root as usize] = next_index;
            next_index += 1;
            stack.push(root);
            frames.push((root, starts[root as usize]));
            while let Some(frame) = frames.last_mut() {
                let node = frame.0;
                let n = node as usize;
                if frame.1 < starts[n + 1] {
                    let target = targets[frame.1 as usize];
                    frame.1 += 1;
                    let t = target as usize;
                    if index_of[t] == UNVISITED {
                        index_of[t] = next_index;
                        low[t] = next_index;
                        next_index += 1;
                        stack.push(target);
                        frames.push((target, starts[t]));
                    } else if scc_of[t] == UNVISITED {
                        low[n] = low[n].min(index_of[t]);
                    }
                    continue;
                }
                frames.pop();
                if let Some(&(parent, _)) = frames.last() {
                    let p = parent as usize;
                    low[p] = low[p].min(low[n]);
                }
                if low[n] == index_of[n] {
                    loop {
                        let member = stack.pop().unwrap();
                        scc_of[member as usize] = num_sccs;
                        members.push(member);
                        if member == node {
                            break;
                        }
                    }
                    member_starts.push(members.len() as u32);
                    num_sccs += 1;
                }
            }
        }

        let mut successor_starts = Vec::with_capacity(num_sccs as usize + 1);
        let mut successors = Vec::with_capacity(targets.len());
        successor_starts.push(0);
        for scc in 0..num_sccs as usize {
            let first = successors.len();
            let scc_members = member_starts[scc] as usize..member_starts[scc + 1] as usize;
            for &member in &members[scc_members] {
                let m = member as usize;
                for &target in &targets[starts[m] as usize..starts[m + 1] as usize] {
                    let target_scc = scc_of[target as usize];
                    if target_scc as usize != scc {
                        successors.push(target_scc);
                    }
                }
            }
            radix::sort(&mut successors[first..]);
            let mut kept = first;
            for at in first..successors.len() {
                if at == first || successors[at] != successors[kept - 1] {
                    successors[kept] = successors[at];
                    kept += 1;
                }
            }
            successors.truncate(kept);
            successor_starts.push(successors.len() as u32);
        }

        Sccs { scc_of, successor_starts, successors }
    }

    fn num_sccs(&self) -> usize {
        self.successor_starts.len() - 1
    }

    fn scc(&self, node: u32) -> usize {
        self.scc_of[node as usize] as usize
    }

    fn successors(&self, scc: usize) -> &[u32] {
        &self.successors[self.successor_starts[scc] as usize..self.successor_starts[scc + 1] as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::Sccs;

    #[test]
    fn sccs_are_numbered_successors_first() {
        // 0 -> 1 -> 2 -> 1, 2 -> 3, 4 alone, 5 -> 0
        let sccs = Sccs::new(6, vec![(0, 1), (1, 2), (2, 1), (2, 3), (5, 0)]);
        assert_eq!(sccs.scc(1), sccs.scc(2));
        let distinct: std::collections::HashSet<_> = (0..6).map(|n| sccs.scc(n)).collect();
        assert_eq!(distinct.len(), sccs.num_sccs());
        assert_eq!(sccs.num_sccs(), 5);
        for scc in 0..sccs.num_sccs() {
            for &succ in sccs.successors(scc) {
                assert!((succ as usize) < scc, "{succ} after {scc}");
            }
        }
        assert_eq!(sccs.successors(sccs.scc(0)), [sccs.scc(1) as u32]);
        assert_eq!(sccs.successors(sccs.scc(1)), [sccs.scc(3) as u32]);
        assert!(sccs.successors(sccs.scc(4)).is_empty());
    }

    #[test]
    fn a_cycle_through_every_node_is_one_scc() {
        let sccs = Sccs::new(4, vec![(0, 1), (1, 2), (2, 3), (3, 0), (1, 1)]);
        assert_eq!(sccs.num_sccs(), 1);
        assert!(sccs.successors(0).is_empty());
    }
}
