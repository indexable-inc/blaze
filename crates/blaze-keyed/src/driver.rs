//! The driver of one request: a sans-IO state machine. Its host performs each [`Effect`] (asks
//! the store, runs a task on the pool, answers a read list or a shape against the facts now) and
//! gives back the answer as an [`Input`], in any order.
//!
//! One request, in one [`Snapshot`], every check made once and kept as its proof (decision 18):
//!
//! - The certificate, an outside record ([`crate::outside`]): per shape filed under the
//!   request's base, the host answers its asks now, and a value at the address they make is
//!   [`Certified`]: the request is answered and nothing below is checked.
//! - Else, per unit in dependency order: key it (a task: file images, the text graph, every
//!   key, before any work); ask for its manifest by key. A hit is a `Holds<UnitManifest>`, which
//!   settles every record it names with no lookup each. A dependency whose surface is unchanged
//!   leaves its dependents' manifest keys equal (they cover dependencies' surfaces only), so
//!   none of their records is examined.
//! - Else per record (its surface, each body) by key: a hit is a `Holds`, and nothing else is
//!   asked for that item. On a miss of a [`Trace::Reuse`](crate::kind::Trace::Reuse) record,
//!   the record the last manifest names for the same item is a candidate, and its read list is
//!   checked once: equal, and it is filed under the new key byte for byte (so the next request
//!   hits by key); else the task runs.
//! - Each unit's manifest is filed after every record it names; the certificate after every
//!   manifest, at the address of what the request read; then exit.
//!
//! Code (instances, programs) is blaze-program's, on the same vocabulary: `check` makes none.

#![blaze::sans_io]

use core::alloc::Allocator;
use std::collections::VecDeque;

use blaze_format::Aligned;
use blaze_ids::ContentId;

use crate::{
    Plant,
    exec::{Executor, LOCAL_TASKS},
    kind::{Body, Certificate, Key, Kind, Layout, Namespace, Surface, Tag, UnitManifest},
    memo::{Arena, Memo},
    outside::{self, Filer, Found, Lookup, OutsideError, Want},
    planted,
    pool::Ticket,
    proof::{self, AnyHolds, Candidate, Certified, Checks, Holds, Snapshot, Verdict},
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
    /// The namespace the unit's records are filed in: [`crate::kind::namespace_of`] of its
    /// package, computed once per package by the host.
    pub namespace: Namespace,
}

#[derive(Clone, Copy, Debug)]
pub struct Plan<'p> {
    /// The request's base: the key its certificate's shapes are filed under.
    pub certificate: Key<Certificate>,
    /// The namespace the certificate is filed in: what the request's own machine reads
    /// ([`Namespace::LOCAL`] unless the machine is a trusted writer).
    pub namespace: Namespace,
    pub units: &'p [UnitPlan<'p>],
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
    /// To [`Effect::Get`].
    Got { ticket: Ticket, record: Option<&'a [u8]> },
    /// To [`Effect::Answer`]: the answer now of each ask of the shape, in its order.
    Answers { ticket: Ticket, now: &'a [ContentId] },
    /// To [`Effect::Verify`]: the digest now of each read's fact, in order (`None`: gone).
    Now { ticket: Ticket, now: &'a [Option<ContentId>] },
    Ran { ticket: Ticket, outcome: Result<Done<'a>, TaskFailed> },
    Put { ticket: Ticket, put: Result<Put, StoreFault> },
    /// To [`Effect::Reads`]: the request's outside reads, as a shape (the asks' bytes), their
    /// answers when the request began, and their answers now.
    Reads { ticket: Ticket, shape: &'a [u8], then: &'a [ContentId], now: &'a [ContentId] },
}

impl Input<'_> {
    fn ticket(&self) -> Ticket {
        match *self {
            Input::Got { ticket, .. }
            | Input::Answers { ticket, .. }
            | Input::Now { ticket, .. }
            | Input::Ran { ticket, .. }
            | Input::Put { ticket, .. }
            | Input::Reads { ticket, .. } => ticket,
        }
    }
}

/// The unit of an effect about the request itself (its certificate).
pub const NO_UNIT: u32 = u32::MAX;

/// What the host is to do.
#[derive(Clone, Copy, Debug)]
pub enum Effect<'d> {
    /// Read the record of `tag`, `key`, for `item` of `unit` ([`NO_UNIT`] and [`SURFACE_ITEM`]
    /// for the certificate; [`SURFACE_ITEM`] for a unit's manifest): by pread for a flat one,
    /// mapped for a plain one.
    Get { ticket: Ticket, unit: u32, item: StableId, tag: Tag, key: ContentId, layout: Layout },
    /// Answer each ask of `shape`, a shape filed under the outside record `key` of kind `tag`,
    /// as it is now: [`Input::Answers`].
    Answer { ticket: Ticket, tag: Tag, key: ContentId, shape: &'d [u8] },
    /// Give the digest now of each read's fact in unit `unit`'s universe: [`Input::Now`].
    Verify { ticket: Ticket, unit: u32, item: StableId, reads: &'d [Fact] },
    /// Run `task`: on the local pool, or, when `remote`, through the executor's
    /// [`RemoteService`](crate::exec::RemoteService) as an action over its inputs.
    Run { ticket: Ticket, task: Task, remote: bool },
    /// File `bytes` under `tag`, `key` in `namespace` ([`crate::store::Store::put`]).
    Put { ticket: Ticket, tag: Tag, key: ContentId, namespace: Namespace, bytes: &'d [u8] },
    /// Give the request's outside reads: [`Input::Reads`].
    Reads { ticket: Ticket },
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
    pub checks: Checks,
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
    /// The certificate's lookup or filing.
    Outside(OutsideError),
    /// An outside read of the request moved while it ran (ask `at` of its shape): nothing is
    /// certified; the next request builds against what is there then.
    Raced { at: u32 },
    /// The host broke the protocol: an answer to no effect, or of the wrong form.
    Misused(&'static str),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// The certificate held: nothing ran, and nothing below it was checked.
    Certified { checks: Checks },
    Built { ran: Ran },
    Failed { ran: Ran, why: Failure },
    /// Nothing ready and nothing asked: a bug of the driver, said by name.
    Stalled { ran: Ran },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Work {
    CertShape,
    CertAnswers,
    CertValue,
    Reads,
    FileRead,
    FilePut,
    Key(u32),
    Manifest(u32),
    Last(u32),
    Ask(u32, u32),
    Old(u32, u32),
    Verify(u32, u32),
    Run(u32, u32),
    Put(u32, u32),
    PutManifest(u32),
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

struct Slot<'s> {
    tag: Tag,
    item: StableId,
    key: ContentId,
    old: Option<ContentId>,
    state: SlotState,
    /// An old record whose reads are being checked, or the bytes being put.
    held: Option<Aligned>,
    /// The old record, as a candidate for the new key, until its one check.
    candidate: Option<Candidate<'s>>,
    /// The record's proof in this snapshot, once made.
    proved: Option<AnyHolds<'s>>,
}

impl<'s> Slot<'s> {
    fn open(tag: Tag, item: StableId, key: ContentId) -> Slot<'s> {
        Slot { tag, item, key, old: None, state: SlotState::Open, held: None, candidate: None, proved: None }
    }
}

struct Unit<'s, A: Allocator> {
    state: UnitState,
    waiting: u32,
    last: Option<Key<UnitManifest>>,
    manifest: Option<Key<UnitManifest>>,
    proof: Option<Holds<'s, UnitManifest>>,
    slots: Vec<Slot<'s>, A>,
    open: u32,
    last_entries: Vec<Entry, A>,
    manifest_bytes: Option<Aligned>,
    namespace: Namespace,
}

enum Phase {
    Start,
    Certifying,
    Building,
    Filing,
    Done(Outcome),
}

/// The driver's placement rule, whatever the executor: a key task, and every task while fewer
/// than [`LOCAL_TASKS`] are queued or running (the edit loop), run locally; a round trip costs
/// more than the task. Only a cold fan-out goes remote, in batches.
pub fn stays_local(task: &Task, queued: u32) -> bool {
    matches!(task, Task::Key { .. }) || queued < LOCAL_TASKS
}

/// The item a unit's surface is filed under in its manifest; also the item of a unit's manifest
/// and of the certificate in [`Effect::Get`].
pub const SURFACE_ITEM: StableId = StableId(ContentId([0; 32]));

/// The driver of one request in snapshot `'s`, over memo `M` and executor `X`, allocating in the
/// request's frame `A`.
///
/// The invariants (tests.rs):
/// - A hit is only what a from-scratch build would file under the key; a record reused under a
///   moved key is byte for byte what redoing it would write.
/// - Each item is looked up at most once and its read list verified at most once in a snapshot;
///   nothing is checked after a hit; nothing under a held manifest is looked up; no dependent of
///   an unchanged surface is examined; `checks.rechecks` is zero.
/// - No task runs for a record the store or the memo holds.
/// - At most `max(workers, 1)` tasks run at once.
/// - A manifest is filed after every record it names, the certificate after every manifest.
/// - Every request ends in an [`Outcome`]: the host's loop is bounded by the effects asked.
pub struct Driver<'s, 'm, M: Memo, X: Executor, A: Allocator + Clone> {
    snap: Snapshot<'s>,
    memo: &'m mut M,
    executor: X,
    workers: u32,
    running: u32,
    phase: Phase,
    units: Vec<Unit<'s, A>, A>,
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
    certificate: Key<Certificate>,
    namespace: Namespace,
    lookup: Lookup<'s, Certificate>,
    certified: Option<Certified<'s>>,
    filer: Option<Filer<Certificate>>,
    /// The shape being answered, then the request's own shape being filed.
    cert_shape: Option<Aligned>,
    cert_shape_record: Option<Aligned>,
    cert_value: Option<Aligned>,
}

/// The one key check of a record the driver asks, by its tag.
fn hit_any<'s>(snap: Snapshot<'s>, tag: Tag, key: ContentId, bytes: &[u8]) -> Result<AnyHolds<'s>, RecordError> {
    match tag {
        t if t == Surface::TAG => proof::hit::<Surface>(snap, Key::from_id(key), bytes).map(|hit| hit.holds().into()),
        t if t == Body::TAG => proof::hit::<Body>(snap, Key::from_id(key), bytes).map(|hit| hit.holds().into()),
        found => Err(RecordError::Kind { found, wanted: Body::TAG }),
    }
}

fn candidate_any<'s>(snap: Snapshot<'s>, tag: Tag, new: ContentId, old: ContentId, bytes: &[u8]) -> Result<Candidate<'s>, RecordError> {
    match tag {
        t if t == Surface::TAG => proof::candidate::<Surface>(snap, Key::from_id(new), Key::from_id(old), bytes),
        t if t == Body::TAG => proof::candidate::<Body>(snap, Key::from_id(new), Key::from_id(old), bytes),
        found => Err(RecordError::Kind { found, wanted: Body::TAG }),
    }
}

fn memo_keep<M: Memo>(memo: &mut M, arena: Arena, proved: AnyHolds<'_>, bytes: &[u8]) {
    if let Some(holds) = proved.get::<Surface>() {
        memo.keep(arena, &holds, bytes);
    } else if let Some(holds) = proved.get::<Body>() {
        memo.keep(arena, &holds, bytes);
    } else if let Some(holds) = proved.get::<UnitManifest>() {
        memo.keep(arena, &holds, bytes);
    }
}

/// A held manifest settles its unit: its slots become the records it names, each proved by it.
fn settle_by_manifest<'s, A: Allocator>(
    snap: Snapshot<'s>,
    unit: &mut Unit<'s, A>,
    key: Key<UnitManifest>,
    bytes: &[u8],
) -> Result<(), RecordError> {
    let hit = proof::hit::<UnitManifest>(snap, key, bytes)?;
    let named = hit.named()?;
    unit.slots.clear();
    for (item, proved) in named {
        let mut slot = Slot::open(proved.tag(), item, proved.key_id());
        slot.state = SlotState::Done;
        slot.proved = Some(proved);
        unit.slots.push(slot);
    }
    unit.proof = Some(hit.holds());
    Ok(())
}

fn layout_of(tag: Tag) -> Layout {
    match tag {
        t if t == Surface::TAG => Surface::LAYOUT,
        t if t == Body::TAG => Body::LAYOUT,
        _ => UnitManifest::LAYOUT,
    }
}

impl<'s, 'm, M: Memo, X: Executor, A: Allocator + Clone> Driver<'s, 'm, M, X, A> {
    /// A driver of one request in `snap`, over memo `M` and executor `X`, both static policies;
    /// at most `executor.capacity()` tasks run at once.
    pub fn new(snap: Snapshot<'s>, memo: &'m mut M, executor: X, plan: Plan<'_>, alloc: A) -> Result<Self, PlanError> {
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
                proof: None,
                slots: Vec::new_in(alloc.clone()),
                open: 0,
                last_entries: Vec::new_in(alloc.clone()),
                manifest_bytes: None,
                namespace: unit.namespace,
            });
        }
        let mut manifests = Vec::with_capacity_in(count, alloc.clone());
        manifests.resize(count, Key::from_id(ContentId([0; 32])));
        Ok(Driver {
            snap,
            memo,
            workers: executor.capacity().max(1),
            executor,
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
            certificate: plan.certificate,
            namespace: plan.namespace,
            lookup: Lookup::new(snap, plan.certificate),
            certified: None,
            filer: None,
            cert_shape: None,
            cert_shape_record: None,
            cert_value: None,
        })
    }

    pub fn executor(&self) -> &X {
        &self.executor
    }

    pub fn asks(&self) -> Asks {
        self.asks
    }

    pub fn ran(&self) -> Ran {
        self.ran
    }

    /// The certificate's proof, when it held.
    pub fn certified(&self) -> Option<Certified<'s>> {
        self.certified
    }

    /// Unit `unit`'s manifest's proof, once it is held or filed.
    pub fn manifest(&self, unit: u32) -> Option<Holds<'s, UnitManifest>> {
        self.units.get(unit as usize).and_then(|unit| unit.proof)
    }

    /// The records of unit `unit` proved so far, by item: what a later stage (blaze-program, an
    /// action's reader) takes instead of checking again.
    pub fn proofs(&self, unit: u32) -> impl Iterator<Item = (StableId, AnyHolds<'s>)> + '_ {
        self.units
            .get(unit as usize)
            .into_iter()
            .flat_map(|unit| unit.slots.iter().filter_map(|slot| slot.proved.map(|proved| (slot.item, proved))))
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
                self.cert_step();
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
                        Work::Run(unit, slot) => {
                            self.ran.checks.records_run += 1;
                            match self.units[unit as usize].slots[slot as usize].tag {
                                t if t == Surface::TAG => self.ran.surfaces += 1,
                                _ => self.ran.bodies += 1,
                            }
                        }
                        _ => {}
                    }
                    return Some((work, self.ticket(work)));
                }
            }
            break;
        }
        if self.pending.is_empty() && !matches!(self.phase, Phase::Done(_)) {
            let ran = self.ran;
            self.finish(Outcome::Stalled { ran });
        }
        None
    }

    /// Whether `work` needs an effect; a memo hit is settled here. Counts each lookup, and a
    /// check of an item already proved (a bug: `rechecks`).
    fn begin(&mut self, work: Work) -> bool {
        match work {
            Work::Manifest(unit) => {
                let Some(key) = self.units[unit as usize].manifest else {
                    return true;
                };
                self.ran.checks.key_lookups += 1;
                let settled = match self.memo.get::<UnitManifest>(unit, &key) {
                    None => None,
                    Some(bytes) => Some(settle_by_manifest(self.snap, &mut self.units[unit as usize], key, bytes)),
                };
                match settled {
                    None => {
                        self.asks.gets += 1;
                        true
                    }
                    Some(Ok(())) => {
                        self.asks.memo_hits += 1;
                        self.held_manifest(unit);
                        false
                    }
                    Some(Err(why)) => {
                        self.fail(Failure::Record { tag: UnitManifest::TAG, key: key.id(), why });
                        false
                    }
                }
            }
            Work::Ask(unit, at) => {
                let (tag, key, proved) = {
                    let slot = &self.units[unit as usize].slots[at as usize];
                    (slot.tag, slot.key, slot.proved.is_some())
                };
                if proved {
                    self.ran.checks.rechecks += 1;
                }
                self.ran.checks.key_lookups += 1;
                let found = match tag {
                    t if t == Surface::TAG => self.memo.get::<Surface>(unit, &Key::from_id(key)),
                    t if t == Body::TAG => self.memo.get::<Body>(unit, &Key::from_id(key)),
                    _ => None,
                }
                .map(|bytes| hit_any(self.snap, tag, key, bytes));
                match found {
                    None => {
                        self.asks.gets += 1;
                        true
                    }
                    Some(Ok(holds)) => {
                        self.asks.memo_hits += 1;
                        self.units[unit as usize].slots[at as usize].proved = Some(holds);
                        self.slot_done(unit, at);
                        false
                    }
                    Some(Err(why)) => {
                        self.fail(Failure::Record { tag, key, why });
                        false
                    }
                }
            }
            Work::Old(unit, at) | Work::Verify(unit, at) => {
                if self.units[unit as usize].slots[at as usize].proved.is_some() {
                    self.ran.checks.rechecks += 1;
                }
                if matches!(work, Work::Old(..)) {
                    self.ran.checks.key_lookups += 1;
                    self.asks.gets += 1;
                }
                true
            }
            Work::Last(_) | Work::CertShape | Work::CertValue | Work::FileRead => {
                self.asks.gets += 1;
                if matches!(work, Work::Last(_)) {
                    self.ran.checks.key_lookups += 1;
                }
                true
            }
            _ => true,
        }
    }

    fn effect(&self, work: Work, ticket: Ticket) -> Effect<'_> {
        let slot = |unit: u32, slot: u32| &self.units[unit as usize].slots[slot as usize];
        let manifest_get = |unit: u32, key: Option<Key<UnitManifest>>| Effect::Get {
            ticket,
            unit,
            item: SURFACE_ITEM,
            tag: UnitManifest::TAG,
            key: key.map_or(ContentId([0; 32]), |key| key.id()),
            layout: UnitManifest::LAYOUT,
        };
        let cert_get = |key: Key<Certificate>| Effect::Get {
            ticket,
            unit: NO_UNIT,
            item: SURFACE_ITEM,
            tag: Certificate::TAG,
            key: key.id(),
            layout: Certificate::LAYOUT,
        };
        let none = ContentId([0; 32]);
        match work {
            Work::CertShape | Work::CertValue => match self.lookup.want() {
                Want::ReadShape(at) | Want::ReadValue(at) => cert_get(at),
                _ => cert_get(Key::from_id(none)),
            },
            Work::CertAnswers => Effect::Answer {
                ticket,
                tag: Certificate::TAG,
                key: self.certificate.id(),
                shape: self.cert_shape.as_deref().unwrap_or(&[]),
            },
            Work::Reads => Effect::Reads { ticket },
            Work::FileRead | Work::FilePut => {
                let want = self.filer.as_ref().map_or(Want::Done, |filer| filer.want());
                match want {
                    Want::ReadShape(at) => cert_get(at),
                    Want::PutShape(at) => Effect::Put {
                        ticket,
                        tag: Certificate::TAG,
                        key: at.id(),
                        namespace: self.namespace,
                        bytes: self.cert_shape_record.as_deref().unwrap_or(&[]),
                    },
                    Want::PutValue(at) => Effect::Put {
                        ticket,
                        tag: Certificate::TAG,
                        key: at.id(),
                        namespace: self.namespace,
                        bytes: self.cert_value.as_deref().unwrap_or(&[]),
                    },
                    _ => cert_get(Key::from_id(none)),
                }
            }
            Work::Key(unit) => Effect::Run { ticket, task: Task::Key { unit }, remote: false },
            Work::Manifest(unit) => manifest_get(unit, self.units[unit as usize].manifest),
            Work::Last(unit) => manifest_get(unit, self.units[unit as usize].last),
            Work::Ask(unit, at) => {
                let slot = slot(unit, at);
                Effect::Get { ticket, unit, item: slot.item, tag: slot.tag, key: slot.key, layout: layout_of(slot.tag) }
            }
            Work::Old(unit, at) => {
                let slot = slot(unit, at);
                let key = slot.old.unwrap_or(slot.key);
                Effect::Get { ticket, unit, item: slot.item, tag: slot.tag, key, layout: layout_of(slot.tag) }
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
                let queued = self.runs.len() as u32 + self.running;
                Effect::Run { ticket, task, remote: X::REMOTE && !stays_local(&task, queued) }
            }
            Work::Put(unit, at) => {
                let namespace = self.units[unit as usize].namespace;
                let slot = slot(unit, at);
                let bytes = slot.held.as_deref().expect("a record is held while it is put");
                Effect::Put { ticket, tag: slot.tag, key: slot.key, namespace, bytes }
            }
            Work::PutManifest(unit) => {
                let unit = &self.units[unit as usize];
                Effect::Put {
                    ticket,
                    tag: UnitManifest::TAG,
                    key: unit.manifest.map_or(none, |key| key.id()),
                    namespace: unit.namespace,
                    bytes: unit.manifest_bytes.as_deref().expect("a manifest is held while it is put"),
                }
            }
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
            (Work::CertShape, Input::Got { record, .. }) => match self.lookup.shape(record, &mut self.ran.checks) {
                Ok(shape) => {
                    self.cert_shape = shape.map(Aligned::copy_from);
                    self.cert_step();
                }
                Err(why) => self.fail(Failure::Outside(why)),
            },
            (Work::CertAnswers, Input::Answers { now, .. }) => match self.lookup.answers(now, &mut self.ran.checks) {
                Ok(()) => self.cert_step(),
                Err(why) => self.fail(Failure::Outside(why)),
            },
            (Work::CertValue, Input::Got { record, .. }) => match self.lookup.value(record, &mut self.ran.checks) {
                Ok(()) => self.cert_step(),
                Err(why) => self.fail(Failure::Outside(why)),
            },
            (Work::Reads, Input::Reads { shape, then, now, .. }) => self.reads(shape, then, now),
            (Work::FileRead, Input::Got { record, .. }) => {
                let shape = self.cert_shape.as_deref().unwrap_or(&[]);
                let read = match self.filer.as_mut() {
                    Some(filer) => filer.read(record, shape, &mut self.ran.checks),
                    None => Err(OutsideError::Misused("a certificate read with no filing")),
                };
                match read {
                    Ok(()) => self.file_step(),
                    Err(why) => self.fail(Failure::Outside(why)),
                }
            }
            (Work::FilePut, Input::Put { put, .. }) => {
                let filed = match (put, self.filer.as_mut()) {
                    (Ok(put), Some(filer)) => filer.put(put).map_err(Failure::Outside),
                    (Err(fault), _) => Err(Failure::Store(fault)),
                    (_, None) => Err(Failure::Misused("a certificate put with no filing")),
                };
                match filed {
                    Ok(()) => self.file_step(),
                    Err(why) => self.fail(why),
                }
            }
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
            (Work::Verify(unit, slot), Input::Now { now, .. }) => self.verified(unit, slot, now),
            (Work::Run(unit, at), Input::Ran { outcome, .. }) => {
                self.running -= 1;
                let (tag, key) = {
                    let slot = &self.units[unit as usize].slots[at as usize];
                    (slot.tag, slot.key)
                };
                match outcome {
                    Ok(Done::Wrote { record }) => match hit_any(self.snap, tag, key, record) {
                        Ok(proved) => {
                            let slot = &mut self.units[unit as usize].slots[at as usize];
                            slot.held = Some(Aligned::copy_from(record));
                            slot.proved = Some(proved);
                            self.ready.push_back(Work::Put(unit, at));
                        }
                        Err(why) => self.fail(Failure::Record { tag, key, why }),
                    },
                    Ok(Done::Keyed { .. }) => self.fail(Failure::Misused("a record task that keyed")),
                    Err(failed) => self.fail(Failure::Task { tag, unit, failed }),
                }
            }
            (Work::Put(unit, slot), Input::Put { put, .. }) => {
                let (tag, key, held, proved) = {
                    let slot = &mut self.units[unit as usize].slots[slot as usize];
                    (slot.tag, slot.key, slot.held.take(), slot.proved)
                };
                match put {
                    Ok(Put::Written | Put::Same) => {
                        if let (Some(held), Some(proved)) = (held, proved) {
                            memo_keep(&mut *self.memo, unit, proved, &held);
                        }
                        self.slot_done(unit, slot);
                    }
                    Ok(Put::Other) => self.fail(Failure::TwoValues { tag, key }),
                    Err(fault) => self.fail(Failure::Store(fault)),
                }
            }
            (Work::PutManifest(unit), Input::Put { put, .. }) => {
                let (key, held, proof) = {
                    let unit = &mut self.units[unit as usize];
                    (unit.manifest.map_or(ContentId([0; 32]), |key| key.id()), unit.manifest_bytes.take(), unit.proof)
                };
                match put {
                    Ok(Put::Written | Put::Same) => {
                        if let (Some(held), Some(proof)) = (held, proof) {
                            memo_keep(&mut *self.memo, unit, proof.into(), &held);
                        }
                        self.ran.manifests += 1;
                        self.unit_complete(unit);
                    }
                    Ok(Put::Other) => self.fail(Failure::TwoValues { tag: UnitManifest::TAG, key }),
                    Err(fault) => self.fail(Failure::Store(fault)),
                }
            }
            _ => self.fail(Failure::Misused("an answer of another form than its effect")),
        }
    }

    /// The certificate's lookup, one step: the next read or answer, or its end.
    fn cert_step(&mut self) {
        match self.lookup.want() {
            Want::ReadShape(_) => self.ready.push_back(Work::CertShape),
            Want::ReadValue(_) => self.ready.push_back(Work::CertValue),
            Want::Answers => self.ready.push_back(Work::CertAnswers),
            _ => match self.lookup.found() {
                Some(Found::Hit(holds)) => {
                    self.certified = Some(holds.certified());
                    let checks = self.ran.checks;
                    self.finish(Outcome::Certified { checks });
                }
                _ => {
                    self.cert_shape = None;
                    self.start();
                }
            },
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
            self.ready.push_back(Work::Reads);
        }
    }

    /// The request's outside reads, settled: its certificate is filed at their address, its
    /// value every unit's manifest key.
    fn reads(&mut self, shape: &[u8], then: &[ContentId], now: &[ContentId]) {
        let settled = match outside::settle(self.certificate, shape, then, now, &mut self.ran.checks) {
            Ok(settled) => settled,
            Err(raced) => return self.fail(Failure::Raced { at: raced.at }),
        };
        // The lookup missed in this snapshot: it read the slots, so filing reads none again.
        let filer = match self.lookup.filer(settled, shape) {
            Ok(filer) => filer,
            Err(why) => return self.fail(Failure::Outside(why)),
        };
        let mut payload = Vec::with_capacity_in(self.manifests.len() * 32, self.alloc.clone());
        self.manifests.iter().for_each(|key| payload.extend_from_slice(&key.id().0));
        self.cert_value = Some(Aligned::copy_from(&outside::write_value(&settled.address, &[], &payload)));
        self.cert_shape = Some(Aligned::copy_from(shape));
        self.filer = Some(filer);
        self.file_step();
    }

    fn file_step(&mut self) {
        let want = self.filer.as_ref().map_or(Want::Done, |filer| filer.want());
        match want {
            Want::ReadShape(_) => self.ready.push_back(Work::FileRead),
            Want::PutShape(at) => {
                let shape = self.cert_shape.as_deref().unwrap_or(&[]);
                self.cert_shape_record = Some(Aligned::copy_from(&outside::write_shape(&at, shape)));
                self.ready.push_back(Work::FilePut);
            }
            Want::PutValue(_) => self.ready.push_back(Work::FilePut),
            _ => {
                let ran = self.ran;
                self.finish(Outcome::Built { ran });
            }
        }
    }

    fn keyed(&mut self, unit: u32, manifest: Key<UnitManifest>, surface: Key<Surface>, bodies: &[(StableId, Key<Body>)]) {
        let this = &mut self.units[unit as usize];
        this.manifest = Some(manifest);
        this.state = UnitState::Manifest;
        this.slots.clear();
        this.slots.push(Slot::open(Surface::TAG, SURFACE_ITEM, surface.id()));
        for &(item, key) in bodies {
            this.slots.push(Slot::open(Body::TAG, item, key.id()));
        }
        self.ready.push_back(Work::Manifest(unit));
    }

    /// A unit settled by its manifest: complete, with nothing it names looked up.
    fn held_manifest(&mut self, unit: u32) {
        if planted(Plant::LooksUpUnderHeldManifest) {
            for slot in 0..self.units[unit as usize].slots.len() {
                self.ready.push_back(Work::Ask(unit, slot as u32));
            }
        }
        self.unit_complete(unit);
    }

    fn got_manifest(&mut self, unit: u32, record: Option<&[u8]>) {
        let Some(key) = self.units[unit as usize].manifest else {
            return self.fail(Failure::Misused("a manifest asked of a unit not keyed"));
        };
        match record {
            Some(bytes) => match settle_by_manifest(self.snap, &mut self.units[unit as usize], key, bytes) {
                Ok(()) => {
                    if let Some(proof) = self.units[unit as usize].proof {
                        memo_keep(&mut *self.memo, unit, proof.into(), bytes);
                    }
                    self.held_manifest(unit);
                }
                Err(why) => self.fail(Failure::Record { tag: UnitManifest::TAG, key: key.id(), why }),
            },
            None => match self.units[unit as usize].last {
                Some(last) if last != key => self.ready.push_back(Work::Last(unit)),
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

    fn got_ask(&mut self, unit: u32, at: u32, record: Option<&[u8]>) {
        let (tag, key, old) = {
            let slot = &self.units[unit as usize].slots[at as usize];
            (slot.tag, slot.key, slot.old)
        };
        match record {
            Some(bytes) => match hit_any(self.snap, tag, key, bytes) {
                Ok(proved) => {
                    self.units[unit as usize].slots[at as usize].proved = Some(proved);
                    memo_keep(&mut *self.memo, unit, proved, bytes);
                    if planted(Plant::ChecksAfterHit) && old.is_some() {
                        self.ready.push_back(Work::Old(unit, at));
                    }
                    self.slot_done(unit, at);
                }
                Err(why) => self.fail(Failure::Record { tag, key, why }),
            },
            None if old.is_some() => self.ready.push_back(Work::Old(unit, at)),
            None => self.runs.push_back(Work::Run(unit, at)),
        }
    }

    fn got_old(&mut self, unit: u32, at: u32, record: Option<&[u8]>) {
        let (tag, key, old) = {
            let slot = &self.units[unit as usize].slots[at as usize];
            (slot.tag, slot.key, slot.old.unwrap_or(slot.key))
        };
        let Some(bytes) = record else {
            return self.runs.push_back(Work::Run(unit, at));
        };
        match candidate_any(self.snap, tag, key, old, bytes) {
            Ok(candidate) => {
                let slot = &mut self.units[unit as usize].slots[at as usize];
                slot.candidate = Some(candidate);
                slot.held = Some(Aligned::copy_from(bytes));
                self.ready.push_back(Work::Verify(unit, at));
            }
            Err(why) => self.fail(Failure::Record { tag, key: old, why }),
        }
    }

    fn verified(&mut self, unit: u32, at: u32, now: &[Option<ContentId>]) {
        let slot = &mut self.units[unit as usize].slots[at as usize];
        let key = slot.key;
        let (Some(candidate), Some(held)) = (slot.candidate.take(), slot.held.take()) else {
            return self.fail(Failure::Misused("an answer to a read list with no candidate"));
        };
        let reads = match blaze_format::open::<Traced>(&held).and_then(|root| root.reads.follow(&held)) {
            Ok(reads) => reads,
            Err(error) => return self.fail(Failure::Record { tag: candidate.tag(), key, why: RecordError::Format(error) }),
        };
        match candidate.verify(reads, now, &mut self.ran.checks) {
            Ok(Verdict::Holds(proved)) => match record::reheaded(&held, key) {
                Some(bytes) => {
                    let slot = &mut self.units[unit as usize].slots[at as usize];
                    slot.held = Some(Aligned::copy_from(&bytes));
                    slot.proved = Some(proved);
                    self.ran.reused += 1;
                    self.ready.push_back(Work::Put(unit, at));
                }
                None => self.fail(Failure::Misused("a verified record that has no head")),
            },
            Ok(Verdict::Redo { .. }) => self.runs.push_back(Work::Run(unit, at)),
            Err(_) => self.fail(Failure::Misused("a read list answered with another number of facts than it has")),
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
        let bytes = Aligned::copy_from(&record::write_manifest(&key, &entries));
        match proof::hit::<UnitManifest>(self.snap, key, &bytes) {
            Ok(hit) => this.proof = Some(hit.holds()),
            Err(why) => return self.fail(Failure::Record { tag: UnitManifest::TAG, key: key.id(), why }),
        }
        this.manifest_bytes = Some(bytes);
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
            self.ready.push_back(Work::Reads);
        }
    }
}
