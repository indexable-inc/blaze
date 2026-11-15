//! What every record starts with, the roots of the kinds, the fact vocabulary, and the one home
//! of a fact id's layout: how an ask is routed to the table that answers it.
//!
//! A record is one blaze-format buffer: its header, then its arrays, then its root, whose first
//! field is a [`RecordHead`] holding the full 32-byte key it is filed under, the kind's tag and
//! version. The store's index may key by fewer bits; [`open`] checks the full key against the
//! head, so an index collision is a typed refusal, never a wrong record.
//!
//! **Read-addressed records** (docs/v2/plan/design/read-addressed.md). A body's and a surface's
//! record is filed at its *address*: [`address`] of its key (what it is: its own text and
//! settings) and its *shape* with the digests its work saw (the facts it asked for, sorted by id,
//! each once, absences included). Its root is a [`Traced`]: `reads` is the shape with those
//! digests, `facts` the facts the work makes (a surface's table; a body's const value, hidden
//! type, escaped items, effects), `payload` its value. A changed fact is a new address; an undone
//! one is the old address again; a fact the work never asked for moves nothing.
//!
//! **The fact id** ([`fact`]): 32 bytes, laid out so that the id alone routes an ask
//! ([`route`]), with no table of ids and no scan:
//!
//! | bytes | what |
//! |---|---|
//! | `0..8` | the [`Owner`]: the first eight bytes of the key of the crate whose records hold the fact; [`Owner::ASKER`] for what the asking unit sees; [`Owner::OUTSIDE`] for a late read |
//! | `8..10` | the front end's query code, little-endian ([`code_of`]) |
//! | `10` | the [`FactKind`] byte ([`kind_of`]): whether the owner's surface or one of its bodies makes it |
//! | `11..32` | the hash of the kind, the subject's stable id and the object ([`fact_id`]) |

#![blaze::sans_io]
// `blaze_format::record!` writes the `unsafe impl Record` of each root; nothing else here is
// unsafe.
#![allow(unsafe_code)]

use core::{fmt, mem::offset_of};

use blaze_format::{FormatError, Record, Slice, Writer};
use blaze_ids::{ContentId, Hasher};

use crate::kind::{Key, Kind, Shape, Tag, UnitChunk, UnitManifest};

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
    /// One fact and its digest: a row of a surface's table, a fact a body makes, or one ask of a
    /// shape with the digest the work saw.
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
    pub struct Fact: 1 {
        pub id: ContentId,
        pub digest: ContentId,
    }
}

blaze_format::record! {
    /// The root of a traced kind (read-addressed or outside): the shape the work asked with the
    /// digests it saw (sorted by id, each once), the facts it makes (sorted by id, each once), and
    /// the payload.
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
    /// One node of a key's shape trie ([`crate::kind::Shape`]): the ids of a shape some run of
    /// the key asked, sorted, each once.
    #[derive(Clone, Copy, Debug)]
    pub struct ShapeRoot: 1 {
        pub head: RecordHead,
        pub ids: Slice<ContentId>,
    }
}

blaze_format::record! {
    /// A unit's mark ([`crate::kind::Mark`]): the manifest its text and view name, the address of
    /// its surface, and its universe digest ([`universe`]).
    #[derive(Clone, Copy, Debug)]
    pub struct MarkRoot: 1 {
        pub head: RecordHead,
        pub manifest: ContentId,
        pub surface: ContentId,
        pub universe: ContentId,
    }
}

blaze_format::record! {
    /// One item of a unit's chunk: its stable id, its key, the address its record is filed at,
    /// the record's result digest ([`result_digest`]), its shape (`shape_len` ordinals into the
    /// chunk's dictionary at `shape_at` of `shapes`) and its kind.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Entry: 1 {
        pub item: ContentId,
        pub key: ContentId,
        pub address: ContentId,
        pub result: ContentId,
        pub shape_at: u32,
        pub shape_len: u32,
        pub tag: u16,
        pub pad: u16,
        pub pad2: u32,
    }
}

blaze_format::record! {
    /// One file's chunk of a unit manifest ([`crate::kind::UnitChunk`]): the entries of the
    /// file's items in stable-id order, their shapes as ordinals into the chunk's own
    /// dictionary, and the postings (dictionary ordinal to entries reading it). The chunk holds
    /// no digest, so it moves only when one of its items' key, address or shape does.
    #[derive(Clone, Copy, Debug)]
    pub struct ChunkRoot: 1 {
        pub head: RecordHead,
        /// The unit's identity and the file's key: what the shared cache indexes a chunk by.
        pub unit: ContentId,
        pub file: ContentId,
        pub entries: Slice<Entry>,
        pub shapes: Slice<u32>,
        /// Fact ids, sorted, each once.
        pub dictionary: Slice<ContentId>,
        /// Entry indices: the readers of ordinal `k` are `postings[posting_at[k]..posting_at[k + 1]]`.
        pub postings: Slice<u32>,
        pub posting_at: Slice<u32>,
    }
}

blaze_format::record! {
    /// A file of a unit manifest: its key, its chunk's address, the digest of its items' keys
    /// ([`keys_digest`]: equal now, and no item of the file moved key, so the chunk need not be
    /// read to know it) and how many items it holds.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct FileChunk: 1 {
        pub file: ContentId,
        pub chunk: ContentId,
        pub keys: ContentId,
        pub items: u32,
        pub pad: u32,
    }
}

blaze_format::record! {
    /// A unit manifest's root ([`crate::kind::UnitManifest`]), filed by content: the unit's
    /// surface, a chunk per file, the dictionary of every fact the unit's records read (with the
    /// digest they saw, and which chunks read it), and the facts its bodies make with their makers.
    #[derive(Clone, Copy, Debug)]
    pub struct ManifestRoot: 1 {
        pub head: RecordHead,
        /// The manifest's key, from text ([`crate::kind::UnitInputs`]).
        pub key: ContentId,
        pub surface_key: ContentId,
        pub surface: ContentId,
        pub surface_result: ContentId,
        pub files: Slice<FileChunk>,
        /// Sorted by id, each once: every fact a record of the unit read, with the digest seen.
        pub dictionary: Slice<Fact>,
        /// Chunk indices reading row `r`: `readers[reader_at[r]..reader_at[r + 1]]`; the index
        /// `files.len()` is the surface.
        pub readers: Slice<u32>,
        pub reader_at: Slice<u32>,
        /// The surface's shape, as ordinals into `dictionary`.
        pub surface_shape: Slice<u32>,
        /// The facts the unit's bodies make, sorted by id, each once; `makers[i]` made `made[i]`.
        pub made: Slice<Fact>,
        pub makers: Slice<ContentId>,
    }
}

/// A root: a record whose first field is its [`RecordHead`].
pub trait Headed: Record {
    fn head(&self) -> &RecordHead;
}

/// A root that carries a payload: [`Opaque`] and [`Traced`]. What [`crate::proof::payload`]
/// reads by proof.
pub trait HasPayload: Headed {
    fn payload(&self) -> &Slice<u8>;
}

impl HasPayload for Opaque {
    fn payload(&self) -> &Slice<u8> {
        &self.payload
    }
}

impl HasPayload for Traced {
    fn payload(&self) -> &Slice<u8> {
        &self.payload
    }
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

headed!(Traced, Opaque, Sections, ShapeRoot, MarkRoot, ChunkRoot, ManifestRoot);

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

/// What is read of an item, at item grain. The byte is stamped at byte 10 of a fact's id
/// ([`fact`]); [`FactKind::made_by_body`] says which kinds a body makes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum FactKind {
    /// A record's own row: a surface's own inputs.
    Own = 1,
    Signature = 2,
    Fields = 3,
    Layout = 4,
    /// The impl rows for one (trait, simplified self type head).
    ImplRows = 5,
    /// What a name is bound to in one scope; unbound is a digest too.
    Binding = 6,
    /// A const's value, as const evaluation reads it: made by the const's (or `const fn`'s) body.
    ConstValue = 7,
    /// An inlinable body's code key: read by code records only, never by a check.
    InlineCode = 8,
    /// A late input (`env!`, `include!`): answered by the host's outside answerer.
    Late = 9,
    /// An opaque type's hidden type with its auto traits (return-position `impl Trait`, an
    /// `async fn`'s future, a `#[define_opaque]` alias): made by the body that defines it.
    HiddenType = 10,
    /// The items and impls declared inside a body (after expansion), as one digest: made by the
    /// body.
    Escaped = 11,
    /// A body's own effect and purity summary, its callees by item: made by the body.
    Effects = 12,
    /// The kinds a front end's tasks read (blaze-front, not built on main yet).
    Attrs = 13,
    CrateRoot = 14,
    TraitsInScope = 15,
    WholeScope = 16,
    CrateNames = 17,
    FieldOrder = 18,
    Setting = 19,
    InherentAssocTypes = 20,
    Name = 21,
    Coherence = 22,
    Specializes = 23,
    DropCheck = 24,
    Target = 25,
    RpititTys = 26,
    StepInfer = 27,
    StepMir = 28,
    StepLower = 29,
    MatchCheck = 30,
    Hashes = 31,
    Method = 32,
    Vtable = 33,
    InherentImpls = 34,
    TraitImpls = 35,
    TraitsNamed = 36,
    ScopeTraitsNamed = 37,
    BlockImpls = 38,
    /// A surface's exports: the digest of its other rows.
    Exports = 39,
    /// What the asking unit sees as a whole ([`seen_id`]): the row an [`Owner::ASKER`] fact the
    /// asker's table has no row of is answered by.
    Seen = 40,
}

impl FactKind {
    const ALL: [FactKind; 40] = [
        FactKind::Own,
        FactKind::Signature,
        FactKind::Fields,
        FactKind::Layout,
        FactKind::ImplRows,
        FactKind::Binding,
        FactKind::ConstValue,
        FactKind::InlineCode,
        FactKind::Late,
        FactKind::HiddenType,
        FactKind::Escaped,
        FactKind::Effects,
        FactKind::Attrs,
        FactKind::CrateRoot,
        FactKind::TraitsInScope,
        FactKind::WholeScope,
        FactKind::CrateNames,
        FactKind::FieldOrder,
        FactKind::Setting,
        FactKind::InherentAssocTypes,
        FactKind::Name,
        FactKind::Coherence,
        FactKind::Specializes,
        FactKind::DropCheck,
        FactKind::Target,
        FactKind::RpititTys,
        FactKind::StepInfer,
        FactKind::StepMir,
        FactKind::StepLower,
        FactKind::MatchCheck,
        FactKind::Hashes,
        FactKind::Method,
        FactKind::Vtable,
        FactKind::InherentImpls,
        FactKind::TraitImpls,
        FactKind::TraitsNamed,
        FactKind::ScopeTraitsNamed,
        FactKind::BlockImpls,
        FactKind::Exports,
        FactKind::Seen,
    ];

    /// The kind of byte `byte`, if any.
    pub const fn of_byte(byte: u8) -> Option<FactKind> {
        match byte {
            1..=40 => Some(FactKind::ALL[byte as usize - 1]),
            _ => None,
        }
    }

    /// Whether a body's task makes facts of this kind (its readers ask them of the maker's
    /// unit manifest); else the owner's surface does (they are rows of its table).
    pub const fn made_by_body(self) -> bool {
        matches!(self, FactKind::ConstValue | FactKind::HiddenType | FactKind::Escaped | FactKind::Effects)
    }
}

/// The unit whose records hold a fact: the first eight bytes of its crate key.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Owner(pub [u8; 8]);

impl Owner {
    /// What the asking unit sees (a listing across the crates it loads, a fact of the whole
    /// build): answered from the asking unit's own surface. No crate key begins with it.
    pub const ASKER: Owner = Owner([0xff; 8]);
    /// A late read (`env!`, `include!`): answered by the host's outside answerer, never by a
    /// table. No crate key begins with it.
    pub const OUTSIDE: Owner = Owner([0xfe; 8]);

    /// The owner of a fact whose subject is `subject`, an item, module or crate key: its first
    /// eight bytes, its crate key's.
    pub fn of_subject(subject: &ContentId) -> Owner {
        let mut prefix = [0; 8];
        prefix.copy_from_slice(&subject.0[..8]);
        Owner(prefix)
    }

    /// The owner stamped in `id` ([`fact`]).
    pub fn of_id(id: &ContentId) -> Owner {
        Owner::of_subject(id)
    }

    /// Whether a crate key may own facts: not one of the reserved prefixes.
    pub fn is_crate(self) -> bool {
        self != Owner::ASKER && self != Owner::OUTSIDE
    }
}

/// A fact's unstamped hash: its kind, the item's stable id, and what of the item (a scope's
/// name, a (trait, self head) pair) for the kinds that need it. [`fact`] stamps its first eleven
/// bytes; an id used as a fact's is always [`fact`]'s.
pub fn fact_id(kind: FactKind, item: &StableId, what: &[u8]) -> ContentId {
    let mut hasher = Hasher::new("blaze-keyed fact id");
    hasher.update(&[kind as u8]).id(&item.0).part(what);
    hasher.finish()
}

/// The id of a fact of `kind` about `item` (`what` of it), owned by `owner` and of the front
/// end's query code `code`: [`fact_id`] with the owner, the code and the kind stamped over its
/// first eleven bytes. The one id function every side (the surface, the body task, the driver)
/// writes ids through.
pub fn fact(kind: FactKind, owner: Owner, code: u16, item: &StableId, what: &[u8]) -> ContentId {
    let mut id = fact_id(kind, item, what);
    id.0[..8].copy_from_slice(&owner.0);
    id.0[8..10].copy_from_slice(&code.to_le_bytes());
    id.0[10] = kind as u8;
    id
}

/// The query code stamped in `id`.
pub fn code_of(id: &ContentId) -> u16 {
    u16::from_le_bytes([id.0[8], id.0[9]])
}

/// The kind stamped in `id`, if it is one.
pub fn kind_of(id: &ContentId) -> Option<FactKind> {
    FactKind::of_byte(id.0[10])
}

/// The id of a late read: `which` macro (its [`crate::kind::Late`] byte) of `name`.
pub fn late_id(which: u8, name: &[u8]) -> ContentId {
    let mut what = [0u8; 1];
    what[0] = which;
    let mut hasher = Hasher::new("blaze-keyed late read");
    hasher.update(&what).part(name);
    fact(FactKind::Late, Owner::OUTSIDE, 0, &StableId(hasher.finish()), b"")
}

/// The id of the row every surface holds for what its unit sees as a whole: what an
/// [`Owner::ASKER`] fact its table has no row of is answered by.
pub fn seen_id() -> ContentId {
    fact(FactKind::Seen, Owner::ASKER, 0xffff, &StableId(ContentId([0; 32])), b"")
}

/// Where the answer to an ask is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Route {
    /// The asking unit's own surface table (its row, else its [`seen_id`] row).
    Asker,
    /// The host's outside answers (a late read).
    Outside,
    /// A row of the owner's surface table.
    Surface(Owner),
    /// A fact one of the owner's bodies makes: the owner's manifest's `made` table (or, in the
    /// owner's own unit, the facts its settled bodies made).
    Made(Owner),
}

/// Why an id routes nowhere.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Unrouted {
    pub id: ContentId,
    /// The byte at 10 that is no [`FactKind`].
    pub kind: u8,
}

/// The table that answers `id`: from its bytes alone. Floor: O(1).
pub fn route(id: &ContentId) -> Result<Route, Unrouted> {
    let kind = kind_of(id).ok_or(Unrouted { id: *id, kind: id.0[10] })?;
    Ok(match Owner::of_id(id) {
        Owner::ASKER => Route::Asker,
        Owner::OUTSIDE => Route::Outside,
        owner if kind.made_by_body() => Route::Made(owner),
        owner => Route::Surface(owner),
    })
}

/// The digest of a fact whose owner is in the asker's universe and holds no row of it: a value,
/// never a hole. Every side writes an absence as this.
pub fn absent(id: &ContentId) -> ContentId {
    ContentId::of("blaze-keyed absent", &id.0[8..11])
}

/// The digest of a fact whose owner is not in the asker's universe at all.
pub const GONE: ContentId = ContentId([0; 32]);

/// A fact's digest: the hash of its value's bytes.
pub fn digest(bytes: &[u8]) -> ContentId {
    ContentId::of("blaze-keyed fact digest", bytes)
}

/// The address the record of key `key` is filed at when its work asked `reads` (sorted by id,
/// each once) and saw their digests: one hash, no allocation. Floor: O(|reads|).
pub fn address<K: Kind>(key: Key<K>, reads: &[Fact]) -> Key<K> {
    let mut hasher = Hasher::new("blaze-keyed address 2");
    hasher.update(&K::TAG.0.to_le_bytes()).id(&key.id()).update(&(reads.len() as u64).to_le_bytes());
    for read in reads {
        hasher.id(&read.id).id(&read.digest);
    }
    Key::from_id(hasher.finish())
}

/// The first node of `key`'s shape trie: the shape the first run of the key filed.
pub fn shape_root<K: Kind>(key: Key<K>) -> Key<Shape> {
    let mut hasher = Hasher::new("blaze-keyed shape root");
    hasher.update(&K::TAG.0.to_le_bytes()).id(&key.id());
    Key::from_id(hasher.finish())
}

/// The node after a missed `address`: the shape a run filed whose answers to the shape that
/// made `address` were those that made it, and which asked another shape.
pub fn shape_next<K: Kind>(address: Key<K>) -> Key<Shape> {
    let mut hasher = Hasher::new("blaze-keyed shape next");
    hasher.update(&K::TAG.0.to_le_bytes()).id(&address.id());
    Key::from_id(hasher.finish())
}

/// The digest of a record's result, apart from its key and reads: what a dependent sees of it
/// (a surface's in its unit's universe digest, a body's in an instance plan's key).
pub fn result_digest(root: &Traced, buffer: &[u8]) -> Result<ContentId, RecordError> {
    let facts = root.facts.follow(buffer).map_err(RecordError::Format)?;
    let payload = root.payload.follow(buffer).map_err(RecordError::Format)?;
    Ok(result_of(facts, payload))
}

/// [`result_digest`] of `facts` and `payload`.
pub fn result_of(facts: &[Fact], payload: &[u8]) -> ContentId {
    let mut hasher = Hasher::new("blaze-keyed result");
    hasher.update(&(facts.len() as u64).to_le_bytes());
    for fact in facts {
        hasher.id(&fact.id).id(&fact.digest);
    }
    hasher.part(payload);
    hasher.finish()
}

/// The digest of a sorted table of facts (a unit's `made`).
pub fn table_digest(facts: &[Fact]) -> ContentId {
    result_of(facts, &[])
}

/// A unit's universe digest: its surface's result, the facts its bodies make, and each direct
/// dependency's universe digest in plan order. Made once per unit; equal exactly when nothing a
/// dependent can read of the unit or below it moved.
pub fn universe(surface_result: ContentId, made: ContentId, deps: &[ContentId]) -> ContentId {
    let mut hasher = Hasher::new("blaze-keyed universe");
    hasher.id(&surface_result).id(&made).update(&(deps.len() as u64).to_le_bytes());
    deps.iter().for_each(|dep| {
        hasher.id(dep);
    });
    hasher.finish()
}

/// The digest of a file's items with their keys, in the file's order: what a
/// [`FileChunk::keys`] holds.
pub fn keys_digest<K>(items: &[(StableId, Key<K>)]) -> ContentId {
    let mut hasher = Hasher::new("blaze-keyed file keys");
    hasher.update(&(items.len() as u64).to_le_bytes());
    for (item, key) in items {
        hasher.id(&item.0).id(&key.id());
    }
    hasher.finish()
}

/// A unit's view: its direct dependencies' universe digests in plan order: with its manifest
/// key, what its mark is filed under.
pub fn view(deps: &[ContentId]) -> ContentId {
    let mut hasher = Hasher::new("blaze-keyed view");
    hasher.update(&(deps.len() as u64).to_le_bytes());
    deps.iter().for_each(|dep| {
        hasher.id(dep);
    });
    hasher.finish()
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
    /// A read-addressed record whose head is not the address of its key and its reads.
    Address { found: ContentId, wanted: ContentId },
    /// A shape or a table not sorted by id, or holding an id twice, at row `at`.
    Unsorted { at: u32 },
    /// An index of a manifest or a chunk out of the table it points into, or a fact id that
    /// routes nowhere, at row `at` of the table `what`.
    Malformed { at: u32, what: &'static str },
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
            RecordError::Address { found, wanted } => write!(
                f,
                "a record headed {found:?} whose key and reads address {wanted:?} (rule keys-cover-every-input)"
            ),
            RecordError::Unsorted { at } => {
                write!(f, "a shape or table not sorted by id, each once, at row {at}")
            }
            RecordError::Malformed { at, what } => {
                write!(f, "row {at} of a record's {what} points outside its table or routes nowhere")
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

/// Whether `facts` is sorted by id, each once: the first row out of order if not.
pub fn sorted(facts: &[Fact]) -> Result<(), RecordError> {
    match facts.windows(2).position(|pair| pair[0].id >= pair[1].id) {
        Some(at) => Err(RecordError::Unsorted { at: at as u32 + 1 }),
        None => Ok(()),
    }
}

/// Whether `ids` is sorted, each once: the first id out of order if not.
pub fn sorted_ids(ids: &[ContentId]) -> Result<(), RecordError> {
    match ids.windows(2).position(|pair| pair[0] >= pair[1]) {
        Some(at) => Err(RecordError::Unsorted { at: at as u32 + 1 }),
        None => Ok(()),
    }
}

/// A record a task wrote for key `key`, read-addressed: it opens as a [`Traced`] of kind `K`, its
/// shape and its facts are sorted by id, each once, and its head is the address its key and
/// shape make. What the driver checks of every record a task gives it. Floor: O(|reads|).
pub fn check_read<'b, K: Kind<Root = Traced>>(buffer: &'b [u8], key: Key<K>) -> Result<(Key<K>, &'b Traced), RecordError> {
    let root = blaze_format::open::<Traced>(buffer).map_err(RecordError::Format)?;
    if root.head.tag != K::TAG.0 {
        return Err(RecordError::Kind { found: Tag(root.head.tag), wanted: K::TAG });
    }
    if root.head.version != K::VERSION {
        return Err(RecordError::Version { found: root.head.version, wanted: K::VERSION });
    }
    let reads = root.reads.follow(buffer).map_err(RecordError::Format)?;
    sorted(reads)?;
    sorted(root.facts.follow(buffer).map_err(RecordError::Format)?)?;
    let at = address(key, reads);
    if root.head.key != at.id() {
        return Err(RecordError::Address { found: root.head.key, wanted: at.id() });
    }
    Ok((at, root))
}

fn head_of<K: Kind>(key: &Key<K>) -> RecordHead {
    RecordHead { key: key.id(), tag: K::TAG.0, version: K::VERSION, flags: 0 }
}

/// A [`Traced`] record of kind `K` filed at `key`.
pub fn write_traced<K: Kind<Root = Traced>>(key: &Key<K>, reads: &[Fact], facts: &[Fact], payload: &[u8]) -> Vec<u8> {
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

/// A read-addressed record of kind `K` for key `key`: filed at [`address`]`(key, reads)`, which
/// it returns with the bytes. `reads` and `facts` sorted by id, each once. What a front end's
/// task writes.
pub fn write_read<K: Kind<Root = Traced>>(key: Key<K>, reads: &[Fact], facts: &[Fact], payload: &[u8]) -> (Key<K>, Vec<u8>) {
    let at = address(key, reads);
    (at, write_traced(&at, reads, facts, payload))
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

/// A shape trie node at `at` holding `ids` (sorted, each once).
pub fn write_shape(at: &Key<Shape>, ids: &[ContentId]) -> Vec<u8> {
    let mut out = Writer::new(ShapeRoot::SCHEMA);
    let ids_at = out.push_slice(ids);
    let at_root = out.next::<ShapeRoot>();
    let root = ShapeRoot {
        head: head_of(at),
        ids: Slice::between(at_root + offset_of!(ShapeRoot, ids) as u64, ids_at, ids.len()),
    };
    let root_at = out.push(&root);
    out.finish(root_at)
}

/// A mark at `at`.
pub fn write_mark<K: Kind<Root = MarkRoot>>(at: &Key<K>, manifest: ContentId, surface: ContentId, universe: ContentId) -> Vec<u8> {
    let mut out = Writer::new(MarkRoot::SCHEMA);
    let root = MarkRoot { head: head_of(at), manifest, surface, universe };
    let root_at = out.push(&root);
    out.finish(root_at)
}

/// The parts of a unit's chunk ([`ChunkRoot`]).
#[derive(Clone, Copy, Debug)]
pub struct ChunkParts<'a> {
    pub unit: ContentId,
    pub file: ContentId,
    pub entries: &'a [Entry],
    pub shapes: &'a [u32],
    pub dictionary: &'a [ContentId],
    pub postings: &'a [u32],
    pub posting_at: &'a [u32],
}

fn hash_u32s(hasher: &mut Hasher, values: &[u32]) {
    hasher.update(&(values.len() as u64).to_le_bytes());
    values.iter().for_each(|value| {
        hasher.update(&value.to_le_bytes());
    });
}

/// A chunk, filed by content: its address and its bytes. Floor: one pass over its parts.
pub fn write_chunk(parts: &ChunkParts<'_>) -> (Key<UnitChunk>, Vec<u8>) {
    let mut hasher = Hasher::new("blaze-keyed unit chunk");
    hasher.id(&parts.unit).id(&parts.file).update(&(parts.entries.len() as u64).to_le_bytes());
    for entry in parts.entries {
        hasher.id(&entry.item).id(&entry.key).id(&entry.address).id(&entry.result);
        hasher.update(&entry.shape_at.to_le_bytes()).update(&entry.shape_len.to_le_bytes()).update(&entry.tag.to_le_bytes());
    }
    hash_u32s(&mut hasher, parts.shapes);
    hasher.update(&(parts.dictionary.len() as u64).to_le_bytes());
    parts.dictionary.iter().for_each(|id| {
        hasher.id(id);
    });
    hash_u32s(&mut hasher, parts.postings);
    hash_u32s(&mut hasher, parts.posting_at);
    let key: Key<UnitChunk> = Key::from_id(hasher.finish());
    let mut out = Writer::new(ChunkRoot::SCHEMA);
    let entries_at = out.push_slice(parts.entries);
    let shapes_at = out.push_slice(parts.shapes);
    let dictionary_at = out.push_slice(parts.dictionary);
    let postings_at = out.push_slice(parts.postings);
    let posting_at_at = out.push_slice(parts.posting_at);
    let at = out.next::<ChunkRoot>();
    let field = |offset: usize| at + offset as u64;
    let root = ChunkRoot {
        head: head_of(&key),
        unit: parts.unit,
        file: parts.file,
        entries: Slice::between(field(offset_of!(ChunkRoot, entries)), entries_at, parts.entries.len()),
        shapes: Slice::between(field(offset_of!(ChunkRoot, shapes)), shapes_at, parts.shapes.len()),
        dictionary: Slice::between(field(offset_of!(ChunkRoot, dictionary)), dictionary_at, parts.dictionary.len()),
        postings: Slice::between(field(offset_of!(ChunkRoot, postings)), postings_at, parts.postings.len()),
        posting_at: Slice::between(field(offset_of!(ChunkRoot, posting_at)), posting_at_at, parts.posting_at.len()),
    };
    let root_at = out.push(&root);
    (key, out.finish(root_at))
}

/// The parts of a unit manifest's root ([`ManifestRoot`]).
#[derive(Clone, Copy, Debug)]
pub struct ManifestParts<'a> {
    pub key: Key<UnitManifest>,
    pub surface_key: ContentId,
    pub surface: ContentId,
    pub surface_result: ContentId,
    pub files: &'a [FileChunk],
    pub dictionary: &'a [Fact],
    pub readers: &'a [u32],
    pub reader_at: &'a [u32],
    pub surface_shape: &'a [u32],
    pub made: &'a [Fact],
    pub makers: &'a [ContentId],
}

/// A manifest root, filed by content: its address and its bytes. Floor: one pass over its parts.
pub fn write_manifest(parts: &ManifestParts<'_>) -> (Key<UnitManifest>, Vec<u8>) {
    let mut hasher = Hasher::new("blaze-keyed unit manifest");
    hasher.id(&parts.key.id()).id(&parts.surface_key).id(&parts.surface).id(&parts.surface_result);
    hasher.update(&(parts.files.len() as u64).to_le_bytes());
    for file in parts.files {
        hasher.id(&file.file).id(&file.chunk).id(&file.keys).update(&file.items.to_le_bytes());
    }
    hasher.update(&(parts.dictionary.len() as u64).to_le_bytes());
    for row in parts.dictionary {
        hasher.id(&row.id).id(&row.digest);
    }
    hash_u32s(&mut hasher, parts.readers);
    hash_u32s(&mut hasher, parts.reader_at);
    hash_u32s(&mut hasher, parts.surface_shape);
    hasher.update(&(parts.made.len() as u64).to_le_bytes());
    for (row, maker) in parts.made.iter().zip(parts.makers) {
        hasher.id(&row.id).id(&row.digest).id(maker);
    }
    let key: Key<UnitManifest> = Key::from_id(hasher.finish());
    let mut out = Writer::new(ManifestRoot::SCHEMA);
    let files_at = out.push_slice(parts.files);
    let dictionary_at = out.push_slice(parts.dictionary);
    let readers_at = out.push_slice(parts.readers);
    let reader_at_at = out.push_slice(parts.reader_at);
    let shape_at = out.push_slice(parts.surface_shape);
    let made_at = out.push_slice(parts.made);
    let makers_at = out.push_slice(parts.makers);
    let at = out.next::<ManifestRoot>();
    let field = |offset: usize| at + offset as u64;
    let root = ManifestRoot {
        head: head_of(&key),
        key: parts.key.id(),
        surface_key: parts.surface_key,
        surface: parts.surface,
        surface_result: parts.surface_result,
        files: Slice::between(field(offset_of!(ManifestRoot, files)), files_at, parts.files.len()),
        dictionary: Slice::between(field(offset_of!(ManifestRoot, dictionary)), dictionary_at, parts.dictionary.len()),
        readers: Slice::between(field(offset_of!(ManifestRoot, readers)), readers_at, parts.readers.len()),
        reader_at: Slice::between(field(offset_of!(ManifestRoot, reader_at)), reader_at_at, parts.reader_at.len()),
        surface_shape: Slice::between(field(offset_of!(ManifestRoot, surface_shape)), shape_at, parts.surface_shape.len()),
        made: Slice::between(field(offset_of!(ManifestRoot, made)), made_at, parts.made.len()),
        makers: Slice::between(field(offset_of!(ManifestRoot, makers)), makers_at, parts.makers.len()),
    };
    let root_at = out.push(&root);
    (key, out.finish(root_at))
}
