//! The session memo: what a process keeps of records between asks, as a static policy of the
//! driver (`Driver<M: Memo>`).
//!
//! - [`RecordsOnly`]: every ask goes to the store; nothing outlives the request. One-shot
//!   commands (`build`, `check`, `test`, `run`) use it and exit.
//! - [`Resident`]: a table per crate arena kept by a process that stays alive (`watch`,
//!   `run --live`), filled from and backed by the same records, bounded, safe to drop at any
//!   time. This is the reference form the simulation holds the driver to; blaze-memo builds the
//!   production one (entries pointing into mapped segments, an arena a bump region freed at
//!   once) against the same trait and the same refinement test.
//!
//! Dispatch is static: [`with_policy`] matches the policy once per process and runs the work
//! monomorphized for it. The cost: the driver and everything generic over `M` is in the binary
//! twice; per ask there is no dynamic call and no flag (for `RecordsOnly`, `get` is a constant
//! `None` the optimizer folds away, and `keep` is empty).
//!
//! The soundness invariant: nothing holds a record's value by any route other than the memo,
//! and the memo answers only by the full content key, so an entry can never outlive its inputs
//! (a changed input is a different key). The lint that enforces it (blaze-lints, to write): no
//! field of a type that outlives one request may hold a `Kind::Root` or bytes read from the
//! store, outside a `Memo` implementation.

#![blaze::sans_io]

use std::alloc::Global;

use blaze_format::Aligned;
use blaze_ids::ContentId;

use crate::{
    Plant,
    kind::{Keep, Key, Kind, Tag, UnitManifest},
    planted,
    proof::Holds,
};

/// A crate's arena: the unit index of the request's plan.
pub type Arena = u32;

/// A session memo.
///
/// The invariant, a refinement: a driver over any `Memo` gives the same answers, runs the same
/// tasks and writes the same records as one over [`RecordsOnly`], for any sequence of requests
/// and edits (tests.rs, `resident_refines_records_only`).
pub trait Memo {
    const NAME: &'static str;

    /// The record of kind `K` under `key`, 16-aligned, if this memo holds it.
    fn get<K: Kind>(&self, arena: Arena, key: &Key<K>) -> Option<&[u8]>;

    /// Offers `record`, proved in the asking request (`holds`: read under its key, verified, or
    /// written), so nothing unproved is kept. What `get` answers in a later request is checked
    /// once there, by its key ([`crate::proof::hit`]).
    fn keep<K: Kind>(&mut self, arena: Arena, holds: &Holds<'_, K>, record: &[u8]);

    /// Drops everything of `arena`, wholesale: a crate whose files changed, or a dependent whose
    /// imported digests changed. Always safe.
    fn drop_arena(&mut self, arena: Arena);

    fn end_request(&mut self);

    fn resident_bytes(&self) -> u64;
}

/// Every ask goes to the store.
#[derive(Clone, Copy, Debug, Default)]
pub struct RecordsOnly;

impl Memo for RecordsOnly {
    const NAME: &'static str = "records-only";

    #[inline(always)]
    fn get<K: Kind>(&self, _arena: Arena, _key: &Key<K>) -> Option<&[u8]> {
        None
    }

    #[inline(always)]
    fn keep<K: Kind>(&mut self, _arena: Arena, _holds: &Holds<'_, K>, _record: &[u8]) {}

    #[inline(always)]
    fn drop_arena(&mut self, _arena: Arena) {}

    #[inline(always)]
    fn end_request(&mut self) {}

    fn resident_bytes(&self) -> u64 {
        0
    }
}

struct Table {
    /// Sorted by (tag, key).
    entries: Vec<(Tag, ContentId, Aligned), Global>,
    bytes: u64,
    /// The planted bug's route: the last manifest kept, by arena rather than by key.
    last_manifest: Option<usize>,
}

/// A bounded table per arena.
pub struct Resident {
    arenas: Vec<Table, Global>,
    cap_bytes: u64,
    bytes: u64,
}

impl Resident {
    pub fn new(cap_bytes: u64) -> Resident {
        Resident { arenas: Vec::new_in(Global), cap_bytes, bytes: 0 }
    }

    fn table(&mut self, arena: Arena) -> &mut Table {
        let at = arena as usize;
        while self.arenas.len() <= at {
            self.arenas.push(Table { entries: Vec::new_in(Global), bytes: 0, last_manifest: None });
        }
        &mut self.arenas[at]
    }
}

impl Memo for Resident {
    const NAME: &'static str = "resident";

    fn get<K: Kind>(&self, arena: Arena, key: &Key<K>) -> Option<&[u8]> {
        let table = self.arenas.get(arena as usize)?;
        let wanted = (K::TAG, key.id());
        match table.entries.binary_search_by(|(tag, id, _)| (*tag, *id).cmp(&wanted)) {
            Ok(at) => Some(&*table.entries[at].2),
            Err(_) if planted(Plant::StaleResident) && K::TAG == UnitManifest::TAG => {
                table.last_manifest.map(|at| &*table.entries[at].2)
            }
            Err(_) => None,
        }
    }

    fn keep<K: Kind>(&mut self, arena: Arena, holds: &Holds<'_, K>, record: &[u8]) {
        let key = holds.key();
        if K::KEEP == Keep::Disk {
            return;
        }
        let len = record.len() as u64;
        if self.bytes + len > self.cap_bytes {
            // Over the bound: every arena goes, wholesale; they refill from the store.
            self.arenas.clear();
            self.bytes = 0;
            if len > self.cap_bytes {
                return;
            }
        }
        let table = self.table(arena);
        let wanted = (K::TAG, key.id());
        let Err(at) = table.entries.binary_search_by(|(tag, id, _)| (*tag, *id).cmp(&wanted)) else {
            return;
        };
        table.entries.insert(at, (K::TAG, key.id(), Aligned::copy_from(record)));
        if let Some(last) = table.last_manifest.as_mut().filter(|last| **last >= at) {
            *last += 1;
        }
        if K::TAG == UnitManifest::TAG {
            table.last_manifest = Some(at);
        }
        table.bytes += len;
        self.bytes += len;
    }

    fn drop_arena(&mut self, arena: Arena) {
        if let Some(table) = self.arenas.get_mut(arena as usize) {
            self.bytes -= table.bytes;
            table.entries.clear();
            table.bytes = 0;
            table.last_manifest = None;
        }
    }

    fn end_request(&mut self) {}

    fn resident_bytes(&self) -> u64 {
        self.bytes
    }
}

/// Which memo a process runs with, from the command: one-shot commands are `RecordsOnly`,
/// `watch` and live runs `Resident`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Policy {
    RecordsOnly,
    Resident { cap_bytes: u64 },
}

/// Work generic over the memo, run once per process with the memo its policy names.
pub trait WithMemo {
    type Out;
    fn run<M: Memo>(self, memo: M) -> Self::Out;
}

/// The one place the policy is looked at.
pub fn with_policy<W: WithMemo>(policy: Policy, work: W) -> W::Out {
    match policy {
        Policy::RecordsOnly => work.run(RecordsOnly),
        Policy::Resident { cap_bytes } => work.run(Resident::new(cap_bytes)),
    }
}
