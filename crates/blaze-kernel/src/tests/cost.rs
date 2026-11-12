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
            _ => Err(WorldError::new(read, "no such fact")),
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
/// session: its gets are the path's 26 and at most 3 nodes for each of those rows and
/// postings.
#[test]
fn a_session_opened_from_the_store_carries_as_the_one_that_was_saved() {
    let mut kept = built();
    let changed = kept.set(37, 12);
    let (carried_kept, _) = kept.carry(&changed);
    let (hit_kept, value_kept, _) = kept.whole();

    let mut built = built();
    let (root, written) = built.save();
    println!("first save of 256 entries: {written} records");
    assert_eq!(built.save(), (root, 0), "a save of nothing changed writes nothing");
    assert_eq!(built.reopen(root), Cost { probes: 0, gets: 1, valued: 0 });
    assert_eq!((built.session.len(), built.session.held()), (256, (0, 0)));
    let changed = built.set(37, 12);
    let (carried, cost) = built.carry(&changed);
    println!("path carry after open: {carried:?} {cost:?} held {:?}", built.session.held());
    assert_eq!(carried, carried_kept);
    assert_eq!((cost.probes, cost.valued), (0, 2), "{cost:?}");
    // The fact's posting, the leaf's row and posting, the group's row and posting, the
    // whole's row, 15 leaves' rows: 21 paths.
    assert!(cost.gets > 26 && cost.gets <= 26 + 21 * 3, "{cost:?}");
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
    assert_eq!((cost.probes, cost.valued), (0, 0), "{cost:?}");
    assert!(cost.gets >= 2 && cost.gets <= 1 + 3, "its row's path and its entry: {cost:?}");
    assert_eq!(built.session.held(), (1, 0));
}

/// A save writes the paths of what changed, not the session: after a carry that repaired two
/// entries and dropped the top, three rows differ (no posting does: their lists were posted),
/// so at most a root node, three nodes under it, three leaves, and the root record.
#[test]
fn a_save_writes_the_paths_of_what_changed() {
    let mut built = built();
    let (root, whole) = built.save();
    let changed = built.set(37, 12);
    built.carry(&changed);
    let (next, written) = built.save();
    println!("save: {whole} records for 256 entries, {written} after a carry that repaired 2");
    assert_ne!(next, root);
    assert!(whole >= 3 * 256 / 8 && whole <= 900, "{whole}");
    assert!(written >= 3 && written <= 8, "{written}");
}

/// A session opened and carried over one changed fact whose leaf comes out the same reads
/// that fact's posting and that leaf's row (a path each), and the leaf's lookup, run and keep
/// (5 gets, as with the session in memory): one row and two names are in memory after it.
#[test]
fn an_opened_session_reads_only_what_a_carry_reaches() {
    let mut built = built();
    let (root, _) = built.save();
    built.reopen(root);
    let changed = built.set(37, 9);
    let (carried, cost) = built.carry(&changed);
    println!("cutoff carry after open: {carried:?} {cost:?}");
    assert_eq!(carried, Carried { seeded: 1, repaired: 1, cut: 1, ..NOTHING });
    assert_eq!((cost.probes, cost.valued), (0, 2), "{cost:?}");
    assert!(cost.gets >= 5 + 2 * 2 && cost.gets <= 5 + 2 * 3, "{cost:?}");
    assert_eq!(built.session.held(), (1, 2), "the leaf; the fact's name and the leaf's own");
    assert_eq!(built.session.len(), 256);
}

/// Rule 26, the previous layout: a root record of layout 0 is refused naming the layout, and
/// so is a root the store does not have; neither opens as an empty session.
#[test]
fn a_saved_session_of_another_layout_is_refused_by_name() {
    let mut built = built();
    let (root, _) = built.save();
    let mut bytes = built.store.values.get(&root).unwrap().clone();
    assert_eq!(&bytes[..16], b"blaze kernel s 1");
    bytes[15] = b'0';
    let old = Key(ContentId::of("blaze v2 kernel cost test old root 1", &bytes));
    built.store.values.insert(old, bytes);
    let refused = Session::open(&built.store, old).map(|_| ()).unwrap_err();
    assert!(matches!(&refused, KernelError::Store { key, .. } if *key == old), "{refused}");
    assert!(refused.to_string().contains("blaze kernel s 0"), "{refused}");
    let nowhere = Key(ContentId([9; 32]));
    let refused = Session::open(&built.store, nowhere).map(|_| ()).unwrap_err();
    assert!(matches!(&refused, KernelError::Store { key, .. } if *key == nowhere), "{refused}");
    assert!(refused.to_string().contains("missing"), "{refused}");
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
