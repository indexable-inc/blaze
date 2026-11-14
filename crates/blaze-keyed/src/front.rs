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
//! - [`Front::body`]: [`Task::Body`](crate::driver::Task::Body), one body's record, filed at its
//!   address.
//!
//! There is no `now`: a fact's digest now is [`Universe::answer`], a lookup in mapped sorted
//! tables by the fact id alone ([`crate::record::route`]), which the host runs for the driver's
//! [`Effect::Answer`](crate::driver::Effect::Answer) and the front end runs for its own asks.
//! No fact needs the front end to answer it: every fact is a row of a surface, a fact a body
//! made (in its unit's manifest or, in the asking unit, the facts its settled bodies made so
//! far), what the asking unit sees (its own surface's row, else its seen row), or a late read
//! (the request's outside answers). Outside asks of a run (a file's stamp, a variable) stay
//! with the host's `Files`.
//!
//! A task reads records only through what it is handed: the [`Universe`]. It reads no file, no
//! store and no clock; a key task reads the unit's text through the front's own sans-IO inputs,
//! which the command gathered before the request (blaze-unitkey's file images).
//!
//! Every method takes `&self` and is called from worker threads at once, for different units
//! and items: a `Front` is `Sync`, and keeps nothing mutable it does not lock itself.

#![blaze::sans_io]

use core::alloc::Allocator;

use blaze_format::Aligned;
use blaze_ids::ContentId;

use crate::{
    driver::TaskFailed,
    kind::{Body, FileImage, Key, Surface, UnitManifest},
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
/// dependencies, transitively; for its body tasks, those and `u`'s own (`asker`); the request's
/// late reads.
#[derive(Clone, Copy, Debug)]
pub struct Universe<'u> {
    /// The asking unit.
    pub asker: u32,
    /// Sorted by owner, each owner once.
    pub tables: &'u [Table<'u>],
    /// The request's late reads (`env!`, `include!`), sorted by id, each once.
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
}
