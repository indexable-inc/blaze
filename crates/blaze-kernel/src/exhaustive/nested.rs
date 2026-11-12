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
//! The session is carried ([`Kernel::carry`]): a get that finds the world moved carries the
//! session to the world as it is, with the exact difference of the facts between the two pins,
//! and asks again; a step of a sequence may also carry it where it stands (1 way), so two
//! carries meet with no get between. [`Over`] is there for the carry: `Over(0)` reads a fact
//! and a [`Mid`], and `Over(1)` reads `Over(0)`, so a value with a reader of its own reads
//! both the world and a value two steps above a fact, which is where repairing a parent
//! before its child shows; and its value says nothing of its fact, so a repair that comes to
//! the same bytes (the cutoff) is common in scope.
//!
//! A value a get asks for at the top is often one no step reads: `Over(1)` always, `Mid(1)`
//! and `Leaf(2)` until `Top` or `Mid(1)` asks for them. Each carry also checks that every such
//! root of a step that may be asked for which the session held before the carry it holds
//! after it (repaired in place, never dropped: [`Failure::Unrepaired`]); the oracle checks
//! that what it holds is fresh. `Top` may not be asked for, so it is the root a carry drops.
//!
//! The bound: `16^4 = 65,536` sequences from each of two starts, each prefix checked: an empty
//! store, and a store in which a session that is gone kept every value at the first world (so
//! a sequence admits entries it finds, not only ones it makes: [`Did::Warm`]). Four steps hold
//! every planted bug's shortest failure (a get, a change, a get; for a parent repaired first,
//! a get, two changes, a get) and the shortest early cutoff (a fact set, a get, the fact
//! changed within one leaf value, a get). A fifth step is 15 times the work; the count was
//! chosen before any timing, to stay well under a minute in a debug build.

use std::cell::Cell;

use blaze_cas::Kind;
use blaze_ids::ContentId;
use blaze_store_check::Setting;

use super::{ABSENT, At, Did, FACT, FACTS, Facts, Failure, Undoable, World3};
use crate::nest::planted::{self, Bug};
use crate::*;

const STEPS: u32 = 4;
/// A fact changed (9 ways), a get (4), the session dropped, the session carried, the session
/// saved and opened again.
const WAYS: u64 = 16;

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
step!(Over, 0x7f26, "test over");

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

/// `Over(0)` reads fact 0 and `Mid(1)`, and says nothing of the fact: whenever fact 0 changes
/// it is looked up again and comes to the same bytes unless `Mid(1)` changed, which is the
/// carry's cutoff, met in three steps (a get, a change of fact 0, a get). Any other input
/// reads `Over(0)`.
impl Compute for Over {
    type World = Facts;

    fn compute(ctx: &mut Reads<'_, Facts>, input: &u8) -> Result<Vec<u8>, StepError> {
        ran();
        if *input != 0 {
            let mut out = vec![*input];
            out.extend(ctx.step::<Over>(&0)?);
            return Ok(out);
        }
        ctx.get::<At>(&0)?;
        let mut out = vec![0];
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

fn over(facts: &World3, input: u8) -> Vec<u8> {
    if input != 0 {
        return [vec![input], over(facts, 0)].concat();
    }
    [vec![0], mid(facts, 1)].concat()
}

fn kernel() -> Kernel<Facts> {
    let mut kernel = Kernel::new();
    kernel.register_ask::<Leaf>(CodeId(ContentId([21; 32]))).unwrap();
    kernel.register_ask::<Mid>(CodeId(ContentId([22; 32]))).unwrap();
    kernel.register::<Top>(CodeId(ContentId([23; 32]))).unwrap();
    kernel.register_ask::<Loop>(CodeId(ContentId([24; 32]))).unwrap();
    kernel.register_ask::<Flip>(CodeId(ContentId([25; 32]))).unwrap();
    kernel.register_ask::<Over>(CodeId(ContentId([26; 32]))).unwrap();
    kernel
}

/// What the store holds when a sequence starts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Start {
    /// Nothing.
    Cold,
    /// Every value, kept at the first world by a session that is gone: what a restarted
    /// process, or a second one, finds.
    Warm,
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
    /// sequence's steps, carried when the kernel refuses it for a world that moved, and
    /// dropped where the sequence says.
    session: Session,
    /// The facts at the session's pin: what its holder says the difference from.
    pinned: World3,
    /// Gets the session's world had moved under, refused and asked again after a carry.
    moved: u64,
    /// What the carries did, summed.
    carried: Carried,
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
            // What a session's holder does when the world moved: it carries the session.
            self.moved += 1;
            self.carry()?;
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

    /// Carries the session to the world as it is, with what its holder knows differs: the
    /// facts that are not what they were at the session's pin, each by name, a fact that
    /// appeared or went like any other.
    fn carry(&mut self) -> Result<(), Failure> {
        let from = Facts { facts: self.pinned }.pin();
        let mut changed = Changed::between(from, self.world.pin());
        changed.exact(FACT);
        for at in 0..FACTS {
            let (was, now) = (self.pinned[at], self.world.facts[at]);
            if was == now || (planted::is(Bug::AbsenceUnsaid) && was.is_none()) {
                continue;
            }
            changed.differs(Read { kind: FACT, key: &[at as u8] });
        }
        // The values a get asks for at the top whose steps may be asked for (`Top` may not):
        // none reads `Over(1)`, and `Mid(1)` and `Leaf(2)` are read only once `Top` or `Mid(1)`
        // asked for them, so each is often a root no step reads. Each the session holds
        // before the carry it holds after it: a reached one is repaired in place, never
        // dropped (every value here can be valued at any world).
        let roots = [
            ("Mid(1)", self.kernel.identity::<Mid>(&1).unwrap().id()),
            ("Leaf(2)", self.kernel.identity::<Leaf>(&2).unwrap().id()),
            ("Over(1)", self.kernel.identity::<Over>(&1).unwrap().id()),
        ];
        let mut held = [false; 3];
        for (n, (_, id)) in roots.iter().enumerate() {
            held[n] = self.session.holds(&self.store, id).map_err(|why| Failure::Kernel {
                after: self.path.clone(),
                why: why.to_string(),
            })?;
        }
        let carried = self.kernel.carry(
            &mut self.session,
            &mut self.store,
            &self.world,
            &changed,
            &Setting::OFF,
        );
        if let Ok(Carried { dropped: None, .. }) = carried {
            for (n, (root, id)) in roots.iter().enumerate() {
                let now = self.session.holds(&self.store, id).map_err(|why| Failure::Kernel {
                    after: self.path.clone(),
                    why: why.to_string(),
                })?;
                if held[n] && !now {
                    return Err(Failure::Unrepaired { after: self.path.clone(), root: (*root).to_owned() });
                }
            }
        }
        match carried {
            Ok(carried) => {
                self.pinned = self.world.facts;
                self.carried.seeded += carried.seeded;
                self.carried.repaired += carried.repaired;
                self.carried.changed += carried.changed;
                self.carried.cut += carried.cut;
                self.carried.gone += carried.gone;
                match carried.dropped {
                    None => Ok(()),
                    Some(why) => Err(Failure::Kernel {
                        after: self.path.clone(),
                        why: format!("a carry dropped the session whole: {why:?}"),
                    }),
                }
            }
            Err(why) => Err(Failure::Kernel { after: self.path.clone(), why: why.to_string() }),
        }
    }

    fn ask(&mut self, which: u8) -> Result<(), Failure> {
        let facts = self.world.facts;
        match which {
            0 => self.asked::<Top>(0, top(&facts)),
            1 => self.asked::<Mid>(1, mid(&facts, 1)),
            2 => self.asked::<Leaf>(2, vec![leaf(&facts, 2)]),
            _ => self.asked::<Over>(1, over(&facts, 1)),
        }
    }

    fn go(&mut self, left: u32) -> Result<(), Failure> {
        if left == 0 {
            self.sequences += 1;
            return Ok(());
        }
        // The session as it is here, for each way on from here: the store is undone after a
        // get, and what a session verified is of the store it was verified in.
        let (session, pinned) = (self.session.saved(), self.pinned);
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
        for which in 0..4 {
            let mark = self.store.put.len();
            self.path.push(Did::Get(which));
            self.ask(which)?;
            self.go(left - 1)?;
            self.path.pop();
            self.store.undo(mark);
            self.session = Session::restored(&session);
            self.pinned = pinned;
        }
        self.path.push(Did::Drop);
        self.session.clear();
        self.go(left - 1)?;
        self.path.pop();
        self.session = Session::restored(&session);
        // A carry may run and keep, as a get does.
        let mark = self.store.put.len();
        self.path.push(Did::Carry);
        self.carry()?;
        self.go(left - 1)?;
        self.path.pop();
        self.store.undo(mark);
        self.session = Session::restored(&session);
        self.pinned = pinned;
        // The session as another process would have it: saved, dropped, opened from its root,
        // with nothing in memory but what the steps after this read back.
        let mark = self.store.put.len();
        self.path.push(Did::Reopen);
        self.reopen()?;
        self.go(left - 1)?;
        self.path.pop();
        self.store.undo(mark);
        self.session = Session::restored(&session);
        Ok(())
    }

    fn reopen(&mut self) -> Result<(), Failure> {
        let failed = |after: &[Did], why: KernelError| Failure::Kernel {
            after: after.to_vec(),
            why: why.to_string(),
        };
        let root = self.session.save(&mut self.store).map_err(|why| failed(&self.path, why))?;
        self.session = Session::open(&self.store, root).map_err(|why| failed(&self.path, why))?;
        Ok(())
    }

    /// Keeps every value at the world as it is, then forgets the session: the store of
    /// [`Start::Warm`].
    fn warm(&mut self) -> Result<(), Failure> {
        self.path.push(Did::Warm);
        for which in 0..4 {
            self.ask(which)?;
        }
        self.session = Session::new(&self.world);
        self.pinned = self.world.facts;
        Ok(())
    }
}

fn explore(bug: Bug, start: Start) -> (Exploring, Option<Failure>) {
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
        pinned: [None; FACTS],
        moved: 0,
        carried: Carried::default(),
    };
    let warmed = match start {
        Start::Cold => Ok(()),
        Start::Warm => exploring.warm(),
    };
    let failure = warmed.and_then(|()| exploring.go(STEPS)).err();
    planted::plant(Bug::None);
    (exploring, failure)
}

fn stale_hit(bug: Bug) -> Vec<Did> {
    match explore(bug, Start::Cold).1 {
        Some(Failure::Stale { after, hit: true, .. }) => after,
        other => panic!("{bug:?} was planted and the oracle found {other:?}"),
    }
}

/// The sequence after which a carried session gave a stale value, hit or computed from one.
fn stale_after_a_carry(bug: Bug, start: Start) -> Vec<Did> {
    match explore(bug, start).1 {
        Some(Failure::Stale { after, .. }) => after,
        other => panic!("{bug:?} was planted and the oracle found {other:?}"),
    }
}

/// The invariant: over every sequence of [`STEPS`] steps (a fact changed; a get of a step
/// that asks for steps, of one of those, of a leaf, or of a step two above a step that reads
/// a fact; the session dropped; the session carried), from an empty store and from a store
/// another session filled, a get gives what the steps make of the world as it is now with no
/// kernel, hit or computed, and a lookup straight after a get finds what it kept. The session
/// is carried from pin to pin and never made again, so this is the carry's invariant too
/// ([`Kernel::carry`]): every value a carried session gives is the fresh one, and so is every value of a session
/// saved, dropped and opened again from the store at any step (rule 26), and a reached root
/// that no step reads is repaired, not dropped. 65,536
/// sequences a start, each prefix checked; some gets hit although a step under them ran
/// again (early cutoff), and some carries stopped at an entry whose value came out the same.
#[test]
fn every_nested_sequence_in_scope_gives_what_a_fresh_run_gives() {
    for start in [Start::Cold, Start::Warm] {
        let (explored, failure) = explore(Bug::None, start);
        assert_eq!(failure, None, "{start:?}");
        assert_eq!(explored.sequences, WAYS.pow(STEPS));
        println!(
            "{start:?}: {} sequences, {} hits, {} computed, {} early cutoffs, {} gets after a carry, carries {:?}",
            explored.sequences,
            explored.hits,
            explored.computed,
            explored.cutoffs,
            explored.moved,
            explored.carried
        );
        assert!(explored.hits > 100 && explored.computed > 100 && explored.cutoffs > 0);
        assert!(explored.moved > 100, "no get met a session of a world that had moved");
        let carried = explored.carried;
        assert!(
            carried.seeded > 100 && carried.repaired > 100 && carried.gone > 100,
            "{carried:?}"
        );
        assert!(carried.changed > 100 && carried.cut > 100, "{carried:?}");
    }
}

/// The control of I1, a hit: an entry found in the store and admitted with no posting is not
/// reached by the change of a fact it read, and the carried session gives it stale. From the
/// warm start: the entries are another session's.
#[test]
fn a_hit_admitted_without_postings_is_caught() {
    let after = stale_after_a_carry(Bug::HitUnposted, Start::Warm);
    assert!(after.iter().any(|it| matches!(it, Did::Set(..))), "{after:?}");
}

/// The control of I1, the time: postings written at the next carry, after its seed, miss the
/// change that carry is of.
#[test]
fn a_posting_written_at_the_next_carry_is_caught() {
    let after = stale_after_a_carry(Bug::PostLate, Start::Cold);
    assert!(after.iter().any(|it| matches!(it, Did::Set(..))), "{after:?}");
}

/// The control of I2: a change set that leaves out a fact that appeared where nothing was
/// leaves the entries that saw nothing there verified.
#[test]
fn an_absence_left_out_of_the_change_set_is_caught() {
    let after = stale_after_a_carry(Bug::AbsenceUnsaid, Start::Cold);
    assert!(after.iter().any(|it| matches!(it, Did::Set(_, Some(_)))), "{after:?}");
}

/// The control of the repair of a value no step reads: a carry that keeps such a row as it
/// was, in place of looking it up again, gives it stale once its value changed (`Mid(1)`
/// asked, fact 2 changed, `Mid(1)` asked again: three steps). The invariant's own check that
/// a reached root is repaired, not dropped, holds under the bug (the row is live), so it is
/// the oracle that catches it.
#[test]
fn a_root_kept_without_its_repair_is_caught() {
    let after = stale_after_a_carry(Bug::RootKept, Start::Cold);
    assert!(after.iter().any(|it| matches!(it, Did::Set(..))), "{after:?}");
    assert!(after.iter().any(|it| matches!(it, Did::Get(1..))), "{after:?}");
}

/// The control of rule 26 for the session's records: a session opened from the store that
/// reads no posting back (it "starts empty" of readers) is carried past an entry it holds.
#[test]
fn a_session_opened_without_its_postings_is_caught() {
    let after = stale_after_a_carry(Bug::OpenUnposted, Start::Cold);
    assert!(after.iter().any(|it| matches!(it, Did::Reopen)), "{after:?}");
    assert!(after.iter().any(|it| matches!(it, Did::Set(..))), "{after:?}");
}

/// The control of I3: a carry that repairs the highest entry first, and none twice, looks a
/// parent up against a child that is still as it was.
#[test]
fn a_parent_repaired_before_its_child_is_caught() {
    let after = stale_after_a_carry(Bug::ParentFirst, Start::Cold);
    let sets = after.iter().filter(|it| matches!(it, Did::Set(..))).count();
    assert!(sets >= 2, "one change queues no parent beside a child: {after:?}");
}

/// The control of "a session is of one pin": a session that answers whatever its world's pin
/// is now gives, after a fact changed, the value it verified before.
#[test]
fn a_session_that_outlives_its_pin_is_caught() {
    let after = match explore(Bug::StalePin, Start::Cold).1 {
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
