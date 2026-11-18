//! A body's reads of the heads it used, and the `Signature` task that makes each head's out
//! (D114 piece (d); signature.rs says what a `Signature` is).
//!
//! THE BODY'S SIDE ([`read_heads`]): after its check, each fact of its read list that is about
//! a head (`head_digest::reads_a_head`) is named by its want, the want's place (crate, file,
//! entry), and, when that file is a checkout file, by its `Signature` name (the file's real
//! path and the entry's chain). The body READS it at the out its task was handed, the
//! `Signature` task's own (the want's head records' digest and the head's tokens, signature.rs
//! `signature_out`); one it was not handed it NEEDS, at its file's `FileItems`, and is entered
//! again with it ([`HeadReads`]). The body never makes a head's out: when it did (tempo, a fresh
//! home, 2026-11-17), its digest, made in its own world, differed from the task's, the drive
//! found it stale, handed it the task's out, which it ignored, and entered it 64 times; and
//! whether its digest could be made at all hung on the heads the job had made before, so its
//! read list differed between jobs ("two values under one key", rule 21; blaze-drive
//! `DriveError::ReadNotHanded` now refuses it by name). A `Signature` that cannot be made is
//! still a record (its out the tokens alone, with its finding), read like any other: the read
//! list is the same on every entry. That (name, out) is a
//! read of the body: a head edited in ANOTHER file or crate moves the out, and with it the body,
//! while its own text and its crates' `CrateScope` outs stand. A head of a crate outside the
//! checkout is covered by that crate's `CrateScope` read (its identity is in its key) and is
//! never listed: no task of this request settles it, and a read no task settles is refused by
//! the drive's stale loop (drive.md, D114 rulings). Which crate a head is of is asked FIRST, by
//! its key alone (`NamesSource::crate_of`, which opens nothing), before its place: a head of a
//! crate outside the workspace that a macro wrote (core's derives, std's `macro_rules!` impls)
//! has no file entry, and is as covered by its crate's identity as any other head of it (hello
//! world, 2026-11-17: "the head Fn(..) has no file entry: a macro wrote it", exit 4, for a head
//! of the standard library). A head a macro of a WORKSPACE crate wrote is read by the
//! `Signature` of the file entry its call (or its outermost caller) is at
//! ([`Checker::expansion_head`]; signature.rs, "A HEAD A MACRO WROTE"): hello world's blaze
//! edition `fn main(cx: &Cx, out: Out)`, which `bstd::main` takes unasked (D104), is read by
//! `main`'s own entry, whose out then holds the expansion's heads ([`Checker::entry_out`], the
//! task's alone). A head written inside the reading body, or a fn declared in a body (which
//! only a body holding it can name: `bstd::main`'s inner `fn main`), is covered by the body's
//! own text, and so is any item declared in a body but an impl ([`Checker::local_fact`]). A
//! head is named by the entry that COVERS it (signature.rs `covering_entry`): its own, its
//! trait's, its impl's (a member a macro call in the impl block wrote included:
//! [`Checker::member_writer`]) or its `extern` block's. A head that cannot
//! be named (its expansion keeps no origins; it is inside another body, read from outside it;
//! its digest could not be made) is one `TYPES_NOT_CHECKED`
//! finding with its words, saying what the read list holds for it (the crates' `CrateScope`
//! reads, no `Signature`), never a silently dropped read (rule 3). Every such finding, the body's and the `Signature`
//! task's, is filed by not_checked/items.rs `file` with the item's path, a fixed cause and the
//! words: its one form, which the command's printer refuses any other of (hello world,
//! 2026-11-17 00:34: one-argument findings from here stopped `blaze check` with "a
//! types-not-checked finding without its item and cause" and no finding printed).
//!
//! THE TASK'S SIDE ([`FileTypes::signature`]): in its file's body job, the want at its entry
//! (`want_at_entry`), and the entry's out ([`Checker::entry_out`]: its own head, then each
//! expansion at it); its reads are the job's (graph, crates).
//!
//! THE PLANT (`work::Plant::HeadReadLeftOut`): the body drops its first head
//! read (neither read nor needed). signature_grain_tests.rs `a_head_read_left_out_is_caught` runs the stale check with it
//! and asserts the right counts are NOT seen.

use blaze_drive::Name;
use blaze_file_items::Outline;
use blaze_ids::ContentId;
use blaze_ladder::front2::by_origin;
use blaze_ladder::front2::heads::Want;
use blaze_ladder::front2::keys::{MemberKind, Origin, checker_of, member_key_of};
use blaze_ladder::front2::locals::LocalKind;
use blaze_ladder::front2::reads::Fact;
use blaze_ladder::front2::project::BodyVerdict;
use blaze_ladder::front2::project::head_digest::{HeadDigestError, WantPlace, reads_a_head, want_lib};
use blaze_ladder::front2::source::NamesSource;
use blaze_scope::items_record::ItemsRecord;
use blaze_scope::resolve::At;
use blaze_surface::{Edition, Item, ItemIndex, ItemKind};
use std::alloc::Global;

use super::{Checker, FileTypes};
use crate::not_checked::items::{file as not_checked, item_path};
use crate::signature::{NO_WANT, covering_entry, entry_out, expansion_out, file_subject, signature_name, signature_out, uncovered_why, unencoded, wants_digest};
use crate::work::Record;

/// A file of a crate a body's heads are in, as a job reads it once: its subject, its text and
/// outline as they are now, its crate's edition. `None`: not a checkout file.
pub(super) struct HeadFile<'n> {
    subject: ContentId,
    /// The file's `FileItems` name (its checkout path, as the drive names it): the home whose
    /// parts list its entries' `Signature`s, which a body that lacks one asks it of.
    home: Name,
    text: &'n str,
    outline: Outline<'n>,
    edition: Edition,
}

/// What one owner's checker keeps of heads across the bodies of its job.
#[derive(Default)]
pub(super) struct Heads<'n> {
    /// (crate, file) -> the file, or `None` (not a checkout file), or why it could not be read.
    files: std::vec::Vec<((usize, u32), Result<Option<HeadFile<'n>>, &'n str>), Global>,
    /// Each want's digest (`want_digest`), once a job.
    digests: std::vec::Vec<(Want, Result<ContentId, &'n str>), Global>,
    /// Each (crate, file, entry)'s `Signature` out ([`Checker::entry_out`]), once a job.
    entries: std::vec::Vec<((usize, u32, ItemIndex), Result<ContentId, &'n str>), Global>,
    /// Per crate, the calls whose expansions wrote heads ([`Expanded`]), once a job.
    expanded: std::vec::Vec<(usize, Result<std::vec::Vec<Expanded<'n>, Global>, &'n str>), Global>,
}

/// One macro call of a crate whose expansion wrote a head: its key, the file entry it is at
/// (its own, or its outermost caller's: signature.rs, "A HEAD A MACRO WROTE"), or why that
/// could not be read, and the wants of the heads it wrote, by want.
struct Expanded<'n> {
    call: blaze_id::ItemKey,
    top: Result<(u32, ItemIndex), &'n str>,
    wants: std::vec::Vec<Want, Global>,
}

/// A want's place in a fixed order (its kind, its item's key, an impl fn's key): an
/// expansion's wants are digested in it, whatever order the crate's items were walked in.
fn want_order(want: Want) -> (u8, blaze_id::ItemKey, u128) {
    match want {
        Want::Head(k) => (0, k, 0),
        Want::Adt(k) => (1, k, 0),
        Want::Impl(k) => (2, k, 0),
        Want::Fn(k) => (3, k, 0),
        Want::Members(k) => (4, k, 0),
        Want::MemberFn(k, f) => (5, k, f.0),
    }
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

    fn read_head_file(&self, krate: usize, file: u32, _frame: &'n bstd::mem::Frame) -> Result<Option<HeadFile<'n>>, &'n str> {
        let g = self.heap;
        let reader = self.readers.get(krate).ok_or("a head's crate is not among the body's crates (body_types/heads.rs)")?;
        let items = ItemsRecord::open(reader.records.items, g).map_err(|_| "a head's crate's items record does not open (body_types/heads.rs)")?;
        let row = match items.file(file) {
            Ok(Some(row)) => row,
            _ => return Err("a head's file is not in its crate's items record (body_types/heads.rs)"),
        };
        let joined = row.path.in_package(reader.package, g);
        let Some(real) = joined.as_deref().and_then(|p| p.strip_prefix("/0/")) else { return Ok(None) };
        let text = self.records.file_now(krate, file).ok_or("a head's file has no text now (front2 `Records::file_now`)")?;
        let edition = items.edition();
        let outline = blaze_file_items::outline(text, edition, g);
        Ok(Some(HeadFile { subject: file_subject(real), home: Name::file(real.as_bytes()), text, outline, edition }))
    }

    /// The `Signature` that covers a head written at entry `at` of file `file` of crate `krate`
    /// (signature.rs `covering_entry`: the entry's own, or its trait's, impl's or `extern`
    /// block's), and its home (the file's `FileItems`, whose parts list it): what a body that
    /// read that head reads. `None` when the file is not a checkout file. A body only NAMES it
    /// here: its out is the `Signature` task's, handed to the body ([`read_heads`]), never made
    /// by the body.
    fn entry_name(&mut self, krate: usize, file: u32, at: ItemIndex, frame: &'n bstd::mem::Frame) -> Result<Option<(Name, Name)>, &'n str> {
        let read = match self.head_file(krate, file, frame)? {
            None => return Ok(None),
            Some(read) => read,
        };
        let Some(entry) = covering_entry(&read.outline, at) else {
            return Err(uncovered_why(&read.outline, at));
        };
        let item = read.outline.item(entry);
        Ok(Some((signature_name(read.subject, &read.outline, item, read.text), read.home)))
    }

    /// The entry that covers a head at entry `at` of file `file` of crate `krate` (signature.rs
    /// `covering_entry`), `at` itself when the file is not a checkout file or no entry covers it
    /// (named by [`Checker::entry_name`] when a body reads it).
    fn covered_at(&mut self, krate: usize, file: u32, at: ItemIndex, frame: &'n bstd::mem::Frame) -> ItemIndex {
        match self.head_file(krate, file, frame) {
            Ok(Some(read)) => covering_entry(&read.outline, at).unwrap_or(at),
            _ => at,
        }
    }

    /// `want`'s digest, once a job.
    fn digest(&mut self, want: Want, frame: &'n bstd::mem::Frame) -> Result<ContentId, &'n str> {
        if let Some((_, digest)) = self.heads.digests.iter().find(|(w, _)| *w == want) {
            return *digest;
        }
        let digest = self.world.want_digest(want, self.heap).map_err(|e| head_words(frame, &e));
        self.heads.digests.push((want, digest));
        digest
    }

    /// The out of the `Signature` of entry `at` of file `file` of crate `krate` (signature.rs
    /// `entry_out`): its own heads' (`own`'s digests by `wants_digest`, `NO_WANT` for none,
    /// with `tokens`), then each expansion of a call at it that wrote a head, by call key. Once
    /// a job, by the `Signature` task alone: a body reads the out it was handed
    /// ([`read_heads`]).
    #[expect(clippy::too_many_arguments, reason = "an entry's place, its own heads and the frame its words go in")]
    fn entry_out(&mut self, krate: usize, file: u32, at: ItemIndex, own: &[Want], tokens: ContentId, edition: Edition, frame: &'n bstd::mem::Frame) -> Result<ContentId, &'n str> {
        if let Some((_, out)) = self.heads.entries.iter().find(|(k, _)| *k == (krate, file, at)) {
            return *out;
        }
        let out = self.make_entry_out(krate, file, at, own, tokens, edition, frame);
        self.heads.entries.push(((krate, file, at), out));
        out
    }

    #[expect(clippy::too_many_arguments, reason = "as `entry_out`")]
    fn make_entry_out(&mut self, krate: usize, file: u32, at: ItemIndex, own: &[Want], tokens: ContentId, edition: Edition, frame: &'n bstd::mem::Frame) -> Result<ContentId, &'n str> {
        let mut digests: std::vec::Vec<ContentId, Global> = std::vec::Vec::with_capacity(own.len());
        for want in own {
            digests.push(self.digest(*want, frame)?);
        }
        let own = signature_out(wants_digest(&digests), tokens);
        // The calls at this entry, copied out of the job's table (each by call key, the
        // table's order) so their digests can be made.
        let mut calls: std::vec::Vec<(blaze_id::ItemKey, std::vec::Vec<Want, Global>), Global> = std::vec::Vec::new();
        for e in self.expanded(krate, frame)?.iter() {
            if e.top == Ok((file, at)) {
                calls.push((e.call, e.wants.clone()));
            }
        }
        if calls.is_empty() {
            return Ok(entry_out(own, &[]));
        }
        let readers = self.readers;
        let reader = readers.get(krate).ok_or("a head's crate is not among the body's crates (body_types/heads.rs)")?;
        let texts = &reader.records.expansions;
        let mut outs: std::vec::Vec<ContentId, Global> = std::vec::Vec::with_capacity(calls.len());
        for (call, wants) in calls.iter() {
            let mut digests: std::vec::Vec<ContentId, Global> = std::vec::Vec::with_capacity(wants.len());
            for want in wants.iter() {
                digests.push(self.digest(*want, frame)?);
            }
            let text = match texts.text(*call) {
                Ok(Some(text)) => text,
                Ok(None) => return Err("a macro call that wrote a head has no expansion text in its crate's record (names/expansion_texts.rs)"),
                // The why names the call and the record the store lacks (`Missing`).
                Err(why) => return Err(super::held_str(bstd::mem::format_in!(frame, "a macro call's expansion text does not read from its crate's record: {why} (names/expansion_texts.rs)").as_str(), frame)),
            };
            let outline = blaze_file_items::outline(text, edition, self.heap);
            outs.push(expansion_out(&digests, &outline, text, edition));
        }
        Ok(entry_out(own, &outs))
    }

    /// The calls of crate `krate` whose expansions wrote a head, by call key, once a job. Each
    /// call's top is the ENTRY that covers it (signature.rs `covering_entry`): a call in an impl
    /// block is at its impl's entry, one in an `extern` block at its block's.
    fn expanded(&mut self, krate: usize, frame: &'n bstd::mem::Frame) -> Result<&[Expanded<'n>], &'n str> {
        let at = match self.heads.expanded.iter().position(|(k, _)| *k == krate) {
            Some(at) => at,
            None => {
                let mut made = self.make_expanded(krate);
                if let Ok(calls) = made.as_mut() {
                    for e in calls.iter_mut() {
                        if let Ok((file, item)) = e.top {
                            e.top = Ok((file, self.covered_at(krate, file, item, frame)));
                        }
                    }
                }
                self.heads.expanded.push((krate, made));
                self.heads.expanded.len() - 1
            }
        };
        match &self.heads.expanded[at].1 {
            Ok(calls) => Ok(calls.as_slice()),
            Err(why) => Err(*why),
        }
    }

    /// Every head of `krate` an expansion wrote, by its call: an item's want (as
    /// `want_at_entry` classifies an entry's item); for an impl an expansion wrote, each fn it
    /// declares (`Want::MemberFn`); and for ANY impl, each member a macro call inside its block
    /// wrote (`impl T { some_macro!(..); }`): a fn as its `Want::MemberFn`, another member as
    /// its impl's `Want::Impl` (the impl's header holds its member list), under that call.
    fn make_expanded(&self, krate: usize) -> Result<std::vec::Vec<Expanded<'n>, Global>, &'n str> {
        let names = self.records;
        let reader = self.readers.get(krate).ok_or("a head's crate is not among the body's crates (body_types/heads.rs)")?;
        let mut written: std::vec::Vec<(blaze_id::ItemKey, Want), Global> = std::vec::Vec::new();
        let read = NamesSource::each_item(names, krate, &mut |key, _module| {
            let Some(place) = NamesSource::place_of(names, key) else { return };
            let item_call = match place.at {
                At::Expansion(call) => Some(call),
                At::File(_) => None,
            };
            if item_call.is_none() && NamesSource::kind_of_item(names, key) != Some(ItemKind::Impl) {
                return;
            }
            let Some(want) = by_origin::kind_of_lib(names, key).ok().and_then(|kind| by_origin::item_want(names, kind).ok()) else { return };
            if let Some(call) = item_call {
                written.push((call, want));
            }
            if let Want::Impl(_) = want {
                NamesSource::each_member(names, key, &mut |m| {
                    // Its writer: a call in the impl's block, else the call that wrote the impl.
                    let Some(call) = m.written_by.or(item_call) else { return };
                    if m.kind == ItemKind::Function {
                        // Its key as computed, never as filed: a member is filed only once its
                        // impl's head was made in THIS world, and a `Signature` task's world has
                        // made no head a body made (an out must not hang on what ran before it).
                        if let Some(name) = m.name {
                            written.push((call, Want::MemberFn(key, member_key_of(checker_of(key), MemberKind::Fn, name))));
                        }
                    } else if m.written_by.is_some() && m.kind != ItemKind::MacroCall {
                        written.push((call, Want::Impl(key)));
                    }
                });
            }
        });
        if !read {
            return Err("a head's crate's items record is not read (front2 `NamesSource::each_item`)");
        }
        written.sort_unstable_by_key(|(call, want)| (*call, want_order(*want)));
        written.dedup();
        let mut calls: std::vec::Vec<Expanded<'n>, Global> = std::vec::Vec::new();
        for (call, want) in written {
            match calls.last_mut() {
                Some(last) if last.call == call => last.wants.push(want),
                _ => {
                    // The file entry its call (or its outermost caller) is at: covered.rs's walk.
                    let top = super::covered::callers(&reader.records.expansions, call, &mut |_| {});
                    let mut wants = std::vec::Vec::new();
                    wants.push(want);
                    calls.push(Expanded { call, top: top.map_err(|why| why as &'n str), wants });
                }
            }
        }
        Ok(calls)
    }

    /// Where the entry is that covers `want`, a head a macro of a workspace crate wrote: the
    /// entry its call (or its outermost caller) is at, which [`Checker::expanded`] made the
    /// covering one (a call in an impl block: its impl). The call is the item's own place's,
    /// or, for a fn of an impl that a call inside the impl's block wrote, that member's writer
    /// ([`Checker::member_writer`]). `Err`: why it cannot be placed, by its cause.
    fn expansion_head(&mut self, want: Want, frame: &'n bstd::mem::Frame) -> Result<WantPlace, &'n str> {
        let place = NamesSource::place_of(self.records, want_lib(want)).ok_or("a head a macro wrote has no place its crate's record answers")?;
        let call = match (want, place.at) {
            (Want::MemberFn(impl_, f), at) => match (self.member_writer(impl_, f), at) {
                (Some(call), _) | (None, At::Expansion(call)) => call,
                (None, At::File(_)) => return Err("a fn of an impl written in a file has no outline entry and no macro call wrote it (front2 `want_place`)"),
            },
            (_, At::Expansion(call)) => call,
            (_, At::File(_)) => return Err("a head placed in a file has no outline entry there (front2 `want_place`)"),
        };
        // By call key: the table is sorted by it (`make_expanded`).
        let calls = self.expanded(place.krate, frame)?;
        let top = match calls.binary_search_by_key(&call, |e| e.call) {
            Ok(at) => calls[at].top,
            Err(_) => return Err("a head a macro wrote is not among its crate's expanded heads (body_types/heads.rs `make_expanded`)"),
        };
        let (file, item) = top?;
        Ok(WantPlace { krate: place.krate, file, item })
    }

    /// The macro call inside impl `impl_`'s block that wrote its fn `f` (front2 keys an impl's
    /// fn by its impl, kind and name: keys.rs `member_key`), `None` when its impl's own text
    /// declares it.
    fn member_writer(&self, impl_: blaze_id::ItemKey, f: blaze_typeck::ItemKey) -> Option<blaze_id::ItemKey> {
        let Some(Origin::Member { name, .. }) = self.world.front.keys.origin(f) else { return None };
        let mut by = None;
        NamesSource::each_member(self.records, impl_, &mut |m| {
            if m.kind == ItemKind::Function && m.name == Some(&**name) && m.written_by.is_some() {
                by = m.written_by;
            }
        });
        by
    }

    /// Whether `want` is the head of a fn, const or static declared in a body (front2
    /// `Keys::local`, `Want::Fn` by `kind_of_local`): its name is in scope only inside the block
    /// that declares it (the Reference, "Statements", "Item declarations"), so only a body that
    /// holds it reads it by name, and that body's own text, its key, covers its head. Every fn
    /// a body reaches through such items is checked in the same task (covered.rs
    /// `each_covered`). An impl in a body is not one: its members are seen from outside.
    /// Hello world's blaze edition: `bstd::main` writes `fn main() -> Exit { fn main(cx: &Cx,
    /// out: Out) .. }`, and the outer body reads the inner `main`'s head, which has no place of
    /// its own (2026-11-17 03:03: "a head a macro wrote has no place its crate's record answers").
    fn body_local(&self, want: Want) -> bool {
        let Want::Fn(lib) = want else { return false };
        self.world.front.keys.local_kind(checker_of(lib)).is_some()
    }

    /// Whether `fact` is about an item declared in a body of any kind but an impl (front2
    /// `Keys::local`: a struct, enum, union, trait, alias, const, static, fn, or one a macro call
    /// in the body wrote): named only inside its block, so read only by a body that holds it,
    /// whose text covers it, as [`Checker::body_local`] says of a fn. Asked of the fact's key
    /// BEFORE its want, which a type declared in a body has none of (`kind_of_local`'s
    /// `LocalWithoutHead`). An impl in a body is seen from outside, so not covered here.
    fn local_fact(&self, fact: Fact) -> bool {
        matches!(self.world.front.keys.local_kind(blaze_typeck::ItemKey(fact.key)), Some(kind) if !matches!(kind, LocalKind::Impl))
    }

    /// Whether `want`'s head is of a workspace crate (a member, its text in the checkout), by
    /// its key's crate alone: nothing is opened, and a head a macro wrote is answered too.
    fn member_head(&self, want: Want) -> Result<bool, &'static str> {
        let krate = NamesSource::crate_of(self.records, want_lib(want)).ok_or("a head's key begins no crate of the body's crates (front2 `NamesSource::crate_of`)")?;
        let reader = self.readers.get(krate).ok_or("a head's crate is not among the body's crates (body_types/heads.rs)")?;
        Ok(reader.member)
    }

    /// The words of a head a workspace crate's macro wrote that no `Signature` covers: the
    /// head, its crate, and `why` (one of [`Checker::expansion_head`]'s causes).
    fn unnamed_words(&self, want: Want, why: &str, frame: &'n bstd::mem::Frame) -> &'n str {
        let krate = NamesSource::crate_of(self.records, want_lib(want)).and_then(|k| self.readers.get(k)).map_or("?", |reader| reader.crate_name);
        let words = bstd::mem::format_in!(frame, "the head {want:?} of workspace crate `{krate}` was written by a macro: {why}");
        super::held_str(words.as_str(), frame)
    }
}

/// The cause of a body that read a head a macro of a workspace crate wrote where no file
/// entry's `Signature` covers it (the words say which shape: signature.rs, "A HEAD A MACRO
/// WROTE"; heads.rs `expansion_head`).
const HEAD_IN_EXPANSION: &str = "a head it reads was written by a macro of a workspace crate at no entry a `Signature` covers";

/// The cause (not_checked/items.rs groups by it) of an item one of whose heads could not be
/// named or digested as a `Signature`; the words say which and why ([`head_words`]).
const HEAD_NOT_NAMED: &str = "a head it reads could not be named as a `Signature`";

/// The cause of a body whose read list was refused, so the heads it used are not named.
const READS_REFUSED: &str = "its read list was refused, so the heads it used are not named";

/// The cause of a head whose making the records did not answer (the `Signature` task's).
const HEAD_RECORDS: &str = "the records did not answer while its head was made";

/// The words of a head that could not be named or digested: the error's own (the cause is
/// [`HEAD_NOT_NAMED`]).
fn head_words<'n>(frame: &'n bstd::mem::Frame, error: &HeadDigestError) -> &'n str {
    super::held_str(bstd::mem::format_in!(frame, "{error}").as_str(), frame)
}

/// The body `item` (over `text`, in the owner's file `own_file`; `item_path` its path there,
/// not_checked/items.rs `item_path`) read the heads of `verdict`'s facts: each is a read of
/// `made` at the out its `Signature` task was handed to the body, or a need of that task
/// (`reads`, see [`HeadReads`] and the module docs). Which heads are named is a function of the
/// body's facts and the records alone, never of what the job made before: the read list, and
/// the needs, are the same on every entry and in every order.
pub(super) fn read_heads<'n>(checker: &mut Checker<'n>, verdict: &BodyVerdict<'_>, item: &Item, item_path: &str, frame: &'n bstd::mem::Frame, made: &mut Record<Global>, reads: &mut HeadReads<'_>) {
    let facts = match &verdict.facts {
        Ok(facts) => facts,
        Err(error) => {
            let words = bstd::mem::format_in!(frame, "{error:?}");
            not_checked(made, (0, 0), item_path, READS_REFUSED, words.as_str());
            return;
        }
    };
    let own = (checker.file, item.range.start, item.range.end);
    for read in facts.rows.iter() {
        if !reads_a_head(read.fact.kind) {
            continue;
        }
        // An item declared in a body (but an impl) is read only by a body that holds it.
        if checker.local_fact(read.fact) {
            continue;
        }
        let want = match checker.world.want_of_fact(read.fact) {
            Ok(Some(want)) => want,
            Ok(None) => continue,
            Err(error) => {
                not_checked(made, (0, 0), item_path, HEAD_NOT_NAMED, unread_words(head_words(frame, &error), frame));
                continue;
            }
        };
        // A fn declared in a body is read only by a body that holds it: covered by its text.
        if checker.body_local(want) {
            continue;
        }
        // Another crate's head (a file's or a macro's) is covered by its crate's `CrateScope`
        // read: its identity is its text. Asked before the place, which a macro's head has not.
        match checker.member_head(want) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(why) => {
                not_checked(made, (0, 0), item_path, HEAD_NOT_NAMED, unread_words(why, frame));
                continue;
            }
        }
        let place = match checker.world.want_place(want) {
            Ok(place) => place,
            // A macro of this workspace wrote it: read the `Signature` of the entry its call is
            // at (signature.rs, "A HEAD A MACRO WROTE"), found as any other head's below.
            Err(HeadDigestError::Unplaced { .. }) => match checker.expansion_head(want, frame) {
                Ok(top) => top,
                Err(why) => {
                    let words = checker.unnamed_words(want, why, frame);
                    not_checked(made, (0, 0), item_path, HEAD_IN_EXPANSION, unread_words(words, frame));
                    continue;
                }
            },
            Err(error) => {
                not_checked(made, (0, 0), item_path, HEAD_NOT_NAMED, unread_words(head_words(frame, &error), frame));
                continue;
            }
        };
        // A head written inside this body (or by a call inside it) is read with the body's own
        // text: its key covers it.
        let at = if place.krate == 0 && place.file == own.0 { where_is(checker, place, own.1, own.2, frame) } else { Where::Elsewhere };
        if at == Where::Inside {
            continue;
        }
        match checker.entry_name(place.krate, place.file, place.item, frame) {
            Ok(Some((name, home))) => reads.read(name, home, made, at == Where::Own),
            // Not a checkout file: covered by its crate's `CrateScope` read.
            Ok(None) => {}
            Err(why) => not_checked(made, (0, 0), item_path, HEAD_NOT_NAMED, unread_words(why, frame)),
        }
    }
}

/// A body's reads of `Signature`s as it names them: each at the out its task was handed
/// (`handed`, sorted by name), or, when not handed, a need of it at its home (`needs`): the
/// body is then entered again with it handed. Never an out the body made (rule 21: the
/// drive's `DriveError::ReadNotHanded`). `dropped`: the plant's one head, left out of every
/// fact that names it (`None` once armed and before the first head).
pub(super) struct HeadReads<'h> {
    handed: &'h [(Name, ContentId)],
    needs: &'h mut std::vec::Vec<blaze_drive::Want, Global>,
    planted: bool,
    dropped: Option<Name>,
}

impl<'h> HeadReads<'h> {
    /// A body's reads over what its task was `handed` (by name: [`handed_reads`]), its needs
    /// into `needs`; `left_out`: the plant.
    pub(super) fn new(handed: &'h [(Name, ContentId)], needs: &'h mut std::vec::Vec<blaze_drive::Want, Global>, left_out: bool) -> HeadReads<'h> {
        HeadReads { handed, needs, planted: left_out, dropped: None }
    }

    /// `own`: the head is the body's own entry's, which the plant never takes (the edit it is
    /// caught by is of another entry's head).
    fn read(&mut self, name: Name, home: Name, made: &mut Record<Global>, own: bool) {
        if self.planted && !own {
            // PLANT (work::Plant::HeadReadLeftOut): the first head of another entry is neither
            // listed nor asked, by any fact that names it, so its edit does not reach the body.
            if *self.dropped.get_or_insert(name) == name {
                return;
            }
        }
        match self.handed.binary_search_by_key(&name, |(handed, _)| *handed) {
            Ok(at) => {
                if !made.reads.iter().any(|(read, _)| *read == name) {
                    made.reads.push((name, self.handed[at].1));
                }
            }
            Err(_) => {
                if !self.needs.iter().any(|want| want.name == name) {
                    self.needs.push(blaze_drive::Want { name, home });
                }
            }
        }
    }
}

/// The words of a head no `Signature` names, with what the body's read list holds for it: the
/// crates' `CrateScope` reads (every body's), so a change of its crate's surface still reaches
/// the body; a change of the head alone does not until a `Signature` covers it.
fn unread_words<'n>(why: &str, frame: &'n bstd::mem::Frame) -> &'n str {
    let words = bstd::mem::format_in!(frame, "{why}; its read list holds the crates' `CrateScope` reads for it, and no `Signature`");
    super::held_str(words.as_str(), frame)
}

/// The reads a body names by what it found, of the inputs its task was handed (`inputs`): its
/// heads' `Signature`s and its file's `Surface`, by name, in `jobs` (the round's frame, which
/// outlives the body's job). What [`read_heads`] reads a head at, and work.rs
/// `bodies_syntax` the `Surface`: each at its task's out, never one the body made.
pub(crate) fn handed_reads<'j>(inputs: &[blaze_drive::Input], jobs: &'j bstd::mem::Frame) -> Result<&'j [(Name, ContentId)], &'static str> {
    let named = |input: &&blaze_drive::Input| matches!(input.name.kind, blaze_drive::Kind::Signature | blaze_drive::Kind::Surface);
    let count = inputs.iter().filter(named).count();
    let mut held: blaze_own::Vec<(Name, ContentId), &'j bstd::mem::Frame> = blaze_own::Vec::new_in(jobs);
    held.try_reserve_exact(count).map_err(|_| "the frame of one round's jobs is full: a body's handed `Signature`s are more than it holds")?;
    held.extend(inputs.iter().filter(named).map(|input| (input.name, input.out)));
    held.sort_unstable_by_key(|(name, _)| *name);
    Ok(held.leak())
}

/// Where the entry at `place` (in the owner's own file) lies against `start..end`, the reading
/// body's range in today's text.
fn where_is<'n>(checker: &mut Checker<'n>, place: WantPlace, start: u32, end: u32, frame: &'n bstd::mem::Frame) -> Where {
    let range = match checker.head_file(place.krate, place.file, frame) {
        Ok(Some(file)) => file.outline.items().get(place.item.0 as usize).map(|it| (it.range.start, it.range.end)),
        _ => None,
    };
    match range {
        // The body's own entry is not inside itself: a recursive fn reads its own head.
        Some(range) if range == (start, end) => Where::Own,
        Some((s, e)) if start <= s && e <= end => Where::Inside,
        _ => Where::Elsewhere,
    }
}

/// The wants an entry `at` of `outline` (the checker's own file) makes itself: its item's
/// (`want_at_entry`), or, for an `extern` block, each foreign item's in the block's order.
fn own_wants(checker: &Checker<'_>, outline: &Outline<'_>, at: ItemIndex) -> std::vec::Vec<Want, Global> {
    let mut wants = std::vec::Vec::new();
    if outline.items().get(at.0 as usize).is_some_and(|item| item.kind() == ItemKind::ExternBlock) {
        for (child, _) in outline.children(Some(at)) {
            wants.extend(checker.world.want_at_entry(checker.file, child));
        }
    } else {
        wants.extend(checker.world.want_at_entry(checker.file, at));
    }
    wants
}

/// A head's entry against the reading body ([`where_is`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Where {
    /// The body's own entry: read like any other head (its out holds what its text resolves to).
    Own,
    /// Inside the body: covered by its text.
    Inside,
    Elsewhere,
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
        // The head as a reader finds it: every note below names it (not_checked/items.rs).
        let path = item_path(outline, &outline.items()[at.0 as usize], text, self.frame);
        let path = path.as_str();
        // The owner whose crate makes the entry's heads, else (a macro call, an item a macro
        // rewrote) the first owner: its crate (0) is the one whose expansions are at the entry.
        // An owner whose crate does not hold the file is never chosen (`OwnerCheck::NotInCrate`).
        let chosen = self.owners.iter().position(|owner| owner.checker().is_some_and(|checker| !own_wants(checker, outline, at).is_empty()));
        let chosen = chosen.or_else(|| self.owners.iter().position(|owner| owner.checker().is_some()));
        let mut out = None;
        if let Some(checker) = chosen.and_then(|i| self.owners.get_mut(i)).and_then(super::OwnerCheck::checker_mut) {
            let own = own_wants(checker, outline, at);
            let errors = checker.records.errors().len();
            // The one making of an entry's out (`Checker::entry_out`): its readers are handed it.
            let file = checker.file;
            match checker.entry_out(0, file, at, &own, tokens, edition, self.frame) {
                Ok(made_out) => out = Some(made_out),
                Err(why) => not_checked(&mut made, (0, 0), path, HEAD_NOT_NAMED, why),
            }
            // A written type of the entry's signatures that names nothing: the E0412 is this
            // task's, at the path in the signature; the head was made with the error type there,
            // so each caller is checked against the rest of it and is not blamed.
            for want in own.iter() {
                let misses = checker.world.want_type_misses(*want);
                if misses.is_empty() {
                    continue;
                }
                // The fn's text starts at its own outline entry: a member's is inside the entry.
                let shift = match checker.world.want_place(*want) {
                    Ok(place) if place.krate == 0 && place.file == file => outline.items().get(place.item.0 as usize).zip(outline.items().get(at.0 as usize)).and_then(|(fn_item, entry)| fn_item.range.start.checked_sub(entry.range.start)),
                    _ => None,
                };
                let Some(shift) = shift else {
                    not_checked(&mut made, (0, 0), path, HEAD_NOT_NAMED, "a signature's type names nothing, and where the fn is written in the entry is not known");
                    continue;
                };
                for miss in misses {
                    let range = (shift + miss.at.start, shift + miss.at.end);
                    super::covered::find_once(&mut made, super::TYPES, range, "E0412", "", &miss.words);
                }
            }
            if let Some(error) = checker.records.errors().get(errors) {
                let words = super::record_words(error, checker.readers, checker.packs, self.frame, checker.heap);
                not_checked(&mut made, (0, 0), path, HEAD_RECORDS, words.as_str());
            }
        }
        // A head not made is a finding above, and its out the tokens alone: a record like any
        // other, which each reader reads at that out, so its read list is the same whether the
        // head could be made or not, and in any order the tasks ran.
        made.out = out.unwrap_or_else(|| signature_out(NO_WANT, tokens));
        made
    }
}
