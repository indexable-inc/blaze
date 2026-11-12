//! The exhaustive small-scope check of the kernel's lookup rule (docs/v2/plan/design/
//! end-state.md, "The kernel is the home of all of this", point 4): every sequence of
//! [`STEPS`] steps over [`FACTS`] facts, by enumeration, not by draws.
//!
//! - **The world**: [`FACTS`] facts, each absent or naming a fact. It starts with every fact
//!   absent.
//! - **A step of a sequence**: one fact changed to one of its other three states (9 ways), or a
//!   get of the step under test from one of [`INPUTS`] inputs (2 ways): 11 ways at every
//!   point, so `11^STEPS` sequences ([`SEQUENCES`]), each prefix checked as it is made.
//! - **The step under test** ([`Hop`]) follows facts from its input, so which facts it reads
//!   depends on what it read: its read-lists change between runs, and a fact that is absent
//!   is a read of nothing.
//! - **The oracle**: what a get gives, hit or computed, is what the step makes of the world as
//!   it is now, worked out with no kernel ([`fresh`]); and what a get kept, a lookup straight
//!   after finds.
//! - **The controls**: the kernel's rule holds for a step that reads only through its context.
//!   Each planted bug ([`Plant`]) breaks that in one way, and the oracle has to find a
//!   sequence in scope that gives a stale value.
//!
//! The enumeration is a depth-first walk that undoes each step (a fact put back, the store's
//! puts taken out), so a sequence costs one step, not its length.

use std::alloc::Global;
use std::cell::Cell;

use blaze_cas::{Key, Kind, Mismatch, content_of};
use blaze_ids::ContentId;
use blaze_store_check::Setting;

use super::*;

mod nested;

/// How many facts the world has (K).
const FACTS: usize = 3;
/// How many steps a sequence has (N).
const STEPS: u32 = 5;
/// How many inputs the step is asked for: two identities over the same facts.
const INPUTS: usize = 2;
/// How many sequences are enumerated: 11 ways a step, [`STEPS`] steps.
const SEQUENCES: u64 = 161_051;

const FACT: ReadKind = ReadKind(1);
const HASHED: &str = "blaze v2 kernel exhaustive fact 1";
/// What the step's value says of a fact that is absent.
const ABSENT: u8 = 0xff;

type World3 = [Option<u8>; FACTS];

struct Facts {
    facts: World3,
}

fn seen(at: u8, value: Option<u8>) -> Seen {
    match value {
        Some(value) => Seen::Is(ContentId::of(HASHED, &[at, value])),
        None => Seen::Gone,
    }
}

impl World for Facts {
    fn hash(&self, read: Read<'_>) -> Result<Seen, WorldError> {
        match (read.kind, read.key) {
            (FACT, &[at]) if usize::from(at) < FACTS => Ok(seen(at, self.facts[usize::from(at)])),
            _ => Err(WorldError::new(read, "the exhaustive world has no such read")),
        }
    }

    fn pin(&self) -> Pin {
        let mut bytes = [0_u8; 2 * FACTS];
        for (at, fact) in self.facts.iter().enumerate() {
            bytes[2 * at] = fact.is_some() as u8;
            bytes[2 * at + 1] = fact.unwrap_or(0);
        }
        Pin(ContentId::of("blaze v2 kernel exhaustive pin 1", &bytes))
    }
}

struct At;

impl Fact<Facts> for At {
    const KIND: ReadKind = FACT;
    type Key = u8;
    type Value<'w> = Option<u8>;

    fn key(key: &u8, out: &mut Vec<u8>) {
        out.push(*key);
    }

    fn get<'w>(world: &'w Facts, key: &u8) -> Result<(Option<u8>, Seen), WorldError> {
        let value = world.facts[usize::from(*key)];
        Ok((value, seen(*key, value)))
    }
}

/// One way for a step to read past its context. Each is a rule the lookup's correctness
/// rests on: every read is recorded, a read of nothing is a read, and the reads recorded are
/// this run's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// The last fact is read behind the context: a read left out.
    ReadLeftOut,
    /// A fact that is absent is not read through the context: a negative read dropped.
    NegativeDropped,
    /// A run records only the facts the first run of its input read: a stale list kept.
    StaleList,
}

/// What the step reads behind the kernel's back when a bug is planted: the world, and which
/// facts the first run of each input read (a bit a fact).
#[derive(Clone, Copy)]
struct Behind {
    plant: Plant,
    facts: World3,
    first: [Option<u8>; INPUTS],
}

thread_local! {
    static BEHIND: Cell<Behind> =
        const { Cell::new(Behind { plant: Plant::None, facts: [None; FACTS], first: [None; INPUTS] }) };
}

/// Follows facts from its input, at most [`FACTS`] hops: its value is the input, then each
/// fact's value on the way, ending at an absent one.
struct Hop;

impl Step for Hop {
    const ID: StepId = StepId(0x7f11);
    const NAME: &'static str = "test hop";
    const KIND: Kind = Kind::Query;
    type Input = u8;

    fn subject(input: &u8) -> Subject {
        Subject {
            of: ContentId::of("blaze v2 kernel exhaustive input 1", &[*input]),
            flags: ContentId::default(),
        }
    }

    fn label(input: &u8) -> String {
        format!("hop from {input}")
    }
}

impl Compute for Hop {
    type World = Facts;

    fn compute(ctx: &mut Reads<'_, Facts>, input: &u8) -> Result<Vec<u8>, StepError> {
        let mut behind = BEHIND.with(Cell::get);
        let first = behind.first[usize::from(*input)];
        let mut made = 0_u8;
        let mut out = vec![*input];
        let mut at = *input;
        for _ in 0..FACTS {
            let mirror = behind.facts[usize::from(at)];
            let through = match behind.plant {
                Plant::None => true,
                Plant::ReadLeftOut => usize::from(at) != FACTS - 1,
                Plant::NegativeDropped => mirror.is_some(),
                Plant::StaleList => first.is_none_or(|first| first & (1 << at) != 0),
            };
            let value = match through {
                true => {
                    made |= 1 << at;
                    ctx.get::<At>(&at)?
                }
                false => mirror,
            };
            match value {
                Some(next) => {
                    out.push(next);
                    at = next;
                }
                None => {
                    out.push(ABSENT);
                    break;
                }
            }
        }
        if first.is_none() {
            behind.first[usize::from(*input)] = Some(made);
            BEHIND.with(|it| it.set(behind));
        }
        Ok(out)
    }
}

/// What [`Hop`] makes of `input` in `facts`, with no kernel and no context: the oracle.
fn fresh(input: u8, facts: &World3) -> Vec<u8> {
    let mut out = vec![input];
    let mut at = input;
    for _ in 0..FACTS {
        match facts[usize::from(at)] {
            Some(next) => {
                out.push(next);
                at = next;
            }
            None => {
                out.push(ABSENT);
                break;
            }
        }
    }
    out
}

/// A store in memory that keeps the first value of a key, as blaze-cas does, and can take
/// back the puts made since a mark.
#[derive(Default)]
struct Undoable {
    values: blaze_hash::HashMap<Key, Vec<u8>, Global>,
    put: Vec<Key>,
}

impl Undoable {
    fn undo(&mut self, mark: usize) {
        for key in self.put.drain(mark..) {
            self.values.remove(&key);
        }
    }
}

impl Get for Undoable {
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        Ok(self.values.get(key).map(Vec::as_slice))
    }
}

impl Put for Undoable {
    fn put(&mut self, key: Key, _: Kind, bytes: Vec<u8>) -> Result<(), Mismatch> {
        match self.values.get(&key) {
            Some(kept) if *kept != bytes => {
                Err(Mismatch { key, kept: content_of(kept), given: content_of(&bytes) })
            }
            Some(_) => Ok(()),
            None => {
                self.values.insert(key, bytes);
                self.put.push(key);
                Ok(())
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Did {
    Set(u8, Option<u8>),
    Get(u8),
    /// The session dropped what it verified (`nested`).
    Drop,
    /// The session was carried to the world as it is (`nested`).
    Carry,
    /// Before the sequence: every value was kept in the store at the first world, by a
    /// session that is gone (`nested`).
    Warm,
}

/// What the oracle refused, with the sequence that led to it.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Failure {
    /// A get gave `gave` where the step makes `fresh` of the world as it is: a stale value.
    Stale { after: Vec<Did>, hit: bool, gave: Vec<u8>, fresh: Vec<u8> },
    /// What a get kept, the lookup straight after did not find.
    Lost { after: Vec<Did> },
    Kernel { after: Vec<Did>, why: String },
}

struct Explored {
    sequences: u64,
    hits: u64,
    computed: u64,
    failure: Option<Failure>,
}

struct Exploring {
    kernel: Kernel<Facts>,
    store: Undoable,
    world: Facts,
    path: Vec<Did>,
    sequences: u64,
    hits: u64,
    computed: u64,
}

impl Exploring {
    fn get(&mut self, input: u8) -> Result<(), Failure> {
        BEHIND.with(|it| it.set(Behind { facts: self.world.facts, ..it.get() }));
        let kernel_failed = |path: &[Did], why: KernelError| Failure::Kernel {
            after: path.to_vec(),
            why: why.to_string(),
        };
        let (hit, gave) =
            match self.kernel.get::<Hop, _>(&mut self.store, &self.world, &input, &Setting::OFF) {
                Ok(got) => (got.is_hit(), got.value().to_vec()),
                Err(why) => return Err(kernel_failed(&self.path, why)),
            };
        let fresh = fresh(input, &self.world.facts);
        if gave != fresh {
            return Err(Failure::Stale { after: self.path.clone(), hit, gave, fresh });
        }
        match hit {
            true => self.hits += 1,
            false => self.computed += 1,
        }
        match self.kernel.find::<Hop, _>(&self.store, &self.world, &input) {
            Ok(Some(found)) if found.value == gave.as_slice() => Ok(()),
            Ok(_) => Err(Failure::Lost { after: self.path.clone() }),
            Err(why) => Err(kernel_failed(&self.path, why)),
        }
    }

    /// Every sequence that goes on from here for `left` more steps.
    fn go(&mut self, left: u32) -> Result<(), Failure> {
        if left == 0 {
            self.sequences += 1;
            return Ok(());
        }
        for at in 0..FACTS {
            let was = self.world.facts[at];
            let states = std::iter::once(None).chain((0..FACTS as u8).map(Some));
            for now in states.filter(|now| *now != was) {
                self.world.facts[at] = now;
                self.path.push(Did::Set(at as u8, now));
                self.go(left - 1)?;
                self.path.pop();
            }
            self.world.facts[at] = was;
        }
        for input in 0..INPUTS as u8 {
            let mark = self.store.put.len();
            let first = BEHIND.with(Cell::get).first;
            self.path.push(Did::Get(input));
            self.get(input)?;
            self.go(left - 1)?;
            self.path.pop();
            self.store.undo(mark);
            BEHIND.with(|it| it.set(Behind { first, ..it.get() }));
        }
        Ok(())
    }
}

fn explore(plant: Plant) -> Explored {
    BEHIND.with(|it| it.set(Behind { plant, facts: [None; FACTS], first: [None; INPUTS] }));
    let mut kernel = Kernel::new();
    kernel.register::<Hop>(CodeId(ContentId([5; 32]))).unwrap();
    let mut exploring = Exploring {
        kernel,
        store: Undoable::default(),
        world: Facts { facts: [None; FACTS] },
        path: Vec::new(),
        sequences: 0,
        hits: 0,
        computed: 0,
    };
    let failure = exploring.go(STEPS).err();
    Explored {
        sequences: exploring.sequences,
        hits: exploring.hits,
        computed: exploring.computed,
        failure,
    }
}

/// The first stale value a planted bug leads to; any other end of the walk fails the control.
fn stale(plant: Plant) -> (Vec<Did>, bool) {
    match explore(plant).failure {
        Some(Failure::Stale { after, hit, .. }) => (after, hit),
        other => panic!("{plant:?} was planted and the oracle found {other:?}"),
    }
}

/// The invariant: over every sequence of [`STEPS`] steps (a fact changed, made absent or
/// present, or a get from either input) on [`FACTS`] facts, a get gives what the step makes
/// of the world as it is now, hit or computed, and a lookup straight after a get finds what
/// it kept. 161,051 sequences, each prefix checked.
#[test]
fn every_sequence_in_scope_gives_what_a_fresh_run_gives() {
    let explored = explore(Plant::None);
    assert_eq!(explored.failure, None);
    assert_eq!(explored.sequences, SEQUENCES);
    assert_eq!(SEQUENCES, 11_u64.pow(STEPS));
    // The scope is not vacuous: gets hit and gets compute, many times each.
    assert!(
        explored.hits > 100 && explored.computed > 100,
        "{} hits and {} computed",
        explored.hits,
        explored.computed
    );
}

/// The control of "every read is recorded": a step that reads one fact behind its context
/// gives a stale hit within the scope.
#[test]
fn a_read_left_out_is_caught() {
    let (after, hit) = stale(Plant::ReadLeftOut);
    assert!(hit, "the stale value after {after:?} was computed, not a hit");
}

/// The control of "a read of nothing is a read": a step that does not record a fact it found
/// absent gives a stale hit once the fact is there.
#[test]
fn a_negative_read_dropped_is_caught() {
    let (after, hit) = stale(Plant::NegativeDropped);
    assert!(hit, "the stale value after {after:?} was computed, not a hit");
}

/// The control of "the reads recorded are this run's": a step that keeps recording the list
/// of its first run gives a stale hit once its reads go elsewhere.
#[test]
fn a_stale_list_kept_is_caught() {
    let (after, hit) = stale(Plant::StaleList);
    assert!(hit, "the stale value after {after:?} was computed, not a hit");
    assert!(after.len() >= 4, "a stale list needs a run before it and a change: {after:?}");
}
