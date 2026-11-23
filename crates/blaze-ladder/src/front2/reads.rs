//! A body's read list as facts (drive.md, "End state: the whole pipeline on the command",
//! piece 8 and the gap row "Read lists as facts"): what the body's check asked of the world,
//! each as (which fact, the digest of the answer it got), sorted by the fact's bytes, with no
//! duplicate. Every body has one: clean, with diagnostics, with gaps or refused.
//!
//! A FACT ([`Fact`]) is identified by keys only, never by a path of the machine or a position
//! in a text:
//!
//! - its kind ([`FactKind`]: a name answer, an item's head, an impl bucket, a constant's
//!   value, ..);
//! - the key it is about (`key`: the checker key of the item, trait, impl or constant, or of
//!   the module a name is resolved in; 0 where the fact has none), which is what the driver
//!   maps to the record that answers it (the first 16 bytes of the library key:
//!   `NamesSource::key_beginning` gives the whole key back);
//! - the rest of the question as one 64-bit digest (`detail`): the names written, a self
//!   type's shape, a lang item, the digest of ground arguments.
//!
//! The answer's digest is the type checker's own (`blaze_typeck::Reads::digest`, the one a
//! replay compares), so a list here and a replay there cannot disagree about what moved.
//!
//! WHERE THE ROWS COME FROM. The checker's read list of the run that finished (blaze-typeck
//! `ReadList`: each question with its answer's digest), translated by [`facts_of`]. A body's
//! paths, the traits in scope, the heads (made on demand by the front end, then read by the
//! checker as `FnSig`, `Predicates`, `Adt`, `TraitDef`, `ImplHeader`), the impl buckets
//! (`ImplsOf(trait, self shape)`, `Inherent(self shape, name)`: Copy and Drop are buckets of
//! those traits; a layout's inputs are the `Adt` rows), constants' values and lang items are
//! all questions of the checker, so they are all rows. A check that stopped before it finished
//! (a gap, a refusal of the loop) lists the questions its last round asked with NO answer
//! digest ([`Reach::Asked`]): what to watch to try the body again.
//!
//! WHAT IS NOT LISTED, by name ([`NotListed`], rule 3: said, never guessed): what the body's
//! LOWERING asks of the names source directly, before the checker runs (the macro a call in
//! the body names and its definition's text; the body's own item text and its container's
//! member list). Those answers are held in the world's per-command tables (the head store, the
//! key map), which an earlier body may have filled, so a recorder under the names source would
//! list them for the first body that asked and for no later one: a list that depended on the
//! order bodies were checked in. They need a recorder at the lowering's own seam (front2
//! body.rs), which is not built. A consumer must take [`BodyReads::not_listed`] as part of the
//! body's inputs until that list is empty.

use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc};
use blaze_typeck as tc;
use blaze_typeck::{BodyPath, Question, Segment, StableHasher};

/// The domain of a read list's digest ([`BodyReads::digest`]).
pub const READS_DOMAIN: &str = "blaze-ladder front2 body reads v2";
/// The domain of a name fact's detail: the block scope and each segment's name.
const NAME_DOMAIN: &str = "blaze-ladder front2 read name v1";
/// The domain of a detail made from a question's part (a shape, a lang item).
const PART_DOMAIN: &str = "blaze-ladder front2 read part v1";

/// Which fact a row is. The byte is the row's first byte; a variant is never renumbered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum FactKind {
    /// A name answer: what a path written in the body names. `key`: the module the body is
    /// written in; `detail`: the block scope and the segments' names.
    Name = 1,
    /// The traits in scope. `key`: the body's module; `detail`: the block scope.
    TraitsInScope = 2,
    /// An item's head: a fn's signature. `key`: the fn.
    Head = 3,
    /// An item's generics and where-clauses. `key`: the item.
    Predicates = 4,
    /// A struct's, enum's or union's definition (what a layout is made from). `key`: the type.
    Adt = 5,
    /// An impl bucket of a trait: the impls that may apply to a self type of one shape (Copy,
    /// Drop and every other trait). `key`: the trait; `detail`: the shape.
    ImplBucket = 6,
    /// An impl's header. `key`: the impl.
    ImplHeader = 7,
    /// An inherent bucket: the inherent members of one name on a self type of one shape.
    /// `key`: 0; `detail`: the shape and the name.
    InherentBucket = 8,
    /// An associated item of a trait or impl by name. `key`: the container; `detail`: the name.
    AssocItem = 9,
    /// A lang item. `key`: 0; `detail`: which.
    Lang = 10,
    /// The definition a desugaring's path names. `key`: 0; `detail`: which lang item.
    LangPath = 11,
    /// A constant's value. `key`: the constant.
    Const = 12,
    /// An opaque type's hidden type. `key`: the opaque type.
    Opaque = 13,
    /// An item's attributes. `key`: the item.
    Attrs = 14,
    /// A trait's definition. `key`: the trait.
    TraitDef = 15,
    /// What an associated item is a member of. `key`: the item.
    AssocParent = 16,
    /// A `where const` condition at ground arguments. `key`: the condition's constant;
    /// `detail`: the arguments' digest.
    ConstHolds = 17,
    /// An item's const generic parameters. `key`: the item.
    ConstParams = 18,
    /// A closure's facts. `key`: its owner's body; `detail`: its ordinal.
    Closure = 19,
    /// The generics of a trait's associated type. `key`: the trait; `detail`: the name.
    AssocGenerics = 20,
    /// One crate's bucket of impls the world read to answer an impl question over records
    /// (front2 buckets/asked.rs `BucketRead`). `key`: the first 16 bytes of the crate's key (0
    /// for a fact of the whole graph); `detail`: the bucket (the first 8 bytes of its key's
    /// digest, or `buckets::UNFILED_LIST`, or `buckets::INCOHERENT_FACT`). Its answer is the
    /// bucket's own digest: an impl added to another bucket or crate leaves it equal.
    CrateBucket = 21,
    /// An async block's facts. `key`: its owner's body; `detail`: its ordinal.
    Coroutine = 22,
    /// What the move and borrow check took about an ADT (front2 borrow_reads.rs): whether it
    /// is `Copy`, implements `Drop`, its fields, whether its head declares lifetimes. `key`:
    /// the ADT; `detail`: 0. Listed only by `ProjectWorld::check_borrows`.
    BorrowAdt = 23,
    /// What the move and borrow check took about a fn's head (front2 borrow_reads.rs): the
    /// lifetime part read from its text, whether it writes a lifetime, whether it takes
    /// `&self`. `key`: the fn; `detail`: 0. Listed only by `ProjectWorld::check_borrows`.
    BorrowHead = 24,
    /// A type alias's definition (`Question::TypeAlias`: its generics, defaults and aliased
    /// type), read by a body path through it. `key`: the alias.
    TypeAlias = 25,
    /// The default values of an ADT's fields (`Question::FieldDefaults`), read by a
    /// base-less struct expression `S { a: 1, .. }`. `key`: the ADT.
    FieldDefaults = 26,
    /// Whether a body may name an inherent impl's member fn (`Question::MemberVisible`: the
    /// member's visibility, kept in its impl's head, against the body's module). `key`: the
    /// impl; `detail`: the member's name and the body's module.
    MemberVisible = 27,
    /// Whether an inherent impl's member is unstable under a feature the body's crate does not
    /// turn on (`Question::MemberUnstable`: the member's `#[unstable]`, kept in its impl's head,
    /// against the crate root's `#![feature]` list). `key`: the impl; `detail`: the member's
    /// name and the body's module.
    MemberUnstable = 28,
}

/// One fact a body's check asked for (module docs). Ordered as its bytes are
/// ([`Fact::bytes`]): kind, key, detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fact {
    pub kind: FactKind,
    pub key: u128,
    pub detail: u64,
}

impl Fact {
    /// The fact's 25 bytes: kind, key and detail big-endian, so the order of two facts is the
    /// order of their bytes.
    pub fn bytes(&self) -> [u8; 25] {
        let mut out = [0u8; 25];
        out[0] = self.kind as u8;
        out[1..17].copy_from_slice(&self.key.to_be_bytes());
        out[17..25].copy_from_slice(&self.detail.to_be_bytes());
        out
    }
}

/// One row of a body's list: a fact and the digest of the answer the check got, or `None` for
/// a fact a check asked for and stopped before it finished ([`Reach::Asked`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Read {
    pub fact: Fact,
    pub answer: Option<u64>,
}

/// How far the check that made a list got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// The check finished (clean, with diagnostics, or past a gap): every fact it asked for,
    /// each with its answer's digest.
    Whole,
    /// The check stopped: the facts its last round asked for, with no answer digests.
    Asked,
    /// The body was refused before its check ran (lowering, spelling): no fact was asked of
    /// the checker's world.
    NotChecked,
}

/// A class of ask the list does not hold (module docs, "What is not listed").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotListed {
    /// The macros the body's calls name, and their definitions' texts, asked while lowering.
    LoweringMacros,
    /// The body's own item text and its container's member list, asked while lowering.
    LoweringItemTexts,
}

/// Why a read list could not be made into facts: each a refusal, never a row guessed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadsError {
    /// A `Path` question the body's path table does not name as asked (no such path, another
    /// scope, a desugaring's lang path, segments out of range): its names cannot be read.
    UnnamedPath(Question),
    /// Two questions are one fact and got two answers (rule 21): the fact's bytes miss what
    /// tells them apart.
    TwoAnswers { fact: Fact, first: u64, second: u64 },
    /// The crates the body sees, whose impl buckets its list keeps, were not told
    /// (front2 buckets.rs `Seen`).
    Unseen(super::buckets::Unseen),
}

impl core::fmt::Display for ReadsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ReadsError::UnnamedPath(q) => write!(f, "the read {q:?} names no path of the body's path table as it was asked"),
            ReadsError::TwoAnswers { fact, first, second } => write!(f, "the fact {fact:?} has two answers ({first:016x}, {second:016x}) in one read list (rule keys-cover-every-input)"),
            ReadsError::Unseen(why) => write!(f, "the read list's impl buckets: {why}"),
        }
    }
}

/// A body's read list (module docs).
#[derive(Debug)]
pub struct BodyReads<'a> {
    /// Sorted by fact, one row per fact.
    pub rows: AVec<'a, Read>,
    pub reach: Reach,
}

impl<'a> BodyReads<'a> {
    /// The list of a body whose check did not run.
    pub fn not_checked(alloc: Alloc<'a>) -> BodyReads<'a> {
        BodyReads { rows: AVec::new_in(alloc), reach: Reach::NotChecked }
    }

    /// What the list does not hold, for every body alike.
    pub fn not_listed(&self) -> &'static [NotListed] {
        &[NotListed::LoweringMacros, NotListed::LoweringItemTexts]
    }

    /// The row of `fact`, if the body's check asked for it.
    pub fn get(&self, fact: Fact) -> Option<Read> {
        let at = self.rows.binary_search_by(|row| row.fact.cmp(&fact)).ok()?;
        Some(self.rows[at])
    }

    /// The rows of one kind about one key (an item's head: `(FactKind::Head, key)`).
    pub fn about(&self, kind: FactKind, key: u128) -> impl Iterator<Item = &Read> {
        self.rows.iter().filter(move |row| row.fact.kind == kind && row.fact.key == key)
    }

    /// The list's one digest: its reach, then each row's fact bytes, whether it has an answer
    /// and the answer (little-endian), in the list's order.
    pub fn digest(&self, alloc: Alloc<'_>) -> ContentId {
        // One allocation: the reach byte, then per row its fact (25), the answer flag (1) and
        // the answer (a `u64`, 8): 34. Capacity changes no byte hashed.
        let mut bytes: AVec<'_, u8> = AVec::with_capacity_in(1 + self.rows.len() * 34, alloc);
        bytes.push(match self.reach {
            Reach::Whole => 1,
            Reach::Asked => 2,
            Reach::NotChecked => 3,
        });
        for row in self.rows.iter() {
            bytes.extend_from_slice(&row.fact.bytes());
            bytes.push(u8::from(row.answer.is_some()));
            bytes.extend_from_slice(&row.answer.unwrap_or(0).to_le_bytes());
        }
        ContentId::of(READS_DOMAIN, &bytes)
    }
}

/// What a check left for its caller to list, filled by `check::check_body_asked`.
pub struct Asked<'a> {
    /// The read list of a check that finished but is returned as a failure (diagnostics, or a
    /// gap answered as missing): a finished clean check hands its list back in its `Checked`.
    pub finished: Option<tc::ReadList<'static>>,
    /// Every question the last round that ended in a need asked, sorted, without duplicates.
    pub questions: AVec<'a, Question>,
}

impl<'a> Asked<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Asked<'a> {
        Asked { finished: None, questions: AVec::new_in(alloc) }
    }
}

/// A hasher of a question's part through its derived `Hash`, every integer at a fixed width
/// and little-endian (an enum's discriminant is hashed as an `isize`: 8 bytes here on any
/// machine), into the checker's stable hasher.
struct Fixed(StableHasher);

impl core::hash::Hasher for Fixed {
    fn write(&mut self, bytes: &[u8]) {
        self.0 = self.0.bytes(bytes);
    }
    fn write_u8(&mut self, v: u8) {
        self.write(&[v]);
    }
    fn write_u16(&mut self, v: u16) {
        self.write(&v.to_le_bytes());
    }
    fn write_u32(&mut self, v: u32) {
        self.write(&v.to_le_bytes());
    }
    fn write_u64(&mut self, v: u64) {
        self.write(&v.to_le_bytes());
    }
    fn write_u128(&mut self, v: u128) {
        self.write(&v.to_le_bytes());
    }
    fn write_usize(&mut self, v: usize) {
        self.write(&(v as u64).to_le_bytes());
    }
    fn write_i8(&mut self, v: i8) {
        self.write(&v.to_le_bytes());
    }
    fn write_i16(&mut self, v: i16) {
        self.write(&v.to_le_bytes());
    }
    fn write_i32(&mut self, v: i32) {
        self.write(&v.to_le_bytes());
    }
    fn write_i64(&mut self, v: i64) {
        self.write(&v.to_le_bytes());
    }
    fn write_i128(&mut self, v: i128) {
        self.write(&v.to_le_bytes());
    }
    fn write_isize(&mut self, v: isize) {
        self.write(&(v as i64).to_le_bytes());
    }
    fn finish(&self) -> u64 {
        self.0.finish()
    }
}

/// The detail of a question's part.
fn part<T: core::hash::Hash>(value: &T) -> u64 {
    use core::hash::Hasher;
    let mut h = Fixed(StableHasher::new(PART_DOMAIN));
    value.hash(&mut h);
    h.finish()
}

/// The fact the question `q` asks for, in a body written in `module` whose path table is
/// `paths` over `segments` (`BodyView::paths`, `BodyView::segments`).
pub fn fact_of(q: Question, module: tc::ItemKey, paths: &[BodyPath], segments: &[Segment]) -> Result<Fact, ReadsError> {
    let about = |kind: FactKind, key: tc::ItemKey, detail: u64| Fact { kind, key: key.0, detail };
    Ok(match q {
        Question::FnSig(item) => about(FactKind::Head, item, 0),
        Question::Predicates(item) => about(FactKind::Predicates, item, 0),
        Question::Adt(item) => about(FactKind::Adt, item, 0),
        Question::ImplsOf(trait_, shape) => about(FactKind::ImplBucket, trait_, part(&shape)),
        Question::ImplHeader(item) => about(FactKind::ImplHeader, item, 0),
        Question::Inherent(shape, name) => Fact { kind: FactKind::InherentBucket, key: 0, detail: part(&(shape, name)) },
        Question::AssocItem(container, name) => about(FactKind::AssocItem, container, name.0),
        Question::TraitsInScope(scope) => about(FactKind::TraitsInScope, module, u64::from(scope.0)),
        Question::Path(scope, path) => {
            // The check asked with every segment's name of the body's path at `path`
            // (blaze-typeck replay.rs `ask`): the fact is those names in that scope, so two
            // places of the body that write one path are one fact.
            let written = paths.get(path.ix()).copied().ok_or(ReadsError::UnnamedPath(q))?;
            if written.scope != scope || written.lang.is_some() {
                return Err(ReadsError::UnnamedPath(q));
            }
            let (start, len) = (written.segments.start as usize, written.segments.len as usize);
            let names = start.checked_add(len).and_then(|end| segments.get(start..end)).ok_or(ReadsError::UnnamedPath(q))?;
            let mut h = StableHasher::new(NAME_DOMAIN);
            h.u32(scope.0);
            h.u32(written.segments.len);
            for segment in names {
                h.u64(segment.name.0);
            }
            about(FactKind::Name, module, h.finish())
        }
        Question::Lang(item) => Fact { kind: FactKind::Lang, key: 0, detail: part(&item) },
        Question::LangPath(item) => Fact { kind: FactKind::LangPath, key: 0, detail: part(&item) },
        Question::Const(item) => about(FactKind::Const, item, 0),
        Question::Opaque(item) => about(FactKind::Opaque, item, 0),
        Question::Attrs(item) => about(FactKind::Attrs, item, 0),
        Question::TraitDef(item) => about(FactKind::TraitDef, item, 0),
        Question::AssocParent(item) => about(FactKind::AssocParent, item, 0),
        Question::ConstHolds(item, args) => about(FactKind::ConstHolds, item, args),
        Question::ConstParams(item) => about(FactKind::ConstParams, item, 0),
        Question::Closure(owner, ordinal) => about(FactKind::Closure, owner, u64::from(ordinal)),
        Question::Coroutine(owner, ordinal) => about(FactKind::Coroutine, owner, u64::from(ordinal)),
        Question::AssocGenerics(trait_, name) => about(FactKind::AssocGenerics, trait_, name.0),
        Question::TypeAlias(item) => about(FactKind::TypeAlias, item, 0),
        Question::FieldDefaults(adt) => about(FactKind::FieldDefaults, adt, 0),
        // The answer is the body's module's: two bodies of one module that ask it are one fact.
        Question::MemberVisible(container, name) => about(FactKind::MemberVisible, container, part(&(name, module))),
        // The answer is of the body's crate (its `#![feature]` list); the module names it.
        Question::MemberUnstable(container, name) => about(FactKind::MemberUnstable, container, part(&(name, module))),
    })
}

/// The rows of `questions` (each with its answer's digest in `answers`, or none of them with
/// one), sorted by fact, one row per fact. Two questions that are one fact must have one
/// answer.
pub fn facts_of<'a>(questions: &[Question], answers: Option<&[u64]>, module: tc::ItemKey, paths: &[BodyPath], segments: &[Segment], alloc: Alloc<'a>) -> Result<AVec<'a, Read>, ReadsError> {
    facts_with_buckets(questions, answers, module, paths, segments, None, alloc)
}

/// [`facts_of`] that also lists, for each impl question (`ImplsOf`, `Inherent`), the buckets
/// of each crate the world read to answer it (`buckets::asked`: `Buckets::trait_reads`,
/// `Buckets::inherent_reads`; a pure function of the question and the records, whichever body
/// asked first) as [`FactKind::CrateBucket`] rows: `key` the crate, `detail` the bucket, each
/// with its own answer digest. Over the solved names no bucket is read and no row is added.
///
/// `buckets` comes with what the body sees (front2 buckets.rs `Seen`, its check world's:
/// `Seen::of_checked_body`): a trait question's rows are only the buckets of the crates it
/// sees, so a body of crate A does not read the impl buckets of a crate downstream of A,
/// whose impls its answer never takes (lane RF13). An inherent question's crate is the self
/// type's, which the body names, so it is seen: its rows are kept whole.
pub fn facts_with_buckets<'a>(
    questions: &[Question],
    answers: Option<&[u64]>,
    module: tc::ItemKey,
    paths: &[BodyPath],
    segments: &[Segment],
    buckets: Option<(&super::buckets::Buckets, super::buckets::Seen)>,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, Read>, ReadsError> {
    let mut rows: AVec<'a, Read> = AVec::new_in(alloc);
    let mut bucket_facts = super::buckets::BucketReadUnion::new(alloc);
    let mut cached_sight: Option<super::buckets::Sight<'_>> = None;
    let mut unread_added = false;
    let mut traits_added: blaze_hash::HashSet<tc::ItemKey, Alloc<'a>> = blaze_hash::HashSet::new_in(alloc);
    for (i, &q) in questions.iter().enumerate() {
        let answer = answers.and_then(|all| all.get(i).copied());
        rows.push(Read { fact: fact_of(q, module, paths, segments)?, answer });
        let Some((b, seen)) = buckets else { continue };
        let bucket_row = |bucket: &super::buckets::BucketRead| Read { fact: Fact { kind: FactKind::CrateBucket, key: bucket.krate, detail: bucket.bucket }, answer: Some(bucket.answer) };
        match q {
            Question::ImplsOf(trait_, shape) => {
                let mut sight_err = None;
                b.with_trait_reads(trait_, shape, |own, shared| {
                    let empty = own.is_empty() && shared.is_none_or(|(u, t)| u.is_empty() && t.is_empty());
                    if empty {
                        return;
                    }
                    let sight = match cached_sight {
                        Some(s) => s,
                        None => match b.sight_made(seen) {
                            Ok(s) => {
                                cached_sight = Some(s);
                                s
                            }
                            Err(e) => {
                                sight_err = Some(ReadsError::Unseen(e));
                                return;
                            }
                        },
                    };
                    rows.extend(own.iter().filter(|bucket| sight.sees_read(bucket) && bucket_facts.keep_fact(bucket)).map(bucket_row));
                    if let Some((unread, trait_filter)) = shared {
                        if !unread_added {
                            unread_added = true;
                            rows.extend(unread.iter().filter(|bucket| sight.sees_read(bucket) && bucket_facts.keep_fact(bucket)).map(bucket_row));
                        }
                        if traits_added.insert(trait_) {
                            rows.extend(trait_filter.iter().filter(|bucket| sight.sees_read(bucket) && bucket_facts.keep_fact(bucket)).map(bucket_row));
                        }
                    }
                });
                if let Some(err) = sight_err {
                    return Err(err);
                }
            }
            Question::Inherent(shape, _) => rows.extend(b.inherent_reads(shape).iter().filter(|bucket| bucket_facts.keep_fact(bucket)).map(bucket_row)),
            _ => {}
        }
    }
    rows.sort_unstable();
    let mut kept = 0;
    for i in 0..rows.len() {
        let row = rows[i];
        if kept > 0 && rows[kept - 1].fact == row.fact {
            if let (Some(first), Some(second)) = (rows[kept - 1].answer, row.answer)
                && first != second
            {
                return Err(ReadsError::TwoAnswers { fact: row.fact, first, second });
            }
            continue;
        }
        rows[kept] = row;
        kept += 1;
    }
    rows.truncate(kept);
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use blaze_own::Alloc;
    use blaze_typeck::{BodyPath, ItemKey, List, Name, PathIdx, Question, ScopeId, Segment};

    use super::{Fact, FactKind, ReadsError, facts_of};

    const MODULE: ItemKey = ItemKey(7);
    const F: ItemKey = ItemKey(100);
    const G: ItemKey = ItemKey(200);

    fn path(scope: u32, start: u32, len: u32) -> BodyPath {
        BodyPath { scope: ScopeId(scope), segments: List { start, len }, lang: None, anchor: None }
    }

    fn segment(text: &str) -> Segment {
        Segment { name: Name::of(text), generic_args: List::default(), has_args: false }
    }

    /// Expected line: `READS: 3 facts from 4 questions; one path written twice is one fact;
    /// two answers to one fact refused`.
    #[test]
    fn a_read_list_is_sorted_facts_one_row_each() {
        // The body writes `a::f` twice (paths 0 and 2) and `g` once (path 1).
        let segments = [segment("a"), segment("f"), segment("g"), segment("a"), segment("f")];
        let paths = [path(0, 0, 2), path(0, 2, 1), path(0, 3, 2)];
        let questions = [Question::FnSig(G), Question::FnSig(F), Question::Path(ScopeId(0), PathIdx(0)), Question::Path(ScopeId(0), PathIdx(2))];
        let rows = facts_of(&questions, Some(&[20, 10, 5, 5]), MODULE, &paths, &segments, Alloc::global()).expect("every question names a fact");
        assert_eq!(rows.len(), 3, "one path written twice is one fact: {rows:?}");
        assert!(rows.windows(2).all(|w| w[0].fact.bytes() < w[1].fact.bytes()), "sorted by the fact's bytes, no duplicate: {rows:?}");
        assert_eq!(rows[1].fact, Fact { kind: FactKind::Head, key: F.0, detail: 0 });
        assert_eq!(rows[1].answer, Some(10));
        assert_eq!(rows[2].fact, Fact { kind: FactKind::Head, key: G.0, detail: 0 });
        assert_eq!(rows[0].fact.kind, FactKind::Name);
        assert_eq!(rows[0].fact.key, MODULE.0, "a name fact is about the module it is resolved in");

        // The planted bug: the two writings of `a::f` answered differently. One fact with two
        // answers is refused, never merged.
        let two = facts_of(&questions, Some(&[20, 10, 5, 6]), MODULE, &paths, &segments, Alloc::global());
        assert!(matches!(two, Err(ReadsError::TwoAnswers { first: 5, second: 6, .. })), "two answers to one fact: {two:?}");

        // A path the table does not hold is refused by name.
        let none = facts_of(&[Question::Path(ScopeId(0), PathIdx(9))], None, MODULE, &paths, &segments, Alloc::global());
        assert_eq!(none.err(), Some(ReadsError::UnnamedPath(Question::Path(ScopeId(0), PathIdx(9)))));
        println!("READS: {} facts from {} questions; one path written twice is one fact; two answers to one fact refused", rows.len(), questions.len());
    }
}
