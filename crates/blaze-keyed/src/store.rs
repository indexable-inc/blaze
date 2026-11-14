//! The store's interface and the one home of its vocabulary: what the driver, the front end and
//! the code lanes call, what the plain store (crates/blaze-cas) implements, and what the shared
//! cache (crates/blaze-remote) puts on the wire, whatever the physical form underneath.
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
//! - A miss may be answered from a shared cache through [`Source`], segment by segment: each
//!   fetched segment's [`Statement`] is checked once by a [`Verifier`], and its bytes' digest
//!   once by the store, before anything reads it: the fetch-on-miss seam.
//! - Every record is filed in a [`Namespace`] ([`Store::put`]), which the driver computes once per
//!   package ([`crate::kind::namespace_of`]): on a machine that is not a trusted writer, always
//!   [`Namespace::LOCAL`], so a fully local private build never names a shared namespace.
//! - Determinism is a protocol requirement: two builders of one key write byte-identical
//!   values (rule 25); [`Put::Other`] is the check locally, and CI's push refuses a key whose
//!   value differs from the one already in the bucket.
//!
//! The segment's geometry ([`HEADER`], [`FOOTER`], [`ENTRY`], [`ALIGN`], [`SEGMENT_MOST`]), the
//! reference to a segment the bucket's objects carry ([`SegmentRef`]) and the statement that
//! vouches for one ([`Statement`]) live here once; the plain store's reader and writer and the
//! remote's objects use these, and declare none of their own (rule 30).

#![blaze::sans_io]

use core::{alloc::Allocator, fmt, ops::{Deref, Range}};

use blaze_ids::ContentId;

use crate::{
    exec::RecordRef,
    kind::{Key, Kind, Namespace, Tag, Tier},
};

/// A segment's header: its magic, format, namespace and identity, in bytes.
pub const HEADER: usize = 128;
/// A segment's footer: its index's place and count, and its digest, in bytes.
pub const FOOTER: usize = 48;
/// One entry of a segment's index: key 32, offset u64, length u32, kind and checksum, in bytes.
pub const ENTRY: usize = 48;
/// What every payload and the index start on: the flat format's alignment, so a record read in
/// place is a record [`blaze_format::open`] takes.
pub const ALIGN: u64 = blaze_format::ALIGN as u64;
/// The most a segment can be, in bytes: what a fetch reads into memory at most, refused before
/// a byte is fetched.
pub const SEGMENT_MOST: u64 = 4 << 30;

const _: () = assert!(HEADER as u64 % ALIGN == 0 && ENTRY as u64 % ALIGN == 0 && FOOTER as u64 % ALIGN == 0);

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

/// A segment, local or in the shared bucket, by the digest over every byte of it: the name a
/// statement signs, a bucket object is filed under and a fetch is checked against. A pulled
/// segment is mapped like a local one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SegmentId(pub ContentId);

/// A segment's length: at least a header and a footer, at most [`SEGMENT_MOST`]. The one bound
/// a statement, a bundle, an outcome and an index entry share. Made only by [`SegmentLen::new`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SegmentLen(u64);

impl SegmentLen {
    /// The least a segment can be: its header and its footer.
    pub const LEAST: u64 = (HEADER + FOOTER) as u64;
    /// The most a segment can be.
    pub const MOST: u64 = SEGMENT_MOST;

    /// The length, or [`WireError::SegmentLen`] when it is outside `LEAST..=MOST`.
    pub const fn new(len: u64) -> Result<SegmentLen, WireError> {
        if len < SegmentLen::LEAST || len > SegmentLen::MOST {
            return Err(WireError::SegmentLen { len });
        }
        Ok(SegmentLen(len))
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A segment as the bucket's objects and a [`Located`] name it: its id, its length, where its
/// index is and how many records it holds. Made only by [`SegmentRef::new`], which checks the
/// shape once, so a reference a decoder would refuse cannot be encoded (rule 76).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SegmentRef {
    digest: SegmentId,
    len: SegmentLen,
    index_at: u64,
    count: u32,
}

impl SegmentRef {
    /// The bytes of a reference on the wire.
    pub const ROW: usize = 56;

    /// The reference, or why not: a length out of [`SegmentLen`]'s bounds
    /// ([`WireError::SegmentLen`]), or an index that starts inside the header, is not on
    /// [`ALIGN`] or does not end where the footer starts ([`WireError::Index`]).
    pub const fn new(digest: SegmentId, len: u64, index_at: u64, count: u32) -> Result<SegmentRef, WireError> {
        let len = match SegmentLen::new(len) {
            Ok(len) => len,
            Err(why) => return Err(why),
        };
        let ends = match index_at.checked_add(count as u64 * ENTRY as u64) {
            Some(run) => run.checked_add(FOOTER as u64),
            None => None,
        };
        let whole = match ends {
            Some(end) => end == len.get(),
            None => false,
        };
        if index_at < HEADER as u64 || index_at % ALIGN != 0 || !whole {
            return Err(WireError::Index { len: len.get(), index_at, count });
        }
        Ok(SegmentRef { digest, len, index_at, count })
    }

    pub const fn digest(&self) -> SegmentId {
        self.digest
    }

    pub const fn segment_len(&self) -> SegmentLen {
        self.len
    }

    /// Its length in bytes.
    pub const fn size(&self) -> u64 {
        self.len.get()
    }

    /// Where its index starts, in bytes from the segment's start.
    pub const fn index_at(&self) -> u64 {
        self.index_at
    }

    pub const fn count(&self) -> u32 {
        self.count
    }

    /// Its row: digest 32, length u64, index offset u64, records u32, zero u32.
    pub fn encode(&self) -> [u8; SegmentRef::ROW] {
        let mut row = [0u8; SegmentRef::ROW];
        row[0..32].copy_from_slice(&self.digest.0.0);
        row[32..40].copy_from_slice(&self.len.get().to_le_bytes());
        row[40..48].copy_from_slice(&self.index_at.to_le_bytes());
        row[48..52].copy_from_slice(&self.count.to_le_bytes());
        row
    }

    /// The reference in `row`, exactly [`SegmentRef::ROW`] bytes with its padding zero, checked
    /// as [`SegmentRef::new`] checks.
    pub fn decode(row: &[u8]) -> Result<SegmentRef, WireError> {
        if row.len() != SegmentRef::ROW {
            return Err(WireError::Length { len: row.len(), wanted: SegmentRef::ROW });
        }
        if u32_at(row, 52) != 0 {
            return Err(WireError::Padding { at: 52 });
        }
        SegmentRef::new(SegmentId(id_at(row, 0)), u64_at(row, 32), u64_at(row, 40), u32_at(row, 48))
    }
}

/// Where a record is: a byte range of a segment. What vouches for it is the segment's
/// [`Statement`] ([`Source::statement`]), checked once per segment, not per record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Located {
    pub segment: SegmentRef,
    /// Its first byte, from the segment's start.
    pub at: u64,
    pub len: u32,
}

/// A builder's statement about one whole segment: that the segment of this id and length holds
/// `records` records of `namespace`, built by `toolchain` at `tier`. A key names inputs, not the
/// value, so a fetched segment is trusted only through one of these; its signature covers
/// [`Statement::body`]. Encoding and decoding check the layout only: the signature and the
/// signer are the [`Verifier`]'s.
///
/// Layout, [`Statement::LEN`] bytes: `BLRT`, format u32, namespace u64, segment id 32, length
/// u64, toolchain 32, records u32, tier u8 (its [`Tier`] number), zero 3, issued u64, signer 32
/// ([`Statement::BODY`] bytes signed), signature 64.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Statement {
    pub namespace: Namespace,
    pub segment: SegmentId,
    pub len: SegmentLen,
    /// The digest of the toolchain that built every record of the segment.
    pub toolchain: ContentId,
    pub records: u32,
    pub tier: Tier,
    /// When it was signed, in seconds since the Unix epoch: what a signer's validity window is
    /// checked against.
    pub issued: u64,
    /// The signer's public key.
    pub signer: [u8; 32],
    pub signature: [u8; 64],
}

impl Statement {
    /// Its bytes.
    pub const LEN: usize = 200;
    /// The bytes its signature covers: all but the signature.
    pub const BODY: usize = 136;
    /// Its format.
    pub const FORMAT: u32 = 1;
    const MAGIC: [u8; 4] = *b"BLRT";

    /// The signed part.
    pub fn body(&self) -> [u8; Statement::BODY] {
        let mut out = [0u8; Statement::BODY];
        out[0..4].copy_from_slice(&Statement::MAGIC);
        out[4..8].copy_from_slice(&Statement::FORMAT.to_le_bytes());
        out[8..16].copy_from_slice(&self.namespace.0.to_le_bytes());
        out[16..48].copy_from_slice(&self.segment.0.0);
        out[48..56].copy_from_slice(&self.len.get().to_le_bytes());
        out[56..88].copy_from_slice(&self.toolchain.0);
        out[88..92].copy_from_slice(&self.records.to_le_bytes());
        out[92] = self.tier as u8;
        out[96..104].copy_from_slice(&self.issued.to_le_bytes());
        out[104..136].copy_from_slice(&self.signer);
        out
    }

    /// Its bytes: the body, then the signature.
    pub fn encode(&self) -> [u8; Statement::LEN] {
        let mut out = [0u8; Statement::LEN];
        out[..Statement::BODY].copy_from_slice(&self.body());
        out[Statement::BODY..].copy_from_slice(&self.signature);
        out
    }

    /// The statement in `bytes`, exactly [`Statement::LEN`] of them: its magic, format,
    /// padding, tier and length checked; its signature not (the [`Verifier`]'s).
    pub fn decode(bytes: &[u8]) -> Result<Statement, WireError> {
        if bytes.len() != Statement::LEN {
            return Err(WireError::Length { len: bytes.len(), wanted: Statement::LEN });
        }
        if bytes[0..4] != Statement::MAGIC {
            return Err(WireError::Magic { found: [bytes[0], bytes[1], bytes[2], bytes[3]] });
        }
        let format = u32_at(bytes, 4);
        if format != Statement::FORMAT {
            return Err(WireError::Format { found: format, wanted: Statement::FORMAT });
        }
        if let Some(at) = (93..96).find(|&at| bytes[at] != 0) {
            return Err(WireError::Padding { at });
        }
        let tier = match bytes[92] {
            0 => Tier::Top,
            1 => Tier::Fast,
            found => return Err(WireError::Tier { found }),
        };
        let mut signer = [0u8; 32];
        signer.copy_from_slice(&bytes[104..136]);
        let mut signature = [0u8; 64];
        signature.copy_from_slice(&bytes[Statement::BODY..]);
        Ok(Statement {
            namespace: Namespace(u64_at(bytes, 8)),
            segment: SegmentId(id_at(bytes, 16)),
            len: SegmentLen::new(u64_at(bytes, 48))?,
            toolchain: id_at(bytes, 56),
            records: u32_at(bytes, 88),
            tier,
            issued: u64_at(bytes, 96),
            signer,
            signature,
        })
    }
}

/// Why bytes on the wire (a [`SegmentRef`]'s row, a [`Statement`]) are not one: the layout
/// only, before any trust is weighed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WireError {
    /// `len` bytes where the form is `wanted`.
    Length { len: usize, wanted: usize },
    /// It does not start with its magic.
    Magic { found: [u8; 4] },
    /// Of format `found`; this blaze reads `wanted`.
    Format { found: u32, wanted: u32 },
    /// The padding byte at `at` is not zero.
    Padding { at: usize },
    /// A tier byte that names no [`Tier`].
    Tier { found: u8 },
    /// A segment length outside [`SegmentLen::LEAST`]`..=`[`SegmentLen::MOST`].
    SegmentLen { len: u64 },
    /// An index that starts inside the header, off [`ALIGN`], or does not end at the footer.
    Index { len: u64, index_at: u64, count: u32 },
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Length { len, wanted } => write!(f, "{len} bytes where the form is {wanted}"),
            WireError::Magic { found } => write!(f, "it starts with {found:02x?}, not its magic"),
            WireError::Format { found, wanted } => write!(f, "it is of format {found}; this blaze reads {wanted}"),
            WireError::Padding { at } => write!(f, "its padding byte at {at} is not zero"),
            WireError::Tier { found } => write!(f, "its tier byte {found} names no tier"),
            WireError::SegmentLen { len } => write!(
                f,
                "a segment length of {len} bytes, outside {}..={} (a header and a footer, and the most a fetch reads)",
                SegmentLen::LEAST,
                SegmentLen::MOST
            ),
            WireError::Index { len, index_at, count } => write!(
                f,
                "an index of {count} entries at {index_at} in a segment of {len} bytes: it must start past the header, on {ALIGN} bytes, and end where the footer starts"
            ),
        }
    }
}

/// Why a fetched segment is refused, by the [`Verifier`] (its statement) or by the store (its
/// bytes' digest, [`Refused::Digest`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// The signer is not a builder trusted for the statement's namespace.
    Untrusted { signer: [u8; 32] },
    /// The signer is trusted, but not at the time the statement was issued.
    Expired { signer: [u8; 32], issued: u64 },
    /// The signature does not hold over the statement's body under its signer's key.
    Signature { signer: [u8; 32] },
    /// The statement is of another namespace than the one the verifier verifies for.
    Namespace { found: Namespace, wanted: Namespace },
    /// The statement's toolchain is not the one this machine reads.
    Toolchain { found: ContentId, wanted: ContentId },
    /// The statement is about another segment than the one found.
    Segment { found: SegmentId, wanted: SegmentId },
    /// The statement gives another length than the segment found.
    Length { found: SegmentLen, wanted: SegmentLen },
    /// The fetched bytes' digest is not the segment's id: the store's one check of them.
    Digest { found: ContentId, wanted: SegmentId },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Untrusted { signer } => write!(f, "its signer {} is not a builder trusted for its namespace", Hex(signer)),
            Refused::Expired { signer, issued } => {
                write!(f, "its signer {} was not trusted when it was issued, at {issued}", Hex(signer))
            }
            Refused::Signature { signer } => write!(f, "its signature does not hold under its signer {}", Hex(signer)),
            Refused::Namespace { found, wanted } => {
                write!(f, "it is of namespace {:016x}, and it was asked of {:016x}", found.0, wanted.0)
            }
            Refused::Toolchain { found, wanted } => {
                write!(f, "it was built by toolchain {found:?}, and this machine reads {wanted:?}")
            }
            Refused::Segment { found, wanted } => {
                write!(f, "it is about segment {:?}, and the segment found is {:?}", found.0, wanted.0)
            }
            Refused::Length { found, wanted } => {
                write!(f, "it says {} bytes, and the segment found is {}", found.get(), wanted.get())
            }
            Refused::Digest { found, wanted } => {
                write!(f, "the fetched bytes digest to {found:?}, not the segment's id {:?}", wanted.0)
            }
        }
    }
}

/// A failure of the store, by name and class: never a formatted string.
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
    /// A fetched segment refused: by its statement ([`Verifier`]) or its bytes' digest.
    Refused { segment: SegmentId, why: Refused },
    /// A segment's bytes are not a segment's layout: `what` is wrong at byte `at`.
    Layout { segment: SegmentId, what: &'static str, at: u64 },
    /// Bytes on the wire (`what`: a statement, a segment reference) are not of their form.
    Wire { what: &'static str, why: WireError },
    /// A file of the store (`what`: a manifest, a segment, the head) is of format `found`; this
    /// blaze reads `wanted`.
    Format { what: &'static str, found: u32, wanted: u32 },
    /// Generation `generation`'s manifest: `what` is wrong at byte `at`.
    Manifest { generation: u64, what: &'static str, at: u64 },
    /// Two values under one key within one segment being sealed: the key misses an input
    /// (rule 21).
    TwoValues { tag: Tag, key: ContentId },
    /// A namespace other than the one asked: a key is never answered from a namespace the
    /// caller did not ask for.
    Namespace { found: Namespace, wanted: Namespace },
    /// A fetch of `segment` returned `len` bytes where its reference names `wanted`.
    Fetched { segment: SegmentId, len: u64, wanted: u64 },
    /// A segment generation `generation` names is gone or cannot be mapped (`what`): removed
    /// outside blaze, or lost in a power cut; its records are dropped, and said.
    Lost { segment: u64, generation: u64, what: &'static str },
    /// The store's driver ended `what` (a commit, a compaction slice, an open) without the
    /// outcome it owes.
    Unfinished { what: &'static str },
    /// The store waited for `waited_for` past its bound.
    Stalled { waited_for: &'static str },
    /// The record of `tag`, `key` is `len` bytes: more than a segment's entry can say.
    TooLong { tag: Tag, key: ContentId, len: u64 },
    /// A put into `namespace`, which this machine does not write (it is not a trusted writer of
    /// it): private builds stay local.
    NotWritable { namespace: Namespace },
    /// The record of `tag`, `key` was put by this process and is not committed yet: it has no
    /// mapped place, so it is said, never answered as absent.
    Uncommitted { tag: Tag, key: ContentId },
    /// The store's core stopped: `why`, as the core said it. The one variant that still carries
    /// text; its reasons become variants as the core's stop reasons are typed.
    Broken { why: String },
    /// A failure only printed, never matched on.
    Other { what: &'static str },
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
            StoreError::Refused { segment, why } => write!(f, "segment {:?} refused: {why}", segment.0),
            StoreError::Layout { segment, what, at } => {
                write!(f, "segment {:?} is not a segment: {what} at byte {at}", segment.0)
            }
            StoreError::Wire { what, why } => write!(f, "{what}: {why}"),
            StoreError::Format { what, found, wanted } => {
                write!(f, "{what} is of format {found}, and this blaze reads format {wanted}")
            }
            StoreError::Manifest { generation, what, at } => {
                write!(f, "generation {generation}'s manifest: {what} at byte {at}")
            }
            StoreError::TwoValues { tag, key } => {
                write!(f, "two values of kind {} under key {key:?}: the key misses an input", tag.0)
            }
            StoreError::Namespace { found, wanted } => write!(
                f,
                "namespace {:016x} where {:016x} was asked: a key is never answered from a namespace the caller did not ask for",
                found.0, wanted.0
            ),
            StoreError::Fetched { segment, len, wanted } => {
                write!(f, "segment {:?}: {len} bytes fetched, {wanted} named", segment.0)
            }
            StoreError::Lost { segment, generation, what } => {
                write!(f, "segment {segment:016x}, named by generation {generation}: {what}; its records are dropped")
            }
            StoreError::Unfinished { what } => write!(f, "the store's driver ended {what} without an outcome"),
            StoreError::Stalled { waited_for } => write!(f, "the store stalled waiting for {waited_for}"),
            StoreError::TooLong { tag, key, len } => {
                write!(f, "the record of kind {} key {key:?} is {len} bytes: too long for a segment", tag.0)
            }
            StoreError::NotWritable { namespace } => {
                write!(f, "namespace {:016x} is not written by this machine", namespace.0)
            }
            StoreError::Uncommitted { tag, key } => {
                write!(f, "the record of kind {} key {key:?} is put but not committed: it has no mapped place yet", tag.0)
            }
            StoreError::Broken { why } => write!(f, "the store stopped: {why}"),
            StoreError::Other { what } => f.write_str(what),
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
/// - A record is filed in the namespace its put names, and only there.
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

    /// Files `bytes` under `key` in `namespace`, in this process's segment for that namespace.
    /// No sync. The driver computes `namespace` once per package
    /// ([`crate::kind::namespace_of`]); a store never derives one.
    fn put<K: Kind>(&self, key: &Key<K>, namespace: Namespace, bytes: &[u8]) -> Result<Put, StoreError>;

    /// Publishes this process's segment: the generation flip.
    fn commit(&self) -> Result<Generation, StoreError>;

    /// Compacts into new segments what `roots` reach, under `cap` bytes, and unlinks the rest;
    /// run beside builds, never blocking a reader.
    fn compact(&self, roots: &[RootRef], cap: u64) -> Result<Compacted, StoreError>;
}

/// The fetch-on-miss seam: a shared cache (CI writes everything it builds; anyone reads; std's
/// and blaze-std's records ship as bundles). No upload here; no credentials assumed. Object
/// safe, so the plain store's driver holds a `&mut dyn Source`.
pub trait Source {
    /// Where each of `keys` is in `namespace`, in one round trip per index shard: `found[i]`
    /// for `keys[i]`, `None` where the source holds none (`found` is `keys.len()` long; a
    /// negative answer is cached by the caller). Every answer is of `namespace`: a source never
    /// answers from another.
    fn find(&mut self, namespace: Namespace, keys: &[RecordRef], found: &mut [Option<Located>]) -> Result<(), StoreError>;

    /// The bytes `range` of `segment` into `out`, exactly `range.end - range.start` of them;
    /// unchecked (the store checks a whole fetched segment's digest once).
    fn fetch_into(&mut self, segment: &SegmentRef, range: Range<u64>, out: &mut [u8]) -> Result<(), StoreError>;

    /// The statement that vouches for `segment`, decoded ([`Statement::decode`]) and not yet
    /// verified.
    fn statement(&mut self, segment: &SegmentRef) -> Result<Statement, StoreError>;
}

/// Checks a fetched segment's statement once, before a byte of the segment is read, mapped or
/// filed; the store records the result, with its own digest check, as its [`SegmentProof`].
///
/// The invariant, and the split of it: nothing fetched is read, mapped or filed unless
/// - `verify` accepted its statement: the signer is a builder trusted for the statement's
///   namespace at the time it was issued, the signature holds over [`Statement::body`], the
///   statement's segment id and length are `segment`'s, its namespace is the one this verifier
///   verifies for (an implementation is bound to one namespace) and its toolchain the one this
///   machine reads; and
/// - the digest over every fetched byte is `segment`'s id: the store's one check of the bytes,
///   refused as [`Refused::Digest`]. A verifier never sees the bytes, and the store never weighs
///   a signer.
///
/// Neither check is repeated for a segment once recorded (decision 18).
pub trait Verifier {
    fn verify(&self, statement: &Statement, segment: &SegmentRef) -> Result<(), Refused>;
}

/// No shared cache: every find finds nothing, so nothing is ever fetched from it.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoSource;

impl Source for NoSource {
    fn find(&mut self, _namespace: Namespace, _keys: &[RecordRef], found: &mut [Option<Located>]) -> Result<(), StoreError> {
        found.fill(None);
        Ok(())
    }

    fn fetch_into(&mut self, _segment: &SegmentRef, _range: Range<u64>, _out: &mut [u8]) -> Result<(), StoreError> {
        Err(StoreError::Other { what: "a fetch from no shared cache, whose find found nothing" })
    }

    fn statement(&mut self, _segment: &SegmentRef) -> Result<Statement, StoreError> {
        Err(StoreError::Other { what: "a statement from no shared cache, whose find found nothing" })
    }
}

/// A key's bytes in lowercase hex, for a message.
struct Hex<'a>(&'a [u8; 32]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut out = [0u8; 4];
    out.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(out)
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(out)
}

fn id_at(bytes: &[u8], at: usize) -> ContentId {
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes[at..at + 32]);
    ContentId(out)
}
