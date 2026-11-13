//! The driver of one request: a sans-IO state machine. Its host performs each [`Effect`] (asks
//! the store, runs a task on the pool, checks a read list against the current surfaces) and
//! gives back the answer as an [`Input`], in any order.
//!
//! One request: check the certificate; on a hit, exit. On a miss, per unit in dependency order:
//! key it (a task: file images, the text graph, every key, before any work); ask for its manifest
//! by key, a hit meaning the whole unit is filed; else ask for each record (its surface, each
//! body) by key; on a miss of a [`Trace::Reuse`](crate::kind::Trace::Reuse) record, ask for the
//! record the last manifest names for the same item and check its read list: equal, and it is
//! filed under the new key byte for byte; else the task runs. Each unit's manifest is filed
//! after every record it names; the certificate after every manifest; then exit.
//!
//! Code (instances, programs) is blaze-program's, on the same vocabulary: `check` makes none.

#![blaze::sans_io]

use core::alloc::Allocator;
use std::collections::VecDeque;

use blaze_format::Aligned;
use blaze_ids::ContentId;

use crate::{
    Plant,
    kind::{Body, Key, Kind, Layout, Surface, Tag, UnitManifest},
    memo::{Arena, Memo},
    planted,
    pool::Ticket,
    record::{self, Entry, Fact, RecordError, StableId, Traced},
    store::Put,
};

/// One unit of the request: its dependencies, by index below its own, and the manifest the last
/// certified build filed for it (the certificate's list), whose entries name the records a read
/// list can be checked from.
#[derive(Clone, Copy, Debug)]
pub struct UnitPlan<'p> {
    pub deps: &'p [u32],
    pub last: Option<Key<UnitManifest>>,
}

#[derive(Clone, Copy, Debug)]
pub struct Plan<'p> {
    pub units: &'p [UnitPlan<'p>],
}

/// From the machine descriptor: how many tasks may run at once; 0 is the main thread alone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    pub workers: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlanError {
    DepOrder { unit: u32, dep: u32 },
}

/// A task the host runs on the pool.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Task {
    /// Key unit `unit`: its manifest's, surface's and bodies' keys, from text.
    Key { unit: u32 },
    Surface { unit: u32, key: Key<Surface> },
    Body { unit: u32, item: StableId, key: Key<Body> },
}

/// What a task gives back; bytes borrowed from the worker's frame, 16-aligned.
#[derive(Clone, Copy, Debug)]
pub enum Done<'a> {
    Keyed {
        manifest: Key<UnitManifest>,
        surface: Key<Surface>,
        bodies: &'a [(StableId, Key<Body>)],
    },
    Wrote {
        record: &'a [u8],
    },
}

/// A task that did not finish.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskFailed {
    /// The task panicked: only it fails (rule 20).
    Panicked { what: &'static str },
    /// The task refused its input, naming what.
    Refused { what: &'static str },
}

/// A store failure, by name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StoreFault {
    pub what: &'static str,
}

/// The answer to an effect. Record bytes are 16-aligned.
#[derive(Clone, Copy, Debug)]
pub enum Input<'a> {
    Certified { ticket: Ticket, hit: bool },
    Got { ticket: Ticket, record: Option<&'a [u8]> },
    Verified { ticket: Ticket, equal: bool },
    Ran { ticket: Ticket, outcome: Result<Done<'a>, TaskFailed> },
    Put { ticket: Ticket, put: Result<Put, StoreFault> },
    CertificateWritten { ticket: Ticket, result: Result<(), StoreFault> },
}

impl Input<'_> {
    fn ticket(&self) -> Ticket {
        match *self {
            Input::Certified { ticket, .. }
            | Input::Got { ticket, .. }
            | Input::Verified { ticket, .. }
            | Input::Ran { ticket, .. }
            | Input::Put { ticket, .. }
            | Input::CertificateWritten { ticket, .. } => ticket,
        }
    }
}

/// What the host is to do.
#[derive(Clone, Copy, Debug)]
pub enum Effect<'d> {
    Certify { ticket: Ticket },
    /// Read the record of `tag`, `key`: by pread for a flat one, mapped for a plain one.
    Get { ticket: Ticket, tag: Tag, key: ContentId, layout: Layout },
    /// Check each read against the current facts of unit `unit`'s universe: `equal` when every
    /// digest is the current one (a read of the item's own text against its text now).
    Verify { ticket: Ticket, unit: u32, item: StableId, reads: &'d [Fact] },
    Run { ticket: Ticket, task: Task },
    Put { ticket: Ticket, tag: Tag, key: ContentId, bytes: &'d [u8] },
    WriteCertificate { ticket: Ticket, units: &'d [Key<UnitManifest>] },
}

#[derive(Clone, Copy, Debug)]
pub enum Step<'d> {
    Do(Effect<'d>),
    /// Nothing to do until an answer comes: what is waited for.
    Wait(&'static str),
    Exit(&'d Outcome),
}

/// What a request did. Equal under every memo (the refinement oracle compares it).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Ran {
    pub keyed: u32,
    pub surfaces: u32,
    pub bodies: u32,
    /// Records filed under a moved key because their read list verified.
    pub reused: u32,
    pub manifests: u32,
}

/// Asks, which differ by memo.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Asks {
    pub gets: u32,
    pub memo_hits: u32,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Failure {
    Task { tag: Tag, unit: u32, failed: TaskFailed },
    /// Other bytes are filed under a key this request made: a key that misses an input.
    TwoValues { tag: Tag, key: ContentId },
    Store(StoreFault),
    /// A record that is not what its key says.
    Record { tag: Tag, key: ContentId, why: RecordError },
    /// The host broke the protocol: an answer to no effect, or of the wrong form.
    Misused(&'static str),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// The certificate held: nothing ran.
    Certified,
    Built { ran: Ran },
    Failed { ran: Ran, why: Failure },
    /// Nothing ready and nothing asked: a bug of the driver, said by name.
    Stalled { ran: Ran },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Work {
    Certify,
    Key(u32),
    Manifest(u32),
    Last(u32),
    Ask(u32, u32),
    Old(u32, u32),
    Verify(u32, u32),
    Run(u32, u32),
    Put(u32, u32),
    PutManifest(u32),
    Certificate,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum UnitState {
    Waiting,
    Keying,
    Manifest,
    Checking,
    Writing,
    Complete,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SlotState {
    Open,
    Done,
}

struct Slot {
    tag: Tag,
    item: StableId,
    key: ContentId,
    old: Option<ContentId>,
    state: SlotState,
    /// An old record whose reads are being checked, or the bytes being put.
    held: Option<Aligned>,
}

struct Unit<A: Allocator> {
    state: UnitState,
    waiting: u32,
    last: Option<Key<UnitManifest>>,
    manifest: Option<Key<UnitManifest>>,
    slots: Vec<Slot, A>,
    open: u32,
    last_entries: Vec<Entry, A>,
    manifest_bytes: Option<Aligned>,
}

enum Phase {
    Start,
    Certifying,
    Building,
    Filing,
    Done(Outcome),
}

/// The item a unit's surface is filed under in its manifest.
pub const SURFACE_ITEM: StableId = StableId(ContentId([0; 32]));

/// The driver of one request, over memo `M`, allocating in the request's frame `A`.
///
/// The invariants (tests.rs):
/// - A hit is only what a from-scratch build would file under the key; a record reused under a
///   moved key is byte for byte what redoing it would write.
/// - No task runs for a record the store or the memo holds.
/// - At most `max(workers, 1)` tasks run at once.
/// - A manifest is filed after every record it names, the certificate after every manifest.
/// - Every request ends in an [`Outcome`]: the host's loop is bounded by the effects asked.
pub struct Driver<'m, M: Memo, A: Allocator + Clone> {
    memo: &'m mut M,
    workers: u32,
    running: u32,
    phase: Phase,
    units: Vec<Unit<A>, A>,
    dependents: Vec<Vec<u32, A>, A>,
    ready: VecDeque<Work, A>,
    runs: VecDeque<Work, A>,
    pending: Vec<(Ticket, Work), A>,
    next_ticket: u64,
    manifests: Vec<Key<UnitManifest>, A>,
    complete: u32,
    ran: Ran,
    asks: Asks,
    ended: bool,
    alloc: A,
}

fn check<K: Kind>(bytes: &[u8], key: ContentId) -> Result<(), RecordError> {
    record::open::<K>(bytes, &Key::from_id(key)).map(|_| ())
}

fn check_tag(tag: Tag, bytes: &[u8], key: ContentId) -> Result<(), RecordError> {
    match tag {
        t if t == Surface::TAG => check::<Surface>(bytes, key),
        t if t == Body::TAG => check::<Body>(bytes, key),
        _ => check::<UnitManifest>(bytes, key),
    }
}

/// The memo's answer for a record the driver asks, checked against its key. A branch on what is
/// asked, never on the policy.
fn memo_check<M: Memo>(memo: &M, tag: Tag, arena: Arena, key: ContentId) -> Option<Result<(), RecordError>> {
    match tag {
        t if t == Surface::TAG => memo.get::<Surface>(arena, &Key::from_id(key)).map(|b| check::<Surface>(b, key)),
        t if t == Body::TAG => memo.get::<Body>(arena, &Key::from_id(key)).map(|b| check::<Body>(b, key)),
        _ => memo.get::<UnitManifest>(arena, &Key::from_id(key)).map(|b| check::<UnitManifest>(b, key)),
    }
}

fn memo_keep<M: Memo>(memo: &mut M, tag: Tag, arena: Arena, key: ContentId, bytes: &[u8]) {
    match tag {
        t if t == Surface::TAG => memo.keep::<Surface>(arena, &Key::from_id(key), bytes),
        t if t == Body::TAG => memo.keep::<Body>(arena, &Key::from_id(key), bytes),
        _ => memo.keep::<UnitManifest>(arena, &Key::from_id(key), bytes),
    }
}

fn layout_of(tag: Tag) -> Layout {
    match tag {
        t if t == Surface::TAG => Surface::LAYOUT,
        t if t == Body::TAG => Body::LAYOUT,
        _ => UnitManifest::LAYOUT,
    }
}

impl<'m, M: Memo, A: Allocator + Clone> Driver<'m, M, A> {
    pub fn new(memo: &'m mut M, plan: Plan<'_>, config: Config, alloc: A) -> Result<Self, PlanError> {
        let count = plan.units.len();
        let mut units = Vec::with_capacity_in(count, alloc.clone());
        let mut dependents = Vec::with_capacity_in(count, alloc.clone());
        for _ in 0..count {
            dependents.push(Vec::new_in(alloc.clone()));
        }
        for (index, unit) in plan.units.iter().enumerate() {
            for &dep in unit.deps {
                if dep as usize >= index {
                    return Err(PlanError::DepOrder { unit: index as u32, dep });
                }
                dependents[dep as usize].push(index as u32);
            }
            units.push(Unit {
                state: UnitState::Waiting,
                waiting: unit.deps.len() as u32,
                last: unit.last,
                manifest: None,
                slots: Vec::new_in(alloc.clone()),
                open: 0,
                last_entries: Vec::new_in(alloc.clone()),
                manifest_bytes: None,
            });
        }
        let mut manifests = Vec::with_capacity_in(count, alloc.clone());
        manifests.resize(count, Key::from_id(ContentId([0; 32])));
        Ok(Driver {
            memo,
            workers: config.workers.max(1),
            running: 0,
            phase: Phase::Start,
            units,
            dependents,
            ready: VecDeque::new_in(alloc.clone()),
            runs: VecDeque::new_in(alloc.clone()),
            pending: Vec::new_in(alloc.clone()),
            next_ticket: 0,
            manifests,
            complete: 0,
            ran: Ran::default(),
            asks: Asks::default(),
            ended: false,
            alloc,
        })
    }

    pub fn asks(&self) -> Asks {
        self.asks
    }

    pub fn ran(&self) -> Ran {
        self.ran
    }

    /// The next thing to do.
    pub fn next(&mut self) -> Step<'_> {
        match self.plan_next() {
            Some((work, ticket)) => Step::Do(self.effect(work, ticket)),
            None => match &self.phase {
                Phase::Done(outcome) => Step::Exit(outcome),
                _ => Step::Wait("the answers of the effects asked"),
            },
        }
    }

    fn ticket(&mut self, work: Work) -> Ticket {
        let ticket = Ticket(self.next_ticket);
        self.next_ticket += 1;
        self.pending.push((ticket, work));
        ticket
    }

    fn finish(&mut self, outcome: Outcome) {
        if matches!(self.phase, Phase::Done(_)) {
            return;
        }
        self.phase = Phase::Done(outcome);
        if !self.ended {
            self.ended = true;
            self.memo.end_request();
        }
    }

    fn fail(&mut self, why: Failure) {
        let ran = self.ran;
        self.finish(Outcome::Failed { ran, why });
    }

    /// The next work with its ticket, owned, so the effect can borrow the driver after.
    fn plan_next(&mut self) -> Option<(Work, Ticket)> {
        match self.phase {
            Phase::Done(_) => return None,
            Phase::Start => {
                self.phase = Phase::Certifying;
                return Some((Work::Certify, self.ticket(Work::Certify)));
            }
            _ => {}
        }
        loop {
            if let Some(work) = self.ready.pop_front() {
                if self.begin(work) {
                    return Some((work, self.ticket(work)));
                }
                if matches!(self.phase, Phase::Done(_)) {
                    return None;
                }
                continue;
            }
            if self.running < self.workers || planted(Plant::OverCommitsPool) {
                if let Some(work) = self.runs.pop_front() {
                    self.running += 1;
                    match work {
                        Work::Key(_) => self.ran.keyed += 1,
                        Work::Run(unit, slot) => match self.units[unit as usize].slots[slot as usize].tag {
                            t if t == Surface::TAG => self.ran.surfaces += 1,
                            _ => self.ran.bodies += 1,
                        },
                        _ => {}
                    }
                    return Some((work, self.ticket(work)));
                }
            }
            break;
        }
        if self.pending.is_empty() {
            let ran = self.ran;
            self.finish(Outcome::Stalled { ran });
        }
        None
    }

    /// Whether `work` needs an effect; a memo hit is settled here.
    fn begin(&mut self, work: Work) -> bool {
        let (unit, tag, key) = match work {
            Work::Manifest(unit) => match self.units[unit as usize].manifest {
                Some(key) => (unit, UnitManifest::TAG, key.id()),
                None => return true,
            },
            Work::Ask(unit, slot) => {
                let slot = &self.units[unit as usize].slots[slot as usize];
                (unit, slot.tag, slot.key)
            }
            _ => return true,
        };
        match memo_check(&*self.memo, tag, unit, key) {
            None => {
                self.asks.gets += 1;
                true
            }
            Some(Err(why)) => {
                self.fail(Failure::Record { tag, key, why });
                false
            }
            Some(Ok(())) => {
                self.asks.memo_hits += 1;
                match work {
                    Work::Manifest(unit) => self.unit_complete(unit),
                    Work::Ask(unit, slot) => self.slot_done(unit, slot),
                    _ => {}
                }
                false
            }
        }
    }

    fn effect(&self, work: Work, ticket: Ticket) -> Effect<'_> {
        let slot = |unit: u32, slot: u32| &self.units[unit as usize].slots[slot as usize];
        match work {
            Work::Certify => Effect::Certify { ticket },
            Work::Key(unit) => Effect::Run { ticket, task: Task::Key { unit } },
            Work::Manifest(unit) => Effect::Get {
                ticket,
                tag: UnitManifest::TAG,
                key: self.units[unit as usize].manifest.map_or(ContentId([0; 32]), |key| key.id()),
                layout: UnitManifest::LAYOUT,
            },
            Work::Last(unit) => Effect::Get {
                ticket,
                tag: UnitManifest::TAG,
                key: self.units[unit as usize].last.map_or(ContentId([0; 32]), |key| key.id()),
                layout: UnitManifest::LAYOUT,
            },
            Work::Ask(unit, at) => {
                let slot = slot(unit, at);
                Effect::Get { ticket, tag: slot.tag, key: slot.key, layout: layout_of(slot.tag) }
            }
            Work::Old(unit, at) => {
                let slot = slot(unit, at);
                let key = slot.old.unwrap_or(slot.key);
                Effect::Get { ticket, tag: slot.tag, key, layout: layout_of(slot.tag) }
            }
            Work::Verify(unit, at) => {
                let slot = slot(unit, at);
                let held = slot.held.as_deref().expect("an old record is held while its reads are checked");
                let root = blaze_format::open::<Traced>(held).expect("the old record opened when it was read");
                let reads = root.reads.follow(held).expect("the old record's reads opened when it was read");
                Effect::Verify { ticket, unit, item: slot.item, reads }
            }
            Work::Run(unit, at) => {
                let slot = slot(unit, at);
                let task = match slot.tag {
                    t if t == Surface::TAG => Task::Surface { unit, key: Key::from_id(slot.key) },
                    _ => Task::Body { unit, item: slot.item, key: Key::from_id(slot.key) },
                };
                Effect::Run { ticket, task }
            }
            Work::Put(unit, at) => {
                let slot = slot(unit, at);
                let bytes = slot.held.as_deref().expect("a record is held while it is put");
                Effect::Put { ticket, tag: slot.tag, key: slot.key, bytes }
            }
            Work::PutManifest(unit) => {
                let unit = &self.units[unit as usize];
                Effect::Put {
                    ticket,
                    tag: UnitManifest::TAG,
                    key: unit.manifest.map_or(ContentId([0; 32]), |key| key.id()),
                    bytes: unit.manifest_bytes.as_deref().expect("a manifest is held while it is put"),
                }
            }
            Work::Certificate => Effect::WriteCertificate { ticket, units: &self.manifests },
        }
    }

    /// Gives the answer to an effect.
    pub fn give(&mut self, input: Input<'_>) {
        if matches!(self.phase, Phase::Done(_)) {
            return;
        }
        let ticket = input.ticket();
        let Ok(at) = self.pending.binary_search_by_key(&ticket, |(ticket, _)| *ticket) else {
            return self.fail(Failure::Misused("an answer to no effect asked"));
        };
        let (_, work) = self.pending.remove(at);
        match (work, input) {
            (Work::Certify, Input::Certified { hit: true, .. }) => self.finish(Outcome::Certified),
            (Work::Certify, Input::Certified { hit: false, .. }) => self.start(),
            (Work::Key(unit), Input::Ran { outcome, .. }) => {
                self.running -= 1;
                match outcome {
                    Ok(Done::Keyed { manifest, surface, bodies }) => self.keyed(unit, manifest, surface, bodies),
                    Ok(Done::Wrote { .. }) => self.fail(Failure::Misused("a key task that wrote a record")),
                    Err(failed) => self.fail(Failure::Task { tag: UnitManifest::TAG, unit, failed }),
                }
            }
            (Work::Manifest(unit), Input::Got { record, .. }) => self.got_manifest(unit, record),
            (Work::Last(unit), Input::Got { record, .. }) => self.got_last(unit, record),
            (Work::Ask(unit, slot), Input::Got { record, .. }) => self.got_ask(unit, slot, record),
            (Work::Old(unit, slot), Input::Got { record, .. }) => self.got_old(unit, slot, record),
            (Work::Verify(unit, slot), Input::Verified { equal, .. }) => self.verified(unit, slot, equal),
            (Work::Run(unit, slot), Input::Ran { outcome, .. }) => {
                self.running -= 1;
                let (tag, key) = {
                    let slot = &self.units[unit as usize].slots[slot as usize];
                    (slot.tag, slot.key)
                };
                match outcome {
                    Ok(Done::Wrote { record }) => match check_tag(tag, record, key) {
                        Ok(()) => {
                            self.units[unit as usize].slots[slot as usize].held = Some(Aligned::copy_from(record));
                            self.ready.push_back(Work::Put(unit, slot));
                        }
                        Err(why) => self.fail(Failure::Record { tag, key, why }),
                    },
                    Ok(Done::Keyed { .. }) => self.fail(Failure::Misused("a record task that keyed")),
                    Err(failed) => self.fail(Failure::Task { tag, unit, failed }),
                }
            }
            (Work::Put(unit, slot), Input::Put { put, .. }) => {
                let (tag, key, held) = {
                    let slot = &mut self.units[unit as usize].slots[slot as usize];
                    (slot.tag, slot.key, slot.held.take())
                };
                match put {
                    Ok(Put::Written | Put::Same) => {
                        if let Some(held) = held {
                            memo_keep(&mut *self.memo, tag, unit, key, &held);
                        }
                        self.slot_done(unit, slot);
                    }
                    Ok(Put::Other) => self.fail(Failure::TwoValues { tag, key }),
                    Err(fault) => self.fail(Failure::Store(fault)),
                }
            }
            (Work::PutManifest(unit), Input::Put { put, .. }) => {
                let (key, held) = {
                    let unit = &mut self.units[unit as usize];
                    (unit.manifest.map_or(ContentId([0; 32]), |key| key.id()), unit.manifest_bytes.take())
                };
                match put {
                    Ok(Put::Written | Put::Same) => {
                        if let Some(held) = held {
                            memo_keep(&mut *self.memo, UnitManifest::TAG, unit, key, &held);
                        }
                        self.ran.manifests += 1;
                        self.unit_complete(unit);
                    }
                    Ok(Put::Other) => self.fail(Failure::TwoValues { tag: UnitManifest::TAG, key }),
                    Err(fault) => self.fail(Failure::Store(fault)),
                }
            }
            (Work::Certificate, Input::CertificateWritten { result, .. }) => match result {
                Ok(()) => {
                    let ran = self.ran;
                    self.finish(Outcome::Built { ran });
                }
                Err(fault) => self.fail(Failure::Store(fault)),
            },
            _ => self.fail(Failure::Misused("an answer of another form than its effect")),
        }
    }

    fn start(&mut self) {
        self.phase = Phase::Building;
        for unit in 0..self.units.len() {
            if self.units[unit].waiting == 0 {
                self.units[unit].state = UnitState::Keying;
                self.runs.push_back(Work::Key(unit as u32));
            }
        }
        if self.units.is_empty() {
            self.phase = Phase::Filing;
            self.ready.push_back(Work::Certificate);
        }
    }

    fn keyed(&mut self, unit: u32, manifest: Key<UnitManifest>, surface: Key<Surface>, bodies: &[(StableId, Key<Body>)]) {
        let this = &mut self.units[unit as usize];
        this.manifest = Some(manifest);
        this.state = UnitState::Manifest;
        this.slots.clear();
        let open = |tag: Tag, item: StableId, key: ContentId| Slot { tag, item, key, old: None, state: SlotState::Open, held: None };
        this.slots.push(open(Surface::TAG, SURFACE_ITEM, surface.id()));
        for &(item, key) in bodies {
            this.slots.push(open(Body::TAG, item, key.id()));
        }
        self.ready.push_back(Work::Manifest(unit));
    }

    fn got_manifest(&mut self, unit: u32, record: Option<&[u8]>) {
        let this = &self.units[unit as usize];
        let key = this.manifest.map_or(ContentId([0; 32]), |key| key.id());
        match record {
            Some(bytes) => match check::<UnitManifest>(bytes, key) {
                Ok(()) => {
                    memo_keep(&mut *self.memo, UnitManifest::TAG, unit, key, bytes);
                    self.unit_complete(unit);
                }
                Err(why) => self.fail(Failure::Record { tag: UnitManifest::TAG, key, why }),
            },
            None => match this.last {
                Some(last) if last.id() != key => self.ready.push_back(Work::Last(unit)),
                _ => self.start_checking(unit),
            },
        }
    }

    fn got_last(&mut self, unit: u32, record: Option<&[u8]>) {
        let Some(last) = self.units[unit as usize].last else {
            return self.fail(Failure::Misused("a last manifest asked of a unit with none"));
        };
        if let Some(bytes) = record {
            match record::open::<UnitManifest>(bytes, &last) {
                Ok(root) => match root.entries.follow(bytes) {
                    Ok(entries) => self.units[unit as usize].last_entries.extend_from_slice(entries),
                    Err(error) => {
                        let why = RecordError::Format(error);
                        return self.fail(Failure::Record { tag: UnitManifest::TAG, key: last.id(), why });
                    }
                },
                Err(why) => return self.fail(Failure::Record { tag: UnitManifest::TAG, key: last.id(), why }),
            }
        }
        self.start_checking(unit);
    }

    fn start_checking(&mut self, unit: u32) {
        let this = &mut self.units[unit as usize];
        this.state = UnitState::Checking;
        this.open = this.slots.len() as u32;
        for slot in this.slots.iter_mut() {
            slot.old = this
                .last_entries
                .iter()
                .find(|entry| entry.tag == slot.tag.0 && entry.item == slot.item.0)
                .map(|entry| entry.key)
                .filter(|old| *old != slot.key);
        }
        for slot in 0..this.slots.len() {
            self.ready.push_back(Work::Ask(unit, slot as u32));
        }
    }

    fn got_ask(&mut self, unit: u32, slot: u32, record: Option<&[u8]>) {
        let (tag, key, old) = {
            let slot = &self.units[unit as usize].slots[slot as usize];
            (slot.tag, slot.key, slot.old)
        };
        match record {
            Some(bytes) => match check_tag(tag, bytes, key) {
                Ok(()) => {
                    memo_keep(&mut *self.memo, tag, unit, key, bytes);
                    self.slot_done(unit, slot);
                }
                Err(why) => self.fail(Failure::Record { tag, key, why }),
            },
            None if old.is_some() => self.ready.push_back(Work::Old(unit, slot)),
            None => self.runs.push_back(Work::Run(unit, slot)),
        }
    }

    fn got_old(&mut self, unit: u32, slot: u32, record: Option<&[u8]>) {
        let (tag, old) = {
            let slot = &self.units[unit as usize].slots[slot as usize];
            (slot.tag, slot.old.unwrap_or(slot.key))
        };
        let Some(bytes) = record else {
            return self.runs.push_back(Work::Run(unit, slot));
        };
        if let Err(why) = check_tag(tag, bytes, old) {
            return self.fail(Failure::Record { tag, key: old, why });
        }
        self.units[unit as usize].slots[slot as usize].held = Some(Aligned::copy_from(bytes));
        self.ready.push_back(Work::Verify(unit, slot));
    }

    fn verified(&mut self, unit: u32, at: u32, equal: bool) {
        let slot = &mut self.units[unit as usize].slots[at as usize];
        let held = slot.held.take();
        if !equal {
            return self.runs.push_back(Work::Run(unit, at));
        }
        let reheaded = held.and_then(|old| record::reheaded(&old, slot.key));
        match reheaded {
            Some(bytes) => {
                slot.held = Some(Aligned::copy_from(&bytes));
                self.ran.reused += 1;
                self.ready.push_back(Work::Put(unit, at));
            }
            None => self.fail(Failure::Misused("a verified record that has no head")),
        }
    }

    fn slot_done(&mut self, unit: u32, slot: u32) {
        let this = &mut self.units[unit as usize];
        if this.slots[slot as usize].state == SlotState::Done || this.state != UnitState::Checking {
            this.slots[slot as usize].state = SlotState::Done;
            return;
        }
        this.slots[slot as usize].state = SlotState::Done;
        this.open -= 1;
        let early = planted(Plant::ManifestBeforeRecords) && this.open == 1;
        if this.open == 0 || early {
            self.write_manifest(unit);
        }
    }

    fn write_manifest(&mut self, unit: u32) {
        let this = &mut self.units[unit as usize];
        let Some(key) = this.manifest else {
            return self.fail(Failure::Misused("a manifest written before its unit was keyed"));
        };
        let mut entries = Vec::with_capacity_in(this.slots.len(), self.alloc.clone());
        entries.extend(this.slots.iter().map(|slot| Entry { item: slot.item.0, key: slot.key, tag: slot.tag.0, pad: 0 }));
        this.manifest_bytes = Some(Aligned::copy_from(&record::write_manifest(&key, &entries)));
        this.state = UnitState::Writing;
        self.ready.push_back(Work::PutManifest(unit));
    }

    fn unit_complete(&mut self, unit: u32) {
        let this = &mut self.units[unit as usize];
        if this.state == UnitState::Complete {
            return;
        }
        this.state = UnitState::Complete;
        let Some(manifest) = this.manifest else {
            return self.fail(Failure::Misused("a unit complete before it was keyed"));
        };
        self.manifests[unit as usize] = manifest;
        self.complete += 1;
        for at in 0..self.dependents[unit as usize].len() {
            let dependent = self.dependents[unit as usize][at] as usize;
            self.units[dependent].waiting -= 1;
            if self.units[dependent].waiting == 0 {
                self.units[dependent].state = UnitState::Keying;
                self.runs.push_back(Work::Key(dependent as u32));
            }
        }
        if self.complete as usize == self.units.len() {
            self.phase = Phase::Filing;
            self.ready.push_back(Work::Certificate);
        }
    }
}
