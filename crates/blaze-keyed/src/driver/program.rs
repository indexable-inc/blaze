//! The driver's program phase (crate::program, command.md "Code"): once every unit is
//! complete, each program's plans are found by what they read or made, its code by its plan's
//! content, and one link step is given the rows that moved.
//!
//! Per program, in order:
//! - **Kept**: its kept manifest (`ProgramPlan::last`) is read once. Every fact a kept plan read
//!   is answered now: a crate's fact over every unit's tables (one [`Effect::Answer`]), a body
//!   ([`FactKind::Mir`]) by the driver itself: a body of a unit settled by its mark, or kept by
//!   its unit, stands; a body this request settled is its record's result; a body of a unit not
//!   asked whose text moved is demanded (`Driver::demand`, the made-fact mechanism, with
//!   [`Waiter::Plan`]).
//! - **Examined**: the kept plans posted under a moved fact try their kept shape, then their
//!   key's shape trie. One that misses everywhere is moved, and waits for the walk to ask it
//!   (whether it is still reached is the walk's to say); it runs with its kept ask unless the
//!   walk gave another.
//! - **Bodies a plan reads**: its own item's, and each other one a run named by
//!   [`TaskFailed::NoBody`], found as a plan's read of that body is answered (`mir_now`): the plan
//!   runs again given it, at most [`super::BODIES_MOST`] bodies a plan. Its code task is given
//!   the same bodies and the plan's universe (`Front::code`): those the plan was given in this
//!   request, or, for a plan found by address, its own and each other its code names by
//!   [`TaskFailed::NoBody`] that the plan record read (any other is refused by name: code is a
//!   function of its plan and the bodies that plan read, so a code record keeps no reads).
//! - **Walked**: the walker is begun with the plans whose result moved, and its asks are
//!   answered: a plan (named by the walker) by its kept address when nothing it read moved (one
//!   read, for its payload), else found or made; code by the key the walker computed, the kept
//!   row's, read, or made from the plan and its callees' plans (filed, named by address).
//! - **Spans**: every span table the final rows name ([`crate::kind::SpanTable`]: a walked
//!   row's from its code ask, a kept row's from the kept manifest) is found by the files it
//!   read, each a [`FactKind::SpanFile`] fact the driver answers from its units' key rows with
//!   no host and no record: a kept table whose reads are unmoved stands at its kept address,
//!   unread; one whose reads moved is read at the address its reads make now; one no kept
//!   manifest names tries its key's shape root (one level: its reads are files, never a value
//!   that picks another shape). A miss runs its owner's span task (`Front::spans`), once per
//!   owner a request, which makes every table of the owner under one read list; each read must
//!   be the driver's own answer now, and each table a row names is filed at its address. So a
//!   comment added above an item costs the span tasks of the owners in its file, and nothing of
//!   any plan or code: a plan reads no text.
//! - **Linked**: the walk's rows against the kept rows give the changes, and the tables against
//!   the kept ones (each kept table's digest) the tables that came or moved; neither, and the
//!   kept manifest stands (filed again, with no link step, when a plan's or a table's address
//!   moved); else one link step, given the changes, the plans of the rows that came or moved
//!   and of their callees, and the tables that came or moved (a step of tables alone patches
//!   their `Location` records), and the kept manifest is filed again with the linker's state.
//!
//! The invariant (the simulation's oracle): every plan and code record a request settles, and
//! the rows of its kept manifest, are those a from-scratch build makes now; the link step is
//! given exactly the rows that differ from the kept ones. A program whose task or walk fails
//! fails alone: every other program goes on, and the request ends with every failure.
//!
//! **Durability** (the order a request makes things durable; a put is durable once answered,
//! and a kill drops every put not answered). Each step names only what is durable already:
//! 1. Records: bodies (checked or MIR alone), plan records and their shape nodes, code records,
//!    span tables and their shape nodes.
//!    Each is found by a key that covers its inputs, or at the address its reads make, so one
//!    put before a kill is true whenever a later request finds it.
//! 2. Unit manifests and marks, as the unit phase files them: before any kept program manifest.
//! 3. The program's store file (its image), named by the key of the kept manifest about to be
//!    put: O(1) to name, never a hash of the image ([`Effect::Image`]). It is a clone of the
//!    kept manifest's image (reflink, `clonefile`; a copy where the filesystem has neither),
//!    with the link step's patch applied (`Front::link` returns it and writes nothing), so a
//!    link writes bytes in proportion to the change and the old image stays whole until nothing
//!    names it. A manifest filed again with no link step clones the image unpatched. A kill
//!    after it leaves an orphan image that nothing names; the store's collection takes it.
//! 4. The kept program manifest naming it, put once every plan and code record its rows name
//!    settled (each settles on its put's answer) and its image is durable. It names the unit
//!    manifests its body reads were answered against (`KeptParts::manifests`).
//! 5. The certificate, naming each kept program manifest filed.
//! 6. The place, last: each program's path, the one the user runs, made to name the image of
//!    the kept manifest the answer certifies ([`Effect::Place`]: an atomic rename, only when it
//!    names another). Every answer places, a certified one included, so a path a request killed
//!    after 4 left unplaced, a deleted file or a user's overwrite is placed again from the store
//!    with no link step and no task: one look at the path per program on a no-op, its floor.
//!    The path never names an image whose kept manifest is not durable: placed before 4, a kill
//!    between would leave it naming what no answer certified.
//!
//! A kill between 2 and 4 leaves units ahead of the kept manifest the certificate names: a unit
//! settled by its mark next time says nothing moved since *its* last manifest, not since the
//! kept rows' one. So a body read by kept rows stands only when its unit's manifest now is one
//! the kept manifest names (`answered_against`); else its result is read from its unit's records
//! and compared. A kept manifest is filed only when a row moved, never because unit manifests
//! moved alone: an edit no plan reads writes nothing of the program, and a request repeated
//! files nothing at all (the simulation's oracle holds both). A row's plan address is part of
//! the row: a kept row is read at its address with no lookup by key, so an edit that moves a
//! plan's reads and not its result costs one kept-manifest write, and the next request finds the
//! plan in one read without examining it again.
//! The cost of that choice, past the floor: a unit whose manifest moved since the kept manifest
//! was filed costs its manifest root and the chunks of the bodies the rows read, once per
//! program request that is not certified, until a row moves and the kept manifest is filed
//! again naming it (reads of a few kilobytes against an `O(rows)` write).
//!
//! A plan reads facts owned by crates only (surface and made rows, and bodies): one that read
//! what its asker sees, or a late read, could not be answered from the program's one
//! dictionary, and is refused by name. Plans are not held for a wave: every table they read is
//! final when the phase starts (every unit is complete), and a body they read is final once its
//! demand settled, which they wait for.
//!
//! Floors: as crate::program's; span tables cost one answer per file a kept table read (a
//! binary search in its unit's key rows) and nothing more while unmoved. Past them: a span task
//! makes every table of its owner though a row names only some; a file edited anywhere reruns
//! the span task of every owner whose tables read it (the read is the whole file's image key,
//! not the bases above the owner). Past them too: every instance waiting for a body is retried each
//! time a unit manifest or chunk is read for a body's address (`O(instances)` per read, only
//! when a plan of a body its unit kept must be probed or run); a body its unit kept costs its
//! unit's manifest root and its file's chunk, once a request.

use alloc::vec::Vec;
use core::alloc::Allocator;

use blaze_format::Aligned;
use blaze_ids::ContentId;

use super::{
    Bodies, BodyAt, Demand, Done, Driver, Effect, Failure, Input, MakerRows, NO_UNIT, Phase, SlotState, StoreFault, TableRef, Task, TaskFailed, Waiter, Work,
    ZERO, copy_in, first_difference, malformed,
};
use crate::{
    Plant,
    exec::Executor,
    kind::{Body, CodeRecord, Key, Kind, PlanRecord, ProgramInputs, ProgramManifest, Shape, SpanTable, Tag, UnitChunk, UnitManifest},
    memo::Memo,
    planted,
    pool::Ticket,
    program::{Kept, PlanAt, Planned, ProgramPlan, Row, RowChange, SpanAt, SpanRecordAt, WalkAsk, Walker, changes},
    record::{self, Fact, FactKind, KeptParts, KeptRow, KeptSpan, Owner, RecordError, Route, StableId, Traced},
    store::Put,
};

/// The instance index of a program's own dictionary rows, as a [`Waiter::Plan`] names them.
const ROWS: u32 = u32::MAX;

/// A walker's code ask, copied out of its borrow (its callees and statics apart).
#[derive(Clone, Copy, Debug)]
struct CodeAsk {
    ask: u32,
    instance: Key<PlanRecord>,
    key: Key<CodeRecord>,
    tier: u32,
}

/// One step of the phase: the work a ticket is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Step {
    GetKept(u32),
    Rows(u32),
    Probe(u32, u32),
    Address(u32, u32),
    Node(u32, u32),
    Run(u32, u32),
    PutNode(u32, u32),
    PutPlan(u32, u32),
    GetCode(u32, u32),
    RunCode(u32, u32),
    PutCode(u32, u32),
    /// Unit `u`'s manifest root, for the address of a body it kept.
    BodyRoot(u32),
    /// Unit `u`'s chunk of file `f`, for the same.
    BodyChunk(u32, u32),
    /// Span table `s`'s shape root, for one no kept manifest names.
    SpanNode(u32, u32),
    /// Span table `s`'s record at the address its reads make now.
    SpanGet(u32, u32),
    /// Span owner `o`'s task.
    SpanRun(u32, u32),
    SpanPutNode(u32, u32),
    SpanPut(u32, u32),
    Link(u32),
    /// The program's image named by the kept manifest about to be put, made before it is put.
    Image(u32),
    PutKept(u32),
    /// The program's path made to name the image the answer certifies: after the certificate.
    Place(u32),
}

impl Step {
    /// The program the step is of; none for a read of a unit's records.
    fn program(self) -> Option<u32> {
        match self {
            Step::BodyRoot(_) | Step::BodyChunk(..) => None,
            Step::GetKept(p)
            | Step::Rows(p)
            | Step::Probe(p, _)
            | Step::Address(p, _)
            | Step::Node(p, _)
            | Step::Run(p, _)
            | Step::PutNode(p, _)
            | Step::PutPlan(p, _)
            | Step::GetCode(p, _)
            | Step::RunCode(p, _)
            | Step::PutCode(p, _)
            | Step::SpanNode(p, _)
            | Step::SpanGet(p, _)
            | Step::SpanRun(p, _)
            | Step::SpanPutNode(p, _)
            | Step::SpanPut(p, _)
            | Step::Link(p)
            | Step::Image(p)
            | Step::PutKept(p)
            | Step::Place(p) => Some(p),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PState {
    Waiting,
    Kept,
    Rows,
    Examining,
    Walking,
    /// The walk ended: the span tables its rows name are found or made.
    Spans,
    Linking,
    Filing,
    Done,
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum IState {
    Idle,
    /// Waiting for a body: its result (to answer a shape) or its address (to run).
    Body,
    Asking,
    Getting,
    Walking,
    Running,
    Putting,
    /// An examined plan every candidate missed: moved, to run once the walk asks it.
    Missed,
    Settled,
    Failed,
}

/// Where an instance's lookup is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Probing {
    /// Its kept address, nothing it read having moved: a miss is the store's fault.
    Kept,
    /// Its kept shape.
    Old,
    /// A node of its key's shape trie.
    Node,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum After {
    Probe,
    Run,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CState {
    None,
    /// Not found: waiting for a body its plan read, to run with it given.
    Body,
    Getting,
    Running,
    Putting,
    Done,
}

/// Where a span table's lookup is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SState {
    /// Its shape root being read: no kept manifest names it.
    Node,
    /// Its record being read at the address its reads make now.
    Getting,
    /// Waiting for its owner's span task.
    Owner,
    PuttingNode,
    Putting,
    Settled,
}

/// One span table the program's rows name.
struct Span<A: Allocator> {
    anchor: ContentId,
    key: Key<SpanTable>,
    owner: StableId,
    /// The unit of its owner.
    unit: u32,
    /// The kept manifest's span of its anchor, under the same key and owner.
    kept: Option<KeptSpan>,
    state: SState,
    /// The files it read, with the digests now, sorted by id.
    reads: Vec<Fact, A>,
    node: Key<Shape>,
    address: Key<SpanTable>,
    /// Its record's result digest: what the link step sees of it.
    digest: ContentId,
    /// Its record made, until put; its shape node, until put; under audit the record found.
    bytes: Option<Aligned>,
    node_bytes: Option<Aligned>,
    audit: Option<Aligned>,
}

/// One owner whose span task runs in this request: once, for all its tables.
struct SpanOwner<A: Allocator> {
    owner: StableId,
    unit: u32,
    ran: bool,
    reads: Vec<Fact, A>,
    /// Its tables by anchor, sorted, each once.
    tables: Vec<(ContentId, Aligned), A>,
}

/// A unit record read for bodies' addresses.
enum Holding {
    Asked,
    Held(Aligned),
}

struct Inst<A: Allocator> {
    /// Its plan's name.
    key: Key<PlanRecord>,
    /// The unit of its body's item; [`NO_UNIT`] for an instance with no body, or a kept row of
    /// an item no unit of the request owns.
    unit: u32,
    /// The item whose body its plan reads, by its fact subject; `None`: no body.
    item: Option<StableId>,
    /// The front end's ask its plan task is given.
    ask: Vec<u8, A>,
    /// Asked by the walk: its `ask` is the walk's.
    walked: bool,
    /// Its row of the kept manifest.
    kept: Option<u32>,
    examined: bool,
    state: IState,
    after: After,
    probing: Probing,
    old: Vec<ContentId, A>,
    probe: Vec<ContentId, A>,
    host_asks: Vec<ContentId, A>,
    /// The probe's answers, in probe order: bodies filled by the driver, the rest by the host.
    now: Vec<ContentId, A>,
    asked: ContentId,
    node: Option<Key<Shape>>,
    depth: u32,
    /// The bodies of other items its plan read and asked for ([`TaskFailed::NoBody`]), in the
    /// order asked: each given to its next run beside its own.
    also: Vec<StableId, A>,
    /// The body records its plan task is given, sorted by item: made by [`Driver::plan_run`].
    given: Vec<BodyAt, A>,
    address: ContentId,
    result: ContentId,
    reads: Vec<Fact, A>,
    bytes: Option<Aligned>,
    node_bytes: Option<Aligned>,
    audit: Option<Aligned>,
    /// What its code ask named, each sorted, each once: the instances it calls and the statics
    /// it reads, whose plans its code task reads; its row's targets, apart.
    callees: Vec<Key<PlanRecord>, A>,
    statics: Vec<Key<PlanRecord>, A>,
    /// Its code ask's tier: its kept row's.
    tier: u32,
    /// The span tables its code ask named, sorted by anchor, each once.
    spans: Vec<SpanAt, A>,
    /// Where each plan of `callees` and `statics` is filed, sorted by instance, each once: the
    /// code task's reads.
    callee_plans: Vec<PlanAt, A>,
    /// The walker's plan asks this instance answers, not yet answered.
    asks: Vec<u32, A>,
    code_ask: Option<u32>,
    code: Key<CodeRecord>,
    code_state: CState,
    code_bytes: Option<Aligned>,
}

struct Prog<A: Allocator> {
    plan: ProgramPlan,
    state: PState,
    /// The kept manifest's payload, owned and 16-aligned, checked once read.
    kept: Option<Aligned>,
    /// The dictionary ordinals answered by the host, and their ids.
    rows_ord: Vec<u32, A>,
    rows_ids: Vec<ContentId, A>,
    rows_out: bool,
    changed: Vec<bool, A>,
    insts: Vec<Inst<A>, A>,
    /// Instance key to index, sorted.
    index: Vec<(ContentId, u32), A>,
    ended: bool,
    rows: Vec<Row, A>,
    gone: Vec<Key<PlanRecord>, A>,
    changes: Vec<RowChange, A>,
    /// The plans the link step reads, sorted by instance: each changed row's that came or
    /// moved, and each of its callees'.
    link_plans: Vec<PlanAt, A>,
    /// The span tables the final rows name, sorted by anchor, each once.
    spans: Vec<Span<A>, A>,
    /// The owners whose span task ran in this request, in the order asked.
    owners: Vec<SpanOwner<A>, A>,
    /// The span tables the link step is given: those that came or moved, sorted by anchor.
    link_spans: Vec<SpanRecordAt, A>,
    /// The kept manifest this request names: the one read, or the one filed, or the one the
    /// certificate found names. The program's image is named by it.
    key: Option<Key<ProgramManifest>>,
    out: Option<Aligned>,
    /// A link step ran in this request: its patch, held by the host, makes the new image.
    patched: bool,
}

pub(super) struct Progs<A: Allocator> {
    list: Vec<Prog<A>, A>,
    /// Every unit's own table, sorted by owner: what a plan reads.
    every: Vec<TableRef, A>,
    /// Owner to unit, sorted.
    owners: Vec<(Owner, u32), A>,
    roots: Vec<(u32, Holding), A>,
    chunks: Vec<((u32, u32), Holding), A>,
}

impl<A: Allocator + Copy> Progs<A> {
    pub(super) fn new(programs: &[ProgramPlan], alloc: A) -> Progs<A> {
        let mut list = Vec::with_capacity_in(programs.len(), alloc);
        for plan in programs {
            list.push(Prog {
                plan: *plan,
                state: PState::Waiting,
                kept: None,
                rows_ord: Vec::new_in(alloc),
                rows_ids: Vec::new_in(alloc),
                rows_out: false,
                changed: Vec::new_in(alloc),
                insts: Vec::new_in(alloc),
                index: Vec::new_in(alloc),
                ended: false,
                rows: Vec::new_in(alloc),
                gone: Vec::new_in(alloc),
                changes: Vec::new_in(alloc),
                link_plans: Vec::new_in(alloc),
                spans: Vec::new_in(alloc),
                owners: Vec::new_in(alloc),
                link_spans: Vec::new_in(alloc),
                key: None,
                out: None,
                patched: false,
            });
        }
        Progs {
            list,
            every: Vec::new_in(alloc),
            owners: Vec::new_in(alloc),
            roots: Vec::new_in(alloc),
            chunks: Vec::new_in(alloc),
        }
    }
}

impl<A: Allocator> Progs<A> {
    pub(super) fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub(super) fn len(&self) -> u32 {
        self.list.len() as u32
    }

    pub(super) fn kept_key(&self, p: u32) -> Option<Key<ProgramManifest>> {
        self.list.get(p as usize).and_then(|prog| prog.key)
    }

    /// Program `p`'s kept manifest as a certificate found names it: what the answer places.
    pub(super) fn certified(&mut self, p: u32, key: Option<Key<ProgramManifest>>) {
        if let Some(prog) = self.list.get_mut(p as usize) {
            prog.key = key;
        }
    }
}

/// A kept program's parts, from the payload a [`Prog`] holds (checked when it was read).
fn kept_parts(kept: &Option<Aligned>) -> Option<KeptParts<'_>> {
    kept.as_deref().and_then(|bytes| record::read_kept(bytes).ok())
}

fn kept_of(kept: &Option<Aligned>) -> Kept<'_> {
    kept_parts(kept).map_or(Kept { rows: &[], targets: &[], asks: &[] }, |parts| Kept::of(&parts))
}

/// A body's answer now, for a plan's read of it.
enum MirNow {
    /// Nothing of its unit moved: the digest read stands (only for the kept dictionary).
    Stands,
    Known { result: ContentId, address: Option<Key<Body>> },
    /// Asked (a demand, a unit record's read); the waiter is resumed.
    Pending,
}

/// A body fact's text id, by its unit's maker rows.
enum Text {
    Pending,
    /// No maker row: the subject has no body.
    None,
    Is(StableId),
}

impl<'s, 'm, M: Memo, X: Executor, A: Allocator + Copy, W: Walker> Driver<'s, 'm, M, X, A, W> {
    /// A run of the phase starts: counted in [`super::Ran`].
    pub(super) fn counted(&mut self, step: Step) {
        match step {
            Step::Run(..) => self.ran.plans += 1,
            Step::RunCode(..) => self.ran.codes += 1,
            Step::SpanRun(..) => self.ran.spans += 1,
            _ => {}
        }
    }

    /// Every unit is complete: each program starts from its kept manifest, or walks from none.
    pub(super) fn programs_start(&mut self) {
        let alloc = self.alloc;
        let mut every = Vec::with_capacity_in(self.units.len(), alloc);
        let mut owners = Vec::with_capacity_in(self.units.len(), alloc);
        for (u, unit) in self.units.iter().enumerate() {
            if let Some(own) = unit.closure.iter().find(|table| table.owner == unit.owner) {
                every.push(*own);
            }
            owners.push((unit.owner, u as u32));
        }
        every.sort_by_key(|table| table.owner);
        owners.sort_unstable();
        self.progs.every = every;
        self.progs.owners = owners;
        for p in 0..self.progs.len() {
            let prog = &mut self.progs.list[p as usize];
            match prog.plan.last {
                // Under audit every plan is made again from scratch, so it is compared whole.
                Some(_) if !M::AUDIT => {
                    prog.state = PState::Kept;
                    self.ready.push_back(Work::Program(Step::GetKept(p)));
                }
                _ => self.walk_begin(p),
            }
            if self.done() {
                return;
            }
        }
        self.program_progress();
    }

    pub(super) fn program_get(&self, step: Step) -> Option<(u32, Tag, ContentId)> {
        let progs = &self.progs;
        Some(match step {
            Step::GetKept(p) => (NO_UNIT, ProgramManifest::TAG, progs.list[p as usize].plan.last?.id()),
            Step::Address(p, i) => (NO_UNIT, PlanRecord::TAG, progs.list[p as usize].insts[i as usize].asked),
            Step::Node(p, i) => (NO_UNIT, Shape::TAG, progs.list[p as usize].insts[i as usize].node?.id()),
            Step::GetCode(p, i) => (NO_UNIT, CodeRecord::TAG, progs.list[p as usize].insts[i as usize].code.id()),
            Step::SpanNode(p, s) => (NO_UNIT, Shape::TAG, progs.list[p as usize].spans[s as usize].node.id()),
            Step::SpanGet(p, s) => (NO_UNIT, SpanTable::TAG, progs.list[p as usize].spans[s as usize].address.id()),
            Step::BodyRoot(u) => (u, UnitManifest::TAG, self.units[u as usize].manifest?.id()),
            Step::BodyChunk(u, f) => (u, UnitChunk::TAG, self.chunk_key(u, f)?.id()),
            _ => return None,
        })
    }

    /// The chunk of file `f` unit `u`'s manifest names, once its root is held.
    fn chunk_key(&self, u: u32, f: u32) -> Option<Key<UnitChunk>> {
        let at = self.progs.roots.binary_search_by_key(&u, |(unit, _)| *unit).ok()?;
        let Holding::Held(bytes) = &self.progs.roots[at].1 else { return None };
        let root = record::open::<UnitManifest>(bytes, &self.units[u as usize].manifest?).ok()?;
        let files = root.files.follow(bytes).ok()?;
        Some(Key::from_id(files.get(f as usize)?.chunk))
    }

    pub(super) fn program_effect(&self, step: Step, ticket: Ticket) -> Effect<'_> {
        let progs = &self.progs;
        match step {
            Step::Rows(p) => {
                Effect::Answer { ticket, unit: NO_UNIT, asks: &progs.list[p as usize].rows_ids, universe: &progs.every, made: &self.demanded }
            }
            Step::Probe(p, i) => {
                let inst = &progs.list[p as usize].insts[i as usize];
                Effect::Answer { ticket, unit: NO_UNIT, asks: &inst.host_asks, universe: &progs.every, made: &self.demanded }
            }
            Step::Run(p, i) => {
                let inst = &progs.list[p as usize].insts[i as usize];
                let task = Task::Plan { program: p, key: inst.key };
                let (universe, made) = (&progs.every[..], &self.demanded[..]);
                Effect::Run { ticket, task, remote: false, universe, made, bytes: &inst.ask, plans: &[], bodies: &inst.given }
            }
            Step::RunCode(p, i) => {
                // The plan's universe and the bodies its plan read: the code task opens the
                // snapshot its plan task opened (`Front::code`).
                let inst = &progs.list[p as usize].insts[i as usize];
                let task = Task::Code { program: p, key: inst.code };
                let bytes = inst.bytes.as_deref().unwrap_or(&[]);
                let (universe, made) = (&progs.every[..], &self.demanded[..]);
                Effect::Run { ticket, task, remote: false, universe, made, bytes, plans: &inst.callee_plans, bodies: &inst.given }
            }
            Step::PutNode(p, i) => {
                let prog = &progs.list[p as usize];
                let inst = &prog.insts[i as usize];
                let key = inst.node.map_or(ZERO, |node| node.id());
                Effect::Put { ticket, tag: Shape::TAG, key, namespace: prog.plan.namespace, bytes: inst.node_bytes.as_deref().unwrap_or(&[]) }
            }
            Step::PutPlan(p, i) => {
                let prog = &progs.list[p as usize];
                let inst = &prog.insts[i as usize];
                let bytes = inst.bytes.as_deref().unwrap_or(&[]);
                Effect::Put { ticket, tag: PlanRecord::TAG, key: inst.address, namespace: prog.plan.namespace, bytes }
            }
            Step::PutCode(p, i) => {
                let prog = &progs.list[p as usize];
                let inst = &prog.insts[i as usize];
                let bytes = inst.code_bytes.as_deref().unwrap_or(&[]);
                Effect::Put { ticket, tag: CodeRecord::TAG, key: inst.code.id(), namespace: prog.plan.namespace, bytes }
            }
            Step::SpanRun(p, o) => {
                let owner = &progs.list[p as usize].owners[o as usize];
                let task = Task::Spans { program: p, unit: owner.unit, owner: owner.owner };
                let (universe, made) = (&progs.every[..], &self.demanded[..]);
                Effect::Run { ticket, task, remote: false, universe, made, bytes: &[], plans: &[], bodies: &[] }
            }
            Step::SpanPutNode(p, s) => {
                let prog = &progs.list[p as usize];
                let span = &prog.spans[s as usize];
                let bytes = span.node_bytes.as_deref().unwrap_or(&[]);
                Effect::Put { ticket, tag: Shape::TAG, key: span.node.id(), namespace: prog.plan.namespace, bytes }
            }
            Step::SpanPut(p, s) => {
                let prog = &progs.list[p as usize];
                let span = &prog.spans[s as usize];
                let bytes = span.bytes.as_deref().unwrap_or(&[]);
                Effect::Put { ticket, tag: SpanTable::TAG, key: span.address.id(), namespace: prog.plan.namespace, bytes }
            }
            Step::Link(p) => {
                let prog = &progs.list[p as usize];
                let payload = kept_parts(&prog.kept).map_or(&[][..], |parts| parts.linker);
                Effect::Link { ticket, program: p, changes: &prog.changes, plans: &prog.link_plans, spans: &prog.link_spans, payload }
            }
            Step::Image(p) => {
                // From the image of the kept manifest read (none for a first build, or under
                // audit, which links from nothing), to the one of the manifest about to be put.
                let prog = &progs.list[p as usize];
                let from = prog.kept.as_ref().and(prog.plan.last);
                let to = prog.key.unwrap_or(Key::from_id(ZERO));
                Effect::Image { ticket, program: p, from, to, patched: prog.patched }
            }
            Step::Place(p) => Effect::Place { ticket, program: p, image: progs.list[p as usize].key.unwrap_or(Key::from_id(ZERO)) },
            Step::PutKept(p) => {
                let prog = &progs.list[p as usize];
                let key = prog.key.map_or(ZERO, |key| key.id());
                Effect::Put { ticket, tag: ProgramManifest::TAG, key, namespace: prog.plan.namespace, bytes: prog.out.as_deref().unwrap_or(&[]) }
            }
            // Every get was answered by `get_of`; none is asked here.
            Step::GetKept(_)
            | Step::Address(..)
            | Step::Node(..)
            | Step::GetCode(..)
            | Step::SpanNode(..)
            | Step::SpanGet(..)
            | Step::BodyRoot(_)
            | Step::BodyChunk(..) => {
                Effect::Get { ticket, unit: NO_UNIT, tag: ProgramManifest::TAG, key: ZERO, layout: ProgramManifest::LAYOUT }
            }
        }
    }

    pub(super) fn program_give(&mut self, step: Step, input: Input<'_>) {
        if let Input::Ran { .. } = input {
            self.running -= 1;
        }
        // A program that failed takes no more answers: its work out is dropped as it comes.
        if step.program().is_some_and(|p| self.progs.list[p as usize].state == PState::Failed) {
            return self.program_progress();
        }
        match (step, input) {
            (_, Input::Got { record, .. }) => self.program_got(step, record),
            (Step::Rows(p), Input::Answers { now, .. }) => self.kept_rows_answered(p, now),
            (Step::Probe(p, i), Input::Answers { now, .. }) => self.plan_probed(p, i, now),
            (Step::Run(p, i), Input::Ran { outcome, .. }) => match outcome {
                Ok(Done::Wrote { record }) => self.plan_ran(p, i, record),
                Ok(Done::Keyed { .. } | Done::Spans { .. }) => self.fail(Failure::Misused("a plan task that keyed")),
                Err(TaskFailed::NoBody { item }) => self.plan_wants_body(p, i, item),
                Err(failed) => {
                    let unit = self.progs.list[p as usize].insts[i as usize].unit;
                    self.plan_failed(p, i, Failure::Task { tag: PlanRecord::TAG, unit, failed });
                }
            },
            (Step::RunCode(p, i), Input::Ran { outcome, .. }) => match outcome {
                Ok(Done::Wrote { record }) => self.code_ran(p, i, record),
                Ok(Done::Keyed { .. } | Done::Spans { .. }) => self.fail(Failure::Misused("a code task that keyed")),
                Err(TaskFailed::NoBody { item }) => self.code_wants_body(p, i, item),
                Err(failed) => {
                    let unit = self.progs.list[p as usize].insts[i as usize].unit;
                    self.plan_failed(p, i, Failure::Task { tag: CodeRecord::TAG, unit, failed });
                }
            },
            (Step::PutNode(p, i) | Step::PutPlan(p, i), Input::Put { put, .. }) => self.plan_put(p, i, step, put),
            (Step::PutCode(p, i), Input::Put { put, .. }) => match put {
                Err(fault) => self.fail(Failure::Store(fault)),
                Ok(Put::Other) => {
                    let key = self.progs.list[p as usize].insts[i as usize].code.id();
                    self.fail(Failure::TwoValues { tag: CodeRecord::TAG, key });
                }
                Ok(_) => self.code_settled(p, i),
            },
            (Step::SpanRun(p, o), Input::Ran { outcome, .. }) => match outcome {
                Ok(Done::Spans { reads, tables }) => self.span_owner_ran(p, o, reads, tables),
                Ok(Done::Keyed { .. } | Done::Wrote { .. }) => self.fail(Failure::Misused("a span task that keyed or wrote one record")),
                Err(failed) => {
                    let unit = self.progs.list[p as usize].owners[o as usize].unit;
                    self.program_failed(p, Failure::Task { tag: SpanTable::TAG, unit, failed });
                }
            },
            (Step::SpanPutNode(p, s), Input::Put { put, .. }) => match put {
                Err(fault) => self.fail(Failure::Store(fault)),
                Ok(put) => {
                    if put == Put::Other {
                        self.ran.checks.shape_races += 1;
                    }
                    let span = &mut self.progs.list[p as usize].spans[s as usize];
                    span.node_bytes = None;
                    span.state = SState::Putting;
                    self.ready.push_back(Work::Program(Step::SpanPut(p, s)));
                }
            },
            (Step::SpanPut(p, s), Input::Put { put, .. }) => match put {
                Err(fault) => self.fail(Failure::Store(fault)),
                Ok(Put::Other) => {
                    let key = self.progs.list[p as usize].spans[s as usize].address.id();
                    self.fail(Failure::TwoValues { tag: SpanTable::TAG, key });
                }
                Ok(_) => {
                    let span = &mut self.progs.list[p as usize].spans[s as usize];
                    span.bytes = None;
                    span.state = SState::Settled;
                }
            },
            (Step::Link(p), Input::Linked { outcome, .. }) => match outcome {
                Ok(linker) => self.linked(p, linker, true),
                Err(failed) => self.program_failed(p, Failure::Task { tag: ProgramManifest::TAG, unit: NO_UNIT, failed }),
            },
            // The image is durable: the kept manifest naming it may be put. The plant places the
            // path now, before that manifest is durable.
            (Step::Image(p), Input::Imaged { outcome, .. }) => match outcome {
                Ok(()) if planted(Plant::PlacedBeforeKept) && self.placing.is_none() => {
                    self.ready.push_back(Work::Program(Step::Place(p)));
                }
                Ok(()) => self.ready.push_back(Work::Program(Step::PutKept(p))),
                Err(failed) => self.program_failed(p, Failure::Task { tag: ProgramManifest::TAG, unit: NO_UNIT, failed }),
            },
            (Step::Place(p), Input::Placed { outcome, .. }) => match outcome {
                // The plant's early place: the kept manifest is put after it.
                Ok(()) if self.placing.is_none() => self.ready.push_back(Work::Program(Step::PutKept(p))),
                Ok(()) => self.placed(),
                Err(failed) => self.fail(Failure::Task { tag: ProgramManifest::TAG, unit: NO_UNIT, failed }),
            },
            (Step::PutKept(p), Input::Put { put, .. }) => match put {
                Err(fault) => self.fail(Failure::Store(fault)),
                Ok(Put::Other) => {
                    let key = self.progs.list[p as usize].key.map_or(ZERO, |key| key.id());
                    self.fail(Failure::TwoValues { tag: ProgramManifest::TAG, key });
                }
                Ok(_) => {
                    let prog = &mut self.progs.list[p as usize];
                    prog.state = PState::Done;
                    prog.out = None;
                }
            },
            _ => self.fail(Failure::Misused("an answer of another form than its program effect")),
        }
        self.program_progress();
    }

    /// A read of the phase answered, by the store or the memo (`Driver::begin`).
    pub(super) fn program_got(&mut self, step: Step, record: Option<&[u8]>) {
        if step.program().is_some_and(|p| self.progs.list[p as usize].state == PState::Failed) {
            return;
        }
        match step {
            Step::GetKept(p) => self.got_kept(p, record),
            Step::Address(p, i) => self.plan_got_address(p, i, record),
            Step::Node(p, i) => self.plan_got_node(p, i, record),
            Step::SpanNode(p, s) => self.span_got_node(p, s, record),
            Step::SpanGet(p, s) => self.span_got(p, s, record),
            Step::GetCode(p, i) => match record {
                None => self.code_run(p, i),
                Some(bytes) => {
                    let key = self.progs.list[p as usize].insts[i as usize].code;
                    if let Err(why) = record::open::<CodeRecord>(bytes, &key) {
                        return self.fail(Failure::Record { tag: CodeRecord::TAG, key: key.id(), why });
                    }
                    self.code_settled(p, i);
                }
            },
            Step::BodyRoot(u) | Step::BodyChunk(u, _) => {
                let Some(bytes) = record else {
                    let what = "a unit manifest or chunk a complete unit names, read for a body's address: the store does not hold it";
                    return self.fail(Failure::Store(StoreFault { what }));
                };
                let held = Holding::Held(Aligned::copy_from(bytes));
                match step {
                    Step::BodyChunk(_, f) => {
                        if let Ok(at) = self.progs.chunks.binary_search_by_key(&(u, f), |(at, _)| *at) {
                            self.progs.chunks[at].1 = held;
                        }
                    }
                    _ => {
                        if let Ok(at) = self.progs.roots.binary_search_by_key(&u, |(unit, _)| *unit) {
                            self.progs.roots[at].1 = held;
                        }
                    }
                }
                self.bodies_moved();
            }
            _ => self.fail(Failure::Misused("a record given to a program step that reads none")),
        }
    }

    // ---- the kept manifest ----

    fn got_kept(&mut self, p: u32, record: Option<&[u8]>) {
        let Some(key) = self.progs.list[p as usize].plan.last else {
            return self.fail(Failure::Misused("a kept program read with none named"));
        };
        let Some(bytes) = record else {
            // The certificate's list named it and the store does not hold it: never walked as
            // if new (a fallback); the store's fault, by name.
            let what = "the kept manifest of a program the last certificate names: the store does not hold it";
            return self.fail(Failure::Store(StoreFault { what }));
        };
        let opened = record::open::<ProgramManifest>(bytes, &key).and_then(|root| root.payload.follow(bytes).map_err(RecordError::Format));
        let payload = match opened {
            Ok(payload) => Aligned::copy_from(payload),
            Err(why) => return self.fail(Failure::Record { tag: ProgramManifest::TAG, key: key.id(), why }),
        };
        if let Err(why) = record::read_kept(&payload) {
            return self.fail(Failure::Record { tag: ProgramManifest::TAG, key: key.id(), why });
        }
        let alloc = self.alloc;
        let prog = &mut self.progs.list[p as usize];
        prog.kept = Some(payload);
        prog.key = Some(key);
        prog.state = PState::Rows;
        let count = kept_parts(&prog.kept).map_or(0, |parts| parts.dictionary.len());
        prog.changed = Vec::with_capacity_in(count, alloc);
        prog.changed.resize(count, false);
        if let Some(parts) = kept_parts(&prog.kept) {
            for (ord, fact) in parts.dictionary.iter().enumerate() {
                // A body is answered by the driver (`rows_bodies`). The plant: a layout, or any
                // crate's fact, a kept plan read is never answered again.
                if record::kind_of(&fact.id) == Some(FactKind::Mir) || planted(Plant::PlanKeptAfterLayoutMoved) {
                    continue;
                }
                prog.rows_ord.push(ord as u32);
                prog.rows_ids.push(fact.id);
            }
        }
        if !prog.rows_ids.is_empty() {
            prog.rows_out = true;
            self.ready.push_back(Work::Program(Step::Rows(p)));
        }
        self.rows_bodies(p);
    }

    /// A program's kept dictionary rows answered now: each moved one is marked. Not one generic
    /// with the unit's `rows_answered`: a unit's answer marks its own slots' records, a
    /// program's marks posting lists over its kept rows, and the two share only the compare.
    fn kept_rows_answered(&mut self, p: u32, now: &[ContentId]) {
        let prog = &mut self.progs.list[p as usize];
        if now.len() != prog.rows_ids.len() {
            return self.fail(Failure::Misused("a program's rows answered with another number of facts than asked"));
        }
        let Some(parts) = kept_parts(&prog.kept) else {
            return self.fail(Failure::Misused("a program's rows answered with no kept manifest"));
        };
        for (k, &ord) in prog.rows_ord.iter().enumerate() {
            if parts.dictionary[ord as usize].digest != now[k] {
                prog.changed[ord as usize] = true;
            }
        }
        prog.rows_out = false;
        self.ran.checks.facts_compared += now.len() as u32;
        self.rows_bodies(p);
    }

    /// The kept dictionary's body rows, answered by the driver: a moved one is marked; one of a
    /// body demanded and not settled, or of a unit record not read, waits ([`Waiter::Plan`] with
    /// `ROWS`). With every row answered, the kept plans are examined.
    fn rows_bodies(&mut self, p: u32) {
        if self.progs.list[p as usize].state != PState::Rows {
            return;
        }
        let count = self.progs.list[p as usize].changed.len();
        let mut pending = false;
        for ord in 0..count {
            let Some(fact) = kept_parts(&self.progs.list[p as usize].kept).map(|parts| parts.dictionary[ord]) else {
                return;
            };
            if record::kind_of(&fact.id) != Some(FactKind::Mir) || self.progs.list[p as usize].changed[ord] {
                continue;
            }
            match self.mir_now(p, ROWS, &fact.id, true) {
                MirNow::Stands => {}
                MirNow::Known { result, .. } => {
                    if result != fact.digest {
                        self.progs.list[p as usize].changed[ord] = true;
                    }
                }
                MirNow::Pending => pending = true,
            }
            if self.done() {
                return;
            }
        }
        if pending || self.progs.list[p as usize].rows_out {
            return;
        }
        self.examine_kept_rows(p);
    }

    /// The kept rows posted under a moved fact are examined; the rest stand. Not the unit's
    /// `examine` (one slot's record against its read list): a kept row is an instance whose
    /// plan is looked up again at its address, which a unit slot has no notion of.
    fn examine_kept_rows(&mut self, p: u32) {
        let alloc = self.alloc;
        let prog = &mut self.progs.list[p as usize];
        prog.state = PState::Examining;
        let Some(parts) = kept_parts(&prog.kept) else { return };
        let mut rows = Vec::new_in(alloc);
        for (ord, &moved) in prog.changed.iter().enumerate() {
            if moved {
                rows.extend_from_slice(&parts.postings[parts.posting_at[ord] as usize..parts.posting_at[ord + 1] as usize]);
            }
        }
        rows.sort_unstable();
        rows.dedup();
        let mut fresh = Vec::with_capacity_in(rows.len(), alloc);
        for &r in rows.iter() {
            let row = parts.rows[r as usize];
            let mut old = Vec::with_capacity_in(row.shape_len as usize, alloc);
            let ords = &parts.shapes[row.shape_at as usize..(row.shape_at + row.shape_len) as usize];
            old.extend(ords.iter().map(|&ord| parts.dictionary[ord as usize].id));
            fresh.push((r, row, old));
        }
        for (r, row, old) in fresh {
            // A row of no body keeps `ZERO` as its item.
            let item = (row.item != ZERO).then_some(StableId(row.item));
            let unit = item.map_or(Some(NO_UNIT), |item| self.unit_of_item(&item));
            let i = self.new_inst(p, Key::from_id(row.instance), unit.unwrap_or(NO_UNIT), item, &[], Some(r));
            let inst = &mut self.progs.list[p as usize].insts[i as usize];
            inst.examined = true;
            if unit.is_none() {
                // Its item left the request: moved, and gone unless the walk asks it.
                inst.state = IState::Missed;
                continue;
            }
            inst.old = old;
            self.try_plan(p, i);
            if self.done() {
                return;
            }
        }
    }

    // ---- the walk ----

    fn walk_begin(&mut self, p: u32) {
        let alloc = self.alloc;
        let prog = &mut self.progs.list[p as usize];
        prog.state = PState::Walking;
        let mut moved = Vec::new_in(alloc);
        if let Some(parts) = kept_parts(&prog.kept) {
            for inst in prog.insts.iter().filter(|inst| inst.examined) {
                let stood = inst.state == IState::Settled && inst.kept.is_some_and(|r| parts.rows[r as usize].result == inst.result);
                if !stood {
                    moved.push(inst.key);
                }
            }
        }
        moved.sort_unstable_by_key(|key| key.id());
        // Every unit has settled before a program begins (`units_ended`): the manifests are
        // all filled.
        let begun = self.walker.begin(p, kept_of(&prog.kept), &moved, &self.manifests);
        if let Err(failed) = begun {
            self.program_failed(p, Failure::Task { tag: ProgramManifest::TAG, unit: NO_UNIT, failed });
        }
    }

    /// Answers what each walking program asks, ends each walk with nothing out, and ends the
    /// phase once every program is done or failed.
    pub(super) fn program_progress(&mut self) {
        if self.done() || !matches!(self.phase, Phase::Building) {
            return;
        }
        for p in 0..self.progs.len() {
            if self.progs.list[p as usize].state == PState::Examining && !self.insts_out(p) {
                self.walk_begin(p);
            }
            if self.progs.list[p as usize].state == PState::Walking {
                self.pump(p);
            }
            if self.progs.list[p as usize].state == PState::Spans && !self.spans_out(p) {
                self.spans_end(p);
            }
            if self.done() {
                return;
            }
        }
        if self.progs.list.iter().all(|prog| matches!(prog.state, PState::Done | PState::Failed)) {
            if !self.failures.is_empty() {
                return self.end_failed();
            }
            self.phase = Phase::Filing;
            self.ready.push_back(Work::Reads);
        }
    }

    fn insts_out(&self, p: u32) -> bool {
        self.progs.list[p as usize].insts.iter().any(|inst| {
            matches!(inst.state, IState::Body | IState::Asking | IState::Getting | IState::Walking | IState::Running | IState::Putting)
                || matches!(inst.code_state, CState::Body | CState::Getting | CState::Running | CState::Putting)
        })
    }

    fn pump(&mut self, p: u32) {
        loop {
            // The ask borrows the walker: its slices are copied into the driver's allocator
            // before `self` is used again (`self.walker` and `self.alloc` are disjoint fields).
            let ask = match self.walker.ask(p) {
                None => break,
                Some(WalkAsk::Plan { ask, instance, bytes, body }) => {
                    Ok((ask, instance, bytes.map(|bytes| copy_in(bytes, self.alloc)), body))
                }
                Some(WalkAsk::Code { ask, instance, key, tier, callees, statics, spans }) => {
                    let code = CodeAsk { ask, instance, key, tier };
                    let alloc = self.alloc;
                    Err((code, copy_in(callees, alloc), copy_in(statics, alloc), copy_in(spans, alloc)))
                }
            };
            match ask {
                Ok((ask, key, bytes, body)) => self.ask_plan(p, ask, body, key, bytes),
                Err((code, callees, statics, spans)) => self.ask_code(p, code, callees, statics, spans),
            }
            if self.done() || self.progs.list[p as usize].state != PState::Walking {
                return;
            }
        }
        if !self.insts_out(p) && self.progs.list[p as usize].insts.iter().all(|inst| inst.asks.is_empty()) {
            self.walk_end(p);
        }
    }

    fn ask_plan(&mut self, p: u32, ask: u32, item: Option<StableId>, key: Key<PlanRecord>, bytes: Option<Vec<u8, A>>) {
        // No bytes: the kept row's ask (crate::program, `WalkAsk::Plan`), else a refusal.
        let bytes = match bytes {
            Some(bytes) => bytes,
            None => {
                let prog = &self.progs.list[p as usize];
                let kept = kept_of(&prog.kept);
                match kept.row(&key.id()) {
                    Some(row) => copy_in(kept.ask_of(row), self.alloc),
                    None => {
                        let failed = TaskFailed::Refused { what: "a plan asked with no ask bytes of an instance no kept row holds" };
                        return self.program_failed(p, Failure::Task { tag: PlanRecord::TAG, unit: NO_UNIT, failed });
                    }
                }
            }
        };
        let found = self.progs.list[p as usize].index.binary_search_by_key(&key.id(), |(id, _)| *id);
        let i = match found {
            Ok(at) => self.progs.list[p as usize].index[at].1,
            Err(_) => {
                let unit = match item {
                    None => NO_UNIT,
                    Some(item) => match self.unit_of_item(&item) {
                        Some(unit) => unit,
                        None => {
                            let failed = TaskFailed::Refused { what: "a plan asked of a body no unit of the request owns" };
                            return self.program_failed(p, Failure::Task { tag: PlanRecord::TAG, unit: NO_UNIT, failed });
                        }
                    },
                };
                let kept = kept_parts(&self.progs.list[p as usize].kept)
                    .and_then(|parts| parts.rows.binary_search_by(|row| row.instance.cmp(&key.id())).ok().map(|r| r as u32));
                self.new_inst(p, key, unit, item, &[], kept)
            }
        };
        let inst = &mut self.progs.list[p as usize].insts[i as usize];
        inst.asks.push(ask);
        if !inst.walked {
            inst.walked = true;
            // The item is the kept row's, if it was one: the name holds the ask, and the ask
            // the body's item, so one name has one item.
            inst.ask = bytes;
        }
        match (inst.state, inst.kept) {
            (IState::Settled, _) => self.answer_plan(p, i),
            (IState::Missed, _) => self.plan_run(p, i),
            // A kept row not examined: nothing it read moved, so its record is at its kept
            // address (one read, for its edges).
            (IState::Idle, Some(r)) => {
                let plan = kept_parts(&self.progs.list[p as usize].kept).map_or(ZERO, |parts| parts.rows[r as usize].plan);
                let inst = &mut self.progs.list[p as usize].insts[i as usize];
                inst.asked = plan;
                inst.probing = Probing::Kept;
                inst.state = IState::Getting;
                self.ready.push_back(Work::Program(Step::Address(p, i)));
            }
            (IState::Idle, None) => self.try_plan(p, i),
            // On its way: answered when it settles.
            _ => {}
        }
    }

    fn ask_code(&mut self, p: u32, code: CodeAsk, mut callees: Vec<Key<PlanRecord>, A>, mut statics: Vec<Key<PlanRecord>, A>, spans: Vec<SpanAt, A>) {
        let CodeAsk { ask, instance, key, tier } = code;
        if spans.windows(2).any(|pair| pair[0].anchor >= pair[1].anchor) {
            let failed = TaskFailed::Refused { what: "a code ask whose span tables are not sorted by anchor, each once" };
            return self.program_failed(p, Failure::Task { tag: CodeRecord::TAG, unit: NO_UNIT, failed });
        }
        let prog = &self.progs.list[p as usize];
        let found = prog.index.binary_search_by_key(&instance.id(), |(id, _)| *id).ok().map(|at| prog.index[at].1);
        let Some(i) = found.filter(|&i| prog.insts[i as usize].state == IState::Settled) else {
            return self.fail(Failure::Misused("code asked of an instance whose plan was not answered"));
        };
        let inst = &prog.insts[i as usize];
        let unit = inst.unit;
        if inst.code_ask.is_some() {
            let failed = TaskFailed::Refused { what: "code of one instance asked twice in one walk (one tier per instance per request)" };
            return self.program_failed(p, Failure::Task { tag: CodeRecord::TAG, unit, failed });
        }
        callees.sort_unstable_by_key(|key| key.id());
        callees.dedup();
        statics.sort_unstable_by_key(|key| key.id());
        statics.dedup();
        // The plans the code task reads: of both, sorted by instance, each once.
        let mut read = Vec::with_capacity_in(callees.len() + statics.len(), self.alloc);
        read.extend_from_slice(&callees);
        read.extend_from_slice(&statics);
        read.sort_unstable_by_key(|key| key.id());
        read.dedup();
        let Some(callee_plans) = self.plans_at(p, &read) else {
            let failed = TaskFailed::Refused { what: "code asked naming a callee whose plan is not in yet (not answered in this walk, nor a kept row unmoved)" };
            return self.program_failed(p, Failure::Task { tag: CodeRecord::TAG, unit, failed });
        };
        let prog = &self.progs.list[p as usize];
        let inst = &prog.insts[i as usize];
        let kept = inst.kept.and_then(|r| kept_parts(&prog.kept).map(|parts| parts.rows[r as usize].code));
        let inst = &mut self.progs.list[p as usize].insts[i as usize];
        inst.code_ask = Some(ask);
        inst.code = key;
        inst.tier = tier;
        inst.callees = callees;
        inst.statics = statics;
        inst.spans = spans;
        inst.callee_plans = callee_plans;
        if kept == Some(key.id()) {
            // The kept row's code is this key's: filed by the request that kept it.
            return self.code_settled(p, i);
        }
        inst.code_state = CState::Getting;
        self.ready.push_back(Work::Program(Step::GetCode(p, i)));
    }

    /// Where each of `instances` (sorted, each once) has its plan filed now: an instance of this
    /// walk at the address it settled at, else a kept row not examined at its kept address.
    /// `None` when one is neither (asked and not answered yet, or examined and moved).
    fn plans_at(&self, p: u32, instances: &[Key<PlanRecord>]) -> Option<Vec<PlanAt, A>> {
        let prog = &self.progs.list[p as usize];
        let parts = kept_parts(&prog.kept);
        let mut out = Vec::with_capacity_in(instances.len(), self.alloc);
        for instance in instances {
            let inst = prog.index.binary_search_by_key(&instance.id(), |(id, _)| *id).ok().map(|at| &prog.insts[prog.index[at].1 as usize]);
            let address = match inst {
                Some(inst) if inst.state == IState::Settled => inst.address,
                Some(inst) if inst.examined || inst.walked => return None,
                _ => {
                    let parts = parts.as_ref()?;
                    let r = parts.rows.binary_search_by(|row| row.instance.cmp(&instance.id())).ok()?;
                    parts.rows[r].plan
                }
            };
            out.push(PlanAt { instance: *instance, address });
        }
        Some(out)
    }

    fn walk_end(&mut self, p: u32) {
        if self.progs.list[p as usize].ended {
            return;
        }
        self.progs.list[p as usize].ended = true;
        let alloc = self.alloc;
        let (rows, gone) = match self.walker.end(p) {
            Ok(end) => (copy_in(end.rows, alloc), copy_in(end.gone, alloc)),
            Err(failed) => return self.program_failed(p, Failure::Task { tag: ProgramManifest::TAG, unit: NO_UNIT, failed }),
        };
        if rows.windows(2).any(|pair| pair[0].instance.id() >= pair[1].instance.id()) || gone.windows(2).any(|pair| pair[0].id() >= pair[1].id()) {
            return self.fail(Failure::Misused("a walk ended with rows or gone instances out of order"));
        }
        let prog = &mut self.progs.list[p as usize];
        let mut out = Vec::new_in(alloc);
        changes(kept_of(&prog.kept), &rows, &gone, &mut out);
        prog.rows = rows;
        prog.gone = gone;
        prog.changes = out;
        self.spans_begin(p);
    }

    /// Every span table settled: the link step, if a row or a table moved; else the kept
    /// manifest filed again, if a plan's or a table's address moved; else it stands.
    fn spans_end(&mut self, p: u32) {
        let alloc = self.alloc;
        let prog = &mut self.progs.list[p as usize];
        // The tables the link step is given: each that came, or whose bytes moved. One whose
        // reads moved and whose bytes did not (a table above a comment) is not: its address
        // moves, which a refiled kept manifest records.
        let mut link_spans = Vec::new_in(alloc);
        let mut span_moved = false;
        for span in prog.spans.iter() {
            match span.kept {
                Some(kept) if kept.digest == span.digest => span_moved |= kept.address != span.address.id(),
                _ => link_spans.push(SpanRecordAt { anchor: span.anchor, address: span.address }),
            }
        }
        // A kept table no row names any more: the kept manifest must stop naming it.
        let named = prog.spans.iter().filter(|span| span.kept.is_some()).count();
        span_moved |= kept_parts(&prog.kept).is_some_and(|parts| parts.spans.len() != named);
        prog.link_spans = link_spans;
        let only_spans_skipped = planted(Plant::SpanOnlyLinkSkipped) && prog.changes.is_empty();
        let link = !prog.changes.is_empty() || (!prog.link_spans.is_empty() && !only_spans_skipped);
        if !link && let Some(parts) = kept_parts(&prog.kept) {
            // No row the linker holds moved, and no table: no link step. The kept manifest
            // stands unless a row's plan address moved (a fact it read moved and its result
            // stood; the address is part of the row) or a table's did: then it is filed again
            // with the same linker state, so the next request reads that plan at its address and
            // does not examine it again, and finds that table by its kept reads.
            // A unit manifest that moved with no row is not a reason: that write is `O(rows)` on an
            // edit no plan reads (module doc, "Durability"). The plant files it again.
            let moved_plan = prog.insts.iter().any(|inst| {
                inst.examined && inst.state == IState::Settled && inst.kept.is_some_and(|r| parts.rows[r as usize].plan != inst.address)
            });
            let units_moved = planted(Plant::KeptRefiledForUnits)
                && self.units.iter().filter_map(|unit| unit.manifest).any(|key| parts.manifests.binary_search(&key.id()).is_err());
            let refile = moved_plan || units_moved || span_moved || !prog.link_spans.is_empty();
            if !refile {
                prog.state = PState::Done;
                return;
            }
            // No link step: the image is the kept one's, cloned under the new manifest's key.
            let linker = copy_in(parts.linker, alloc);
            return self.linked(p, &linker, false);
        }
        // The plans the link step reads: each row that came or moved, and its callees'.
        let mut wanted = Vec::new_in(alloc);
        for change in prog.changes.iter().filter(|change| change.now.is_some()) {
            wanted.push(change.instance);
            if let Ok(at) = prog.index.binary_search_by_key(&change.instance.id(), |(id, _)| *id) {
                let inst = &prog.insts[prog.index[at].1 as usize];
                wanted.extend_from_slice(&inst.callees);
                wanted.extend_from_slice(&inst.statics);
            }
        }
        wanted.sort_unstable_by_key(|key| key.id());
        wanted.dedup();
        let Some(plans) = self.plans_at(p, &wanted) else {
            return self.fail(Failure::Misused("a link step of a row or callee whose plan is not filed"));
        };
        let prog = &mut self.progs.list[p as usize];
        prog.link_plans = plans;
        prog.state = PState::Linking;
        self.ran.links += 1;
        self.ready.push_back(Work::Program(Step::Link(p)));
    }

    /// The link step answered: the kept manifest is made again from the kept rows the walk did
    /// not touch, the walked rows, the facts their plans read and the linker's state, and filed
    /// by content, after its image ([`Step::Image`]). `wrote`: a link step ran, and its patch
    /// makes the image from the kept one; else the image is the kept one's, cloned under the
    /// new manifest's key.
    fn linked(&mut self, p: u32, linker: &[u8], wrote: bool) {
        let alloc = self.alloc;
        let prog = &self.progs.list[p as usize];
        // Each new row with the facts its plan read, what it refers to (its callees, then its
        // statics: `row.call_len` says where they part) and its ask.
        let mut rows: Vec<(KeptRow, Vec<Fact, A>, Vec<ContentId, A>, Vec<u8, A>), A> = Vec::new_in(alloc);
        if let Some(parts) = kept_parts(&prog.kept) {
            for row in parts.rows {
                let walked = prog.rows.binary_search_by(|walked| walked.instance.id().cmp(&row.instance)).is_ok();
                let went = prog.gone.binary_search_by(|gone| gone.id().cmp(&row.instance)).is_ok();
                if walked || went {
                    continue;
                }
                // An examined plan whose result stood is still this row, at the address and with
                // the reads it has now.
                let examined = prog.index.binary_search_by_key(&row.instance, |(id, _)| *id).ok().map(|at| &prog.insts[prog.index[at].1 as usize]);
                let mut reads = Vec::new_in(alloc);
                let mut kept = *row;
                match examined.filter(|inst| inst.state == IState::Settled) {
                    Some(inst) => {
                        reads.extend_from_slice(&inst.reads);
                        kept.plan = inst.address;
                    }
                    None => {
                        let ords = &parts.shapes[row.shape_at as usize..(row.shape_at + row.shape_len) as usize];
                        reads.extend(ords.iter().map(|&ord| parts.dictionary[ord as usize]));
                    }
                }
                let view = Kept::of(&parts);
                rows.push((kept, reads, copy_in(view.targets_of(row), alloc), copy_in(view.ask_of(row), alloc)));
            }
        }
        for walked in prog.rows.iter() {
            let Ok(at) = prog.index.binary_search_by_key(&walked.instance.id(), |(id, _)| *id) else {
                return self.fail(Failure::Misused("a walk's row of an instance it never asked the plan of"));
            };
            let inst = &prog.insts[prog.index[at].1 as usize];
            if inst.state != IState::Settled {
                return self.fail(Failure::Misused("a walk's row of an instance whose plan was not answered"));
            }
            let mut targets = Vec::with_capacity_in(inst.callees.len() + inst.statics.len(), alloc);
            targets.extend(inst.callees.iter().chain(inst.statics.iter()).map(|key| key.id()));
            let row = KeptRow {
                instance: inst.key.id(),
                item: inst.item.map_or(ZERO, |item| item.0),
                plan: inst.address,
                result: inst.result,
                code: walked.code.id(),
                shape_at: 0,
                shape_len: 0,
                target_at: 0,
                target_len: 0,
                call_len: inst.callees.len() as u32,
                tier: inst.tier,
                ask_at: 0,
                ask_len: 0,
            };
            rows.push((row, copy_in(&inst.reads, alloc), targets, copy_in(&inst.ask, alloc)));
        }
        rows.sort_unstable_by_key(|(row, _, _, _)| row.instance);
        // The dictionary: every fact a row's plan read, with the digest it saw.
        let mut dictionary = Vec::new_in(alloc);
        for (_, reads, _, _) in rows.iter() {
            dictionary.extend_from_slice(reads);
        }
        dictionary.sort_unstable_by_key(|fact: &Fact| fact.id);
        dictionary.dedup_by_key(|fact| fact.id);
        let mut out_rows = Vec::with_capacity_in(rows.len(), alloc);
        let (mut targets, mut shapes, mut asks) = (Vec::new_in(alloc), Vec::new_in(alloc), Vec::new_in(alloc));
        let mut posted: Vec<(u32, u32), A> = Vec::new_in(alloc);
        for (r, (row, reads, row_targets, ask)) in rows.iter().enumerate() {
            let mut row = *row;
            row.shape_at = shapes.len() as u32;
            for read in reads.iter() {
                let Ok(ord) = dictionary.binary_search_by(|fact| fact.id.cmp(&read.id)) else { continue };
                shapes.push(ord as u32);
                posted.push((ord as u32, r as u32));
            }
            row.shape_len = shapes.len() as u32 - row.shape_at;
            row.target_at = targets.len() as u32;
            targets.extend_from_slice(row_targets);
            row.target_len = row_targets.len() as u32;
            row.ask_at = asks.len() as u32;
            asks.extend_from_slice(ask);
            row.ask_len = ask.len() as u32;
            out_rows.push(row);
        }
        posted.sort_unstable();
        let mut postings = Vec::with_capacity_in(posted.len(), alloc);
        let mut posting_at = Vec::with_capacity_in(dictionary.len() + 1, alloc);
        let mut at = 0;
        for ord in 0..dictionary.len() as u32 {
            posting_at.push(postings.len() as u32);
            while at < posted.len() && posted[at].0 == ord {
                postings.push(posted[at].1);
                at += 1;
            }
        }
        posting_at.push(postings.len() as u32);
        // The span tables, as settled this request (every one the rows name), their reads
        // shared by the tables of one owner that read the same files.
        let mut by_owner = Vec::with_capacity_in(prog.spans.len(), self.alloc);
        by_owner.extend(prog.spans.iter().enumerate().map(|(s, span)| (span.owner, s as u32)));
        by_owner.sort_unstable();
        let mut ranges = Vec::with_capacity_in(prog.spans.len(), self.alloc);
        ranges.resize(prog.spans.len(), (0u32, 0u32));
        let mut span_reads: Vec<Fact, A> = Vec::new_in(self.alloc);
        let mut last: Option<(StableId, u32)> = None;
        for &(owner, s) in by_owner.iter() {
            let reads = &prog.spans[s as usize].reads;
            let shared = last.filter(|&(was, first)| was == owner && prog.spans[first as usize].reads == *reads);
            ranges[s as usize] = match shared {
                Some((_, first)) => ranges[first as usize],
                None => {
                    let at = span_reads.len() as u32;
                    span_reads.extend_from_slice(reads);
                    last = Some((owner, s));
                    (at, reads.len() as u32)
                }
            };
        }
        let mut spans = Vec::with_capacity_in(prog.spans.len(), self.alloc);
        for (span, &(read_at, read_len)) in prog.spans.iter().zip(ranges.iter()) {
            spans.push(KeptSpan {
                anchor: span.anchor,
                key: span.key.id(),
                owner: span.owner.0,
                address: span.address.id(),
                digest: span.digest,
                read_at,
                read_len,
            });
        }
        // Each row's tables, as ordinals into `spans`: a walked row's from its code ask, a kept
        // row's from the kept manifest.
        let kept = kept_parts(&prog.kept);
        let (mut row_spans, mut row_span_at) = (Vec::new_in(self.alloc), Vec::with_capacity_in(out_rows.len() + 1, self.alloc));
        for row in out_rows.iter() {
            row_span_at.push(row_spans.len() as u32);
            let start = row_spans.len();
            let walked = prog.rows.binary_search_by(|walked| walked.instance.id().cmp(&row.instance)).is_ok();
            let inst = prog.index.binary_search_by_key(&row.instance, |(id, _)| *id).ok().map(|at| &prog.insts[prog.index[at].1 as usize]);
            let mut anchors = Vec::new_in(self.alloc);
            match (walked, inst, kept.as_ref()) {
                (true, Some(inst), _) => anchors.extend(inst.spans.iter().map(|span| span.anchor)),
                (false, _, Some(parts)) => {
                    let Ok(r) = parts.rows.binary_search_by(|kept| kept.instance.cmp(&row.instance)) else {
                        return self.fail(Failure::Misused("a kept row the kept manifest does not hold"));
                    };
                    anchors.extend(parts.spans_of_row(r).iter().map(|&ord| parts.spans[ord as usize].anchor));
                }
                _ => return self.fail(Failure::Misused("a walk's row of an instance it never asked the plan of")),
            }
            for anchor in anchors.iter() {
                let Ok(ord) = spans.binary_search_by(|span: &KeptSpan| span.anchor.cmp(anchor)) else {
                    return self.fail(Failure::Misused("a row's span table not settled in its program's span phase"));
                };
                row_spans.push(ord as u32);
            }
            row_spans[start..].sort_unstable();
        }
        row_span_at.push(row_spans.len() as u32);
        // The unit manifests every body read was answered against now: each unit's, as this
        // request settled or marked it (every unit is complete in this phase).
        let mut manifests = Vec::with_capacity_in(self.units.len(), self.alloc);
        manifests.extend(self.units.iter().filter_map(|unit| unit.manifest.map(|key| key.id())));
        manifests.sort_unstable();
        manifests.dedup();
        let program = prog.plan.program;
        let payload = record::write_kept(&KeptParts {
            program,
            manifests: &manifests,
            rows: &out_rows,
            targets: &targets,
            asks: &asks,
            shapes: &shapes,
            dictionary: &dictionary,
            postings: &postings,
            posting_at: &posting_at,
            spans: &spans,
            span_reads: &span_reads,
            row_spans: &row_spans,
            row_span_at: &row_span_at,
            linker,
        }, self.alloc);
        let key = ProgramManifest::key(&ProgramInputs { program, header: &payload, chunks: &[] });
        let bytes = record::write_opaque(&key, &payload);
        let prog = &mut self.progs.list[p as usize];
        prog.key = Some(key);
        prog.out = Some(Aligned::copy_from(&bytes));
        prog.state = PState::Filing;
        prog.patched = wrote;
        // Its image first, named by this manifest's key; the manifest is put on its answer.
        self.ready.push_back(Work::Program(Step::Image(p)));
    }

    // ---- span tables ----

    /// The walk ended: every span table the final rows name is found or made. A walked row names
    /// its code ask's; a kept row neither walked nor gone the kept manifest's.
    fn spans_begin(&mut self, p: u32) {
        let alloc = self.alloc;
        let prog = &self.progs.list[p as usize];
        let parts = kept_parts(&prog.kept);
        let mut wanted: Vec<SpanAt, A> = Vec::new_in(alloc);
        for row in prog.rows.iter() {
            if let Ok(at) = prog.index.binary_search_by_key(&row.instance.id(), |(id, _)| *id) {
                wanted.extend_from_slice(&prog.insts[prog.index[at].1 as usize].spans);
            }
        }
        if let Some(parts) = parts.as_ref() {
            for (r, row) in parts.rows.iter().enumerate() {
                let walked = prog.rows.binary_search_by(|walked| walked.instance.id().cmp(&row.instance)).is_ok();
                let went = prog.gone.binary_search_by(|gone| gone.id().cmp(&row.instance)).is_ok();
                if walked || went {
                    continue;
                }
                for &ord in parts.spans_of_row(r) {
                    let span = parts.spans[ord as usize];
                    wanted.push(SpanAt { anchor: span.anchor, key: Key::from_id(span.key), owner: StableId(span.owner) });
                }
            }
        }
        wanted.sort_unstable_by_key(|span| (span.anchor, span.key.id(), span.owner));
        wanted.dedup();
        if wanted.windows(2).any(|pair| pair[0].anchor == pair[1].anchor) {
            let failed = TaskFailed::Refused { what: "one span anchor named under two keys or two owners (rule keys-cover-every-input)" };
            return self.program_failed(p, Failure::Task { tag: SpanTable::TAG, unit: NO_UNIT, failed });
        }
        let mut spans = Vec::with_capacity_in(wanted.len(), alloc);
        for at in wanted.iter() {
            let Some(unit) = self.unit_of_item(&at.owner) else {
                let failed = TaskFailed::Refused { what: "a span table whose owner no unit of the request owns" };
                return self.program_failed(p, Failure::Task { tag: SpanTable::TAG, unit: NO_UNIT, failed });
            };
            // A kept span stands for this anchor only under the same key and owner.
            let kept = parts.as_ref().and_then(|parts| parts.span(&at.anchor)).filter(|kept| kept.key == at.key.id() && kept.owner == at.owner.0);
            spans.push(Span {
                anchor: at.anchor,
                key: at.key,
                owner: at.owner,
                unit,
                kept: kept.copied(),
                state: SState::Getting,
                reads: Vec::new_in(alloc),
                node: record::shape_root::<SpanTable>(at.key),
                address: Key::from_id(ZERO),
                digest: ZERO,
                bytes: None,
                node_bytes: None,
                audit: None,
            });
        }
        let prog = &mut self.progs.list[p as usize];
        prog.spans = spans;
        prog.state = PState::Spans;
        for s in 0..prog.spans.len() as u32 {
            self.span_start(p, s);
            if self.done() || self.progs.list[p as usize].state != PState::Spans {
                return;
            }
        }
    }

    /// Whether a span table of program `p` is not settled yet.
    fn spans_out(&self, p: u32) -> bool {
        self.progs.list[p as usize].spans.iter().any(|span| span.state != SState::Settled)
    }

    /// The digest now of `id`, a [`FactKind::SpanFile`] fact: from its unit's key rows this
    /// request, with no host. `None` for an id of another kind.
    fn span_file_now(&self, id: &ContentId) -> Option<ContentId> {
        if record::kind_of(id) != Some(FactKind::SpanFile) {
            return None;
        }
        let Ok(at) = self.progs.owners.binary_search_by_key(&Owner::of_id(id), |(owner, _)| *owner) else {
            return Some(record::GONE);
        };
        let files = &self.units[self.progs.owners[at].1 as usize].span_files;
        Some(match files.binary_search_by(|file| file.id.cmp(id)) {
            Ok(at) => files[at].digest,
            Err(_) => record::absent(id),
        })
    }

    /// Each of `ids` with its digest now; `None` if one is not a [`FactKind::SpanFile`] fact.
    fn span_reads_now(&self, ids: impl Iterator<Item = ContentId>) -> Option<Vec<Fact, A>> {
        let mut now = Vec::new_in(self.alloc);
        for id in ids {
            now.push(Fact { id, digest: self.span_file_now(&id)? });
        }
        Some(now)
    }

    /// Starts span table `s`'s lookup. A kept one: its kept reads answered now (no record read,
    /// no host); unmoved, it stands at its kept address; moved, its record is read at the
    /// address the answers make. One no kept manifest names: its shape root, then that address.
    fn span_start(&mut self, p: u32, s: u32) {
        let span = &self.progs.list[p as usize].spans[s as usize];
        let Some(kept) = span.kept else {
            self.progs.list[p as usize].spans[s as usize].state = SState::Node;
            return self.ready.push_back(Work::Program(Step::SpanNode(p, s)));
        };
        let Some(parts) = kept_parts(&self.progs.list[p as usize].kept) else {
            return self.fail(Failure::Misused("a kept span table with no kept manifest"));
        };
        let was = parts.span_reads_of(&kept);
        let Some(now) = self.span_reads_now(was.iter().map(|read| read.id)) else {
            return self.fail(Failure::Misused("a kept span table that read a fact other than a file"));
        };
        self.ran.checks.read_lists += 1;
        self.ran.checks.facts_compared += now.len() as u32;
        // The plant: a kept table stands without its reads compared.
        let stands = planted(Plant::SpanFoundAfterBasesMoved) || *now == *was;
        let span = &mut self.progs.list[p as usize].spans[s as usize];
        span.reads = now;
        if stands {
            span.address = Key::from_id(kept.address);
            span.digest = kept.digest;
            span.state = SState::Settled;
            return;
        }
        span.address = record::address::<SpanTable>(span.key, &span.reads);
        span.state = SState::Getting;
        self.ready.push_back(Work::Program(Step::SpanGet(p, s)));
    }

    /// Span table `s`'s shape root read: none, and its owner's task runs; else its files are
    /// answered now and its record read at the address they make.
    fn span_got_node(&mut self, p: u32, s: u32, record: Option<&[u8]>) {
        let node = self.progs.list[p as usize].spans[s as usize].node;
        let Some(bytes) = record else {
            return self.span_needs_owner(p, s);
        };
        let ids = match record::open::<Shape>(bytes, &node).and_then(|root| root.ids.follow(bytes).map_err(RecordError::Format)) {
            Ok(ids) => ids,
            Err(why) => return self.fail(Failure::Record { tag: Shape::TAG, key: node.id(), why }),
        };
        if let Err(why) = record::sorted_ids(ids) {
            return self.fail(Failure::Record { tag: Shape::TAG, key: node.id(), why });
        }
        let Some(now) = self.span_reads_now(ids.iter().copied()) else {
            let why = malformed(0, "a span table's shape names a fact other than a file");
            return self.fail(Failure::Record { tag: Shape::TAG, key: node.id(), why });
        };
        self.ran.checks.read_lists += 1;
        self.ran.checks.facts_compared += now.len() as u32;
        let span = &mut self.progs.list[p as usize].spans[s as usize];
        span.address = record::address::<SpanTable>(span.key, &now);
        span.reads = now;
        span.state = SState::Getting;
        self.ready.push_back(Work::Program(Step::SpanGet(p, s)));
    }

    /// Span table `s`'s record read at its address: none, and its owner's task runs; else it
    /// settles there (under audit, its owner's task runs and must make these bytes).
    fn span_got(&mut self, p: u32, s: u32, record: Option<&[u8]>) {
        let span = &self.progs.list[p as usize].spans[s as usize];
        let (key, address) = (span.key, span.address);
        let Some(bytes) = record else {
            return self.span_needs_owner(p, s);
        };
        let root = match record::check_read::<SpanTable>(bytes, key) {
            Ok((at, root)) if at.id() == address.id() => root,
            Ok((at, _)) => {
                let why = RecordError::Address { found: at.id(), wanted: address.id() };
                return self.fail(Failure::Record { tag: SpanTable::TAG, key: address.id(), why });
            }
            Err(why) => return self.fail(Failure::Record { tag: SpanTable::TAG, key: address.id(), why }),
        };
        let digest = match record::result_digest(root, bytes) {
            Ok(digest) => digest,
            Err(why) => return self.fail(Failure::Record { tag: SpanTable::TAG, key: address.id(), why }),
        };
        if M::AUDIT {
            self.progs.list[p as usize].spans[s as usize].audit = Some(Aligned::copy_from(bytes));
            self.ran.checks.audits += 1;
            return self.span_needs_owner(p, s);
        }
        self.ran.spans_found += 1;
        let span = &mut self.progs.list[p as usize].spans[s as usize];
        span.digest = digest;
        span.state = SState::Settled;
    }

    /// Span table `s` is not found: its owner's task makes it, run once a request per owner.
    fn span_needs_owner(&mut self, p: u32, s: u32) {
        let alloc = self.alloc;
        let prog = &mut self.progs.list[p as usize];
        let (owner, unit) = (prog.spans[s as usize].owner, prog.spans[s as usize].unit);
        prog.spans[s as usize].state = SState::Owner;
        match prog.owners.iter().position(|it| it.owner == owner) {
            Some(o) if prog.owners[o].ran => self.span_from_owner(p, s, o as u32),
            Some(_) => {}
            None => {
                let o = prog.owners.len() as u32;
                prog.owners.push(SpanOwner { owner, unit, ran: false, reads: Vec::new_in(alloc), tables: Vec::new_in(alloc) });
                self.runs.push_back(Work::Program(Step::SpanRun(p, o)));
            }
        }
    }

    /// Owner `o`'s span task gave its tables and its reads: each read must be the driver's own
    /// answer now (the front end hashed the file as the driver does, or the record would be
    /// filed at an address no lookup makes), and every table waiting for it is filed.
    fn span_owner_ran(&mut self, p: u32, o: u32, reads: &[Fact], tables: &[(ContentId, &[u8])]) {
        let alloc = self.alloc;
        let unit = self.progs.list[p as usize].owners[o as usize].unit;
        let refuse = |what| Failure::Task { tag: SpanTable::TAG, unit, failed: TaskFailed::Refused { what } };
        if reads.windows(2).any(|pair| pair[0].id >= pair[1].id) || tables.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return self.program_failed(p, refuse("a span task whose reads or tables are not sorted, each once"));
        }
        if reads.iter().any(|read| self.span_file_now(&read.id) != Some(read.digest)) {
            return self.program_failed(p, refuse("a span task read a file other than as the driver answers it now (its printed name or its image key)"));
        }
        let mut held = Vec::with_capacity_in(tables.len(), alloc);
        held.extend(tables.iter().map(|(anchor, bytes)| (*anchor, Aligned::copy_from(bytes))));
        let owner = &mut self.progs.list[p as usize].owners[o as usize];
        owner.ran = true;
        owner.reads = copy_in(reads, alloc);
        owner.tables = held;
        let who = owner.owner;
        for s in 0..self.progs.list[p as usize].spans.len() as u32 {
            let span = &self.progs.list[p as usize].spans[s as usize];
            if span.owner == who && span.state == SState::Owner {
                self.span_from_owner(p, s, o);
            }
            if self.done() || self.progs.list[p as usize].state == PState::Failed {
                return;
            }
        }
    }

    /// Span table `s` made by owner `o`'s task: filed at the address of the owner's reads, its
    /// shape node first (the next lookup of its key finds which files to answer).
    fn span_from_owner(&mut self, p: u32, s: u32, o: u32) {
        let prog = &self.progs.list[p as usize];
        let (span, owner) = (&prog.spans[s as usize], &prog.owners[o as usize]);
        let Ok(at) = owner.tables.binary_search_by(|(anchor, _)| anchor.cmp(&span.anchor)) else {
            let failed = TaskFailed::Refused { what: "a span table a row names that its owner's span task did not make" };
            return self.program_failed(p, Failure::Task { tag: SpanTable::TAG, unit: span.unit, failed });
        };
        let table = &owner.tables[at].1;
        let (address, bytes) = record::write_read::<SpanTable>(span.key, &owner.reads, &[], table);
        let digest = record::result_of(&[], table);
        let mut ids = Vec::with_capacity_in(owner.reads.len(), self.alloc);
        ids.extend(owner.reads.iter().map(|read| read.id));
        let node = record::write_shape(&span.node, &ids);
        let reads = copy_in(&owner.reads, self.alloc);
        let span = &mut self.progs.list[p as usize].spans[s as usize];
        if let Some(found) = span.audit.take() {
            if *found != *bytes {
                let first = first_difference(&found, &bytes);
                return self.fail(Failure::Audit { tag: SpanTable::TAG, address: address.id(), first });
            }
        }
        span.reads = reads;
        span.address = address;
        span.digest = digest;
        span.bytes = Some(Aligned::copy_from(&bytes));
        span.node_bytes = Some(Aligned::copy_from(&node));
        span.state = SState::PuttingNode;
        self.ready.push_back(Work::Program(Step::SpanPutNode(p, s)));
    }

    // ---- an instance's plan ----

    fn new_inst(&mut self, p: u32, key: Key<PlanRecord>, unit: u32, item: Option<StableId>, ask: &[u8], kept: Option<u32>) -> u32 {
        let alloc = self.alloc;
        let prog = &mut self.progs.list[p as usize];
        let i = prog.insts.len() as u32;
        prog.insts.push(Inst {
            key,
            unit,
            item,
            ask: copy_in(ask, alloc),
            walked: false,
            kept,
            examined: false,
            state: IState::Idle,
            after: After::Probe,
            probing: Probing::Old,
            old: Vec::new_in(alloc),
            probe: Vec::new_in(alloc),
            host_asks: Vec::new_in(alloc),
            now: Vec::new_in(alloc),
            asked: ZERO,
            node: None,
            depth: 0,
            also: Vec::new_in(alloc),
            given: Vec::new_in(alloc),
            address: ZERO,
            result: ZERO,
            reads: Vec::new_in(alloc),
            bytes: None,
            node_bytes: None,
            audit: None,
            callees: Vec::new_in(alloc),
            statics: Vec::new_in(alloc),
            tier: 0,
            spans: Vec::new_in(alloc),
            callee_plans: Vec::new_in(alloc),
            asks: Vec::new_in(alloc),
            code_ask: None,
            code: Key::from_id(ZERO),
            code_state: CState::None,
            code_bytes: None,
        });
        if let Err(at) = prog.index.binary_search_by_key(&key.id(), |(id, _)| *id) {
            prog.index.insert(at, (key.id(), i));
        }
        i
    }

    fn unit_of_item(&self, item: &StableId) -> Option<u32> {
        let owner = Owner::of_subject(&item.0);
        self.progs.owners.binary_search_by_key(&owner, |(owner, _)| *owner).ok().map(|at| self.progs.owners[at].1)
    }

    /// Starts an instance's lookup: its kept shape, else its key's shape trie.
    fn try_plan(&mut self, p: u32, i: u32) {
        let alloc = self.alloc;
        let inst = &mut self.progs.list[p as usize].insts[i as usize];
        if inst.old.is_empty() {
            return self.plan_root(p, i);
        }
        inst.probe = core::mem::replace(&mut inst.old, Vec::new_in(alloc));
        inst.probing = Probing::Old;
        self.plan_probe(p, i);
    }

    fn plan_root(&mut self, p: u32, i: u32) {
        let inst = &mut self.progs.list[p as usize].insts[i as usize];
        inst.depth = 0;
        inst.node = Some(record::shape_root::<PlanRecord>(inst.key));
        inst.state = IState::Walking;
        self.ready.push_back(Work::Program(Step::Node(p, i)));
    }

    /// Answers the instance's candidate shape: its bodies here, the rest by the host.
    fn plan_probe(&mut self, p: u32, i: u32) {
        let alloc = self.alloc;
        let probe = copy_in(&self.progs.list[p as usize].insts[i as usize].probe, alloc);
        let mut now = Vec::with_capacity_in(probe.len(), alloc);
        let mut host = Vec::new_in(alloc);
        for id in probe.iter() {
            if matches!(record::route(id), Ok(Route::Asker | Route::Outside) | Err(_)) {
                let failed = TaskFailed::Refused { what: "a plan's shape reads what its asker sees, or a late read: plans read facts crates own" };
                let unit = self.progs.list[p as usize].insts[i as usize].unit;
                return self.plan_failed(p, i, Failure::Task { tag: PlanRecord::TAG, unit, failed });
            }
            if record::kind_of(id) != Some(FactKind::Mir) {
                now.push(ZERO);
                host.push(*id);
                continue;
            }
            match self.mir_now(p, i, id, false) {
                MirNow::Known { result, .. } => now.push(result),
                MirNow::Stands | MirNow::Pending => {
                    let inst = &mut self.progs.list[p as usize].insts[i as usize];
                    inst.after = After::Probe;
                    inst.state = IState::Body;
                    return;
                }
            }
        }
        let inst = &mut self.progs.list[p as usize].insts[i as usize];
        inst.now = now;
        inst.host_asks = host;
        if inst.host_asks.is_empty() {
            return self.plan_probed(p, i, &[]);
        }
        inst.state = IState::Asking;
        self.ready.push_back(Work::Program(Step::Probe(p, i)));
    }

    fn plan_probed(&mut self, p: u32, i: u32, answers: &[ContentId]) {
        let alloc = self.alloc;
        let inst = &mut self.progs.list[p as usize].insts[i as usize];
        if answers.len() != inst.host_asks.len() {
            return self.fail(Failure::Misused("a plan's shape answered with another number of facts than asked"));
        }
        let mut host = answers.iter();
        let mut facts = Vec::with_capacity_in(inst.probe.len(), alloc);
        for (id, now) in inst.probe.iter().zip(inst.now.iter()) {
            let digest = match record::kind_of(id) == Some(FactKind::Mir) {
                true => *now,
                false => host.next().copied().unwrap_or(ZERO),
            };
            facts.push(Fact { id: *id, digest });
        }
        inst.asked = record::address::<PlanRecord>(inst.key, &facts).id();
        inst.state = IState::Getting;
        self.ran.checks.read_lists += 1;
        self.ran.checks.facts_compared += facts.len() as u32;
        self.ready.push_back(Work::Program(Step::Address(p, i)));
    }

    fn plan_got_address(&mut self, p: u32, i: u32, record: Option<&[u8]>) {
        let inst = &mut self.progs.list[p as usize].insts[i as usize];
        let (key, asked, probing) = (inst.key, inst.asked, inst.probing);
        let Some(bytes) = record else {
            return match probing {
                Probing::Kept => {
                    let what = "a plan the kept program names at its address: the store does not hold it";
                    self.fail(Failure::Store(StoreFault { what }))
                }
                Probing::Old => self.plan_root(p, i),
                Probing::Node => {
                    inst.depth += 1;
                    if inst.depth >= super::WALK_MOST {
                        inst.node = None;
                        self.ran.checks.walks_capped += 1;
                        return self.plan_run(p, i);
                    }
                    inst.node = Some(record::shape_next::<PlanRecord>(Key::from_id(asked)));
                    inst.state = IState::Walking;
                    self.ready.push_back(Work::Program(Step::Node(p, i)));
                }
            };
        };
        match record::check_read::<PlanRecord>(bytes, key) {
            Ok((at, _)) if at.id() == asked => {}
            Ok((at, _)) => {
                let why = RecordError::Address { found: at.id(), wanted: asked };
                return self.fail(Failure::Record { tag: PlanRecord::TAG, key: asked, why });
            }
            Err(why) => return self.fail(Failure::Record { tag: PlanRecord::TAG, key: asked, why }),
        }
        if M::AUDIT {
            inst.audit = Some(Aligned::copy_from(bytes));
            self.ran.checks.audits += 1;
            return self.plan_run(p, i);
        }
        self.ran.plans_found += 1;
        self.plan_settle(p, i, asked, bytes);
    }

    fn plan_got_node(&mut self, p: u32, i: u32, record: Option<&[u8]>) {
        let alloc = self.alloc;
        let Some(node) = self.progs.list[p as usize].insts[i as usize].node else {
            return self.fail(Failure::Misused("a plan's shape node read with no node asked"));
        };
        let Some(bytes) = record else {
            return self.plan_run(p, i);
        };
        let ids = match record::open::<Shape>(bytes, &node).and_then(|root| root.ids.follow(bytes).map_err(RecordError::Format)) {
            Ok(ids) => ids,
            Err(why) => return self.fail(Failure::Record { tag: Shape::TAG, key: node.id(), why }),
        };
        if let Err(why) = record::sorted_ids(ids) {
            return self.fail(Failure::Record { tag: Shape::TAG, key: node.id(), why });
        }
        let inst = &mut self.progs.list[p as usize].insts[i as usize];
        if inst.depth > 0 {
            self.ran.walked += 1;
        }
        inst.probe = copy_in(ids, alloc);
        inst.probing = Probing::Node;
        self.plan_probe(p, i);
    }

    /// Runs the instance's plan once the address of each body it is given is known: its own
    /// item's (none for an instance with no body) and each another body it asked for. Each is
    /// found as a plan's read of it is answered ([`Driver::mir_now`]), so the record given is the
    /// one whose result the plan's shape records: a body of a unit asked is its checked record, one
    /// of a unit not asked is demanded ([`crate::kind::Steps::Mir`]). An examined plan the walk
    /// has not asked waits for it: its ask is the walk's.
    fn plan_run(&mut self, p: u32, i: u32) {
        let inst = &mut self.progs.list[p as usize].insts[i as usize];
        if !inst.walked {
            inst.state = IState::Missed;
            return;
        }
        let Some(given) = self.bodies_given(p, i) else {
            let inst = &mut self.progs.list[p as usize].insts[i as usize];
            inst.after = After::Run;
            inst.state = IState::Body;
            return;
        };
        let inst = &mut self.progs.list[p as usize].insts[i as usize];
        inst.given = given;
        inst.state = IState::Running;
        self.runs.push_back(Work::Program(Step::Run(p, i)));
    }

    /// The body records instance `i` is given, sorted by item, each once: its own item's and
    /// each in `also`, each where [`Driver::mir_now`] finds it. `None` while one is pending (the
    /// instance is woken by [`Driver::plan_resumed`]) or once the request failed.
    fn bodies_given(&mut self, p: u32, i: u32) -> Option<Vec<BodyAt, A>> {
        let alloc = self.alloc;
        let inst = &self.progs.list[p as usize].insts[i as usize];
        let mut items = Vec::with_capacity_in(inst.also.len() + 1, alloc);
        items.extend(inst.item);
        items.extend_from_slice(&inst.also);
        let mut given = Vec::with_capacity_in(items.len(), alloc);
        for item in items.iter() {
            match self.mir_now(p, i, &record::mir_id(item), false) {
                MirNow::Known { address: Some(address), .. } => given.push(BodyAt { item: *item, address }),
                // No record: the item has no body, and the task is given none of it.
                MirNow::Known { address: None, .. } => {}
                MirNow::Stands | MirNow::Pending => return None,
            }
            if self.done() {
                return None;
            }
        }
        given.sort_unstable_by_key(|body| body.item);
        given.dedup_by_key(|body| body.item);
        Some(given)
    }

    /// Runs the instance's code task (its code record was not found) given the bodies its plan
    /// read: its own item's and each other one the plan was given in this request, or a code run
    /// before named ([`Driver::code_wants_body`]).
    fn code_run(&mut self, p: u32, i: u32) {
        let Some(given) = self.bodies_given(p, i) else {
            self.progs.list[p as usize].insts[i as usize].code_state = CState::Body;
            return;
        };
        let inst = &mut self.progs.list[p as usize].insts[i as usize];
        inst.given = given;
        inst.code_state = CState::Running;
        self.runs.push_back(Work::Program(Step::RunCode(p, i)));
    }

    /// The code task read the MIR of `item`'s body, which it was not given. A plan found by its
    /// address names the bodies it read only by their facts, so its code is first given its own
    /// body alone and asks for the rest. Given again only if its plan read that body (the plan
    /// record holds [`crate::record::mir_id`] of it): code is a function of its plan and the
    /// bodies the plan read (`Front::code`), so a code task that reads another body is refused
    /// by name, as are one given already and one past [`super::BODIES_MOST`].
    fn code_wants_body(&mut self, p: u32, i: u32, item: StableId) {
        let inst = &self.progs.list[p as usize].insts[i as usize];
        let unit = inst.unit;
        let id = record::mir_id(&item);
        if !inst.reads.iter().any(|read| read.id == id) {
            let failed = TaskFailed::Refused { what: "a code task read a body its plan did not read (code is a function of its plan and the bodies it read)" };
            return self.plan_failed(p, i, Failure::Task { tag: CodeRecord::TAG, unit, failed });
        }
        let asked = inst.item == Some(item) || inst.also.contains(&item);
        let past = inst.also.len() as u32 + 1 >= super::BODIES_MOST;
        if asked || past {
            return self.plan_failed(p, i, Failure::Task { tag: CodeRecord::TAG, unit, failed: TaskFailed::NoBody { item } });
        }
        self.progs.list[p as usize].insts[i as usize].also.push(item);
        self.code_run(p, i);
    }

    /// The plan task read the MIR of `item`'s body, which it was not given: the plan runs again
    /// given it too, once its record is found or made. One it was given already, or one with no
    /// record, or one past [`super::BODIES_MOST`], fails the program by the body's name: asked
    /// again it would read the same and fail the same way.
    fn plan_wants_body(&mut self, p: u32, i: u32, item: StableId) {
        let inst = &self.progs.list[p as usize].insts[i as usize];
        let (unit, asked) = (inst.unit, inst.item == Some(item) || inst.also.contains(&item));
        let past = inst.also.len() as u32 + 1 >= super::BODIES_MOST;
        if asked || past || self.unit_of_item(&item).is_none() {
            return self.plan_failed(p, i, Failure::Task { tag: PlanRecord::TAG, unit, failed: TaskFailed::NoBody { item } });
        }
        self.progs.list[p as usize].insts[i as usize].also.push(item);
        self.plan_run(p, i);
    }

    fn plan_ran(&mut self, p: u32, i: u32, record: &[u8]) {
        let alloc = self.alloc;
        // The `Copy` fields a failure names are copied out first: `inst` borrows `self.progs`,
        // and `fail` takes all of `self`.
        let inst = &mut self.progs.list[p as usize].insts[i as usize];
        let (key, asked) = (inst.key, inst.asked);
        let (address, root) = match record::check_read::<PlanRecord>(record, key) {
            Ok((at, root)) => (at.id(), root),
            Err(why) => return self.fail(Failure::Record { tag: PlanRecord::TAG, key: key.id(), why }),
        };
        let inst = &mut self.progs.list[p as usize].insts[i as usize];
        if let Some(found) = inst.audit.take() {
            if *found != *record {
                let first = first_difference(&found, record);
                return self.fail(Failure::Audit { tag: PlanRecord::TAG, address: asked, first });
            }
            return self.plan_settle(p, i, address, record);
        }
        let reads = match root.reads.follow(record) {
            Ok(reads) => reads,
            Err(error) => return self.fail(Failure::Record { tag: PlanRecord::TAG, key: address, why: RecordError::Format(error) }),
        };
        let same = inst.probing == Probing::Node && inst.probe.len() == reads.len() && inst.probe.iter().zip(reads).all(|(id, read)| *id == read.id);
        inst.address = address;
        inst.bytes = Some(Aligned::copy_from(record));
        inst.state = IState::Putting;
        match inst.node.filter(|_| !same) {
            Some(node) => {
                let mut ids = Vec::with_capacity_in(reads.len(), alloc);
                ids.extend(reads.iter().map(|read| read.id));
                inst.node_bytes = Some(Aligned::copy_from(&record::write_shape(&node, &ids)));
                self.ready.push_back(Work::Program(Step::PutNode(p, i)));
            }
            None => self.ready.push_back(Work::Program(Step::PutPlan(p, i))),
        }
    }

    fn plan_put(&mut self, p: u32, i: u32, step: Step, put: Result<Put, StoreFault>) {
        let inst = &mut self.progs.list[p as usize].insts[i as usize];
        match (step, put) {
            (_, Err(fault)) => self.fail(Failure::Store(fault)),
            (Step::PutNode(..), Ok(put)) => {
                inst.node_bytes = None;
                if put == Put::Other {
                    self.ran.checks.shape_races += 1;
                }
                self.ready.push_back(Work::Program(Step::PutPlan(p, i)));
            }
            (_, Ok(Put::Other)) => {
                let key = inst.address;
                self.fail(Failure::TwoValues { tag: PlanRecord::TAG, key });
            }
            (_, Ok(_)) => {
                let Some(bytes) = inst.bytes.take() else {
                    return self.fail(Failure::Misused("a plan put with no bytes held"));
                };
                let address = inst.address;
                self.plan_settle(p, i, address, &bytes);
            }
        }
    }

    /// The instance holds the plan at `address`: its reads and result. Its payload is the
    /// front end's, handed to the walker unread.
    fn plan_settle(&mut self, p: u32, i: u32, address: ContentId, bytes: &[u8]) {
        let alloc = self.alloc;
        let root = match blaze_format::open::<Traced>(bytes) {
            Ok(root) => root,
            Err(error) => return self.fail(Failure::Record { tag: PlanRecord::TAG, key: address, why: RecordError::Format(error) }),
        };
        let (reads, result) = match (root.reads.follow(bytes), root.payload.follow(bytes), record::result_digest(root, bytes)) {
            (Ok(reads), Ok(_), Ok(result)) => (reads, result),
            _ => return self.fail(Failure::Record { tag: PlanRecord::TAG, key: address, why: malformed(0, "a plan record's root") }),
        };
        if reads.iter().any(|read| matches!(record::route(&read.id), Ok(Route::Asker | Route::Outside) | Err(_))) {
            let why = malformed(0, "a plan read of what its asker sees, or a late read: plans read facts crates own");
            return self.fail(Failure::Record { tag: PlanRecord::TAG, key: address, why });
        }
        let inst = &mut self.progs.list[p as usize].insts[i as usize];
        inst.address = address;
        inst.result = result;
        inst.reads = copy_in(reads, alloc);
        inst.bytes = Some(Aligned::copy_from(bytes));
        inst.state = IState::Settled;
        self.answer_plan(p, i);
    }

    /// Gives the walker the plan for each of its asks this instance answers.
    fn answer_plan(&mut self, p: u32, i: u32) {
        let asks = core::mem::replace(&mut self.progs.list[p as usize].insts[i as usize].asks, Vec::new_in(self.alloc));
        let mut failed = None;
        {
            // `inst` borrows `self.progs` and the walker `self.walker`: disjoint fields, so the
            // payload is handed over in place, never copied.
            let inst = &self.progs.list[p as usize].insts[i as usize];
            let payload = inst.bytes.as_deref().and_then(|bytes| blaze_format::open::<Traced>(bytes).ok()?.payload.follow(bytes).ok());
            let Some(payload) = payload else {
                return self.fail(Failure::Misused("a settled plan with no record held"));
            };
            for &ask in asks.iter() {
                let planned = Planned { instance: inst.key, result: inst.result, payload };
                if let Err(why) = self.walker.planned(p, ask, planned) {
                    failed = Some(why);
                    break;
                }
            }
        }
        if let Some(failed) = failed {
            self.program_failed(p, Failure::Task { tag: ProgramManifest::TAG, unit: NO_UNIT, failed });
        }
    }

    fn code_ran(&mut self, p: u32, i: u32, record: &[u8]) {
        let code = self.progs.list[p as usize].insts[i as usize].code;
        if let Err(why) = record::open::<CodeRecord>(record, &code) {
            return self.fail(Failure::Record { tag: CodeRecord::TAG, key: code.id(), why });
        }
        let inst = &mut self.progs.list[p as usize].insts[i as usize];
        inst.code_bytes = Some(Aligned::copy_from(record));
        inst.code_state = CState::Putting;
        self.ready.push_back(Work::Program(Step::PutCode(p, i)));
    }

    fn code_settled(&mut self, p: u32, i: u32) {
        let inst = &mut self.progs.list[p as usize].insts[i as usize];
        inst.code_state = CState::Done;
        inst.code_bytes = None;
        let (Some(ask), code) = (inst.code_ask, inst.code) else { return };
        if let Err(failed) = self.walker.coded(p, ask, code) {
            self.program_failed(p, Failure::Task { tag: ProgramManifest::TAG, unit: NO_UNIT, failed });
        }
    }

    // ---- bodies ----

    /// What a plan reads of the body `id` names now. `rows`: for the kept dictionary, where a
    /// body nothing of whose unit moved stands; else (a shape answered, a run) its result and
    /// address are needed, read from its unit's records when its unit kept it.
    fn mir_now(&mut self, p: u32, i: u32, id: &ContentId, rows: bool) -> MirNow {
        let Ok(at) = self.progs.owners.binary_search_by_key(&Owner::of_id(id), |(owner, _)| *owner) else {
            // Its crate left the request.
            return MirNow::Known { result: record::GONE, address: None };
        };
        let u = self.progs.owners[at].1;
        let (bodies, marked) = (self.units[u as usize].bodies, self.units[u as usize].marked);
        // A body this request did not settle stands for the kept rows only when its unit's
        // manifest now is one they were answered against: a mark says nothing moved since the
        // unit's last manifest, which a request killed before it filed this kept manifest may
        // have made (module doc, "Durability").
        let against = rows && self.answered_against(p, u);
        // Settled by its mark, against a manifest the rows read: no body of it moved.
        if against && marked {
            return MirNow::Stands;
        }
        match bodies {
            Bodies::Asked => {
                // The body's slot is named by its text id, which the unit's surface's maker row
                // of the subject gives (the slots are the key task's, by text id).
                let text = match self.text_of(u, id, Waiter::Plan(p, i)) {
                    Text::Pending => return MirNow::Pending,
                    Text::None => return MirNow::Known { result: record::absent(id), address: None },
                    Text::Is(text) => text,
                };
                let unit = &self.units[u as usize];
                let Ok(at) = unit.index.binary_search_by(|(item, _)| item.cmp(&text)) else {
                    self.fail(Failure::MakerNotKeyed { unit: u, item: text });
                    return MirNow::Pending;
                };
                let s = unit.index[at].1;
                let slot = &unit.slots[s as usize];
                let (state, file, result, address) = (slot.state, slot.file, slot.result, slot.address);
                match state {
                    SlotState::Settled => MirNow::Known { result, address: Some(Key::from_id(address)) },
                    _ if against => MirNow::Stands,
                    _ => self.kept_body(u, file, text),
                }
            }
            Bodies::NotAsked => {
                // The plant: a body of a unit not asked is never demanded again.
                if rows && planted(Plant::DemandedBodyNotRerun) {
                    return MirNow::Stands;
                }
                match self.demand(u, *id, Waiter::Plan(p, i)) {
                    Err(why) => {
                        self.fail(why);
                        MirNow::Pending
                    }
                    Ok(false) => MirNow::Pending,
                    Ok(true) => self.demanded_body(u, id),
                }
            }
        }
    }

    /// Whether unit `u`'s manifest now is one program `p`'s kept rows were answered against
    /// (`KeptParts::manifests`, one binary search).
    fn answered_against(&self, p: u32, u: u32) -> bool {
        // The plant: a unit's mark is believed against any kept rows.
        if planted(Plant::KeptRowsTrustUnitMarks) {
            return true;
        }
        let Some(manifest) = self.units[u as usize].manifest else { return false };
        kept_parts(&self.progs.list[p as usize].kept).is_some_and(|parts| parts.manifests.binary_search(&manifest.id()).is_ok())
    }

    /// The text id of the body fact `id` is about, by unit `u`'s surface's maker row of its
    /// subject: the rows are read once a request (`Work::GetMakers`, as a demand reads them),
    /// `waiter` resumed when they are.
    fn text_of(&mut self, u: u32, id: &ContentId, waiter: Waiter) -> Text {
        let Some(row) = record::maker_row_of(id) else {
            self.fail(Failure::Misused("a body fact whose kind no body makes"));
            return Text::Pending;
        };
        let unit = &mut self.units[u as usize];
        if matches!(unit.maker_rows, MakerRows::Unread) {
            unit.maker_rows = MakerRows::Asked;
            self.ready.push_back(Work::GetMakers(u));
        }
        let MakerRows::Read(rows) = &unit.maker_rows else {
            if !self.loading.contains(&(u, waiter)) {
                self.loading.push((u, waiter));
            }
            return Text::Pending;
        };
        match rows.binary_search_by(|fact| fact.id.cmp(&row)) {
            Ok(at) => Text::Is(StableId(rows[at].digest)),
            Err(_) => Text::None,
        }
    }

    /// A demanded body that settled (`Driver::demand` said final), or `absent` when its unit's
    /// surface names no maker of it.
    fn demanded_body(&self, u: u32, id: &ContentId) -> MirNow {
        let absent = MirNow::Known { result: record::absent(id), address: None };
        let Some(row) = record::maker_row_of(id) else { return absent };
        let MakerRows::Read(rows) = &self.units[u as usize].maker_rows else { return absent };
        let Ok(at) = rows.binary_search_by(|fact| fact.id.cmp(&row)) else { return absent };
        let text = StableId(rows[at].digest);
        let found = self.demands.iter().find(|demand: &&Demand<A>| demand.unit == u && demand.item == text);
        match found {
            Some(demand) => {
                let slot = &self.units[u as usize].slots[demand.slot as usize];
                MirNow::Known { result: slot.result, address: Some(Key::from_id(slot.address)) }
            }
            None => absent,
        }
    }

    /// A body its unit kept (not examined this request): its entry in the chunk of its file,
    /// read once per request.
    fn kept_body(&mut self, u: u32, file: u32, item: StableId) -> MirNow {
        let root_at = match self.progs.roots.binary_search_by_key(&u, |(unit, _)| *unit) {
            Ok(at) => at,
            Err(at) => {
                self.progs.roots.insert(at, (u, Holding::Asked));
                self.ready.push_back(Work::Program(Step::BodyRoot(u)));
                return MirNow::Pending;
            }
        };
        if matches!(self.progs.roots[root_at].1, Holding::Asked) {
            return MirNow::Pending;
        }
        let chunk_at = match self.progs.chunks.binary_search_by_key(&(u, file), |(at, _)| *at) {
            Ok(at) => at,
            Err(at) => {
                self.progs.chunks.insert(at, ((u, file), Holding::Asked));
                self.ready.push_back(Work::Program(Step::BodyChunk(u, file)));
                return MirNow::Pending;
            }
        };
        let Some(key) = self.chunk_key(u, file) else {
            self.fail(Failure::Misused("a body's chunk read with no manifest root held"));
            return MirNow::Pending;
        };
        let Holding::Held(bytes) = &self.progs.chunks[chunk_at].1 else {
            return MirNow::Pending;
        };
        let entry = record::open::<UnitChunk>(bytes, &key).and_then(|root| root.entries.follow(bytes).map_err(RecordError::Format)).map(
            |entries| match entries.binary_search_by(|entry| entry.item.cmp(&item.0)) {
                Ok(at) => Some((entries[at].result, entries[at].address)),
                Err(_) => None,
            },
        );
        match entry {
            // Filed silent while its surface now names it: its key and file did not move, so it
            // was not examined (driver doc, "Silent items": a flip is not yet re-examined).
            Ok(Some((_, address))) if address == record::SILENT => {
                self.fail(Failure::Misused("a plan reads a body its surface names that its unit filed silent (not examined since)"));
                MirNow::Pending
            }
            Ok(Some((result, address))) => MirNow::Known { result, address: Some(Key::from_id(address)) },
            Ok(None) => {
                self.fail(Failure::Misused("a body its unit keeps that the chunk of its file does not name"));
                MirNow::Pending
            }
            Err(why) => {
                self.fail(Failure::Record { tag: UnitChunk::TAG, key: key.id(), why });
                MirNow::Pending
            }
        }
    }

    /// A unit record was read for bodies' addresses: every program's rows, and every instance
    /// waiting for a body, try again.
    fn bodies_moved(&mut self) {
        for p in 0..self.progs.len() {
            self.plan_resumed(p, ROWS);
            for i in 0..self.progs.list[p as usize].insts.len() as u32 {
                if self.done() {
                    return;
                }
                self.plan_resumed(p, i);
            }
        }
    }

    /// A demanded body settled, or a body's address was read: the waiter goes on.
    pub(super) fn plan_resumed(&mut self, p: u32, i: u32) {
        if self.done() || self.progs.list[p as usize].state == PState::Failed {
            return;
        }
        if i == ROWS {
            return self.rows_bodies(p);
        }
        let inst = &self.progs.list[p as usize].insts[i as usize];
        if inst.state == IState::Settled && inst.code_state == CState::Body {
            return self.code_run(p, i);
        }
        if inst.state != IState::Body {
            return;
        }
        match inst.after {
            After::Probe => self.plan_probe(p, i),
            After::Run => self.plan_run(p, i),
        }
    }

    /// Instance `i` of program `p` failed (its plan or code task, or a body it waited on):
    /// the program fails; every other program goes on.
    pub(super) fn plan_failed(&mut self, p: u32, i: u32, why: Failure) {
        if i != ROWS {
            self.progs.list[p as usize].insts[i as usize].state = IState::Failed;
        }
        self.program_failed(p, why);
    }

    /// Program `p` failed: its failure is recorded at `(NO_UNIT, p)`, past every unit's, and
    /// nothing more of it is answered or filed.
    fn program_failed(&mut self, p: u32, why: Failure) {
        if self.progs.list[p as usize].state == PState::Failed {
            return;
        }
        self.progs.list[p as usize].state = PState::Failed;
        self.failed_at((NO_UNIT, p), why);
    }
}
