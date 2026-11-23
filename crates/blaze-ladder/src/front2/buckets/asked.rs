//! Impl questions answered BY BUCKET over records (drive.md, "End state: the whole pipeline on
//! the command", piece 12; the bullet "An impl bucket is a fact, not a task").
//!
//! [`Buckets::admit`] files EVERY impl of each crate a question's home admits: one question
//! costs in proportion to those crates (measured 2026-11-16, front2_records, 70 crates: `bool:
//! Clone` alone filed 28,719 impls and opened 7,222 source texts). Over a source whose items
//! records hold a bucket index (`NamesSource::has_buckets`), [`Buckets::admit_asked`] files
//! instead, for the ONE question asked and each crate its home admits:
//!
//! - the buckets the question can be answered from, each one binary search of the crate's
//!   record ([`NamesSource::bucket`]): for `T0: Trait`, (Trait, the head of `T0`), (Trait, any
//!   head: blanket impls and self types with no constructor) and the negative impls of Trait;
//!   for an inherent question, (inherent, the head) and (inherent, any head). No other impl's
//!   row is read and no text is opened: a filed impl's text is read when the check lowers its
//!   header as a candidate, and its members' when the check uses one;
//! - once per crate, the crate's UNFILED list ([`NamesSource::unfiled`]: the impls its
//!   record's writer could not resolve, each with why): filed here as [`Buckets::admit`] files
//!   a row (`file_row`, else the header's text), so none is dropped; counted by reason
//!   ([`Buckets::not_filed`]).
//!
//! WHICH CRATES are asked is the question's home (`trait_home`, `inherent_home`: the orphan
//! rules); for `Home::Incoherent` the crates are ONE fact of the command
//! ([`NamesSource::incoherent_crates`]), never found by opening every crate's record.
//!
//! WHAT A QUESTION READ ([`BucketRead`]) is kept per question: each (crate, bucket) asked with
//! the bucket's digest, each asked crate's unfiled list with its digest, and the incoherent
//! fact where it was used. It is a function of the question and the records alone (a bucket
//! filed for an earlier question is not read again, its digest is kept), so a body's read list
//! takes the rows of each impl question it asked whatever body asked it first.

use blaze_id::ContentId;
use blaze_scope::items_record::buckets::{BucketImpl, BucketKey, Head, NOT_FILED, NotFiled};

use super::*;

/// The domain of a bucket key's 64-bit name in a [`BucketRead`].
const KEY_DOMAIN: &str = "blaze-ladder front2 bucket key 1";
/// [`BucketRead::bucket`] of a crate's unfiled list.
pub const UNFILED_LIST: u64 = 1;
/// [`BucketRead::bucket`] of the incoherent-crates fact (its `krate` is 0).
pub const INCOHERENT_FACT: u64 = 2;

/// One impl question.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Ask {
    /// `ImplsOf(trait, self shape)`.
    Trait(ItemKey, Shape),
    /// `Inherent(self shape, _)`: every name at once.
    Inherent(Shape),
}

/// One fact an impl question read: which crate (the first 16 bytes of its key, big-endian;
/// 0 for a fact of the whole graph), which bucket of it (the first 8 bytes of the digest of
/// the bucket's key; [`UNFILED_LIST`]; [`INCOHERENT_FACT`]) and the first 8 bytes of the
/// answer's digest. Ordered (crate, bucket, answer): a sorted list is a function of its rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BucketRead {
    pub krate: u128,
    pub bucket: u64,
    pub answer: u64,
}

/// What one answered trait question read over buckets or impl tables.
pub(super) struct TraitReads {
    /// Question-specific reads (all reads for `admit_asked` and non-`Home::Any` `admit_tables`;
    /// shape-specific reads only when `uses_any_filter` is true).
    pub(super) own: Vec<BucketRead, Global>,
    /// When true, this question also read the per-body `TablesCache` (`all_unread_reads` and
    /// `trait_any_filter[&trait_]`).
    pub(super) uses_any_filter: bool,
}

/// What [`Buckets::admit_asked`] keeps: which buckets are filed, and what each question read.
pub(super) struct Indexed {
    /// Each (crate, bucket) filed, with its digest.
    filed: HashMap<(usize, BucketKey), u64, Global>,
    /// Each crate whose unfiled list is filed, with the list's digest.
    unfiled: HashMap<usize, u64, Global>,
    /// The incoherent fact's digest, once `BucketIndex::crate_state` holds it.
    pub(super) incoherent: Option<u64>,
    pub(super) trait_reads: HashMap<(ItemKey, Shape), TraitReads, Global>,
    pub(super) inherent_reads: HashMap<Shape, Vec<BucketRead, Global>, Global>,
    /// How many impls were filed from a bucket: no impl row decoded, no text opened.
    pub(super) from_index: u32,
    /// How many (crate, bucket) were looked up in a crate's record ([`Buckets::bucket_lookups`]).
    pub(super) lookups: u32,
    /// How many questions were asked by bucket ([`Buckets::bucket_asks`]).
    pub(super) asks: u32,
    /// How many unfiled impls were met, by [`NotFiled`] as `usize`.
    not_filed: [u32; NOT_FILED.len()],
}

impl Indexed {
    pub(super) fn new() -> Indexed {
        Indexed {
            filed: HashMap::new_in(Global),
            unfiled: HashMap::new_in(Global),
            incoherent: None,
            trait_reads: HashMap::new_in(Global),
            inherent_reads: HashMap::new_in(Global),
            from_index: 0,
            lookups: 0,
            asks: 0,
            not_filed: [0; NOT_FILED.len()],
        }
    }
}

/// How an impl found in a bucket is filed ([`Buckets::put`]).
#[derive(Clone, Copy)]
enum Under {
    Trait(Option<Shape>),
    Negative,
    Inherent(Option<Shape>),
}

fn first8(id: ContentId) -> u64 {
    let [a, b, c, d, e, f, g, h, ..] = id.0;
    u64::from_be_bytes([a, b, c, d, e, f, g, h])
}

fn first16(id: ContentId) -> u128 {
    let mut first = [0u8; 16];
    first.copy_from_slice(&id.0[..16]);
    u128::from_be_bytes(first)
}

/// The heads the index files an impl at `shape` under, each with the shape such an impl has
/// (`AnyInt` is every integer type's head). Empty for a shape the index names no head for (a
/// fn item, a fn pointer, an opaque type, a closure, a trait object, a pattern type: an impl
/// on one is unfiled or under "any"). Refused: a nominal type whose library key the key map
/// and the records do not give.
fn heads_of<'n, S: NamesSource<'n>>(names: S, keys: &Keys, shape: Shape) -> Result<Vec<(Head, Shape), Global>, ImplQuestionError> {
    let mut out = Vec::new_in(Global);
    match shape {
        Shape::Bool | Shape::Char | Shape::Str | Shape::Int(_) | Shape::Uint(_) | Shape::Float(_) | Shape::AnyInt | Shape::AnyFloat => {
            for (name, _) in PRIMS.iter() {
                let Some(of) = prim_shape(name) else { continue };
                if fits(Some(of), shape) {
                    if let Some(def) = blaze_scope::builtin_type(name) {
                        out.push((Head::Named(def.key), of));
                    }
                }
            }
        }
        Shape::Never => out.push((Head::Never, shape)),
        Shape::Tuple(n) => {
            if let Ok(n) = u16::try_from(n) {
                out.push((Head::Tuple(n), shape));
            }
        }
        Shape::Ref(Mutbl::Not) => out.push((Head::Ref, shape)),
        Shape::Ref(Mutbl::Mut) => out.push((Head::RefMut, shape)),
        Shape::Ptr(Mutbl::Not) => out.push((Head::PtrConst, shape)),
        Shape::Ptr(Mutbl::Mut) => out.push((Head::PtrMut, shape)),
        Shape::Array => out.push((Head::Array, shape)),
        Shape::Slice => out.push((Head::Slice, shape)),
        Shape::Adt(k) => {
            let lib = keys.library_in(names, k).ok_or(ImplQuestionError::NoLibraryKey { of: "the nominal type an impl question is about", key: k })?;
            out.push((Head::Named(lib), shape));
        }
        _ => {}
    }
    Ok(out)
}

impl BucketIndex {
    /// Files what the question `ask` at `home` can be answered from, by bucket (module docs);
    /// nothing over the solved names, over a source with no bucket index, or with the rows
    /// planted off ([`Buckets::admit`] then files whole crates). Call it after
    /// [`Buckets::admit`] and before the question's candidates are read. Returns how many
    /// impls this call filed; refused by name where a bucket the question reads cannot be
    /// asked (no library key for its trait or self type, a crate with no key), never an empty
    /// filing that would answer "no impl" (D193 point 5).
    pub fn admit_asked<'n, S: NamesSource<'n>>(&mut self, names: S, keys: &Keys, home: Homes, ask: Ask) -> Result<usize, AdmitError> {
        if !self.is_lazy() || self.no_rows || !names.has_buckets() {
            return Ok(0);
        }
        // Every field: a new kind of home is a compile error here, not a crate left unasked.
        // `local` judges filed impls, never which crates are asked.
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
                        let answer = first8(digest);
                        self.indexed.incoherent = Some(answer);
                        reads.push(BucketRead { krate: 0, bucket: INCOHERENT_FACT, answer });
                    }
                    // Not told: every crate stays admitted (the wide side), and the gap voids
                    // the verdict by name.
                    None => names.gap("which crates may write an incoherent inherent impl (the records were named no lang crates: `Records::name_lang_crates`)", None),
                }
            }
        }
        // The buckets asked of each crate, how an impl found in each is filed, and whether the
        // bucket is the "any" head's (where a self path the writer read to no constructor is:
        // `PathSelf`).
        let mut wanted: Vec<(BucketKey, Under, bool), Global> = Vec::new_in(Global);
        // The asked trait's crate (`PathSelf::Trait`).
        let mut trait_krate = None;
        let (trait_, shape) = match ask {
            Ask::Trait(t, shape) => (Some(t), shape),
            Ask::Inherent(shape) => (None, shape),
        };
        let heads = heads_of(names, keys, shape)?;
        // A builtin the checker has no shape for is filed with no constructor (`file_row`).
        let shapeless = blaze_scope::BUILTIN_TYPES.iter().filter(|name| prim_shape(name).is_none()).filter_map(|name| blaze_scope::builtin_type(name)).map(|def| (Head::Named(def.key), None));
        let all_heads = heads.iter().map(|(head, of)| (*head, Some(*of))).chain(core::iter::once((Head::Any, None))).chain(shapeless);
        match trait_ {
            Some(t) => {
                let lib = keys.library_in(names, t).ok_or(ImplQuestionError::NoLibraryKey { of: "the trait an impl question is about", key: t })?;
                trait_krate = names.crate_of(lib);
                wanted.extend(all_heads.map(|(head, of)| (BucketKey::of_trait(lib, head), Under::Trait(of), head == Head::Any)));
                wanted.push((BucketKey::negatives(lib), Under::Negative, false));
            }
            None => wanted.extend(all_heads.map(|(head, of)| (BucketKey::inherent(head), Under::Inherent(of), head == Head::Any))),
        }
        let mut filed = 0usize;
        for krate in 0..self.lazy_filed.len() {
            // A crate filed whole (`Buckets::admit`, before this was called) holds every impl.
            if self.lazy_filed[krate] || !(self.admits(all, krate) || self.admits(bare, krate) || self.admits(blanket, krate)) {
                continue;
            }
            // A proc-macro unit the program reaches answers none of its questions; the
            // program's own crate (0) does, a proc macro's too (linked.rs `links_unit`).
            if !self.crate_linked(names, krate) {
                continue;
            }
            // Never skipped without a word: an admitted crate not asked leaves its impls out.
            let crate_key = names.crate_key(krate).ok_or(TablesReadError::NoCrateKey { krate })?;
            let krate_id = first16(crate_key.0);
            // The crate's unfiled list, the first time the crate is asked anything.
            let unfiled = match self.indexed.unfiled.get(&krate).copied() {
                Some(digest) => digest,
                None => {
                    let mut rows: Vec<(ImplRow<'n>, NotFiled), Global> = Vec::new_in(Global);
                    // No record, or one that did not read: the source filed why.
                    let Some(digest) = names.unfiled(krate, &mut |row, why| rows.push((row, why))) else { continue };
                    filed += rows.len();
                    self.file_unfiled(names, krate, &rows);
                    self.indexed.unfiled.insert(krate, first8(digest));
                    first8(digest)
                }
            };
            reads.push(BucketRead { krate: krate_id, bucket: UNFILED_LIST, answer: unfiled });
            for (key, under, any_head) in wanted.iter() {
                let digest = match self.indexed.filed.get(&(krate, *key)).copied() {
                    Some(digest) => digest,
                    None => {
                        let mut got: Vec<BucketImpl, Global> = Vec::new_in(Global);
                        self.indexed.lookups += 1;
                        let Some(digest) = names.bucket(krate, key, &mut |it| got.push(it)) else { continue };
                        for it in got.iter() {
                            self.krates.insert(it.key, krate);
                            if it.blanket {
                                self.blanket.insert(it.key);
                            }
                            let (how, path_self) = match (*under, trait_) {
                                (Under::Trait(of), Some(t)) => (Filed::Trait(t, of, it.bare), Some(PathSelf::Trait { of: trait_krate, bare: it.bare })),
                                (Under::Negative, Some(t)) => (Filed::Negative(t, None), None),
                                (Under::Inherent(of), _) | (Under::Trait(of), None) => (Filed::Inherent(of), Some(PathSelf::Inherent)),
                                (Under::Negative, None) => continue,
                            };
                            self.put(it.key, how, false);
                            // A self path under the "any" head that is no parameter: the writer
                            // read it to no constructor, as `file_row` reads `SelfAt::Unread`.
                            let read_to_nothing = *any_head && it.self_path && !it.blanket;
                            self.bound(names, it.key, krate, path_self.filter(|_| read_to_nothing));
                            self.indexed.from_index += 1;
                        }
                        filed += got.len();
                        self.indexed.filed.insert((krate, *key), first8(digest));
                        first8(digest)
                    }
                };
                reads.push(BucketRead { krate: krate_id, bucket: first8(ContentId::of(KEY_DOMAIN, key.bytes())), answer: digest });
            }
        }
        // Nothing filed is sorted here: each set above is a hash set (`Buckets`'s docs). The
        // one sort is of this question's own read list, whose length is the question's
        // (crates asked x buckets), never the job's.
        self.indexed.asks += 1;
        reads.sort_unstable();
        reads.dedup();
        match ask {
            Ask::Trait(t, shape) => {
                self.indexed.trait_reads.insert((t, shape), TraitReads { own: reads, uses_any_filter: false });
            }
            Ask::Inherent(shape) => {
                self.indexed.inherent_reads.insert(shape, reads);
            }
        }
        Ok(filed)
    }

    /// Files the impls of crate `krate` its record's writer filed under no bucket, as
    /// [`Buckets::admit`] files a crate's rows: by the row (`file_row`: the reader's names
    /// resolve what the writer's did not), else by the header's text.
    fn file_unfiled<'n, S: NamesSource<'n>>(&mut self, names: S, krate: usize, rows: &[(ImplRow<'n>, NotFiled)]) {
        let mut plain: Vec<ImplRow<'n>, Global> = Vec::with_capacity_in(rows.len(), Global);
        for (row, why) in rows.iter() {
            self.indexed.not_filed[*why as usize] += 1;
            plain.push(*row);
        }
        let counts = file_rows(names, krate, &plain, &mut |f| self.take(names, krate, f));
        self.count(counts);
    }

    /// Visits the parts of an answered trait question's reads without allocating: its own reads
    /// and, when answered via the per-body `Home::Any` filter cache, the shared `UnreadTrait`
    /// and per-trait `NoImplsOf` read slices.
    pub(super) fn with_trait_reads<R>(&self, trait_: ItemKey, shape: Shape, f: impl FnOnce(&[BucketRead], Option<(&[BucketRead], &[BucketRead])>) -> R) -> R {
        let Some(entry) = self.indexed.trait_reads.get(&(trait_, shape)) else {
            return f(&[], None);
        };
        if !entry.uses_any_filter {
            return f(&entry.own, None);
        }
        let shared = self.tables_cache.as_ref().map(|cache| (cache.all_unread_reads(), cache.trait_reads_for(trait_)));
        f(&entry.own, shared)
    }

    /// What the answered trait question read, sorted: each (crate, bucket) with its digest
    /// (module docs). Empty before the question is asked, and where no bucket index is read.
    pub fn trait_reads(&self, trait_: ItemKey, shape: Shape) -> blaze_own::Vec<BucketRead, Global> {
        let mut out: blaze_own::Vec<BucketRead, Global> = blaze_own::Vec::new_in(Global);
        self.with_trait_reads(trait_, shape, |own, shared| {
            let extra = shared.map_or(0, |(u, t)| u.len() + t.len());
            out.reserve(own.len() + extra);
            out.extend_from_slice(own);
            if let Some((unread, trait_filter)) = shared {
                out.extend_from_slice(unread);
                out.extend_from_slice(trait_filter);
                out.sort_unstable();
                out.dedup();
            }
        });
        out
    }

    /// What the answered inherent question at `shape` read, sorted.
    pub fn inherent_reads(&self, shape: Shape) -> &[BucketRead] {
        self.indexed.inherent_reads.get(&shape).map_or(&[], |v| v.as_slice())
    }

    /// Every question answered by bucket, sorted: what a real check asked, to be asked again
    /// of another source (tests/front2_tables_core.rs).
    pub fn asked(&self) -> Vec<Ask, Global> {
        let mut out: Vec<Ask, Global> = Vec::with_capacity_in(self.indexed.trait_reads.len() + self.indexed.inherent_reads.len(), Global);
        out.extend(self.indexed.trait_reads.keys().map(|(t, s)| Ask::Trait(*t, *s)));
        out.extend(self.indexed.inherent_reads.keys().map(|s| Ask::Inherent(*s)));
        out.sort_unstable();
        out
    }

    /// How many impls were filed from a bucket of an items record: no impl row decoded to file
    /// them, no text opened.
    pub fn filed_from_index(&self) -> u32 {
        self.indexed.from_index
    }

    /// How many (crate, bucket) pairs were looked up in their crate's items record: each at
    /// most once per [`Buckets`], whatever questions read it, so N questions over one bucket
    /// look it up once.
    pub fn bucket_lookups(&self) -> u32 {
        self.indexed.lookups
    }

    /// How many questions [`Buckets::admit_asked`] answered by bucket.
    pub fn bucket_asks(&self) -> u32 {
        self.indexed.asks
    }

    /// How many crates were asked by bucket (each had its unfiled list filed).
    pub fn crates_asked(&self) -> usize {
        self.indexed.unfiled.len()
    }

    /// The crates asked by bucket, by index, sorted (what [`Self::crates_asked`] counts).
    pub fn crates_asked_sorted(&self) -> blaze_own::Vec<usize, Global> {
        let mut out = blaze_own::Vec::new_in(Global);
        out.extend(self.indexed.unfiled.keys().copied());
        out.sort_unstable();
        out
    }

    /// The unfiled impls met in the crates asked, by why their record's writer did not file
    /// them: (class, count), in [`NotFiled`]'s order.
    pub fn not_filed(&self) -> [(&'static str, u32); NOT_FILED.len()] {
        let mut out = [("", 0u32); NOT_FILED.len()];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = (NOT_FILED[i], self.indexed.not_filed[i]);
        }
        out
    }
}
