//! The forward solver of a group of machines (one machine alone is a group of one): a
//! worklist over the group's pending items, where an item that cannot advance parks on the
//! exact keys its evaluation read, and a change to a key wakes only what is parked on it.
//!
//! # The keys
//!
//! A [`Key`] names one piece of a member's state, or one answer:
//!
//! - `Name(m, n)`: what `m` declares under `n` and the named imports of `m` that bind `n` (their
//!   namespaces, failure and blocked flag);
//! - `Globs(m)`: `m`'s globs, each one's source and blocked flag;
//! - `CallsState(m)`: whether a call of `m` may still add names, and whether one pending waits;
//! - `Call(m, c)`: call `c`'s state and blocked flag (what a textual walk or a derive reads);
//! - `Frags(m)`: `m`'s fragments (where a `mod` line is);
//! - `Read(r)`: the answer to read `r`.
//!
//! An event is "this key changed". It wakes the items parked on it and drops the group glob
//! fixpoints that read it; nothing else is looked at, so the work of a fill is the events
//! times the items each one wakes.
//!
//! # The modes, and why the result is one whatever the order
//!
//! Most of the time every pending item reads as blocked: a lookup that meets one waits, so an
//! item advances only on what no pending item can change. That is monotone, and the order the
//! queue is taken in moves nothing. What is not monotone is decided when the group is quiet
//! (no event left, no answer outstanding): the blocked flags are lowered to their least fixpoint
//! (an item is blocked only if what it waits on reaches an answer not given yet), every pending
//! item is evaluated against those flags, and every outcome is applied at once, so no item
//! decides on another's half-done move. If nothing moves even then, the stall rule
//! ([`Work::stall`]): the sink components of the wait graph fail as not found, and the loop
//! goes on; when nothing is pending, the final pass.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use blaze_hash::HashMap;
use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc, Vec};
use blaze_surface::{Def, Unresolved};

use crate::error::ResolveError;
use crate::facts::{Name, ScopeFacts};
use crate::read::{ExpandRequest, Read, Reply};
#[cfg(test)]
use crate::resolver::Plant;
use crate::resolver::{
    Blame, CallState, Env, FillStats, Item, NsRes, Outbox, Ran, Resolver, Scan, Sink, Source,
    MAX_EXPANSIONS, cycle_of, finish,
};

/// One piece of a member's state, or one answer: what an evaluation read and an event names.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) enum Key<N> {
    Name(ItemKey, N),
    Globs(ItemKey),
    CallsState(ItemKey),
    Call(ItemKey, u32),
    Frags(ItemKey),
    Read(Read<N>),
}

/// What one evaluation of an item came to, before it is applied.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Outcome {
    Import { ns: [NsRes; 3], failed: Option<(u32, Unresolved)>, first: Option<Def>, wait: bool },
    Glob { source: Option<Source>, first: Option<Def>, wait: bool },
    Call { state: Option<CallState>, first: Option<Def>, wait: bool },
}

impl Outcome {
    fn wait(&self) -> bool {
        match *self {
            Outcome::Import { wait, .. } | Outcome::Glob { wait, .. } | Outcome::Call { wait, .. } => wait,
        }
    }
}

/// The end of a list in `nodes`.
const NONE: u32 = u32::MAX;

/// Who is parked on a key: an item as of one evaluation.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Waiter {
    Item { m: u32, item: Item, era: u32 },
}

#[derive(Clone, Copy)]
struct Node {
    waiter: Waiter,
    next: u32,
}

/// A read an item's last evaluation needed (or found in progress).
#[derive(Clone, Copy)]
struct Parked<N> {
    read: Read<N>,
    m: u32,
    item: Item,
    era: u32,
    /// Made with the flags lowered: no longer what the item needs once they are raised.
    relaxed: bool,
}

/// An item's evaluation count, and whether it is in the queue.
#[derive(Clone, Copy, Default)]
struct Mark {
    era: u32,
    queued: bool,
}

/// The parked lists: a head per key, nodes in one arena, freed nodes reused.
struct Lists<'a, N> {
    heads: HashMap<Key<N>, u32, Alloc<'a>>,
    nodes: AVec<'a, Node>,
    free: u32,
}

impl<'a, N: Name> Lists<'a, N> {
    /// Parks `waiter` on `key`; false when it is parked there already. One evaluation parks
    /// its keys one after another, so a repeat finds itself at the head of the key's list:
    /// the repeats of an evaluation's read list drop out with one probe each, no sort.
    fn park(&mut self, key: Key<N>, waiter: Waiter) -> bool {
        let head = self.heads.entry(key).or_insert(NONE);
        let next = *head;
        if next != NONE && self.nodes[next as usize].waiter == waiter {
            return false;
        }
        let node = Node { waiter, next };
        let at = if self.free != NONE {
            let at = self.free;
            self.free = self.nodes[at as usize].next;
            self.nodes[at as usize] = node;
            at
        } else {
            let at = u32::try_from(self.nodes.len()).unwrap_or(NONE);
            self.nodes.push(node);
            at
        };
        *head = at;
        true
    }

    /// Drops every waiter `live` says is stale (an item evaluated since, a fixpoint dropped
    /// since), so the lists hold at most one node per key an item's last evaluation read.
    fn compact(&mut self, alloc: Alloc<'a>, mut live: impl FnMut(&Waiter) -> bool) {
        let mut empty: AVec<'a, Key<N>> = Vec::new_in(alloc);
        for (key, head) in self.heads.iter_mut() {
            let mut at = *head;
            let mut kept = NONE;
            while at != NONE {
                let node = self.nodes[at as usize];
                if live(&node.waiter) {
                    self.nodes[at as usize].next = kept;
                    kept = at;
                } else {
                    self.nodes[at as usize].next = self.free;
                    self.free = at;
                }
                at = node.next;
            }
            *head = kept;
            if kept == NONE {
                empty.push(*key);
            }
        }
        for key in empty.iter() {
            self.heads.remove(key);
        }
    }

    /// Nodes in use.
    fn used(&self) -> usize {
        let mut free = 0usize;
        let mut at = self.free;
        while at != NONE {
            free += 1;
            at = self.nodes[at as usize].next;
        }
        self.nodes.len() - free
    }
}

/// The worklist state of one group, kept across its steps.
pub(crate) struct Work<'a, N> {
    queue: AVec<'a, (u32, Item)>,
    /// Per member, per kind (imports, globs, calls), per item.
    marks: AVec<'a, [AVec<'a, Mark>; 3]>,
    lists: Lists<'a, N>,
    /// The one sink every evaluation records into; it holds the group glob fixpoints.
    sink: Sink<'a, N>,
    needs: AVec<'a, Parked<N>>,
    waits: AVec<'a, Parked<N>>,
    /// Per member, what [`Resolver::calls_state`] was when last looked at.
    pairs: AVec<'a, (u32, u32)>,
    /// Calls resolved and not yet expanded, as `(member, call)`.
    resolved: AVec<'a, (u32, u32)>,
    /// The flags are not all raised: the next drain raises them first.
    relaxed: bool,
    /// Evaluations are recorded, not applied (the lowering of the flags).
    relaxing: bool,
    /// The lowering's first round: every pending item evaluated with every flag down, the
    /// flags of those that wait raised together after (so its reads are the most any round
    /// makes).
    first_round: bool,
    /// The items the first round found waiting.
    raised: AVec<'a, (u32, Item)>,
    outcomes: HashMap<(u32, Item), Outcome, Alloc<'a>>,
    /// The node count at which the parked lists are next compacted: twice what the last
    /// compaction kept, so compaction costs O(1) amortized per park.
    compact_at: usize,
    era: u32,
    alloc: Alloc<'a>,
    /// The work done, added to the outbox's at the end of each step.
    stats: FillStats,
    #[cfg(test)]
    plant: Plant,
    /// Under [`Plant::WakeAll`], an event queues every pending item.
    #[cfg(test)]
    flood: bool,
    #[cfg(test)]
    rng: u64,
}

fn idx(item: Item) -> (usize, usize) {
    match item {
        Item::Import(i) => (0, i as usize),
        Item::Glob(g) => (1, g as usize),
        Item::Call(c) => (2, c as usize),
    }
}

fn pending<N>(r: &Resolver<'_, N>, item: Item) -> bool {
    match item {
        Item::Import(i) => !r.imports[i as usize].done(),
        Item::Glob(g) => matches!(r.globs[g as usize].source, Source::Pending),
        Item::Call(c) => r.calls[c as usize].state == CallState::Pending,
    }
}

/// Whether decided `item` read a binding through globs (an import still found, a glob still
/// resolved): it is evaluated again when what it read changes.
fn via_glob<N>(r: &Resolver<'_, N>, item: Item) -> bool {
    match item {
        Item::Import(i) => {
            let imp = &r.imports[i as usize];
            imp.via_glob && imp.failed.is_none()
        }
        Item::Glob(g) => {
            let glob = &r.globs[g as usize];
            glob.via_glob && matches!(glob.source, Source::Resolved { .. })
        }
        Item::Call(_) => false,
    }
}

fn set_via_glob<N>(r: &mut Resolver<'_, N>, item: Item, to: bool) {
    match item {
        Item::Import(i) => r.imports[i as usize].via_glob = to,
        Item::Glob(g) => r.globs[g as usize].via_glob = to,
        Item::Call(_) => {}
    }
}

/// The strongly connected components of the graph whose node `v` has the edges
/// `edges[starts[v]..starts[v + 1]]`: each node's component number (Tarjan's, with an explicit
/// stack instead of recursion).
fn components<'a>(starts: &[usize], edges: &[u32], a: Alloc<'a>) -> AVec<'a, u32> {
    const UNSEEN: u32 = u32::MAX;
    let n = starts.len().saturating_sub(1);
    let mut index: AVec<'a, u32> = Vec::with_capacity_in(n, a);
    index.resize(n, UNSEEN);
    let mut low: AVec<'a, u32> = Vec::with_capacity_in(n, a);
    low.resize(n, 0);
    let mut on: AVec<'a, bool> = Vec::with_capacity_in(n, a);
    on.resize(n, false);
    let mut comp: AVec<'a, u32> = Vec::with_capacity_in(n, a);
    comp.resize(n, UNSEEN);
    let mut stack: AVec<'a, u32> = Vec::new_in(a);
    let mut calls: AVec<'a, (u32, usize)> = Vec::new_in(a);
    let mut next = 0u32;
    let mut comps = 0u32;
    for root in 0..n {
        if index[root] != UNSEEN {
            continue;
        }
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(u32_of(root));
        on[root] = true;
        calls.push((u32_of(root), starts[root]));
        while let Some(top) = calls.last_mut() {
            let v = top.0 as usize;
            if top.1 < starts[v + 1] {
                let w = edges[top.1] as usize;
                top.1 += 1;
                if index[w] == UNSEEN {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(u32_of(w));
                    on[w] = true;
                    calls.push((u32_of(w), starts[w]));
                } else if on[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            calls.pop();
            if let Some(&(p, _)) = calls.last() {
                low[p as usize] = low[p as usize].min(low[v]);
            }
            if low[v] == index[v] {
                while let Some(w) = stack.pop() {
                    on[w as usize] = false;
                    comp[w as usize] = comps;
                    if w as usize == v {
                        break;
                    }
                }
                comps += 1;
            }
        }
    }
    comp
}

fn blocked<N>(r: &Resolver<'_, N>, item: Item) -> bool {
    match item {
        Item::Import(i) => r.imports[i as usize].blocked,
        Item::Glob(g) => r.globs[g as usize].blocked,
        Item::Call(c) => r.calls[c as usize].blocked,
    }
}

fn u32_of(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// One evaluation of `item` of member `m` against the state as it is.
fn eval<'a, N: Name, F: ScopeFacts<Name = N>>(
    ms: &[Resolver<'a, N>],
    m: usize,
    item: Item,
    facts: &F,
    sink: &mut Sink<'a, N>,
) -> Result<Outcome, ResolveError> {
    let env = Env { members: ms };
    let r = &ms[m];
    Ok(match item {
        Item::Import(i) => {
            let stored = &r.imports[i as usize];
            let o = r.resolve_import(i as usize, env, sink);
            let mut ns = stored.ns;
            for (k, slot) in ns.iter_mut().enumerate() {
                if *slot == NsRes::Pending {
                    *slot = o.ns[k];
                }
            }
            Outcome::Import { ns, failed: stored.failed.or(o.failed), first: o.first, wait: o.wait }
        }
        Item::Glob(g) => {
            let o = r.resolve_glob(g as usize, facts, env, sink)?;
            Outcome::Glob { source: o.source, first: o.first, wait: o.wait }
        }
        Item::Call(c) => {
            let o = r.resolve_call(c as usize, env, sink);
            Outcome::Call { state: o.state, first: o.first, wait: o.wait }
        }
    })
}

impl<'a, N: Name> Work<'a, N> {
    /// The state of a group about to run, every pending item queued.
    pub(crate) fn new_in(ms: &[Resolver<'a, N>], alloc: Alloc<'a>) -> Work<'a, N> {
        let mut w = Work {
            queue: Vec::new_in(alloc),
            marks: Vec::with_capacity_in(ms.len(), alloc),
            lists: Lists { heads: HashMap::new_in(alloc), nodes: Vec::new_in(alloc), free: NONE },
            sink: Sink::new_in(alloc, Scan::Early),
            needs: Vec::new_in(alloc),
            waits: Vec::new_in(alloc),
            pairs: Vec::with_capacity_in(ms.len(), alloc),
            resolved: Vec::new_in(alloc),
            relaxed: true,
            relaxing: false,
            first_round: false,
            raised: Vec::new_in(alloc),
            outcomes: HashMap::new_in(alloc),
            compact_at: 4096,
            era: 0,
            alloc,
            stats: FillStats::default(),
            #[cfg(test)]
            plant: ms.first().map_or(Plant::None, |r| r.plant),
            #[cfg(test)]
            flood: false,
            #[cfg(test)]
            rng: 0,
        };
        #[cfg(test)]
        if let Plant::Shuffle(seed) = w.plant {
            w.rng = seed | 1;
        }
        for (m, r) in ms.iter().enumerate() {
            w.marks.push([Vec::new_in(alloc), Vec::new_in(alloc), Vec::new_in(alloc)]);
            w.pairs.push(r.calls_state());
            for (c, call) in r.calls.iter().enumerate() {
                if matches!(call.state, CallState::Resolved(_)) {
                    w.resolved.push((u32_of(m), u32_of(c)));
                }
            }
        }
        w.queue_all(ms, false);
        w
    }

    fn mark(&mut self, m: u32, item: Item) -> &mut Mark {
        let (kind, i) = idx(item);
        let list = &mut self.marks[m as usize][kind];
        if list.len() <= i {
            list.resize(i + 1, Mark::default());
        }
        &mut list[i]
    }

    fn push(&mut self, m: u32, item: Item) {
        if self.relaxing {
            self.stats.relax_queued += 1;
        }
        let mark = self.mark(m, item);
        if !mark.queued {
            mark.queued = true;
            self.queue.push((m, item));
        }
    }

    fn pop(&mut self) -> Option<(u32, Item)> {
        #[cfg(test)]
        if self.rng != 0 && !self.queue.is_empty() {
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 7;
            self.rng ^= self.rng << 17;
            let at = (self.rng % self.queue.len() as u64) as usize;
            let last = self.queue.len() - 1;
            self.queue.swap(at, last);
        }
        let (m, item) = self.queue.pop()?;
        self.mark(m, item).queued = false;
        Some((m, item))
    }

    /// Queues every pending item; `decided` also queues the decided items that read through
    /// globs. Only the planted flood passes it: a decided via-glob item is solved again ONLY
    /// when a key it read fires (its park from the evaluation that decided it), since a later
    /// glob can change its answer only by adding a binding to a module it walked. Queuing them
    /// at every relax round re-solved each one once per round: rounds times items.
    fn queue_all(&mut self, ms: &[Resolver<'a, N>], decided: bool) {
        for (m, r) in ms.iter().enumerate() {
            let m = u32_of(m);
            for i in 0..r.imports.len() {
                let item = Item::Import(u32_of(i));
                if !r.imports[i].done() || (decided && via_glob(r, item)) {
                    self.push(m, item);
                } else if via_glob(r, item) {
                    self.stats.agains_spared += 1;
                }
            }
            for g in 0..r.globs.len() {
                let item = Item::Glob(u32_of(g));
                if matches!(r.globs[g].source, Source::Pending) || (decided && via_glob(r, item)) {
                    self.push(m, item);
                } else if via_glob(r, item) {
                    self.stats.agains_spared += 1;
                }
            }
            for c in 0..r.calls.len() {
                if r.calls[c].state == CallState::Pending {
                    self.push(m, Item::Call(u32_of(c)));
                }
            }
        }
    }

    fn next_era(&mut self) -> u32 {
        self.era = self.era.wrapping_add(1);
        self.era
    }

    /// An event on `key`: its parked items are queued, its fixpoints dropped.
    fn fire(&mut self, key: Key<N>) {
        self.stats.events += 1;
        #[cfg(test)]
        {
            if self.plant == Plant::WakeAll {
                self.flood = true;
                
                return;
            }
            if self.plant == Plant::DropWake && matches!(key, Key::Name(..)) {
                return;
            }
        }
        let Some(mut at) = self.lists.heads.remove(&key) else { return };
        while at != NONE {
            let node = self.lists.nodes[at as usize];
            match node.waiter {
                Waiter::Item { m, item, era } => {
                    if self.mark(m, item).era == era {
                        self.stats.wakes += 1;
                        self.push(m, item);
                    }
                }
            }
            self.lists.nodes[at as usize].next = self.lists.free;
            self.lists.free = at;
            at = node.next;
        }
    }

    /// What the evaluation of `item` (as of `era`) recorded since `mark`: it parks on every key
    /// it read, its needs and waits are kept, and the fixpoints it made park on theirs.
    fn take(&mut self, m: u32, item: Item, era: u32, mark: crate::resolver::Mark) {
        let waiter = Waiter::Item { m, item, era };
        let from = mark.touched;
        self.stats.touches += (self.sink.touched.len() - from) as u64;
        for k in from..self.sink.touched.len() {
            if self.lists.park(self.sink.touched[k], waiter) {
                self.stats.parks += 1;
            }
        }
        self.sink.touched.truncate(from);
        let relaxed = self.relaxing;
        for k in mark.need..self.sink.need.len() {
            let read = self.sink.need[k];
            self.needs.push(Parked { read, m, item, era, relaxed });
        }
        self.sink.need.truncate(mark.need);
        for k in mark.waits..self.sink.waits.len() {
            let read = self.sink.waits[k];
            self.waits.push(Parked { read, m, item, era, relaxed });
        }
        self.sink.waits.truncate(mark.waits);
        
        if self.lists.free == NONE && self.lists.nodes.len() >= self.compact_at {
            self.compact();
        }
    }

    /// Drops the stale waiters from the parked lists.
    fn compact(&mut self) {
        let Work { lists, marks, alloc, .. } = self;
        lists.compact(*alloc, |w| match *w {
            Waiter::Item { m, item, era } => {
                let (kind, i) = idx(item);
                marks[m as usize][kind].get(i).is_some_and(|mk| mk.era == era)
            }
        });
        self.compact_at = self.lists.used().saturating_mul(2).max(4096);
    }

    /// Whether member `m`'s calls changed what [`Resolver::calls_state`] says; an event if so.
    fn pair(&mut self, ms: &[Resolver<'a, N>], m: usize) {
        let now = ms[m].calls_state();
        if self.pairs[m] != now {
            self.pairs[m] = now;
            self.fire(Key::CallsState(ms[m].module));
        }
    }

    /// Every pending item blocked, and the fixpoints read under the lowered flags dropped; what
    /// an item needed with the flags lowered is not what it needs now, so it is evaluated again.
    fn raise(&mut self, ms: &mut [Resolver<'a, N>]) {
        if !self.relaxed {
            return;
        }
        self.relaxed = false;
        for r in ms.iter_mut() {
            r.block_all();
        }
        for (m, r) in ms.iter().enumerate() {
            self.pairs[m] = r.calls_state();
        }
        
        let mut k = 0;
        while k < self.needs.len() {
            let p = self.needs[k];
            if p.relaxed {
                self.needs.swap_remove(k);
                if self.mark(p.m, p.item).era == p.era {
                    self.push(p.m, p.item);
                }
            } else {
                k += 1;
            }
        }
        let mut k = 0;
        while k < self.waits.len() {
            let p = self.waits[k];
            if p.relaxed {
                self.waits.swap_remove(k);
                if self.mark(p.m, p.item).era == p.era {
                    self.push(p.m, p.item);
                }
            } else {
                k += 1;
            }
        }
    }

    /// Evaluates queued items until none is left: each outcome applied (or, while the flags are
    /// being lowered, recorded and its wait raised).
    fn drain<F: ScopeFacts<Name = N>>(&mut self, ms: &mut [Resolver<'a, N>], facts: &F) -> Result<(), ResolveError> {
        let items: usize = ms.iter().map(|r| r.imports.len() + r.globs.len() + r.calls.len()).sum();
        let cap = (items as u64 + 16).saturating_mul(4096);
        let mut evals = 0u64;
        loop {
            #[cfg(test)]
            if self.flood {
                self.flood = false;
                self.queue_all(ms, true);
            }
            let Some((m, item)) = self.pop() else { break };
            let mu = m as usize;
            // A decided item that read a binding through globs is tried again when what it read
            // changes: a later glob may have made that binding ambiguous.
            let again = !pending(&ms[mu], item);
            if again && !via_glob(&ms[mu], item) {
                continue;
            }
            if again {
                self.stats.agains += 1;
            }
            evals += 1;
            if evals > cap {
                return Err(ResolveError::NoProgressBound { module: ms[mu].module, passes: u32_of(evals as usize) });
            }
            let era = self.next_era();
            self.mark(m, item).era = era;
            let mark = self.sink.begin();
            self.sink.via_glob = false;
            let o = eval(ms, mu, item, facts, &mut self.sink)?;
            let via = self.sink.via_glob;
            self.sink.check(ms[mu].module)?;
            self.take(m, item, era, mark);
            self.stats.plans += 1;
            match item {
                Item::Import(_) => self.stats.plan_imports += 1,
                Item::Glob(_) => self.stats.plan_globs += 1,
                Item::Call(_) => {}
            }
            if again {
                self.ambiguous_again(ms, mu, item, o);
                continue;
            }
            // Sticky: whatever later decides it (an apply here, the relaxed pass, a stall), it
            // read a binding through globs.
            if via {
                set_via_glob(&mut ms[mu], item, true);
            }
            if self.relaxing {
                self.outcomes.insert((m, item), o);
                if o.wait() && !blocked(&ms[mu], item) {
                    if self.first_round {
                        self.raised.push((m, item));
                    } else {
                        self.set_blocked(ms, mu, item);
                    }
                }
            } else if self.apply(ms, mu, item, o, facts)? && pending(&ms[mu], item) {
                self.push(m, item);
            }
        }
        Ok(())
    }

    /// A decided import or glob evaluated again: if it now reads an ambiguous binding (a later
    /// glob brought a different item under a name it found through globs), it fails as
    /// ambiguous, with its event; nothing else of a decided item changes.
    fn ambiguous_again(&mut self, ms: &mut [Resolver<'a, N>], m: usize, item: Item, o: Outcome) {
        let r = &mut ms[m];
        let module = r.module;
        match (item, o) {
            (Item::Import(i), Outcome::Import { failed: Some((s, Unresolved::Ambiguous(k))), .. }) => {
                let imp = &mut r.imports[i as usize];
                if imp.failed.is_some() {
                    return;
                }
                imp.failed = Some((s, Unresolved::Ambiguous(k)));
                imp.ns = [NsRes::Absent; 3];
                imp.via_glob = false;
                if let Some(name) = imp.bound {
                    self.fire(Key::Name(module, name));
                }
            }
            (Item::Glob(g), Outcome::Glob { source: Some(Source::Failed(s, Unresolved::Ambiguous(k))), .. }) => {
                if !matches!(r.globs[g as usize].source, Source::Resolved { .. }) {
                    return;
                }
                r.set_glob_source(g as usize, Source::Failed(s, Unresolved::Ambiguous(k)));
                r.globs[g as usize].via_glob = false;
                self.fire(Key::Globs(module));
            }
            (Item::Import(_) | Item::Glob(_) | Item::Call(_), _) => {}
        }
    }

    /// The stall rule at a quiet point (bottom.md section 6): every pending item is evaluated
    /// again for the pending items it waits on, the wait graph's strongly connected components
    /// are found (Tarjan's, with an explicit stack), and the members of every sink component
    /// fail as not found, each with its event; what waited on them is woken and solved as
    /// usual. Sinks are a property of the graph, so the answer is one in any order. Whether
    /// any failed.
    fn stall<F: ScopeFacts<Name = N>>(&mut self, ms: &mut [Resolver<'a, N>], facts: &F) -> Result<bool, ResolveError> {
        let a = self.alloc;
        let mut items: AVec<'a, (u32, Item)> = Vec::new_in(a);
        for (m, r) in ms.iter().enumerate() {
            let m = u32_of(m);
            for (i, imp) in r.imports.iter().enumerate() {
                if !imp.done() {
                    items.push((m, Item::Import(u32_of(i))));
                }
            }
            for (g, glob) in r.globs.iter().enumerate() {
                if matches!(glob.source, Source::Pending) {
                    items.push((m, Item::Glob(u32_of(g))));
                }
            }
            for (c, call) in r.calls.iter().enumerate() {
                if call.state == CallState::Pending {
                    items.push((m, Item::Call(u32_of(c))));
                }
            }
        }
        if items.is_empty() {
            return Ok(false);
        }
        items.sort_unstable();
        let n = items.len();
        // The edges: item k waits on `edges[starts[k]..starts[k + 1]]`.
        let mut starts: AVec<'a, usize> = Vec::with_capacity_in(n + 1, a);
        let mut edges: AVec<'a, u32> = Vec::new_in(a);
        let mut blames: AVec<'a, Blame> = Vec::new_in(a);
        // The waits on one call, as `(waiter, call)`: a textual read past it or a derive before
        // it (what that call may define), as opposed to a module's calls as a whole.
        let mut ones: AVec<'a, (u32, u32)> = Vec::new_in(a);
        let mut sink = Sink::new_in(a, Scan::Stall);
        for (v, &(m, item)) in items.iter().enumerate() {
            starts.push(edges.len());
            sink.blame.clear();
            eval(ms, m as usize, item, facts, &mut sink)?;
            blames.clear();
            blames.extend_from_slice(&sink.blame);
            let env = Env { members: &*ms };
            let at = |m: usize, item: Item| items.binary_search(&(u32_of(m), item)).ok().map(u32_of);
            for &b in blames.iter() {
                let (module, wanted) = match b {
                    Blame::Import(module, i) => (module, Some(Item::Import(i))),
                    Blame::Glob(module, g) => (module, Some(Item::Glob(g))),
                    Blame::Call(module, c) => (module, Some(Item::Call(c))),
                    Blame::Calls(module, _) => (module, None),
                };
                // A module outside the group is finished: nothing of it is pending.
                let Some(mm) = env.index(module) else { continue };
                match (wanted, b) {
                    (Some(item), _) => {
                        let w = at(mm, item);
                        if let (Some(w), Blame::Call(..)) = (w, b) {
                            ones.push((u32_of(v), w));
                        }
                        edges.extend(w);
                    }
                    (None, Blame::Calls(_, except)) => {
                        for c in 0..ms[mm].calls.len() {
                            if Some(u32_of(c)) != except && ms[mm].calls[c].state == CallState::Pending {
                                edges.extend(at(mm, Item::Call(u32_of(c))));
                            }
                        }
                    }
                    (None, _) => {}
                }
            }
        }
        starts.push(edges.len());
        sink.stats.passes += 1;
        sink.stats.stalls += 1;
        sink.stats.stall_items += n as u64;
        sink.stats.stall_edges += edges.len() as u64;
        self.stats.add(&sink.stats);
        sink.check(ms[0].module)?;
        let comp = components(&starts, &edges, a);
        let comps = comp.iter().map(|&c| c as usize + 1).max().unwrap_or(0);
        let mut leaves: AVec<'a, bool> = Vec::with_capacity_in(comps, a);
        leaves.resize(comps, true);
        for v in 0..n {
            for &w in edges[starts[v]..starts[v + 1]].iter() {
                if comp[w as usize] != comp[v] {
                    leaves[comp[v] as usize] = false;
                }
            }
        }
        // In a sink, a member that waits on one call of the same sink (a textual read past it,
        // a derive before it) is spared this round: that call fails first and adds nothing, so
        // what it held is solved again (the Reference's textual scope: a failed call defines
        // no `macro_rules!`). Text order makes these waits acyclic, so every sink keeps a member
        // to fail.
        let mut spared: AVec<'a, bool> = Vec::with_capacity_in(n, a);
        spared.resize(n, false);
        for &(v, w) in ones.iter() {
            if comp[v as usize] == comp[w as usize] {
                spared[v as usize] = true;
            }
        }
        let mut moved = false;
        for v in 0..n {
            if !leaves[comp[v] as usize] || spared[v] {
                continue;
            }
            let (m, item) = items[v];
            let mu = m as usize;
            if !ms[mu].fail_stalled(item) {
                continue;
            }
            moved = true;
            let module = ms[mu].module;
            match item {
                Item::Import(i) => {
                    if let Some(name) = ms[mu].imports[i as usize].bound {
                        self.fire(Key::Name(module, name));
                    }
                }
                Item::Glob(_) => self.fire(Key::Globs(module)),
                Item::Call(c) => {
                    self.fire(Key::Call(module, c));
                    self.pair(ms, mu);
                }
            }
        }
        if moved {
            self.relaxed = true;
        }
        Ok(moved)
    }

    /// The worklist's invariant at a quiet point, checked in test builds: every pending item
    /// evaluated afresh (fixpoints made again too) moves nothing, so no change it read was
    /// missed.
    #[cfg(test)]
    fn verify_quiet<F: ScopeFacts<Name = N>>(&self, ms: &[Resolver<'a, N>], facts: &F) -> Result<(), ResolveError> {
        let mut sink = Sink::new_in(self.alloc, Scan::Early);
        for (m, r) in ms.iter().enumerate() {
            let items = (0..r.imports.len())
                .map(|i| Item::Import(u32_of(i)))
                .chain((0..r.globs.len()).map(|g| Item::Glob(u32_of(g))))
                .chain((0..r.calls.len()).map(|c| Item::Call(u32_of(c))));
            for item in items {
                if !pending(r, item) {
                    continue;
                }
                let o = eval(ms, m, item, facts, &mut sink)?;
                let moves = match (item, o) {
                    (Item::Import(i), Outcome::Import { ns, failed, .. }) => {
                        let imp = &r.imports[i as usize];
                        imp.ns != ns || imp.failed != failed
                    }
                    (Item::Glob(_), Outcome::Glob { source, .. }) => source.is_some(),
                    (Item::Call(c), Outcome::Call { state, .. }) => state.is_some_and(|s| s != r.calls[c as usize].state),
                    (Item::Import(_) | Item::Glob(_) | Item::Call(_), _) => false,
                };
                if moves {
                    return Err(ResolveError::MissedWake { module: r.module });
                }
                sink.touched.clear();
                sink.need.clear();
                sink.waits.clear();
                
                
            }
        }
        Ok(())
    }

    /// Raises `item`'s blocked flag (the lowering found it waits), with its event.
    fn set_blocked(&mut self, ms: &mut [Resolver<'a, N>], m: usize, item: Item) {
        self.set_flag(ms, m, item, true);
    }

    /// Sets `item`'s blocked flag to `to`; a change is an event on what readers of the item
    /// read (a lookup that meets it waits on it blocked, takes it as undetermined lowered).
    fn set_flag(&mut self, ms: &mut [Resolver<'a, N>], m: usize, item: Item, to: bool) {
        let r = &mut ms[m];
        let module = r.module;
        match item {
            Item::Import(i) => {
                let imp = &mut r.imports[i as usize];
                if imp.blocked == to {
                    return;
                }
                imp.blocked = to;
                if let Some(name) = imp.bound {
                    self.fire(Key::Name(module, name));
                }
            }
            Item::Glob(g) => {
                if r.globs[g as usize].blocked == to {
                    return;
                }
                r.globs[g as usize].blocked = to;
                self.fire(Key::Globs(module));
            }
            Item::Call(c) => {
                if !r.set_call_blocked(c as usize, to) {
                    return;
                }
                self.fire(Key::Call(module, c));
                self.pair(ms, m);
            }
        }
    }

    /// Applies one outcome; whether it moved anything. Every change is an event.
    fn apply<F: ScopeFacts<Name = N>>(
        &mut self,
        ms: &mut [Resolver<'a, N>],
        m: usize,
        item: Item,
        o: Outcome,
        facts: &F,
    ) -> Result<bool, ResolveError> {
        let r = &mut ms[m];
        let module = r.module;
        match (item, o) {
            (Item::Import(i), Outcome::Import { ns, failed, first, .. }) => {
                let imp = &mut r.imports[i as usize];
                if imp.ns == ns && imp.failed == failed {
                    return Ok(false);
                }
                imp.ns = ns;
                imp.failed = failed;
                if imp.first.is_none() {
                    imp.first = first;
                }
                if let Some(name) = imp.bound {
                    self.fire(Key::Name(module, name));
                }
                Ok(true)
            }
            (Item::Glob(g), Outcome::Glob { source: Some(source), first, .. }) => {
                r.set_glob_source(g as usize, source);
                let glob = &mut r.globs[g as usize];
                if glob.first.is_none() {
                    glob.first = first;
                }
                self.fire(Key::Globs(module));
                Ok(true)
            }
            (Item::Call(c), Outcome::Call { state: Some(state), first, .. }) => {
                let old = r.calls[c as usize].state;
                if old == state {
                    return Ok(false);
                }
                r.set_call_state(c as usize, state);
                let call = &mut r.calls[c as usize];
                if call.first.is_none() {
                    call.first = first;
                }
                self.fire(Key::Call(module, c));
                if let CallState::Resolved(_) = state {
                    if !self.expand(ms, m, c, facts)? {
                        self.resolved.push((u32_of(m), c));
                    }
                }
                self.pair(ms, m);
                Ok(true)
            }
            (Item::Import(_) | Item::Glob(_) | Item::Call(_), _) => Ok(false),
        }
    }

    /// Takes in call `c`'s expansion if it is given: its declarations are events and its items
    /// are queued. Whether it was given.
    fn expand<F: ScopeFacts<Name = N>>(
        &mut self,
        ms: &mut [Resolver<'a, N>],
        m: usize,
        c: u32,
        facts: &F,
    ) -> Result<bool, ResolveError> {
        let r = &mut ms[m];
        let module = r.module;
        let CallState::Resolved(def) = r.calls[c as usize].state else { return Ok(false) };
        let key = r.calls[c as usize].key;
        let expanded = facts.expansion(key).map_err(|error| ResolveError::Surface { module, error })?;
        let Some(expanded) = expanded else { return Ok(false) };
        r.expansions += 1;
        if r.expansions > MAX_EXPANSIONS {
            return Err(ResolveError::TooManyExpansions { module });
        }
        let at = r.calls[c as usize].at;
        let depth = r.frags[at.frag as usize].expansion + 1;
        let before = r.upto();
        let frag = r.add_fragment(&expanded, Some(key), depth, Some(at))?;
        r.set_call_state(c as usize, CallState::Expanded { frag, def });
        let after = r.upto();
        let mu = u32_of(m);
        self.fire(Key::Call(module, c));
        self.fire(Key::Frags(module));
        for k in 0..ms[m].frags[frag as usize].items.len() {
            let (_, name, _) = ms[m].frags[frag as usize].items[k];
            self.fire(Key::Name(module, name));
        }
        for i in before[0]..after[0] {
            if let Some(name) = ms[m].imports[i].bound {
                self.fire(Key::Name(module, name));
            }
            self.push(mu, Item::Import(u32_of(i)));
        }
        if after[1] > before[1] {
            self.fire(Key::Globs(module));
        }
        for g in before[1]..after[1] {
            self.push(mu, Item::Glob(u32_of(g)));
        }
        for k in before[2]..after[2] {
            if ms[m].calls[k].state == CallState::Pending {
                self.push(mu, Item::Call(u32_of(k)));
            } else if matches!(ms[m].calls[k].state, CallState::Resolved(_)) {
                self.resolved.push((mu, u32_of(k)));
            }
        }
        self.pair(ms, m);
        Ok(true)
    }

    /// Takes in every expansion given since the last step.
    fn take_expansions<F: ScopeFacts<Name = N>>(&mut self, ms: &mut [Resolver<'a, N>], facts: &F) -> Result<(), ResolveError> {
        let mut k = 0;
        while k < self.resolved.len() {
            let (m, c) = self.resolved[k];
            let state = ms[m as usize].calls[c as usize].state;
            let still = matches!(state, CallState::Resolved(_)) && !self.expand(ms, m as usize, c, facts)?;
            if still {
                k += 1;
            } else {
                self.resolved.swap_remove(k);
            }
        }
        Ok(())
    }

    /// The reads the items' last evaluations need, into `out.asks`; whether there is one. While
    /// the flags are lowered, every read an evaluation of the lowering needed is kept: an item
    /// that blocks no longer makes the reads it blocked on.
    fn asks(&mut self, ms: &[Resolver<'a, N>], out: &mut Outbox<'a, N>) -> bool {
        let Work { needs, marks, .. } = self;
        needs.retain(|p| {
            let (kind, i) = idx(p.item);
            let current = p.relaxed || marks[p.m as usize][kind].get(i).is_some_and(|mk| mk.era == p.era);
            current && pending(&ms[p.m as usize], p.item)
        });
        out.asks.clear();
        out.asks.extend(needs.iter().map(|p| p.read));
        out.asks.sort_unstable();
        out.asks.dedup();
        !out.asks.is_empty()
    }

    /// The reads in progress the items' last evaluations met, into `reads`; whether there is one.
    fn waits(&mut self, ms: &[Resolver<'a, N>], reads: &mut AVec<'a, Read<N>>) -> bool {
        let Work { waits, marks, .. } = self;
        waits.retain(|p| {
            let (kind, i) = idx(p.item);
            let current = p.relaxed || marks[p.m as usize][kind].get(i).is_some_and(|mk| mk.era == p.era);
            current && pending(&ms[p.m as usize], p.item)
        });
        reads.clear();
        reads.extend(waits.iter().map(|p| p.read));
        !reads.is_empty()
    }

    /// The calls whose expansion is needed, into `out.expand`; whether there is one.
    fn expansions(&self, ms: &[Resolver<'a, N>], out: &mut Outbox<'a, N>) -> bool {
        out.expand.clear();
        for &(m, c) in self.resolved.iter() {
            let call = &ms[m as usize].calls[c as usize];
            if let CallState::Resolved(def) = call.state {
                out.expand.push(ExpandRequest { call: call.key, def });
            }
        }
        out.expand.sort_unstable();
        out.expand.dedup();
        !out.expand.is_empty()
    }

    /// The group is quiet: the flags are lowered to their least fixpoint, every pending item is
    /// evaluated against them, and every outcome is applied at once. Whether anything moved.
    fn relax<F: ScopeFacts<Name = N>>(&mut self, ms: &mut [Resolver<'a, N>], facts: &F) -> Result<bool, ResolveError> {
        self.stats.relax_rounds += 1;
        self.relaxed = true;
        self.relaxing = true;
        self.outcomes.clear();
        
        for r in ms.iter_mut() {
            let upto = r.upto();
            r.set_flags(&[], upto);
        }
        for (m, r) in ms.iter().enumerate() {
            self.pairs[m] = r.calls_state();
        }
        self.queue_all(ms, false);
        self.first_round = true;
        let first = self.drain(ms, facts);
        self.first_round = false;
        let drained = first.and_then(|()| {
            while let Some((m, item)) = self.raised.pop() {
                if !blocked(&ms[m as usize], item) {
                    self.set_blocked(ms, m as usize, item);
                }
            }
            self.drain(ms, facts)
        });
        self.relaxing = false;
        self.raised.clear();
        drained?;
        let mut all: AVec<'a, ((u32, Item), Outcome)> = Vec::with_capacity_in(self.outcomes.len(), self.alloc);
        all.extend(self.outcomes.iter().map(|(k, o)| (*k, *o)));
        self.outcomes.clear();
        all.sort_unstable_by_key(|(k, _)| *k);
        let mut moved = false;
        for &((m, item), o) in all.iter() {
            let mu = m as usize;
            if !pending(&ms[mu], item) {
                continue;
            }
            if self.apply(ms, mu, item, o, facts)? {
                moved = true;
                if pending(&ms[mu], item) {
                    self.push(m, item);
                }
            }
        }
        Ok(moved)
    }

    /// Runs the group until it needs something or is done (the protocol of [`Resolver::step`]).
    fn run<F: ScopeFacts<Name = N>>(
        &mut self,
        ms: &mut [Resolver<'a, N>],
        facts: &F,
        replies: &[Reply<'_, N>],
        out: &mut Outbox<'a, N>,
        alloc: Alloc<'a>,
    ) -> Result<Ran<'a, N>, ResolveError> {
        for reply in replies {
            self.fire(Key::Read(reply.read));
        }
        self.take_expansions(ms, facts)?;
        let bound = ms.iter().map(|m| m.pass_bound()).fold(4u32, |a, b| a.saturating_add(b.saturating_mul(2)));
        let mut rounds = 0u32;
        let mut reads: AVec<'a, Read<N>> = Vec::new_in(alloc);
        loop {
            rounds += 1;
            if rounds > bound {
                return Err(ResolveError::NoProgressBound { module: ms[0].module, passes: rounds });
            }
            self.raise(ms);
            self.stats.passes += 1;
            self.drain(ms, facts)?;
            #[cfg(test)]
            self.verify_quiet(ms, facts)?;
            if self.asks(ms, out) {
                return Ok(Ran::Ask);
            }
            let waits = self.waits(ms, &mut reads);
            if waits {
                if self.expansions(ms, out) {
                    return Ok(Ran::Expand);
                }
                if cycle_of(ms, &reads, out) {
                    return Ok(Ran::Cycle);
                }
            } else {
                if self.relax(ms, facts)? {
                    continue;
                }
                if self.asks(ms, out) {
                    self.raise(ms);
                    return Ok(Ran::Ask);
                }
                if self.expansions(ms, out) {
                    self.raise(ms);
                    return Ok(Ran::Expand);
                }
                if self.waits(ms, &mut reads) && cycle_of(ms, &reads, out) {
                    self.raise(ms);
                    return Ok(Ran::Cycle);
                }
            }
            if self.stall(ms, facts)? {
                continue;
            }
            return finish(ms, facts, out, alloc);
        }
    }
}

/// The run of a group (one machine alone is a group of one): the worklist is
/// kept in the outbox across steps, made at the first.
pub(crate) fn run<'a, N: Name, F: ScopeFacts<Name = N>>(
    ms: &mut [Resolver<'a, N>],
    facts: &F,
    replies: &[Reply<'_, N>],
    out: &mut Outbox<'a, N>,
    alloc: Alloc<'a>,
) -> Result<Ran<'a, N>, ResolveError> {
    let mut work = match out.work.take() {
        Some(w) => w,
        None => Work::new_in(ms, alloc),
    };
    let ran = work.run(ms, facts, replies, out, alloc);
    work.stats.lookups += work.sink.stats.lookups;
    work.stats.member_nodes += work.sink.stats.member_nodes;
    work.stats.member_edge_bytes += work.sink.stats.member_edge_bytes;
    work.sink.stats = FillStats::default();
    out.stats.add(&work.stats);
    work.stats = FillStats::default();
    if !matches!(ran, Ok(Ran::Done(_))) {
        out.work = Some(work);
    }
    ran
}
