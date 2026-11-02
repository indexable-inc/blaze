//! What each release compile is given, decided from the summaries of the whole build before
//! any compile runs (docs/v2-codegen.md, "Release"; #899, #176).
//!
//! [`decide`] is a function of the summaries alone, in key order, so a warm build gives every
//! function the [`Imports`] a cold one did without building any IR, and two builds of one
//! program give the same.
//!
//! Every fact of the whole program that is not an import ([`Imports::constants`],
//! [`Imports::dead`]) is about what a function's callers do, so it holds only of a function
//! whose every caller has a summary: one with `external == false` whose address no summary
//! takes. Only the build's driver knows what code without a summary reaches, and says it
//! ([`close`]).

#![forbid(unsafe_code)]

use std::collections::VecDeque;

use blaze_graph::par;
use blaze_hash::{HashMap, HashSet};
use blaze_ids::bits::BitSet;

use crate::{
    InstanceKey,
    summary::{Arg, Attrs, Hint, Imports, Summary},
};

/// How much a compile imports, as ThinLTO's options say it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Limits {
    /// The most instructions, as LLVM counts them after its pre-link pipeline, a body with no
    /// `#[inline]` may have to be imported by a direct call.
    pub plain: u32,
    /// The same for an `#[inline]` body. `#[inline(always)]` has all of [`Limits::budget`];
    /// `#[inline(never)]` none.
    pub inline: u32,
    /// The share of its room an import leaves to each import of what it calls, as
    /// `-import-instr-evolution-factor`.
    pub evolution: f64,
    /// How many calls deep imports go.
    pub depth: u32,
    /// The most instructions one compile imports in all.
    pub budget: u32,
}

/// Bun on 16 cores against cargo's release build (c8a.24xlarge, 2026-10-23): rooms of 100
/// and 300 built in 118.0 s and ran 1.244x, 200 and 600 161.5 s and 1.242x, 40 and 100
/// 93.6 s and 1.311x. Its hot paths go through chains of small wrappers: 3 deep it ran
/// 1.785x, 12 deep as 8, and an evolution of 0.9 imported more for no faster a run.
impl Default for Limits {
    fn default() -> Limits {
        Limits { plain: 100, inline: 300, evolution: 0.7, depth: 8, budget: 40_000 }
    }
}

impl Limits {
    /// The room of a body with `hint` at the first call, `None` for one never imported.
    fn room(&self, hint: Hint) -> Option<u32> {
        match hint {
            Hint::None => Some(self.plain),
            Hint::Inline => Some(self.inline),
            Hint::Always => Some(self.budget),
            Hint::Never => None,
        }
    }
}

/// A summary that does not agree with itself or with another.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ImportError {
    /// The summary under `at` is of another function.
    Key { at: InstanceKey, says: InstanceKey },
    /// A call of `caller` passes more arguments than `callee` has parameters.
    Args { caller: InstanceKey, callee: InstanceKey, args: u32, params: u32 },
    /// A function's summary names a parameter it does not have as unused.
    Unused { function: InstanceKey, param: u32, params: u32 },
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Key { at, says } => {
                write!(f, "the summary kept under {} is of {}", at.0, says.0)
            }
            ImportError::Args { caller, callee, args, params } => write!(
                f,
                "{} calls {} with {args} arguments, and its summary has {params} parameters",
                caller.0, callee.0
            ),
            ImportError::Unused { function, param, params } => write!(
                f,
                "the summary of {} says parameter {param} is unused, of {params}",
                function.0
            ),
        }
    }
}

impl std::error::Error for ImportError {}

/// Says of every summary whether code the summaries don't show can call it: `outside` is
/// every function an exported symbol, a patchable slot, a static's value or a unit of another
/// backend reaches. A summary is made external ([`Summary::external`] is a fact of the build,
/// not of the function), so a build that does not close its summaries gets no whole-program
/// facts, never wrong ones.
pub fn close(summaries: &mut HashMap<InstanceKey, Summary>, outside: &HashSet<InstanceKey>) {
    for (key, summary) in summaries {
        summary.external = outside.contains(key);
    }
}

/// What every function of `summaries` is compiled with: the functions it imports, its
/// parameters every caller passes one constant for, what it may assume of its callees, and
/// which of their parameters it need not compute.
///
/// A callee without a summary (not an LLVM unit of this build) is imported by nothing and
/// assumed nothing of.
///
/// The summaries become columns over dense node numbers in key order ([`Graph`]) once, the
/// keys sorted there; every answer is then a function of those numbers, so it is the same
/// whatever the map's history. Each importer's imports are chosen on its own, in parallel,
/// into the worker's scratch.
pub fn decide(
    summaries: &HashMap<InstanceKey, Summary>,
    limits: &Limits,
) -> Result<HashMap<InstanceKey, Imports>, ImportError> {
    let graph = Graph::of(summaries)?;
    let attrs = graph.attrs();
    let constants = graph.constants();
    // alloc: one exact-size column per build
    let decided: Vec<Imports> = par::map(0..graph.len() as usize, |node| {
        // `node < graph.len()`, a `u32`.
        SCRATCH.with_borrow_mut(|scratch| graph.imports(node as u32, limits, &attrs, &constants, scratch))
    });
    // alloc: the build's answer, once per build
    Ok(graph.keys.iter().copied().zip(decided).collect())
}

/// No node: a callee without a summary, an unset slot.
const NONE: u32 = u32::MAX;

/// Node numbers by key: open addressing on the key's first 8 bytes, which are already a
/// content hash's and are never hashed again. 4 bytes per slot at most half full; a lookup
/// is one probe and one 32-byte compare, as a rule.
struct Index {
    slots: Vec<u32>,
    mask: usize,
}

impl Index {
    fn of(keys: &[InstanceKey]) -> Index {
        let len = (keys.len() * 2).next_power_of_two().max(2);
        // alloc: the index's table, sized once per build
        let mut index = Index { slots: vec![NONE; len], mask: len - 1 };
        for (node, key) in keys.iter().enumerate() {
            let mut at = home(key) & index.mask;
            while index.slots[at] != NONE {
                at = (at + 1) & index.mask;
            }
            index.slots[at] = node as u32;
        }
        index
    }

    fn get(&self, keys: &[InstanceKey], key: &InstanceKey) -> u32 {
        let mut at = home(key) & self.mask;
        loop {
            let node = self.slots[at];
            if node == NONE || keys[node as usize] == *key {
                return node;
            }
            at = (at + 1) & self.mask;
        }
    }
}

fn home(key: &InstanceKey) -> usize {
    let mut first = [0; 8];
    first.copy_from_slice(&key.0.0[..8]);
    u64::from_le_bytes(first) as usize
}

/// The summaries as columns over node numbers, which follow the keys' order. Per node: its
/// key (32 bytes), its summary (8), its calls (4 per call), its sorted unused parameters (4
/// each) and whether code outside the summaries can call it (1 bit).
struct Graph<'a> {
    keys: Vec<InstanceKey>,
    summaries: Vec<&'a Summary>,
    /// A node's calls are `to[start[node]..start[node + 1]]`, each its callee's node or
    /// [`NONE`], in the order of [`Summary::calls`].
    start: Vec<u32>,
    to: Vec<u32>,
    /// A node's [`Summary::unused`], sorted, each once: `unused[unused_start[node]..]`.
    unused_start: Vec<u32>,
    unused: Vec<u32>,
    /// Neither external nor addressed by any summary: every caller has a summary.
    closed: BitSet<u32>,
}

impl<'a> Graph<'a> {
    /// The columns of `summaries`, checking each as it goes, in key order: the first
    /// inconsistency in that order is the error.
    fn of(summaries: &'a HashMap<InstanceKey, Summary>) -> Result<Graph<'a>, ImportError> {
        let len = summaries.len();
        assert!(len < NONE as usize, "more than 2^32 - 1 summaries");
        // alloc: one exact-size column per build
        let mut summaries: Vec<(&'a InstanceKey, &'a Summary)> = summaries.iter().collect();
        summaries.sort_unstable_by_key(|it| *it.0);
        // alloc: one exact-size column per build
        let keys: Vec<InstanceKey> = summaries.iter().map(|it| *it.0).collect();
        let index = Index::of(&keys);
        // alloc: one exact-size column per build
        let params: Vec<u32> = summaries.iter().map(|it| it.1.params).collect();
        let calls: usize = summaries.iter().map(|it| it.1.calls.len()).sum();
        let unused: usize = summaries.iter().map(|it| it.1.unused.len()).sum();
        let mut graph = Graph {
            summaries: Vec::with_capacity(len),
            start: Vec::with_capacity(len + 1),
            to: Vec::with_capacity(calls),
            unused_start: Vec::with_capacity(len + 1),
            unused: Vec::with_capacity(unused),
            closed: BitSet::new(len),
            // alloc: empty, allocates nothing; `keys` is moved in once the columns are built
            keys: Vec::new(),
        };
        graph.start.push(0);
        graph.unused_start.push(0);
        let mut addressed = BitSet::<u32>::new(len);
        for (at, summary) in summaries {
            if summary.key != *at {
                return Err(ImportError::Key { at: *at, says: summary.key });
            }
            if let Some(&param) = summary.unused.iter().find(|it| **it >= summary.params) {
                return Err(ImportError::Unused { function: *at, param, params: summary.params });
            }
            for call in &summary.calls {
                let callee = index.get(&keys, &call.callee);
                if callee != NONE {
                    let params = params[callee as usize];
                    let args = u32::try_from(call.args.len()).unwrap_or(u32::MAX);
                    if args > params {
                        return Err(ImportError::Args { caller: *at, callee: call.callee, args, params });
                    }
                }
                graph.to.push(callee);
            }
            graph.start.push(graph.to.len() as u32);
            let from = graph.unused.len();
            graph.unused.extend_from_slice(&summary.unused);
            blaze_ids::radix::sort(&mut graph.unused[from..]);
            let kept = from + dedup(&mut graph.unused[from..]);
            graph.unused.truncate(kept);
            graph.unused_start.push(graph.unused.len() as u32);
            for key in &summary.addressed {
                let node = index.get(&keys, key);
                if node != NONE {
                    addressed.insert(node);
                }
            }
            graph.summaries.push(summary);
        }
        for (node, summary) in graph.summaries.iter().enumerate() {
            if !summary.external && !addressed.contains(node as u32) {
                graph.closed.insert(node as u32);
            }
        }
        graph.keys = keys;
        Ok(graph)
    }

    fn len(&self) -> u32 {
        self.keys.len() as u32
    }

    fn calls(&self, node: u32) -> &[u32] {
        &self.to[self.start[node as usize] as usize..self.start[node as usize + 1] as usize]
    }

    fn unused(&self, node: u32) -> &[u32] {
        let node = node as usize;
        &self.unused[self.unused_start[node] as usize..self.unused_start[node + 1] as usize]
    }

    /// Per closed callee and parameter, the integer constant every call site of the build
    /// passes, where they all agree: `passed[start[node]..start[node + 1]]`, one slot per
    /// parameter, filled only for a callee some call reaches (`reached`).
    fn constants(&self) -> Passed {
        let mut start = Vec::with_capacity(self.keys.len() + 1);
        start.push(0u32);
        let mut total = 0u32;
        for (node, summary) in self.summaries.iter().enumerate() {
            if self.closed.contains(node as u32) {
                total = total.checked_add(summary.params).expect("fewer than 2^32 parameters in all");
            }
            start.push(total);
        }
        // alloc: one column sized once per build
        let mut passed = Passed { start, slots: vec![None; total as usize], reached: BitSet::new(self.keys.len()) };
        for (caller, summary) in self.summaries.iter().enumerate() {
            for (call, &callee) in std::iter::zip(&summary.calls, self.calls(caller as u32)) {
                if callee == NONE || !self.closed.contains(callee) || call.count == 0 {
                    continue;
                }
                let range = passed.start[callee as usize] as usize..passed.start[callee as usize + 1] as usize;
                let slots = &mut passed.slots[range];
                if !passed.reached.contains(callee) {
                    passed.reached.insert(callee);
                    for (at, slot) in slots.iter_mut().enumerate() {
                        *slot = match call.args.get(at) {
                            Some(arg @ Arg::Int { .. }) => Some(*arg),
                            _ => None,
                        };
                    }
                }
                for (at, known) in slots.iter_mut().enumerate() {
                    if *known != call.args.get(at).copied() {
                        *known = None;
                    }
                }
            }
        }
        passed
    }

    /// What each node may be assumed to be by its callers, as [`Attrs::bits`]: its body's
    /// attributes met with its callees' bottom up over the call graph's strongly connected
    /// components, as LLVM's `function-attrs` infers them over an SCC: nounwind, readonly and
    /// readnone hold of a component when they hold of every member's body and of every callee
    /// outside it; willreturn and norecurse never hold of a member of a cycle. noreturn is the
    /// function's own, as inferred alone.
    fn attrs(&self) -> Vec<u8> {
        let len = self.keys.len();
        // The edges between summaries, each node's sorted and once.
        let mut start = Vec::with_capacity(len + 1);
        let mut edges = Vec::with_capacity(self.to.len());
        start.push(0u32);
        for node in 0..self.len() {
            let from = edges.len();
            edges.extend(self.calls(node).iter().copied().filter(|&it| it != NONE));
            blaze_ids::radix::sort(&mut edges[from..]);
            let kept = from + dedup(&mut edges[from..]);
            edges.truncate(kept);
            start.push(edges.len() as u32);
        }
        let (order, bounds) = components(&start, &edges);
        // alloc: one column sized once per build
        let mut component_of = vec![NONE; len];
        // alloc: one column sized once per build
        let mut known = vec![0u8; len];
        let read = |bits: u8| if bits & Attrs::READNONE != 0 { bits | Attrs::READONLY } else { bits };
        for (component, bound) in bounds.windows(2).enumerate() {
            let members = &order[bound[0] as usize..bound[1] as usize];
            for &member in members {
                component_of[member as usize] = component as u32;
            }
            let first = members[0];
            let looped = edges[start[first as usize] as usize..start[first as usize + 1] as usize]
                .binary_search(&first)
                .is_ok();
            let cyclic = members.len() > 1 || looped;
            let mut met = Attrs::NOUNWIND | Attrs::READONLY | Attrs::READNONE;
            if !cyclic {
                met |= Attrs::NORECURSE | Attrs::WILLRETURN;
            }
            let mut outside = false;
            for &member in members {
                met &= read(self.summaries[member as usize].body.bits());
                for &callee in self.calls(member) {
                    if callee == NONE {
                        outside = true;
                    } else if component_of[callee as usize] != component as u32 {
                        met &= read(known[callee as usize]);
                    }
                }
            }
            if outside {
                met = 0;
            }
            for &member in members {
                let alone = self.summaries[member as usize].attrs.bits();
                known[member as usize] = read(met & !Attrs::NORETURN | alone);
            }
        }
        known
    }

    /// The [`Imports`] of `importer`.
    fn imports(
        &self,
        importer: u32,
        limits: &Limits,
        attrs: &[u8],
        passed: &Passed,
        scratch: &mut Scratch,
    ) -> Imports {
        scratch.next(self.keys.len());
        self.imported(importer, limits, scratch);
        let Scratch { listed, generation, taken, callees, .. } = scratch;
        let generation = *generation;
        // What the compile calls: its own calls and those of the bodies it builds in.
        callees.clear();
        for &caller in std::iter::once(&importer).chain(taken.iter()) {
            for &callee in self.calls(caller) {
                if callee != NONE && listed[callee as usize] != generation {
                    listed[callee as usize] = generation;
                    callees.push(callee);
                }
            }
        }
        blaze_ids::radix::sort(&mut callees[..]);
        // The `Imports` below are the function's answer, which its compile owns
        // (`summary::Imports`): each list is built once per function and kept.
        let constants = match passed.reached.contains(importer) {
            true => passed
                .of(importer)
                .iter()
                .enumerate()
                .filter_map(|(at, arg)| Some((at as u32, (*arg)?)))
                // alloc: the function's answer, kept by its compile
                .collect(),
            // alloc: empty, allocates nothing
            false => Vec::new(),
        };
        Imports {
            // alloc: the function's answer, kept by its compile
            imported: taken.iter().map(|&it| self.keys[it as usize]).collect(),
            constants,
            callees: callees
                .iter()
                .filter(|&&it| attrs[it as usize] != 0)
                .map(|&it| (self.keys[it as usize], Attrs::from_bits(attrs[it as usize])))
                // alloc: the function's answer, kept by its compile
                .collect(),
            dead: callees
                .iter()
                .filter(|&&it| self.closed.contains(it) && !self.unused(it).is_empty())
                // alloc: the function's answer, kept by its compile
                .map(|&it| (self.keys[it as usize], self.unused(it).to_vec()))
                // alloc: the function's answer, kept by its compile
                .collect(),
        }
    }

    /// What `importer` imports into `scratch.taken`, sorted, as ThinLTO's
    /// `computeImportForFunction` chooses: each call's callee if its body fits the room its
    /// hint gives, shrunk by [`Limits::evolution`] at each call deeper, and then what that
    /// callee calls, breadth first, each callee once, until [`Limits::depth`] or
    /// [`Limits::budget`].
    fn imported(&self, importer: u32, limits: &Limits, scratch: &mut Scratch) {
        let generation = scratch.generation;
        let (seen, next, taken) = (&mut scratch.seen, &mut scratch.next, &mut scratch.taken);
        next.clear();
        taken.clear();
        seen[importer as usize] = generation;
        let mut spent: u64 = 0;
        next.extend(self.calls(importer).iter().map(|&it| (it, 1.0, 0)));
        while let Some((node, share, depth)) = next.pop_front() {
            // Breadth first, a callee is first met where its room is the largest it gets: one
            // that does not fit there fits nowhere deeper.
            if node == NONE || seen[node as usize] == generation {
                continue;
            }
            seen[node as usize] = generation;
            let callee = self.summaries[node as usize];
            let Some(room) = limits.room(callee.hint) else { continue };
            if f64::from(callee.size) > f64::from(room) * share
                || spent + u64::from(callee.size) > u64::from(limits.budget)
            {
                continue;
            }
            spent += u64::from(callee.size);
            taken.push(node);
            if depth + 1 < limits.depth {
                let share = share * limits.evolution;
                next.extend(self.calls(node).iter().map(|&it| (it, share, depth + 1)));
            }
        }
        blaze_ids::radix::sort(&mut taken[..]);
    }
}

/// [`Graph::constants`]: 16 bytes per parameter of a closed function.
struct Passed {
    start: Vec<u32>,
    slots: Vec<Option<Arg>>,
    reached: BitSet<u32>,
}

impl Passed {
    fn of(&self, node: u32) -> &[Option<Arg>] {
        &self.slots[self.start[node as usize] as usize..self.start[node as usize + 1] as usize]
    }
}

/// A worker's buffers for choosing one importer's imports after another: nothing is
/// allocated per importer once they have grown, and none is cleared per importer, as a node
/// is marked with the importer's `generation`.
#[derive(Default)]
struct Scratch {
    /// Per node: the generation that met it as a callee to import.
    seen: Vec<u32>,
    /// Per node: the generation that listed it among the compile's callees.
    listed: Vec<u32>,
    generation: u32,
    next: VecDeque<(u32, f64, u32)>,
    taken: Vec<u32>,
    callees: Vec<u32>,
}

impl Scratch {
    /// A generation no node of a graph of `len` nodes is marked with.
    fn next(&mut self, len: usize) {
        if self.seen.len() < len {
            self.seen.resize(len, 0);
            self.listed.resize(len, 0);
        }
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.seen.fill(0);
            self.listed.fill(0);
            self.generation = 1;
        }
    }
}

thread_local! {
    static SCRATCH: std::cell::RefCell<Scratch> = std::cell::RefCell::default();
}

/// Moves each first of a run of equal elements of the sorted `items` to the front; gives how
/// many there are.
fn dedup(items: &mut [u32]) -> usize {
    let mut kept = 0;
    for at in 0..items.len() {
        if kept == 0 || items[at] != items[kept - 1] {
            items[kept] = items[at];
            kept += 1;
        }
    }
    kept
}

/// The strongly connected components of the graph whose node `n` has the edges
/// `edges[start[n]..start[n + 1]]`, callees before callers (Tarjan's order, iterative): the
/// members of component `c` are `order[bounds[c]..bounds[c + 1]]`, sorted.
fn components(start: &[u32], edges: &[u32]) -> (Vec<u32>, Vec<u32>) {
    let len = start.len() - 1;
    // alloc: one column sized once per build
    let mut index = vec![NONE; len];
    // alloc: one column sized once per build
    let mut low = vec![0u32; len];
    let mut on_stack = BitSet::<u32>::new(len);
    let mut stack = Vec::with_capacity(len);
    let mut order = Vec::with_capacity(len);
    let mut bounds = Vec::with_capacity(len + 1);
    bounds.push(0u32);
    let mut work: Vec<(u32, u32)> = Vec::with_capacity(len);
    let mut next = 0u32;
    for root in 0..len as u32 {
        if index[root as usize] != NONE {
            continue;
        }
        index[root as usize] = next;
        low[root as usize] = next;
        next += 1;
        stack.push(root);
        on_stack.insert(root);
        work.push((root, start[root as usize]));
        while let Some(&(node, edge)) = work.last() {
            if edge < start[node as usize + 1] {
                work.last_mut().expect("just read").1 += 1;
                let to = edges[edge as usize];
                if index[to as usize] == NONE {
                    index[to as usize] = next;
                    low[to as usize] = next;
                    next += 1;
                    stack.push(to);
                    on_stack.insert(to);
                    work.push((to, start[to as usize]));
                } else if on_stack.contains(to) {
                    low[node as usize] = low[node as usize].min(index[to as usize]);
                }
                continue;
            }
            work.pop();
            if let Some(&(parent, _)) = work.last() {
                low[parent as usize] = low[parent as usize].min(low[node as usize]);
            }
            if low[node as usize] == index[node as usize] {
                let from = order.len();
                loop {
                    let member = stack.pop().expect("the component's root is on the stack");
                    on_stack.remove(member);
                    order.push(member);
                    if member == node {
                        break;
                    }
                }
                blaze_ids::radix::sort(&mut order[from..]);
                bounds.push(order.len() as u32);
            }
        }
    }
    (order, bounds)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use blaze_ids::{ContentId, rng::SplitMix64};

    use super::*;
    use crate::summary::Call;

    fn key(n: u8) -> InstanceKey {
        InstanceKey(ContentId([n; 32]))
    }

    fn all() -> Attrs {
        Attrs {
            nounwind: true,
            readonly: true,
            readnone: true,
            norecurse: true,
            willreturn: true,
            noreturn: false,
        }
    }

    fn function(n: u8, size: u32, calls: &[u8]) -> Summary {
        Summary {
            key: key(n),
            size,
            hint: Hint::None,
            calls: calls
                .iter()
                .map(|it| Call { callee: key(*it), count: 1, args: Vec::new() })
                .collect(),
            addressed: Vec::new(),
            params: 0,
            unused: Vec::new(),
            attrs: Attrs::default(),
            body: Attrs::default(),
            external: false,
        }
    }

    fn build(summaries: Vec<Summary>) -> HashMap<InstanceKey, Summary> {
        summaries.into_iter().map(|it| (it.key, it)).collect()
    }

    fn decided(summaries: Vec<Summary>) -> HashMap<InstanceKey, Imports> {
        decide(&build(summaries), &Limits::default()).unwrap()
    }

    fn int(value: u64) -> Arg {
        Arg::Int { bits: 32, value }
    }

    #[test]
    fn imports_shrink_by_depth() {
        // 1 calls 2 (90), which calls 3 (80 > 70), which calls 4 (10).
        let imports = decided(vec![
            function(1, 5, &[2]),
            function(2, 90, &[3]),
            function(3, 80, &[4]),
            function(4, 10, &[]),
        ]);
        assert_eq!(imports[&key(1)].imported, vec![key(2)]);
        // From 2, 3 is one call deep: 80 fits 100.
        assert_eq!(imports[&key(2)].imported, vec![key(3), key(4)]);
    }

    #[test]
    fn a_large_body_is_not_imported_and_hints_change_the_room() {
        let mut inline = function(3, 250, &[]);
        inline.hint = Hint::Inline;
        let mut always = function(4, 5_000, &[]);
        always.hint = Hint::Always;
        let mut never = function(5, 1, &[]);
        never.hint = Hint::Never;
        let imports = decided(vec![
            function(1, 5, &[2, 3, 4, 5]),
            function(2, 101, &[]),
            inline,
            always,
            never,
        ]);
        assert_eq!(imports[&key(1)].imported, vec![key(3), key(4)]);
    }

    #[test]
    fn imports_stop_at_the_depth() {
        let limits = Limits { depth: 2, ..Limits::default() };
        let summaries =
            build(vec![function(1, 1, &[2]), function(2, 1, &[3]), function(3, 1, &[4]), function(4, 1, &[])]);
        let imports = decide(&summaries, &limits).unwrap();
        assert_eq!(imports[&key(1)].imported, vec![key(2), key(3)]);
    }

    #[test]
    fn a_callee_is_imported_once_and_never_the_importer() {
        // 1 calls 2 and 3, both of which call 4; 4 calls 1.
        let imports = decided(vec![
            function(1, 1, &[2, 3]),
            function(2, 1, &[4]),
            function(3, 1, &[4]),
            function(4, 1, &[1]),
        ]);
        assert_eq!(imports[&key(1)].imported, vec![key(2), key(3), key(4)]);
    }

    #[test]
    fn the_budget_is_spent_in_order() {
        let limits = Limits { budget: 150, ..Limits::default() };
        let summaries = build(vec![
            function(1, 1, &[4, 2, 3]),
            function(2, 60, &[]),
            function(3, 60, &[]),
            function(4, 60, &[]),
        ]);
        let imports = decide(&summaries, &limits).unwrap();
        // 4 and 2 are met first; 3 would make 180.
        assert_eq!(imports[&key(1)].imported, vec![key(2), key(4)]);
    }

    #[test]
    fn a_callee_without_a_summary_is_a_case() {
        let imports = decided(vec![function(1, 1, &[9])]);
        assert_eq!(imports[&key(1)], Imports::default());
    }

    #[test]
    fn constants_only_for_internal_functions_with_agreeing_call_sites() {
        let callee = |n: u8| {
            let mut it = function(n, 1_000, &[]);
            it.params = 3;
            it
        };
        let call = |callee: u8, args: Vec<Arg>| Call { callee: key(callee), count: 2, args };
        let (internal, mut external, addressed, disagreeing) =
            (callee(2), callee(3), callee(4), callee(5));
        external.external = true;
        let mut taker = function(6, 1_000, &[]);
        taker.addressed = vec![key(4)];
        let args = vec![int(7), Arg::Unknown, int(1)];
        let mut first = function(1, 1_000, &[]);
        first.calls = vec![
            call(2, args.clone()),
            call(3, args.clone()),
            call(4, args.clone()),
            call(5, args.clone()),
        ];
        let mut second = function(7, 1_000, &[]);
        // Fewer arguments than parameters: the last is unknown.
        second.calls = vec![
            call(2, vec![int(7), int(3)]),
            call(3, args.clone()),
            call(4, args.clone()),
            call(5, vec![int(8), Arg::Unknown, int(1)]),
        ];
        let imports = decided(vec![first, second, internal, external, addressed, disagreeing, taker]);
        assert_eq!(imports[&key(2)].constants, vec![(0, int(7))]);
        assert_eq!(imports[&key(3)].constants, vec![]);
        assert_eq!(imports[&key(4)].constants, vec![]);
        assert_eq!(imports[&key(5)].constants, vec![(2, int(1))]);
        // No callers, no constants.
        assert_eq!(imports[&key(1)].constants, vec![]);
    }

    #[test]
    fn what_is_called_from_outside_gets_no_constants() {
        let mut callee = function(2, 1_000, &[]);
        callee.params = 1;
        let mut caller = function(1, 1_000, &[]);
        caller.calls = vec![Call { callee: key(2), count: 1, args: vec![int(4)] }];
        let mut summaries = build(vec![caller, callee]);
        assert_eq!(decide(&summaries, &Limits::default()).unwrap()[&key(2)].constants, vec![(0, int(4))]);
        close(&mut summaries, &HashSet::from_iter([key(2)]));
        assert!(summaries[&key(2)].external && !summaries[&key(1)].external);
        assert_eq!(decide(&summaries, &Limits::default()).unwrap()[&key(2)].constants, vec![]);
    }

    #[test]
    fn attributes_are_met_through_a_cycle() {
        // 1 calls 2; 2 and 3 call each other; 3 calls 4. Every body is everything.
        let mut summaries =
            vec![function(1, 1_000, &[2]), function(2, 1_000, &[3]), function(3, 1_000, &[2, 4]), function(4, 1_000, &[])];
        for it in &mut summaries {
            it.body = all();
        }
        let imports = decided(summaries.clone());
        let cyclic = Attrs { norecurse: false, willreturn: false, ..all() };
        assert_eq!(imports[&key(1)].callees, vec![(key(2), cyclic)]);
        assert_eq!(imports[&key(2)].callees, vec![(key(3), cyclic)]);
        assert_eq!(imports[&key(3)].callees, vec![(key(2), cyclic), (key(4), all())]);
        // 4 may unwind: so may the cycle and its caller; 4 still reads nothing.
        summaries[3].body.nounwind = false;
        let imports = decided(summaries.clone());
        let unwinding = Attrs { nounwind: false, ..cyclic };
        assert_eq!(imports[&key(1)].callees, vec![(key(2), unwinding)]);
        assert_eq!(imports[&key(3)].callees[1], (key(4), Attrs { nounwind: false, ..all() }));
        // A callee without a summary makes its callers assume nothing of it or of themselves.
        summaries[3].body = all();
        summaries[3].calls.push(Call { callee: key(9), count: 1, args: Vec::new() });
        let imports = decided(summaries.clone());
        assert_eq!(imports[&key(1)].callees, vec![]);
        // What LLVM inferred alone still holds.
        summaries[1].attrs = Attrs { nounwind: true, noreturn: true, ..Attrs::default() };
        let imports = decided(summaries);
        assert_eq!(
            imports[&key(1)].callees,
            vec![(key(2), Attrs { nounwind: true, noreturn: true, ..Attrs::default() })]
        );
    }

    #[test]
    fn a_self_call_is_a_cycle_and_readnone_is_readonly() {
        let mut recursive = function(2, 1_000, &[2]);
        recursive.body = Attrs { readnone: true, ..all() };
        recursive.body.readonly = false;
        let imports = decided(vec![function(1, 1_000, &[2]), recursive]);
        assert_eq!(
            imports[&key(1)].callees,
            vec![(key(2), Attrs { norecurse: false, willreturn: false, ..all() })]
        );
    }

    #[test]
    fn dead_arguments_of_closed_callees() {
        let mut internal = function(2, 1_000, &[]);
        internal.params = 3;
        internal.unused = vec![2, 0];
        let mut external = internal.clone();
        external.key = key(3);
        external.external = true;
        let mut addressed = internal.clone();
        addressed.key = key(4);
        let mut caller = function(1, 1_000, &[2, 3, 4]);
        caller.addressed = vec![key(4)];
        let imports = decided(vec![caller, internal, external, addressed]);
        assert_eq!(imports[&key(1)].dead, vec![(key(2), vec![0, 2])]);
    }

    #[test]
    fn an_imported_body_brings_its_callees_facts() {
        // 1 imports 2, which calls 3: 1's compile has 3's declaration too.
        let mut leaf = function(3, 1_000, &[]);
        leaf.body = all();
        leaf.params = 1;
        leaf.unused = vec![0];
        let imports = decided(vec![function(1, 1, &[2]), function(2, 1, &[3]), leaf]);
        assert_eq!(imports[&key(1)].imported, vec![key(2)]);
        assert_eq!(imports[&key(1)].callees, vec![(key(3), all())]);
        assert_eq!(imports[&key(1)].dead, vec![(key(3), vec![0])]);
    }

    #[test]
    fn inconsistent_summaries_are_refused() {
        let mut caller = function(1, 1, &[2]);
        caller.calls[0].args = vec![Arg::Unknown, Arg::Unknown];
        let mut callee = function(2, 1, &[]);
        callee.params = 1;
        assert_eq!(
            decide(&build(vec![caller.clone(), callee.clone()]), &Limits::default()),
            Err(ImportError::Args { caller: key(1), callee: key(2), args: 2, params: 1 })
        );
        callee.unused = vec![1];
        caller.calls[0].args.pop();
        assert_eq!(
            decide(&build(vec![caller.clone(), callee]), &Limits::default()),
            Err(ImportError::Unused { function: key(2), param: 1, params: 1 })
        );
        let mut summaries = build(vec![caller]);
        summaries.insert(key(5), function(6, 1, &[]));
        assert_eq!(
            decide(&summaries, &Limits::default()),
            Err(ImportError::Key { at: key(5), says: key(6) })
        );
    }

    #[test]
    fn the_same_whatever_the_order_of_insertion() {
        let mut summaries = Vec::new();
        for n in 1..40u8 {
            let calls: Vec<u8> = [n.wrapping_mul(7) % 40 + 1, n.wrapping_mul(13) % 40 + 1, n % 5 + 1]
                .into_iter()
                .filter(|it| *it < 40)
                .collect();
            let mut it = function(n, u32::from(n) * 3, &calls);
            it.params = 2;
            it.unused = vec![u32::from(n % 2)];
            it.body = Attrs { nounwind: n % 3 != 0, readonly: n % 4 != 0, ..all() };
            for call in &mut it.calls {
                call.args = vec![int(u64::from(n % 2)), Arg::Unknown];
            }
            summaries.push(it);
        }
        let forward = decided(summaries.clone());
        summaries.reverse();
        let mut map = HashMap::default();
        for it in summaries.iter().cloned() {
            map.insert(it.key, it);
        }
        let backward = decide(&map, &Limits::default()).unwrap();
        // Another capacity, so another iteration order of the same keys.
        let mut wide = HashMap::with_capacity_and_hasher(4_096, Default::default());
        wide.extend(summaries.iter().map(|it| (it.key, it.clone())));
        let widened = decide(&wide, &Limits::default()).unwrap();
        assert_eq!(forward, backward);
        assert_eq!(forward, widened);
        assert_eq!(forward.len(), 39);
        for (key, imports) in &forward {
            assert_eq!(imports.blob_key(*key), backward[key].blob_key(*key));
        }
    }

    #[test]
    fn the_first_inconsistency_in_key_order_is_the_error_whatever_the_map() {
        let mut summaries = Vec::new();
        for n in (1..30u8).rev() {
            let mut it = function(n, 1, &[]);
            it.params = 1;
            if n % 7 == 0 {
                it.unused = vec![3];
            }
            summaries.push(it);
        }
        let mut map = HashMap::with_capacity_and_hasher(1_024, Default::default());
        map.extend(summaries.into_iter().map(|it| (it.key, it)));
        assert_eq!(
            decide(&map, &Limits::default()),
            Err(ImportError::Unused { function: key(7), param: 3, params: 1 })
        );
    }

    #[test]
    fn components_come_callees_first() {
        // 0 -> 1 -> 2 -> 1, 2 -> 3.
        let (start, edges) = ([0, 1, 2, 4, 4], [1, 2, 1, 3]);
        assert_eq!(components(&start, &edges), (vec![3, 1, 2, 0], vec![0, 1, 3, 4]));
    }

    #[test]
    fn keys_that_share_their_first_bytes_are_told_apart() {
        let mut keys: Vec<InstanceKey> = (0..20u8)
            .map(|n| {
                let mut bytes = [7; 32];
                bytes[31] = n;
                InstanceKey(ContentId(bytes))
            })
            .collect();
        keys.sort();
        let index = Index::of(&keys);
        for (node, key) in keys.iter().enumerate() {
            assert_eq!(index.get(&keys, key), node as u32);
        }
        assert_eq!(index.get(&keys, &key(9)), NONE);
        assert_eq!(Index::of(&[]).get(&[], &key(1)), NONE);
    }

    #[test]
    fn nothing_to_decide() {
        assert_eq!(decide(&HashMap::default(), &Limits::default()), Ok(HashMap::default()));
    }

    /// A random build of summaries: keys of which some share their first 8 bytes, calls of
    /// summaries and of functions without one, agreeing and disagreeing constants, external
    /// and addressed functions, cycles, and now and then an inconsistent summary.
    fn random(seeded: &mut SplitMix64) -> (BTreeMap<InstanceKey, Summary>, Limits) {
        let len = 1 + seeded.below(48) as usize;
        let mut keys: Vec<InstanceKey> = (0..len + 4)
            .map(|_| {
                let mut bytes = [0u8; 32];
                for byte in &mut bytes {
                    *byte = seeded.next_u64() as u8;
                }
                if seeded.one_in(3) {
                    bytes[..8].copy_from_slice(&[0x5a; 8]);
                }
                InstanceKey(ContentId(bytes))
            })
            .collect();
        // The last four have no summary.
        let missing = keys.split_off(len);
        let args = [Arg::Unknown, int(0), int(1), Arg::Int { bits: 64, value: 1 }];
        let params: Vec<u32> = (0..len).map(|_| seeded.below(4) as u32).collect();
        let mut summaries = BTreeMap::new();
        for (at, key) in keys.iter().enumerate() {
            let calls = (0..seeded.below(5))
                .map(|_| {
                    let (callee, room) = match seeded.one_in(6) {
                        true => (missing[seeded.below(4) as usize], 4),
                        false => {
                            let callee = seeded.below(len as u64) as usize;
                            (keys[callee], params[callee] + u32::from(seeded.one_in(200)))
                        }
                    };
                    let args = (0..seeded.below(u64::from(room) + 1))
                        .map(|_| args[seeded.below(4) as usize])
                        .collect();
                    Call { callee, count: seeded.below(3) as u32, args }
                })
                .collect();
            let past = seeded.one_in(300).then_some(params[at]);
            let unused = (0..params[at]).filter(|_| seeded.one_in(2)).chain(past).collect();
            let addressed = (0..seeded.below(2))
                .map(|_| match seeded.one_in(3) {
                    true => missing[seeded.below(4) as usize],
                    false => keys[seeded.below(len as u64) as usize],
                })
                .collect();
            let summary = Summary {
                key: if seeded.one_in(400) { missing[0] } else { *key },
                size: match seeded.one_in(10) {
                    true => seeded.below(50_000) as u32,
                    false => seeded.below(400) as u32,
                },
                hint: [Hint::None, Hint::Inline, Hint::Always, Hint::Never][seeded.below(4) as usize],
                calls,
                addressed,
                params: params[at],
                unused,
                attrs: Attrs::from_bits(seeded.below(64) as u8),
                body: Attrs::from_bits(seeded.below(64) as u8),
                external: seeded.one_in(5),
            };
            summaries.insert(*key, summary);
        }
        let limits = Limits {
            plain: seeded.below(200) as u32,
            inline: seeded.below(600) as u32,
            evolution: [0.5, 0.7, 0.9, 1.0][seeded.below(4) as usize],
            depth: seeded.below(10) as u32,
            budget: seeded.below(3_000) as u32,
        };
        (summaries, limits)
    }

    #[test]
    fn the_columns_decide_as_the_maps_did() {
        let mut seeded = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
        let mut decided = 0;
        for case in 0..2_000 {
            let (summaries, limits) = random(&mut seeded);
            let hashed: HashMap<InstanceKey, Summary> =
                summaries.iter().map(|(key, it)| (*key, it.clone())).collect();
            let new = decide(&hashed, &limits).map(|it| it.into_iter().collect::<BTreeMap<_, _>>());
            let old = model::decide(&summaries, &limits);
            assert_eq!(new, old, "case {case}");
            decided += usize::from(new.is_ok());
        }
        // Most cases are consistent, so most compare imports rather than errors.
        assert!(decided > 1_000, "{decided}");
    }

    #[test]
    fn the_columns_find_the_components_the_lists_did() {
        let mut seeded = SplitMix64::new(17);
        for _ in 0..500 {
            let len = 1 + seeded.below(30) as usize;
            let lists: Vec<Vec<usize>> = (0..len)
                .map(|_| {
                    let mut to: Vec<usize> =
                        (0..seeded.below(4)).map(|_| seeded.below(len as u64) as usize).collect();
                    to.sort_unstable();
                    to.dedup();
                    to
                })
                .collect();
            let mut start = vec![0u32];
            let mut edges = Vec::new();
            for to in &lists {
                edges.extend(to.iter().map(|&it| it as u32));
                start.push(edges.len() as u32);
            }
            let (order, bounds) = components(&start, &edges);
            let flat: Vec<Vec<usize>> = bounds
                .windows(2)
                .map(|it| order[it[0] as usize..it[1] as usize].iter().map(|&it| it as usize).collect())
                .collect();
            assert_eq!(flat, model::components(&lists));
        }
    }

    /// `decide` as it was written first, over `BTreeMap`s per node: what the columns must
    /// answer exactly.
    mod model {
        use super::*;

        /// What every function of `summaries` is compiled with: the functions it imports, its
        /// parameters every caller passes one constant for, what it may assume of its callees, and
        /// which of their parameters it need not compute.
        ///
        /// A callee without a summary (not an LLVM unit of this build) is imported by nothing and
        /// assumed nothing of.
        pub(super) fn decide(
            summaries: &BTreeMap<InstanceKey, Summary>,
            limits: &Limits,
        ) -> Result<BTreeMap<InstanceKey, Imports>, ImportError> {
            check(summaries)?;
            let addressed: BTreeSet<InstanceKey> =
                summaries.values().flat_map(|it| it.addressed.iter().copied()).collect();
            let closed = |key: &InstanceKey| {
                summaries.get(key).is_some_and(|it| !it.external && !addressed.contains(key))
            };
            let constants = constants(summaries, &closed);
            let attrs = attrs(summaries);
            let mut decided = BTreeMap::new();
            for (key, summary) in summaries {
                let imported = imported(summaries, summary, limits);
                let mut callees = BTreeMap::new();
                let mut dead = BTreeMap::new();
                let callers = std::iter::once(summary).chain(imported.iter().map(|it| &summaries[it]));
                for caller in callers {
                    for call in &caller.calls {
                        let Some(callee) = summaries.get(&call.callee) else { continue };
                        let assumed = attrs[&call.callee];
                        if assumed != Attrs::default() {
                            callees.insert(call.callee, assumed);
                        }
                        if closed(&call.callee) && !callee.unused.is_empty() {
                            let mut unused = callee.unused.clone();
                            unused.sort_unstable();
                            unused.dedup();
                            dead.insert(call.callee, unused);
                        }
                    }
                }
                let imports = Imports {
                    imported,
                    constants: constants.get(key).cloned().unwrap_or_default(),
                    callees: callees.into_iter().collect(),
                    dead: dead.into_iter().collect(),
                };
                decided.insert(*key, imports);
            }
            Ok(decided)
        }

        fn check(summaries: &BTreeMap<InstanceKey, Summary>) -> Result<(), ImportError> {
            for (at, summary) in summaries {
                if summary.key != *at {
                    return Err(ImportError::Key { at: *at, says: summary.key });
                }
                if let Some(&param) = summary.unused.iter().find(|it| **it >= summary.params) {
                    return Err(ImportError::Unused { function: *at, param, params: summary.params });
                }
                for call in &summary.calls {
                    let Some(callee) = summaries.get(&call.callee) else { continue };
                    let args = u32::try_from(call.args.len()).unwrap_or(u32::MAX);
                    if args > callee.params {
                        return Err(ImportError::Args {
                            caller: *at,
                            callee: call.callee,
                            args,
                            params: callee.params,
                        });
                    }
                }
            }
            Ok(())
        }

        /// What `importer` imports, as ThinLTO's `computeImportForFunction` chooses: each call's
        /// callee if its body fits the room its hint gives, shrunk by [`Limits::evolution`] at each
        /// call deeper, and then what that callee calls, breadth first, each callee once, until
        /// [`Limits::depth`] or [`Limits::budget`]. Sorted.
        fn imported(
            summaries: &BTreeMap<InstanceKey, Summary>,
            importer: &Summary,
            limits: &Limits,
        ) -> Vec<InstanceKey> {
            let mut seen = BTreeSet::from([importer.key]);
            let mut taken = BTreeSet::new();
            let mut spent: u64 = 0;
            let mut next: VecDeque<(InstanceKey, f64, u32)> =
                importer.calls.iter().map(|it| (it.callee, 1.0, 0)).collect();
            while let Some((key, share, depth)) = next.pop_front() {
                // Breadth first, a callee is first met where its room is the largest it gets: one
                // that does not fit there fits nowhere deeper.
                if !seen.insert(key) {
                    continue;
                }
                let Some(callee) = summaries.get(&key) else { continue };
                let Some(room) = limits.room(callee.hint) else { continue };
                if f64::from(callee.size) > f64::from(room) * share
                    || spent + u64::from(callee.size) > u64::from(limits.budget)
                {
                    continue;
                }
                spent += u64::from(callee.size);
                taken.insert(key);
                if depth + 1 < limits.depth {
                    let share = share * limits.evolution;
                    next.extend(callee.calls.iter().map(|it| (it.callee, share, depth + 1)));
                }
            }
            taken.into_iter().collect()
        }

        /// Per function whose callers all have summaries, the parameters every call site of the
        /// build passes one integer constant for, sorted.
        fn constants(
            summaries: &BTreeMap<InstanceKey, Summary>,
            closed: &dyn Fn(&InstanceKey) -> bool,
        ) -> BTreeMap<InstanceKey, Vec<(u32, Arg)>> {
            // Per callee and parameter, what its call sites pass: `None` once two disagree or one
            // is unknown.
            let mut passed: BTreeMap<InstanceKey, Vec<Option<Arg>>> = BTreeMap::new();
            for caller in summaries.values() {
                for call in &caller.calls {
                    if !closed(&call.callee) || call.count == 0 {
                        continue;
                    }
                    let params = summaries[&call.callee].params as usize;
                    let seen = passed.entry(call.callee).or_insert_with(|| {
                        (0..params)
                            .map(|at| match call.args.get(at) {
                                Some(arg @ Arg::Int { .. }) => Some(*arg),
                                _ => None,
                            })
                            .collect()
                    });
                    for (at, known) in seen.iter_mut().enumerate() {
                        if *known != call.args.get(at).copied() {
                            *known = None;
                        }
                    }
                }
            }
            passed
                .into_iter()
                .map(|(key, args)| {
                    let known = args
                        .into_iter()
                        .enumerate()
                        .filter_map(|(at, arg)| Some((at as u32, arg?)))
                        .collect::<Vec<_>>();
                    (key, known)
                })
                .filter(|(_, known)| !known.is_empty())
                .collect()
        }

        /// What each function may be assumed to be by its callers, its body's attributes met with
        /// its callees' bottom up over the call graph's strongly connected components, as LLVM's
        /// `function-attrs` infers them over an SCC: nounwind, readonly and readnone hold of a
        /// component when they hold of every member's body and of every callee outside it;
        /// willreturn and norecurse never hold of a member of a cycle. noreturn is the function's
        /// own, as inferred alone.
        fn attrs(summaries: &BTreeMap<InstanceKey, Summary>) -> BTreeMap<InstanceKey, Attrs> {
            let nodes: Vec<InstanceKey> = summaries.keys().copied().collect();
            let at: BTreeMap<InstanceKey, usize> =
                nodes.iter().enumerate().map(|(at, key)| (*key, at)).collect();
            let edges: Vec<Vec<usize>> = nodes
                .iter()
                .map(|key| {
                    let mut to: Vec<usize> =
                        summaries[key].calls.iter().filter_map(|it| at.get(&it.callee).copied()).collect();
                    to.sort_unstable();
                    to.dedup();
                    to
                })
                .collect();
            let mut known: Vec<Option<Attrs>> = vec![None; nodes.len()];
            for component in components(&edges) {
                let cyclic = component.len() > 1 || edges[component[0]].contains(&component[0]);
                let mut met = Attrs {
                    nounwind: true,
                    readonly: true,
                    readnone: true,
                    norecurse: !cyclic,
                    willreturn: !cyclic,
                    noreturn: false,
                };
                for &member in &component {
                    let summary = &summaries[&nodes[member]];
                    let body = summary.body;
                    met.nounwind &= body.nounwind;
                    met.readnone &= body.readnone;
                    met.readonly &= body.readonly || body.readnone;
                    met.norecurse &= body.norecurse;
                    met.willreturn &= body.willreturn;
                    for call in &summary.calls {
                        match at.get(&call.callee) {
                            Some(callee) if component.contains(callee) => {}
                            Some(callee) => {
                                let callee = known[*callee].expect("components come callees first");
                                met.nounwind &= callee.nounwind;
                                met.readnone &= callee.readnone;
                                met.readonly &= callee.readonly || callee.readnone;
                                met.norecurse &= callee.norecurse;
                                met.willreturn &= callee.willreturn;
                            }
                            None => {
                                met = Attrs::default();
                            }
                        }
                    }
                }
                for &member in &component {
                    let alone = summaries[&nodes[member]].attrs;
                    let readnone = met.readnone || alone.readnone;
                    known[member] = Some(Attrs {
                        nounwind: met.nounwind || alone.nounwind,
                        readonly: met.readonly || alone.readonly || readnone,
                        readnone,
                        norecurse: met.norecurse || alone.norecurse,
                        willreturn: met.willreturn || alone.willreturn,
                        noreturn: alone.noreturn,
                    });
                }
            }
            nodes.into_iter().zip(known).map(|(key, it)| (key, it.expect("every node is in a component"))).collect()
        }

        /// The strongly connected components of `edges`, each sorted, callees before callers
        /// (Tarjan's order).
        pub(super) fn components(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
            const UNSEEN: usize = usize::MAX;
            let mut index = vec![UNSEEN; edges.len()];
            let mut low = vec![0; edges.len()];
            let mut on_stack = vec![false; edges.len()];
            let mut stack = Vec::new();
            let mut found = Vec::new();
            let mut next = 0;
            for root in 0..edges.len() {
                if index[root] != UNSEEN {
                    continue;
                }
                index[root] = next;
                low[root] = next;
                next += 1;
                stack.push(root);
                on_stack[root] = true;
                let mut work: Vec<(usize, usize)> = vec![(root, 0)];
                while let Some(&(node, edge)) = work.last() {
                    if let Some(&to) = edges[node].get(edge) {
                        work.last_mut().expect("just read").1 += 1;
                        if index[to] == UNSEEN {
                            index[to] = next;
                            low[to] = next;
                            next += 1;
                            stack.push(to);
                            on_stack[to] = true;
                            work.push((to, 0));
                        } else if on_stack[to] {
                            low[node] = low[node].min(index[to]);
                        }
                        continue;
                    }
                    work.pop();
                    if let Some(&(parent, _)) = work.last() {
                        low[parent] = low[parent].min(low[node]);
                    }
                    if low[node] == index[node] {
                        let mut component = Vec::new();
                        loop {
                            let member = stack.pop().expect("the component's root is on the stack");
                            on_stack[member] = false;
                            component.push(member);
                            if member == node {
                                break;
                            }
                        }
                        component.sort_unstable();
                        found.push(component);
                    }
                }
            }
            found
        }
    }
}
