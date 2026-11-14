//! Text-closure keys: what a body's key is made of, from text alone, before any work.
//!
//! The rules are Relax's (compiler/src/cache/deps.rs, MIT, by its authors), copied with credit:
//!
//! - A node is an item's piece: its text with bodies cut, except bodies that count as interface
//!   (an inline or generic fn's, a const fn's, an async fn's, one that returns `impl Trait`).
//! - A node's names are the identifiers in its piece, hashed ([`name_of`]); a collision makes
//!   more dependencies, never fewer.
//! - A node is found under its keys: the names it is declared as. A trait impl is found under its
//!   self type's names, or under its trait's name when the self type is generic, primitive, an
//!   alias or not a path.
//! - A node depends on every node found under one of its names in its universe (its crate and
//!   the crates it depends on, transitively), so an absent name that appears later is an edge.
//! - Its closure is the hash of its strongly connected component's members' own content and the
//!   closures of the components they reach (a Merkle closure over the condensation).
//! - "Always" nodes (glob and `as _` imports, inner attributes, unexpanded item macros) are in
//!   every key of their crate and of their crate's dependents.
//! - A "coarse" node (a macro call outside item position that is not on the quiet list) has its
//!   crate's whole print as its own content: an edit anywhere in that crate moves it.
//!
//! The identity of the crate is in every node's own content: two crates with equal text key
//! apart.

#![blaze::sans_io]

use core::{alloc::Allocator, fmt};

use blaze_ids::{ContentId, Hasher};

/// A name's hash: FNV-1a over its bytes, then mixed (Relax's `name_hash`). Never 0.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NameHash(pub u64);

pub fn name_of(text: &[u8]) -> NameHash {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in text {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    NameHash(hash.max(1))
}

/// One item's piece, as the file image gives it.
#[derive(Clone, Copy, Debug)]
pub struct Node<'a> {
    pub piece: &'a [u8],
    /// The names the piece mentions.
    pub names: &'a [NameHash],
    /// The names the node is found under.
    pub keys: &'a [NameHash],
    pub always: bool,
    pub coarse: bool,
}

/// One crate's nodes, its identity (name, version, source) and its dependencies by index, each
/// below its own.
#[derive(Clone, Copy, Debug)]
pub struct CrateNodes<'a> {
    pub identity: ContentId,
    pub nodes: &'a [Node<'a>],
    pub deps: &'a [u32],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GraphError {
    /// Crate `krate` names a dependency at or above its own index.
    DepOrder { krate: u32, dep: u32 },
    /// An index past the graph.
    NoCrate { krate: u32 },
    NoNode { krate: u32, node: u32 },
    /// More than `u32::MAX` nodes or edges.
    TooLarge,
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphError::DepOrder { krate, dep } => {
                write!(f, "crate {krate} depends on crate {dep}, not below it")
            }
            GraphError::NoCrate { krate } => write!(f, "no crate {krate} in the text graph"),
            GraphError::NoNode { krate, node } => write!(f, "no node {node} in crate {krate}"),
            GraphError::TooLarge => write!(f, "a text graph of more than 2^32 nodes or edges"),
        }
    }
}

const UNSET: u32 = u32::MAX;

/// The closures of every node of a set of crates.
///
/// The invariant: `closure(c, n)` changes when, and only when, the own content of a node that
/// `n` reaches by names changes, or a node appears or goes under a name a reached node mentions,
/// in the universe of the node's crate; equal text in equal universes gives equal closures.
pub struct Graph<A: Allocator + Clone> {
    /// Per crate: its first node's global index; one more entry at the end.
    first: Vec<u32, A>,
    /// Per crate, a bitset row of the crates in its universe.
    universe: Vec<u64, A>,
    words: usize,
    /// Every (key, node) pair, sorted.
    found: Vec<(NameHash, u32), A>,
    node_crate: Vec<u32, A>,
    closure: Vec<ContentId, A>,
    always: Vec<ContentId, A>,
}

impl<A: Allocator + Clone> Graph<A> {
    pub fn build(crates: &[CrateNodes<'_>], alloc: A) -> Result<Graph<A>, GraphError> {
        let count = crates.len();
        let words = count.div_ceil(64).max(1);
        let mut first = Vec::with_capacity_in(count + 1, alloc.clone());
        let mut universe = Vec::new_in(alloc.clone());
        universe.resize(count * words, 0u64);
        let mut node_crate = Vec::new_in(alloc.clone());
        let mut total: u64 = 0;
        for (index, krate) in crates.iter().enumerate() {
            first.push(u32::try_from(total).map_err(|_| GraphError::TooLarge)?);
            total += krate.nodes.len() as u64;
            node_crate.extend(core::iter::repeat_n(index as u32, krate.nodes.len()));
            universe[index * words + index / 64] |= 1u64 << (index % 64);
            for &dep in krate.deps {
                if dep as usize >= index {
                    return Err(GraphError::DepOrder { krate: index as u32, dep });
                }
                for word in 0..words {
                    let bits = universe[dep as usize * words + word];
                    universe[index * words + word] |= bits;
                }
            }
        }
        let total = u32::try_from(total).map_err(|_| GraphError::TooLarge)?;
        first.push(total);
        let node = |global: u32| {
            let krate = node_crate[global as usize] as usize;
            crates[krate].nodes[(global - first[krate]) as usize]
        };

        let mut found = Vec::new_in(alloc.clone());
        for global in 0..total {
            found.extend(node(global).keys.iter().map(|&key| (key, global)));
        }
        found.sort_unstable();

        // Each crate's print (every piece, for coarse nodes) and its always closure.
        let mut print = Vec::with_capacity_in(count, alloc.clone());
        let mut always = Vec::with_capacity_in(count, alloc.clone());
        for krate in crates {
            let mut hasher = Hasher::new("blaze-keyed crate print");
            hasher.id(&krate.identity);
            krate.nodes.iter().for_each(|node| {
                hasher.part(node.piece);
            });
            print.push(hasher.finish());
            let mut hasher = Hasher::new("blaze-keyed always");
            hasher.id(&krate.identity);
            krate.nodes.iter().filter(|node| node.always).for_each(|node| {
                hasher.part(node.piece);
            });
            krate.deps.iter().for_each(|&dep| {
                hasher.id(&always[dep as usize]);
            });
            always.push(hasher.finish());
        }

        let mut local = Vec::with_capacity_in(total as usize, alloc.clone());
        for global in 0..total {
            let krate = node_crate[global as usize] as usize;
            let this = node(global);
            let mut hasher = Hasher::new("blaze-keyed node");
            hasher.id(&crates[krate].identity).id(&always[krate]).update(&[this.coarse as u8]);
            match this.coarse {
                true => hasher.part(this.piece).id(&print[krate]),
                false => hasher.part(this.piece),
            };
            local.push(hasher.finish());
        }

        // Edges, compressed: a node to every node found under one of its names that its crate
        // can see.
        let sees = |krate: usize, other: usize| {
            universe[krate * words + other / 64] >> (other % 64) & 1 == 1
        };
        let mut starts = Vec::with_capacity_in(total as usize + 1, alloc.clone());
        let mut targets = Vec::new_in(alloc.clone());
        for global in 0..total {
            starts.push(u32::try_from(targets.len()).map_err(|_| GraphError::TooLarge)?);
            let krate = node_crate[global as usize] as usize;
            for name in node(global).names {
                let from = found.partition_point(|(key, _)| key < name);
                for &(key, target) in &found[from..] {
                    if key != *name {
                        break;
                    }
                    if target != global && sees(krate, node_crate[target as usize] as usize) {
                        targets.push(target);
                    }
                }
            }
        }
        starts.push(u32::try_from(targets.len()).map_err(|_| GraphError::TooLarge)?);

        let closure = tarjan(&starts, &targets, &local, alloc)?;
        Ok(Graph { first, universe, words, found, node_crate, closure, always })
    }

    fn global(&self, krate: u32, node: u32) -> Result<u32, GraphError> {
        let start = *self.first.get(krate as usize).ok_or(GraphError::NoCrate { krate })?;
        let end = *self.first.get(krate as usize + 1).ok_or(GraphError::NoCrate { krate })?;
        match start + node < end {
            true => Ok(start + node),
            false => Err(GraphError::NoNode { krate, node }),
        }
    }

    /// The closure of node `node` of crate `krate`.
    pub fn closure(&self, krate: u32, node: u32) -> Result<ContentId, GraphError> {
        Ok(self.closure[self.global(krate, node)? as usize])
    }

    /// The closure of what `name` can resolve to from crate `krate`: the sorted, deduplicated
    /// closures of the nodes found under it in the crate's universe; `None` when there are none.
    pub fn name_closure<B: Allocator>(
        &self,
        krate: u32,
        name: NameHash,
        scratch: &mut Vec<ContentId, B>,
    ) -> Result<Option<ContentId>, GraphError> {
        if krate as usize + 1 >= self.first.len() {
            return Err(GraphError::NoCrate { krate });
        }
        scratch.clear();
        let from = self.found.partition_point(|(key, _)| *key < name);
        for &(key, target) in &self.found[from..] {
            if key != name {
                break;
            }
            let other = self.node_crate[target as usize] as usize;
            let row = krate as usize * self.words;
            if self.universe[row + other / 64] >> (other % 64) & 1 == 1 {
                scratch.push(self.closure[target as usize]);
            }
        }
        if scratch.is_empty() {
            return Ok(None);
        }
        scratch.sort_unstable();
        scratch.dedup();
        let mut hasher = Hasher::new("blaze-keyed name closure");
        hasher.update(&name.0.to_le_bytes());
        scratch.iter().for_each(|id| {
            hasher.id(id);
        });
        Ok(Some(hasher.finish()))
    }

    /// Crate `krate`'s always closure.
    pub fn always(&self, krate: u32) -> Result<ContentId, GraphError> {
        self.always.get(krate as usize).copied().ok_or(GraphError::NoCrate { krate })
    }
}

/// Tarjan's components over the edges `starts`/`targets`, iterative, and each node's closure:
/// a component's members' own content sorted, then the closures of the components it reaches,
/// sorted and deduplicated. Tarjan emits a component after every component it reaches, so each
/// closure is made from finished ones.
fn tarjan<A: Allocator + Clone>(
    starts: &[u32],
    targets: &[u32],
    local: &[ContentId],
    alloc: A,
) -> Result<Vec<ContentId, A>, GraphError> {
    let total = local.len();
    let new = |fill: u32| {
        let mut vec = Vec::with_capacity_in(total, alloc.clone());
        vec.resize(total, fill);
        vec
    };
    let (mut index, mut low, mut component) = (new(UNSET), new(UNSET), new(UNSET));
    let mut on_stack = Vec::with_capacity_in(total, alloc.clone());
    on_stack.resize(total, false);
    let mut stack: Vec<u32, A> = Vec::new_in(alloc.clone());
    let mut calls: Vec<(u32, u32), A> = Vec::new_in(alloc.clone());
    let mut component_closure: Vec<ContentId, A> = Vec::new_in(alloc.clone());
    let mut members: Vec<u32, A> = Vec::new_in(alloc.clone());
    let mut parts: Vec<ContentId, A> = Vec::new_in(alloc.clone());
    let mut reached: Vec<ContentId, A> = Vec::new_in(alloc.clone());
    let mut counter: u32 = 0;
    for root in 0..total as u32 {
        if index[root as usize] != UNSET {
            continue;
        }
        index[root as usize] = counter;
        low[root as usize] = counter;
        counter += 1;
        stack.push(root);
        on_stack[root as usize] = true;
        calls.push((root, starts[root as usize]));
        while let Some(depth) = calls.len().checked_sub(1) {
            let (at, edge) = calls[depth];
            if edge < starts[at as usize + 1] {
                calls[depth].1 += 1;
                let to = targets[edge as usize];
                if index[to as usize] == UNSET {
                    index[to as usize] = counter;
                    low[to as usize] = counter;
                    counter += 1;
                    stack.push(to);
                    on_stack[to as usize] = true;
                    calls.push((to, starts[to as usize]));
                } else if on_stack[to as usize] {
                    low[at as usize] = low[at as usize].min(index[to as usize]);
                }
                continue;
            }
            calls.pop();
            if let Some(&(parent, _)) = calls.last() {
                low[parent as usize] = low[parent as usize].min(low[at as usize]);
            }
            if low[at as usize] != index[at as usize] {
                continue;
            }
            let id = u32::try_from(component_closure.len()).map_err(|_| GraphError::TooLarge)?;
            members.clear();
            while let Some(member) = stack.pop() {
                on_stack[member as usize] = false;
                component[member as usize] = id;
                members.push(member);
                if member == at {
                    break;
                }
            }
            parts.clear();
            reached.clear();
            for &member in members.iter() {
                parts.push(local[member as usize]);
                let edges = starts[member as usize]..starts[member as usize + 1];
                for &to in &targets[edges.start as usize..edges.end as usize] {
                    let other = component[to as usize];
                    if other != id {
                        reached.push(component_closure[other as usize]);
                    }
                }
            }
            parts.sort_unstable();
            reached.sort_unstable();
            reached.dedup();
            let mut hasher = Hasher::new("blaze-keyed closure");
            hasher.update(&(parts.len() as u64).to_le_bytes());
            parts.iter().for_each(|part| {
                hasher.id(part);
            });
            hasher.update(&(reached.len() as u64).to_le_bytes());
            reached.iter().for_each(|part| {
                hasher.id(part);
            });
            component_closure.push(hasher.finish());
        }
    }
    let mut closure = Vec::with_capacity_in(total, alloc);
    closure.extend(component.iter().map(|&id| component_closure[id as usize]));
    Ok(closure)
}
