//! The host end to end: a real [`PlainStore`] in a temporary directory, the real [`Disk`], the
//! real pool, and a scripted [`Front`] over two small text units:
//!
//! - unit 0: `new`, `a` (calls `new`), `b`, `k` (a const: its body makes a const value);
//! - unit 1, on unit 0: `run` (calls `a`), `c` (calls `new`), `d` (reads `k`'s value).
//!
//! An item's text is its name, signature, body and calls; its body's key is its text. A body
//! asks, per call, the callee's signature fact, and `d` asks the const value `k`'s body makes:
//! each answered by [`Universe::answer`] over the tables the host hands it (the callee's unit's
//! surface, unit 0's manifest root's made table). A surface asks the signature of every item of
//! its dependencies its own items call, and makes a signature row per item and its seen row.
//! Keys are text keys, as blaze-unitkey's: a unit's text key is its file image's.
//!
//! Each test states the claim it checks and runs its control: the same request without the
//! cause, where the effect goes.

use std::{
    alloc::{Allocator, Global},
    collections::BTreeMap,
    marker::PhantomData,
    path::Path,
    time::Duration,
};

use blaze_cas::plain::{Form, PlainStore, Settings as StoreSettings};
use blaze_core::{lock::Lock, temp::TempDir};
use blaze_format::Aligned;
use blaze_ids::ContentId;
use blaze_keyed::{
    driver::{Failure, Filed, Outcome, Plan, Ran, StoreFault, TaskFailed, UnitPlan},
    front::{FileItems, Front, Keyed, Unanswerable, Universe},
    kind::{
        Body, BodyInputs, Certificate, CertificateInputs, FileImage, FileInputs, Key, Kind, Namespace, Settings, Surface,
        SurfaceInputs, Tag, ToolId, UnitInputs, UnitManifest,
    },
    machine::Machine,
    memo::{Audit, Memo, RecordsOnly},
    proof,
    record::{self, Fact, FactKind, Owner, StableId},
    store::{Compacted, Generation, Put, Read, RootRef, Store, StoreError},
};
use blaze_sim::{Config, Tape};

use crate::{
    HostError, Request,
    disk::Disk,
    outside::{Ask, Leaves, file_answer},
    run,
};

const TOOL: ToolId = ToolId { tool: ContentId([1; 32]), target: ContentId([4; 32]), namespace: Namespace::LOCAL };
const SETTINGS: Settings = Settings(ContentId([2; 32]));
const DEPS: [&[u32]; 2] = [&[], &[0]];
const MACHINE: Machine = Machine {
    cores: 4,
    fast_cores: 4,
    page_bytes: 16384,
    memory_bytes: 16 << 30,
    qos: false,
    huge_pages: false,
};

// --- the world --------------------------------------------------------------------------------

#[derive(Clone, PartialEq, Eq, Debug)]
struct Item {
    name: &'static str,
    sig: u8,
    body: u8,
    calls: &'static [&'static str],
}

const fn item(name: &'static str, calls: &'static [&'static str]) -> Item {
    Item { name, sig: 1, body: 1, calls }
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct World {
    units: [Vec<Item>; 2],
}

fn world() -> World {
    World {
        units: [
            vec![item("new", &[]), item("a", &["new"]), item("b", &[]), item("k", &[])],
            vec![item("run", &["a"]), item("c", &["new"]), item("d", &[])],
        ],
    }
}

impl World {
    fn text(&self, unit: u32) -> Vec<u8> {
        let mut text = String::new();
        for item in &self.units[unit as usize] {
            text.push_str(&format!("{} {} {} {}\n", item.name, item.sig, item.body, item.calls.join(",")));
        }
        text.into_bytes()
    }

    fn edit(&self, unit: u32, name: &str, edit: impl FnOnce(&mut Item)) -> World {
        let mut next = self.clone();
        let item = next.units[unit as usize].iter_mut().find(|item| item.name == name).expect("the edited item is in its unit");
        edit(item);
        next
    }

    fn sig(&self, unit: u32, name: &str) -> World {
        self.edit(unit, name, |item| item.sig += 1)
    }

    fn body(&self, unit: u32, name: &str) -> World {
        self.edit(unit, name, |item| item.body += 1)
    }

    fn item(&self, unit: u32, id: StableId) -> Option<&Item> {
        self.units[unit as usize].iter().find(|item| stable(unit, item.name) == id)
    }

    /// What `call` in unit `unit` is bound to: an item of the unit, else of its dependencies.
    fn resolve(&self, unit: u32, call: &str) -> Option<(u32, &Item)> {
        std::iter::once(unit)
            .chain(DEPS[unit as usize].iter().copied())
            .find_map(|at| self.units[at as usize].iter().find(|item| item.name == call).map(|item| (at, item)))
    }

    /// The items of its dependencies unit `unit`'s items call: what its surface asks.
    fn dep_callees(&self, unit: u32) -> Vec<(u32, &'static str)> {
        let mut callees = Vec::new();
        for item in &self.units[unit as usize] {
            for call in item.calls {
                if let Some((at, callee)) = self.resolve(unit, call)
                    && at != unit
                {
                    callees.push((at, callee.name));
                }
            }
        }
        callees
    }

    /// The value `k`'s body makes.
    fn const_value(&self) -> ContentId {
        let k = self.units[0].iter().find(|item| item.name == "k").expect("unit 0 has `k`");
        record::digest(&[b'k', k.body])
    }
}

fn unit_id(unit: u32) -> ContentId {
    ContentId([0x10 + unit as u8; 32])
}

fn owner(unit: u32) -> Owner {
    Owner([0x10 + unit as u8; 8])
}

fn stable(unit: u32, name: &str) -> StableId {
    StableId::of(format!("u{unit}::{name}").as_bytes(), 1)
}

fn sig_fact(unit: u32, name: &str) -> ContentId {
    record::fact(FactKind::Signature, owner(unit), 1, &stable(unit, name), b"")
}

fn sig_digest(sig: u8) -> ContentId {
    record::digest(&[b's', sig])
}

/// The const value `k`'s body makes, as `d` asks it.
fn const_fact() -> ContentId {
    record::fact(FactKind::ConstValue, owner(0), 2, &stable(0, "k"), b"")
}

fn text_of(item: &Item) -> ContentId {
    record::digest(format!("{} {} {} {:?}", item.name, item.sig, item.body, item.calls).as_bytes())
}

// --- keys, from text only ---------------------------------------------------------------------

fn certificate(base: u8) -> Key<Certificate> {
    Certificate::key(&CertificateInputs { tool: TOOL, base: ContentId([base; 32]), of: PhantomData })
}

fn surface_key(world: &World, unit: u32) -> Key<Surface> {
    let pieces: Vec<ContentId> = world.units[unit as usize]
        .iter()
        .map(|item| record::digest(format!("{} {}", item.name, item.sig).as_bytes()))
        .collect();
    Surface::key(&SurfaceInputs { tool: TOOL, settings: SETTINGS, unit: unit_id(unit), pieces: &pieces })
}

fn body_key(unit: u32, item: &Item) -> Key<Body> {
    Body::key(&BodyInputs { tool: TOOL, settings: SETTINGS, unit: unit_id(unit), item: stable(unit, item.name), text: text_of(item) })
}

fn file_key(world: &World, unit: u32) -> Key<FileImage> {
    FileImage::key(&FileInputs { tool: TOOL, edition: 2024, bytes: &world.text(unit) })
}

fn manifest_key(world: &World, unit: u32) -> Key<UnitManifest> {
    let files = [file_key(world, unit)];
    UnitManifest::key(&UnitInputs { tool: TOOL, settings: SETTINGS, unit: unit_id(unit), files: &files, late: &[] })
}

// --- the scripted front -----------------------------------------------------------------------

struct Scripted {
    world: World,
    /// A body task that panics: (unit, item).
    panics: Option<(u32, &'static str)>,
    /// The bodies write how many tasks ran before them into their payload: other bytes at one
    /// address, which only an audit sees.
    counts: bool,
    log: Lock<Vec<String>>,
}

impl Scripted {
    fn new(world: World) -> Scripted {
        Scripted { world, panics: None, counts: false, log: Lock::new("test.front.log", Vec::new()) }
    }

    fn note(&self, what: String) -> usize {
        let mut log = self.log.lock();
        log.push(what);
        log.len()
    }

    /// What ran, sorted, whose name starts with `prefix` (`key`, `surface`, `body`, and those
    /// with a unit: `body u1`).
    fn ran(&self, prefix: &str) -> Vec<String> {
        let mut ran: Vec<String> = self.log.lock().iter().filter(|it| it.starts_with(prefix)).cloned().collect();
        ran.sort();
        ran
    }

    fn calls(&self) -> usize {
        self.log.lock().len()
    }
}

/// The answer now of `id` in `universe`, recorded in `asked`.
fn ask_of(universe: &Universe<'_>, asked: &mut BTreeMap<ContentId, ContentId>, order: &mut Vec<u8>, id: ContentId) -> Result<(), TaskFailed> {
    let answer = universe.answer(&id).map_err(Unanswerable::failed)?;
    asked.insert(id, answer);
    order.extend_from_slice(&answer.0);
    Ok(())
}

fn facts_of(asked: &BTreeMap<ContentId, ContentId>) -> Vec<Fact> {
    asked.iter().map(|(id, digest)| Fact { id: *id, digest: *digest }).collect()
}

impl Front for Scripted {
    fn key<A: Allocator + Clone>(&self, unit: u32, frame: A) -> Result<Keyed<A>, TaskFailed> {
        self.note(format!("key u{unit}"));
        let mut items = Vec::new_in(frame.clone());
        for item in &self.world.units[unit as usize] {
            items.push((stable(unit, item.name), body_key(unit, item)));
        }
        items.sort_by_key(|(item, _)| *item);
        let mut files = Vec::new_in(frame);
        files.push(FileItems { image: file_key(&self.world, unit), at: 0, len: items.len() as u32 });
        Ok(Keyed { manifest: manifest_key(&self.world, unit), surface: surface_key(&self.world, unit), files, items })
    }

    fn surface(&self, unit: u32, key: Key<Surface>, universe: Universe<'_>) -> Result<Aligned, TaskFailed> {
        self.note(format!("surface u{unit}"));
        let mut asked = BTreeMap::new();
        let mut order = Vec::new();
        for (at, name) in self.world.dep_callees(unit) {
            ask_of(&universe, &mut asked, &mut order, sig_fact(at, name))?;
        }
        let mut seen = vec![unit as u8];
        let mut facts: Vec<Fact> = self.world.units[unit as usize]
            .iter()
            .map(|item| {
                seen.push(item.sig);
                Fact { id: sig_fact(unit, item.name), digest: sig_digest(item.sig) }
            })
            .collect();
        facts.push(Fact { id: record::seen_id(), digest: record::digest(&seen) });
        facts.sort_by_key(|fact| fact.id);
        let (_, bytes) = record::write_read::<Surface>(key, &facts_of(&asked), &facts, b"surface");
        Ok(Aligned::copy_from(&bytes))
    }

    fn body(&self, unit: u32, id: StableId, key: Key<Body>, universe: Universe<'_>) -> Result<Aligned, TaskFailed> {
        let item = self.world.item(unit, id).ok_or(TaskFailed::Refused { what: "a body of no item of its unit" })?;
        if self.panics == Some((unit, item.name)) {
            panic!("the scripted front panics in the body of u{unit}::{}", item.name);
        }
        let count = self.note(format!("body u{unit}::{}", item.name));
        let mut asked = BTreeMap::new();
        let mut order = text_of(item).0.to_vec();
        for call in item.calls {
            if let Some((at, callee)) = self.world.resolve(unit, call) {
                ask_of(&universe, &mut asked, &mut order, sig_fact(at, callee.name))?;
            }
        }
        if unit == 1 && item.name == "d" {
            ask_of(&universe, &mut asked, &mut order, const_fact())?;
        }
        let made = match unit == 0 && item.name == "k" {
            true => vec![Fact { id: const_fact(), digest: self.world.const_value() }],
            false => Vec::new(),
        };
        if self.counts {
            order.extend_from_slice(&(count as u64).to_le_bytes());
        }
        let payload = record::digest(&order);
        let (_, bytes) = record::write_read::<Body>(key, &facts_of(&asked), &made, &payload.0);
        Ok(Aligned::copy_from(&bytes))
    }
}

// --- a store whose commit fails ---------------------------------------------------------------

struct Failing<'s> {
    inner: &'s PlainStore,
    fail: bool,
}

impl Store for Failing<'_> {
    type Held<A: Allocator> = <PlainStore as Store>::Held<A>;
    type Verified = <PlainStore as Store>::Verified;

    fn read<K: Kind, A: Allocator>(&self, key: &Key<K>, frame: A) -> Result<Option<Read<Self::Verified, Self::Held<A>>>, StoreError> {
        self.inner.read(key, frame)
    }

    fn map<K: Kind>(&self, key: &Key<K>) -> Result<Option<Read<Self::Verified, &[u8]>>, StoreError> {
        self.inner.map(key)
    }

    fn put<K: Kind>(&self, key: &Key<K>, namespace: Namespace, bytes: &[u8]) -> Result<Put, StoreError> {
        self.inner.put(key, namespace, bytes)
    }

    fn commit(&self) -> Result<Generation, StoreError> {
        match self.fail {
            true => Err(StoreError::Os { call: "fsync", what: "a segment", errno: 5 }),
            false => self.inner.commit(),
        }
    }

    fn compact(&self, roots: &[RootRef], cap: u64) -> Result<Compacted, StoreError> {
        self.inner.compact(roots, cap)
    }
}

// --- one request ------------------------------------------------------------------------------

/// A request's answer, out of its snapshot.
struct Asked {
    outcome: Outcome,
    gets: u32,
    certified: bool,
    manifests: Vec<Option<Key<UnitManifest>>>,
    /// (unit, item, kind, key) of every record proved.
    records: Vec<(u32, StableId, Tag, ContentId)>,
    store_error: Option<StoreError>,
}

impl Asked {
    fn built(&self) -> Ran {
        match &self.outcome {
            Outcome::Built { ran, filed: Filed::Certificate } => *ran,
            other => panic!("the request was to build and file its certificate, and ended {other:?}"),
        }
    }
}

struct Bench {
    dir: TempDir,
}

impl Bench {
    fn new(prefix: &str) -> Bench {
        let dir = TempDir::for_test(prefix);
        std::fs::create_dir_all(dir.path().join("src")).expect("the bench's source directory is made");
        Bench { dir }
    }

    fn store(&self) -> PlainStore {
        PlainStore::open(&self.dir.path().join("store"), &StoreSettings { form: Form::Plain, ..StoreSettings::default() })
            .expect("the plain store opens")
    }

    fn src(&self) -> std::path::PathBuf {
        self.dir.path().join("src")
    }
}

/// One request of `front`'s world against `store` under memo `memo`, its units' files written
/// first under `src`.
fn ask_with<S: Store, M: Memo>(
    src: &Path,
    store: &S,
    front: &Scripted,
    memo: &mut M,
    base: u8,
    last: &[Option<Key<UnitManifest>>],
) -> Result<Asked, HostError> {
    let texts: Vec<Vec<u8>> = (0..2).map(|unit| front.world.text(unit)).collect();
    for (unit, text) in texts.iter().enumerate() {
        std::fs::write(src.join(format!("u{unit}.rs")), text).expect("a unit's file is written");
    }
    let paths: Vec<Vec<u8>> = (0..2).map(|unit| format!("u{unit}.rs").into_bytes()).collect();
    let reads: Vec<Ask<'_>> = paths.iter().map(|path| Ask::File(path)).collect();
    let then: Vec<ContentId> = texts.iter().map(|text| file_answer(text)).collect();
    let units: Vec<UnitPlan<'_>> = (0..2u32)
        .map(|unit| UnitPlan {
            deps: DEPS[unit as usize],
            last: last.get(unit as usize).copied().flatten(),
            namespace: Namespace::LOCAL,
            owner: owner(unit),
        })
        .collect();
    let plan = Plan { certificate: certificate(base), namespace: Namespace::LOCAL, units: &units };
    let request = Request { plan, reads: &reads, then: &then, late: &[], leaves: Leaves::NONE, stall: Duration::from_secs(60) };
    let files = Disk::new(src);
    proof::scope(|snap| {
        let answered = run(snap, &request, store, front, memo, &files, MACHINE)?;
        let mut records = Vec::new();
        for (unit, proved) in answered.units.iter().enumerate() {
            for (item, holds) in &proved.records {
                records.push((unit as u32, *item, holds.tag(), holds.key_id()));
            }
        }
        Ok(Asked {
            outcome: answered.outcome,
            gets: answered.asks.gets,
            certified: answered.certified.is_some(),
            manifests: answered.manifests.iter().copied().collect(),
            records,
            store_error: answered.store_error,
        })
    })
}

fn ask<S: Store>(src: &Path, store: &S, front: &Scripted, base: u8, last: &[Option<Key<UnitManifest>>]) -> Result<Asked, HostError> {
    ask_with(src, store, front, &mut RecordsOnly, base, last)
}

/// The bytes the store holds for a record proved, by its kind's layout.
fn bytes_of(store: &PlainStore, tag: Tag, key: ContentId) -> Option<Vec<u8>> {
    if tag == Surface::TAG {
        return store.map::<Surface>(&Key::from_id(key)).expect("a surface is read").map(|read| read.bytes.to_vec());
    }
    if tag == Body::TAG {
        return store.read::<Body, _>(&Key::from_id(key), Global).expect("a body is read").map(|read| read.bytes.to_vec());
    }
    panic!("a record of kind {} proved in a unit", tag.0);
}

// --- the tests --------------------------------------------------------------------------------

/// The claim: a cold request runs every task once (2 keyings, 2 surfaces, 7 bodies) and files
/// both manifests, and every record proved is in the store when it is opened again. The same
/// request again, on the reopened store, runs no task and reads only the certificate (its
/// shape and its value: two gets). The control: the same request under another certificate
/// base finds no certificate, so it keys both units and settles each by its mark (no surface,
/// no body): the certificate is what the no-op stood on.
#[test]
fn cold_files_every_record_and_the_same_request_reads_only_the_certificate() {
    let bench = Bench::new("blaze-host-cold");
    let front = Scripted::new(world());
    let cold = {
        let store = bench.store();
        ask(&bench.src(), &store, &front, 5, &[]).expect("the cold request runs")
    };
    let ran = cold.built();
    assert_eq!((ran.keyed, ran.surfaces, ran.bodies, ran.found, ran.manifests), (2, 2, 7, 0, 2));
    assert_eq!(front.ran("key").len(), 2);
    assert_eq!(front.ran("surface").len(), 2);
    assert_eq!(front.ran("body").len(), 7, "each body once: unit 1 reads unit 0's made table only after it settles");

    let store = bench.store();
    assert_eq!(cold.records.len(), 9, "a surface and every body per unit");
    for &(unit, item, tag, key) in &cold.records {
        assert!(bytes_of(&store, tag, key).is_some(), "unit {unit}'s record of {item:?} is filed");
    }
    for manifest in &cold.manifests {
        let manifest = manifest.expect("each unit's manifest is filed");
        let read = store.read::<UnitManifest, _>(&manifest, Global).expect("a manifest is read");
        assert!(read.is_some(), "the manifest {manifest:?} is in the reopened store");
    }

    let again = Scripted::new(world());
    let noop = ask(&bench.src(), &store, &again, 5, &cold.manifests).expect("the same request runs");
    assert!(matches!(noop.outcome, Outcome::Certified { .. }), "the same request is certified: {:?}", noop.outcome);
    assert!(noop.certified);
    assert_eq!(again.calls(), 0, "nothing of the front end is asked");
    assert_eq!(noop.gets, 2, "the certificate's shape and value, nothing else");
    assert_eq!(noop.manifests, cold.manifests, "the certificate names the manifests the cold request filed");

    let control = Scripted::new(world());
    let other = ask(&bench.src(), &store, &control, 6, &cold.manifests).expect("the request under another base runs");
    let ran = other.built();
    assert!(!other.certified);
    assert_eq!((ran.keyed, ran.surfaces, ran.bodies, ran.marked), (2, 0, 0, 2), "keyed, and every unit settled by its mark");
}

/// The claim: an edit of one body's text runs that body's task and nothing else, files its
/// unit's manifest, and settles the unit above by its mark (one lookup): the body makes no fact,
/// so unit 0's universe digest is unmoved. The control: an edit of `k`'s body, which makes the
/// const value unit 1's `d` reads, misses unit 1's mark and redoes `d`, and only `d`: the made
/// fact moving is what reaches the unit above.
#[test]
fn a_body_edit_runs_one_task_and_marks_the_units_above() {
    let bench = Bench::new("blaze-host-body");
    let store = bench.store();
    let cold = ask(&bench.src(), &store, &Scripted::new(world()), 5, &[]).expect("the cold request runs");

    let edited = Scripted::new(world().body(0, "b"));
    let ran = ask(&bench.src(), &store, &edited, 5, &cold.manifests).expect("the edited request runs").built();
    assert_eq!((ran.surfaces, ran.bodies, ran.marked, ran.manifests), (0, 1, 1, 1));
    assert_eq!(edited.ran("body"), ["body u0::b"]);
    assert!(edited.ran("surface").is_empty());

    let control = Scripted::new(world().body(0, "k"));
    let ran = ask(&bench.src(), &store, &control, 5, &cold.manifests).expect("the control request runs").built();
    assert_eq!((ran.surfaces, ran.bodies, ran.marked), (0, 2, 0));
    assert_eq!(control.ran("body"), ["body u0::k", "body u1::d"]);
}

/// The claim: a signature edit of `b`, which no item of unit 1 calls, runs unit 0's surface and
/// `b`'s body, and no task of unit 1: unit 1's mark misses (unit 0's surface moved), its
/// dictionary's rows owned by unit 0 answer as they read, and its manifest stands (only a mark
/// filed). The control: a signature edit of `a`, which unit 1's `run` calls, runs unit 1's
/// surface and `run`.
#[test]
fn a_dependency_fact_nobody_read_runs_no_task_above_it() {
    let bench = Bench::new("blaze-host-unread");
    let store = bench.store();
    let cold = ask(&bench.src(), &store, &Scripted::new(world()), 5, &[]).expect("the cold request runs");

    let edited = Scripted::new(world().sig(0, "b"));
    let ran = ask(&bench.src(), &store, &edited, 5, &cold.manifests).expect("the edited request runs").built();
    assert_eq!(edited.ran("surface"), ["surface u0"]);
    assert_eq!(edited.ran("body"), ["body u0::b"]);
    assert_eq!((ran.stood, ran.marked, ran.manifests), (1, 0, 1), "unit 1 files its mark alone");

    let control = Scripted::new(world().sig(0, "a"));
    ask(&bench.src(), &store, &control, 5, &cold.manifests).expect("the control request runs").built();
    assert_eq!(control.ran("surface"), ["surface u0", "surface u1"]);
    assert_eq!(control.ran("body"), ["body u0::a", "body u1::run"]);
}

/// The claim: a signature edit of `new` redoes exactly the records the postings of its fact
/// name: `a` and `c` (they call it), unit 1's surface (it asks it), and `new` itself and unit
/// 0's surface (their keys moved); `b`, `k`, `run` and `d` stand. The control: an edit of `a`'s
/// signature instead redoes `run` (which calls `a`) and not `c`: what was read, not the key's
/// neighbourhood, decides.
#[test]
fn a_signature_edit_redoes_exactly_the_readers_its_postings_name() {
    let bench = Bench::new("blaze-host-sig");
    let store = bench.store();
    let cold = ask(&bench.src(), &store, &Scripted::new(world()), 5, &[]).expect("the cold request runs");

    let edited = Scripted::new(world().sig(0, "new"));
    let ran = ask(&bench.src(), &store, &edited, 5, &cold.manifests).expect("the edited request runs").built();
    assert_eq!((ran.surfaces, ran.bodies, ran.manifests), (2, 3, 2));
    assert_eq!(edited.ran("body"), ["body u0::a", "body u0::new", "body u1::c"]);

    let bench = Bench::new("blaze-host-sig-control");
    let store = bench.store();
    let cold = ask(&bench.src(), &store, &Scripted::new(world()), 5, &[]).expect("the cold request runs");
    let control = Scripted::new(world().sig(0, "a"));
    let ran = ask(&bench.src(), &store, &control, 5, &cold.manifests).expect("the control request runs").built();
    assert_eq!((ran.surfaces, ran.bodies), (2, 2));
    assert_eq!(control.ran("body"), ["body u0::a", "body u1::run"]);
}

/// The claim: an edit undone finds its records by address. After two body edits, undoing one
/// (a state never built: no certificate, no mark) runs no task: the undone body's key is its
/// old one and its record is at the address its old reads make. Undoing both finds the first
/// certificate: certified, nothing run. The control: a state whose body was never built runs
/// that body.
#[test]
fn an_undo_finds_its_records_by_address() {
    let bench = Bench::new("blaze-host-undo");
    let store = bench.store();
    let cold = ask(&bench.src(), &store, &Scripted::new(world()), 5, &[]).expect("the cold request runs");
    let both = ask(&bench.src(), &store, &Scripted::new(world().body(0, "b").body(0, "new")), 5, &cold.manifests)
        .expect("the edit runs");
    assert_eq!(both.built().bodies, 2);

    let one = Scripted::new(world().body(0, "new"));
    let asked = ask(&bench.src(), &store, &one, 5, &both.manifests).expect("the undo of one edit runs");
    let ran = asked.built();
    assert!(one.ran("body").is_empty(), "the undone body is found, not run: {:?}", one.ran("body"));
    assert!(one.ran("surface").is_empty());
    assert_eq!(ran.found, 1, "`b`'s record, at its old address");

    let undone = Scripted::new(world());
    let asked = ask(&bench.src(), &store, &undone, 5, &both.manifests).expect("the undo runs");
    assert!(matches!(asked.outcome, Outcome::Certified { .. }), "the undo is certified: {:?}", asked.outcome);
    assert_eq!(undone.calls(), 0);
    assert_eq!(asked.manifests, cold.manifests);

    let novel = Scripted::new(world().body(1, "d"));
    let asked = ask(&bench.src(), &store, &novel, 5, &both.manifests).expect("the novel state runs");
    assert!(!asked.certified, "a state never built is not certified");
    assert_eq!(novel.ran("body"), ["body u1::d"]);
}

/// The claim: a machine whose store holds another machine's records (as the shared cache gives
/// them) and no certificate of its own, and which knows no last manifest, settles every unit
/// through its mark: two key tasks, two mark lookups, nothing else run, and the manifests the
/// marks name are the ones the build filed. The control: the same request on a store that
/// never built runs everything.
#[test]
fn a_store_with_no_certificate_settles_every_unit_through_its_mark() {
    let bench = Bench::new("blaze-host-marks");
    let store = bench.store();
    let cold = ask(&bench.src(), &store, &Scripted::new(world()), 5, &[]).expect("the cold request runs");

    let other = Scripted::new(world());
    let asked = ask(&bench.src(), &store, &other, 6, &[]).expect("the other machine's request runs");
    let ran = asked.built();
    assert_eq!((ran.keyed, ran.marked, ran.surfaces, ran.bodies), (2, 2, 0, 0));
    assert_eq!(other.ran("key").len(), other.calls(), "only key tasks");
    assert_eq!(asked.manifests, cold.manifests);

    let fresh = Bench::new("blaze-host-marks-control");
    let store = fresh.store();
    let control = Scripted::new(world());
    let ran = ask(&fresh.src(), &store, &control, 6, &[]).expect("the control runs").built();
    assert_eq!((ran.marked, ran.surfaces, ran.bodies), (0, 2, 7));
}

/// The claim: under [`Audit`] every hit is redone and compared, and a deterministic front end
/// gives no difference over each scenario above (a cold build, a no-op, a body edit, an unread
/// fact, a signature edit, a made fact, an undo, another machine). The control: a front end
/// whose bodies write how many tasks ran before them is caught at the first hit the audit redoes,
/// by name ([`Failure::Audit`]), where [`RecordsOnly`] trusts the certificate and runs nothing.
#[test]
fn an_audit_redoes_every_hit_and_finds_no_difference() {
    let bench = Bench::new("blaze-host-audit");
    let store = bench.store();
    let states = [
        (world(), 5, "a cold build"),
        (world(), 5, "a no-op"),
        (world().body(0, "b"), 5, "a body edit"),
        (world().body(0, "b").sig(0, "b"), 5, "an unread fact"),
        (world().body(0, "b").sig(0, "b").sig(0, "new"), 5, "a signature edit"),
        (world().body(0, "b").sig(0, "b").sig(0, "new").body(0, "k"), 5, "a made fact"),
        (world().sig(0, "new"), 5, "an undo of three edits"),
        (world(), 6, "another machine"),
    ];
    let mut last: Vec<Option<Key<UnitManifest>>> = Vec::new();
    for (state, base, what) in states {
        let front = Scripted::new(state);
        let last_of = if base == 6 { Vec::new() } else { last.clone() };
        let asked = ask_with(&bench.src(), &store, &front, &mut Audit, base, &last_of).expect("the audited request runs");
        match &asked.outcome {
            Outcome::Built { .. } => {}
            other => panic!("{what}: an audited request ended {other:?}"),
        }
        if what == "a no-op" {
            assert!(
                matches!(asked.outcome, Outcome::Built { filed: Filed::Audited, .. }),
                "a no-op under audit builds again to the certificate's value: {:?}",
                asked.outcome
            );
        }
        last = asked.manifests;
    }

    let bench = Bench::new("blaze-host-audit-control");
    let store = bench.store();
    let mut counting = Scripted::new(world());
    counting.counts = true;
    let cold = ask(&bench.src(), &store, &counting, 5, &[]).expect("the counting cold request runs");
    cold.built();
    let mut counting = Scripted::new(world());
    counting.counts = true;
    let asked = ask_with(&bench.src(), &store, &counting, &mut Audit, 5, &cold.manifests).expect("the audited request runs");
    assert!(
        matches!(asked.outcome, Outcome::Failed { why: Failure::Audit { .. }, .. }),
        "a front end that is not deterministic is caught by the audit: {:?}",
        asked.outcome
    );
    let mut counting = Scripted::new(world());
    counting.counts = true;
    let trusted = ask(&bench.src(), &store, &counting, 5, &cold.manifests).expect("the unaudited request runs");
    assert!(matches!(trusted.outcome, Outcome::Certified { .. }), "records only trusts the hit: {:?}", trusted.outcome);
}

/// The claim: a task that panics fails the request by name (its kind, unit and the panic),
/// and leaves the store usable: the next request on it builds. The control: the same cold
/// request with no panic builds.
#[test]
fn a_panicking_task_fails_by_name_and_leaves_the_store_usable() {
    let bench = Bench::new("blaze-host-panic");
    let store = bench.store();
    let mut panicking = Scripted::new(world());
    panicking.panics = Some((0, "b"));
    let asked = ask(&bench.src(), &store, &panicking, 5, &[]).expect("the request runs to an outcome");
    match &asked.outcome {
        Outcome::Failed { why: Failure::Task { tag, unit, failed }, .. } => {
            assert_eq!((*tag, *unit, *failed), (Body::TAG, 0, TaskFailed::Panicked { what: "a body task" }));
        }
        other => panic!("a panicking body fails the request by name, and it ended {other:?}"),
    }
    assert!(!asked.certified);

    let after = ask(&bench.src(), &store, &Scripted::new(world()), 5, &[]).expect("the next request runs");
    after.built();

    let control = Bench::new("blaze-host-panic-control");
    let store = control.store();
    ask(&control.src(), &store, &Scripted::new(world()), 5, &[]).expect("the control runs").built();
}

/// The claim: a failed sync of the commit is reported: the request fails by the store, and the
/// store's own error is given. The control: the same store with a commit that succeeds builds.
#[test]
fn a_failed_store_sync_is_reported() {
    let bench = Bench::new("blaze-host-sync");
    let plain = bench.store();
    let failing = Failing { inner: &plain, fail: true };
    let asked = ask(&bench.src(), &failing, &Scripted::new(world()), 5, &[]).expect("the request runs to an outcome");
    match &asked.outcome {
        Outcome::Failed { why: Failure::Store(fault), .. } => {
            assert_eq!(*fault, StoreFault { what: "the commit of the request's records" });
        }
        other => panic!("a failed sync fails the request by the store, and it ended {other:?}"),
    }
    assert_eq!(asked.store_error, Some(StoreError::Os { call: "fsync", what: "a segment", errno: 5 }));

    let control = Bench::new("blaze-host-sync-control");
    let plain = control.store();
    let working = Failing { inner: &plain, fail: false };
    let asked = ask(&control.src(), &working, &Scripted::new(world()), 5, &[]).expect("the control runs");
    asked.built();
    assert_eq!(asked.store_error, None);
}

#[derive(Clone, Copy, Debug)]
enum Edit {
    Sig(u32, usize),
    Body(u32, usize),
    None,
}

fn edit(tape: &Tape) -> Edit {
    let unit = u32::from(tape.int_in(0..=1u8, "unit"));
    let at = usize::from(tape.int_in(0..=2u8, "at"));
    match tape.int_in(0..=2u8, "edit") {
        0 => Edit::Sig(unit, at),
        1 => Edit::Body(unit, at),
        _ => Edit::None,
    }
}

/// The invariant: for any sequence of edits, each request on the kept store builds or is
/// certified, a request with no edit since the last is certified, the manifests it ends with
/// are a fresh store's for the same text, and every record it proves is, at the same address
/// and byte for byte, the record a fresh build proves for its item (a record found by address
/// is only what redoing it makes). Its planted-bug controls are the scenario tests' (each
/// removes the cause and watches the effect go) and the audit's.
#[test]
fn any_edits_file_what_a_fresh_build_files() {
    blaze_sim::check(Config::cases(40), any_edits);
}

/// One case of `any_edits_file_what_a_fresh_build_files`.
fn any_edits(tape: &Tape) {
    let edits: Vec<Edit> = tape.vec(0..=6, "edits", edit);
    let bench = Bench::new("blaze-host-edits");
    let store = bench.store();
    let mut state = world();
    let mut last: Vec<Option<Key<UnitManifest>>> = Vec::new();
    for (step, edit) in std::iter::once(Edit::Body(0, 0)).chain(edits).enumerate() {
        let before = state.clone();
        state = match edit {
            Edit::Sig(unit, at) => {
                let name = state.units[unit as usize][at].name;
                state.sig(unit, name)
            }
            Edit::Body(unit, at) => {
                let name = state.units[unit as usize][at].name;
                state.body(unit, name)
            }
            Edit::None => state,
        };
        let asked = ask(&bench.src(), &store, &Scripted::new(state.clone()), 5, &last).expect("the request runs");
        match &asked.outcome {
            Outcome::Built { filed: Filed::Certificate, .. } => {
                assert!(step == 0 || before != state, "step {step}: a request with no edit built")
            }
            Outcome::Certified { .. } => {}
            other => panic!("step {step}: the request ended {other:?}"),
        }
        let fresh = Bench::new("blaze-host-edits-fresh");
        let fresh_store = fresh.store();
        let built = ask(&fresh.src(), &fresh_store, &Scripted::new(state.clone()), 5, &[]).expect("the fresh request runs");
        built.built();
        assert_eq!(asked.manifests, built.manifests, "step {step}: the manifests are the fresh build's");
        let by_item: BTreeMap<(u32, StableId), (Tag, ContentId)> =
            built.records.iter().map(|&(unit, item, tag, key)| ((unit, item), (tag, key))).collect();
        for &(unit, item, tag, key) in &asked.records {
            assert_eq!(by_item.get(&(unit, item)), Some(&(tag, key)), "step {step}: unit {unit}'s record of {item:?} is at the fresh one's address");
            assert_eq!(
                bytes_of(&store, tag, key),
                bytes_of(&fresh_store, tag, key),
                "step {step}: unit {unit}'s record of {item:?} is what a fresh build files"
            );
        }
        last = asked.manifests;
    }
}
