//! Merkle identities over a call graph with cycles, shared by every root that reaches a node.
//!
//! A node's identity is a function of the subgraph it reaches and of nothing else: not of the
//! root that asked, nor of the order queries ran in. Each graph has three queries per node: its
//! local part ([`Graph::local`]: its own content and its callees in site order), whether it is
//! on a cycle ([`Graph::cyclic`]), and its identity ([`Graph::id`]), which calls [`identity`].
//!
//! - A node on no cycle is hashed from its content and its callees' identities, each its own
//!   query, memoized, so every node is hashed once whatever reaches it.
//! - A node on a cycle is hashed with its whole strongly connected component as one unit: a
//!   structural Tarjan over the cyclic nodes from it finds the component, every callee outside
//!   it is asked for its identity (it cannot reach back, so no query cycle forms), and the
//!   component is hashed by [`component`]. Its members are put in a canonical order (each one's
//!   local hash: its content, the identities of its callees outside, and a placeholder for each
//!   inside, then its name, a stable hash of its key and arguments, which only orders members
//!   whose local hashes are equal). A member's identity is the component's hash and its place.
//!
//! A node on no cycle is a component of one with no edge into itself, and [`component`] is the
//! one formula for both, so a cycle detector that calls a node cyclic when it is not (a query
//! cycle through const eval makes every query on it a member) changes the path taken, never an
//! identity.
//!
//! The cycle detector is a query that asks every callee's and returns `false`, with
//! `cycle_result = true`: blaze_query gives every member of a query cycle the fallback, so it
//! is `true` exactly on the nodes of a cycle (and a self-loop), whatever order queries ran in.

use std::{alloc::Global, hash::Hash};

use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};

/// A callee of a member: another member of its component, by place, or a node outside it, by
/// identity.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Ref {
    Internal(u32),
    External(ContentId),
}

/// One member of a component: its own content, its name, and its callees in site order.
pub(crate) struct Member<'a> {
    pub(crate) content: ContentId,
    pub(crate) name: ContentId,
    pub(crate) callees: &'a [Ref],
}

/// The identity of each member of one component, in the members' order, under `domain`.
pub(crate) fn component(domain: &'static str, members: &[Member<'_>]) -> Vec<ContentId> {
    fn edges(h: &mut Hasher, callees: &[Ref], place: Option<&[u32]>) {
        h.update(&(callees.len() as u32).to_le_bytes());
        for callee in callees {
            match (*callee, place) {
                (Ref::Internal(at), Some(place)) => {
                    h.update(&[0]).update(&place[at as usize].to_le_bytes())
                }
                (Ref::Internal(_), None) => h.update(&[0]),
                (Ref::External(id), _) => h.update(&[1]).id(&id),
            };
        }
    }
    let [one] = members else {
        // alloc: one component's members, each hashed once per component
        let local: Vec<ContentId> = members
            .iter()
            .map(|member| {
                let mut h = Hasher::new(domain);
                h.update(&[0]).id(&member.content);
                edges(&mut h, member.callees, None);
                h.finish()
            })
            .collect();
        let mut order: Vec<u32> = (0..members.len() as u32).collect(); // alloc: as `local`
        order.sort_unstable_by_key(|&at| (local[at as usize], members[at as usize].name));
        let mut place = vec![0u32; members.len()]; // alloc: as `local`
        for (at, &member) in order.iter().enumerate() {
            place[member as usize] = at as u32;
        }
        let whole = hash_whole(
            domain,
            order.iter().map(|&at| &members[at as usize]),
            members.len(),
            &place,
            edges,
        );
        // alloc: as `local`
        return place.iter().map(|&place| member_id(domain, &whole, place)).collect();
    };
    let whole = hash_whole(domain, [one].into_iter(), 1, &[0], edges);
    vec![member_id(domain, &whole, 0)] // alloc: one identity
}

fn hash_whole<'m, 'a: 'm>(
    domain: &'static str,
    ordered: impl Iterator<Item = &'m Member<'a>>,
    len: usize,
    place: &[u32],
    edges: fn(&mut Hasher, &[Ref], Option<&[u32]>),
) -> ContentId {
    let mut h = Hasher::new(domain);
    h.update(&[1]).update(&(len as u32).to_le_bytes());
    for member in ordered {
        h.id(&member.content);
        edges(&mut h, member.callees, Some(place));
    }
    h.finish()
}

fn member_id(domain: &'static str, whole: &ContentId, place: u32) -> ContentId {
    let mut h = Hasher::new(domain);
    h.update(&[2]).id(whole).update(&place.to_le_bytes());
    h.finish()
}

/// What a node's identity reads of it: its own content and its callees, in site order.
pub(crate) struct Local<'g, N> {
    pub(crate) content: ContentId,
    pub(crate) callees: &'g [N],
}

impl<N> Clone for Local<'_, N> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<N> Copy for Local<'_, N> {}

/// One graph of identities: its nodes, their queries, and how its errors name a path.
pub(crate) trait Graph<'g> {
    type Node: Clone + Eq + Hash + 'g;
    type Error;
    /// The node's own content and callees: its local query.
    fn local(&self, node: &Self::Node) -> Result<Local<'g, Self::Node>, Self::Error>;
    /// Whether the node is on a cycle: its cycle detector.
    fn cyclic(&self, node: &Self::Node) -> bool;
    /// The node's identity: its identity query, which calls [`identity`].
    fn id(&self, node: &Self::Node) -> Result<ContentId, Self::Error>;
    /// A stable hash of the node's key and arguments, never an interned id: it only orders the
    /// members of one component whose local hashes are equal.
    fn name(&self, node: &Self::Node) -> Result<ContentId, Self::Error>;
    /// `error`, of a node `node` reaches: with `node` first on its path.
    fn through(&self, node: &Self::Node, error: Self::Error) -> Self::Error;
    /// Past `fuel` nodes of one component, at `node` (rule 18).
    fn exhausted(&self, node: &Self::Node, fuel: usize) -> Self::Error;
}

/// The identity of `node` in `graph`, under `domain`, reading at most `fuel` nodes of its
/// component. What [`Graph::id`] runs.
pub(crate) fn identity<'g, G: Graph<'g>>(
    graph: &G,
    domain: &'static str,
    node: &G::Node,
    fuel: usize,
) -> Result<ContentId, G::Error> {
    let local = graph.local(node)?;
    if !graph.cyclic(node) {
        // alloc: one node's callees, its query's miss, memoized after
        let callees = local
            .callees
            .iter()
            .map(|callee| {
                graph.id(callee).map(Ref::External).map_err(|error| graph.through(node, error))
            })
            .collect::<Result<Vec<Ref>, G::Error>>()?;
        let member =
            Member { content: local.content, name: ContentId::default(), callees: &callees };
        return Ok(component(domain, &[member])[0]);
    }
    let members = component_of(graph, node, local, fuel)?;
    let mut at: HashMap<&G::Node, u32, Global> = HashMap::new_in(Global);
    for (place, (member, _)) in members.iter().enumerate() {
        at.insert(member, place as u32);
    }
    // alloc: one component's callees, as above
    let mut refs: Vec<Vec<Ref>> = Vec::with_capacity(members.len());
    for (member, local) in &members {
        let mut callees = Vec::with_capacity(local.callees.len()); // alloc: as `refs`
        for callee in local.callees {
            callees.push(match at.get(callee) {
                Some(&place) => Ref::Internal(place),
                None => Ref::External(graph.id(callee).map_err(|error| {
                    let error = graph.through(member, error);
                    if member == node { error } else { graph.through(node, error) }
                })?),
            });
        }
        refs.push(callees);
    }
    let named = members.len() > 1;
    let mut parts = Vec::with_capacity(members.len()); // alloc: as `refs`
    for ((member, local), callees) in members.iter().zip(&refs) {
        let name = match named {
            true => graph.name(member).map_err(|error| graph.through(node, error))?,
            false => ContentId::default(),
        };
        parts.push(Member { content: local.content, name, callees });
    }
    // `node` is the first member: the component's root in the walk from it.
    Ok(component(domain, &parts)[0])
}

struct Visit<'g, N> {
    node: N,
    local: Local<'g, N>,
    low: u32,
    on_stack: bool,
}

/// The strongly connected component of `start`, which is on a cycle, with `start` first: an
/// iterative Tarjan over the cyclic nodes it reaches (a node not on a cycle is its own
/// component, and outside), at most `fuel` of them.
fn component_of<'g, G: Graph<'g>>(
    graph: &G,
    start: &G::Node,
    first: Local<'g, G::Node>,
    fuel: usize,
) -> Result<Vec<(G::Node, Local<'g, G::Node>)>, G::Error> {
    // A node's index is its place in `visits`; every node visited is cyclic.
    // alloc: one component's walk, its query's miss, memoized after
    let mut visits: Vec<Visit<'g, G::Node>> = Vec::new();
    let mut at: HashMap<G::Node, u32, Global> = HashMap::new_in(Global);
    let mut stack: Vec<u32> = Vec::new(); // alloc: as `visits`
    let mut frames: Vec<(u32, usize)> = Vec::new(); // alloc: as `visits`
    visits.push(Visit { node: start.clone(), local: first, low: 0, on_stack: true }); // clone: a key, reference counts
    at.insert(start.clone(), 0); // clone: as above
    stack.push(0);
    frames.push((0, 0));
    let mut root: Option<Vec<u32>> = None;
    while let Some(&(v, cursor)) = frames.last() {
        if let Some(callee) = visits[v as usize].local.callees.get(cursor) {
            frames.last_mut().expect("a frame").1 += 1;
            if let Some(&w) = at.get(callee) {
                if visits[w as usize].on_stack {
                    let visit = &mut visits[v as usize];
                    visit.low = visit.low.min(w);
                }
                continue;
            }
            if !graph.cyclic(callee) {
                continue;
            }
            if visits.len() == fuel {
                return Err(graph.through(start, graph.exhausted(callee, fuel)));
            }
            let local = graph.local(callee).map_err(|error| graph.through(start, error))?;
            let w = visits.len() as u32;
            visits.push(Visit { node: callee.clone(), local, low: w, on_stack: true }); // clone: as above
            at.insert(callee.clone(), w); // clone: as above
            stack.push(w);
            frames.push((w, 0));
            continue;
        }
        frames.pop();
        let low = visits[v as usize].low;
        if let Some(&(parent, _)) = frames.last() {
            let parent = &mut visits[parent as usize];
            parent.low = parent.low.min(low);
        }
        if low == v {
            let from =
                stack.iter().rposition(|&it| it == v).expect("a component's root is on the stack");
            let members = stack.split_off(from); // alloc: one component
            for &member in &members {
                visits[member as usize].on_stack = false;
            }
            if v == 0 {
                root = Some(members);
            }
        }
    }
    let members = root.expect("the start roots the last component");
    debug_assert_eq!(members.first(), Some(&0));
    // alloc: as `visits`
    Ok(members
        .into_iter()
        .map(|member| {
            let visit = &visits[member as usize];
            (visit.node.clone(), visit.local) // clone: a key, reference counts
        })
        .collect())
}
