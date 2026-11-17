//! One home for a placed output's bytes. An output of [`crate::CLONE_FROM`] bytes or more is
//! placed by cloning the whole file of it the store keeps beside its packs (`placed/<content>`),
//! and until 2026-11-12 its record kept its bytes in a pack as well: every such output was on
//! disk twice (hello world's home: 917 MB against makepad's Relax's 72 MB). Once the file is
//! durable it is made the record's one home: the record is written again naming the file
//! ([`Form::External`], no bytes stored, the head listing the file as placed,
//! [`disk::ExternalRef::placed`]), and the pack's copy is dead bytes the collector gives back.
//!
//! - **Durable first.** The file is written with no sync under its writing name, cloned from
//!   there once written back, and given its content's name only once a sync of it completed
//!   ([`Store::name_staged`]), by a rename that is not made durable: a crash leaves the name
//!   holding the content, or no name. Nothing names it as a home before the file's sync and
//!   an [`Effect::SyncDir`] of `placed/` after its rename have both answered
//!   ([`Store::home_synced`]); then it is mapped, and only then are records moved to it, so a
//!   reader of a moved record always finds its mapping ([`Store::home_due`]). A crash before
//!   the commit that writes the moved records leaves the pack's copy, which the old head
//!   names; after it, the file, synced before.
//! - **Off the answer's path.** A place answers as it does without this (its bytes written
//!   where they go, or the file cloned); the syncs are asked only once the file is there under
//!   its name ([`crate::PlacedFile::There`]) and no place made of it is in flight, and the
//!   records move on a later
//!   completion. No answer waits for a sync: the span `placed file made its value's home`
//!   says what the syncs, the mapping and the move took, apart from every request.
//! - **The record goes by the forget, never the file alone.** A home is not let go when no path
//!   names it (`Store::names_step` skips a file of its own, and asks no forget of it: D45); its
//!   record is forgotten by the collector, or once asked ([`crate::forget`]) and nothing roots
//!   it, and once the head that lists it removed is durable the file
//!   is a placed file again, removed by the placed files' own steps when no path names it
//!   ([`Store::let_go`]). The open's sweep of `placed/` keeps a home its head names.
//! - **A record not yet in a pack** (its put not yet committed) moves once a commit has drained
//!   it: [`Store::homes_due`] after each completion while any waits, bounded by the commits that
//!   drain them; one whose key changed value or went meanwhile is let go.
//!
//! The invariant (`tests::place_crash`, its oracle `homes`, a crash at every step, and
//! `tests::home`): a record never names a file that is not durable; after the syncs and a
//! commit, the value's bytes are on disk once (the pack's census holds no bytes of it); after
//! any crash the pack's copy or the file reads the value, never neither, and the store opens.
//! Controls: [`crate::PlacePlant::HomesBeforeSync`] (the records moved before the syncs: a
//! crash tears the file a head names) and [`crate::PlacePlant::HomesNothing`] (the pack keeps
//! the bytes).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use blaze_core::{Done, Effect, Extent, Mapping};
use blaze_ids::ContentId;

use crate::{
    External, Key, Kind, Op, PlacedFile, Store, describe,
    disk::{Form, Loc},
    read::Place,
};

/// A placed file being made its value's home ([`Store::homing`]).
pub(crate) struct Homing {
    /// The keys placed from it, whose records move to it once it is durable and mapped.
    keys: Vec<Key>, // alloc: the keys a placed file was placed of, a few
    step: Step,
}

enum Step {
    /// Waits for the file to be there and written back ([`PlacedFile::There`]).
    Wanted,
    /// The file's and `placed/`'s syncs asked as `round`: `left` still in flight, `failed`
    /// the first that failed. `span`, the home's, open until its records moved or it gave up.
    Syncing { round: u64, left: u8, failed: Option<String>, span: tracing::Span },
    /// Synced; its mapping asked as `round`.
    Mapping { round: u64, span: tracing::Span },
    /// Mapped: its records move once each is in a pack and no commit is in flight.
    Mapped { mapping: Mapping, span: tracing::Span },
}

impl Store {
    /// `key`, placed from the whole file of `content`, is to have its record moved to that
    /// file once the file is durable ([`crate::home`]). A file already a home: the record
    /// moves now, or once it is in a pack.
    pub(crate) fn home_wanted(&mut self, content: ContentId, key: Key, effects: &mut Vec<Effect>) {
        if self.derived.contains_key(&content) {
            return;
        }
        // The planted bug of the control: no record ever moves, the pack keeps the bytes.
        #[cfg(test)]
        if self.plant == crate::PlacePlant::HomesNothing {
            return;
        }
        let homed = match self.tiers[0].externals.get(&content) {
            // A value that is no output with the same bytes, kept as `file-<content>`, or a
            // home going: nothing moves to it.
            Some(external) if !external.placed || external.dropped => return,
            // clone: a count of the mapping, which the moves read the file's length of
            Some(external) => Some(external.mapping.clone()),
            None => None,
        };
        let homing = self.homing.entry(content).or_insert_with(|| Homing {
            // alloc: the keys of one placed file, a few
            keys: Vec::new(),
            step: match homed {
                Some(Some(mapping)) => Step::Mapped { mapping, span: home_span(&content) },
                _ => Step::Wanted,
            },
        });
        if !homing.keys.contains(&key) {
            homing.keys.push(key);
        }
        match matches!(homing.step, Step::Mapped { .. }) {
            true => self.home_due(content, effects),
            false => self.home_ready(content, effects),
        }
    }

    /// `key` placed from the whole file of `content`, as [`Store::home_wanted`], with nothing
    /// asked yet: what [`Store::home_ready`] then moves.
    pub(crate) fn home_key(&mut self, content: ContentId, key: Key) {
        if let Some(homing) = self.homing.get_mut(&content)
            && !homing.keys.contains(&key)
        {
            homing.keys.push(key);
        }
    }

    /// The whole file of `content` is there under its content's name: the syncs that make it
    /// durable are asked, where a home of it is wanted and none is begun, once no place made
    /// of it is in flight, so no answer comes after a sync asked for its file (asked again as
    /// each such place ends, [`Store::names_step`]). Nothing waits for them. Both are asked
    /// though the file's bytes were synced before it was named ([`Store::name_staged`]): the
    /// rename was not, and a name is durable only once the file under it and its directory
    /// are synced (until 2026-11-13 the file's was skipped, and a crash after the head could
    /// undo the rename, leaving the head naming a file not there).
    pub(crate) fn home_ready(&mut self, content: ContentId, effects: &mut Vec<Effect>) {
        if !self.home_file_there(&content)
            || self.staged_files.contains(&content)
            || self.names.in_flight(&content) > 0
        {
            return;
        }
        let Some(homing) = self.homing.get_mut(&content) else { return };
        if !matches!(homing.step, Step::Wanted) || homing.keys.is_empty() {
            return;
        }
        let round = self.home_rounds;
        self.home_rounds += 1;
        let span = home_span(&content);
        // The planted bug: its records moved before anything of it is durable.
        #[cfg(test)]
        if self.plant == crate::PlacePlant::HomesBeforeSync {
            homing.step = Step::Mapping { round, span };
            return self.ask_home_map(content, round, effects);
        }
        homing.step = Step::Syncing { round, left: 2, failed: None, span };
        // The file under its content's name, after the rename that gave it (its bytes were
        // synced before, [`Store::name_staged`], so this writes no data): with the sync of
        // `placed/`, what makes the name durable. The rename was asked with no sync, so until
        // both answered a crash may leave the file under its writing name and the content's
        // name absent, never present with other bytes; a head names it only after.
        let file = self.placed_file(&content);
        let tag = self.tags.ask(Op::HomeSynced { content, round });
        // durable: the file becomes the one home of its value's bytes, which a head will name
        effects.push(Effect::Sync { file, tag });
        let tag = self.tags.ask(Op::HomeSynced { content, round });
        // durable: the file's name, which its rename made with no sync
        effects.push(Effect::SyncDir { dir: self.placed_dir(), tag });
    }

    /// A sync of the whole file of `content` or of `placed/` answered ([`Store::home_ready`]).
    /// Once both have, the file is mapped; one that failed leaves the bytes where they are,
    /// in the pack and the file both, said in the problems.
    pub(crate) fn home_synced(
        &mut self,
        content: ContentId,
        round: u64,
        done: &Done,
        effects: &mut Vec<Effect>,
    ) {
        let Some(homing) = self.homing.get_mut(&content) else { return };
        let Step::Syncing { round: asked, left, failed, .. } = &mut homing.step else { return };
        if *asked != round {
            return;
        }
        *left -= 1;
        if !matches!(done, Done::Synced) && failed.is_none() {
            *failed = Some(describe(done));
        }
        if *left > 0 {
            return;
        }
        let failed = failed.take();
        let file = self.placed_file(&content);
        if let Some(why) = failed {
            self.homing.remove(&content);
            // alloc: a problem reported
            return self.placed_problem(format!(
                "syncing {file} to make it the one home of its value's bytes: {why}. Its value is kept in its pack as well; the next place of it asks again"
            ));
        }
        // Let go of or removed meanwhile: nothing moves to it.
        if !self.home_file_there(&content) {
            self.homing.remove(&content);
            return;
        }
        let homing = self.homing.get_mut(&content).expect("found above");
        let Step::Syncing { span, .. } = std::mem::replace(&mut homing.step, Step::Wanted) else {
            unreachable!("matched above")
        };
        homing.step = Step::Mapping { round, span };
        self.ask_home_map(content, round, effects);
    }

    fn ask_home_map(&mut self, content: ContentId, round: u64, effects: &mut Vec<Effect>) {
        let file = self.placed_file(&content);
        let tag = self.tags.ask(Op::HomeMapped { content, round });
        effects.push(Effect::Map { file, extent: Extent::Whole, tag });
    }

    /// The whole file of `content`, synced, mapped: its records move to it now, or as each
    /// is in a pack ([`Store::home_due`]).
    pub(crate) fn home_mapped(
        &mut self,
        content: ContentId,
        round: u64,
        done: Done,
        effects: &mut Vec<Effect>,
    ) {
        let Some(homing) = self.homing.get_mut(&content) else { return };
        let Step::Mapping { round: asked, .. } = &homing.step else { return };
        if *asked != round {
            return;
        }
        let mapping = match done {
            Done::Mapped(mapping) if self.home_file_there(&content) => mapping,
            Done::Mapped(_) => {
                self.homing.remove(&content);
                return;
            }
            done => {
                self.homing.remove(&content);
                let file = self.placed_file(&content);
                // alloc: a problem reported
                return self.placed_problem(format!(
                    "mapping {file}, synced to be the one home of its value's bytes: {}. Its value is kept in its pack as well",
                    describe(&done)
                ));
            }
        };
        let homing = self.homing.get_mut(&content).expect("found above");
        let Step::Mapping { span, .. } = std::mem::replace(&mut homing.step, Step::Wanted) else {
            unreachable!("matched above")
        };
        homing.step = Step::Mapped { mapping, span };
        self.home_due(content, effects);
    }

    /// Every home mapped with records still to move ([`Store::homes_waiting`]).
    pub(crate) fn homes_due(&mut self, effects: &mut Vec<Effect>) {
        self.homes_waiting = false;
        let due: Vec<ContentId> = self
            .homing
            .iter()
            .filter(|(_, it)| matches!(it.step, Step::Mapped { .. }))
            .map(|(content, _)| *content)
            // alloc: the homes waiting, a few, once per completion while any waits
            .collect();
        for content in due {
            self.home_due(content, effects);
        }
    }

    /// The records of the keys placed from `content`'s mapped home that are in a pack move
    /// to it: [`Store::home_placed`].
    pub(crate) fn home_due(&mut self, content: ContentId, effects: &mut Vec<Effect>) {
        // As a settle: no move while a commit is in flight or an index swap is not done (its
        // moves and rows are a commit's), asked again after the completion that ends it.
        if self.commit.is_some() || self.index_mapping || self.migrating.is_some() || self.broken.is_some() {
            self.homes_waiting |= self.homing.contains_key(&content);
            return;
        }
        let Some(homing) = self.homing.get(&content) else { return };
        let Step::Mapped { mapping, .. } = &homing.step else { return };
        let len = mapping.bytes().len() as u64;
        let homed = self.tiers[0].externals.contains_key(&content);
        // A home not yet any record's is let go of when its file is: nothing moves to a file
        // going. Once one record is there, the file stays while the record does.
        if !homed && !self.home_file_there(&content) {
            self.homing.remove(&content);
            return;
        }
        let look = self.look();
        // alloc: the records that move now, and the keys that wait, a few
        let mut moving: Vec<(Key, Loc)> = Vec::new();
        // alloc: the keys still in a segment, a few
        let mut waiting: Vec<Key> = Vec::new();
        for key in &homing.keys {
            let Some(Ok(found)) = look.find(key) else { continue };
            if found.tier != 0 {
                continue;
            }
            match found.place {
                Place::Pack(loc) if loc.form != Form::External && loc.kind == Kind::Output && loc.raw == len => {
                    if look.record(found, key).is_ok_and(|(header, _)| header.content == content) {
                        moving.push((*key, loc));
                    }
                }
                // Its put is not yet committed: it moves once a commit drained it.
                Place::Segment(seg) if seg.form != Form::External && seg.kind == Kind::Output => {
                    waiting.push(*key)
                }
                _ => {}
            }
        }
        if !moving.is_empty() {
            self.home_placed(content, len, &moving, effects);
        }
        match waiting.is_empty() {
            true => {
                if let Some(Homing { step: Step::Mapped { span, .. }, .. }) = self.homing.remove(&content) {
                    span.record("moved", moving.len());
                }
            }
            false => {
                let homing = self.homing.get_mut(&content).expect("found above");
                homing.keys = waiting;
                self.homes_waiting = true;
            }
        }
    }

    /// The records of `moving` (of `content`, `len` bytes each, in packs) written again
    /// naming the whole file of `content` as their one home, which is synced and mapped: the
    /// file is listed as the tier's, placed ([`External::placed`]), and published before any
    /// record names it, so a reader of a moved record finds its mapping; each record's old
    /// bytes are dead ([`Store::replace`], as a settle's move counts them). A commit is
    /// asked: until it is durable a crash leaves the old records, whose bytes the old head
    /// names.
    fn home_placed(&mut self, content: ContentId, len: u64, moving: &[(Key, Loc)], effects: &mut Vec<Effect>) {
        let Some(Homing { step: Step::Mapped { mapping, .. }, .. }) = self.homing.get(&content) else {
            unreachable!("a home moves records only once it is mapped")
        };
        // clone: a count of the mapping, which the tier's list of files holds as it is read
        let mapping = mapping.clone();
        let file = self.placed_file(&content);
        let own = &mut self.tiers[0];
        let external = own.externals.entry(content).or_insert_with(|| External {
            len,
            file,
            placed: true,
            mapping: Some(mapping),
            memory: None,
            committed: true,
            keys: 0,
            dropped: false,
        });
        external.keys = external.keys.saturating_add(moving.len() as u32);
        external.dropped = false;
        self.publish_view();
        for &(key, loc) in moving {
            self.replace(key, loc, Form::External, 0, content, &[], false);
        }
        self.flush(effects);
    }

    /// Whether the whole file of `content` is there, written back, and not let go of: what a
    /// home is made of.
    fn home_file_there(&self, content: &ContentId) -> bool {
        matches!(self.files.get(content), Some(PlacedFile::There { doomed: false, lost: false, .. }))
    }
}

/// The span of one home, from its syncs' ask to its records' move: no parent, since it ends
/// on a later completion under whatever request is current then, and none waits for it.
fn home_span(content: &ContentId) -> tracing::Span {
    tracing::info_span!(
        target: "blaze",
        parent: None,
        "placed file made its value's home",
        content = %format_args!("{content:x}"),
        moved = tracing::field::Empty,
    )
}
