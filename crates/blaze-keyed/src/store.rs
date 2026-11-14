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
//! - A miss may be answered from a shared cache through [`Source`], each fetched record checked
//!   by a [`Verifier`] first: the fetch-on-miss seam.
//! - Determinism is a protocol requirement: two builders of one key write byte-identical
//!   values (rule 25); [`Put::Other`] is the check locally, and CI's push refuses a key whose
//!   value differs from the one already in the bucket.

#![blaze::sans_io]

use core::{alloc::Allocator, fmt, ops::{Deref, Range}};

use blaze_ids::ContentId;

use crate::{
    exec::RecordRef,
    kind::{Key, Kind, Namespace, Tag},
};

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

/// What the store's proof that a segment's bytes were checked must be (blaze-cas's
/// `Verified<Segment>` meets it). Bytes are verified once, not on every read (decision 18):
///
/// - Made only by the store's one check of a segment: a segment this process wrote is trusted
///   as written; a segment of an earlier boot is checked in full once (its digest over every
///   byte, its layout) and the result recorded in the generation manifest; a fetched segment's
///   statement ([`Verifier`]) and digest are checked once at fetch and recorded.
/// - Branded with what it is valid for: the generation it was checked in, and the segment.
/// - At most one word beyond the segment's id; no pointer, no count.
///
/// A read of a record in a proved segment checks nothing: its checksum and layout were covered
/// by the segment's check.
pub trait SegmentProof: Copy {
    fn segment(&self) -> SegmentId;
    fn generation(&self) -> Generation;
}

/// The proof of a store that holds its bytes only in this process (a simulation's): everything
/// in it was written by this process, so it is trusted as written. Not for a store on disk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InProcess;

impl SegmentProof for InProcess {
    fn segment(&self) -> SegmentId {
        SegmentId(ContentId([0; 32]))
    }

    fn generation(&self) -> Generation {
        Generation(0)
    }
}

/// A record as read: its bytes and the proof of the segment they are in.
#[derive(Clone, Copy, Debug)]
pub struct Read<V, H> {
    pub bytes: H,
    pub from: V,
}

/// The store, as one process sees it: the generation it mapped at start, plus its own segment.
///
/// The invariants:
/// - A key's bytes never change: [`Store::put`] of other bytes under a filed key is
///   [`Put::Other`], and nothing is overwritten. A put of bytes the store holds writes nothing.
/// - What [`Store::read`] and [`Store::map`] return is 16-aligned, in a segment whose proof is
///   returned with it, and stays valid for the borrow: a mapped segment is not unmapped while
///   the process runs. Neither checks anything per record.
/// - A record put by this process is readable by it at once, and by other processes after
///   [`Store::commit`].
pub trait Store {
    /// A small record read into the asking task's frame.
    type Held<A: Allocator>: Deref<Target = [u8]>;
    /// The proof that a segment's bytes were checked once.
    type Verified: SegmentProof;

    /// The record of kind `K` under `key`, read by `pread` into `frame`; `None` when no
    /// generation holds it and the remote has none.
    fn read<K: Kind, A: Allocator>(
        &self,
        key: &Key<K>,
        frame: A,
    ) -> Result<Option<Read<Self::Verified, Self::Held<A>>>, StoreError>;

    /// The record of kind `K` under `key`, in place in its mapped segment: for
    /// [`Layout::Plain`](crate::kind::Layout::Plain) kinds.
    fn map<K: Kind>(&self, key: &Key<K>) -> Result<Option<Read<Self::Verified, &[u8]>>, StoreError>;

    /// Files `bytes` under `key` in this process's segment. No sync.
    fn put<K: Kind>(&self, key: &Key<K>, bytes: &[u8]) -> Result<Put, StoreError>;

    /// Publishes this process's segment: the generation flip.
    fn commit(&self) -> Result<Generation, StoreError>;

    /// Compacts into new segments what `roots` reach, under `cap` bytes, and unlinks the rest;
    /// run beside builds, never blocking a reader.
    fn compact(&self, roots: &[RootRef], cap: u64) -> Result<Compacted, StoreError>;
}

/// A segment of a store, local or in the shared bucket: the wire format, unchanged (a segment
/// file is an object; a pulled one is mapped like a local one).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SegmentId(pub ContentId);

/// Where a record is: a byte range of a segment, and the signed statement that vouches for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Located {
    pub segment: SegmentId,
    pub at: u64,
    pub len: u32,
    pub statement: Statement,
}

/// A trusted builder's signed binding of a key to its value's digest. A key names inputs, not
/// the value, so a fetched value is trusted only through one of these.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Statement {
    pub namespace: Namespace,
    pub tag: Tag,
    pub key: ContentId,
    pub digest: ContentId,
    pub signer: ContentId,
    pub signature: [u8; 64],
}

/// The fetch-on-miss seam: a shared cache (CI writes everything it builds; anyone reads; std's
/// and blaze-std's records ship as bundles). No upload here; no credentials assumed.
pub trait Source {
    /// Which of `keys` the source holds, in one round trip per index shard: `found[i]` for
    /// `keys[i]`. A negative answer is cached by the caller.
    fn find(
        &self,
        namespace: Namespace,
        keys: &[RecordRef],
        found: &mut Vec<Option<Located>>,
    ) -> Result<(), StoreError>;

    /// A ranged read of a segment, into `frame`.
    fn fetch<A: Allocator>(
        &self,
        segment: SegmentId,
        range: Range<u64>,
        frame: A,
    ) -> Result<Vec<u8, A>, StoreError>;
}

/// Why fetched bytes are refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// The signer is not a builder trusted for the statement's namespace.
    Signer { signer: ContentId },
    BadSignature,
    /// The bytes' digest is not the one the statement signs.
    Digest { found: ContentId, signed: ContentId },
    /// More bytes than the kind allows.
    TooLarge { len: u64, most: u64 },
}

/// Checks a statement and the bytes it vouches for before anything maps them: once per fetched
/// segment, the store recording the result as its [`SegmentProof`].
///
/// The invariant: nothing fetched is read, mapped or filed unless `verify` accepted it: the
/// signer is trusted for the namespace, the signature holds, the bytes' digest is the signed
/// one, and their size is within the kind's limit.
pub trait Verifier {
    fn verify(&self, statement: &Statement, bytes: &[u8]) -> Result<(), Refused>;
}

/// No shared cache: every find finds nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoSource;

impl Source for NoSource {
    fn find(
        &self,
        _namespace: Namespace,
        keys: &[RecordRef],
        found: &mut Vec<Option<Located>>,
    ) -> Result<(), StoreError> {
        found.clear();
        found.resize(keys.len(), None);
        Ok(())
    }

    fn fetch<A: Allocator>(
        &self,
        _segment: SegmentId,
        _range: Range<u64>,
        _frame: A,
    ) -> Result<Vec<u8, A>, StoreError> {
        Err(StoreError::Remote { what: "a fetch from no shared cache: find found nothing", status: 0 })
    }
}
