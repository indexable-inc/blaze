//! The chain of read-lists under a base, the lookup through it and the filing into it: the
//! rule is in the crate's doc.

#![blaze::sans_io]

use alloc::vec::Vec;
use core::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

use blaze_ids::ContentId;

use crate::{
    keys::{AnswerKey, Digest, Slot, list_key},
    list::{ListError, ListRef},
};

/// Which of a record's two parts a key is of: what a store that keeps them under kinds of
/// its own tells them apart by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    List,
    Answer,
}

/// The write-once content-addressed store a record is kept in, as its caller holds it.
pub trait Store {
    /// A value as the store holds it, kept alive while its bytes are read.
    type Held;
    type Error;

    /// The value under `key`, or `None` where the store has none.
    fn get(&self, kind: Kind, key: &ContentId) -> Result<Option<Self::Held>, Self::Error>;

    /// Keeps `bytes` under `key`. A store may refuse a put of other bytes under a key it
    /// holds; it need not show a put to a later `get` of the same filing ([`Filing`]).
    fn put(&self, kind: Kind, key: ContentId, bytes: &[u8]) -> Result<(), Self::Error>;

    fn bytes(held: &Self::Held) -> &[u8];

    /// Where the lookups through this store are counted ([`Tally`]): the store's one, or the
    /// one of the kind of work this handle is for, where the store keeps its kinds apart.
    /// Required: a store whose lookups nobody counts does not compile.
    fn tally(&self) -> &Tally;
}

/// What the lookups through one store came to, counted where the form is searched ([`walk`],
/// [`lookup`]) and nowhere else: how many lookups, how many found an answer, how many
/// read-lists they read and how many reads were valued, each of which its caller hashed again.
///
/// A store owns one, or one for each kind of work it keeps apart, and gives it through
/// [`Store::tally`]; its owner takes the counts where it reports ([`Tally::take`]). A placing
/// ([`Filing`]) and a keeping ([`keep_answer`]) count nothing: neither is a lookup.
///
/// Time is not counted: this crate holds no clock. How long a lookup took is in the span its
/// caller opens around it.
///
/// Alone on its cache lines: every thread that looks up through the store adds to it.
#[derive(Debug, Default)]
#[repr(align(128))]
pub struct Tally {
    lookups: AtomicU64,
    hits: AtomicU64,
    lists: AtomicU64,
    reads: AtomicU64,
}

impl Tally {
    pub const fn new() -> Tally {
        Tally {
            lookups: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            lists: AtomicU64::new(0),
            reads: AtomicU64::new(0),
        }
    }

    /// Counts `reads` reads valued: what a [`walk`]'s caller says of the reads its visitor
    /// valued, each one it asked the value of, whether or not it got one. [`lookup`] says its
    /// own.
    pub fn valued(&self, reads: u64) {
        if reads > 0 {
            self.reads.fetch_add(reads, Ordering::Relaxed);
        }
    }

    /// One walk, of `lists` read-lists, that found an answer when `hit`.
    fn walked(&self, lists: u32, hit: bool) {
        self.lookups.fetch_add(1, Ordering::Relaxed);
        if lists > 0 {
            self.lists.fetch_add(u64::from(lists), Ordering::Relaxed);
        }
        if hit {
            self.hits.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The counts since the last take, leaving zero. Each of the four is taken on its own: a
    /// lookup that ends while they are taken can have its parts in this take and the next,
    /// and the sum over takes is exact.
    pub fn take(&self) -> Counts {
        Counts {
            lookups: self.lookups.swap(0, Ordering::Relaxed),
            hits: self.hits.swap(0, Ordering::Relaxed),
            lists: self.lists.swap(0, Ordering::Relaxed),
            reads: self.reads.swap(0, Ordering::Relaxed),
        }
    }
}

/// What a [`Tally`] counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Walks of a base's chain: every [`lookup`] and every [`walk`], one that stopped on an
    /// error too.
    pub lookups: u64,
    /// Those that found an answer ([`Found::Hit`], [`Walked::Found`]).
    pub hits: u64,
    /// Read-lists read from the store and opened.
    pub lists: u64,
    /// Reads whose value now was asked for, each a hash made again by the caller: the ones
    /// that had none ([`lookup`]'s `digest` giving `None`) too.
    pub reads: u64,
}

impl Counts {
    /// `more` added to these: the counts of two tallies, or of two takes of one.
    pub fn add(&mut self, more: Counts) {
        self.lookups += more.lookups;
        self.hits += more.hits;
        self.lists += more.lists;
        self.reads += more.reads;
    }
}

/// As a note says them: `7 lookups, 3 hit, 9 read-lists read, 21 reads valued`.
impl fmt::Display for Counts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Counts { lookups, hits, lists, reads } = self;
        write!(f, "{lookups} lookups, {hits} hit, {lists} read-lists read, {reads} reads valued")
    }
}

/// The chain's shape: `width` slots a generation, at most `generations` of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    width: u32,
    generations: u32,
}

impl Bounds {
    /// Four lists a generation, as the forms this replaces kept four slots; 1024 generations,
    /// so 4096 lists a base before [`Placed::Full`].
    pub const STANDARD: Bounds = Bounds::new(4, 1024);

    pub const fn new(width: u32, generations: u32) -> Bounds {
        assert!(width >= 1 && generations >= 1, "a chain has a slot and a generation");
        assert!(width.checked_mul(generations).is_some(), "a chain's slots are counted in u32");
        Bounds { width, generations }
    }

    pub const fn width(self) -> u32 {
        self.width
    }

    pub const fn generations(self) -> u32 {
        self.generations
    }
}

/// What the store was asked when it failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Doing {
    ReadingList,
    KeepingList,
    ReadingAnswer,
    KeepingAnswer,
}

impl fmt::Display for Doing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Doing::ReadingList => "reading the read-list",
            Doing::KeepingList => "keeping the read-list",
            Doing::ReadingAnswer => "reading the answer",
            Doing::KeepingAnswer => "keeping the answer",
        })
    }
}

/// Why a lookup or a filing stopped. Never a miss: a miss is [`Found::Miss`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error<E> {
    /// The store could not read, or would not keep, the value under `key`.
    Store { doing: Doing, key: ContentId, error: E },
    /// The slot `at` of the base's chain holds what is no read-list of that base.
    List { at: Slot, why: ListError },
    /// Another answer is under `key` already: the work read something its list does not name.
    TwoAnswers { key: ContentId },
}

impl<E: fmt::Display> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Store { doing, key, error } => write!(f, "{doing} under {key:x}: {error}"),
            Error::List { at, why } => {
                write!(f, "generation {} slot {} of the chain holds {why}", at.generation, at.n)
            }
            Error::TwoAnswers { key } => write!(
                f,
                "two answers under {key:x}: the work read something its read-list does not name"
            ),
        }
    }
}

fn get<S: Store>(
    store: &S,
    kind: Kind,
    doing: Doing,
    key: &ContentId,
) -> Result<Option<S::Held>, Error<S::Error>> {
    store.get(kind, key).map_err(|error| Error::Store { doing, key: *key, error })
}

/// What a lookup found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found<H> {
    /// The answer under `key`, whose read-list is in slot `at`: every read the list names has
    /// now the digest it had when the answer was kept.
    Hit { answer: H, key: ContentId, at: Slot },
    /// No answer. `lists` read-lists were read. `full`: the chain is at its bound, so a list
    /// the chain does not hold cannot be placed ([`Placed::Full`]).
    Miss { lists: u32, full: bool },
}

/// What a walk of a base's chain came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Walked<T> {
    /// The visitor ended the walk with `found`, at the read-list in slot `at`.
    Found { found: T, at: Slot },
    /// The visitor passed over every read-list searched. `lists` and `full`: as
    /// [`Found::Miss`]'s.
    Passed { lists: u32, full: bool },
}

enum Searched<T> {
    Hit(T, Slot),
    /// A slot held nothing: the generation is not full.
    Ended,
    Full,
}

/// The answer of `base` under what its reads are now, if the store has one. `digest` is asked
/// the digest a read has now, by the name its list has for it; `None` where it cannot be
/// worked out now, which passes over that list.
///
/// Counted in the store's [`Tally`]: the lookup, the lists it read, each read `digest` was
/// asked (the one it had no digest of too) and the hit.
pub fn lookup<S: Store>(
    store: &S,
    bounds: Bounds,
    base: &ContentId,
    mut digest: impl FnMut(&[u8]) -> Option<Digest>,
) -> Result<Found<S::Held>, Error<S::Error>> {
    let mut valued = 0_u64;
    let walked = walk::<S, (S::Held, ContentId), Error<S::Error>, _>(
        store,
        bounds,
        base,
        |_at, bytes, list| {
            let mut key = AnswerKey::new(bytes);
            for read in list.reads() {
                valued += 1;
                let Some(now) = digest(read) else { return Ok(None) };
                key.value(now);
            }
            let key = key.finish();
            let answer = get(store, Kind::Answer, Doing::ReadingAnswer, &key)?;
            Ok(answer.map(|answer| (answer, key)))
        },
    );
    // Before the error goes on: the reads of a lookup that stopped were valued all the same.
    store.tally().valued(valued);
    let walked = walked?;
    Ok(match walked {
        Walked::Found { found: (answer, key), at } => Found::Hit { answer, key, at },
        Walked::Passed { lists, full } => Found::Miss { lists, full },
    })
}

/// Shows `visit` the read-lists of `base` a lookup searches, in its order (generation 0, the
/// open generation, the one before it), each with its slot, its bytes and the list opened:
/// until `visit` gives `Some`, which ends the walk. [`lookup`] is this with a visitor that
/// keys the answer by each read's digest and gets it.
///
/// For a caller whose answer is not one record of the same store, whose reads are valued a
/// list at a time or can fail to be, or who may refuse an answer found and go on: the visitor
/// values the list (an [`AnswerKey`] of `bytes` keeps the answer's key this crate's), looks
/// for the answer where it is, and gives `None` to pass over the list. Its error, `E`, ends
/// the walk; the store's and the chain's errors are made into it.
///
/// Counted in the store's [`Tally`]: one lookup, the lists read (those read before an error
/// too) and a hit when the visitor gave `Some`. The reads are the visitor's to value, so this
/// counts none: the caller says how many its visitor asked the value of, with
/// `store.tally().valued(n)` ([`Tally::valued`]), or the store's count of reads valued is
/// short by them.
pub fn walk<S, T, E, V>(
    store: &S,
    bounds: Bounds,
    base: &ContentId,
    visit: V,
) -> Result<Walked<T>, E>
where
    S: Store,
    E: From<Error<S::Error>>,
    V: FnMut(Slot, &[u8], ListRef<'_>) -> Result<Option<T>, E>,
{
    let mut lists = 0;
    let walked = walk_chain(store, bounds, base, visit, &mut lists);
    store.tally().walked(lists, matches!(walked, Ok(Walked::Found { .. })));
    walked
}

/// [`walk`], uncounted: `lists` is how many read-lists it read, whatever it came to.
fn walk_chain<S, T, E, V>(
    store: &S,
    bounds: Bounds,
    base: &ContentId,
    mut visit: V,
    lists: &mut u32,
) -> Result<Walked<T>, E>
where
    S: Store,
    E: From<Error<S::Error>>,
    V: FnMut(Slot, &[u8], ListRef<'_>) -> Result<Option<T>, E>,
{
    match search(store, bounds, base, 0, &mut visit, lists)? {
        Searched::Hit(found, at) => return Ok(Walked::Found { found, at }),
        Searched::Ended => return Ok(Walked::Passed { lists: *lists, full: false }),
        Searched::Full => {}
    }
    let open = first_open(bounds.generations, |generation| {
        let last = list_key(base, Slot { generation, n: bounds.width - 1 });
        Ok::<bool, Error<S::Error>>(
            get(store, Kind::List, Doing::ReadingList, &last)?.is_some(),
        )
    })?;
    // The newest lists first. Generation 0 is searched above; the bound itself holds nothing.
    for generation in [open, open - 1] {
        if generation == 0 || generation == bounds.generations {
            continue;
        }
        let searched = search(store, bounds, base, generation, &mut visit, lists)?;
        if let Searched::Hit(found, at) = searched {
            return Ok(Walked::Found { found, at });
        }
    }
    Ok(Walked::Passed { lists: *lists, full: open == bounds.generations })
}

fn search<S, T, E, V>(
    store: &S,
    bounds: Bounds,
    base: &ContentId,
    generation: u32,
    visit: &mut V,
    lists: &mut u32,
) -> Result<Searched<T>, E>
where
    S: Store,
    E: From<Error<S::Error>>,
    V: FnMut(Slot, &[u8], ListRef<'_>) -> Result<Option<T>, E>,
{
    for n in 0..bounds.width {
        let at = Slot { generation, n };
        let Some(held) = get(store, Kind::List, Doing::ReadingList, &list_key(base, at))? else {
            return Ok(Searched::Ended);
        };
        *lists += 1;
        let bytes = S::bytes(&held);
        let list =
            ListRef::open(bytes, base).map_err(|why| Error::<S::Error>::List { at, why })?;
        if let Some(found) = visit(at, bytes, list)? {
            return Ok(Searched::Hit(found, at));
        }
    }
    Ok(Searched::Full)
}

/// The open generation: the first that is not full, given that generation 0 is, in
/// `1..=generations` (`generations`: every one is full). `full(g)` is whether `g`'s last slot
/// holds a list. Doubling steps from 0 until a generation is not full, then halving between:
/// at most `2 log2(generations) + 1` calls. Where a collector took an old generation's lists
/// and left a newer one's, it is some generation that is not full and follows a full one.
fn first_open<E>(generations: u32, mut full: impl FnMut(u32) -> Result<bool, E>) -> Result<u32, E> {
    // The newest generation known full, and the oldest known not to be.
    let mut newest_full = 0_u32;
    let mut open = generations;
    let mut found_open = false;
    let mut step = 1_u32;
    while newest_full + 1 < open {
        let probe = match found_open {
            false => newest_full.saturating_add(step).min(generations - 1),
            true => newest_full + (open - newest_full) / 2,
        };
        match full(probe)? {
            true => {
                newest_full = probe;
                step = step.saturating_mul(2);
            }
            false => {
                open = probe;
                found_open = true;
            }
        }
    }
    Ok(open)
}

/// Where a read-list was placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placed {
    /// This placing put it in the slot.
    Written(Slot),
    /// The slot held it already.
    Kept(Slot),
    /// Every generation is full of other lists: the bound. The list is not filed, and its
    /// answer would not be found; the caller counts it and names the base.
    Full,
}

/// The placing of the read-lists of one base by one flush, one after another.
///
/// One flush can place several lists of one base, and its store need not show a put it has
/// not published yet: so what this filing placed is remembered here and looked at before the
/// store. A flush that places the lists of one base in an order that is of their bytes alone
/// gives each the same slot whichever thread offered it.
pub struct Filing<'a> {
    base: ContentId,
    bounds: Bounds,
    placed: Vec<(Slot, &'a [u8])>,
    forgetful: bool,
}

impl<'a> Filing<'a> {
    pub fn new(base: ContentId, bounds: Bounds) -> Filing<'a> {
        Filing { base, bounds, placed: Vec::new(), forgetful: false }
    }

    /// The planted bug of a flush's control: nothing placed is remembered, so the second list
    /// of a base is placed against the store alone. For tests.
    #[doc(hidden)]
    pub fn planted_forgetful(mut self) -> Filing<'a> {
        self.forgetful = true;
        self
    }

    fn remember(&mut self, at: Slot, list: &'a [u8]) {
        if !self.forgetful {
            // alloc: one entry a list placed, for the length of one base's flush
            self.placed.push((at, list));
        }
    }

    fn placed_at(&self, at: Slot) -> Option<&'a [u8]> {
        self.placed.iter().find(|(slot, _)| *slot == at).map(|&(_, list)| list)
    }

    /// Whether slot `at` holds a list, placed here or in the store.
    fn holds<S: Store>(&self, store: &S, at: Slot) -> Result<bool, Error<S::Error>> {
        if self.placed_at(at).is_some() {
            return Ok(true);
        }
        Ok(get(store, Kind::List, Doing::ReadingList, &list_key(&self.base, at))?.is_some())
    }

    /// Places `list` (the bytes [`crate::write_list`] wrote for this base) in the chain: in
    /// generation 0 while it has room or holds the list; else where the generation before the
    /// open one holds it; else in the open generation.
    pub fn place<S: Store>(
        &mut self,
        store: &S,
        list: &'a [u8],
    ) -> Result<Placed, Error<S::Error>> {
        if let Some(placed) = self.place_in(store, 0, list, true)? {
            return Ok(placed);
        }
        let (width, generations) = (self.bounds.width, self.bounds.generations);
        let open = first_open(generations, |generation| {
            self.holds(store, Slot { generation, n: width - 1 })
        })?;
        // A lookup searches the generation before the open one: a list it holds is not filed
        // again.
        if open - 1 > 0 {
            if let Some(placed) = self.place_in(store, open - 1, list, false)? {
                return Ok(placed);
            }
        }
        // More than one step only where other writers filled the open generation meanwhile.
        for generation in open..generations {
            if let Some(placed) = self.place_in(store, generation, list, true)? {
                return Ok(placed);
            }
        }
        Ok(Placed::Full)
    }

    /// `list` in the first slot of `generation` that holds it, or (when `put`) is free; `None`
    /// when every slot holds another list, or the list is not there and nothing may be put.
    ///
    /// A slot this filing placed a list in is known from here; any other is read from the
    /// store, and a free one is put. A put the store refuses is read again: another writer's
    /// list kept in between takes the slot, and this one goes on to the next.
    fn place_in<S: Store>(
        &mut self,
        store: &S,
        generation: u32,
        list: &'a [u8],
        put: bool,
    ) -> Result<Option<Placed>, Error<S::Error>> {
        for n in 0..self.bounds.width {
            let at = Slot { generation, n };
            match self.placed_at(at) {
                Some(placed) if placed == list => return Ok(Some(Placed::Kept(at))),
                Some(_) => continue,
                None => {}
            }
            let key = list_key(&self.base, at);
            match get(store, Kind::List, Doing::ReadingList, &key)? {
                Some(held) if S::bytes(&held) == list => {
                    self.remember(at, list);
                    return Ok(Some(Placed::Kept(at)));
                }
                Some(_) => continue,
                None => {}
            }
            if !put {
                return Ok(None);
            }
            match store.put(Kind::List, key, list) {
                Ok(()) => {
                    self.remember(at, list);
                    return Ok(Some(Placed::Written(at)));
                }
                Err(error) => match get(store, Kind::List, Doing::ReadingList, &key)? {
                    Some(held) if S::bytes(&held) == list => {
                        self.remember(at, list);
                        return Ok(Some(Placed::Kept(at)));
                    }
                    Some(_) => continue,
                    None => return Err(Error::Store { doing: Doing::KeepingList, key, error }),
                },
            }
        }
        Ok(None)
    }
}

/// What keeping an answer did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kept {
    Written,
    /// The store held these bytes under the key already.
    Held,
}

/// Keeps `answer` under `key` (an [`AnswerKey`] of the list placed and the digests its reads
/// had when the work ran). Other bytes under the key already are [`Error::TwoAnswers`], and
/// nothing is put: a key names one value.
pub fn keep_answer<S: Store>(
    store: &S,
    key: ContentId,
    answer: &[u8],
) -> Result<Kept, Error<S::Error>> {
    match get(store, Kind::Answer, Doing::ReadingAnswer, &key)? {
        Some(held) if S::bytes(&held) == answer => Ok(Kept::Held),
        Some(_) => Err(Error::TwoAnswers { key }),
        None => store
            .put(Kind::Answer, key, answer)
            .map(|()| Kept::Written)
            .map_err(|error| Error::Store { doing: Doing::KeepingAnswer, key, error }),
    }
}

#[cfg(test)]
pub(crate) fn first_open_for_tests(generations: u32, full_below: u32, probes: &mut u32) -> u32 {
    let found: Result<u32, core::convert::Infallible> = first_open(generations, |generation| {
        *probes += 1;
        Ok(generation < full_below)
    });
    match found {
        Ok(open) => open,
        Err(never) => match never {},
    }
}
