//! A body's reads of the heads it used, and the `Signature` task that makes each head's out
//! (D114 piece (d); signature.rs says what a `Signature` is).
//!
//! THE BODY'S SIDE ([`read_heads`]): after its check, each fact of its read list that is about
//! a head (`head_digest::reads_a_head`) is named by its want, the want's place (crate, file,
//! entry), and, when that file is a checkout file, by its `Signature` name (the file's real
//! path and the entry's chain) with the out the `Signature` task gives for it (the want's head
//! records' digest and the head's tokens, signature.rs `signature_out`). That (name, out) is a
//! read of the body: a head edited in ANOTHER file or crate moves the out, and with it the body,
//! while its own text and its crates' `CrateScope` outs stand. A head of a crate outside the
//! checkout is covered by that crate's `CrateScope` read (its identity is in its key) and is
//! never listed: no task of this request settles it, and a read no task settles is refused by
//! the drive's stale loop (drive.md, D114 rulings). A head written inside the reading body is
//! covered by the body's own text. A head that cannot be named (a macro wrote it, it is inside
//! another body, its digest could not be made) is one `TYPES_NOT_CHECKED` finding with its
//! words, never a dropped read (rule 3).
//!
//! THE TASK'S SIDE ([`FileTypes::signature`]): in its file's body job, the want at its entry
//! (`want_at_entry`), its digest, and its out; its reads are the job's (graph, crates).
//!
//! THE PLANT (`work::Plant::HeadReadLeftOut`): the body drops its first head
//! read. signature_grain_tests.rs `a_head_read_left_out_is_caught` runs the stale check with it
//! and asserts the right counts are NOT seen.

use blaze_drive::Name;
use blaze_file_items::Outline;
use blaze_ids::ContentId;
use blaze_ladder::front2::heads::Want;
use blaze_ladder::front2::project::BodyVerdict;
use blaze_ladder::front2::project::head_digest::{HeadDigestError, WantPlace, reads_a_head};
use blaze_own::Alloc;
use blaze_scope::items_record::ItemsRecord;
use blaze_surface::{Edition, Item, ItemIndex};
use std::alloc::Global;

use super::{Checker, FileTypes, TYPES_NOT_CHECKED};
use crate::signature::{NO_WANT, file_subject, is_signature_entry, signature_name, signature_out, unencoded};
use crate::work::Record;

/// A file of a crate a body's heads are in, as a job reads it once: its subject, its text and
/// outline as they are now, its crate's edition. `None`: not a checkout file.
pub(super) struct HeadFile<'n> {
    subject: ContentId,
    text: &'n str,
    outline: Outline<'n>,
    edition: Edition,
}

/// What one owner's checker keeps of heads across the bodies of its job.
#[derive(Default)]
pub(super) struct Heads<'n> {
    /// (crate, file) -> the file, or `None` (not a checkout file), or why it could not be read.
    files: std::vec::Vec<((usize, u32), Result<Option<HeadFile<'n>>, &'n str>), Global>,
    /// Each want's out, once a job.
    outs: std::vec::Vec<(Want, Result<ContentId, &'n str>), Global>,
}

impl<'n> Checker<'n> {
    /// The file `file` of crate `krate` (the owner's reader order), read once a job.
    fn head_file(&mut self, krate: usize, file: u32, frame: &'n bstd::mem::Frame) -> Result<Option<&HeadFile<'n>>, &'n str> {
        let at = match self.heads.files.iter().position(|(k, _)| *k == (krate, file)) {
            Some(at) => at,
            None => {
                let read = self.read_head_file(krate, file, frame);
                self.heads.files.push(((krate, file), read));
                self.heads.files.len() - 1
            }
        };
        match &self.heads.files[at].1 {
            Ok(found) => Ok(found.as_ref()),
            Err(why) => Err(*why),
        }
    }

    fn read_head_file(&self, krate: usize, file: u32, frame: &'n bstd::mem::Frame) -> Result<Option<HeadFile<'n>>, &'n str> {
        let g = Alloc::global();
        let reader = self.readers.get(krate).ok_or("a head's crate is not among the body's crates (body_types/heads.rs)")?;
        let items = ItemsRecord::open(reader.records.items, g).map_err(|_| "a head's crate's items record does not open (body_types/heads.rs)")?;
        let row = match items.file(file) {
            Ok(Some(row)) => row,
            _ => return Err("a head's file is not in its crate's items record (body_types/heads.rs)"),
        };
        let joined = row.path.in_package(reader.package, g);
        let Some(real) = joined.as_deref().and_then(|p| p.strip_prefix("/0/")) else { return Ok(None) };
        let real = super::held_str(real, frame);
        let text = self.records.file_now(krate, file).ok_or("a head's file has no text now (front2 `Records::file_now`)")?;
        let edition = items.edition();
        let outline = blaze_file_items::outline(text, edition, g);
        Ok(Some(HeadFile { subject: file_subject(real), text, outline, edition }))
    }

    /// The out of `want`'s `Signature`, placed at `place`, once a job.
    fn head_out(&mut self, want: Want, place: WantPlace, frame: &'n bstd::mem::Frame) -> Result<Option<(Name, ContentId)>, &'n str> {
        let file = match self.head_file(place.krate, place.file, frame)? {
            None => return Ok(None),
            Some(file) => file,
        };
        let Some(item) = file.outline.items().get(place.item.0 as usize) else {
            return Err("a head's entry is not in its file as it is now (body_types/heads.rs)");
        };
        if !is_signature_entry(&file.outline, item) {
            return Err("a head written inside a body or in an extern block has no `Signature` yet (D114 FACTS (iii): an item in a body the crate sees)");
        }
        let name = signature_name(file.subject, &file.outline, item, file.text);
        let tokens = unencoded(&file.outline, place.item, file.text, file.edition);
        if let Some((_, out)) = self.heads.outs.iter().find(|(w, _)| *w == want) {
            return out.map(|out| Some((name, out)));
        }
        let digest = self.world.want_digest(want, Alloc::global()).map_err(|e| head_words(frame, &e));
        let out = digest.map(|d| signature_out(d, tokens));
        self.heads.outs.push((want, out));
        out.map(|out| Some((name, out)))
    }
}

/// The words of a head that could not be named or digested.
fn head_words<'n>(frame: &'n bstd::mem::Frame, error: &HeadDigestError) -> &'n str {
    super::held_str(bstd::mem::format_in!(frame, "a head the body read could not be named as a `Signature`: {error}").as_str(), frame)
}

/// The body `item` (over `text`, in the owner's file `own_file`) read the heads of `verdict`'s
/// facts: each is a read of `made` (see the module docs). `left_out`: the planted bug.
pub(super) fn read_heads<'n>(checker: &mut Checker<'n>, verdict: &BodyVerdict<'_>, item: &Item, frame: &'n bstd::mem::Frame, made: &mut Record<Global>, left_out: bool) {
    let facts = match &verdict.facts {
        Ok(facts) => facts,
        Err(error) => {
            let words = bstd::mem::format_in!(frame, "the body's read list is refused, so the heads it used are not named: {error:?}");
            made.find(TYPES_NOT_CHECKED, (0, 0), &[words.as_str()]);
            return;
        }
    };
    let own = (checker.file, item.range.start, item.range.end);
    let mut dropped = !left_out;
    for read in facts.rows.iter() {
        if !reads_a_head(read.fact.kind) {
            continue;
        }
        let want = match checker.world.want_of_fact(read.fact) {
            Ok(Some(want)) => want,
            Ok(None) => continue,
            Err(error) => {
                made.find(TYPES_NOT_CHECKED, (0, 0), &[head_words(frame, &error)]);
                continue;
            }
        };
        let place = match checker.world.want_place(want) {
            Ok(place) => place,
            Err(error) => {
                made.find(TYPES_NOT_CHECKED, (0, 0), &[head_words(frame, &error)]);
                continue;
            }
        };
        // A head written inside this body is read with the body's own text: its key covers it.
        if place.krate == 0 && place.file == own.0 && within(checker, place, own.1, own.2, frame) {
            continue;
        }
        match checker.head_out(want, place, frame) {
            Ok(Some(read)) => {
                if !dropped {
                    // PLANT (work::Plant::HeadReadLeftOut): the first head read is not listed.
                    dropped = true;
                    continue;
                }
                if !made.reads.iter().any(|(name, _)| *name == read.0) {
                    made.reads.push(read);
                }
            }
            // Not a checkout file: covered by its crate's `CrateScope` read.
            Ok(None) => {}
            Err(why) => made.find(TYPES_NOT_CHECKED, (0, 0), &[why]),
        }
    }
}

/// Whether the entry at `place` (in the owner's own file) lies inside `start..end`, the reading
/// body's range in today's text.
fn within<'n>(checker: &mut Checker<'n>, place: WantPlace, start: u32, end: u32, frame: &'n bstd::mem::Frame) -> bool {
    match checker.head_file(place.krate, place.file, frame) {
        // The body's own entry is not inside itself: a recursive fn reads its own head.
        Ok(Some(file)) => file
            .outline
            .items()
            .get(place.item.0 as usize)
            .is_some_and(|it| start <= it.range.start && it.range.end <= end && (it.range.start, it.range.end) != (start, end)),
        _ => false,
    }
}

impl<'n> FileTypes<'n> {
    /// PLANT (`work::Plant::HeadReadLeftOut`): each body of this job drops its first head read.
    pub(crate) fn plant_head_read_left_out(&mut self) {
        self.left_out = true;
    }

    /// The `Signature` task of entry `at` of `outline` (over `text`, in `edition`), named
    /// `name`: its out, its reads (the job's), and a `TYPES_NOT_CHECKED` finding when the head
    /// could not be made.
    pub(crate) fn signature(&mut self, outline: &Outline<'_>, at: ItemIndex, text: &str, edition: Edition, name: Name) -> Record<Global> {
        let mut made = Record::new_in(name, Global);
        made.reads.extend(self.reads.iter().copied());
        let tokens = unencoded(outline, at, text, edition);
        let mut digest = None;
        for owner in self.owners.iter_mut() {
            let Ok(checker) = owner else { continue };
            let Some(want) = checker.world.want_at_entry(checker.file, at) else { continue };
            let errors = checker.records.errors().len();
            match checker.world.want_digest(want, Alloc::global()) {
                Ok(d) => digest = Some(d),
                Err(error) => made.find(TYPES_NOT_CHECKED, (0, 0), &[head_words(self.frame, &error)]),
            }
            if let Some(error) = checker.records.errors().get(errors) {
                let words = bstd::mem::format_in!(self.frame, "the records did not answer while making a head: {error}");
                made.find(TYPES_NOT_CHECKED, (0, 0), &[words.as_str()]);
            }
            break;
        }
        made.out = signature_out(digest.unwrap_or(NO_WANT), tokens);
        made
    }
}
