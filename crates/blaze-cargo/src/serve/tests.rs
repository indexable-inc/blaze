//! The daemon on the simulation driver, with a build of fakes: a load that needs two sources
//! before it plans, a front end that compiles every unit at once, build scripts that end as
//! they start. Tasks run inline in a seeded order, so a run is a function of its seed.

#![forbid(unsafe_code)]

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    sync::Arc,
    time::Duration,
};

use blaze_api::{
    ApiError, Artifact, CrateType, Diagnostic, Envelope, FetchVerb, Level, Reach, Request, Response, SCHEMA, Table,
    TargetKind, Targets, TestOutcome, TestResult, Ub, UnitOf, UnitTarget, Verb, ViewRef,
    frames::{self, Frames},
};
use blaze_cas::Store;
use blaze_core::{
    Address, Completion, Conn, Driver, Effect, FileName, Tag,
    sim::{Faults, Rng, Sim},
};
use blaze_ids::{ContentId, Hasher};
use blaze_query::{Cancelled, Cx, Database, DbError, Query, QueryKey, QueryKind};
use blaze_units::{
    query::{
        BuildConfig, Cbor, Command, Fetch, FetchKind, Invocation, LoadError, LockWrite, Origin, PackagePlan, Planned, TestSandbox,
        UnitPlan, Units,
    },
    schema::{Mode, Profile, Strip, Stripped, Target, Unit, UnitDep, UnitGraph},
};
use blaze_vfs::{Change, Op, ViewId};
use parking_lot::Mutex;

use super::{
    Daemon, Options,
    job::{DURABLE_WITHIN, RECHECK},
    plane::{Inline, Runner},
    work::{
        Broke, Compiled, Failed, Front, FrontEnd, Goal, Image, JobInput, Kept, KeptDb, KeptPut, Library, Output, Phase, Put,
        RunPuts, Runs, Selection, Source, SourceFile, Steps, Writer, applying,
    },
};

const SOCKET: &str = "run/blaze/v2.sock";
const STATE: &str = "cache/blaze/v2";
/// [`Options::std`]: the machine's, outside every state directory.
const STD: &str = "cache/blaze/std/0b";
const FINISHED: &str = "`dev` profile [unoptimized + debuginfo]";
const LIBRARY: &str = "/rustc/0123abcd/library";
const DEP: &str = "/home/u/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/dep-1.0.0";
const INDEX: &str = "/home/u/.cargo/registry/index/index.crates.io-1949cf8c6b5b557f/.cache/de/p/dep";
/// How many turns a test gives the daemon before it says it hangs.
const TURNS: usize = 100_000;

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

thread_local! {
    /// How many times the load ran on this thread (tasks run inline, on the test's).
    static PLANS: Cell<usize> = const { Cell::new(0) };
    /// The configuration of each run of the load.
    static SAW: RefCell<Vec<BuildConfig>> = const { RefCell::new(Vec::new()) };
    /// How many build scripts' runs were asked of a writer.
    static ASKED: Cell<usize> = const { Cell::new(0) };
    /// Whether a build script's run is [`Lost`].
    static LOSES_RUNS: Cell<bool> = const { Cell::new(false) };
    /// How many times a writer applied what was asked of it.
    static APPLIES: Cell<usize> = const { Cell::new(0) };
    /// The build script's run whose write panics as it is applied, as a maintainer's would.
    static PANICS_RUN: Cell<Option<usize>> = const { Cell::new(None) };
    /// The unit whose compile panics, as a bug of blaze's would.
    static PANICS_COMPILE: Cell<Option<usize>> = const { Cell::new(None) };
    /// The unit whose compile ends in an error that is no refusal, as an `Unsupported` codegen
    /// error of the 2026-10-31 `blaze test --workspace` did.
    static ERRS_COMPILE: Cell<Option<usize>> = const { Cell::new(None) };
    /// Each compile's unit, and the runs applied in the front end it read.
    static SEEN: RefCell<Vec<(usize, Vec<usize>)>> = const { RefCell::new(Vec::new()) };
    /// Each phase of a unit's compile as it ran, in order.
    static PHASES: RefCell<Vec<(usize, Phase)>> = const { RefCell::new(Vec::new()) };
    /// Whether the first unit's compile keeps its reader in [`HOLD`]: a reader still alive,
    /// as one of a task that is still running.
    static KEEP: Cell<bool> = const { Cell::new(false) };
    static HOLD: RefCell<Vec<FakeReader>> = const { RefCell::new(Vec::new()) };
    /// How many times a job asked the plan ([`Steps::plan`]), from the database or not.
    static ASKS: Cell<usize> = const { Cell::new(0) };
    /// How many lowerings were given the head's last front end, and how many lowered anew.
    static INTO_KEPT: Cell<usize> = const { Cell::new(0) };
    static ANEW: Cell<usize> = const { Cell::new(0) };
    /// How many kept front ends were stored ([`Steps::store_kept`]).
    static STORES: Cell<usize> = const { Cell::new(0) };
    /// The checkout whose kept front end leaves nothing for the store: a job that puts
    /// nothing new, and may still have read what another job put.
    static PUTS_NOTHING: RefCell<String> = const { RefCell::new(String::new()) };
    /// The tests' run keeps how it ran in the store ([`Keeps`]).
    static TESTS_KEEP: Cell<bool> = const { Cell::new(false) };
    /// ... and hands the put's ticket to no one: a put no caller told the loop of.
    static TESTS_KEEP_UNTOLD: Cell<bool> = const { Cell::new(false) };
    /// ... through this writer of the store's, a segment other than the loop's, when one is
    /// set: an off-loop put no one told the loop of.
    static UNTOLD_WRITER: RefCell<Option<blaze_cas::Writer>> = const { RefCell::new(None) };
    /// ... and runs this long before it keeps anything: a run of user code still going.
    static KEEPS_FOR: Cell<Duration> = const { Cell::new(Duration::ZERO) };
    /// How many of its runs started.
    static KEEPS_STARTED: Cell<usize> = const { Cell::new(0) };
    /// How many lowerings anew found in the store all a build of their checkout left for it
    /// ([`kept_key`]): what a restarted daemon's first build reads instead of working it out.
    static KEPT_READS: Cell<usize> = const { Cell::new(0) };
    /// How many times the standard library's files were read.
    static LIBRARY_READS: Cell<usize> = const { Cell::new(0) };
    /// Whether the program's compile gives [`APP`] as its output at [`APP_AT`].
    static OUTPUT: Cell<bool> = const { Cell::new(false) };
    /// Whether the program's compile warns twice, as its unit.
    static WARNS: Cell<bool> = const { Cell::new(false) };
    /// Whether the first unit's compile shares [`SHARED`] in the standard library's shared
    /// store, as a std unit made here does.
    static SHARES: Cell<bool> = const { Cell::new(false) };
}

const SHARED: &[u8] = b"a std unit's record\n";

fn shared_key() -> blaze_cas::Key {
    blaze_cas::Key(ContentId::of("blaze serve tests shared", SHARED))
}

/// One of the values a kept front end of the fakes on `checkout` leaves for the store, as a
/// real one's dependency images and check outcomes are: `at` 0 and 1, its own.
fn kept_key(checkout: &str, at: u8) -> blaze_cas::Key {
    blaze_cas::Key(ContentId::of("blaze serve tests kept", format!("{checkout} {at}").as_bytes()))
}

fn kept_value(checkout: &str, at: u8) -> Vec<u8> {
    format!("what the build of {checkout} left for the store, part {at}\n").into_bytes()
}

/// What every checkout's kept front end leaves under one key, as the image of a dependency two
/// checkouts lock at one version is: the second checkout's put of it is handed the ticket of
/// the first's, from wherever that was put.
const SHARED_KEPT: &[u8] = b"what every checkout's build left for the store\n";

fn shared_kept_key() -> blaze_cas::Key {
    blaze_cas::Key(ContentId::of("blaze serve tests kept shared", SHARED_KEPT))
}

/// Whether `store` holds all a build of `checkout` left for it.
fn holds_kept(store: &Store, checkout: &str) -> bool {
    let shared = store.get(&shared_kept_key()).unwrap().is_some_and(|it| it.bytes == SHARED_KEPT);
    shared && (0..2).all(|at| store.get(&kept_key(checkout, at)).unwrap().is_some_and(|it| it.bytes == kept_value(checkout, at)))
}

const APP: &[u8] = b"\x7fELF the program\n";
const APP_AT: &str = "/w/target/debug/app";

/// A warning of the program's unit, as its compile gives it.
fn warning_of_app(message: &str) -> Diagnostic {
    let target = UnitTarget {
        doc: true,
        kind: TargetKind::Bin,
        name: "app".into(),
        test: true,
        doctest: false,
        edition: "2021".into(),
        src_path: "/w/src/main.rs".into(),
        crate_types: vec![CrateType::Bin],
    };
    let unit = UnitOf { id: 0, test: false, target, package_id: "app".into(), manifest_path: "/w/Cargo.toml".into() };
    Diagnostic {
        level: Level::Warning,
        code: None,
        message: message.to_owned(),
        spans: Vec::new(),
        children: Vec::new(),
        facts: Vec::new(),
        rendered: format!("warning: {message}\n\n"),
        unit: Some(unit),
        explanation: None,
    }
}

fn needed() -> Vec<Fetch> {
    vec![
        Fetch {
            what: "crates.io index".into(),
            url: "https://index.crates.io/de/p/dep".into(),
            kind: FetchKind::IndexFile,
            at: INDEX.into(),
        },
        Fetch {
            what: "dep v1.0.0".into(),
            url: "https://static.crates.io/crates/dep/1.0.0/download".into(),
            kind: FetchKind::Crate { sha256: "ab".repeat(32) },
            at: DEP.into(),
        },
    ]
}

/// The load: cargo's error for a manifest that starts with `fail`, else the sources it lacks,
/// else the units of the checkout's `app` on `dep`, with the checkout's manifest as a warning
/// so a test sees what it read.
struct FakePlan;

impl Query for FakePlan {
    const KIND: QueryKind = QueryKind(0x67f0);
    const NAME: &'static str = "fake plan";
    const PRODUCER: ContentId = ContentId([7; 32]);
    type Key = ();
    type Value = Cbor<Planned>;

    fn input(_: &()) -> ContentId {
        ContentId::of("blaze serve tests fake plan", b"")
    }

    fn execute(cx: &mut Cx<'_>, _: &()) -> Result<Cbor<Planned>, Cancelled> {
        PLANS.with(|it| it.set(it.get() + 1));
        let config = BuildConfig::decode(cx.config()?).expect("a view of the daemon has a build config");
        SAW.with(|it| it.borrow_mut().push(config.clone()));
        let manifest = cx.file("Cargo.toml")?.map(|it| String::from_utf8_lossy(it.bytes).into_owned()).unwrap_or_default();
        if manifest.starts_with("fail") {
            let at = Some(format!("{}/Cargo.toml", config.invocation.root));
            return Ok(Cbor(Planned::Failed(LoadError { what: "error: the fake load refuses this manifest".into(), at })));
        }
        let missing: Vec<Fetch> =
            needed().into_iter().filter(|fetch| !config.mounts.iter().any(|it| it.at == fetch.at)).collect();
        if !missing.is_empty() {
            return Ok(Cbor(Planned::Needs(missing)));
        }
        let wave = manifest.strip_prefix("wave ").and_then(|it| it.trim().parse().ok());
        Ok(Cbor(Planned::Ready(match wave {
            Some(scripts) => wave_units(&config.invocation.root, manifest, scripts),
            None => units(&config.invocation.root, manifest),
        })))
    }
}

fn profile() -> Profile {
    Profile { strip: Strip::Resolved(Stripped::None), ..Profile::dev() }
}

pub(super) fn unit(pkg_id: &str, kind: &str, mode: Mode, deps: &[usize]) -> Unit {
    Unit {
        pkg_id: pkg_id.into(),
        target: Target {
            kind: vec![kind.into()],
            crate_types: vec!["bin".into()],
            name: kind.into(),
            src_path: "src/main.rs".into(),
            edition: "2024".into(),
            required_features: None,
            doc: false,
            doctest: false,
            test: false,
        },
        profile: profile(),
        platform: None,
        macro_wasm: false,
        mode,
        checked_as_test: false,
        features: Vec::new(),
        is_std: false,
        dependencies: deps
            .iter()
            .map(|&index| UnitDep {
                index,
                extern_crate_name: "dep".into(),
                public: Some(false),
                noprelude: Some(false),
                nounused: Some(false),
                dev: false,
            })
            .collect(),
    }
}

fn units(root: &str, manifest: String) -> Units {
    let app = format!("path+file://{root}#app@0.1.0");
    let dep = "registry+https://github.com/rust-lang/crates.io-index#dep@1.0.0".to_owned();
    let graph = UnitGraph {
        version: 1,
        units: vec![
            unit(&dep, "lib", Mode::Build, &[]),
            unit(&app, "custom-build", Mode::Build, &[]),
            unit(&app, "custom-build", Mode::RunCustomBuild, &[1]),
            unit(&app, "bin", Mode::Build, &[0, 2]),
        ],
        roots: vec![3],
    };
    of_graph(root, manifest, graph)
}

/// A wave of `scripts` build scripts' runs (units `1..=scripts`), each on `dep`'s library
/// (unit 0), all ready at once, and the program on all of them.
fn wave_units(root: &str, manifest: String, scripts: usize) -> Units {
    let app = format!("path+file://{root}#app@0.1.0");
    let dep = "registry+https://github.com/rust-lang/crates.io-index#dep@1.0.0".to_owned();
    let mut units = vec![unit(&dep, "lib", Mode::Build, &[])];
    units.extend((0..scripts).map(|_| unit(&app, "custom-build", Mode::RunCustomBuild, &[0])));
    let all: Vec<usize> = (0..=scripts).collect();
    units.push(unit(&app, "bin", Mode::Build, &all));
    of_graph(root, manifest, UnitGraph { version: 1, units, roots: vec![scripts + 1] })
}

pub(super) fn of_graph(root: &str, manifest: String, graph: UnitGraph) -> Units {
    let app = format!("path+file://{root}#app@0.1.0");
    let dep = "registry+https://github.com/rust-lang/crates.io-index#dep@1.0.0".to_owned();
    let plan = UnitPlan {
        checked_as_test: false,
        metadata: "0".into(),
        extra_filename: String::new(),
        unit_id: "0123456789abcdef".to_owned(),
        cfgs: Vec::new(),
        check_cfg: Vec::new(),
        rustflags: Vec::new(),
        lints: Vec::new(),
        env: Vec::new(),
        outputs: Vec::new(),
        out_dir: None,
        links: None,
        unsandboxed: false,
        runner: Vec::new(),
        harness: true,
        clippy: None,
        macro_wasm: false,
    };
    let package = |id: &str, name: &str, version: &str, source: Option<String>, is_member| PackagePlan {
        id: id.into(),
        name: name.into(),
        version: version.into(),
        manifest_path: format!("{root}/Cargo.toml"),
        source,
        is_member,
        default_run: None,
        checksum: None,
        tests: TestSandbox::default(),
        const_macros: None,
    };
    Units {
        plans: vec![plan; graph.units.len()],
        graph,
        packages: vec![
            package(&dep, "dep", "1.0.0", Some("registry `crates-io`".into()), false),
            package(&app, "app", "0.1.0", Some(format!("({root})")), true),
        ],
        lock: Some(LockWrite { path: format!("{root}/Cargo.lock"), text: "# a lock\n".into() }),
        warnings: vec![manifest],
        target_dir: format!("{root}/target"),
        build_dir: format!("{root}/target"),
        finished: FINISHED.into(),
        link_dirs: Vec::new(),
        sandboxed_tests: false,
    }
}

/// The build of fakes; `fetched` counts each fetch by its `at`.
#[derive(Default)]
struct Fake {
    fetched: Mutex<BTreeMap<String, usize>>,
}

impl Steps for Fake {
    type Front = FakeFront;
    type Writer = FakeWriter;
    type Reader = FakeReader;
    type KeptFront = FakeInputs;
    type Linked = NoImage;
    type Runs = FakeRuns;

    fn fetch(&self, fetch: &Fetch) -> Result<Source, ApiError> {
        *self.fetched.lock().entry(fetch.at.clone()).or_default() += 1;
        let source = match fetch.kind {
            FetchKind::IndexFile => Source {
                files: vec![SourceFile { path: "dep".into(), bytes: b"{}\n".to_vec(), mode: blaze_vfs::Mode::Regular }],
                under: "dep".into(),
            },
            _ => Source {
                files: vec![SourceFile {
                    path: "Cargo.toml".into(),
                    bytes: format!("# {}\n", fetch.what).into_bytes(),
                    mode: blaze_vfs::Mode::Regular,
                }],
                under: String::new(),
            },
        };
        Ok(source)
    }

    fn library(&self, kept: &[(String, String)]) -> Result<Library, ApiError> {
        let fingerprint = "fake library".to_owned();
        if kept.iter().any(|(it, at)| *it == fingerprint && at == LIBRARY) {
            return Ok(Library { at: LIBRARY.into(), fingerprint, source: None });
        }
        LIBRARY_READS.set(LIBRARY_READS.get() + 1);
        let file = SourceFile { path: "core/src/lib.rs".into(), bytes: b"#![no_core]\n".to_vec(), mode: blaze_vfs::Mode::Regular };
        Ok(Library { at: LIBRARY.into(), fingerprint, source: Some(Source { files: vec![file], under: String::new() }) })
    }

    /// The fakes' build dirs are paths in the simulation (`/w/target`), never on disk: no lease.
    fn lease(&self, _: &Units, _: &str, _: &str) -> super::work::Leased {
        Ok(Ok(Vec::new())) // alloc: none, an empty vec
    }

    fn store_kept(&self, kept: &mut FakeInputs) -> Result<Vec<KeptPut>, ApiError> {
        if kept.stored {
            return Ok(Vec::new());
        }
        kept.stored = true;
        STORES.set(STORES.get() + 1);
        if PUTS_NOTHING.with_borrow(|it| *it == kept.checkout) {
            return Ok(Vec::new());
        }
        let put = |key, bytes, what: &str| KeptPut { put: Put { key, kind: blaze_cas::Kind::Query, bytes }, what: Arc::from(what) };
        let checkout = &kept.checkout;
        // The shared value last: a job that waited on its last put's ticket alone answers before
        // its own two are durable once another checkout put the shared one first (review C1).
        Ok(vec![
            put(kept_key(checkout, 0), kept_value(checkout, 0), "the fakes' first value of a checkout"),
            put(kept_key(checkout, 1), kept_value(checkout, 1), "the fakes' second value of a checkout"),
            put(shared_kept_key(), SHARED_KEPT.to_vec(), "the fakes' value of every checkout"),
        ])
    }

    fn lower(&self, job: &JobInput, kept: Option<FakeInputs>) -> Result<FrontEnd<Fake>, Failed> {
        if kept.as_ref().is_some_and(|it| !it.stored) {
            return Err(Failed::Error(ApiError::new("the fakes' kept front end is lowered into unstored", None)));
        }
        let inputs = match kept {
            Some(inputs) => {
                INTO_KEPT.set(INTO_KEPT.get() + 1);
                inputs
            }
            None => {
                ANEW.set(ANEW.get() + 1);
                let checkout = job.config.invocation.root.clone();
                // A real lowering anew serves its dependencies from what the last build left
                // in the store: the fakes count whether it is all there.
                let guard = blaze_cas::enter();
                let read = |at| job.store.get(&guard, &kept_key(&checkout, at)).ok().flatten().is_some_and(|it| it.bytes == kept_value(&checkout, at));
                if read(0) && read(1) {
                    KEPT_READS.set(KEPT_READS.get() + 1);
                }
                FakeInputs { checkout, ..FakeInputs::default() }
            }
        };
        let writer = FakeWriter { inputs: Arc::new(inputs), asked: Vec::new(), broke: Vec::new() };
        Ok(FrontEnd { front: Arc::new(FakeFront { goal: job.goal }), writer, puts: Vec::new() })
    }

    fn register(&self, db: &mut Database) {
        db.register::<FakePlan>();
    }

    fn plan(&self, db: &Database, view: ViewId) -> Result<Planned, Failed> {
        ASKS.set(ASKS.get() + 1);
        match db.pin().get::<FakePlan>(view, &()) {
            Ok(planned) => Ok(planned.get()),
            Err(DbError::Cancelled) => Err(Failed::Again),
            Err(error) => Err(Failed::Error(ApiError::new(error.to_string(), None))),
        }
    }

    fn plan_key(&self) -> QueryKey {
        QueryKey { kind: FakePlan::KIND, input: FakePlan::input(&()) }
    }
}

struct FakeFront {
    goal: Goal,
}

/// A front end's inputs: the build scripts' runs applied to it, in the order applied, whether
/// it was stored since it was last kept, and the checkout it was lowered for.
#[derive(Default)]
struct FakeInputs {
    runs: Vec<usize>,
    stored: bool,
    checkout: String,
}

/// The fakes' front end has no database: their images store no library trees.
impl KeptDb for FakeInputs {
    fn db(&self) -> Option<&base_db::Db> {
        None
    }
}

/// The front end written as the real one is: each write asked for waits until no reader of
/// the inputs is alive.
struct FakeWriter {
    inputs: Arc<FakeInputs>,
    asked: Vec<usize>,
    broke: Vec<Broke>,
}

/// A shared reference to the inputs, as the loop posted a task with it.
struct FakeReader(Arc<FakeInputs>);

/// The fakes link nothing: no image is ever kept.
enum NoImage {}

impl Kept for NoImage {
    fn tables(&self) -> Vec<(&'static str, u64, u64)> {
        match *self {}
    }
}

impl Writer<Fake> for FakeWriter {
    fn reader(&self) -> Option<FakeReader> {
        (!self.waiting()).then(|| FakeReader(Arc::clone(&self.inputs)))
    }

    fn ran(&mut self, run: usize, _: blaze_lower::Ran) -> Result<(), ApiError> {
        ASKED.with(|it| it.set(it.get() + 1));
        self.asked.push(run);
        Ok(())
    }

    fn macros(&mut self, unit: usize, _: blaze_lower::ProcMacroLoadResult) -> Result<(), ApiError> {
        Err(ApiError::new(format!("the fakes build no proc macro, and unit {unit} is not one"), None))
    }

    fn waiting(&self) -> bool {
        !self.asked.is_empty()
    }

    fn served(&self) -> Result<Vec<blaze_api::ServedCrate>, ApiError> {
        Ok(Vec::new()) // the fakes read no dependency from the store
    }

    fn goals_noted(&self) -> Option<blaze_api::Diagnostic> {
        None // the fakes offer no goal
    }

    fn apply(&mut self) -> Result<bool, ApiError> {
        if self.asked.is_empty() {
            return Ok(true);
        }
        let Some(inputs) = Arc::get_mut(&mut self.inputs) else { return Ok(false) };
        for run in self.asked.drain(..) {
            let applied = applying(run, "a build script's run", || {
                assert_ne!(PANICS_RUN.get(), Some(run), "serve-test: the run's write panics");
                inputs.runs.push(run);
            });
            if let Err(broke) = applied {
                self.broke.push(broke);
            }
        }
        APPLIES.with(|it| it.set(it.get() + 1));
        Ok(true)
    }

    fn broke(&mut self) -> Option<Broke> {
        self.broke.pop()
    }

    fn tables(&mut self) -> Result<Vec<(&'static str, u64, u64)>, Failed> {
        match Arc::get_mut(&mut self.inputs) {
            Some(inputs) => Ok(vec![("run", inputs.runs.len() as u64, 0)]),
            None => Err(ApiError::new("a task still reads the front end", None).into()),
        }
    }

    fn keep(mut self) -> Result<FakeInputs, ApiError> {
        if !self.apply()? {
            return Err(ApiError::new("the fakes' runs are kept while a task still reads them", None));
        }
        let mut inputs = Arc::into_inner(self.inputs)
            .ok_or_else(|| ApiError::new("the fakes' inputs are kept while a task still holds them", None))?;
        // Kept again: what this build left is not stored yet, as a real front end's is not.
        inputs.stored = false;
        Ok(inputs)
    }
}

/// The fakes' user code: a build script's run, or what is over as it starts, or a run that
/// keeps what it did in the store.
enum FakeRuns {
    Script(Script),
    Over(Over),
    Lost(Lost),
    Keeps(Keeps),
}

impl Runs<Fake> for FakeRuns {
    fn start(
        &mut self,
        store: &Store,
        writer: Option<&mut FakeWriter>,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
        match self {
            FakeRuns::Script(it) => it.start(store, writer, effects, out),
            FakeRuns::Over(it) => it.start(store, writer, effects, out),
            FakeRuns::Lost(it) => it.start(store, writer, effects, out),
            FakeRuns::Keeps(it) => it.start(store, writer, effects, out),
        }
    }

    fn owns(&self, tag: Tag) -> bool {
        match self {
            FakeRuns::Script(it) => it.owns(tag),
            FakeRuns::Over(it) => it.owns(tag),
            FakeRuns::Lost(it) => it.owns(tag),
            FakeRuns::Keeps(it) => it.owns(tag),
        }
    }

    fn complete(
        &mut self,
        completion: Completion,
        store: &mut Store,
        writer: Option<&mut FakeWriter>,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
        match self {
            FakeRuns::Script(it) => it.complete(completion, store, writer, effects, out),
            FakeRuns::Over(it) => it.complete(completion, store, writer, effects, out),
            FakeRuns::Lost(it) => it.complete(completion, store, writer, effects, out),
            FakeRuns::Keeps(it) => it.complete(completion, store, writer, effects, out),
        }
    }

    fn cancel(&mut self, effects: &mut Vec<Effect>) -> Option<bool> {
        match self {
            FakeRuns::Script(it) => it.cancel(effects),
            FakeRuns::Over(it) => it.cancel(effects),
            FakeRuns::Lost(it) => it.cancel(effects),
            FakeRuns::Keeps(it) => it.cancel(effects),
        }
    }

    fn take_puts(&mut self) -> RunPuts {
        match self {
            FakeRuns::Script(it) => it.take_puts(),
            FakeRuns::Over(it) => it.take_puts(),
            FakeRuns::Lost(it) => it.take_puts(),
            FakeRuns::Keeps(it) => it.take_puts(),
        }
    }
}

/// A build script's run that is over as it starts, its run asked of the writer.
struct Script(usize);

impl Runs<Fake> for Script {
    fn start(&mut self, _: &Store, writer: Option<&mut FakeWriter>, _: &mut Vec<Effect>, _: &mut Vec<Response>) -> Option<bool> {
        let writer = writer.expect("a build script's run is given its job's writer");
        Some(writer.ran(self.0, blaze_lower::Ran::default()).is_ok())
    }

    fn owns(&self, _: Tag) -> bool {
        false
    }

    fn complete(
        &mut self,
        _: Completion,
        _: &mut Store,
        _: Option<&mut FakeWriter>,
        _: &mut Vec<Effect>,
        _: &mut Vec<Response>,
    ) -> Option<bool> {
        unreachable!("it asked for nothing")
    }

    fn cancel(&mut self, _: &mut Vec<Effect>) -> Option<bool> {
        Some(false)
    }

    fn take_puts(&mut self) -> RunPuts {
        RunPuts::default()
    }
}

/// A build script's run whose end never comes: it says it waits on its effects and asked
/// for none, as a run whose completion was lost.
struct Lost;

impl Runs<Fake> for Lost {
    fn start(&mut self, _: &Store, _: Option<&mut FakeWriter>, _: &mut Vec<Effect>, _: &mut Vec<Response>) -> Option<bool> {
        None
    }

    fn owns(&self, _: Tag) -> bool {
        false
    }

    fn complete(
        &mut self,
        _: Completion,
        _: &mut Store,
        _: Option<&mut FakeWriter>,
        _: &mut Vec<Effect>,
        _: &mut Vec<Response>,
    ) -> Option<bool> {
        unreachable!("it asked for nothing")
    }

    fn cancel(&mut self, _: &mut Vec<Effect>) -> Option<bool> {
        Some(false)
    }

    fn take_puts(&mut self) -> RunPuts {
        RunPuts::default()
    }
}

/// The key and bytes of how the fakes' tests ran, as [`Keeps`] keeps them.
fn test_record_key() -> blaze_cas::Key {
    blaze_cas::Key(ContentId::of("blaze serve tests test record", b"the tests of /w"))
}

const TEST_RECORD: &[u8] = b"how the tests of /w ran\n";

/// The tests' run as a real one is to its job (`UnitRuns`): it ends on a completion of its
/// own, and on it keeps how its tests ran in the store, handing the put's ticket over
/// ([`Runs::take_puts`]) for its job to wait on before it answers.
struct Keeps {
    tag: Tag,
    puts: RunPuts,
    /// Whether it hands its put's ticket over; not, under [`TESTS_KEEP_UNTOLD`].
    tells: bool,
}

impl Runs<Fake> for Keeps {
    fn start(&mut self, _: &Store, _: Option<&mut FakeWriter>, effects: &mut Vec<Effect>, _: &mut Vec<Response>) -> Option<bool> {
        effects.push(Effect::Timer { after: KEEPS_FOR.get(), tag: self.tag });
        KEEPS_STARTED.set(KEEPS_STARTED.get() + 1);
        None
    }

    fn owns(&self, tag: Tag) -> bool {
        tag == self.tag
    }

    fn complete(
        &mut self,
        _: Completion,
        store: &mut Store,
        _: Option<&mut FakeWriter>,
        _: &mut Vec<Effect>,
        _: &mut Vec<Response>,
    ) -> Option<bool> {
        let off_loop = UNTOLD_WRITER.with_borrow_mut(|writer| {
            writer.as_mut().map(|writer| {
                let guard = blaze_cas::enter();
                writer.put(&guard, test_record_key(), blaze_cas::Kind::Query, TEST_RECORD.to_vec()).expect("one value under its key")
            })
        });
        let ticket = match off_loop {
            Some(ticket) => ticket,
            None => store.put(test_record_key(), blaze_cas::Kind::Query, TEST_RECORD.to_vec()).expect("one value under its key"),
        };
        if self.tells {
            self.puts.kept([ticket]);
        }
        Some(true)
    }

    fn cancel(&mut self, _: &mut Vec<Effect>) -> Option<bool> {
        Some(false)
    }

    fn take_puts(&mut self) -> RunPuts {
        std::mem::take(&mut self.puts)
    }
}

/// User code that is over as it starts.
struct Over(Vec<Response>);

impl Runs<Fake> for Over {
    fn start(&mut self, _: &Store, _: Option<&mut FakeWriter>, _: &mut Vec<Effect>, out: &mut Vec<Response>) -> Option<bool> {
        out.append(&mut self.0);
        Some(true)
    }

    fn owns(&self, _: Tag) -> bool {
        false
    }

    fn complete(
        &mut self,
        _: Completion,
        _: &mut Store,
        _: Option<&mut FakeWriter>,
        _: &mut Vec<Effect>,
        _: &mut Vec<Response>,
    ) -> Option<bool> {
        unreachable!("it asked for nothing")
    }

    fn cancel(&mut self, _: &mut Vec<Effect>) -> Option<bool> {
        Some(false)
    }

    fn take_puts(&mut self) -> RunPuts {
        RunPuts::default()
    }
}

impl Front<Fake> for FakeFront {
    fn fresh(&self, _: usize) -> bool {
        false
    }

    fn compile(&self, reader: FakeReader, unit: usize, phase: Phase, _: &mut Vec<Image<Fake>>) -> Result<Option<Compiled>, Failed> {
        assert_ne!(self.goal, Goal::Reach, "a reach compiles nothing");
        PHASES.with(|it| it.borrow_mut().push((unit, phase)));
        assert_ne!(PANICS_COMPILE.get(), Some(unit), "serve-test: the unit's compile panics");
        if ERRS_COMPILE.get() == Some(unit) {
            return Err(Failed::Error(ApiError::new("serve-test: unsupported: no `lang_start`", Some(format!("unit {unit}")))));
        }
        if phase == Phase::Names {
            SEEN.with(|it| it.borrow_mut().push((unit, reader.0.runs.clone())));
            if unit == 0 && KEEP.get() {
                HOLD.with(|it| it.borrow_mut().push(reader));
            }
        }
        if phase != Phase::Store {
            return Ok(None);
        }
        let (mut puts, mut outputs) = (Vec::new(), Vec::new());
        if unit == 3 && OUTPUT.get() {
            let key = blaze_cas::Key(ContentId::of("blaze serve tests output", APP));
            puts.push(Put { key, kind: blaze_cas::Kind::Output, bytes: APP.to_vec() });
            outputs.push(Output { path: APP_AT.into(), key, executable: true });
        }
        let diagnostics = match unit == 3 && WARNS.get() {
            true => vec![warning_of_app("unused variable: `x`"), warning_of_app("unused import: `y`")],
            false => Vec::new(),
        };
        let share = match unit == 0 && SHARES.get() {
            true => vec![Put { key: shared_key(), kind: blaze_cas::Kind::Query, bytes: SHARED.to_vec() }],
            false => Vec::new(),
        };
        Ok(Some(Compiled { diagnostics, ok: true, puts, share, outputs, artifact: None, macros: None, links: Vec::new(), misses: Vec::new() }))
    }

    fn refused(&self, _: usize, what: String) -> Diagnostic {
        Diagnostic { level: Level::Error, rendered: format!("error: {what}\n"), ..warning_of_app(&what) }
    }

    fn script(&self, unit: usize, _: u64) -> Result<FakeRuns, ApiError> {
        Ok(match LOSES_RUNS.get() {
            true => FakeRuns::Lost(Lost),
            false => FakeRuns::Script(Script(unit)),
        })
    }

    fn tests(&self, _: &[Artifact], selection: Selection<'_>, tags: u64) -> Result<FakeRuns, ApiError> {
        assert!(selection.ub.is_none(), "tests under the evaluator are evaluated, not run");
        if TESTS_KEEP.get() {
            return Ok(FakeRuns::Keeps(Keeps { tag: Tag(tags), puts: RunPuts::default(), tells: !TESTS_KEEP_UNTOLD.get() }));
        }
        Ok(FakeRuns::Over(Over(Vec::new())))
    }

    fn ub_tests(&self, FakeReader(_): FakeReader, selection: Selection<'_>) -> Result<Vec<String>, Failed> {
        assert_eq!(self.goal, Goal::Check, "tests under the evaluator only check their units");
        let all = ["a::adds", "a::overflows", "b::reads"];
        Ok(all.iter().filter(|it| selection.filter.iter().all(|filter| it.contains(&**filter))).map(|it| (*it).to_owned()).collect())
    }

    fn evaluate(&self, FakeReader(_): FakeReader, test: &str, ub: Ub) -> Result<TestResult, Failed> {
        let (outcome, output) = match test {
            "a::overflows" => (TestOutcome::Failed, format!("undefined behavior\nseed: {}\n", ub.seed.unwrap_or(0)).into_bytes()),
            _ => (TestOutcome::Passed, Vec::new()),
        };
        Ok(TestResult {
            name: test.to_owned(),
            binary: "a".into(),
            executable: 0,
            millis: 0,
            output,
            outcome,
            cached: None,
            outside: false,
        })
    }

    fn run(&self, _: &[Artifact], _: Option<&str>, _: &[String], _: u64) -> Result<FakeRuns, ApiError> {
        Ok(FakeRuns::Over(Over(vec![Response::Ran { code: Some(0), stdout: b"hi\n".to_vec(), stderr: Vec::new() }])))
    }

    fn reach(&self, FakeReader(_): FakeReader, _: &FakeFront, FakeReader(_): FakeReader, changes: &[Change]) -> Result<Reach, Failed> {
        Ok(Reach { changes: changes.to_vec(), items: Vec::new(), compiled: Vec::new(), tests: Vec::new() })
    }

    fn edit(&self, FakeReader(_): FakeReader, _: &[blaze_api::Edit]) -> Result<blaze_api::Edited, Failed> {
        Ok(blaze_api::Edited { ops: Vec::new(), items: Vec::new() })
    }

    fn ahead(&self, FakeReader(_): FakeReader, filter: &[String]) -> Result<Vec<u32>, Failed> {
        Ok(if filter.iter().all(|it| "a::adds".contains(&**it)) { vec![0] } else { Vec::new() })
    }

    fn codegen_inputs(&self, _: FakeReader) -> Result<Vec<(ContentId, ContentId)>, Failed> {
        Ok(Vec::new())
    }

    fn tables(&self, mut writer: FakeWriter, _: &[(&'static str, u64, u64)]) -> Result<Vec<Table>, Failed> {
        let counted = writer.tables()?;
        Ok(counted.into_iter().map(|(name, elements, bytes)| Table { name: name.to_owned(), elements, bytes }).collect())
    }

    /// The program and the build scripts' runs its front end had applied, read with the front
    /// end exclusive, as the real pack sets its proc macros.
    fn pack(&self, mut writer: FakeWriter, program: &str) -> Result<Vec<u8>, Failed> {
        assert_eq!(self.goal, Goal::Check, "a pack only checks its units");
        writer.apply()?;
        match Arc::get_mut(&mut writer.inputs) {
            Some(inputs) => Ok(format!("{program} {:?}", inputs.runs).into_bytes()),
            None => Err(ApiError::new("a task still reads the front end", None).into()),
        }
    }

    /// One page naming the build scripts' runs its front end had applied, read with the front
    /// end exclusive, as the real documentation sets `cfg(doc)`.
    fn doc(&self, mut writer: FakeWriter, no_deps: bool, private: bool) -> Result<Vec<Response>, Failed> {
        assert_eq!(self.goal, Goal::Check, "the documentation only checks its units");
        writer.apply()?;
        let Some(inputs) = Arc::get_mut(&mut writer.inputs) else {
            return Err(ApiError::new("a task still reads the front end", None).into());
        };
        let page = format!("{no_deps} {private} {:?}", inputs.runs).into_bytes();
        let crates = vec![blaze_api::DocumentedCrate { name: "app".into(), package: "app v0.1.0 (/w)".into(), asked: true }];
        Ok(vec![
            Response::Documented(blaze_api::Documented { dir: "/w/target/doc".into(), crates, pages: 1, source_pages: 0 }),
            Response::DocPage { path: "app/index.html".into(), page },
        ])
    }
}

fn options() -> Options {
    Options {
        state: name(STATE),
        std: name(STD),
        builds: name("cache/blaze/build"),
        writer: Hasher::new("blaze v2 test writer").part(STATE.as_bytes()).finish(),
        socket: name(SOCKET),
        http: None,
        budget: None,
        threads: 2,
        io: 2,
        env: vec![("PATH".into(), "/bin".into()), ("HOME".into(), "/home/u".into())],
        host: "x86_64-unknown-linux-gnu".into(),
        toolchain: Hasher::new("blaze v2 test toolchain").finish(),
        color: false,
        build: blaze_api::Build::default(),
        // A table of its own: each daemon drives one, and tests run at once.
        outside: Box::leak(Box::new(blaze_vfs::outside::Dirs::new())),
        stall: blaze_profile::Watchdog::BOUND,
        #[cfg(unix)]
        activated: None,
        profiling: None,
    }
}

fn checkout(sim: &mut Sim, manifest: &str) {
    sim.change(&name("w/Cargo.toml"), manifest.as_bytes());
    sim.change(&name("w/src/main.rs"), b"fn main() {}\n");
}

/// The simulation's daemon: on [`Sim`], building with the fakes.
type SimDaemon = Daemon<Sim, Fake>;

fn start(sim: Sim, steps: &Arc<Fake>, seed: u64) -> SimDaemon {
    Daemon::start(sim, options(), steps.clone(), Runner::Inline(Inline::new(seed))).unwrap_or_else(|why| panic!("{why}"))
}

struct Client {
    conn: Conn,
    frames: Frames,
    next: u64,
    got: Vec<(u64, Response)>,
}

fn dial(daemon: &mut SimDaemon) -> Client {
    for _ in 0..TURNS {
        if let Ok(conn) = daemon.driver().dial(&Address::Unix(name(SOCKET))) {
            return Client { conn, frames: Frames::new(), next: 1, got: Vec::new() };
        }
        daemon.turn();
    }
    panic!("the daemon never listened");
}

impl Client {
    fn send(&mut self, daemon: &mut SimDaemon, request: Request) -> u64 {
        let id = self.next;
        self.next += 1;
        let bytes = frames::encode(&Envelope { schema: SCHEMA, id, body: request }).unwrap();
        daemon.driver().peer_send(self.conn, &bytes);
        id
    }

    /// Sends `requests` in one write, so the daemon reads them in one turn, each taken before
    /// anything of the ones before it runs; their ids, in order.
    fn send_together(&mut self, daemon: &mut SimDaemon, requests: Vec<Request>) -> Vec<u64> {
        let (mut bytes, mut ids) = (Vec::new(), Vec::new());
        for body in requests {
            let id = self.next;
            self.next += 1;
            bytes.extend(frames::encode(&Envelope { schema: SCHEMA, id, body }).unwrap());
            ids.push(id);
        }
        daemon.driver().peer_send(self.conn, &bytes);
        ids
    }

    fn take(&mut self, daemon: &mut SimDaemon) {
        self.frames.push(&daemon.driver().peer_received(self.conn));
        while let Some(item) = self.frames.next().unwrap() {
            let envelope: Envelope<Response> = frames::decode(&item).unwrap();
            self.got.push((envelope.id, envelope.body));
        }
    }

    fn done(&self, id: u64) -> bool {
        self.got.iter().any(|(at, it)| *at == id && matches!(it, Response::Done { .. }))
    }

    fn of(&self, id: u64) -> Vec<Response> {
        self.got.iter().filter(|(at, _)| *at == id).map(|(_, it)| it.clone()).collect()
    }

    /// Turns the daemon until request `id` is done; what it got.
    fn wait(&mut self, daemon: &mut SimDaemon, id: u64) -> Vec<Response> {
        for _ in 0..TURNS {
            daemon.turn();
            self.take(daemon);
            if self.done(id) {
                return self.of(id);
            }
        }
        panic!("request {id} never ended; it got {:?}", self.of(id));
    }
}

fn agent() -> Targets {
    Targets {
        packages: Vec::new(),
        profile: String::new(),
        target: None,
        features: Vec::new(),
        all_features: false,
        no_default_features: false,
        cargo: None,
    }
}

fn warnings(responses: &[Response]) -> Vec<String> {
    responses
        .iter()
        .filter_map(|it| match it {
            Response::Diagnostic(diagnostic) if diagnostic.level == Level::Warning => Some(diagnostic.message.clone()),
            _ => None,
        })
        .collect()
}

fn turn_until(daemon: &mut SimDaemon, mut what: impl FnMut(&mut SimDaemon) -> bool) {
    for _ in 0..TURNS {
        if what(daemon) {
            return;
        }
        daemon.turn();
    }
    panic!("the daemon never got there");
}

/// Stops it as `blaze stop` would, and starts another on the same disk.
fn restart(mut daemon: SimDaemon, steps: &Arc<Fake>, seed: u64) -> SimDaemon {
    daemon.stop();
    let mut turns = 0;
    while daemon.turn() {
        turns += 1;
        assert!(turns < TURNS, "the daemon never stopped");
    }
    start(daemon.into_driver(), steps, seed)
}

/// The daemon listens once `start` returns, before its first turn: `blaze serve` then says so
/// and writes the ready byte the CLI that started it waits for, and that CLI connects at once.
#[test]
fn a_started_daemon_is_reached_before_its_first_turn() {
    let sim = Sim::new(62, Faults::default());
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 62);
    let reached = daemon.driver().dial(&Address::Unix(name(SOCKET)));
    assert!(reached.is_ok(), "a client that connects as start returns finds nothing: {reached:?}");
}

/// The same on this machine's own driver and file system, as the CLI meets it: `blaze serve`
/// says it listens once `start` returns (and one turn has heard the bind), and the CLI
/// connects at once, with no retry.
#[cfg(unix)]
#[test]
fn a_cli_that_connects_as_the_daemon_says_it_listens_finds_its_socket() {
    // Under /tmp: a socket's path is at most about a hundred bytes, which a deeper temporary
    // directory may pass.
    let root = tempfile::tempdir_in("/tmp").unwrap();
    std::fs::create_dir_all(root.path().join(SOCKET).parent().unwrap()).unwrap();
    let driver = blaze_core::native(root.path(), 2).unwrap();
    let steps = Arc::new(Fake::default());
    let daemon = Daemon::start(driver, options(), steps, Runner::Inline(Inline::new(63))).unwrap_or_else(|why| panic!("{why}"));
    let reached = std::os::unix::net::UnixStream::connect(root.path().join(SOCKET));
    assert!(reached.is_ok(), "a CLI that connects as start returns finds no socket: {reached:?}");
    drop(daemon);
}

#[test]
fn a_build_of_a_checkout_goes_from_the_server_to_built() {
    let mut sim = Sim::new(1, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 1);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);

    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    assert!(got.contains(&Response::Fetched { verb: FetchVerb::Downloaded, what: "dep v1.0.0".into() }), "{got:#?}");
    assert!(got.contains(&Response::Fetched { verb: FetchVerb::Updating, what: "crates.io index".into() }), "{got:#?}");
    assert_eq!(warnings(&got), vec!["[package]\nname = \"app\"\n".to_owned()]);
    let units: Vec<(String, Verb)> = got
        .iter()
        .filter_map(|it| match it {
            Response::Unit(unit) => Some((unit.name.clone(), unit.verb)),
            _ => None,
        })
        .collect();
    assert_eq!(units.len(), 2, "one `Compiling` for each package: {units:?}");
    assert!(units.contains(&("dep".into(), Verb::Compiling)) && units.contains(&("app".into(), Verb::Compiling)));
    let built = got.iter().position(|it| matches!(it, Response::Built { profile, .. } if profile == FINISHED));
    let worked = got.iter().position(|it| matches!(it, Response::Work(_)));
    assert!(built.is_some() && built < worked, "`Built`, then `Work`: {got:#?}");
    assert_eq!(daemon.driver().contents(&name("w/Cargo.lock")), b"# a lock\n");
    assert!(daemon.mounts().has(DEP) && daemon.mounts().has(INDEX) && daemon.mounts().has(LIBRARY));
}

/// A build script's run whose end never comes (as a lost completion) leaves the build with
/// nothing that can move it: no task runs, no effect is in flight. The build fails within
/// seconds with the stall, naming the run still open and the program waiting on it, rather
/// than waiting for the watchdog, which only watches tasks that run.
#[test]
fn a_build_nothing_can_move_fails_with_the_stall_naming_each_wait() {
    LOSES_RUNS.set(true);
    let mut sim = Sim::new(3, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 3);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    LOSES_RUNS.set(false);

    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");
    let said = got
        .iter()
        .find_map(|it| match it {
            Response::Error(error) if error.what.starts_with("blaze stalled: for") => Some(error.what.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the build fails with the stall: {got:#?}"));
    for part in ["nothing could move the build", "1 loop tasks open", "waits on node"] {
        assert!(said.contains(part), "the stall says `{part}`: {said}");
    }
    let quiet: f64 = said["blaze stalled: for ".len()..].split(' ').next().unwrap().parse().unwrap();
    assert!((2.0..10.0).contains(&quiet), "the stall is said within seconds of the build going still: {said}");
}

/// What a daemon of one `BLAZE_HOME` shares of the standard library, a daemon of another finds
/// once it starts: one shared store under [`Options::std`], each a writer of its own.
#[test]
fn what_one_homes_daemon_shares_of_std_another_homes_finds() {
    SHARES.set(true);
    let mut sim = Sim::new(23, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut first = start(sim, &steps, 23);
    let mut client = dial(&mut first);
    let id = client.send(&mut first, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut first, id);
    SHARES.set(false);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    assert_eq!(first.std_store().get(&shared_key()).unwrap().map(|it| it.bytes.to_vec()), Some(SHARED.to_vec()));
    first.stop();
    let mut turns = 0;
    while first.turn() {
        turns += 1;
        assert!(turns < TURNS, "the daemon never stopped");
    }

    let other = Options {
        state: name("cache/blaze/v2-other"),
        socket: name("run/blaze/other.sock"),
        writer: Hasher::new("blaze v2 test writer").part(b"cache/blaze/v2-other").finish(),
        ..options()
    };
    let second = Daemon::start(first.into_driver(), other, steps.clone(), Runner::Inline(Inline::new(24)))
        .unwrap_or_else(|why| panic!("{why}"));
    assert_eq!(second.std_store().get(&shared_key()).unwrap().map(|it| it.bytes.to_vec()), Some(SHARED.to_vec()));
    assert!(second.store().get(&shared_key()).unwrap().is_none(), "the home's own store has none of it");
}

/// Each unit's compile is a node a phase: its names first, its check and its codegen once those
/// are, its link once both are, and its store last, each once.
#[test]
fn a_units_compile_runs_each_of_its_phases_once_in_order() {
    PHASES.with(|it| it.borrow_mut().clear());
    let mut sim = Sim::new(17, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 17);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");

    let ran = PHASES.with(|it| it.take());
    let mut units: Vec<usize> = ran.iter().map(|it| it.0).collect();
    units.sort_unstable();
    units.dedup();
    assert!(!units.is_empty(), "the build compiled no unit");
    for unit in units {
        let phases: Vec<Phase> = ran.iter().filter(|it| it.0 == unit).map(|it| it.1).collect();
        let mut each = phases.clone();
        each.sort_unstable_by_key(|it| *it as u8);
        assert!(each == Phase::BUILD || each == Phase::CHECK, "unit {unit} ran {phases:?}");
        assert_eq!((phases[0], phases[phases.len() - 1]), (Phase::Names, Phase::Store), "unit {unit}: {phases:?}");
        if let Some(link) = phases.iter().position(|it| *it == Phase::Link) {
            assert!(phases[..link].contains(&Phase::Bodies) && phases[..link].contains(&Phase::Codegen), "unit {unit}: {phases:?}");
        }
    }
}

#[test]
fn a_unit_that_warned_ends_after_its_warnings_and_built_counts_the_members() {
    let mut sim = Sim::new(31, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 31);
    WARNS.set(true);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    WARNS.set(false);

    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    let of_app = |it: &Response| matches!(it, Response::Diagnostic(it) if it.unit.is_some());
    let said: Vec<usize> = (0..got.len()).filter(|&at| of_app(&got[at])).collect();
    let ended: Vec<usize> = (0..got.len()).filter(|&at| matches!(got[at], Response::UnitEnded(_))).collect();
    assert_eq!(said.len(), 2, "{got:#?}");
    assert_eq!(ended, [said[1] + 1], "the unit ends once, right after its last warning: {got:#?}");
    let Response::UnitEnded(unit) = &got[ended[0]] else { unreachable!() };
    assert_eq!(unit.target.name, "app");
    let built = got.iter().find_map(|it| match it {
        Response::Built { members, .. } => Some(*members),
        _ => None,
    });
    assert_eq!(built, Some(1), "`app` is the one member; `dep` is from the registry");
}

#[test]
fn a_request_on_a_checkout_sees_what_was_saved_before_it() {
    let mut sim = Sim::new(2, Faults::default());
    checkout(&mut sim, "first\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 2);
    let mut client = dial(&mut daemon);
    let first = client.send(&mut daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
    assert_eq!(warnings(&client.wait(&mut daemon, first)), vec!["first\n".to_owned()]);
    for round in 0..8 {
        let saved = format!("saved before request {round}\n");
        daemon.driver().change(&name("w/Cargo.toml"), saved.as_bytes());
        let id = client.send(&mut daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
        let got = client.wait(&mut daemon, id);
        assert_eq!(warnings(&got), vec![saved], "{got:#?}");
    }
}

#[test]
fn the_mount_table_survives_a_restart() {
    let mut sim = Sim::new(3, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 3);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
    assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }));
    let sorted = |daemon: &SimDaemon| {
        let mut all: Vec<_> = daemon.mounts().all().cloned().collect();
        all.sort_by(|a, b| a.at.cmp(&b.at));
        all
    };
    let before = sorted(&daemon);
    assert_eq!(before.len(), 3);
    let roots = daemon.mounts().roots();
    assert_eq!(LIBRARY_READS.get(), 1);

    let mut daemon = restart(daemon, &steps, 4);
    assert_eq!(sorted(&daemon), before);
    assert_eq!(daemon.mounts().roots(), roots);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    assert!(!got.iter().any(|it| matches!(it, Response::Fetched { .. })), "nothing fetched again: {got:#?}");
    let fetched = steps.fetched.lock().clone();
    assert!(fetched.values().all(|&it| it == 1), "each source fetched once: {fetched:?}");
    assert_eq!(LIBRARY_READS.get(), 1, "the library's files are read once, not again by the next daemon");
}

#[test]
fn an_agents_targets_are_the_views_invocation() {
    let sim = Sim::new(5, Faults::default());
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 5);
    let mut client = dial(&mut daemon);
    let base = daemon.vfs().empty();
    let ops = vec![Op::Write { path: "Cargo.toml".into(), bytes: b"agent\n".to_vec(), mode: blaze_vfs::Mode::Regular }];
    let commit = client.send(&mut daemon, Request::Commit { head: "agents/one".into(), base, ops });
    let root = match client.wait(&mut daemon, commit).first() {
        Some(Response::Committed { root, .. }) => *root,
        other => panic!("the commit answered {other:?}"),
    };
    let targets = Targets {
        packages: vec!["app".into()],
        profile: "release".into(),
        target: None,
        features: vec!["x".into()],
        all_features: false,
        no_default_features: true,
        cargo: None,
    };
    SAW.with(|it| it.borrow_mut().clear());
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("agents/one".into()), targets });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    assert_eq!(warnings(&got), vec!["agent\n".to_owned()]);

    let at = format!("/{STATE}/views/{root:x}");
    let config = SAW.with(|it| it.borrow().last().cloned()).expect("the load ran");
    let args = ["-p", "app", "--profile", "release", "-F", "x", "--no-default-features"];
    assert_eq!(
        config.invocation,
        Invocation {
            command: Command::Build,
            args: args.iter().map(|it| (*it).to_owned()).collect(),
            root: at.clone(),
            cwd: at.clone(),
            env: options().env,
            files: Vec::new(),
        }
    );
    assert_eq!(config.library, LIBRARY);
    assert_eq!(config.host, "x86_64-unknown-linux-gnu");
    let view = config.mounts.iter().find(|it| it.origin == Origin::View).expect("the view is mounted");
    assert_eq!(view.at, at);
    assert!(config.mounts.windows(2).all(|it| it[0].at <= it[1].at), "mounts sorted by where");
    assert_eq!(daemon.driver().contents(&FileName::new(&format!("{}/Cargo.lock", &at[1..])).unwrap()), b"# a lock\n");
}

#[test]
fn a_start_from_the_image_runs_no_load_again() {
    let mut sim = Sim::new(6, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 6);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
    assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }));
    turn_until(&mut daemon, |it| it.settled());
    assert!(daemon.driver().exists(&name(&format!("{STATE}/db/image"))), "the cold build wrote the image");

    let mut daemon = restart(daemon, &steps, 7);
    PLANS.with(|it| it.set(0));
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
    assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }));
    // Started again in this process, its database is warm only if nothing here had interned a
    // symbol before the restart restored them.
    let loads = PLANS.with(Cell::get);
    assert_eq!(loads == 0, daemon.warm(), "the load ran again exactly when the database started empty ({loads})");
}

#[test]
fn a_daemon_stopped_as_its_first_build_answers_writes_the_image_the_next_start_restores() {
    let mut sim = Sim::new(9, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 9);
    assert!(daemon.started().restored.is_none(), "the first start has no image");
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
    assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }));
    // Stopped before the loop turns idle, where a build's image is otherwise asked.
    let mut daemon = restart(daemon, &steps, 10);
    assert!(daemon.driver().exists(&name(&format!("{STATE}/db/image"))), "the stop wrote the build's image");
    // A second daemon in one process can't take the image's symbols (`Cold::Interned`); every
    // other start opens it.
    let started = daemon.started();
    assert!(
        started.restored.is_some() || matches!(started.cold, Some(super::daemon::Cold::Interned)),
        "the next start opens the stop's image: {started}"
    );
    assert!(!matches!(started.cold, Some(super::daemon::Cold::NoImage)), "{started}");
}

/// What a build leaves for the store as its front end is kept ([`Steps::store_kept`]) is
/// durable by the time it answers, a failed build's too: a power loss right after the answer
/// (no stop, no image) loses none of it, and the next start finds it in the store. Before, it
/// was written only when the head's next build lowered into the front end, so a build followed
/// by a stop or a crash left the store none of its dependency images or check outcomes
/// (persisted-state-round-trips).
///
/// A crash keeps a random prefix of what was not synced (`Sim::crash`), so the put could
/// survive one seed by chance: across these seeds, only a sync before the answer keeps it in
/// every one.
#[test]
fn what_a_build_left_for_the_store_is_durable_once_it_answers_and_survives_a_crash() {
    let runs = (73..81).flat_map(|seed| [(seed, None), (seed + 100, Some(1))]);
    for (seed, errs) in runs {
        ERRS_COMPILE.set(errs);
        STORES.set(0);
        let mut sim = Sim::new(seed, Faults::default());
        checkout(&mut sim, "[package]\nname = \"app\"\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        let mut client = dial(&mut daemon);
        let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
        let got = client.wait(&mut daemon, id);
        ERRS_COMPILE.set(None);
        assert_eq!(got.last(), Some(&Response::Done { ok: errs.is_none() }), "{got:#?}");
        assert_eq!(STORES.get(), 1, "the kept front end is stored once, as its build ends");
        assert!(holds_kept(daemon.store(), "/w"), "it is in the store as the build answers");
        let mut sim = daemon.into_driver();
        sim.crash();
        let daemon = start(sim, &steps, seed + 1000);
        assert!(holds_kept(daemon.store(), "/w"), "a crash after the answer keeps it (failing build: {errs:?})");
    }
}

fn second_checkout(sim: &mut Sim) {
    sim.change(&name("v/Cargo.toml"), b"[package]\nname = \"app\"\n");
    sim.change(&name("v/src/main.rs"), b"fn main() {}\n");
}

/// The store's directory under the daemon's state.
fn store_dir() -> FileName {
    name(&format!("{STATE}/store"))
}

/// `turns` turns of the daemon, `client` taking what it is sent.
fn turns(daemon: &mut SimDaemon, client: &mut Client, turns: usize) {
    for _ in 0..turns {
        daemon.turn();
        client.take(daemon);
    }
}

/// Review C1, with two segments: a put of a value the store keeps already is handed the
/// ticket of the put that kept it, from any writer's segment. Here a writer of its own (a
/// segment above the loop's) puts the value every checkout leaves first, and `/v`'s build
/// makes it durable. `/w`'s build then puts its own two on the loop's segment and that value
/// last, handed the writer's ticket, durable already: one `max` of its tickets is that one, so
/// a job that waited on it answered before its own were durable and a crash lost them. Across
/// seeds, as a crash keeps a random prefix of what was not synced.
#[test]
fn a_checkout_whose_last_put_another_put_first_answers_once_its_own_are_durable() {
    for seed in 120..136 {
        let mut sim = Sim::new(seed, Faults::default());
        checkout(&mut sim, "[package]\nname = \"app\"\n");
        second_checkout(&mut sim);
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        let mut writer = daemon.store_mut().writer();
        let guard = blaze_cas::enter();
        let first = writer.put(&guard, shared_kept_key(), blaze_cas::Kind::Query, SHARED_KEPT.to_vec()).unwrap();
        drop(guard);
        let mut client = dial(&mut daemon);
        for head in ["/v", "/w"] {
            let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head(head.into()), targets: agent() });
            assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }), "{head}");
            // `/v` answered on the writer's put, so it waited for it.
            assert!(daemon.store().durable(first), "seed {seed}: {head} answered before the value it put was durable");
        }
        let mut sim = daemon.into_driver();
        sim.crash();
        let daemon = start(sim, &steps, seed + 1000);
        assert!(holds_kept(daemon.store(), "/w") && holds_kept(daemon.store(), "/v"), "seed {seed}");
        drop(writer);
    }
}

/// Wait::Everything (the store model's finding): a value read from the store comes with no
/// ticket, so a job that put nothing new may have read another job's value that is not durable
/// yet, and answering on it before a crash loses it gives what a restart can't. With the
/// store's syncs held, `/w`'s build puts what it leaves and can make none of it durable; a
/// build of `/v` that puts nothing then must not answer, though it waits on no put of its own.
/// Let go at the bound, it fails naming the store's ticket, as `/w` does.
#[test]
fn a_build_that_puts_nothing_waits_for_what_the_loop_put_before_it_ended() {
    let mut sim = Sim::new(180, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    second_checkout(&mut sim);
    PUTS_NOTHING.with_borrow_mut(|it| *it = "/v".to_owned());
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 180);
    let mut client = dial(&mut daemon);
    daemon.driver().hold_syncs(store_dir());
    let start = daemon.driver().now();
    let w = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    for _ in 0..TURNS {
        if holds_kept(daemon.store(), "/w") {
            break;
        }
        daemon.turn();
        client.take(&mut daemon);
    }
    assert!(holds_kept(daemon.store(), "/w"), "the build of /w never put what it left");
    let v = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/v".into()), targets: agent() });
    turns(&mut daemon, &mut client, 3000);
    assert!(client.of(v).iter().any(|it| matches!(it, Response::Built { .. })), "/v was built: {:#?}", client.of(v));
    assert!(!client.done(v), "/v answered on a loop whose puts are not durable: {:#?}", client.of(v));
    assert!(!client.done(w), "/w answered with what it left not durable: {:#?}", client.of(w));
    // The waits above ran on the clock's own pace, far short of the bound: they did not
    // answer because they wait, not because time ran out first.
    let ran = daemon.driver().now() - start;
    assert!(ran < Duration::from_secs(1), "the turns took {ran:?} of the clock before the advance");
    for _ in 0..10 {
        if client.done(v) && client.done(w) {
            break;
        }
        daemon.driver().advance(DURABLE_WITHIN);
        turns(&mut daemon, &mut client, 50);
    }
    for (head, id) in [("/v", v), ("/w", w)] {
        let got = client.of(id);
        assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{head}: {got:#?}");
        let named = |it: &Response| matches!(it, Response::Error(error) if error.what.contains("store ticket") && error.what.contains("answered nothing"));
        assert!(got.iter().any(named), "{head}: {got:#?}");
    }
    PUTS_NOTHING.with_borrow_mut(String::clear);
}

/// Bounded progress: a store whose commit hangs (a sync that never returns) holds a build's
/// answer, and the build fails naming the checkout and the store once the store has answered
/// nothing for [`DURABLE_WITHIN`] on the injected clock, not before; the daemon keeps serving.
#[test]
fn a_build_whose_store_commit_hangs_fails_naming_it_once_the_store_answers_nothing_for_the_bound() {
    let mut sim = Sim::new(190, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 190);
    let mut client = dial(&mut daemon);
    daemon.driver().hold_syncs(store_dir());
    let start = daemon.driver().now();
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    turns(&mut daemon, &mut client, 3000);
    assert!(!client.done(id), "answered with what it left not durable: {:#?}", client.of(id));
    assert!(daemon.driver().held_syncs() > 0, "the store's commit is held");
    // So that "short of the bound" below is short of it: the build itself took next to none
    // of the clock.
    let ran = daemon.driver().now() - start;
    assert!(ran < Duration::from_secs(1), "the build took {ran:?} of the clock before the advance");
    // Short of the bound since the store was last heard from: it still waits.
    daemon.driver().advance(DURABLE_WITHIN - Duration::from_secs(1));
    turns(&mut daemon, &mut client, 50);
    assert!(!client.done(id), "gave up before the bound: {:#?}", client.of(id));
    for _ in 0..10 {
        if client.done(id) {
            break;
        }
        daemon.driver().advance(Duration::from_secs(1).max(DURABLE_WITHIN / 10));
        turns(&mut daemon, &mut client, 50);
    }
    let got = client.of(id);
    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");
    let named = |it: &Response| {
        matches!(it, Response::Error(error) if error.what.contains("the build of /w")
            && error.what.contains("answered nothing") && error.what.contains("store ticket"))
    };
    assert!(got.iter().any(named), "{got:#?}");
    let status = client.send(&mut daemon, Request::Status);
    let said = client.wait(&mut daemon, status);
    assert!(!said.is_empty(), "the daemon serves after the build failed");
}

/// Bounded progress is no progress, not total time: a store whose commit is slow but moving
/// (each write and sync of it taking well under [`DURABLE_WITHIN`], the commit's steps one
/// after another taking well over it in all) is waited for, and the build succeeds. A bound
/// on the whole wait, or one that counted from the wait's start alone, fails it.
#[test]
fn a_build_whose_store_commit_is_slow_but_moving_waits_past_the_bound_and_succeeds() {
    let mut sim = Sim::new(197, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 197);
    let mut client = dial(&mut daemon);
    // Under the profile's watchdogs' bound (60 s) too, should a pool task run meanwhile; the
    // commit's four steps one after another (truncate, write, sync, head) still take well past
    // the bound.
    let step = DURABLE_WITHIN * 4 / 10;
    daemon.driver().slow_writes(store_dir(), step);
    let start = daemon.driver().now();
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    let took = daemon.driver().now() - start;
    assert!(took > DURABLE_WITHIN, "the commit took {took:?}, not past the bound: the test tests nothing");
    assert!(holds_kept(daemon.store(), "/w"), "answered, and what it left is not in the store");
}

/// The backstop of Wait::Everything ([`blaze_cas::Store::last_puts`]): a put that no caller
/// hands its ticket over for (the tests' run here keeps how they ran and tells no one, as an
/// error path that put before it failed does) is still durable before the job answers, so a
/// crash after the answer keeps it. Across seeds, as a crash keeps a random prefix of what was
/// not synced. Without the backstop the job waits only on what it was told of, which its
/// earlier puts made durable already, and a crash loses the record.
#[test]
fn a_put_no_one_hands_over_is_durable_once_its_job_answers() {
    TESTS_KEEP.set(true);
    TESTS_KEEP_UNTOLD.set(true);
    for seed in 220..236 {
        let mut sim = Sim::new(seed, Faults::default());
        checkout(&mut sim, "[package]\nname = \"app\"\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        let mut client = dial(&mut daemon);
        let request =
            Request::Test { view: ViewRef::Head("/w".into()), targets: agent(), filter: Vec::new(), all: false, args: Vec::new(), ub: None };
        let id = client.send(&mut daemon, request);
        let got = client.wait(&mut daemon, id);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
        assert!(daemon.store().get(&test_record_key()).unwrap().is_some(), "seed {seed}: the run kept nothing");
        let mut sim = daemon.into_driver();
        sim.crash();
        let daemon = start(sim, &steps, seed + 1000);
        let kept = daemon.store().get(&test_record_key()).unwrap().is_some_and(|it| it.bytes == TEST_RECORD);
        assert!(kept, "seed {seed}: the job answered and a crash lost a put no one handed over");
    }
    TESTS_KEEP_UNTOLD.set(false);
    TESTS_KEEP.set(false);
}

/// [`a_put_no_one_hands_over_is_durable_once_its_job_answers`], with the untold put made
/// through a writer of the store's own ([`blaze_cas::Store::writer`], as a worker's or a
/// [`blaze_cas::Handle`]'s is): a segment other than the loop's. The backstop covers every
/// segment; one that covered the loop's alone answered before this put was durable, and a
/// crash lost it.
#[test]
fn a_put_no_one_hands_over_through_another_segment_is_durable_once_its_job_answers() {
    TESTS_KEEP.set(true);
    TESTS_KEEP_UNTOLD.set(true);
    for seed in 250..266 {
        let mut sim = Sim::new(seed, Faults::default());
        checkout(&mut sim, "[package]\nname = \"app\"\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        let writer = daemon.store_mut().writer();
        UNTOLD_WRITER.with_borrow_mut(|it| *it = Some(writer));
        let mut client = dial(&mut daemon);
        let request =
            Request::Test { view: ViewRef::Head("/w".into()), targets: agent(), filter: Vec::new(), all: false, args: Vec::new(), ub: None };
        let id = client.send(&mut daemon, request);
        let got = client.wait(&mut daemon, id);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
        let written = UNTOLD_WRITER.with_borrow_mut(Option::take).expect("the writer is still set");
        let put = daemon.store().get(&test_record_key()).unwrap().is_some();
        assert!(put, "seed {seed}: the run kept nothing");
        let mut sim = daemon.into_driver();
        sim.crash();
        drop(written);
        let daemon = start(sim, &steps, seed + 1000);
        let kept = daemon.store().get(&test_record_key()).unwrap().is_some_and(|it| it.bytes == TEST_RECORD);
        assert!(kept, "seed {seed}: the job answered and a crash lost a put no one handed over, made on another segment");
    }
    TESTS_KEEP_UNTOLD.set(false);
    TESTS_KEEP.set(false);
}

/// Review 6 T3, the daemon's half of D2: a put counted and not yet published (another
/// thread's, mid-way, held here with [`blaze_cas::Writer::hold_put`]) is waited for at the
/// gate though no commit takes it, and once it ends, with nothing else in flight, the job
/// answers within a few [`RECHECK`]s. The gate's flush on every visit drains it, and the
/// recheck deadline is what wakes the loop: without either, the job sits until the bound
/// ([`DURABLE_WITHIN`]) and fails.
#[test]
fn a_put_counted_and_not_yet_published_is_waited_for_and_answered_soon_after_it_ends() {
    let mut sim = Sim::new(267, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 267);
    let mut writer = daemon.store_mut().writer();
    let held = writer.hold_put();
    let mut client = dial(&mut daemon);
    let start = daemon.driver().now();
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    turns(&mut daemon, &mut client, 3000);
    assert!(daemon.store_deadline().is_some(), "the job waits at its gate: {:#?}", client.of(id));
    for _ in 0..20 {
        daemon.driver().advance(RECHECK);
        turns(&mut daemon, &mut client, 5);
    }
    assert!(!client.done(id), "answered while a put it could have read is not published: {:#?}", client.of(id));
    let ran = daemon.driver().now() - start;
    assert!(ran < DURABLE_WITHIN / 2, "the wait ran {ran:?} of the clock while held");
    // The put ends with nothing else of the store's in flight: only the gate's recheck wakes
    // the loop, and only its flush takes the put.
    let ended = daemon.driver().now();
    drop(held);
    for _ in 0..10 {
        if client.done(id) {
            break;
        }
        daemon.driver().advance(RECHECK);
        turns(&mut daemon, &mut client, 5);
    }
    let got = client.of(id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "not answered soon after the put ended: {got:#?}");
    let took = daemon.driver().now() - ended;
    assert!(took <= RECHECK * 10, "answered {took:?} after the put ended");
    drop(writer);
}

/// Review 6 T4: the store breaking while a job is still running (its tests' run going, so not
/// at its gate) answers that job naming the daemon's store, as soon as its run is killed;
/// not a hangup, nor an answer only once the run ends of itself.
///
/// The store is broken in the turn after the run starts: the simulation's clock jumps to the
/// next thing in flight once nothing is ready, so as soon as the loop goes idle with only the
/// run's timer left, the run ends, however long its timer.
#[test]
fn a_job_still_running_when_the_store_breaks_is_answered_naming_the_store() {
    TESTS_KEEP.set(true);
    KEEPS_FOR.set(Duration::from_secs(3600));
    let mut sim = Sim::new(268, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 268);
    let mut client = dial(&mut daemon);
    let request =
        Request::Test { view: ViewRef::Head("/w".into()), targets: agent(), filter: Vec::new(), all: false, args: Vec::new(), ub: None };
    let started = KEEPS_STARTED.get();
    let id = client.send(&mut daemon, request);
    turn_until(&mut daemon, |daemon| {
        client.take(daemon);
        KEEPS_STARTED.get() > started || client.done(id)
    });
    let start = daemon.driver().now();
    assert!(!client.done(id), "the tests' run is still going: {:#?}", client.of(id));
    assert!(daemon.store_deadline().is_none(), "the job is not at its gate");
    assert!(daemon.store().broken().is_none(), "the store has not broken yet");
    daemon.driver().fail_syncs(store_dir());
    let key = blaze_cas::Key(ContentId::of("blaze serve tests store breaks", b"a put"));
    daemon.store_mut().put(key, blaze_cas::Kind::Query, b"a put whose commit fails\n".to_vec()).unwrap();
    daemon.flush_store_for_test();
    for _ in 0..200 {
        if client.done(id) {
            break;
        }
        daemon.turn();
        client.take(&mut daemon);
    }
    KEEPS_FOR.set(Duration::ZERO);
    TESTS_KEEP.set(false);
    assert!(daemon.store().broken().is_some(), "the store broke");
    let took = daemon.driver().now() - start;
    let got = client.of(id);
    assert!(took < Duration::from_secs(3600), "not answered while its run was still going ({took:?} on the clock): {got:#?}");
    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");
    let named = |it: &Response| {
        matches!(it, Response::Error(error) if error.what.contains("the daemon's store")
            && error.what.contains("Input/output error"))
    };
    assert!(got.iter().any(named), "{got:#?}");
}

/// Review C1/T1: a client that stopped reading while its answer is being sent holds a stop
/// open only for [`LAST_EFFECTS`](super::daemon::LAST_EFFECTS). Then the daemon hangs up on it,
/// names the connection, the bytes it never got and the request they answer, and stops.
/// Before, the stop ended at the bound with the connection left open and nothing said.
#[test]
fn a_stop_hangs_up_on_a_client_that_stopped_reading_once_its_last_sends_outlast_the_bound() {
    let sim = Sim::new(73, Faults::default());
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 73);
    let mut client = dial(&mut daemon);
    let first = client.send(&mut daemon, Request::Status);
    client.wait(&mut daemon, first);
    daemon.driver().hold_sends(client.conn);
    let id = client.send(&mut daemon, Request::Status);
    turn_until(&mut daemon, |daemon| daemon.driver().held_sends() > 0);
    let asked = daemon.driver().now();
    daemon.stop();
    // The simulation's clock jumps only to what will end; a send held for ever will not, so
    // the test lets time pass as it would for a real daemon.
    let mut turns = 0;
    while daemon.turn() {
        daemon.driver().advance(Duration::from_millis(1));
        turns += 1;
        assert!(turns < TURNS, "the daemon never stopped");
    }
    let took = daemon.driver().now() - asked;
    let bound = super::daemon::LAST_EFFECTS;
    assert!(took > bound && took < bound + Duration::from_secs(1), "stopped {took:?} after it was asked; its bound for last sends is {bound:?}");
    assert!(daemon.driver().peer_hung_up_on(client.conn), "the stop left the client that stopped reading connected");
    assert_eq!(daemon.driver().held_sends(), 0, "the hang-up ends the held send");
    client.take(&mut daemon);
    assert!(!client.done(id), "a held send reached the client: {:?}", client.of(id));
    let abandoned = daemon.abandoned().expect("the stop names the client it gave up on");
    assert_eq!(abandoned.conns.len(), 1, "{abandoned}");
    let lost = abandoned.conns[0];
    assert_eq!((lost.conn, lost.last_request), (client.conn, Some(id)), "{abandoned}");
    assert!(lost.bytes > 0 && abandoned.unsent_bytes == lost.bytes, "{abandoned}");
}

/// Review W1: only the commit's own steps are its progress. With its syncs held, the store
/// still answers other things (placements, here, every quarter of the bound, up to and in the
/// tick of the bound itself), and the build fails once the commit has stood still for
/// [`DURABLE_WITHIN`] all the same, and not before. A rule that counted any completion of the
/// store kept it waiting for ever.
#[test]
fn a_hung_commit_fails_at_the_bound_though_the_store_answers_placements_meanwhile() {
    let mut sim = Sim::new(198, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 198);
    let mut client = dial(&mut daemon);
    daemon.driver().hold_syncs(store_dir());
    let start = daemon.driver().now();
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    turns(&mut daemon, &mut client, 3000);
    assert!(!client.done(id), "answered with what it left not durable: {:#?}", client.of(id));
    assert!(daemon.driver().held_syncs() > 0, "the store's commit is held");
    let ran = daemon.driver().now() - start;
    assert!(ran < Duration::from_secs(1), "the build took {ran:?} of the clock before the advance");
    let deadline = daemon.store_deadline().expect("the job waits at its gate");
    assert!(deadline >= start + DURABLE_WITHIN, "the job's next deadline {deadline:?} is no bound from {start:?}");
    let mut placed = 0;
    for quarter in 0..12 {
        if client.done(id) {
            break;
        }
        daemon.driver().advance(DURABLE_WITHIN / 4);
        assert!(daemon.place_for_test(&kept_key("/w", 0), name(&format!("placed/{quarter}"))).is_some(), "a placement asked");
        turns(&mut daemon, &mut client, 50);
        placed += 1;
    }
    // The wait began within the opening turns, under a second in, so the bound falls on the
    // fourth quarter: the placement asked then answers in the bound's own tick, dispatched
    // before the turn's expiry judges the wait (`>=` in `gave_up`), and the build fails there.
    assert!(placed >= 4, "the store answered placements every quarter up to the bound ({placed} quarters)");
    let got = client.of(id);
    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");
    let named = |it: &Response| matches!(it, Response::Error(error) if error.what.contains("answered nothing") && error.what.contains("store ticket"));
    assert!(got.iter().any(named), "{got:#?}");
    let now = daemon.driver().now();
    assert!(now >= deadline, "failed at {now:?}, before the bound at {deadline:?}");
    let took = now - start;
    assert!(took < DURABLE_WITHIN * 2, "failed at {took:?}, not at the bound");
}

/// Review C1 and W3, at the boundary (`slow_writes` and `release_held` set to the deadline,
/// [`DURABLE_WITHIN`] from the wait's start): the commit's held syncs are let go to answer
/// exactly at the bound, in the same tick as placements, other store traffic, that wind the jobs up
/// as they are dispatched. Only [`Jobs::expire`](super::job::Jobs::expire), after every
/// completion of the turn, judges the time, so the step that answered counts and the build
/// succeeds. Judged inside a placement's dispatch (when it comes first; the seed shuffles
/// them) or before the turn's completions, it failed.
#[test]
fn a_commit_step_that_answers_at_the_bound_counts_whatever_else_answers_then() {
    // Seeds on which a placement was told before the commit's sync in the tick of the
    // deadline: the order a judge inside a placement's dispatch gets wrong.
    let mut placement_first = 0;
    for seed in 240..248 {
        let mut sim = Sim::new(seed, Faults::default());
        checkout(&mut sim, "[package]\nname = \"app\"\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        let mut client = dial(&mut daemon);
        daemon.driver().hold_syncs(store_dir());
        let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
        turns(&mut daemon, &mut client, 3000);
        assert!(!client.done(id), "seed {seed}: answered with what it left not durable: {:#?}", client.of(id));
        assert!(daemon.driver().held_syncs() > 0, "seed {seed}: the store's commit is held");
        // The syncs and the placements all answer at the deadline itself: the bound from the
        // tick the wait began, less what the clock has run since.
        let deadline = daemon.store_deadline().expect("a job waits for the store");
        let after = deadline - daemon.driver().now();
        assert!(after > DURABLE_WITHIN / 2 && after <= DURABLE_WITHIN, "seed {seed}: the wait's deadline is {after:?} away");
        daemon.driver().record(vec![store_dir(), name("placed")]);
        daemon.driver().slow_writes(name("placed"), after);
        daemon.driver().release_held(after);
        for at in 0..4 {
            assert!(daemon.place_for_test(&kept_key("/w", 0), name(&format!("placed/{at}"))).is_some(), "a placement asked");
        }
        let got = client.wait(&mut daemon, id);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
        let recorded = daemon.driver().recorded();
        let placed = name("placed");
        let sync_at = recorded.iter().position(|(_, file, sync)| *sync && file.is_under(&store_dir()));
        let sync_at = sync_at.unwrap_or_else(|| panic!("seed {seed}: no sync of the store was told: {recorded:?}"));
        let tick = recorded[sync_at].0;
        assert_eq!(tick, deadline, "seed {seed}: the released sync was told at {tick:?}, not at the deadline");
        let before = recorded[..sync_at].iter().any(|(at, file, sync)| *at == tick && !*sync && file.is_under(&placed));
        placement_first += usize::from(before);
    }
    assert!(placement_first > 0, "no seed told a placement before the commit's sync in the deadline's tick: the test tests nothing");
}

/// Bounded progress: a store that breaks (its syncs fail until it gives up) never makes a
/// build's puts durable, and the build fails at once naming why, not at the bound; the
/// daemon then stops, failed naming its store.
#[test]
fn a_build_whose_store_breaks_fails_naming_why() {
    let mut sim = Sim::new(195, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 195);
    let mut client = dial(&mut daemon);
    daemon.driver().fail_syncs(store_dir());
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert!(daemon.store().broken().is_some(), "the store broke");
    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");
    let named = |it: &Response| {
        matches!(it, Response::Error(error) if error.what.contains("the build of /w")
            && error.what.contains("Input/output error"))
    };
    assert!(got.iter().any(named), "{got:#?}");
    // And the daemon stops, naming its store, as it does for the shared one: the next
    // request starts a fresh daemon rather than build on a store that can't commit.
    let failed = daemon.failed().map(ToString::to_string);
    assert!(failed.as_deref().is_some_and(|it| it.contains("the daemon's store") && it.contains("Input/output error")), "{failed:?}");
}

/// Review round 2, C1 and C2: what user code run by the job put (here the tests' run keeping
/// how they ran, through `Runs::take_puts` as `UnitRuns` forwards it) is durable before the
/// job answers, so a crash after the answer keeps it. Across seeds, as a crash keeps a random
/// prefix of what was not synced.
#[test]
fn what_a_run_put_is_durable_once_its_job_answers() {
    TESTS_KEEP.set(true);
    for seed in 200..216 {
        let mut sim = Sim::new(seed, Faults::default());
        checkout(&mut sim, "[package]\nname = \"app\"\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        let mut client = dial(&mut daemon);
        let request =
            Request::Test { view: ViewRef::Head("/w".into()), targets: agent(), filter: Vec::new(), all: false, args: Vec::new(), ub: None };
        let id = client.send(&mut daemon, request);
        let got = client.wait(&mut daemon, id);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
        assert!(daemon.store().get(&test_record_key()).unwrap().is_some(), "seed {seed}: the run kept nothing");
        let mut sim = daemon.into_driver();
        sim.crash();
        let daemon = start(sim, &steps, seed + 1000);
        let kept = daemon.store().get(&test_record_key()).unwrap().is_some_and(|it| it.bytes == TEST_RECORD);
        assert!(kept, "seed {seed}: the job answered and a crash lost what its run put");
    }
    TESTS_KEEP.set(false);
}

/// What a build of `/w` gives that a restart must not change: whether it is done, and the
/// bytes of its program and of each value it left for the store.
fn outcome(daemon: &mut SimDaemon, got: &[Response]) -> (Option<Response>, Vec<u8>, Vec<Option<Vec<u8>>>) {
    let app = daemon.driver().contents(&name(&APP_AT[1..]));
    let keys = [kept_key("/w", 0), kept_key("/w", 1), shared_kept_key()];
    let kept = keys.iter().map(|key| daemon.store().get(key).unwrap().map(|it| it.bytes.to_vec())).collect();
    (got.last().cloned(), app, kept)
}

/// Rule 23, at every point: two checkouts build at once, so one's answer may rest on what the
/// other put and has not made durable (a value read from the store carries no ticket); the
/// daemon crashes after turn `k`, for every `k` up to both answering. After each crash, every
/// checkout whose `Done` the client saw still has all it left in the store (durable before it
/// answered), and a build of `/w` on the restored disk gives byte for byte what a fresh
/// daemon's gives: no torn or half-kept value is refused as a mismatch, and what was lost is
/// made again. `Sim::crash` keeps a seeded prefix of each file's unsynced writes; it does not
/// model directory entries (lane storemodel's `CrashFs` does).
#[test]
fn a_crash_after_any_turn_keeps_what_was_answered_and_restores_to_a_fresh_build() {
    OUTPUT.set(true);
    let fresh = {
        let mut sim = Sim::new(140, Faults::default());
        checkout(&mut sim, "[package]\nname = \"app\"\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, 140);
        let mut client = dial(&mut daemon);
        let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
        let got = client.wait(&mut daemon, id);
        outcome(&mut daemon, &got)
    };
    assert_eq!(fresh.0, Some(Response::Done { ok: true }));
    let seed = 141;
    let begin = |steps: &Arc<Fake>| {
        let mut sim = Sim::new(seed, Faults::default());
        checkout(&mut sim, "[package]\nname = \"app\"\n");
        second_checkout(&mut sim);
        let mut daemon = start(sim, steps, seed);
        let mut client = dial(&mut daemon);
        let builds = ["/w", "/v"].map(|head| Request::Build { view: ViewRef::Head(head.into()), targets: agent() });
        let ids = client.send_together(&mut daemon, builds.into());
        (daemon, client, ids)
    };
    // The turns an uncrashed run takes until both answer: the crash points.
    let turns = {
        let steps = Arc::new(Fake::default());
        let (mut daemon, mut client, ids) = begin(&steps);
        let mut turns = 0;
        while !ids.iter().all(|id| client.done(*id)) {
            assert!(turns < TURNS, "the builds never ended");
            daemon.turn();
            client.take(&mut daemon);
            turns += 1;
        }
        turns
    };
    // Crash points where a checkout had answered, so the check that it kept what it left
    // bites; and of those, ones where the other had not, so one's answer may rest on the
    // other's puts not yet durable.
    let (mut answered_any, mut answered_one) = (0, 0);
    for k in 0..=turns {
        let steps = Arc::new(Fake::default());
        let (mut daemon, mut client, ids) = begin(&steps);
        for _ in 0..k {
            daemon.turn();
            client.take(&mut daemon);
        }
        let answered: Vec<&str> = ["/w", "/v"]
            .into_iter()
            .zip(&ids)
            .filter(|(_, id)| client.of(**id).last() == Some(&Response::Done { ok: true }))
            .map(|(head, _)| head)
            .collect();
        answered_any += usize::from(!answered.is_empty());
        answered_one += usize::from(answered.len() == 1);
        let mut sim = daemon.into_driver();
        sim.crash();
        let mut daemon = start(sim, &steps, seed + 1000 + k as u64);
        for head in &answered {
            assert!(holds_kept(daemon.store(), head), "crash after turn {k}: {head} answered, and lost what it left");
        }
        let mut client = dial(&mut daemon);
        let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
        let got = client.wait(&mut daemon, id);
        assert_eq!(outcome(&mut daemon, &got), fresh, "crash after turn {k}");
    }
    assert!(answered_any > 0, "no crash point came after an answer, of {turns} turns: the test checks nothing kept");
    assert!(
        answered_one > 0,
        "no crash point came after one checkout answered and before the other did, of {turns} turns: the race it is for \
         was never crashed in"
    );
    eprintln!("crash points: {} in all, {answered_any} after an answer, {answered_one} after one alone", turns + 1);
    OUTPUT.set(false);
}

/// A build of a checkout asked while the checkout's last front end is being stored waits for
/// the store (its lowering held, named in a stall) and lowers into the front end it stored,
/// rather than lowering anew beside it. An inline run is over as it is taken, the next turn
/// after the store is posted, long before a second build's view is ready, so the store's task
/// is held on the runner ([`Daemon::hold_tasks_for_test`]), as a large front end's store runs
/// on a worker, and let go once the second build's lowering is seen waiting on it. Across
/// seeds, for the order of everything else.
#[test]
fn a_build_asked_while_its_front_end_is_stored_waits_and_lowers_into_it() {
    for seed in 150..170 {
        let mut sim = Sim::new(seed, Faults::default());
        checkout(&mut sim, "[package]\nname = \"app\"\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        daemon.hold_tasks_for_test(blaze_graph::admission::kinds::STORE_KEPT, true);
        let mut client = dial(&mut daemon);
        let first = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
        turn_until(&mut daemon, |daemon| {
            client.take(daemon);
            daemon.storing() > 0 || client.done(first)
        });
        assert!(!client.done(first), "seed {seed}: answered before its front end was stored: {:#?}", client.of(first));
        let (anew, into_kept) = (ANEW.get(), INTO_KEPT.get());
        let second = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
        // Let go in the turn it is seen: with the store and the lowering both waiting, nothing
        // else is in flight, and the simulation's clock would jump to the stall's timer.
        turn_until(&mut daemon, |daemon| {
            client.take(daemon);
            let held = daemon.held_lowerings().iter().any(|it| it.checkout == "/w" && it.on.is_some());
            held || client.done(second)
        });
        assert!(!client.done(second), "seed {seed}: the second build never waited for the store: {:#?}", client.of(second));
        assert!(daemon.storing() > 0, "seed {seed}: the store ended while held");
        daemon.hold_tasks_for_test(blaze_graph::admission::kinds::STORE_KEPT, false);
        turn_until(&mut daemon, |daemon| {
            client.take(daemon);
            client.done(first) && client.done(second)
        });
        assert_eq!(client.of(first).last(), Some(&Response::Done { ok: true }), "seed {seed}");
        assert_eq!(client.of(second).last(), Some(&Response::Done { ok: true }), "seed {seed}");
        assert_eq!((ANEW.get(), INTO_KEPT.get()), (anew, into_kept + 1), "seed {seed}: the second lowers into the stored front end");
    }
}

/// After a restart the front end the last build kept is gone from memory: the first build
/// lowers anew, and reads from the store all the last build left for it, rather than working
/// it out again (the fakes' lowering counts the reads).
#[test]
fn after_a_restart_the_first_lowering_reads_what_the_last_build_stored() {
    for (seed, crash) in [(171, false), (172, true), (173, true)] {
        let mut sim = Sim::new(seed, Faults::default());
        checkout(&mut sim, "[package]\nname = \"app\"\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        let mut client = dial(&mut daemon);
        let reads = KEPT_READS.get();
        let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
        assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }));
        assert_eq!(KEPT_READS.get(), reads, "the first build of a fresh daemon finds nothing stored");
        let mut daemon = match crash {
            false => restart(daemon, &steps, seed + 1000),
            true => {
                let mut sim = daemon.into_driver();
                sim.crash();
                start(sim, &steps, seed + 1000)
            }
        };
        let mut client = dial(&mut daemon);
        let anew = ANEW.get();
        let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
        assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }));
        assert_eq!(ANEW.get(), anew + 1, "a restarted daemon lowers anew");
        assert_eq!(KEPT_READS.get(), reads + 1, "and reads what the last build stored (crash: {crash})");
    }
}

/// The head's next build lowers into the front end its last one kept and stored, and each
/// build stores what it left: lowering never writes the last build's results.
#[test]
fn a_build_lowers_into_the_front_end_its_last_build_stored() {
    let mut sim = Sim::new(75, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 75);
    let mut client = dial(&mut daemon);
    let (anew, into_kept, stores) = (ANEW.get(), INTO_KEPT.get(), STORES.get());
    for _ in 0..2 {
        let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
        assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }));
    }
    assert_eq!((ANEW.get(), INTO_KEPT.get()), (anew + 1, into_kept + 1), "the second build lowers into the first's");
    assert_eq!(STORES.get(), stores + 2, "each build stores what it left");
}

#[test]
fn a_run_reports_what_ran_after_built() {
    let mut sim = Sim::new(8, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 8);
    let mut client = dial(&mut daemon);
    let id = client.send(
        &mut daemon,
        Request::Run { view: ViewRef::Head("/w".into()), targets: agent(), bin: None, args: Vec::new() },
    );
    let got = client.wait(&mut daemon, id);
    let at = |what: fn(&Response) -> bool| got.iter().position(what);
    let built = at(|it| matches!(it, Response::Built { .. }));
    let ran = at(|it| matches!(it, Response::Ran { code: Some(0), .. }));
    let worked = at(|it| matches!(it, Response::Work(_)));
    assert!(built.is_some() && built < ran && ran < worked, "{got:#?}");
    assert_eq!(got.last(), Some(&Response::Done { ok: true }));
}

#[test]
fn a_pack_is_made_once_its_check_is_built_with_every_scripts_run_applied() {
    let mut sim = Sim::new(41, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 41);
    let mut client = dial(&mut daemon);
    let pack = Request::Pack { view: ViewRef::Head("/w".into()), targets: agent(), program: "app".into() };
    let id = client.send(&mut daemon, pack);
    let got = client.wait(&mut daemon, id);
    let built = got.iter().position(|it| matches!(it, Response::Built { .. }));
    let packed = got.iter().position(|it| matches!(it, Response::Packed { .. }));
    assert!(built.is_some() && built < packed, "`Built`, then `Packed`: {got:#?}");
    assert!(got.contains(&Response::Packed { pack: b"app [2]".to_vec() }), "app's build script ran first: {got:#?}");
    assert_eq!(got.last(), Some(&Response::Done { ok: true }));

    let id = client.send(&mut daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
    assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }), "the head builds after a pack");
}

#[test]
fn the_documentation_is_made_once_its_check_is_built_with_every_scripts_run_applied() {
    let mut sim = Sim::new(42, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 42);
    let mut client = dial(&mut daemon);
    let doc = Request::Doc { view: ViewRef::Head("/w".into()), targets: agent(), no_deps: true, private: false };
    let id = client.send(&mut daemon, doc);
    let got = client.wait(&mut daemon, id);
    let at = |what: fn(&Response) -> bool| got.iter().position(what);
    let built = at(|it| matches!(it, Response::Built { .. }));
    let documented = at(|it| matches!(it, Response::Documented(_)));
    let page = at(|it| matches!(it, Response::DocPage { .. }));
    assert!(built.is_some() && built < documented && documented < page, "`Built`, `Documented`, then its pages: {got:#?}");
    let made = Response::DocPage { path: "app/index.html".into(), page: b"true false [2]".to_vec() };
    assert!(got.contains(&made), "app's build script ran first, and the request's flags came through: {got:#?}");
    assert_eq!(got.last(), Some(&Response::Done { ok: true }));

    let id = client.send(&mut daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
    let after = client.wait(&mut daemon, id);
    assert_eq!(after.last(), Some(&Response::Done { ok: true }), "the head builds after its documentation");
}

#[test]
fn tests_under_the_evaluator_are_each_evaluated_and_one_failing_fails_the_job() {
    let mut sim = Sim::new(9, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 9);
    let mut client = dial(&mut daemon);
    let test = |filter: &str| Request::Test {
        view: ViewRef::Head("/w".into()),
        targets: agent(),
        filter: vec![filter.to_owned()],
        all: true,
        args: Vec::new(),
        ub: Some(Ub { seed: Some(3), schedules: None }),
    };
    let results = |got: &[Response]| -> Vec<(String, TestOutcome)> {
        let mut results: Vec<_> = got
            .iter()
            .filter_map(|it| match it {
                Response::TestResult(result) => Some((result.name.clone(), result.outcome)),
                _ => None,
            })
            .collect();
        results.sort_by(|a, b| a.0.cmp(&b.0));
        results
    };
    let id = client.send(&mut daemon, test("a::"));
    let got = client.wait(&mut daemon, id);
    assert_eq!(
        results(&got),
        vec![("a::adds".to_owned(), TestOutcome::Passed), ("a::overflows".to_owned(), TestOutcome::Failed)],
        "{got:#?}"
    );
    let failed = got.iter().find_map(|it| match it {
        Response::TestResult(result) if result.outcome == TestOutcome::Failed => Some(result.output.clone()),
        _ => None,
    });
    assert_eq!(failed.as_deref(), Some(&b"undefined behavior\nseed: 3\n"[..]));
    assert_eq!(got.last(), Some(&Response::Done { ok: false }));
    let id = client.send(&mut daemon, test("b::"));
    let got = client.wait(&mut daemon, id);
    assert_eq!(results(&got), vec![("b::reads".to_owned(), TestOutcome::Passed)], "{got:#?}");
    assert_eq!(got.last(), Some(&Response::Done { ok: true }));
}

#[test]
fn a_build_whose_load_fails_before_its_units_ends_not_ok_and_the_daemon_answers_the_next() {
    let mut sim = Sim::new(42, Faults::default());
    checkout(&mut sim, "fail\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 42);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");
    let refused = got.iter().any(|it| matches!(it, Response::Error(error) if error.what.contains("refuses this manifest")));
    assert!(refused, "the load's error is the build's: {got:#?}");
    assert!(!got.iter().any(|it| matches!(it, Response::Built { .. })), "{got:#?}");

    daemon.driver().change(&name("w/Cargo.toml"), b"[package]\n");
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
}

/// A write to the front end that panics as it is applied on the loop, as a maintainer's did
/// (the self-build of 2026-11-01 00:17), fails the units after it with a typed error naming
/// the panic, while keep-going builds the unit that does not depend on it, and the daemon
/// answers the next request: the build script's run (unit 2) panics as it is applied, the
/// program (unit 3) is never compiled, and dep's library (unit 0) is.
#[test]
fn a_write_that_panics_fails_the_units_after_it_and_keep_going_builds_the_rest() {
    PHASES.with(|it| it.borrow_mut().clear());
    PANICS_RUN.set(Some(2));
    let mut sim = Sim::new(44, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 44);
    let mut client = dial(&mut daemon);
    let cargo = blaze_api::Cargo {
        cwd: "/w".into(),
        env: Vec::new(),
        args: vec!["--keep-going".into()],
        root: "/w".into(),
        color: false,
        files: Vec::new(),
        width: None,
        command: "build".into(),
        refused: None,
        verbose: 0,
    };
    let targets = Targets { cargo: Some(cargo), ..agent() };
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets });
    let got = client.wait(&mut daemon, id);
    PANICS_RUN.set(None);

    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");
    let named = got.iter().any(|it| {
        matches!(it, Response::Error(error) if error.what.contains("panicked")
            && error.what.contains("serve-test: the run's write panics")
            && error.what.contains("applying a build script's run"))
    });
    assert!(named, "the build's error names the panic and the write: {got:#?}");
    let ran = PHASES.with(|it| it.take());
    assert!(ran.contains(&(0, Phase::Store)), "dep's library, which needs no run, is built: {ran:?}");
    assert!(!ran.iter().any(|it| it.0 == 3), "the program, after the run, is never compiled: {ran:?}");

    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "the daemon builds the next: {got:#?}");
}

/// A unit whose compile panics, as blaze-diagnostics did on a span past its file's end (the
/// self-build of 2026-11-01 04:37), fails with the panic's text as an error of that unit, not
/// of the job, while keep-going builds its sibling, and the daemon answers the next request:
/// the build script's compile (unit 1) panics, its run (2) and the program (3) are never
/// compiled, and dep's library (unit 0) is.
#[test]
fn a_unit_whose_compile_panics_fails_with_the_panic_while_its_sibling_builds() {
    PHASES.with(|it| it.borrow_mut().clear());
    PANICS_COMPILE.set(Some(1));
    let mut sim = Sim::new(45, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 45);
    let mut client = dial(&mut daemon);
    let cargo = blaze_api::Cargo {
        cwd: "/w".into(),
        env: Vec::new(),
        args: vec!["--keep-going".into()],
        root: "/w".into(),
        color: false,
        files: Vec::new(),
        width: None,
        command: "build".into(),
        refused: None,
        verbose: 0,
    };
    let targets = Targets { cargo: Some(cargo), ..agent() };
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets });
    let got = client.wait(&mut daemon, id);
    PANICS_COMPILE.set(None);

    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");
    let said = got.iter().any(|it| {
        matches!(it, Response::Diagnostic(diagnostic) if diagnostic.level == Level::Error
            && diagnostic.message.contains("panicked")
            && diagnostic.message.contains("serve-test: the unit's compile panics"))
    });
    assert!(said, "the unit's error names the panic: {got:#?}");
    let job_failed = got.iter().any(|it| matches!(it, Response::Error(error) if error.what.contains("panicked")));
    assert!(!job_failed, "the panic fails its unit, not the job: {got:#?}");
    let ran = PHASES.with(|it| it.take());
    assert!(ran.contains(&(0, Phase::Store)), "dep's library, the sibling, is built: {ran:?}");
    assert!(!ran.iter().any(|it| it.0 == 3), "the program, after the build script, is never compiled: {ran:?}");

    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "the daemon builds the next: {got:#?}");
}

/// A unit whose compile ends in an error (no panic, no refusal) fails that unit with the error
/// naming it, not the job: in the released engine such an error (`unsupported: no
/// `lang_start``, one `#![no_std]` crate's test) ended the worker, and all 211 programs of
/// `blaze test --workspace` failed with its text. Here the build script's compile (unit 1)
/// errs, its run (2) and the program (3) are never compiled, dep's library (0) is, under
/// `--keep-going`, and the daemon answers the next request.
#[test]
fn a_unit_whose_compile_errs_fails_alone_while_its_sibling_builds() {
    PHASES.with(|it| it.borrow_mut().clear());
    ERRS_COMPILE.set(Some(1));
    let mut sim = Sim::new(47, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 47);
    let mut client = dial(&mut daemon);
    let cargo = blaze_api::Cargo {
        cwd: "/w".into(),
        env: Vec::new(),
        args: vec!["--keep-going".into()],
        root: "/w".into(),
        color: false,
        files: Vec::new(),
        width: None,
        command: "build".into(),
        refused: None,
        verbose: 0,
    };
    let targets = Targets { cargo: Some(cargo), ..agent() };
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets });
    let got = client.wait(&mut daemon, id);
    ERRS_COMPILE.set(None);

    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");
    let said = got.iter().any(|it| {
        matches!(it, Response::Diagnostic(diagnostic) if diagnostic.level == Level::Error
            && diagnostic.message.contains("no `lang_start`")
            && diagnostic.message.contains("unit 1"))
    });
    assert!(said, "the unit's error says what and where: {got:#?}");
    let job_failed = got.iter().any(|it| matches!(it, Response::Error(error) if error.what.contains("lang_start")));
    assert!(!job_failed, "the error fails its unit, not the job: {got:#?}");
    let ran = PHASES.with(|it| it.take());
    assert!(ran.contains(&(0, Phase::Store)), "dep's library, the sibling, is built: {ran:?}");
    assert!(!ran.iter().any(|it| it.0 == 3), "the program, after the build script, is never compiled: {ran:?}");

    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "the daemon builds the next: {got:#?}");
}

/// A stop (what SIGTERM asks) right after a build failed on a unit's panicking compile writes
/// the image, and the next start opens it: a caught panic leaves nothing that skips the write.
#[test]
fn a_daemon_stopped_after_a_build_failed_on_a_panic_writes_the_image_the_next_start_opens() {
    PANICS_COMPILE.set(Some(1));
    let mut sim = Sim::new(46, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 46);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    PANICS_COMPILE.set(None);
    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");

    let image = name(&format!("{STATE}/db/image"));
    let mut daemon = restart(daemon, &steps, 47);
    assert!(daemon.driver().exists(&image), "the stop wrote the failed build's image");
    let started = daemon.started();
    assert!(
        started.restored.is_some() || matches!(started.cold, Some(super::daemon::Cold::Interned)),
        "the next start opens the stop's image: {started}"
    );
    assert!(!matches!(started.cold, Some(super::daemon::Cold::NoImage)), "{started}");
}

#[test]
fn a_build_cancelled_while_nothing_of_it_runs_ends_not_ok_and_the_daemon_answers_the_next() {
    let sim = Sim::new(43, Faults::default());
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 43);
    let mut client = dial(&mut daemon);
    let base = daemon.vfs().empty();
    let ops = vec![Op::Write { path: "Cargo.toml".into(), bytes: b"[package]\n".to_vec(), mode: blaze_vfs::Mode::Regular }];
    let commit = client.send(&mut daemon, Request::Commit { head: "agents/one".into(), base, ops });
    assert!(matches!(client.wait(&mut daemon, commit).first(), Some(Response::Committed { .. })));

    // An agent's head waits for no watcher, so all three start in the turn they arrive: the
    // second build's standard library node joins the first's task, and the cancel finds
    // nothing of the second's request running.
    let build = || Request::Build { view: ViewRef::Head("agents/one".into()), targets: agent() };
    let (first, second) = (client.next, client.next + 1);
    let ids = client.send_together(&mut daemon, vec![build(), build(), Request::Cancel { id: second }]);
    let got = client.wait(&mut daemon, second);
    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");
    assert!(!got.iter().any(|it| matches!(it, Response::Built { .. })), "{got:#?}");
    assert_eq!(client.wait(&mut daemon, ids[2]), vec![Response::Done { ok: true }]);
    let got = client.wait(&mut daemon, first);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");

    let id = client.send(&mut daemon, build());
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
}

#[test]
fn each_kind_of_batch_runs_its_own_step_of_the_front_end() {
    use std::sync::atomic::AtomicBool;

    use super::{
        job::{Does, Step, Value},
        plane::{Did, Work as _},
        work::Readers,
    };

    let cancelled = Arc::new(AtomicBool::new(false));
    let front = Arc::new(FakeFront { goal: Goal::Check });
    let inputs = Arc::new(FakeInputs { runs: vec![4, 7], ..FakeInputs::default() });
    let reading = || Readers::<Fake> { this: Some(FakeReader(inputs.clone())), ..Readers::default() };
    let run = |does: Does<Fake>, readers: Readers<Fake>| match Step::of(&cancelled, does).run(0, readers) {
        Some(Did { outcome: blaze_graph::Outcome::Built, value }) => value,
        _ => panic!("a step of the fakes is built at once"),
    };
    let answered = |value: Value<Fake>| match value {
        Value::Answer(Ok(response)) => response,
        _ => panic!("the step gave no answer"),
    };
    let ub = Ub { seed: Some(5), schedules: None };

    let from = Readers::<Fake> { from: Some(FakeReader(inputs.clone())), ..reading() };
    let reached = answered(run(Does::Reach { to: front.clone(), from: front.clone(), changes: Vec::new() }, from));
    assert!(matches!(&reached, Response::Reached(Reach { changes, .. }) if changes.is_empty()), "{reached:?}");

    let codegen = answered(run(Does::CodegenInputs { front: front.clone() }, reading()));
    assert_eq!(codegen, Response::CodegenInputs(Vec::new()));

    let writer = FakeWriter { inputs: Arc::new(FakeInputs { runs: vec![1, 2, 3], ..FakeInputs::default() }), asked: Vec::new(), broke: Vec::new() };
    let tables = Does::Tables { front: front.clone(), images: Vec::new() };
    let tables = answered(run(tables, Readers { writer: Some(writer), ..Readers::default() }));
    assert_eq!(tables, Response::Tables(vec![Table { name: "run".into(), elements: 3, bytes: 0 }]));

    let writer = FakeWriter { inputs: Arc::new(FakeInputs { runs: vec![1], ..FakeInputs::default() }), asked: vec![2], broke: Vec::new() };
    let pack = Does::Pack { front: front.clone(), program: "app".into() };
    let packed = answered(run(pack, Readers { writer: Some(writer), ..Readers::default() }));
    assert_eq!(packed, Response::Packed { pack: b"app [1, 2]".to_vec() }, "a write asked for is applied first");

    let db = Arc::new(Database::new(blaze_query::Config::memory()));
    let queries = db.open_with(blaze_vfs::View { root: ContentId([0; 32]) }, b"");
    let traced = answered(run(Does::Trace { db, queries, root: Fake::default().plan_key() }, Readers::default()));
    assert_eq!(traced, Response::Trace(Vec::new()), "a view no query ran in traces nothing");

    let select = Does::Select { front: front.clone(), filter: vec!["a::".into()], args: Vec::new(), all: true, ub };
    match run(select, reading()) {
        Value::Selected(Ok(tests)) => assert_eq!(tests, ["a::adds", "a::overflows"]),
        _ => panic!("the selection gave no tests"),
    }
    let evaluate = Does::Evaluate { front: front.clone(), tests: vec!["a::overflows".to_owned()].into(), ub };
    match run(evaluate, reading()) {
        Value::Evaluated(Ok(result)) => assert_eq!((result.name.as_str(), result.outcome), ("a::overflows", TestOutcome::Failed)),
        _ => panic!("the evaluation gave no result"),
    }

    let compile = || Step::of(&cancelled, Does::Compile { front: Arc::new(FakeFront { goal: Goal::Build }), back: Default::default() });
    match compile().run(Phase::Names.arg(0), reading()) {
        Some(Did { outcome: blaze_graph::Outcome::Built, value: Value::Phase(0, images) }) => assert!(images.is_empty()),
        _ => panic!("a phase before the last gives no compile"),
    }
    match compile().run(Phase::Store.arg(0), reading()) {
        Some(Did { outcome: blaze_graph::Outcome::Built, value: Value::Compiled(0, Ok(compiled), images) }) => {
            assert!(compiled.ok && images.is_empty());
        }
        _ => panic!("the compile gave no unit"),
    }
}

/// The units each compile saw the runs of, by unit, the runs sorted.
fn seen() -> Vec<(usize, Vec<usize>)> {
    SEEN.with(|it| it.borrow().iter().map(|(unit, runs)| (*unit, sorted(runs.clone()))).collect())
}

fn sorted(mut runs: Vec<usize>) -> Vec<usize> {
    runs.sort_unstable();
    runs
}

#[test]
fn a_wave_of_build_scripts_waits_for_the_readers_before_it_and_is_one_write() {
    const SCRIPTS: usize = 5;
    let mut sim = Sim::new(10, Faults::default());
    checkout(&mut sim, &format!("wave {SCRIPTS}\n"));
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 10);
    KEEP.set(true);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    turn_until(&mut daemon, |_| ASKED.get() == SCRIPTS);
    for _ in 0..500 {
        daemon.turn();
    }
    assert_eq!(APPLIES.get(), 0, "a write waits for the reader posted before it");
    assert_eq!(seen(), vec![(0, Vec::new())], "no reading task is posted while a write waits");

    HOLD.with(|it| it.borrow_mut().clear());
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    assert_eq!(APPLIES.get(), 1, "the wave's runs are one write");
    let all: Vec<usize> = (1..=SCRIPTS).collect();
    assert_eq!(seen(), vec![(0, Vec::new()), (SCRIPTS + 1, all)], "the program reads every run");
}

#[test]
fn no_reader_posted_after_a_write_sees_the_inputs_before_it_over_many_seeds() {
    const SCRIPTS: usize = 3;
    for seed in 0..32 {
        SEEN.with(|it| it.borrow_mut().clear());
        APPLIES.set(0);
        let mut sim = Sim::new(seed, Faults::default());
        checkout(&mut sim, &format!("wave {SCRIPTS}\n"));
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        let mut client = dial(&mut daemon);
        let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
        let got = client.wait(&mut daemon, id);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
        let all: Vec<usize> = (1..=SCRIPTS).collect();
        assert_eq!(seen(), vec![(0, Vec::new()), (SCRIPTS + 1, all)], "seed {seed}");
        assert!((1..=SCRIPTS).contains(&APPLIES.get()), "seed {seed}: {} writes", APPLIES.get());
    }
}

#[test]
fn a_build_of_an_unchanged_checkout_asks_no_plan_and_lowers_into_the_last_front_end() {
    let mut sim = Sim::new(11, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 11);
    let mut client = dial(&mut daemon);
    let build = |client: &mut Client, daemon: &mut SimDaemon| {
        let id = client.send(daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
        let got = client.wait(daemon, id);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    };
    // The first build writes the lock file into the checkout, so the second's view is another.
    build(&mut client, &mut daemon);
    build(&mut client, &mut daemon);
    let (asks, anew, into_kept) = (ASKS.get(), ANEW.get(), INTO_KEPT.get());
    assert!(into_kept >= 1, "the second build lowers into the first's front end");
    build(&mut client, &mut daemon);
    assert_eq!(ASKS.get(), asks, "the view and its config are the last build's: its plan is not asked");
    assert_eq!((ANEW.get(), INTO_KEPT.get()), (anew, into_kept + 1));

    daemon.driver().change(&name("w/src/main.rs"), b"fn main() { }\n");
    build(&mut client, &mut daemon);
    assert_eq!(ASKS.get(), asks + 1, "a changed view's plan is asked");
    assert_eq!((ANEW.get(), INTO_KEPT.get()), (anew, into_kept + 2));
}

#[test]
fn an_output_is_placed_again_once_its_file_is_not_as_placed() {
    let mut sim = Sim::new(12, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 12);
    OUTPUT.set(true);
    let mut client = dial(&mut daemon);
    let app = name(&APP_AT[1..]);
    let mut build = |daemon: &mut SimDaemon| {
        let id = client.send(daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
        let got = client.wait(daemon, id);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
        assert_eq!(daemon.driver().contents(&app), APP);
        assert_eq!(daemon.driver().mode(&app).map(|it| it & 0o777), Some(0o755));
    };
    build(&mut daemon);
    build(&mut daemon);
    daemon.driver().delete(&app);
    build(&mut daemon);
    daemon.driver().change(&app, b"a file longer than the one the build placed\n");
    build(&mut daemon);
}

/// What one client asked and how it ended.
struct Asked {
    client: usize,
    id: u64,
    cancel_at: Option<usize>,
}

/// Every request answered by exactly one `Done`, nothing of it after, an `Error` only right
/// before its `Done`.
fn keeps_the_contract(got: &[(u64, Response)], ids: &[u64]) {
    for &id in ids {
        let of: Vec<&Response> = got.iter().filter(|(at, _)| *at == id).map(|(_, it)| it).collect();
        let dones = of.iter().filter(|it| matches!(it, Response::Done { .. })).count();
        assert_eq!(dones, 1, "request {id} got {dones} `Done`s: {of:#?}");
        assert!(matches!(of.last(), Some(Response::Done { .. })), "request {id} got something after its `Done`: {of:#?}");
        for (at, it) in of.iter().enumerate() {
            if matches!(it, Response::Error(_)) {
                assert_eq!(at + 2, of.len(), "request {id}'s error is not right before its `Done`: {of:#?}");
            }
        }
    }
    let known: Vec<u64> = got.iter().map(|(id, _)| *id).collect();
    assert!(known.iter().all(|it| ids.contains(it)), "a reply to a request no one made");
}

#[test]
fn every_request_ends_once_over_many_seeds() {
    for seed in 0..48 {
        let mut rng = Rng::new(seed);
        let faults = Faults { max_latency: std::time::Duration::from_millis(rng.below(4)), ..Faults::default() };
        let mut sim = Sim::new(seed, faults);
        checkout(&mut sim, "[package]\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        let mut clients = vec![dial(&mut daemon), dial(&mut daemon), dial(&mut daemon)];
        let gone = rng.below(3) as usize;
        let hang_up_at = if rng.one_in(2) { Some(rng.below(400) as usize) } else { None };
        let mut asked = Vec::new();
        for _ in 0..6 {
            let client = rng.below(3) as usize;
            let view = match rng.below(3) {
                0 => ViewRef::Head("/w".into()),
                1 => ViewRef::WhatIf {
                    base: Box::new(ViewRef::Head("/w".into())),
                    ops: vec![Op::Write { path: "Cargo.toml".into(), bytes: b"what if\n".to_vec(), mode: blaze_vfs::Mode::Regular }],
                },
                _ => ViewRef::Head("agents/none".into()),
            };
            let request = match rng.below(6) {
                0 => Request::Check { view, targets: agent() },
                1 => Request::Build { view, targets: agent() },
                2 => Request::Test { view, targets: agent(), filter: Vec::new(), all: false, args: Vec::new(), ub: None },
                3 => Request::Pack { view, targets: agent(), program: "app".into() },
                4 => Request::Doc { view, targets: agent(), no_deps: false, private: true },
                _ => Request::Run { view, targets: agent(), bin: None, args: Vec::new() },
            };
            let id = clients[client].send(&mut daemon, request);
            let cancel_at = rng.one_in(3).then(|| rng.below(300) as usize);
            asked.push(Asked { client, id, cancel_at });
        }
        let mut cancels: Vec<(usize, u64)> = Vec::new();
        let mut hung_up = false;
        for turn in 0..TURNS {
            daemon.turn();
            for it in &mut clients {
                it.take(&mut daemon);
            }
            if hang_up_at == Some(turn) && !hung_up {
                hung_up = true;
                let conn = clients[gone].conn;
                daemon.driver().peer_hangup(conn);
            }
            for it in &asked {
                if it.cancel_at == Some(turn) && !(hung_up && it.client == gone) {
                    let id = clients[it.client].send(&mut daemon, Request::Cancel { id: it.id });
                    cancels.push((it.client, id));
                }
            }
            let live = |client: usize| !(hung_up && client == gone);
            let over = asked.iter().filter(|it| live(it.client)).all(|it| clients[it.client].done(it.id))
                && cancels.iter().filter(|it| live(it.0)).all(|(client, id)| clients[*client].done(*id));
            if over && turn > hang_up_at.unwrap_or(0) && turn > 300 {
                break;
            }
        }
        for _ in 0..500 {
            daemon.turn();
            for it in &mut clients {
                it.take(&mut daemon);
            }
        }
        for (at, client) in clients.iter().enumerate() {
            if hung_up && at == gone {
                continue;
            }
            let mut ids: Vec<u64> = asked.iter().filter(|it| it.client == at).map(|it| it.id).collect();
            ids.extend(cancels.iter().filter(|it| it.0 == at).map(|it| it.1));
            keeps_the_contract(&client.got, &ids);
        }
        turn_until(&mut daemon, |it| it.settled());
    }
}

/// Daemon B started on A's state while A still ends (writing its image): B says it waits, does
/// not start while A holds the state, and starts as A ends, naming its own pid in the lock.
#[cfg(unix)]
#[test]
fn a_daemon_started_while_the_one_before_it_ends_waits_for_it_and_then_holds_the_state() {
    use std::io::Read as _;
    let state = tempfile::tempdir().unwrap();
    let a = super::hold(state.path(), None).unwrap();
    let lock = state.path().join("serve.lock");
    assert_eq!(std::fs::read_to_string(&lock).unwrap(), std::process::id().to_string());
    let (mut heard, said) = std::io::pipe().unwrap();
    let said: std::os::fd::OwnedFd = said.into();
    let mut said = std::fs::File::from(said);
    let path = state.path().to_owned();
    let b = std::thread::spawn(move || super::hold(&path, Some(&mut said)).map(drop));
    let mut byte = [0u8; 1];
    heard.read_exact(&mut byte).unwrap();
    assert_eq!(super::Phase::of(byte[0]), Some(super::Phase::Waiting));
    assert!(!b.is_finished(), "B started while A still held the state");
    drop(a);
    b.join().unwrap().unwrap();
    assert_eq!(std::fs::read_to_string(&lock).unwrap(), std::process::id().to_string());
}

#[test]
fn serve_takes_activated_alone_and_names_it_among_its_flags() {
    let parse = |args: &[&str]| super::parse(args.iter().map(std::ffi::OsString::from));
    let args = parse(&["--activated", "--socket", "/run/b.sock"]).unwrap();
    assert!(args.activated);
    assert_eq!(args.socket.as_deref(), Some(std::path::Path::new("/run/b.sock")));
    assert!(!parse(&[]).unwrap().activated);
    let refused = parse(&["--activated=yes"]).err().unwrap().to_string();
    assert!(refused.contains("`--activated` and `--ready-fd N`, not `--activated=yes`"), "{refused}");
}

#[test]
fn status_says_the_daemons_pid() {
    let mut sim = Sim::new(32, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 32);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Status);
    let got = client.wait(&mut daemon, id);
    let pid = got.iter().find_map(|it| match it {
        Response::Status(status) => Some(status.pid),
        _ => None,
    });
    assert_eq!(pid, Some(std::process::id()), "{got:#?}");
}
