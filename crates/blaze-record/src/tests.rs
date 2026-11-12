//! The simulation of a record with its reads: seeded sequences of changes to what is read,
//! against a model that runs the work every time. Each property has a planted bug its oracle
//! has to catch; a failing seed replays.
//!
//! The work of the model reads as real work does: what it reads next depends on what it read,
//! so its read-lists change between runs, and a read that found nothing sends it elsewhere.
//! blaze-core's `sim::Sim` simulates files, sockets and a clock; this core has none of them,
//! so the simulation is the seeded generator that one uses (`blaze_rng`) over a store in
//! memory.

use core::cell::{Cell, RefCell};
use std::{collections::BTreeMap, format, string::String, vec, vec::Vec};

use blaze_ids::ContentId;
use blaze_rng::SplitMix64;

use crate::{
    AnswerKey, Bounds, Digest, Error, Filing, Found, Kept, Kind, LAYOUT, ListError, ListRef,
    Placed, Slot, Store, Walked, chain::first_open_for_tests, keep_answer, list_key, lookup, walk,
    write_list, write_list_with,
};

/// The things the work can read: each absent, or one of [`READS`] values.
const READS: usize = 6;
const BASES: u64 = 3;
const STEPS: u32 = 300;
const SEEDS: u64 = 200;

type World = [Option<u8>; READS];

/// A write-once store in memory. A put of other bytes under a held key is refused and counted.
#[derive(Default)]
struct Memory {
    kept: RefCell<BTreeMap<(Kind, ContentId), Vec<u8>>>,
    /// Every put that landed, in order.
    log: RefCell<Vec<(Kind, ContentId, Vec<u8>)>>,
    clashes: Cell<u32>,
}

impl Store for Memory {
    type Held = Vec<u8>;
    type Error = &'static str;

    fn get(&self, kind: Kind, key: &ContentId) -> Result<Option<Vec<u8>>, &'static str> {
        Ok(self.kept.borrow().get(&(kind, *key)).cloned())
    }

    fn put(&self, kind: Kind, key: ContentId, bytes: &[u8]) -> Result<(), &'static str> {
        let mut kept = self.kept.borrow_mut();
        match kept.get(&(kind, key)) {
            Some(held) if held == bytes => Ok(()),
            Some(_) => {
                self.clashes.set(self.clashes.get() + 1);
                Err("other bytes under this key")
            }
            None => {
                kept.insert((kind, key), bytes.to_vec());
                self.log.borrow_mut().push((kind, key, bytes.to_vec()));
                Ok(())
            }
        }
    }

    fn bytes(held: &Vec<u8>) -> &[u8] {
        held
    }
}

/// One run of the work: what it read (sorted by name, each once) and its answer.
struct Ran {
    reads: Vec<(u8, Option<u8>)>,
    answer: Vec<u8>,
}

/// The work, and the model: it reads the thing its base names, then the thing that one's
/// value names (the next one, where it found nothing), three reads at most. Its answer is
/// the trail, a function of the base and of the values of exactly the reads it made.
fn run(base: u8, world: &World) -> Ran {
    let mut reads: Vec<(u8, Option<u8>)> = Vec::new();
    let mut answer = vec![base];
    let mut at = base as usize % READS;
    for _ in 0..3 {
        if reads.iter().any(|&(read, _)| read as usize == at) {
            break;
        }
        let value = world[at];
        reads.push((at as u8, value));
        answer.push(at as u8);
        answer.push(value.map_or(0xff, |value| value));
        at = match value {
            None => (at + 1) % READS,
            Some(value) => value as usize % READS,
        };
    }
    reads.sort_unstable();
    Ran { reads, answer }
}

fn base_key(base: u8) -> ContentId {
    // No hash: a test's base key is its number, so the test adds no hash domain.
    ContentId([base; 32])
}

fn name(read: u8) -> [u8; 2] {
    [b'r', read]
}

fn digest(value: Option<u8>) -> Digest {
    match value {
        None => Digest::Nothing,
        Some(value) => Digest::Is(ContentId([value; 32])),
    }
}

fn digest_now(world: &World, name: &[u8]) -> Option<Digest> {
    Some(digest(world[name[1] as usize]))
}

/// A bug planted in the filer, for a control.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bug {
    None,
    /// A read is left out of the list, so of the answer's key.
    ReadLeftOut,
    /// A read that found nothing is not listed.
    NegativeDropped,
    /// The list is whole, and the answer's key leaves out the digest of its first read.
    AnswerKeyedWithoutARead,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    /// A lookup hit an answer the work would not give now.
    Stale,
    /// A result filed and not [`Placed::Full`] was not found by the next lookup.
    NotFound,
    /// Two answers under one key, or a put of other bytes under a held key.
    TwoValues,
    /// The store or a list refused: the text says what.
    Other,
}

/// The step a run failed at, how, and the error's text where there is one.
type Failed = (u32, Failure, String);

fn failed(step: u32, failure: Failure) -> Failed {
    (step, failure, String::new())
}

fn other(step: u32) -> impl Fn(Error<&'static str>) -> Failed {
    move |error| (step, Failure::Other, format!("{error}"))
}

/// Files what `ran` read and answered, as a caller of this crate does after a miss.
fn file(
    store: &Memory,
    bounds: Bounds,
    base: u8,
    ran: &Ran,
    bug: Bug,
    step: u32,
) -> Result<Placed, Failed> {
    let mut reads = ran.reads.clone();
    match bug {
        Bug::ReadLeftOut => _ = reads.pop(),
        Bug::NegativeDropped => reads.retain(|(_, value)| value.is_some()),
        Bug::None | Bug::AnswerKeyedWithoutARead => {}
    }
    let base = base_key(base);
    let mut list = Vec::new();
    write_list(&mut list, &base, b"", reads.iter().map(|&(read, _)| name(read)));
    let placed = Filing::new(base, bounds).place(store, &list).map_err(other(step))?;
    if placed == Placed::Full {
        return Ok(placed);
    }
    let mut key = AnswerKey::new(&list);
    for (n, &(_, value)) in reads.iter().enumerate() {
        if bug == Bug::AnswerKeyedWithoutARead && n == 0 {
            continue;
        }
        key.value(digest(value));
    }
    match keep_answer(store, key.finish(), &ran.answer) {
        Ok(Kept::Written | Kept::Held) => Ok(placed),
        Err(Error::TwoAnswers { .. }) => Err(failed(step, Failure::TwoValues)),
        Err(error) => Err(other(step)(error)),
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
struct Stats {
    hits: u32,
    misses: u32,
    /// Filings that hit the bound.
    full: u32,
    /// Lookups that said the chain is at its bound.
    said_full: u32,
}

fn change(rng: &mut SplitMix64, world: &mut World) {
    if rng.one_in(4) {
        return;
    }
    let at = rng.index(READS);
    world[at] = match rng.below(READS as u64 + 2) {
        // Absent as often as two of the values: negatives turn positive and back.
        0 | 1 => None,
        value => Some((value - 2) as u8),
    };
}

/// One seeded run. `always_file`: no lookup, every result is filed, as two daemons that both
/// ran the work file theirs.
fn drive(
    seed: u64,
    bounds: Bounds,
    bug: Bug,
    always_file: bool,
) -> Result<(Stats, Memory), Failed> {
    let mut rng = SplitMix64::new(seed);
    let store = Memory::default();
    let mut world: World = [None; READS];
    let mut stats = Stats::default();
    for step in 0..STEPS {
        change(&mut rng, &mut world);
        let base = rng.below(BASES) as u8;
        let want = run(base, &world);
        if !always_file {
            let found = lookup(&store, bounds, &base_key(base), |name| digest_now(&world, name));
            match found.map_err(other(step))? {
                Found::Hit { answer, .. } => {
                    stats.hits += 1;
                    if answer != want.answer {
                        return Err(failed(step, Failure::Stale));
                    }
                    continue;
                }
                Found::Miss { full, .. } => {
                    stats.misses += 1;
                    stats.said_full += u32::from(full);
                }
            }
        }
        if file(&store, bounds, base, &want, bug, step)? == Placed::Full {
            stats.full += 1;
            continue;
        }
        if bug == Bug::None {
            let again = lookup(&store, bounds, &base_key(base), |name| digest_now(&world, name));
            match again.map_err(other(step))? {
                Found::Hit { answer, .. } if answer == want.answer => {}
                Found::Hit { .. } => return Err(failed(step, Failure::Stale)),
                Found::Miss { .. } => return Err(failed(step, Failure::NotFound)),
            }
        }
        if store.clashes.get() != 0 {
            return Err(failed(step, Failure::TwoValues));
        }
    }
    Ok((stats, store))
}

/// The chains the simulation runs under: one that never fills in a run, one a run fills.
const ROOMY: Bounds = Bounds::new(4, 64);
const TIGHT: Bounds = Bounds::new(2, 2);

/// The first seed under which `bug` fails, and how.
fn first_failure(bounds: Bounds, bug: Bug, always_file: bool) -> Option<(u64, Failed)> {
    (0..SEEDS).find_map(|seed| drive(seed, bounds, bug, always_file).err().map(|why| (seed, why)))
}

/// The refinement: a lookup's hit equals what running the work now gives (the model runs it
/// every time), under random changes to what is read, read-lists that change between runs and
/// absent reads turning present; and a result filed short of the bound is found by the next
/// lookup.
#[test]
fn a_hit_is_what_the_work_would_answer_now() {
    for bounds in [ROOMY, TIGHT] {
        let mut hits = 0;
        for seed in 0..SEEDS {
            match drive(seed, bounds, Bug::None, false) {
                Ok((stats, _)) => hits += stats.hits,
                Err(why) => panic!("seed {seed}, {bounds:?}: {why:?}"),
            }
        }
        assert!(hits > 0, "{bounds:?}: no lookup hit, so the oracle compared nothing");
    }
}

/// The control of the refinement: a read left out of the list is a stale hit.
#[test]
fn a_read_left_out_of_the_list_is_caught() {
    let caught = first_failure(ROOMY, Bug::ReadLeftOut, false);
    assert!(
        matches!(caught, Some((_, (_, Failure::Stale | Failure::TwoValues, _)))),
        "{caught:?}"
    );
}

/// The control of the refinement: an absent read not listed is a stale hit once it is there.
#[test]
fn a_negative_read_dropped_is_caught() {
    let caught = first_failure(ROOMY, Bug::NegativeDropped, false);
    assert!(
        matches!(caught, Some((_, (_, Failure::Stale | Failure::TwoValues, _)))),
        "{caught:?}"
    );
}

/// The invariant: the same sequence writes the same bytes under the same keys in the same
/// order (rule deterministic-output).
#[test]
fn the_same_sequence_writes_identical_bytes() {
    for seed in 0..20 {
        let (first_stats, first) = drive(seed, TIGHT, Bug::None, false).unwrap();
        let (again_stats, again) = drive(seed, TIGHT, Bug::None, false).unwrap();
        assert_eq!(first_stats, again_stats, "seed {seed}");
        assert_eq!(*first.log.borrow(), *again.log.borrow(), "seed {seed}");
        assert!(!first.log.borrow().is_empty(), "seed {seed}: nothing was written");
    }
    let (_, one) = drive(1, TIGHT, Bug::None, false).unwrap();
    let (_, two) = drive(2, TIGHT, Bug::None, false).unwrap();
    assert_ne!(*one.log.borrow(), *two.log.borrow(), "two seeds wrote alike: the log shows nothing");
}

/// The invariant: no put offers other bytes under a key the store holds, when every result is
/// filed whether or not the store had it (two writers of one result).
#[test]
fn no_put_offers_other_bytes_under_a_held_key() {
    for bounds in [ROOMY, TIGHT] {
        for seed in 0..SEEDS {
            match drive(seed, bounds, Bug::None, true) {
                Ok((_, store)) => assert_eq!(store.clashes.get(), 0, "seed {seed}"),
                Err(why) => panic!("seed {seed}, {bounds:?}: {why:?}"),
            }
        }
    }
}

/// The control of write-once: an answer keyed without one of its reads is two answers under
/// one key.
#[test]
fn an_answer_keyed_without_a_read_is_caught() {
    let caught = first_failure(ROOMY, Bug::AnswerKeyedWithoutARead, true);
    assert!(matches!(caught, Some((_, (_, Failure::TwoValues, _)))), "{caught:?}");
}

/// The invariant: at the bound the filing says so, the lookup says so, and every hit is still
/// what the work would answer.
#[test]
fn a_full_chain_is_counted_and_stays_correct() {
    let (mut full, mut said_full) = (0, 0);
    for seed in 0..SEEDS {
        match drive(seed, TIGHT, Bug::None, false) {
            Ok((stats, _)) => {
                full += stats.full;
                said_full += stats.said_full;
            }
            Err(why) => panic!("seed {seed}: {why:?}"),
        }
    }
    assert!(full > 0, "no run filled a chain of {TIGHT:?}: the bound was not tested");
    assert!(said_full >= full, "a filing hit the bound after a lookup that did not say so");
}

fn list_of(base: &ContentId, read: u8) -> Vec<u8> {
    let mut list = Vec::new();
    write_list(&mut list, base, b"", [name(read)]);
    list
}

/// The defect this form had with four slots: a fifth list of a base was never stored. Here it
/// goes to the next generation and is found; the bound is `width * generations` lists.
#[test]
fn a_list_past_the_first_generation_is_stored_and_found() {
    let bounds = Bounds::new(4, 2);
    let base = base_key(0);
    let store = Memory::default();
    let value = Some(7);
    for read in 0..9_u8 {
        let list = list_of(&base, read);
        let placed = Filing::new(base, bounds).place(&store, &list).unwrap();
        let want = match read {
            0..4 => Placed::Written(Slot { generation: 0, n: u32::from(read) }),
            4..8 => Placed::Written(Slot { generation: 1, n: u32::from(read) - 4 }),
            _ => Placed::Full,
        };
        assert_eq!(placed, want, "list {read}");
        if placed == Placed::Full {
            continue;
        }
        let mut key = AnswerKey::new(&list);
        key.value(digest(value));
        assert_eq!(keep_answer(&store, key.finish(), &[read]), Ok(Kept::Written));
        // Only this list's read has the value its answer was kept under.
        let now = |name: &[u8]| Some(digest(value.filter(|_| name[1] == read)));
        match lookup(&store, bounds, &base, now).unwrap() {
            Found::Hit { answer, .. } => assert_eq!(answer, [read]),
            miss => panic!("list {read} was filed and not found: {miss:?}"),
        }
        // Placed again, it is kept where it is.
        let again = Filing::new(base, bounds).place(&store, &list).unwrap();
        assert!(matches!(again, Placed::Kept(_)), "list {read}: {again:?}");
    }
    let nothing = lookup(&store, bounds, &base, |_| Some(Digest::Nothing)).unwrap();
    assert_eq!(nothing, Found::Miss { lists: 8, full: true });
}

/// The invariant: a walk shows its visitor the lists a lookup searches, in the lookup's
/// order, each with its slot and its own bytes; a list the visitor passes over is followed by
/// the next, the visitor's `Some` ends the walk at that list's slot, and its error ends the
/// walk as that error.
#[test]
fn a_walk_shows_the_lists_a_lookup_searches_and_ends_where_its_visitor_says() {
    #[derive(Debug, PartialEq)]
    enum Stopped {
        Chain(Error<&'static str>),
        Visitor(u8),
    }
    impl From<Error<&'static str>> for Stopped {
        fn from(error: Error<&'static str>) -> Stopped {
            Stopped::Chain(error)
        }
    }
    let bounds = Bounds::new(2, 4);
    let base = base_key(1);
    let store = Memory::default();
    // Three generations of lists: 0 and 1 full, 2 open with one list.
    let lists: Vec<Vec<u8>> = (0..5_u8).map(|read| list_of(&base, read)).collect();
    for list in &lists {
        let placed = Filing::new(base, bounds).place(&store, list).unwrap();
        assert!(matches!(placed, Placed::Written(_)), "{placed:?}");
    }
    let slot = |generation, n| Slot { generation, n };
    // Generation 0, then the open one (2), then the one before it (1).
    let searched =
        [(slot(0, 0), 0), (slot(0, 1), 1), (slot(2, 0), 4), (slot(1, 0), 2), (slot(1, 1), 3)];

    let mut seen = Vec::new();
    let passed = walk(&store, bounds, &base, |at, bytes: &[u8], list| {
        assert_eq!(list.reads().collect::<Vec<_>>(), [&bytes[bytes.len() - 2..]]);
        seen.push((at, bytes.to_vec()));
        Ok::<Option<()>, Stopped>(None)
    });
    assert_eq!(passed, Ok(Walked::Passed { lists: 5, full: false }));
    let want: Vec<(Slot, Vec<u8>)> =
        searched.iter().map(|&(at, list)| (at, lists[list].clone())).collect();
    assert_eq!(seen, want);

    // The visitor refuses the first list it could take and takes a later one.
    let mut asked = 0;
    let found = walk(&store, bounds, &base, |_, bytes: &[u8], _| {
        asked += 1;
        Ok::<Option<Vec<u8>>, Stopped>((asked >= 3).then(|| bytes.to_vec()))
    });
    assert_eq!(found, Ok(Walked::Found { found: lists[4].clone(), at: slot(2, 0) }));
    assert_eq!(asked, 3, "nothing is shown after the visitor's `Some`");

    let mut asked = 0;
    let stopped = walk(&store, bounds, &base, |_, _: &[u8], _| {
        asked += 1;
        match asked {
            2 => Err(Stopped::Visitor(9)),
            _ => Ok::<Option<()>, Stopped>(None),
        }
    });
    assert_eq!((stopped, asked), (Err(Stopped::Visitor(9)), 2));

    // The control: a lookup over the same chain reads the same five lists.
    let miss = lookup(&store, bounds, &base, |_| Some(Digest::Nothing));
    assert_eq!(miss, Ok(Found::Miss { lists: 5, full: false }));
}

/// A store that shows no put until the flush has ended, as blaze-cargo's does.
#[derive(Default)]
struct Buffered {
    puts: RefCell<Vec<(ContentId, Vec<u8>)>>,
}

impl Store for Buffered {
    type Held = Vec<u8>;
    type Error = &'static str;

    fn get(&self, _: Kind, _: &ContentId) -> Result<Option<Vec<u8>>, &'static str> {
        Ok(None)
    }

    fn put(&self, _: Kind, key: ContentId, bytes: &[u8]) -> Result<(), &'static str> {
        self.puts.borrow_mut().push((key, bytes.to_vec()));
        Ok(())
    }

    fn bytes(held: &Vec<u8>) -> &[u8] {
        held
    }
}

/// The invariant: two lists of one base placed by one filing take two slots, though the store
/// shows neither put. The control: a filing that forgets what it placed puts both under one
/// key.
#[test]
fn one_filing_places_two_lists_in_two_slots() {
    let base = base_key(1);
    let (one, two) = (list_of(&base, 1), list_of(&base, 2));
    let keys = |forgetful: bool| {
        let store = Buffered::default();
        let mut filing = Filing::new(base, ROOMY);
        if forgetful {
            filing = filing.planted_forgetful();
        }
        let first = filing.place(&store, &one).unwrap();
        let second = filing.place(&store, &two).unwrap();
        let again = filing.place(&store, &one).unwrap();
        let puts: Vec<ContentId> = store.puts.borrow().iter().map(|(key, _)| *key).collect();
        (first, second, again, puts)
    };
    let slot = |n| Slot { generation: 0, n };
    let (first, second, again, puts) = keys(false);
    assert_eq!(first, Placed::Written(slot(0)));
    assert_eq!(second, Placed::Written(slot(1)));
    assert_eq!(again, Placed::Kept(slot(0)));
    assert_eq!(puts, [list_key(&base, slot(0)), list_key(&base, slot(1))]);
    let (_, second, _, puts) = keys(true);
    assert_eq!(second, Placed::Written(slot(0)), "the control: the planted bug changed nothing");
    assert_eq!(puts[0], puts[1]);
}

/// The invariant: a read-list reads back as written; bytes of another layout, another base or
/// cut anywhere are refused by name and never read as a shorter list.
#[test]
fn a_read_list_round_trips_and_refuses_what_is_not_one() {
    let base = base_key(2);
    let names: [&[u8]; 3] = [b"", b"a name", &[0xff; 70]];
    let mut bytes = vec![9, 9];
    write_list(&mut bytes, &base, b"note", names);
    let list = &bytes[2..];
    let read = ListRef::open(list, &base).unwrap();
    assert_eq!(read.note(), b"note");
    assert_eq!(read.len(), 3);
    assert_eq!(read.reads().collect::<Vec<_>>(), names);
    let mut in_place = vec![9, 9];
    write_list_with(
        &mut in_place,
        &base,
        |out| {
            out.extend_from_slice(b"no");
            out.extend_from_slice(b"te");
        },
        names,
    );
    assert_eq!(in_place, bytes, "a note written in place gives the same list");

    assert_eq!(ListRef::open(list, &base_key(3)).err(), Some(ListError::OtherBase));
    for cut in 0..list.len() {
        assert_eq!(ListRef::open(&list[..cut], &base).err(), Some(ListError::Short), "cut {cut}");
    }
    let mut longer = list.to_vec();
    longer.push(0);
    assert_eq!(ListRef::open(&longer, &base).err(), Some(ListError::Trailing));

    let mut previous = list.to_vec();
    previous[..8].copy_from_slice(b"bzrecl00");
    let why = ListRef::open(&previous, &base).err().unwrap();
    assert_eq!(why, ListError::Layout(u64::from_le_bytes(*b"bzrecl00")));
    assert_eq!(format!("{why}"), "a read-list of layout 00, where this blaze reads layout 01");
    let mut foreign = list.to_vec();
    foreign[..8].copy_from_slice(b"bzgoal1s");
    let why = ListRef::open(&foreign, &base).err().unwrap();
    assert!(format!("{why}").starts_with("not a read-list"), "{why}");
    assert_eq!(LAYOUT.to_le_bytes(), *b"bzrecl01");
}

/// The invariant: the open generation is the first not full, found in a number of reads
/// logarithmic in the bound.
#[test]
fn the_open_generation_is_found_in_logarithmic_reads() {
    for generations in 1..=70_u32 {
        // Generations `0..full_below` are full; generation 0 always is when this is asked.
        for full_below in 1..=generations {
            let mut probes = 0;
            let open = first_open_for_tests(generations, full_below, &mut probes);
            assert_eq!(open, full_below, "{generations} generations");
            let most = 2 * (u32::BITS - generations.leading_zeros()) + 1;
            assert!(probes <= most, "{probes} reads for {full_below} of {generations}");
        }
    }
    let mut probes = 0;
    assert_eq!(first_open_for_tests(1024, 1, &mut probes), 1);
    assert_eq!(probes, 1, "a base one list past its first generation costs one more read");
}
