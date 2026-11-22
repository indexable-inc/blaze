//! The cold Code pool for one request. Construct it with [`CodePool::new`]. On each turn:
//!
//! 1. Call [`CodePool::wait`] for the named wait and its absolute deadline on the driver's
//!    clock. Receive one child, store, producer or worker event, or a tick at the deadline.
//!    `None` ends the request after the last child exits and outstanding work drains.
//! 2. Call [`CodePool::step`] with the time, event and an empty, reusable action buffer.
//!    Perform the actions appended to the buffer in order. A typed protocol or deadline error ends the
//!    request. Announce each available worker with `Free`, including after a completion.
//!
//! Discovery claims the full key and emits its lookup in the same turn. A worker can
//! report sorted direct calls before emission (`Direct`), then the full sorted
//! list including legalization helpers at `Finished`. A hit carries the same classified list.
//! Declaration of an indirect target stays with the child. No build waits for a call edge.
//! `AwaitProducer` subscribes a key to a fact, including an already settled fact. Return
//! one answer per subscription. A blocked worker has returned and can serve another key.
//!
//! A delivery answers a specific demand. Slots here are per-child first-demand ordinals.
//! At adoption, Needs maps these to its declared wire slots and sends personality, declarations,
//! data/TLS and then Publish. Keep the parent's declarations, placement, hooks and warm image.
//! Replace its cold want/pump, per-slot build waiters and failure fanout with this pool
//! (blaze-run/src/parent.rs:575, :587, :767, :777, :820). A ready reply can leave on the
//! demand turn as soon as the preceding demands OF ITS CHILD have replies: the order kept is
//! each child's own (D194), so one child's long make never holds another child's reply.
//! Exit withdraws the child's undelivered receipts. A build already given to a worker
//! returns exactly once. Its completion can settle the key without writing to an exited child.
//!
//! The frontier counts outstanding speculative keys, including builds. At capacity, skip
//! further hints. Keep at most its width of sorted hints per caller for future demanders.
//! One worker disables hints. The request's demand budget bounds all demand receipts, slots
//! and waiters. The same number of additional claims is reserved for speculative discoveries
//! over the request's lifetime. Exhausting that reserve skips new hints. Speculation cannot
//! consume a demand receipt. The absolute deadline covers lookup, admission to a worker,
//! build, producer settlement, ordered delivery, and the children themselves.

#![blaze::sans_io]

use blaze_drive::table::{Hashed, Table};
pub use blaze_drive::Key as CodeKey;
use blaze_id::ContentId;
use blaze_own::{SharedAlloc, VecDeque};

use crate::{SMap, SSet, SVec};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Child(pub usize);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Thread(pub u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Worker(pub usize);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot(pub usize);
/// A tick of the monotonic clock chosen by the driver, in the same units as its deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Time(pub u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Producer(pub ContentId);

#[derive(Clone, Copy)]
pub struct Options<'a> {
    pub alloc: SharedAlloc<'a>,
    pub hash_seed: u64,
    /// Children and workers have dense ids starting at zero.
    pub children: usize,
    pub workers: usize,
    pub frontier: usize,
    pub demands: usize,
    pub deadline: Time,
}

/// The identity of the immutable diagnostic, and the semantic input when it failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    Store(ContentId),
    Build(ContentId),
    Producer { producer: Producer, diagnostic: ContentId },
}

#[derive(Clone, Copy, Debug)]
pub struct Record<'r> {
    pub identity: ContentId,
    pub direct: &'r [CodeKey],
}

#[derive(Clone, Copy, Debug)]
pub enum Finished<'r> {
    Made(Record<'r>),
    Need(Producer),
    Refused(Cause),
}

#[derive(Clone, Copy, Debug)]
pub enum Event<'r> {
    Demand { child: Child, thread: Thread, key: CodeKey },
    Looked { key: CodeKey, result: Result<Option<Record<'r>>, Cause> },
    Direct { worker: Worker, key: CodeKey, callees: &'r [CodeKey] },
    Finished { worker: Worker, key: CodeKey, result: Finished<'r> },
    Produced { key: CodeKey, producer: Producer, result: Result<(), ContentId> },
    Exited(Child),
    Free(Worker),
    Tick,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Demand {
    pub child: Child,
    pub thread: Thread,
    pub slot: Slot,
    pub key: CodeKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Lookup(CodeKey),
    Build { worker: Worker, key: CodeKey, speculative: bool },
    AwaitProducer { key: CodeKey, producer: Producer },
    Publish { demand: Demand, record: ContentId },
    Refuse { demand: Demand, cause: Cause },
    /// The key lost its last sponsor before build. Remove the queued preparation. Already
    /// issued lookups and producer subscriptions still return once, even across a new demand.
    Cancel(CodeKey),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    Child(Child),
    Lookup(CodeKey),
    WorkerFor(CodeKey),
    Build { key: CodeKey, worker: Worker },
    Producer { key: CodeKey, producer: Producer },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    NoWorkers,
    UnknownChild(Child),
    ExitedChild(Child),
    WorkerNotAvailable(Worker),
    UnexpectedLookup(CodeKey),
    NotOwner { key: CodeKey, worker: Worker },
    UnexpectedProducer { key: CodeKey, producer: Producer },
    UnsortedDirect(CodeKey),
    DemandLimit { child: Child, key: CodeKey, limit: usize },
    Deadline { waiting: Wait, deadline: Time },
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoWorkers => f.write_str("the Code pool requires at least one worker"),
            Self::UnknownChild(child) => write!(f, "child {} is outside this Code pool", child.0),
            Self::ExitedChild(child) => write!(f, "child {} has already exited", child.0),
            Self::WorkerNotAvailable(worker) => write!(f, "unexpected free notification for worker {}", worker.0),
            Self::UnexpectedLookup(key) => write!(f, "no lookup outstanding for {key}"),
            Self::NotOwner { key, worker } => write!(f, "worker {} does not own {key}", worker.0),
            Self::UnexpectedProducer { key, producer } => write!(f, "{key} was not waiting on producer {}", producer.0),
            Self::UnsortedDirect(key) => write!(f, "direct calls of {key} arrived out of order"),
            Self::DemandLimit { child, key, limit } => write!(f, "child {} demanded {key} beyond the {limit}-demand limit", child.0),
            Self::Deadline { waiting, deadline } => write!(f, "Code pool deadline {} reached waiting for {waiting:?}", deadline.0),
        }
    }
}

impl core::error::Error for Error {}

// Table compares all 256 bits. Seeding its probe changes no identity or scheduling order.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Probe {
    key: CodeKey,
    seed: u64,
}

impl Hashed for Probe {
    fn hash(&self) -> u64 {
        self.key.hash() ^ self.seed
    }
}

type RowId = usize;
type Outcome = Result<ContentId, Cause>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Lookup,
    Queued,
    Building(Worker),
    WaitingForProducer(Producer),
    /// A cancelled hint with a known miss and possibly a known missing producer.
    Dormant(Option<Producer>),
    Ready { record: ContentId, source: Source },
    Refused(Cause),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source { Hit, Made }

impl State {
    fn outcome(self) -> Option<Outcome> {
        match self {
            Self::Ready { record, .. } => Some(Ok(record)),
            Self::Refused(cause) => Some(Err(cause)),
            _ => None,
        }
    }
}

struct Row<'a> {
    key: CodeKey,
    state: State,
    demanded: bool,
    /// Live children which demanded this caller, even after their reply.
    callers: SVec<'a, Child>,
    /// Live children sponsoring preparation of this key.
    sponsors: SVec<'a, Child>,
    direct: SVec<'a, CodeKey>,
}

struct ChildState<'a> {
    live: bool,
    slots: SMap<'a, CodeKey, Slot>,
    /// This child's demands not yet answered, in its own order (D194: the delivery order is
    /// per child; a global queue let one test's 77 s make hold every other test's reply).
    pending: VecDeque<(RowId, Demand), SharedAlloc<'a>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkerState {
    Unavailable,
    Free,
    Busy { row: RowId, speculative: bool },
}

pub struct CodePool<'a> {
    opts: Options<'a>,
    index: Table<Probe, RowId, SharedAlloc<'a>>,
    rows: SVec<'a, Row<'a>>,
    children: SVec<'a, ChildState<'a>>,
    workers: SVec<'a, WorkerState>,
    idle: VecDeque<Worker, SharedAlloc<'a>>,
    // false (demanded) sorts before true. Removal on promotion leaves one queue entry.
    queue: SSet<'a, (bool, RowId)>,
    frontier: SSet<'a, RowId>,
    background: usize,
    demands: usize,
    spec_claims: usize,
    #[cfg(test)]
    plant: Plant,
}

impl<'a> CodePool<'a> {
    pub fn new(opts: Options<'a>) -> Result<Self, Error> {
        if opts.workers == 0 {
            return Err(Error::NoWorkers);
        }
        let a = opts.alloc;
        let mut children = SVec::with_capacity_in(opts.children, a);
        children.resize_with(opts.children, || ChildState { live: true, slots: SMap::new_in(a), pending: VecDeque::new_in(a) });
        Ok(Self {
            opts, index: Table::new_in(a), rows: SVec::new_in(a), children,
            workers: SVec::from_elem_in(WorkerState::Unavailable, opts.workers, a),
            idle: VecDeque::new_in(a),
            queue: SSet::new_in(a), frontier: SSet::new_in(a),
            background: 0, demands: 0, spec_claims: 0,
            #[cfg(test)]
            plant: Plant::None,
        })
    }

    /// The first child's head demand names the critical wait. With no waiter, outstanding
    /// operations still need draining, including builds whose last interested child has exited.
    pub fn wait(&self) -> Option<(Wait, Time)> {
        let waiting = self.children.iter().find_map(|c| c.pending.front().and_then(|(id, _)| self.row_wait(*id)))
            .or_else(|| self.frontier.first().and_then(|&id| self.row_wait(id)))
            .or_else(|| self.children.iter().position(|c| c.live).map(|i| Wait::Child(Child(i))))
            .or_else(|| (0..self.rows.len()).find_map(|id| self.row_wait(id)))?;
        Some((waiting, self.opts.deadline))
    }

    fn row_wait(&self, id: RowId) -> Option<Wait> {
        let row = &self.rows[id];
        let key = row.key;
        Some(match row.state {
            State::Lookup => Wait::Lookup(key),
            State::Queued => Wait::WorkerFor(key),
            State::Building(worker) => Wait::Build { key, worker },
            State::WaitingForProducer(producer) => Wait::Producer { key, producer },
            State::Dormant(_) | State::Ready { .. } | State::Refused(_) => return None,
        })
    }

    fn find(&self, key: CodeKey) -> Option<RowId> {
        self.index.get(&Probe { key, seed: self.opts.hash_seed }).copied()
    }

    fn claim(&mut self, key: CodeKey, out: &mut SVec<'_, Action>) -> RowId {
        let id = self.rows.len();
        let a = self.opts.alloc;
        self.rows.push(Row { key, state: State::Lookup, demanded: false,
            callers: SVec::new_in(a), sponsors: SVec::new_in(a), direct: SVec::new_in(a) });
        self.index.insert(Probe { key, seed: self.opts.hash_seed }, id);
        out.push(Action::Lookup(key));
        id
    }

    fn live(&self, child: Child) -> Result<(), Error> {
        match self.children.get(child.0) {
            Some(c) if c.live => Ok(()),
            Some(_) => Err(Error::ExitedChild(child)),
            None => Err(Error::UnknownChild(child)),
        }
    }

    pub fn step(&mut self, now: Time, event: Event<'_>, out: &mut SVec<'_, Action>) -> Result<(), Error> {
        if now.0 >= self.opts.deadline.0 && let Some((waiting, deadline)) = self.wait() {
            return Err(Error::Deadline { waiting, deadline });
        }
        match event {
            Event::Demand { child, thread, key } => self.demand(child, thread, key, out)?,
            Event::Exited(child) => self.exit(child, out)?,
            Event::Free(worker) => match self.workers.get_mut(worker.0) {
                Some(state @ WorkerState::Unavailable) => {
                    *state = WorkerState::Free;
                    self.idle.push_back(worker);
                }
                _ => return Err(Error::WorkerNotAvailable(worker)),
            },
            Event::Looked { key, result } => {
                let id = self.find(key).filter(|&id| self.rows[id].state == State::Lookup)
                    .ok_or(Error::UnexpectedLookup(key))?;
                match result {
                    Ok(Some(record)) => self.record(id, record, Source::Hit, out)?,
                    Ok(None) => self.queue_miss(id),
                    Err(cause) => self.settle(id, State::Refused(cause)),
                }
            }
            Event::Direct { key, worker, callees } => {
                let (id, _) = self.owner(worker, key)?;
                self.edges(id, callees, out)?;
            }
            Event::Finished { key, worker, result } => {
                let (id, speculative) = self.owner(worker, key)?;
                match result {
                    Finished::Made(record) => self.record(id, record, Source::Made, out)?,
                    Finished::Refused(cause) => self.settle(id, State::Refused(cause)),
                    Finished::Need(producer) => {
                        if self.rows[id].demanded || !self.rows[id].sponsors.is_empty() {
                            self.await_producer(id, producer, out);
                        } else {
                            self.rows[id].state = State::Dormant(Some(producer));
                            self.frontier.remove(&id);
                            out.push(Action::Cancel(key));
                        }
                    }
                }
                self.workers[worker.0] = WorkerState::Unavailable;
                self.background -= usize::from(speculative);
            }
            Event::Produced { key, producer, result } => {
                let id = self.find(key).filter(|&id| self.rows[id].state == State::WaitingForProducer(producer))
                    .ok_or(Error::UnexpectedProducer { key, producer })?;
                match result {
                    Ok(()) => self.queue_miss(id),
                    Err(diagnostic) => self.settle(id, State::Refused(Cause::Producer { producer, diagnostic })),
                }
            }
            Event::Tick => {}
        }
        self.deliver(out);
        self.dispatch(out);
        Ok(())
    }

    fn owner(&self, worker: Worker, key: CodeKey) -> Result<(RowId, bool), Error> {
        match self.workers.get(worker.0).copied() {
            Some(WorkerState::Busy { row, speculative }) if self.rows[row].key == key => Ok((row, speculative)),
            _ => Err(Error::NotOwner { key, worker }),
        }
    }

    fn demand(&mut self, child: Child, thread: Thread, key: CodeKey, out: &mut SVec<'_, Action>) -> Result<(), Error> {
        self.live(child)?;
        if self.demands == self.opts.demands {
            return Err(Error::DemandLimit { child, key, limit: self.opts.demands });
        }
        self.demands += 1;
        let id = self.find(key).unwrap_or_else(|| self.claim(key, out));
        let state = &mut self.children[child.0];
        let next = Slot(state.slots.len());
        let slot = *state.slots.entry(key).or_insert(next);
        state.pending.push_back((id, Demand { child, thread, key, slot }));
        let row = &mut self.rows[id];
        if !row.demanded {
            self.frontier.remove(&id);
            row.sponsors.clear();
            row.demanded = true;
            if row.state == State::Queued {
                self.queue.remove(&(true, id));
                self.queue.insert((false, id));
            }
        }
        #[cfg(test)]
        if self.plant == Plant::DoubleOwner && matches!(row.state, State::Building(_)) {
            self.queue.insert((false, id));
        }
        if !row.callers.contains(&child) {
            row.callers.push(child);
            self.resume(id, out);
            self.expand(id, child, out);
        }
        Ok(())
    }

    fn edges(&mut self, id: RowId, direct: &[CodeKey], out: &mut SVec<'_, Action>) -> Result<(), Error> {
        if !direct.is_sorted() {
            return Err(Error::UnsortedDirect(self.rows[id].key));
        }
        let row = &mut self.rows[id];
        row.direct.clear();
        if self.opts.workers > 1 {
            for &callee in direct {
                if row.direct.len() == self.opts.frontier {
                    break;
                }
                if callee != row.key && row.direct.last() != Some(&callee) {
                    row.direct.push(callee);
                }
            }
        }
        for i in 0..row.callers.len() {
            let child = self.rows[id].callers[i];
            self.expand(id, child, out);
        }
        Ok(())
    }

    fn expand(&mut self, caller: RowId, child: Child, out: &mut SVec<'_, Action>) {
        for i in 0..self.rows[caller].direct.len() {
            let key = self.rows[caller].direct[i];
            let known = self.find(key);
            if let Some(id) = known && (self.rows[id].demanded || self.rows[id].state.outcome().is_some()) {
                continue;
            }
            if !known.is_some_and(|id| self.frontier.contains(&id)) && self.frontier.len() == self.opts.frontier {
                continue;
            }
            let id = match known {
                Some(id) => id,
                None if self.spec_claims < self.opts.demands => {
                    self.spec_claims += 1;
                    self.claim(key, out)
                }
                None => continue,
            };
            self.frontier.insert(id);
            let row = &mut self.rows[id];
            if !row.sponsors.contains(&child) {
                row.sponsors.push(child);
            }
            #[cfg(test)]
            if self.plant == Plant::EnterHint {
                row.demanded = true;
            }
            self.resume(id, out);
        }
    }

    fn resume(&mut self, id: RowId, out: &mut SVec<'_, Action>) {
        match self.rows[id].state {
            State::Dormant(Some(producer)) => self.await_producer(id, producer, out),
            State::Dormant(None) => self.queue_miss(id),
            _ => {}
        }
    }

    fn await_producer(&mut self, id: RowId, producer: Producer, out: &mut SVec<'_, Action>) {
        self.rows[id].state = State::WaitingForProducer(producer);
        out.push(Action::AwaitProducer { key: self.rows[id].key, producer });
    }

    fn queue_miss(&mut self, id: RowId) {
        let row = &mut self.rows[id];
        if row.demanded || !row.sponsors.is_empty() {
            row.state = State::Queued;
            self.queue.insert((!row.demanded, id));
        } else {
            row.state = State::Dormant(None);
            self.frontier.remove(&id);
        }
    }

    fn settle(&mut self, id: RowId, state: State) {
        self.rows[id].state = state;
        self.rows[id].sponsors.clear();
        self.frontier.remove(&id);
    }

    fn record(&mut self, id: RowId, record: Record<'_>, source: Source, out: &mut SVec<'_, Action>) -> Result<(), Error> {
        self.edges(id, record.direct, out)?;
        self.settle(id, State::Ready { record: record.identity, source });
        Ok(())
    }

    fn dispatch(&mut self, out: &mut SVec<'_, Action>) {
        while let Some(&(speculative, id)) = self.queue.first() {
            if speculative && self.background >= self.opts.workers - 1 {
                break;
            }
            let Some(worker) = self.idle.pop_front() else { break };
            self.queue.remove(&(speculative, id));
            self.workers[worker.0] = WorkerState::Busy { row: id, speculative };
            self.background += usize::from(speculative);
            self.rows[id].state = State::Building(worker);
            out.push(Action::Build { worker, key: self.rows[id].key, speculative });
        }
    }

    /// Each child's ready heads, in child order then that child's demand order.
    fn deliver(&mut self, out: &mut SVec<'_, Action>) {
        #[cfg(test)]
        let plant = self.plant;
        let rows = &self.rows;
        for child in self.children.iter_mut() {
            #[cfg(test)]
            if plant == Plant::CompletionOrder {
                child.pending.retain(|&(id, demand)| match rows[id].state.outcome() {
                    Some(result) => { Self::reply(demand, result, out); false }
                    None => true,
                });
                continue;
            }
            while let Some(&(id, demand)) = child.pending.front() {
                let Some(result) = rows[id].state.outcome() else { break };
                #[cfg(test)]
                if plant == Plant::LostRefusal && result.is_err() {
                    break;
                }
                child.pending.pop_front();
                Self::reply(demand, result, out);
            }
        }
    }

    fn reply(demand: Demand, result: Outcome, out: &mut SVec<'_, Action>) {
        out.push(match result {
            Ok(record) => Action::Publish { demand, record },
            Err(cause) => Action::Refuse { demand, cause },
        });
    }

    fn exit(&mut self, child: Child, out: &mut SVec<'_, Action>) -> Result<(), Error> {
        self.live(child)?;
        self.children[child.0].live = false;
        self.children[child.0].pending.clear();
        for key in self.children[child.0].slots.keys() {
            let id = self.index.get(&Probe { key: *key, seed: self.opts.hash_seed }).expect("a demanded slot has a claim");
            self.rows[*id].callers.retain(|&c| c != child);
        }
        #[cfg(test)]
        if self.plant == Plant::KeepOrphans {
            return Ok(());
        }
        self.frontier.retain(|&id| {
            let row = &mut self.rows[id];
            let Some(i) = row.sponsors.iter().position(|&c| c == child) else { return true };
            row.sponsors.swap_remove(i);
            if !row.sponsors.is_empty() || matches!(row.state, State::Building(_)) {
                return true;
            }
            out.push(Action::Cancel(row.key));
            if row.state == State::Queued {
                self.queue.remove(&(true, id));
                row.state = State::Dormant(None);
                return false;
            }
            // An issued lookup/subscription keeps its permit until its answer arrives.
            true
        });
        Ok(())
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plant { None, DoubleOwner, CompletionOrder, EnterHint, LostRefusal, KeepOrphans }

#[cfg(test)]
#[path = "code_pool_sim.rs"]
mod tests;

/// The relay that drives this pool for `blaze test`'s children (D194).
#[path = "code_pool_relay.rs"]
pub mod relay;
