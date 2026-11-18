//! A crate's macro expansions as the readers of its `CrateScope` record see them (runtime.md
//! plan row B3, "The end state" 4: "expansions as their own records by content").
//!
//! The record's expansions section is the crate's DIRECTORY (blaze-scope expansion_records.rs:
//! a row a call that made a text, sorted by call key: its record's key, the text's and the
//! column's lengths, and the CALL PART of its origins); each text with its origin column is its
//! own record in the store under the digest of its bytes, written once however many calls of
//! however many crates made it, put after its crate's record by the names task's puts
//! (engine_store.rs `put_all`; the machine's path: work.rs `PendingExtras`, at the owner's put) and filed in the cold
//! family ([`crate::store::plain_place::EXPANSION_TAG`]). The items record's reader still checks
//! each served text against its row's digest once ([`blaze_scope::items_record`] `Stale`).
//!
//! [`ExpansionTexts`] is what a reader holds: the directory, read in place, and for each row the
//! record it names, SERVED by a strategy at the one seam [`Fetch`] (rule 78):
//! - [`Eager`] (today's): at the open (front_load.rs `read_crate`, on the coordinator thread,
//!   where the store can be read) each distinct record the directory names is probed once and
//!   copied into the reader's allocator. A row whose record the store lacks is served MISSING:
//!   only a reader of that call's text or column is refused, by
//!   [`ExpansionRecordsError::Missing`] naming the call and the key, never a re-expansion
//!   (rule 3); every other call, and every walk up a call's callers (it reads the call part
//!   alone, which is in the directory: body_types/covered.rs `callers`), still answers.
//! - [`OnDemand`]: each record fetched at its first read, from a pool worker. That needs a store
//!   pool workers can read by key ([`ON_DEMAND_PIECE`], another lane's); until it is built its
//!   open is a typed refusal naming that piece.

use blaze_id::ItemKey;
use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc};
use blaze_scope::expansion_origins::CallView;
use blaze_scope::expansion_records::{Directory, Entry, ExpansionRecord, ExpansionRecordsError};

use crate::CliError;

/// Why a crate's expansions could not be served or read (rule: names/expansion_texts.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ExpansionTextsError {
    /// The directory, a record, or a row's record the store lacks
    /// ([`ExpansionRecordsError::Missing`]): blaze-scope's typed error, naming the call.
    Records(ExpansionRecordsError),
    /// The strategy `strategy` is not built: `piece` is what builds it.
    NotBuilt { strategy: &'static str, piece: &'static str },
}

impl core::fmt::Display for ExpansionTextsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ExpansionTextsError::Records(error) => write!(f, "{error}"),
            ExpansionTextsError::NotBuilt { strategy, piece } => {
                write!(f, "the `{strategy}` way of serving a crate's expansion records is not built: it needs {piece} (rule: names/expansion_texts.rs)")
            }
        }
    }
}

/// The piece [`OnDemand`] waits for.
pub(crate) const ON_DEMAND_PIECE: &str = "a `Sync` snapshot of the store that pool workers read by key (runtime.md plan row B3, the readers' second form)";

/// Where a strategy reads a record by its key, its bytes 16-aligned for `'s`: the command's
/// store copied into the reader's allocator (front_load.rs `StoreRecords`), the store's mapping
/// borrowed in place for the life of its `Lent` (engine_records.rs `InPlaceRecords`), or a
/// test's.
pub(crate) trait RecordSource<'s> {
    /// The record under `key`, 16-aligned for `'s` (copied into `alloc`, or where it lies);
    /// `None` when the store holds none.
    fn record(&mut self, key: ContentId, alloc: Alloc<'s>) -> Result<Option<&'s [u8]>, ServeError>;
}

/// Why a strategy could not serve a directory's records: the store's failure, a record the
/// in-place reader cannot borrow where it lies, or the records'.
#[derive(Debug)]
pub(crate) enum ServeError {
    Store(CliError),
    /// The store holds the record under `key` but not in place (this command's arena, or not
    /// at a multiple of sixteen): refused, never copied (engine_records.rs).
    NotInPlace { key: ContentId, why: blaze_cas::plain::NotInPlace },
    Texts(ExpansionTextsError),
}

impl From<ExpansionRecordsError> for ServeError {
    fn from(error: ExpansionRecordsError) -> ServeError {
        ServeError::Texts(ExpansionTextsError::Records(error))
    }
}

/// How each row of a directory is given its record: the one seam of the readers' strategies
/// (rule 78).
pub(crate) trait Fetch {
    /// Its name, as a refusal says it.
    const NAME: &'static str;
    /// Each row of `directory`'s record, in row order (`None`: the store holds none), read
    /// through `source` into `alloc`.
    fn serve<'s>(directory: &Directory<'s>, source: &mut dyn RecordSource<'s>, alloc: Alloc<'s>) -> Result<&'s [Option<ExpansionRecord<'s>>], ServeError>;
}

/// Every record fetched at the open, each distinct key probed once.
pub(crate) enum Eager {}

/// Each record fetched at its first read: not built ([`ON_DEMAND_PIECE`]).
pub(crate) enum OnDemand {}

impl Fetch for Eager {
    const NAME: &'static str = "eager";

    fn serve<'s>(directory: &Directory<'s>, source: &mut dyn RecordSource<'s>, alloc: Alloc<'s>) -> Result<&'s [Option<ExpansionRecord<'s>>], ServeError> {
        let rows = directory.len();
        // Each row by its record's key, so equal keys are probed and copied once; the rows are
        // at most a u32's count (the directory's head).
        let mut order: AVec<'_, (ContentId, u32)> = AVec::with_capacity_in(rows, alloc);
        for at in 0..rows {
            let entry = directory.entry_at(at)?;
            order.push((entry.record, at as u32));
        }
        order.sort_unstable();
        let mut served: AVec<'s, Option<ExpansionRecord<'s>>> = AVec::with_capacity_in(rows, alloc);
        served.resize(rows, None);
        let mut last: Option<(ContentId, Option<ExpansionRecord<'s>>)> = None;
        for &(key, at) in order.iter() {
            let record = match last {
                Some((held, record)) if held == key => record,
                _ => {
                    let record = match source.record(key, alloc)? {
                        Some(bytes) => Some(ExpansionRecord::open(bytes)?),
                        None => None,
                    };
                    last = Some((key, record));
                    record
                }
            };
            served[at as usize] = record;
        }
        Ok(served.leak())
    }
}

impl Fetch for OnDemand {
    const NAME: &'static str = "on demand";

    fn serve<'s>(_directory: &Directory<'s>, _source: &mut dyn RecordSource<'s>, _alloc: Alloc<'s>) -> Result<&'s [Option<ExpansionRecord<'s>>], ServeError> {
        Err(ServeError::Texts(ExpansionTextsError::NotBuilt { strategy: Self::NAME, piece: ON_DEMAND_PIECE }))
    }
}

/// A crate's directory read in place, with each row's served record ([`Fetch`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ExpansionTexts<'s> {
    directory: Directory<'s>,
    /// One per directory row, in row order; `None`: the store holds no record under the row's
    /// key.
    served: &'s [Option<ExpansionRecord<'s>>],
}

impl<'s> ExpansionTexts<'s> {
    /// The directory `bytes` (16-aligned, in `alloc`), its records served by `F` through
    /// `source`.
    pub(crate) fn open<F: Fetch>(bytes: &'s [u8], source: &mut dyn RecordSource<'s>, alloc: Alloc<'s>) -> Result<ExpansionTexts<'s>, ServeError> {
        let directory = Directory::open(bytes)?;
        let served = if directory.is_empty() { &[][..] } else { F::serve(&directory, source, alloc)? };
        Ok(ExpansionTexts { directory, served })
    }

    /// How many calls made a text.
    pub(crate) fn len(&self) -> usize {
        self.directory.len()
    }

    /// The call of the directory's row `at` (`at < len()`), in the directory's order (by call
    /// key): what a reader that lists every call of the crate walks (engine_heads.rs
    /// `heads_of_crate`). No record is read.
    pub(crate) fn call_at(&self, at: usize) -> Result<ItemKey, ExpansionRecordsError> {
        Ok(self.directory.entry_at(at)?.call)
    }

    /// The row of `call` and its served record: `None` when the call made no text; refused by
    /// [`ExpansionRecordsError::Missing`] when the store held no record under the row's key.
    fn row(&self, call: ItemKey) -> Result<Option<(Entry<'s>, ExpansionRecord<'s>)>, ExpansionRecordsError> {
        let Some(at) = self.directory.position(call) else { return Ok(None) };
        let entry = self.directory.entry_at(at)?;
        match self.served.get(at).copied().flatten() {
            Some(record) => Ok(Some((entry, record))),
            None => Err(ExpansionRecordsError::Missing { call, key: entry.record }),
        }
    }

    /// The text the call `call` made; `None` when the crate made none for it.
    pub(crate) fn text(&self, call: ItemKey) -> Result<Option<&'s str>, ExpansionRecordsError> {
        match self.row(call)? {
            Some((entry, record)) => entry.text(&record).map(Some),
            None => Ok(None),
        }
    }

    /// Where the call `call` stands and the macro's path (its origins' call part, read from the
    /// directory alone: no record is read). `Ok(None)`: the crate made no text for it;
    /// `Ok(Some(None))`: its text keeps no origins.
    pub(crate) fn call(&self, call: ItemKey) -> Result<Option<Option<CallView<'s>>>, ExpansionRecordsError> {
        let Some(entry) = self.directory.entry(call)? else { return Ok(None) };
        if entry.call_part.is_empty() {
            return Ok(Some(None));
        }
        CallView::open(entry.call_part).map(|view| Some(Some(view))).map_err(|error| ExpansionRecordsError::Origins { call, error })
    }

    /// The origins of the text `call` made in their joined form (blaze-scope expansion_origins.rs
    /// `OriginsView::open`'s: the call part, then the column), copied into `alloc`: what the
    /// ladder's `Load::expansion_origins` hands its reader. `None` when the crate made no text
    /// for it, `Some(&[])` when the text keeps no origins.
    pub(crate) fn joined_origins<'a>(&self, call: ItemKey, alloc: Alloc<'a>) -> Result<Option<&'a [u8]>, ExpansionRecordsError>
    where
        's: 'a,
    {
        let Some((entry, record)) = self.row(call)? else { return Ok(None) };
        entry.text(&record)?;
        if entry.call_part.is_empty() {
            return Ok(Some(&[][..]));
        }
        let mut joined: AVec<'a, u8> = AVec::with_capacity_in(entry.call_part.len() + record.column.len(), alloc);
        joined.extend_from_slice(entry.call_part);
        joined.extend_from_slice(record.column);
        Ok(Some(&*joined.leak()))
    }
}
