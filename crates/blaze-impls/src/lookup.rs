//! The lookups across a build's crates: [`candidates`], the impls of a trait that could apply
//! to a self shape, and [`inherent_candidates`], a shape's inherent impls.
//!
//! **Which crates are asked.** An impl names its trait and its self type, so it is written in
//! a crate that loads the crate of each: the holders of an impl of `T` for a shape of
//! definition `S` are `closure(from) ∧ dependents(crate of T) ∧ dependents(crate of S)`, one
//! AND of three rows of the crate graph, and with no definition in the shape (a primitive, a
//! reference) the last row drops out. A blanket impl names no `S`, so the blanket bucket is
//! asked of `closure(from) ∧ dependents(crate of T)`, and the shape's bucket of those also in
//! `dependents(crate of S)`. This is the rule the old layer's
//! `CratesThatName::holders` reads. The orphan rule does not narrow it to the crates of `T`
//! and `S`: an impl whose trait has a local type among its parameters is written anywhere
//! that type's crate is (`impl From<Local> for String`, serde_json's `impl PartialEq<Value>
//! for f64`), and a fundamental wrapper (`&Local`, `Box<Local>`) has no definition in its
//! shape at all. What the rows leave, `NoImplsOf` narrows: a holder with no impl of `T`, or no
//! bucket of the key, is passed over on its `NoImplsOf` alone.
//!
//! An inherent impl is written in its type's crate, so a shape of definition `S` asks `S`'s
//! crate alone, and, when the type is `#[rustc_has_incoherent_inherent_impls]`
//! ([`Incoherent::Allowed`], the caller reads it from the type's attributes), the crates of
//! the language too. A shape with no definition (a primitive) asks the crates of the
//! language and the asking crate, as the old layer's `with_incoherent_inherent_impls` and
//! `for_self_ty_in_crate_and_block` do.
//!
//! **In what order.** Crate by crate in index order (each crate after what it depends on);
//! in a crate, the rows of the shape's bucket, then the trait's blanket rows, then the impls
//! whose trait was not read at the shape and at any shape, then the blind impls (an inherent
//! lookup: the shape's, those whose self type was not read, the blind); in a bucket, by impl
//! key. A row says which it is ([`ImplRow::is_blanket`], and its flags: an unread row has no
//! trait, and its caller decides from the impl's header whether it answers).
//!
//! **What was not read is a candidate too** (lane BK, 2026-11-17): an impl whose trait path the
//! writer did not resolve, or whose header it did not read, is in its crate's
//! [`BucketKey::UnreadTrait`] or [`BucketKey::Blind`] bucket, and a crate holding one is never
//! passed over by its `NoImplsOf` trait column. So a lookup's rows are a superset of the
//! impls that apply, as the front end's questions need, never a guess.
//!
//! [`candidates_in`] and [`inherent_candidates_in`] ask the crates a caller names, with no
//! crate graph: the front end narrows by the orphan rules' homes, which it reads itself.
//!
//! **What is read.** Of each holder: its `NoImplsOf`; where that may hold the trait and the
//! bucket, its directory; where that names the bucket, the bucket. Every read is handed to
//! [`ImplRecords::note`] or the observer installed by [`Candidates::recording`], with its
//! answer, the negative ones included.

use core::fmt;

use blaze_format::FormatError;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_int::Int;

use blaze_surface::{
    BucketHash, BucketKey, BucketView, CrateFlags, CrateGraphView, CrateIndex, DirectoryView, ImplRow,
    NoImplsOfView, SelfShape,
};

/// Where a lookup gets a crate's impl records: their bytes, read in place, each at an address
/// aligned to `blaze_format::ALIGN`. An implementation holds what it has fetched or mapped,
/// and is told of every read so the caller's read list holds it.
pub trait ImplRecords {
    /// Why a record could not be had (not built yet, not fetched, unreadable).
    type Error;

    /// The crate's `NoImplsOf` record.
    fn no_impls_of(&self, krate: CrateItemKey) -> Result<&[u8], Self::Error>;

    /// The crate's impl directory.
    fn directory(&self, krate: CrateItemKey) -> Result<&[u8], Self::Error>;

    /// The crate's bucket of content id `bucket`, as its directory names it.
    fn bucket(&self, krate: CrateItemKey, bucket: ContentId) -> Result<&[u8], Self::Error>;

    /// The hash of `key` used by the writer. A reader serving many lookups can retain it
    /// across questions, including questions that visit different crates.
    fn bucket_hash(&self, key: BucketKey) -> BucketHash {
        key.hash()
    }

    /// A read a lookup made, with what it answered.
    fn note(&self, read: ImplRead);
}

/// What was asked of a `NoImplsOf`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Probe {
    /// Whether the crate has an impl of the trait.
    Trait(ItemKey),
    /// Whether an impl with an unknown trait prevents the crate from ruling out any trait.
    UnreadTrait,
    /// Whether the crate has a bucket of the hash.
    Bucket(BucketHash),
}

impl Probe {
    /// The wanted buckets, the trait precheck and its crate-wide unread-impl dependency.
    pub const SLOTS: usize = WANTS + 2;
}

/// One read of a lookup, with its answer: an entry of the asker's read list, valid while the
/// same question gets the same answer.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ImplRead {
    /// `NoImplsOf` of `krate` was asked `probe`; `may_hold` false is an exact none.
    Filter { krate: CrateItemKey, probe: Probe, may_hold: bool },
    /// The directory of `krate` was asked for the bucket of `bucket`; `found` is where it is,
    /// none for a miss.
    Directory { krate: CrateItemKey, bucket: BucketHash, found: Option<ContentId> },
    /// The bucket of content id `bucket` of `krate` was read.
    Bucket { krate: CrateItemKey, bucket: ContentId },
}

/// Which record of a crate's.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RecordKind {
    NoImplsOf,
    Directory,
    Bucket,
}

impl fmt::Display for RecordKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RecordKind::NoImplsOf => "NoImplsOf record",
            RecordKind::Directory => "impl directory",
            RecordKind::Bucket => "impl bucket",
        })
    }
}

/// Whether a type's inherent impls may be written outside its crate: rustc's
/// `#[rustc_has_incoherent_inherent_impls]`, which the caller reads from the type.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Incoherent {
    No,
    /// In the crates of the language as well.
    Allowed,
}

/// Why a lookup stopped.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ImplLookupError<E> {
    /// The record could not be had: the source's own error.
    Records { krate: CrateItemKey, record: RecordKind, error: E },
    /// The record's bytes are not one.
    Format { krate: CrateItemKey, record: RecordKind, error: FormatError },
    /// The record asked for `krate` is another crate's.
    WrongCrate { krate: CrateItemKey, record: RecordKind, found: CrateItemKey },
    /// The bucket the directory named for `wanted` holds another key.
    WrongBucket { krate: CrateItemKey, bucket: ContentId, wanted: BucketKey, found: BucketKey },
    /// A key (the trait's, or the shape's definition's) of a crate not in the graph.
    UnknownCrate { item: ItemKey },
}

impl<E: fmt::Display> fmt::Display for ImplLookupError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImplLookupError::Records { krate, record, error } => {
                write!(f, "the {record} of the crate {krate:x} could not be read: {error}")
            }
            ImplLookupError::Format { krate, record, error } => {
                write!(f, "the {record} of the crate {krate:x} is not one: {error}")
            }
            ImplLookupError::WrongCrate { krate, record, found } => {
                write!(f, "the {record} read for the crate {krate:x} is of the crate {found:x}")
            }
            ImplLookupError::WrongBucket { krate, bucket, wanted, found } => write!(
                f,
                "the impl bucket {bucket:x} of the crate {krate:x}, named for {wanted:?}, holds {found:?}"
            ),
            ImplLookupError::UnknownCrate { item } => write!(
                f,
                "the item {item:x} is of a crate not in the crate graph (its key prefix names none)"
            ),
        }
    }
}

impl<E: fmt::Display + fmt::Debug> core::error::Error for ImplLookupError<E> {}

/// A bucket a lookup wants of each crate it asks.
#[derive(Clone, Copy, Debug)]
struct Want {
    key: BucketKey,
    /// Filled when the first holder reaches this bucket after its trait precheck.
    hash: Option<BucketHash>,
    /// Asked only of the crates that can name the goal's shape's definition (its rows are at
    /// that shape).
    shaped: bool,
}

impl Want {
    fn of(key: BucketKey, shaped: bool) -> Want {
        Want { key, hash: None, shaped }
    }
}

/// The most buckets one lookup wants of a crate: a trait lookup's shape, blanket, unread at
/// the shape, unread at any shape, and blind buckets.
const WANTS: usize = 5;

/// The crates a lookup asks, a word of a bit row at a time.
#[derive(Clone, Copy, Debug)]
enum Reach<'g> {
    /// The AND of the asking crate's closure and the trait's crate's dependents.
    Rows { closure: &'g [u64], trait_row: &'g [u64] },
    /// Of the asking crate's closure: the definition's crate, the asking crate, and with
    /// `lang` the crates of the language.
    Inherent { closure: &'g [u64], def: Option<CrateIndex>, from: Option<CrateIndex>, lang: bool },
    /// The crates the caller names, in its order, with no crate graph: a caller that narrows
    /// the crates by rules of its own (the front end's orphan-rule homes).
    Listed(&'g [CrateItemKey]),
}

impl<'g> Reach<'g> {
    fn words(&self) -> usize {
        match self {
            Reach::Rows { closure, .. } | Reach::Inherent { closure, .. } => closure.len(),
            Reach::Listed(_) => 0,
        }
    }

    fn word(&self, graph: &CrateGraphView<'g>, w: usize) -> u64 {
        match *self {
            Reach::Listed(_) => 0,
            Reach::Rows { closure, trait_row } => closure[w] & trait_row[w],
            Reach::Inherent { closure, def, from, lang } => {
                let (mut bits, mut kept) = (closure[w], 0u64);
                while bits != 0 {
                    let bit = bits.trailing_zeros();
                    bits &= bits - 1;
                    let at = CrateIndex::of_bit(w * 64 + bit.widen_to::<usize>());
                    let asked = Some(at) == def
                        || Some(at) == from
                        || (lang && graph.flags(at).contains(CrateFlags::LANG));
                    if asked {
                        kept |= 1 << bit;
                    }
                }
                kept
            }
        }
    }
}

/// What a lookup holds of the crate it is reading.
struct InCrate<'c> {
    krate: CrateItemKey,
    holder: usize,
    filter: NoImplsOfView<'c>,
    directory: Option<DirectoryView<'c>>,
    /// Whether the crate can name the goal's shape's definition: else its shaped wants are
    /// passed over.
    names_shape: bool,
    /// The next of the lookup's wants to look for.
    next_want: usize,
    /// The bucket being read, and its next row.
    bucket: Option<(BucketView<'c>, usize)>,
}

/// The rows a lookup finds, read lazily: an iterator of rows, or of the one error that ends
/// it. It allocates nothing.
pub struct Candidates<'g, 'c, C: ImplRecords> {
    /// None for a [`Reach::Listed`] lookup, which reads no crate graph.
    graph: Option<CrateGraphView<'g>>,
    cx: &'c C,
    observer: Option<&'c dyn Fn(usize, usize, ImplRead)>,
    reach: Reach<'g>,
    /// Asked of each crate's `NoImplsOf` before its buckets: the trait, for a trait lookup.
    trait_: Option<ItemKey>,
    wants: [Option<Want>; WANTS],
    /// The crates the shaped wants are asked of, beyond the reach: the dependents of the
    /// shape's definition's crate, for a trait lookup of a shape with one. None asks them of
    /// all.
    first_in: Option<&'g [u64]>,
    /// The next word of the reach to load (or, listed, the next crate), and what is left of
    /// the word before.
    word: usize,
    bits: u64,
    current: Option<InCrate<'c>>,
    done: bool,
}

/// What a trait lookup wants of each crate, in this order: the shape's bucket, the trait's
/// blanket bucket, the unread trait impls at the shape and at any shape, and the blind impls.
/// A shape no impl is filed under (a blanket's, a placeholder's) wants no shaped bucket.
fn trait_wants(trait_: ItemKey, shape: SelfShape) -> [Option<Want>; WANTS] {
    let any = |key| Some(Want::of(key, false));
    let (by_shape, unread_at) = match shape.files_no_impl() {
        true => (None, None),
        false => (
            Some(Want::of(BucketKey::Trait { trait_, shape }, true)),
            Some(Want::of(BucketKey::UnreadTrait { shape }, true)),
        ),
    };
    [
        by_shape,
        any(BucketKey::Blanket { trait_ }),
        unread_at,
        any(BucketKey::UnreadTrait { shape: SelfShape::BLANKET }),
        any(BucketKey::Blind),
    ]
}

/// What an inherent lookup wants of each crate, in this order: the shape's bucket, the
/// inherent impls whose self type was not read to a shape, and the blind impls.
fn inherent_wants(shape: SelfShape) -> [Option<Want>; WANTS] {
    let any = |key| Some(Want::of(key, false));
    let by_shape = match shape.files_no_impl() {
        true => None,
        false => any(BucketKey::Inherent { shape }),
    };
    [
        by_shape,
        any(BucketKey::Inherent { shape: SelfShape::BLANKET }),
        any(BucketKey::Blind),
        None,
        None,
    ]
}

/// The crate of `item`, by its key's prefix.
fn crate_of<E>(graph: &CrateGraphView<'_>, item: ItemKey) -> Result<CrateIndex, ImplLookupError<E>> {
    graph.crate_of_prefix(item.crate_prefix()).ok_or(ImplLookupError::UnknownCrate { item })
}

/// The impls of `trait_` that could apply to a self type of shape `shape`, among the crates
/// `from` loads: those for the shape, then the blanket ones, crate by crate in index order.
/// A shape no impl is filed under (a blanket's, a placeholder's) finds the blanket impls
/// alone.
pub fn candidates<'g, 'c, C: ImplRecords>(
    graph: &CrateGraphView<'g>,
    from: CrateIndex,
    trait_: ItemKey,
    shape: SelfShape,
    cx: &'c C,
) -> Result<Candidates<'g, 'c, C>, ImplLookupError<C::Error>> {
    let trait_crate = crate_of(graph, trait_)?;
    let self_row = match shape.def() {
        Some(def) => Some(graph.dependents_row(crate_of(graph, def)?)),
        None => None,
    };
    let reach =
        Reach::Rows { closure: graph.closure_row(from), trait_row: graph.dependents_row(trait_crate) };
    Ok(Candidates::new(Some(*graph), cx, reach, Some(trait_), trait_wants(trait_, shape), self_row))
}

/// [`candidates`] over the crates `crates` names, in its order, with no crate graph: for a
/// caller that chooses the crates by rules of its own. Every crate named is asked every
/// want; a crate's `NoImplsOf` and directory still pass over what it does not hold.
pub fn candidates_in<'g, 'c, C: ImplRecords>(
    crates: &'g [CrateItemKey],
    trait_: ItemKey,
    shape: SelfShape,
    cx: &'c C,
) -> Candidates<'g, 'c, C> {
    Candidates::new(None, cx, Reach::Listed(crates), Some(trait_), trait_wants(trait_, shape), None)
}

/// [`inherent_candidates`] over the crates `crates` names, in its order, with no crate graph.
pub fn inherent_candidates_in<'g, 'c, C: ImplRecords>(
    crates: &'g [CrateItemKey],
    shape: SelfShape,
    cx: &'c C,
) -> Candidates<'g, 'c, C> {
    Candidates::new(None, cx, Reach::Listed(crates), None, inherent_wants(shape), None)
}

/// The inherent impls of shape `shape` among the crates `from` loads, crate by crate in
/// index order. `incoherent` is whether the type is `#[rustc_has_incoherent_inherent_impls]`;
/// it is not read for a shape with no definition, whose impls are the language's.
pub fn inherent_candidates<'g, 'c, C: ImplRecords>(
    graph: &CrateGraphView<'g>,
    from: CrateIndex,
    shape: SelfShape,
    incoherent: Incoherent,
    cx: &'c C,
) -> Result<Candidates<'g, 'c, C>, ImplLookupError<C::Error>> {
    let def = match shape.def() {
        Some(def) => Some(crate_of(graph, def)?),
        None => None,
    };
    let wants = inherent_wants(shape);
    let reach = Reach::Inherent {
        closure: graph.closure_row(from),
        def,
        from: match def {
            Some(_) => None,
            None => Some(from),
        },
        lang: def.is_none() || incoherent == Incoherent::Allowed,
    };
    Ok(Candidates::new(Some(*graph), cx, reach, None, wants, None))
}

impl<'g, 'c, C: ImplRecords> Candidates<'g, 'c, C> {
    fn new(
        graph: Option<CrateGraphView<'g>>,
        cx: &'c C,
        reach: Reach<'g>,
        trait_: Option<ItemKey>,
        wants: [Option<Want>; WANTS],
        first_in: Option<&'g [u64]>,
    ) -> Candidates<'g, 'c, C> {
        let done = wants.iter().all(Option::is_none);
        Candidates { graph, cx, observer: None, reach, trait_, wants, first_in, word: 0, bits: 0, current: None, done }
    }

    /// Visits just the shape-specific buckets. A survey over several numeric widths visits
    /// the shared blanket and blind buckets with its first width, then calls this for each
    /// later width over the same holders. Trait lookups still ask each holder's trait filter.
    pub fn only_at_shape(mut self) -> Self {
        for want in &mut self.wants {
            if want.is_some_and(|w| w.key.shape().is_blanket()) {
                *want = None;
            }
        }
        if let Some(current) = &mut self.current
            && current.bucket.as_ref().is_some_and(|(bucket, _)| bucket.key().shape().is_blanket())
        {
            current.bucket = None;
        }
        self.done |= self.trait_.is_none() && self.wants.iter().all(Option::is_none);
        self
    }

    /// Routes subsequent reads to `observer(holder, slot, read)`. The holder is the crate's
    /// index in the supplied list or in the graph. Slot zero asks whether the crate may
    /// hold the trait. Slots 1 through WANTS name the wanted buckets in order. The final
    /// slot reads the unread-impl flag on which the trait precheck also depends.
    pub fn recording(mut self, observer: &'c dyn Fn(usize, usize, ImplRead)) -> Self {
        self.observer = Some(observer);
        self
    }

    /// The next crate of the reach, in index order (listed: in the caller's order), and
    /// whether it can name the goal's shape's definition.
    fn next_holder(&mut self) -> Option<(usize, CrateItemKey, bool)> {
        if let Reach::Listed(crates) = self.reach {
            let at = self.word;
            let krate = *crates.get(at)?;
            self.word += 1;
            return Some((at, krate, true));
        }
        let graph = self.graph?;
        loop {
            if self.bits != 0 {
                let bit = self.bits.trailing_zeros().widen_to::<usize>();
                self.bits &= self.bits - 1;
                let at = (self.word - 1) * 64 + bit;
                let names_shape = self.first_in.is_none_or(|row| row[at / 64] & (1 << (at % 64)) != 0);
                return Some((at, graph.key(CrateIndex::of_bit(at)), names_shape));
            }
            if self.word >= self.reach.words() {
                return None;
            }
            self.bits = self.reach.word(&graph, self.word);
            self.word += 1;
        }
    }

    /// The next row, none at the end.
    fn step(&mut self) -> Result<Option<ImplRow>, ImplLookupError<C::Error>> {
        let cx = self.cx;
        let observer = self.observer;
        let note = |holder, slot, read| match observer {
            Some(observe) => observe(holder, slot, read),
            None => cx.note(read),
        };
        loop {
            if let Some(current) = &mut self.current {
                let krate = current.krate;
                if let Some((bucket, at)) = &mut current.bucket {
                    if let Some(row) = bucket.row(*at) {
                        *at += 1;
                        return row.map(Some).map_err(|error| ImplLookupError::Format {
                            krate,
                            record: RecordKind::Bucket,
                            error,
                        });
                    }
                    current.bucket = None;
                }
                if let Some(want) = self.wants.get_mut(current.next_want) {
                    current.next_want += 1;
                    // A crate that cannot name the shape's definition holds none of the
                    // shape's impls, only those for any shape.
                    if let Some(want) = want.as_mut().filter(|want| current.names_shape || !want.shaped) {
                        let slot = current.next_want;
                        let opened = open_want(cx, current, want, slot, &note)?;
                        current.bucket = opened;
                    }
                    continue;
                }
                self.current = None;
                continue;
            }
            let Some((holder, krate, names_shape)) = self.next_holder() else { return Ok(None) };
            let bytes = cx.no_impls_of(krate).map_err(|error| ImplLookupError::Records {
                krate,
                record: RecordKind::NoImplsOf,
                error,
            })?;
            let filter = NoImplsOfView::read(bytes).map_err(|error| ImplLookupError::Format {
                krate,
                record: RecordKind::NoImplsOf,
                error,
            })?;
            if filter.krate() != krate {
                return Err(ImplLookupError::WrongCrate {
                    krate,
                    record: RecordKind::NoImplsOf,
                    found: filter.krate(),
                });
            }
            if let Some(trait_) = self.trait_ {
                let may_hold = filter.may_hold_trait(trait_);
                note(holder, 0, ImplRead::Filter { krate, probe: Probe::Trait(trait_), may_hold });
                note(holder, WANTS + 1, ImplRead::Filter { krate, probe: Probe::UnreadTrait, may_hold: filter.has_unread() });
                if !may_hold {
                    continue;
                }
            }
            self.current = Some(InCrate { krate, holder, filter, directory: None, names_shape, next_want: 0, bucket: None });
        }
    }
}

/// The bucket of `want` in the crate `current` reads, through its `NoImplsOf` and its
/// directory, each read noted; none when either says the crate has none.
fn open_want<'c, C: ImplRecords>(
    cx: &'c C,
    current: &mut InCrate<'c>,
    want: &mut Want,
    slot: usize,
    note: &impl Fn(usize, usize, ImplRead),
) -> Result<Option<(BucketView<'c>, usize)>, ImplLookupError<C::Error>> {
    let krate = current.krate;
    let hash = *want.hash.get_or_insert_with(|| cx.bucket_hash(want.key));
    let may_hold = current.filter.may_hold_bucket(hash);
    note(current.holder, slot, ImplRead::Filter { krate, probe: Probe::Bucket(hash), may_hold });
    if !may_hold {
        return Ok(None);
    }
    let directory = match current.directory {
        Some(directory) => directory,
        None => {
            let bytes = cx.directory(krate).map_err(|error| ImplLookupError::Records {
                krate,
                record: RecordKind::Directory,
                error,
            })?;
            let directory = DirectoryView::read(bytes).map_err(|error| ImplLookupError::Format {
                krate,
                record: RecordKind::Directory,
                error,
            })?;
            if directory.krate() != krate {
                return Err(ImplLookupError::WrongCrate {
                    krate,
                    record: RecordKind::Directory,
                    found: directory.krate(),
                });
            }
            current.directory = Some(directory);
            directory
        }
    };
    let hit = directory.lookup(hash);
    note(current.holder, slot, ImplRead::Directory { krate, bucket: hash, found: hit.map(|hit| hit.bucket) });
    let Some(hit) = hit else { return Ok(None) };
    let bytes = cx.bucket(krate, hit.bucket).map_err(|error| ImplLookupError::Records {
        krate,
        record: RecordKind::Bucket,
        error,
    })?;
    note(current.holder, slot, ImplRead::Bucket { krate, bucket: hit.bucket });
    let bucket = BucketView::read(bytes).map_err(|error| ImplLookupError::Format {
        krate,
        record: RecordKind::Bucket,
        error,
    })?;
    if bucket.key() != want.key {
        return Err(ImplLookupError::WrongBucket {
            krate,
            bucket: hit.bucket,
            wanted: want.key,
            found: bucket.key(),
        });
    }
    Ok(Some((bucket, 0)))
}

impl<C: ImplRecords> Iterator for Candidates<'_, '_, C> {
    type Item = Result<ImplRow, ImplLookupError<C::Error>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.step() {
            Ok(Some(row)) => Some(Ok(row)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(error) => {
                self.done = true;
                Some(Err(error))
            }
        }
    }
}
