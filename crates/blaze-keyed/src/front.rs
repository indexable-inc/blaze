//! The seam between the process and the front end (docs/v2/plan/design/host.md, "Crates"): the
//! three tasks the driver's host asks of a front end, and the one lookup every side answers a
//! fact by. Here, so neither side depends on the other: the host (crates/blaze-host) performs
//! every [`Effect`](crate::driver::Effect) with a `Front` it is given; blaze-unitkey and
//! blaze-front export plain functions, and the adapter that implements `Front` with them is the
//! command's.
//!
//! - [`Front::key`]: [`Task::Key`](crate::driver::Task::Key), every key of a unit from its text.
//! - [`Front::surface`]: [`Task::Surface`](crate::driver::Task::Surface), the unit's surface
//!   record, filed at its address.
//! - [`Front::body_of`]: [`Task::Body`](crate::driver::Task::Body), one body's record running
//!   its [`Steps`], filed at its address; [`Front::body`] is its checked steps.
//! - [`Front::plan`], [`Front::code`], [`Front::spans`], [`Front::link`]: a program's instances,
//!   their code, their owners' span tables and its one link step per request (crate::program),
//!   which returns a patch and writes nothing.
//!   Their default bodies refuse by name, so a front end that builds no program (`check`) need
//!   not implement them.
//!
//! There is no `now`: a fact's digest now is [`Universe::answer`], a lookup in mapped sorted
//! tables by the fact id alone ([`crate::record::route`]), which the host runs for the driver's
//! [`Effect::Answer`](crate::driver::Effect::Answer) and the front end runs for its own asks.
//! No fact needs the front end to answer it: every fact is a row of a surface, a fact a body
//! made (in its unit's manifest or, in the asking unit, the facts its settled bodies made so
//! far), what the asking unit sees (its own surface's row, else its seen row), or a late read
//! (the asking unit's own late table). Outside asks of a run (a file's stamp, a variable) stay
//! with the host's `Files`.
//!
//! A task reads records only through what it is handed: the [`Universe`]. It reads no file, no
//! store and no clock; a key task reads the unit's text through the front's own sans-IO inputs,
//! which the command gathered before the request (blaze-unitkey's file images).
//!
//! Every method takes `&self` and is called from worker threads at once, for different units
//! and items: a `Front` is `Sync`, and keeps nothing mutable it does not lock itself.

#![blaze::sans_io]

use alloc::vec::Vec;
use core::alloc::Allocator;

use blaze_format::Aligned;
use blaze_ids::ContentId;

use crate::{
    driver::TaskFailed,
    kind::{Body, CodeRecord, FileImage, Key, PlanRecord, Steps, Surface, UnitManifest},
    program::RowChange,
    record::{self, Fact, Owner, Route, StableId, Traced},
};

/// One file of a unit as its key task gives it: the image's key and its items, which are
/// `items[at..at + len]` of [`Keyed::items`], in stable-id order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileItems {
    pub image: Key<FileImage>,
    pub at: u32,
    pub len: u32,
}

/// What a key task gives: blaze-unitkey's `UnitKeys`, field for field (its `FileKeys` is
/// [`FileItems`]), so the adapter moves the item list without a copy.
pub struct Keyed<A: Allocator> {
    /// The unit's text key ([`crate::kind::UnitInputs`]).
    pub manifest: Key<UnitManifest>,
    pub surface: Key<Surface>,
    /// Every file of the unit, in the unit's file order.
    pub files: Vec<FileItems, A>,
    /// Every body of the unit with its key: each file's in a run, in stable-id order, each item
    /// once in the unit.
    pub items: Vec<(StableId, Key<Body>), A>,
    /// Per file of `files`, in that order: [`record::printed_name`] of the name the program
    /// prints for it (`file!()`, a panic's location), or `ZERO` for a file it cannot name. The
    /// driver answers each file's [`record::FactKind::SpanFile`] fact from it and the file's
    /// image key ([`record::span_file_id`]), so a span table's read of a file covers both. The
    /// unit's manifest key covers the names (an obligation of the front end's
    /// [`crate::kind::UnitInputs`]): a name moves the key, as a file's text does.
    pub names: Vec<ContentId, A>,
}

/// What a span task gives ([`Front::spans`]): an owner's span tables, by anchor (sorted, each
/// once), each one hir-ty-codegen `BZSPANS2` table, and the one read list every table of the
/// owner is filed with: a [`record::FactKind::SpanFile`] fact of each file a table names (sorted
/// by id, each once), with the digest [`record::span_file_digest`] gives it now.
pub struct SpanTables<A: Allocator> {
    pub reads: Vec<Fact, A>,
    pub tables: Vec<(ContentId, Vec<u8, A>), A>,
}

/// One unit's tables as a task or an answer reads them: its surface record (in place), that
/// record's fact table, and the facts its bodies made (its manifest's, or for the asking unit
/// the facts of its bodies settled so far).
#[derive(Clone, Copy, Debug)]
pub struct Table<'u> {
    pub unit: u32,
    pub owner: Owner,
    /// The surface record's bytes, 16-aligned.
    pub surface: &'u [u8],
    /// Its `facts`, sorted by id, each once.
    pub facts: &'u [Fact],
    /// Sorted by id, each once.
    pub made: &'u [Fact],
}

impl<'u> Table<'u> {
    /// The table of unit `unit` owned by `owner` whose surface record is `surface` and whose
    /// bodies made `made`: its fact table read in place.
    pub fn of(unit: u32, owner: Owner, surface: &'u [u8], made: &'u [Fact]) -> Result<Table<'u>, Unanswerable> {
        let refused = Unanswerable::NotASurface { unit };
        let root = blaze_format::open::<Traced>(surface).map_err(|_| refused)?;
        let facts = root.facts.follow(surface).map_err(|_| refused)?;
        Ok(Table { unit, owner, surface, facts, made })
    }
}

/// What a task may read, and nothing else: for unit `u`'s surface task, the tables of `u`'s
/// dependencies, transitively; for its body tasks, those and `u`'s own (`asker`); `u`'s own
/// late reads.
#[derive(Clone, Copy, Debug)]
pub struct Universe<'u> {
    /// The asking unit.
    pub asker: u32,
    /// Sorted by owner, each owner once.
    pub tables: &'u [Table<'u>],
    /// The asking unit's own late reads (`env!`, `include!`), sorted by id, each once: never
    /// another unit's, as a late id ([`record::late_id`]) names no unit and two units read one
    /// name with two values.
    pub outside: &'u [Fact],
}

/// Why a fact has no answer: each names the fact or the unit, and the rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unanswerable {
    /// The id's kind byte is no fact kind ([`record::Unrouted`]).
    Unrouted { id: ContentId, kind: u8 },
    /// An ask of what the asking unit sees, by a task with no own table (a surface task).
    NoAsker { id: ContentId },
    /// A late read the request did not read: its key would have missed it (rule
    /// keys-cover-every-input).
    NoLate { id: ContentId },
    /// A table whose surface record does not open.
    NotASurface { unit: u32 },
    /// A fact only the driver answers, from its own key rows: a file a span table reads
    /// ([`record::FactKind::SpanFile`]). No table holds it, so a table's `absent` would be a
    /// wrong answer, never given.
    DriverOwn { id: ContentId },
}

impl Unanswerable {
    /// The task failure an ask the front end cannot answer is.
    pub fn failed(self) -> TaskFailed {
        TaskFailed::Refused {
            what: match self {
                Unanswerable::Unrouted { .. } => "an ask whose fact id names no fact kind (record::route)",
                Unanswerable::NoAsker { .. } => "an ask of what the asking unit sees, by a task with no own surface",
                Unanswerable::NoLate { .. } => "a late read the request did not read (rule keys-cover-every-input)",
                Unanswerable::NotASurface { .. } => "a table of the universe whose surface record does not open",
                Unanswerable::DriverOwn { .. } => "an ask of a file a span table reads: the driver answers it from its key rows, no table does",
            },
        }
    }
}

fn row(facts: &[Fact], id: &ContentId) -> Option<ContentId> {
    facts.binary_search_by(|fact| fact.id.cmp(id)).ok().map(|at| facts[at].digest)
}

impl<'u> Universe<'u> {
    /// The table owned by `owner`.
    pub fn table(&self, owner: Owner) -> Option<&Table<'u>> {
        self.tables.binary_search_by(|table| table.owner.cmp(&owner)).ok().map(|at| &self.tables[at])
    }

    /// The asking unit's own table, if the task has one.
    pub fn own(&self) -> Option<&Table<'u>> {
        self.tables.iter().find(|table| table.unit == self.asker)
    }

    /// The digest now of fact `id`: the one answer every side gives (the host for the driver's
    /// asks, the front end for its own). An owner in the universe without the row answers
    /// [`record::absent`]; an owner not in it, [`record::GONE`].
    ///
    /// Floor: one binary search in the owning table. Here: that and one over the universe's
    /// owners (`log u`); nothing allocated.
    pub fn answer(&self, id: &ContentId) -> Result<ContentId, Unanswerable> {
        let route = record::route(id).map_err(|unrouted| Unanswerable::Unrouted { id: unrouted.id, kind: unrouted.kind })?;
        if record::kind_of(id) == Some(record::FactKind::SpanFile) {
            return Err(Unanswerable::DriverOwn { id: *id });
        }
        Ok(match route {
            Route::Outside => row(self.outside, id).ok_or(Unanswerable::NoLate { id: *id })?,
            Route::Asker => {
                let own = self.own().ok_or(Unanswerable::NoAsker { id: *id })?;
                match row(own.facts, id).or_else(|| row(own.facts, &record::seen_id())) {
                    Some(digest) => digest,
                    None => record::absent(id),
                }
            }
            Route::Surface(owner) => match self.table(owner) {
                Some(table) => row(table.facts, id).unwrap_or_else(|| record::absent(id)),
                None => record::GONE,
            },
            Route::Made(owner) => match self.table(owner) {
                Some(table) => row(table.made, id).unwrap_or_else(|| record::absent(id)),
                None => record::GONE,
            },
        })
    }
}

/// A body record given to a plan task, or to its code task (blaze-front's `body::mir::GivenRecord`, served by
/// `hir_ty::body_store::install_given`): the item by its fact subject (what
/// [`crate::record::mir_id`] takes), the record's bytes, checked by the driver under its key
/// and read in place, and its [`crate::record::result_digest`], the digest the plan's read of
/// the body's MIR fact records.
#[derive(Clone, Copy, Debug)]
pub struct GivenBody<'a> {
    pub item: StableId,
    pub record: &'a [u8],
    pub result: ContentId,
}

/// The front end, as the host calls it.
///
/// The invariants each implementation keeps (blaze-host's tests hold a scripted one to them;
/// the real one is held to them by blaze-unitkey's and blaze-front's own tests):
/// - Deterministic: the same unit text and the same answers give the same keys and the same
///   record bytes (rule 25), whatever thread runs it and whatever ran before.
/// - Read-addressed: a record's `reads` are every fact its work asked, each once, sorted by id,
///   with the digest [`Universe::answer`] gave (absences included), and nothing it did not ask;
///   what it asks next depends only on its key and the answers so far. Its `facts` are the facts
///   it makes, sorted by id, each once. It is written with [`crate::record::write_read`], so its
///   head is its address; the driver checks that ([`crate::record::check_read`]).
/// - A surface's `facts` hold the [`crate::record::seen_id`] row.
/// - A task that cannot do its work fails with [`TaskFailed::Refused`] naming what; a panic is
///   caught by the host and fails that task only ([`TaskFailed::Panicked`]).
pub trait Front: Sync {
    /// Keys unit `unit` from its text: its text key, its surface's key and every body's key,
    /// before any work and reading no record; the lists in `frame`.
    ///
    /// Floor: the unit's changed files reparsed and its items' keys hashed (blaze-unitkey).
    fn key<A: Allocator + Clone>(&self, unit: u32, frame: A) -> Result<Keyed<A>, TaskFailed>;

    /// Unit `unit`'s surface record for `key`: its exported facts and its shape over `universe`
    /// (its dependencies' tables). Returns the record's bytes, 16-aligned.
    ///
    /// Floor: the unit's own scopes once; one lookup per fact asked of a dependency.
    fn surface(&self, unit: u32, key: Key<Surface>, universe: Universe<'_>) -> Result<Aligned, TaskFailed>;

    /// The body record of `item` in unit `unit` for `key`: what the body's check made, the facts
    /// it makes, and its shape over `universe` (the dependencies' tables and the unit's own).
    /// Returns the record's bytes, 16-aligned.
    ///
    /// Floor: the body's own check plus one lookup per fact it asks.
    fn body(&self, unit: u32, item: StableId, key: Key<Body>, universe: Universe<'_>) -> Result<Aligned, TaskFailed>;

    /// [`Front::body`] running `steps` ([`Steps`]), under `key`, which the driver made with
    /// [`Steps::key`]: what the host calls. A front end that makes MIR-alone bodies (blaze-front's
    /// `body::body_with`) implements it; the default runs [`Steps::Checked`] by [`Front::body`]
    /// and refuses [`Steps::Mir`] by name, so a unit not asked is never checked in its place.
    ///
    /// Floor: as [`Front::body`], less the borrow check for [`Steps::Mir`].
    fn body_of(&self, unit: u32, item: StableId, key: Key<Body>, steps: Steps, universe: Universe<'_>) -> Result<Aligned, TaskFailed> {
        match steps {
            Steps::Checked => self.body(unit, item, key, universe),
            Steps::Mir => Err(TaskFailed::Refused { what: "this front end makes no MIR-alone body (a body of a unit no request checks)" }),
        }
    }

    /// The plan record named `key` ([`crate::kind::PlanRecordInputs`]) of program `program`,
    /// whose task is given `ask` (the front end's ask bytes: a function's or a static's, told
    /// apart by their magic), written with [`crate::record::write_read`] with the front end's
    /// plan as its payload. `bodies` are the body records the plan may read, sorted by item, each
    /// checked by the driver under its key: the instance's own body (none for an instance with
    /// no body) and each other body a run before named by [`TaskFailed::NoBody`]. Its reads
    /// hold every fact asked of `universe` (every unit's tables; its asker is [`crate::driver::NO_UNIT`] with no late table, as a plan reads only facts crates
    /// own, and the driver refuses a plan that read what an asker sees or a late fact) and, per
    /// body it read, the fact [`crate::record::mir_id`] of its item with its
    /// [`GivenBody::result`] (the universe does not answer it). A read of a body not given is
    /// [`TaskFailed::NoBody`] naming it.
    ///
    /// Floor: the instance's own description (the impls chosen, layouts, vtables) plus one
    /// lookup per fact it asks; nothing of a body is checked again (its record is the proof).
    fn plan(
        &self,
        program: u32,
        ask: &[u8],
        key: Key<PlanRecord>,
        bodies: &[GivenBody<'_>],
        universe: Universe<'_>,
    ) -> Result<Aligned, TaskFailed> {
        let _ = (program, ask, key, bodies, universe);
        Err(TaskFailed::Refused { what: "this front end makes no plans (the code path is not built in it)" })
    }

    /// The code record for `key` ([`crate::kind::CodeRecordInputs`]) of program `program`, from
    /// its plan record `plan` and `callees`, the plan records of its callees as the walker named
    /// them in the code ask (each `(instance, record)`, sorted by instance: the places its code
    /// is made against), written with [`crate::record::write_opaque`]: what blaze-link reads in
    /// place. A function's and a static's plan are told apart by their payload's magic.
    /// `bodies` are the body records its plan read, as [`Front::plan`]'s (sorted by item, each
    /// checked by the driver under its key), and `universe` the plan's: the front end opens the
    /// program's snapshot over them as its plan task does. A read of a body not given is
    /// [`TaskFailed::NoBody`] naming it; the driver gives it only if the plan read it too.
    ///
    /// Invariant: code is a function of its plan record and the bodies that plan read, nothing
    /// else. The plan record's address covers every fact the plan read, each body's MIR result
    /// among them, so the code record keeps no reads of its own: it is opaque, named by its key
    /// alone (the walker's template identity, which covers the plan's description and the
    /// results of the bodies it names). `universe` is given only to open the snapshot those
    /// records are read in; a fact the code step reads that the plan did not (a layout codegen
    /// computes again rather than reading the plan's) breaks the invariant: the front end must
    /// make its plan read every fact its code step reads. A break shows as two code records
    /// under one key, refused by name ([`crate::driver::Failure::TwoValues`]).
    ///
    /// Floor: prepare and emit of one function (or one static's value); no other's code.
    fn code(
        &self,
        program: u32,
        key: Key<CodeRecord>,
        plan: &[u8],
        callees: &[(Key<PlanRecord>, &[u8])],
        bodies: &[GivenBody<'_>],
        universe: Universe<'_>,
    ) -> Result<Aligned, TaskFailed> {
        let _ = (program, key, plan, callees, bodies, universe);
        Err(TaskFailed::Refused { what: "this front end makes no code (the code path is not built in it)" })
    }

    /// The span tables of `owner` (a function, const or static of unit `unit`, by its item key)
    /// over `universe` (every unit's tables, asked as `unit`, with `unit`'s late table): every
    /// table its code's locations can name, by anchor, and the one read list they are filed with
    /// ([`SpanTables`]). Run by the driver for an owner one of whose anchors a program's rows
    /// name and whose table is not found by address (crate::driver::program, "Spans"); the
    /// driver files each table it needs as one [`crate::kind::SpanTable`] record, and refuses an
    /// owner whose reads are not its own answers now (each a [`record::FactKind::SpanFile`] fact
    /// of a file of `unit`, digest [`record::span_file_digest`] of the file's image key).
    ///
    /// Invariant: a table is a function of its anchor, the toolchain and the files it reads
    /// (their text and printed names), nothing else: a plan reads no text and keys by item, so a
    /// comment above an item moves its span tables and nothing of its plan or code.
    ///
    /// Floor: the owner's body lowered from text and its tables made, O(its spans).
    fn spans<A: Allocator + Clone>(&self, unit: u32, owner: StableId, universe: Universe<'_>, frame: A) -> Result<SpanTables<A>, TaskFailed> {
        let _ = (unit, owner, universe, frame);
        Err(TaskFailed::Refused { what: "this front end makes no span tables (the code path is not built in it)" })
    }

    /// One link step of program `program`: `changes` are exactly the rows that came, went or
    /// moved against the kept manifest, in instance order, and `code[i]` the code record of
    /// `changes[i].now` (empty for a row that went); `plans` the plan records of every row
    /// that came or moved and of each of their callees (`(instance, record)`, sorted by
    /// instance); `spans` the span tables (by anchor, sorted, each once: each a
    /// [`crate::kind::SpanTable`] record's payload) that came or moved against the kept
    /// manifest (which names each anchor's table digest); `payload` is the linker's state the
    /// kept manifest holds (empty for a first build). A step may carry no row and only span
    /// tables (a comment added above an item): it rewrites the `Location`s of those tables.
    /// Returns, in `frame`, the new state, which the driver files in the next kept manifest, and
    /// the [`Patch`] that turns the program's image of `payload` into the image of the new
    /// state. It writes nothing: the host applies the patch to a clone of the old image, or to
    /// an empty file ([`PatchBase`], [`crate::driver::Effect::Image`]). The patch is a function
    /// of `payload` and the changes alone, never of a file on disk.
    ///
    /// Floor: a patch in proportion to `changes` and `spans` (blaze-link patches in place);
    /// never a relink of the rows that did not move.
    fn link<A: Allocator + Clone>(
        &self,
        program: u32,
        changes: &[RowChange],
        code: &[&[u8]],
        plans: &[(Key<PlanRecord>, &[u8])],
        spans: &[(ContentId, &[u8])],
        payload: &[u8],
        frame: A,
    ) -> Result<Linked<A>, TaskFailed> {
        let _ = (program, changes, code, plans, spans, payload, frame);
        Err(TaskFailed::Refused { what: "this front end links nothing (blaze-link is not behind it yet)" })
    }
}

/// What a link step returns ([`Front::link`]): the linker's new state and the patch from the
/// old state's image to the new one's.
pub struct Linked<A: Allocator> {
    pub payload: Vec<u8, A>,
    pub patch: Patch<A>,
}

/// The bytes a link step changes in a program's image: what the writes start from (`base`), the
/// image's new length, and each write (`bytes[from..from + len]` at offset `at`), in offset
/// order, none overlapping. Its form is all the host needs to apply it (`pwrite` each, then cut
/// to `len`); what the writes hold is the linker's. A first link's patch, and a whole relink
/// after an earlier image, writes the whole image over [`PatchBase::Empty`]: never zero runs
/// written over the old image's bytes below its old length.
pub struct Patch<A: Allocator> {
    pub base: PatchBase,
    pub len: u64,
    pub writes: Vec<PatchWrite, A>,
    pub bytes: Vec<u8, A>,
}

/// What a [`Patch`]'s writes start from, as blaze-link's `PatchBase`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PatchBase {
    /// An empty file: the writes and the length make the whole image, and the old image (the
    /// effect's `from`) is not read.
    Empty,
    /// A clone of the old image (the effect's `from`); refused with none.
    Clone,
}

/// One write of a [`Patch`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PatchWrite {
    pub at: u64,
    pub from: u32,
    pub len: u32,
}
