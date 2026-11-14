//! Which callees an instance takes in, and so its instance key: the two-step rule.
//!
//! **Step one, the plan.** An instance's [`Plan`](crate::record::Plan) (found by its plan
//! key, which needs no other instance) lists its calls in MIR order and its own facts
//! ([`OwnFacts`]): the size class of its own body before any inlining, its `#[inline]`
//! attribute and whether it is cold. These are facts of the callee alone, records found
//! before anything is lowered.
//!
//! **Step two, the decision.** A call `a -> c` is inlined exactly when (1) `c`'s own facts pass
//! the tier's rule ([`inlines`]), and (2) `a` and `c` are not in one strongly connected
//! component of the graph of such calls (a recursion is never inlined into itself, at any
//! depth). The decision reads the caller's plan and the callees' plans, nothing lowered, so
//! [`Keys::resolve`] gives the instance key before the work: `key(a) = H(plan_key(a), [(c, key(c))
//! for c inlined into a])`. Cutting whole components (never "the edge that closes the cycle
//! on this path") makes the decision a function of the graph, not of the order it was walked
//! in: the key of `c` is the same whichever caller asked first.
//!
//! **The tier's rule** ([`inlines`]):
//!
//! | tier | inlined |
//! |---|---|
//! | `Edit` | only `#[inline(always)]`: every other inline would turn a one-function edit into a remake of each caller, for run time the edit loop does not need |
//! | `Release` | `#[inline(always)]`; `#[inline]` up to [`SizeClass::Medium`] (rustc's hint threshold, 100); otherwise up to [`SizeClass::Small`] (rustc's threshold, 50) unless cold; never `#[inline(never)]` |
//!
//! **Cross-function optimization at the top tier.** At `Release` an instance takes in callee
//! bodies across functions and crates (link-time optimization, at function grain): its key holds
//! the key of every body it took in, at every depth, so an edit of an inlined callee remakes
//! exactly its inliners, transitively through inlining only (`K` makes: the edited instance, if
//! it is placed, and every instance whose inline tree holds it; its plain callers are not
//! remade). The decision reads only facts that are records of the callee itself (size class,
//! `#[inline]`, `#[cold]`); a call-site count is not used, as it is a fact of the whole program
//! (it would make a callee's inlining depend on code far from it, and every caller's key move
//! when a caller is added). The callee is taken in at the caller's tier: its key in the
//! caller's is its key at that tier.
//!
//! It differs from rustc's MIR inliner (`hir-ty-mir/src/mir/optimize/inline.rs`) on purpose
//! (rule useful-not-parity): rustc measures the callee's *optimized* body (itself after
//! inlining) and scales the threshold by the caller's own facts, so its decision needs work
//! done first; here it needs only records. No program's meaning depends on which calls are
//! inlined.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;

use crate::key::{InstanceKey, PlanKey, Symbol, Tier, instance_key};
use crate::Plant;

/// An instance's `#[inline]` attribute.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum InlineAttr {
    None,
    Hint,
    Always,
    Never,
}

/// The size of an instance's own body before any inlining, by the MIR inliner's cost.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum SizeClass {
    /// At most 30: a forwarder (rustc's forwarder threshold).
    Tiny,
    /// At most 50 (rustc's threshold).
    Small,
    /// At most 100 (rustc's threshold for `#[inline]`).
    Medium,
    Large,
}

impl SizeClass {
    pub fn of_cost(cost: u32) -> SizeClass {
        match cost {
            0..=30 => SizeClass::Tiny,
            31..=50 => SizeClass::Small,
            51..=100 => SizeClass::Medium,
            _ => SizeClass::Large,
        }
    }
}

/// An instance's own facts, from its plan: what a caller's inline decision reads of it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct OwnFacts {
    pub size: SizeClass,
    pub attr: InlineAttr,
    pub cold: bool,
}

/// Whether a callee with `facts` is inlined into its callers at `tier` (the component rule
/// aside).
pub fn inlines(tier: Tier, facts: &OwnFacts) -> bool {
    match (tier, facts.attr) {
        (_, InlineAttr::Never) => false,
        (_, InlineAttr::Always) => true,
        (Tier::Edit, _) => false,
        (Tier::Release, InlineAttr::Hint) => facts.size <= SizeClass::Medium,
        (Tier::Release, InlineAttr::None) => facts.size <= SizeClass::Small && !facts.cold,
    }
}

/// An instance's plan as [`Keys::resolve`] reads it: its plan key, its own facts, and its calls when
/// the reader still holds them (a walk drops the calls of an instance no caller inlines once
/// it is keyed).
#[derive(Clone, Copy, Debug)]
pub struct PlanRef<'a> {
    pub key: PlanKey,
    pub facts: OwnFacts,
    pub calls: Option<&'a [Symbol]>,
}

/// The plans [`Keys::resolve`] reads.
pub trait PlanView {
    fn plan(&self, symbol: &Symbol) -> Option<PlanRef<'_>>;
}

/// An instance's key and the callees it takes in directly, with theirs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Keyed {
    pub key: InstanceKey,
    pub took_in: Vec<(Symbol, InstanceKey), Global>,
}

/// The instance keys known so far, each with its direct inlinees: a key is a function of the
/// plans alone, so it is kept once made and is the same whichever instance asked first.
pub struct Keys {
    tier: Tier,
    plant: Plant,
    known: blaze_hash::HashMap<Symbol, Keyed, Global>,
}

/// What [`Keys::resolve`] found.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Resolution {
    Keyed(InstanceKey),
    /// The plans of these symbols (sorted, each once) are needed first.
    Needs(Vec<Symbol, Global>),
}

/// One inline edge of an instance's inline tree: `callee`, keyed `key`, is taken into
/// `caller`'s code.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct InlineEdge {
    pub caller: Symbol,
    pub callee: Symbol,
    pub key: InstanceKey,
}

#[derive(Clone, Copy)]
enum Edge {
    /// To a node of this resolution, by its index.
    Node(usize),
    /// To an instance keyed before.
    Known(Symbol),
}

impl Keys {
    pub fn new(tier: Tier, plant: Plant) -> Keys {
        Keys { tier, plant, known: blaze_hash::HashMap::new_in(Global) }
    }

    pub fn tier(&self) -> Tier {
        self.tier
    }

    pub fn get(&self, symbol: &Symbol) -> Option<&Keyed> {
        self.known.get(symbol)
    }

    pub fn len(&self) -> usize {
        self.known.len()
    }

    /// Every edge of `root`'s inline tree (the callees it takes in at any depth, each edge
    /// once), sorted: what the code generator is told to take in, and from which body.
    pub fn inline_tree(&self, root: &Symbol) -> Vec<InlineEdge, Global> {
        let mut edges = Vec::new_in(Global);
        let mut seen = blaze_hash::HashSet::new_in(Global);
        let mut stack = Vec::new_in(Global);
        stack.push(*root);
        seen.insert(*root);
        while let Some(caller) = stack.pop() {
            let Some(keyed) = self.known.get(&caller) else { continue };
            for (callee, key) in &keyed.took_in {
                edges.push(InlineEdge { caller, callee: *callee, key: *key });
                if seen.insert(*callee) {
                    stack.push(*callee);
                }
            }
        }
        edges.sort_unstable();
        edges.dedup();
        edges
    }

    /// The symbols `root` takes in at any depth, sorted, each once.
    pub fn took_in_closure(&self, root: &Symbol) -> Vec<Symbol, Global> {
        let mut out = Vec::new_in(Global);
        for edge in self.inline_tree(root) {
            out.push(edge.callee);
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Resolves `root`'s instance key from `view`, keying every instance of its inline closure
    /// on the way; or the plans still needed. See the module's two-step rule.
    pub fn resolve(&mut self, root: &Symbol, view: &impl PlanView) -> Resolution {
        if let Some(keyed) = self.known.get(root) {
            return Resolution::Keyed(keyed.key);
        }
        let tier = self.tier;
        // The closure: the root and every instance an inline call reaches from it, with the
        // inline calls as edges. Instances keyed before are leaves.
        let mut nodes = Vec::new_in(Global);
        let mut edges: Vec<Vec<Edge, Global>, Global> = Vec::new_in(Global);
        let mut index = blaze_hash::HashMap::new_in(Global);
        let mut needs = Vec::new_in(Global);
        let mut stack = Vec::new_in(Global);
        nodes.push(*root);
        edges.push(Vec::new_in(Global));
        index.insert(*root, 0usize);
        stack.push(0usize);
        while let Some(at) = stack.pop() {
            let symbol = nodes[at];
            let Some(plan) = view.plan(&symbol) else {
                needs.push(symbol);
                continue;
            };
            let Some(calls) = plan.calls else {
                needs.push(symbol);
                continue;
            };
            for callee in calls {
                if *callee == symbol {
                    // A direct recursion: its own component, never inlined.
                    continue;
                }
                let Some(callee_plan) = view.plan(callee) else {
                    needs.push(*callee);
                    continue;
                };
                if !inlines(tier, &callee_plan.facts) {
                    continue;
                }
                if self.known.contains_key(callee) {
                    edges[at].push(Edge::Known(*callee));
                    continue;
                }
                let to = match index.get(callee) {
                    Some(to) => *to,
                    None => {
                        let to = nodes.len();
                        nodes.push(*callee);
                        edges.push(Vec::new_in(Global));
                        index.insert(*callee, to);
                        stack.push(to);
                        to
                    }
                };
                edges[at].push(Edge::Node(to));
            }
        }
        if !needs.is_empty() {
            needs.sort_unstable();
            needs.dedup();
            return Resolution::Needs(needs);
        }
        let components = components(&edges);
        // Tarjan gives each component after every component it reaches: callees first.
        let mut component_of = Vec::with_capacity_in(nodes.len(), Global);
        component_of.resize(nodes.len(), usize::MAX);
        for (number, members) in components.iter().enumerate() {
            for member in members {
                component_of[*member] = number;
            }
        }
        for (number, members) in components.iter().enumerate() {
            for member in members {
                let symbol = nodes[*member];
                let Some(plan) = view.plan(&symbol) else {
                    unreachable!("a plan read a moment ago is in the view");
                };
                let mut took_in = Vec::new_in(Global);
                for edge in &edges[*member] {
                    match edge {
                        Edge::Known(callee) => {
                            if let Some(keyed) = self.known.get(callee) {
                                took_in.push((*callee, keyed.key));
                            }
                        }
                        Edge::Node(to) if component_of[*to] != number => {
                            let callee = nodes[*to];
                            if let Some(keyed) = self.known.get(&callee) {
                                took_in.push((callee, keyed.key));
                            }
                        }
                        Edge::Node(_) => {}
                    }
                }
                took_in.sort_unstable();
                took_in.dedup();
                let key = match self.plant {
                    Plant::InlineeLeftOut => instance_key(&plan.key, tier, &[]),
                    _ => instance_key(&plan.key, tier, &took_in),
                };
                self.known.insert(symbol, Keyed { key, took_in });
            }
        }
        match self.known.get(root) {
            Some(keyed) => Resolution::Keyed(keyed.key),
            None => unreachable!("the root is a member of its own closure's components"),
        }
    }
}

/// The strongly connected components of the graph `edges` (over node indices; `Known` edges
/// are leaves outside it), each a list of members, in Tarjan's order: a component comes after
/// every component it reaches. Iterative, so a long chain of small functions cannot overflow
/// the stack.
fn components(edges: &[Vec<Edge, Global>]) -> Vec<Vec<usize, Global>, Global> {
    const UNSEEN: usize = usize::MAX;
    let count = edges.len();
    let mut order = Vec::with_capacity_in(count, Global);
    order.resize(count, UNSEEN);
    let mut low = Vec::with_capacity_in(count, Global);
    low.resize(count, 0usize);
    let mut on_stack = Vec::with_capacity_in(count, Global);
    on_stack.resize(count, false);
    let mut stack = Vec::new_in(Global);
    let mut calls: Vec<(usize, usize), Global> = Vec::new_in(Global);
    let mut out = Vec::new_in(Global);
    let mut next = 0usize;
    for start in 0..count {
        if order[start] != UNSEEN {
            continue;
        }
        order[start] = next;
        low[start] = next;
        next += 1;
        stack.push(start);
        on_stack[start] = true;
        calls.push((start, 0));
        while let Some(&(node, at)) = calls.last() {
            if at < edges[node].len() {
                if let Some(top) = calls.last_mut() {
                    top.1 += 1;
                }
                let Edge::Node(to) = edges[node][at] else { continue };
                if order[to] == UNSEEN {
                    order[to] = next;
                    low[to] = next;
                    next += 1;
                    stack.push(to);
                    on_stack[to] = true;
                    calls.push((to, 0));
                } else if on_stack[to] {
                    low[node] = low[node].min(order[to]);
                }
                continue;
            }
            calls.pop();
            if let Some(&(parent, _)) = calls.last() {
                low[parent] = low[parent].min(low[node]);
            }
            if low[node] == order[node] {
                let mut members = Vec::new_in(Global);
                while let Some(member) = stack.pop() {
                    on_stack[member] = false;
                    members.push(member);
                    if member == node {
                        break;
                    }
                }
                members.sort_unstable();
                out.push(members);
            }
        }
    }
    out
}
