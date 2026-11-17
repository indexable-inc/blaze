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

use core::cell::RefCell;
use core::fmt;

use blaze_ids::Hasher;
use blaze_impls::{ImplLookupError, ImplRead, ImplRecords, Probe, RecordKind, candidates_in, inherent_candidates_in};
use blaze_own::{AVec, Alloc};
use blaze_scope::items_record::SelfHead;
use blaze_surface::{self as surface, ContentId, CrateImpls, CrateItemKey, FloatWidth, ImplIndexError, IntWidth, Mutability, SelfShape, UintWidth};
use blaze_typeck::{FloatTy, IntTy, UintTy};

use super::*;

/// The domain of [`row_digest`].
const ROW_DOMAIN: &str = "blaze-ladder impl table row 1";
/// The domain of [`ImplTables::key`].
const TABLES_DOMAIN: &str = "blaze-ladder impl tables 1";
/// The domain of a [`Probe::Trait`]'s name in a [`BucketRead`].
const PROBE_DOMAIN: &str = "blaze-ladder impl table probe 1";

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
    /// per trait the crate has an impl of ([`trait_cell`], [`TRAIT_HELD`]). A lookup's reads
    /// map onto them by [`cell_read`]; a cell a reader finds no row for is absent, a value it
    /// reads, so an impl added at a new shape or of a new trait dirties exactly the readers
    /// that probed that bucket or that trait.
    pub defines: AVec<'a, ([u8; 16], ContentId)>,
}

/// The domain of [`trait_cell`].
const TRAIT_CELL_DOMAIN: &str = "blaze-ladder impl tables trait cell 1";

/// What a [`trait_cell`] holds when the crate has an impl of the trait.
pub const TRAIT_HELD: ContentId = ContentId([1; 32]);

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

/// The cell one lookup read maps to (module docs): `None` for a bucket record's read, which
/// its directory cell covers.
pub fn cell_read(read: &ImplRead) -> Option<[u8; 16]> {
    match read {
        ImplRead::Filter { probe: Probe::Trait(t), .. } => Some(trait_cell(*t)),
        ImplRead::Filter { probe: Probe::Bucket(hash), .. } | ImplRead::Directory { bucket: hash, .. } => Some(bucket_cell(*hash)),
        ImplRead::Bucket { .. } => None,
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
    h.update(&[u8::from(row.has_trait), u8::from(row.bare), u8::from(row.header_unread), u8::from(row.trait_unread)]);
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

/// Crate `krate`'s impl tables from its impl rows `rows` (its items record's, in any order)
/// and the names `names` resolves them by: a pure function of the two, its buffers in
/// `alloc`. Every row is filed once ([`file_rows`]); a row blaze-surface refuses is in
/// [`CrateImpls::refused`], named.
pub fn write_impl_tables<'n, 'a, S: NamesSource<'n>>(names: S, krate: usize, rows: &[ImplRow<'n>], alloc: Alloc<'a>) -> Result<ImplTables<'a>, TablesError> {
    let crate_key = names.crate_key(krate).ok_or(TablesError::NoCrateKey { krate })?;
    // Each row's digest and header flags, by impl key.
    let mut by_key: AVec<'a, (LibKey, ContentId, surface::ImplFlags)> = AVec::with_capacity_in(rows.len(), alloc);
    by_key.extend(rows.iter().map(|row| (row.key, row_digest(row), row.flags.intersect(surface::ImplFlags::HEADER))));
    by_key.sort_unstable_by_key(|it| it.0);
    let mut key = Hasher::new(TABLES_DOMAIN);
    key.update(&crate_key.0.0);
    for (_, digest, _) in by_key.iter() {
        key.id(digest);
    }
    let mut out: AVec<'a, surface::ImplRow> = AVec::with_capacity_in(rows.len(), alloc);
    let mut named: AVec<'a, CrateItemKey> = AVec::new_in(alloc);
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
    named.sort_unstable();
    named.dedup();
    let records = surface::build_impl_index(crate_key, &out, alloc).map_err(TablesError::Index)?;
    let mut defines: AVec<'a, ([u8; 16], ContentId)> = AVec::with_capacity_in(records.buckets.len() + out.len(), alloc);
    defines.extend(records.buckets.iter().map(|b| (bucket_cell(b.hash), b.id)));
    defines.extend(out.iter().filter_map(|row| row.trait_).map(|t| (trait_cell(t), TRAIT_HELD)));
    defines.sort_unstable();
    defines.dedup();
    Ok(ImplTables { krate: crate_key, key: key.finish(), records, named, counts, no_checker_type, defines })
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
        Shape::FnDef(_) | Shape::Param | Shape::Opaque(_) | Shape::AnyInt | Shape::AnyFloat => return Err(Unwritten::Shape),
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
/// alone) for a shape no impl is filed at (a fn item, an opaque type, a parameter). `None`: a
/// nominal type whose library key the key map and the records do not give.
fn asked_shapes<'n, S: NamesSource<'n>>(names: S, keys: &Keys, shape: Shape) -> Option<Vec<SelfShape, Global>> {
    let mut out: Vec<SelfShape, Global> = Vec::new_in(Global);
    let lib = |k: ItemKey| keys.library_in(names, k);
    let mut put = |s: Shape| -> Option<()> {
        out.push(self_shape(s, &lib).ok()?);
        Some(())
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
        Shape::FnDef(_) | Shape::Param | Shape::Opaque(_) => out.push(SelfShape::PLACEHOLDER),
        other => put(other)?,
    }
    Some(out)
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

/// Why a question could not be filed from the impl tables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TablesReadError {
    Lookup(ImplLookupError<NoTables>),
    /// A row at a shape the writer never writes.
    Shape { impl_: LibKey, shape: SelfShape },
    /// A row of a crate the question did not ask.
    Crate { impl_: LibKey },
}

impl fmt::Display for TablesReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TablesReadError::Lookup(error) => write!(f, "an impl table lookup: {error}"),
            TablesReadError::Shape { impl_, shape } => write!(f, "the impl table row of {impl_:x} is at {shape:?}, which no table writer writes"),
            TablesReadError::Crate { impl_ } => write!(f, "the impl table row of {impl_:x} is of no crate the question asked"),
        }
    }
}

impl core::error::Error for TablesReadError {}

/// The impl tables of a set of crates, held in memory: the [`ImplRecords`] a test and the
/// interim reader read ([`Buckets::admit_tables`]), and every read noted.
pub struct TableSet<'a> {
    /// Sorted by crate key.
    tables: AVec<'a, ImplTables<'a>>,
    /// Per table, its buckets' content ids with their places, sorted.
    ids: AVec<'a, AVec<'a, (ContentId, u32)>>,
    notes: RefCell<AVec<'a, ImplRead>>,
    alloc: Alloc<'a>,
}

impl<'a> TableSet<'a> {
    pub fn new(alloc: Alloc<'a>) -> TableSet<'a> {
        TableSet { tables: AVec::new_in(alloc), ids: AVec::new_in(alloc), notes: RefCell::new(AVec::new_in(alloc)), alloc }
    }

    /// Holds `tables`, replacing the crate's earlier ones.
    pub fn put(&mut self, tables: ImplTables<'a>) {
        let mut ids: AVec<'a, (ContentId, u32)> = AVec::with_capacity_in(tables.records.buckets.len(), self.alloc);
        ids.extend(tables.records.buckets.iter().enumerate().map(|(at, b)| (b.id, at as u32)));
        ids.sort_unstable();
        match self.tables.binary_search_by_key(&tables.krate, |t| t.krate) {
            Ok(at) => {
                self.tables[at] = tables;
                self.ids[at] = ids;
            }
            Err(at) => {
                self.tables.insert(at, tables);
                self.ids.insert(at, ids);
            }
        }
    }

    /// The crate's tables.
    pub fn of(&self, krate: CrateItemKey) -> Option<&ImplTables<'a>> {
        let at = self.tables.binary_search_by_key(&krate, |t| t.krate).ok()?;
        Some(&self.tables[at])
    }

    fn at(&self, krate: CrateItemKey, record: RecordKind) -> Result<usize, NoTables> {
        self.tables.binary_search_by_key(&krate, |t| t.krate).map_err(|_| NoTables { krate, record })
    }

    /// The reads noted since the last call, and none kept.
    pub fn take_notes(&self) -> AVec<'a, ImplRead> {
        core::mem::replace(&mut *self.notes.borrow_mut(), AVec::new_in(self.alloc))
    }
}

impl ImplRecords for TableSet<'_> {
    type Error = NoTables;

    fn no_impls_of(&self, krate: CrateItemKey) -> Result<&[u8], NoTables> {
        let at = self.at(krate, RecordKind::NoImplsOf)?;
        Ok(&self.tables[at].records.no_impls_of)
    }

    fn directory(&self, krate: CrateItemKey) -> Result<&[u8], NoTables> {
        let at = self.at(krate, RecordKind::Directory)?;
        Ok(&self.tables[at].records.directory)
    }

    fn bucket(&self, krate: CrateItemKey, bucket: ContentId) -> Result<&[u8], NoTables> {
        let at = self.at(krate, RecordKind::Bucket)?;
        let ids = &self.ids[at];
        let place = ids.binary_search_by_key(&bucket, |it| it.0).map_err(|_| NoTables { krate, record: RecordKind::Bucket })?;
        Ok(&self.tables[at].records.buckets[ids[place].1 as usize].bytes)
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

/// One lookup read as a read-list row: (the crate, what was asked, what it answered).
fn bucket_read(read: ImplRead) -> BucketRead {
    match read {
        ImplRead::Filter { krate, probe, may_hold } => {
            let bucket = match probe {
                Probe::Trait(t) => first8(&ContentId::of(PROBE_DOMAIN, &t.0.0).0),
                Probe::Bucket(hash) => hash.mark(),
            };
            BucketRead { krate: first16(krate.0), bucket, answer: u64::from(may_hold) }
        }
        ImplRead::Directory { krate, bucket, found } => BucketRead { krate: first16(krate.0), bucket: bucket.mark(), answer: found.map_or(0, |id| first8(&id.0)) },
        ImplRead::Bucket { krate, bucket } => BucketRead { krate: first16(krate.0), bucket: first8(&bucket.0), answer: first8(&bucket.0) },
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

impl Buckets {
    /// Files what the question `ask` at `home` can be answered from, from the impl tables
    /// `tables` (module docs): the crates `home` admits, as [`Buckets::admit_asked`] chooses
    /// them, each asked through blaze-impls' lookups at every shape `ask`'s stands for. A
    /// question asked before is not asked again. Returns how many rows this call filed.
    pub fn admit_tables<'n, S: NamesSource<'n>>(&mut self, names: S, keys: &Keys, home: Homes, ask: Ask, tables: &TableSet<'_>) -> Result<usize, TablesReadError> {
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
            match names.incoherent_crates() {
                Some((crates, digest)) => {
                    for krate in 0..self.incoherent.len() {
                        self.incoherent[krate] = crates.contains(&krate);
                    }
                    reads.push(BucketRead { krate: 0, bucket: INCOHERENT_FACT, answer: first8(&digest.0) });
                }
                None => names.gap("which crates may write an incoherent inherent impl (the records were named no lang crates: `Records::name_lang_crates`)", None),
            }
        }
        let (trait_, shape) = match ask {
            Ask::Trait(t, shape) => (Some(t), shape),
            Ask::Inherent(shape) => (None, shape),
        };
        let Some(shapes) = asked_shapes(names, keys, shape) else {
            names.gap("the library key of the nominal type an impl question is about", None);
            return Ok(0);
        };
        let lib_trait = match trait_ {
            Some(t) => match keys.library_in(names, t) {
                Some(lib) => Some(lib),
                None => {
                    names.gap("the library key of the trait an impl question is about", None);
                    return Ok(0);
                }
            },
            None => None,
        };
        // The crates asked, in index order: those `home` admits that the program links.
        let mut crates: Vec<CrateItemKey, Global> = Vec::new_in(Global);
        let mut indexes: Vec<usize, Global> = Vec::new_in(Global);
        for krate in 0..self.incoherent.len() {
            if !(self.admits(all, krate) || self.admits(bare, krate) || self.admits(blanket, krate)) {
                continue;
            }
            if !crate::front2::linked::links_unit(krate == 0, names.proc_macro(krate) == Some(true)) {
                continue;
            }
            let Some(key) = names.crate_key(krate) else { continue };
            crates.push(key);
            indexes.push(krate);
        }
        let mut filed = 0usize;
        tables.take_notes();
        for s in shapes.iter() {
            let found = match lib_trait {
                Some(t) => candidates_in(&crates, t, *s, tables),
                None => inherent_candidates_in(&crates, *s, tables),
            };
            for row in found {
                let row = row.map_err(TablesReadError::Lookup)?;
                let at = crates.iter().position(|c| row.impl_.is_of(*c)).ok_or(TablesReadError::Crate { impl_: row.impl_ })?;
                let filing = filing_of(names, &row)?;
                self.take(names, indexes[at], filing);
                filed += 1;
            }
        }
        for read in tables.take_notes().iter() {
            if matches!(read, ImplRead::Bucket { .. }) {
                self.indexed.lookups += 1;
            }
            reads.push(bucket_read(*read));
        }
        self.indexed.asks += 1;
        self.indexed.from_index += filed as u32;
        reads.sort_unstable();
        reads.dedup();
        match ask {
            Ask::Trait(t, shape) => self.indexed.trait_reads.insert((t, shape), reads),
            Ask::Inherent(shape) => self.indexed.inherent_reads.insert(shape, reads),
        };
        Ok(filed)
    }
}

/// Where a job's index files a question's impls from: the one seam the old index and the impl
/// tables stand behind (rule 78), each tested equal (tests/front2_buckets_order.rs).
pub trait BucketSource {
    /// Files what `ask` at `home` can be answered from; how many impls it filed.
    fn admit<'n, S: NamesSource<'n>>(&self, b: &mut Buckets, names: S, keys: &Keys, home: Homes, ask: Ask) -> Result<usize, TablesReadError>;
}

/// Today's index: whole crates by their rows ([`Buckets::admit`]), else the items records'
/// bucket section ([`Buckets::admit_asked`]). Deleted with them.
pub struct ItemsIndex;

impl BucketSource for ItemsIndex {
    fn admit<'n, S: NamesSource<'n>>(&self, b: &mut Buckets, names: S, keys: &Keys, home: Homes, ask: Ask) -> Result<usize, TablesReadError> {
        let whole = b.admit(names, home);
        Ok(whole + b.admit_asked(names, keys, home, ask))
    }
}

impl BucketSource for TableSet<'_> {
    fn admit<'n, S: NamesSource<'n>>(&self, b: &mut Buckets, names: S, keys: &Keys, home: Homes, ask: Ask) -> Result<usize, TablesReadError> {
        b.admit_tables(names, keys, home, ask, self)
    }
}
