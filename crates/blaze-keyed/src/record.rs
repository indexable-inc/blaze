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

use alloc::vec::Vec;
use core::{alloc::Allocator, fmt, mem::offset_of};

use blaze_format::{FormatError, Record, Slice, Writer, WriterOpts};
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

/// The address and result of the [`Entry`] of a silent item: one the key task names (text) and
/// its unit's surface does not (no maker row: text the def map does not reach, such as an item
/// only an unused macro arm holds). No body task runs for it and no record is filed; its shape
/// is empty. Not a digest of anything, so no record is ever filed at it.
pub const SILENT: ContentId = ContentId([0; 32]);

blaze_format::record! {
    /// One item of a unit's chunk: its stable id, its key, the address its record is filed at,
    /// the record's result digest ([`result_digest`]), its shape (`shape_len` ordinals into the
    /// chunk's dictionary at `shape_at` of `shapes`) and its kind. A silent item's address and
    /// result are [`SILENT`].
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
// `PlanPayload` and `ProgramKeptRoot` are payloads inside a headed record, not roots: no head.

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
    /// Another crate's settings (its `Crate` input's content id): a row of that crate's
    /// surface, as its surface key covers its settings.
    CrateSettings = 41,
    // Byte 42 is retired, unused forever: it was `BlockScope`, a block's def map as one fact.
    // A body that reads its own block now lists the block's lookups as ordinary rows
    // (`Resolves`, `TraitsInScope`, `WholeScope`, `Modules`, `CrateNames`), so nothing makes
    // or asks it; an id persisted with byte 42 is no kind.
    /// Every module of a crate (a walk of them): a row of the crate's surface.
    Modules = 43,
    /// Which body makes the facts about a subject: one row of the subject's crate's surface per
    /// body-bearing definition, its id [`maker_row`] of the subject (formed from any made
    /// fact's id about it by [`maker_row_of`]), its digest the body's text id (the
    /// [`StableId`] the unit's key task gives the item). What a driver finds a demanded maker
    /// by when the maker's unit's bodies are not checked; no task reads it.
    Maker = 44,
    /// What a plan reads of a body: the body record's result digest ([`result_digest`]), its
    /// subject the body's item ([`mir_id`]). Made by the body, so a plan of an item of a unit
    /// whose bodies are not asked demands its maker as a body's reader does a const's value. No
    /// unit's made table holds it (a body edit moves no universe digest, so no dependent's
    /// mark): the driver answers it from the bodies it settled or the manifests that name them.
    Mir = 45,
    /// A file of a unit as a span table reads it: its id [`span_file_id`] of the unit's owner
    /// and the name the program prints for the file ([`printed_name`]), its digest
    /// [`span_file_digest`] of the file's image key, so a read covers the file's text (every
    /// base and position a table holds) and its printed name. Owned by the unit and answered by
    /// the driver alone from the unit's key rows this request ([`crate::front::Keyed::names`]):
    /// no table holds it ([`crate::front::Unanswerable::DriverOwn`]). Only span records read it.
    SpanFile = 46,
}

impl FactKind {
    /// Every kind, at index `byte - 1`, and `None` at a retired byte (42, once `BlockScope`).
    /// A new kind takes the next byte: an id's byte is persisted, so no kind is ever
    /// renumbered and no retired byte is ever given again.
    const ALL: [Option<FactKind>; 46] = [
        Some(FactKind::Own),
        Some(FactKind::Signature),
        Some(FactKind::Fields),
        Some(FactKind::Layout),
        Some(FactKind::ImplRows),
        Some(FactKind::Binding),
        Some(FactKind::ConstValue),
        Some(FactKind::InlineCode),
        Some(FactKind::Late),
        Some(FactKind::HiddenType),
        Some(FactKind::Escaped),
        Some(FactKind::Effects),
        Some(FactKind::Attrs),
        Some(FactKind::CrateRoot),
        Some(FactKind::TraitsInScope),
        Some(FactKind::WholeScope),
        Some(FactKind::CrateNames),
        Some(FactKind::FieldOrder),
        Some(FactKind::Setting),
        Some(FactKind::InherentAssocTypes),
        Some(FactKind::Name),
        Some(FactKind::Coherence),
        Some(FactKind::Specializes),
        Some(FactKind::DropCheck),
        Some(FactKind::Target),
        Some(FactKind::RpititTys),
        Some(FactKind::StepInfer),
        Some(FactKind::StepMir),
        Some(FactKind::StepLower),
        Some(FactKind::MatchCheck),
        Some(FactKind::Hashes),
        Some(FactKind::Method),
        Some(FactKind::Vtable),
        Some(FactKind::InherentImpls),
        Some(FactKind::TraitImpls),
        Some(FactKind::TraitsNamed),
        Some(FactKind::ScopeTraitsNamed),
        Some(FactKind::BlockImpls),
        Some(FactKind::Exports),
        Some(FactKind::Seen),
        Some(FactKind::CrateSettings),
        // 42: retired (once `BlockScope`), never given again.
        None,
        Some(FactKind::Modules),
        Some(FactKind::Maker),
        Some(FactKind::Mir),
        Some(FactKind::SpanFile),
    ];

    /// The kind of byte `byte`, if any: none for 0, a retired byte (42) or a byte past the
    /// last kind.
    pub const fn of_byte(byte: u8) -> Option<FactKind> {
        match byte {
            1..=46 => FactKind::ALL[byte as usize - 1],
            _ => None,
        }
    }

    /// Whether a body's task makes facts of this kind (its readers ask them of the maker's
    /// unit manifest); else the owner's surface does (they are rows of its table).
    pub const fn made_by_body(self) -> bool {
        matches!(self, FactKind::ConstValue | FactKind::HiddenType | FactKind::Escaped | FactKind::Effects | FactKind::Mir)
    }

    /// Whether an id of this kind of no object names its subject by [`maker_tag`] rather than
    /// by a hash ([`fact`]): the made kinds and [`FactKind::Maker`], so a made fact's id turns
    /// into its maker row's by its code and kind bytes alone ([`maker_row_of`]).
    pub const fn tags_subject(self) -> bool {
        self.made_by_body() || matches!(self, FactKind::Maker)
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
///
/// A fact a body makes ([`FactKind::made_by_body`]) of no object (`what` empty, as every made
/// fact is: blaze-front refuses a made kind with an object), and a [`FactKind::Maker`] row,
/// name their subject instead of a hash: the last 21 bytes are [`maker_tag`] of `item`, so a
/// driver asked such a fact of a unit whose bodies it does not check forms the id of the
/// subject's maker row ([`maker_row_of`]) and finds the body there.
pub fn fact(kind: FactKind, owner: Owner, code: u16, item: &StableId, what: &[u8]) -> ContentId {
    let mut id = match kind.tags_subject() && what.is_empty() {
        true => {
            let mut id = ContentId([0; 32]);
            id.0[11..].copy_from_slice(&maker_tag(item));
            id
        }
        false => fact_id(kind, item, what),
    };
    id.0[..8].copy_from_slice(&owner.0);
    id.0[8..10].copy_from_slice(&code.to_le_bytes());
    id.0[10] = kind as u8;
    id
}

/// The bytes a made fact's id and its maker row's id name their subject by: bytes 8..29 of the
/// subject (a blaze-items item key), whose first eight bytes are its crate's [`Owner`], already
/// in the id, and whose next 21 are the item's own hash.
pub fn maker_tag(subject: &StableId) -> [u8; 21] {
    let mut tag = [0; 21];
    tag.copy_from_slice(&subject.0.0[8..29]);
    tag
}

/// The query code a [`FactKind::Maker`] row is stamped with: no query reads it, so it is one
/// of the codes the front end keeps for its own rows (blaze-front's `facts::MAKER`).
pub const MAKER_CODE: u16 = 0xf005;

/// The id of the maker row of `subject` (a body-bearing definition's item key): a row of the
/// subject's crate's surface, its digest the body's text id.
pub fn maker_row(subject: &StableId) -> ContentId {
    fact(FactKind::Maker, Owner::of_subject(&subject.0), MAKER_CODE, subject, b"")
}

/// The id of the maker row of the subject made fact `id` is about: `id` with the maker code and
/// kind stamped over its bytes 8..11 (both name the subject by [`maker_tag`]). `None` for an
/// id of a kind no body makes. Floor: O(1), no table.
pub fn maker_row_of(id: &ContentId) -> Option<ContentId> {
    if !kind_of(id)?.made_by_body() {
        return None;
    }
    let mut row = *id;
    row.0[8..10].copy_from_slice(&MAKER_CODE.to_le_bytes());
    row.0[10] = FactKind::Maker as u8;
    Some(row)
}

/// The query code a [`FactKind::Mir`] fact is stamped with: no front-end query makes it; the
/// driver answers it ([`mir_id`]).
pub const MIR_CODE: u16 = 0xf006;

/// The id of the fact "the body of `subject`", as a plan reads it: a made fact of the subject's
/// crate, so [`maker_row_of`] names the subject's maker row and a body not asked is demanded by
/// it. Its digest is the body record's result digest.
pub fn mir_id(subject: &StableId) -> ContentId {
    fact(FactKind::Mir, Owner::of_subject(&subject.0), MIR_CODE, subject, b"")
}

/// The query code a [`FactKind::SpanFile`] fact is stamped with: no front-end query makes it;
/// the driver answers it ([`span_file_id`]).
pub const SPAN_FILE_CODE: u16 = 0xf007;

/// The id of a printed file name (`file!()`, a panic's location: a std file's
/// `/rustc/<commit>/library/...`, a workspace member's path from the checkout), whole: what a
/// front end's [`crate::front::Keyed::names`] holds per file. Never `ZERO`, which says a file
/// has no printed name.
pub fn printed_name(name: &[u8]) -> ContentId {
    ContentId::of("blaze-keyed printed file name", name)
}

/// The id of the fact "the file of unit `owner` the program prints as `name`" ([`printed_name`]
/// of it), as a span table reads it. By name, not by the file's place in the unit, so a file
/// added before it moves nothing, and a file renamed is another fact (its old id answers
/// `absent`). Floor: one hash.
pub fn span_file_id(owner: Owner, name: &ContentId) -> ContentId {
    fact(FactKind::SpanFile, owner, SPAN_FILE_CODE, &StableId(*name), b"")
}

/// The digest of a [`FactKind::SpanFile`] fact: the file's image key, which covers its text,
/// so every base and position a table of it holds. The name is the id's.
pub fn span_file_digest(image: &Key<crate::kind::FileImage>) -> ContentId {
    image.id()
}

blaze_format::record! {
    /// One edge of a plan ([`PlanPayload`]): an instance the planned code calls or refers to (a
    /// static, a vtable's method, drop glue), by item and its type arguments' bytes
    /// (`args[args_at..args_at + args_len]` of the payload), and what the edge is (`kind`, the
    /// codegen's own byte: the driver reads none of it). The driver keys the callee's plan from
    /// these alone ([`crate::kind::PlanRecordInputs`]), with no database.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct PlanEdge: 1 {
        pub item: ContentId,
        pub args_at: u32,
        pub args_len: u32,
        pub kind: u16,
        pub pad: u16,
        pub pad2: u32,
    }
}

blaze_format::record! {
    /// A plan record's payload: its edges, the bytes their type arguments are in, and the
    /// codegen's own description of the instance (`describe`, opaque to the driver: what a code
    /// task reads, the layouts and impls it was planned with included, so a code key over the
    /// plan's result covers them).
    #[derive(Clone, Copy, Debug)]
    pub struct PlanPayload: 1 {
        pub edges: Slice<PlanEdge>,
        pub args: Slice<u8>,
        pub describe: Slice<u8>,
    }
}

/// A plan's payload, read in place: what the driver and a walker read of a plan record.
#[derive(Clone, Copy, Debug)]
pub struct PlanView<'a> {
    pub edges: &'a [PlanEdge],
    pub args: &'a [u8],
    pub describe: &'a [u8],
}

impl<'a> PlanView<'a> {
    /// The type arguments of edge `edge`, or `None` past the payload's bytes.
    pub fn args_of(&self, edge: &PlanEdge) -> Option<&'a [u8]> {
        self.args.get(edge.args_at as usize..(edge.args_at as usize).checked_add(edge.args_len as usize)?)
    }
}

/// The bytes of a plan's payload, in `alloc`. Floor: one copy of its parts.
pub fn write_plan_payload<A: Allocator>(edges: &[PlanEdge], args: &[u8], describe: &[u8], alloc: A) -> Vec<u8, A> {
    let mut out = Writer::new(WriterOpts { schema: PlanPayload::SCHEMA, alloc: alloc, capacity: None });
    let edges_at = out.push_slice(edges);
    // Bytes go in as buffers, as a traced record's payload does.
    let args_at = out.push_buffer(args);
    let describe_at = out.push_buffer(describe);
    let at = out.next::<PlanPayload>();
    let field = |offset: usize| at + offset as u64;
    let root = PlanPayload {
        edges: Slice::between(field(offset_of!(PlanPayload, edges)), edges_at, edges.len()),
        args: Slice::between(field(offset_of!(PlanPayload, args)), args_at, args.len()),
        describe: Slice::between(field(offset_of!(PlanPayload, describe)), describe_at, describe.len()),
    };
    let root_at = out.push(&root);
    out.finish(root_at)
}

/// A plan record's payload (a [`Traced`] record's `payload`, 16-aligned in place: it is pushed as
/// a buffer), opened: every edge's arguments inside the payload's bytes.
pub fn read_plan_payload(payload: &[u8]) -> Result<PlanView<'_>, RecordError> {
    let root = blaze_format::open::<PlanPayload>(payload).map_err(RecordError::Format)?;
    let view = PlanView {
        edges: root.edges.follow(payload).map_err(RecordError::Format)?,
        args: root.args.follow(payload).map_err(RecordError::Format)?,
        describe: root.describe.follow(payload).map_err(RecordError::Format)?,
    };
    if let Some(at) = view.edges.iter().position(|edge| view.args_of(edge).is_none()) {
        return Err(RecordError::Malformed { at: at as u32, what: "plan edge arguments" });
    }
    Ok(view)
}

blaze_format::record! {
    /// One row of a kept program ([`ProgramKeptRoot`]): an instance (its plan's key), the plan's
    /// address and result, the code record's key and tier, what its code refers to as instance
    /// keys (`targets[target_at..target_at + target_len]`: first the `call_len` it calls, then
    /// the statics it reads, each part sorted, each once), and the front end's ask its plan task
    /// was given (`asks[ask_at..ask_at + ask_len]`), so a moved instance is asked by itself.
    /// What a walker's prior reads.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct KeptRow: 2 {
        pub instance: ContentId,
        pub item: ContentId,
        pub plan: ContentId,
        pub result: ContentId,
        pub code: ContentId,
        pub shape_at: u32,
        pub shape_len: u32,
        pub target_at: u32,
        pub target_len: u32,
        pub call_len: u32,
        /// The tier the walker's code key holds (blaze-program's `Tier`, by its number): the
        /// seam carries it, its meaning is the walker's.
        pub tier: u32,
        pub ask_at: u32,
        pub ask_len: u32,
    }
}

blaze_format::record! {
    /// One span table a kept program's rows name ([`ProgramKeptRoot`]): its anchor, its
    /// [`crate::kind::SpanTable`] key, its owner (the item whose span task makes it), the
    /// address its record is filed at, its result digest (what the link step was given), and
    /// the files it read with the digests seen (`span_reads[read_at..read_at + read_len]`,
    /// sorted by id; the tables of one owner share one range).
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct KeptSpan: 1 {
        pub anchor: ContentId,
        pub key: ContentId,
        pub owner: ContentId,
        pub address: ContentId,
        pub digest: ContentId,
        pub read_at: u32,
        pub read_len: u32,
    }
}

blaze_format::record! {
    /// A program's kept state, the payload of its [`crate::kind::ProgramManifest`]: its rows
    /// sorted by instance, each plan's shape as ordinals into the dictionary of every fact a plan
    /// read (with the digest seen), the postings (dictionary ordinal to rows reading it), every
    /// row's ask bytes, the unit manifests its reads of bodies were answered against
    /// (`manifests`, sorted, each once), the span tables its rows name (`spans`, sorted by
    /// anchor, each once, with their reads in `span_reads`; row `r`'s are the ordinals
    /// `row_spans[row_span_at[r]..row_span_at[r + 1]]`), and the linker's own state (`linker`,
    /// opaque here).
    #[derive(Clone, Copy, Debug)]
    pub struct ProgramKeptRoot: 3 {
        pub program: ContentId,
        pub manifests: Slice<ContentId>,
        pub rows: Slice<KeptRow>,
        pub targets: Slice<ContentId>,
        pub asks: Slice<u8>,
        pub shapes: Slice<u32>,
        pub dictionary: Slice<Fact>,
        pub postings: Slice<u32>,
        pub posting_at: Slice<u32>,
        pub spans: Slice<KeptSpan>,
        pub span_reads: Slice<Fact>,
        pub row_spans: Slice<u32>,
        pub row_span_at: Slice<u32>,
        pub linker: Slice<u8>,
    }
}

/// The parts of a kept program ([`ProgramKeptRoot`]).
#[derive(Clone, Copy, Debug)]
pub struct KeptParts<'a> {
    pub program: ContentId,
    /// Every unit manifest (by address) whose bodies the rows' reads were answered against:
    /// a body of a unit whose manifest now is one of them stands without being read again.
    pub manifests: &'a [ContentId],
    pub rows: &'a [KeptRow],
    pub targets: &'a [ContentId],
    pub asks: &'a [u8],
    pub shapes: &'a [u32],
    pub dictionary: &'a [Fact],
    pub postings: &'a [u32],
    pub posting_at: &'a [u32],
    /// The span tables the rows name, sorted by anchor, each once.
    pub spans: &'a [KeptSpan],
    pub span_reads: &'a [Fact],
    /// Ordinals into `spans`: row `r`'s are `row_spans[row_span_at[r]..row_span_at[r + 1]]`,
    /// sorted, each once (`row_span_at` has one more entry than `rows`).
    pub row_spans: &'a [u32],
    pub row_span_at: &'a [u32],
    pub linker: &'a [u8],
}

impl<'a> KeptParts<'a> {
    /// The kept span of `anchor` (one binary search).
    pub fn span(&self, anchor: &ContentId) -> Option<&'a KeptSpan> {
        self.spans.binary_search_by(|span| span.anchor.cmp(anchor)).ok().map(|at| &self.spans[at])
    }

    /// The files `span` read, with the digests seen.
    pub fn span_reads_of(&self, span: &KeptSpan) -> &'a [Fact] {
        &self.span_reads[span.read_at as usize..(span.read_at + span.read_len) as usize]
    }

    /// The ordinals into `spans` of the tables row `r` names.
    pub fn spans_of_row(&self, r: usize) -> &'a [u32] {
        &self.row_spans[self.row_span_at[r] as usize..self.row_span_at[r + 1] as usize]
    }
}

/// The bytes of a kept program's payload, in `alloc`. Floor: one copy of its parts.
pub fn write_kept<A: Allocator>(parts: &KeptParts<'_>, alloc: A) -> Vec<u8, A> {
    let mut out = Writer::new(WriterOpts { schema: ProgramKeptRoot::SCHEMA, alloc: alloc, capacity: None });
    let manifests_at = out.push_slice(parts.manifests);
    let rows_at = out.push_slice(parts.rows);
    let targets_at = out.push_slice(parts.targets);
    let asks_at = out.push_buffer(parts.asks);
    let shapes_at = out.push_slice(parts.shapes);
    let dictionary_at = out.push_slice(parts.dictionary);
    let postings_at = out.push_slice(parts.postings);
    let posting_at_at = out.push_slice(parts.posting_at);
    let spans_at = out.push_slice(parts.spans);
    let span_reads_at = out.push_slice(parts.span_reads);
    let row_spans_at = out.push_slice(parts.row_spans);
    let row_span_at_at = out.push_slice(parts.row_span_at);
    let linker_at = out.push_buffer(parts.linker);
    let at = out.next::<ProgramKeptRoot>();
    let field = |offset: usize| at + offset as u64;
    let root = ProgramKeptRoot {
        program: parts.program,
        manifests: Slice::between(field(offset_of!(ProgramKeptRoot, manifests)), manifests_at, parts.manifests.len()),
        rows: Slice::between(field(offset_of!(ProgramKeptRoot, rows)), rows_at, parts.rows.len()),
        targets: Slice::between(field(offset_of!(ProgramKeptRoot, targets)), targets_at, parts.targets.len()),
        asks: Slice::between(field(offset_of!(ProgramKeptRoot, asks)), asks_at, parts.asks.len()),
        shapes: Slice::between(field(offset_of!(ProgramKeptRoot, shapes)), shapes_at, parts.shapes.len()),
        dictionary: Slice::between(field(offset_of!(ProgramKeptRoot, dictionary)), dictionary_at, parts.dictionary.len()),
        postings: Slice::between(field(offset_of!(ProgramKeptRoot, postings)), postings_at, parts.postings.len()),
        posting_at: Slice::between(field(offset_of!(ProgramKeptRoot, posting_at)), posting_at_at, parts.posting_at.len()),
        spans: Slice::between(field(offset_of!(ProgramKeptRoot, spans)), spans_at, parts.spans.len()),
        span_reads: Slice::between(field(offset_of!(ProgramKeptRoot, span_reads)), span_reads_at, parts.span_reads.len()),
        row_spans: Slice::between(field(offset_of!(ProgramKeptRoot, row_spans)), row_spans_at, parts.row_spans.len()),
        row_span_at: Slice::between(field(offset_of!(ProgramKeptRoot, row_span_at)), row_span_at_at, parts.row_span_at.len()),
        linker: Slice::between(field(offset_of!(ProgramKeptRoot, linker)), linker_at, parts.linker.len()),
    };
    let root_at = out.push(&root);
    out.finish(root_at)
}

/// A kept program's payload, opened: every part in bounds, the rows sorted by instance, the
/// dictionary by id, every ordinal and posting inside its table.
pub fn read_kept(payload: &[u8]) -> Result<KeptParts<'_>, RecordError> {
    let root = blaze_format::open::<ProgramKeptRoot>(payload).map_err(RecordError::Format)?;
    let f = RecordError::Format;
    let parts = KeptParts {
        program: root.program,
        manifests: root.manifests.follow(payload).map_err(f)?,
        rows: root.rows.follow(payload).map_err(f)?,
        targets: root.targets.follow(payload).map_err(f)?,
        asks: root.asks.follow(payload).map_err(f)?,
        shapes: root.shapes.follow(payload).map_err(f)?,
        dictionary: root.dictionary.follow(payload).map_err(f)?,
        postings: root.postings.follow(payload).map_err(f)?,
        posting_at: root.posting_at.follow(payload).map_err(f)?,
        spans: root.spans.follow(payload).map_err(f)?,
        span_reads: root.span_reads.follow(payload).map_err(f)?,
        row_spans: root.row_spans.follow(payload).map_err(f)?,
        row_span_at: root.row_span_at.follow(payload).map_err(f)?,
        linker: root.linker.follow(payload).map_err(f)?,
    };
    sorted(parts.dictionary)?;
    if let Some(at) = parts.manifests.windows(2).position(|pair| pair[0] >= pair[1]) {
        return Err(RecordError::Unsorted { at: at as u32 + 1 });
    }
    if let Some(at) = parts.rows.windows(2).position(|pair| pair[0].instance >= pair[1].instance) {
        return Err(RecordError::Unsorted { at: at as u32 + 1 });
    }
    let inside = |at: u32, len: u32, of: usize| (at as usize).checked_add(len as usize).is_some_and(|end| end <= of);
    if let Some(at) = parts.rows.iter().position(|row| {
        !inside(row.shape_at, row.shape_len, parts.shapes.len())
            || !inside(row.target_at, row.target_len, parts.targets.len())
            || row.call_len > row.target_len
            || !inside(row.ask_at, row.ask_len, parts.asks.len())
    }) {
        return Err(RecordError::Malformed { at: at as u32, what: "kept program row" });
    }
    if let Some(at) = parts.shapes.iter().position(|&ord| ord as usize >= parts.dictionary.len()) {
        return Err(RecordError::Malformed { at: at as u32, what: "kept program shape" });
    }
    if parts.posting_at.len() != parts.dictionary.len() + 1
        || parts.posting_at.windows(2).any(|pair| pair[0] > pair[1])
        || parts.posting_at.last().is_some_and(|&end| end as usize > parts.postings.len())
        || parts.postings.iter().any(|&row| row as usize >= parts.rows.len())
    {
        return Err(RecordError::Malformed { at: 0, what: "kept program postings" });
    }
    if let Some(at) = parts.spans.windows(2).position(|pair| pair[0].anchor >= pair[1].anchor) {
        return Err(RecordError::Unsorted { at: at as u32 + 1 });
    }
    if let Some(at) = parts.spans.iter().position(|span| {
        !inside(span.read_at, span.read_len, parts.span_reads.len())
            || parts.span_reads[span.read_at as usize..(span.read_at + span.read_len) as usize]
                .iter()
                .any(|read| kind_of(&read.id) != Some(FactKind::SpanFile))
    }) {
        return Err(RecordError::Malformed { at: at as u32, what: "kept program span table" });
    }
    if parts.row_span_at.len() != parts.rows.len() + 1
        || parts.row_span_at.windows(2).any(|pair| pair[0] > pair[1])
        || parts.row_span_at.first().is_some_and(|&start| start != 0)
        || parts.row_span_at.last().is_some_and(|&end| end as usize != parts.row_spans.len())
        || parts.row_spans.iter().any(|&span| span as usize >= parts.spans.len())
    {
        return Err(RecordError::Malformed { at: 0, what: "kept program row spans" });
    }
    Ok(parts)
}

/// The query code stamped in `id`.
pub fn code_of(id: &ContentId) -> u16 {
    u16::from_le_bytes([id.0[8], id.0[9]])
}

/// The kind stamped in `id`, if it is one.
pub fn kind_of(id: &ContentId) -> Option<FactKind> {
    FactKind::of_byte(id.0[10])
}

/// The id of a late read: `which` macro (its [`crate::kind::Late`] byte) of `name`. It names no
/// unit, and need not: every table it is looked up in (a unit's dictionary, a record's reads, a
/// task's universe, whose late table is its own unit's) is one unit's, so two units that read
/// one name with two values never meet under it. A table that mixed units would need the unit
/// in the id.
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
    let mut out = Writer::on_the_global_heap_until_ported(Traced::SCHEMA);
    let reads_at = out.push_slice(reads);
    let facts_at = out.push_slice(facts);
    // Aligned as a buffer is: a payload that is itself a blaze-format buffer (a surface's
    // sections, a file image) is opened in place, where the record is mapped.
    let payload_at = out.push_buffer(payload);
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
    let mut out = Writer::on_the_global_heap_until_ported(Opaque::SCHEMA);
    // Aligned as a buffer is: a payload that is itself a blaze-format buffer (a surface's
    // sections, a file image) is opened in place, where the record is mapped.
    let payload_at = out.push_buffer(payload);
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
    let mut out = Writer::on_the_global_heap_until_ported(ShapeRoot::SCHEMA);
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
    let mut out = Writer::on_the_global_heap_until_ported(MarkRoot::SCHEMA);
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
    let mut out = Writer::on_the_global_heap_until_ported(ChunkRoot::SCHEMA);
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
    let mut out = Writer::on_the_global_heap_until_ported(ManifestRoot::SCHEMA);
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
