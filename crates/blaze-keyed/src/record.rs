//! What every record starts with, the roots of the kinds, and the fact vocabulary a surface's
//! exports and a record's read list are written in.
//!
//! A record is one blaze-format buffer: its header, then its arrays, then its root, whose first
//! field is a [`RecordHead`] holding the full 32-byte key, the kind's tag and version. The
//! store's index may key by fewer bits; [`open`] checks the full key against the head, so an
//! index collision is a typed refusal, never a wrong record.

#![blaze::sans_io]
// `blaze_format::record!` writes the `unsafe impl Record` of each root; nothing else here is
// unsafe.
#![allow(unsafe_code)]

use core::{fmt, mem::offset_of};

use blaze_format::{FormatError, Record, Slice, Writer};
use blaze_ids::{ContentId, Hasher};

use crate::kind::{Key, Kind, Tag};

blaze_format::record! {
    /// The first field of every root.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct RecordHead: 1 {
        pub key: ContentId,
        pub tag: u16,
        pub version: u16,
        pub flags: u32,
    }
}

blaze_format::record! {
    /// One fact and its digest: an exported fact of a surface, or a read a record's work made.
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
    pub struct Fact: 1 {
        pub id: ContentId,
        pub digest: ContentId,
    }
}

blaze_format::record! {
    /// The root of a traced kind ([`crate::kind::Trace::Reuse`] or `Outside`): the reads the work
    /// made, the facts it exports (a surface's; a body's const value or opaque type), and the
    /// payload.
    #[derive(Clone, Copy, Debug)]
    pub struct Traced: 1 {
        pub head: RecordHead,
        pub reads: Slice<Fact>,
        pub facts: Slice<Fact>,
        pub payload: Slice<u8>,
    }
}

blaze_format::record! {
    /// The root of a kind whose payload is its own business.
    #[derive(Clone, Copy, Debug)]
    pub struct Opaque: 1 {
        pub head: RecordHead,
        pub payload: Slice<u8>,
    }
}

blaze_format::record! {
    /// One plain array of a [`Layout::Plain`](crate::kind::Layout::Plain) record: `len` bytes at
    /// `at` from the buffer's start, of elements `elem` bytes wide.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Section: 1 {
        pub at: u64,
        pub len: u64,
        pub id: u32,
        pub elem: u32,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    pub struct Sections: 1 {
        pub head: RecordHead,
        pub sections: Slice<Section>,
    }
}

blaze_format::record! {
    /// One record a unit manifest names: the item it is of (by stable id), its kind and key.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Entry: 1 {
        pub item: ContentId,
        pub key: ContentId,
        pub tag: u16,
        pub pad: u16,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    pub struct ManifestRoot: 1 {
        pub head: RecordHead,
        pub entries: Slice<Entry>,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    pub struct ProgramRoot: 1 {
        pub head: RecordHead,
        pub image: ContentId,
        pub instances: Slice<ContentId>,
    }
}

/// A root: a record whose first field is its [`RecordHead`].
pub trait Headed: Record {
    fn head(&self) -> &RecordHead;
}

macro_rules! headed {
    ($($root:ty),*) => {$(
        const _: () = assert!(offset_of!($root, head) == 0, "a root's head is its first field");
        impl Headed for $root {
            fn head(&self) -> &RecordHead {
                &self.head
            }
        }
    )*};
}

headed!(Traced, Opaque, Sections, ManifestRoot, ProgramRoot);

/// What an item is across edits: the hash of its path and its kind. Never a position, never a
/// dense id.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct StableId(pub ContentId);

impl StableId {
    pub fn of(path: &[u8], item_kind: u8) -> StableId {
        let mut hasher = Hasher::new("blaze-keyed stable id");
        hasher.update(&[item_kind]).part(path);
        StableId(hasher.finish())
    }
}

/// What is read of an item, at item grain.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum FactKind {
    /// The record's own text: the first read of every traced record.
    Own = 1,
    Signature = 2,
    Fields = 3,
    Layout = 4,
    /// The impl rows for one (trait, simplified self type head).
    ImplRows = 5,
    /// What a name is bound to in one scope; unbound is a digest too.
    Binding = 6,
    ConstValue = 7,
    /// An inlinable body's code key: read by code records only, never by a check.
    InlineCode = 8,
    /// A late input (`env!`, `include!`) or an outside read of a run.
    Late = 9,
}

/// A fact's id: its kind, the item's stable id, and what of the item (a scope's name, a
/// (trait, self head) pair) for the kinds that need it.
pub fn fact_id(kind: FactKind, item: &StableId, what: &[u8]) -> ContentId {
    let mut hasher = Hasher::new("blaze-keyed fact id");
    hasher.update(&[kind as u8]).id(&item.0).part(what);
    hasher.finish()
}

/// A fact's digest: the hash of its value's bytes.
pub fn digest(bytes: &[u8]) -> ContentId {
    ContentId::of("blaze-keyed fact digest", bytes)
}

/// The digest of a record's result, apart from its key and reads: what an instance is keyed by,
/// so a body reused under a moved key keeps its code.
pub fn result_digest(root: &Traced, buffer: &[u8]) -> Result<ContentId, RecordError> {
    let facts = root.facts.follow(buffer).map_err(RecordError::Format)?;
    let payload = root.payload.follow(buffer).map_err(RecordError::Format)?;
    let mut hasher = Hasher::new("blaze-keyed result");
    hasher.update(&(facts.len() as u64).to_le_bytes());
    for fact in facts {
        hasher.id(&fact.id).id(&fact.digest);
    }
    hasher.part(payload);
    Ok(hasher.finish())
}

/// Why a buffer is not the record it was asked as.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RecordError {
    Format(FormatError),
    /// A head of another kind.
    Kind { found: Tag, wanted: Tag },
    /// A head of another version of the kind.
    Version { found: u16, wanted: u16 },
    /// A head of another key: an index collision, or a wrong record filed.
    Key { found: ContentId, wanted: ContentId },
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordError::Format(error) => write!(f, "a record that does not open: {error}"),
            RecordError::Kind { found, wanted } => {
                write!(f, "a record of kind {} asked as kind {}", found.0, wanted.0)
            }
            RecordError::Version { found, wanted } => {
                write!(f, "a record of version {found} asked as version {wanted}")
            }
            RecordError::Key { found, wanted } => {
                write!(f, "the record of key {found:?} filed under key {wanted:?}")
            }
        }
    }
}

/// The root of `buffer` as a record of kind `K` under `key`: schema, kind, version and the full
/// key checked. `buffer` is 16-aligned (a `Store::Held`, a mapped record, an `Aligned`).
pub fn open<'b, K: Kind>(buffer: &'b [u8], key: &Key<K>) -> Result<&'b K::Root, RecordError> {
    let root = blaze_format::open::<K::Root>(buffer).map_err(RecordError::Format)?;
    let head = root.head();
    if head.tag != K::TAG.0 {
        return Err(RecordError::Kind { found: Tag(head.tag), wanted: K::TAG });
    }
    if head.version != K::VERSION {
        return Err(RecordError::Version { found: head.version, wanted: K::VERSION });
    }
    if head.key != key.id() {
        return Err(RecordError::Key { found: head.key, wanted: key.id() });
    }
    Ok(root)
}

fn head_of<K: Kind>(key: &Key<K>) -> RecordHead {
    RecordHead { key: key.id(), tag: K::TAG.0, version: K::VERSION, flags: 0 }
}

/// A [`Traced`] record of kind `K`.
pub fn write_traced<K: Kind<Root = Traced>>(
    key: &Key<K>,
    reads: &[Fact],
    facts: &[Fact],
    payload: &[u8],
) -> Vec<u8> {
    let mut out = Writer::new(Traced::SCHEMA);
    let reads_at = out.push_slice(reads);
    let facts_at = out.push_slice(facts);
    let payload_at = out.push_slice(payload);
    let at = out.next::<Traced>();
    let field = |offset: usize| at + offset as u64;
    let root = Traced {
        head: head_of(key),
        reads: Slice::between(field(offset_of!(Traced, reads)), reads_at, reads.len()),
        facts: Slice::between(field(offset_of!(Traced, facts)), facts_at, facts.len()),
        payload: Slice::between(field(offset_of!(Traced, payload)), payload_at, payload.len()),
    };
    let root_at = out.push(&root);
    debug_assert_eq!(root_at, at);
    out.finish(root_at)
}

/// An [`Opaque`] record of kind `K`.
pub fn write_opaque<K: Kind<Root = Opaque>>(key: &Key<K>, payload: &[u8]) -> Vec<u8> {
    let mut out = Writer::new(Opaque::SCHEMA);
    let payload_at = out.push_slice(payload);
    let at = out.next::<Opaque>();
    let root = Opaque {
        head: head_of(key),
        payload: Slice::between(at + offset_of!(Opaque, payload) as u64, payload_at, payload.len()),
    };
    let root_at = out.push(&root);
    out.finish(root_at)
}

/// A [`ManifestRoot`] record of kind `K`.
pub fn write_manifest<K: Kind<Root = ManifestRoot>>(key: &Key<K>, entries: &[Entry]) -> Vec<u8> {
    let mut out = Writer::new(ManifestRoot::SCHEMA);
    let entries_at = out.push_slice(entries);
    let at = out.next::<ManifestRoot>();
    let root = ManifestRoot {
        head: head_of(key),
        entries: Slice::between(
            at + offset_of!(ManifestRoot, entries) as u64,
            entries_at,
            entries.len(),
        ),
    };
    let root_at = out.push(&root);
    out.finish(root_at)
}

/// `record`, a record of some key, as the same record under `key`: the reuse of a result whose
/// read list verified (Trace::Reuse), byte for byte what doing the work again would write.
/// `None` when `record` is not a buffer with a head at its root.
pub fn reheaded(record: &[u8], key: ContentId) -> Option<Vec<u8>> {
    let root = usize::try_from(blaze_format::header(record).ok()?.root).ok()?;
    let at = root + offset_of!(RecordHead, key);
    let mut out = Vec::with_capacity_in(record.len(), std::alloc::Global);
    out.extend_from_slice(record);
    out.get_mut(at..at + 32)?.copy_from_slice(&key.0);
    Some(out)
}
