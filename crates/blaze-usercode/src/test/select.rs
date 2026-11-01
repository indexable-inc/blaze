//! Which tests run (#227, #1330): a test runs only when it is new, reaches code it did not
//! reach when it last passed, read something at run time that is not as it was, has no pass
//! kept, or reaches the outside world (codegen marks those: [`Test::outside`]).
//!
//! A pass is kept in the store by content, never by binary, view or worktree: under a
//! [`RunKey`] whose code is the test's [`Test::identity`] (its name, the code hashes it
//! reaches, its attributes) and whose given is its [`Suite`], then by what it read at run time
//! as [`crate::index`] keeps a build script's result (the shapes of its reads, then their
//! values now). So two views at the same code with the same files skip the same tests.
//!
//! Beside the passes, each test has a history in the store: a chain of entries, one per
//! recorded run, each with whether it passed, the attributes and reached set it ran with, how
//! long it took and, for a kept pass, what it read. The latest entry gives the expected
//! duration ([`super::queue`] runs the longest first) and the explanation of a rerun ([`Why`]).
//! The store never replaces a value, so the chain only grows; its end is found by doubling
//! and halving, a few gets however long it is. An entry is read in place ([`Entry`]): what a
//! kept pass read is decoded only to explain a rerun, never for a test that is skipped.

use std::{borrow::Cow, convert::Infallible, ffi::OsString, fmt, path::PathBuf, time::Duration};

use blaze_cas::{Corrupt, Key, Kind, Mismatch, Store, Ticket};
use blaze_core::{Traced, Untraceable};
use blaze_format::cursor::{Put, Reader};
use blaze_hash::{HashMap, HashSet};
use blaze_ids::{ContentId, Hasher, radix};
use blaze_items::CodeHash;

use crate::{
    Given, Reached, RunKey, RunKind,
    index::{self, FindError, Found, Shape},
    trace::{self, Inputs, Report},
};

/// Where passes and histories are kept: the store ([`Kind::Run`]), or a map in a test.
pub trait Passes {
    /// The value of `key`, verified.
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, Corrupt>;
    /// Keeps `bytes` under `key`: a second put of equal bytes is nothing, of other bytes an
    /// error.
    fn put(&mut self, key: Key, bytes: Vec<u8>) -> Result<Ticket, Mismatch>;
}

impl Passes for Store {
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, Corrupt> {
        Ok(Store::get(self, key)?.map(|entry| entry.bytes))
    }

    fn put(&mut self, key: Key, bytes: Vec<u8>) -> Result<Ticket, Mismatch> {
        Store::put(self, key, Kind::Run, bytes)
    }
}

#[derive(PartialEq, Eq, Hash, Debug)]
pub enum ShouldPanic {
    No,
    Yes,
    WithMessage(String),
}

/// What of a test's attributes changes what running it means.
#[derive(PartialEq, Eq, Hash, Debug)]
pub struct Attrs {
    pub ignore: bool,
    pub should_panic: ShouldPanic,
    pub bench: bool,
}

impl Default for Attrs {
    fn default() -> Attrs {
        Attrs { ignore: false, should_panic: ShouldPanic::No, bench: false }
    }
}

impl Attrs {
    pub fn id(&self) -> ContentId {
        let mut hasher = Hasher::new("blaze v2 test attributes 1");
        hasher.update(&[u8::from(self.ignore), u8::from(self.bench)]);
        match &self.should_panic {
            ShouldPanic::No => hasher.update(&[0]),
            ShouldPanic::Yes => hasher.update(&[1]),
            ShouldPanic::WithMessage(message) => hasher.update(&[2]).part(message.as_bytes()),
        };
        hasher.finish()
    }
}

/// A test as selection sees it. Made only by [`Test::of`] or [`Test::whole`], which work out
/// its identity once, so its fields are read-only: [`select`] and [`record`] each key by
/// the identity, and a test of 10,000 reached functions takes about 90 µs to hash.
#[derive(PartialEq, Eq, Debug)]
pub struct Test {
    name: String,
    reached: Vec<CodeHash>,
    attrs: Attrs,
    outside: bool,
    identity: ContentId,
}

/// The name of the test that stands for a whole binary run as libtest's threads.
pub const WHOLE: &str = "\0the whole binary";

impl Test {
    /// The test of what codegen found it reaches, its set moved, not copied.
    pub fn of(reached: Reached, attrs: Attrs, outside: bool) -> Test {
        Test::new(reached.test, reached.reached, attrs, outside)
    }

    fn new(name: String, reached: Vec<CodeHash>, attrs: Attrs, outside: bool) -> Test {
        let identity = identity_of(&name, &reached, &attrs);
        Test { name, reached, attrs, outside, identity }
    }

    /// The name libtest shows, the path from the crate's root.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The code hashes of what it reaches, in the order codegen's walk found them
    /// ([`Reached::reached`]): an explanation names the first that changed.
    pub fn reached(&self) -> &[CodeHash] {
        &self.reached
    }

    pub fn attrs(&self) -> &Attrs {
        &self.attrs
    }

    /// It reaches the outside world (files, the environment, processes, the network through
    /// code the walk can't see into), as codegen marks it: it always runs.
    pub fn outside(&self) -> bool {
        self.outside
    }

    /// The test, binary-independent: its name, the set of code hashes it reaches and its
    /// attributes, as worked out when it was made.
    pub fn identity(&self) -> ContentId {
        self.identity
    }

    /// The test that stands for `tests` run together in one process: it reaches what any of
    /// them reaches and is outside when any is. Which tests they are and their attributes
    /// are in [`Suite::given_whole`].
    pub fn whole(tests: &[Test]) -> Test {
        let mut reached = Vec::new(); // alloc: empty, allocates nothing
        let mut seen = HashSet::default();
        for test in tests {
            for hash in &test.reached {
                if seen.insert(*hash) {
                    reached.push(*hash);
                }
            }
        }
        let outside = tests.iter().any(|it| it.outside);
        Test::new(WHOLE.to_owned(), reached, Attrs::default(), outside) // alloc: once per binary run whole
    }
}

/// The identity of the test `name` that reaches `reached` with `attrs`: its name, its
/// attributes and the set of what it reaches, sorted and each once.
fn identity_of(name: &str, reached: &[CodeHash], attrs: &Attrs) -> ContentId {
    let mut hasher = Hasher::new("blaze v2 test identity 1");
    hasher.part(name.as_bytes()).id(&attrs.id());
    blaze_mem::scratch::with_u64s(|order| {
        set_order(reached, order);
        hasher.update(&(order.len() as u64).to_le_bytes());
        // 16 KiB an update, not 32 bytes: BLAKE3 hashes the whole 1 KiB chunks of one update
        // side by side, as many as its widest SIMD (16 with AVX-512), and a 32-byte update at
        // a time it cannot. On the stack, so no buffer of the set's size.
        let mut block = [0u8; 32 * 512];
        for group in order.chunks(512) {
            for (at, &it) in group.iter().enumerate() {
                block[32 * at..32 * at + 32].copy_from_slice(&reached[it as u32 as usize].0.0);
            }
            hasher.update(&block[..32 * group.len()]);
        }
    });
    hasher.finish()
}

/// `hashes` sorted, each once: for an explanation ([`explain`]), the edge.
fn sorted(hashes: &[CodeHash]) -> Vec<CodeHash> {
    blaze_mem::scratch::with_u64s(|order| {
        set_order(hashes, order);
        let mut set = Vec::with_capacity(order.len());
        set.extend(order.iter().map(|&it| hashes[it as u32 as usize]));
        set
    })
}

/// The set of `hashes` in order, each once, as indices into `hashes`: each in the low half of
/// an entry of `order`, which comes empty, a buffer the caller keeps (a worker's scratch). The
/// radix sort's buffer is the thread's scratch too, so a warm call below
/// [`radix::PARALLEL`] hashes allocates nothing.
///
/// Code hashes are uniform, so their first four bytes already order all but a few: a radix
/// sort on that prefix, linear where a comparison sort of 32-byte keys is `n log n`
/// compares, then the rare runs of one prefix by the whole hash.
fn set_order(hashes: &[CodeHash], order: &mut Vec<u64>) {
    let count = u32::try_from(hashes.len()).expect("a test reaches fewer than 2^32 functions");
    order.extend((0..count).map(|at| (u64::from(prefix(&hashes[at as usize])) << 32) | u64::from(at)));
    let hash = |it: &u64| &hashes[*it as u32 as usize];
    if order.len() < SMALL {
        order.sort_unstable_by(|a, b| hash(a).cmp(hash(b)));
    } else {
        blaze_mem::scratch::with_u64s(|scratch| radix::sort_copy_by_key_in(order, |it| (*it >> 32) as u32, scratch));
        let mut start = 0;
        while start < order.len() {
            let mut end = start + 1;
            while end < order.len() && order[end] >> 32 == order[start] >> 32 {
                end += 1;
            }
            if end - start > 1 {
                order[start..end].sort_unstable_by(|a, b| hash(a).cmp(hash(b)));
            }
            start = end;
        }
    }
    order.dedup_by(|a, b| hash(a) == hash(b));
}

/// Below this many hashes a comparison sort beats a radix sort's passes.
const SMALL: usize = 64;

/// The first four bytes of `hash`, ordered as the hash is.
fn prefix(hash: &CodeHash) -> u32 {
    let bytes = &hash.0.0;
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// Which test binary a test is of, without the binary: the same test of another package or
/// target is another test. `context` is the hash of everything outside the reached code that
/// what the binary does depends on (the target triple, the toolchain, the codegen settings
/// and panic strategy), made by whoever builds the binary; a key that misses one is a bug.
#[derive(PartialEq, Eq, Debug)]
pub struct Suite {
    pub package: String,
    /// The target as cargo names it: `lib`, `test:parse`, `bin:blaze`.
    pub target: String,
    pub context: ContentId,
}

impl Suite {
    fn base(&self) -> Given {
        let mut given = Given::new(RunKind::Test);
        given
            .part("package", self.package.as_bytes())
            .part("target", self.target.as_bytes())
            .id("context", &self.context);
        given
    }

    /// What each test of the suite run in a process of its own is given.
    pub fn given(&self) -> ContentId {
        let mut given = self.base();
        given.part("mode", b"process");
        given.finish()
    }

    /// What `tests` run together as libtest's threads are given: which tests they are, with
    /// their attributes.
    pub fn given_whole(&self, tests: &[Test]) -> ContentId {
        let mut members: Vec<(&str, ContentId)> =
            tests.iter().map(|it| (it.name.as_str(), it.attrs.id())).collect(); // alloc: once per binary run whole
        members.sort();
        let mut hasher = Hasher::new("blaze v2 test members 1");
        for (name, attrs) in &members {
            hasher.part(name.as_bytes()).id(attrs);
        }
        let mut given = self.base();
        given.part("mode", b"threads").id("members", &hasher.finish());
        given.finish()
    }
}

/// Whether libtest's flags ask for a test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ignored {
    /// `#[ignore]` tests are reported ignored, not run (the default).
    Skip,
    /// `--ignored`: only they run.
    Only,
    /// `--include-ignored`: they run too.
    Include,
}

/// The names asked for, as libtest takes them.
#[derive(PartialEq, Eq, Debug)]
pub struct Filter {
    /// A test is asked for when its name holds any of these (equals one, with `exact`); all
    /// are when there are none.
    pub names: Vec<String>,
    pub exact: bool,
    /// `--skip`: never a test whose name holds one of these (equals, with `exact`).
    pub skip: Vec<String>,
    pub ignored: Ignored,
}

impl Default for Filter {
    fn default() -> Filter {
        Filter { names: Vec::new(), exact: false, skip: Vec::new(), ignored: Ignored::Skip } // alloc: empty, allocates nothing
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wanted {
    Yes,
    /// Not asked for: not reported.
    Filtered,
    /// Reported ignored, not run.
    Ignored,
}

pub fn wanted(test: &Test, filter: &Filter) -> Wanted {
    asked(&test.name, &test.attrs, filter)
}

/// [`wanted`] of the test `name` with `attrs`, before its reached set is known (a listing).
pub fn asked(name: &str, attrs: &Attrs, filter: &Filter) -> Wanted {
    let matches = |pattern: &String| match filter.exact {
        true => name == pattern.as_str(),
        false => name.contains(pattern.as_str()),
    };
    if !filter.names.is_empty() && !filter.names.iter().any(matches) {
        return Wanted::Filtered;
    }
    if filter.skip.iter().any(matches) {
        return Wanted::Filtered;
    }
    match (filter.ignored, attrs.ignore) {
        (Ignored::Skip, true) => Wanted::Ignored,
        (Ignored::Only, false) => Wanted::Filtered,
        _ => Wanted::Yes,
    }
}

/// What a run read that is not as it was.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Input {
    File(PathBuf),
    Dir(PathBuf),
    Env(OsString),
}

/// Why a test runs (`--explain`).
#[derive(PartialEq, Eq, Debug)]
pub enum Why {
    /// It has never run here.
    New,
    /// Its last run failed, or passed with what can't be kept (it read the network, or what
    /// it read is not all known), or its pass is gone from the store.
    NoPass,
    /// It reaches the outside world.
    Outside,
    /// Every test was asked for.
    All,
    /// The first code hash it reaches, in the walk's order, that it did not reach when it
    /// last passed.
    Changed(CodeHash),
    /// It no longer reaches this, and reaches nothing new.
    Dropped(CodeHash),
    /// Its attributes (`#[ignore]`, `#[should_panic]`) are not as they were.
    Attributes,
    /// The same code, and something it read at run time is not as it was.
    Read(Input),
}

impl Why {
    /// The explanation, with a code hash named by `name_of` when it knows the function.
    pub fn describe(&self, name_of: &dyn Fn(&CodeHash) -> Option<String>) -> String { // dyn: the edge: an explanation for people
        let named = |hash: &CodeHash| name_of(hash).unwrap_or_else(|| format!("code {}", hash.0)); // alloc: the edge: an explanation for people
        match self {
            Why::New => "new".to_owned(), // alloc: the edge: an explanation for people
            Why::NoPass => "no pass kept".to_owned(), // alloc: the edge: an explanation for people
            Why::Outside => "outside world".to_owned(), // alloc: the edge: an explanation for people
            Why::All => "all tests asked for".to_owned(), // alloc: the edge: an explanation for people
            Why::Changed(hash) => format!("reaches {}, changed", named(hash)), // alloc: the edge: an explanation for people
            Why::Dropped(hash) => format!("no longer reaches {}", named(hash)), // alloc: the edge: an explanation for people
            Why::Attributes => "attributes changed".to_owned(), // alloc: the edge: an explanation for people
            Why::Read(Input::File(path)) => format!("read {}, changed", path.display()), // alloc: the edge: an explanation for people
            Why::Read(Input::Dir(path)) => format!("listed {}, changed", path.display()), // alloc: the edge: an explanation for people
            Why::Read(Input::Env(name)) => format!("read ${}, changed", name.to_string_lossy()), // alloc: the edge: an explanation for people
        }
    }
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe(&|_| None))
    }
}

#[derive(PartialEq, Eq, Debug)]
pub enum Decision {
    /// It runs; `expected` is how long it took when it last ran, if it has.
    Run { why: Why, expected: Option<Duration> },
    /// A pass is kept for it as it is now: it is reported unreached, and took this when it
    /// passed last.
    Skip { took: Option<Duration> },
}

#[derive(Debug)]
pub enum SelectError<E> {
    Corrupt(Corrupt),
    /// A value under a key of ours that is not what we keep there: a key of another schema.
    NotOurs { key: Key, why: String },
    Inputs(E),
}

impl<E> From<Corrupt> for SelectError<E> {
    fn from(it: Corrupt) -> SelectError<E> {
        SelectError::Corrupt(it)
    }
}

impl<E: fmt::Display> fmt::Display for SelectError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SelectError::Corrupt(it) => it.fmt(f),
            SelectError::NotOurs { key, why } => {
                write!(f, "the store's value of {:?} is not a test's: {why}", key.0)
            }
            SelectError::Inputs(it) => it.fmt(f),
        }
    }
}

/// Whether `test`, of the suite given `given` ([`Suite::given`], or [`Suite::given_whole`]
/// with [`Test::whole`]), runs, with every test asked for when `all`. `inputs` are the files
/// and environment as the test would see them now.
pub fn select<P: Passes + ?Sized, I: Inputs>(
    store: &P,
    given: ContentId,
    test: &Test,
    all: bool,
    inputs: &mut I,
) -> Result<Decision, SelectError<I::Error>> {
    let last = last(store, given, &test.name).map_err(widen)?.map(|(_, key, entry)| (key, entry));
    let expected = last.as_ref().map(|(_, it)| it.took);
    if test.outside {
        return Ok(Decision::Run { why: Why::Outside, expected });
    }
    if all {
        return Ok(Decision::Run { why: Why::All, expected });
    }
    let run = RunKey { code: test.identity(), given };
    if find(store, &run, inputs)? {
        return Ok(Decision::Skip { took: expected });
    }
    let why = explain(store, test, last, inputs)?;
    Ok(Decision::Run { why, expected })
}

fn widen<E>(error: SelectError<Infallible>) -> SelectError<E> {
    match error {
        SelectError::Corrupt(it) => SelectError::Corrupt(it),
        SelectError::NotOurs { key, why } => SelectError::NotOurs { key, why },
        SelectError::Inputs(never) => match never {},
    }
}

/// Whether a pass of `run` is kept for its inputs as they are now: [`index::find`]'s search,
/// over any [`Passes`].
fn find<P: Passes + ?Sized, I: Inputs>(
    store: &P,
    run: &RunKey,
    inputs: &mut I,
) -> Result<bool, SelectError<I::Error>> {
    match index::search(|key| store.get(key), run, inputs) {
        Ok(Found::Hit { value, .. }) if value == PASS => Ok(true),
        Ok(Found::Hit { key, .. }) => Err(SelectError::NotOurs { key, why: "not a test's pass".to_owned() }), // alloc: error path
        Ok(Found::Miss { .. }) => Ok(false),
        Err(FindError::Corrupt(corrupt)) => Err(corrupt.into()),
        Err(FindError::NotAShape { key, why }) => Err(SelectError::NotOurs { key, why }),
        Err(FindError::Inputs(error)) => Err(SelectError::Inputs(error)),
    }
}

/// Why `test` runs, against its history's last entry (under `key`).
fn explain<P: Passes + ?Sized, I: Inputs>(
    store: &P,
    test: &Test,
    last: Option<(Key, Entry<'_>)>,
    inputs: &mut I,
) -> Result<Why, SelectError<I::Error>> {
    let Some((key, last)) = last else { return Ok(Why::New) };
    if !last.passed || !last.kept {
        return Ok(Why::NoPass);
    }
    if last.attrs != test.attrs.id() {
        return Ok(Why::Attributes);
    }
    let Some(bytes) = store.get(&last.reached)? else { return Ok(Why::NoPass) };
    let before = decode_reached(bytes).map_err(|why| SelectError::NotOurs { key: last.reached, why })?;
    if let Some(hash) = test.reached.iter().find(|it| before.binary_search(*it).is_err()) {
        return Ok(Why::Changed(*hash));
    }
    let now = sorted(&test.reached);
    if let Some(hash) = before.iter().find(|it| now.binary_search(*it).is_err()) {
        return Ok(Why::Dropped(*hash));
    }
    let Some(traced) = last.traced().map_err(|why| SelectError::NotOurs { key, why })? else { return Ok(Why::NoPass) };
    let now = Shape::of(&traced).values(inputs).map_err(SelectError::Inputs)?;
    Ok(changed_input(&traced, &now).map_or(Why::NoPass, Why::Read))
}

/// The first input of `before` whose value `now` does not have.
pub fn changed_input(before: &Traced, now: &Traced) -> Option<Input> {
    let files: HashMap<_, _> = now.files.iter().map(|(path, hash)| (path, hash)).collect(); // alloc: the edge: an explanation for people, made only when asked
    let dirs: HashMap<_, _> = now.dirs.iter().map(|(path, hash)| (path, hash)).collect(); // alloc: the edge: an explanation for people, made only when asked
    let env: HashMap<_, _> = now.env.iter().map(|(name, value)| (name, value)).collect(); // alloc: the edge: an explanation for people, made only when asked
    for (path, hash) in &before.files {
        if files.get(path) != Some(&hash) {
            return Some(Input::File(path.clone())); // clone: the edge: an explanation for people, made only when asked
        }
    }
    for (path, hash) in &before.dirs {
        if dirs.get(path) != Some(&hash) {
            return Some(Input::Dir(path.clone())); // clone: the edge: an explanation for people, made only when asked
        }
    }
    for (name, value) in &before.env {
        if env.get(name) != Some(&value) {
            let name = match name == trace::ALL_ENV_NAME {
                true => OsString::from("*"),
                false => name.clone(), // clone: the edge: an explanation for people, made only when asked
            };
            return Some(Input::Env(name));
        }
    }
    None
}

/// How a run went, to record.
#[derive(Clone, Copy, Debug)]
pub struct Ran<'a> {
    pub passed: bool,
    pub took: Duration,
    /// What it read at run time ([`super::queue::Trace::traced`]); `None` when that is not
    /// known, and then no pass is kept.
    pub traced: Option<&'a Traced>,
}

#[derive(Debug)]
pub enum RecordError {
    Corrupt(Corrupt),
    /// The store has a value of a pass's key with other bytes: never kept over.
    Mismatch(Mismatch),
    /// The last entry of the test's history is not one.
    NotOurs { key: Key, why: String },
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordError::Corrupt(it) => it.fmt(f),
            RecordError::NotOurs { key, why } => {
                write!(f, "the store's value of {:?} is not a test's history: {why}", key.0)
            }
            RecordError::Mismatch(it) => write!(
                f,
                "the store has a test's pass under {:?} with other bytes ({:?}, not {:?})",
                it.key.0, it.kept, it.given
            ),
        }
    }
}

impl From<Corrupt> for RecordError {
    fn from(it: Corrupt) -> RecordError {
        RecordError::Corrupt(it)
    }
}

/// Whether a pass that read `traced` can be kept: not when it read the network or what it
/// read is not all known. One that read the clock or random numbers is kept, as libtest's
/// result of it would be taken: a test whose result depends on them is flaky either way.
pub fn keepable(traced: &Traced) -> bool {
    !traced.untraceable.iter().any(|it| matches!(it, Untraceable::Network | Untraceable::Descriptor))
}

/// Records how `test` ran: its pass, when it passed and that can be kept, and an entry of its
/// history. The tickets of what was put.
pub fn record<P: Passes + ?Sized>(
    store: &mut P,
    given: ContentId,
    test: &Test,
    ran: &Ran<'_>,
) -> Result<Vec<Ticket>, RecordError> {
    // A pass, its shape, the reached set and the history entry.
    let mut tickets = Vec::with_capacity(4);
    let traced: Option<Cow<'_, Traced>> = ran.traced.map(|it| match trace::normal(it) {
        true => Cow::Borrowed(it),
        false => {
            let mut it = it.clone(); // clone: only a trace not in normal order, which `trace::normal` says is rare
            trace::normalize(&mut it);
            Cow::Owned(it)
        }
    });
    let kept = ran.passed && !test.outside && traced.as_deref().is_some_and(keepable);
    if kept && let Some(traced) = traced.as_deref() {
        let run = RunKey { code: test.identity(), given };
        // alloc: the store's `put` owns the bytes it keeps.
        tickets.push(store.put(index::result_key(&run, traced), PASS.to_vec()).map_err(RecordError::Mismatch)?);
        let mut shape = Shape::of(traced).encode();
        let mut n = 0;
        loop {
            let at = index::slot(&run, n);
            match store.get(&at)? {
                Some(bytes) if bytes == shape.as_slice() => break,
                Some(_) => n += 1,
                None => match store.put(at, std::mem::take(&mut shape)) {
                    Ok(ticket) => {
                        tickets.push(ticket);
                        break;
                    }
                    // Another kept another shape here first: made again for the next slot.
                    Err(_) => {
                        shape = Shape::of(traced).encode();
                        n += 1;
                    }
                },
            }
        }
    }
    let set = blaze_mem::scratch::with_u64s(|order| {
        set_order(&test.reached, order);
        encode_reached(order.len(), order.iter().map(|&it| &test.reached[it as u32 as usize]))
    });
    let reached = Key(ContentId::of("blaze v2 test reached set 1", &set));
    if store.get(&reached)?.is_none() {
        tickets.push(store.put(reached, set).map_err(RecordError::Mismatch)?);
    }
    let report = match (kept, traced) {
        (true, Some(traced)) => trace::encode(&Report::Ran(Some(traced.into_owned()))),
        _ => Vec::new(), // alloc: empty, allocates nothing
    };
    let entry = || Entry { passed: ran.passed, kept, attrs: test.attrs.id(), reached, took: ran.took, traced: &report }.encode();
    let mut bytes = entry();
    let mut n = last(&*store, given, &test.name)
        .map_err(|error| match error {
            SelectError::Corrupt(it) => RecordError::Corrupt(it),
            SelectError::NotOurs { key, why } => RecordError::NotOurs { key, why },
            SelectError::Inputs(never) => match never {},
        })?
        .map_or(0, |it| it.0 + 1);
    loop {
        let at = history_slot(given, &test.name, n);
        match store.get(&at)? {
            Some(held) if held == bytes.as_slice() => break,
            Some(_) => n += 1,
            None => match store.put(at, std::mem::take(&mut bytes)) {
                Ok(ticket) => {
                    tickets.push(ticket);
                    break;
                }
                Err(_) => {
                    bytes = entry();
                    n += 1;
                }
            },
        }
    }
    Ok(tickets)
}

/// The value kept under a pass's key: the same bytes for every pass, so two runs that passed
/// with the same inputs keep one value.
const PASS: &[u8] = b"blaze v2 test pass 1";
const HISTORY: &[u8] = b"blaze v2 test history 1";
const REACHED: &[u8] = b"blaze v2 test reached 1";

/// One recorded run of a test, read in place from the store's bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Entry<'s> {
    passed: bool,
    /// A pass was kept for it.
    kept: bool,
    attrs: ContentId,
    /// The key of the set of code hashes it reached.
    reached: Key,
    took: Duration,
    /// What it read, for a kept pass, as [`trace::encode`] wrote it; empty for none.
    traced: &'s [u8],
}

impl<'s> Entry<'s> {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HISTORY.len() + 1 + 32 + 32 + 8 + 8 + self.traced.len());
        out.put_bytes(HISTORY);
        out.put_u8(u8::from(self.passed) | (u8::from(self.kept) << 1));
        out.put_bytes(&self.attrs.0);
        out.put_bytes(&self.reached.0.0);
        out.put_u64(self.took.as_micros().min(u128::from(u64::MAX)) as u64);
        out.put_bytes_u64(self.traced);
        out
    }

    /// What a kept pass read, decoded.
    fn traced(&self) -> Result<Option<Traced>, String> {
        if self.traced.is_empty() {
            return Ok(None);
        }
        match trace::decode(self.traced)? {
            Report::Ran(Some(traced)) => Ok(Some(traced)),
            _ => Err("a test's history holds a report with no record".to_owned()), // alloc: error path
        }
    }

    fn decode(bytes: &'s [u8]) -> Result<Entry<'s>, String> {
        let rest = bytes.strip_prefix(HISTORY).ok_or_else(|| "not a test's history".to_owned())?; // alloc: error path
        let mut at = Reader::new(rest);
        let flags = at.u8()?;
        let attrs = ContentId(*at.array()?);
        let reached = Key(ContentId(*at.array()?));
        let took = at.u64()?;
        let traced = at.bytes_u64()?;
        at.finish()?;
        Ok(Entry {
            passed: flags & 1 != 0,
            kept: flags & 2 != 0,
            attrs,
            reached,
            took: Duration::from_micros(took),
            traced,
        })
    }
}

/// The `count` hashes of `set`, in order, as the store keeps a reached set.
fn encode_reached<'h>(count: usize, set: impl Iterator<Item = &'h CodeHash>) -> Vec<u8> {
    let mut out = Vec::with_capacity(REACHED.len() + 8 + 32 * count);
    out.put_bytes(REACHED);
    out.put_u64(count as u64);
    for hash in set {
        out.put_bytes(&hash.0.0);
    }
    out
}

/// The sorted set [`encode_reached`] wrote.
fn decode_reached(bytes: &[u8]) -> Result<Vec<CodeHash>, String> {
    let rest = bytes.strip_prefix(REACHED).ok_or_else(|| "not a reached set".to_owned())?; // alloc: the edge: an explanation for people, made only when asked
    let mut at = Reader::new(rest);
    let count = at.count_u64(32)?;
    let hashes = at.take(32 * count)?;
    at.finish()?;
    Ok(hashes
        .chunks_exact(32)
        .map(|it| CodeHash(ContentId(it.try_into().expect("32 bytes"))))
        .collect()) // alloc: the edge: an explanation for people, made only when asked
}

fn history_slot(given: ContentId, name: &str, n: u32) -> Key {
    Key(Hasher::new("blaze v2 test history slot 1")
        .id(&given)
        .part(name.as_bytes())
        .update(&n.to_le_bytes())
        .finish())
}

/// The last entry of a test's history, its slot and its key. Slots are filled from 0 with no
/// gap (an entry is put only in the first free one), so the end is found by doubling then
/// halving; the last slot found filled is read where it was got.
fn last<'s, P: Passes + ?Sized>(
    store: &'s P,
    given: ContentId,
    name: &str,
) -> Result<Option<(u32, Key, Entry<'s>)>, SelectError<Infallible>> {
    let get = move |n: u32| store.get(&history_slot(given, name, n));
    let Some(mut held) = get(0)? else { return Ok(None) };
    // `low` is filled, its bytes `held`; `high` is empty, or `u32::MAX` and filled.
    let (mut low, mut high) = (0u32, 1u32);
    while let Some(bytes) = get(high)? {
        (low, held) = (high, bytes);
        if high == u32::MAX {
            break;
        }
        high = high.saturating_mul(2);
    }
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        match get(middle)? {
            Some(bytes) => (low, held) = (middle, bytes),
            None => high = middle,
        }
    }
    let key = history_slot(given, name, low);
    let entry = Entry::decode(held).map_err(|why| SelectError::NotOurs { key, why })?;
    Ok(Some((low, key, entry)))
}

#[cfg(test)]
mod tests;
