//! A record forgotten alone: the smallest primitive the store had not got (rule 32). The
//! collector forgets a record only with its whole pack ([`crate::collect`]); a value whose
//! bytes are a file of their own ([`Form::External`], `file-<content>`), or a placed output
//! whose file is its one home, is worth forgetting the moment nothing needs it, and its pack
//! holds no more than its header. So one record is forgotten here, its pack kept.
//!
//! - **Asked, then due.** [`Store::forget`] queues a key (the place that held it was
//!   replaced, [`crate::whole`]); [`Store::forget_due`], which the driver calls between
//!   builds once a scan was handed back (a build never finds a key and then misses it),
//!   forgets each queued record no root names, by the collector's own rule asked of one
//!   record ([`crate::collect::Collecting::unrooted`]): no pin, no path placed from it, no
//!   place in flight of it, no record that names it (a set's listing, a chunked value, a
//!   derived value's base), no reader's keep, under a mark of a scan taken after the last
//!   root was added. A key a root names is let go of the queue: it is not forgotten.
//! - **In memory at once, on disk with the next head.** The record's offset goes into its
//!   pack's list ([`crate::Pack`]`::forgotten`, shared with the views): a slot or a spot that
//!   points at it finds nothing ([`crate::read::TierView::gone`]), its bytes are counted dead
//!   (a settle or the collector retires the pack in time), and a whole file it named loses a
//!   key ([`Store::external_replaced`]). The next head lists it ([`disk::ForgottenRef`],
//!   format 7) with the file as removed; the file goes only once that head is durable. So a
//!   crash leaves the head before (the record is there, with its file) or after (the record
//!   is no key's, the file is removed at the open): never a record whose bytes are gone.
//! - **A put after.** A put of a forgotten key writes the value again: [`crate::read::Look::kept`]
//!   finds nothing, the claim keeps its record in its segment though the delta's spot is the
//!   forgotten one (`segment::Own::claim`), and the drain makes it the key's home
//!   (`Store::record_home`), the one place the store swaps a spot.
//!
//! The invariant (`tests::collect`, the collector's simulation with forgets, a crash at every
//! step and failed effects): a forgotten record is never served; a record a pin, a place or a
//! set names is never forgotten; after a crash the store opens to the state before the forget
//! or after it, never between (every record found reads its bytes whole). Controls:
//! [`crate::collect::Plant::ForgetIgnoresRoots`] and [`crate::collect::Plant::ForgetUnlisted`].

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::sync::Arc;

use crate::{
    Key, Store,
    disk::{self, Form},
    read::Place,
};

/// What one [`Store::forget_due`] did.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Forgot {
    /// The keys whose records were forgotten.
    pub forgotten: Vec<Key>,
    /// The keys let go of the queue unforgotten, each with why: a root names it, or there is
    /// nothing of it in this store.
    pub kept: Vec<(Key, &'static str)>,
    /// How many stay queued: no mark holds yet, or the record is still in a writer's segment
    /// or its file not yet committed. Asked again at the next [`Store::forget_due`].
    pub waiting: usize,
}

/// What became of one queued key.
enum Outcome {
    Forgotten,
    Kept(&'static str),
    Waits,
}

impl Store {
    /// Asks for the record under `key` to be forgotten alone once nothing roots it
    /// ([`Store::forget_due`]). Asked twice, queued once.
    pub fn forget(&mut self, key: Key) {
        if !self.forgets.contains(&key) {
            self.forgets.push(key);
        }
    }

    /// How many keys wait to be forgotten: a driver that sees some after a build runs a scan
    /// ([`Store::pins_take`]) and then [`Store::forget_due`].
    pub fn forgets_waiting(&self) -> usize {
        self.forgets.len()
    }

    /// Forgets each queued record no root names (the module's rule), between builds: what was
    /// forgotten is in memory at once and in the next head ([`Store::flush`] writes one though
    /// nothing else changed). Bounded by the queue: one look a key.
    pub fn forget_due(&mut self) -> Forgot {
        self.tidy();
        let mut out = Forgot::default();
        if self.broken.is_some() || !self.tiers[0].opened || !self.collecting.marked() {
            out.waiting = self.forgets.len();
            return out;
        }
        let queued = std::mem::take(&mut self.forgets);
        for key in queued {
            match self.forget_one(&key) {
                Outcome::Forgotten => out.forgotten.push(key),
                Outcome::Kept(why) => out.kept.push((key, why)),
                Outcome::Waits => self.forgets.push(key),
            }
        }
        out.waiting = self.forgets.len();
        if !out.forgotten.is_empty() {
            self.forgot = true;
            self.publish_view();
        }
        out
    }

    fn forget_one(&mut self, key: &Key) -> Outcome {
        if self.names.holds(key) {
            return Outcome::Kept("a path is placed from it");
        }
        if self.naming.values().any(|it| it.key == Some(*key)) {
            return Outcome::Kept("a place of it is in flight");
        }
        if !self.collecting.unrooted(key, self.shared.kept_changes()) {
            return Outcome::Kept("a pin, a set, a record that names it or a reader's keep roots it");
        }
        self.forget_record(key)
    }

    /// `blaze store verify`'s repair, with no daemon on the store: every own record whose
    /// bytes are not what it records ([`Store::verify`]) forgotten now, whatever roots it,
    /// since no root is served by bytes that are wrong. A placed output whose whole file is
    /// its one home ([`crate::home`]) and was changed outside the daemon (or by a disk that
    /// damages files) is such a record: its file was its only copy, so the record is
    /// forgotten rather than left naming bytes that are gone, and the next build makes the
    /// output again. Its file goes with the head that lists it removed, as a forget's does.
    /// In memory at once; on disk with the next head ([`Store::flush`]). The corrupt values
    /// that are no record of this store's (an index, a dictionary, a layer's) are kept, with
    /// why; `waiting` counts those still in a writer's segment, which are not queued.
    pub fn forget_corrupt(&mut self) -> Forgot {
        self.tidy();
        let mut out = Forgot::default();
        if self.broken.is_some() || !self.tiers[0].opened {
            return out;
        }
        let nothing = Key(blaze_ids::ContentId::default());
        for corrupt in self.verify(None) {
            if corrupt.key == nothing || out.forgotten.contains(&corrupt.key) {
                continue;
            }
            // The file of its own its bytes were read from, if any: no record may read it again.
            let external = {
                let look = self.look();
                match look.find(&corrupt.key) {
                    Some(Ok(found)) if found.tier == 0 && found.place.form() == Form::External => {
                        look.record(found, &corrupt.key).ok().map(|(header, _)| header.content)
                    }
                    _ => None,
                }
            };
            match self.forget_record(&corrupt.key) {
                Outcome::Forgotten => {
                    out.forgotten.push(corrupt.key);
                    // Dropped whatever count of keys the store has of it (one opened has none
                    // yet, `External::keys`): a put of the same content after this must write
                    // its bytes again, never take back a file whose bytes are wrong (until
                    // 2026-11-13 it took the dropped file back, mapping and all). The head
                    // after lists it removed, and the file goes with it.
                    if let Some(content) = external
                        && let Some(it) = self.tiers[0].externals.get_mut(&content)
                        && it.committed
                    {
                        it.keys = 0;
                        it.dropped = true;
                    }
                }
                Outcome::Kept(why) => out.kept.push((corrupt.key, why)),
                Outcome::Waits => out.waiting += 1,
            }
        }
        if !out.forgotten.is_empty() {
            self.forgot = true;
            self.publish_view();
        }
        out
    }

    /// The record under `key` forgotten alone, its pack kept (the module's second point): what
    /// [`Store::forget_due`] does to a record no root names, and [`Store::forget_corrupt`] to
    /// one whose bytes are wrong.
    fn forget_record(&mut self, key: &Key) -> Outcome {
        let look = self.look();
        let found = match look.find(key) {
            None => return Outcome::Kept("the store holds nothing of it"),
            Some(Err(_)) => return Outcome::Kept("its slot is not whole: it is no record to forget"),
            Some(Ok(found)) => found,
        };
        if found.tier != 0 {
            return Outcome::Kept("its record is another store's (a layer's)");
        }
        let loc = match found.place {
            Place::Pack(loc) => loc,
            // Drained into a pack at the next commit.
            Place::Segment(_) => return Outcome::Waits,
        };
        let content = match look.record(found, key) {
            Ok((header, _)) => header.content,
            Err(_) => return Outcome::Kept("its record does not read: it is no record to forget"),
        };
        let own = &mut self.tiers[0];
        if loc.form == Form::External && !own.externals.get(&content).is_some_and(|it| it.committed) {
            // Its file is written by the next commit; forgotten after it.
            return Outcome::Waits;
        }
        let Some(pack) = own.packs.get_mut(loc.pack as usize) else {
            return Outcome::Kept("its pack is not the store's");
        };
        if pack.removed || pack.retiring {
            return Outcome::Kept("its pack is retired already");
        }
        let Some(span) = disk::span_of(loc.stored) else {
            return Outcome::Kept("its length is no record's");
        };
        let offsets = Arc::make_mut(&mut pack.forgotten);
        match offsets.binary_search(&loc.offset) {
            Ok(_) => return Outcome::Kept("it is forgotten already"),
            // alloc: once per record forgotten, into its pack's list
            Err(at) => offsets.insert(at, loc.offset),
        }
        pack.dead += span;
        if loc.form == Form::External {
            self.external_replaced(&content);
        }
        Outcome::Forgotten
    }
}
