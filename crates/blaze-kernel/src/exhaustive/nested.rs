//! The exhaustive check, for steps that ask for steps: every sequence of [`STEPS`] steps over
//! the parent's three facts, where a step of a sequence changes one fact (9 ways) or asks for
//! one of three values (3 ways): [`Top`], which asks for two [`Mid`]s; a [`Mid`], which asks
//! for one or two [`Leaf`]s (which, depends on the first one's value) and reads a fact itself;
//! a [`Leaf`], which reads one fact and says less than it read (two facts' values give one
//! leaf value, so a leaf computed again often gives the same bytes: early cutoff).
//!
//! The oracle is the same three functions written with no kernel ([`top`], [`mid`], [`leaf`]).
//! The controls plant a bug in the kernel's own rule (`nest::planted`), and each must end in a
//! stale hit.
//!
//! Every get of a sequence is in one session ([`Session`]), which a step of a sequence may
//! also drop (1 way): the same answers with it dropped at any place is the session's
//! invariant.
//!
//! The bound: `13^4 = 28,561` sequences, each prefix checked. Four steps hold every planted
//! bug's shortest failure (a get, a change, a get) and the shortest early cutoff (a fact set,
//! a get, the fact changed within one leaf value, a get). A fifth step is 12 times the work,
//! and a nested get is several lookups; the count was chosen before any timing, to stay well
//! under a minute in a debug build.

use std::cell::Cell;

use blaze_cas::Kind;
use blaze_ids::ContentId;
use blaze_store_check::Setting;

use super::{ABSENT, At, Did, FACTS, Facts, Failure, Undoable, World3};
use crate::nest::planted::{self, Bug};
use crate::*;

const STEPS: u32 = 4;
/// Twelve as before (a fact changed, 9 ways; a get, 3), and the session dropped.
const WAYS: u64 = 13;

thread_local! {
    /// How many times a step of this file ran, on this thread.
    static RUNS: Cell<u64> = const { Cell::new(0) };
}

fn ran() {
    RUNS.with(|it| it.set(it.get() + 1));
}

fn subject(domain: &'static str, input: u8) -> Subject {
    Subject { of: ContentId::of(domain, &[input]), flags: ContentId::default() }
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
                subject($text, *input)
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

step!(Leaf, 0x7f21, "test leaf");
step!(Mid, 0x7f22, "test mid");
step!(Top, 0x7f23, "test top");
step!(Loop, 0x7f24, "test loop");
step!(Flip, 0x7f25, "test flip");

impl Compute for Leaf {
    type World = Facts;

    fn compute(ctx: &mut Reads<'_, Facts>, input: &u8) -> Result<Vec<u8>, StepError> {
        ran();
        Ok(vec![ctx.get::<At>(input)?.map_or(ABSENT, |it| it / 2)])
    }
}

impl Compute for Mid {
    type World = Facts;

    fn compute(ctx: &mut Reads<'_, Facts>, input: &u8) -> Result<Vec<u8>, StepError> {
        ran();
        let first = ctx.step::<Leaf>(input)?[0];
        let mut out = vec![*input, first];
        if first == 0 {
            out.push(ctx.step::<Leaf>(&((input + 1) % FACTS as u8))?[0]);
        }
        out.push(ctx.get::<At>(&2)?.unwrap_or(ABSENT));
        Ok(out)
    }
}

impl Compute for Top {
    type World = Facts;

    fn compute(ctx: &mut Reads<'_, Facts>, _: &u8) -> Result<Vec<u8>, StepError> {
        ran();
        let mut out = ctx.step::<Mid>(&0)?;
        out.extend(ctx.step::<Mid>(&1)?);
        Ok(out)
    }
}

/// Asks for the other of 0 and 1; any other input asks for itself.
impl Compute for Loop {
    type World = Facts;

    fn compute(ctx: &mut Reads<'_, Facts>, input: &u8) -> Result<Vec<u8>, StepError> {
        let next = if *input < 2 { 1 - *input } else { *input };
        ctx.step::<Loop>(&next)
    }
}

/// Asks for the other of 0 and 1 only while fact 0 names its own input.
impl Compute for Flip {
    type World = Facts;

    fn compute(ctx: &mut Reads<'_, Facts>, input: &u8) -> Result<Vec<u8>, StepError> {
        let mut out = vec![*input];
        if ctx.get::<At>(&0)? == Some(*input) {
            out.extend(ctx.step::<Flip>(&(1 - *input))?);
        }
        Ok(out)
    }
}

fn leaf(facts: &World3, input: u8) -> u8 {
    facts[usize::from(input)].map_or(ABSENT, |it| it / 2)
}

fn mid(facts: &World3, input: u8) -> Vec<u8> {
    let first = leaf(facts, input);
    let mut out = vec![input, first];
    if first == 0 {
        out.push(leaf(facts, (input + 1) % FACTS as u8));
    }
    out.push(facts[2].unwrap_or(ABSENT));
    out
}

fn top(facts: &World3) -> Vec<u8> {
    [mid(facts, 0), mid(facts, 1)].concat()
}

fn kernel() -> Kernel<Facts> {
    let mut kernel = Kernel::new();
    kernel.register_ask::<Leaf>(CodeId(ContentId([21; 32]))).unwrap();
    kernel.register_ask::<Mid>(CodeId(ContentId([22; 32]))).unwrap();
    kernel.register::<Top>(CodeId(ContentId([23; 32]))).unwrap();
    kernel.register_ask::<Loop>(CodeId(ContentId([24; 32]))).unwrap();
    kernel.register_ask::<Flip>(CodeId(ContentId([25; 32]))).unwrap();
    kernel
}

struct Exploring {
    kernel: Kernel<Facts>,
    store: Undoable,
    world: Facts,
    path: Vec<Did>,
    sequences: u64,
    hits: u64,
    computed: u64,
    /// Gets that hit although a step under them ran: a child computed again to the same bytes.
    cutoffs: u64,
    /// The one session every get of a sequence is in, as a build's would be: kept over the
    /// sequence's steps, made again when the kernel refuses it for a world that moved, and
    /// dropped where the sequence says.
    session: Session,
    /// Gets the session's world had moved under, refused and asked again in a new session.
    moved: u64,
}

impl Exploring {
    fn asked<S: Compute<Input = u8, World = Facts>>(
        &mut self,
        input: u8,
        fresh: Vec<u8>,
    ) -> Result<(), Failure> {
        let failed =
            |path: &[Did], why: KernelError| Failure::Kernel { after: path.to_vec(), why: why.to_string() };
        let before = RUNS.with(Cell::get);
        let mut got = self.get_in::<S>(input);
        if matches!(got, Err(KernelError::Moved { .. })) {
            // What a session's holder does when the world moved: a session of the world now.
            self.moved += 1;
            self.session = Session::new(&self.world);
            got = self.get_in::<S>(input);
        }
        let (hit, gave) = match got {
            Ok(got) => got,
            Err(why) => return Err(failed(&self.path, why)),
        };
        if gave != fresh {
            return Err(Failure::Stale { after: self.path.clone(), hit, gave, fresh });
        }
        match hit {
            true => self.hits += 1,
            false => self.computed += 1,
        }
        if hit && RUNS.with(Cell::get) != before {
            self.cutoffs += 1;
        }
        match self.kernel.find_in::<S, _>(&mut self.session, &self.store, &self.world, &input) {
            Ok(Some(found)) if found.value == gave.as_slice() => Ok(()),
            Ok(_) => Err(Failure::Lost { after: self.path.clone() }),
            Err(why) => Err(failed(&self.path, why)),
        }
    }

    fn get_in<S: Compute<Input = u8, World = Facts>>(
        &mut self,
        input: u8,
    ) -> Result<(bool, Vec<u8>), KernelError> {
        self.kernel
            .get_in::<S, _>(&mut self.session, &mut self.store, &self.world, &input, &Setting::OFF)
            .map(|got| (got.is_hit(), got.value().to_vec()))
    }

    fn ask(&mut self, which: u8) -> Result<(), Failure> {
        let facts = self.world.facts;
        match which {
            0 => self.asked::<Top>(0, top(&facts)),
            1 => self.asked::<Mid>(1, mid(&facts, 1)),
            _ => self.asked::<Leaf>(2, vec![leaf(&facts, 2)]),
        }
    }

    fn go(&mut self, left: u32) -> Result<(), Failure> {
        if left == 0 {
            self.sequences += 1;
            return Ok(());
        }
        // The session as it is here, for each way on from here: the store is undone after a
        // get, and what a session verified is of the store it was verified in.
        let session = self.session.saved();
        for at in 0..FACTS {
            let was = self.world.facts[at];
            let states = std::iter::once(None).chain((0..FACTS as u8).map(Some));
            for now in states.filter(|now| *now != was) {
                self.world.facts[at] = now;
                self.path.push(Did::Set(at as u8, now));
                self.go(left - 1)?;
                self.path.pop();
                self.session = Session::restored(&session);
            }
            self.world.facts[at] = was;
        }
        for which in 0..3 {
            let mark = self.store.put.len();
            self.path.push(Did::Get(which));
            self.ask(which)?;
            self.go(left - 1)?;
            self.path.pop();
            self.store.undo(mark);
            self.session = Session::restored(&session);
        }
        self.path.push(Did::Drop);
        self.session.clear();
        self.go(left - 1)?;
        self.path.pop();
        self.session = Session::restored(&session);
        Ok(())
    }
}

fn explore(bug: Bug) -> (Exploring, Option<Failure>) {
    planted::plant(bug);
    let mut exploring = Exploring {
        kernel: kernel(),
        store: Undoable::default(),
        world: Facts { facts: [None; FACTS] },
        path: Vec::new(),
        sequences: 0,
        hits: 0,
        computed: 0,
        cutoffs: 0,
        session: Session::new(&Facts { facts: [None; FACTS] }),
        moved: 0,
    };
    let failure = exploring.go(STEPS).err();
    planted::plant(Bug::None);
    (exploring, failure)
}

fn stale_hit(bug: Bug) -> Vec<Did> {
    match explore(bug).1 {
        Some(Failure::Stale { after, hit: true, .. }) => after,
        other => panic!("{bug:?} was planted and the oracle found {other:?}"),
    }
}

/// The invariant: over every sequence of [`STEPS`] steps (a fact changed, or a get of a step
/// that asks for steps, of one of those, or of a leaf), a get gives what the three steps make
/// of the world as it is now with no kernel, hit or computed, and a lookup straight after a
/// get finds what it kept. 28,561 sequences, each prefix checked; some gets hit although a
/// step under them ran again (early cutoff).
#[test]
fn every_nested_sequence_in_scope_gives_what_a_fresh_run_gives() {
    let (explored, failure) = explore(Bug::None);
    assert_eq!(failure, None);
    assert_eq!(explored.sequences, WAYS.pow(STEPS));
    println!(
        "{} sequences, {} hits, {} computed, {} early cutoffs",
        explored.sequences, explored.hits, explored.computed, explored.cutoffs
    );
    assert!(explored.hits > 100 && explored.computed > 100 && explored.cutoffs > 0);
    assert!(explored.moved > 100, "no get met a session of a world that had moved");
}

/// The control of "a session is of one pin": a session that answers whatever its world's pin
/// is now gives, after a fact changed, the value it verified before.
#[test]
fn a_session_that_outlives_its_pin_is_caught() {
    let after = match explore(Bug::StalePin).1 {
        Some(Failure::Stale { after, .. }) => after,
        other => panic!("a stale pin was planted and the oracle found {other:?}"),
    };
    assert!(after.iter().any(|it| matches!(it, Did::Set(..))), "{after:?}");
}

/// The control of "one read of the child's value": a parent that records its child's reads in
/// place of it, and leaves one out, gives a stale hit within the scope.
#[test]
fn a_parent_that_records_its_childs_reads_but_one_is_caught() {
    stale_hit(Bug::ChildReads);
}

/// The control of "the read sees the value's hash": a read of a step's value that sees only
/// which step it is gives a stale hit once the child's value changes.
#[test]
fn a_cutoff_by_the_childs_identity_alone_is_caught() {
    stale_hit(Bug::IdentityOnly);
}

fn root(error: &KernelError) -> &KernelError {
    match error {
        KernelError::Child { child, .. } => root(child),
        other => other,
    }
}

/// A step that asks for itself, directly or through another, is refused naming the
/// identities on the cycle, and nothing is kept.
#[test]
fn a_cycle_is_refused_naming_its_identities() {
    let kernel = kernel();
    let world = Facts { facts: [None; FACTS] };
    let mut store = Undoable::default();
    for (input, through) in [(0_u8, vec![0_u8, 1]), (2, vec![2])] {
        let refused = kernel.get::<Loop, _>(&mut store, &world, &input, &Setting::OFF).unwrap_err();
        let KernelError::Cycle { of, cycle } = root(&refused) else {
            panic!("a cycle from {input} was refused as {refused}");
        };
        let ids: Vec<ContentId> =
            through.iter().map(|it| kernel.identity::<Loop>(it).unwrap().id()).collect();
        assert_eq!(of.identity, ids[0]);
        assert_eq!(cycle.iter().map(|it| it.identity).collect::<Vec<_>>(), ids);
        assert!(refused.to_string().contains("a cycle of steps"), "{refused}");
        assert!(store.values.is_empty(), "a refused cycle kept something");
    }
}

/// A read-list is a guess at what a run would read now: a list kept when 0 asked for 1 names
/// a value that, now that 1 asks for 0, asks back. That list does not hold; it is no cycle,
/// and both give what a fresh run gives.
#[test]
fn a_stale_list_that_asks_back_is_a_miss_not_a_cycle() {
    let kernel = kernel();
    let mut world = Facts { facts: [Some(0), None, None] };
    let mut store = Undoable::default();
    let mut get = |world: &Facts, input: u8| {
        kernel.get::<Flip, _>(&mut store, world, &input, &Setting::OFF).unwrap().value().to_vec()
    };
    assert_eq!(get(&world, 0), [0, 1]);
    assert_eq!(get(&world, 1), [1]);
    world.facts[0] = Some(1);
    assert_eq!(get(&world, 1), [1, 0]);
    assert_eq!(get(&world, 0), [0]);
    world.facts[0] = Some(0);
    assert_eq!(get(&world, 0), [0, 1]);
}
