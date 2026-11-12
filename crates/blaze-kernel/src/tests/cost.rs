//! What a hit costs, pinned: the probes and gets of the store and the reads the world is asked
//! to value, for each shape of step (docs/v2/plan/design/kernel.md, "The kernel as the home":
//! every front-end result is an entry, so a hit's cost is multiplied by every entry of a build).
//!
//! The store's own counts (`Memory`) and a world that counts are used, not the process's tally
//! ([`take_lookups`]): that one is shared by every test of the process.

use std::cell::Cell;

use blaze_cas::Kind;
use blaze_ids::ContentId;
use blaze_store_check::Setting;

use super::Memory;
use crate::*;

const SLOT: ReadKind = ReadKind(1);

/// Four facts, and how many times one was valued.
#[derive(Default)]
struct Counted {
    facts: [u8; 4],
    valued: Cell<usize>,
}

impl World for Counted {
    fn hash(&self, read: Read<'_>) -> Result<Seen, WorldError> {
        self.valued.set(self.valued.get() + 1);
        match (read.kind, read.key) {
            (SLOT, &[at]) if usize::from(at) < self.facts.len() => Ok(Seen::Is(ContentId::of(
                "blaze v2 kernel cost test fact 1",
                &[self.facts[usize::from(at)]],
            ))),
            _ => Err(WorldError::new(read, "no such fact")),
        }
    }

    fn pin(&self) -> Pin {
        Pin(ContentId::of("blaze v2 kernel cost test pin 1", &self.facts))
    }
}

struct Slot;

impl Fact<Counted> for Slot {
    const KIND: ReadKind = SLOT;
    type Key = u8;
    type Value<'w> = u8;

    fn key(key: &u8, out: &mut Vec<u8>) {
        out.push(*key);
    }

    fn get<'w>(world: &'w Counted, key: &u8) -> Result<(u8, Seen), WorldError> {
        let seen = world.hash(Read { kind: SLOT, key: &[*key] })?;
        Ok((world.facts[usize::from(*key)], seen))
    }
}

fn one_byte(key: &[u8]) -> Result<u8, String> {
    match key {
        [input] => Ok(*input),
        _ => Err(format!("{} bytes, where an input is one", key.len())),
    }
}

macro_rules! step {
    ($name:ident, $id:literal, $text:literal) => {
        struct $name;

        impl Step for $name {
            const ID: StepId = StepId($id);
            const NAME: &'static str = $text;
            const KIND: Kind = Kind::Query;
            type Input = u8;

            fn subject(input: &u8) -> Subject {
                Subject { of: ContentId::of($text, &[*input]), flags: ContentId::default() }
            }

            fn label(input: &u8) -> String {
                format!("{} {input}", $text)
            }
        }

        impl Ask for $name {
            fn key(input: &u8, out: &mut Vec<u8>) {
                out.push(*input);
            }

            fn input(key: &[u8]) -> Result<u8, String> {
                one_byte(key)
            }
        }
    };
}

step!(Bare, 0x7f31, "cost bare");
step!(Narrow, 0x7f32, "cost narrow");
step!(Pair, 0x7f33, "cost pair");
step!(Mixed, 0x7f34, "cost mixed");
step!(Fan, 0x7f35, "cost fan");

/// No reads: its entry is its bare value.
impl Compute for Bare {
    type World = Counted;

    fn compute(_: &mut Reads<'_, Counted>, input: &u8) -> Result<Vec<u8>, StepError> {
        Ok(vec![*input; 48])
    }
}

/// One read of the world.
impl Compute for Narrow {
    type World = Counted;

    fn compute(ctx: &mut Reads<'_, Counted>, input: &u8) -> Result<Vec<u8>, StepError> {
        Ok(vec![*input, ctx.get::<Slot>(input)?])
    }
}

/// One read of the world and two children with reads, the same two whatever its input.
impl Compute for Pair {
    type World = Counted;

    fn compute(ctx: &mut Reads<'_, Counted>, input: &u8) -> Result<Vec<u8>, StepError> {
        let mut out = vec![*input, ctx.get::<Slot>(&3)?];
        out.extend(ctx.step::<Narrow>(&0)?);
        out.extend(ctx.step::<Narrow>(&1)?);
        Ok(out)
    }
}

/// A child with no reads and a child with reads.
impl Compute for Mixed {
    type World = Counted;

    fn compute(ctx: &mut Reads<'_, Counted>, _: &u8) -> Result<Vec<u8>, StepError> {
        let mut out = ctx.step::<Bare>(&5)?;
        out.extend(ctx.step::<Narrow>(&0)?);
        Ok(out)
    }
}

/// Three parents that ask for the same two children.
impl Compute for Fan {
    type World = Counted;

    fn compute(ctx: &mut Reads<'_, Counted>, _: &u8) -> Result<Vec<u8>, StepError> {
        let mut out = Vec::new();
        for parent in 0..3 {
            out.extend(ctx.step::<Pair>(&parent)?);
        }
        Ok(out)
    }
}

fn kernel() -> Kernel<Counted> {
    let mut kernel = Kernel::new();
    kernel.register_ask::<Bare>(CodeId(ContentId([31; 32]))).unwrap();
    kernel.register_ask::<Narrow>(CodeId(ContentId([32; 32]))).unwrap();
    kernel.register_ask::<Pair>(CodeId(ContentId([33; 32]))).unwrap();
    kernel.register_ask::<Mixed>(CodeId(ContentId([34; 32]))).unwrap();
    kernel.register_ask::<Fan>(CodeId(ContentId([35; 32]))).unwrap();
    kernel
}

/// What a hit costs.
#[derive(Debug, PartialEq, Eq)]
struct Cost {
    probes: usize,
    gets: usize,
    /// Reads the world was asked to value.
    valued: usize,
}

/// The cost of a hit of `S` of 0: kept by a first get, then counted over a second, which must
/// be a hit of the same bytes.
fn hit<S: Compute<Input = u8, World = Counted>>() -> Cost {
    let kernel = kernel();
    let mut store = Memory::default();
    let world = Counted { facts: [4, 5, 6, 7], valued: Cell::new(0) };
    let first = kernel.get::<S, _>(&mut store, &world, &0, &Setting::OFF).unwrap().value().to_vec();
    store.gets.set(0);
    store.probes.set(0);
    world.valued.set(0);
    let got = kernel.get::<S, _>(&mut store, &world, &0, &Setting::OFF).unwrap();
    assert!(got.is_hit(), "the second get of {} computed again", S::NAME);
    assert_eq!(got.value(), first);
    Cost { probes: store.probes.get(), gets: store.gets.get(), valued: world.valued.get() }
}

/// No reads: the probe of its entry and the get of it. Nothing is valued and no list is read.
#[test]
fn a_hit_with_no_reads_is_one_probe_and_one_get() {
    assert_eq!(hit::<Bare>(), Cost { probes: 1, gets: 1, valued: 0 });
}

/// Reads of the world: the probe that finds no entry with no reads, the one list, each read
/// valued once, the probe of the answer and the one get of it.
#[test]
fn a_hit_with_reads_is_its_list_and_one_get_of_its_answer() {
    assert_eq!(hit::<Narrow>(), Cost { probes: 2, gets: 2, valued: 1 });
}

/// Children with reads: the parent as above (2 probes, its list, its answer: 2 gets, 1 read),
/// and each child one get that finds no entry with no reads, its list and one get of its
/// answer (3 gets, 1 read), whose kept hash the parent's read sees. One walk: no list is read
/// twice and no read valued twice.
#[test]
fn a_hit_that_names_children_looks_each_up_once_in_one_walk() {
    assert_eq!(hit::<Pair>(), Cost { probes: 2, gets: 2 + 3 + 3, valued: 1 + 1 + 1 });
}

/// A child with no reads is one get: its bare value, which is hashed (an entry with no reads
/// has no header to keep a hash in).
#[test]
fn a_child_with_no_reads_is_one_get() {
    assert_eq!(hit::<Mixed>(), Cost { probes: 2, gets: 2 + 1 + 3, valued: 1 });
}

/// A child asked for by three parents in one request is looked up once: the first parent pays
/// for its two children (2 + 3 + 3 gets, 3 reads), the other two only for themselves (their
/// get that finds no entry with no reads, their list, their answer: 3 gets, and their own read
/// of the world).
#[test]
fn a_child_of_many_parents_is_looked_up_once_a_request() {
    assert_eq!(hit::<Fan>(), Cost { probes: 2, gets: 2 + (3 + 3 + 3) + 3 + 3, valued: 3 + 1 + 1 });
}

/// The kept hash is the value's: what a parent's read of a child saw at its run (the hash of
/// the bytes the run made) is what a later lookup reads from the child's entry.
#[test]
fn an_entry_keeps_the_hash_of_its_value() {
    let kernel = kernel();
    let mut store = Memory::default();
    let world = Counted { facts: [4, 5, 6, 7], valued: Cell::new(0) };
    kernel.get::<Narrow, _>(&mut store, &world, &0, &Setting::OFF).unwrap();
    let entry = kernel.find::<Narrow, _>(&store, &world, &0).unwrap().expect("the entry kept");
    assert_eq!(entry.value_hash(), blaze_cas::content_of(entry.value));
    let bare = kernel.get::<Bare, _>(&mut store, &world, &0, &Setting::OFF).unwrap();
    assert_eq!(bare.value(), vec![0_u8; 48]);
}

/// One get counted in `session`, which must hit.
fn hit_in<S: Compute<Input = u8, World = Counted>>(
    kernel: &Kernel<Counted>,
    session: &mut Session,
    store: &mut Memory,
    world: &Counted,
    input: u8,
) -> (Cost, Vec<u8>) {
    store.gets.set(0);
    store.probes.set(0);
    world.valued.set(0);
    let value = {
        let got =
            kernel.get_in::<S, _>(session, &mut *store, world, &input, &Setting::OFF).unwrap();
        assert!(got.is_hit(), "{} {input} computed again", S::NAME);
        got.value().to_vec()
    };
    (Cost { probes: store.probes.get(), gets: store.gets.get(), valued: world.valued.get() }, value)
}

/// In a session, a value found at the top before is one get: no probe, no list, no read
/// valued. And a child verified by one request is not looked up by the next: a second parent
/// of the same two children pays for itself alone (as a hit with one read: 2 probes, its list
/// and its answer, 1 read), where alone it pays 8 gets and 3 reads.
#[test]
fn a_session_verifies_a_value_once_over_its_requests() {
    let kernel = kernel();
    let mut store = Memory::default();
    let world = Counted { facts: [4, 5, 6, 7], valued: Cell::new(0) };
    for parent in 0..2 {
        kernel.get::<Pair, _>(&mut store, &world, &parent, &Setting::OFF).unwrap();
    }
    let mut session = Session::new(&world);
    let (first, value) = hit_in::<Pair>(&kernel, &mut session, &mut store, &world, 0);
    assert_eq!(first, Cost { probes: 2, gets: 2 + 3 + 3, valued: 3 });
    let (again, same) = hit_in::<Pair>(&kernel, &mut session, &mut store, &world, 0);
    assert_eq!(again, Cost { probes: 0, gets: 1, valued: 0 });
    assert_eq!(same, value);
    let (sibling, _) = hit_in::<Pair>(&kernel, &mut session, &mut store, &world, 1);
    assert_eq!(sibling, Cost { probes: 2, gets: 2, valued: 1 });
    // Dropped, it answers the same bytes at the cost of a session with nothing verified.
    session.clear();
    let (dropped, same) = hit_in::<Pair>(&kernel, &mut session, &mut store, &world, 0);
    assert_eq!(dropped, first);
    assert_eq!(same, value);
}

/// A world that moved under a session is refused, never answered from what the session
/// verified before; a session of the world as it is answers what a fresh run gives.
#[test]
fn a_world_that_moved_under_a_session_is_refused() {
    let kernel = kernel();
    let mut store = Memory::default();
    let mut world = Counted { facts: [4, 5, 6, 7], valued: Cell::new(0) };
    let mut session = Session::new(&world);
    let old = kernel
        .get_in::<Narrow, _>(&mut session, &mut store, &world, &0, &Setting::OFF)
        .unwrap()
        .value()
        .to_vec();
    assert_eq!(old, [0, 4]);
    world.facts[0] = 9;
    let refused = kernel
        .get_in::<Narrow, _>(&mut session, &mut store, &world, &0, &Setting::OFF)
        .map(|got| got.value().to_vec())
        .unwrap_err();
    assert!(matches!(refused, KernelError::Moved { .. }), "{refused}");
    let found = kernel.find_in::<Narrow, _>(&mut session, &store, &world, &0).map(|_| ());
    assert!(matches!(found, Err(KernelError::Moved { .. })), "{found:?}");
    let mut session = Session::new(&world);
    let now = kernel
        .get_in::<Narrow, _>(&mut session, &mut store, &world, &0, &Setting::OFF)
        .unwrap()
        .value()
        .to_vec();
    assert_eq!(now, [0, 9]);
}
