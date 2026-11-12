//! What a hit costs, pinned: the probes and gets of the store and the reads the world is asked
//! to value, for each shape of step (docs/v2/plan/design/kernel.md, "The kernel as the home":
//! every front-end result is an entry, so a hit's cost is multiplied by every entry of a build).
//!
//! The store's own counts (`Memory`) and a world that counts are used, not the process's tally
//! ([`take_lookups`]): that one is shared by every test of the process.

use std::cell::Cell;

use blaze_cas::{Key, Kind};
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

// What a carry costs (docs/v2/plan/design/kernel.md, "Carrying a session"): a session of 256
// entries (240 leaves that each read one fact, 15 groups of 16 leaves, one whole of the groups),
// so that anything that touches every entry is seen at once: 240 reads valued, hundreds of gets.

const TILE: ReadKind = ReadKind(2);
const DIGEST: ReadKind = ReadKind(3);
const LEAVES: u8 = 240;
const GROUPS: u8 = 15;

/// 256 facts, the last 16 read by no step, and how many times one was valued.
struct Wide {
    facts: Vec<u8>,
    valued: Cell<usize>,
}

impl World for Wide {
    fn hash(&self, read: Read<'_>) -> Result<Seen, WorldError> {
        self.valued.set(self.valued.get() + 1);
        match (read.kind, read.key) {
            (TILE, &[at]) => Ok(Seen::Is(ContentId::of(
                "blaze v2 kernel cost test tile 1",
                &[self.facts[usize::from(at)]],
            ))),
            // A group's interface digest: of its 16 facts halved, which is all a leaf's
            // value shows of a fact. It ignores each fact's low bit, as no value can see it.
            (DIGEST, &[group]) => {
                let from = usize::from(group) * 16;
                let halves: Vec<u8> = self.facts[from..from + 16].iter().map(|it| it / 2).collect();
                Ok(Seen::Is(ContentId::of("blaze v2 kernel cost test digest 1", &halves)))
            }
            _ => Err(WorldError::new(read, "no such fact")),
        }
    }

    fn cover<'r>(&'r self, read: Read<'r>) -> Option<Read<'r>> {
        static GROUPS_BY_NUMBER: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        match (read.kind, read.key) {
            (TILE, &[at]) => {
                let group = usize::from(at) / 16;
                Some(Read { kind: DIGEST, key: &GROUPS_BY_NUMBER[group..group + 1] })
            }
            _ => None,
        }
    }

    fn pin(&self) -> Pin {
        Pin(ContentId::of("blaze v2 kernel cost test wide pin 1", &self.facts))
    }
}

struct Tile;

impl Fact<Wide> for Tile {
    const KIND: ReadKind = TILE;
    type Key = u8;
    type Value<'w> = u8;

    fn key(key: &u8, out: &mut Vec<u8>) {
        out.push(*key);
    }

    fn get<'w>(world: &'w Wide, key: &u8) -> Result<(u8, Seen), WorldError> {
        let seen = world.hash(Read { kind: TILE, key: &[*key] })?;
        Ok((world.facts[usize::from(*key)], seen))
    }
}

step!(Leaf, 0x7f41, "cost leaf");
step!(Group, 0x7f42, "cost group");
step!(Whole, 0x7f43, "cost whole");

/// Half of one fact: two facts that halve the same leave its value as it was.
impl Compute for Leaf {
    type World = Wide;

    fn compute(ctx: &mut Reads<'_, Wide>, input: &u8) -> Result<Vec<u8>, StepError> {
        Ok(vec![*input, ctx.get::<Tile>(input)? / 2])
    }
}

/// Sixteen leaves.
impl Compute for Group {
    type World = Wide;

    fn compute(ctx: &mut Reads<'_, Wide>, input: &u8) -> Result<Vec<u8>, StepError> {
        let mut out = Vec::new();
        for leaf in 0..16 {
            out.extend(ctx.step::<Leaf>(&(input * 16 + leaf))?);
        }
        Ok(out)
    }
}

/// Every group.
impl Compute for Whole {
    type World = Wide;

    fn compute(ctx: &mut Reads<'_, Wide>, _: &u8) -> Result<Vec<u8>, StepError> {
        let mut out = Vec::new();
        for group in 0..GROUPS {
            out.extend(ctx.step::<Group>(&group)?);
        }
        Ok(out)
    }
}

fn wide_kernel() -> Kernel<Wide> {
    let mut kernel = Kernel::new();
    kernel.register_ask::<Leaf>(CodeId(ContentId([41; 32]))).unwrap();
    kernel.register_ask::<Group>(CodeId(ContentId([42; 32]))).unwrap();
    kernel.register_ask::<Whole>(CodeId(ContentId([43; 32]))).unwrap();
    kernel
}

/// A kernel, a store and a world, and a session that holds the whole and all it reads.
struct Built {
    kernel: Kernel<Wide>,
    store: Memory,
    world: Wide,
    session: Session,
}

fn built() -> Built {
    let kernel = wide_kernel();
    let mut store = Memory::default();
    let world = Wide { facts: vec![8; 256], valued: Cell::new(0) };
    let mut session = Session::new(&world);
    kernel.get_in::<Whole, _>(&mut session, &mut store, &world, &0, &Setting::OFF).unwrap();
    assert_eq!(session.len(), usize::from(LEAVES) + usize::from(GROUPS) + 1);
    Built { kernel, store, world, session }
}

impl Built {
    /// Sets fact `at` to `to`: the exact change between the two pins.
    fn set(&mut self, at: u8, to: u8) -> Changed {
        let from = self.world.pin();
        self.world.facts[usize::from(at)] = to;
        let mut changed = Changed::between(from, self.world.pin());
        changed.differs(Read { kind: TILE, key: &[at] });
        changed
    }

    fn reset(&self) {
        self.store.gets.set(0);
        self.store.probes.set(0);
        self.world.valued.set(0);
    }

    fn cost(&self) -> Cost {
        Cost {
            probes: self.store.probes.get(),
            gets: self.store.gets.get(),
            valued: self.world.valued.get(),
        }
    }

    /// The carry, and what it cost.
    fn carry(&mut self, changed: &Changed) -> (Carried, Cost) {
        self.reset();
        let carried = self
            .kernel
            .carry(&mut self.session, &mut self.store, &self.world, changed, &Setting::OFF)
            .unwrap();
        (carried, self.cost())
    }

    /// A get of the whole in the session: whether it hit, its bytes, what it cost.
    fn whole(&mut self) -> (bool, Vec<u8>, Cost) {
        self.reset();
        let (hit, value) = {
            let got = self
                .kernel
                .get_in::<Whole, _>(&mut self.session, &mut self.store, &self.world, &0, &Setting::OFF)
                .unwrap();
            (got.is_hit(), got.value().to_vec())
        };
        (hit, value, self.cost())
    }

    /// The whole as a computation in an empty store with no session gives it now.
    fn fresh(&self) -> Vec<u8> {
        let world = Wide { facts: self.world.facts.clone(), valued: Cell::new(0) };
        let mut fresh = Memory::default();
        let got = self.kernel.get::<Whole, _>(&mut fresh, &world, &0, &Setting::OFF);
        got.unwrap().value().to_vec()
    }
}

impl Built {
    /// Saves the session: its root's key, and how many records the save wrote.
    fn save(&mut self) -> (Key, usize) {
        let had = self.store.values.len();
        let root = self.session.save(&mut self.store).unwrap();
        (root, self.store.values.len() - had)
    }

    /// The bytes of every record in the store: what the store keeps beside its own overhead.
    fn bytes(&self) -> usize {
        self.store.values.values().map(Vec::len).sum()
    }

    /// Drops the session and opens it from `root`, as another process would: what that costs.
    fn reopen(&mut self, root: Key) -> Cost {
        self.reset();
        self.session = Session::open(&self.store, root).unwrap();
        self.cost()
    }
}

const NOTHING: Carried =
    Carried { dropped: None, seeded: 0, repaired: 0, changed: 0, cut: 0, gone: 0 };

/// A change no entry of the session read: the carry reads nothing of the store and values
/// nothing, and every entry is a session hit after it: one get of its entry, no probe, no
/// read-list read (so no list bytes hashed) and no read valued.
#[test]
fn a_carry_that_reaches_nothing_costs_nothing() {
    let mut built = built();
    let changed = built.set(255, 1);
    let (carried, cost) = built.carry(&changed);
    assert_eq!(carried, NOTHING);
    assert_eq!(cost, Cost { probes: 0, gets: 0, valued: 0 });
    assert_eq!(built.session.len(), 256);
    let (hit, value, cost) = built.whole();
    assert!(hit);
    assert_eq!(value, built.fresh());
    assert_eq!(cost, Cost { probes: 0, gets: 1, valued: 0 });
}

/// The cutoff: one fact of 240 changes and the one leaf that read it comes to the same bytes.
/// The carry looks up and runs that leaf alone (its fact is valued once by the lookup and
/// once by the run); its group, the whole and the 239 other leaves stay verified with no
/// work, and the whole is a session hit after it.
///
/// The leaf's 5 gets, against a floor of none (its run reads no entry, its keep is one put):
/// - the lookup, 3: its read-list in slot 0, the entry its reads name now (not there), slot 1
///   (empty: the end of its chain). The first two are what finds an entry another session or
///   an earlier pin kept; the third is owed: a row could keep how many lists its chain had.
/// - the keep, 2: `blaze_record::keep_answer` reads the entry's key before its put (the
///   lookup just saw it empty), and `Filing::place` reads slot 0 again to find the list
///   there. Both are owed: they are `blaze_record`'s, and `blaze-storemodel` counts each put
///   as a schedule point, so a put with no get before it is a change to that model too.
/// Not among them since the row says its identity has reads (`Session::listed`): a get of
/// the entry with no reads.
#[test]
fn a_change_that_leaves_a_value_the_same_stops_at_its_entry() {
    let mut built = built();
    let changed = built.set(37, 9);
    let (carried, cost) = built.carry(&changed);
    println!("cutoff carry: {carried:?} {cost:?}");
    assert_eq!(carried, Carried { seeded: 1, repaired: 1, cut: 1, ..NOTHING });
    assert_eq!(cost, Cost { probes: 0, gets: 3 + 2, valued: 2 });
    assert_eq!(built.session.len(), 256);
    let (hit, value, cost) = built.whole();
    assert!(hit);
    assert_eq!(value, built.fresh());
    assert_eq!(cost, Cost { probes: 0, gets: 1, valued: 0 });
}

/// One fact of 240 changes and its leaf's value with it: the carry repairs the leaf and its
/// group (the path), drops the whole (no step read it: it is looked up when asked for), and
/// the next get of the whole runs it from 15 groups the session holds. The 239 other leaves
/// and 14 other groups cost no read valued and no get of their own, in the carry or after:
/// the bounds are of the path (16 leaves read by the group's run, 15 groups by the whole's),
/// far under the 240 reads and hundreds of gets of looking every entry up again.
///
/// The carry's 26 gets: the leaf's 5 (as in the cutoff's test) and the group's 21: its
/// lookup's 3 (its 16 leaves are session rows: no get), the 16 leaves' entries read for the
/// run (the floor: a run reads its children's values), its keep's 2. The get of the whole
/// after it is 19 gets and 1 probe the same way: its list, the probe of the entry its reads
/// name now, slot 1; 15 groups' entries for the run; the keep's 2. So of 45 gets, 31 are the
/// floor's and 14 are not (2 a lookup that could find a kept entry, 1 the end of a chain, 2
/// the keep's, for each of three entries, less the top's probe in place of a get).
#[test]
fn a_change_costs_the_path_from_its_entry_to_the_top() {
    let mut built = built();
    let changed = built.set(37, 12);
    let (carried, cost) = built.carry(&changed);
    println!("path carry: {carried:?} {cost:?}");
    assert_eq!(carried, Carried { seeded: 1, repaired: 2, changed: 2, gone: 1, ..NOTHING });
    assert_eq!(cost, Cost { probes: 0, gets: 5 + (3 + 16 + 2), valued: 2 });
    assert_eq!(built.session.len(), 255);
    let (hit, value, cost) = built.whole();
    println!("path get of the top: {cost:?}");
    assert!(!hit, "the whole reads a group whose value changed");
    assert_eq!(value, built.fresh());
    assert_eq!(cost, Cost { probes: 1, gets: 2 + 15 + 2, valued: 0 });
    assert_eq!(built.session.len(), 256);
    let (hit, _, cost) = built.whole();
    assert!(hit);
    assert_eq!(cost, Cost { probes: 0, gets: 1, valued: 0 });
}

/// A kind with no exact difference: every entry with a read of it is looked up again, and
/// the cutoff still holds for each one whose value is the same.
///
/// 504 gets: each of the 239 leaves that come out the same is 2 (its read-list, and its
/// entry for the hash its group saw: the same entry the row had, so that get is owed: a row
/// that kept its key while queued would know the hash without it), the changed leaf 5 and
/// its group 21 as on the path. 241 reads valued: one a leaf's lookup, and the changed
/// leaf's run.
#[test]
fn a_kind_with_no_exact_difference_looks_up_every_entry_that_read_it() {
    let mut built = built();
    let from = built.world.pin();
    built.world.facts[37] = 12;
    let mut changed = Changed::between(from, built.world.pin());
    changed.everything(TILE);
    let (carried, cost) = built.carry(&changed);
    let expected =
        Carried { seeded: 240, repaired: 241, changed: 2, cut: 239, gone: 1, ..NOTHING };
    println!("everything carry: {carried:?} {cost:?}");
    assert_eq!(carried, expected);
    assert_eq!(cost, Cost { probes: 0, gets: 239 * 2 + 5 + 21, valued: 241 });
    let (_, value, _) = built.whole();
    assert_eq!(value, built.fresh());
}

/// A change set that is not total, or not from the session's pin, is refused by name and the
/// session is as it was; a kernel with other steps is refused at a request and drops the
/// session at a carry.
#[test]
fn a_carry_refuses_what_it_cannot_trust() {
    let mut built = built();
    let from = built.world.pin();
    built.world.facts[37] = 12;
    let to = built.world.pin();
    let carry = |built: &mut Built, kernel: &Kernel<Wide>, changed: &Changed| {
        kernel.carry(&mut built.session, &mut built.store, &built.world, changed, &Setting::OFF)
    };
    let kernel = wide_kernel();

    let unsaid = carry(&mut built, &kernel, &Changed::between(from, to)).unwrap_err();
    assert_eq!(unsaid, KernelError::Carry(CarryError::Unsaid { kind: TILE, names: 240 }));
    assert!(unsaid.to_string().contains("kind 2"), "{unsaid}");

    let mut other = Changed::between(to, to);
    other.exact(TILE);
    let refused = carry(&mut built, &kernel, &other).unwrap_err();
    assert_eq!(refused, KernelError::Carry(CarryError::From { session: from, from: to }));

    let mut steps = Changed::between(from, to);
    steps.exact(TILE);
    steps.differs(Read { kind: ReadKind::STEP, key: &[0x7f, 0x41, 37] });
    let refused = carry(&mut built, &kernel, &steps).unwrap_err();
    assert_eq!(refused, KernelError::Carry(CarryError::Steps));
    assert_eq!((built.session.pin(), built.session.len()), (from, 256));

    let mut more = wide_kernel();
    more.register_ask::<Bare2>(CodeId(ContentId([44; 32]))).unwrap();
    let refused = more
        .get_in::<Whole, _>(&mut built.session, &mut built.store, &built.world, &0, &Setting::OFF)
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(refused, KernelError::Reregistered { .. }), "{refused}");
    let mut exact = Changed::between(from, to);
    exact.differs(Read { kind: TILE, key: &[37] });
    let carried = carry(&mut built, &more, &exact).unwrap();
    assert_eq!(carried, Carried { dropped: Some(Dropped::Registry), ..NOTHING });
    assert_eq!((built.session.pin(), built.session.len()), (to, 0));
    let value = more
        .get_in::<Whole, _>(&mut built.session, &mut built.store, &built.world, &0, &Setting::OFF)
        .unwrap()
        .value()
        .to_vec();
    assert_eq!(value, built.fresh());
}

step!(Bare2, 0x7f44, "cost bare of wide");

impl Compute for Bare2 {
    type World = Wide;

    fn compute(_: &mut Reads<'_, Wide>, input: &u8) -> Result<Vec<u8>, StepError> {
        Ok(vec![*input])
    }
}

// The session as records of the store (docs/v2/plan/design/kernel.md, "Carrying a session"):
// what a save writes, what a session opened from its root reads, and rule 26's three tests.

/// Rule 26, the restore: a session saved, dropped and opened again answers and carries as the
/// one that was never saved: the same `Carried`, the same bytes, and the bytes of a fresh run.
/// Opened, it holds nothing but its root; the carry reads the changed fact's posting and the
/// rows on the path (and the rows of the 15 leaves its group's run asks for), never the
/// session.
///
/// Its gets against the 26 of the session in memory (the floor of an opened one is those and
/// one get for each record that holds something it needs): 21 rows and postings are looked for
/// (the fact's posting, the leaf's row and posting, the group's row and posting, the whole's
/// row, 15 leaves' rows), in three maps of a root and 16 leaves each. A node is read once a
/// carry (`trie::Nodes`), so they cost the nodes they are in: 2 of the facts' postings, 2 or 3
/// of the steps' postings, the rows' root and one leaf for each first nibble among 18
/// identities. With leaves of 8 and every path read from its root (layout 1) it was 89: the
/// 26 and 3 nodes for each of the 21.
#[test]
fn a_session_opened_from_the_store_carries_as_the_one_that_was_saved() {
    let mut kept = built();
    let changed = kept.set(37, 12);
    let (carried_kept, _) = kept.carry(&changed);
    let (hit_kept, value_kept, _) = kept.whole();

    let mut built = built();
    let (root, written) = built.save();
    println!("first save of 256 entries: {written} records");
    assert_eq!(written, 3 * 17 + 1, "three maps of a root and 16 leaves, and the root record");
    assert_eq!(built.save(), (root, 0), "a save of nothing changed writes nothing");
    assert_eq!(built.reopen(root), Cost { probes: 0, gets: 1, valued: 0 });
    assert_eq!((built.session.len(), built.session.held()), (256, (0, 0)));
    let changed = built.set(37, 12);
    let (carried, cost) = built.carry(&changed);
    println!("path carry after open: {carried:?} {cost:?} held {:?}", built.session.held());
    assert_eq!(carried, carried_kept);
    assert_eq!((cost.probes, cost.valued), (0, 2), "{cost:?}");
    // 17 nodes: 2 of the facts' postings, 2 or 3 of the steps', the rows' root, and the 11
    // or 12 leaves the 18 rows are in (which of the two the nibbles of their hashes decide).
    assert_eq!(cost.gets, 26 + 17, "{cost:?}");
    assert_eq!(built.session.held().0, 18, "the leaves of one group, the group and the whole");
    let (hit, value, _) = built.whole();
    assert_eq!((hit, &value), (hit_kept, &value_kept));
    assert_eq!(value, built.fresh());

    // And once more through the store: the carried session, opened, holds the top verified.
    let (root, _) = built.save();
    built.reopen(root);
    let (hit, value, cost) = built.whole();
    println!("get of the top after open: {cost:?}");
    assert!(hit);
    assert_eq!(value, built.fresh());
    assert_eq!(cost, Cost { probes: 0, gets: 2 + 1, valued: 0 }, "its row's root and leaf, its entry");
    assert_eq!(built.session.held(), (1, 0));
}

/// A save writes the paths of what changed, not the session: after a carry that repaired two
/// entries and dropped the top, three rows differ (no posting does: their lists were posted),
/// so the rows' root, the three leaves they are in, and the root record: 5 records, which is
/// the floor of a map kept as write-once records (a changed row is a new leaf, a new leaf a
/// new root). Layout 1 wrote 8 (a branch more a row) and 537 for the whole.
///
/// The disk floor (rule 67). Records: 52 for 256 entries, 0.2 an entry, beside the records
/// of the entries themselves (printed below), so the session adds that share to the store's
/// overhead a record. Bytes: the session's 73,642 against the entries' own 68,615 (287 and 268
/// an entry): 107% of them, where the bar is a tenth; what is owed to bring them down is in
/// kernel.md.
#[test]
fn a_save_writes_the_paths_of_what_changed() {
    let mut built = built();
    let own = built.bytes();
    println!("records of the 256 entries themselves: {}", built.store.values.len());
    let (root, whole) = built.save();
    let session = built.bytes() - own;
    println!(
        "bytes: the 256 entries' own {own} ({} an entry), the saved session {session} ({} an entry)",
        own / 256,
        session / 256
    );
    // 268 bytes an entry of its own, 287 of the session: 107%, where the bar is 10%.
    assert_eq!((own, session), (68_615, 73_642));
    assert_eq!(built.store.values.len(), 512 + 52, "an entry is two records: its list and itself");
    let changed = built.set(37, 12);
    built.carry(&changed);
    let (next, written) = built.save();
    println!("save: {whole} records for 256 entries, {written} after a carry that repaired 2");
    assert_ne!(next, root);
    assert_eq!((whole, written), (3 * 17 + 1, 1 + 3 + 1));
}

/// A session opened and carried over one changed fact whose leaf comes out the same reads
/// that fact's posting and that leaf's row (a root and a leaf each: 4 gets, which is the
/// floor less one, were a posting and a row in one record), and the leaf's lookup, run and
/// keep (5 gets, as with the session in memory): one row and two names are in memory after it.
#[test]
fn an_opened_session_reads_only_what_a_carry_reaches() {
    let mut built = built();
    let (root, _) = built.save();
    built.reopen(root);
    let changed = built.set(37, 9);
    let (carried, cost) = built.carry(&changed);
    println!("cutoff carry after open: {carried:?} {cost:?}");
    assert_eq!(carried, Carried { seeded: 1, repaired: 1, cut: 1, ..NOTHING });
    assert_eq!(cost, Cost { probes: 0, gets: 5 + 2 * 2, valued: 2 });
    assert_eq!(built.session.held(), (1, 2), "the leaf; the fact's name and the leaf's own");
    assert_eq!(built.session.len(), 256);
}

/// Rule 26, the previous layout: a root record of layout 1 (leaves of 8, lengths of four
/// bytes) is refused naming the layout, and so is a root the store does not have; neither
/// opens as an empty session. A node of layout 1 under a root of this layout is refused the
/// same way by the carry that reads it.
#[test]
fn a_saved_session_of_another_layout_is_refused_by_name() {
    let mut built = built();
    let (root, _) = built.save();
    let mut bytes = built.store.values.get(&root).unwrap().clone();
    assert_eq!(&bytes[..16], b"blaze kernel s 2");
    bytes[15] = b'1';
    let old = Key(ContentId::of("blaze v2 kernel cost test old root 1", &bytes));
    built.store.values.insert(old, bytes);
    let refused = Session::open(&built.store, old).map(|_| ()).unwrap_err();
    assert!(matches!(&refused, KernelError::Store { key, .. } if *key == old), "{refused}");
    assert!(refused.to_string().contains("blaze kernel s 1"), "{refused}");
    let nowhere = Key(ContentId([9; 32]));
    let refused = Session::open(&built.store, nowhere).map(|_| ()).unwrap_err();
    assert!(matches!(&refused, KernelError::Store { key, .. } if *key == nowhere), "{refused}");
    assert!(refused.to_string().contains("missing"), "{refused}");

    let node = built.session.posts_root(TILE).unwrap();
    let bytes = built.store.values.get_mut(&node).unwrap();
    assert_eq!(&bytes[..16], b"blaze kernel t 2");
    bytes[15] = b'1';
    built.reopen(root);
    let changed = built.set(37, 12);
    let refused = built
        .kernel
        .carry(&mut built.session, &mut built.store, &built.world, &changed, &Setting::OFF)
        .unwrap_err();
    assert!(matches!(&refused, KernelError::Store { key, .. } if *key == node), "{refused}");
    assert!(refused.to_string().contains("session node of layout 1"), "{refused}");
    assert_eq!(built.session.len(), 0);
}

/// A count in a session's records has one form (rule 25): what `put_var` writes is read back,
/// and the same number written longer, or one past 64 bits, is no record.
#[test]
fn a_count_in_a_session_record_has_one_form() {
    use crate::trie::{Cursor, put_var};
    for value in [0, 1, 127, 128, 16_383, 16_384, u64::from(u32::MAX), u64::MAX] {
        let mut bytes = Vec::new();
        put_var(&mut bytes, value);
        let mut input = Cursor::new(&bytes);
        assert_eq!((input.var(), input.ended()), (Some(value), true), "{value}");
    }
    assert_eq!(Cursor::new(&[0x7f]).var(), Some(127));
    assert_eq!(Cursor::new(&[0x80, 0x00]).var(), None, "0 written in two bytes");
    assert_eq!(Cursor::new(&[0xff; 9]).var(), None, "ends before its last byte");
    let mut past = [0xff; 10];
    past[9] = 0x02;
    assert_eq!(Cursor::new(&past).var(), None, "a bit past the 64th");
}

/// Rule 26, the lost record: a posting node the store no longer has is an error naming the
/// node at the carry that needs it, never read as "no readers" (which would leave the leaf
/// that read the changed fact verified, and stale). The session is dropped whole, and its
/// holder's rebuild is a new session: lookups, and the same bytes.
#[test]
fn a_posting_node_the_store_lost_is_an_error_naming_it() {
    let mut built = built();
    let (root, _) = built.save();
    let node = built.session.posts_root(TILE).unwrap();
    assert!(built.store.values.remove(&node).is_some());
    built.reopen(root);
    let changed = built.set(37, 12);
    let refused = built
        .kernel
        .carry(&mut built.session, &mut built.store, &built.world, &changed, &Setting::OFF)
        .unwrap_err();
    assert!(matches!(&refused, KernelError::Store { key, .. } if *key == node), "{refused}");
    assert!(refused.to_string().contains("missing"), "{refused}");
    assert_eq!(built.session.len(), 0);
    built.session = Session::new(&built.world);
    let (_, value, _) = built.whole();
    assert_eq!(value, built.fresh());
}

// The cover (docs/v2/plan/design/kernel.md, "A covering digest"): a step whose one read is a
// group's interface digest and whose value is the manifest of the group and its 16 leaves.

/// The cover's step: a [`Step`] only. A cover is kept by the kernel, never run or asked for
/// by a step, so it has no `Compute` and no `Ask` (which [`step!`] would give it).
struct Crate;

impl Step for Crate {
    const ID: StepId = StepId(0x7f45);
    const NAME: &'static str = "cost cover of a group";
    const KIND: Kind = Kind::Query;
    type Input = u8;

    fn subject(input: &u8) -> Subject {
        Subject { of: ContentId::of("cost cover of a group", &[*input]), flags: ContentId::default() }
    }

    fn label(input: &u8) -> String {
        format!("cost cover of a group {input}")
    }
}

/// A kernel with the cover, a store that holds group 2, its leaves and the cover over the
/// 17, and the world they were made in.
fn covered() -> (Kernel<Wide>, Memory, Wide) {
    let mut kernel = wide_kernel();
    kernel.register_cover::<Crate>(CodeId(ContentId([45; 32]))).unwrap();
    let mut store = Memory::default();
    let world = Wide { facts: vec![8; 256], valued: Cell::new(0) };
    let mut session = Session::new(&world);
    kernel.get_in::<Group, _>(&mut session, &mut store, &world, &2, &Setting::OFF).unwrap();
    let mut members = crate::Members::default();
    for leaf in 32..48u8 {
        kernel.member::<Leaf, _>(&mut members, &mut session, &store, &leaf).unwrap();
    }
    // The group without its leaves' digest, and a leaf of another group, are refused by name.
    let other = [Read { kind: DIGEST, key: &[3] }];
    let refused =
        kernel.cover::<Crate, _>(&mut session, &mut store, &world, &2, &other, &members).unwrap_err();
    assert!(matches!(&refused, KernelError::Uncovered { .. }), "{refused}");
    assert!(refused.to_string().contains("under none of the cover's reads"), "{refused}");
    let mut alone = crate::Members::default();
    kernel.member::<Group, _>(&mut alone, &mut session, &store, &2).unwrap();
    let digest = [Read { kind: DIGEST, key: &[2] }];
    let refused =
        kernel.cover::<Crate, _>(&mut session, &mut store, &world, &2, &digest, &alone).unwrap_err();
    assert!(refused.to_string().contains("not a member of the cover"), "{refused}");
    kernel.member::<Group, _>(&mut members, &mut session, &store, &2).unwrap();
    kernel.cover::<Crate, _>(&mut session, &mut store, &world, &2, &digest, &members).unwrap();
    (kernel, store, world)
}

fn group_now(kernel: &Kernel<Wide>, world: &Wide) -> Vec<u8> {
    let world = Wide { facts: world.facts.clone(), valued: Cell::new(0) };
    let mut fresh = Memory::default();
    kernel.get::<Group, _>(&mut fresh, &world, &2, &Setting::OFF).unwrap().value().to_vec()
}

/// A new session seeded through a cover of 17 members values the cover's one read and none
/// of the members' 16, and reads the cover's records and none of the members' 34: the floor
/// is one read valued and one get (the manifest). Each member is then a session hit: one get.
#[test]
fn a_cover_seeds_its_members_with_none_of_their_reads_valued() {
    let (kernel, mut store, world) = covered();
    let mut session = Session::new(&world);
    (store.gets.set(0), store.probes.set(0), world.valued.set(0));
    let seeded = kernel.seed::<Crate, _>(&mut session, &store, &world, &2).unwrap();
    let cost = (store.probes.get(), store.gets.get(), world.valued.get());
    println!("seed through a cover of 17: {seeded:?} (probes, gets, valued) {cost:?}");
    assert_eq!(seeded, Some(crate::Seeded { members: 17, admitted: 17 }));
    assert_eq!(cost.2, 1, "the cover's digest, and no member's read");
    // To pin once it is printed: the cover's list, its entry, the entry again for its value.
    assert!(cost.1 >= 1 && cost.1 <= 4, "{cost:?}");
    assert_eq!(session.len(), 18);
    (store.gets.set(0), store.probes.set(0), world.valued.set(0));
    let got = kernel.get_in::<Group, _>(&mut session, &mut store, &world, &2, &Setting::OFF).unwrap();
    assert!(got.is_hit());
    assert_eq!(got.value(), group_now(&kernel, &world));
    drop(got);
    assert_eq!((store.probes.get(), store.gets.get(), world.valued.get()), (0, 1, 0));
    assert_eq!(kernel.seed::<Crate, _>(&mut session, &store, &world, &2).unwrap().map(|it| it.admitted), Some(0));
    let mut empty = Session::new(&world);
    assert_eq!(kernel.seed::<Crate, _>(&mut empty, &store, &world, &3).unwrap(), None);
}

/// Seeds through the cover, changes fact 37 to `to` and carries: what the carry did, the
/// group's bytes in the session after it, and the bytes of a fresh run.
fn seeded_then_carried(to: u8) -> (Carried, Vec<u8>, Vec<u8>) {
    let (kernel, mut store, mut world) = covered();
    let mut session = Session::new(&world);
    kernel.seed::<Crate, _>(&mut session, &store, &world, &2).unwrap().unwrap();
    let from = world.pin();
    world.facts[37] = to;
    let mut changed = Changed::between(from, world.pin());
    changed.differs(Read { kind: TILE, key: &[37] });
    changed.exact(DIGEST);
    if to / 2 != 4 {
        changed.differs(Read { kind: DIGEST, key: &[2] });
    }
    let carried = kernel.carry(&mut session, &mut store, &world, &changed, &Setting::OFF).unwrap();
    let got = kernel.get_in::<Group, _>(&mut session, &mut store, &world, &2, &Setting::OFF).unwrap();
    let value = got.value().to_vec();
    drop(got);
    (carried, value, group_now(&kernel, &world))
}

/// Invariant I5: a member admitted through a cover is reached by a change of the cover's
/// read. A fact that changes inside what the interface digest ignores reaches nothing (no
/// value can show it); one the digest shows drops the cover and its 17 members, which are
/// then looked up through their own lists. Control: with the posting from the cover to its
/// members left out, the carry reaches the cover alone and the group's stale bytes are served.
#[test]
fn a_member_admitted_through_a_cover_is_dropped_with_it() {
    use crate::nest::planted::{self, Bug};
    let (carried, value, fresh) = seeded_then_carried(9);
    assert_eq!((carried, &value), (NOTHING, &fresh));
    let (carried, value, fresh) = seeded_then_carried(12);
    assert_eq!(carried, Carried { seeded: 1, gone: 18, ..NOTHING });
    assert_eq!(value, fresh);
    planted::plant(Bug::CoverUnposted);
    let (carried, value, fresh) = seeded_then_carried(12);
    planted::plant(Bug::None);
    assert_eq!(carried, Carried { seeded: 1, gone: 1, ..NOTHING });
    assert_ne!(value, fresh, "the planted bug was not caught: a member outlived its cover");
}

// Parts: a table-valued step, its readers by part and a reader of its key set
// (docs/v2/plan/design/kernel.md, "Parts").

const ROWS: ReadKind = ReadKind(5);
/// What a reader gives for a key its table has not.
const ABSENT: [u8; 2] = [0xff, 0xff];

/// Rows by key, read whole by the one step that lays them out as a table.
struct Keyed {
    rows: std::collections::BTreeMap<u16, u8>,
    valued: Cell<usize>,
}

impl Keyed {
    fn bytes(&self) -> Vec<u8> {
        self.rows.iter().flat_map(|(key, value)| [(key >> 8) as u8, *key as u8, *value]).collect()
    }
}

impl World for Keyed {
    fn hash(&self, read: Read<'_>) -> Result<Seen, WorldError> {
        self.valued.set(self.valued.get() + 1);
        match (read.kind, read.key) {
            (ROWS, &[]) => Ok(Seen::Is(ContentId::of("blaze v2 kernel cost test rows 1", &self.bytes()))),
            _ => Err(WorldError::new(read, "no such fact")),
        }
    }

    fn pin(&self) -> Pin {
        Pin(ContentId::of("blaze v2 kernel cost test keyed pin 1", &self.bytes()))
    }
}

struct Rows;

impl Fact<Keyed> for Rows {
    const KIND: ReadKind = ROWS;
    type Key = ();
    type Value<'w> = Vec<(u16, u8)>;

    fn key(_: &(), _: &mut Vec<u8>) {}

    fn get<'w>(world: &'w Keyed, _: &()) -> Result<(Vec<(u16, u8)>, Seen), WorldError> {
        let seen = world.hash(Read { kind: ROWS, key: &[] })?;
        Ok((world.rows.iter().map(|(key, value)| (*key, *value)).collect(), seen))
    }
}

/// The rows as a table: a part a key.
struct Names;

impl Step for Names {
    const ID: StepId = StepId(0x7f51);
    const NAME: &'static str = "cost names";
    const KIND: Kind = Kind::Query;
    type Input = u8;

    fn subject(input: &u8) -> Subject {
        Subject { of: ContentId::of("cost names", &[*input]), flags: ContentId::default() }
    }

    fn label(input: &u8) -> String {
        format!("cost names {input}")
    }
}

impl Compute for Names {
    type World = Keyed;

    fn compute(ctx: &mut Reads<'_, Keyed>, _: &u8) -> Result<Vec<u8>, StepError> {
        let mut names = table::Builder::new();
        for (key, value) in ctx.get::<Rows>(&())? {
            names.part(&key.to_be_bytes(), &[value]);
        }
        names.finish().map_err(StepError::Refused)
    }
}

impl Ask for Names {
    const TABLE: bool = true;

    fn key(input: &u8, out: &mut Vec<u8>) {
        out.push(*input);
    }

    fn input(key: &[u8]) -> Result<u8, String> {
        one_byte(key)
    }
}

/// One key's part, or [`ABSENT`].
struct Reader;

impl Step for Reader {
    const ID: StepId = StepId(0x7f52);
    const NAME: &'static str = "cost reader";
    const KIND: Kind = Kind::Query;
    type Input = u16;

    fn subject(input: &u16) -> Subject {
        Subject { of: ContentId::of("cost reader", &input.to_be_bytes()), flags: ContentId::default() }
    }

    fn label(input: &u16) -> String {
        format!("cost reader {input}")
    }
}

impl Compute for Reader {
    type World = Keyed;

    fn compute(ctx: &mut Reads<'_, Keyed>, input: &u16) -> Result<Vec<u8>, StepError> {
        Ok(ctx.part::<Names>(&0, &input.to_be_bytes())?.unwrap_or_else(|| ABSENT.to_vec()))
    }
}

/// How many keys there are: a reader of the key set.
struct Count;

impl Step for Count {
    const ID: StepId = StepId(0x7f53);
    const NAME: &'static str = "cost count";
    const KIND: Kind = Kind::Query;
    type Input = u8;

    fn subject(input: &u8) -> Subject {
        Subject { of: ContentId::of("cost count", &[*input]), flags: ContentId::default() }
    }

    fn label(input: &u8) -> String {
        format!("cost count {input}")
    }
}

impl Compute for Count {
    type World = Keyed;

    fn compute(ctx: &mut Reads<'_, Keyed>, _: &u8) -> Result<Vec<u8>, StepError> {
        Ok((ctx.keys::<Names>(&0)?.len() as u32).to_be_bytes().to_vec())
    }
}

/// The parts of the first `n` keys, one after another: a reader of n parts of one table.
struct Several;

impl Step for Several {
    const ID: StepId = StepId(0x7f54);
    const NAME: &'static str = "cost several";
    const KIND: Kind = Kind::Query;
    type Input = u8;

    fn subject(input: &u8) -> Subject {
        Subject { of: ContentId::of("cost several", &[*input]), flags: ContentId::default() }
    }

    fn label(input: &u8) -> String {
        format!("cost several {input}")
    }
}

impl Compute for Several {
    type World = Keyed;

    fn compute(ctx: &mut Reads<'_, Keyed>, input: &u8) -> Result<Vec<u8>, StepError> {
        let mut out = Vec::new();
        for key in 0..u16::from(*input) {
            out.extend(ctx.part::<Names>(&0, &key.to_be_bytes())?.unwrap_or_else(|| ABSENT.to_vec()));
        }
        Ok(out)
    }
}

/// A step that says its value is a table and makes none (input 0), or one with a byte of a
/// part changed after it was built (any other input).
struct Bad;

impl Step for Bad {
    const ID: StepId = StepId(0x7f55);
    const NAME: &'static str = "cost bad table";
    const KIND: Kind = Kind::Query;
    type Input = u8;

    fn subject(input: &u8) -> Subject {
        Subject { of: ContentId::of("cost bad table", &[*input]), flags: ContentId::default() }
    }

    fn label(input: &u8) -> String {
        format!("cost bad table {input}")
    }
}

impl Compute for Bad {
    type World = Keyed;

    fn compute(_: &mut Reads<'_, Keyed>, input: &u8) -> Result<Vec<u8>, StepError> {
        if *input == 0 {
            return Ok(b"bytes long enough for a header, and no table".to_vec());
        }
        let mut names = table::Builder::new();
        names.part(&[0], &[1]);
        let mut bytes = names.finish().map_err(StepError::Refused)?;
        // The header (36 bytes), the one entry of the directory (40), the key (1): the part.
        bytes[77] ^= 1;
        Ok(bytes)
    }
}

impl Ask for Bad {
    const TABLE: bool = true;

    fn key(input: &u8, out: &mut Vec<u8>) {
        out.push(*input);
    }

    fn input(key: &[u8]) -> Result<u8, String> {
        one_byte(key)
    }
}

/// A table, a session that holds a reader for each of `readers` and the reader of the keys.
struct Tabled {
    kernel: Kernel<Keyed>,
    store: Memory,
    world: Keyed,
    session: Session,
}

fn tabled(rows: impl Iterator<Item = (u16, u8)>, readers: &[u16]) -> Tabled {
    let mut kernel = Kernel::new();
    kernel.register_ask::<Names>(CodeId(ContentId([51; 32]))).unwrap();
    kernel.register::<Reader>(CodeId(ContentId([52; 32]))).unwrap();
    kernel.register::<Count>(CodeId(ContentId([53; 32]))).unwrap();
    kernel.register::<Several>(CodeId(ContentId([54; 32]))).unwrap();
    kernel.register_ask::<Bad>(CodeId(ContentId([55; 32]))).unwrap();
    let world = Keyed { rows: rows.collect(), valued: Cell::new(0) };
    let session = Session::new(&world);
    let mut it = Tabled { kernel, store: Memory::default(), world, session };
    for key in readers {
        it.reader(*key);
    }
    it.count();
    it
}

impl Tabled {
    /// Sets the row of `key`, or takes it out, and carries the session: what the carry did,
    /// its gets and the reads it valued.
    fn edit(&mut self, key: u16, to: Option<u8>) -> (Carried, usize, usize) {
        let from = self.world.pin();
        match to {
            Some(to) => self.world.rows.insert(key, to),
            None => self.world.rows.remove(&key),
        };
        let mut changed = Changed::between(from, self.world.pin());
        changed.differs(Read { kind: ROWS, key: &[] });
        self.store.gets.set(0);
        self.world.valued.set(0);
        let carried = self
            .kernel
            .carry(&mut self.session, &mut self.store, &self.world, &changed, &Setting::OFF)
            .unwrap();
        (carried, self.store.gets.get(), self.world.valued.get())
    }

    /// The reader of `key` in the session: whether it hit, and its bytes.
    fn reader(&mut self, key: u16) -> (bool, Vec<u8>) {
        let got = self
            .kernel
            .get_in::<Reader, _>(&mut self.session, &mut self.store, &self.world, &key, &Setting::OFF)
            .unwrap();
        (got.is_hit(), got.value().to_vec())
    }

    fn count(&mut self) -> (bool, Vec<u8>) {
        let got = self
            .kernel
            .get_in::<Count, _>(&mut self.session, &mut self.store, &self.world, &0, &Setting::OFF)
            .unwrap();
        (got.is_hit(), got.value().to_vec())
    }

    /// The readers of `keys` that were not hits, each checked against the rows as they are.
    fn reran(&mut self, keys: std::ops::Range<u16>) -> Vec<u16> {
        let mut reran = Vec::new();
        for key in keys {
            let (hit, value) = self.reader(key);
            let fresh = self.world.rows.get(&key).map_or(ABSENT.to_vec(), |value| vec![*value]);
            assert_eq!(value, fresh, "the reader of key {key}");
            if !hit {
                reran.push(key);
            }
        }
        reran
    }
}

/// K = 1000 keys, a reader a key, a reader of a key the table has not (1000) and a reader of
/// the key set.
fn thousand() -> Tabled {
    let readers: Vec<u16> = (0..1001).collect();
    tabled((0..1000).map(|key| (key, 1)), &readers)
}

/// What a carry of one edit of the table does with `gone` readers reached: the table is the
/// one entry seeded and repaired, its value is other bytes, and each reader reached is
/// dropped (no step read it, so it runs when it is next asked for).
fn one_table(gone: usize) -> Carried {
    Carried { seeded: 1, repaired: 1, changed: 1, gone, ..Carried::default() }
}

/// The floor is the table rerun (its rows valued once, its entry put) and the one reader of
/// the part that changed rerun; no get.
///
/// The carry's 7 gets: 5 are the table's rerun, as any rerun entry pays them (its lookup: the
/// read-list of its chain, the chain's next slot, the answer under the key its reads make now,
/// which is not there; its keep: the get before the entry's put and the get of its list), and
/// 2 are the comparison (the entry its readers saw, the entry it has now). Its 2 reads valued:
/// the lookup values the rows to make the answer's key, the run values them again; the floor
/// is 1 (a maintained step pays that: `maintain` in this file).
///
/// After it, 1001 session hits of one get each (the value's bytes: the floor of a hit that
/// gives its bytes) and the one rerun reader's 5: its list, the chain's next slot, the table
/// (once, though the lookup and the run both read the part), and its keep's 2. The other 999
/// readers and the reader of the keys value nothing and get nothing but their own bytes.
#[test]
fn a_part_that_changes_reruns_its_one_reader_of_a_thousand() {
    let mut it = thousand();
    let (carried, gets, valued) = it.edit(437, Some(2));
    println!("one part of 1000 changed: carry gets {gets}, valued {valued}");
    assert_eq!(carried, one_table(1));
    assert_eq!((gets, valued), (7, 2), "the table's rerun 5, the comparison 2; the rows valued twice");
    it.store.gets.set(0);
    it.world.valued.set(0);
    assert_eq!(it.reran(0..1001), [437]);
    assert_eq!(it.count(), (true, 1000u32.to_be_bytes().to_vec()), "the key set is as it was");
    println!("1000 hits and one rerun: gets {}, valued {}", it.store.gets.get(), it.world.valued.get());
    // Measured 1007 while the lookup and the run each got the table; the request holds it now.
    assert_eq!(it.store.gets.get(), 1001 + 5, "1001 hits of one get, and the rerun reader's 5");
    assert_eq!(it.world.valued.get(), 0, "a reader by part values no read of the world");
}

/// A reader of n parts of one table pays one get of the table's entry, not n: the request
/// holds the table it read (the floor: the reader's own lookup and keep, and the table once).
#[test]
fn n_parts_of_one_table_are_one_get() {
    let mut it = thousand();
    let mut gets = |n: u8| {
        it.store.gets.set(0);
        let got = it
            .kernel
            .get_in::<Several, _>(&mut it.session, &mut it.store, &it.world, &n, &Setting::OFF)
            .unwrap();
        assert!(!got.is_hit());
        assert_eq!(got.value().to_vec(), vec![1u8; usize::from(n)]);
        it.store.gets.get()
    };
    let (one, eight) = (gets(1), gets(8));
    println!("n parts of one table: gets {one} for 1 part, {eight} for 8 parts");
    assert_eq!(eight, one, "each part after the first is read from the table the request holds");
    // Nothing is held past the request: `held` refuses a table or a node left in the session.
    let _ = it.session.held();
}

/// What a step that says its value is a table made is checked when it is kept: that it is a
/// table always, every byte of it (`Table::verify`) where the setting samples the run.
#[test]
fn a_table_step_that_makes_no_table_is_refused_by_name() {
    let mut it = tabled((0..2).map(|key| (key, 1)), &[]);
    let mut bad = |input: u8, setting: &Setting| {
        it.kernel
            .get_in::<Bad, _>(&mut it.session, &mut it.store, &it.world, &input, setting)
            .map(|got| got.value().to_vec())
    };
    let Err(KernelError::Compute { of, why }) = bad(0, &Setting::OFF) else {
        panic!("bytes that are no table were kept as one");
    };
    assert_eq!(of.step, "cost bad table");
    assert!(why.contains("`Ask::TABLE`") && why.contains("not a table"), "{why}");
    let Err(KernelError::Compute { why, .. }) = bad(1, &Setting::ALL) else {
        panic!("a table that is not the one form of its entries was kept under verify");
    };
    assert!(why.contains("not the one form"), "{why}");
    assert!(bad(1, &Setting::OFF).is_ok(), "with verify off only the header is read");
}

/// An absence is a read: the key that appears reaches the reader that saw none, and the
/// reader of the key set; no reader of another key.
#[test]
fn a_key_that_appears_reruns_the_reader_that_saw_it_absent() {
    let mut it = thousand();
    assert_eq!(it.reader(1000), (true, ABSENT.to_vec()));
    let (carried, gets, _) = it.edit(1000, Some(7));
    println!("a key read as absent appears: carry gets {gets}");
    assert_eq!(carried, one_table(2));
    assert_eq!(it.count(), (false, 1001u32.to_be_bytes().to_vec()));
    assert_eq!(it.reran(0..1001), [1000]);
}

/// A key nobody read reaches the reader of the key set alone.
#[test]
fn a_key_nobody_read_reruns_no_reader_by_part() {
    let mut it = thousand();
    let (carried, gets, _) = it.edit(2000, Some(7));
    println!("a key nobody read appears: carry gets {gets}");
    assert_eq!(carried, one_table(1));
    assert_eq!(it.count(), (false, 1001u32.to_be_bytes().to_vec()));
    assert_eq!(it.reran(0..1001), [] as [u16; 0]);
}

/// A key that goes reaches its reader and the reader of the key set.
#[test]
fn a_key_that_goes_reruns_its_reader() {
    let mut it = thousand();
    let (carried, ..) = it.edit(5, None);
    assert_eq!(carried, one_table(2));
    assert_eq!(it.count(), (false, 999u32.to_be_bytes().to_vec()));
    assert_eq!(it.reran(0..1001), [5]);
}

/// A table saved with its session and opened: the readers by part come back from the store.
#[test]
fn postings_by_part_are_read_back_from_a_saved_session() {
    let mut it = thousand();
    let root = it.session.save(&mut it.store).unwrap();
    it.session = Session::open(&it.store, root).unwrap();
    let (carried, gets, _) = it.edit(437, Some(2));
    println!("one part of 1000 changed after open: carry gets {gets}");
    assert_eq!(carried, one_table(1));
    // The 7 of a session in memory, and 9 nodes of the saved session's maps: the paths to
    // what the carry reaches (the rows' posting, the table's row, the part's posting, its
    // reader's row), each node once; none of the other 1000 readers' rows or postings.
    assert_eq!(gets, 16, "7 as in memory, and 9 nodes of the saved maps");
    assert_eq!(it.reran(0..1001), [437]);
    let (carried, ..) = it.edit(1000, Some(7));
    assert_eq!(carried, one_table(2));
    assert_eq!(it.reran(0..1001), [1000]);
}

/// Every sequence of three edits of a table of 22 keys (4 of them read, of which 2 start
/// absent; 20 never read, so the index has branches), the session carried after each: the
/// first place a reader, or the reader of the keys, gives other bytes than the rows give.
fn first_stale_table(bug: crate::nest::planted::Bug) -> Option<String> {
    use crate::nest::planted::{Bug, plant};
    let edits: Vec<(u16, Option<u8>)> =
        (0..4).flat_map(|key| [(key, Some(1)), (key, Some(2)), (key, None)]).collect();
    let mut stale = None;
    plant(bug);
    'all: for sequence in 0..edits.len().pow(3) {
        let rows = (0..2).map(|key| (key, 1)).chain((100..120).map(|key| (key, 0)));
        let mut it = tabled(rows, &[0, 1, 2, 3]);
        for turn in 0..3 {
            let (key, to) = edits[sequence / edits.len().pow(turn) % edits.len()];
            it.edit(key, to);
            let count = (it.world.rows.len() as u32).to_be_bytes().to_vec();
            let wrong = (0..4u16).find(|key| {
                it.reader(*key).1 != it.world.rows.get(key).map_or(ABSENT.to_vec(), |value| vec![*value])
            });
            if wrong.is_some() || it.count().1 != count {
                stale = Some(format!("sequence {sequence}, edit {turn} ({key} to {to:?}): reader {wrong:?}"));
                break 'all;
            }
        }
    }
    plant(Bug::None);
    stale
}

/// The invariant: a reader of one part, of an absent key or of the key set gives, after any
/// carries, what a run with no kernel gives.
#[test]
fn every_sequence_of_table_edits_gives_what_the_rows_give() {
    assert_eq!(first_stale_table(crate::nest::planted::Bug::None), None);
}

#[test]
fn an_absence_not_posted_is_caught() {
    assert!(first_stale_table(crate::nest::planted::Bug::AbsenceUnposted).is_some());
}

#[test]
fn a_removed_key_the_comparison_missed_is_caught() {
    assert!(first_stale_table(crate::nest::planted::Bug::DiffRemoved).is_some());
}

#[test]
fn a_part_digested_as_another_is_caught() {
    assert!(first_stale_table(crate::nest::planted::Bug::PartDigest).is_some());
}

// Maintained steps: a table kept by `maintain` from its value before and the reads that
// changed (docs/v2/plan/design/kernel.md, "Maintained steps").

const LINE: ReadKind = ReadKind(6);
const INDEX: ReadKind = ReadKind(7);

/// Rows by key, each its own fact, and the list of the keys: what a table step reads row by
/// row, so its `compute` makes a read a key.
struct Lined {
    rows: std::collections::BTreeMap<u16, u8>,
    valued: Cell<usize>,
}

impl Lined {
    fn keys(&self) -> Vec<u8> {
        self.rows.keys().flat_map(|key| key.to_be_bytes()).collect()
    }
}

impl World for Lined {
    fn hash(&self, read: Read<'_>) -> Result<Seen, WorldError> {
        self.valued.set(self.valued.get() + 1);
        match (read.kind, read.key) {
            (LINE, &[high, low]) => Ok(match self.rows.get(&u16::from_be_bytes([high, low])) {
                Some(value) => Seen::Is(ContentId::of("blaze v2 kernel cost test line 1", &[*value])),
                None => Seen::Gone,
            }),
            (INDEX, &[]) => Ok(Seen::Is(ContentId::of("blaze v2 kernel cost test index 1", &self.keys()))),
            _ => Err(WorldError::new(read, "no such fact")),
        }
    }

    fn pin(&self) -> Pin {
        let rows: Vec<u8> =
            self.rows.iter().flat_map(|(key, value)| [(key >> 8) as u8, *key as u8, *value]).collect();
        Pin(ContentId::of("blaze v2 kernel cost test lined pin 1", &rows))
    }
}

struct Line;

impl Fact<Lined> for Line {
    const KIND: ReadKind = LINE;
    type Key = u16;
    type Value<'w> = Option<u8>;

    fn key(key: &u16, out: &mut Vec<u8>) {
        out.extend_from_slice(&key.to_be_bytes());
    }

    fn get<'w>(world: &'w Lined, key: &u16) -> Result<(Option<u8>, Seen), WorldError> {
        let seen = world.hash(Read { kind: LINE, key: &key.to_be_bytes() })?;
        Ok((world.rows.get(key).copied(), seen))
    }
}

struct Index;

impl Fact<Lined> for Index {
    const KIND: ReadKind = INDEX;
    type Key = ();
    type Value<'w> = Vec<u16>;

    fn key(_: &(), _: &mut Vec<u8>) {}

    fn get<'w>(world: &'w Lined, _: &()) -> Result<(Vec<u16>, Seen), WorldError> {
        let seen = world.hash(Read { kind: INDEX, key: &[] })?;
        Ok((world.rows.keys().copied().collect(), seen))
    }
}

/// The rows as a table, read row by row: the index, then a read a key.
fn lines(ctx: &mut Reads<'_, Lined>) -> Result<Vec<u8>, StepError> {
    let mut table = table::Builder::new();
    for key in ctx.get::<Index>(&())? {
        if let Some(value) = ctx.get::<Line>(&key)? {
            table.part(&key.to_be_bytes(), &[value]);
        }
    }
    table.finish().map_err(StepError::Refused)
}

/// The table of [`lines`] from the one before and the reads that changed: a row that changed
/// is read again and its part set; where the index changed, a key that came is read and put
/// in, and a key that went is taken out and its read forgotten. With `forgets`, the planted
/// bug: a key that went is left in, and so is its read.
fn relined(
    ctx: &mut Reads<'_, Lined>,
    previous: &[u8],
    changed: &[Change<'_>],
    forgets: bool,
) -> Result<Vec<u8>, StepError> {
    let table = table::Table::read(previous).map_err(StepError::Refused)?;
    let mut edits: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
    for change in changed {
        match (change.read.kind, change.read.key) {
            (INDEX, _) => {
                let keys = ctx.get::<Index>(&())?;
                for at in 0..table.len() {
                    let key = table.entry(at).map_err(StepError::Refused)?.0;
                    let number = u16::from_be_bytes([key[0], key[1]]);
                    if keys.binary_search(&number).is_err() && !forgets {
                        ctx.forget::<Line>(&number);
                        edits.push((key.to_vec(), None));
                    }
                }
                for key in keys {
                    let name = key.to_be_bytes();
                    if table.part(&name).map_err(StepError::Refused)?.is_none()
                        && let Some(value) = ctx.get::<Line>(&key)?
                    {
                        edits.push((name.to_vec(), Some(vec![value])));
                    }
                }
            }
            // A row that went: the index changed with it, and says so above.
            (LINE, _) if change.now == Seen::Gone => {}
            (LINE, &[high, low]) => {
                if let Some(value) = ctx.get::<Line>(&u16::from_be_bytes([high, low]))? {
                    edits.push((vec![high, low], Some(vec![value])));
                }
            }
            _ => return Err(StepError::Refused("a read this step does not make".to_owned())),
        }
    }
    let edits: Vec<(&[u8], Option<&[u8]>)> =
        edits.iter().map(|(key, part)| (&key[..], part.as_deref())).collect();
    table.edit(&edits).map_err(StepError::Refused)
}

macro_rules! lined {
    ($name:ident, $id:literal, $text:literal, $forgets:literal, $reader:ident, $reader_id:literal, $reader_text:literal) => {
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

        impl Compute for $name {
            type World = Lined;

            fn compute(ctx: &mut Reads<'_, Lined>, _: &u8) -> Result<Vec<u8>, StepError> {
                lines(ctx)
            }
        }

        impl Ask for $name {
            const TABLE: bool = true;

            fn key(input: &u8, out: &mut Vec<u8>) {
                out.push(*input);
            }

            fn input(key: &[u8]) -> Result<u8, String> {
                one_byte(key)
            }
        }

        impl Maintain for $name {
            fn maintain(
                ctx: &mut Reads<'_, Lined>,
                _: &u8,
                previous: &[u8],
                changed: &[Change<'_>],
            ) -> Result<Vec<u8>, StepError> {
                relined(ctx, previous, changed, $forgets)
            }
        }

        /// One key's part of the table, or [`ABSENT`].
        struct $reader;

        impl Step for $reader {
            const ID: StepId = StepId($reader_id);
            const NAME: &'static str = $reader_text;
            const KIND: Kind = Kind::Query;
            type Input = u16;

            fn subject(input: &u16) -> Subject {
                Subject { of: ContentId::of($reader_text, &input.to_be_bytes()), flags: ContentId::default() }
            }

            fn label(input: &u16) -> String {
                format!("{} {input}", $reader_text)
            }
        }

        impl Compute for $reader {
            type World = Lined;

            fn compute(ctx: &mut Reads<'_, Lined>, input: &u16) -> Result<Vec<u8>, StepError> {
                Ok(ctx.part::<$name>(&0, &input.to_be_bytes())?.unwrap_or_else(|| ABSENT.to_vec()))
            }
        }
    };
}

lined!(Lines, 0x7f56, "cost lines", false, LineReader, 0x7f57, "cost line reader");
lined!(Lossy, 0x7f58, "cost lossy lines", true, LossyReader, 0x7f59, "cost lossy line reader");

/// A table of `count` rows, a session that holds it and the readers of keys 5 and 437.
struct Kept {
    kernel: Kernel<Lined>,
    store: Memory,
    world: Lined,
    session: Session,
}

/// `maintain`: whether the table steps are registered as maintained, or only to be computed.
fn kept<R: Compute<Input = u16, World = Lined>>(count: u16, maintain: bool) -> Kept {
    let mut kernel = Kernel::new();
    if maintain {
        kernel.register_maintain::<Lines>(CodeId(ContentId([56; 32]))).unwrap();
        kernel.register_maintain::<Lossy>(CodeId(ContentId([58; 32]))).unwrap();
    } else {
        kernel.register_ask::<Lines>(CodeId(ContentId([56; 32]))).unwrap();
        kernel.register_ask::<Lossy>(CodeId(ContentId([58; 32]))).unwrap();
    }
    kernel.register::<LineReader>(CodeId(ContentId([57; 32]))).unwrap();
    kernel.register::<LossyReader>(CodeId(ContentId([59; 32]))).unwrap();
    let world = Lined { rows: (0..count).map(|key| (key, 1)).collect(), valued: Cell::new(0) };
    let session = Session::new(&world);
    let mut it = Kept { kernel, store: Memory::default(), world, session };
    for key in [5, 437] {
        it.kernel.get_in::<R, _>(&mut it.session, &mut it.store, &it.world, &key, &Setting::OFF).unwrap();
    }
    it
}

impl Kept {
    /// Sets the row of `key`, or takes it out, and carries the session under `setting`: what
    /// the carry did, its gets, the reads the world valued and the digests of tables made.
    fn edit(
        &mut self,
        key: u16,
        to: Option<u8>,
        setting: &Setting,
    ) -> Result<(Carried, usize, usize, usize), KernelError> {
        let from = self.world.pin();
        let had = match to {
            Some(to) => self.world.rows.insert(key, to),
            None => self.world.rows.remove(&key),
        };
        let mut changed = Changed::between(from, self.world.pin());
        changed.exact(LINE);
        changed.exact(INDEX);
        changed.differs(Read { kind: LINE, key: &key.to_be_bytes() });
        if had.is_some() != to.is_some() {
            changed.differs(Read { kind: INDEX, key: &[] });
        }
        self.store.gets.set(0);
        self.world.valued.set(0);
        table::hashed::take();
        let carried = self.kernel.carry(&mut self.session, &mut self.store, &self.world, &changed, setting)?;
        Ok((carried, self.store.gets.get(), self.world.valued.get(), table::hashed::take()))
    }

    /// The table of `S` as the session gives it.
    fn table<S: Compute<Input = u8, World = Lined>>(&mut self) -> Vec<u8> {
        let got = self
            .kernel
            .get_in::<S, _>(&mut self.session, &mut self.store, &self.world, &0, &Setting::OFF)
            .unwrap();
        got.value().to_vec()
    }

    /// The table a run with no kernel makes of the rows as they are.
    fn fresh(&self) -> Vec<u8> {
        let mut table = table::Builder::new();
        for (key, value) in &self.world.rows {
            table.part(&key.to_be_bytes(), &[*value]);
        }
        table.finish().unwrap()
    }
}

/// One row of 1000 changes. The floor: the row valued once, the part and the index nodes on
/// its path hashed, the table's entry put, and the one reader of the part.
///
/// Maintained, the carry values 2 reads (the kernel asks the world what the changed row is
/// now, to know that it differs; `maintain` reads its value) and makes the digests of one
/// path. Its 5 gets: the entry before (what `maintain` runs from), the keep's 2 (the get
/// before the entry's put, the get of its list), and the comparison's 2. Computed, the lookup
/// values the 1001 reads of the entry's list and the run values them again, and the build
/// hashes every key, part and node.
///
/// Still linear in the table either way, as a table is one record: the entry before is
/// copied out of the store, the table's bytes are written again whole (`Table::edit` copies
/// every digest and writes every node), and the kernel walks the 1001 reads of the entry
/// before to find those that may differ (a comparison of names each, no hash).
#[test]
fn a_maintained_table_pays_for_the_row_that_changed() {
    let mut it = kept::<LineReader>(1000, true);
    let (carried, gets, valued, hashed) = it.edit(437, Some(2), &Setting::OFF).unwrap();
    println!("maintained, one row of 1000 changed: carry gets {gets}, valued {valued}, digests {hashed}");
    assert_eq!((carried.repaired, carried.changed, carried.gone), (1, 1, 1));
    assert_eq!(it.table::<Lines>(), it.fresh());
    let mut computed = kept::<LineReader>(1000, false);
    let (again, c_gets, c_valued, c_hashed) = computed.edit(437, Some(2), &Setting::OFF).unwrap();
    println!("computed, one row of 1000 changed: carry gets {c_gets}, valued {c_valued}, digests {c_hashed}");
    assert_eq!(again, carried, "the carry does the same either way");
    assert_eq!(computed.table::<Lines>(), it.fresh());
    // BOUND on the gets (5 by the count above, 6 if the keep reads its chain's next slot),
    // to pin from the two printed lines; the reads valued are exact.
    assert!(gets <= 6, "maintained: gets {gets}");
    assert_eq!(valued, 2, "maintained: the changed row, by the kernel and by `maintain`");
    assert!(hashed <= 28, "maintained: the digests of one path, {hashed}");
    assert_eq!((c_gets, c_valued), (7, 2002), "computed");
    assert!(c_hashed >= 2000, "computed: every key and part, {c_hashed}");
}

/// The invariant: a maintained step's value and reads are those its `compute` makes
/// (`Maintain::maintain`). Every edit here is carried with the check on, which runs both and
/// refuses a difference: a part set, a key taken out, a key put in, the same again, a key
/// nobody read, the table emptied and filled; and each is the table a run with no kernel
/// makes, as each reader is.
#[test]
fn a_maintained_table_is_the_table_computed() {
    for setting in [Setting::ALL, Setting::OFF] {
        let mut it = kept::<LineReader>(40, true);
        let edits = [(5, Some(2)), (5, None), (5, Some(3)), (5, Some(3)), (900, Some(1)), (437, Some(9)), (437, None)];
        for (key, to) in edits.into_iter().chain((0..40).map(|key| (key, None))).chain([(7, Some(7))]) {
            it.edit(key, to, &setting).unwrap();
            assert!(it.table::<Lines>() == it.fresh(), "after {key} was set to {to:?}");
            for reader in [5u16, 437] {
                let got = it
                    .kernel
                    .get_in::<LineReader, _>(&mut it.session, &mut it.store, &it.world, &reader, &Setting::OFF)
                    .unwrap();
                let fresh = it.world.rows.get(&reader).map_or(ABSENT.to_vec(), |value| vec![*value]);
                assert_eq!(got.value().to_vec(), fresh, "the reader of {reader} after {key} was set to {to:?}");
            }
        }
    }
}

/// The control of the check: a maintainer that leaves in a key that went (and its read) is
/// refused where the setting runs `compute` beside it, naming the step and the first read
/// that differs, and nothing is kept.
#[test]
fn a_maintainer_that_forgets_a_deletion_is_refused_under_verify() {
    let mut it = kept::<LossyReader>(40, true);
    it.edit(6, Some(2), &Setting::ALL).expect("an edit of a part, which it maintains rightly");
    let Err(KernelError::Maintained { of, why }) = it.edit(5, None, &Setting::ALL) else {
        panic!("a maintainer that forgot a deletion was kept under verify");
    };
    println!("a maintainer that forgets a deletion: {why}");
    assert_eq!(of.step, "cost lossy lines");
    assert!(why.contains("`compute`"), "{why}");
}

/// The control of the oracle: with the check off, the same maintainer's table is not the one
/// a run with no kernel makes.
#[test]
fn a_maintainer_that_forgets_a_deletion_is_stale_with_verify_off() {
    let mut it = kept::<LossyReader>(40, true);
    it.edit(5, None, &Setting::OFF).unwrap();
    assert!(it.table::<Lossy>() != it.fresh(), "the planted bug was not caught: the key that went is gone");
}
