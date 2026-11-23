//! A crate's impl index: its impls filed in buckets, a directory of the buckets, and the
//! crate's `NoImplsOf`, each a record of its own.
//!
//! - **A bucket** (`BucketRoot`, version 1) holds the impls of one [`BucketKey`]: the impls
//!   of a trait for one self shape, a trait's blanket impls (`impl<T> Tr for T`), or the
//!   inherent impls of one shape. Its key is written once at its head (kind, trait, shape),
//!   then its rows (`StoredRow`: impl key, header hash, flags; 68 bytes), sorted by impl key,
//!   so a row is found by a binary search in place. A bucket is addressed by its content id:
//!   two crates, or two checkouts, with equal impls of a key share it.
//! - **The directory** (`DirectoryRoot`, version 1) maps each bucket's [`BucketHash`] (16
//!   bytes of a hash of its key) to the bucket's content id and row count, sorted by hash: a
//!   lookup is one binary search, and a key with no bucket is a definite miss there. Two keys
//!   of one hash are a typed refusal ([`ImplIndexError::BucketHashClash`]), so a hash names
//!   one bucket; the bucket's own key is still compared by the reader.
//! - **`NoImplsOf`** (`NoImplsRoot`, version 2) is two sorted columns of `u64`: a mark per
//!   trait the crate has an impl of ([`trait_mark`]) and a mark per bucket (the first 8 bytes
//!   of its hash). A mark absent is an exact "none": every impl puts its trait's mark and its
//!   bucket's mark in. A mark present that is another key's (two keys' marks equal) sends the
//!   reader to the directory, which answers exactly: the false positive is one wasted probe,
//!   never a wrong answer. It is 8 bytes a bucket, a seventh of the directory, so the crates a
//!   lookup cannot use are passed over on a record that stays in the page cache.
//!
//! - **What the writer did not read** is filed too, never dropped (bucket version 2,
//!   `NoImplsOf` version 2; lane BK, 2026-11-17): a trait impl whose trait path did not
//!   resolve is in [`BucketKey::UnreadTrait`] by its self shape, a header not read to trait or
//!   inherent in [`BucketKey::Blind`], and a self type not read to a shape is filed as
//!   [`SelfShape::BLANKET`] with [`ImplFlags::SELF_UNREAD`] or [`ImplFlags::SELF_NO_TYPE`]
//!   (inherent ones too, under `Inherent { BLANKET }`). While a crate holds an unread row its
//!   `NoImplsOf` rules no trait out ([`NoImplsOfView::may_hold_trait`]).
//!
//! [`build_impl_index`] takes the rows in any order and writes equal bytes for every order.
//! Adding an impl changes its one bucket, the directory and, when its trait or bucket is new
//! to the crate, `NoImplsOf`: no other bucket, and no other crate's records.

use core::fmt;
use core::mem::offset_of;

use blaze_format::{FormatError, Part, Record, SchemaHash, Slice, Writer, WriterOpts, schema_of};
use blaze_id::{ContentId, CrateItemKey, ItemKey, Portable};
use blaze_own::{AVec, Alloc};

use crate::column::RecordBytes;
use crate::crate_graph::slice_field;
use crate::impl_row::{ImplFlags, ImplRow};
use crate::shape::SelfShape;

/// The most impl rows one crate's index holds.
pub const MAX_ROWS: usize = 1 << 22;

/// The domain of a bucket key's hash.
const BUCKET_DOMAIN: &str = "blaze-impls bucket key 1";

const KIND_TRAIT: u32 = 1;
const KIND_BLANKET: u32 = 2;
const KIND_INHERENT: u32 = 3;
const KIND_UNREAD_TRAIT: u32 = 4;
const KIND_BLIND: u32 = 5;

/// 16 bytes of the hash of a [`BucketKey`]: what the directory is sorted and searched by.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct BucketHash(pub [u8; 16]);

// SAFETY: bytes of a content hash.
unsafe impl Portable for BucketHash {}

impl BucketHash {
    /// Its first 8 bytes: the bucket's mark in `NoImplsOf`.
    pub fn mark(self) -> u64 {
        let [a, b, c, d, e, f, g, h, ..] = self.0;
        u64::from_le_bytes([a, b, c, d, e, f, g, h])
    }
}

impl fmt::Display for BucketHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// The mark of `trait_` in `NoImplsOf`: 8 bytes of its key's hash, past the crate prefix and
/// the inner bit, so traits of one crate do not share it.
pub fn trait_mark(trait_: ItemKey) -> u64 {
    let bytes = trait_.0.0;
    u64::from_le_bytes([
        bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21], bytes[22], bytes[23],
    ])
}

/// What a bucket files: the key of a lookup.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum BucketKey {
    /// The impls of a trait for one self shape (never a blanket's or a placeholder's).
    Trait { trait_: ItemKey, shape: SelfShape },
    /// A trait's blanket impls.
    Blanket { trait_: ItemKey },
    /// The inherent impls of one self shape; under [`SelfShape::BLANKET`], those whose self
    /// type was not read to a shape ([`ImplFlags::SELF_UNREAD`], [`ImplFlags::SELF_NO_TYPE`]).
    Inherent { shape: SelfShape },
    /// The trait impls whose trait path the writer did not resolve ([`ImplFlags::TRAIT_UNREAD`]),
    /// by their self shape ([`SelfShape::BLANKET`] where that was not read either): a
    /// candidate of every trait question at that shape.
    UnreadTrait { shape: SelfShape },
    /// The impls whose header was not read far enough to tell a trait impl from an inherent
    /// one ([`ImplFlags::BLIND`]): a candidate of every question.
    Blind,
}

// SAFETY: keys and a shape, each `Portable`.
unsafe impl Portable for BucketKey {}

impl BucketKey {
    /// The bucket `row` is filed in.
    pub fn of(row: &ImplRow) -> BucketKey {
        if row.flags.contains(ImplFlags::BLIND) {
            return BucketKey::Blind;
        }
        if row.flags.contains(ImplFlags::TRAIT_UNREAD) {
            return BucketKey::UnreadTrait { shape: row.shape };
        }
        match row.trait_ {
            Some(trait_) if row.shape.is_blanket() => BucketKey::Blanket { trait_ },
            Some(trait_) => BucketKey::Trait { trait_, shape: row.shape },
            None => BucketKey::Inherent { shape: row.shape },
        }
    }

    /// Its kind, trait and shape as a bucket's head holds them: zeros for no trait.
    fn parts(self) -> (u32, [u8; 32], [u8; 32]) {
        match self {
            BucketKey::Trait { trait_, shape } => (KIND_TRAIT, trait_.0.0, shape.to_bytes()),
            BucketKey::Blanket { trait_ } => (KIND_BLANKET, trait_.0.0, SelfShape::BLANKET.to_bytes()),
            BucketKey::Inherent { shape } => (KIND_INHERENT, [0; 32], shape.to_bytes()),
            BucketKey::UnreadTrait { shape } => (KIND_UNREAD_TRAIT, [0; 32], shape.to_bytes()),
            BucketKey::Blind => (KIND_BLIND, [0; 32], SelfShape::BLANKET.to_bytes()),
        }
    }

    /// The key a bucket's head holds, refused when no key writes it.
    fn from_parts(kind: u32, trait_: [u8; 32], shape: [u8; 32]) -> Option<BucketKey> {
        let shape = SelfShape::from_bytes(shape).ok()?;
        let trait_key = ItemKey(ContentId(trait_));
        match kind {
            KIND_TRAIT if !shape.files_no_impl() => Some(BucketKey::Trait { trait_: trait_key, shape }),
            KIND_BLANKET if shape.is_blanket() => Some(BucketKey::Blanket { trait_: trait_key }),
            KIND_INHERENT if trait_ == [0; 32] && shape != SelfShape::PLACEHOLDER => {
                Some(BucketKey::Inherent { shape })
            }
            KIND_UNREAD_TRAIT if trait_ == [0; 32] && shape != SelfShape::PLACEHOLDER => {
                Some(BucketKey::UnreadTrait { shape })
            }
            KIND_BLIND if trait_ == [0; 32] && shape.is_blanket() => Some(BucketKey::Blind),
            _ => None,
        }
    }

    /// The trait it files impls of, none for inherent impls and unread ones.
    pub fn trait_(self) -> Option<ItemKey> {
        match self {
            BucketKey::Trait { trait_, .. } | BucketKey::Blanket { trait_ } => Some(trait_),
            BucketKey::Inherent { .. } | BucketKey::UnreadTrait { .. } | BucketKey::Blind => None,
        }
    }

    /// The self shape of its rows.
    pub fn shape(self) -> SelfShape {
        match self {
            BucketKey::Trait { shape, .. } | BucketKey::Inherent { shape } | BucketKey::UnreadTrait { shape } => shape,
            BucketKey::Blanket { .. } | BucketKey::Blind => SelfShape::BLANKET,
        }
    }

    /// Whether its rows' trait is not known ([`BucketKey::UnreadTrait`], [`BucketKey::Blind`]):
    /// a crate holding one is never passed over by its `NoImplsOf`'s trait column alone.
    pub fn is_unread(self) -> bool {
        matches!(self, BucketKey::UnreadTrait { .. } | BucketKey::Blind)
    }

    /// The hash the directory finds it by: of its kind, trait and shape.
    pub fn hash(self) -> BucketHash {
        let (kind, trait_, shape) = self.parts();
        let mut bytes = [0u8; 68];
        bytes[..4].copy_from_slice(&kind.to_le_bytes());
        bytes[4..36].copy_from_slice(&trait_);
        bytes[36..].copy_from_slice(&shape);
        let id = ContentId::of(BUCKET_DOMAIN, &bytes);
        let mut hash = [0u8; 16];
        hash.copy_from_slice(&id.0[..16]);
        BucketHash(hash)
    }
}

/// Why rows are not a crate's impl index ([`ImplIndexError::TooManyRows`],
/// [`ImplIndexError::BucketHashClash`]), or why one row is left out of it (the others, each
/// naming its impl, in [`CrateImpls::refused`]): one refused impl is a finding on that impl,
/// never an empty index for the crate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImplIndexError {
    TooManyRows { krate: CrateItemKey, count: usize },
    /// An impl whose key is not of the crate whose index is built.
    ForeignImpl { krate: CrateItemKey, impl_: ItemKey },
    /// One impl given twice: every row of it is left out, so the index is the same for every
    /// order of the rows.
    DuplicateImpl { impl_: ItemKey },
    /// An impl for a placeholder: a goal's shape, which no impl's self type simplifies to.
    PlaceholderSelf { impl_: ItemKey },
    /// `impl<T> T { .. }`: an inherent impl for a parameter (rustc's E0118).
    InherentBlanket { impl_: ItemKey },
    /// An inherent impl with a flag only a trait impl has (`unsafe`, `!`, `default`, a
    /// reservation, a derive). `const impl Type { .. }` is an inherent impl's own form.
    InherentFlags { impl_: ItemKey, flags: ImplFlags },
    /// Two bucket keys with one hash: `first` and `second` are an impl of each.
    BucketHashClash { first: ItemKey, second: ItemKey },
    /// What the writer read of a header, said in flags that do not go together: a self type
    /// both read and not read to a shape, an unread row with a trait, a why on a read row.
    ReadFlags { impl_: ItemKey, flags: ImplFlags },
}

/// The flags an inherent impl may have: `const impl Type { .. }`, and a self type not read to a
/// shape.
const INHERENT_FLAGS: ImplFlags =
    ImplFlags::CONST.union(ImplFlags::SELF_UNREAD).union(ImplFlags::SELF_NO_TYPE);

impl fmt::Display for ImplIndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImplIndexError::TooManyRows { krate, count } => write!(
                f,
                "the crate {krate:x} has {count} impls, past the impl index's bound of {MAX_ROWS}"
            ),
            ImplIndexError::ForeignImpl { krate, impl_ } => {
                write!(f, "the impl {impl_:x} is not of the crate {krate:x} whose impl index is built")
            }
            ImplIndexError::DuplicateImpl { impl_ } => {
                write!(f, "the impl {impl_:x} is given to the impl index twice")
            }
            ImplIndexError::PlaceholderSelf { impl_ } => write!(
                f,
                "the impl {impl_:x} has a placeholder for its self shape, which only a goal's has"
            ),
            ImplIndexError::InherentBlanket { impl_ } => write!(
                f,
                "the inherent impl {impl_:x} is for a type parameter (E0118): no shape files it"
            ),
            ImplIndexError::InherentFlags { impl_, flags } => write!(
                f,
                "the inherent impl {impl_:x} has the flags {:#x}, which only a trait impl has",
                flags.bits()
            ),
            ImplIndexError::BucketHashClash { first, second } => write!(
                f,
                "the impls {first:x} and {second:x} are filed under two bucket keys of one hash"
            ),
            ImplIndexError::ReadFlags { impl_, flags } => write!(
                f,
                "the impl {impl_:x} has the flags {:#x}: what its header's writer read does not go together",
                flags.bits()
            ),
        }
    }
}

impl core::error::Error for ImplIndexError {}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    struct StoredRow: 1 {
        impl_: ContentId,
        header: ContentId,
        flags: u32,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy)]
    struct BucketRoot: 1 {
        hash: [u8; 16],
        trait_: ContentId,
        shape: [u8; 32],
        kind: u32,
        rows: Slice<StoredRow>,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    struct DirEntry: 1 {
        hash: [u8; 16],
        bucket: ContentId,
        rows: u32,
        kind: u32,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy)]
    struct DirectoryRoot: 1 {
        krate: ContentId,
        entries: Slice<DirEntry>,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy)]
    struct NoImplsRoot: 2 {
        krate: ContentId,
        traits: Slice<u64>,
        buckets: Slice<u64>,
        /// How many rows of the crate's are [`ImplFlags::TRAIT_UNREAD`]: while any is, the
        /// trait column rules no trait out.
        unread_trait: u32,
        /// How many rows of the crate's are [`ImplFlags::BLIND`].
        blind: u32,
    }
}

/// The schema of an impl bucket's buffer. Version 2 (lane BK, 2026-11-17): the bucket kinds
/// [`BucketKey::UnreadTrait`] and [`BucketKey::Blind`], inherent rows under
/// [`SelfShape::BLANKET`], and the flags of what the writer read ([`ImplFlags::BARE`] on).
pub const BUCKET_SCHEMA: SchemaHash =
    schema_of(&[Part::Str("ImplBucket"), Part::U64(2), Part::Schema(BucketRoot::SCHEMA)]);

/// The schema of an impl directory's buffer.
pub const DIRECTORY_SCHEMA: SchemaHash =
    schema_of(&[Part::Str("ImplDirectory"), Part::U64(1), Part::Schema(DirectoryRoot::SCHEMA)]);

/// The schema of a `NoImplsOf` buffer. Version 2: the counts of unread rows.
pub const NO_IMPLS_SCHEMA: SchemaHash =
    schema_of(&[Part::Str("NoImplsOf"), Part::U64(2), Part::Schema(NoImplsRoot::SCHEMA)]);

/// One bucket a build wrote: its content id (what the directory names it by), its key and
/// its buffer.
#[derive(Clone, Debug)]
pub struct Bucket<'a> {
    pub id: ContentId,
    pub hash: BucketHash,
    pub key: BucketKey,
    pub bytes: RecordBytes<'a>,
}

/// A crate's impl index as [`build_impl_index`] writes it: three kinds of record, each a
/// buffer of its own. The buckets are in the directory's order.
#[derive(Clone, Debug)]
pub struct CrateImpls<'a> {
    pub directory: RecordBytes<'a>,
    pub no_impls_of: RecordBytes<'a>,
    pub buckets: AVec<'a, Bucket<'a>>,
    /// The rows left out, each a finding on its impl, by impl key: the index holds the rest.
    pub refused: AVec<'a, ImplIndexError>,
}

/// Refuses `row` as a row of `krate`'s index.
fn check_row(krate: CrateItemKey, row: &ImplRow) -> Result<(), ImplIndexError> {
    let impl_ = row.impl_;
    if !impl_.is_of(krate) {
        return Err(ImplIndexError::ForeignImpl { krate, impl_ });
    }
    if row.shape == SelfShape::PLACEHOLDER {
        return Err(ImplIndexError::PlaceholderSelf { impl_ });
    }
    let flags = row.flags;
    let self_unread = flags.contains(ImplFlags::SELF_UNREAD);
    let self_no_type = flags.contains(ImplFlags::SELF_NO_TYPE);
    let trait_unread = flags.contains(ImplFlags::TRAIT_UNREAD);
    let blind = flags.contains(ImplFlags::BLIND);
    let refused = (self_unread && self_no_type)
        // A self type not read to a shape is filed as for any type.
        || ((self_unread || self_no_type) && !row.shape.is_blanket())
        // An unread row has no trait, and is one kind of unread.
        || ((trait_unread || blind) && row.trait_.is_some())
        || (trait_unread && blind)
        // A blind row has no self shape read either.
        || (blind && (!row.shape.is_blanket() || self_no_type))
        // A why only on an unread row.
        || (flags.why() != 0 && !trait_unread && !blind)
        // `BARE` is a trait's, read or unread.
        || (flags.contains(ImplFlags::BARE) && row.trait_.is_none() && !trait_unread);
    if refused {
        return Err(ImplIndexError::ReadFlags { impl_, flags });
    }
    if trait_unread || blind {
        return Ok(());
    }
    if row.trait_.is_none() {
        if row.is_param_self() {
            return Err(ImplIndexError::InherentBlanket { impl_ });
        }
        if !INHERENT_FLAGS.contains(row.flags) {
            return Err(ImplIndexError::InherentFlags { impl_, flags: row.flags });
        }
    }
    Ok(())
}

/// `krate`'s impl index of `rows`, in any order, its buffers in `alloc`: equal bytes for
/// every order of the same rows. A row [`check_row`] refuses, or an impl given twice, is
/// left out and named in [`CrateImpls::refused`]; only a bound of the whole index
/// ([`ImplIndexError::TooManyRows`], [`ImplIndexError::BucketHashClash`]) refuses it all.
pub fn build_impl_index<'a>(
    krate: CrateItemKey,
    rows: &[ImplRow],
    alloc: Alloc<'a>,
) -> Result<CrateImpls<'a>, ImplIndexError> {
    if rows.len() > MAX_ROWS {
        return Err(ImplIndexError::TooManyRows { krate, count: rows.len() });
    }
    // The rows by impl key: a key given twice is refused whole, whichever row came first.
    let mut sorted: AVec<'a, ImplRow> = AVec::with_capacity_in(rows.len(), alloc);
    sorted.extend(rows.iter().copied());
    sorted.sort_unstable();
    let mut kept: AVec<'a, ImplRow> = AVec::with_capacity_in(rows.len(), alloc);
    let mut refused: AVec<'a, ImplIndexError> = AVec::new_in(alloc);
    for group in sorted.chunk_by(|a, b| a.impl_ == b.impl_) {
        match group {
            [row] => match check_row(krate, row) {
                Ok(()) => kept.push(*row),
                Err(error) => refused.push(error),
            },
            [row, ..] => refused.push(ImplIndexError::DuplicateImpl { impl_: row.impl_ }),
            [] => unreachable!("`chunk_by` yields no empty group"),
        }
    }
    drop(sorted);
    let rows = &kept[..];
    // Every row with its bucket, by the bucket's hash and then the impl's key.
    let mut keyed: AVec<'a, (BucketHash, BucketKey, ImplRow)> =
        AVec::with_capacity_in(rows.len(), alloc);
    keyed.extend(rows.iter().map(|row| {
        let key = BucketKey::of(row);
        (key.hash(), key, *row)
    }));
    keyed.sort_unstable_by(|a, b| (a.0, a.2.impl_).cmp(&(b.0, b.2.impl_)));

    let mut buckets: AVec<'a, Bucket<'a>> = AVec::new_in(alloc);
    let mut entries: AVec<'a, DirEntry> = AVec::new_in(alloc);
    let mut stored: AVec<'a, StoredRow> = AVec::new_in(alloc);
    let mut start = 0;
    while start < keyed.len() {
        let (hash, key, first) = keyed[start];
        let mut end = start + 1;
        while end < keyed.len() && keyed[end].0 == hash {
            if keyed[end].1 != key {
                return Err(ImplIndexError::BucketHashClash {
                    first: first.impl_,
                    second: keyed[end].2.impl_,
                });
            }
            end += 1;
        }
        stored.clear();
        stored.extend(keyed[start..end].iter().map(|(_, _, row)| StoredRow {
            impl_: row.impl_.0,
            header: row.header,
            flags: row.flags.bits(),
        }));
        let bytes = write_bucket(hash, key, &stored, alloc);
        let id = blaze_format::buffer_id(&bytes);
        let (kind, _, _) = key.parts();
        entries.push(DirEntry { hash: hash.0, bucket: id, rows: (end - start) as u32, kind });
        buckets.push(Bucket { id, hash, key, bytes });
        start = end;
    }
    drop(stored);
    drop(keyed);

    let mut traits: AVec<'a, u64> = AVec::with_capacity_in(rows.len(), alloc);
    traits.extend(rows.iter().filter_map(|row| row.trait_).map(trait_mark));
    traits.sort_unstable();
    traits.dedup();
    let mut marks: AVec<'a, u64> = AVec::with_capacity_in(entries.len(), alloc);
    marks.extend(entries.iter().map(|entry| BucketHash(entry.hash).mark()));
    marks.sort_unstable();
    marks.dedup();
    // At most `MAX_ROWS` rows, checked above: each count fits.
    let unread = Unread {
        trait_: rows.iter().filter(|row| row.flags.contains(ImplFlags::TRAIT_UNREAD)).count() as u32,
        blind: rows.iter().filter(|row| row.flags.contains(ImplFlags::BLIND)).count() as u32,
    };

    Ok(CrateImpls {
        directory: write_directory(krate, &entries, alloc),
        no_impls_of: write_no_impls(krate, &traits, &marks, unread, alloc),
        buckets,
        refused,
    })
}

fn write_bucket<'a>(hash: BucketHash, key: BucketKey, rows: &[StoredRow], alloc: Alloc<'a>) -> RecordBytes<'a> {
    let (kind, trait_, shape) = key.parts();
    let mut out = Writer::new(WriterOpts { schema: BUCKET_SCHEMA, alloc: alloc, capacity: None });
    let rows_at = out.push_slice(rows);
    let at = out.next::<BucketRoot>();
    let root = BucketRoot {
        hash: hash.0,
        trait_: ContentId(trait_),
        shape,
        kind,
        rows: slice_field(at, offset_of!(BucketRoot, rows), rows_at, rows.len()),
    };
    let written = out.push(&root);
    debug_assert_eq!(written, at);
    out.finish(at)
}

fn write_directory<'a>(krate: CrateItemKey, entries: &[DirEntry], alloc: Alloc<'a>) -> RecordBytes<'a> {
    let mut out = Writer::new(WriterOpts { schema: DIRECTORY_SCHEMA, alloc: alloc, capacity: None });
    let entries_at = out.push_slice(entries);
    let at = out.next::<DirectoryRoot>();
    let root = DirectoryRoot {
        krate: krate.0,
        entries: slice_field(at, offset_of!(DirectoryRoot, entries), entries_at, entries.len()),
    };
    let written = out.push(&root);
    debug_assert_eq!(written, at);
    out.finish(at)
}

/// How many of a crate's rows are unread, by kind ([`NoImplsRoot`]).
#[derive(Clone, Copy)]
struct Unread {
    trait_: u32,
    blind: u32,
}

fn write_no_impls<'a>(krate: CrateItemKey, traits: &[u64], buckets: &[u64], unread: Unread, alloc: Alloc<'a>) -> RecordBytes<'a> {
    let mut out = Writer::new(WriterOpts { schema: NO_IMPLS_SCHEMA, alloc: alloc, capacity: None });
    let traits_at = out.push_slice(traits);
    let buckets_at = out.push_slice(buckets);
    let at = out.next::<NoImplsRoot>();
    let root = NoImplsRoot {
        krate: krate.0,
        traits: slice_field(at, offset_of!(NoImplsRoot, traits), traits_at, traits.len()),
        buckets: slice_field(at, offset_of!(NoImplsRoot, buckets), buckets_at, buckets.len()),
        unread_trait: unread.trait_,
        blind: unread.blind,
    };
    let written = out.push(&root);
    debug_assert_eq!(written, at);
    out.finish(at)
}

/// Where the directory says a bucket is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DirHit {
    pub hash: BucketHash,
    /// The bucket's content id.
    pub bucket: ContentId,
    pub rows: u32,
}

/// A crate's impl directory read in place.
#[derive(Clone, Copy, Debug)]
pub struct DirectoryView<'a> {
    krate: CrateItemKey,
    entries: &'a [DirEntry],
}

impl<'a> DirectoryView<'a> {
    /// The directory in `buffer`, its schema and layout checked: O(1), the entries being
    /// plain.
    pub fn read(buffer: &'a [u8]) -> Result<DirectoryView<'a>, FormatError> {
        let root = blaze_format::open_as::<DirectoryRoot>(buffer, DIRECTORY_SCHEMA)?;
        Ok(DirectoryView { krate: CrateItemKey(root.krate), entries: root.entries.follow(buffer)? })
    }

    /// The crate whose directory it is.
    pub fn krate(&self) -> CrateItemKey {
        self.krate
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The bucket of `hash`, none when the crate has no impl of its key.
    pub fn lookup(&self, hash: BucketHash) -> Option<DirHit> {
        let at = self.entries.binary_search_by(|entry| entry.hash.cmp(&hash.0)).ok()?;
        let entry = &self.entries[at];
        Some(DirHit { hash, bucket: entry.bucket, rows: entry.rows })
    }

    /// Every bucket, by hash.
    pub fn hits(&self) -> impl Iterator<Item = DirHit> + use<'a> {
        let entries: &'a [DirEntry] = self.entries;
        entries.iter().map(|entry| DirHit {
            hash: BucketHash(entry.hash),
            bucket: entry.bucket,
            rows: entry.rows,
        })
    }
}

/// An impl bucket read in place.
#[derive(Clone, Copy, Debug)]
pub struct BucketView<'a> {
    hash: BucketHash,
    key: BucketKey,
    rows: &'a [StoredRow],
    at: u64,
}

impl<'a> BucketView<'a> {
    /// The bucket in `buffer`, its schema, layout and key checked: O(1), the rows being plain;
    /// a row's flags are checked when it is read.
    pub fn read(buffer: &'a [u8]) -> Result<BucketView<'a>, FormatError> {
        let root = blaze_format::open_as::<BucketRoot>(buffer, BUCKET_SCHEMA)?;
        let at = blaze_format::header(buffer)?.root;
        let Some(key) = BucketKey::from_parts(root.kind, root.trait_.0, root.shape) else {
            return Err(FormatError::Invalid { at, what: "an impl bucket's key that no bucket writes" });
        };
        Ok(BucketView { hash: BucketHash(root.hash), key, rows: root.rows.follow(buffer)?, at })
    }

    pub fn key(&self) -> BucketKey {
        self.key
    }

    pub fn hash(&self) -> BucketHash {
        self.hash
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Row `at`, by impl key's order.
    pub fn row(&self, at: usize) -> Option<Result<ImplRow, FormatError>> {
        let stored = self.rows.get(at)?;
        Some(match ImplFlags::from_bits(stored.flags) {
            Ok(flags) => Ok(ImplRow {
                impl_: ItemKey(stored.impl_),
                trait_: self.key.trait_(),
                shape: self.key.shape(),
                flags,
                header: stored.header,
            }),
            Err(_) => Err(FormatError::Invalid { at: self.at, what: "an impl row with flags blaze does not know" }),
        })
    }

    /// The row of `impl_`, by a binary search.
    pub fn find(&self, impl_: ItemKey) -> Option<Result<ImplRow, FormatError>> {
        let at = self.rows.binary_search_by(|row| row.impl_.cmp(&impl_.0)).ok()?;
        self.row(at)
    }
}

/// A crate's `NoImplsOf` read in place.
#[derive(Clone, Copy, Debug)]
pub struct NoImplsOfView<'a> {
    krate: CrateItemKey,
    traits: &'a [u64],
    buckets: &'a [u64],
    unread_trait: u32,
    blind: u32,
}

impl<'a> NoImplsOfView<'a> {
    /// The record in `buffer`, its schema and layout checked: O(1).
    pub fn read(buffer: &'a [u8]) -> Result<NoImplsOfView<'a>, FormatError> {
        let root = blaze_format::open_as::<NoImplsRoot>(buffer, NO_IMPLS_SCHEMA)?;
        Ok(NoImplsOfView {
            krate: CrateItemKey(root.krate),
            traits: root.traits.follow(buffer)?,
            buckets: root.buckets.follow(buffer)?,
            unread_trait: root.unread_trait,
            blind: root.blind,
        })
    }

    pub fn krate(&self) -> CrateItemKey {
        self.krate
    }

    /// The sorted `trait_mark` table of traits implemented in this crate.
    pub fn trait_marks(&self) -> &'a [u64] {
        self.traits
    }

    /// False when the crate has no impl of `trait_`: exact. True when it may, and always
    /// while the crate has an impl whose trait was not read ([`ImplFlags::TRAIT_UNREAD`],
    /// [`ImplFlags::BLIND`]): that impl may be of `trait_`.
    pub fn may_hold_trait(&self, trait_: ItemKey) -> bool {
        self.has_unread() || self.traits.binary_search(&trait_mark(trait_)).is_ok()
    }

    /// Whether the crate has an impl whose trait was not read.
    pub fn has_unread(&self) -> bool {
        self.unread_trait != 0 || self.blind != 0
    }

    /// How many of the crate's impls are [`ImplFlags::TRAIT_UNREAD`] and [`ImplFlags::BLIND`].
    pub fn unread(&self) -> (u32, u32) {
        (self.unread_trait, self.blind)
    }

    /// False when the crate has no bucket of `hash`: exact. True when it may.
    pub fn may_hold_bucket(&self, hash: BucketHash) -> bool {
        self.buckets.binary_search(&hash.mark()).is_ok()
    }
}
