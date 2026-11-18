//! The engine's reader of another stage's records (runtime.md C3, "the read side holds
//! nothing"): a crate's `CrateScope` record read IN PLACE from the store's mapping, its names,
//! items and expansions sections borrowed from the segment for `'r`, the life of the command's
//! `Lent` borrow, and never copied into the heap.
//!
//! - The store gives the alignment, and the types say it (rule 76): a record of a segment starts
//!   at a multiple of sixteen (blaze-cas core.rs `lay_out`, refused otherwise at the open by
//!   format.rs `check_segment`), its payload at a multiple of sixteen inside it (blaze-drive
//!   record.rs `PAYLOAD_ALIGN`), each section at a multiple of sixteen inside that
//!   (names/sections.rs). Each is taken as an [`Aligned16`] once, where it is cut; a misaligned
//!   one is [`ReadError::Misaligned`], never a copy to an aligned place (rule 3).
//! - A record of this command's own arena (put and not yet committed) is
//!   [`ReadError::NotInPlace`]: a phase's records are committed before the next phase reads them
//!   (engine_check.rs, `commit_puts` after each put), so a get from the arena here is a bug of
//!   that order, named.
//! - The bytes stay valid for `'r` and never change: the `Lent` keeps every segment it mapped
//!   mapped while it is open (blaze-cas lent.rs `lend`'s contract), and a segment file is named
//!   by the digest of its bytes and never written again under that name (core.rs
//!   `write_segment`: "a segment of the same bytes already here is not written again").
//! - Each crate is opened once a request and its sections' check made once (front_load.rs
//!   `checked_records`, rule 74); every worker reads the same `CrateRecords` (plain borrowed
//!   bytes: `Copy` and `Sync`).
//! - The reads are counted where a record is opened ([`ReadCounts`]): records read, their bytes,
//!   and the bytes copied into the heap on the way, which is 0 on this path (the [`Copying`]
//!   route of the tests is the control that the counter counts a copy).

use std::alloc::Global;

use blaze_cas::plain::{Aligned16, Misaligned, NotInPlace};
use blaze_drive::{Input, Key, Name};
use blaze_id::CrateItemKey;
use blaze_ids::ContentId;
use blaze_own::Alloc;
use bstd::cx::Cx;

use crate::front_load::{CrateRecords, LoadError};
use crate::names::expansion_texts::{Eager, RecordSource, ServeError};
use crate::names::sections;
use crate::store::Store;

/// What the reader read this command: counted at the open of each record, a cache hit not again.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct ReadCounts {
    /// Records opened from the store.
    pub(crate) records: u64,
    /// Their whole bytes (head and payload), read where they lie.
    pub(crate) bytes: u64,
    /// The bytes of their names, items and expansion texts sections, handed to the readers
    /// (in place on [`InPlace`]; [`Copying`] copies the same bytes).
    pub(crate) sections: u64,
    /// Bytes of them copied into this process's heap on the way (0 in place).
    pub(crate) copied: u64,
    /// Asks answered by a crate opened before.
    pub(crate) hits: u64,
}

/// Why a crate's record could not be read in place: the request stops on it, naming it.
#[derive(Debug)]
pub(crate) enum ReadError {
    /// Not in the store, not a whole record, its sections or their checks (front_load.rs).
    Load(LoadError),
    /// The store lent it from this command's arena, or its bytes do not start aligned.
    NotInPlace { name: Name, why: NotInPlace },
    /// A section (or the payload) of an aligned record does not start at a multiple of sixteen:
    /// a record not written by this form's writer.
    Misaligned { name: Name, which: &'static str, why: Misaligned },
}

impl core::fmt::Display for ReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ReadError::Load(error) => write!(f, "{error}"),
            ReadError::NotInPlace { name, why } => write!(f, "the record of {name:?} is not read in place: {why} (rule: engine_records.rs)"),
            ReadError::Misaligned { name, which, why } => write!(f, "the {which} of the record of {name:?}: {why} (rule: engine_records.rs)"),
        }
    }
}

impl From<LoadError> for ReadError {
    fn from(error: LoadError) -> ReadError {
        ReadError::Load(error)
    }
}

/// How a section reaches its reader (rule 78: the strategy is a type parameter at this one
/// seam). [`InPlace`] is the command's; [`Copying`], the tests' control, copies it as the old
/// path does (front_load.rs `crate_records`) and counts the copy.
pub(crate) trait Route {
    fn section<'r>(name: Name, which: &'static str, bytes: &'r [u8], counts: &mut ReadCounts, alloc: Alloc<'r>) -> Result<&'r [u8], ReadError>;

    /// A dependency's exports section, as a names task reads it (names/scope.rs `prepare`,
    /// [`EngineRecords::exports`]): in place on [`InPlace`]; on [`Copying`] as the names stage
    /// read it before, copied twice (the record out of the store into the wave's frame, then the
    /// section 16-aligned again), each copy counted.
    fn exports<'r>(name: Name, bytes: &'r [u8], counts: &mut ReadCounts, alloc: Alloc<'r>) -> Result<&'r [u8], ReadError>;
}

/// The section borrowed where it lies in the mapping, refused when it is not aligned.
pub(crate) enum InPlace {}

impl Route for InPlace {
    fn section<'r>(name: Name, which: &'static str, bytes: &'r [u8], _counts: &mut ReadCounts, _alloc: Alloc<'r>) -> Result<&'r [u8], ReadError> {
        Aligned16::new(bytes).map(Aligned16::bytes).map_err(|why| ReadError::Misaligned { name, which, why })
    }

    fn exports<'r>(name: Name, bytes: &'r [u8], _counts: &mut ReadCounts, _alloc: Alloc<'r>) -> Result<&'r [u8], ReadError> {
        Aligned16::new(bytes).map(Aligned16::bytes).map_err(|why| ReadError::Misaligned { name, which: "exports section", why })
    }
}

/// The section copied 16-aligned into `alloc`, each byte counted as copied: the tests' control.
#[cfg(test)]
pub(crate) enum Copying {}

#[cfg(test)]
impl Route for Copying {
    fn section<'r>(_name: Name, _which: &'static str, bytes: &'r [u8], counts: &mut ReadCounts, alloc: Alloc<'r>) -> Result<&'r [u8], ReadError> {
        counts.copied += bytes.len() as u64;
        Ok(blaze_scope::resolve::aligned(bytes, alloc))
    }

    fn exports<'r>(_name: Name, bytes: &'r [u8], counts: &mut ReadCounts, alloc: Alloc<'r>) -> Result<&'r [u8], ReadError> {
        // Twice, as before: out of the store, then aligned again (names/scope.rs before this
        // reader: `work::payload` into the round's frame, then `aligned`).
        counts.copied += 2 * bytes.len() as u64;
        let once = blaze_scope::resolve::aligned(bytes, alloc);
        Ok(blaze_scope::resolve::aligned(once, alloc))
    }
}

/// The `CrateScope` record `record` of `name` cut where it lies: its payload, aligned, and the
/// payload's sections. The one place a record of this form is opened in place, for
/// [`open_record`] and [`EngineRecords::exports`] (rule 30).
fn payload_of<'r>(name: Name, record: Aligned16<'r>) -> Result<(Aligned16<'r>, sections::Sections<'r>), ReadError> {
    let view = match blaze_drive::record::open(record.bytes()) {
        Ok(blaze_drive::record::Stored::Whole(view)) => view,
        Ok(blaze_drive::record::Stored::Shape(_)) => return Err(LoadError::NotWhole { name, why: "a shape record, which holds no payload" }.into()),
        Err(_) => return Err(LoadError::NotWhole { name, why: "its bytes are not a record of this tool's form" }.into()),
    };
    let payload = Aligned16::new(view.payload).map_err(|why| ReadError::Misaligned { name, which: "payload", why })?;
    let parts = sections::sections(payload.bytes()).map_err(|error| LoadError::Sections { name, error })?;
    Ok((payload, parts))
}

/// The `CrateScope` record `record` of `name`, crate `key`'s, opened by route `R`: its payload
/// and its three sections cut where they lie, checked once (front_load.rs `checked_records`),
/// counted in `counts`; the expansion records its directory names served [`Eager`]ly through
/// `source` (the command's: [`InPlaceRecords`], each borrowed where it lies and counted by it).
/// `alloc` holds what the items record's open check makes, the served rows' table, and the
/// [`Copying`] route's copies.
pub(crate) fn open_record<'r, R: Route>(
    name: Name,
    record: Aligned16<'r>,
    key: CrateItemKey,
    source: &mut dyn RecordSource<'r>,
    counts: &mut ReadCounts,
    alloc: Alloc<'r>,
) -> Result<CrateRecords<'r>, ReadError> {
    let (_, parts) = payload_of(name, record)?;
    counts.records += 1;
    counts.bytes += record.len() as u64;
    counts.sections += (parts.names.len() + parts.items.len() + parts.expansions.len()) as u64;
    let names = R::section(name, "names section", parts.names, counts, alloc)?;
    let items = R::section(name, "items section", parts.items, counts, alloc)?;
    let expansions = R::section(name, "expansions directory", parts.expansions, counts, alloc)?;
    Ok(crate::front_load::checked_records::<Eager>(name, key, names, items, expansions, source, alloc)?)
}

/// The command's store as the in-place reader's [`RecordSource`]: each expansion record
/// borrowed where it lies in the mapping for `'r` (never copied), counted in `counts` as a
/// record read; one not in place is [`ServeError::NotInPlace`], naming its key.
pub(crate) struct InPlaceRecords<'c, 's, 'r> {
    pub(crate) cx: &'c Cx,
    pub(crate) store: &'s mut dyn Store<'r>,
    pub(crate) counts: ReadCounts,
}

impl<'r> RecordSource<'r> for InPlaceRecords<'_, '_, 'r> {
    fn record(&mut self, key: ContentId, _alloc: Alloc<'r>) -> Result<Option<&'r [u8]>, ServeError> {
        let Some(held) = self.store.get(self.cx, Key(key)).map_err(ServeError::Store)? else { return Ok(None) };
        let place = held.place.map_err(|why| ServeError::NotInPlace { key, why })?;
        self.counts.records += 1;
        self.counts.bytes += place.len() as u64;
        Ok(Some(place.bytes()))
    }
}

/// A dependency's exports section as a names task is handed it ([`EngineRecords::exports`]),
/// and the payload it was cut from (both where they lie on [`InPlace`]).
#[derive(Clone, Copy)]
pub(crate) struct Exports<'j> {
    /// The record's whole payload, in place: what the names tests' watch keeps
    /// (names/sections.rs `watch`).
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the names tests' watch alone"))]
    pub(crate) payload: &'j [u8],
    /// The exports section, 16-aligned.
    pub(crate) exports: &'j [u8],
}

/// The command's reader: every crate's records opened once a request, in place, for `'r`.
/// Local to the request (engine_check.rs `check`; `blaze run`'s parent, main.rs at
/// `Effect::StartProgram` through run_names.rs), dropped with the store before its `Lent` is
/// committed and let go: nothing of `'r` lives in a stage.
pub(crate) struct EngineRecords<'r> {
    read: std::vec::Vec<(Name, Key, CrateRecords<'r>), Global>,
    counts: ReadCounts,
    /// The crate (by its name) a test plants unreadable for a check scope's read
    /// ([`EngineRecords::scope_records`]; engine_check.rs `Opts::unreadable`, rule 23). The
    /// command's is `None`.
    unreadable: Option<&'static str>,
}

impl<'r> EngineRecords<'r> {
    pub(crate) fn new() -> EngineRecords<'r> {
        EngineRecords { read: std::vec::Vec::new_in(Global), counts: ReadCounts::default(), unreadable: None }
    }

    /// Plants crate `krate`'s records unreadable for every check scope read
    /// ([`EngineRecords::scope_records`]): a test's cause (rule 23), never the command's.
    pub(crate) fn plant_unreadable(&mut self, krate: Option<&'static str>) {
        self.unreadable = krate;
    }

    /// Crate `krate`'s records for a check scope (engine_bodies.rs `scope_of`), as
    /// [`EngineRecords::records`] reads them; the crate a test planted unreadable reads as a
    /// record gone from the store ([`LoadError::NotInStore`]), the error a refusal then names.
    #[expect(clippy::too_many_arguments, reason = "the read's handles, each named")]
    pub(crate) fn scope_records(&mut self, cx: &Cx, store: &mut dyn Store<'r>, input: &Input, key: CrateItemKey, krate: &str, alloc: Alloc<'r>) -> Result<CrateRecords<'r>, ReadError> {
        if self.unreadable.is_some_and(|planted| planted == krate) {
            return Err(LoadError::NotInStore { name: input.name, at: input.at }.into());
        }
        self.records(cx, store, input, key, alloc)
    }

    /// Crate `key`'s records from the record `input` names: opened in place at the first ask,
    /// the same borrow after it.
    pub(crate) fn records(&mut self, cx: &Cx, store: &mut dyn Store<'r>, input: &Input, key: CrateItemKey, alloc: Alloc<'r>) -> Result<CrateRecords<'r>, ReadError> {
        if let Some((_, _, records)) = self.read.iter().find(|(name, at, _)| *name == input.name && *at == input.at) {
            self.counts.hits += 1;
            return Ok(*records);
        }
        let cx = cx.span("engine: a crate's records read in place");
        let Some(held) = store.get(&cx, input.at).map_err(LoadError::Store)? else {
            return Err(LoadError::NotInStore { name: input.name, at: input.at }.into());
        };
        let record = held.place.map_err(|why| ReadError::NotInPlace { name: input.name, why })?;
        // The expansion records its directory names, read in place from the same store and
        // counted with it.
        let mut source = InPlaceRecords { cx: &cx, store, counts: ReadCounts::default() };
        let records = open_record::<InPlace>(input.name, record, key, &mut source, &mut self.counts, alloc)?;
        self.counts.records += source.counts.records;
        self.counts.bytes += source.counts.bytes;
        self.read.push((input.name, input.at, records));
        Ok(records)
    }

    /// The exports section of the dependency record `input` names, for a names task (names/
    /// scope.rs `prepare`, engine_names.rs): opened where it lies in the store's mapping and
    /// borrowed for `'r`, never copied (route [`InPlace`]), counted in this reader's counts. Not
    /// kept here: the wave that asks keeps each crate's borrow once (names/scope.rs
    /// `ScopeRound`). The record was put and committed by an earlier names phase of this request
    /// (engine_check.rs, `commit_puts` after each phase's puts) or by an earlier request; one in
    /// this command's arena is [`ReadError::NotInPlace`] (rule 3).
    pub(crate) fn exports<'j, R: Route>(&mut self, cx: &Cx, store: &mut dyn Store<'r>, input: &Input, alloc: Alloc<'j>) -> Result<Exports<'j>, ReadError>
    where
        'r: 'j,
    {
        let cx = cx.span("engine: a dependency's exports read in place");
        let Some(held) = store.get(&cx, input.at).map_err(LoadError::Store)? else {
            return Err(LoadError::NotInStore { name: input.name, at: input.at }.into());
        };
        let record = held.place.map_err(|why| ReadError::NotInPlace { name: input.name, why })?;
        let (payload, parts) = payload_of(input.name, record)?;
        self.counts.records += 1;
        self.counts.bytes += record.len() as u64;
        self.counts.sections += parts.exports.len() as u64;
        let exports = R::exports(input.name, parts.exports, &mut self.counts, alloc)?;
        Ok(Exports { payload: payload.bytes(), exports })
    }

    /// The tests section of the `CrateScope` record `input` names (names/sections.rs: a test
    /// unit's tests record; empty for a unit not compiled with `test`), where it lies in the
    /// store's mapping, for `'r`: what `blaze test` runs (run_names.rs `test_unit`). Counted as
    /// one record read; nothing is copied.
    pub(crate) fn tests(&mut self, cx: &Cx, store: &mut dyn Store<'r>, input: &Input) -> Result<&'r [u8], ReadError> {
        let cx = cx.span("engine: a unit's tests record read in place");
        let Some(held) = store.get(&cx, input.at).map_err(LoadError::Store)? else {
            return Err(LoadError::NotInStore { name: input.name, at: input.at }.into());
        };
        let record = held.place.map_err(|why| ReadError::NotInPlace { name: input.name, why })?;
        let (_, parts) = payload_of(input.name, record)?;
        self.counts.records += 1;
        self.counts.bytes += record.len() as u64;
        self.counts.sections += parts.tests.len() as u64;
        Ok(parts.tests)
    }

    /// What it read so far.
    pub(crate) fn counts(&self) -> ReadCounts {
        self.counts
    }
}
