//! Whether a test runs, and how it ran (docs/v2/plan/lanes/fastcompile/select.md, "Selection").
//!
//! A pass is kept in the store under a [`RunKey`] whose code is the test's name, its
//! attributes and a [`Set`] of functions (by identity and bound code), and whose given is its
//! suite's ([`blaze_usercode::test::Suite::given`]); then by what it read at run time, as
//! [`blaze_usercode::index`] keeps a build script's result. The set is what the test
//! executed when its record is whole ([`Basis::Executed`]), and what codegen's walk says it
//! reaches otherwise ([`Basis::Reached`], with why): both cover what the run depends on, the
//! first exactly.
//!
//! Each test has a history beside its passes: one entry per recorded run, with whether it
//! passed, the set and attributes it ran with, how long it took, the basis, and what it read.
//! [`choose`] walks it from the newest entry back, at most [`LOOKBACK`] of them: the first
//! entry whose every function is in the binary as it was is the run the test would repeat now,
//! as a test's run is a function of the code it executes and what it reads. If it passed and
//! its pass is kept for the inputs as they are now, the test is not run and reports it,
//! [`Choice::Cached`]; if it failed with these inputs, it runs. The store never replaces a
//! value, so the history only grows; its end is found by doubling and halving.

use std::{fmt, time::Duration};

use blaze_cas::{Corrupt, Key, Mismatch, Ticket};
use blaze_core::Traced;
use blaze_ids::{ContentId, Hasher};
use blaze_usercode::{
    RunKey,
    index::{self, Shape},
    test::{
        Attrs, Input, Passes,
        select::{SelectError, changed_input, keepable},
    },
    trace::{self, Inputs, Report},
};

use crate::{
    record::Record,
    table::{Change, FnId, Image, Set, SetView, TableError},
};

/// How many entries of a test's history [`choose`] looks at, newest first: a run of edits
/// that comes back to code a test passed at within this many runs finds that pass.
pub const LOOKBACK: u32 = 16;

/// A test as selection sees it.
#[derive(Clone, Copy, Debug)]
pub struct Subject<'a> {
    /// The name libtest shows; [`blaze_usercode::test::select::WHOLE`] for a binary run whole.
    pub name: &'a str,
    pub attrs: &'a Attrs,
    /// It reaches the outside world, as codegen marks it: it always runs and is never kept.
    pub outside: bool,
    /// What codegen's walk says it reaches, by the binary's table index: the set a run whose
    /// record is not whole is kept under.
    pub reached: &'a [u32],
}

/// What set a pass is kept under: the functions the run executed, from its whole record, or
/// what codegen's walk says the test reaches, because the run's record was not whole, and why.
/// The agent API's type, so a cached result carries it as it is.
pub use blaze_api::Basis;

/// A test not run: its last pass stands.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Cached {
    /// How long the run that passed took.
    pub took: Duration,
    pub basis: Basis,
}

#[derive(PartialEq, Eq, Debug)]
pub enum Choice {
    /// It runs; `expected` is how long it took when it last ran, if it has.
    Run { why: Why, expected: Option<Duration> },
    Cached(Cached),
}

/// Why a test runs (`--explain`): of the newest run in its history, unless an older one says
/// more.
#[derive(PartialEq, Eq, Debug)]
pub enum Why {
    /// It has never run here.
    New,
    /// It reaches the outside world.
    Outside,
    /// Every test was asked for.
    All,
    /// A function it executed (or reaches, by [`Basis::Reached`]) is not as it was.
    Changed(FnId),
    /// A function it executed is no longer in the binary.
    Gone(FnId),
    /// Code that is called not through a slot (a dependency, the premapped image) changed.
    Unlogged,
    /// Its attributes (`#[ignore]`, `#[should_panic]`) are not as they were.
    Attributes,
    /// The same code and something it read is not as it was.
    Read(Input),
    /// It failed when it last ran at this code with these inputs.
    Failed,
    /// It passed with what can't be kept (the network, reads not all known), or its pass or
    /// set is gone from the store.
    NoPass,
}

impl Why {
    /// The explanation, with a function named by `name_of` when it knows it.
    pub fn describe(&self, name_of: &impl Fn(&FnId) -> Option<String>) -> String {
        let mut out = String::new(); // alloc: the edge, `--explain`'s text
        self.write(&mut out, name_of).expect("a String takes any text");
        out
    }

    fn write<W: fmt::Write>(&self, out: &mut W, name_of: &impl Fn(&FnId) -> Option<String>) -> fmt::Result {
        fn named<W: fmt::Write>(out: &mut W, id: &FnId, name_of: &impl Fn(&FnId) -> Option<String>) -> fmt::Result {
            match name_of(id) {
                Some(name) => out.write_str(&name),
                None => write!(out, "function {}", id.0),
            }
        }
        match self {
            Why::New => out.write_str("new"),
            Why::Outside => out.write_str("outside world"),
            Why::All => out.write_str("all tests asked for"),
            Why::Changed(id) => {
                out.write_str("ran ")?;
                named(out, id, name_of)?;
                out.write_str(", changed")
            }
            Why::Gone(id) => {
                out.write_str("ran ")?;
                named(out, id, name_of)?;
                out.write_str(", which is gone")
            }
            Why::Unlogged => out.write_str("code called not through a slot changed (a dependency)"),
            Why::Attributes => out.write_str("attributes changed"),
            Why::Read(Input::File(path)) => write!(out, "read {}, changed", path.display()),
            Why::Read(Input::Dir(path)) => write!(out, "listed {}, changed", path.display()),
            Why::Read(Input::Env(name)) => write!(out, "read ${}, changed", name.to_string_lossy()),
            Why::Failed => out.write_str("failed when last run at this code"),
            Why::NoPass => out.write_str("no pass kept"),
        }
    }
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write(f, &|_: &FnId| None)
    }
}

impl From<Change> for Why {
    fn from(change: Change) -> Why {
        match change {
            Change::Changed(id) => Why::Changed(id),
            Change::Gone(id) => Why::Gone(id),
            Change::Unlogged => Why::Unlogged,
        }
    }
}

/// Whether `subject`, of the suite given `given` and the binary `image`, runs, with every
/// test asked for when `all`. `inputs` are the files and environment as it would see them now.
pub fn choose<P: Passes + ?Sized, I: Inputs>(
    store: &P,
    given: ContentId,
    image: &Image,
    subject: &Subject<'_>,
    all: bool,
    inputs: &mut I,
) -> Result<Choice, SelectError<I::Error>> {
    let Some(end) = end(store, given, subject.name)? else {
        return Ok(Choice::Run { why: if subject.outside { Why::Outside } else { Why::New }, expected: None });
    };
    let mut newest = Some(entry(store, given, subject.name, end)?);
    let expected = newest.as_ref().map(|it| it.took);
    if subject.outside {
        return Ok(Choice::Run { why: Why::Outside, expected });
    }
    if all {
        return Ok(Choice::Run { why: Why::All, expected });
    }
    let attrs = subject.attrs.id();
    let mut why: Option<Why> = None;
    // Each set looked at, and what of it changed: entries of one code share their set.
    // alloc: the reference walk; a suite chooses through `Selector`, which reads no set again
    let mut looked: Vec<(Key, Option<Change>)> = Vec::new();
    let oldest = end.saturating_sub(LOOKBACK - 1);
    for n in (oldest..=end).rev() {
        let entry = match newest.take() {
            Some(it) => it,
            None => entry(store, given, subject.name, n)?,
        };
        if entry.attrs != attrs {
            why.get_or_insert(Why::Attributes);
            continue;
        }
        let change = match looked.iter().find(|it| it.0 == entry.set) {
            Some(it) => it.1,
            None => {
                let Some(bytes) = store.get(&entry.set)? else {
                    why.get_or_insert(Why::NoPass);
                    continue;
                };
                let set = SetView::decode(bytes).map_err(|why| SelectError::NotOurs { key: entry.set, why })?;
                let change = image.changed(&set);
                looked.push((entry.set, change));
                change
            }
        };
        if let Some(change) = change {
            why.get_or_insert(change.into());
            continue;
        }
        // The code it ran is as it is now: it would run as it ran, with the same inputs.
        if !entry.passed {
            let read = match &entry.traced {
                Some(traced) => {
                    let now = Shape::of(traced).values(inputs).map_err(SelectError::Inputs)?;
                    changed_input(traced, &now)
                }
                None => None,
            };
            match read {
                Some(input) => {
                    why.get_or_insert(Why::Read(input));
                    continue;
                }
                None => return Ok(Choice::Run { why: why.unwrap_or(Why::Failed), expected }),
            }
        }
        if !entry.kept {
            why.get_or_insert(Why::NoPass);
            continue;
        }
        let run = RunKey { code: identity(subject.name, &attrs, &entry.set), given };
        if find(store, &run, inputs)? {
            return Ok(Choice::Cached(Cached { took: entry.took, basis: entry.basis }));
        }
        let reason = match &entry.traced {
            Some(traced) => {
                let now = Shape::of(traced).values(inputs).map_err(SelectError::Inputs)?;
                changed_input(traced, &now).map_or(Why::NoPass, Why::Read)
            }
            None => Why::NoPass,
        };
        why.get_or_insert(reason);
    }
    Ok(Choice::Run { why: why.unwrap_or(Why::NoPass), expected })
}

/// The code of a pass's run key: the test, its attributes and the set it ran.
pub(crate) fn identity(name: &str, attrs: &ContentId, set: &Key) -> ContentId {
    Hasher::new("blaze v2 testsel identity 1").part(name.as_bytes()).id(attrs).id(&set.0).finish()
}

/// Whether a pass of `run` is kept for its inputs as they are now: [`index`]'s search, over
/// any [`Passes`].
pub(crate) fn find<P: Passes + ?Sized, I: Inputs>(
    store: &P,
    run: &RunKey,
    inputs: &mut I,
) -> Result<bool, SelectError<I::Error>> {
    let mut n = 0;
    loop {
        let key = index::slot(run, n);
        let Some(bytes) = store.get(&key)? else { return Ok(false) };
        let shape = Shape::decode(bytes).map_err(|why| SelectError::NotOurs { key, why })?;
        let traced = shape.values(inputs).map_err(SelectError::Inputs)?;
        let key = index::result_key(run, &traced);
        if let Some(bytes) = store.get(&key)? {
            return match bytes == PASS {
                true => Ok(true),
                // alloc: error path
                false => Err(SelectError::NotOurs { key, why: "not a test's pass".to_owned() }),
            };
        }
        n += 1;
    }
}

/// How a run went, to keep.
#[derive(Clone, Copy, Debug)]
pub struct Ran<'a> {
    pub passed: bool,
    pub took: Duration,
    /// What it read at run time ([`blaze_usercode::test::Trace::traced`]); `None` when not
    /// known, and then no pass is kept.
    pub traced: Option<&'a Traced>,
    /// What it executed.
    pub record: &'a Record,
}

#[derive(Debug)]
pub enum RecordError {
    Corrupt(Corrupt),
    /// The store has a value of one of our keys with other bytes: never kept over.
    Mismatch(Mismatch),
    /// The record or the test does not fit the binary's table.
    Table(TableError),
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordError::Corrupt(it) => it.fmt(f),
            RecordError::Mismatch(it) => write!(
                f,
                "the store has a test's value under {:?} with other bytes ({:?}, not {:?})",
                it.key.0, it.kept, it.given
            ),
            RecordError::Table(it) => it.fmt(f),
        }
    }
}

impl From<Corrupt> for RecordError {
    fn from(it: Corrupt) -> RecordError {
        RecordError::Corrupt(it)
    }
}

/// What [`keep`] kept.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Kept {
    pub tickets: Vec<Ticket>,
    /// The set the run was kept under.
    pub basis: Basis,
    /// A pass was kept.
    pub pass: bool,
}

/// Keeps how `subject` ran: its set, its pass when it passed and can be kept, and an entry
/// of its history.
pub fn keep<P: Passes + ?Sized>(
    store: &mut P,
    given: ContentId,
    image: &Image,
    subject: &Subject<'_>,
    ran: &Ran<'_>,
) -> Result<Kept, RecordError> {
    let (set, basis) = kept_set(image, subject, ran)?;
    let from = end(store, given, subject.name)?.map_or(0, |it| it + 1);
    Ok(keep_set(store, given, subject, ran, &set, basis, from)?.kept)
}

/// The set a run is kept under: what it executed when its record is whole, else what it
/// reaches, saying why.
pub(crate) fn kept_set(image: &Image, subject: &Subject<'_>, ran: &Ran<'_>) -> Result<(Set, Basis), RecordError> {
    Ok(match image.of_record(ran.record) {
        Some(set) => (set.map_err(RecordError::Table)?, Basis::Executed),
        None => {
            // alloc: the reason a basis carries, for a run whose record is not whole
            let why = ran.record.why().unwrap_or_default().to_owned();
            (image.reached(subject.reached).map_err(RecordError::Table)?, Basis::Reached { why })
        }
    })
}

/// What [`keep_set`] kept and where: the set's key, the attributes' and the history slot its
/// entry is in.
pub(crate) struct Landed {
    pub(crate) kept: Kept,
    pub(crate) set: Key,
    pub(crate) attrs: ContentId,
    pub(crate) slot: u32,
}

/// Keeps a run under `set`, its history entry in the first slot from `from` that is free.
pub(crate) fn keep_set<P: Passes + ?Sized>(
    store: &mut P,
    given: ContentId,
    subject: &Subject<'_>,
    ran: &Ran<'_>,
    set: &Set,
    basis: Basis,
    from: u32,
) -> Result<Landed, RecordError> {
    // alloc: once per kept run, the store's receipts
    let mut tickets = Vec::new();
    let encoded = set.encode();
    let key = Set::key_of(&encoded);
    if store.get(&key)?.is_none() {
        tickets.push(store.put(key, encoded).map_err(RecordError::Mismatch)?);
    }
    let traced = ran.traced.map(|it| {
        // clone: `normalize` sorts in place, and the run's record is the caller's
        let mut it = it.clone();
        trace::normalize(&mut it);
        it
    });
    let attrs = subject.attrs.id();
    let pass = ran.passed && !subject.outside && traced.as_ref().is_some_and(keepable);
    if pass && let Some(traced) = &traced {
        let run = RunKey { code: identity(subject.name, &attrs, &key), given };
        // alloc: the store keeps owned bytes, once per kept pass
        tickets.push(store.put(index::result_key(&run, traced), PASS.to_vec()).map_err(RecordError::Mismatch)?);
        let shape = Shape::of(traced).encode();
        put_first_free(store, |n| index::slot(&run, n), 0, &shape, &mut tickets)?;
    }
    let entry = Entry {
        passed: ran.passed,
        kept: pass,
        attrs,
        set: key,
        took: ran.took,
        basis,
        // A failure's reads say whether it would fail again; a pass not kept has none to say.
        traced: traced.filter(|_| pass || !ran.passed),
    };
    let bytes = entry.encode();
    let slot = put_first_free(store, |n| history_slot(given, subject.name, n), from, &bytes, &mut tickets)?;
    Ok(Landed { kept: Kept { tickets, basis: entry.basis, pass }, set: key, attrs, slot })
}

/// Puts `bytes` in the first slot from `from` that is free or holds them already, and says
/// which; a slot another filled with other bytes first is passed over.
fn put_first_free<P: Passes + ?Sized>(
    store: &mut P,
    slot: impl Fn(u32) -> Key,
    from: u32,
    bytes: &[u8],
    tickets: &mut Vec<Ticket>,
) -> Result<u32, RecordError> {
    let mut n = from;
    loop {
        let at = slot(n);
        match store.get(&at)? {
            Some(kept) if kept == bytes => return Ok(n),
            Some(_) => n += 1,
            // alloc: the store keeps owned bytes, once per kept run
            None => match store.put(at, bytes.to_vec()) {
                Ok(ticket) => {
                    tickets.push(ticket);
                    return Ok(n);
                }
                Err(_) => n += 1,
            },
        }
    }
}

/// The value kept under a pass's key: the same bytes for every pass.
const PASS: &[u8] = b"blaze v2 testsel pass 1";
const HISTORY: &[u8] = b"blaze v2 testsel history 1";

/// One recorded run of a test.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Entry {
    pub(crate) passed: bool,
    /// A pass was kept for it.
    pub(crate) kept: bool,
    pub(crate) attrs: ContentId,
    /// The key of the set it was kept under.
    pub(crate) set: Key,
    pub(crate) took: Duration,
    pub(crate) basis: Basis,
    /// What it read: for a kept pass and for a failure.
    pub(crate) traced: Option<Traced>,
}

impl Entry {
    fn encode(&self) -> Vec<u8> {
        let why = match &self.basis {
            Basis::Executed => "",
            Basis::Reached { why } => why.as_str(),
        };
        let traced = match &self.traced {
            // clone: `trace::encode` takes a report, which owns its record; once per kept run
            Some(traced) => trace::encode(&Report::Ran(Some(traced.clone()))),
            None => Vec::new(), // alloc: empty, allocates nothing
        };
        let mut out = Vec::with_capacity(HISTORY.len() + 1 + 64 + 8 + 4 + why.len() + 8 + traced.len());
        out.extend_from_slice(HISTORY);
        let reached = matches!(self.basis, Basis::Reached { .. });
        out.push(u8::from(self.passed) | (u8::from(self.kept) << 1) | (u8::from(reached) << 2));
        out.extend_from_slice(&self.attrs.0);
        out.extend_from_slice(&self.set.0.0);
        out.extend_from_slice(&(self.took.as_micros().min(u128::from(u64::MAX)) as u64).to_le_bytes());
        out.extend_from_slice(&(why.len() as u32).to_le_bytes());
        out.extend_from_slice(why.as_bytes());
        out.extend_from_slice(&(traced.len() as u64).to_le_bytes());
        out.extend_from_slice(&traced);
        out
    }

    fn decode(bytes: &[u8]) -> Result<Entry, String> {
        // alloc: error path
        let rest = bytes.strip_prefix(HISTORY).ok_or_else(|| "not a test's history".to_owned())?;
        let short = || "a test's history ends early".to_owned(); // alloc: error path
        let (head, rest) = rest.split_at_checked(1 + 32 + 32 + 8 + 4).ok_or_else(short)?;
        let flags = head[0];
        let id = |at: usize| ContentId(head[at..at + 32].try_into().expect("32 bytes"));
        let took = u64::from_le_bytes(head[65..73].try_into().expect("8 bytes"));
        let why_len = u32::from_le_bytes(head[73..77].try_into().expect("4 bytes")) as usize;
        let (why, rest) = rest.split_at_checked(why_len).ok_or_else(short)?;
        // alloc: error path
        let why = std::str::from_utf8(why).map_err(|_| "a test's history has a reason that is not UTF-8".to_owned())?;
        let (len, traced) = rest.split_at_checked(8).ok_or_else(short)?;
        let len = u64::from_le_bytes(len.try_into().expect("8 bytes"));
        if traced.len() as u64 != len {
            // alloc: error path
            return Err("a test's history has a record of another length than it says".to_owned());
        }
        let traced = match traced.is_empty() {
            true => None,
            false => match trace::decode(traced)? {
                Report::Ran(Some(traced)) => Some(traced),
                // alloc: error path
                _ => return Err("a test's history holds a report with no record".to_owned()),
            },
        };
        let basis = match flags & 4 != 0 {
            // alloc: the reason a basis of the reached set carries
            true => Basis::Reached { why: why.to_owned() },
            false => Basis::Executed,
        };
        Ok(Entry {
            passed: flags & 1 != 0,
            kept: flags & 2 != 0,
            attrs: id(1),
            set: Key(id(33)),
            took: Duration::from_micros(took),
            basis,
            traced,
        })
    }
}

pub(crate) fn history_slot(given: ContentId, name: &str, n: u32) -> Key {
    Key(Hasher::new("blaze v2 testsel history slot 1").id(&given).part(name.as_bytes()).update(&n.to_le_bytes()).finish())
}

/// What reading a test's history can meet.
pub(crate) enum Unread {
    Corrupt(Corrupt),
    NotOurs { key: Key, why: String },
}

impl From<Corrupt> for Unread {
    fn from(it: Corrupt) -> Unread {
        Unread::Corrupt(it)
    }
}

impl<E> From<Unread> for SelectError<E> {
    fn from(it: Unread) -> SelectError<E> {
        match it {
            Unread::Corrupt(it) => SelectError::Corrupt(it),
            Unread::NotOurs { key, why } => SelectError::NotOurs { key, why },
        }
    }
}

pub(crate) fn entry<P: Passes + ?Sized>(store: &P, given: ContentId, name: &str, n: u32) -> Result<Entry, Unread> {
    let key = history_slot(given, name, n);
    let Some(bytes) = store.get(&key)? else {
        // alloc: error path
        return Err(Unread::NotOurs { key, why: "an entry of a test's history is missing below its end".to_owned() });
    };
    Entry::decode(bytes).map_err(|why| Unread::NotOurs { key, why })
}

/// The slot of the last entry of a test's history. Slots are filled from 0 with no gap, so
/// the end is found by doubling then halving.
pub(crate) fn end<P: Passes + ?Sized>(store: &P, given: ContentId, name: &str) -> Result<Option<u32>, Corrupt> {
    let filled = |n: u32| -> Result<bool, Corrupt> { Ok(store.get(&history_slot(given, name, n))?.is_some()) };
    if !filled(0)? {
        return Ok(None);
    }
    // `low` is filled; `high` is empty, or `u32::MAX` and filled.
    let (mut low, mut high) = (0u32, 1u32);
    while filled(high)? {
        low = high;
        if high == u32::MAX {
            return Ok(Some(high));
        }
        high = high.saturating_mul(2);
    }
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        match filled(middle)? {
            true => low = middle,
            false => high = middle,
        }
    }
    Ok(Some(low))
}

#[cfg(test)]
pub(crate) mod tests;
