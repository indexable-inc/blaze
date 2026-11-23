//! A run's records sent INLINE, over descriptor 3 (blaze-launch wire/bytes.rs `Down::Bytes`,
//! run schema 6): the parent's [`Folder`] that writes no file. [`RecordsDir`], one file a
//! record under `run/records`, is the tested control and, until run_bytes_tests.rs and
//! blaze-launch-rt received_tests.rs pass on a clean compile, the default: each caller names
//! its [`Delivery`] at its one seam line ([`Chosen::of`]; rule 78: `Lazy`'s `D: Folder`).
//!
//! Why: a record a first call needs is often one the parent made a moment ago, still in its
//! store's arena and in no file yet, so the file was a second copy made to be read once. The
//! bytes go over the channel the program already holds, so no sandbox gains a path.
//!
//! What it keeps, and for how long (rule 51: nothing here outlives the run but store pointers):
//! - `sent`: every id whose bytes went on the wire (or are queued for it) this run. The child
//!   keeps every record it was sent for the whole run (blaze-launch-rt run_mode/received.rs),
//!   so each id is sent ONCE a run, however many publications name it (equal statics, equal
//!   code under two symbols, a datum two start-list groups share).
//! - the queue: the bytes of the answer at hand, copied from the store's entry (or the made
//!   entry), until the one writer ([`Needs`](crate::run_parent::Needs)) has framed them; cleared
//!   at the next [`Folder::begin`]. Its capacity is the largest single answer's.
//! - `codes`: each code record's id and the key of the entry holding it, for a crash's read
//!   (the child's copy died with it).
//!
//! A start-list replay (run_parent.rs `ahead_of`) finds a group's bytes without a front end: a
//! code row by its row's key (its entry's record), a datum (no key in its row) through a store
//! POINTER from its id to the entry that carries it, written the first time the datum is sent
//! ([`Folder::kept`]). Two pointer domains say how the entry carries it: as its own record (a
//! static's entry), or as one of its vtables (a code entry's). A datum or code row whose entry
//! the store does not hold (an unkeyed instance's, never put) is not placed ahead: it is
//! resolved at its first call, as a file the folder does not hold is.

use core::fmt;

use blaze_hash::{ByHash, ByHashSet};
use blaze_ids::ContentId;
use blaze_ladder::code_records::{CodeClass, CodeKey, CodeStore, EntryError, StoreError, entry_of};
use blaze_launch::wire::RecordId;
use blaze_launch::wire::run::{BYTES_RECORD_MOST, Symbol};
use blaze_own::{AVec, Alloc, Vec};

use crate::run_parent::{Folder, Front, ParentError, RecordsDir};

/// The pointer domain of a datum that is its entry's own record (a static's datum).
const RECORD_OF: &str = "blaze-cli run record by id: the entry's own record 1";
/// The pointer domain of a datum that is one of its entry's vtables (code_records.rs form 6).
const VTABLE_OF: &str = "blaze-cli run record by id: a vtable the entry carries 1";

/// The store key of the pointer from datum `record` (its BLAKE3) to an entry carrying it, in
/// `domain`. The id names the bytes exactly, so any entry it points to carries equal bytes
/// (rule 21); nothing of the machine is in it (rule 25).
fn pointer(domain: &'static str, record: &[u8; 32]) -> CodeKey {
    CodeKey(ContentId::of(domain, record))
}

/// Where a record's bytes are kept past this run, told to [`Folder::kept`] after it was filed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kept {
    /// A code record: the record of the entry under `key` (its start-list row's key).
    Code(CodeKey),
    /// A static's datum: the own record of the entry stored under `under`.
    Static { under: CodeKey },
    /// A vtable's datum: carried by the code entry stored under `under`.
    Vtable { under: CodeKey },
    /// Only in hand: an unkeyed entry's, never put; a replay resolves it again.
    Unkept,
}

/// The records an answer (or a start-list group) sends inline, in the order queued, each
/// whole: what the one writer frames as `Bytes` before the frames that name them. Every
/// record in it is 1 byte to [`BYTES_RECORD_MOST`] long ([`RunBytes`] refuses any other by
/// name before queueing it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Sent<'s> {
    /// Each record's id and the end of its bytes in `bytes`.
    records: &'s [([u8; 32], usize)],
    bytes: &'s [u8],
}

impl Sent<'static> {
    /// Nothing sent inline: the files control's, and every model resolver's.
    pub(crate) const NONE: Sent<'static> = Sent { records: &[], bytes: &[] };
}

impl<'s> Sent<'s> {
    /// Each record, its id and its bytes, in the order queued.
    pub(crate) fn each(self) -> impl Iterator<Item = (RecordId, &'s [u8])> + 's {
        let mut start = 0;
        self.records.iter().map(move |&(id, end)| {
            let bytes = &self.bytes[start..end];
            start = end;
            (RecordId(id), bytes)
        })
    }
}

/// Why a record could not be sent inline, or found again for a replay or a crash's read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SentWhy {
    /// It has no bytes: a part is never empty (wire/bytes.rs), so it cannot be sent.
    Empty,
    /// It is longer than `BYTES_RECORD_MOST` (1 GiB), which the child refuses before it
    /// allocates.
    TooLong { len: usize },
    /// The parent's allocator refused room for its `len` bytes.
    NoRoom { len: usize },
    /// The store refused the pointer from its id, or a read through it or its row's key.
    Store(StoreError),
    /// The entry its id or key names does not read.
    Entry(EntryError),
    /// A crash's read of a code record this run did not send, or sent from an unkeyed entry
    /// (never put): the store holds no entry of it.
    NotKept,
}

impl fmt::Display for SentWhy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SentWhy::Empty => write!(f, "it has no bytes, and a `Bytes` part is never empty"),
            SentWhy::TooLong { len } => write!(f, "it is {len} bytes, longer than BYTES_RECORD_MOST ({BYTES_RECORD_MOST}), which the program refuses"),
            SentWhy::NoRoom { len } => write!(f, "the parent's allocator refused room for its {len} bytes"),
            SentWhy::Store(why) => write!(f, "{why}"),
            SentWhy::Entry(why) => write!(f, "the entry that holds it does not read: {why}"),
            SentWhy::NotKept => write!(f, "this run did not send it from an entry the store holds (an unkeyed instance's is never put), so its bytes are not here to read"),
        }
    }
}

/// How a run's records reach the program: what a caller names at its seam line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Delivery {
    /// One file a record under the home's `run/records` ([`RecordsDir`]), kept as the tested
    /// control behind the seam (rule 78).
    #[cfg_attr(not(test), expect(dead_code, reason = "kept as the tested control in run_bytes_tests.rs (rule 78); live callers use Delivery::Inline"))]
    Files,
    /// Inline over descriptor 3 ([`RunBytes`]): sends each record once per run, writes no file.
    Inline,
}

/// The folder a caller's [`Delivery`] picks: both strategies behind the one `D: Folder`
/// seam, so the choice is one word at each caller.
pub(crate) enum Chosen<'r, 'a> {
    Files(RecordsDir<'r>),
    Inline(RunBytes<'a>),
}

impl<'r, 'a> Chosen<'r, 'a> {
    /// `delivery`'s folder: `files` (the records folder, opened by the caller either way, as
    /// the launcher's `BLAZE_LAUNCH_STORE` holds the program's native library), or the inline
    /// folder in `alloc`.
    pub(crate) fn of(delivery: Delivery, files: RecordsDir<'r>, alloc: Alloc<'a>) -> Chosen<'r, 'a> {
        match delivery {
            Delivery::Files => Chosen::Files(files),
            Delivery::Inline => Chosen::Inline(RunBytes::new(alloc)),
        }
    }
}

impl Folder for Chosen<'_, '_> {
    fn begin(&mut self) {
        match self {
            Chosen::Files(files) => files.begin(),
            Chosen::Inline(inline) => inline.begin(),
        }
    }

    fn file(&mut self, record: &[u8]) -> Result<RecordId, ParentError> {
        match self {
            Chosen::Files(files) => files.file(record),
            Chosen::Inline(inline) => inline.file(record),
        }
    }

    fn kept(&mut self, record: RecordId, kept: Kept, store: &mut dyn CodeStore) -> Result<(), ParentError> {
        match self {
            Chosen::Files(files) => files.kept(record, kept, store),
            Chosen::Inline(inline) => inline.kept(record, kept, store),
        }
    }

    fn ahead<F: Front>(&mut self, symbol: Symbol, record: &RecordId, code: Option<CodeKey>, front: &F, store: &dyn CodeStore) -> Result<bool, ParentError> {
        match self {
            Chosen::Files(files) => files.ahead(symbol, record, code, front, store),
            Chosen::Inline(inline) => inline.ahead(symbol, record, code, front, store),
        }
    }

    fn take(&mut self) -> Sent<'_> {
        match self {
            Chosen::Files(files) => files.take(),
            Chosen::Inline(inline) => inline.take(),
        }
    }

    fn read<F: Front>(&self, record: &RecordId, front: &F, store: &dyn CodeStore, into: &mut bstd::mem::Bytes<'_>) -> Result<(), ParentError> {
        match self {
            Chosen::Files(files) => files.read(record, front, store, into),
            Chosen::Inline(inline) => inline.read(record, front, store, into),
        }
    }
}

/// THE INLINE FOLDER (the module's doc), in the command's allocator: nothing it holds outlives
/// the run.
pub(crate) struct RunBytes<'a> {
    sent: ByHashSet<[u8; 32], Alloc<'a>>,
    /// Queued since the last [`Folder::begin`]: each id and the end of its bytes in `bytes`.
    queued: AVec<'a, ([u8; 32], usize)>,
    bytes: AVec<'a, u8>,
    /// How many of `queued` the one writer took: the rest are dropped as not sent at `begin`.
    taken: usize,
    codes: ByHash<[u8; 32], CodeKey, Alloc<'a>>,
}

impl<'a> RunBytes<'a> {
    pub(crate) fn new(alloc: Alloc<'a>) -> RunBytes<'a> {
        RunBytes { sent: ByHashSet::new_in(alloc), queued: Vec::new_in(alloc), bytes: Vec::new_in(alloc), taken: 0, codes: ByHash::new_in(alloc) }
    }

    /// `bytes` queued as record `id`'s, once a run; a record no part can carry is refused by
    /// name before anything is queued (rule 3).
    fn queue(&mut self, id: [u8; 32], bytes: &[u8]) -> Result<(), ParentError> {
        if self.sent.contains(&id) {
            return Ok(());
        }
        let refused = |why| ParentError::Sent { record: id, why };
        if bytes.is_empty() {
            return Err(refused(SentWhy::Empty));
        }
        if bytes.len() as u64 > BYTES_RECORD_MOST {
            return Err(refused(SentWhy::TooLong { len: bytes.len() }));
        }
        self.bytes.try_reserve(bytes.len()).map_err(|_| refused(SentWhy::NoRoom { len: bytes.len() }))?;
        self.queued.try_reserve(1).map_err(|_| refused(SentWhy::NoRoom { len: bytes.len() }))?;
        self.bytes.extend_from_slice(bytes);
        self.queued.push((id, self.bytes.len()));
        self.sent.insert(id);
        Ok(())
    }
}

impl Folder for RunBytes<'_> {
    fn begin(&mut self) {
        for (id, _) in &self.queued[self.taken..] {
            self.sent.remove(id);
        }
        self.queued.clear();
        self.bytes.clear();
        self.taken = 0;
    }

    fn file(&mut self, record: &[u8]) -> Result<RecordId, ParentError> {
        let id = blaze_blake3::hash(record);
        self.queue(id, record)?;
        Ok(RecordId(id))
    }

    fn kept(&mut self, record: RecordId, kept: Kept, store: &mut dyn CodeStore) -> Result<(), ParentError> {
        let (domain, under) = match kept {
            Kept::Code(key) => {
                self.codes.insert(record.0, key);
                return Ok(());
            }
            Kept::Static { under } => (RECORD_OF, under),
            Kept::Vtable { under } => (VTABLE_OF, under),
            Kept::Unkept => return Ok(()),
        };
        let refused = |why| ParentError::Sent { record: record.0, why: SentWhy::Store(why) };
        let key = pointer(domain, &record.0);
        // Any entry carrying the bytes will do (the id names them), so one pointed already is
        // kept; one whose target the store let go reads as none and is pointed anew. The class
        // is the program's: the pointer is kept by the root of the run that read it (a
        // toolchain entry's pointer is pointed again by a later program's first send).
        if store.pointed(key).map_err(refused)?.is_none() {
            store.point(CodeClass::Program, key, under).map_err(refused)?;
        }
        Ok(())
    }

    fn ahead<F: Front>(&mut self, symbol: Symbol, record: &RecordId, code: Option<CodeKey>, front: &F, store: &dyn CodeStore) -> Result<bool, ParentError> {
        if self.sent.contains(&record.0) {
            return Ok(true);
        }
        let stored = |why| ParentError::Sent { record: record.0, why: SentWhy::Store(why) };
        let unread = |why| ParentError::Sent { record: record.0, why: SentWhy::Entry(why) };
        if let Some(key) = code {
            // A row's key is its entry's content key, whose entry never changes: its record
            // is the row's. A key that is a pointer (a special's code key) may name another
            // entry since the list was written: its record is checked to be the row's, and a
            // moved one is resolved at its first call.
            let moves = store.pointed(key).map_err(stored)?.is_some();
            let Some(entry) = entry_of(store, key).map_err(stored)? else { return Ok(false) };
            let bytes = front.record_of(entry).map_err(unread)?;
            if moves && blaze_blake3::hash(bytes) != record.0 {
                return Ok(false);
            }
            self.codes.insert(record.0, key);
            self.queue(record.0, bytes)?;
            return Ok(true);
        }
        // A static's datum: the pointed entry's own record, whose bytes the id names.
        if let Some(under) = store.pointed(pointer(RECORD_OF, &record.0)).map_err(stored)?
            && let Some(entry) = store.get(under).map_err(stored)?
        {
            let bytes = front.record_of(entry).map_err(unread)?;
            self.queue(record.0, bytes)?;
            return Ok(true);
        }
        // A vtable's datum: the one of the pointed entry's vtables whose BLAKE3 is the id (a
        // datum is small; the search hashes only them).
        if let Some(under) = store.pointed(pointer(VTABLE_OF, &record.0)).map_err(stored)?
            && let Some(entry) = store.get(under).map_err(stored)?
        {
            let mut found = false;
            front.each_vtable(symbol.0, entry, &mut |_, datum| {
                if !found && blaze_blake3::hash(datum) == record.0 {
                    self.queue(record.0, datum)?;
                    found = true;
                }
                Ok(())
            })?;
            return Ok(found);
        }
        Ok(false)
    }

    fn take(&mut self) -> Sent<'_> {
        self.taken = self.queued.len();
        Sent { records: self.queued.as_slice(), bytes: self.bytes.as_slice() }
    }

    fn read<F: Front>(&self, record: &RecordId, front: &F, store: &dyn CodeStore, into: &mut bstd::mem::Bytes<'_>) -> Result<(), ParentError> {
        let refused = |why| ParentError::Sent { record: record.0, why };
        let Some(&key) = self.codes.get(&record.0) else { return Err(refused(SentWhy::NotKept)) };
        let Some(entry) = entry_of(store, key).map_err(|why| refused(SentWhy::Store(why)))? else { return Err(refused(SentWhy::NotKept)) };
        let bytes = front.record_of(entry).map_err(|why| refused(SentWhy::Entry(why)))?;
        into.try_extend(bytes).map_err(|_| refused(SentWhy::NoRoom { len: bytes.len() }))
    }
}
