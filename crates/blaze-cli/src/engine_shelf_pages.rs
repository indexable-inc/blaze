//! THE READY EDIT'S IMPL TABLES (D187): one crate's at a time, on a body's first probe of that
//! crate. The phase path's `engine_heads::complete` reads every crate's Heads record onto the
//! shelf before the bodies run, one row and one record per crate of the checkout (tempo has
//! 1,206). On the ready path no Heads task enters (engine_ready_edit.rs `admit`), so each crate's
//! stored row is today's. The record a body's probe needs is found from that row the first time
//! any worker asks for that crate, read on the row's own page (the ready path's header index is
//! absent, engine_check.rs `checking`). A crate no body probes is never read.
//!
//! One answer per crate per request: what the first ask found is kept in a shared map, so every
//! worker's reader ([`TableSet::paged`]) sees the same bytes. [`complete`] gives the same
//! answers: a crate with no row, or whose task failed, has no tables. Three things are kept as
//! the request's first fault ([`StoredShelf::fault`]), checked after the drive (rule 3), the same
//! stop `complete` makes up front: a row with no record key, a record missing from the snapshot,
//! and bytes that are not the framed form. Bytes are handed to the reader only once
//! [`check_framed`] has passed them: the reader's contract (blaze-ladder records.rs `paged_of`).
//!
//! [`complete`]: crate::engine_heads::complete
//! [`TableSet::paged`]: blaze_ladder::front2::buckets::TableSet::paged

use core::sync::atomic::{AtomicU32, Ordering};

use blaze_drive::record::Stored as Record;
use blaze_engine::StoredPages;
use blaze_id::CrateItemKey;
use blaze_ladder::front2::buckets::{ShelfError, ShelfSource, check_framed, impl_tables_name};
use blaze_own::SharedAlloc;

use crate::engine_answer::Stored;

/// What the first ask of a crate found. It holds the record's KEY, not its bytes: the shared
/// map's values are `'static` (blaze-hash shared.rs), and the bytes borrow the snapshot; a later
/// ask reads the same record in place again by this key ([`Stored::record_bytes`]: a lookup in
/// the mapped snapshot, no copy) and skips the row's page and the frame check.
#[derive(Clone, Copy)]
enum Found {
    /// The crate's framed tables are the payload of this record, checked once.
    Tables(blaze_drive::Key),
    /// No tables: no row, its task failed, or a fault ([`StoredShelf::fault`]).
    None,
}

/// The ready path's [`ShelfSource`]: each crate's Heads record found from its stored row, once.
pub(crate) struct StoredShelf<'p, 'a> {
    stored: &'p Stored<'a>,
    found: blaze_core::SharedMap<CrateItemKey, Found, SharedAlloc<'static>>,
    /// The crates whose tables were read (the profile's count beside `complete`'s).
    read: AtomicU32,
    fault: std::sync::OnceLock<&'static str>,
}

impl<'p, 'a: 'p> StoredShelf<'p, 'a> {
    pub(crate) fn new(stored: &'p Stored<'a>, heap: &'static blaze_own::Heap) -> StoredShelf<'p, 'a> {
        StoredShelf {
            stored,
            found: blaze_core::SharedMap::new_in("engine.impl tables found on the stored answer's pages", heap.shared()),
            read: AtomicU32::new(0),
            fault: std::sync::OnceLock::new(),
        }
    }

    /// The first crate whose tables could not be read, for the request's error after its drive.
    pub(crate) fn fault(&self) -> Option<&'static str> {
        self.fault.get().copied()
    }

    /// How many crates' tables were read.
    pub(crate) fn read(&self) -> u32 {
        self.read.load(Ordering::Relaxed)
    }

    fn faulted(&self, what: &'static str) -> Found {
        let _first_kept = self.fault.set(what);
        Found::None
    }

    /// The payload of the record under `key`, in place in the snapshot; a record that cannot be
    /// read is the request's fault ([`StoredShelf::fault`]) and no tables.
    fn payload(&self, key: blaze_drive::Key) -> Option<&'a [u8]> {
        let bytes: &'a [u8] = match self.stored.record_bytes(key) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                let _none = self.faulted("a crate's impl tables record the answer names is not in the store's snapshot (engine_shelf_pages.rs `StoredShelf::payload`)");
                return None;
            }
            Err(_) => {
                let _none = self.faulted("the store's snapshot could not read a crate's impl tables record (engine_shelf_pages.rs `StoredShelf::payload`)");
                return None;
            }
        };
        match blaze_drive::record::open(bytes) {
            Ok(Record::Whole(view)) => Some(view.payload),
            Ok(Record::Shape(_)) | Err(_) => {
                let _none = self.faulted("a crate's impl tables record does not read as a whole record (engine_shelf_pages.rs `StoredShelf::payload`)");
                None
            }
        }
    }

    /// `krate`'s tables, as [`crate::engine_heads::complete`] would put them on the shelf: the
    /// record's key, once its payload has passed the frame check.
    fn find(&self, krate: CrateItemKey) -> Found {
        let name = impl_tables_name(krate);
        let header = match StoredPages::header(self.stored, name) {
            Ok(Some(header)) => header,
            Ok(None) => return Found::None,
            // The stored answer keeps it as its `refusal` too: the request's error after the drive.
            Err(_) => return self.faulted("a crate's impl tables row page could not be read from the stored answer (engine_shelf_pages.rs `StoredShelf::find`)"),
        };
        if header.facts.failed {
            return Found::None;
        }
        let Some(key) = header.record else {
            return self.faulted("a standing `ImplBucket` row has no record key (pages.rs `records`; engine_shelf_pages.rs `StoredShelf::find`)");
        };
        let Some(payload) = self.payload(key) else { return Found::None };
        match check_framed(krate, payload) {
            Ok(()) => {}
            Err(ShelfError::Unaligned { .. }) => {
                return self.faulted("a standing crate's impl tables lie at an address their records cannot be read in place at: the store handed a payload off `PAYLOAD_ALIGN` (blaze-drive record.rs; engine_shelf_pages.rs `StoredShelf::find`)");
            }
            Err(ShelfError::Malformed { .. } | ShelfError::Twice(_)) => {
                return self.faulted("a standing crate's impl tables read back malformed from the store (engine_shelf_pages.rs `StoredShelf::find`)");
            }
        }
        self.read.fetch_add(1, Ordering::Relaxed);
        Found::Tables(key)
    }
}

impl<'p, 'a: 'p> ShelfSource for StoredShelf<'p, 'a> {
    fn framed(&self, krate: CrateItemKey) -> Option<&[u8]> {
        let found = match self.found.pin().get(&krate).copied() {
            Some(found) => found,
            None => {
                let found = self.find(krate);
                match self.found.pin().try_insert(krate, found) {
                    Ok(_) => found,
                    Err(occupied) => *occupied.current,
                }
            }
        };
        match found {
            // The same record every ask: the store is content-keyed, and the frame check passed
            // on these bytes at the first ask.
            Found::Tables(key) => self.payload(key),
            Found::None => None,
        }
    }
}
