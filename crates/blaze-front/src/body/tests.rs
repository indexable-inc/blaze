//! The body task's own logic, over a model universe: a body asks facts of surfaces (by kind and
//! subject, absent ones included), facts other bodies make, maybe something refused; its payload
//! is a function of every answer it got and its own text (it reads by value); it makes an
//! effect summary of its own and maybe a const value. The model stands in for the front end
//! behind [`Check`] only; the asks, the shape, the refusals, the made facts' table and the
//! record's bytes are the real code's, and every answer is `Universe::answer`'s.
//!
//! The oracle of reuse is the driver's (read-addressed.md): a record is found again exactly
//! when its key and the answers its shape gets now give its address
//! (`record::address(key, answers now)`), and blaze-keyed's `Audit` policy redoes a found record
//! and compares bytes. Each test that edits the world checks against a rerun.

use std::{alloc::Global, collections::BTreeMap};

use blaze_format::Aligned;
use blaze_ids::{ContentId, Hasher};
use blaze_keyed::{
    driver::TaskFailed,
    front::{Table, Universe},
    kind::{Body, BodyInputs, Key, Kind, Namespace, Settings, Steps, ToolId},
    record::{self, Fact, FactKind, Owner, StableId, Traced, digest},
};
use blaze_rng::SplitMix64;

use super::{
    Check, body,
    exports::{Export, Exports, NOT_OURS, TWO_EXPORTS},
    payload::{self, Writer},
    reads::{Asks, Row},
};

/// Every surface kind a body's check asks in the model.
const READ_KINDS: [FactKind; 5] = [FactKind::Signature, FactKind::Fields, FactKind::Layout, FactKind::ImplRows, FactKind::Binding];

const REFUSED: &str = "model: an environment variable read outside `env!`";

/// The unit whose bodies are checked, and a dependency.
const OWN: Owner = Owner([1; 8]);
const DEP: Owner = Owner([2; 8]);

/// The subject (an item key) `n` of the crate `owner`: its first eight bytes the owner's.
fn subject(owner: Owner, n: u8) -> ContentId {
    let mut bytes = ContentId::of("model: a subject", &[n]).0;
    bytes[..8].copy_from_slice(&owner.0);
    ContentId(bytes)
}

/// The driver's id of body `n` of the unit (blaze-unitkey's, no owner in it).
fn item(n: u8) -> StableId {
    StableId::of(&[n], 1)
}

/// The id of a fact of `kind` about `of`: a made kind as its maker writes it, a surface kind
/// by `record::fact` with the subject's owner.
fn id(kind: FactKind, of: ContentId) -> ContentId {
    match kind {
        FactKind::ConstValue => Export::ConstValue.id(&of),
        FactKind::Effects => Export::Effects.id(&of),
        _ => record::fact(kind, Owner::of_subject(&of), kind as u16, &StableId(of), b""),
    }
}

/// One read a model body's check makes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reads {
    Fact(FactKind, ContentId),
    Refused(&'static str),
}

#[derive(Clone, Debug)]
struct ModelBody {
    /// Its item key: the subject of what it makes.
    subject: ContentId,
    /// Its item's own text: its key.
    text: Vec<u8, Global>,
    /// What its body is apart from its text: what its effect summary digests.
    body: Vec<u8, Global>,
    /// The value it makes, if it is a const.
    value: Option<Vec<u8, Global>>,
    reads: Vec<Reads, Global>,
}

#[derive(Clone, Debug)]
struct World {
    /// Each owner's surface rows.
    rows: BTreeMap<Owner, BTreeMap<ContentId, ContentId>>,
    /// Each owner's made table.
    made: BTreeMap<Owner, BTreeMap<ContentId, ContentId>>,
    bodies: BTreeMap<StableId, ModelBody>,
}

impl World {
    fn new() -> World {
        let mut rows = BTreeMap::new();
        rows.insert(OWN, BTreeMap::new());
        rows.insert(DEP, BTreeMap::new());
        let mut made = BTreeMap::new();
        made.insert(OWN, BTreeMap::new());
        made.insert(DEP, BTreeMap::new());
        World { rows, made, bodies: BTreeMap::new() }
    }

    fn set(&mut self, kind: FactKind, of: ContentId, value: &[u8]) {
        let table = match kind.made_by_body() {
            true => &mut self.made,
            false => &mut self.rows,
        };
        table.get_mut(&Owner::of_subject(&of)).expect("a model owner").insert(id(kind, of), digest(value));
    }

    fn remove(&mut self, kind: FactKind, of: ContentId) {
        let table = match kind.made_by_body() {
            true => &mut self.made,
            false => &mut self.rows,
        };
        table.get_mut(&Owner::of_subject(&of)).expect("a model owner").remove(&id(kind, of));
    }

    fn key(&self, of: StableId) -> Key<Body> {
        let tool = ToolId { tool: digest(b"tool"), target: digest(b"target"), namespace: Namespace::LOCAL };
        Body::key(&BodyInputs {
            tool,
            settings: Settings(digest(b"settings")),
            unit: digest(b"unit"),
            item: of,
            text: digest(&self.bodies[&of].text),
        })
    }

    /// The universe of the unit's body tasks: its own table (unit 0) and its dependency's, each
    /// as a host hands them.
    fn with<R>(&self, f: impl FnOnce(Universe<'_>) -> R) -> R {
        let rows: Vec<(Owner, Vec<Fact, Global>, Vec<Fact, Global>), Global> = {
            let mut out = Vec::new_in(Global);
            for (owner, facts) in &self.rows {
                let mut surface = Vec::new_in(Global);
                surface.extend(facts.iter().map(|(&id, &digest)| Fact { id, digest }));
                let mut made = Vec::new_in(Global);
                made.extend(self.made[owner].iter().map(|(&id, &digest)| Fact { id, digest }));
                out.push((*owner, surface, made));
            }
            out
        };
        let mut tables = Vec::new_in(Global);
        for (owner, facts, made) in &rows {
            let unit = if *owner == OWN { 0 } else { 1 };
            tables.push(Table { unit, owner: *owner, surface: &[], facts, made });
        }
        f(Universe { asker: 0, tables: &tables, outside: &[] })
    }
}

/// The model front end, with a fact kind it may read past [`Asks`] (the planted bug), and an
/// order to ask in.
struct Model<'w, 'u> {
    world: &'w World,
    universe: Universe<'u>,
    sneak: Option<FactKind>,
    reverse: bool,
}

impl Check for Model<'_, '_> {
    type Def = StableId;

    fn def(&self, item: StableId) -> Result<StableId, TaskFailed> {
        match self.world.bodies.contains_key(&item) {
            true => Ok(item),
            false => Err(TaskFailed::Refused { what: "model: no such body" }),
        }
    }

    fn check<A: std::alloc::Allocator>(&self, def: StableId, _steps: Steps, asks: &mut Asks<'_, A>, payload: &mut Writer) -> Result<(), TaskFailed> {
        let body = &self.world.bodies[&def];
        let mut reads = Vec::new_in(Global);
        reads.extend_from_slice(&body.reads);
        if self.reverse {
            reads.reverse();
        }
        // By value: each answer goes into what the check makes, in the body's own order.
        let mut answers = BTreeMap::new();
        for read in reads {
            match read {
                Reads::Fact(kind, of) => {
                    let id = id(kind, of);
                    let answer = match self.sneak == Some(kind) {
                        // The plant: read past the one reader, so the shape lacks it.
                        true => self.universe.answer(&id).map_err(|it| it.failed())?,
                        false => asks.ask(&id)?,
                    };
                    answers.insert(id, answer);
                }
                Reads::Refused(what) => asks.offer(Row::Refused(what))?,
            }
        }
        let mut hasher = Hasher::new("model: a body's check");
        hasher.part(&body.text);
        for (id, answer) in &answers {
            hasher.id(id).id(answer);
        }
        let answer: ContentId = hasher.finish();
        payload.push(0, digest(b"model: the check's schema"), &answer.0);
        Ok(())
    }

    fn exports(&self, def: StableId, out: &mut Exports) -> Result<(), TaskFailed> {
        let body = &self.world.bodies[&def];
        out.export(&body.subject, Export::Effects, digest(&body.body));
        if let Some(value) = &body.value {
            out.export(&body.subject, Export::ConstValue, digest(value));
        }
        Ok(())
    }
}

fn run(world: &World, of: StableId, sneak: Option<FactKind>, reverse: bool) -> Result<Aligned, TaskFailed> {
    world.with(|universe| body(&Model { world, universe, sneak, reverse }, of, world.key(of), universe))
}

fn fresh(world: &World, of: StableId) -> Aligned {
    run(world, of, None, false).expect("a fresh run of a model body")
}

/// The record's root, checked as the driver checks a task's record: its head is its address.
fn traced<'b>(world: &World, of: StableId, bytes: &'b [u8]) -> (Key<Body>, &'b Traced) {
    record::check_read::<Body>(bytes, world.key(of)).expect("a read-addressed body record")
}

fn shape_of(world: &World, of: StableId, bytes: &[u8]) -> Vec<Fact, Global> {
    let mut out = Vec::new_in(Global);
    out.extend_from_slice(traced(world, of, bytes).1.reads.follow(bytes).expect("the record's shape"));
    out
}

/// The address `shape` gives on `world` now: what the driver looks the key's record up by.
fn address_now(world: &World, of: StableId, shape: &[Fact]) -> Key<Body> {
    world.with(|universe| {
        let mut now = Vec::new_in(Global);
        now.extend(shape.iter().map(|fact| Fact { id: fact.id, digest: universe.answer(&fact.id).expect("an answer") }));
        record::address(world.key(of), &now)
    })
}

/// A world of three bodies of the unit and facts of subjects 10..14 of the dependency of every
/// read kind; body 0 reads one fact of each kind of subject 10 and one absent `Binding` of 20.
fn base() -> World {
    let mut world = World::new();
    for n in 10..14 {
        for kind in READ_KINDS {
            world.set(kind, subject(DEP, n), &[n, kind as u8, 0]);
        }
    }
    let mut reads = Vec::new_in(Global);
    reads.extend(READ_KINDS.iter().map(|&kind| Reads::Fact(kind, subject(DEP, 10))));
    reads.push(Reads::Fact(FactKind::Binding, subject(DEP, 20)));
    for n in 0..3 {
        let mut text = Vec::new_in(Global);
        text.extend_from_slice(b"fn f() {}");
        text.push(n);
        let mut body = Vec::new_in(Global);
        body.push(n);
        // clone: a model body's read list
        world.bodies.insert(item(n), ModelBody { subject: subject(OWN, n), text, body, value: None, reads: reads.clone() });
    }
    world
}

/// The invariant (b): a record's shape replays: every fact of it answered again on the same
/// universe gives the digest the shape holds, the shape is sorted by id with each id once, the
/// record's head is its address, and its made facts are sorted; whatever facts and asks were
/// drawn. 200 seeds.
#[test]
fn a_shape_replays_on_the_same_universe() {
    for seed in 0..200 {
        let mut rng = SplitMix64::new(seed);
        let mut world = base();
        for n in 10..14 {
            for kind in READ_KINDS {
                if rng.coin() {
                    world.remove(kind, subject(DEP, n));
                }
            }
        }
        let mut reads = Vec::new_in(Global);
        for _ in 0..rng.below(13) {
            let kind = READ_KINDS[rng.index(READ_KINDS.len())];
            reads.push(Reads::Fact(kind, subject(DEP, 10 + rng.below(5) as u8)));
        }
        world.bodies.get_mut(&item(0)).expect("body 0").reads = reads;
        let bytes = fresh(&world, item(0));
        let shape = shape_of(&world, item(0), &bytes);
        world.with(|universe| {
            for fact in &shape {
                assert_eq!(universe.answer(&fact.id), Ok(fact.digest), "seed {seed}: a shape that does not replay");
            }
        });
        assert!(shape.windows(2).all(|it| it[0].id < it[1].id), "seed {seed}: not sorted by id, each once: {shape:?}");
        let (at, root) = traced(&world, item(0), &bytes);
        assert_eq!(address_now(&world, item(0), &shape), at, "seed {seed}: the address is not the shape's");
        let made = root.facts.follow(&bytes).expect("the record's made facts");
        assert!(made.windows(2).all(|it| it[0].id < it[1].id), "seed {seed}");
    }
}

/// The invariant (a): a body that asks a fact another body makes lists it in its shape with
/// the digest the made table gave, and a made fact no body made with `record::absent`. Control:
/// the maker's row taken out of the made table, the same ask is absent and the address moves.
#[test]
fn a_made_fact_of_another_body_is_asked_from_the_made_table() {
    let mut world = base();
    let maker = world.bodies.get_mut(&item(1)).expect("body 1");
    let mut value = Vec::new_in(Global);
    value.extend_from_slice(b"const K: u32 = 3");
    maker.value = Some(value);
    let maker_subject = maker.subject;
    // The made table as the driver keeps it: the maker's own record's made facts.
    let made = fresh(&world, item(1));
    let made_rows = traced(&world, item(1), &made).1.facts.follow(&made).expect("made facts");
    for fact in made_rows {
        world.made.get_mut(&OWN).expect("own").insert(fact.id, fact.digest);
    }
    let never = subject(OWN, 9);
    let reader = &mut world.bodies.get_mut(&item(0)).expect("body 0").reads;
    reader.push(Reads::Fact(FactKind::ConstValue, maker_subject));
    reader.push(Reads::Fact(FactKind::ConstValue, never));

    let bytes = fresh(&world, item(0));
    let shape = shape_of(&world, item(0), &bytes);
    let row = |id: ContentId| shape.iter().find(|fact| fact.id == id).map(|fact| fact.digest);
    let made_id = Export::ConstValue.id(&maker_subject);
    assert_eq!(record::route(&made_id), Ok(record::Route::Made(OWN)));
    assert_eq!(row(made_id), Some(digest(b"const K: u32 = 3")), "the made fact's digest is not the made table's");
    let never_id = Export::ConstValue.id(&never);
    assert_eq!(row(never_id), Some(record::absent(&never_id)), "a fact no body made is not absent");

    // Control: without the maker's row, the ask is absent and the record is found elsewhere.
    let mut unmade = world.clone();
    unmade.made.get_mut(&OWN).expect("own").remove(&made_id);
    let without = fresh(&unmade, item(0));
    let without_shape = shape_of(&unmade, item(0), &without);
    assert_eq!(
        without_shape.iter().find(|fact| fact.id == made_id).map(|fact| fact.digest),
        Some(record::absent(&made_id))
    );
    assert_ne!(traced(&world, item(0), &bytes).0, traced(&unmade, item(0), &without).0, "the made fact moved no address");
}

/// The invariant: for each fact kind a check asks, a change of a fact the body asked moves the
/// address its shape gives (the record is redone), and a change of a fact of the same kind it
/// did not ask does not (the same address, and a rerun writes the same bytes).
#[test]
fn a_change_to_an_asked_fact_moves_the_address_and_to_another_does_not() {
    let world = base();
    let bytes = fresh(&world, item(0));
    let (at, _) = traced(&world, item(0), &bytes);
    let shape = shape_of(&world, item(0), &bytes);
    for kind in READ_KINDS {
        let mut asked = world.clone();
        asked.set(kind, subject(DEP, 10), b"changed");
        assert_ne!(address_now(&asked, item(0), &shape), at, "{kind:?}: the change is not seen");

        // Control: the same kind's fact of a subject the body did not ask.
        let mut unasked = world.clone();
        unasked.set(kind, subject(DEP, 11), b"changed");
        assert_eq!(address_now(&unasked, item(0), &shape), at, "{kind:?}: an unasked fact's change is seen");
        assert_eq!(&*fresh(&unasked, item(0)), &*bytes, "{kind:?}: an unasked fact's change moved the record");
    }
    // A negative read: the name appears, and the address moves; another name's does not.
    let mut appears = world.clone();
    appears.set(FactKind::Binding, subject(DEP, 20), b"now bound");
    assert_ne!(address_now(&appears, item(0), &shape), at, "a name that appears is not seen");
    let mut elsewhere = world.clone();
    elsewhere.set(FactKind::Binding, subject(DEP, 21), b"bound elsewhere");
    assert_eq!(address_now(&elsewhere, item(0), &shape), at, "another name's binding moved an absent row");
    // The own text: it is the key, so an edit of it is another key.
    let mut own = world.clone();
    own.bodies.get_mut(&item(0)).expect("body 0").text.push(b'x');
    assert_ne!(own.key(item(0)), world.key(item(0)), "an edit of the body's own text is not seen");
}

/// The invariant (c): a fact the check read and did not ask through [`Asks`] (planted in the
/// model, one kind at a time) is a hole the address cannot see: after a change of that fact the
/// old shape gives the old address (the driver would take the old record), and the audit's
/// rerun on the changed universe writes other bytes, so the comparison catches it. Controls:
/// with nothing changed the planted rerun is byte-equal (the audit raises nothing false); with
/// nothing planted, the change moves the address and no stale record is found.
#[test]
fn a_planted_missing_ask_is_caught_by_a_rerun() {
    let world = base();
    for kind in READ_KINDS {
        let old = run(&world, item(0), Some(kind), false).expect("a planted run");
        let (old_at, _) = traced(&world, item(0), &old);
        let old_shape = shape_of(&world, item(0), &old);
        let mut changed = world.clone();
        changed.set(kind, subject(DEP, 10), b"changed");
        let hit = address_now(&changed, item(0), &old_shape) == old_at;
        let rerun = run(&changed, item(0), Some(kind), false).expect("the audit's rerun");
        assert!(hit && *rerun != *old, "{kind:?}: the planted missing ask is not caught (hit: {hit})");

        // Control: nothing changed, the planted rerun is the same bytes.
        let same = run(&world, item(0), Some(kind), false).expect("a planted rerun");
        assert_eq!(&*same, &*old, "{kind:?}: the audit would raise a difference with nothing changed");
        // Control: no plant, the change is seen by the address.
        let honest = fresh(&world, item(0));
        let (honest_at, _) = traced(&world, item(0), &honest);
        let honest_shape = shape_of(&world, item(0), &honest);
        assert_ne!(address_now(&changed, item(0), &honest_shape), honest_at, "{kind:?}: an honest shape missed the change");
    }
}

/// The invariant: the record is a function of the universe alone: two runs write equal bytes,
/// and so does a run that asks in another order; a fact asked twice is one row. 100 seeds.
#[test]
fn the_record_is_deterministic() {
    for seed in 0..100 {
        let mut rng = SplitMix64::new(seed);
        let mut world = base();
        let n = rng.below(3) as u8;
        if rng.coin() {
            world.set(FactKind::Layout, subject(DEP, 10), &[rng.next_u32() as u8]);
        }
        let reads = &mut world.bodies.get_mut(&item(n)).expect("a body").reads;
        reads.push(Reads::Fact(FactKind::Fields, subject(DEP, 12)));
        reads.push(Reads::Fact(FactKind::Fields, subject(DEP, 12)));
        let once = fresh(&world, item(n));
        let twice = fresh(&world, item(n));
        let reversed = run(&world, item(n), None, true).expect("a reversed run");
        assert_eq!(&*once, &*twice, "seed {seed}: two runs, two records");
        assert_eq!(&*once, &*reversed, "seed {seed}: the order of the asks moved the record");
        let shape = shape_of(&world, item(n), &once);
        let fields = id(FactKind::Fields, subject(DEP, 12));
        assert_eq!(shape.iter().filter(|fact| fact.id == fields).count(), 1, "seed {seed}");
    }
}

/// The invariant: a body whose check reads a refused kind fails by that kind's name.
#[test]
fn a_refused_read_fails_by_its_name() {
    let mut world = base();
    world.bodies.get_mut(&item(0)).expect("body 0").reads.push(Reads::Refused(REFUSED));
    assert_eq!(run(&world, item(0), None, false).err(), Some(TaskFailed::Refused { what: REFUSED }));
}

/// The invariant: a body record depends on another body only through what it asks. A callee's
/// body edit that moves its effect summary leaves a caller that asked its signature at the same
/// key and bytes; control: a caller that asked the callee's const value, whose edit moves it,
/// is at another address.
#[test]
fn a_callee_body_edit_moves_only_its_askers() {
    let mut world = base();
    let callee = item(1);
    let callee_subject = world.bodies[&callee].subject;
    world.set(FactKind::Signature, callee_subject, b"fn callee() -> u8");
    world.bodies.get_mut(&item(0)).expect("caller").reads.push(Reads::Fact(FactKind::Signature, callee_subject));
    world.set(FactKind::Effects, callee_subject, &[1]);
    let caller = fresh(&world, item(0));

    let mut edited = world.clone();
    edited.bodies.get_mut(&callee).expect("callee").body.push(b'!');
    edited.set(FactKind::Effects, callee_subject, b"another summary");
    assert_eq!(world.key(item(0)), edited.key(item(0)), "a callee body edit moved the caller's key");
    assert_eq!(&*fresh(&edited, item(0)), &*caller, "a callee body edit moved the caller's record");

    // Control: a caller that asks the callee's const value.
    world.set(FactKind::ConstValue, callee_subject, b"1");
    world.bodies.get_mut(&item(2)).expect("asker").reads.push(Reads::Fact(FactKind::ConstValue, callee_subject));
    let asker = fresh(&world, item(2));
    let shape = shape_of(&world, item(2), &asker);
    let mut moved = world.clone();
    moved.set(FactKind::ConstValue, callee_subject, b"2");
    assert_ne!(address_now(&moved, item(2), &shape), traced(&world, item(2), &asker).0, "a made fact's change is not seen");
}

/// The invariant: a body's made facts route to its own unit's made table, each once: a fact
/// of another crate's owner, or one fact with two digests, is a typed refusal.
#[test]
fn made_facts_are_its_units_each_once() {
    let mut other = Exports::new();
    other.export(&subject(DEP, 1), Export::Effects, digest(b"a"));
    assert_eq!(other.finish(OWN, Global).err(), Some(TaskFailed::Refused { what: NOT_OURS }));
    let mut twice = Exports::new();
    twice.export(&subject(OWN, 1), Export::Effects, digest(b"a"));
    twice.export(&subject(OWN, 1), Export::Effects, digest(b"b"));
    assert_eq!(twice.finish(OWN, Global).err(), Some(TaskFailed::Refused { what: TWO_EXPORTS }));
    // Control: one digest, the unit's own owner.
    let mut once = Exports::new();
    once.export(&subject(OWN, 1), Export::Effects, digest(b"a"));
    once.export(&subject(OWN, 1), Export::Effects, digest(b"a"));
    assert_eq!(once.finish(OWN, Global).map(|it| it.len()), Ok(1));
}

/// The invariant: a payload reads back as the sections written, sorted by (step, schema),
/// and bytes with a section cut short are refused.
#[test]
fn a_payload_round_trips() {
    let mut writer = Writer::new();
    writer.push(2, digest(b"b"), b"borrowck");
    writer.push(0, digest(b"a"), b"inference!");
    let bytes = writer.finish(Global);
    let read = payload::sections(&bytes, Global).expect("a payload");
    assert_eq!(read.len(), 2);
    assert_eq!((read[0].step, read[0].bytes), (0, &b"inference!"[..]));
    assert_eq!((read[1].step, read[1].bytes), (2, &b"borrowck"[..]));
    assert!(payload::sections(&bytes[..bytes.len() - 8], Global).is_err());
}
