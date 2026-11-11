//! What a set of roots reaches in a graph that is edited, kept through each edit by work in
//! proportion to what the edit changes (docs/v2/plan/design/incremental-walk.md, "Added and
//! removed edges"): the delete and re-derive rule of incremental view maintenance (DRed).
//!
//! The graph is items and the edges of what each refers to. A *set* is what some roots
//! reach: the walk keeps two over one graph, everything its check's roots and its own reach
//! ([`ALL`]: what is described) and what its own roots reach ([`BUILT`]: what is built).
//!
//! Each member of a set has a *level*, and its *support*: how many of its in-edges come from
//! members of a lower level, a root counting one more.
//!
//! The invariant: every member has support, and a member's every out-edge goes to a member.
//! So a member is reached from a root (by induction on its level: its supporter is a member
//! of a lower level), and what a root reaches is a member (by induction on the path). A cycle
//! can't hold itself up: of its members the one of the least level has no supporter in it.
//!
//! An edge added to a member's supports its target, or brings a target that is no member in
//! with everything it reaches. An edge removed takes its support away; a member left with
//! none is dropped, and so, in turn, is what it alone supported ([`Graph::settle`]). The
//! dropped ones hold every member that may be unreached, and some that are reached another
//! way, through an edge from a member of no lower level: each dropped one that still has an
//! edge from a member is brought back above that member, and what it reaches among the
//! dropped with it. What is still dropped then has no edge from a member, so no root reaches
//! it.

use std::{cmp::Reverse, collections::BinaryHeap};

/// No level: not a member.
const OUT: u32 = u32::MAX;

/// The set of everything described: what the check's roots and the build's reach.
pub(super) const ALL: usize = 0;
/// The set of what is built: what the build's own roots reach.
pub(super) const BUILT: usize = 1;
const SETS: usize = 2;

#[derive(Default)]
struct Set {
    /// Per item, its level; [`OUT`] for one that is no member.
    level: Vec<u32>,
    /// Per member, its support.
    support: Vec<u32>,
    /// Members whose support an edit took to none, until [`Graph::settle`].
    unsupported: Vec<u32>,
    /// Items that became members since the last [`Graph::settle`].
    entered: Vec<u32>,
}

/// What an edit changed of the sets ([`Graph::settle`]): per set, the items that are members
/// now and were not, and those that were and are not, each sorted.
#[derive(Default, Debug, PartialEq, Eq)]
pub(super) struct Settled {
    pub(super) entered: [Vec<u32>; SETS],
    pub(super) left: [Vec<u32>; SETS],
}

/// A graph and its sets ([`self`]).
#[derive(Default)]
pub(super) struct Graph {
    /// Per item, what it refers to, in the order given, each once.
    outs: Vec<Vec<u32>>,
    /// Per item, what refers to it, sorted.
    ins: Vec<Vec<u32>>,
    /// Items taken out, for [`Graph::add`] to give again.
    free: Vec<u32>,
    sets: [Set; SETS],
    /// Items an edit looked at since the graph was made: entered, dropped or brought back.
    pub(super) visited: usize,
    /// The planted bug of `the_oracle_catches_a_node_dropped_while_a_cycle_supports_it`: what
    /// is dropped is never brought back.
    #[cfg(test)]
    pub(super) plant_drops_supported: bool,
}

impl Graph {
    /// A new item: no edges, a member of nothing until an edge from a member or
    /// [`Self::root`] makes it one.
    pub(super) fn add(&mut self) -> u32 {
        if let Some(item) = self.free.pop() {
            return item;
        }
        // alloc: per item of the graph, its two edge lists, empty until linked
        self.outs.push(Vec::new());
        self.ins.push(Vec::new()); // alloc: as above
        for set in &mut self.sets {
            set.level.push(OUT);
            set.support.push(0);
        }
        self.outs.len() as u32 - 1
    }

    pub(super) fn is_member(&self, set: usize, item: u32) -> bool {
        self.sets[set].level[item as usize] != OUT
    }

    /// What `item` refers to, in the order given.
    pub(super) fn outs(&self, item: u32) -> &[u32] {
        &self.outs[item as usize]
    }

    /// What refers to `item`, sorted.
    pub(super) fn ins(&self, item: u32) -> &[u32] {
        &self.ins[item as usize]
    }

    /// Makes `item` a root of `set`: a member whatever refers to it, with what it reaches.
    pub(super) fn root(&mut self, set: usize, item: u32) {
        match self.is_member(set, item) {
            true => self.sets[set].support[item as usize] += 1,
            false => {
                self.derive(set, item, 0);
                self.sets[set].support[item as usize] += 1;
            }
        }
    }

    /// Sets what `item` refers to: `outs`, in order, a target named twice counted once.
    pub(super) fn set_outs(&mut self, item: u32, mut outs: Vec<u32>) {
        let mut seen = 0;
        for at in 0..outs.len() {
            if !outs[..seen].contains(&outs[at]) {
                outs[seen] = outs[at];
                seen += 1;
            }
        }
        outs.truncate(seen);
        let old = std::mem::replace(&mut self.outs[item as usize], outs);
        // The new edges first: a target both lists hold never loses its support between.
        for at in 0..self.outs[item as usize].len() {
            let target = self.outs[item as usize][at];
            if !old.contains(&target) {
                self.link(item, target);
            }
        }
        for target in old {
            if !self.outs[item as usize].contains(&target) {
                self.unlink(item, target);
            }
        }
    }

    fn link(&mut self, from: u32, to: u32) {
        if let Err(at) = self.ins[to as usize].binary_search(&from) {
            self.ins[to as usize].insert(at, from);
        }
        for set in 0..SETS {
            let level = self.sets[set].level[from as usize];
            if level == OUT {
                continue;
            }
            match self.sets[set].level[to as usize] {
                OUT => self.derive(set, to, level + 1),
                above if level < above => self.sets[set].support[to as usize] += 1,
                _ => {}
            }
        }
    }

    fn unlink(&mut self, from: u32, to: u32) {
        if let Ok(at) = self.ins[to as usize].binary_search(&from) {
            self.ins[to as usize].remove(at);
        }
        for set in &mut self.sets {
            let (level, above) = (set.level[from as usize], set.level[to as usize]);
            if level != OUT && above != OUT && level < above {
                set.support[to as usize] -= 1;
                if set.support[to as usize] == 0 {
                    set.unsupported.push(to);
                }
            }
        }
    }

    /// Makes `item`, no member of `set`, one at `level`, and with it everything it reaches
    /// that is none, each a level above the least member that refers to it.
    fn derive(&mut self, set: usize, item: u32, level: u32) {
        // alloc: per edit that brings items into a set, those it brings
        let mut queue = BinaryHeap::new();
        queue.push(Reverse((level, item)));
        while let Some(Reverse((level, item))) = queue.pop() {
            if self.sets[set].level[item as usize] != OUT {
                continue;
            }
            self.visited += 1;
            let levels = &self.sets[set].level;
            let support = (self.ins[item as usize].iter())
                .filter(|from| levels[**from as usize] < level)
                .count() as u32;
            let it = &mut self.sets[set];
            it.level[item as usize] = level;
            it.support[item as usize] = support;
            it.entered.push(item);
            for &to in &self.outs[item as usize] {
                match it.level[to as usize] {
                    OUT => queue.push(Reverse((level + 1, to))),
                    above if level < above => it.support[to as usize] += 1,
                    _ => {}
                }
            }
        }
    }

    /// Ends an edit: in each set, drops every member left with no support and what it alone
    /// supported, brings back those an edge from a member still reaches, and says what
    /// entered and left ([`Settled`]). An item that left [`ALL`] is in no set and refers to
    /// nothing any more: [`Self::add`] gives it again.
    pub(super) fn settle(&mut self) -> Settled {
        let mut settled = Settled::default();
        for set in 0..SETS {
            // alloc: per edit that removes an edge, the items it drops
            let mut dropped: Vec<(u32, u32)> = Vec::new();
            let mut stack = std::mem::take(&mut self.sets[set].unsupported);
            while let Some(item) = stack.pop() {
                let it = &mut self.sets[set];
                let level = it.level[item as usize];
                // Supported again by an edge added since, or dropped already.
                if level == OUT || it.support[item as usize] != 0 {
                    continue;
                }
                self.visited += 1;
                it.level[item as usize] = OUT;
                dropped.push((item, level));
                for &to in &self.outs[item as usize] {
                    let above = it.level[to as usize];
                    if above != OUT && level < above {
                        it.support[to as usize] -= 1;
                        if it.support[to as usize] == 0 {
                            stack.push(to);
                        }
                    }
                }
            }
            // Each dropped item an edge from a member still reaches is a member, above the
            // least such member; `derive` brings what it reaches among the dropped with it.
            // alloc: per edit that removes an edge, the dropped items a member refers to
            let mut reached: Vec<(u32, u32)> = Vec::new();
            for &(item, _) in &dropped {
                let levels = &self.sets[set].level;
                let least = (self.ins[item as usize].iter())
                    .map(|from| levels[*from as usize])
                    .filter(|level| *level != OUT)
                    .min();
                reached.extend(least.map(|level| (level + 1, item)));
            }
            reached.sort_unstable();
            let entered = std::mem::take(&mut self.sets[set].entered);
            #[cfg(test)]
            if self.plant_drops_supported {
                reached.clear();
            }
            for (level, item) in reached {
                if !self.is_member(set, item) {
                    self.derive(set, item, level);
                }
            }
            // What `derive` brought back just now was a member before: it did not enter.
            self.sets[set].entered.clear();
            // Entered: made a member by the edit and one still. An item the edit brought in
            // and then dropped is in `left` alone, for whoever made something of it since.
            settled.entered[set] = (entered.into_iter())
                .filter(|item| self.is_member(set, *item))
                // alloc: per edit, what entered the set
                .collect();
            settled.entered[set].sort_unstable();
            settled.entered[set].dedup();
            settled.left[set] = (dropped.iter())
                .map(|it| it.0)
                .filter(|item| !self.is_member(set, *item))
                // alloc: per edit, what left the set
                .collect();
            settled.left[set].sort_unstable();
            settled.left[set].dedup();
        }
        for at in 0..settled.left[ALL].len() {
            let item = settled.left[ALL][at];
            debug_assert!(!self.is_member(BUILT, item), "what nothing reaches is not built");
            for to in std::mem::take(&mut self.outs[item as usize]) {
                if let Ok(at) = self.ins[to as usize].binary_search(&item) {
                    self.ins[to as usize].remove(at);
                }
            }
            self.ins[item as usize].clear();
            self.free.push(item);
        }
        settled
    }

    /// Every member of `set`, in order of item: for the tests' oracle.
    #[cfg(test)]
    fn members(&self, set: usize) -> Vec<u32> {
        (0..self.outs.len() as u32).filter(|item| self.is_member(set, *item)).collect()
    }
}

#[cfg(test)]
mod tests {
    use hegel::generators as gs;

    use super::*;

    const ITEMS: u32 = 12;

    /// What `roots` reach over `edges`, sorted: the whole computation.
    fn reached(edges: &[Vec<u32>], roots: &[u32]) -> Vec<u32> {
        let mut seen = vec![false; edges.len()];
        let mut stack = roots.to_vec();
        while let Some(item) = stack.pop() {
            if !std::mem::replace(&mut seen[item as usize], true) {
                stack.extend(edges[item as usize].iter().copied());
            }
        }
        (0..edges.len() as u32).filter(|item| seen[*item as usize]).collect()
    }

    /// The model: `ITEMS` items, item 0 a root of both sets and item 1 a root of [`ALL`]
    /// alone, each item's edges set again `edits` times. After each edit the sets are what
    /// the roots reach, by a walk of the whole graph. Items no root reaches keep the edges
    /// the model gave them only while something reaches them, so the model forgets theirs.
    fn first_difference(edits: &[(u32, Vec<u32>)], plant: bool) -> Option<String> {
        let mut graph = Graph::default();
        #[cfg(test)]
        {
            graph.plant_drops_supported = plant;
        }
        let mut edges: Vec<Vec<u32>> = vec![Vec::new(); ITEMS as usize];
        for _ in 0..ITEMS {
            graph.add();
        }
        graph.root(ALL, 0);
        graph.root(BUILT, 0);
        graph.root(ALL, 1);
        graph.settle();
        for (step, (item, outs)) in edits.iter().enumerate() {
            // Only an item something reaches is described, so only it has edges to set.
            if !graph.is_member(ALL, *item) {
                continue;
            }
            edges[*item as usize] = outs.clone(); // clone: the test's model of the edges
            graph.set_outs(*item, outs.clone()); // clone: as above
            assert_eq!(graph.outs(*item), dedup(outs));
            let settled = graph.settle();
            // What nothing reaches refers to nothing any more; the model's items are fixed,
            // so each slot the graph gave up is taken back.
            let mut back: Vec<u32> = (settled.left[ALL].iter())
                .map(|item| {
                    edges[*item as usize].clear();
                    graph.add()
                })
                .collect();
            back.sort_unstable();
            assert_eq!(back, settled.left[ALL]);
            for (set, roots) in [(ALL, &[0, 1][..]), (BUILT, &[0][..])] {
                let want = reached(&edges, roots);
                if graph.members(set) != want {
                    return Some(format!(
                        "edit {step} ({item} -> {outs:?}): set {set} is {:?}, the roots reach {want:?}",
                        graph.members(set)
                    ));
                }
            }
        }
        None
    }

    /// `outs`, each target once, in order: what the graph keeps of a list.
    fn dedup(outs: &[u32]) -> Vec<u32> {
        let mut seen = Vec::new();
        for it in outs {
            if !seen.contains(it) {
                seen.push(*it);
            }
        }
        seen
    }

    /// The refinement: after any sequence of edits of what items refer to, each set holds
    /// what its roots reach in the graph as it is, as a walk of the whole graph finds it.
    #[hegel::test(test_cases = 200)]
    fn the_sets_are_what_the_roots_reach(tc: hegel::TestCase) {
        let count: usize = tc.draw(gs::integers::<usize>().max_value(24));
        let mut edits = Vec::with_capacity(count);
        for _ in 0..count {
            let item = tc.draw(gs::integers::<u32>().max_value(ITEMS - 1));
            let outs: Vec<u32> =
                tc.draw(gs::vecs(gs::integers::<u32>().max_value(ITEMS - 1)).max_size(4));
            edits.push((item, outs));
        }
        if let Some(found) = first_difference(&edits, false) {
            panic!("{found}");
        }
    }

    /// The control: 0 refers to 2, 2 to 3, 3 to 4, and 4 back to 2 and to 5. Then 0 refers to
    /// 3 in place of 2: 2 loses its one supporter of a lower level and is dropped, though the
    /// cycle still reaches it from 3 through 4, an edge from a member of a higher level. The
    /// rule brings it back above 4; a graph that brings nothing back loses it.
    #[test]
    fn the_oracle_catches_a_node_dropped_while_a_cycle_supports_it() {
        let edits = [
            (0, vec![2]),
            (2, vec![3]),
            (3, vec![4]),
            (4, vec![2, 5]),
            (0, vec![3]),
        ];
        assert_eq!(first_difference(&edits, false), None);
        let found = first_difference(&edits, true).expect("the planted bug was not found");
        assert!(found.starts_with("edit 4 "), "{found}");
    }

    /// A cycle nothing refers to from outside is dropped whole: its members support each
    /// other, none from below.
    #[test]
    fn a_cycle_with_no_outside_support_is_dropped() {
        let edits = [(0, vec![2]), (2, vec![3]), (3, vec![2]), (0, vec![])];
        assert_eq!(first_difference(&edits, false), None);
    }

    /// The work of an edit is what it changes: in a chain of 10,000 with a branch at each,
    /// taking the last branch off visits the one item dropped.
    #[test]
    fn an_edit_visits_what_it_changes() {
        const COUNT: u32 = 10_000;
        let mut graph = Graph::default();
        for _ in 0..2 * COUNT {
            graph.add();
        }
        graph.root(ALL, 0);
        graph.root(BUILT, 0);
        graph.settle();
        // Item `i` refers to `i + 1` and to its branch `COUNT + i`.
        for item in 0..COUNT {
            let next = (item + 1 < COUNT).then_some(item + 1);
            graph.set_outs(item, next.into_iter().chain([COUNT + item]).collect());
        }
        assert_eq!(graph.settle().entered[BUILT].len(), 2 * COUNT as usize - 1);
        let before = graph.visited;
        graph.set_outs(COUNT - 1, Vec::new());
        let settled = graph.settle();
        assert_eq!(settled.left[BUILT], vec![2 * COUNT - 1]);
        assert_eq!(graph.visited - before, 1, "the one item dropped");
        // An edge to an item already reached supports it and visits nothing.
        graph.set_outs(COUNT - 1, vec![5]);
        assert_eq!(graph.settle(), Settled::default());
        assert_eq!(graph.visited - before, 1);
    }
}
