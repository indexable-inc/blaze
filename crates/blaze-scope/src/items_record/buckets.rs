//! The bucket index of the items record (drive.md, "End state: the whole pipeline on the
//! command", piece 12; the bullet "An impl bucket is a fact, not a task"): the crate's impls
//! filed by RESOLVED (trait or "inherent", the self type's head), so "which impls of this crate
//! can answer `T: Trait`" is one binary search over the record's bytes, and a reader that asks
//! one bucket reads no other impl's row and opens no text.
//!
//! Before this section a reader filed EVERY impl of a crate it asked anything (each impl row's
//! trait path resolved in the reader, and the header's text read where the row could not say):
//! one question cost in proportion to the crate. Here the resolving is done ONCE, when the
//! record is written, by the names the writer is given ([`HeadNames`]).
//!
//! WHAT IS FILED. An impl whose header the writer resolves is one row of the bucket table
//! under its [`BucketKey`]: the kind (inherent, trait impl, negative trait impl), the trait's
//! key, and the self type's [`Head`] (a nominal or builtin type by its key; a reference, a
//! pointer, a slice, an array, a tuple of n, `!`; or [`Head::Any`] for a self type with no
//! constructor read here: a parameter of the impl, `impl<T> Tr for T`, and a path that names
//! no type). An impl the writer does NOT resolve is never dropped: it is a row of the crate's
//! UNFILED list with why ([`NotFiled`]), and a reader takes the whole list as candidates of
//! every question it asks the crate.
//!
//! WHAT RESOLVES AN IMPL. The trait's path and the self type's head path, each read by tokens
//! ([`crate::task::parse_path`]) and walked through the names the writer holds
//! ([`crate::names::path`], the walk every later stage uses). [`super::encode`] holds only the
//! crate's own names ([`OwnNames`]): a path that leaves the crate (a prelude name, `fmt::Debug`
//! of another crate) is [`NotFiled::TraitNotRead`] or [`NotFiled::SelfNotRead`], never guessed
//! (the walk stops with `Why::NotRead` at the first scope not held, so it answers as the whole
//! graph's names would or not at all). One case is decided without the prelude: a one-segment
//! self path naming a builtin type, where the walk's only layer not held is the standard
//! prelude ([`crate::names::Unheld::Prelude`]), is that builtin, since the prelude binds no type
//! of such a name; so `impl Rem<BigUint> for u64` outside core is filed under `u64` as core's
//! own `impl Rem for u64` is. [`super::encode_with`] takes the names of the whole
//! graph (`Names`), where every path is walked to its end.
//!
//! THE FORM. Additive: after everything the record held before (no existing field or row
//! changes; [`append`] writes after the last byte, 16-aligned). Header fields
//! [`F_BUCKETS_COUNT`]/[`F_BUCKETS_AT`]: the bucket table, 80-byte rows sorted by their first
//! 72 bytes: the bucket key 68 (kind u8; trait key 32, zero for inherent; head code u8; arity
//! u16 big-endian; head key 32, zero unless the head is a named type), then the impl's row in
//! the impl table u32; 8 zero bytes. Header fields [`F_UNFILED_COUNT`]/[`F_UNFILED_AT`]: the
//! unfiled list, 8-byte rows sorted by impl row: impl row u32, why u8, 3 zero bytes. Both are
//! a function of the impl table and the names: any worker writes equal bytes.

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{CrateRootBuilder, DefKind, ItemKind, Namespace, PathKind, RecordBytes, ScopeBuilder};

use super::{BARE, HAS_TRAIT, ItemsRecord, ItemsRecordError, SelfHead, T_IMPLS, key_at, le32, pad, path_and_bare, put32, u32_of};
use crate::builtin::is_builtin_type;
use crate::names::{self, PathError, Resolved, Unheld, VariantHit, Why, World, Written, variant_in};
use crate::resolve::{CrateNames, Names};
use crate::task::parse_path;

/// The header field holding how many rows the bucket table has.
pub(crate) const F_BUCKETS_COUNT: usize = 31;
/// The header field holding the bucket table's offset.
pub(crate) const F_BUCKETS_AT: usize = 32;
/// The header field holding how many rows the unfiled list has.
pub(crate) const F_UNFILED_COUNT: usize = 33;
/// The header field holding the unfiled list's offset.
pub(crate) const F_UNFILED_AT: usize = 34;
/// A bucket row: key 68, impl row u32, 8 zero bytes.
pub(crate) const BUCKET_ROW: usize = 80;
/// An unfiled row: impl row u32, why u8, 3 zero bytes.
pub(crate) const UNFILED_ROW: usize = 8;
/// A whole bucket key: kind 1, trait key 32, head code 1, arity 2, head key 32.
pub const KEY: usize = 68;
/// A key's prefix naming every head of one trait: kind and trait key.
const TRAIT_PREFIX: usize = 33;

const KIND_INHERENT: u8 = 0;
const KIND_TRAIT: u8 = 1;
const KIND_NEGATIVE: u8 = 2;

/// The domain of a bucket's digest ([`ItemsRecord::bucket_digest`]).
const BUCKET_DOMAIN: &str = "blaze-scope items bucket 1";
/// The domain of the unfiled list's digest ([`ItemsRecord::unfiled_digest`]).
const UNFILED_DOMAIN: &str = "blaze-scope items unfiled 1";

/// The head a bucket is filed under: the outermost constructor of the impl's self type,
/// resolved.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Head {
    /// A struct, enum, union or builtin type, by its key (a builtin's key is
    /// [`crate::builtin_type`]'s, the same in every crate).
    Named(ItemKey),
    /// No constructor: a parameter of the impl (a blanket impl), or a path that names no type.
    /// Such an impl is a candidate at every head of its trait.
    Any,
    Ref,
    RefMut,
    PtrConst,
    PtrMut,
    Slice,
    Array,
    Never,
    /// A tuple of this many elements (`()` is 0).
    Tuple(u16),
}

impl Head {
    /// The code (as [`SelfHead`]'s byte for the same form), the arity and the key.
    fn parts(self) -> (u8, u16, [u8; 32]) {
        match self {
            Head::Named(k) => (0, 0, k.0.0),
            Head::Any => (1, 0, [0; 32]),
            Head::Ref => (2, 0, [0; 32]),
            Head::RefMut => (3, 0, [0; 32]),
            Head::PtrConst => (4, 0, [0; 32]),
            Head::PtrMut => (5, 0, [0; 32]),
            Head::Slice => (6, 0, [0; 32]),
            Head::Array => (7, 0, [0; 32]),
            Head::Never => (8, 0, [0; 32]),
            Head::Tuple(n) => (9, n, [0; 32]),
        }
    }
}

/// Which impls a question asks a crate for: a whole key (one trait or "inherent", one head)
/// or a prefix (every negative impl of one trait). Its bytes are the sort key of the rows.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BucketKey {
    bytes: [u8; KEY],
    len: u8,
}

impl BucketKey {
    fn whole(kind: u8, trait_: Option<ItemKey>, head: Head) -> BucketKey {
        let mut bytes = [0u8; KEY];
        bytes[0] = kind;
        if let Some(t) = trait_ {
            bytes[1..33].copy_from_slice(&t.0.0);
        }
        let (code, arity, key) = head.parts();
        bytes[33] = code;
        bytes[34..36].copy_from_slice(&arity.to_be_bytes());
        bytes[36..68].copy_from_slice(&key);
        BucketKey { bytes, len: KEY as u8 }
    }

    /// The inherent impls whose self type has `head`.
    pub fn inherent(head: Head) -> BucketKey {
        BucketKey::whole(KIND_INHERENT, None, head)
    }

    /// The (positive) impls of `trait_` whose self type has `head`.
    pub fn of_trait(trait_: ItemKey, head: Head) -> BucketKey {
        BucketKey::whole(KIND_TRAIT, Some(trait_), head)
    }

    /// Every negative impl of `trait_` (`impl !Tr for X`), whatever its self type.
    pub fn negatives(trait_: ItemKey) -> BucketKey {
        let mut key = BucketKey::whole(KIND_NEGATIVE, Some(trait_), Head::Any);
        key.bytes[TRAIT_PREFIX..].fill(0);
        key.len = TRAIT_PREFIX as u8;
        key
    }

    /// The one negative impl bucket an impl is written to (a reader asks [`Self::negatives`]).
    fn negative(trait_: ItemKey, head: Head) -> BucketKey {
        BucketKey::whole(KIND_NEGATIVE, Some(trait_), head)
    }

    /// The bytes rows are compared by: a whole key, or the prefix asked.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

/// Why the writer did not file an impl under a bucket: the impl is then in the crate's unfiled
/// list, a candidate of every question the crate is asked, and the reader files it itself (by
/// its impl row, or by its header's text). Each class is a cost to remove at its home; a byte
/// is never renumbered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum NotFiled {
    /// The header's tokens did not read.
    HeaderUnread = 0,
    /// The trait as written is no simple path by its tokens.
    TraitNotPath = 1,
    /// The trait's path walks into names the writer did not hold (another crate's).
    TraitNotRead = 2,
    /// The trait's path names nothing (not found, private, ambiguous) in the impl's module.
    TraitNotFound = 3,
    /// The trait's path names something that is no trait, or only a prefix of it does.
    TraitNotTrait = 4,
    /// A self type with no head read from its tokens ([`SelfHead::Other`]: `dyn Tr`, a fn
    /// pointer, `<T as Tr>::Out`, `(T)`, a macro call).
    SelfOther = 5,
    /// The self type's head is no simple path by its tokens.
    SelfNotPath = 6,
    /// The self type's path walks into names the writer did not hold.
    SelfNotRead = 7,
    /// The self type's path names a type alias: its head is the aliased type's, which the
    /// writer does not read.
    SelfAlias = 8,
    /// The self type's path names an item whose kind the writer did not hold (another crate's
    /// items).
    SelfKindNotHeld = 9,
    /// The row of a refused derive's gap ([`crate::items_record::ImplRow::refused_call`]):
    /// no impl item, so no bucket; every reader files it by its row (D193 point 5).
    RefusedCall = 10,
}

/// The classes of [`NotFiled`], by byte: for a count per class.
pub const NOT_FILED: [&str; 11] = [
    "header unread",
    "trait not a path",
    "trait's names not held",
    "trait not found",
    "trait path names no trait",
    "self type other",
    "self type not a path",
    "self type's names not held",
    "self type an alias",
    "self type's kind not held",
    "a refused derive's gap",
];

impl NotFiled {
    fn from_byte(byte: u8) -> Option<NotFiled> {
        Some(match byte {
            0 => NotFiled::HeaderUnread,
            1 => NotFiled::TraitNotPath,
            2 => NotFiled::TraitNotRead,
            3 => NotFiled::TraitNotFound,
            4 => NotFiled::TraitNotTrait,
            5 => NotFiled::SelfOther,
            6 => NotFiled::SelfNotPath,
            7 => NotFiled::SelfNotRead,
            8 => NotFiled::SelfAlias,
            9 => NotFiled::SelfKindNotHeld,
            10 => NotFiled::RefusedCall,
            _ => return None,
        })
    }
}

/// Where the writer files one impl.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Filing {
    At(BucketKey),
    Not(NotFiled),
}

/// The names the writer resolves impl headers by. A walk that needs names not held answers
/// `Why::NotRead` (never another layer's answer), and an item of a crate not held has no kind.
pub trait HeadNames {
    /// `written` in `module`, its last segment in the type namespace: [`crate::names::path`].
    fn type_path(&self, module: ItemKey, written: Written<'_, &str>, alloc: Alloc<'_>) -> Result<Resolved, PathError>;
    /// The kind of the item `key`, or `None` when its crate's items are not held.
    fn item_kind(&self, key: ItemKey) -> Option<ItemKind>;
}

/// The kind of `key`, an item with text of `c`.
fn kind_in(c: &CrateNames<'_>, key: ItemKey) -> Option<ItemKind> {
    let (it, at) = c.tables.texts.get(&key)?;
    let (_, outline) = c.outline_at(*at)?;
    Some(outline.items().get(it.item.0 as usize)?.detail.kind())
}

/// The names of every crate of the graph: every path is walked to its end.
impl HeadNames for Names<'_> {
    fn type_path(&self, module: ItemKey, written: Written<'_, &str>, alloc: Alloc<'_>) -> Result<Resolved, PathError> {
        self.path(module, Namespace::Types, written, alloc)
    }

    fn item_kind(&self, key: ItemKey) -> Option<ItemKind> {
        kind_in(self.crates().get(self.crate_of(key)?)?, key)
    }
}

/// One crate's names alone: what [`super::encode`] holds. A scope, chain, root or variant list
/// of any other crate is not held, so a walk through one stops with `Why::NotRead`.
#[derive(Clone, Copy)]
pub struct OwnNames<'n, 'a>(pub &'n CrateNames<'a>);

impl<'n, 'a> OwnNames<'n, 'a> {
    fn own(&self, key: ItemKey) -> bool {
        key.crate_prefix() == self.0.key.prefix()
    }
}

impl<'n, 'a> World<&'a str> for OwnNames<'n, 'a> {
    type Scope<'w>
        = &'w ScopeBuilder<'w, &'a str>
    where
        Self: 'w;
    type Root<'w>
        = &'w CrateRootBuilder<'w, &'a str>
    where
        Self: 'w;

    fn scope(&self, module: ItemKey) -> Option<&ScopeBuilder<'_, &'a str>> {
        self.0.done.get(&module).map(|r| &r.scope)
    }

    fn chain(&self, module: ItemKey) -> Option<&[ItemKey]> {
        self.0.chains.get(&module).map(|c| c.as_slice())
    }

    fn root_of(&self, key: ItemKey) -> Option<&CrateRootBuilder<'_, &'a str>> {
        self.own(key).then_some(&self.0.crate_root)
    }

    fn root(&self, krate: CrateItemKey) -> Option<&CrateRootBuilder<'_, &'a str>> {
        (krate == self.0.key).then_some(&self.0.crate_root)
    }

    fn variant(&self, owner: ItemKey, name: &'a str) -> Option<Option<VariantHit>> {
        Some(variant_in(self.0.tables.enums.get(&owner)?.as_slice(), name))
    }

    fn no_implicit_prelude(&self, module: ItemKey) -> bool {
        let Some(chain) = self.0.chains.get(&module) else { return false };
        chain.iter().any(|m| self.0.info(*m).is_some_and(|info| info.no_implicit_prelude))
    }

    fn anonymous(&self, module: ItemKey) -> bool {
        self.0.anonymous(module)
    }
}

impl HeadNames for OwnNames<'_, '_> {
    fn type_path(&self, module: ItemKey, written: Written<'_, &str>, alloc: Alloc<'_>) -> Result<Resolved, PathError> {
        // The records are read as of the segments' shorter borrow (as `Names::path` reads its).
        fn walk<'s>(own: &'s CrateNames<'s>, module: ItemKey, written: Written<'_, &'s str>, alloc: Alloc<'_>) -> Result<Resolved, PathError> {
            names::path(&OwnNames(own), module, Namespace::Types, written, alloc)
        }
        walk(self.0, module, written, alloc)
    }

    fn item_kind(&self, key: ItemKey) -> Option<ItemKind> {
        if !self.own(key) {
            return None;
        }
        kind_in(self.0, key)
    }
}

/// `text` (a path as a header writes it, without arguments) walked in `module` of crate
/// `krate`: `None` when its tokens are no simple path. `root`: the crate its first segment names
/// when an expansion wrote `crate` for another crate's `$crate` (the impl row's root,
/// token-mark-2 R2), never read as `krate`'s.
fn walked<H: HeadNames>(heads: &H, krate: CrateItemKey, module: ItemKey, text: &str, root: Option<CrateItemKey>, alloc: Alloc<'_>) -> Option<(Result<Resolved, PathError>, usize)> {
    let mut path = parse_path(text, krate, alloc)?;
    // A root is kept only for a first segment `crate`/`$crate` (items_record.rs `root_of_path`,
    // by `root_by_mark`), which `parse_path` reads as one of these two kinds.
    if let (Some(k), PathKind::Crate | PathKind::DollarCrate(_)) = (root, path.kind) {
        path.kind = PathKind::DollarCrate(k);
    }
    let written = Written { kind: path.kind, segments: &path.segments };
    Some((heads.type_path(module, written, alloc), path.segments.len()))
}

/// Where the impl written in `module` of crate `krate` is filed: its trait as written
/// (`trait_`, with its arguments; `None` for an inherent impl), whether it is a negative impl,
/// and its self type's head as written (`head`, with its path for [`SelfHead::Path`]).
///
/// It files as the front end's reader files an impl from its row (blaze-ladder front2
/// buckets.rs `file_row`), so an index answer and a row answer agree: a self path that names
/// no struct, enum, union or builtin (an error of the walk but names not held, a prefix only,
/// a trait, a module) has no constructor ([`Head::Any`]); a trait path must name a trait with
/// its whole path.
pub(super) fn filing<H: HeadNames>(
    heads: &H,
    krate: CrateItemKey,
    module: ItemKey,
    trait_: Option<&str>,
    negative: bool,
    head: SelfHead,
    self_path: Option<&str>,
    trait_root: Option<CrateItemKey>,
    self_root: Option<CrateItemKey>,
    alloc: Alloc<'_>,
) -> Filing {
    let head = match head {
        SelfHead::Other => return Filing::Not(NotFiled::SelfOther),
        SelfHead::Param => Head::Any,
        SelfHead::Ref => Head::Ref,
        SelfHead::RefMut => Head::RefMut,
        SelfHead::PtrConst => Head::PtrConst,
        SelfHead::PtrMut => Head::PtrMut,
        SelfHead::Slice => Head::Slice,
        SelfHead::Array => Head::Array,
        SelfHead::Never => Head::Never,
        SelfHead::Tuple(n) => Head::Tuple(n),
        SelfHead::Path => {
            let Some((found, len)) = self_path.and_then(|text| walked(heads, krate, module, text, self_root, alloc)) else {
                return Filing::Not(NotFiled::SelfNotPath);
            };
            match found {
                // `u64` where every nearer layer is read and binds no type of the name, and only
                // the standard prelude is not held: the builtin type. The prelude binds no type
                // named like a builtin, and its contents are the toolchain's, which every key
                // covers. A glob not held (`use dep::*;`) may bind the name, so stays not read.
                Err(PathError { at: 0, why: Why::NotRead { layer: Unheld::Prelude { builtin: Some(builtin) }, .. }, .. }) if len == 1 => Head::Named(builtin),
                Err(PathError { why: Why::NotRead { .. }, .. }) => return Filing::Not(NotFiled::SelfNotRead),
                Err(_) => Head::Any,
                Ok(r) if r.consumed != len => Head::Any,
                Ok(r) => match r.def.kind {
                    DefKind::Enum => Head::Named(r.def.key),
                    DefKind::Other if is_builtin_type(r.def.key) => Head::Named(r.def.key),
                    DefKind::Other => match heads.item_kind(r.def.key) {
                        None => return Filing::Not(NotFiled::SelfKindNotHeld),
                        Some(ItemKind::Struct | ItemKind::Enum | ItemKind::Union) => Head::Named(r.def.key),
                        Some(ItemKind::TypeAlias) => return Filing::Not(NotFiled::SelfAlias),
                        Some(_) => Head::Any,
                    },
                    DefKind::Module | DefKind::Trait | DefKind::Macro(_) => Head::Any,
                },
            }
        }
    };
    let Some(trait_) = trait_ else { return Filing::At(BucketKey::inherent(head)) };
    let (text, _) = path_and_bare(trait_);
    let Some((found, len)) = walked(heads, krate, module, text, trait_root, alloc) else { return Filing::Not(NotFiled::TraitNotPath) };
    match found {
        Ok(r) if r.def.kind == DefKind::Trait && r.consumed == len => {
            Filing::At(if negative { BucketKey::negative(r.def.key, head) } else { BucketKey::of_trait(r.def.key, head) })
        }
        Ok(_) => Filing::Not(NotFiled::TraitNotTrait),
        Err(PathError { why: Why::NotRead { .. }, .. }) => Filing::Not(NotFiled::TraitNotRead),
        Err(_) => Filing::Not(NotFiled::TraitNotFound),
    }
}

/// Appends the two tables for `filings` (impl row `i`'s filing at index `i`, the impl table's
/// order) to `out`, a record whose header is written, and sets their counts and offsets.
pub(super) fn append(out: &mut RecordBytes<'_>, filings: &[Filing], alloc: Alloc<'_>) -> Result<(), ItemsRecordError> {
    let mut filed: AVec<'_, (BucketKey, u32)> = AVec::new_in(alloc);
    let mut not: AVec<'_, (u32, NotFiled)> = AVec::new_in(alloc);
    for (row, filing) in filings.iter().enumerate() {
        let row = u32_of(row, "a crate's impls")?;
        match *filing {
            Filing::At(key) => filed.push((key, row)),
            Filing::Not(why) => not.push((row, why)),
        }
    }
    // By key, then by impl row: the impl table is sorted, so the order is the record's own.
    filed.sort_unstable_by(|a, b| (a.0.bytes, a.1).cmp(&(b.0.bytes, b.1)));
    fn set(out: &mut RecordBytes<'_>, field: usize, value: u32) {
        let at = 8 + 4 * field;
        out[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    pad(out);
    let at = u32_of(out.len(), "the bucket table")?;
    for (key, row) in filed.iter() {
        out.extend_from_slice(&key.bytes);
        put32(out, *row);
        out.extend_from_slice(&[0u8; 8]);
    }
    set(out, F_BUCKETS_COUNT, u32_of(filed.len(), "the bucket table's rows")?);
    set(out, F_BUCKETS_AT, at);
    pad(out);
    let at = u32_of(out.len(), "the unfiled list")?;
    for (row, why) in not.iter() {
        put32(out, *row);
        out.extend_from_slice(&[*why as u8, 0, 0, 0]);
    }
    set(out, F_UNFILED_COUNT, u32_of(not.len(), "the unfiled list's rows")?);
    set(out, F_UNFILED_AT, at);
    Ok(())
}

/// One impl a bucket holds, read from its impl row in place (no string of the row is read).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BucketImpl {
    pub key: ItemKey,
    /// Its row in the impl table ([`ItemsRecord::impl_at`]).
    pub row: usize,
    /// A trait impl whose trait is written with no generic arguments.
    pub bare: bool,
    /// A trait impl whose self type is one of its own parameters (`impl<T> Tr for T`).
    pub blanket: bool,
    /// Its self type is written as a path ([`SelfHead::Path`]): under [`Head::Any`], a path the
    /// writer resolved to no constructor (not found, a prefix only, a trait, a module), which
    /// the coherence rules still bound to the impl's crate's local types (the reader's
    /// `Buckets` files it so).
    pub self_path: bool,
}

impl<'a> ItemsRecord<'a> {
    /// A table of this section: its offset and row count, checked to lie in the record.
    fn bucket_table(&self, what: &'static str, at: usize, count: usize, width: usize) -> Result<(usize, usize), ItemsRecordError> {
        let (base, count) = (self.field(at) as usize, self.field(count) as usize);
        if count.checked_mul(width).and_then(|len| base.checked_add(len)).is_none_or(|end| end > self.bytes.len()) {
            return Err(ItemsRecordError::Short { what, at: base });
        }
        Ok((base, count))
    }

    /// How many impls the writer filed under a bucket.
    pub fn bucket_rows(&self) -> usize {
        self.field(F_BUCKETS_COUNT) as usize
    }

    /// The rows of the bucket table `key` names (a whole key: one bucket; a prefix: every
    /// bucket under it), as a range of [`ItemsRecord::bucket_impl`]'s indices: two binary
    /// searches over the record's bytes; no row outside the range is read.
    pub fn bucket(&self, key: &BucketKey) -> Result<core::ops::Range<usize>, ItemsRecordError> {
        let (base, count) = self.bucket_table("the bucket table", F_BUCKETS_AT, F_BUCKETS_COUNT, BUCKET_ROW)?;
        let want = key.bytes();
        // The first row at or after `want` (`after`: the first row after every row under it).
        let bound = |after: bool| {
            let (mut lo, mut hi) = (0usize, count);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let r = base + mid * BUCKET_ROW;
                let row = &self.bytes[r..r + want.len()];
                if row < want || (after && row == want) {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            lo
        };
        Ok(bound(false)..bound(true))
    }

    /// Row `at` of the bucket table: the impl it files, read from the impl's row in place.
    pub fn bucket_impl(&self, at: usize) -> Result<BucketImpl, ItemsRecordError> {
        let (base, count) = self.bucket_table("the bucket table", F_BUCKETS_AT, F_BUCKETS_COUNT, BUCKET_ROW)?;
        if at >= count {
            return Err(ItemsRecordError::Short { what: "a bucket row", at });
        }
        self.filed_impl("a bucket row's impl row", le32(self.bytes, base + at * BUCKET_ROW + KEY) as usize)
    }

    fn filed_impl(&self, what: &'static str, row: usize) -> Result<BucketImpl, ItemsRecordError> {
        if row >= self.counts[T_IMPLS] {
            return Err(ItemsRecordError::Short { what, at: row });
        }
        let r = self.row(T_IMPLS, row);
        let (head, marks) = (self.bytes[r + 56], self.bytes[r + 57]);
        // The head byte is `SelfHead`'s (items_record.rs `SelfHead::byte`): 0 a path, 1 a parameter.
        Ok(BucketImpl { key: key_at(self.bytes, r), row, bare: marks & BARE != 0, blanket: marks & HAS_TRAIT != 0 && head == 1, self_path: head == 0 })
    }

    /// The digest of what `key` answers: the key asked, and each row under it (the rest of its
    /// key and its impl's KEY, never its row number). An impl added to or taken from another
    /// bucket of the crate leaves it equal: what a body's read list holds for (crate, bucket).
    pub fn bucket_digest(&self, key: &BucketKey, alloc: Alloc<'_>) -> Result<ContentId, ItemsRecordError> {
        let (base, _) = self.bucket_table("the bucket table", F_BUCKETS_AT, F_BUCKETS_COUNT, BUCKET_ROW)?;
        let want = key.bytes();
        let mut bytes: AVec<'_, u8> = AVec::new_in(alloc);
        bytes.extend_from_slice(want);
        for at in self.bucket(key)? {
            let r = base + at * BUCKET_ROW;
            bytes.extend_from_slice(&self.bytes[r + want.len()..r + KEY]);
            bytes.extend_from_slice(&self.bucket_impl(at)?.key.0.0);
        }
        Ok(ContentId::of(BUCKET_DOMAIN, &bytes))
    }

    /// How many impls the writer filed under no bucket.
    pub fn unfiled(&self) -> usize {
        self.field(F_UNFILED_COUNT) as usize
    }

    /// Row `i` of the unfiled list: the impl and why the writer did not file it.
    pub fn unfiled_at(&self, i: usize) -> Result<Option<(BucketImpl, NotFiled)>, ItemsRecordError> {
        let (base, count) = self.bucket_table("the unfiled list", F_UNFILED_AT, F_UNFILED_COUNT, UNFILED_ROW)?;
        if i >= count {
            return Ok(None);
        }
        let r = base + i * UNFILED_ROW;
        let why = NotFiled::from_byte(self.bytes[r + 4]).ok_or(ItemsRecordError::Byte { what: "why an impl is unfiled", byte: self.bytes[r + 4] })?;
        Ok(Some((self.filed_impl("an unfiled row's impl row", le32(self.bytes, r) as usize)?, why)))
    }

    /// The digest of the unfiled list (each impl's key and why): part of every question the
    /// crate is asked, so a body's read list holds it beside each bucket of the crate.
    pub fn unfiled_digest(&self, alloc: Alloc<'_>) -> Result<ContentId, ItemsRecordError> {
        let mut bytes: AVec<'_, u8> = AVec::new_in(alloc);
        let mut i = 0;
        while let Some((it, why)) = self.unfiled_at(i)? {
            bytes.extend_from_slice(&it.key.0.0);
            bytes.push(why as u8);
            i += 1;
        }
        Ok(ContentId::of(UNFILED_DOMAIN, &bytes))
    }
}
