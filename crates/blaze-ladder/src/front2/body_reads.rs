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
        }
        ReadsDigest(h.finish())
    }
}

/// Each checked body's [`ReadsDigest`], by its owner: the run's one copy of what its bodies
/// read, kept in the store ([`super::heads::Store::reads`]). 40 bytes a body.
pub struct Digests {
    by_owner: HashMap<ItemKey, ReadsDigest, Global>,
}

impl Default for Digests {
    fn default() -> Digests {
        Digests::new()
    }
}

impl Digests {
    pub fn new() -> Digests {
        Digests { by_owner: HashMap::new_in(Global) }
    }

    /// `digest` filed for `owner`'s body. A digest already filed for it must be the same:
    /// another is [`CheckFail::ReadsDiffer`] and the first stays.
    pub fn file(&mut self, owner: ItemKey, digest: ReadsDigest) -> Result<(), CheckFail> {
        match self.by_owner.get(&owner) {
            Some(had) if *had != digest => Err(CheckFail::ReadsDiffer { owner }),
            Some(_) => Ok(()),
            None => {
                self.by_owner.insert(owner, digest);
                Ok(())
            }
        }
    }

    /// The digest filed for `owner`'s body, if it was checked clean in this run.
    pub fn get(&self, owner: ItemKey) -> Option<ReadsDigest> {
        self.by_owner.get(&owner).copied()
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
    let filed = done.file_out();
    let digest = ReadsDigest::of(&filed.reads, Alloc::global());
    drop(filed);
    r.store.reads.file(body.typeck.owner, digest)?;
    Ok(done)
}

/// [`check_body_asked`] for the one caller that files a clean check's list whole (`project.rs`:
/// the body's facts): the `Checked`, its list and needs moved out beside it. Nothing is filed
/// in the store: the caller holds the list.
pub fn check_body_filed<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    body: &Body<'_>,
    scope: &BodyScope<'_>,
    count: Option<(&mut tc::Tally, &dyn tc::Clock)>,
    asked: Option<&mut super::reads::Asked<'_>>,
) -> Result<(tc::Checked<'static>, tc::Filed<'static>), CheckFail> {
    let mut done = check_rounds(r, body, scope, count, asked)?;
    let filed = done.file_out();
    Ok((done, filed))
}
