//! Choosing for every test of a suite after each edit in time proportional to what the edit
//! changed (docs/v2/plan/lanes/fastcompile/select.md, "Speed of light").
//!
//! [`choose`](crate::select::choose) decides one test from the store alone: it finds the end
//! of the test's history, reads its entries and, for each, the set it was kept under, and
//! looks every function of the set up in the binary. Over a suite, that is every function of
//! every test's set after any edit. A [`Selector`] keeps what those reads found, for one suite
//! and the binary it was last given:
//!
//! - every set it has read, by a dense id, with how many of its versions are not in the binary
//!   (`stale`): a set fits the binary when none are and its unlogged hash is the binary's;
//! - every version it has met, by a dense id, with the sets that hold it (postings, in blocks
//!   of one cache line), and every function with its bound code in the binary;
//! - each test's history, newest first, as far back as a walk has read it.
//!
//! [`Selector::advance`] to a new binary compares each known function's bound code with the
//! new binary's. For each one that changed, it moves the stale counts of the sets that hold its
//! old and its new version: the work is the changed functions times their postings, as in an
//! inverted index. A choice is the same walk as `choose`, over the memo, with a set's fit read
//! in O(1). The store is asked only what the selector does not remember: whether a pass is
//! kept for the inputs as they are now.
//!
//! A selector gives what `choose` gives over the same store, as long as it is the only writer
//! of its suite's history ([`Selector::keep`]) and the store keeps what it read. The
//! simulation checks this choice by choice.

use std::time::Duration;

use blaze_cas::{Corrupt, Key};
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_usercode::{
    RunKey,
    index::Shape,
    test::{
        Input, Passes,
        select::{SelectError, changed_input},
    },
    trace::Inputs,
};

use crate::{
    select::{
        Basis, Cached, Choice, Kept, LOOKBACK, Ran, RecordError, Subject, Unread, Why, end, entry,
        find, identity, keep_set, kept_set,
    },
    table::{Bound, FnId, Image, SetView, Version},
};

/// No id: an empty place, the end of a chain.
const NONE: u32 = u32::MAX;

/// Set ids per block of postings: with the next block's index, one 64-byte line.
const PER_BLOCK: usize = 15;

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct Block {
    sets: [u32; PER_BLOCK],
    next: u32,
}

const _: () = assert!(size_of::<Block>() == 64);

/// A version's postings: the sets that hold it, in blocks.
#[derive(Clone, Copy)]
struct Chain {
    first: u32,
    last: u32,
    len: u32,
}

const EMPTY: Chain = Chain { first: NONE, last: NONE, len: 0 };

fn post(blocks: &mut Vec<Block>, chain: &mut Chain, set: u32) {
    let at = chain.len as usize % PER_BLOCK;
    if at == 0 {
        let block = blocks.len() as u32;
        // alloc: growth, a block per fifteen sets of a version
        blocks.push(Block { sets: [0; PER_BLOCK], next: NONE });
        match chain.len {
            0 => chain.first = block,
            _ => blocks[chain.last as usize].next = block,
        }
        chain.last = block;
    }
    blocks[chain.last as usize].sets[at] = set;
    chain.len += 1;
}

fn walk(blocks: &[Block], chain: Chain, mut each: impl FnMut(u32)) {
    let (mut block, mut left) = (chain.first, chain.len as usize);
    while left > 0 {
        let here = &blocks[block as usize];
        let n = left.min(PER_BLOCK);
        for &set in &here.sets[..n] {
            each(set);
        }
        left -= n;
        block = here.next;
    }
}

/// The first eight bytes of a BLAKE3 output, which are uniform: a hash with nothing to compute.
fn word(bytes: &[u8; 32]) -> u64 {
    u64::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]])
}

fn version_hash(version: &Version) -> u64 {
    word(&version.id.0.0) ^ word(&version.bound.0.0).rotate_left(32)
}

/// Dense ids of keys the caller keeps in a column, open-addressed by a hash of each, at most
/// half full: a place holds an id.
struct Places {
    mask: usize,
    places: Vec<u32>,
    len: usize,
}

impl Default for Places {
    fn default() -> Places {
        // alloc: once per selector
        Places { mask: 7, places: vec![NONE; 8], len: 0 }
    }
}

impl Places {
    fn find(&self, hash: u64, is: impl Fn(u32) -> bool) -> Option<u32> {
        let mut at = hash as usize & self.mask;
        loop {
            match self.places[at] {
                NONE => return None,
                id if is(id) => return Some(id),
                _ => at = (at + 1) & self.mask,
            }
        }
    }

    /// Adds `id`, not in yet, under `hash`; growing asks `hash_of` the hash of each id held.
    fn insert(&mut self, hash: u64, id: u32, hash_of: impl Fn(u32) -> u64) {
        if (self.len + 1) * 2 > self.places.len() {
            let size = self.places.len() * 2;
            // alloc: growth, a table twice the size once it is half full
            let old = std::mem::replace(&mut self.places, vec![NONE; size]);
            self.mask = size - 1;
            for held in old {
                if held != NONE {
                    self.put(hash_of(held), held);
                }
            }
        }
        self.put(hash, id);
        self.len += 1;
    }

    fn put(&mut self, hash: u64, id: u32) {
        let mut at = hash as usize & self.mask;
        while self.places[at] != NONE {
            at = (at + 1) & self.mask;
        }
        self.places[at] = id;
    }
}

/// By function id.
#[derive(Default)]
struct Functions {
    ids: Vec<FnId>,
    /// Its bound code in the binary the selector is at; `None` when the binary has it not.
    now: Vec<Option<Bound>>,
    /// The newest of its versions met, the head of a chain through `Versions::next`.
    first: Vec<u32>,
    places: Places,
}

/// By version id.
#[derive(Default)]
struct Versions {
    of: Vec<Version>,
    function: Vec<u32>,
    /// The next version of the same function.
    next: Vec<u32>,
    postings: Vec<Chain>,
    places: Places,
}

/// By set id.
#[derive(Default)]
struct Sets {
    keys: Vec<Key>,
    unlogged: Vec<ContentId>,
    /// How many of its versions the binary does not have.
    stale: Vec<u32>,
    places: Places,
}

#[derive(Clone, Copy)]
enum End {
    /// Not read from the store yet.
    Unread,
    /// The test has no history.
    Empty,
    /// The slot of its newest entry.
    At(u32),
}

#[derive(Clone, Copy)]
enum SetRef {
    /// Not read yet, or not in the store when last asked.
    Key(Key),
    Known(u32),
}

/// An entry of a test's history as a walk needs it; what it read stays in the store.
struct Summary {
    slot: u32,
    passed: bool,
    kept: bool,
    attrs: ContentId,
    set: SetRef,
    took: Duration,
    basis: Basis,
}

/// A test's history: `entries` are the newest, from `end` down with no gap.
struct Memo {
    end: End,
    entries: Vec<Summary>,
}

/// The first reason a walk met to run a test, the explanation of a changed set made only if
/// the test runs.
enum Reason {
    Why(Why),
    Changed(u32),
}

/// Selection for one suite (its given) over the binary it was last given, kept across edits:
/// see the module's documentation.
pub struct Selector {
    given: ContentId,
    image: Image,
    functions: Functions,
    versions: Versions,
    blocks: Vec<Block>,
    sets: Sets,
    names: HashMap<Box<str>, u32>,
    tests: Vec<Memo>,
}

impl Selector {
    pub fn new(given: ContentId, image: Image) -> Selector {
        Selector {
            given,
            image,
            functions: Functions::default(),
            versions: Versions::default(),
            // alloc: empty, allocates nothing
            blocks: Vec::new(),
            sets: Sets::default(),
            names: HashMap::default(),
            // alloc: empty, allocates nothing
            tests: Vec::new(),
        }
    }

    pub fn given(&self) -> ContentId {
        self.given
    }

    /// The binary choices are made for.
    pub fn image(&self) -> &Image {
        &self.image
    }

    /// Moves every verdict to `image`, the binary after an edit: one lookup per function the
    /// selector has met, and for each whose bound code changed, its versions' postings.
    pub fn advance(&mut self, image: Image) {
        for function in 0..self.functions.ids.len() {
            let now = image.bound_of(&self.functions.ids[function]);
            let was = self.functions.now[function];
            if now != was {
                self.functions.now[function] = now;
                self.rebind(function as u32, was, now);
            }
        }
        self.image = image;
    }

    /// Moves the stale counts of the sets that hold a version of `function`, whose bound code
    /// was `was` and is `now`.
    fn rebind(&mut self, function: u32, was: Option<Bound>, now: Option<Bound>) {
        let mut version = self.functions.first[function as usize];
        while version != NONE {
            let bound = Some(self.versions.of[version as usize].bound);
            let (fitted, fits) = (bound == was, bound == now);
            if fitted != fits {
                let stale = &mut self.sets.stale;
                walk(&self.blocks, self.versions.postings[version as usize], |set| match fitted {
                    true => stale[set as usize] += 1,
                    false => stale[set as usize] -= 1,
                });
            }
            version = self.versions.next[version as usize];
        }
    }

    fn function(&mut self, id: FnId) -> u32 {
        let hash = word(&id.0.0);
        let ids = &self.functions.ids;
        if let Some(function) = self.functions.places.find(hash, |it| ids[it as usize] == id) {
            return function;
        }
        let function = self.functions.ids.len() as u32;
        let now = self.image.bound_of(&id);
        // alloc: growth, once per function the selector meets
        self.functions.ids.push(id);
        self.functions.now.push(now);
        self.functions.first.push(NONE);
        let ids = &self.functions.ids;
        self.functions.places.insert(hash, function, |it| word(&ids[it as usize].0.0));
        function
    }

    fn version(&mut self, version: Version) -> u32 {
        let hash = version_hash(&version);
        let of = &self.versions.of;
        if let Some(id) = self.versions.places.find(hash, |it| of[it as usize] == version) {
            return id;
        }
        let function = self.function(version.id);
        let id = self.versions.of.len() as u32;
        // alloc: growth, once per version the selector meets
        self.versions.of.push(version);
        self.versions.function.push(function);
        self.versions.next.push(self.functions.first[function as usize]);
        self.versions.postings.push(EMPTY);
        self.functions.first[function as usize] = id;
        let of = &self.versions.of;
        self.versions.places.insert(hash, id, |it| version_hash(&of[it as usize]));
        id
    }

    /// Gives the set kept under `key` an id: its versions, its postings, its stale count.
    fn register(&mut self, key: Key, unlogged: ContentId, versions: impl IntoIterator<Item = Version>) -> u32 {
        let set = self.sets.keys.len() as u32;
        let mut stale = 0;
        for version in versions {
            let id = self.version(version);
            post(&mut self.blocks, &mut self.versions.postings[id as usize], set);
            if self.functions.now[self.versions.function[id as usize] as usize] != Some(version.bound) {
                stale += 1;
            }
        }
        // alloc: growth, once per set the selector meets
        self.sets.keys.push(key);
        self.sets.unlogged.push(unlogged);
        self.sets.stale.push(stale);
        let keys = &self.sets.keys;
        self.sets.places.insert(word(&key.0.0), set, |it| word(&keys[it as usize].0.0));
        set
    }

    fn known(&self, key: &Key) -> Option<u32> {
        self.sets.places.find(word(&key.0.0), |it| self.sets.keys[it as usize] == *key)
    }

    /// Whether every function of the set is in the binary as it was, and the unlogged code is:
    /// what `Image::changed` says, read from counts.
    fn fits(&self, set: u32) -> bool {
        self.sets.stale[set as usize] == 0 && self.sets.unlogged[set as usize] == self.image.unlogged()
    }

    /// The memo of the test `name`, its history's end read once.
    fn test<P: Passes + ?Sized>(&mut self, store: &P, name: &str) -> Result<usize, Corrupt> {
        let t = match self.names.get(name) {
            Some(&t) => t as usize,
            None => {
                let t = self.tests.len();
                // alloc: once per test the selector meets
                self.names.insert(name.into(), t as u32);
                // alloc: empty, allocates nothing until a walk reads an entry
                self.tests.push(Memo { end: End::Unread, entries: Vec::new() });
                t
            }
        };
        if let End::Unread = self.tests[t].end {
            self.tests[t].end = match end(store, self.given, name)? {
                Some(at) => End::At(at),
                None => End::Empty,
            };
        }
        Ok(t)
    }

    /// Reads entries of test `t`'s history (whose newest is at slot `end`) until the `i`th
    /// back is in its memo.
    fn summary<P: Passes + ?Sized>(&mut self, store: &P, t: usize, name: &str, end: u32, i: usize) -> Result<(), Unread> {
        while self.tests[t].entries.len() <= i {
            let slot = end - self.tests[t].entries.len() as u32;
            let read = entry(store, self.given, name, slot)?;
            let summary = Summary {
                slot,
                passed: read.passed,
                kept: read.kept,
                attrs: read.attrs,
                set: SetRef::Key(read.set),
                took: read.took,
                basis: read.basis,
            };
            // alloc: growth, once per entry of a history that a walk reads
            self.tests[t].entries.push(summary);
        }
        Ok(())
    }

    /// The id of the set entry `i` of test `t` was kept under, read from the store the first
    /// time; `None` while the store has no set under its key.
    fn resolve<P: Passes + ?Sized, E>(&mut self, store: &P, t: usize, i: usize) -> Result<Option<u32>, SelectError<E>> {
        let key = match self.tests[t].entries[i].set {
            SetRef::Known(set) => return Ok(Some(set)),
            SetRef::Key(key) => key,
        };
        let set = match self.known(&key) {
            Some(set) => set,
            None => {
                let Some(bytes) = store.get(&key)? else { return Ok(None) };
                let view = SetView::decode(bytes).map_err(|why| SelectError::NotOurs { key, why })?;
                self.register(key, view.unlogged(), view.versions())
            }
        };
        self.tests[t].entries[i].set = SetRef::Known(set);
        Ok(Some(set))
    }

    /// [`choose`](crate::select::choose) for `subject` over the selector's binary, from its
    /// memo.
    pub fn choose<P: Passes + ?Sized, I: Inputs>(
        &mut self,
        store: &P,
        subject: &Subject<'_>,
        all: bool,
        inputs: &mut I,
    ) -> Result<Choice, SelectError<I::Error>> {
        let name = subject.name;
        let t = self.test(store, name)?;
        let End::At(end) = self.tests[t].end else {
            return Ok(Choice::Run { why: if subject.outside { Why::Outside } else { Why::New }, expected: None });
        };
        self.summary(store, t, name, end, 0)?;
        let expected = Some(self.tests[t].entries[0].took);
        if subject.outside {
            return Ok(Choice::Run { why: Why::Outside, expected });
        }
        if all {
            return Ok(Choice::Run { why: Why::All, expected });
        }
        let attrs = subject.attrs.id();
        let mut why: Option<Reason> = None;
        for i in 0..=end.min(LOOKBACK - 1) as usize {
            self.summary(store, t, name, end, i)?;
            let (slot, passed, kept) = {
                let summary = &self.tests[t].entries[i];
                if summary.attrs != attrs {
                    why.get_or_insert(Reason::Why(Why::Attributes));
                    continue;
                }
                (summary.slot, summary.passed, summary.kept)
            };
            let Some(set) = self.resolve(store, t, i)? else {
                why.get_or_insert(Reason::Why(Why::NoPass));
                continue;
            };
            if !self.fits(set) {
                why.get_or_insert(Reason::Changed(set));
                continue;
            }
            // The code it ran is as it is now: it would run as it ran, with the same inputs.
            if !passed {
                match self.read_changed(store, name, slot, inputs)? {
                    Some(input) => {
                        why.get_or_insert(Reason::Why(Why::Read(input)));
                        continue;
                    }
                    None => return Ok(Choice::Run { why: self.explain(store, why, Why::Failed)?, expected }),
                }
            }
            if !kept {
                why.get_or_insert(Reason::Why(Why::NoPass));
                continue;
            }
            let run = RunKey { code: identity(name, &attrs, &self.sets.keys[set as usize]), given: self.given };
            if find(store, &run, inputs)? {
                let summary = &self.tests[t].entries[i];
                // clone: the basis the caller is handed, which owns a reason only for the reached set
                return Ok(Choice::Cached(Cached { took: summary.took, basis: summary.basis.clone() }));
            }
            let reason = self.read_changed(store, name, slot, inputs)?.map_or(Why::NoPass, Why::Read);
            why.get_or_insert(Reason::Why(reason));
        }
        Ok(Choice::Run { why: self.explain(store, why, Why::NoPass)?, expected })
    }

    /// What the run of entry `slot` read that is not as it was, read from the store: asked
    /// only of a failure at unchanged code and of a pass not kept for the inputs now.
    fn read_changed<P: Passes + ?Sized, I: Inputs>(
        &self,
        store: &P,
        name: &str,
        slot: u32,
        inputs: &mut I,
    ) -> Result<Option<Input>, SelectError<I::Error>> {
        let Some(traced) = entry(store, self.given, name, slot)?.traced else { return Ok(None) };
        let now = Shape::of(&traced).values(inputs).map_err(SelectError::Inputs)?;
        Ok(changed_input(&traced, &now))
    }

    /// The `Why` of a run: the first reason met, `otherwise` when none was. A changed set is
    /// explained as `Image::changed` explains it, from its bytes.
    fn explain<P: Passes + ?Sized, E>(&self, store: &P, why: Option<Reason>, otherwise: Why) -> Result<Why, SelectError<E>> {
        let set = match why {
            None => return Ok(otherwise),
            Some(Reason::Why(why)) => return Ok(why),
            Some(Reason::Changed(set)) => set,
        };
        let key = self.sets.keys[set as usize];
        // What `choose` says of a set it can't read.
        let Some(bytes) = store.get(&key)? else { return Ok(Why::NoPass) };
        let view = SetView::decode(bytes).map_err(|why| SelectError::NotOurs { key, why })?;
        match self.image.changed(&view) {
            Some(change) => Ok(change.into()),
            None => Err(SelectError::NotOurs {
                key,
                // alloc: error path
                why: "the selector counts a version of this set as changed that the binary has as it was".to_owned(),
            }),
        }
    }

    /// [`keep`](crate::select::keep) through the selector, which remembers the entry and its
    /// set.
    pub fn keep<P: Passes + ?Sized>(&mut self, store: &mut P, subject: &Subject<'_>, ran: &Ran<'_>) -> Result<Kept, RecordError> {
        let (set, basis) = kept_set(&self.image, subject, ran)?;
        let t = self.test(&*store, subject.name)?;
        let from = match self.tests[t].end {
            End::At(at) => at + 1,
            End::Empty | End::Unread => 0,
        };
        let landed = keep_set(store, self.given, subject, ran, &set, basis, from)?;
        let id = match self.known(&landed.set) {
            Some(id) => id,
            None => self.register(landed.set, set.unlogged, set.fns.iter().copied()),
        };
        let memo = &mut self.tests[t];
        if landed.slot != from {
            // Another writer filled the slots from `from`: read the history again when asked.
            memo.end = End::Unread;
            memo.entries.clear();
            return Ok(landed.kept);
        }
        let summary = Summary {
            slot: landed.slot,
            passed: ran.passed,
            kept: landed.kept.pass,
            attrs: landed.attrs,
            set: SetRef::Known(id),
            // As the entry keeps it: whole microseconds.
            took: Duration::from_micros(ran.took.as_micros().min(u128::from(u64::MAX)) as u64),
            // clone: the memo's copy of what the caller is handed
            basis: landed.kept.basis.clone(),
        };
        memo.entries.insert(0, summary);
        memo.entries.truncate(LOOKBACK as usize);
        memo.end = End::At(landed.slot);
        Ok(landed.kept)
    }
}

#[cfg(test)]
mod tests;
