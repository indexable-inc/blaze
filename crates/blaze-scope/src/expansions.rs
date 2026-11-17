//! One crate's expansions kept between solves ([`crate::crate_scope::Kept::expansions`]): the
//! earlier solve's set read in place, and what this solve found again or expanded.
//!
//! A crate solved again after an edit expands only the calls the edit moved: a call's key is
//! the ONE key of an expansion, [`blaze_expand::call_key`] (the expander's code, the
//! definition's content and site, the call's tokens as shaped: the lowering keys a body's call
//! by the same function, so one call is one key in both layers), made once the call is lexed
//! and shaped and before it is expanded, found in the earlier set by a binary search, and its
//! read list (the cfg atoms the item walk asked, the expansion's own reads) asked again before
//! the kept expansion is taken. What is
//! taken is the [`blaze_expand::Expansion`]'s record read back: the same value a fresh
//! expansion gives, so everything after it (the render, the outline, the lowering) does not
//! know which it got.
//!
//! The driver's duty is three lines around one crate's solve: [`KeptExpansions::open`] with
//! what [`crate::crate_scope::Kept::expansions`] gave, the value handed to the solve (it
//! reaches `expand_decls` as a field of its `Base`), and [`KeptExpansions::record`] given to
//! [`crate::crate_scope::Kept::expansions_made`] after a solve that finished.
//!
//! Not kept, each counted in [`KeptCounts::unkept`]: a function-like builtin's call (its work
//! IS its reads: files, the environment, the calls nested in its input), and an expansion
//! with a read that cannot be asked again by its subject ([`blaze_expand::Reads::askable`]: a
//! proc macro's sandbox reads). They are expanded again each solve, as before.

#![allow(
    clippy::disallowed_types,
    reason = "a std lock between the fill's threads, as resolve.rs: blaze_core::lock is not a dependency of this sans-IO crate"
)]

use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use blaze_expand::CallKey;
use blaze_expand::kept::{KeptError, KeptSet, SetEntry, SetSource, write_set};
use blaze_ids::producer::{Expander, ProducerId};
use blaze_own::{AVec, Alloc};

/// One of this solve's expansions: found in the earlier set (its index there), or made now
/// (its entry's bytes, [`blaze_expand::kept::write_entry`]'s).
#[derive(Clone, Copy)]
enum Now<'a> {
    Found(usize),
    Made(&'a [u8]),
}

/// What one solve did with its kept expansions.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct KeptCounts {
    /// Calls answered from the earlier set: nothing lexed, nothing run.
    pub found: usize,
    /// Calls expanded and kept.
    pub made: usize,
    /// Calls whose key was in the earlier set but whose read list is answered otherwise today.
    pub moved: usize,
    /// Calls expanded and not kept (the module docs say which).
    pub unkept: usize,
}

/// The planted bug of the tests: a key made without the definition's content (so two derives
/// of one item share a key).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Planted {
    None,
    KeyWithoutTheDefinition,
}

/// One crate's kept expansions for one solve. Shared by the fill's threads: the lock is held
/// for a push, never across an expansion.
pub struct KeptExpansions<'k, 'a> {
    keeps: bool,
    /// The expander whose expansions this set holds (decision D105): every key it files or
    /// finds is `kept_key(expander, call)`. `None` exactly when nothing is kept.
    expander: Option<ProducerId<Expander>>,
    earlier: Option<KeptSet<'k>>,
    now: Mutex<AVec<'a, (CallKey, Now<'a>)>>,
    found: AtomicUsize,
    made: AtomicUsize,
    moved: AtomicUsize,
    unkept: AtomicUsize,
    planted: Planted,
}

impl<'k, 'a> KeptExpansions<'k, 'a> {
    /// Nothing kept and nothing collected: a solve with no [`crate::crate_scope::Kept`], or one
    /// that keeps no expansions. No key is made for any call.
    pub fn none(alloc: Alloc<'a>) -> KeptExpansions<'k, 'a> {
        KeptExpansions {
            keeps: false,
            expander: None,
            earlier: None,
            now: Mutex::new(AVec::new_in(alloc)),
            found: AtomicUsize::new(0),
            made: AtomicUsize::new(0),
            moved: AtomicUsize::new(0),
            unkept: AtomicUsize::new(0),
            planted: Planted::None,
        }
    }

    /// A solve that keeps the expansions `expander` makes, with the crate's `earlier` set when
    /// one was kept (aligned to [`blaze_expand::kept::ALIGN`] bytes; its table is checked here,
    /// once). An earlier set made by another expander finds nothing: its keys hold that one.
    pub fn open(expander: ProducerId<Expander>, earlier: Option<&'k [u8]>, alloc: Alloc<'a>) -> Result<KeptExpansions<'k, 'a>, KeptError> {
        // A set of an earlier layout is detected by its magic and rebuilt by this solve (an old
        // home after an upgrade), never read as this one.
        let earlier = match earlier.map(KeptSet::open) {
            Some(Ok(set)) => Some(set),
            Some(Err(KeptError::OldForm)) | None => None,
            Some(Err(error)) => return Err(error),
        };
        Ok(KeptExpansions { keeps: true, expander: Some(expander), earlier, ..KeptExpansions::none(alloc) })
    }

    /// [`KeptExpansions::open`] with a planted bug, for the test that must catch it.
    pub fn open_planted(
        expander: ProducerId<Expander>,
        earlier: Option<&'k [u8]>,
        planted: Planted,
        alloc: Alloc<'a>,
    ) -> Result<KeptExpansions<'k, 'a>, KeptError> {
        Ok(KeptExpansions { planted, ..KeptExpansions::open(expander, earlier, alloc)? })
    }

    /// The key `call`'s expansion is kept under: the call's key and this set's expander.
    fn kept(&self, call: CallKey) -> CallKey {
        match self.expander {
            Some(expander) => blaze_expand::kept_key(expander, call),
            // `keeps` is false: resolve.rs makes no call key, so nothing reaches here (rule 3:
            // stop, naming it, rather than file a key without its producer).
            None => panic!("a call's kept key was asked of a solve that keeps no expansions (D105, expansions.rs)"),
        }
    }

    /// Whether this solve keeps expansions at all: when not, no call's key is made.
    pub fn keeps(&self) -> bool {
        self.keeps
    }

    pub(crate) fn planted(&self) -> Planted {
        self.planted
    }

    /// What this solve found, made, found moved and did not keep, so far.
    pub fn counts(&self) -> KeptCounts {
        KeptCounts {
            found: self.found.load(Ordering::Relaxed),
            made: self.made.load(Ordering::Relaxed),
            moved: self.moved.load(Ordering::Relaxed),
            unkept: self.unkept.load(Ordering::Relaxed),
        }
    }

    fn push(&self, key: CallKey, now: Now<'a>) {
        let key = self.kept(key);
        match self.now.lock() {
            Ok(mut list) => list.push((key, now)),
            Err(_) => panic!("a fill thread panicked holding the kept expansions lock"),
        }
    }

    /// The earlier solve's entry under `key`, opened, with its index for [`Self::found`].
    pub(crate) fn find(&self, key: &CallKey) -> Result<Option<(usize, SetEntry<'k>)>, KeptError> {
        let Some(set) = self.earlier else { return Ok(None) };
        match set.position(&self.kept(*key)) {
            Some(index) => Ok(Some((index, set.entry(index)?))),
            None => Ok(None),
        }
    }

    /// The entry at `index` of the earlier set answered the call of `key`: it is this solve's
    /// too.
    pub(crate) fn found(&self, key: CallKey, index: usize) {
        self.found.fetch_add(1, Ordering::Relaxed);
        self.push(key, Now::Found(index));
    }

    /// The call of `key` was expanded: `entry` is its bytes.
    pub(crate) fn made(&self, key: CallKey, entry: &'a [u8]) {
        self.made.fetch_add(1, Ordering::Relaxed);
        self.push(key, Now::Made(entry));
    }

    /// An entry under a call's key was not taken: its read list is answered otherwise today.
    pub(crate) fn moved(&self) {
        self.moved.fetch_add(1, Ordering::Relaxed);
    }

    /// A call was expanded and is not kept.
    pub(crate) fn unkept(&self) {
        self.unkept.fetch_add(1, Ordering::Relaxed);
    }

    /// This solve's set, in `alloc`: every expansion it found again or made, sorted by key, so
    /// the bytes are the same whatever order the fill's threads expanded in (rule 25). Two
    /// expansions under one key with different bytes are refused ([`KeptError::TwoValues`]).
    pub fn record<'b>(&self, alloc: Alloc<'b>) -> Result<AVec<'b, u8>, KeptError> {
        let list = match self.now.lock() {
            Ok(list) => list,
            Err(_) => panic!("a fill thread panicked holding the kept expansions lock"),
        };
        let mut entries: AVec<'b, (CallKey, SetSource<'_>)> = AVec::with_capacity_in(list.len(), alloc);
        for &(key, now) in list.iter() {
            let source = match (now, self.earlier) {
                (Now::Made(bytes), _) => SetSource::Entry(bytes),
                (Now::Found(index), Some(set)) => SetSource::Kept(set.entry(index)?),
                // `found` is told only an index `find` gave, of the earlier set.
                (Now::Found(index), None) => panic!("a kept expansion found at index {index} of no earlier set"),
            };
            entries.push((key, source));
        }
        write_set(&mut entries, alloc)
    }
}
