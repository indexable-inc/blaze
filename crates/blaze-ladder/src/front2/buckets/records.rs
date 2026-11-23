//! E3, the impl tables (docs/v2/plan/design/engine.md, D131): each crate's impls filed ONCE,
//! by a pure function of its impl rows and the names, into blaze-surface's records (the
//! buckets, the directory and `NoImplsOf`), and read through blaze-impls' lookups. They replace
//! the per-job [`Buckets`] filing and the items record's bucket section, which go when the
//! engine runs `check`; until then [`BucketSource`] is the seam both stand behind (rule 78), and
//! tests/front2_buckets_order.rs holds them equal.
//!
//! **The writer** ([`write_impl_tables`]) files every row of a crate as [`Buckets::admit`] did
//! ([`file_rows`]: by the row, else the header's text, else what the row read) and writes each
//! filing as one [`surface::ImplRow`]:
//!
//! | Filed | row |
//! |---|---|
//! | `Trait(t, s, bare)` | trait `t`, shape `s`, `BARE` when bare |
//! | `Negative(t, s)` | trait `t`, shape `s`, `NEGATIVE`: in the shape's bucket, never every one of the trait's |
//! | `Inherent(s)` | no trait, shape `s` |
//! | `Unread(why, s)`, a trait impl | `TRAIT_UNREAD` at `s`, `BARE` when its trait is written bare, why |
//! | `Unread(why, _)`, any other | `BLIND`, why |
//! | `NoCheckerType` | none (counted) |
//!
//! A shape not read (`None`) is [`SelfShape::BLANKET`]: with no flag for a blanket by its
//! tokens (`impl<T> Tr for T`), `SELF_NO_TYPE` for a self path read to no constructor (its
//! [`PathSelf`] bound is a fact of the row and the reader's crates), `SELF_UNREAD` otherwise.
//! A checker key (16 bytes) is written as the library key it begins
//! ([`NamesSource::key_beginning`]); a shape the writer cannot write is a typed refusal, never a
//! row left out. The row's header id is the digest of the items record's row the impl was filed
//! from ([`row_digest`]), and the tables' key is the digest of all of them: from text, before
//! the work.
//!
//! **The reader** ([`Buckets::admit_tables`]) asks blaze-impls' `candidates_in` /
//! `inherent_candidates_in` over the crates the question's home admits (the orphan rules, as
//! [`Buckets::admit_asked`] chose them), at each shape the question's shape stands for, and
//! files each row found as a [`Filing`] again. Every read a lookup makes is the question's read
//! list ([`BucketRead`]).
//!
//! A negative impl is found at its own shape (and the trait's blanket bucket), so
//! [`Buckets::negative_of`] answers by the question's shape on both sources. What differs from
//! the index by design, the answers equal: a trait row whose lowered header names no trait is the question's gap
//! ([`Buckets::by_text`] holds every trait row), never an impl left out unseen.

use core::cell::{Cell, RefCell};
use core::fmt;

use blaze_drive as drive;
use blaze_ids::Hasher;
use blaze_impls::{ImplLookupError, ImplRead, ImplRecords, Probe, RecordKind, candidates_in, inherent_candidates_in};
use blaze_own::{AVec, Alloc};
use blaze_scope::items_record::SelfHead;
use blaze_surface::{self as surface, ContentId, CrateImpls, CrateItemKey, FloatWidth, ImplIndexError, IntWidth, Mutability, SelfShape, UintWidth};
use blaze_typeck::{FloatTy, IntTy, UintTy};

use super::*;
use crate::front2::source::Records;
use crate::front2::tables::recorder::{ReadsFull, Recorder, RecorderOpts, TableRead};
use crate::front2::tables::view::{Tables, TablesOpts};

/// The domain of [`row_digest`].
const ROW_DOMAIN: &str = "blaze-ladder impl table row 1";
/// The domain of [`ImplTables::key`].
const TABLES_DOMAIN: &str = "blaze-ladder impl tables 1";
/// The domain of a [`Probe::Trait`]'s name in a [`BucketRead`].
const PROBE_DOMAIN: &str = "blaze-ladder impl table probe 1";
/// The domain of a [`Probe::Bucket`]'s name in a [`BucketRead`].
const PROBE_BUCKET_DOMAIN: &str = "blaze-ladder impl table bucket probe 1";

/// One crate's impl tables as [`write_impl_tables`] made them.
pub struct ImplTables<'a> {
    pub krate: CrateItemKey,
    /// What the tables are found by: the digest of the crate's key and its impl rows' digests
    /// ([`row_digest`]), sorted by impl key. From text, before the work; the names the filing
    /// read are [`ImplTables::named`].
    pub key: ContentId,
    pub records: CrateImpls<'a>,
    /// The crates whose items the rows were resolved to (each trait's, each nominal self
    /// type's), sorted: what of the names the filing's answers name. A path walked through
    /// another crate's re-export reads that crate's names too, which only the names' cell
    /// reads will list (engine.md E4); until then a reader that must be sound takes the
    /// crate's dependencies' names records as the reads.
    pub named: AVec<'a, CrateItemKey>,
    pub counts: FileCounts,
    /// Impls on a self type the checker has no type for (blaze's `i80`): in no bucket.
    pub no_checker_type: u32,
    /// The cells the tables define (drive cells.rs: a 128-bit key and its digest), sorted by
    /// key: one per bucket ([`bucket_cell`]: the bucket's hash, its record's content id) and one
    /// per known trait ([`trait_cell`], [`TRAIT_HELD`]). A crate with any unread trait or
    /// blind impl also defines the shared [`Probe::UnreadTrait`] cell. A lookup's reads
    /// map onto them by [`cell_read`]; a cell a reader finds no row for is absent, a value it
    /// reads, so an impl added at a new shape or of a new trait dirties exactly the readers
    /// that probed that bucket or that trait.
    pub defines: AVec<'a, ([u8; 16], ContentId)>,
}

/// The domain of [`trait_cell`].
const TRAIT_CELL_DOMAIN: &str = "blaze-ladder impl tables trait cell 1";

/// What a [`trait_cell`] holds when the crate has an impl of the trait.
pub const TRAIT_HELD: ContentId = ContentId([1; 32]);

/// The crate-wide part of the trait precheck. The first unread or blind bucket defines
/// the cell, the last one's removal makes it absent, and each bucket has its own content cell.
const UNREAD_TRAIT_CELL: [u8; 16] = *b"unread traits v1";

/// The cell of the tables that says whether the crate has an impl of `trait_`: what a
/// `NoImplsOf` trait probe reads.
pub fn trait_cell(trait_: LibKey) -> [u8; 16] {
    let id = ContentId::of(TRAIT_CELL_DOMAIN, &trait_.0.0);
    let mut cell = [0u8; 16];
    cell.copy_from_slice(&id.0[..16]);
    cell
}

/// The cell of the tables for the bucket `hash`: what a directory lookup (and a `NoImplsOf`
/// bucket probe) reads. Its digest is the bucket record's content id, so a bucket record's
/// own read is covered by it.
pub fn bucket_cell(hash: surface::BucketHash) -> [u8; 16] {
    hash.0
}

fn probe_cell(probe: Probe) -> [u8; 16] {
    match probe {
        Probe::Trait(t) => trait_cell(t),
        Probe::UnreadTrait => UNREAD_TRAIT_CELL,
        Probe::Bucket(hash) => bucket_cell(hash),
    }
}

/// The cell of its crate's tables one lookup read maps to (module docs): `None` for a bucket
/// record's read, which its directory cell covers.
pub fn cell_read(read: &ImplRead) -> Option<[u8; 16]> {
    match read {
        ImplRead::Filter { probe, .. } => Some(probe_cell(*probe)),
        ImplRead::Directory { bucket, .. } => Some(bucket_cell(*bucket)),
        ImplRead::Bucket { .. } => None,
    }
}

/// Maps a physical lookup note to the task read a body's recorder lists (front2
/// tables/recorder.rs [`TableRead::Impls`]): the crate's impl tables (its key's first 16 bytes:
/// [`ReadOf::Impls`](crate::front2::tables::recorder::ReadOf::Impls), whose record is the crate's
/// [`impl_tables_name`]) at the cell [`cell_read`] gives. A body that found NO impl reads a cell
/// too: the trait's cell where `NoImplsOf` said none, the bucket's where the directory has none;
/// both are absent today, and an impl added there makes them present, which dirties the body
/// (rule 21).
pub fn cells_of(read: &ImplRead) -> Option<TableRead> {
    let krate = match read {
        ImplRead::Filter { krate, .. } | ImplRead::Directory { krate, .. } | ImplRead::Bucket { krate, .. } => *krate,
    };
    let mut k = [0u8; 16];
    k.copy_from_slice(&krate.0.0[..16]);
    cell_read(read).map(|bucket| TableRead::Impls { krate: k, bucket })
}

/// The domain of [`impl_tables_name`]'s subject.
const SUBJECT_DOMAIN: &str = "blaze-ladder impl tables subject 1";
/// The domain of [`ImplTablesDone::out`].
const OUT_DOMAIN: &str = "blaze-ladder impl tables out 1";

/// The engine's name of crate `krate`'s impl tables task (`Kind::ImplBucket`, one per crate):
/// the record a body's [`TableRead::Impls`] of that crate is a read of.
pub fn impl_tables_name(krate: CrateItemKey) -> drive::Name {
    drive::Name::new(drive::Kind::ImplBucket, ContentId::of(SUBJECT_DOMAIN, &krate.0.0))
}

/// Why the impl tables task failed: its row is the failure, its `defines` empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImplTablesFailed {
    Tables(TablesError),
    Reads(ReadsFull),
}

/// The impl tables task's result in the engine's form (blaze-engine engine.rs `Done`): what
/// the driver turns into its `Done` by allocator and by naming each read's record.
pub struct ImplTablesDone<'a> {
    /// [`impl_tables_name`].
    pub name: drive::Name,
    /// [`ImplTables::key`]: from the rows' text, before the work.
    pub key: drive::Key,
    /// The digest of the three records (the directory names every bucket's content id).
    pub out: ContentId,
    /// The records; `None` when the task failed.
    pub records: Option<CrateImpls<'a>>,
    /// [`ImplTables::defines`] as drive cells, sorted; empty when failed.
    pub defines: AVec<'a, (drive::Cell, ContentId)>,
    /// What the filing read, sorted and each once: the names cells its walks and rows consulted
    /// and each header text it lowered (front2 tables/view.rs records them). A read's record is
    /// [`TableRead::of`] and its cell [`TableRead::cell`]; its digest is decided by the merge
    /// from the same tables a later run checks it against (names_cells.rs `seen_in`).
    pub reads: AVec<'a, TableRead>,
    pub counts: FileCounts,
    pub failed: Option<ImplTablesFailed>,
}

/// The digest of a crate's written impl tables (`OUT_DOMAIN`).
pub fn impl_tables_out(records: &CrateImpls<'_>) -> ContentId {
    let mut out = Hasher::new(OUT_DOMAIN);
    out.part(&records.directory).part(&records.no_impls_of);
    out.finish()
}

/// THE TASK: crate `krate`'s impl tables from its impl rows `rows` (the task's input, which its
/// key covers), each name the filing asks answered through the read-only view over `records`
/// with its cells recorded, its buffers in `alloc`. Pure in (rows, records, alloc).
pub fn impl_tables_task<'n, 'a>(records: &'n Records<'n>, krate: usize, rows: &[ImplRow<'n>], alloc: Alloc<'a>) -> ImplTablesDone<'a> {
    let recorder = Recorder::new(RecorderOpts { alloc });
    // The writer asks no impl question and names no lang item: no tables, no lang crates.
    let view = Tables::new(TablesOpts { records, impls: None, lang_crates: &[], reads: &recorder });
    let written = write_impl_tables(view, krate, rows, alloc);
    let reads = recorder.finish();
    let failed = match (written, reads) {
        (Ok(t), Ok(reads)) => {
            let out = impl_tables_out(&t.records);
            let mut defines: AVec<'a, (drive::Cell, ContentId)> = AVec::with_capacity_in(t.defines.len(), alloc);
            defines.extend(t.defines.iter().map(|(cell, digest)| (drive::Cell(*cell), *digest)));
            return ImplTablesDone { name: impl_tables_name(t.krate), key: drive::Key(t.key), out, records: Some(t.records), defines, reads, counts: t.counts, failed: None };
        }
        (Err(e), _) => ImplTablesFailed::Tables(e),
        (Ok(_), Err(e)) => ImplTablesFailed::Reads(e),
    };
    // A crate with no key fails as `NoCrateKey`; its row is named by the all-zero key.
    let crate_key = NamesSource::crate_key(records, krate).unwrap_or(CrateItemKey(ContentId([0; 32])));
    ImplTablesDone {
        name: impl_tables_name(crate_key),
        key: drive::Key(rows_key(crate_key, rows)),
        out: ContentId::of(OUT_DOMAIN, b"failed"),
        records: None,
        defines: AVec::new_in(alloc),
        reads: AVec::new_in(alloc),
        counts: FileCounts::default(),
        failed: Some(failed),
    }
}

/// Why a crate's impl tables were not written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TablesError {
    /// The source names no key for the crate.
    NoCrateKey { krate: usize },
    /// A filing names an impl that is none of the crate's rows.
    NoRow { impl_: LibKey },
    /// A checker key the filing of `impl_` named begins no library key the source holds.
    LibKey { impl_: LibKey, key: ItemKey },
    /// A self type's shape no impl row is written at (a fn item, an opaque type, a parameter,
    /// `{integer}`): the filing gave one the writer has no form for.
    Shape { impl_: LibKey, shape: Shape },
    /// A blanket impl by its tokens filed at a shape: the two readings of one header disagree.
    BlanketAtShape { impl_: LibKey, shape: Shape },
    /// The definition's key is refused as a shape.
    SelfShape { impl_: LibKey, error: surface::ShapeError },
    /// blaze-surface refused the index as a whole.
    Index(ImplIndexError),
}

impl fmt::Display for TablesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TablesError::NoCrateKey { krate } => write!(f, "the crate {krate} has no key in the source; its impl tables are not written"),
            TablesError::NoRow { impl_ } => write!(f, "the impl {impl_:x} was filed but is none of its crate's impl rows"),
            TablesError::LibKey { impl_, key } => write!(f, "the impl {impl_:x} names the checker key {key:?}, which begins no library key of the records"),
            TablesError::Shape { impl_, shape } => write!(f, "the impl {impl_:x} was filed at {shape:?}, a shape no impl row is written at"),
            TablesError::BlanketAtShape { impl_, shape } => write!(f, "the impl {impl_:x} is a blanket impl by its header's tokens but was filed at {shape:?}"),
            TablesError::SelfShape { impl_, error } => write!(f, "the self shape of the impl {impl_:x}: {error}"),
            TablesError::Index(error) => write!(f, "the impl index: {error}"),
        }
    }
}

impl core::error::Error for TablesError {}

/// The digest of an items record's impl row: every fact the impl is filed from that is the
/// row's own (its key, module, paths and roots as written, its head and flags). The row's
/// header id in the tables.
pub fn row_digest(row: &ImplRow<'_>) -> ContentId {
    let mut h = Hasher::new(ROW_DOMAIN);
    h.update(&row.key.0.0).update(&row.module.0.0);
    h.opt_part(row.trait_path).opt_part(row.self_path);
    h.update(&row.flags.bits().to_le_bytes());
    h.update(&[u8::from(row.has_trait), u8::from(row.bare), u8::from(row.header_unread), u8::from(row.trait_unread), u8::from(row.refused_call)]);
    let (head, n) = match row.head {
        SelfHead::Path => (1u8, 0u16),
        SelfHead::Param => (2, 0),
        SelfHead::Ref => (3, 0),
        SelfHead::RefMut => (4, 0),
        SelfHead::PtrConst => (5, 0),
        SelfHead::PtrMut => (6, 0),
        SelfHead::Slice => (7, 0),
        SelfHead::Array => (8, 0),
        SelfHead::Never => (9, 0),
        SelfHead::Tuple(n) => (10, n),
        SelfHead::Other => (11, 0),
    };
    h.update(&[head]).update(&n.to_le_bytes());
    h.opt_part(row.trait_root.map(|k| k.0.0)).opt_part(row.self_root.map(|k| k.0.0));
    h.finish()
}

/// [`ImplTables::key`]: the crate's key, then its rows' digests in impl key order.
fn key_of(crate_key: CrateItemKey, digests: impl Iterator<Item = ContentId>) -> ContentId {
    let mut key = Hasher::new(TABLES_DOMAIN);
    key.update(&crate_key.0.0);
    for digest in digests {
        key.id(&digest);
    }
    key.finish()
}

/// [`key_of`] of `rows` in any order.
pub fn rows_key(crate_key: CrateItemKey, rows: &[ImplRow<'_>]) -> ContentId {
    let mut by_key: Vec<(LibKey, ContentId), Global> = Vec::with_capacity_in(rows.len(), Global);
    by_key.extend(rows.iter().map(|row| (row.key, row_digest(row))));
    by_key.sort_unstable_by_key(|it| it.0);
    key_of(crate_key, by_key.iter().map(|it| it.1))
}

/// One contiguous chunk of a crate's impl rows filed independently ([`file_impl_rows_chunk`]),
/// combined with every other chunk by [`finish_impl_tables`] so large crates can file impls in
/// parallel across workers while keeping identical output bytes.
pub struct ImplChunk<A: core::alloc::Allocator> {
    pub key_digests: blaze_own::Vec<(LibKey, ContentId), A>,
    pub out: blaze_own::Vec<surface::ImplRow, A>,
    pub named: blaze_own::Vec<CrateItemKey, A>,
    pub counts: FileCounts,
    pub no_checker_type: u32,
}

/// Files one contiguous slice `rows` of crate `krate`'s impl rows against `names`, returning its
/// filed rows and row digests for [`finish_impl_tables`].
pub fn file_impl_rows_chunk<'n, A: core::alloc::Allocator + Copy, S: NamesSource<'n>>(names: S, krate: usize, rows: &[ImplRow<'n>], alloc: A) -> Result<ImplChunk<A>, TablesError> {
    let mut by_key: blaze_own::Vec<(LibKey, ContentId, surface::ImplFlags), A> = blaze_own::Vec::with_capacity_in(rows.len(), alloc);
    by_key.extend(rows.iter().map(|row| (row.key, row_digest(row), row.flags.intersect(surface::ImplFlags::HEADER))));
    by_key.sort_unstable_by_key(|it| it.0);
    let mut key_digests: blaze_own::Vec<(LibKey, ContentId), A> = blaze_own::Vec::with_capacity_in(by_key.len(), alloc);
    key_digests.extend(by_key.iter().map(|it| (it.0, it.1)));
    let mut out: blaze_own::Vec<surface::ImplRow, A> = blaze_own::Vec::with_capacity_in(rows.len(), alloc);
    let mut named: blaze_own::Vec<CrateItemKey, A> = blaze_own::Vec::new_in(alloc);
    let mut no_checker_type = 0u32;
    let mut failed: Option<TablesError> = None;
    let counts = file_rows(names, krate, rows, &mut |filing| {
        if failed.is_some() {
            return;
        }
        let found = by_key.binary_search_by_key(&filing.key, |it| it.0).map(|at| by_key[at]);
        let written = match found {
            Ok((_, header, flags)) => table_row(names, &filing, header, flags),
            Err(_) => Err(TablesError::NoRow { impl_: filing.key }),
        };
        match written {
            Ok(Some(row)) => {
                for k in row.trait_.into_iter().chain(row.shape.def()) {
                    if let Some(c) = names.crate_of(k).and_then(|c| names.crate_key(c)) {
                        named.push(c);
                    }
                }
                out.push(row);
            }
            Ok(None) => no_checker_type += 1,
            Err(error) => failed = Some(error),
        }
    });
    if let Some(error) = failed {
        return Err(error);
    }
    Ok(ImplChunk { key_digests, out, named, counts, no_checker_type })
}

/// Combines filed chunks ([`file_impl_rows_chunk`]) of crate `crate_key` into deterministic
/// [`ImplTables`] (order-independent across chunk splits).
pub fn finish_impl_tables<'a, A: core::alloc::Allocator>(crate_key: CrateItemKey, chunks: &[ImplChunk<A>], alloc: Alloc<'a>) -> Result<ImplTables<'a>, TablesError> {
    let total_digests: usize = chunks.iter().map(|c| c.key_digests.len()).sum();
    let mut all_digests: AVec<'a, (LibKey, ContentId)> = AVec::with_capacity_in(total_digests, alloc);
    for c in chunks.iter() {
        all_digests.extend_from_slice(&c.key_digests);
    }
    all_digests.sort_unstable_by_key(|it| it.0);
    let key = key_of(crate_key, all_digests.iter().map(|it| it.1));
    let total_out: usize = chunks.iter().map(|c| c.out.len()).sum();
    let mut out: AVec<'a, surface::ImplRow> = AVec::with_capacity_in(total_out, alloc);
    let mut named: AVec<'a, CrateItemKey> = AVec::new_in(alloc);
    let mut counts = FileCounts::default();
    let mut no_checker_type = 0u32;
    for c in chunks.iter() {
        out.extend_from_slice(&c.out);
        named.extend_from_slice(&c.named);
        counts.from_rows = counts.from_rows.saturating_add(c.counts.from_rows);
        counts.from_text = counts.from_text.saturating_add(c.counts.from_text);
        for (dst, src) in counts.row_misses.iter_mut().zip(c.counts.row_misses.iter()) {
            *dst = dst.saturating_add(*src);
        }
        no_checker_type = no_checker_type.saturating_add(c.no_checker_type);
    }
    named.sort_unstable();
    named.dedup();
    let records = surface::build_impl_index(crate_key, &out, alloc).map_err(TablesError::Index)?;
    let mut defines: AVec<'a, ([u8; 16], ContentId)> = AVec::with_capacity_in(records.buckets.len() + out.len(), alloc);
    defines.extend(records.buckets.iter().map(|b| (bucket_cell(b.hash), b.id)));
    defines.extend(records.buckets.iter().filter_map(|b| b.key.trait_()).map(|t| (trait_cell(t), TRAIT_HELD)));
    if records.buckets.iter().any(|b| b.key.is_unread()) {
        defines.push((UNREAD_TRAIT_CELL, TRAIT_HELD));
    }
    defines.sort_unstable();
    defines.dedup();
    Ok(ImplTables { krate: crate_key, key, records, named, counts, no_checker_type, defines })
}

/// Crate `krate`'s impl tables from its impl rows `rows` (its items record's, in any order)
/// and the names `names` resolves them by: a pure function of the two, its buffers in
/// `alloc`. Every row is filed once ([`file_rows`]); a row blaze-surface refuses is in
/// [`CrateImpls::refused`], named.
pub fn write_impl_tables<'n, 'a, S: NamesSource<'n>>(names: S, krate: usize, rows: &[ImplRow<'n>], alloc: Alloc<'a>) -> Result<ImplTables<'a>, TablesError> {
    let crate_key = names.crate_key(krate).ok_or(TablesError::NoCrateKey { krate })?;
    let chunk = file_impl_rows_chunk(names, krate, rows, alloc)?;
    finish_impl_tables(crate_key, &[chunk], alloc)
}

/// The table row of one filing (module docs' table); `None` for an impl filed nowhere.
fn table_row<'n, S: NamesSource<'n>>(names: S, f: &Filing, header: ContentId, header_flags: surface::ImplFlags) -> Result<Option<surface::ImplRow>, TablesError> {
    use surface::ImplFlags as F;
    let impl_ = f.key;
    let begun = |k: ItemKey| names.key_beginning(k.0.to_be_bytes());
    let lib = |k: ItemKey| begun(k).ok_or(TablesError::LibKey { impl_, key: k });
    // A shape not read: blanket by tokens, a self path read to no constructor, or unread.
    let not_read = if f.blanket {
        F::NONE
    } else if f.path_self.is_some() {
        F::SELF_NO_TYPE
    } else {
        F::SELF_UNREAD
    };
    let at = |s: Option<Shape>| -> Result<(SelfShape, F), TablesError> {
        match s {
            None => Ok((SelfShape::BLANKET, not_read)),
            Some(shape) if f.blanket => Err(TablesError::BlanketAtShape { impl_, shape }),
            Some(shape) => match self_shape(shape, &begun) {
                Ok(written) => Ok((written, F::NONE)),
                Err(Unwritten::Lib(key)) => Err(TablesError::LibKey { impl_, key }),
                Err(Unwritten::Shape) => Err(TablesError::Shape { impl_, shape }),
                Err(Unwritten::Refused(error)) => Err(TablesError::SelfShape { impl_, error }),
            },
        }
    };
    let bare = |b: bool| if b { F::BARE } else { F::NONE };
    let (trait_, (shape, read), flags) = match &f.filed {
        Filed::NoCheckerType => return Ok(None),
        Filed::Trait(t, s, b) => (Some(lib(*t)?), at(*s)?, bare(*b)),
        Filed::Negative(t, s) => (Some(lib(*t)?), at(*s)?, F::NEGATIVE),
        Filed::Inherent(s) => (None, at(*s)?, F::NONE),
        Filed::Unread(why, s) if why.is_trait_impl() => (None, at(*s)?, F::TRAIT_UNREAD.union(bare(f.unread_bare)).with_why(why.why_byte())),
        // Not read far enough to tell trait from inherent: blind, its self type unread.
        Filed::Unread(why, _) => (None, (SelfShape::BLANKET, F::SELF_UNREAD), F::BLIND.with_why(why.why_byte())),
    };
    Ok(Some(surface::ImplRow { impl_: f.key, trait_, shape, flags: header_flags.union(read).union(flags), header }))
}

/// Why a checker shape has no row shape ([`self_shape`]).
enum Unwritten {
    /// The checker key of its definition begins no library key.
    Lib(ItemKey),
    /// No impl row is written at it.
    Shape,
    Refused(surface::ShapeError),
}

/// A checker shape as an impl row's, its definition's library key from `lib`.
fn self_shape(shape: Shape, lib: &dyn Fn(ItemKey) -> Option<LibKey>) -> Result<SelfShape, Unwritten> {
    use surface::Shape as S;
    let lib = |k: ItemKey| lib(k).ok_or(Unwritten::Lib(k));
    let mutability = |m: Mutbl| match m {
        Mutbl::Not => Mutability::Not,
        Mutbl::Mut => Mutability::Mut,
    };
    let written = match shape {
        Shape::Bool => S::Bool,
        Shape::Char => S::Char,
        Shape::Int(i) => S::Int(int_width(i)),
        Shape::Uint(u) => S::Uint(uint_width(u)),
        Shape::Float(x) => S::Float(float_width(x)),
        Shape::Str => S::Str,
        Shape::Never => S::Never,
        Shape::Tuple(n) => S::Tuple(n),
        Shape::Ref(m) => S::Ref(mutability(m)),
        Shape::Ptr(m) => S::Ptr(mutability(m)),
        Shape::Array => S::Array,
        Shape::Slice => S::Slice,
        Shape::Pat => S::Pat,
        Shape::FnPtr(n) => S::FnPtr(n),
        Shape::Closure => S::Closure,
        Shape::Dynamic(None) => S::DynMarker,
        Shape::Adt(k) | Shape::Foreign(k) | Shape::Dynamic(Some(k)) => S::Def(lib(k)?),
        Shape::FnDef(_) | Shape::Param | Shape::Opaque(_) | Shape::AnyInt | Shape::AnyFloat | Shape::Record(..) | Shape::Union(_) => return Err(Unwritten::Shape),
    };
    SelfShape::new(written).map_err(Unwritten::Refused)
}

fn int_width(i: IntTy) -> IntWidth {
    match i {
        IntTy::Isize => IntWidth::Isize,
        IntTy::I8 => IntWidth::I8,
        IntTy::I16 => IntWidth::I16,
        IntTy::I32 => IntWidth::I32,
        IntTy::I64 => IntWidth::I64,
        IntTy::I128 => IntWidth::I128,
    }
}

fn uint_width(u: UintTy) -> UintWidth {
    match u {
        UintTy::Usize => UintWidth::Usize,
        UintTy::U8 => UintWidth::U8,
        UintTy::U16 => UintWidth::U16,
        UintTy::U32 => UintWidth::U32,
        UintTy::U64 => UintWidth::U64,
        UintTy::U128 => UintWidth::U128,
    }
}

fn float_width(x: FloatTy) -> FloatWidth {
    match x {
        FloatTy::F16 => FloatWidth::F16,
        FloatTy::F32 => FloatWidth::F32,
        FloatTy::F64 => FloatWidth::F64,
        FloatTy::F128 => FloatWidth::F128,
    }
}

const INTS: [IntTy; 6] = [IntTy::Isize, IntTy::I8, IntTy::I16, IntTy::I32, IntTy::I64, IntTy::I128];
const UINTS: [UintTy; 6] = [UintTy::Usize, UintTy::U8, UintTy::U16, UintTy::U32, UintTy::U64, UintTy::U128];
const FLOATS: [FloatTy; 4] = [FloatTy::F16, FloatTy::F32, FloatTy::F64, FloatTy::F128];

/// A table row's shape as the checker's: `None` for [`SelfShape::BLANKET`]; `Err` for a shape
/// the writer never writes.
fn checker_shape(shape: SelfShape) -> Result<Option<Shape>, ()> {
    use surface::Shape as S;
    let m = |m: Mutability| match m {
        Mutability::Not => Mutbl::Not,
        Mutability::Mut => Mutbl::Mut,
    };
    Ok(Some(match shape.shape() {
        S::Blanket => return Ok(None),
        // The writer writes a definition only for a nominal type (`shape_step`, `row_self`).
        S::Def(k) => Shape::Adt(checker_of(k)),
        S::Bool => Shape::Bool,
        S::Char => Shape::Char,
        S::Int(w) => Shape::Int(INTS[INT_WIDTHS.iter().position(|x| *x == w).ok_or(())?]),
        S::Uint(w) => Shape::Uint(UINTS[UINT_WIDTHS.iter().position(|x| *x == w).ok_or(())?]),
        S::Float(w) => Shape::Float(FLOATS[FLOAT_WIDTHS.iter().position(|x| *x == w).ok_or(())?]),
        S::Str => Shape::Str,
        S::Array => Shape::Array,
        S::Slice => Shape::Slice,
        S::Ref(x) => Shape::Ref(m(x)),
        S::Ptr(x) => Shape::Ptr(m(x)),
        S::Never => Shape::Never,
        S::Tuple(n) => Shape::Tuple(n),
        S::FnPtr(n) => Shape::FnPtr(n),
        S::DynMarker => Shape::Dynamic(None),
        S::Closure => Shape::Closure,
        S::Pat => Shape::Pat,
        S::Coroutine | S::CoroutineWitness | S::UnsafeBinder | S::Placeholder => return Err(()),
    }))
}

/// Whether a table row at `shape` can answer a question at the checker's `asked`, as
/// [`Buckets::negative_of`] judges a filed negative: a blanket row always, a row at `asked`,
/// an integer or float row for `{integer}` or `{float}`. False for a shape the writer never
/// writes.
pub fn row_answers(shape: SelfShape, asked: Shape) -> bool {
    match checker_shape(shape) {
        Ok(filed) => negative_fits(filed, asked),
        Err(()) => false,
    }
}

const INT_WIDTHS: [IntWidth; 6] = [IntWidth::Isize, IntWidth::I8, IntWidth::I16, IntWidth::I32, IntWidth::I64, IntWidth::I128];
const UINT_WIDTHS: [UintWidth; 6] = [UintWidth::Usize, UintWidth::U8, UintWidth::U16, UintWidth::U32, UintWidth::U64, UintWidth::U128];
const FLOAT_WIDTHS: [FloatWidth; 4] = [FloatWidth::F16, FloatWidth::F32, FloatWidth::F64, FloatWidth::F128];

/// The row shapes a question at the checker's `shape` reads: one, but every width for
/// `{integer}` and `{float}`, and [`SelfShape::PLACEHOLDER`] (the blanket and unread buckets
/// alone) for a shape no impl is filed at (a fn item, an opaque type, a parameter). Refused by
/// name where the shape has no row shape: a nominal type whose library key the key map and the
/// records do not give, or a shape blaze-surface refuses. Never an empty answer in its place:
/// a question that reads no bucket answers "no impl", which is not what is known (D193 point 5).
fn asked_shapes<'n, S: NamesSource<'n>>(names: S, keys: &Keys, shape: Shape, alloc: Alloc<'static>) -> Result<AVec<'static, SelfShape>, ImplQuestionError> {
    let mut out: AVec<'static, SelfShape> = AVec::new_in(alloc);
    let lib = |k: ItemKey| keys.library_in(names, k);
    let mut put = |s: Shape| -> Result<(), ImplQuestionError> {
        let written = self_shape(s, &lib).map_err(|why| match why {
            Unwritten::Lib(key) => ImplQuestionError::NoLibraryKey { of: "the nominal type an impl question is about", key },
            Unwritten::Shape => ImplQuestionError::AskedShape { shape: s },
            Unwritten::Refused(error) => ImplQuestionError::AskedShapeRefused { shape: s, error },
        })?;
        out.push(written);
        Ok(())
    };
    match shape {
        Shape::AnyInt => {
            for i in INTS {
                put(Shape::Int(i))?;
            }
            for u in UINTS {
                put(Shape::Uint(u))?;
            }
        }
        Shape::AnyFloat => {
            for x in FLOATS {
                put(Shape::Float(x))?;
            }
        }
        Shape::FnDef(_) | Shape::Param | Shape::Opaque(_) | Shape::Record(..) | Shape::Union(_) => out.push(SelfShape::PLACEHOLDER),
        other => put(other)?,
    }
    Ok(out)
}

/// What a reader could not have of a crate's impl tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoTables {
    pub krate: CrateItemKey,
    pub record: RecordKind,
}

impl fmt::Display for NoTables {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no {} of the crate {:x} is held: its impl tables were not written", self.record, self.krate)
    }
}

/// Why a question could not be filed from the impl tables: a fault of the TABLES (or of the
/// crate list they are asked over), so every question that reads them meets it; a run stops
/// on it (blaze-ladder scope.rs `CheckFail::scope`). A fault of one question is an
/// [`ImplQuestionError`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TablesReadError {
    Lookup(ImplLookupError<NoTables>),
    /// A row at a shape the writer never writes.
    Shape { impl_: LibKey, shape: SelfShape },
    /// A row of a crate the question did not ask.
    Crate { impl_: LibKey },
    /// A crate the question's home admits has no key in the source, so its tables cannot be
    /// asked: its impls are unknown, never none (D193 point 5). Over the records the key is
    /// the crate list's, so this is a crate index past the list: the engine's fault.
    NoCrateKey { krate: usize },
}

impl fmt::Display for TablesReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TablesReadError::Lookup(error) => write!(f, "an impl table lookup: {error}"),
            TablesReadError::Shape { impl_, shape } => write!(f, "the impl table row of {impl_:x} is at {shape:?}, which no table writer writes"),
            TablesReadError::Crate { impl_ } => write!(f, "the impl table row of {impl_:x} is of no crate the question asked"),
            TablesReadError::NoCrateKey { krate } => write!(f, "crate {krate} may hold impls of the question, and the source names no key for it, so its impl tables cannot be asked"),
        }
    }
}

impl core::error::Error for TablesReadError {}

/// Why ONE impl question could not be asked, of the impl tables or of the bucket index: a fault
/// of that question's trait or self type, never of the tables, which answer every other
/// question. The body that asked it is refused by name (`CheckFail::ImplQuestion`: that test's
/// NOT RUN), never answered "no impl" (D193 point 5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImplQuestionError {
    /// The question names `key` (`of` says what it is), and the key map and the records give
    /// no library key for it, so no bucket of it can be asked.
    NoLibraryKey { of: &'static str, key: ItemKey },
    /// The question's shape has no row shape ([`self_shape`]).
    AskedShape { shape: Shape },
    /// blaze-surface refused the question's shape as a row shape.
    AskedShapeRefused { shape: Shape, error: surface::ShapeError },
}

impl fmt::Display for ImplQuestionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImplQuestionError::NoLibraryKey { of, key } => write!(f, "{of} ({:x}) has no library key in the key map or the records, so no impl of it can be asked for", key.0),
            ImplQuestionError::AskedShape { shape } => write!(f, "an impl question at {shape:?}, a shape no impl table row is written at"),
            ImplQuestionError::AskedShapeRefused { shape, error } => write!(f, "an impl question at {shape:?}, which is no impl table row shape: {error}"),
        }
    }
}

impl core::error::Error for ImplQuestionError {}

/// Why a question's impls could not be filed ([`BucketSource::admit`]): the tables' fault or the
/// question's, kept apart so a run can tell the suite's stop from one test's NOT RUN.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmitError {
    Tables(TablesReadError),
    Question(ImplQuestionError),
}

impl From<TablesReadError> for AdmitError {
    fn from(error: TablesReadError) -> AdmitError {
        AdmitError::Tables(error)
    }
}

impl From<ImplQuestionError> for AdmitError {
    fn from(error: ImplQuestionError) -> AdmitError {
        AdmitError::Question(error)
    }
}

impl fmt::Display for AdmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdmitError::Tables(error) => write!(f, "{error}"),
            AdmitError::Question(error) => write!(f, "{error}"),
        }
    }
}

impl core::error::Error for AdmitError {}

/// Every crate's impl tables of one request, made ONCE by the Heads stage's tasks and read in
/// place by every body on every worker (engine.md, "The design" 1): each record's bytes held in
/// `Global`, because a task's `Alloc` is not `Sync`; immutable once the stage has put them, so
/// `Sync` by its fields. A reader reads it through a [`TableSet::lent`], which notes its reads.
pub struct ImplShelf {
    /// Sorted by crate key, one each.
    tables: std::vec::Vec<Shelved, Global>,
}

/// One crate's tables on an [`ImplShelf`].
struct Shelved {
    krate: CrateItemKey,
    directory: std::vec::Vec<u8, Global>,
    no_impls_of: std::vec::Vec<u8, Global>,
    /// Each bucket record by its content id, sorted.
    buckets: std::vec::Vec<(ContentId, std::vec::Vec<u8, Global>), Global>,
}

/// [`ImplShelf::new`]'s choices: none yet (rule 83).
pub struct ImplShelfOpts {}

/// Two tables of one crate put on one shelf: the task is planned once a crate (rule 21).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShelvedTwice {
    pub krate: CrateItemKey,
}

impl fmt::Display for ShelvedTwice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "crate {:x}'s impl tables were put on the request's shelf twice (front2 buckets/records.rs `ImplShelf::put`)", self.krate.0)
    }
}

impl core::error::Error for ShelvedTwice {}

/// Why a crate's framed tables ([`framed`]) were not put on a shelf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShelfError {
    Twice(ShelvedTwice),
    /// The bytes are not [`framed`]'s form: cut short, not its magic, or padding that is not
    /// zeros, at byte `at`.
    Malformed { krate: CrateItemKey, at: usize },
    /// The form, but at an address not aligned to [`FRAMED_ALIGN`]: its parts cannot be read in
    /// place ([`check_framed`]; a store payload is aligned, blaze-drive record.rs
    /// `PAYLOAD_ALIGN`).
    Unaligned { krate: CrateItemKey },
}

impl fmt::Display for ShelfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShelfError::Twice(twice) => twice.fmt(f),
            ShelfError::Malformed { krate, at } => write!(f, "crate {:x}'s framed impl tables are malformed at byte {at} (front2 buckets/records.rs `framed`)", krate.0),
            ShelfError::Unaligned { krate } => write!(
                f,
                "crate {:x}'s framed impl tables are not at an address aligned to {FRAMED_ALIGN} bytes, so their records cannot be read in place (front2 buckets/records.rs `check_framed`)",
                krate.0
            ),
        }
    }
}

impl core::error::Error for ShelfError {}

/// Where a reader ([`TableSet::paged`]) finds a crate its shelf does not hold: the [`framed`]
/// payload of that crate's Heads record, read on the first ask (the ready edit, D187: only the
/// crates a body probes are read, never every crate before the bodies run). `None` means the
/// crate has no tables (its task failed, or it has no row), and the same ask answers the same
/// again. Bytes that are not [`framed`]'s form are the source's to refuse ([`check_framed`])
/// before it hands them on: it keeps its own first error for its request (rule 3), and a reader
/// handed malformed bytes panics, failing its own job (rule 20).
pub trait ShelfSource: Sync {
    fn framed(&self, krate: CrateItemKey) -> Option<&[u8]>;
}

/// `bytes` read as [`framed`]'s form, refused where they are not, all in one pass, and refused
/// at an address its parts cannot be read in place at ([`ShelfError::Unaligned`]). A
/// [`ShelfSource`] calls it before handing bytes on.
pub fn check_framed(krate: CrateItemKey, bytes: &[u8]) -> Result<(), ShelfError> {
    open_framed(krate, bytes, None).map(|_| ())
}

/// One crate's framed tables read in place: no byte copied, the buckets' places indexed into
/// `alloc` (or only counted, when `alloc` is `None`).
struct Opened<'a> {
    krate: CrateItemKey,
    directory: &'a [u8],
    no_impls_of: &'a [u8],
    /// Each bucket by its content id, sorted (the form's own order).
    buckets: Option<AVec<'a, (ContentId, &'a [u8])>>,
}

/// [`ImplShelf::put_framed`]'s reading without its copies: the same checks in the same order,
/// then the address. Each part is handed to a blaze-format reader IN PLACE, which refuses a
/// buffer not aligned to 16 (blaze-format-core `header`): form 1 packed its parts, so every
/// part past the first was refused by its address and a paged reader's lookups failed where
/// the shelf's own copies read (front2_tables_task.rs `a_paged_shelf_answers_as_the_whole_one`).
fn open_framed<'a>(krate: CrateItemKey, bytes: &'a [u8], alloc: Option<Alloc<'a>>) -> Result<Opened<'a>, ShelfError> {
    let mut r = Unframe { bytes, at: 0 };
    let malformed = |r: &Unframe<'_>| ShelfError::Malformed { krate, at: r.at };
    if r.take(FRAMED_ALIGN) != Some(&FRAMED_MAGIC[..]) {
        return Err(ShelfError::Malformed { krate, at: 0 });
    }
    let directory = r.part().ok_or_else(|| malformed(&r))?;
    let no_impls_of = r.part().ok_or_else(|| malformed(&r))?;
    let count = r.len().ok_or_else(|| malformed(&r))?;
    // Each bucket is at least its id and its length word: a count past the bytes is refused
    // before the room.
    if count > bytes.len() / FRAMED_BUCKET_LEAST {
        return Err(malformed(&r));
    }
    let mut buckets = alloc.map(|alloc| AVec::with_capacity_in(count, alloc));
    let mut last: Option<ContentId> = None;
    for _ in 0..count {
        let mut id = [0u8; 32];
        id.copy_from_slice(r.take(32).ok_or_else(|| malformed(&r))?);
        let id = ContentId(id);
        let held = r.part().ok_or_else(|| malformed(&r))?;
        if last.is_some_and(|before| before > id) {
            return Err(malformed(&r));
        }
        last = Some(id);
        if let Some(buckets) = buckets.as_mut() {
            buckets.push((id, held));
        }
    }
    if r.at != bytes.len() {
        return Err(malformed(&r));
    }
    // The form is the bytes'; the address is where they lie: a part is aligned only when the
    // payload is (each starts a multiple of `FRAMED_ALIGN` from it).
    if !bytes.as_ptr().addr().is_multiple_of(FRAMED_ALIGN) {
        return Err(ShelfError::Unaligned { krate });
    }
    Ok(Opened { krate, directory, no_impls_of, buckets })
}

/// Where each part of [`framed`]'s form starts: a multiple of this from the payload's first
/// byte. A part is a blaze-format record, read in place only at an address aligned to 16
/// (blaze-format-core `header`, `ALIGN`); the store hands a record's payload at an address
/// aligned to `PAYLOAD_ALIGN` (blaze-drive record.rs), so a part of it is aligned too.
const FRAMED_ALIGN: usize = drive::record::PAYLOAD_ALIGN;

/// What a bucket takes at least: its content id and its length word.
const FRAMED_BUCKET_LEAST: usize = 32 + FRAMED_ALIGN;

// A length word holds its u64; a bucket's 32-byte id keeps the next part aligned.
const _: () = assert!(FRAMED_ALIGN >= 8 && 32 % FRAMED_ALIGN == 0, "framed's words and ids keep its parts aligned");

/// The first bytes of [`framed`]'s form, one aligned word. Form 2: every part aligned. Form 1
/// packed them, and a reader of its parts in place was refused by their addresses; its records
/// are made again, never read (D160): a task's key holds the binary's id (blaze-cli home.rs
/// `tools`), so a rebuilt blaze never meets one under a standing key, and a form-1 payload met
/// anyway is refused by this magic as `ShelfError::Malformed`, by name.
const FRAMED_MAGIC: &[u8; FRAMED_ALIGN] = b"bzimpl2\0\0\0\0\0\0\0\0\0";

/// A crate's three kinds of record as ONE payload (the Heads task's record in the store, which a
/// later request puts on its shelf by [`ImplShelf::put_framed`], or reads in place through a
/// [`TableSet::paged`]): the magic, then the directory and `NoImplsOf` each as its length word
/// (a little-endian u64, zeros to [`FRAMED_ALIGN`]), its bytes and zeros to the next multiple of
/// [`FRAMED_ALIGN`], then the bucket count's length word, then each bucket sorted by content id
/// as its id (32 bytes) and its part as before. Every part starts a multiple of [`FRAMED_ALIGN`]
/// from the first byte, so in an aligned payload each is read where it lies.
pub fn framed<A: core::alloc::Allocator>(records: &CrateImpls<'_>, alloc: A) -> std::vec::Vec<u8, A> {
    let part = |n: usize| FRAMED_ALIGN + n.next_multiple_of(FRAMED_ALIGN);
    let size = FRAMED_ALIGN
        + part(records.directory.len())
        + part(records.no_impls_of.len())
        + FRAMED_ALIGN
        + records.buckets.iter().map(|b| 32 + part(b.bytes.len())).sum::<usize>();
    let mut out = std::vec::Vec::with_capacity_in(size, alloc);
    out.extend_from_slice(FRAMED_MAGIC);
    put_part(&mut out, &records.directory[..]);
    put_part(&mut out, &records.no_impls_of[..]);
    let mut order: std::vec::Vec<usize, Global> = std::vec::Vec::with_capacity_in(records.buckets.len(), Global);
    order.extend(0..records.buckets.len());
    order.sort_unstable_by_key(|at| records.buckets[*at].id);
    put_len(&mut out, order.len());
    for at in order {
        let b = &records.buckets[at];
        out.extend_from_slice(&b.id.0);
        put_part(&mut out, &b.bytes[..]);
    }
    debug_assert_eq!(out.len(), size, "framed's size is what it writes");
    out
}

/// `n` as [`framed`]'s length word: a little-endian u64, then zeros to [`FRAMED_ALIGN`].
fn put_len<A: core::alloc::Allocator>(out: &mut std::vec::Vec<u8, A>, n: usize) {
    out.extend_from_slice(&(n as u64).to_le_bytes());
    out.resize(out.len() + (FRAMED_ALIGN - 8), 0);
}

/// `part` as [`framed`] writes one: its length word, its bytes, zeros to the next multiple of
/// [`FRAMED_ALIGN`].
fn put_part<A: core::alloc::Allocator>(out: &mut std::vec::Vec<u8, A>, part: &[u8]) {
    put_len(out, part.len());
    out.extend_from_slice(part);
    out.resize(out.len().next_multiple_of(FRAMED_ALIGN), 0);
}

/// A reader of [`framed`]'s form: each step refuses a cut, or padding that is not zeros, by
/// its place (one form, one byte string per tables: rule 25).
struct Unframe<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Unframe<'b> {
    fn take(&mut self, n: usize) -> Option<&'b [u8]> {
        let end = self.at.checked_add(n)?;
        let part = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(part)
    }

    /// `n` bytes of padding: none when they are not all zeros.
    fn zeros(&mut self, n: usize) -> Option<()> {
        self.take(n)?.iter().all(|b| *b == 0).then_some(())
    }

    fn len(&mut self) -> Option<usize> {
        let mut b = [0u8; 8];
        b.copy_from_slice(self.take(8)?);
        self.zeros(FRAMED_ALIGN - 8)?;
        usize::try_from(u64::from_le_bytes(b)).ok()
    }

    /// A part: its length word, its bytes (returned), its padding. `take` has bounded `n` by
    /// the bytes, so the padding's sum cannot overflow.
    fn part(&mut self) -> Option<&'b [u8]> {
        let n = self.len()?;
        let part = self.take(n)?;
        self.zeros(n.next_multiple_of(FRAMED_ALIGN) - n)?;
        Some(part)
    }

    fn owned(&mut self) -> Option<std::vec::Vec<u8, Global>> {
        let part = self.part()?;
        let mut held = std::vec::Vec::with_capacity_in(part.len(), Global);
        held.extend_from_slice(part);
        Some(held)
    }
}

impl ImplShelf {
    pub fn new(_opts: ImplShelfOpts) -> ImplShelf {
        ImplShelf { tables: std::vec::Vec::new_in(Global) }
    }

    /// Holds crate `krate`'s three kinds of record, copied once.
    pub fn put(&mut self, krate: CrateItemKey, records: &CrateImpls<'_>) -> Result<(), ShelvedTwice> {
        match self.put_framed(krate, &framed(records, Global)) {
            Ok(()) => Ok(()),
            Err(ShelfError::Twice(twice)) => Err(twice),
            // `framed` wrote them this call: a malformed read of its own bytes is this file's fault.
            Err(ShelfError::Malformed { at, .. }) => panic!("impl tables framed here read back malformed at byte {at} (front2 buckets/records.rs `ImplShelf::put`)"),
            // `put_framed` copies, so it never asks the address.
            Err(ShelfError::Unaligned { .. }) => panic!("impl tables put by copy were refused for their address (front2 buckets/records.rs `ImplShelf::put`)"),
        }
    }

    /// Holds crate `krate`'s records from their [`framed`] payload (the store's record of its
    /// Heads task), copied once.
    pub fn put_framed(&mut self, krate: CrateItemKey, bytes: &[u8]) -> Result<(), ShelfError> {
        let at = match self.tables.binary_search_by_key(&krate, |t| t.krate) {
            Ok(_) => return Err(ShelfError::Twice(ShelvedTwice { krate })),
            Err(at) => at,
        };
        let mut r = Unframe { bytes, at: 0 };
        let malformed = |r: &Unframe<'_>| ShelfError::Malformed { krate, at: r.at };
        if r.take(FRAMED_ALIGN) != Some(&FRAMED_MAGIC[..]) {
            return Err(ShelfError::Malformed { krate, at: 0 });
        }
        let directory = r.owned().ok_or_else(|| malformed(&r))?;
        let no_impls_of = r.owned().ok_or_else(|| malformed(&r))?;
        let count = r.len().ok_or_else(|| malformed(&r))?;
        // Each bucket is at least its id and its length word: a count past the bytes is refused
        // before the room.
        if count > bytes.len() / FRAMED_BUCKET_LEAST {
            return Err(malformed(&r));
        }
        let mut buckets = std::vec::Vec::with_capacity_in(count, Global);
        for _ in 0..count {
            let mut id = [0u8; 32];
            id.copy_from_slice(r.take(32).ok_or_else(|| malformed(&r))?);
            let held = r.owned().ok_or_else(|| malformed(&r))?;
            buckets.push((ContentId(id), held));
        }
        if r.at != bytes.len() || !buckets.is_sorted_by_key(|(id, _)| *id) {
            return Err(malformed(&r));
        }
        self.tables.insert(at, Shelved { krate, directory, no_impls_of, buckets });
        Ok(())
    }

    /// Whether crate `krate`'s tables are on the shelf.
    pub fn holds(&self, krate: CrateItemKey) -> bool {
        self.tables.binary_search_by_key(&krate, |t| t.krate).is_ok()
    }

    /// How many crates' tables it holds.
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    fn of(&self, krate: CrateItemKey, record: RecordKind) -> Result<&Shelved, NoTables> {
        self.tables.binary_search_by_key(&krate, |t| t.krate).map(|at| &self.tables[at]).map_err(|_| NoTables { krate, record })
    }
}

/// Where a [`TableSet`]'s tables are.
enum Held<'a> {
    /// Its own, put one by one ([`TableSet::put`]): one reader's set (the fixtures' tests).
    Own {
        /// Sorted by crate key.
        tables: AVec<'a, ImplTables<'a>>,
        /// Per table, its buckets' content ids with their places, sorted.
        ids: AVec<'a, AVec<'a, (ContentId, u32)>>,
    },
    /// A request's shelf, shared read-only by every reader ([`TableSet::lent`]).
    Lent(&'a ImplShelf),
    /// A request's shelf, and on a miss its source's framed tables, opened in place on this
    /// reader's first ask of the crate and kept for its later asks ([`TableSet::paged`]).
    Paged { shelf: &'a ImplShelf, source: &'a dyn ShelfSource, opened: RefCell<AVec<'a, Opened<'a>>> },
}

/// Stretch-level inverted index over linked crates' `NoImplsOf` records, shared across every
/// body checked through one [`TableSet`]. Decodes each crate's `NoImplsOfView` once per stretch
/// (invalidated on [`TableSet::put`]) and caches per-trait holder lists and `BucketRead` slices.
struct AnyFilterCache<'a> {
    linked: AVec<'a, (CrateItemKey, usize)>,
    krate_ids: AVec<'a, (u128, [u8; 16])>,
    all_unread_reads: AVec<'a, BucketRead>,
    unread_positions: AVec<'a, usize>,
    by_trait_mark: blaze_hash::HashMap<u64, AVec<'a, usize>, Alloc<'a>>,
    traits: blaze_hash::HashMap<LibKey, CachedTraitAny<'a>, Alloc<'a>>,
}

struct CachedTraitAny<'a> {
    name: ProbeName,
    crates: AVec<'a, CrateItemKey>,
    indexes: AVec<'a, usize>,
    reads: AVec<'a, BucketRead>,
}

/// The impl tables of a set of crates as ONE reader reads them: the [`ImplRecords`] the reader
/// reads ([`Buckets::admit_tables`]), and every read it made noted. Never shared between
/// threads (its notes); many readers share one [`ImplShelf`].
pub struct TableSet<'a> {
    held: Held<'a>,
    notes: RefCell<AVec<'a, ImplRead>>,
    hashes: RefCell<blaze_own::BTreeMap<surface::BucketKey, surface::BucketHash, Alloc<'a>>>,
    probes: RefCell<HashMap<ProbeKey, ProbeName, Alloc<'a>>>,
    probe_derivations: Cell<usize>,
    any_cache: RefCell<Option<AnyFilterCache<'a>>>,
    alloc: Alloc<'a>,
}

/// The marks used for an in-memory probe. Equality still compares the complete key,
/// including the trait key's crate and all bytes beyond its mark.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ProbeKey(Probe);

impl core::hash::Hash for ProbeKey {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        match self.0 {
            Probe::Trait(t) => state.write_u64(blaze_impls::trait_mark(t)),
            Probe::UnreadTrait => state.write(&UNREAD_TRAIT_CELL),
            Probe::Bucket(hash) => state.write_u64(hash.mark()),
        }
    }
}

/// The stable names published by the two read-list consumers. Neither includes the
/// observing crate or the current answer.
#[derive(Clone, Copy)]
struct ProbeName {
    fact: u64,
    cell: [u8; 16],
}

impl ProbeName {
    // A fixed name for the one crate-wide bit. No table probe or hash derivation is needed.
    const UNREAD: Self = Self { fact: u64::from_be_bytes(*b"unreadT1"), cell: UNREAD_TRAIT_CELL };

    fn of(probe: Probe) -> Self {
        let fact = match probe {
            Probe::Trait(t) => ContentId::of(PROBE_DOMAIN, &t.0.0),
            Probe::UnreadTrait => return Self::UNREAD,
            Probe::Bucket(hash) => ContentId::of(PROBE_BUCKET_DOMAIN, &hash.0),
        };
        Self { fact: first8(&fact.0), cell: probe_cell(probe) }
    }
}

/// The dense slot exists only in this list. A body records the stable cell bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ProbeId(usize);

impl<'a> TableSet<'a> {
    pub fn new(alloc: Alloc<'a>) -> TableSet<'a> {
        Self::reader(Held::Own { tables: AVec::new_in(alloc), ids: AVec::new_in(alloc) }, alloc)
    }

    /// A reader of the shelf `shelf`, its notes in `alloc`: O(1), nothing copied.
    pub fn lent(shelf: &'a ImplShelf, alloc: Alloc<'a>) -> TableSet<'a> {
        Self::reader(Held::Lent(shelf), alloc)
    }

    /// A reader of the shelf `shelf` that reads a crate the shelf lacks from `source` on its
    /// first ask ([`ShelfSource`]): only the crates its body probes are ever read. It answers
    /// as [`TableSet::lent`] over a shelf holding every crate (front2_tables_task.rs
    /// `a_paged_shelf_answers_as_the_whole_one`).
    pub fn paged(shelf: &'a ImplShelf, source: &'a dyn ShelfSource, alloc: Alloc<'a>) -> TableSet<'a> {
        Self::reader(Held::Paged { shelf, source, opened: RefCell::new(AVec::new_in(alloc)) }, alloc)
    }

    /// Crate `krate`'s directory and `NoImplsOf` from the source, opened once a reader, and
    /// with `bucket`, that bucket's bytes. `missing` when the source has no tables for it or the
    /// bucket is not among them; bytes that do not read break the source's contract
    /// ([`check_framed`]) and panic.
    fn paged_of(&self, source: &'a dyn ShelfSource, opened: &RefCell<AVec<'a, Opened<'a>>>, krate: CrateItemKey, bucket: Option<ContentId>, missing: NoTables) -> Result<(&'a [u8], &'a [u8], Option<&'a [u8]>), NoTables> {
        let mut held = opened.borrow_mut();
        let at = match held.binary_search_by_key(&krate, |one| one.krate) {
            Ok(at) => at,
            Err(at) => {
                let bytes = source.framed(krate).ok_or(missing)?;
                let one = open_framed(krate, bytes, Some(self.alloc)).unwrap_or_else(|e| panic!("{e}: a `ShelfSource` handed bytes `check_framed` refuses (front2 buckets/records.rs `TableSet::paged_of`)"));
                held.insert(at, one);
                at
            }
        };
        let one = &held[at];
        let found = match (bucket, one.buckets.as_ref()) {
            (Some(id), Some(buckets)) => Some(buckets.binary_search_by_key(&id, |(one, _)| *one).map(|place| buckets[place].1).map_err(|_| missing)?),
            _ => None,
        };
        Ok((one.directory, one.no_impls_of, found))
    }

    fn reader(held: Held<'a>, alloc: Alloc<'a>) -> Self {
        Self {
            held,
            notes: RefCell::new(AVec::new_in(alloc)),
            hashes: RefCell::new(blaze_own::BTreeMap::new_in(alloc)),
            probes: RefCell::new(HashMap::new_in(alloc)),
            probe_derivations: Cell::new(0),
            any_cache: RefCell::new(None),
            alloc,
        }
    }

    fn ensure_any_cache(&self, linked: &[(CrateItemKey, usize)]) -> Result<(), TablesReadError> {
        if self.any_cache.borrow().as_ref().is_some_and(|c| c.linked.as_slice() == linked) {
            return Ok(());
        }
        let alloc = self.alloc;
        let mut linked_copy = AVec::with_capacity_in(linked.len(), alloc);
        linked_copy.extend_from_slice(linked);
        let mut krate_ids = AVec::with_capacity_in(linked.len(), alloc);
        let mut all_unread_reads = AVec::with_capacity_in(linked.len(), alloc);
        let mut unread_positions = AVec::new_in(alloc);
        let mut by_trait_mark: blaze_hash::HashMap<u64, AVec<'a, usize>, Alloc<'a>> = blaze_hash::HashMap::new_in(alloc);
        for (pos, &(key, _)) in linked.iter().enumerate() {
            let filter = read_no_impls_of(self, key)?;
            let krate_id = first16(key.0);
            krate_ids.push((krate_id, krate_id.to_be_bytes()));
            let has_unread = filter.has_unread();
            all_unread_reads.push(BucketRead {
                krate: krate_id,
                bucket: ProbeName::UNREAD.fact,
                answer: u64::from(has_unread),
            });
            if has_unread {
                unread_positions.push(pos);
            } else {
                for &mark in filter.trait_marks() {
                    by_trait_mark.entry(mark).or_insert_with(|| AVec::new_in(alloc)).push(pos);
                }
            }
        }
        *self.any_cache.borrow_mut() = Some(AnyFilterCache {
            linked: linked_copy,
            krate_ids,
            all_unread_reads,
            unread_positions,
            by_trait_mark,
            traits: blaze_hash::HashMap::new_in(alloc),
        });
        Ok(())
    }

    fn with_any_unread<R>(
        &self,
        linked: &[(CrateItemKey, usize)],
        f: impl FnOnce(&[(u128, [u8; 16])], &[BucketRead]) -> R,
    ) -> Result<R, TablesReadError> {
        self.ensure_any_cache(linked)?;
        let guard = self.any_cache.borrow();
        let Some(cache) = guard.as_ref() else {
            return Err(TablesReadError::NoCrateKey { krate: 0 });
        };
        Ok(f(&cache.krate_ids, &cache.all_unread_reads))
    }

    fn with_any_trait<R>(
        &self,
        linked: &[(CrateItemKey, usize)],
        lib_trait: LibKey,
        f: impl FnOnce(&[(u128, [u8; 16])], ProbeName, &[CrateItemKey], &[usize], &[BucketRead]) -> R,
    ) -> Result<R, TablesReadError> {
        self.ensure_any_cache(linked)?;
        let need_build = self.any_cache.borrow().as_ref().is_some_and(|c| !c.traits.contains_key(&lib_trait));
        if need_build {
            let name = self.probe(Probe::Trait(lib_trait));
            let mark = blaze_impls::trait_mark(lib_trait);
            let alloc = self.alloc;
            let mut guard = self.any_cache.borrow_mut();
            if let Some(cache) = guard.as_mut() {
                let mut reads = AVec::with_capacity_in(cache.linked.len(), alloc);
                for &(krate_id, _) in cache.krate_ids.iter() {
                    reads.push(BucketRead { krate: krate_id, bucket: name.fact, answer: 0 });
                }
                let marked: &[usize] = cache.by_trait_mark.get(&mark).map_or(&[], |v| v.as_slice());
                let unread: &[usize] = cache.unread_positions.as_slice();
                let mut crates = AVec::with_capacity_in(unread.len() + marked.len(), alloc);
                let mut indexes = AVec::with_capacity_in(unread.len() + marked.len(), alloc);
                let (mut ui, mut mi) = (0usize, 0usize);
                while ui < unread.len() || mi < marked.len() {
                    let pos = match (unread.get(ui), marked.get(mi)) {
                        (Some(&u), Some(&m)) if u < m => {
                            ui += 1;
                            u
                        }
                        (Some(&u), Some(&m)) if m < u => {
                            mi += 1;
                            m
                        }
                        (Some(&u), Some(_)) => {
                            ui += 1;
                            mi += 1;
                            u
                        }
                        (Some(&u), None) => {
                            ui += 1;
                            u
                        }
                        (None, Some(&m)) => {
                            mi += 1;
                            m
                        }
                        (None, None) => break,
                    };
                    if let Some(row) = reads.get_mut(pos) {
                        row.answer = 1;
                    }
                    if let Some(&(key, krate)) = cache.linked.get(pos) {
                        crates.push(key);
                        indexes.push(krate);
                    }
                }
                cache.traits.insert(lib_trait, CachedTraitAny { name, crates, indexes, reads });
            }
        }
        let guard = self.any_cache.borrow();
        let Some(cache) = guard.as_ref() else {
            return Err(TablesReadError::NoCrateKey { krate: 0 });
        };
        let Some(entry) = cache.traits.get(&lib_trait) else {
            return Err(TablesReadError::NoCrateKey { krate: 0 });
        };
        Ok(f(&cache.krate_ids, entry.name, &entry.crates, &entry.indexes, &entry.reads))
    }

    fn probe(&self, probe: Probe) -> ProbeName {
        if probe == Probe::UnreadTrait {
            return ProbeName::UNREAD;
        }
        let mut probes = self.probes.borrow_mut();
        *probes.entry(ProbeKey(probe)).or_insert_with(|| {
            self.probe_derivations.set(self.probe_derivations.get() + 1);
            ProbeName::of(probe)
        })
    }

    /// Stable names derived in the stretch. The unread-impl probe has a constant name.
    pub fn probe_ids_derived(&self) -> usize {
        self.probe_derivations.get()
    }

    /// Holds `tables`, replacing the crate's earlier ones. A lent set holds the shelf's alone:
    /// putting into it is the caller's fault, refused by a panic (its task fails alone).
    pub fn put(&mut self, tables: ImplTables<'a>) {
        let Held::Own { tables: held, ids: held_ids } = &mut self.held else {
            panic!("impl tables put into a set lent from a request's shelf (front2 buckets/records.rs `TableSet::put`)");
        };
        *self.any_cache.borrow_mut() = None;
        let mut ids: AVec<'a, (ContentId, u32)> = AVec::with_capacity_in(tables.records.buckets.len(), self.alloc);
        ids.extend(tables.records.buckets.iter().enumerate().map(|(at, b)| (b.id, at as u32)));
        ids.sort_unstable();
        match held.binary_search_by_key(&tables.krate, |t| t.krate) {
            Ok(at) => {
                held[at] = tables;
                held_ids[at] = ids;
            }
            Err(at) => {
                held.insert(at, tables);
                held_ids.insert(at, ids);
            }
        }
    }

    /// The crate's tables as written, in a set of its own ([`TableSet::new`]); `None` in a lent
    /// one, whose shelf holds the records' bytes alone.
    pub fn of(&self, krate: CrateItemKey) -> Option<&ImplTables<'a>> {
        let Held::Own { tables, .. } = &self.held else { return None };
        let at = tables.binary_search_by_key(&krate, |t| t.krate).ok()?;
        Some(&tables[at])
    }

    /// The reads noted since the last call, and none kept.
    pub fn take_notes(&self) -> AVec<'a, ImplRead> {
        core::mem::replace(&mut *self.notes.borrow_mut(), AVec::new_in(self.alloc))
    }
}

impl<'a> ImplRecords for TableSet<'a> {
    type Error = NoTables;

    fn bucket_hash(&self, key: surface::BucketKey) -> surface::BucketHash {
        *self.hashes.borrow_mut().entry(key).or_insert_with(|| key.hash())
    }

    fn no_impls_of(&self, krate: CrateItemKey) -> Result<&[u8], NoTables> {
        let record = RecordKind::NoImplsOf;
        match &self.held {
            Held::Own { tables, .. } => tables.binary_search_by_key(&krate, |t| t.krate).map(|at| &tables[at].records.no_impls_of[..]).map_err(|_| NoTables { krate, record }),
            Held::Lent(shelf) => Ok(&shelf.of(krate, record)?.no_impls_of),
            Held::Paged { shelf, source, opened } => match shelf.of(krate, record) {
                Ok(one) => Ok(&one.no_impls_of),
                Err(missing) => self.paged_of(*source, opened, krate, None, missing).map(|(_, no_impls_of, _)| no_impls_of),
            },
        }
    }

    fn directory(&self, krate: CrateItemKey) -> Result<&[u8], NoTables> {
        let record = RecordKind::Directory;
        match &self.held {
            Held::Own { tables, .. } => tables.binary_search_by_key(&krate, |t| t.krate).map(|at| &tables[at].records.directory[..]).map_err(|_| NoTables { krate, record }),
            Held::Lent(shelf) => Ok(&shelf.of(krate, record)?.directory),
            Held::Paged { shelf, source, opened } => match shelf.of(krate, record) {
                Ok(one) => Ok(&one.directory),
                Err(missing) => self.paged_of(*source, opened, krate, None, missing).map(|(directory, _, _)| directory),
            },
        }
    }

    fn bucket(&self, krate: CrateItemKey, bucket: ContentId) -> Result<&[u8], NoTables> {
        let missing = NoTables { krate, record: RecordKind::Bucket };
        match &self.held {
            Held::Own { tables, ids } => {
                let at = tables.binary_search_by_key(&krate, |t| t.krate).map_err(|_| missing)?;
                let ids = &ids[at];
                let place = ids.binary_search_by_key(&bucket, |it| it.0).map_err(|_| missing)?;
                Ok(&tables[at].records.buckets[ids[place].1 as usize].bytes)
            }
            Held::Lent(shelf) => {
                let one = shelf.of(krate, RecordKind::Bucket)?;
                let place = one.buckets.binary_search_by_key(&bucket, |(id, _)| *id).map_err(|_| missing)?;
                Ok(&one.buckets[place].1)
            }
            Held::Paged { shelf, source, opened } => match shelf.of(krate, RecordKind::Bucket) {
                Ok(one) => {
                    let place = one.buckets.binary_search_by_key(&bucket, |(id, _)| *id).map_err(|_| missing)?;
                    Ok(&one.buckets[place].1)
                }
                Err(_) => self.paged_of(*source, opened, krate, Some(bucket), missing).and_then(|(_, _, found)| found.ok_or(missing)),
            },
        }
    }

    fn note(&self, read: ImplRead) {
        self.notes.borrow_mut().push(read);
    }
}

fn first8(bytes: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&bytes[..8]);
    u64::from_be_bytes(b)
}

fn first16(id: ContentId) -> u128 {
    let mut b = [0u8; 16];
    b.copy_from_slice(&id.0[..16]);
    u128::from_be_bytes(b)
}

/// Mark a probe in one body's crate row. A directory hit or miss reads the same cell
/// as its preceding bucket filter. The directory digest covers the contents too.
fn first_read<A: core::alloc::Allocator>(words: &mut Vec<u64, A>, ProbeId(id): ProbeId) -> bool {
    let word = id / u64::BITS as usize;
    if word >= words.len() {
        words.resize(word + 1, 0);
    }
    let bit = 1u64 << (id % u64::BITS as usize);
    let first = words[word] & bit == 0;
    words[word] |= bit;
    first
}

/// A union for one list of reads, with one bit per dense name in each crate.
/// A lookup supplies the crate's index. A consumer of portable rows interns the crate
/// half as well, including the graph-wide incoherence fact.
pub struct BucketReadUnion<A: core::alloc::Allocator + Copy> {
    cells: blaze_hash::IndexSet<u128, A>,
    crate_keys: blaze_hash::IndexSet<u128, A>,
    words: Vec<Vec<u64, A>, A>,
    alloc: A,
}

impl<A: core::alloc::Allocator + Copy> BucketReadUnion<A> {
    pub fn new(alloc: A) -> Self {
        Self { cells: blaze_hash::IndexSet::new_in(alloc), crate_keys: blaze_hash::IndexSet::new_in(alloc), words: Vec::new_in(alloc), alloc }
    }

    /// Keep each impl dependency once for the group. Other read kinds pass through.
    pub fn keep(&mut self, read: TableRead) -> bool {
        let TableRead::Impls { krate, bucket } = read else { return true };
        self.insert(u128::from_be_bytes(krate), u128::from_be_bytes(bucket))
    }

    /// Keep a question's bucket fact in the checker's read list. Include its answer so
    /// that differing answers reach the existing TwoAnswers check.
    pub fn keep_fact(&mut self, read: &BucketRead) -> bool {
        self.insert(read.krate, (u128::from(read.bucket) << 64) | u128::from(read.answer))
    }

    fn insert(&mut self, krate: u128, cell: u128) -> bool {
        let (krate, _) = self.crate_keys.insert_full(krate);
        let id = self.id(cell);
        self.mark(krate, id)
    }

    fn id(&mut self, cell: u128) -> ProbeId {
        ProbeId(self.cells.insert_full(cell).0)
    }

    fn mark(&mut self, krate: usize, id: ProbeId) -> bool {
        if self.words.len() <= krate {
            let alloc = self.alloc;
            self.words.resize_with(krate + 1, || Vec::new_in(alloc));
        }
        first_read(&mut self.words[krate], id)
    }
}

/// A table row as a [`Filing`] again: the inverse of [`table_row`] on what the questions read.
fn filing_of<'n, S: NamesSource<'n>>(names: S, row: &surface::ImplRow) -> Result<Filing, TablesReadError> {
    use surface::ImplFlags as F;
    let shape = checker_shape(row.shape).map_err(|()| TablesReadError::Shape { impl_: row.impl_, shape: row.shape })?;
    let flags = row.flags;
    let bare = flags.contains(F::BARE);
    let blanket = row.is_param_self();
    let unread_bare = bare && flags.contains(F::TRAIT_UNREAD);
    let filed = if flags.contains(F::BLIND) {
        Filed::Unread(Unread::Indexed { trait_impl: false, class: flags.why() }, None)
    } else if flags.contains(F::TRAIT_UNREAD) {
        Filed::Unread(Unread::Indexed { trait_impl: true, class: flags.why() }, shape)
    } else {
        match row.trait_ {
            Some(t) if flags.contains(F::NEGATIVE) => Filed::Negative(checker_of(t), shape),
            Some(t) => Filed::Trait(checker_of(t), shape, bare),
            None => Filed::Inherent(shape),
        }
    };
    let path_self = match (flags.contains(F::SELF_NO_TYPE), row.trait_) {
        (false, _) => None,
        (true, Some(t)) => Some(PathSelf::Trait { of: names.crate_of(t), bare }),
        (true, None) => Some(PathSelf::Inherent),
    };
    // Every trait row: one whose lowered header names no trait is the question's gap.
    let by_text = row.trait_.is_some() || flags.contains(F::TRAIT_UNREAD);
    Ok(Filing { key: row.impl_, filed, blanket, by_text, unread_bare, path_self })
}

/// Per-body cache for `Home::Any` trait lookups over impl tables, allocated in the body's
/// scratch `alloc`. Probing `NoImplsOf` for `Probe::UnreadTrait` once per body and
/// `Probe::Trait(t)` once per distinct trait avoids rescanning all linked crates and
/// reallocating ~1,000 `BucketRead` rows on every `(trait, shape)` question.
pub(super) struct TablesCache {
    alloc: Alloc<'static>,
    /// All linked crates in index order (`(crate_key, crate_index)`).
    linked_crates: AVec<'static, (CrateItemKey, usize)>,
    /// `Probe::UnreadTrait` fact row for every linked crate, probed once per body.
    all_unread_reads: AVec<'static, BucketRead>,
    /// Per trait (keyed by checker `ItemKey`), the linked crates whose `NoImplsOf` filter may
    /// hold the trait, plus the `Probe::Trait` fact row for every linked crate.
    trait_any_filter: HashMap<ItemKey, TraitAnyFilter, Alloc<'static>>,
}

struct TraitAnyFilter {
    crates: AVec<'static, CrateItemKey>,
    indexes: AVec<'static, usize>,
    reads: AVec<'static, BucketRead>,
}

impl TablesCache {
    pub(super) fn all_unread_reads(&self) -> &[BucketRead] {
        &self.all_unread_reads
    }

    pub(super) fn trait_reads_for(&self, trait_: ItemKey) -> &[BucketRead] {
        self.trait_any_filter.get(&trait_).map_or(&[], |entry| &entry.reads)
    }
}

fn read_no_impls_of<'a>(tables: &'a TableSet<'_>, krate: CrateItemKey) -> Result<surface::NoImplsOfView<'a>, TablesReadError> {
    let bytes = tables.no_impls_of(krate).map_err(|error| {
        TablesReadError::Lookup(ImplLookupError::Records { krate, record: RecordKind::NoImplsOf, error })
    })?;
    let filter = surface::NoImplsOfView::read(bytes).map_err(|error| {
        TablesReadError::Lookup(ImplLookupError::Format { krate, record: RecordKind::NoImplsOf, error })
    })?;
    if filter.krate() != krate {
        return Err(TablesReadError::Lookup(ImplLookupError::WrongCrate {
            krate,
            record: RecordKind::NoImplsOf,
            found: filter.krate(),
        }));
    }
    Ok(filter)
}

impl BucketIndex {
    /// Initializes [`BucketIndex::tables_cache`] on the first `Home::Any` trait question:
    /// records `NameCell::Root` once per crate (via [`BucketIndex::crate_linked`]) and
    /// `Probe::UnreadTrait` once per linked crate.
    fn ensure_tables_cache<'n, S: NamesSource<'n>>(&mut self, names: S, tables: &TableSet<'_>, alloc: Alloc<'static>) -> Result<(), TablesReadError> {
        if self.tables_cache.is_some() {
            return Ok(());
        }
        let n = self.crate_state.len();
        let mut linked_crates = AVec::with_capacity_in(n, alloc);
        for krate in 0..n {
            if !self.crate_linked(names, krate) {
                continue;
            }
            let key = names.crate_key(krate).ok_or(TablesReadError::NoCrateKey { krate })?;
            linked_crates.push((key, krate));
        }
        let mut all_unread_reads = AVec::with_capacity_in(linked_crates.len(), alloc);
        let probes = self.table_reads.get_or_insert_with(|| BucketReadUnion::new(alloc));
        let unread_id = probes.id(u128::from_be_bytes(ProbeName::UNREAD.cell));
        tables.with_any_unread(&linked_crates, |krate_ids, cached_reads| {
            for (&(_, krate), &(_, krate_bytes)) in linked_crates.iter().zip(krate_ids.iter()) {
                if probes.mark(krate, unread_id) {
                    names.impls_read(TableRead::Impls { krate: krate_bytes, bucket: ProbeName::UNREAD.cell });
                }
            }
            all_unread_reads.extend_from_slice(cached_reads);
        })?;
        self.tables_cache = Some(TablesCache {
            alloc,
            linked_crates,
            all_unread_reads,
            trait_any_filter: HashMap::new_in(alloc),
        });
        Ok(())
    }

    /// Ensures `trait_any_filter` holds `trait_`, probing `NoImplsOf::may_hold_trait(lib_trait)`
    /// once across all linked crates (via `TableSet`'s stretch-level inverted index) and recording
    /// `Probe::Trait(lib_trait)` into `names`.
    fn ensure_trait_any_filter<'n, S: NamesSource<'n>>(
        &mut self,
        names: S,
        trait_: ItemKey,
        lib_trait: LibKey,
        tables: &TableSet<'_>,
        alloc: Alloc<'static>,
    ) -> Result<(), TablesReadError> {
        self.ensure_tables_cache(names, tables, alloc)?;
        let Some(cache) = self.tables_cache.as_mut() else { return Ok(()) };
        if cache.trait_any_filter.contains_key(&trait_) {
            return Ok(());
        }
        let cache_alloc = cache.alloc;
        let probes = self.table_reads.get_or_insert_with(|| BucketReadUnion::new(cache_alloc));
        let entry = tables.with_any_trait(&cache.linked_crates, lib_trait, |krate_ids, name, cached_crates, cached_indexes, cached_reads| {
            let probe_id = probes.id(u128::from_be_bytes(name.cell));
            for (&(_, krate), &(_, krate_bytes)) in cache.linked_crates.iter().zip(krate_ids.iter()) {
                if probes.mark(krate, probe_id) {
                    names.impls_read(TableRead::Impls { krate: krate_bytes, bucket: name.cell });
                }
            }
            let mut crates = AVec::with_capacity_in(cached_crates.len(), cache_alloc);
            crates.extend_from_slice(cached_crates);
            let mut indexes = AVec::with_capacity_in(cached_indexes.len(), cache_alloc);
            indexes.extend_from_slice(cached_indexes);
            let mut reads = AVec::with_capacity_in(cached_reads.len(), cache_alloc);
            reads.extend_from_slice(cached_reads);
            TraitAnyFilter { crates, indexes, reads }
        })?;
        cache.trait_any_filter.insert(trait_, entry);
        Ok(())
    }

    /// Files what the question `ask` at `home` can be answered from, from the impl tables
    /// `tables` (module docs): the crates `home` admits, as [`Buckets::admit_asked`] chooses
    /// them, each asked through blaze-impls' lookups at every shape `ask`'s stands for. A
    /// question asked before is not asked again. Returns how many rows this call filed.
    pub fn admit_tables<'n, S: NamesSource<'n>>(&mut self, names: S, keys: &Keys, home: Homes, ask: Ask, tables: &TableSet<'_>, alloc: Alloc<'static>) -> Result<usize, AdmitError> {
        let asked = match ask {
            Ask::Trait(t, shape) => self.indexed.trait_reads.contains_key(&(t, shape)),
            Ask::Inherent(shape) => self.indexed.inherent_reads.contains_key(&shape),
        };
        if asked {
            return Ok(0);
        }
        let Homes { all, bare, blanket, local: _ } = home;
        let mut reads: Vec<BucketRead, Global> = Vec::new_in(Global);
        if [all, bare, blanket].contains(&Home::Incoherent) {
            if let Some(digest) = self.indexed.incoherent {
                reads.push(BucketRead { krate: 0, bucket: INCOHERENT_FACT, answer: digest });
            } else {
                match names.incoherent_crates() {
                    Some((crates, digest)) => {
                        for krate in 0..self.crate_state.len() {
                            self.crate_state[krate].incoherent = crates.contains(&krate);
                        }
                        let answer = first8(&digest.0);
                        self.indexed.incoherent = Some(answer);
                        reads.push(BucketRead { krate: 0, bucket: INCOHERENT_FACT, answer });
                    }
                    None => names.gap("which crates may write an incoherent inherent impl (the records were named no lang crates: `Records::name_lang_crates`)", None),
                }
            }
        }
        let (trait_, shape) = match ask {
            Ask::Trait(t, shape) => (Some(t), shape),
            Ask::Inherent(shape) => (None, shape),
        };
        // A question no bucket of which can be asked is refused by name: answered from no
        // bucket it would read "no impl", which is not what is known (D193 point 5). These were
        // a names-log gap and `Ok(0)`, so the body went on to that wrong answer.
        let shapes = asked_shapes(names, keys, shape, alloc)?;
        let lib_trait = match trait_ {
            Some(t) => Some(keys.library_in(names, t).ok_or(ImplQuestionError::NoLibraryKey { of: "the trait an impl question is about", key: t })?),
            None => None,
        };
        // When `home` admits all linked crates for a trait question, share the per-body
        // `NoImplsOf` precheck across shapes so `candidates_in` only visits matching holders.
        let uses_any_filter = matches!((trait_, lib_trait), (Some(_), Some(_))) && [all, bare, blanket].contains(&Home::Any);
        let mut crates: AVec<'static, CrateItemKey> = AVec::new_in(alloc);
        let mut indexes: AVec<'static, usize> = AVec::new_in(alloc);
        if let (true, Some(t), Some(lib_t)) = (uses_any_filter, trait_, lib_trait) {
            self.ensure_trait_any_filter(names, t, lib_t, tables, alloc)?;
            if let Some(entry) = self.tables_cache.as_ref().and_then(|c| c.trait_any_filter.get(&t)) {
                crates.reserve(entry.crates.len());
                indexes.reserve(entry.indexes.len());
                crates.extend_from_slice(&entry.crates);
                indexes.extend_from_slice(&entry.indexes);
            }
        } else {
            // The crates asked, in index order: those `home` admits that the program links.
            for krate in 0..self.crate_state.len() {
                if !(self.admits(all, krate) || self.admits(bare, krate) || self.admits(blanket, krate)) {
                    continue;
                }
                if !self.crate_linked(names, krate) {
                    continue;
                }
                // Never skipped without a word: a crate the home admits whose tables are not asked
                // would leave its impls out of the answer unseen.
                let key = names.crate_key(krate).ok_or(TablesReadError::NoCrateKey { krate })?;
                crates.push(key);
                indexes.push(krate);
            }
        }
        let mut filed = 0usize;
        for (width, s) in shapes.iter().enumerate() {
            if crates.is_empty() {
                break;
            }
            // A trait rejection covers every width of a numeric survey. Only holders
            // that passed the first width's trait check need the remaining heads.
            // Under `uses_any_filter`, every crate in `crates` already passed the trait check.
            let narrow = !uses_any_filter && width == 0 && shapes.len() > 1 && lib_trait.is_some();
            let remaining = RefCell::new(Vec::new_in(alloc));
            // Slots are local to a concrete shape. Its nth want is the same probe in
            // every holder, so only the first holder needs the stretch interner.
            let slots = [const { OnceCell::new() }; Probe::SLOTS];
            let ix = RefCell::new(&mut *self);
            let lookups = Cell::new(0u32);
            let rows = RefCell::new(&mut reads);
            let note = |holder: usize, slot: usize, read: ImplRead| {
                let row = match read {
                    ImplRead::Filter { krate, probe, may_hold } => {
                        if (uses_any_filter || width != 0) && matches!(probe, Probe::Trait(_) | Probe::UnreadTrait) {
                            return;
                        }
                        if narrow && may_hold && matches!(probe, Probe::Trait(_)) {
                            remaining.borrow_mut().push(holder);
                        }
                        let mut ix = ix.borrow_mut();
                        let probes = ix.table_reads.get_or_insert_with(|| BucketReadUnion::new(alloc));
                        let &(id, name) = slots[slot].get_or_init(|| {
                            let name = tables.probe(probe);
                            (probes.id(u128::from_be_bytes(name.cell)), name)
                        });
                        let krate = first16(krate.0);
                        if probes.mark(indexes[holder], id) {
                            names.impls_read(TableRead::Impls { krate: krate.to_be_bytes(), bucket: name.cell });
                        }
                        BucketRead { krate, bucket: name.fact, answer: u64::from(may_hold) }
                    }
                    ImplRead::Directory { krate, bucket, found } => BucketRead {
                        krate: first16(krate.0),
                        bucket: bucket.mark(),
                        answer: found.map_or(0, |id| first8(&id.0)),
                    },
                    ImplRead::Bucket { krate, bucket } => {
                        lookups.set(lookups.get() + 1);
                        let digest = first8(&bucket.0);
                        BucketRead { krate: first16(krate.0), bucket: digest, answer: digest }
                    }
                };
                rows.borrow_mut().push(row);
            };
            let found = match lib_trait {
                Some(t) => candidates_in(&crates, t, *s, tables),
                None => inherent_candidates_in(&crates, *s, tables),
            };
            // The first width has already filed and recorded the blanket, unread-at-any
            // and blind buckets. Every following width adds only its shaped buckets.
            let found = if width == 0 { found } else { found.only_at_shape() };
            for row in found.recording(&note) {
                let row = row.map_err(TablesReadError::Lookup)?;
                let at = crates.iter().position(|c| row.impl_.is_of(*c)).ok_or(TablesReadError::Crate { impl_: row.impl_ })?;
                let filing = filing_of(names, &row)?;
                ix.borrow_mut().take(names, indexes[at], filing);
                filed += 1;
            }
            ix.borrow_mut().indexed.lookups += lookups.get();
            if narrow {
                let kept = remaining.into_inner();
                for (to, &from) in kept.iter().enumerate() {
                    crates[to] = crates[from];
                    indexes[to] = indexes[from];
                }
                crates.truncate(kept.len());
                indexes.truncate(kept.len());
            }
        }
        self.indexed.asks += 1;
        self.indexed.from_index += filed as u32;
        reads.sort_unstable();
        reads.dedup();
        match ask {
            Ask::Trait(t, shape) => {
                self.indexed.trait_reads.insert((t, shape), asked::TraitReads { own: reads, uses_any_filter });
            }
            Ask::Inherent(shape) => {
                self.indexed.inherent_reads.insert(shape, reads);
            }
        }
        Ok(filed)
    }
}

/// Where a job's index files a question's impls from: the one seam the old index and the impl
/// tables stand behind (rule 78), each tested equal (tests/front2_buckets_order.rs).
pub trait BucketSource {
    /// Files what `ask` at `home` can be answered from; how many impls it filed.
    fn admit<'n, S: NamesSource<'n>>(&self, b: &mut Buckets, names: S, keys: &Keys, home: Homes, ask: Ask, alloc: Alloc<'static>) -> Result<usize, AdmitError>;
}

/// Today's index: whole crates by their rows ([`Buckets::admit`]), else the items records'
/// bucket section ([`Buckets::admit_asked`]). Deleted with them.
pub struct ItemsIndex;

impl BucketSource for ItemsIndex {
    fn admit<'n, S: NamesSource<'n>>(&self, b: &mut Buckets, names: S, keys: &Keys, home: Homes, ask: Ask, _alloc: Alloc<'static>) -> Result<usize, AdmitError> {
        let whole = b.admit(names, home);
        Ok(whole + b.admit_asked(names, keys, home, ask)?)
    }
}

impl BucketSource for TableSet<'_> {
    fn admit<'n, S: NamesSource<'n>>(&self, b: &mut Buckets, names: S, keys: &Keys, home: Homes, ask: Ask, alloc: Alloc<'static>) -> Result<usize, AdmitError> {
        b.admit_tables(names, keys, home, ask, self, alloc)
    }
}

#[cfg(test)]
mod read_id_tests {
    use super::*;

    #[test]
    fn two_probe_keys_with_the_same_mark_require_two_read_ids() {
        let tables = TableSet::new(blaze_own::testing::heap().alloc());
        let first = surface::BucketHash([0; 16]);
        let mut second = first;
        second.0[15] = 1;
        assert_eq!(first.mark(), second.mark());
        let a = tables.probe(Probe::Bucket(first)).cell;
        let b = tables.probe(Probe::Bucket(second)).cell;
        let mut body = BucketReadUnion::new(tables.alloc);
        let id_a = body.id(u128::from_be_bytes(a));
        let id_b = body.id(u128::from_be_bytes(b));
        assert_eq!(body.id(u128::from_be_bytes(tables.probe(Probe::Bucket(first)).cell)), id_a);
        assert_eq!(tables.probe_ids_derived(), 2);

        // The oracle compares writer-visible names. Plant the first slot on the second
        // bucket before marking it, exactly where an alias would lose its dependency.
        let collected = |second_slot| {
            let mut words = Vec::new();
            [(id_a, a), (second_slot, b)].into_iter()
                .filter_map(|(id, cell)| first_read(&mut words, id).then_some(cell))
                .collect::<Vec<_>>()
        };
        assert_eq!(collected(id_b), [first.0, second.0]);
        assert_eq!(collected(id_a), [first.0]);
        assert_ne!(collected(id_a), [first.0, second.0]);
        std::println!("two buckets sharing a mark keep two cells; reusing a dense ID loses the second");
    }

    #[test]
    fn expansion_items_share_one_bucket_union_only_inside_their_task() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut group = BucketReadUnion::new(alloc);
        let mut next = BucketReadUnion::new(alloc);
        for item in 0..3 {
            for i in 0..130u128 {
                for krate in [[1; 16], [2; 16]] {
                    let cell = TableRead::Impls { krate, bucket: i.to_be_bytes() };
                    assert_eq!(group.keep(cell), item == 0);
                    if item == 2 {
                        assert!(next.keep(cell));
                    }
                }
            }
        }
        let head = TableRead::Head { item: [1; 16], part: 0 };
        assert!(group.keep(head) && group.keep(head));
        let mut facts = BucketReadUnion::new(alloc);
        let missing = BucketRead { krate: 1, bucket: 7, answer: 0 };
        assert!(facts.keep_fact(&missing));
        assert!(!facts.keep_fact(&missing));
        assert!(facts.keep_fact(&BucketRead { answer: 1, ..missing }));
        std::println!("three expansion items retain 260 bucket reads; a second task retains its own 260");
    }
}
