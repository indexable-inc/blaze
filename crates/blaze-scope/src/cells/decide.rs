//! D116's first step (drive.md "Names at module grain"): the cell core decides ONE cell on
//! demand. [`Decider::decide`] runs only what the cell reads: its module's explicit entry, the
//! named imports binding its name and the module's glob links, and through them the cells of
//! other modules, never every item of the crate to a quiet point.
//!
//! The walk's stack replaces waiter records. It is Tarjan's walk over the nodes a decision reads:
//! a node on the stack read again answers the bottom (`Absent`), and a strongly connected group
//! of open nodes (a glob cycle, an import and the cell it binds) is decided together as its least
//! fixed point by the same rules the whole-crate solve uses ([`Cells::member_step`],
//! [`Cells::conclude`]: one home for the rules). A node that read nothing open is decided at
//! once; one that did is never kept until its group closes ([`CellPlant::DecideCachesOpenCycle`]
//! is that bug, planted).
//!
//! A read of a call that has not grown stops the decision with a typed [`DecideWait`] naming the
//! call's module (rule 3: never a guess). Inside a module the core speculates as it does today
//! (a macro path that finds a candidate in a module with pending calls takes it; R6, R7,
//! GlobVsExpanded are the core's): the wait is only what the core itself would wait on.
//!
//! Every decision logs its reads: one hop per cell read, the cell by its id and what it held
//! ([`Read`]); a cell of a module with calls not grown is logged [`Read::Speculated`] (absent,
//! answer speculated). The record's form of a hop, `(table, cell key, digest | Absent)` with the
//! digest a [`ContentId`] over the decided value's fields, is derived only when a read list is
//! written ([`Decider::row`]): the walk hashes nothing (rule 65: the record's key stays
//! text-derived).
//!
//! The walk's state is flat arrays in a frame the caller names, grown to the core's size and
//! reused: once grown, deciding a cell allocates nothing
//! (`a_text_cell_is_decided_with_no_allocation`).
//!
//! [`check`] is the acceptance: after a whole-crate solve, the module tables are frozen in place
//! (every import and cell of this crate made undecided again; texts, expansions and calls kept),
//! and every cell the whole solve decided is decided again on demand, forward and backward, and
//! compared. It also times a pass over the cells answered from text, against the run's own hash
//! probe cost (rule 67's floor).

#![blaze::sans_io]

use core::fmt;

use blaze_id::{ContentId, ItemKey};
use blaze_own::{ABTreeMap, AVec, Alloc};

use super::{CellError, CellId, CellPlant, Cells, Guess, GuessStep, ItemId, Layer, ModId, Node, Step, Value};
use crate::cell_key::{CellKey, cell_key};
use crate::{DefKind, Namespace, Visibility};

/// What one decision gives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Decided {
    /// The cell's value.
    Value(Value),
    /// It read a call that has not grown: the decision waits on that call's module.
    Wait(DecideWait),
}

/// Which reader met the call not grown (runner 20's refinement of the stall rule): a macro path
/// that found nothing (A), an import's or a body's type or value lookup (B), or a cell's own
/// lookup with no item on the walk (the cell's namespace says which case it is).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WaitBy {
    MacroPath,
    Import,
    Lookup(Namespace),
}

/// A decision waits on `module`, whose call `call` has not added what it writes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct DecideWait {
    pub(crate) module: ItemKey,
    pub(crate) call: ItemKey,
    pub(crate) by: WaitBy,
}

/// Why a decision stopped without an answer, naming the thing (rule 19).
#[derive(Clone, Debug)]
pub enum DecideError {
    /// The core's own refusal (a group with no fixed point within its bound).
    Core(CellError),
    /// The walk took more than `steps` evaluations deciding a cell of `module` (rule 18).
    Bound { module: ItemKey, steps: u64 },
    /// A rule answered "wait" on a node it had already been given (decided, or in its guess):
    /// the walk would loop. A core bug, refused by the module of the node.
    Stuck { module: ItemKey },
}

impl fmt::Display for DecideError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecideError::Core(e) => write!(f, "D116 decide: {e}"),
            DecideError::Bound { module, steps } => write!(f, "D116 decide: a cell of module {module:?} took more than {steps} evaluations (rule 18)"),
            DecideError::Stuck { module } => {
                write!(f, "D116 decide: a rule waited again on a node of module {module:?} it had been given (decided or guessed)")
            }
        }
    }
}

/// What a hop read in its cell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Seen {
    /// Not found, or not decided.
    Absent,
    /// The module has calls not grown: read as absent, its answer speculated (a found-but-
    /// unsettled read; the reader reruns when the module settles with the name).
    Speculated,
    /// A decided value, by its digest ([`digest`]).
    Digest(ContentId),
}

/// What a hop read in its cell, as the walk holds it: the value itself. Its record form
/// ([`Seen`], a digest) is made only when the read list is written ([`Decider::row`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Read {
    /// Not decided.
    Absent,
    /// The module has calls not grown ([`Seen::Speculated`]).
    Speculated,
    Value(Value),
}

/// One read of a decision, as the walk holds it: the cell by its id in this core.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Hop {
    pub(crate) cell: CellId,
    pub(crate) read: Read,
}

/// One read as a record holds it: a cell of one module's table, by its text-derived key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct HopRow {
    pub(crate) table: ItemKey,
    pub(crate) cell: CellKey,
    pub(crate) layer: Layer,
    pub(crate) seen: Seen,
}

/// The digest of a decided value: deterministic, over its fields (68 bytes: a tag, the
/// definition's key, its kind and macro shapes, the visibility's tag and key).
pub(crate) fn digest(v: Value) -> Seen {
    let (tag, def, vis) = match v {
        Value::NotFound => return Seen::Absent,
        Value::Found(d, vis) => (1u8, Some(d), Some(vis)),
        Value::Ambiguous(d, vis) => (2u8, Some(d), Some(vis)),
        Value::Outside => (3u8, None, None),
    };
    let mut b = [0u8; 68];
    b[0] = tag;
    if let Some(d) = def {
        b[1..33].copy_from_slice(&d.key.0.0);
        let (k, shapes) = match d.kind {
            DefKind::Module => (0u8, 0u8),
            DefKind::Enum => (1, 0),
            DefKind::Trait => (2, 0),
            DefKind::Macro(s) => (3, s.byte()),
            DefKind::Other => (4, 0),
        };
        b[33] = k;
        b[34] = shapes;
    }
    if let Some(vis) = vis {
        let (t, key) = match vis {
            Visibility::Public => (0u8, [0u8; 32]),
            Visibility::Crate(k) => (1, k.0.0),
            Visibility::Restricted(m) => (2, m.0.0),
        };
        b[35] = t;
        b[36..68].copy_from_slice(&key);
    }
    Seen::Digest(ContentId::of("blaze cells decided value 1", &b))
}

/// Not open (in `cell_at` and `item_at`), or the end of a read list (in `reads`).
const NONE: u32 = u32::MAX;

/// A node on the walk's open stack: its Tarjan index and low link, and the head of its read
/// list in [`Decider::reads`]: the open nodes its last evaluation read (at the bottom).
#[derive(Clone, Copy)]
struct Open {
    node: Node,
    index: u32,
    low: u32,
    reads: u32,
}

/// Decides cells one at a time over `core`'s tables. What it decides stays decided in `core`:
/// the memo of one job. Its own state is flat arrays in `scratch` (the caller's frame).
pub(crate) struct Decider<'c, 'a, 's, N> {
    core: &'c mut Cells<'a, N>,
    scratch: Alloc<'s>,
    open: AVec<'s, Open>,
    /// Each open cell's and item's place in `open`, by id (`NONE`: not open); grown when the
    /// core makes a cell during a walk.
    cell_at: AVec<'s, u32>,
    item_at: AVec<'s, u32>,
    /// The open nodes' read lists, `(node read, next link)`, emptied with the walk.
    reads: AVec<'s, (Node, u32)>,
    /// The nodes being evaluated, innermost last; each is open.
    walk: AVec<'s, Node>,
    next: u32,
    steps: u64,
    bound: u64,
    root_module: ItemKey,
    /// The read log, lent to the core during a decision ([`Cells::cell`] appends to it).
    log: AVec<'a, CellId>,
    /// `mark[c] == epoch`: cell `c` is in `hops` already (one epoch per decision).
    mark: AVec<'s, u32>,
    epoch: u32,
    /// The last decision's reads, in the order made.
    pub(crate) hops: AVec<'s, Hop>,
}

impl<'c, 'a, 's, N: crate::Name> Decider<'c, 'a, 's, N> {
    pub(crate) fn new(core: &'c mut Cells<'a, N>, scratch: Alloc<'s>) -> Decider<'c, 'a, 's, N> {
        let root_module = core.modules[core.root as usize].key;
        let mut cell_at = AVec::new_in(scratch);
        cell_at.resize(core.cells.len(), NONE);
        let mut item_at = AVec::new_in(scratch);
        item_at.resize(core.items.len(), NONE);
        let mut mark = AVec::new_in(scratch);
        mark.resize(core.cells.len(), 0);
        let log = AVec::new_in(core.alloc);
        Decider {
            core,
            scratch,
            open: AVec::new_in(scratch),
            cell_at,
            item_at,
            reads: AVec::new_in(scratch),
            walk: AVec::new_in(scratch),
            next: 0,
            steps: 0,
            bound: 0,
            root_module,
            log,
            mark,
            epoch: 0,
            hops: AVec::new_in(scratch),
        }
    }

    /// The record's form of a hop: what a writer of the read list stores (two hashes, paid once
    /// per record written, never on the walk).
    pub(crate) fn row(&self, hop: Hop) -> HopRow {
        let cell = &self.core.cells[hop.cell as usize];
        let seen = match hop.read {
            Read::Absent => Seen::Absent,
            Read::Speculated => Seen::Speculated,
            Read::Value(v) => digest(v),
        };
        HopRow { table: self.core.modules[cell.module as usize].key, cell: cell_key(cell.ns, cell.name.as_ref()), layer: cell.layer, seen }
    }

    /// Decides `name` in `ns` of module `m`, layer `layer`.
    pub(crate) fn decide(&mut self, m: ModId, ns: Namespace, name: N, layer: Layer) -> Result<Decided, DecideError> {
        let c = self.core.cell(m, ns, name, layer);
        self.decide_cell(c)
    }

    /// Decides cell `c`.
    pub(crate) fn decide_cell(&mut self, c: CellId) -> Result<Decided, DecideError> {
        self.begin(c);
        let run = self.run(Node::Cell(c));
        self.end();
        match run? {
            Some(w) => Ok(Decided::Wait(w)),
            None => match self.core.cells[c as usize].value {
                Some(v) => Ok(Decided::Value(v)),
                None => Err(DecideError::Stuck { module: self.module_of(Node::Cell(c)) }),
            },
        }
    }

    fn begin(&mut self, c: CellId) {
        self.steps = 0;
        self.bound = 16 * (self.core.cells.len() + self.core.items.len()) as u64 + 4096;
        self.root_module = self.module_of(Node::Cell(c));
        let mut log = core::mem::replace(&mut self.log, AVec::new_in(self.core.alloc));
        log.clear();
        log.push(c);
        self.core.decide_log = Some(log);
    }

    /// Clears the walk (a wait or an error leaves nodes open: none is decided by it) and turns
    /// the read log into hops.
    fn end(&mut self) {
        self.walk.clear();
        self.release(0);
        self.reads.clear();
        if let Some(log) = self.core.decide_log.take() {
            self.log = log;
        }
        self.hops.clear();
        self.epoch = match self.epoch.checked_add(1) {
            Some(e) => e,
            None => {
                self.mark.fill(0);
                1
            }
        };
        let cells = self.core.cells.len();
        if self.mark.len() < cells {
            self.mark.resize(cells, 0);
        }
        for k in 0..self.log.len() {
            let c = self.log[k];
            if self.mark[c as usize] == self.epoch {
                continue;
            }
            self.mark[c as usize] = self.epoch;
            let cell = &self.core.cells[c as usize];
            let module = &self.core.modules[cell.module as usize];
            let read = if module.growth > 0 && !module.foreign {
                Read::Speculated
            } else {
                match cell.value {
                    None => Read::Absent,
                    Some(v) => Read::Value(v),
                }
            };
            self.hops.push(Hop { cell: c, read });
        }
    }

    /// Node `n`'s place on the open stack.
    fn place(&self, n: Node) -> Option<usize> {
        let p = match n {
            Node::Cell(c) => self.cell_at.get(c as usize),
            Node::Item(i) => self.item_at.get(i as usize),
        };
        match p {
            Some(&p) if p != NONE => Some(p as usize),
            _ => None,
        }
    }

    fn set_place(&mut self, n: Node, p: u32) {
        let (at, k) = match n {
            Node::Cell(c) => (&mut self.cell_at, c as usize),
            Node::Item(i) => (&mut self.item_at, i as usize),
        };
        if at.len() <= k {
            at.resize(k + 1, NONE);
        }
        at[k] = p;
    }

    /// Whether open node `p`'s last evaluation read `x`.
    fn has_read(&self, p: usize, x: Node) -> bool {
        let mut k = self.open[p].reads;
        while k != NONE {
            let (r, next) = self.reads[k as usize];
            if r == x {
                return true;
            }
            k = next;
        }
        false
    }

    fn decided(&self, n: Node) -> bool {
        match n {
            Node::Cell(c) => self.core.cells[c as usize].value.is_some(),
            Node::Item(i) => self.core.items[i as usize].outcome.is_some(),
        }
    }

    fn module_of(&self, n: Node) -> ItemKey {
        let m = match n {
            Node::Cell(c) => self.core.cells[c as usize].module,
            Node::Item(i) => self.core.items[i as usize].module,
        };
        self.core.modules[m as usize].key
    }

    fn run(&mut self, root: Node) -> Result<Option<DecideWait>, DecideError> {
        // A group the R5 rule left partly undecided is entered again from the root.
        while !self.decided(root) {
            self.enter(root);
            while let Some(&n) = self.walk.last() {
                self.step()?;
                if let Some(w) = self.visit(n)? {
                    return Ok(Some(w));
                }
            }
            self.release(0);
        }
        Ok(None)
    }

    fn step(&mut self) -> Result<(), DecideError> {
        self.steps += 1;
        if self.steps > self.bound {
            return Err(DecideError::Bound { module: self.root_module, steps: self.bound });
        }
        Ok(())
    }

    fn enter(&mut self, x: Node) {
        let index = self.next;
        self.next += 1;
        self.set_place(x, self.open.len() as u32);
        self.open.push(Open { node: x, index, low: index, reads: NONE });
        self.walk.push(x);
    }

    /// Drops the open nodes from place `p` up: each is decided, or undecided and entered again
    /// when read again.
    fn release(&mut self, p: usize) {
        while self.open.len() > p {
            if let Some(o) = self.open.pop() {
                self.set_place(o.node, NONE);
            }
        }
    }

    /// The walk's top is done for now and stays open: its parent takes its low link.
    fn finish(&mut self) {
        let Some(n) = self.walk.pop() else { return };
        let Some(p) = self.place(n) else { return };
        let low = self.open[p].low;
        if let Some(&parent) = self.walk.last()
            && let Some(q) = self.place(parent)
        {
            let o = &mut self.open[q];
            o.low = o.low.min(low);
        }
    }

    fn clear_waits(&mut self, n: Node) {
        if let Node::Item(i) = n {
            self.core.items[i as usize].waits.clear();
        }
    }

    /// The guess node `p` is evaluated under: the open nodes it read, at the bottom.
    fn guess_of(&self, p: usize) -> Guess<'a> {
        let alloc = self.core.alloc;
        let mut g = Guess { cells: ABTreeMap::new_in(alloc), items: ABTreeMap::new_in(alloc) };
        let mut k = self.open[p].reads;
        while k != NONE {
            let (r, next) = self.reads[k as usize];
            match r {
                Node::Cell(c) => {
                    g.cells.insert(c, None);
                }
                Node::Item(i) => {
                    g.items.insert(i, None);
                }
            }
            k = next;
        }
        g
    }

    fn commit(&mut self, n: Node, s: GuessStep) -> Result<(), DecideError> {
        match (n, s) {
            (Node::Cell(c), GuessStep::Cell(v)) => self.core.decide(c, v.unwrap_or(Value::NotFound)),
            (Node::Item(i), GuessStep::Item(Some(o))) => self.core.settle(i, o),
            _ => return Err(DecideError::Stuck { module: self.module_of(n) }),
        }
        Ok(())
    }

    /// One evaluation of the walk's top `n`.
    fn visit(&mut self, n: Node) -> Result<Option<DecideWait>, DecideError> {
        let Some(p) = self.place(n) else { return Err(DecideError::Stuck { module: self.module_of(n) }) };
        if self.decided(n) {
            self.release(p);
            self.walk.pop();
            return Ok(None);
        }
        let guess = self.guess_of(p);
        self.clear_waits(n);
        match self.core.member_step(n, &guess) {
            Step::Wait(x) => {
                if let Some(q) = self.place(x) {
                    // A back edge: `x` is open, read at the bottom from now on.
                    let index = self.open[q].index;
                    if self.has_read(p, x) {
                        return Err(DecideError::Stuck { module: self.module_of(x) });
                    }
                    let link = self.reads.len() as u32;
                    self.reads.push((x, self.open[p].reads));
                    let o = &mut self.open[p];
                    o.reads = link;
                    o.low = o.low.min(index);
                    return Ok(None);
                }
                self.met(x)
            }
            Step::Done(s) => {
                if self.open[p].reads == NONE || self.core.plant == CellPlant::DecideCachesOpenCycle {
                    // Read nothing open: its value is final (the plant keeps a value read off an
                    // open node too).
                    self.commit(n, s)?;
                    self.release(p);
                    self.walk.pop();
                    Ok(None)
                } else if self.open[p].low == self.open[p].index {
                    self.close(p)
                } else {
                    self.finish();
                    Ok(None)
                }
            }
        }
    }

    /// An undecided node outside the open stack was read.
    fn met(&mut self, x: Node) -> Result<Option<DecideWait>, DecideError> {
        if self.decided(x) {
            return Err(DecideError::Stuck { module: self.module_of(x) });
        }
        if let Node::Item(j) = x
            && let Some(at) = self.core.items[j as usize].call
            && !self.core.items[j as usize].grown
        {
            return Ok(Some(self.wait_on(j, at.key)));
        }
        self.enter(x);
        Ok(None)
    }

    fn wait_on(&self, j: ItemId, call: ItemKey) -> DecideWait {
        let mut by = None;
        for &n in self.walk.iter().rev() {
            if let Node::Item(i) = n {
                by = Some(if self.core.items[i as usize].call.is_some() { WaitBy::MacroPath } else { WaitBy::Import });
                break;
            }
        }
        let by = by.unwrap_or_else(|| match self.walk.first() {
            Some(&Node::Cell(c)) => WaitBy::Lookup(self.core.cells[c as usize].ns),
            _ => WaitBy::Lookup(Namespace::Types),
        });
        let module = self.core.modules[self.core.items[j as usize].module as usize].key;
        DecideWait { module, call, by }
    }

    /// The open nodes from `p` up are a group (`p`'s low link is its own index): decided
    /// together as their least fixed point, then concluded by the whole solve's own tail.
    fn close(&mut self, p: usize) -> Result<Option<DecideWait>, DecideError> {
        let mut members: AVec<'s, Node> = AVec::with_capacity_in(self.open.len() - p, self.scratch);
        members.extend(self.open[p..].iter().map(|o| o.node));
        let alloc = self.core.alloc;
        let mut guess = Guess { cells: ABTreeMap::new_in(alloc), items: ABTreeMap::new_in(alloc) };
        for &n in members.iter() {
            match n {
                Node::Cell(c) => {
                    guess.cells.insert(c, None);
                }
                Node::Item(i) => {
                    guess.items.insert(i, None);
                }
            }
        }
        let bound = 4 * members.len() + 4;
        let mut rounds = 0;
        loop {
            let mut changed = false;
            for k in 0..members.len() {
                let n = members[k];
                self.step()?;
                self.clear_waits(n);
                match self.core.member_step(n, &guess) {
                    Step::Done(GuessStep::Cell(v)) => {
                        if let Node::Cell(c) = n
                            && guess.cells.insert(c, v) != Some(v)
                        {
                            changed = true;
                        }
                    }
                    Step::Done(GuessStep::Item(o)) => {
                        if let Node::Item(i) = n
                            && guess.items.insert(i, o) != Some(o)
                        {
                            changed = true;
                        }
                    }
                    Step::Wait(y) => {
                        if let Some(q) = self.place(y) {
                            // Under a risen guess a member read an open node below the group:
                            // the group is part of a larger one, closed by an older root.
                            if q >= p {
                                return Err(DecideError::Stuck { module: self.module_of(y) });
                            }
                            let index = self.open[q].index;
                            let o = &mut self.open[p];
                            o.low = o.low.min(index);
                            self.finish();
                            return Ok(None);
                        }
                        // A new input: walked first, the group closed again after it.
                        return self.met(y);
                    }
                }
            }
            if !changed {
                break;
            }
            rounds += 1;
            if rounds > bound {
                let module = self.module_of(members[0]);
                return Err(DecideError::Core(CellError::NoFixpoint { module, cells: members.len() }));
            }
        }
        self.core.conclude(&members, &guess).map_err(DecideError::Core)?;
        self.release(p);
        self.walk.pop();
        Ok(None)
    }
}

/// Makes every import and cell of this crate undecided again, in place: the module tables
/// (texts, explicit entries, the named and glob indexes, calls and their expansions) are kept,
/// what was decided from them is not. Cells of dependencies' modules, enums' and unread modules
/// are decided when made and stay.
fn freeze<N: crate::Name>(core: &mut Cells<'_, N>) {
    for k in 0..core.cells.len() {
        let m = core.cells[k].module as usize;
        if core.modules[m].foreign || core.modules[m].unread {
            continue;
        }
        let c = &mut core.cells[k];
        c.value = None;
        c.waiters.clear();
        c.waiting_on = None;
    }
    for it in core.items.iter_mut() {
        it.waiters.clear();
        it.waits.clear();
        if it.call.is_some() {
            continue;
        }
        it.outcome = None;
        it.why = None;
        it.clamp = None;
        it.of_enum = false;
        it.seg = 0;
    }
    for m in core.modules.iter_mut() {
        m.edges.clear();
        m.globs_pending = m.glob_items.len() as u32;
    }
    core.cell_queue.clear();
    core.item_queue.clear();
    core.extra.clear();
    core.taken.clear();
    core.weak_error = None;
}

/// One cell the walk decided otherwise than the whole solve (`decided: None`: it waited).
#[derive(Clone, Copy, Debug)]
pub struct Difference<N> {
    pub module: ItemKey,
    pub ns: Namespace,
    pub name: N,
    pub layer: Layer,
    pub whole: Value,
    pub decided: Option<Value>,
}

/// [`check`]'s report.
#[derive(Debug)]
pub struct DecideCheck<'a, N> {
    /// The cells the whole solve decided (of this crate's modules).
    pub cells: u64,
    /// Summed over the forward, backward and text passes.
    pub differences: u64,
    pub waits: u64,
    /// Hops logged by the forward pass.
    pub hops: u64,
    /// The text pass: `Full` cells whose module has an explicit entry of the name.
    pub(crate) text_answered: u64,
    pub(crate) text_ns: u64,
    /// One hash probe of a cell key, in hundredths of a nanosecond ([`Cells::probe_cost`]).
    pub(crate) probe_centi_ns: u64,
    /// The forward pass, every cell.
    pub(crate) all_ns: u64,
    /// The first 8 differences, of any pass.
    pub examples: AVec<'a, Difference<N>>,
    /// The text pass taken apart: the bare rule alone, the hop's cell key alone.
    pub(crate) text_rule_ns: u64,
    pub(crate) text_key_ns: u64,
    /// Over the text pass's full decisions: tree-map node inserts (the open stack's `at`, the
    /// log's `seen`) and index lookups (`explicit`, `vis_paths`).
    pub(crate) text_inserts: u64,
    pub(crate) text_lookups: u64,
    /// D116 point 7: the records at module grain against the written scopes (cells/agree.rs),
    /// checked before `check` re-decides the core; `None` where the solve did not run it.
    pub(crate) links: Option<Result<super::Agreed, super::Disagreement<'a, N>>>,
}

impl<'a, N: crate::Name> DecideCheck<'a, N> {
    /// The equality check of names at module grain, when the solve ran it.
    pub fn links(&self) -> Option<&Result<super::Agreed, super::Disagreement<'a, N>>> {
        self.links.as_ref()
    }

    fn note(&mut self, core: &Cells<'_, N>, c: CellId, whole: Value, got: Decided) {
        let decided = match got {
            Decided::Value(v) if v == whole => return,
            Decided::Value(v) => Some(v),
            Decided::Wait(_) => {
                self.waits += 1;
                None
            }
        };
        self.differences += 1;
        if self.examples.len() < 8 {
            let cell = &core.cells[c as usize];
            let module = core.modules[cell.module as usize].key;
            self.examples.push(Difference { module, ns: cell.ns, name: cell.name, layer: cell.layer, whole, decided });
        }
    }
}

impl<N: fmt::Debug> fmt::Display for DecideCheck<'_, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let each_centi = if self.text_answered == 0 { 0 } else { self.text_ns * 100 / self.text_answered };
        let ratio_tenths = if self.probe_centi_ns == 0 { 0 } else { each_centi * 10 / self.probe_centi_ns };
        write!(
            f,
            "D116 decide: {} differences, {} cells (forward, backward and text passes), {} waits, {} hops; all cells forward in {} ns; \
             answered from text: {} cells, {}.{:02} ns each, probe {}.{:02} ns, {}.{} probes each",
            self.differences,
            self.cells,
            self.waits,
            self.hops,
            self.all_ns,
            self.text_answered,
            each_centi / 100,
            each_centi % 100,
            self.probe_centi_ns / 100,
            self.probe_centi_ns % 100,
            ratio_tenths / 10,
            ratio_tenths % 10,
        )?;
        for d in self.examples.iter() {
            write!(f, "\n  differs: module {:?} {:?} {:?} {:?}: whole solve {:?}, decided {:?}", d.module, d.ns, d.name, d.layer, d.whole, d.decided)?;
        }
        let per = |ns: u64| if self.text_answered == 0 { 0 } else { ns * 100 / self.text_answered };
        let (rule, key) = (per(self.text_rule_ns), per(self.text_key_ns));
        let walk = per(self.text_ns).saturating_sub(rule + key);
        let per10 = |n: u64| if self.text_answered == 0 { 0 } else { n * 10 / self.text_answered };
        let (ins, look) = (per10(self.text_inserts), per10(self.text_lookups));
        write!(
            f,
            "\ntext split: rule {}.{:02} ns, cell key {}.{:02} ns, walk+log {}.{:02} ns per cell; {}.{} tree-map inserts, {}.{} index lookups per cell",
            rule / 100,
            rule % 100,
            key / 100,
            key % 100,
            walk / 100,
            walk % 100,
            ins / 10,
            ins % 10,
            look / 10,
            look % 10
        )?;
        Ok(())
    }
}

/// The acceptance (equality): every cell the whole solve decided in `core` is decided again on
/// demand over the frozen tables, forward then backward, and compared; then the cells answered
/// from text are decided again under `now` (nanoseconds, the caller's clock) for the floor.
/// Leaves `core` decided by the last pass.
/// The report is made in `report` (the caller's allocator: a solve's `Cells` borrow its locals,
/// the report outlives them).
pub(crate) fn check<'a, 'o, N: crate::Name>(core: &mut Cells<'a, N>, now: &dyn Fn() -> u64, report: Alloc<'o>) -> Result<DecideCheck<'o, N>, DecideError> {
    let alloc = core.alloc;
    let mut want: AVec<'a, (CellId, Value)> = AVec::new_in(alloc);
    for (id, c) in core.cells.iter().enumerate() {
        let m = &core.modules[c.module as usize];
        if m.foreign || m.unread {
            continue;
        }
        if let Some(v) = c.value {
            want.push((id as CellId, v));
        }
    }
    let mut out = DecideCheck {
        cells: want.len() as u64,
        differences: 0,
        waits: 0,
        hops: 0,
        text_answered: 0,
        text_ns: 0,
        probe_centi_ns: 0,
        all_ns: 0,
        examples: AVec::new_in(report),
        text_rule_ns: 0,
        text_key_ns: 0,
        text_inserts: 0,
        text_lookups: 0,
        links: None,
    };
    let mut rows = 0u64;
    for backward in [false, true] {
        freeze(core);
        let mut d = Decider::new(core, alloc);
        let start = now();
        for k in 0..want.len() {
            let (c, v) = if backward { want[want.len() - 1 - k] } else { want[k] };
            let got = d.decide_cell(c)?;
            if !backward {
                out.hops += d.hops.len() as u64;
                for &h in d.hops.iter() {
                    rows = rows.wrapping_add(u64::from(d.row(h).cell.0[0]));
                }
            }
            out.note(d.core, c, v, got);
        }
        if !backward {
            out.all_ns = now() - start;
        }
    }
    core::hint::black_box(rows);
    // The floor's pass: a cell its module's text answers costs a cell lookup, the entry's
    // lookup and the log; nothing else is read. Run twice over one walk, and the second run
    // kept: warm, as an edit loop's decisions are (the first pays for the caches and for the
    // walk's own growth).
    let mut text: AVec<'a, (CellId, Value)> = AVec::new_in(alloc);
    for &(c, v) in want.iter() {
        let cell = &core.cells[c as usize];
        if cell.layer == Layer::Full && core.modules[cell.module as usize].explicit.contains_key(&(cell.ns, cell.name)) {
            text.push((c, v));
        }
    }
    out.text_answered = text.len() as u64;
    let mut got: AVec<'a, Decided> = AVec::with_capacity_in(text.len(), alloc);
    let mut d = Decider::new(core, alloc);
    for warm in [false, true] {
        freeze(d.core);
        got.clear();
        let start = now();
        for &(c, _) in text.iter() {
            got.push(d.decide_cell(c)?);
        }
        out.text_ns = now() - start;
        if warm {
            for (k, &(c, v)) in text.iter().enumerate() {
                out.note(d.core, c, v, got[k]);
            }
        }
        // Counted, not timed: what one full text decision does. The open stack and the log are
        // flat arrays (no tree-map insert); its rule reads `explicit` once and, through
        // `entry_vis`, `vis_paths` once.
        out.text_inserts = 0;
        out.text_lookups = 2 * text.len() as u64;
        // The text split, by removal (rule 23): the bare rule alone (no walk, no log). The hop
        // derives no key now (`cell key` reads 0; the walk is the text pass less the rule).
        freeze(d.core);
        let start = now();
        for &(c, _) in text.iter() {
            let g = Guess { cells: ABTreeMap::new_in(alloc), items: ABTreeMap::new_in(alloc) };
            if let Step::Done(GuessStep::Cell(v)) = d.core.member_step(Node::Cell(c), &g) {
                d.core.decide(c, v.unwrap_or(Value::NotFound));
            }
        }
        out.text_rule_ns = now() - start;
        out.text_key_ns = 0;
    }
    out.probe_centi_ns = d.core.probe_cost(now);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::format;

    use blaze_id::ItemKey;
    use blaze_sim::{Broke, Config};

    use super::{DecideWait, Decided, Decider, Value, WaitBy, check, freeze};
    use crate::cells::{CellPlant, Cells, Layer};
    use crate::tests::{N, V, World, reference_world};
    use crate::{CallShape, Edition, Namespace};

    /// The whole-crate solve of `w`, the tests' driver expanding each call it is asked to.
    fn whole(w: &World, root: ItemKey) -> Result<Cells<'static, N>, Broke> {
        w.expanded.borrow_mut().clear();
        let mut c = Cells::load(w, root, blaze_own::testing::heap().alloc()).map_err(|e| Broke::Oracle(format!("{e}")))?;
        let expand = |call: ItemKey, _: crate::Def| {
            w.expanded.borrow_mut().insert(call);
        };
        c.run(w, &expand, &[]).map_err(|e| Broke::Oracle(format!("{e}")))?;
        Ok(c)
    }

    /// `a` globs `b` and `d`, `b` globs `a`, `d` holds `x`; the root imports `crate::a::x`, so
    /// `a`'s cells of `x` are made, and decided again, before `b`'s.
    fn cycle_world() -> (World, ItemKey) {
        let mut w = World::new();
        let root = w.root(1, Edition::E2021);
        let a = w.module(root, "a", V::Pub);
        let b = w.module(root, "b", V::Pub);
        let d = w.module(root, "d", V::Pub);
        w.glob(a, "crate::b", V::Pub);
        w.glob(a, "crate::d", V::Pub);
        w.glob(b, "crate::a", V::Pub);
        w.fun(d, "x", V::Pub);
        w.use_(root, "crate::a::x", V::Priv);
        (w, root)
    }

    fn clock() -> impl Fn() -> u64 {
        let t = core::cell::Cell::new(0u64);
        move || {
            t.set(t.get() + 1);
            t.get()
        }
    }

    /// Expected line: `test cells::decide::tests::a_glob_cycle_decided_on_demand_equals_the_whole_solve ... ok`.
    /// The planted bug (a node that read an open node kept at once, not Absent until its group
    /// closes) makes `b`'s cells of `x` not found: the equality check catches it.
    #[test]
    fn a_glob_cycle_decided_on_demand_equals_the_whole_solve() {
        let (w, root) = cycle_world();
        let now = clock();
        let mut ok = whole(&w, root).expect("the whole solve runs");
        assert!(ok.stats.cycle_passes >= 1, "the whole solve met the cycle");
        let report = ok.alloc;
        let r = check(&mut ok, &now, report).expect("decides");
        assert!(r.cells > 0 && r.hops > 0, "{r}");
        assert_eq!((r.differences, r.waits), (0, 0), "{r}");
        let mut bad = whole(&w, root).expect("the whole solve runs");
        bad.plant = CellPlant::DecideCachesOpenCycle;
        let report = bad.alloc;
        let r = check(&mut bad, &now, report).expect("decides");
        assert!(r.differences > 0, "the control: the planted bug is caught: {r}");
    }

    /// Expected line: `test cells::decide::tests::a_call_not_grown_is_a_typed_wait_on_its_module ... ok`.
    /// Loaded, not run: `a`'s call has not grown, so a type lookup in `a` waits on `a`.
    #[test]
    fn a_call_not_grown_is_a_typed_wait_on_its_module() {
        let mut w = World::new();
        let root = w.root(1, Edition::E2021);
        let a = w.module(root, "a", V::Pub);
        let call = w.call(a, "m", CallShape::FnLike);
        let heap = blaze_own::testing::heap().alloc();
        let mut c = Cells::load(&w, root, heap).expect("loads");
        let m = *c.by_key.get(&a).expect("a is a module");
        let mut d = Decider::new(&mut c, heap);
        let got = d.decide(m, Namespace::Types, "x", Layer::Full).expect("decides");
        assert_eq!(got, Decided::Wait(DecideWait { module: a, call, by: WaitBy::Lookup(Namespace::Types) }));
    }

    const TEXT_NAMES: [&str; 8] = ["x0", "x1", "x2", "x3", "x4", "x5", "x6", "x7"];

    /// Expected line: `test cells::decide::tests::a_text_cell_is_decided_with_no_allocation ... ok`.
    /// The floor's claim: once the walk's arrays have grown, deciding a cell its module's text
    /// answers allocates nothing in the walk's frame. Control: the first decision, on empty
    /// arrays, does allocate there (the counter sees the frame).
    #[test]
    fn a_text_cell_is_decided_with_no_allocation() {
        let mut w = World::new();
        let root = w.root(1, Edition::E2021);
        let a = w.module(root, "a", V::Pub);
        for name in TEXT_NAMES {
            w.fun(a, name, V::Pub);
        }
        // Imported, so the whole solve makes their cells: a decision then makes none.
        for path in ["crate::a::x0", "crate::a::x1", "crate::a::x2", "crate::a::x3", "crate::a::x4", "crate::a::x5", "crate::a::x6", "crate::a::x7"] {
            w.use_(root, path, V::Priv);
        }
        let mut c = whole(&w, root).expect("the whole solve runs");
        let m = *c.by_key.get(&a).expect("a is a module");
        freeze(&mut c);
        let frame = blaze_own::Erased::new(blaze_own::testing::Counting::new(std::alloc::System));
        let mut d = Decider::new(&mut c, frame.handle());
        let made = frame.inner().allocations();
        let first = d.decide(m, Namespace::Values, TEXT_NAMES[0], Layer::Full).expect("decides");
        assert!(matches!(first, Decided::Value(Value::Found(..))), "{first:?}");
        let warm = frame.inner().allocations();
        assert!(warm > made, "the control: the first decision grows the frame's arrays");
        for &name in &TEXT_NAMES[1..] {
            let got = d.decide(m, Namespace::Values, name, Layer::Full).expect("decides");
            assert!(matches!(got, Decided::Value(Value::Found(..))), "{name}: {got:?}");
        }
        assert_eq!(frame.inner().allocations(), warm, "a cell answered from text allocated in the walk's frame");
    }

    const CASES: u32 = 500;

    /// Expected line: `D116 decide vs the whole solve: 500 drawn crates, 0 differences, <n> cells, <h> hops`.
    #[test]
    fn decided_on_demand_equals_the_whole_solve_on_drawn_crates() {
        let (mut cells, mut hops) = (0u64, 0u64);
        let now = clock();
        blaze_sim::check(Config::cases(CASES), |tape| {
            let (w, root) = reference_world(tape, true, true);
            let mut c = whole(&w, root)?;
            let report = c.alloc;
            let r = check(&mut c, &now, report).map_err(|e| Broke::Oracle(format!("{e}")))?;
            if r.differences > 0 || r.waits > 0 {
                return Err(Broke::Oracle(format!("{r}")));
            }
            cells += r.cells;
            hops += r.hops;
            Ok::<(), Broke>(())
        });
        std::println!("D116 decide vs the whole solve: {CASES} drawn crates, 0 differences, {cells} cells, {hops} hops");
        assert!(cells > 0 && hops > 0);
    }
}
