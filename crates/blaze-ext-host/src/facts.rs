//! The facts of a crate's items, to a fixpoint, kept by code identity.
//!
//! An extension's fact of an item is a function of the item and of the facts it may ask for:
//! the facts it requires, of the item itself, and those and its own, of the items it calls.
//! [`solve`] orders the items so callees come first (the strongly connected components of the
//! call graph, callees' first), computes a component with no cycle once, and runs a cycle
//! from each member's bottom until a whole round changes nothing, as blaze's own effect
//! summaries do (`hir-ty-pure`'s `summary_in`). The round count is bounded; past it the cycle
//! is refused, naming the fact and an item of it (rule `bounded-progress`).
//!
//! Each value is kept under a key that covers everything its function could read (rules
//! `cache-by-code-id`, `keys-cover-every-input`): the extension's code identity, the fact's
//! name, the item's own identity (its code, and what the host told the extension of it), and
//! the value of every fact it may ask for. So an edit computes again exactly the facts whose
//! inputs changed, and stops where a value comes out equal; nothing invalidates by hand.
//!
//! The engine knows nothing of the front end: items are indices, values are bytes, and the
//! function is a [`Runner`]. `crate::run` gives it bodies and jobs; the property test gives
//! it a reference.
//!
//! Slice 1 keeps the values in memory ([`Cache`]), for the daemon's life. Their home is the
//! store, each a blaze-format record under the same key (rule `outputs-are-records`): slice 2.

use std::alloc::Global;

use blaze_hash::HashMap;
use blaze_ids::ContentId;

use crate::key::{count, part};

/// One item of the crate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// As an error names it.
    pub name: String,
    /// Which item it is: stable across edits of what it is. What [`Cache`] remembers a
    /// fact's last key by, for [`Reason`].
    pub which: ContentId,
    /// What it is, as far as a fact's function can see: a change to it computes its facts
    /// again.
    pub identity: ContentId,
    /// The items it calls, by index, each once, in the order of its code.
    pub callees: Vec<u32>,
}

/// One fact to solve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decl {
    pub name: String,
    /// The other facts its function may ask for, by index in the list given to [`solve`],
    /// each before this one there.
    pub requires: Vec<usize>,
    /// Its least value: what a member of a cycle has before it is first computed.
    pub bottom: Vec<u8>,
}

/// What a fact's function is given to ask: the values it may read, and no other.
pub struct View<'t> {
    table: &'t Table,
    decls: &'t [Decl],
    nodes: &'t [Node],
    fact: usize,
    node: u32,
}

/// What a [`View`] answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot<'t> {
    Value(&'t [u8]),
    /// The fact's own job failed on that item.
    Failed,
    /// Not one this function may ask for: a fact it does not require, or of an item that is
    /// neither its own nor one it calls.
    Undeclared,
}

impl<'t> View<'t> {
    /// The fact being computed, and the item it is computed of.
    pub fn of(&self) -> (usize, u32) {
        (self.fact, self.node)
    }

    /// The value of `fact` of `node`.
    pub fn get(&self, fact: usize, node: u32) -> Slot<'t> {
        let own = fact == self.fact;
        let required = self.decls[self.fact].requires.contains(&fact);
        let callee = self.nodes[self.node as usize].callees.contains(&node);
        let allowed = (required && (node == self.node || callee)) || (own && callee);
        if !allowed {
            return Slot::Undeclared;
        }
        self.table.slot(fact, node)
    }
}

/// The value of every fact of every item.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Table {
    /// By fact, then by item: `None` until computed, `Some(Err(()))` for a failed job.
    values: Vec<Vec<Option<Result<Vec<u8>, ()>>>>,
}

impl Table {
    fn new(facts: usize, nodes: usize) -> Table {
        // alloc: the crate's facts, once per check, handed to the caller
        Table { values: vec![vec![None; nodes]; facts] }
    }

    /// The value of `fact` of `node`, when its job ended.
    pub fn value(&self, fact: usize, node: u32) -> Option<&[u8]> {
        match self.values.get(fact)?.get(node as usize)? {
            Some(Ok(bytes)) => Some(bytes),
            _ => None,
        }
    }

    fn slot(&self, fact: usize, node: u32) -> Slot<'_> {
        match self.values.get(fact).and_then(|it| it.get(node as usize)) {
            Some(Some(Ok(bytes))) => Slot::Value(bytes),
            Some(Some(Err(()))) => Slot::Failed,
            // Not computed: only a fact later in the order, which `View::get` never allows.
            Some(None) | None => Slot::Undeclared,
        }
    }

    /// A read-only view for a job that is not a fact's (a lint's): `requires` of `node` and
    /// of its callees.
    pub fn for_lint<'t>(
        &'t self,
        requires: &'t [usize],
        nodes: &'t [Node],
        node: u32,
    ) -> impl Fn(usize, u32) -> Slot<'t> {
        move |fact, of| {
            let near = of == node || nodes[node as usize].callees.contains(&of);
            match requires.contains(&fact) && near {
                true => self.slot(fact, of),
                false => Slot::Undeclared,
            }
        }
    }
}

/// What computes a fact: the extension's function, in a job.
pub trait Runner {
    type Error;

    /// The fact `view.of()`: its bytes, or the job's failure.
    fn run(&mut self, view: &View<'_>) -> Result<Vec<u8>, Self::Error>;
}

/// One input of a fact's value: a part of its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Part {
    /// The extension's code identity.
    Extension,
    /// The item's own identity.
    Item,
    /// The fact `fact` of the item itself.
    Own { fact: usize },
    /// The fact `fact` of the item's callee `callee`.
    Callee { fact: usize, callee: u32 },
}

/// Why a fact was computed and not found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    /// No fact of this name was ever computed of this item.
    FirstSeen,
    /// This part of its key is not what it was when the fact was last computed.
    Changed(Part),
    /// The item calls something it did not, or no longer calls something it did.
    Calls,
    /// A member of a cycle: computed again in a later round, as its cycle's values rose.
    Cycle,
}

/// One fact computed in a [`solve`], in the order computed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Computed {
    pub fact: usize,
    pub node: u32,
    pub reason: Reason,
}

impl Computed {
    /// As `blaze why` says it: by name, never by hash.
    pub fn said(&self, extension: &str, decls: &[Decl], nodes: &[Node]) -> String {
        let fact = &decls[self.fact].name;
        let item = &nodes[self.node as usize].name;
        let why = match &self.reason {
            Reason::FirstSeen => "it had not been computed before".to_owned(), // alloc: a report's line
            Reason::Changed(Part::Extension) => {
                format!("the code of the extension `{extension}` changed")
            }
            Reason::Changed(Part::Item) => format!("the code of `{item}` changed"),
            Reason::Changed(Part::Own { fact }) => {
                format!("the fact `{}` of `{item}` changed", decls[*fact].name)
            }
            Reason::Changed(Part::Callee { fact, callee }) => format!(
                "the fact `{}` of `{}`, which `{item}` calls, changed",
                decls[*fact].name, nodes[*callee as usize].name
            ),
            Reason::Calls => format!("what `{item}` calls changed"),
            Reason::Cycle => {
                format!("`{item}` is in a cycle of calls whose values were still rising")
            }
        };
        format!("fact `{fact}` of `{item}` (extension `{extension}`) was computed again: {why}")
    }
}

/// A cycle of calls whose fact did not settle within its bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unsettled {
    pub fact: usize,
    /// The first member of the cycle, and how many it has.
    pub node: u32,
    pub members: usize,
    pub rounds: usize,
}

impl Unsettled {
    pub fn said(&self, extension: &str, decls: &[Decl], nodes: &[Node]) -> String {
        format!(
            "extension `{extension}`: fact `{}` did not reach a fixpoint over the cycle of {} functions through `{}` \
             in {} rounds: its function is not monotone from its `bottom`, or its values rise without end",
            decls[self.fact].name, self.members, nodes[self.node as usize].name, self.rounds
        )
    }
}

/// The facts kept, by key, and each fact's last key, for [`Reason`].
pub struct Cache {
    values: HashMap<ContentId, Vec<u8>, Global>,
    last: HashMap<ContentId, Vec<(Part, ContentId)>, Global>,
    /// Facts found, and facts computed, since it was made.
    pub hits: u64,
    pub misses: u64,
}

impl Default for Cache {
    fn default() -> Cache {
        Cache {
            values: HashMap::new_in(Global), // alloc: the daemon's facts, for its life
            last: HashMap::new_in(Global),   // alloc: as above
            hits: 0,
            misses: 0,
        }
    }
}

/// A safeguard of the key left out, for the controls of the property test: each is a bug the
/// oracle must catch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Plant {
    #[default]
    None,
    /// The key leaves out the facts of the item's callees: a fact is found, stale, after a
    /// callee's fact changed.
    CalleeFactsOutOfKey,
    /// The key leaves out the extension's code identity: one extension finds another's fact.
    ExtensionOutOfKey,
}

/// What a [`solve`] gives.
pub struct Solved<E> {
    pub table: Table,
    /// The facts computed, not found, in order.
    pub computed: Vec<Computed>,
    /// The jobs that failed, with the fact and item of each.
    pub failed: Vec<(usize, u32, E)>,
    pub unsettled: Vec<Unsettled>,
}

/// How many rounds a cycle is given per member, and at least: enough for a fact whose
/// values rise a few dozen times per member; a cycle past it is refused, never spun.
pub const ROUNDS_PER_MEMBER: usize = 64;
pub const ROUNDS_MIN: usize = 8;

const KEY_DOMAIN: &str = "blaze v2 extension fact 1";
const VALUE_DOMAIN: &str = "blaze v2 extension fact value 1";
const LAST_DOMAIN: &str = "blaze v2 extension fact last 1";

/// The facts `decls` (each after those it requires) of every item of `nodes`, for the
/// extension whose code identity is `extension`, found in `cache` or computed by `runner`.
pub fn solve<R: Runner>(
    extension: ContentId,
    decls: &[Decl],
    nodes: &[Node],
    cache: &mut Cache,
    runner: &mut R,
    plant: Plant,
) -> Solved<R::Error> {
    let mut solved = Solved {
        table: Table::new(decls.len(), nodes.len()),
        computed: Vec::new(),  // alloc: the check's report, handed to the caller
        failed: Vec::new(),    // alloc: as above
        unsettled: Vec::new(), // alloc: as above
    };
    let components = components(nodes);
    for fact in 0..decls.len() {
        for members in &components {
            let cyclic = members.len() > 1
                || members.first().is_some_and(|it| nodes[*it as usize].callees.contains(it));
            let mut cx = Solving { extension, decls, nodes, cache: &mut *cache, plant, fact };
            if !cyclic {
                for &node in members {
                    cx.compute(node, &mut solved, runner, false);
                }
                continue;
            }
            for &node in members {
                // clone: a cycle member's starting value, once per cycle
                solved.table.values[fact][node as usize] = Some(Ok(decls[fact].bottom.clone()));
            }
            let rounds = ROUNDS_MIN + ROUNDS_PER_MEMBER * members.len();
            let mut settled = false;
            for round in 0..rounds {
                let mut changed = false;
                for &node in members {
                    changed |= cx.compute(node, &mut solved, runner, round > 0);
                }
                if !changed {
                    settled = true;
                    break;
                }
            }
            if !settled {
                // A value that was still rising is not the fact: whoever asks is stopped.
                for &node in members {
                    solved.table.values[fact][node as usize] = Some(Err(()));
                }
                solved.unsettled.push(Unsettled {
                    fact,
                    node: members[0],
                    members: members.len(),
                    rounds,
                });
            }
        }
    }
    solved
}

struct Solving<'s> {
    extension: ContentId,
    decls: &'s [Decl],
    nodes: &'s [Node],
    cache: &'s mut Cache,
    plant: Plant,
    fact: usize,
}

impl Solving<'_> {
    /// Finds or computes the fact of `node`; whether its value is not what the table held.
    fn compute<R: Runner>(
        &mut self,
        node: u32,
        solved: &mut Solved<R::Error>,
        runner: &mut R,
        in_cycle: bool,
    ) -> bool {
        let fact = self.fact;
        // A job that failed stays failed for this solve: it is not run again each round.
        if matches!(solved.table.values[fact][node as usize], Some(Err(()))) {
            return false;
        }
        let (key, parts) = self.key(node, &solved.table);
        let value = match self.cache.values.get(&key) {
            Some(found) => {
                self.cache.hits += 1;
                Ok(found.clone()) // clone: a kept fact, into the check's table
            }
            None => {
                self.cache.misses += 1;
                let reason = if in_cycle { Reason::Cycle } else { self.reason(node, &parts) };
                solved.computed.push(Computed { fact, node, reason });
                let view =
                    View { table: &solved.table, decls: self.decls, nodes: self.nodes, fact, node };
                match runner.run(&view) {
                    Ok(bytes) => {
                        // clone: the fact, kept and in the check's table
                        self.cache.values.insert(key, bytes.clone());
                        Ok(bytes)
                    }
                    Err(error) => {
                        solved.failed.push((fact, node, error));
                        Err(())
                    }
                }
            }
        };
        self.cache.last.insert(self.last_key(node), parts);
        let slot = &mut solved.table.values[fact][node as usize];
        let changed = slot.as_ref() != Some(&value);
        *slot = Some(value);
        changed
    }

    /// The key of the fact of `node` as the table stands, and its parts by name.
    fn key(&self, node: u32, table: &Table) -> (ContentId, Vec<(Part, ContentId)>) {
        let decl = &self.decls[self.fact];
        let of = &self.nodes[node as usize];
        let mut parts = Vec::new(); // alloc: a key's parts, kept for `Reason`
        if self.plant != Plant::ExtensionOutOfKey {
            parts.push((Part::Extension, self.extension));
        }
        parts.push((Part::Item, of.identity));
        for &fact in &decl.requires {
            parts.push((Part::Own { fact }, value_id(table.slot(fact, node))));
        }
        if self.plant != Plant::CalleeFactsOutOfKey {
            for &callee in &of.callees {
                for &fact in decl.requires.iter().chain(std::iter::once(&self.fact)) {
                    parts.push((Part::Callee { fact, callee }, value_id(table.slot(fact, callee))));
                }
            }
        }
        let mut bytes = Vec::new(); // alloc: a key's preimage, once per fact per check
        part(&mut bytes, decl.name.as_bytes());
        count(&mut bytes, parts.len());
        for (_, id) in &parts {
            bytes.extend_from_slice(&id.0);
        }
        (ContentId::of(KEY_DOMAIN, &bytes), parts)
    }

    fn last_key(&self, node: u32) -> ContentId {
        let mut bytes = Vec::new(); // alloc: a key's preimage, once per fact per check
        part(&mut bytes, self.decls[self.fact].name.as_bytes());
        bytes.extend_from_slice(&self.nodes[node as usize].which.0);
        ContentId::of(LAST_DOMAIN, &bytes)
    }

    /// Why the fact of `node` is computed: the first part of its key that is not what it was.
    fn reason(&self, node: u32, parts: &[(Part, ContentId)]) -> Reason {
        let Some(last) = self.cache.last.get(&self.last_key(node)) else {
            return Reason::FirstSeen;
        };
        for (now, was) in parts.iter().zip(last) {
            if now.0 != was.0 {
                return Reason::Calls;
            }
            if now.1 != was.1 {
                return Reason::Changed(now.0);
            }
        }
        // Equal as far as both go: one is longer, so the calls differ.
        Reason::Calls
    }
}

/// What a slot is, in a key: a value by its bytes, each other state by a mark of its own.
fn value_id(slot: Slot<'_>) -> ContentId {
    match slot {
        Slot::Value(bytes) => {
            let mut tagged = Vec::with_capacity(bytes.len() + 1); // alloc: a key's preimage
            tagged.push(0);
            tagged.extend_from_slice(bytes);
            ContentId::of(VALUE_DOMAIN, &tagged)
        }
        Slot::Failed => ContentId::of(VALUE_DOMAIN, &[1]),
        Slot::Undeclared => ContentId::of(VALUE_DOMAIN, &[2]),
    }
}

/// The strongly connected components of the call graph, each sorted, in an order where a
/// component comes after every component it calls into (Tarjan's, with its own stack: a
/// crate's call chains are deeper than a thread's stack should go).
fn components(nodes: &[Node]) -> Vec<Vec<u32>> {
    const UNSEEN: u32 = u32::MAX;
    let n = nodes.len();
    let mut index = vec![UNSEEN; n]; // alloc: the walk's tables, once per check
    let mut low = vec![0u32; n]; // alloc: as above
    let mut on_stack = vec![false; n]; // alloc: as above
    let mut stack: Vec<u32> = Vec::new(); // alloc: as above
    let mut work: Vec<(u32, usize)> = Vec::new(); // alloc: as above
    let mut out = Vec::new(); // alloc: the components, handed to the solve
    let mut next = 0u32;
    for root in 0..n as u32 {
        if index[root as usize] != UNSEEN {
            continue;
        }
        work.push((root, 0));
        // Each step visits a node, follows one edge or leaves a node: bounded by the graph.
        while let Some(&(v, at)) = work.last() {
            if at == 0 && index[v as usize] == UNSEEN {
                index[v as usize] = next;
                low[v as usize] = next;
                next += 1;
                stack.push(v);
                on_stack[v as usize] = true;
            }
            let callees = &nodes[v as usize].callees;
            if at < callees.len() {
                if let Some(top) = work.last_mut() {
                    top.1 += 1;
                }
                let w = callees[at];
                if (w as usize) >= n {
                    continue;
                }
                if index[w as usize] == UNSEEN {
                    work.push((w, 0));
                } else if on_stack[w as usize] {
                    low[v as usize] = low[v as usize].min(index[w as usize]);
                }
                continue;
            }
            work.pop();
            if let Some(&(parent, _)) = work.last() {
                low[parent as usize] = low[parent as usize].min(low[v as usize]);
            }
            if low[v as usize] == index[v as usize] {
                let mut members = Vec::new(); // alloc: one component, handed to the solve
                while let Some(w) = stack.pop() {
                    on_stack[w as usize] = false;
                    members.push(w);
                    if w == v {
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

#[cfg(test)]
mod tests;
