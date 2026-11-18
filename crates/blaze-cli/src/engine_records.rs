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
use blaze_own::Alloc;
use bstd::cx::Cx;

use crate::front_load::{CrateRecords, LoadError};
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
}

/// The section borrowed where it lies in the mapping, refused when it is not aligned.
pub(crate) enum InPlace {}

impl Route for InPlace {
    fn section<'r>(name: Name, which: &'static str, bytes: &'r [u8], _counts: &mut ReadCounts, _alloc: Alloc<'r>) -> Result<&'r [u8], ReadError> {
        Aligned16::new(bytes).map(Aligned16::bytes).map_err(|why| ReadError::Misaligned { name, which, why })
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
}

/// The `CrateScope` record `record` of `name`, crate `key`'s, opened by route `R`: its payload
/// and its three sections cut where they lie, checked once (front_load.rs `checked_records`),
/// counted in `counts`. `alloc` holds what the items record's open check makes, and the
/// [`Copying`] route's copies.
pub(crate) fn open_record<'r, R: Route>(name: Name, record: Aligned16<'r>, key: CrateItemKey, counts: &mut ReadCounts, alloc: Alloc<'r>) -> Result<CrateRecords<'r>, ReadError> {
    let view = match blaze_drive::record::open(record.bytes()) {
        Ok(blaze_drive::record::Stored::Whole(view)) => view,
        Ok(blaze_drive::record::Stored::Shape(_)) => return Err(LoadError::NotWhole { name, why: "a shape record, which holds no payload" }.into()),
        Err(_) => return Err(LoadError::NotWhole { name, why: "its bytes are not a record of this tool's form" }.into()),
    };
    let payload = Aligned16::new(view.payload).map_err(|why| ReadError::Misaligned { name, which: "payload", why })?;
    let parts = sections::sections(payload.bytes()).map_err(|error| LoadError::Sections { name, error })?;
    counts.records += 1;
    counts.bytes += record.len() as u64;
    counts.sections += (parts.names.len() + parts.items.len() + parts.expansions.len()) as u64;
    let names = R::section(name, "names section", parts.names, counts, alloc)?;
    let items = R::section(name, "items section", parts.items, counts, alloc)?;
    let expansions = R::section(name, "expansion texts section", parts.expansions, counts, alloc)?;
    Ok(crate::front_load::checked_records(name, key, names, items, expansions, alloc)?)
}

/// The command's reader: every crate's records opened once a request, in place, for `'r`.
/// Local to the request (engine_check.rs `check`; `blaze run`'s parent, main.rs at
/// `Effect::StartProgram` through run_names.rs), dropped with the store before its `Lent` is
/// committed and let go: nothing of `'r` lives in a stage.
pub(crate) struct EngineRecords<'r> {
    read: std::vec::Vec<(Name, Key, CrateRecords<'r>), Global>,
    counts: ReadCounts,
}

impl<'r> EngineRecords<'r> {
    pub(crate) fn new() -> EngineRecords<'r> {
        EngineRecords { read: std::vec::Vec::new_in(Global), counts: ReadCounts::default() }
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
        let records = open_record::<InPlace>(input.name, record, key, &mut self.counts, alloc)?;
        self.read.push((input.name, input.at, records));
        Ok(records)
    }

    /// What it read so far.
    pub(crate) fn counts(&self) -> ReadCounts {
        self.counts
    }
}
