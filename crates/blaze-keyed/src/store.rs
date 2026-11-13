//! The store's interface: what the driver, the front end and the code lanes call, whatever the
//! physical form underneath (crates/blaze-cas builds it).
//!
//! The form it promises (target.md, "On disk"):
//!
//! - Immutable segments and a generation manifest. A reader maps a generation once per process
//!   and takes no lock; segments are never unmapped while the process runs (an unmap is a TLB
//!   shootdown on every core).
//! - Each process writes its own segment. [`Store::commit`] flips the generation manifest: the
//!   only lock, held for the flip alone.
//! - No sync on the edit path: the store is a cache that can be rebuilt. Integrity is a checksum
//!   per record and the generation flip; a torn tail is dropped and reported by name
//!   ([`StoreError::TornTail`]), never read as empty.
//! - Collection is compaction into new segments then unlinking the old ones, under a byte cap,
//!   at utility QoS, never stopping a build: a reader that mapped an old segment keeps it.
//! - Hot segments are raw for mapping; only compaction compresses cold ones.
//! - A miss may be answered from a shared cache through [`Remote`]: the fetch-on-miss seam.

#![blaze::sans_io]

use core::{alloc::Allocator, fmt, ops::Deref};

use blaze_ids::ContentId;

use crate::kind::{Key, Kind, Tag};

/// What a put did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Put {
    Written,
    /// The same bytes were already filed under the key.
    Same,
    /// Other bytes are filed under the key: two values under one key, a key that misses an
    /// input (rule 21). The caller refuses with the kind and key; nothing is overwritten.
    Other,
}

/// One root: what pruning keeps, with everything the root's record names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RootRef {
    pub tag: Tag,
    pub key: ContentId,
}

/// A generation of the store: the manifest's number after a flip.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Generation(pub u64);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Compacted {
    pub kept: u64,
    pub kept_bytes: u64,
    pub freed: u64,
    pub freed_bytes: u64,
}

/// A failure of the store, by name.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StoreError {
    /// The segment `segment` ends in a record cut short at byte `at`: dropped, and said.
    TornTail { segment: u64, at: u64 },
    /// The record of `tag`, `key` fails its checksum.
    Checksum { tag: Tag, key: ContentId },
    /// An OS call failed: which, on what, and the error number.
    Os { call: &'static str, what: &'static str, errno: i32 },
    /// The remote cache refused or failed: what, with its status.
    Remote { what: &'static str, status: u32 },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::TornTail { segment, at } => {
                write!(f, "segment {segment} ends in a torn record at byte {at}; dropped")
            }
            StoreError::Checksum { tag, key } => {
                write!(f, "the record of kind {} key {key:?} fails its checksum", tag.0)
            }
            StoreError::Os { call, what, errno } => write!(f, "{call} on {what} failed: errno {errno}"),
            StoreError::Remote { what, status } => write!(f, "the remote cache: {what} ({status})"),
        }
    }
}

/// The store, as one process sees it: the generation it mapped at start, plus its own segment.
///
/// The invariants:
/// - A key's bytes never change: [`Store::put`] of other bytes under a filed key is
///   [`Put::Other`], and nothing is overwritten.
/// - What [`Store::read`] and [`Store::map`] return is 16-aligned, checksummed, and stays valid
///   for the borrow: a mapped segment is not unmapped while the process runs.
/// - A record put by this process is readable by it at once, and by other processes after
///   [`Store::commit`].
pub trait Store {
    /// A small record read into the asking task's frame.
    type Held<A: Allocator>: Deref<Target = [u8]>;

    /// The record of kind `K` under `key`, read by `pread` into `frame`; `None` when no
    /// generation holds it and the remote has none.
    fn read<K: Kind, A: Allocator>(
        &self,
        key: &Key<K>,
        frame: A,
    ) -> Result<Option<Self::Held<A>>, StoreError>;

    /// The record of kind `K` under `key`, in place in its mapped segment: for
    /// [`Layout::Plain`](crate::kind::Layout::Plain) kinds.
    fn map<K: Kind>(&self, key: &Key<K>) -> Result<Option<&[u8]>, StoreError>;

    /// Files `bytes` under `key` in this process's segment. No sync.
    fn put<K: Kind>(&self, key: &Key<K>, bytes: &[u8]) -> Result<Put, StoreError>;

    /// Publishes this process's segment: the generation flip.
    fn commit(&self) -> Result<Generation, StoreError>;

    /// Compacts into new segments what `roots` reach, under `cap` bytes, and unlinks the rest;
    /// run beside builds, never blocking a reader.
    fn compact(&self, roots: &[RootRef], cap: u64) -> Result<Compacted, StoreError>;
}

/// The fetch-on-miss seam: a shared cache records are fetched from by key (CI writes, people
/// read; std's records ship with the toolchain). No upload; no credentials assumed.
///
/// The invariant: a fetched record is checked as one read from disk is (checksum, then
/// `record::open` against the key) before anyone sees it.
pub trait Remote {
    fn fetch<A: Allocator>(
        &self,
        tag: Tag,
        key: &ContentId,
        frame: A,
    ) -> Result<Option<Vec<u8, A>>, StoreError>;
}

/// No shared cache: every miss is a miss.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoRemote;

impl Remote for NoRemote {
    fn fetch<A: Allocator>(
        &self,
        _tag: Tag,
        _key: &ContentId,
        _frame: A,
    ) -> Result<Option<Vec<u8, A>>, StoreError> {
        Ok(None)
    }
}
