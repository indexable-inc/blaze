//! A checked body's read list, reduced where the check finishes.
//!
//! A read list exists to be written with the body's record, so a later run checks the record
//! against today's facts. Nothing of the same run reads a body's list after its check: the two
//! later steps that name it want a digest of it, and only that: the code record's key
//! ([`crate::code_records::body_digest`], the record walk of `run.rs`) and a test's code
//! identity (`program/suite.rs`). So the check's one caller that keeps bodies,
//! [`check_body_asked`], files the list out of the `Checked` ([`blaze_typeck::Checked::file_out`]),
//! reduces it at once to a [`ReadsDigest`] in the store ([`Digests`], by the body's owner) and
//! drops it: its memory goes back to the allocator it came from (the checker's, `Global`) before
//! the next body is checked. The `Checked` that lives on (the reach walk's bodies, the program's
//! table) holds an empty list. 16,317 bodies of a hello world held 254.5 MB of lists until the
//! program was made (port27/hello-1116/TABLES-NOTES.md, "Peak attribution", holder 4).
//!
//! [`check_body_filed`] hands the list itself back, for the one caller that files it whole:
//! `project.rs` (a body's verdict lists its reads as facts).
//!
//! A body checked twice in one run (a `main` the walk reaches too, a constant's body) files the
//! same digest: the check reads the same records whatever was checked before it (check.rs).
//! Two different digests under one owner are [`CheckFail::ReadsDiffer`], never a second write
//! (rule 21).

use std::alloc::Global;

use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};
use blaze_own::{AVec, Alloc, Vec};
use blaze_typeck as tc;
use blaze_typeck::{ItemKey, ReadList};

use super::body::Body;
use super::check::{BodyScope, CheckFail, Records, check_rounds};
use super::source::NamesSource;

const READS_DOMAIN: &str = "blaze-ladder checked body read list 1";

/// The digest of one body's read list: each question (sorted, as the checker keeps them, its
/// derived `Hash` written at fixed widths, `code_records::Fixed`) with its answer's hash, and
/// their count. What a later step of the run reads of a body's list.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ReadsDigest(pub ContentId);

impl ReadsDigest {
    /// The digest of `list`; `scratch` holds one question's bytes at a time.
    pub fn of(list: &ReadList<'_>, scratch: Alloc<'_>) -> ReadsDigest {
        digest_rows(list, scratch, None)
    }
}

/// The digest of `list` ([`ReadsDigest::of`]); with `keep`, also each row's [`KeptRow`]
/// pushed there, in the list's (sorted) order.
fn digest_rows(list: &ReadList<'_>, scratch: Alloc<'_>, mut keep: Option<&mut Vec<KeptRow, Global>>) -> ReadsDigest {
    use core::hash::Hash;
    let mut h = Hasher::new(READS_DOMAIN);
    let questions = list.questions();
    let answers = list.rows.column::<1>();
    h.update(&(questions.len() as u64).to_le_bytes());
    let mut bytes: AVec<'_, u8> = Vec::new_in(scratch);
    for (q, answer) in questions.iter().zip(answers) {
        bytes.clear();
        q.hash(&mut crate::code_records::Fixed(&mut bytes));
        h.part(&bytes);
        h.update(&answer.to_le_bytes());
        if let Some(keep) = keep.as_deref_mut() {
            keep.push(KeptRow { question: question_print(&bytes), answer: *answer });
        }
    }
    ReadsDigest(h.finish())
}

const ROW_DOMAIN: &str = "blaze-ladder kept read-list row 1";

/// A question's fingerprint, from its fixed-width bytes: in-process only, never written.
fn question_print(bytes: &[u8]) -> u64 {
    let id = Hasher::new(ROW_DOMAIN).update(bytes).finish();
    let mut first = [0u8; 8];
    first.copy_from_slice(&id.0[..8]);
    u64::from_le_bytes(first)
}

/// The most rows of a list whose rows [`Digests`] keeps beside its digest, so a second check
/// that reads other answers names the first row that differs (rule 19). A constant's body
/// (the class this exists for: tempo's census, "checking .. again read other answers") reads
/// about 40 (tests/front2_const_reads_once.rs: 39 and 40); 16 bytes a row, so at most 1 KiB a
/// body, and nothing for a longer list.
pub const MOST_KEPT_ROWS: usize = 64;

/// One row of a kept list: its question's fingerprint and its answer's hash.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct KeptRow {
    question: u64,
    answer: u64,
}

/// What one owner's first check filed.
#[derive(Clone, Copy)]
struct FiledReads {
    digest: ReadsDigest,
    count: u32,
    /// Its rows in `Digests::rows` (start, count), if the list had at most [`MOST_KEPT_ROWS`].
    kept: Option<(u32, u32)>,
}

/// Where two checks of one body read differently ([`CheckFail::ReadsDiffer`]): the rows of
/// the second check that the first did not read as it did, by question, and how many of the
/// first check's questions the second did not ask. `first_kept` false: the first list was
/// longer than [`MOST_KEPT_ROWS`] and only its row count is known.
#[derive(Debug)]
pub struct ReadsDiff {
    pub first_rows: u32,
    pub second_rows: u32,
    pub first_kept: bool,
    /// Each second-check row not in the first, in question order: the question, its answer
    /// now, and the first check's answer to the same question (`None`: not asked then). At
    /// most [`MOST_DIFF_ROWS`].
    pub second_only: Vec<(tc::Question, u64, Option<u64>), Global>,
    /// How many rows of the second check differ in all (`second_only` holds the first ones).
    pub second_differ: u32,
    /// How many of the first check's questions the second did not ask.
    pub first_only: u32,
    /// The checked body, by path, and each `second_only` row's item, by path (rule 19): filled
    /// where the store is at hand ([`check_body_asked`]), else empty.
    pub shown: Vec<Box<str, Global>, Global>,
}

/// The most differing rows a [`ReadsDiff`] names.
pub const MOST_DIFF_ROWS: usize = 4;

/// The item a question is about, if it names one.
fn question_item(q: &tc::Question) -> Option<ItemKey> {
    use tc::Question as Q;
    match *q {
        Q::FnSig(k) | Q::Predicates(k) | Q::Adt(k) | Q::ImplsOf(k, _) | Q::ImplHeader(k) | Q::AssocItem(k, _) | Q::Const(k) | Q::Opaque(k) | Q::Attrs(k) | Q::TraitDef(k) | Q::AssocParent(k) => Some(k),
        Q::ConstHolds(k, _) | Q::ConstParams(k) | Q::Closure(k, _) | Q::Coroutine(k, _) | Q::AssocGenerics(k, _) | Q::TypeAlias(k) | Q::FieldDefaults(k) => Some(k),
        Q::Inherent(tc::Shape::Adt(k), _) => Some(k),
        Q::Inherent(..) | Q::TraitsInScope(_) | Q::Path(..) | Q::Lang(_) | Q::LangPath(_) => None,
    }
}

impl ReadsDiff {
    /// Names the body `owner` and each differing row's item by path, as [`super::check::key_shown`] does.
    fn name<'n, S: NamesSource<'n>>(&mut self, r: &Records<'_, 'n, S>, owner: ItemKey) {
        self.shown.clear();
        self.shown.push(super::check::key_shown(r, owner));
        for (q, _, _) in self.second_only.iter() {
            self.shown.push(match question_item(q) {
                Some(k) => super::check::key_shown(r, k),
                None => Box::from("-"),
            });
        }
    }
}

impl core::fmt::Display for ReadsDiff {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if let Some(body) = self.shown.first() {
            write!(f, "the body of {body}: ")?;
        }
        write!(f, "first check {} reads, second {}", self.first_rows, self.second_rows)?;
        if !self.first_kept {
            return write!(f, "; the first list was longer than {MOST_KEPT_ROWS} rows, not kept");
        }
        write!(f, "; {} second-check rows differ, {} first-check questions not asked again", self.second_differ, self.first_only)?;
        for (at, (q, now, then)) in self.second_only.iter().enumerate() {
            match then {
                Some(then) => write!(f, "; {q:?} answered {then:#018x} first, {now:#018x} second")?,
                None => write!(f, "; {q:?} asked only by the second check (answer {now:#018x})")?,
            }
            if let Some(item) = self.shown.get(at + 1) {
                write!(f, " [{item}]")?;
            }
        }
        Ok(())
    }
}

/// Each checked body's [`ReadsDigest`], by its owner: the run's one copy of what its bodies
/// read, kept in the store ([`super::heads::Store::reads`]). About 50 bytes a body, and 16 a
/// row for a list of at most [`MOST_KEPT_ROWS`] rows (the refusal's evidence).
pub struct Digests {
    by_owner: HashMap<ItemKey, FiledReads, Global>,
    rows: Vec<KeptRow, Global>,
}

impl Default for Digests {
    fn default() -> Digests {
        Digests::new()
    }
}

impl Digests {
    pub fn new() -> Digests {
        Digests { by_owner: HashMap::new_in(Global), rows: Vec::new_in(Global) }
    }

    /// `list`, read by a clean check of `owner`'s body, filed as its digest (and its rows, for
    /// a list of at most [`MOST_KEPT_ROWS`]); `scratch` holds one question's bytes at a time.
    /// A digest already filed for `owner` must be the same: another is
    /// [`CheckFail::ReadsDiffer`], naming the rows that differ, and the first stays.
    pub fn file(&mut self, owner: ItemKey, list: &ReadList<'_>, scratch: Alloc<'_>) -> Result<(), CheckFail> {
        let count = list.questions().len();
        if let Some(had) = self.by_owner.get(&owner).copied() {
            if had.digest == ReadsDigest::of(list, scratch) {
                return Ok(());
            }
            return Err(CheckFail::ReadsDiffer { owner, diff: Box::new(self.diff(had, list, scratch)) });
        }
        let filed = if count <= MOST_KEPT_ROWS {
            let start = self.rows.len() as u32;
            let digest = digest_rows(list, scratch, Some(&mut self.rows));
            FiledReads { digest, count: count as u32, kept: Some((start, count as u32)) }
        } else {
            FiledReads { digest: ReadsDigest::of(list, scratch), count: count as u32, kept: None }
        };
        self.by_owner.insert(owner, filed);
        Ok(())
    }

    /// How `list` differs from the first check's rows `had`.
    fn diff(&self, had: FiledReads, list: &ReadList<'_>, scratch: Alloc<'_>) -> ReadsDiff {
        let questions = list.questions();
        let mut diff = ReadsDiff { first_rows: had.count, second_rows: questions.len() as u32, first_kept: had.kept.is_some(), second_only: Vec::new_in(Global), second_differ: 0, first_only: 0, shown: Vec::new_in(Global) };
        let Some((start, len)) = had.kept else { return diff };
        let first = &self.rows[start as usize..(start + len) as usize];
        let mut now: Vec<KeptRow, Global> = Vec::with_capacity_in(questions.len(), Global);
        let _ = digest_rows(list, scratch, Some(&mut now));
        for (at, row) in now.iter().enumerate() {
            let then = first.iter().find(|r| r.question == row.question);
            if then.is_some_and(|t| t.answer == row.answer) {
                continue;
            }
            diff.second_differ += 1;
            if diff.second_only.len() < MOST_DIFF_ROWS {
                diff.second_only.push((questions[at], row.answer, then.map(|t| t.answer)));
            }
        }
        diff.first_only = first.iter().filter(|t| !now.iter().any(|r| r.question == t.question)).count() as u32;
        diff
    }

    /// The digest filed for `owner`'s body, if it was checked clean in this run.
    pub fn get(&self, owner: ItemKey) -> Option<ReadsDigest> {
        self.by_owner.get(&owner).map(|f| f.digest)
    }

    /// How many bodies filed one.
    pub fn len(&self) -> usize {
        self.by_owner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_owner.is_empty()
    }
}

/// `check_body_counted` that also leaves, in `asked`, what a check that did NOT end in a
/// clean `Checked` asked of the world (front2 reads.rs: every body has a read list): the read
/// list of a run that finished but is returned as a failure (diagnostics, or a gap answered as
/// missing), and the questions of the last round that ended in a need. A clean check's list is
/// reduced to its [`ReadsDigest`], filed in `r.store.reads` by the body's owner, and dropped:
/// the `Checked` returned holds an empty list (the module doc). What is left in `asked` is a
/// report, never an input to the outcome.
pub fn check_body_asked<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    body: &Body<'_>,
    scope: &BodyScope<'_>,
    count: Option<(&mut tc::Tally, &dyn tc::Clock)>,
    asked: Option<&mut super::reads::Asked<'_>>,
) -> Result<tc::Checked<'static>, CheckFail> {
    let mut done = check_rounds(r, body, scope, count, asked)?;
    // Its opaques' hidden types, for an auto trait asked by another body (front2 opaque_auto.rs).
    super::opaque_auto::file(r, body, &done)?;
    let filed = done.file_out();
    let scratch = r.store.tys.alloc();
    let owner = body.typeck.owner;
    match r.store.reads.file(owner, &filed.reads, scratch) {
        Ok(()) => {}
        Err(CheckFail::ReadsDiffer { owner, mut diff }) => {
            diff.name(r, owner);
            return Err(CheckFail::ReadsDiffer { owner, diff });
        }
        Err(other) => return Err(other),
    }
    drop(filed);
    // Its closures, for a body that names one outside its owner (front2 foreign_closures.rs).
    let store = &mut *r.store;
    store.closures.file(&body.typeck.view(tc::PTys::EMPTY), &done, &mut store.tys);
    Ok(done)
}

/// [`check_body_asked`] for the one caller that files a clean check's list whole (`project.rs`:
/// the body's facts): the `Checked`, its list and needs moved out beside it. Its read list is
/// not filed in the store (the caller holds it); its closures are, as `check_body_asked` files
/// them (front2 foreign_closures.rs).
pub fn check_body_filed<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    body: &Body<'_>,
    scope: &BodyScope<'_>,
    count: Option<(&mut tc::Tally, &dyn tc::Clock)>,
    asked: Option<&mut super::reads::Asked<'_>>,
) -> Result<(tc::Checked<'static>, tc::Filed<'static>), CheckFail> {
    check_body_filed_as(r, body, scope, count, asked, super::stalled_goals::Stalls::Gap)
}

/// [`check_body_filed`] with the check's entry for a stalled auto-trait goal named (front2
/// stalled_goals.rs `Stalls`): `ProjectWorld::check_borrows` alone passes `Stalls::Decide`.
pub fn check_body_filed_as<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    body: &Body<'_>,
    scope: &BodyScope<'_>,
    count: Option<(&mut tc::Tally, &dyn tc::Clock)>,
    asked: Option<&mut super::reads::Asked<'_>>,
    stalls: super::stalled_goals::Stalls,
) -> Result<(tc::Checked<'static>, tc::Filed<'static>), CheckFail> {
    let mut done = super::check::check_rounds_as(r, body, scope, count, asked, stalls)?;
    // Its opaques' hidden types, for an auto trait asked by another body (front2 opaque_auto.rs).
    super::opaque_auto::file(r, body, &done)?;
    let filed = done.file_out();
    // Its closures, for a body that names one outside its owner (front2 foreign_closures.rs):
    // the same filing as `check_body_asked`'s, so a closure is found whichever path checked
    // its owner, and the owner is never checked again for it (rule 77).
    let store = &mut *r.store;
    store.closures.file(&body.typeck.view(tc::PTys::EMPTY), &done, &mut store.tys);
    Ok((done, filed))
}

#[cfg(test)]
mod tests {
    use blaze_own::Soa;
    use blaze_typeck::{ItemKey, LangItem, Question, ReadList};

    use super::{CheckFail, Digests};

    fn list<'a>(rows: &[(Question, u64)], alloc: blaze_own::Alloc<'a>) -> ReadList<'a> {
        let mut out = Soa::new_in(alloc);
        for &row in rows {
            out.push(row);
        }
        ReadList { rows: out }
    }

    /// One owner filed twice: the same list is one digest; a list that answers one question
    /// otherwise and asks one more is refused, naming both rows and the first's answer.
    #[test]
    fn reads_differ_names_the_rows() {
        let alloc = blaze_own::testing::heap().alloc();
        let owner = ItemKey(7);
        let (sig, sized, copy) = (Question::FnSig(ItemKey(9)), Question::Lang(LangItem::Sized), Question::Lang(LangItem::Copy));
        let mut digests = Digests::new();
        digests.file(owner, &list(&[(sig, 1), (sized, 2)], alloc), alloc).expect("first filing");
        digests.file(owner, &list(&[(sig, 1), (sized, 2)], alloc), alloc).expect("the same list again is the same digest");
        let mut rows = [(sig, 1), (sized, 3), (copy, 4)];
        rows.sort_unstable();
        let Err(CheckFail::ReadsDiffer { diff, .. }) = digests.file(owner, &list(&rows, alloc), alloc) else { panic!("another list under one owner was filed") };
        assert_eq!((diff.first_rows, diff.second_rows, diff.second_differ, diff.first_only), (2, 3, 2, 0));
        assert!(diff.second_only.contains(&(sized, 3, Some(2))), "{diff}");
        assert!(diff.second_only.contains(&(copy, 4, None)), "{diff}");
        let shown = format!("{diff}");
        assert!(shown.contains("answered 0x0000000000000002 first, 0x0000000000000003 second"), "{shown}");
    }
}
