//! The coherence marks of each nominal type asked about in one command, read once
//! (SIG-ONCE-1, port27/hello-1116 TABLES-NOTES "Briefs, runner 3").
//!
//! [`super::buckets::trait_home`] reads two attribute bits of an ADT (`#[fundamental]`,
//! `#[rustc_has_incoherent_inherent_impls]`) by lowering the ADT's whole signature, and it is
//! asked once per (trait, ADT) question: on a hello world 68% of all signature lowering
//! (1,639 of 2,393 samples under `heads::lower`) was the same ADTs lowered again for every trait
//! asked of them. The marks are a function of the item's text, its crate's cfg atoms and its
//! edition, all fixed by its library key within one command, so they are kept here by that key
//! for the command (the `Store` holds the memo) and each ADT is lowered at most once
//! (rule 77: never compute what a key can skip).
//!
//! The memo is only looked up, never iterated: no map order reaches an answer.

use core::alloc::Allocator;

use blaze_hash::HashMap;
use blaze_id::ItemKey as LibKey;

use super::buckets::{Marks, adt_marks};
use super::source::NamesSource;

/// Each nominal type's marks by its library key, as [`adt_marks`] read them (`None` where they
/// could not be read: kept too, so an unreadable type is not lowered again either).
pub struct MarksMemo<A: Allocator> {
    by_key: HashMap<LibKey, Option<Marks>, A>,
    /// How many times the memo lowered a signature: one per key it holds when it does its job
    /// (the counter tests/front2_sig_once.rs reads).
    pub lowered: u64,
}

impl<A: Allocator> MarksMemo<A> {
    /// An empty memo whose table lives in `alloc` (the store's).
    pub fn new_in(alloc: A) -> MarksMemo<A> {
        MarksMemo { by_key: HashMap::new_in(alloc), lowered: 0 }
    }

    /// The marks of the nominal type `lib`: read (one lowering of its signature) the first
    /// time it is asked in this command, then from the memo.
    pub fn get<'n, S: NamesSource<'n>>(&mut self, names: S, lib: LibKey) -> Option<Marks> {
        if let Some(&kept) = self.by_key.get(&lib) {
            return kept;
        }
        let read = adt_marks(names, lib);
        self.lowered += 1;
        self.by_key.insert(lib, read);
        read
    }

    /// How many nominal types the memo holds.
    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    /// Whether the memo holds nothing.
    pub fn is_empty(&self) -> bool {
        self.by_key.len() == 0
    }

    /// Whether `lib`'s marks were read in this command.
    pub fn holds(&self, lib: LibKey) -> bool {
        self.by_key.contains_key(&lib)
    }
}
