//! Which crate uses which capability, with one shortest witness each: the pure core the trust
//! facts run over the graph a reach makes ([`Graph`]), so a property test can run it over
//! generated graphs against a brute-force model (`trust::tests`).
//!
//! The rule: a capability a node uses is its own crate's, the crate where the call into the
//! toolchain or the thing itself is written, never the caller's; a crate of the workspace uses
//! nothing the trust lints hold it to. Each (crate, capability) keeps the witness with the
//! fewest calls from a root: the walk is breadth-first from every root at once, so the first
//! node that shows it is one of the nearest, and of those the first met.

use std::alloc::Global;

use super::capability::Capability;

/// The instances a set of roots reaches, as the core reads them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Graph {
    /// Whether each crate, by index, is the workspace's.
    pub members: Box<[bool]>,
    /// How many of `nodes` are roots (the first ones).
    pub roots: u32,
    pub nodes: Box<[Node]>,
}

/// One instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// Its crate, by index in [`Graph::members`].
    pub krate: u32,
    /// What it uses, each with what it does that uses it.
    pub uses: Box<[(Capability, Box<str>)]>,
    /// The nodes it calls, by index.
    pub calls: Box<[u32]>,
}

/// One capability of one crate, with its witness: the nodes from a root to the node that uses
/// it, and what that node does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Use {
    pub krate: u32,
    pub capability: Capability,
    pub chain: Box<[u32]>,
    pub what: Box<str>,
}

#[cfg(test)]
thread_local! {
    /// The bug the property test's control plants: a capability attributed to the caller's
    /// crate, not the crate whose node uses it.
    pub(super) static TO_CALLER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Each (crate, capability) a crate outside the workspace uses through the nodes the roots
/// reach, once, with one shortest witness; sorted by crate, then capability.
pub fn attribute(graph: &Graph) -> Vec<Use> {
    let count = graph.nodes.len();
    let mut parent: Vec<Option<u32>> = Vec::with_capacity_in(count, Global);
    parent.resize(count, None);
    let mut met: Vec<bool> = Vec::with_capacity_in(count, Global);
    met.resize(count, false);
    let mut queue: Vec<u32> = Vec::with_capacity_in(count, Global);
    for root in 0..graph.roots.min(u32::try_from(count).unwrap_or(u32::MAX)) {
        met[root as usize] = true;
        queue.push(root);
    }
    let mut found: Vec<Use> = Vec::new_in(Global);
    let mut next = 0;
    while let Some(&at) = queue.get(next) {
        next += 1;
        let node = &graph.nodes[at as usize];
        let krate = attributed(graph, &parent, at);
        if !graph.members.get(krate as usize).copied().unwrap_or(true) {
            for (capability, what) in &node.uses {
                if found.iter().any(|it| it.krate == krate && it.capability == *capability) {
                    continue;
                }
                let chain = chain_to(&parent, at);
                found.push(Use { krate, capability: *capability, chain, what: what.clone() });
            }
        }
        for &callee in &node.calls {
            let Some(seen) = met.get_mut(callee as usize) else { continue };
            if !*seen {
                *seen = true;
                parent[callee as usize] = Some(at);
                queue.push(callee);
            }
        }
    }
    found.sort_by_key(|it| (it.krate, it.capability));
    found
}

/// The crate a capability of the node `at` is attributed to: its own.
fn attributed(graph: &Graph, parent: &[Option<u32>], at: u32) -> u32 {
    let own = graph.nodes[at as usize].krate;
    #[cfg(test)]
    if TO_CALLER.get() {
        return parent[at as usize].map_or(own, |caller| graph.nodes[caller as usize].krate);
    }
    let _ = parent;
    own
}

/// The nodes from a root to `at`, the root first.
fn chain_to(parent: &[Option<u32>], at: u32) -> Box<[u32]> {
    let mut chain: Vec<u32> = Vec::new_in(Global);
    let mut step = Some(at);
    while let Some(it) = step {
        chain.push(it);
        step = parent[it as usize];
    }
    chain.reverse();
    chain.into_boxed_slice()
}
