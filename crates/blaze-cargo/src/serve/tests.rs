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
    ApiError, Artifact, ArtifactKind, CrateType, Diagnostic, Envelope, FetchVerb, Level, Reach,
    Request, Response, SCHEMA, Table, TargetKind, Targets, TestOutcome, TestResult, Ub, UnitOf,
    UnitProfile, UnitTarget, Verb, ViewRef,
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
        BuildConfig, Cbor, Command, Fetch, FetchKind, Invocation, LoadError, LockFile, LockWrite,
        Origin, PackagePlan, Planned, TestSandbox, UnitPlan, Units,
    },
    schema::{Mode, Profile, Strip, Stripped, Target, Unit, UnitDep, UnitGraph},
};
use blaze_vfs::{Change, Op, ViewId};

use super::{
    Daemon, Options,
    job::{DURABLE_WITHIN, ENDED_LOST, ENDED_ROWS, RECHECK},
    plane::{Inline, Runner},
    work::{
        Broke, Compiled, Failed, Front, FrontEnd, Image, JobInput, JobKind, Kept, KeptDb, KeptPut,
        Library, Output, Phase, Put, RunPuts, Runs, Selection, Source, SourceFile, Steps, Writer,
        applying_fallible,
    },
};

const SOCKET: &str = "run/blaze/v2.sock";
/// The socket of a second home's daemon on the same machine.
const OTHER_SOCKET: &str = "run/blaze/other.sock";
const STATE: &str = "cache/blaze/v2";
/// [`Options::std`]: the machine's, outside every state directory.
const STD: &str = "cache/blaze/std/0b";
const FINISHED: &str = "`dev` profile [unoptimized + debuginfo]";
const LIBRARY: &str = "/rustc/0123abcd/library";
const DEP: &str = "/home/u/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/dep-1.0.0";
const INDEX: &str =
    "/home/u/.cargo/registry/index/index.crates.io-1949cf8c6b5b557f/.cache/de/p/dep";
/// How many turns a test gives the daemon before it says it hangs.
const TURNS: usize = 100_000;

/// How long [`Client::wait_for_the_pool`] turns the daemon while the process's worker pool runs
/// other tests' jobs before it says what it waited for.
const POOL_WAIT: Duration = Duration::from_secs(120);

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

thread_local! {
    /// The crates the fakes' writer gives the front end's carry ([`Writer::view_crates`]):
    /// none but for a test of the carry.
    static CARRY_CRATES: std::cell::RefCell<Vec<crate::serve::front_carry::CrateRoot>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// How many times the load ran on this thread (tasks run inline, on the test's).
    static PLANS: Cell<usize> = const { Cell::new(0) };
    /// The configuration of each run of the load.
    static SAW: RefCell<Vec<BuildConfig>> = const { RefCell::new(Vec::new()) };
    /// How many build scripts' runs were asked of a writer.
    static ASKED: Cell<usize> = const { Cell::new(0) };
    /// Whether a build script's run is [`Lost`].
    static LOSES_RUNS: Cell<bool> = const { Cell::new(false) };
    /// Whether every fetch fails, as each does in a blaze built without `net`.
    static FAILS_FETCHES: Cell<bool> = const { Cell::new(false) };
    /// How many times a writer applied what was asked of it.
    static APPLIES: Cell<usize> = const { Cell::new(0) };
    /// The build script's run whose write panics as it is applied, as a maintainer's would.
    static PANICS_RUN: Cell<Option<usize>> = const { Cell::new(None) };
    /// The build script's run whose write fails with an error as it is applied, as a real
    /// one's does on a proc macro it cannot load.
    static FAILS_RUN: Cell<Option<usize>> = const { Cell::new(None) };
    /// Each build script's run that reached the runner ([`Front::script`]), by its unit.
    static SCRIPTS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
    /// The unit whose compile panics, as a bug of blaze's would.
    static PANICS_COMPILE: Cell<Option<usize>> = const { Cell::new(None) };
    /// The unit whose compile ends in an error that is no refusal, as an `Unsupported` codegen
    /// error of the 2026-10-31 `blaze test --workspace` did.
    static ERRS_COMPILE: Cell<Option<usize>> = const { Cell::new(None) };
    /// Set, unit 3's store saves a body entries' session of its crate `app` ([`body_session`]),
    /// a newer one each save, as a real unit's serve of its bodies does.
    static BODY_SESSIONS: Cell<bool> = const { Cell::new(false) };
    /// How many body entries' sessions the fakes saved.
    static BODY_SAVES: Cell<u8> = const { Cell::new(0) };
    /// Each lowering's `JobInput::body_marks`, in order.
    static BODY_MARKS_SEEN: RefCell<Vec<BTreeMap<String, blaze_cas::Key>>> = const { RefCell::new(Vec::new()) };
    /// Each compile's unit, and the runs applied in the front end it read.
    static SEEN: RefCell<Vec<(usize, Vec<usize>)>> = const { RefCell::new(Vec::new()) };
    /// Each phase of a unit's compile as it ran, in order.
    static PHASES: RefCell<Vec<(usize, Phase)>> = const { RefCell::new(Vec::new()) };
    /// Each reach's runs applied in the front ends it read: its view's from, then its to.
    static REACHED: RefCell<Vec<(Vec<usize>, Vec<usize>)>> = const { RefCell::new(Vec::new()) };
    /// Each edit's and each build ahead's choice's runs applied in the front end it read.
    static EDITED: RefCell<Vec<Vec<usize>>> = const { RefCell::new(Vec::new()) };
    static AHEAD_READ: RefCell<Vec<Vec<usize>>> = const { RefCell::new(Vec::new()) };
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
    /// How many kept front ends' members were stored, the second store of one
    /// (`Jobs::store_members`): as many as members a restart reads from pages.
    static MEMBER_STORES: Cell<usize> = const { Cell::new(0) };
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
    /// Whether the program's compile gives [`APP`] as its output at [`APP_AT`], the file its
    /// artifact names ([`app_artifact`]): what the build's answer waits for.
    static OUTPUT: Cell<bool> = const { Cell::new(false) };
    /// Whether the program's compile also gives [`SIDE`] at [`SIDE_AT`], a file its artifact
    /// does not name (as a program's hashed copy under `deps`): placed after the answer.
    static SIDE_OUTPUT: Cell<bool> = const { Cell::new(false) };
    /// Whether the program's compile warns twice, as its unit.
    static WARNS: Cell<bool> = const { Cell::new(false) };
    /// Whether the fake load says no warning: by default it warns its manifest's text, which
    /// every answer then says (and which the thin client hands to the engine to print).
    static QUIET_LOAD: Cell<bool> = const { Cell::new(false) };
    /// Whether a writer's apply fails as a whole, an error of the writer's own that no unit's
    /// write owns: it ends the job.
    static FAILS_APPLY: Cell<bool> = const { Cell::new(false) };
    /// The unit whose compile gives [`SCRIPT`] as its output at [`SCRIPT_AT`]: a build
    /// script's program, which its run waits to be placed.
    static SCRIPT_OUTPUT: Cell<Option<usize>> = const { Cell::new(None) };
    /// The build script's run that ends this long after it starts, on a timer of its own.
    static SLOW_SCRIPT: Cell<Option<(usize, Duration)>> = const { Cell::new(None) };
    /// Whether the first unit's compile shares [`SHARED`] in the standard library's shared
    /// store, as a std unit made here does.
    static SHARES: Cell<bool> = const { Cell::new(false) };
    /// Where the fakes' kept front end puts [`SEALED_RECORD`], the front-end record of the
    /// sealed crate unit 0 is (a registry crate's, as `deps::store` routes it): `None` puts
    /// none of it.
    static SEALED_INTO: Cell<Option<crate::serve::work::PutInto>> = const { Cell::new(None) };
    /// Whether the job's lowering anew found [`SEALED_RECORD`] through its home's look, as a
    /// real front end serves a dependency from its record: unit 0 then builds nothing.
    static SEALED_FOUND: Cell<bool> = const { Cell::new(false) };
    /// How many times unit 0, the sealed crate, was built for want of its record.
    static SEALED_BUILDS: Cell<usize> = const { Cell::new(0) };
}

thread_local! {
    /// Whether the fakes keep the model of the units that stand (`job::standing`): a record a
    /// unit, made over its sources' version, the records of the units it depends on and the
    /// request's `STANDING`, kept in [`STAND_RECORDS`]. Off but in those tests.
    static STAND_MODEL: Cell<bool> = const { Cell::new(false) };
    /// The units the fakes' front end says are sealed ([`Front::sealed`]).
    static STAND_SEALED: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
    /// Each unit's dependencies, as the test's plan has them: what a record is made over, and
    /// how many units [`Front::sealed`] answers of.
    static STAND_DEPS: RefCell<Vec<Vec<usize>>> = const { RefCell::new(Vec::new()) };
    /// Each unit's sources' version, 0 unless an edit of the test's moved it.
    static STAND_VERSIONS: RefCell<BTreeMap<usize, u64>> = const { RefCell::new(BTreeMap::new()) };
    /// The request's `STANDING`, as the job's lowering saw it.
    static STAND_ENV: RefCell<String> = const { RefCell::new(String::new()) };
    /// The fakes' store of records, by unit: what each unit's last store put, what a unit that
    /// stands finds ([`Front::stand`]).
    static STAND_RECORDS: RefCell<BTreeMap<usize, ContentId>> =
        const { RefCell::new(BTreeMap::new()) };
    /// Each unit's record as a build ended it, made or stood, in the order they ended.
    static STAND_ENDED: RefCell<Vec<(usize, ContentId)>> = const { RefCell::new(Vec::new()) };
    /// Each unit stood in its one task ([`Front::stand`]), in order.
    static STOOD: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
    /// Set while a standing unit's task runs its store, which replays the record it found.
    static STANDING_TASK: Cell<bool> = const { Cell::new(false) };
}

/// The record unit `unit` is made into now, in the model of the units that stand
/// ([`STAND_MODEL`]): of its sources' version, the request's `STANDING` and the record of each
/// unit it depends on (none for one that has none, as a build script's run).
fn made_record(unit: usize) -> ContentId {
    let version = STAND_VERSIONS.with_borrow(|it| it.get(&unit).copied().unwrap_or(0));
    let mut hasher = Hasher::new("blaze serve tests standing record");
    hasher.part(&(unit as u64).to_le_bytes()).part(&version.to_le_bytes());
    STAND_ENV.with_borrow(|it| {
        hasher.part(it.as_bytes());
    });
    let deps = STAND_DEPS.with_borrow(|it| it.get(unit).cloned().unwrap_or_default()); // clone: a test's plan, read out of its cell
    for dep in deps {
        let record = STAND_RECORDS.with_borrow(|it| it.get(&dep).copied());
        hasher.part(&record.map_or([0; 32], |it| it.0));
    }
    hasher.finish()
}

/// The front-end record of the sealed crate the fakes' unit 0 is ([`SEALED_INTO`]).
const SEALED_RECORD: &[u8] = b"a sealed crate's front-end record\n";

fn sealed_key() -> blaze_cas::Key {
    blaze_cas::Key(ContentId::of("blaze serve tests sealed", SEALED_RECORD))
}

const SHARED: &[u8] = b"a std unit's record\n";

/// The `n`th body entries' session the fakes save: its root, and the value under it.
fn body_session(n: u8) -> (blaze_cas::Key, Vec<u8>) {
    let bytes = format!("the fakes' body entries' session {n}\n").into_bytes();
    (blaze_cas::Key(ContentId::of("blaze serve tests body session", &bytes)), bytes)
}

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

/// What the fakes' second store of `checkout`'s kept front end puts: its members' pages.
fn members_key(checkout: &str) -> blaze_cas::Key {
    blaze_cas::Key(ContentId::of("blaze serve tests kept members", checkout.as_bytes()))
}

fn members_value(checkout: &str) -> Vec<u8> {
    format!("the pages of the members of {checkout}\n").into_bytes()
}

/// Whether `store` holds the pages of `checkout`'s members ([`members_key`]).
fn holds_members(store: &Store, checkout: &str) -> bool {
    store.get(&members_key(checkout)).unwrap().is_some_and(|it| it.bytes == members_value(checkout))
}

/// Whether `store` holds all a build of `checkout` left for it.
fn holds_kept(store: &Store, checkout: &str) -> bool {
    let shared = store.get(&shared_kept_key()).unwrap().is_some_and(|it| it.bytes == SHARED_KEPT);
    shared
        && (0..2).all(|at| {
            store
                .get(&kept_key(checkout, at))
                .unwrap()
                .is_some_and(|it| it.bytes == kept_value(checkout, at))
        })
}

const SCRIPT: &[u8] = b"\x7fELF a build script\n";
const SCRIPT_AT: &str = "/w/target/debug/build/app-b/build-script-build";

const APP: &[u8] = b"\x7fELF the program\n";
const APP_AT: &str = "/w/target/debug/app";

const SIDE: &[u8] = b"\x7fELF the program, under the name with its hash\n";
const SIDE_AT: &str = "/w/target/debug/deps/app-0123";

/// The program's artifact as its compile reports it: it names [`APP_AT`] and nothing else.
fn app_artifact() -> Artifact {
    Artifact {
        env: Vec::new(),
        kind: ArtifactKind::Bin,
        path: APP_AT.into(),
        fresh: false,
        runner: Vec::new(),
        target: UnitTarget {
            doc: false,
            kind: TargetKind::Bin,
            name: "app".into(),
            test: false,
            doctest: false,
            edition: "2021".into(),
            src_path: "/w/src/main.rs".into(),
            crate_types: vec![CrateType::Bin],
        },
        content: ContentId::of("blaze serve tests output", APP),
        harness: true,
        package: "path+file:///w#0.1.0".into(),
        profile: UnitProfile {
            test: false,
            debuginfo: 2,
            opt_level: "0".into(),
            overflow_checks: true,
            debug_assertions: true,
        },
        features: Vec::new(),
        filenames: vec![APP_AT.into()],
        executable: Some(APP_AT.into()),
        manifest_path: "/w/Cargo.toml".into(),
    }
}

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
    let unit = UnitOf {
        id: 0,
        test: false,
        doctest: false,
        target,
        package_id: "app".into(),
        manifest_path: "/w/Cargo.toml".into(),
        primary: true,
    };
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
        let config =
            BuildConfig::decode(cx.config()?).expect("a view of the daemon has a build config");
        SAW.with(|it| it.borrow_mut().push(config.clone()));
        let manifest = cx
            .file("Cargo.toml")?
            .map(|it| String::from_utf8_lossy(it.bytes).into_owned())
            .unwrap_or_default();
        if manifest.starts_with("fail") {
            let at = Some(format!("{}/Cargo.toml", config.invocation.root));
            return Ok(Cbor(Planned::Failed(LoadError {
                what: "error: the fake load refuses this manifest".into(),
                at,
            })));
        }
        let missing: Vec<Fetch> = needed()
            .into_iter()
            .filter(|fetch| !config.mounts.iter().any(|it| it.at == fetch.at))
            .collect();
        if !missing.is_empty() {
            return Ok(Cbor(Planned::Needs(missing)));
        }
        let wave = manifest.strip_prefix("wave ").and_then(|it| it.trim().parse().ok());
        Ok(Cbor(Planned::Ready(match wave {
            Some(scripts) => wave_units(&config.invocation.root, manifest, scripts),
            None if manifest.starts_with("two scripts") => {
                two_script_units(&config.invocation.root, manifest)
            }
            None if manifest.starts_with("links scripts") => {
                links_script_units(&config.invocation.root, manifest)
            }
            None if manifest.starts_with("proc macro") => {
                proc_macro_units(&config.invocation.root, manifest)
            }
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
                gpu: false,
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

/// Two build scripts, each compiled (units 1 and 3) and run (2 and 4), and the program on both
/// runs and `dep`'s library (unit 0).
fn two_script_units(root: &str, manifest: String) -> Units {
    let app = format!("path+file://{root}#app@0.1.0");
    let dep = "registry+https://github.com/rust-lang/crates.io-index#dep@1.0.0".to_owned();
    let graph = UnitGraph {
        version: 1,
        units: vec![
            unit(&dep, "lib", Mode::Build, &[]),
            unit(&app, "custom-build", Mode::Build, &[]),
            unit(&app, "custom-build", Mode::RunCustomBuild, &[1]),
            unit(&app, "custom-build", Mode::Build, &[]),
            unit(&app, "custom-build", Mode::RunCustomBuild, &[3]),
            unit(&app, "bin", Mode::Build, &[0, 2, 4]),
        ],
        roots: vec![5],
    };
    let mut units = of_graph(root, manifest, graph);
    // The second script's program is a planned output of its compile, so its run waits for
    // the program's placement ([`super::job`]'s `waits_for_program`).
    units.plans[3].outputs = vec![SCRIPT_AT.to_owned()];
    units
}

/// `dep`'s build script (compiled as unit 1, run as 2), which its library (unit 0) depends on,
/// and the program's (compiled as 3, run as 4), whose run depends on `dep`'s run as well, the
/// edge cargo makes for a dependency with `links` (its run is told `DEP_<links>_*`); the
/// program (5) on `dep`'s library and its own run.
fn links_script_units(root: &str, manifest: String) -> Units {
    let app = format!("path+file://{root}#app@0.1.0");
    let dep = "registry+https://github.com/rust-lang/crates.io-index#dep@1.0.0".to_owned();
    let graph = UnitGraph {
        version: 1,
        units: vec![
            unit(&dep, "lib", Mode::Build, &[2]),
            unit(&dep, "custom-build", Mode::Build, &[]),
            unit(&dep, "custom-build", Mode::RunCustomBuild, &[1]),
            unit(&app, "custom-build", Mode::Build, &[]),
            unit(&app, "custom-build", Mode::RunCustomBuild, &[3, 2]),
            unit(&app, "bin", Mode::Build, &[0, 4]),
        ],
        roots: vec![5],
    };
    of_graph(root, manifest, graph)
}

/// `dep`'s proc macro (unit 1) on `dep`'s library (unit 0), the program's build script
/// (compiled as 2, run as 3), and the program's library, checked (4), on the macro and the run.
fn proc_macro_units(root: &str, manifest: String) -> Units {
    let app = format!("path+file://{root}#app@0.1.0");
    let dep = "registry+https://github.com/rust-lang/crates.io-index#dep@1.0.0".to_owned();
    let graph = UnitGraph {
        version: 1,
        units: vec![
            unit(&dep, "lib", Mode::Build, &[]),
            unit(&dep, "proc-macro", Mode::Build, &[0]),
            unit(&app, "custom-build", Mode::Build, &[]),
            unit(&app, "custom-build", Mode::RunCustomBuild, &[2]),
            unit(&app, "lib", Mode::Check, &[1, 3]),
        ],
        roots: vec![4],
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
        gpu_msl: None,
    };
    let package =
        |id: &str, name: &str, version: &str, source: Option<String>, is_member| PackagePlan {
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
        lock: Some(LockWrite {
            files: vec![LockFile { path: format!("{root}/Cargo.lock"), text: "# a lock\n".into() }],
            said: Vec::new(),
        }),
        warnings: match QUIET_LOAD.get() {
            true => Vec::new(),
            false => vec![manifest],
        },
        notes: Vec::new(),
        target_dir: format!("{root}/target"),
        build_dir: format!("{root}/target"),
        finished: FINISHED.into(),
        link_dirs: Vec::new(),
        sandboxed_tests: false,
        native: Vec::new(),
    }
}

/// The build of fakes; `fetched` counts each fetch by its `at`.
pub(super) struct Fake {
    fetched: blaze_core::lock::Lock<BTreeMap<String, usize>>,
    /// The launch host's join of a program's instants, the real one
    /// ([`crate::serve::unit::run::launch::ProgramTimes`]): the loop tells it a client's exec,
    /// a test plays the launcher's `Hello` and end into it as the launcher's reader would.
    times: blaze_core::lock::Lock<crate::serve::unit::run::launch::ProgramTimes>,
}

impl Default for Fake {
    fn default() -> Fake {
        Fake {
            fetched: blaze_core::lock::Lock::new("serve.tests.fetched", BTreeMap::new()),
            times: blaze_core::lock::Lock::new("serve.tests.times", Default::default()),
        }
    }
}

impl Steps for Fake {
    type Front = FakeFront;
    type Writer = FakeWriter;
    type Reader = FakeReader;
    type KeptFront = FakeInputs;
    type Linked = FakeImage;
    type Runs = FakeRuns;

    fn launch_execd(&self, pid: u32, build: u64, exec_ns: i64) {
        self.times.lock().execd(pid, build, exec_ns);
    }

    fn launch_spans(&self) -> Vec<crate::serve::unit::run::launch::ProgramSpans> {
        self.times.lock().take()
    }

    fn fetch(&self, fetch: &Fetch) -> Result<Source, ApiError> {
        *self.fetched.lock().entry(fetch.at.clone()).or_default() += 1;
        if FAILS_FETCHES.get() {
            // The text of blaze-registry's `NoNet`, which only a blaze without `net` has.
            let why = format!(
                "{}: this blaze is built without blaze-registry's `net` feature, so it has no HTTP client to ask it with",
                fetch.url
            );
            return Err(ApiError::new(why, Some(fetch.what.clone())));
        }
        let source = match fetch.kind {
            FetchKind::IndexFile => Source {
                files: vec![SourceFile {
                    path: "dep".into(),
                    bytes: blaze_vfs::Bytes::from_static(b"{}\n"),
                    mode: blaze_vfs::Mode::Regular,
                }],
                under: "dep".into(),
            },
            _ => Source {
                files: vec![SourceFile {
                    path: "Cargo.toml".into(),
                    bytes: blaze_vfs::Bytes::copy_kept(
                        blaze_mem::Tag::Vfs,
                        format!("# {}\n", fetch.what).as_bytes(),
                    ),
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
        let file = SourceFile {
            path: "core/src/lib.rs".into(),
            bytes: blaze_vfs::Bytes::from_static(b"#![no_core]\n"),
            mode: blaze_vfs::Mode::Regular,
        };
        Ok(Library {
            at: LIBRARY.into(),
            fingerprint,
            source: Some(Source { files: vec![file], under: String::new() }),
        })
    }

    /// The fakes' build dirs are paths in the simulation (`/w/target`), never on disk: no lease.
    fn lease(&self, _: &Units, _: &str, _: &str) -> super::work::Leased {
        Ok(Ok(Vec::new())) // alloc: none, an empty vec
    }

    fn store_kept(&self, kept: &mut FakeInputs) -> Result<Vec<KeptPut>, ApiError> {
        // The second store, before an idle front end is let go or at a stop: the workspace
        // members' pages, as a real front end's second store writes them (`Stored::Sealed`).
        if kept.stored {
            if kept.members {
                return Ok(Vec::new());
            }
            kept.members = true;
            MEMBER_STORES.set(MEMBER_STORES.get() + 1);
            return Ok(vec![KeptPut {
                put: Put {
                    key: members_key(&kept.checkout),
                    kind: blaze_cas::Kind::Query,
                    bytes: members_value(&kept.checkout),
                },
                what: Arc::from("the fakes' pages of a checkout's members"),
                into: crate::serve::work::PutInto::Home,
            }]);
        }
        kept.stored = true;
        STORES.set(STORES.get() + 1);
        if PUTS_NOTHING.with_borrow(|it| *it == kept.checkout) {
            return Ok(Vec::new());
        }
        let put = |key, bytes, what: &str| KeptPut {
            put: Put { key, kind: blaze_cas::Kind::Query, bytes },
            what: Arc::from(what),
            into: crate::serve::work::PutInto::Home,
        };
        let checkout = &kept.checkout;
        // The shared value last: a job that waited on its last put's ticket alone answers before
        // its own two are durable once another checkout put the shared one first (review C1).
        Ok(vec![
            put(
                kept_key(checkout, 0),
                kept_value(checkout, 0),
                "the fakes' first value of a checkout",
            ),
            put(
                kept_key(checkout, 1),
                kept_value(checkout, 1),
                "the fakes' second value of a checkout",
            ),
            put(shared_kept_key(), SHARED_KEPT.to_vec(), "the fakes' value of every checkout"),
        ]
        .into_iter()
        .chain(SEALED_INTO.get().map(|into| KeptPut {
            put: Put {
                key: sealed_key(),
                kind: blaze_cas::Kind::Query,
                bytes: SEALED_RECORD.to_vec(),
            },
            what: Arc::from("the fakes' front-end record of a sealed crate"),
            into,
        }))
        .collect())
    }

    fn lower(&self, job: &JobInput, kept: Option<FakeInputs>) -> Result<FrontEnd<Fake>, Failed> {
        // clone: a test's record of the marks the job opens
        BODY_MARKS_SEEN.with_borrow_mut(|it| it.push((*job.body_marks).clone()));
        // What the model of the units that stand makes its records under ([`made_record`]).
        let standing = job.config.invocation.env.iter().find(|(name, _)| name == "STANDING");
        STAND_ENV.set(standing.map(|(_, value)| value.clone()).unwrap_or_default()); // clone: a test's record
        if kept.as_ref().is_some_and(|it| !it.stored) {
            return Err(Failed::Error(ApiError::new(
                "the fakes' kept front end is lowered into unstored",
                None,
            )));
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
                let read = |at| {
                    job.store
                        .get(&guard, &kept_key(&checkout, at))
                        .ok()
                        .flatten()
                        .is_some_and(|it| it.bytes == kept_value(&checkout, at))
                };
                if read(0) && read(1) {
                    KEPT_READS.set(KEPT_READS.get() + 1);
                }
                // The home's look reads the machine's layer live (`Store::read_live`): a
                // sealed crate's record another home put there is found through it.
                SEALED_FOUND.set(
                    job.store
                        .get(&guard, &sealed_key())
                        .ok()
                        .flatten()
                        .is_some_and(|it| it.bytes == SEALED_RECORD),
                );
                FakeInputs { checkout, ..FakeInputs::default() }
            }
        };
        let writer = FakeWriter { inputs: Arc::new(inputs), asked: Vec::new(), broke: Vec::new() };
        Ok(FrontEnd {
            front: Arc::new(FakeFront { kind: job.kind }),
            writer,
            puts: Vec::new(),
            misses: Vec::new(),
        })
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

    // The launch host's side, as `launched_restart` drives it: what each start hands over is
    // kept, and the images and wants it queued are handed to the loop.
    fn launch_marked(
        &self,
        store: blaze_cas::Reader,
        waker: Option<blaze_core::Waker>,
        marked: Vec<crate::serve::work::LaunchMarked>,
    ) {
        launched_restart::marked(store, waker, marked);
    }

    fn launch_kept(&self) -> Vec<crate::serve::work::LaunchKept> {
        launched_restart::kept()
    }

    fn launch_wants(&self) -> Vec<String> {
        launched_restart::wants()
    }

    fn launch_built(&self, program: &str, built: Result<(), String>) {
        launched_restart::built(program, built);
    }

    // The launch host's code for a program, as solo.rs plays it.
    fn held_code(&self, holder: crate::serve::work::CodeHolder, program: &str) -> Option<crate::serve::work::HeldCode> {
        solo::held_code(holder, program)
    }
}

pub(super) struct FakeFront {
    kind: JobKind,
}

/// A front end's inputs: the build scripts' runs applied to it, in the order applied, whether
/// it was stored since it was last kept, and the checkout it was lowered for.
#[derive(Default)]
pub(super) struct FakeInputs {
    runs: Vec<usize>,
    stored: bool,
    /// Whether its second store, of its members, was made since it was last kept.
    members: bool,
    checkout: String,
}

/// The fakes' front end has no database: their images store no library trees. What it holds
/// and gives back for memory is the give-back simulation's model of its checkout
/// (`job::giving::sim`): nothing, unless a test says so.
impl KeptDb for FakeInputs {
    fn db(&self) -> Option<&base_db::Db> {
        None
    }

    fn held(&self) -> u64 {
        super::job::giving::sim::held(&self.checkout)
    }

    fn values(&mut self) -> u64 {
        super::job::giving::sim::values(&self.checkout)
    }

    fn give_values(&mut self, bytes: u64) -> u64 {
        super::job::giving::sim::give(&self.checkout, bytes)
    }
}

/// The front end written as the real one is: each write asked for waits until no reader of
/// the inputs is alive.
pub(super) struct FakeWriter {
    inputs: Arc<FakeInputs>,
    asked: Vec<usize>,
    broke: Vec<Broke>,
}

/// A shared reference to the inputs, as the loop posted a task with it.
pub(super) struct FakeReader(Arc<FakeInputs>);

/// A kept image as the kept images' budget sees it: what it weighs and whether a run maps it.
/// The fakes' compiles link nothing, so only a test of the budget ([`super::job`]'s `Paths`)
/// makes one.
pub(super) struct FakeImage {
    pub(super) held: u64,
    pub(super) live: bool,
}

impl Kept for FakeImage {
    fn tables(&self) -> Vec<(&'static str, u64, u64)> {
        Vec::new()
    }

    fn held(&self) -> u64 {
        self.held
    }

    fn live(&self) -> bool {
        self.live
    }

    fn rest(&mut self) {}
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
        Err(ApiError::new(
            format!("the fakes build no proc macro, and unit {unit} is not one"),
            None,
        ))
    }

    fn gpu_msl(&mut self, unit: usize, _: Vec<u8>) -> Result<(), ApiError> {
        Err(ApiError::new(format!("the fakes build no device unit, and unit {unit} is not one"), None))
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
        if FAILS_APPLY.get() {
            return Err(ApiError::new(
                format!("serve-test: applying runs {:?} fails", self.asked),
                None,
            ));
        }
        let Some(inputs) = Arc::get_mut(&mut self.inputs) else { return Ok(false) };
        for run in self.asked.drain(..) {
            let applied = applying_fallible(run, "a build script's run", || {
                assert_ne!(PANICS_RUN.get(), Some(run), "serve-test: the run's write panics");
                if FAILS_RUN.get() == Some(run) {
                    return Err(ApiError::new(
                        "serve-test: the run's write fails",
                        Some("serve-test's run".into()),
                    ));
                }
                inputs.runs.push(run);
                Ok(())
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

    /// The fake lowers no crates: the front-end carry has none of its builds, but for a
    /// test that names some ([`CARRY_CRATES`]).
    fn view_crates(&self) -> Result<Vec<crate::serve::front_carry::CrateRoot>, String> {
        Ok(CARRY_CRATES.with_borrow(Clone::clone))
    }

    fn keep(mut self) -> Result<FakeInputs, ApiError> {
        if !self.apply()? {
            return Err(ApiError::new(
                "the fakes' runs are kept while a task still reads them",
                None,
            ));
        }
        let mut inputs = Arc::into_inner(self.inputs).ok_or_else(|| {
            ApiError::new("the fakes' inputs are kept while a task still holds them", None)
        })?;
        // Kept again: what this build left is not stored yet, as a real front end's is not.
        inputs.stored = false;
        inputs.members = false;
        Ok(inputs)
    }
}

/// The fakes' user code: a build script's run, or what is over as it starts, or a run that
/// keeps what it did in the store.
pub(super) enum FakeRuns {
    Script(Script),
    Slow(Slow),
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
            FakeRuns::Slow(it) => it.start(store, writer, effects, out),
            FakeRuns::Over(it) => it.start(store, writer, effects, out),
            FakeRuns::Lost(it) => it.start(store, writer, effects, out),
            FakeRuns::Keeps(it) => it.start(store, writer, effects, out),
        }
    }

    fn owns(&self, tag: Tag) -> bool {
        match self {
            FakeRuns::Script(it) => it.owns(tag),
            FakeRuns::Slow(it) => it.owns(tag),
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
            FakeRuns::Slow(it) => it.complete(completion, store, writer, effects, out),
            FakeRuns::Over(it) => it.complete(completion, store, writer, effects, out),
            FakeRuns::Lost(it) => it.complete(completion, store, writer, effects, out),
            FakeRuns::Keeps(it) => it.complete(completion, store, writer, effects, out),
        }
    }

    fn cancel(&mut self, effects: &mut Vec<Effect>) -> Option<bool> {
        match self {
            FakeRuns::Script(it) => it.cancel(effects),
            FakeRuns::Slow(it) => it.cancel(effects),
            FakeRuns::Over(it) => it.cancel(effects),
            FakeRuns::Lost(it) => it.cancel(effects),
            FakeRuns::Keeps(it) => it.cancel(effects),
        }
    }

    fn take_puts(&mut self) -> RunPuts {
        match self {
            FakeRuns::Script(it) => it.take_puts(),
            FakeRuns::Slow(it) => it.take_puts(),
            FakeRuns::Over(it) => it.take_puts(),
            FakeRuns::Lost(it) => it.take_puts(),
            FakeRuns::Keeps(it) => it.take_puts(),
        }
    }
}

/// A build script's run that is over as it starts, its run asked of the writer.
pub(super) struct Script(usize);

impl Runs<Fake> for Script {
    fn start(
        &mut self,
        _: &Store,
        writer: Option<&mut FakeWriter>,
        _: &mut Vec<Effect>,
        _: &mut Vec<Response>,
    ) -> Option<bool> {
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

/// A build script's run that ends on a timer of its own ([`SLOW_SCRIPT`]), its run then
/// asked of the writer.
pub(super) struct Slow {
    unit: usize,
    tag: Tag,
    after: Duration,
}

impl Runs<Fake> for Slow {
    fn start(
        &mut self,
        _: &Store,
        _: Option<&mut FakeWriter>,
        effects: &mut Vec<Effect>,
        _: &mut Vec<Response>,
    ) -> Option<bool> {
        effects.push(Effect::Timer { after: self.after, tag: self.tag });
        None
    }

    fn owns(&self, tag: Tag) -> bool {
        tag == self.tag
    }

    fn complete(
        &mut self,
        _: Completion,
        _: &mut Store,
        writer: Option<&mut FakeWriter>,
        _: &mut Vec<Effect>,
        _: &mut Vec<Response>,
    ) -> Option<bool> {
        // A run that ends after its job left its build has no writer to ask: it fails.
        Some(
            writer.is_some_and(|writer| writer.ran(self.unit, blaze_lower::Ran::default()).is_ok()),
        )
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
pub(super) struct Lost;

impl Runs<Fake> for Lost {
    fn start(
        &mut self,
        _: &Store,
        _: Option<&mut FakeWriter>,
        _: &mut Vec<Effect>,
        _: &mut Vec<Response>,
    ) -> Option<bool> {
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
pub(super) struct Keeps {
    tag: Tag,
    puts: RunPuts,
    /// Whether it hands its put's ticket over; not, under [`TESTS_KEEP_UNTOLD`].
    tells: bool,
}

impl Runs<Fake> for Keeps {
    fn start(
        &mut self,
        _: &Store,
        _: Option<&mut FakeWriter>,
        effects: &mut Vec<Effect>,
        _: &mut Vec<Response>,
    ) -> Option<bool> {
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
                writer
                    .put(&guard, test_record_key(), blaze_cas::Kind::Query, TEST_RECORD)
                    .expect("one value under its key")
            })
        });
        let ticket = match off_loop {
            Some(ticket) => ticket,
            None => store
                .put(test_record_key(), blaze_cas::Kind::Query, TEST_RECORD)
                .expect("one value under its key"),
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
pub(super) struct Over(Vec<Response>);

impl Runs<Fake> for Over {
    fn start(
        &mut self,
        _: &Store,
        _: Option<&mut FakeWriter>,
        _: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
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

    fn compile(
        &self,
        reader: FakeReader,
        unit: usize,
        phase: Phase,
        _: &mut Vec<Image<Fake>>,
    ) -> Result<Option<Compiled>, Failed> {
        PHASES.with(|it| it.borrow_mut().push((unit, phase)));
        assert_ne!(PANICS_COMPILE.get(), Some(unit), "serve-test: the unit's compile panics");
        if ERRS_COMPILE.get() == Some(unit) {
            return Err(Failed::Error(ApiError::new(
                "serve-test: unsupported: no `lang_start`",
                Some(format!("unit {unit}")),
            )));
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
        // The model of the units that stand: a unit stored makes its record now, and the
        // store keeps it; one that stands replays the record it finds ([`Front::stand`]).
        if STAND_MODEL.get() && !STANDING_TASK.get() {
            let record = made_record(unit);
            STAND_RECORDS.with_borrow_mut(|it| it.insert(unit, record));
            STAND_ENDED.with_borrow_mut(|it| it.push((unit, record)));
        }
        if unit == 0 && SEALED_INTO.get().is_some() && !SEALED_FOUND.get() {
            SEALED_BUILDS.set(SEALED_BUILDS.get() + 1);
        }
        let (mut puts, mut outputs) = (Vec::new(), Vec::new());
        if SCRIPT_OUTPUT.get() == Some(unit) {
            let key = blaze_cas::Key(ContentId::of("blaze serve tests output", SCRIPT));
            puts.push(Put { key, kind: blaze_cas::Kind::Output, bytes: SCRIPT.to_vec() });
            outputs.push(Output { path: SCRIPT_AT.into(), key, executable: true, made: None });
        }
        if unit == 3 && OUTPUT.get() {
            let key = blaze_cas::Key(ContentId::of("blaze serve tests output", APP));
            puts.push(Put { key, kind: blaze_cas::Kind::Output, bytes: APP.to_vec() });
            outputs.push(Output { path: APP_AT.into(), key, executable: true, made: None });
        }
        if unit == 3 && SIDE_OUTPUT.get() {
            let key = blaze_cas::Key(ContentId::of("blaze serve tests output", SIDE));
            puts.push(Put { key, kind: blaze_cas::Kind::Output, bytes: SIDE.to_vec() });
            outputs.push(Output { path: SIDE_AT.into(), key, executable: true, made: None });
        }
        let mut body_sessions = Vec::new();
        if unit == 3 && BODY_SESSIONS.get() {
            BODY_SAVES.set(BODY_SAVES.get() + 1);
            let (key, bytes) = body_session(BODY_SAVES.get());
            puts.push(Put { key, kind: blaze_cas::Kind::Query, bytes });
            body_sessions.push(("app".to_owned(), key));
        }
        let artifact = (unit == 3 && OUTPUT.get()).then(app_artifact);
        let diagnostics = match unit == 3 && WARNS.get() {
            true => {
                vec![warning_of_app("unused variable: `x`"), warning_of_app("unused import: `y`")]
            }
            false => Vec::new(),
        };
        let share = match unit == 0 && SHARES.get() {
            true => vec![Put {
                key: shared_key(),
                kind: blaze_cas::Kind::Query,
                bytes: SHARED.to_vec(),
            }],
            false => Vec::new(),
        };
        Ok(Some(Compiled {
            diagnostics,
            ok: true,
            open: false,
            why: None,
            puts,
            record: None,
            body_sessions,
            share,
            outputs,
            artifact,
            macros: None,
            gpu_msl: None,
            links: Vec::new(),
            misses: Vec::new(),
        }))
    }

    /// Its names, then, when the fakes' store has its record ([`STAND_RECORDS`]), its store,
    /// which replays that record: both phases in the one task, as the real front end's.
    fn stand(
        &self,
        reader: FakeReader,
        unit: usize,
        images: &mut Vec<Image<Fake>>,
    ) -> Result<Option<Compiled>, Failed> {
        STOOD.with_borrow_mut(|it| it.push(unit));
        let stores = FakeReader(reader.0.clone()); // clone: a refcount, the store's reader
        self.compile(reader, unit, Phase::Names, images)?;
        let Some(record) = STAND_RECORDS.with_borrow(|it| it.get(&unit).copied()) else {
            return Ok(None);
        };
        STANDING_TASK.set(true);
        let stored = self.compile(stores, unit, Phase::Store, images);
        STANDING_TASK.set(false);
        STAND_ENDED.with_borrow_mut(|it| it.push((unit, record)));
        stored
    }

    fn sealed(&self) -> Result<Vec<bool>, ApiError> {
        let units = STAND_DEPS.with_borrow(Vec::len);
        let sealed: Vec<bool> = STAND_SEALED.with_borrow(|it| (0..units).map(|at| it.contains(&at)).collect());
        Ok(sealed)
    }

    fn refused(&self, _: usize, what: String) -> Diagnostic {
        Diagnostic {
            level: Level::Error,
            rendered: format!("error: {what}\n"),
            ..warning_of_app(&what)
        }
    }

    fn script(&self, unit: usize, tags: u64) -> Result<FakeRuns, ApiError> {
        SCRIPTS.with_borrow_mut(|it| it.push(unit));
        Ok(match (LOSES_RUNS.get(), SLOW_SCRIPT.get()) {
            (true, _) => FakeRuns::Lost(Lost),
            (false, Some((slow, after))) if slow == unit => {
                FakeRuns::Slow(Slow { unit, tag: Tag(tags), after })
            }
            (false, _) => FakeRuns::Script(Script(unit)),
        })
    }

    fn tests(
        &self,
        _: &[Artifact],
        selection: Selection<'_>,
        tags: u64,
    ) -> Result<FakeRuns, ApiError> {
        assert!(selection.ub.is_none(), "tests under the evaluator are evaluated, not run");
        if TESTS_KEEP.get() {
            return Ok(FakeRuns::Keeps(Keeps {
                tag: Tag(tags),
                puts: RunPuts::default(),
                tells: !TESTS_KEEP_UNTOLD.get(),
            }));
        }
        Ok(FakeRuns::Over(Over(Vec::new())))
    }

    fn ub_tests(
        &self,
        FakeReader(_): FakeReader,
        selection: Selection<'_>,
    ) -> Result<Vec<String>, Failed> {
        assert_eq!(self.kind, JobKind::Check, "tests under the evaluator only check their units");
        let all = ["a::adds", "a::overflows", "b::reads"];
        Ok(all
            .iter()
            .filter(|it| selection.filter.iter().all(|filter| it.contains(&**filter)))
            .map(|it| (*it).to_owned())
            .collect())
    }

    fn evaluate(
        &self,
        FakeReader(_): FakeReader,
        test: &str,
        ub: Ub,
    ) -> Result<TestResult, Failed> {
        let (outcome, output) = match test {
            "a::overflows" => (
                TestOutcome::Failed,
                format!("undefined behavior\nseed: {}\n", ub.seed.unwrap_or(0)).into_bytes(),
            ),
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

    fn run(
        &self,
        _: &[Artifact],
        _: Option<&str>,
        _: &[String],
        _: u64,
    ) -> Result<FakeRuns, ApiError> {
        Ok(FakeRuns::Over(Over(vec![Response::Ran {
            code: Some(0),
            stdout: b"hi\n".to_vec(),
            stderr: Vec::new(),
        }])))
    }

    fn reach(
        &self,
        FakeReader(to): FakeReader,
        _: &FakeFront,
        FakeReader(from): FakeReader,
        changes: &[Change],
    ) -> Result<Reach, Failed> {
        REACHED.with_borrow_mut(|it| it.push((from.runs.clone(), to.runs.clone())));
        Ok(Reach {
            changes: changes.to_vec(),
            items: Vec::new(),
            compiled: Vec::new(),
            tests: Vec::new(),
        })
    }

    fn edit(
        &self,
        FakeReader(read): FakeReader,
        _: &[blaze_api::Edit],
    ) -> Result<blaze_api::Edited, Failed> {
        EDITED.with_borrow_mut(|it| it.push(read.runs.clone()));
        Ok(blaze_api::Edited { ops: Vec::new(), items: Vec::new() })
    }

    fn ahead(&self, FakeReader(read): FakeReader, filter: &[String]) -> Result<Vec<u32>, Failed> {
        AHEAD_READ.with_borrow_mut(|it| it.push(read.runs.clone()));
        Ok(if filter.iter().all(|it| "a::adds".contains(&**it)) { vec![0] } else { Vec::new() })
    }

    fn codegen_inputs(&self, _: FakeReader) -> Result<Vec<(ContentId, ContentId)>, Failed> {
        Ok(Vec::new())
    }

    fn tables(
        &self,
        mut writer: FakeWriter,
        _: &[(&'static str, u64, u64)],
    ) -> Result<Vec<Table>, Failed> {
        let counted = writer.tables()?;
        Ok(counted
            .into_iter()
            .map(|(name, elements, bytes)| Table { name: name.to_owned(), elements, bytes })
            .collect())
    }


    /// One page naming the build scripts' runs its front end had applied, read with the front
    /// end exclusive, as the real documentation sets `cfg(doc)`.
    fn doc(
        &self,
        mut writer: FakeWriter,
        no_deps: bool,
        private: bool,
    ) -> Result<Vec<Response>, Failed> {
        assert_eq!(self.kind, JobKind::Check, "the documentation only checks its units");
        writer.apply()?;
        let Some(inputs) = Arc::get_mut(&mut writer.inputs) else {
            return Err(ApiError::new("a task still reads the front end", None).into());
        };
        let page = format!("{no_deps} {private} {:?}", inputs.runs).into_bytes();
        let crates = vec![blaze_api::DocumentedCrate {
            name: "app".into(),
            package: "app v0.1.0 (/w)".into(),
            asked: true,
        }];
        Ok(vec![
            Response::Documented(blaze_api::Documented {
                dir: "/w/target/doc".into(),
                crates,
                pages: 1,
                source_pages: 0,
            }),
            Response::DocPage { path: "app/index.html".into(), page },
        ])
    }
}

fn options() -> Options {
    Options {
        state: name(STATE),
        std: name(STD),
        builds: name("cache/blaze/build"),
        sources: name("cache/blaze/sources"),
        writer: Hasher::new("blaze v2 test writer").part(STATE.as_bytes()).finish(),
        // The simulation's files are in no file system a loader's thread could read: the
        // stores load their packs through the daemon's driver.
        store_loader: blaze_cas::Loader::Pumped,
        socket: Some(name(SOCKET)),
        http: None,
        budget: None,
        threads: 2,
        io: 2,
        env: vec![("PATH".into(), "/bin".into()), ("HOME".into(), "/home/u".into())],
        host: "x86_64-unknown-linux-gnu".into(),
        toolchain: Hasher::new("blaze v2 test toolchain").finish(),
        color: false,
        build: blaze_api::Build::default(),
        settings: Vec::new(),
        // A table of its own: each daemon drives one, and tests run at once.
        outside: Box::leak(Box::new(blaze_vfs::outside::Dirs::new())),
        stall: blaze_profile::Watchdog::BOUND,
        #[cfg(unix)]
        activated: None,
        profiling: None,
        machine: None,
        without: super::daemon::Without::default(),
        premises: super::daemon::Premises::of_this_process().expect("the test's own executable"),
        unheld: None,
    }
}

fn checkout(sim: &mut Sim, manifest: &str) {
    sim.change(&name("w/Cargo.toml"), manifest.as_bytes());
    sim.change(&name("w/src/main.rs"), b"fn main() {}\n");
}

/// The simulation's daemon: on [`Sim`], building with the fakes.
type SimDaemon = Daemon<Sim, Fake>;

fn start(sim: Sim, steps: &Arc<Fake>, seed: u64) -> SimDaemon {
    // A view's build config names the blaze that runs it (`BuildConfig::blaze`).
    crate::release::for_tests();
    Daemon::start(sim, options(), steps.clone(), Runner::Inline(Inline::new(seed)))
        .unwrap_or_else(|why| panic!("{why}"))
}

struct Client {
    conn: Conn,
    frames: Frames,
    next: u64,
    got: Vec<(u64, Response)>,
}

fn dial(daemon: &mut SimDaemon) -> Client {
    dial_at(daemon, SOCKET)
}

/// [`dial`] for a daemon listening at `socket`, another than [`SOCKET`] (a second home's).
fn dial_at(daemon: &mut SimDaemon, socket: &str) -> Client {
    for _ in 0..TURNS {
        if let Ok(conn) = daemon.driver().dial(&Address::Unix(name(socket))) {
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

    /// [`Client::wait`] for a request whose end needs the process's worker pool idle: the
    /// daemon's stall ([`Daemon::still`]) holds only while the pool runs and queues nothing, and
    /// the pool is the process's, which the other tests of this binary use at once. Only turns
    /// with the pool idle count against [`TURNS`]; the rest go on turning, at most
    /// [`POOL_WAIT`], until the other tests' jobs end.
    fn wait_for_the_pool(&mut self, daemon: &mut SimDaemon, id: u64) -> Vec<Response> {
        let pool = blaze_graph::exec::shared();
        let since = std::time::Instant::now();
        let (mut idle, mut busy) = (0usize, 0usize);
        while idle < TURNS {
            daemon.turn();
            self.take(daemon);
            if self.done(id) {
                return self.of(id);
            }
            if pool.idle() {
                idle += 1;
                continue;
            }
            busy += 1;
            assert!(
                since.elapsed() < POOL_WAIT,
                "request {id} never ended: for {POOL_WAIT:?} the process's worker pool ran other jobs in {busy} turns \
                 ({idle} turns with it idle); it got {:?}",
                self.of(id)
            );
        }
        panic!(
            "request {id} never ended in {TURNS} turns with the process's worker pool idle ({busy} with it busy); it got {:?}",
            self.of(id)
        );
    }
}

fn agent() -> Targets {
    Targets {
        packages: Vec::new(),
        profile: String::new(),
        goal: None,
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
            Response::Diagnostic(diagnostic) if diagnostic.level == Level::Warning => {
                Some(diagnostic.message.clone())
            }
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
    let daemon = Daemon::start(driver, options(), steps, Runner::Inline(Inline::new(63)))
        .unwrap_or_else(|why| panic!("{why}"));
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
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);

    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    assert!(
        got.contains(&Response::Fetched { verb: FetchVerb::Downloaded, what: "dep v1.0.0".into() }),
        "{got:#?}"
    );
    assert!(
        got.contains(&Response::Fetched {
            verb: FetchVerb::Updating,
            what: "crates.io index".into()
        }),
        "{got:#?}"
    );
    assert_eq!(warnings(&got), vec!["[package]\nname = \"app\"\n".to_owned()]);
    let units: Vec<(String, Verb)> = got
        .iter()
        .filter_map(|it| match it {
            Response::Unit(unit) => Some((unit.name.clone(), unit.verb)),
            _ => None,
        })
        .collect();
    assert_eq!(units.len(), 2, "one `Compiling` for each package: {units:?}");
    assert!(
        units.contains(&("dep".into(), Verb::Compiling))
            && units.contains(&("app".into(), Verb::Compiling))
    );
    let built = got
        .iter()
        .position(|it| matches!(it, Response::Built { profile, .. } if profile == FINISHED));
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
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait_for_the_pool(&mut daemon, id);
    LOSES_RUNS.set(false);

    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");
    let said = got
        .iter()
        .find_map(|it| match it {
            Response::Error(error) if error.what.starts_with("blaze stalled: for") => {
                Some(error.what.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("the build fails with the stall: {got:#?}"));
    for part in [
        "nothing could move the build",
        "1 loop tasks open",
        "waits on node",
        "its run of user code started and waits on its effects, none of which is in flight",
    ] {
        assert!(said.contains(part), "the stall says `{part}`: {said}");
    }
    let quiet: f64 =
        said["blaze stalled: for ".len()..].split(' ').next().unwrap().parse().unwrap();
    assert!(
        (2.0..10.0).contains(&quiet),
        "the stall is said within seconds of the build going still: {said}"
    );
}

/// Lane seedmem's hang (2026-11-02): a blaze built without `net` fails every fetch (`NoNet`),
/// and a fetch that settled after the first failure was never counted, so the job sat in its
/// fetch stage with each fetch settled, no task running and nothing in flight: an idle engine,
/// no error and no stall. Every fetch failing, the build ends with the fetch's error. Were a
/// job left so again, the clock run past the stall bound makes the watchdog fail it, naming
/// the job and the stage it waits in (what this test then shows, failing).
#[test]
fn a_build_whose_fetches_all_fail_ends_with_the_fetch_error_rather_than_hanging() {
    FAILS_FETCHES.set(true);
    let mut sim = Sim::new(4, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 4);
    let mut client = dial(&mut daemon);
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    for _ in 0..3 {
        turns(&mut daemon, &mut client, 5_000);
        if client.done(id) {
            break;
        }
        // The simulation's clock moves only to what is due: a job nothing moves sees no time
        // pass until it is moved on.
        daemon.driver().advance(blaze_profile::Watchdog::BOUND);
    }
    FAILS_FETCHES.set(false);
    let got = client.of(id);

    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");
    let errors: Vec<&str> = got
        .iter()
        .filter_map(|it| match it {
            Response::Error(error) => Some(error.what.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        errors.iter().any(|it| it.contains("has no HTTP client")),
        "the build fails with the fetch's error: {errors:?}"
    );
    assert!(
        !errors.iter().any(|it| it.starts_with("blaze stalled")),
        "the build ends, not stalls: {errors:?}"
    );
    // Lane earlyfail: every source that failed is named in one error, not the first a run
    // (`hashbrown v0.16.1`, then `fnv v1.0.7` the next run), with where blaze looked.
    let all: Vec<&&str> =
        errors.iter().filter(|it| it.starts_with("2 of the 2 sources this build needs")).collect();
    assert_eq!(all.len(), 1, "one error names both failed fetches: {errors:?}");
    for part in [
        "crates.io index (index file) from https://index.crates.io/de/p/dep",
        "dep v1.0.0 (package) from https://static.crates.io/crates/dep/1.0.0/download",
        "blaze looked for each first in its store (/",
    ] {
        assert!(all[0].contains(part), "the error says `{part}`: {}", all[0]);
    }
}

/// The 2026-11-02 self-build stall: a build script's run that the plane starts while its
/// program's placement is still in flight is held for it ([`Jobs::release_held`]); the job
/// ends meanwhile (here the writer's apply failing as a whole, there a proc macro's write), and the held run
/// was let go without being ended at the plane. Its node stayed running, so its request, and
/// the job, never finished, and the build sat still with its error unsaid until the stall
/// detector fired. A run its job ends before it starts now fails at once, naming the run and
/// the job's error, and the build ends with that error, not a stall.
#[test]
fn a_build_script_run_held_for_its_program_fails_when_its_job_ends_rather_than_stalling() {
    use super::job::{ENDED_HELD, HELD_RUNS};
    ASKED.set(0);
    HELD_RUNS.with_borrow_mut(Vec::clear);
    ENDED_HELD.with_borrow_mut(Vec::clear);
    // Script 2's compile (unit 1) places nothing, and its run ends on a 1 s timer; script 4's
    // compile (unit 3) places its program, through writes under /w/target slowed to 5 s, so
    // its run is held for the placement past the moment run 2 ends and its apply fails.
    SCRIPT_OUTPUT.set(Some(3));
    SLOW_SCRIPT.set(Some((2, Duration::from_secs(1))));
    FAILS_APPLY.set(true);
    let mut sim = Sim::new(41, Faults::default());
    checkout(&mut sim, "two scripts\n");
    sim.slow_writes(name("w/target"), Duration::from_secs(5));
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 41);
    let mut client = dial(&mut daemon);
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    SCRIPT_OUTPUT.set(None);
    SLOW_SCRIPT.set(None);
    FAILS_APPLY.set(false);
    let held = HELD_RUNS.with_borrow_mut(std::mem::take);
    let ended = ENDED_HELD.with_borrow_mut(std::mem::take);

    // The setup did what it means to: one run was held for its program, and the job ended
    // while it still was.
    assert_eq!(held.len(), 1, "exactly one run is held for its program's placement: {held:?}");
    assert_eq!(
        ended.len(),
        1,
        "the held run is ended as its job ends: held {held:?}, ended {ended:?}"
    );
    let (node, why) = &ended[0];
    assert_eq!(*node, held[0], "the run ended is the run held");
    for part in [
        "the build script run of `app`",
        "never started",
        "its job ended while its program was still being placed",
        "serve-test: applying runs",
    ] {
        assert!(why.contains(part), "the held run's failure says `{part}`: {why}");
    }

    // The build ends with the job's own error, rather than stalling.
    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");
    let errors: Vec<&str> = got
        .iter()
        .filter_map(|it| match it {
            Response::Error(error) => Some(error.what.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        !errors.iter().any(|it| it.starts_with("blaze stalled")),
        "the build ends, not stalls: {errors:#?}"
    );
    assert!(
        errors.iter().any(|it| it.contains("serve-test: applying runs")),
        "the build says the job's own error: {errors:#?}"
    );
    // Only the run that was never held reached the writer: the held one never started.
    assert_eq!(ASKED.get(), 1, "the held run never starts after its job ended");
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
    let id = client
        .send(&mut first, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut first, id);
    SHARES.set(false);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    assert_eq!(
        first.std_store().get(&shared_key()).unwrap().map(|it| it.bytes.to_vec()),
        Some(SHARED.to_vec())
    );
    first.stop();
    let mut turns = 0;
    while first.turn() {
        turns += 1;
        assert!(turns < TURNS, "the daemon never stopped");
    }

    let other = Options {
        state: name("cache/blaze/v2-other"),
        socket: Some(name(OTHER_SOCKET)),
        writer: Hasher::new("blaze v2 test writer").part(b"cache/blaze/v2-other").finish(),
        ..options()
    };
    let second =
        Daemon::start(first.into_driver(), other, steps.clone(), Runner::Inline(Inline::new(24)))
            .unwrap_or_else(|why| panic!("{why}"));
    assert_eq!(
        second.std_store().get(&shared_key()).unwrap().map(|it| it.bytes.to_vec()),
        Some(SHARED.to_vec())
    );
    // The home's look finds it in place, through the machine's layer it reads behind its own
    // records (`Config::layers`), and its own tier holds none of it.
    assert_eq!(
        second.store().get(&shared_key()).unwrap().map(|it| it.bytes.to_vec()),
        Some(SHARED.to_vec()),
        "the home's look finds the layer's record"
    );
    assert_eq!(second.store().stored_bytes(&[shared_key()]), 0, "the home's own store has none of it");
}

/// What two daemons of two homes on one machine layer did with the sealed crate unit 0 is,
/// its front-end record put where [`SEALED_INTO`] says.
#[derive(Debug, PartialEq, Eq)]
struct SealedAcrossHomes {
    /// How many times each daemon built unit 0 for want of its record.
    built: (usize, usize),
    /// Whether the first daemon's home, the machine layer and the second daemon's home hold
    /// the record, each read from that store's own tiers.
    first_home: bool,
    layer: bool,
    second_home: bool,
}

fn sealed_across_homes(seed: u64, into: crate::serve::work::PutInto) -> SealedAcrossHomes {
    let holds = |store: &Store| {
        store.get(&sealed_key()).unwrap().is_some_and(|it| it.bytes == SEALED_RECORD)
    };
    // A home's look finds what its layers hold too (`Config::layers`, read in place behind
    // its own records): a home holds the record only with it committed in its own tier.
    let home_holds = |store: &Store| holds(store) && store.stored_bytes(&[sealed_key()]) > 0;
    let stopped = |daemon: &mut SimDaemon| {
        daemon.stop();
        let mut turns = 0;
        while daemon.turn() {
            turns += 1;
            assert!(turns < TURNS, "seed {seed}: the daemon never stopped");
        }
    };
    SEALED_INTO.set(Some(into));
    SEALED_FOUND.set(false);
    let builds = SEALED_BUILDS.get();
    let mut sim = Sim::new(seed, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut first = start(sim, &steps, seed);
    let mut client = dial(&mut first);
    let id = client
        .send(&mut first, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut first, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
    // The stop stores the kept front end and waits for its puts, the layer's on the layer.
    stopped(&mut first);
    assert!(first.lost().is_empty(), "seed {seed}: {:#?}", first.lost());
    let first_home = home_holds(first.store());
    let first_built = SEALED_BUILDS.get() - builds;

    let other = Options {
        state: name("cache/blaze/v2-other"),
        socket: Some(name(OTHER_SOCKET)),
        writer: Hasher::new("blaze v2 test writer").part(b"cache/blaze/v2-other").finish(),
        ..options()
    };
    let mut second = Daemon::start(
        first.into_driver(),
        other,
        steps.clone(),
        Runner::Inline(Inline::new(seed + 1)),
    )
    .unwrap_or_else(|why| panic!("seed {seed}: {why}"));
    let mut client = dial_at(&mut second, OTHER_SOCKET);
    let id = client
        .send(&mut second, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut second, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
    settle(&mut second, &mut client);
    stopped(&mut second);
    assert!(second.lost().is_empty(), "seed {seed}: {:#?}", second.lost());
    SEALED_INTO.set(None);
    SealedAcrossHomes {
        built: (first_built, SEALED_BUILDS.get() - builds - first_built),
        first_home,
        layer: holds(second.std_store()),
        second_home: home_holds(second.store()),
    }
}

/// The invariant: a sealed crate's front-end record that one home's daemon put into the
/// machine layer (`PutInto::Layer`, as `deps::store` routes every record of a sealed crate) is
/// found by a daemon of another home on that layer, through its home's live read of it
/// (`Store::read_live`, at the home's open): the second builds no unit of the crate, and
/// neither home holds the record, the layer alone does, across seeds.
///
/// Control: the same puts routed to the first home (`PutInto::Home`, what `store` did for a
/// crate before the route) leave the layer without it, and the second daemon builds the crate.
#[test]
fn a_sealed_crate_one_home_built_on_the_machine_layer_another_home_builds_none_of() {
    use crate::serve::work::PutInto;
    for seed in [211, 212, 213] {
        assert_eq!(
            sealed_across_homes(seed, PutInto::Layer),
            SealedAcrossHomes {
                built: (1, 0),
                first_home: false,
                layer: true,
                second_home: false
            },
            "seed {seed}"
        );
    }
    // The planted bug: the sealed crate's record put into the home.
    assert_eq!(
        sealed_across_homes(214, PutInto::Home),
        SealedAcrossHomes { built: (1, 1), first_home: true, layer: false, second_home: true },
        "the control: a record in the first home is no other home's"
    );
}

/// A reach reads each view's front end as rustc compiles it, so before it is answered each
/// view builds, through the units and the plane a check uses, its proc macros (unit 1, with
/// the library it depends on, 0) and its build scripts' runs (3, with the script's compile,
/// 2), applied to the front end the reach then reads; the library the change is of (4) is never
/// compiled. Before, a reach built nothing, so on a fresh front end every crate with a proc
/// macro was refused as pending (pmwait2's backstop) and a crate's build script never applied.
/// Both views' builds are made in the reach's one plane request: one row of the history, kept
/// records or not (before, each view's build was a request and a row of its own).
#[test]
fn a_reach_builds_its_views_proc_macros_and_build_scripts_before_it_reads_them() {
    PHASES.with(|it| it.borrow_mut().clear());
    REACHED.with(|it| it.borrow_mut().clear());
    ENDED_ROWS.with(|it| it.borrow_mut().clear());
    let mut sim = Sim::new(61, Faults::default());
    checkout(&mut sim, "proc macro\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 61);
    let mut client = dial(&mut daemon);
    let head = || ViewRef::Head("/w".into());
    let id =
        client.send(&mut daemon, Request::Reach { to: head(), from: head(), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    assert!(got.iter().any(|it| matches!(it, Response::Reached(_))), "{got:#?}");

    let ran = PHASES.with(|it| it.take());
    for unit in [0, 1, 2] {
        let stores = ran.iter().filter(|it| **it == (unit, Phase::Store)).count();
        assert_eq!(
            stores, 2,
            "unit {unit} is built once for each of the reach's two views: {ran:?}"
        );
    }
    assert!(
        !ran.iter().any(|it| it.0 == 4),
        "the library a reach reads is never compiled: {ran:?}"
    );
    let reached = REACHED.with(|it| it.take());
    assert_eq!(
        reached,
        vec![(vec![3], vec![3])],
        "both views read with the build script's run applied: {reached:?}"
    );
    let rows = ENDED_ROWS.with(|it| it.take());
    assert_eq!(
        rows,
        ["reach"],
        "a reach is one plane request and one row of the history, both views' builds in it"
    );

    // Again, with every record kept: its builds make nothing again, and still cost no row.
    let id =
        client.send(&mut daemon, Request::Reach { to: head(), from: head(), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    let rows = ENDED_ROWS.with(|it| it.take());
    assert_eq!(rows, ["reach"], "a reach whose builds are all kept is still one row");
}

/// An edit reads its view's def maps as rustc would resolve them: before it is posted, the view
/// builds its proc macros (unit 1, with its library 0) and its build script (compiled as 2, run
/// as 3), applied to the front end the edit reads, in the edit's one plane request (one row);
/// the library it edits (4) is never compiled. Before, an edit on a fresh front end read the
/// crates with no derive expanded and no build script's cfg applied, and said nothing.
#[test]
fn an_edit_builds_its_views_proc_macros_and_build_scripts_before_it_reads_it() {
    PHASES.with(|it| it.borrow_mut().clear());
    EDITED.with(|it| it.borrow_mut().clear());
    ENDED_ROWS.with(|it| it.borrow_mut().clear());
    let mut sim = Sim::new(270, Faults::default());
    checkout(&mut sim, "proc macro\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 270);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Open { view: ViewRef::Head("/w".into()) });
    let got = client.wait(&mut daemon, id);
    let Some(root) = got.iter().find_map(|it| match it {
        Response::Opened { root } => Some(*root),
        _ => None,
    }) else {
        panic!("the open said no root: {got:#?}")
    };
    let id = client
        .send(&mut daemon, Request::Edit { base: root, head: "/w".into(), edits: Vec::new() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    assert!(got.iter().any(|it| matches!(it, Response::Edited(_))), "{got:#?}");

    let ran = PHASES.with(|it| it.take());
    for unit in [0, 1, 2] {
        let stores = ran.iter().filter(|it| **it == (unit, Phase::Store)).count();
        assert_eq!(stores, 1, "unit {unit} is built once, for the edit's view: {ran:?}");
    }
    assert!(
        !ran.iter().any(|it| it.0 == 4),
        "the library an edit reads is never compiled: {ran:?}"
    );
    let edited = EDITED.with(|it| it.take());
    assert_eq!(
        edited,
        vec![vec![3]],
        "the edit read its view with the build script's run applied: {edited:?}"
    );
    let rows = ENDED_ROWS.with(|it| it.take());
    assert_eq!(
        rows,
        ["edit"],
        "an edit is one plane request and one row of the history, its view's build in it"
    );
}

/// A build ahead chooses its tests from its view's def maps: before it chooses, the view builds
/// its proc macros (unit 1, with 0) and its build script (2, run as 3), applied to the front
/// end it reads, then builds what it chose, all in one plane request (one row).
#[test]
fn a_build_ahead_builds_its_views_proc_macros_and_build_scripts_before_it_chooses() {
    PHASES.with(|it| it.borrow_mut().clear());
    AHEAD_READ.with(|it| it.borrow_mut().clear());
    ENDED_ROWS.with(|it| it.borrow_mut().clear());
    let mut sim = Sim::new(271, Faults::default());
    checkout(&mut sim, "proc macro\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 271);
    let mut client = dial(&mut daemon);
    let intent = Request::WillTest {
        view: ViewRef::Head("/w".into()),
        filter: vec!["adds".into()],
        targets: agent(),
        wait_ms: 600_000,
    };
    let id = client.send(&mut daemon, intent);
    let got = client.wait(&mut daemon, id);
    assert!(got.iter().any(|it| matches!(it, Response::Announced { .. })), "{got:#?}");
    turn_until(&mut daemon, |_| ENDED_ROWS.with_borrow(|it| !it.is_empty()));

    let read = AHEAD_READ.with(|it| it.take());
    assert_eq!(
        read,
        vec![vec![3]],
        "the build ahead chose with the build script's run applied: {read:?}"
    );
    let ran = PHASES.with(|it| it.take());
    let first = ran.iter().position(|it| *it == (1, Phase::Store));
    assert!(first.is_some(), "the view's proc macro was built: {ran:?}");
    assert!(
        !ran.iter().any(|it| it.0 == 4),
        "the library no chosen test needs is never compiled: {ran:?}"
    );
    let rows = ENDED_ROWS.with(|it| it.take());
    assert_eq!(rows, ["ahead"], "a build ahead is one plane request and one row of the history");
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
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
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
        assert_eq!(
            (phases[0], phases[phases.len() - 1]),
            (Phase::Names, Phase::Store),
            "unit {unit}: {phases:?}"
        );
        if let Some(link) = phases.iter().position(|it| *it == Phase::Link) {
            assert!(
                phases[..link].contains(&Phase::Bodies) && phases[..link].contains(&Phase::Codegen),
                "unit {unit}: {phases:?}"
            );
        }
    }
}

/// The model of the units that stand (`job::standing`) on the fakes' plan of `[package]`
/// (dep's library, unit 0; the program's build script compiled, 1, and run, 2; the program, 3,
/// on 0 and 2), with the units `sealed` the fakes' sealed ones, and nothing kept yet.
fn standing_model(sealed: &[usize]) {
    STAND_MODEL.set(true);
    STAND_SEALED.set(sealed.to_vec());
    STAND_DEPS.set(vec![Vec::new(), Vec::new(), vec![1], vec![0, 2]]);
    STAND_VERSIONS.set(BTreeMap::new());
    STAND_RECORDS.set(BTreeMap::new());
    STAND_ENDED.set(Vec::new());
    STOOD.set(Vec::new());
    PHASES.with(|it| it.borrow_mut().clear());
}

fn standing_off() {
    STAND_MODEL.set(false);
    STAND_SEALED.set(Vec::new());
    STAND_DEPS.set(Vec::new());
    super::job::standing::PLANT.set(super::job::standing::Plant::None);
}

/// A build of `/w` asked as cargo asks it, with `STANDING` at `env`, settled; what it got.
fn standing_build(daemon: &mut SimDaemon, client: &mut Client, env: &str) -> Vec<Response> {
    let mut vars = command_env();
    vars.push(("STANDING".into(), env.into()));
    let cargo = blaze_api::Cargo {
        cwd: "/w".into(),
        env: vars,
        args: Vec::new(),
        root: "/w".into(),
        color: false,
        files: Vec::new(),
        width: None,
        command: "build".into(),
        refused: None,
        verbose: 0,
        prebuilt: true,
    };
    let targets = Targets { cargo: Some(cargo), ..agent() };
    let id = client.send(daemon, Request::Build { view: ViewRef::Head("/w".into()), targets });
    let got = client.wait(daemon, id);
    settle(daemon, client);
    got
}

/// An edit of unit `unit`'s sources, the `n`th: its version moves, and so does the program's
/// file, so the view moves with it.
fn standing_edit(daemon: &mut SimDaemon, unit: usize, n: u64) {
    STAND_VERSIONS.with_borrow_mut(|it| it.insert(unit, n));
    let text = format!("fn main() {{ let _ = {n}; }}\n");
    daemon.driver().change(&name("w/src/main.rs"), text.as_bytes());
}

/// Each unit's record the builds since the last take ended with, by unit, taken.
fn ended_records() -> Vec<(usize, ContentId)> {
    let mut ended = STAND_ENDED.take();
    ended.sort_by_key(|it| it.0);
    ended
}

/// The oracle: what a fresh daemon's build of the checkout as it is now (its program's file
/// the `n`th edit's, its units' versions as they are, under `STANDING` at `env`) ends each
/// unit's record with, its store of records empty. The edited daemon's store is put back.
fn fresh_records(seed: u64, n: u64, env: &str) -> Vec<(usize, ContentId)> {
    let (kept, stood, phases) = (STAND_RECORDS.take(), STOOD.take(), PHASES.with(|it| it.take()));
    let plant = super::job::standing::PLANT.replace(super::job::standing::Plant::None);
    let mut sim = Sim::new(seed, Faults::default());
    checkout(&mut sim, "[package]\n");
    sim.change(&name("w/src/main.rs"), format!("fn main() {{ let _ = {n}; }}\n").as_bytes());
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, seed);
    let mut client = dial(&mut daemon);
    let got = standing_build(&mut daemon, &mut client, env);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "the fresh daemon's build: {got:#?}");
    assert!(STOOD.with_borrow(Vec::is_empty), "a fresh daemon stands nothing");
    let fresh = ended_records();
    STAND_RECORDS.set(kept);
    STOOD.set(stood);
    PHASES.with(|it| *it.borrow_mut() = phases);
    super::job::standing::PLANT.set(plant);
    fresh
}

/// The phases unit `unit` ran since the last take of [`PHASES`], in order.
fn phases_of(ran: &[(usize, Phase)], unit: usize) -> Vec<Phase> {
    ran.iter().filter(|it| it.0 == unit).map(|it| it.1).collect()
}

/// The invariant: after an edit of one workspace crate (the program's build script and the
/// program, units 1 and 3), the sealed unit the last build kept (dep's library, 0) stands: one
/// task, its names then its store, the five phase nodes of none; the edited units and every
/// unit above them post their phases; and every unit's record equals what a fresh daemon's
/// build of the same tree ends it with (the oracle). A standing unit whose record left the
/// store fails the build naming the unit, never posts its phases quietly, and the build after
/// that failure stands nothing.
#[test]
fn a_sealed_unit_the_last_build_kept_stands_in_one_task_and_its_record_gone_fails_by_name() {
    standing_model(&[0]);
    let mut sim = Sim::new(3101, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 3101);
    let mut client = dial(&mut daemon);
    let got = standing_build(&mut daemon, &mut client, "1");
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    assert!(STOOD.with_borrow(Vec::is_empty), "the first build has nothing standing");
    let _ = (ended_records(), PHASES.with(|it| it.take()));

    standing_edit(&mut daemon, 1, 1);
    standing_edit(&mut daemon, 3, 1);
    let got = standing_build(&mut daemon, &mut client, "1");
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    let (ran, stood) = (PHASES.with(|it| it.take()), STOOD.take());
    assert_eq!(stood, [0], "dep's library stands, and only it: {ran:?}");
    assert_eq!(
        phases_of(&ran, 0),
        [Phase::Names, Phase::Store],
        "the unit that stands runs its names and its store in its one task: {ran:?}"
    );
    for unit in [1, 3] {
        let mut each = phases_of(&ran, unit);
        each.sort_unstable_by_key(|it| *it as u8);
        assert!(each == Phase::BUILD || each == Phase::CHECK, "unit {unit} ran {ran:?}");
    }
    let records = ended_records();
    assert_eq!(records, fresh_records(3102, 1, "1"), "the edited build against a fresh daemon's");

    // Its record gone from the store: the unit is posted standing, and fails naming itself.
    STAND_RECORDS.with_borrow_mut(|it| it.remove(&0));
    standing_edit(&mut daemon, 3, 2);
    let got = standing_build(&mut daemon, &mut client, "1");
    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");
    let named = got.iter().any(|it| {
        format!("{it:?}").contains("unit 0: its record stood in the last build of this plan")
    });
    assert!(named, "the build names the unit whose record is gone: {got:#?}");
    let (ran, stood) = (PHASES.with(|it| it.take()), STOOD.take());
    assert_eq!(stood, [0], "{ran:?}");
    assert_eq!(phases_of(&ran, 0), [Phase::Names], "nothing of it is made: {ran:?}");
    let _ = ended_records();

    // The failed build left nothing standing: the next posts every phase, and is right.
    standing_edit(&mut daemon, 3, 3);
    let got = standing_build(&mut daemon, &mut client, "1");
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    let (ran, stood) = (PHASES.with(|it| it.take()), STOOD.take());
    assert!(stood.is_empty(), "after a failed build nothing stands: {stood:?}");
    let mut each = phases_of(&ran, 0);
    each.sort_unstable_by_key(|it| *it as u8);
    assert_eq!(each, Phase::BUILD, "dep's library posts its phases again: {ran:?}");
    let records = ended_records();
    assert_eq!(records, fresh_records(3103, 3, "1"), "the build after the failure");
    standing_off();
}

/// The controls of the test above, each a planted bug of `job::standing` its oracle catches:
/// a sealed unit (the program, 3, sealed here) over an edited crate (dep's library, 0, here
/// the edited path crate) stood whatever its dependencies do serves its stale record; a sealed
/// unit (0) stood under a config whose environment moved serves the record of the old one.
/// Unplanted, each build's records are a fresh daemon's.
#[test]
fn a_unit_stood_over_an_edited_crate_or_a_moved_environment_is_caught_by_the_oracle() {
    use super::job::standing::{PLANT, Plant};
    // Returns the second build's records and the fresh daemon's.
    let run = |seed: u64, sealed: &[usize], edited: usize, envs: [&str; 2], plant: Plant| {
        standing_model(sealed);
        let mut sim = Sim::new(seed, Faults::default());
        checkout(&mut sim, "[package]\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        let mut client = dial(&mut daemon);
        let got = standing_build(&mut daemon, &mut client, envs[0]);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
        let _ = ended_records();
        standing_edit(&mut daemon, edited, 1);
        PLANT.set(plant);
        let got = standing_build(&mut daemon, &mut client, envs[1]);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{plant:?}: {got:#?}");
        let records = ended_records();
        let fresh = fresh_records(seed + 1, 1, envs[1]);
        standing_off();
        (records, fresh)
    };
    for (seed, sealed, edited, envs, plant) in [
        (3111, &[3][..], 0, ["1", "1"], Plant::DepsBlind),
        (3121, &[0][..], 3, ["1", "2"], Plant::IgnoresConfig),
    ] {
        let (records, fresh) = run(seed, sealed, edited, envs, Plant::None);
        assert_eq!(records, fresh, "unplanted, seed {seed}: the build is a fresh daemon's");
        let (records, fresh) = run(seed + 2, sealed, edited, envs, plant);
        assert_ne!(records, fresh, "{plant:?} is not caught: the stale record went unseen");
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
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    WARNS.set(false);

    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    let of_app = |it: &Response| matches!(it, Response::Diagnostic(it) if it.unit.is_some());
    let said: Vec<usize> = (0..got.len()).filter(|&at| of_app(&got[at])).collect();
    let ended: Vec<usize> =
        (0..got.len()).filter(|&at| matches!(got[at], Response::UnitEnded(_))).collect();
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
    let first = client
        .send(&mut daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
    assert_eq!(warnings(&client.wait(&mut daemon, first)), vec!["first\n".to_owned()]);
    for round in 0..8 {
        let saved = format!("saved before request {round}\n");
        daemon.driver().change(&name("w/Cargo.toml"), saved.as_bytes());
        let id = client.send(
            &mut daemon,
            Request::Check { view: ViewRef::Head("/w".into()), targets: agent() },
        );
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
    let id = client
        .send(&mut daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
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
    let id = client
        .send(&mut daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    assert!(
        !got.iter().any(|it| matches!(it, Response::Fetched { .. })),
        "nothing fetched again: {got:#?}"
    );
    let fetched = steps.fetched.lock().clone();
    assert!(fetched.values().all(|&it| it == 1), "each source fetched once: {fetched:?}");
    assert_eq!(
        LIBRARY_READS.get(),
        1,
        "the library's files are read once, not again by the next daemon"
    );
}

#[test]
fn an_agents_targets_are_the_views_invocation() {
    let sim = Sim::new(5, Faults::default());
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 5);
    let mut client = dial(&mut daemon);
    let base = daemon.vfs().empty();
    let ops = vec![Op::Write {
        path: "Cargo.toml".into(),
        bytes: b"agent\n".to_vec(),
        mode: blaze_vfs::Mode::Regular,
    }];
    let commit = client.send(&mut daemon, Request::Commit { head: "agents/one".into(), base, ops });
    let root = match client.wait(&mut daemon, commit).first() {
        Some(Response::Committed { root, .. }) => *root,
        other => panic!("the commit answered {other:?}"),
    };
    let targets = Targets {
        packages: vec!["app".into()],
        profile: "release".into(),
        goal: None,
        target: None,
        features: vec!["x".into()],
        all_features: false,
        no_default_features: true,
        cargo: None,
    };
    SAW.with(|it| it.borrow_mut().clear());
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("agents/one".into()), targets });
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
            // The view is opened with what the load reads of the environment
            // (`BuildConfig::planned`): `HOME`, and not `PATH`, which no plan reads.
            env: options()
                .env
                .into_iter()
                .filter(|(name, _)| blaze_units::query::read_by_plan(name))
                .collect(),
            files: Vec::new(),
        }
    );
    assert_eq!(config.library, LIBRARY);
    assert_eq!(config.host, "x86_64-unknown-linux-gnu");
    let view =
        config.mounts.iter().find(|it| it.origin == Origin::View).expect("the view is mounted");
    assert_eq!(view.at, at);
    assert!(config.mounts.windows(2).all(|it| it[0].at <= it[1].at), "mounts sorted by where");
    assert_eq!(
        daemon.driver().contents(&FileName::new(&format!("{}/Cargo.lock", &at[1..])).unwrap()),
        b"# a lock\n"
    );
}

/// A build of the head of `/w`, answered and settled, on `daemon`: what it answered.
fn build_settled(daemon: &mut SimDaemon) -> Vec<Response> {
    let mut client = dial(daemon);
    let id = client
        .send(daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(daemon, id);
    settle(daemon, &mut client);
    turn_until(daemon, |it| it.settled());
    got
}

/// What the same build asks after a stop and a start with nothing changed: how many plan
/// loads ran (each is a job's first stage: 0 is no job), the bytes the driver read for it, and
/// how it ended. `drops_marks` plants the bug the control catches: the start keeps no mark.
fn after_a_restart(seed: u64, drops_marks: bool) -> (usize, u64, Option<Response>) {
    OUTPUT.set(true);
    let mut sim = Sim::new(seed, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, seed);
    assert_eq!(build_settled(&mut daemon).last(), Some(&Response::Done { ok: true }));
    super::daemon::DROPS_MARKS.set(drops_marks);
    MEMBER_STORES.set(0);
    let mut daemon = restart(daemon, &steps, seed + 1);
    super::daemon::DROPS_MARKS.set(false);
    // R3: the stop wrote the kept front end's members, once, and the next daemon reads them.
    assert_eq!(MEMBER_STORES.get(), 1, "seed {seed}: the stop stores the kept front end's members once");
    assert!(holds_members(daemon.store(), "/w"), "seed {seed}: the members' pages are in the store after the start");
    PLANS.with(|it| it.set(0));
    let before = daemon.driver().bytes_read();
    let got = build_settled(&mut daemon);
    let read = daemon.driver().bytes_read() - before;
    OUTPUT.set(false);
    (PLANS.with(Cell::get), read, got.last().cloned())
}

/// The invariant: after a build, a stop and a start (no engine state restored), the same
/// request with nothing changed ends with no job and reads neither a source nor an output
/// again: the certificate answers, its evidence from the journal's marks (`serve::local`). It
/// answers as the build did. Needs track 2's H1-H3 (/tmp/plan/handoff-6.md): until they are
/// in, the outputs are hashed again and the bytes read say so.
#[test]
fn a_restart_with_nothing_changed_answers_from_the_certificate_with_no_job() {
    for seed in [6, 7, 8] {
        let (loads, read, done) = after_a_restart(seed, false);
        assert_eq!(done, Some(Response::Done { ok: true }), "seed {seed}");
        assert_eq!(loads, 0, "seed {seed}: the restarted daemon ran a job for a request nothing changed");
        let (_, planted, _) = after_a_restart(seed, true);
        assert!(
            read + APP.len() as u64 <= planted,
            "seed {seed}: with the marks the request read {read} bytes, without them {planted}: \
             the marks spared no read of the output"
        );
    }
}

/// The control: a start that drops the marks reads the program's output again, which the
/// oracle above counts.
#[test]
fn the_oracle_catches_a_start_that_drops_the_marks() {
    let (_, kept, _) = after_a_restart(9, false);
    let (_, dropped, done) = after_a_restart(9, true);
    assert_eq!(done, Some(Response::Done { ok: true }));
    assert!(kept < dropped, "the planted start read {dropped} bytes, the real one {kept}");
}

/// Rule 26, the edit after a start: a build after a stop, a start (database empty, nothing
/// restored) and an edit gives byte for byte what a daemon that never stopped gives for the
/// same edit, and what a fresh home gives for the edited checkout.
#[test]
fn a_restart_then_an_edit_builds_what_a_fresh_daemon_builds() {
    const EDITED: &[u8] = b"fn main() { let _ = 1; }\n";
    let edited_on = |seed: u64, restarts: bool| {
        OUTPUT.set(true);
        let mut sim = Sim::new(seed, Faults::default());
        checkout(&mut sim, "[package]\nname = \"app\"\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        assert_eq!(build_settled(&mut daemon).last(), Some(&Response::Done { ok: true }));
        if restarts {
            MEMBER_STORES.set(0);
            daemon = restart(daemon, &steps, seed + 1);
            // R3: the stop wrote the members' pages once; the edit's build reads them.
            assert_eq!(MEMBER_STORES.get(), 1, "seed {seed}: the stop stores the kept front end's members once");
            assert!(holds_members(daemon.store(), "/w"), "seed {seed}: the members' pages survive the stop");
        }
        daemon.driver().change(&name("w/src/main.rs"), EDITED);
        let got = build_settled(&mut daemon);
        let said = outcome(&mut daemon, &got);
        OUTPUT.set(false);
        said
    };
    let fresh = {
        OUTPUT.set(true);
        let mut sim = Sim::new(150, Faults::default());
        checkout(&mut sim, "[package]\nname = \"app\"\n");
        sim.change(&name("w/src/main.rs"), EDITED);
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, 150);
        let got = build_settled(&mut daemon);
        let said = outcome(&mut daemon, &got);
        OUTPUT.set(false);
        said
    };
    assert_eq!(fresh.0, Some(Response::Done { ok: true }));
    for seed in [151, 153, 155] {
        let restarted = edited_on(seed, true);
        assert_eq!(restarted, edited_on(seed, false), "seed {seed}: against the daemon that never stopped");
        assert_eq!(restarted, fresh, "seed {seed}: against a fresh home");
    }
}

/// Rule 26, an image of the layout before this one: one with the database's, the symbols',
/// the library trees' and the carry sessions' sections beside the VFS's and the history's is
/// mapped, its VFS and history used and its other sections never read (here they hold bytes
/// no reader of theirs would take), and the next request answers.
#[test]
fn an_image_of_the_previous_layout_starts_by_name() {
    use blaze_image::{Builder, Image, SectionId};
    let mut sim = Sim::new(160, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 160);
    assert_eq!(build_settled(&mut daemon).last(), Some(&Response::Done { ok: true }));
    daemon.stop();
    let mut turns = 0;
    while daemon.turn() {
        turns += 1;
        assert!(turns < TURNS, "the daemon never stopped");
    }
    let mut sim = daemon.into_driver();
    let file = name(&format!("{STATE}/db/image"));
    let written = Image::open(sim.contents(&file)).expect("the stop wrote an image");
    let mut builder = Builder::new();
    for section in written.sections() {
        builder.add(section.id, section.schema, written.whole(section).expect("a whole section"));
    }
    let junk = |n: u8| blaze_format::SchemaHash(ContentId([n; 32]));
    for (at, retired) in SectionId::RETIRED.into_iter().enumerate() {
        builder.add(retired, junk(4 + at as u8), b"not trees, not carry sessions");
    }
    sim.change(&file, &builder.finish().expect("the image of the layout before"));
    let mut daemon = start(sim, &steps, 161);
    let started = daemon.started();
    assert!(started.mapped.is_some(), "the image was mapped: {started}");
    assert!(started.whole(), "its VFS and history were used and nothing else read: {started}");
    assert_eq!(build_settled(&mut daemon).last(), Some(&Response::Done { ok: true }));
}

#[test]
fn a_daemon_stopped_as_its_first_build_answers_writes_the_image_the_next_start_restores() {
    let mut sim = Sim::new(9, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 9);
    assert!(daemon.started().mapped.is_none(), "the first start has no image");
    let mut client = dial(&mut daemon);
    let id = client
        .send(&mut daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
    assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }));
    // Stopped before the loop turns idle, where a build's image is otherwise asked.
    let mut daemon = restart(daemon, &steps, 10);
    assert!(
        daemon.driver().exists(&name(&format!("{STATE}/db/image"))),
        "the stop wrote the build's image"
    );
    // No engine state is restored, so a second daemon in one process opens it as any start.
    let started = daemon.started();
    assert!(started.mapped.is_some(), "the next start opens the stop's image: {started}");
    assert_eq!(started.unimaged, None, "{started}");
}

/// A stop while the cold build's settle image is still being made, with nothing asked since,
/// waits for that image and makes none of its own: one image, which the next start opens. A
/// request between the settle and the stop changes the daemon, so the stop makes a second,
/// fresher one (durability: never a stop that keeps an image older than what it answered).
#[test]
fn a_stop_during_an_unchanged_settle_writes_one_image_and_a_change_between_writes_a_second() {
    for changed in [false, true] {
        let seed = if changed { 72 } else { 71 };
        let mut sim = Sim::new(seed, Faults::default());
        checkout(&mut sim, "[package]\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        let mut client = dial(&mut daemon);
        let check = || Request::Check { view: ViewRef::Head("/w".into()), targets: agent() };
        let id = client.send(&mut daemon, check());
        assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }));
        turn_until(&mut daemon, |it| it.images_made() == 1);
        if changed {
            let id = client.send(&mut daemon, check());
            assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }));
        } else {
            assert!(!daemon.settled(), "the stop comes while the settle image is still being made");
        }
        daemon.stop();
        let mut turns = 0;
        while daemon.turn() {
            turns += 1;
            assert!(turns < TURNS, "the daemon never stopped");
        }
        let made = daemon.images_made();
        assert_eq!(
            made,
            if changed { 2 } else { 1 },
            "changed between the settle and the stop: {changed}"
        );
        let daemon = start(daemon.into_driver(), &steps, seed + 10);
        let started = daemon.started();
        assert!(started.mapped.is_some(), "the next start opens the stop's image: {started}");
    }
}

/// What a build leaves for the store as its front end is kept ([`Steps::store_kept`]) is
/// durable once it has settled ([`settle`]: its answer does not wait for it), a failed build's
/// too: a power loss then (no stop, no image) loses none of it, and the next start finds it
/// in the store.
///
/// A crash keeps a random prefix of what was not synced (`Sim::crash`), so the put could
/// survive one seed by chance: across these seeds, only a sync before it is settled keeps it
/// in every one.
#[test]
fn what_a_build_left_for_the_store_is_durable_once_it_settles_and_survives_a_crash() {
    let runs = (73..81).flat_map(|seed| [(seed, None), (seed + 100, Some(1))]);
    for (seed, errs) in runs {
        ERRS_COMPILE.set(errs);
        STORES.set(0);
        let mut sim = Sim::new(seed, Faults::default());
        checkout(&mut sim, "[package]\nname = \"app\"\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        let mut client = dial(&mut daemon);
        let id = client.send(
            &mut daemon,
            Request::Build { view: ViewRef::Head("/w".into()), targets: agent() },
        );
        let got = client.wait(&mut daemon, id);
        ERRS_COMPILE.set(None);
        assert_eq!(got.last(), Some(&Response::Done { ok: errs.is_none() }), "{got:#?}");
        settle(&mut daemon, &mut client);
        assert!(daemon.lost().is_empty(), "seed {seed}: {:#?}", daemon.lost());
        assert_eq!(STORES.get(), 1, "the kept front end is stored once, after its build's answer");
        assert!(holds_kept(daemon.store(), "/w"), "it is in the store once the build has settled");
        let mut sim = daemon.into_driver();
        sim.crash();
        let daemon = start(sim, &steps, seed + 1000);
        assert!(
            holds_kept(daemon.store(), "/w"),
            "a crash after it settled keeps it (failing build: {errs:?})"
        );
    }
}

/// The `errors` rows a build of `/w` whose build script's compile errs gave, and how it ended.
fn failed_build_rows(seed: u64) -> (Option<Response>, Vec<(u64, String, String)>) {
    use super::job::ERROR_ROWS;
    ERROR_ROWS.with_borrow_mut(Vec::clear);
    ERRS_COMPILE.set(Some(1));
    let mut sim = Sim::new(seed, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, seed);
    let mut client = dial(&mut daemon);
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    ERRS_COMPILE.set(None);
    settle(&mut daemon, &mut client);
    (got.last().cloned(), ERROR_ROWS.with_borrow_mut(std::mem::take))
}

/// The invariant: a build that ends not ok and was not cancelled has a row in the history's
/// `errors` table saying what failed: here the error of the unit whose compile erred, as
/// the build said it. `builds.outcome = 'failed'` beside an empty `errors` is a failure
/// with no cause to read (the cold build of ripgrep, 2026-11-11).
#[test]
fn a_build_that_fails_has_its_error_among_the_historys_error_rows() {
    for seed in 190..194 {
        let (done, rows) = failed_build_rows(seed);
        assert_eq!(done, Some(Response::Done { ok: false }), "seed {seed}");
        assert!(!rows.is_empty(), "seed {seed}: the build failed and gave the history no error");
        assert!(
            rows.iter().all(|(_, kind, message)| !kind.is_empty() && !message.is_empty()),
            "seed {seed}: a row with no kind or no message: {rows:#?}"
        );
        let build = rows[0].0;
        assert!(rows.iter().all(|it| it.0 == build), "seed {seed}: rows of two builds: {rows:#?}");
    }
}

/// The planted-bug control: a daemon that notes no job's errors, as it was, gives a failed
/// build no row.
#[test]
fn the_oracle_catches_a_failed_build_with_no_error_row() {
    use super::job::PLANT_NO_ERROR_ROWS;
    PLANT_NO_ERROR_ROWS.set(true);
    let (done, rows) = failed_build_rows(190);
    PLANT_NO_ERROR_ROWS.set(false);
    assert_eq!(done, Some(Response::Done { ok: false }));
    assert!(rows.is_empty(), "the plant notes nothing, and the oracle's check would fail: {rows:#?}");
}

/// How many units each of two builds of `/w` sized the sources of, a source file edited
/// between them (so the view's root moves and the plan is asked again, the same as it was).
fn sized_across_an_edit(seed: u64) -> Vec<u32> {
    use super::job::SIZED_UNITS;
    SIZED_UNITS.with_borrow_mut(Vec::clear);
    let mut sim = Sim::new(seed, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, seed);
    let mut client = dial(&mut daemon);
    for text in [None, Some(&b"fn main() { }\n"[..])] {
        if let Some(text) = text {
            daemon.driver().change(&name("w/src/main.rs"), text);
        }
        let id = client.send(
            &mut daemon,
            Request::Build { view: ViewRef::Head("/w".into()), targets: agent() },
        );
        let got = client.wait(&mut daemon, id);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
        settle(&mut daemon, &mut client);
    }
    SIZED_UNITS.with_borrow_mut(std::mem::take)
}

/// The invariant: a build whose plan is the units its checkout's last build had (an edit of
/// a source file: the plan is asked again and is the same) walks no unit's source
/// directories again: what is kept by the units is found by them, because equal units of one
/// checkout are one value. The walk was of every unit's sources, the standard library's and
/// each dependency's among them, on the loop's thread before any unit was posted.
#[test]
fn an_edit_that_leaves_the_plan_as_it_was_sizes_no_unit_again() {
    let sized = sized_across_an_edit(200);
    assert_eq!(sized.len(), 2, "two builds: {sized:?}");
    assert!(sized[0] > 0, "the first build of a plan sizes its units: {sized:?}");
    assert_eq!(sized[1], 0, "the build after the edit sized units again: {sized:?}");
}

/// The planted-bug control: a plan asked again taken as units of their own, as it was, and
/// every unit is sized again after the edit.
#[test]
fn the_oracle_catches_a_plan_asked_again_taken_as_new_units() {
    use super::job::PLANT_PLAN_ANEW;
    PLANT_PLAN_ANEW.set(true);
    let sized = sized_across_an_edit(200);
    PLANT_PLAN_ANEW.set(false);
    assert_eq!(sized.len(), 2, "two builds: {sized:?}");
    assert_eq!(sized[1], sized[0], "the oracle missed units sized again after an edit: {sized:?}");
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

/// Turns the daemon until nothing an answered build left is still settling: its kept front
/// end stored and back with its checkout, and its puts durable or said lost.
fn settle(daemon: &mut SimDaemon, client: &mut Client) {
    for _ in 0..TURNS {
        if daemon.jobs_settling() == 0 {
            return;
        }
        daemon.turn();
        client.take(daemon);
    }
    panic!("what the answered builds left never settled: {} still settle", daemon.jobs_settling());
}

/// How many times request `id` was told it is done: once, whatever settles after.
fn dones(client: &Client, id: u64) -> usize {
    client.got.iter().filter(|(at, it)| *at == id && matches!(it, Response::Done { .. })).count()
}

/// Review C1, with two segments: a put of a value the store keeps already is handed the
/// ticket of the put that kept it, from any writer's segment. Here a writer of its own (a
/// segment above the loop's) puts the value every checkout leaves first, and `/v`'s build
/// makes it durable. `/w`'s build then puts its own two on the loop's segment and that value
/// last, handed the writer's ticket, durable already: one `max` of its tickets is that one, so
/// what waited on it alone was settled before its own were durable and a crash lost them.
/// Across seeds, as a crash keeps a random prefix of what was not synced.
#[test]
fn a_checkout_whose_last_put_another_put_first_settles_once_its_own_are_durable() {
    for seed in 120..136 {
        let mut sim = Sim::new(seed, Faults::default());
        checkout(&mut sim, "[package]\nname = \"app\"\n");
        second_checkout(&mut sim);
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        let mut writer = daemon.store_mut().writer();
        let guard = blaze_cas::enter();
        let first =
            writer.put(&guard, shared_kept_key(), blaze_cas::Kind::Query, SHARED_KEPT).unwrap();
        drop(guard);
        let mut client = dial(&mut daemon);
        for head in ["/v", "/w"] {
            let id = client.send(
                &mut daemon,
                Request::Build { view: ViewRef::Head(head.into()), targets: agent() },
            );
            assert_eq!(
                client.wait(&mut daemon, id).last(),
                Some(&Response::Done { ok: true }),
                "{head}"
            );
            settle(&mut daemon, &mut client);
            // `/v` rests on the writer's put, so what it left settled only once that was durable.
            assert!(
                daemon.store().durable(first),
                "seed {seed}: {head} settled before the value it put was durable"
            );
        }
        let mut sim = daemon.into_driver();
        sim.crash();
        let daemon = start(sim, &steps, seed + 1000);
        assert!(
            holds_kept(daemon.store(), "/w") && holds_kept(daemon.store(), "/v"),
            "seed {seed}"
        );
        drop(writer);
    }
}

/// Wait::Everything (the store model's finding), for what settles after an answer: a value
/// read from the store comes with no ticket, so a job that put nothing new may have read
/// another job's value that is not durable yet. With the store's syncs held, `/w`'s build puts
/// what it leaves and can make none of it durable; a build of `/v` that puts nothing then
/// answers, as `/w` did, and what it left settles only with every put the loop made before it
/// ended. At the bound each is a loss naming the store's ticket, and neither is answered again.
#[test]
fn what_a_build_that_puts_nothing_left_settles_with_what_the_loop_put_before_it_ended() {
    let mut sim = Sim::new(180, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    second_checkout(&mut sim);
    PUTS_NOTHING.with_borrow_mut(|it| *it = "/v".to_owned());
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 180);
    let mut client = dial(&mut daemon);
    daemon.driver().hold_syncs(store_dir());
    let start = daemon.driver().now();
    let w = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    for _ in 0..TURNS {
        if holds_kept(daemon.store(), "/w") {
            break;
        }
        daemon.turn();
        client.take(&mut daemon);
    }
    assert!(holds_kept(daemon.store(), "/w"), "the build of /w never put what it left");
    let v = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/v".into()), targets: agent() });
    turns(&mut daemon, &mut client, 3000);
    for (head, id) in [("/v", v), ("/w", w)] {
        let got = client.of(id);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{head}: {got:#?}");
    }
    assert_eq!(
        daemon.jobs_settling(),
        2,
        "/v put nothing, and what it may have read of the loop's puts is not durable: it settles too"
    );
    assert!(daemon.lost().is_empty(), "{:#?}", daemon.lost());
    // The waits above ran on the clock's own pace, far short of the bound.
    let ran = daemon.driver().now() - start;
    assert!(ran < Duration::from_secs(1), "the turns took {ran:?} of the clock before the advance");
    let ((), reported) = stdx::assert::reported::expecting("settling", || {
        for _ in 0..10 {
            if daemon.jobs_settling() == 0 {
                break;
            }
            daemon.driver().advance(DURABLE_WITHIN);
            turns(&mut daemon, &mut client, 50);
        }
    });
    let lost = daemon.lost().to_vec();
    assert_eq!((lost.len(), reported), (2, 2), "{lost:#?}");
    for head in ["/v", "/w"] {
        let named = |it: &&super::job::Lost| {
            it.checkout.as_deref() == Some(head)
                && it.what.contains("store ticket")
                && it.what.contains("answered nothing")
        };
        assert!(lost.iter().any(|it| named(&it)), "{head}: {lost:#?}");
    }
    assert_eq!((dones(&client, v), dones(&client, w)), (1, 1), "a loss is never a second answer");
    PUTS_NOTHING.with_borrow_mut(String::clear);
}

/// The daemon's side of the store model's invariant on answers sent before their results are
/// durable (stated, with its controls, in `crates/blaze-storemodel/src/tests.rs`,
/// `what_is_kept_after_an_answer_survives_any_crash_and_no_lost_value_is_served`): what is
/// lost is said, never dropped silently and never a second answer.
///
/// A store whose commit hangs (a sync that never returns) holds no build's answer: the build
/// is answered at once, and what it left settles after. Once the store has answered nothing
/// for [`DURABLE_WITHIN`] on the injected clock, not before, that is given up as a loss: one
/// line naming the checkout, the command and the store's ticket, one bug for `blaze status`,
/// and a miss of the checkout's next build that `blaze why` says. The daemon keeps serving.
#[test]
fn a_build_answers_before_its_puts_are_durable_and_a_commit_that_hangs_is_a_loss_said_once() {
    ENDED_LOST.with(|it| it.borrow_mut().clear());
    let mut sim = Sim::new(190, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 190);
    let mut client = dial(&mut daemon);
    daemon.driver().hold_syncs(store_dir());
    let start = daemon.driver().now();
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    turns(&mut daemon, &mut client, 3000);
    assert!(daemon.driver().held_syncs() > 0, "the store's commit is held");
    assert_eq!(daemon.jobs_settling(), 1, "what it left waits for the store's commit");
    assert_eq!(daemon.storing(), 0, "its kept front end is back with its checkout");
    // So that "short of the bound" below is short of it: the build itself took next to none
    // of the clock.
    let ran = daemon.driver().now() - start;
    assert!(ran < Duration::from_secs(1), "the build took {ran:?} of the clock before the advance");
    // Short of the bound since the store was last heard from: it still waits.
    daemon.driver().advance(DURABLE_WITHIN - Duration::from_secs(1));
    turns(&mut daemon, &mut client, 50);
    assert!(daemon.lost().is_empty(), "gave up before the bound: {:#?}", daemon.lost());
    assert_eq!(daemon.jobs_settling(), 1);
    let ((), reported) = stdx::assert::reported::expecting("settling", || {
        for _ in 0..10 {
            if daemon.jobs_settling() == 0 {
                break;
            }
            daemon.driver().advance(Duration::from_secs(1).max(DURABLE_WITHIN / 10));
            turns(&mut daemon, &mut client, 50);
        }
    });
    let lost = daemon.lost().to_vec();
    assert_eq!(lost.len(), 1, "{lost:#?}");
    assert_eq!(reported, 1, "the loss is one bug `blaze status` shows");
    assert_eq!(lost[0].checkout.as_deref(), Some("/w"), "{lost:#?}");
    assert!(!lost[0].command.is_empty(), "{lost:#?}");
    assert!(
        lost[0].what.contains("the build of /w")
            && lost[0].what.contains("answered nothing")
            && lost[0].what.contains("store ticket"),
        "{lost:#?}"
    );
    assert_eq!(dones(&client, id), 1, "the answer sent stands: {:#?}", client.of(id));
    assert_eq!(client.of(id).last(), Some(&Response::Done { ok: true }));
    // The daemon serves on, and the checkout's next build says what was not kept and why.
    // The commit is let go first, so nothing more is lost while it builds.
    daemon.driver().release_held(Duration::ZERO);
    let again = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, again);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    let said = ENDED_LOST.with(|it| it.take());
    assert!(
        said.iter().any(|it| it.contains("the last build of /w")
            && it.contains("was not kept")
            && it.contains("answered nothing")),
        "the next build's misses, for `blaze why`: {said:#?}"
    );
    let status = client.send(&mut daemon, Request::Status);
    let said = client.wait(&mut daemon, status);
    assert!(!said.is_empty(), "the daemon serves after the loss");
}

/// Bounded progress is no progress, not total time: a store whose commit is slow but moving
/// (each write and sync of it taking well under [`DURABLE_WITHIN`], the commit's steps one
/// after another taking well over it in all) is waited for by what the build left to settle,
/// and nothing is lost. A bound on the whole wait, or one that counted from the wait's start
/// alone, gives it up.
#[test]
fn what_a_build_left_settles_past_the_bound_while_the_stores_commit_is_slow_but_moving() {
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
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    settle(&mut daemon, &mut client);
    let took = daemon.driver().now() - start;
    assert!(
        took > DURABLE_WITHIN,
        "the commit took {took:?}, not past the bound: the test tests nothing"
    );
    assert!(daemon.lost().is_empty(), "a commit that moves is waited for: {:#?}", daemon.lost());
    assert!(holds_kept(daemon.store(), "/w"), "settled, and what it left is not in the store");
    assert_eq!(dones(&client, id), 1);
}

/// The backstop of Wait::Everything ([`blaze_cas::Store::last_puts`]): a put that no caller
/// hands its ticket over for (the tests' run here keeps how they ran and tells no one, as an
/// error path that put before it failed does) is still durable before what the job left is
/// settled, so a crash after that keeps it. Across seeds, as a crash keeps a random prefix of
/// what was not synced. Without the backstop it waits only on what it was told of, which its
/// earlier puts made durable already, and a crash loses the record.
#[test]
fn a_put_no_one_hands_over_is_durable_once_its_job_settles() {
    TESTS_KEEP.set(true);
    TESTS_KEEP_UNTOLD.set(true);
    for seed in 220..236 {
        let mut sim = Sim::new(seed, Faults::default());
        checkout(&mut sim, "[package]\nname = \"app\"\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        let mut client = dial(&mut daemon);
        let request = Request::Test {
            view: ViewRef::Head("/w".into()),
            targets: agent(),
            filter: Vec::new(),
            all: false,
            args: Vec::new(),
            ub: None,
        };
        let id = client.send(&mut daemon, request);
        let got = client.wait(&mut daemon, id);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
        assert!(
            daemon.store().get(&test_record_key()).unwrap().is_some(),
            "seed {seed}: the run kept nothing"
        );
        settle(&mut daemon, &mut client);
        assert!(daemon.lost().is_empty(), "seed {seed}: {:#?}", daemon.lost());
        let mut sim = daemon.into_driver();
        sim.crash();
        let daemon = start(sim, &steps, seed + 1000);
        let kept = daemon
            .store()
            .get(&test_record_key())
            .unwrap()
            .is_some_and(|it| it.bytes == TEST_RECORD);
        assert!(kept, "seed {seed}: the job settled and a crash lost a put no one handed over");
    }
    TESTS_KEEP_UNTOLD.set(false);
    TESTS_KEEP.set(false);
}

/// [`a_put_no_one_hands_over_is_durable_once_its_job_settles`], with the untold put made
/// through a writer of the store's own ([`blaze_cas::Store::writer`], as a worker's or a
/// [`blaze_cas::Handle`]'s is): a segment other than the loop's. The backstop covers every
/// segment; one that covered the loop's alone was settled before this put was durable, and a
/// crash lost it.
#[test]
fn a_put_no_one_hands_over_through_another_segment_is_durable_once_its_job_settles() {
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
        let request = Request::Test {
            view: ViewRef::Head("/w".into()),
            targets: agent(),
            filter: Vec::new(),
            all: false,
            args: Vec::new(),
            ub: None,
        };
        let id = client.send(&mut daemon, request);
        let got = client.wait(&mut daemon, id);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
        let written = UNTOLD_WRITER.with_borrow_mut(Option::take).expect("the writer is still set");
        let put = daemon.store().get(&test_record_key()).unwrap().is_some();
        assert!(put, "seed {seed}: the run kept nothing");
        settle(&mut daemon, &mut client);
        assert!(daemon.lost().is_empty(), "seed {seed}: {:#?}", daemon.lost());
        let mut sim = daemon.into_driver();
        sim.crash();
        drop(written);
        let daemon = start(sim, &steps, seed + 1000);
        let kept = daemon
            .store()
            .get(&test_record_key())
            .unwrap()
            .is_some_and(|it| it.bytes == TEST_RECORD);
        assert!(
            kept,
            "seed {seed}: the job settled and a crash lost a put no one handed over, made on another segment"
        );
    }
    TESTS_KEEP_UNTOLD.set(false);
    TESTS_KEEP.set(false);
}

/// Review 6 T3, the daemon's half of D2: a put counted and not yet published (another
/// thread's, mid-way, held here with [`blaze_cas::Writer::hold_put`]) is waited for by what an
/// answered build left to settle, though no commit takes it, and once it ends, with nothing
/// else in flight, that settles within a few [`RECHECK`]s. The flush on every visit drains
/// it, and the recheck deadline is what wakes the loop: without either, it sits until the
/// bound ([`DURABLE_WITHIN`]) and is given up as lost. The build's answer waits for none of it.
#[test]
fn a_put_counted_and_not_yet_published_is_settled_soon_after_it_ends() {
    let mut sim = Sim::new(267, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 267);
    let mut writer = daemon.store_mut().writer();
    let held = writer.hold_put();
    let mut client = dial(&mut daemon);
    let start = daemon.driver().now();
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    turns(&mut daemon, &mut client, 3000);
    assert!(daemon.store_deadline().is_some(), "what the build left waits for the store");
    for _ in 0..20 {
        daemon.driver().advance(RECHECK);
        turns(&mut daemon, &mut client, 5);
    }
    assert_eq!(
        daemon.jobs_settling(),
        1,
        "settled while a put the build could have read is not published"
    );
    let ran = daemon.driver().now() - start;
    assert!(ran < DURABLE_WITHIN / 2, "the wait ran {ran:?} of the clock while held");
    // The put ends with nothing else of the store's in flight: only the recheck wakes the
    // loop, and only its flush takes the put.
    let ended = daemon.driver().now();
    drop(held);
    for _ in 0..10 {
        if daemon.jobs_settling() == 0 {
            break;
        }
        daemon.driver().advance(RECHECK);
        turns(&mut daemon, &mut client, 5);
    }
    assert_eq!(daemon.jobs_settling(), 0, "not settled soon after the put ended");
    assert!(daemon.lost().is_empty(), "{:#?}", daemon.lost());
    let took = daemon.driver().now() - ended;
    assert!(took <= RECHECK * 10, "settled {took:?} after the put ended");
    assert_eq!(dones(&client, id), 1);
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
    let request = Request::Test {
        view: ViewRef::Head("/w".into()),
        targets: agent(),
        filter: Vec::new(),
        all: false,
        args: Vec::new(),
        ub: None,
    };
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
    daemon.store_mut().put(key, blaze_cas::Kind::Query, b"a put whose commit fails\n").unwrap();
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
    assert!(
        took < Duration::from_secs(3600),
        "not answered while its run was still going ({took:?} on the clock): {got:#?}"
    );
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
    assert!(
        took > bound && took < bound + Duration::from_secs(1),
        "stopped {took:?} after it was asked; its bound for last sends is {bound:?}"
    );
    assert!(
        daemon.driver().peer_hung_up_on(client.conn),
        "the stop left the client that stopped reading connected"
    );
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
/// tick of the bound itself), and what the build left is given up as lost once the commit has
/// stood still for [`DURABLE_WITHIN`] all the same, and not before. A rule that counted any
/// completion of the store kept it waiting for ever.
#[test]
fn a_hung_commit_is_a_loss_at_the_bound_though_the_store_answers_placements_meanwhile() {
    let mut sim = Sim::new(198, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 198);
    let mut client = dial(&mut daemon);
    daemon.driver().hold_syncs(store_dir());
    let start = daemon.driver().now();
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    turns(&mut daemon, &mut client, 3000);
    assert!(daemon.driver().held_syncs() > 0, "the store's commit is held");
    let ran = daemon.driver().now() - start;
    assert!(ran < Duration::from_secs(1), "the build took {ran:?} of the clock before the advance");
    let deadline = daemon.store_deadline().expect("what the build left waits for the store");
    assert!(
        deadline >= start + DURABLE_WITHIN,
        "the wait's next deadline {deadline:?} is no bound from {start:?}"
    );
    let mut placed = 0;
    let ((), reported) = stdx::assert::reported::expecting("settling", || {
        for quarter in 0..12 {
            if !daemon.lost().is_empty() {
                break;
            }
            daemon.driver().advance(DURABLE_WITHIN / 4);
            assert!(
                daemon
                    .place_for_test(&kept_key("/w", 0), name(&format!("placed/{quarter}")))
                    .is_some(),
                "a placement asked"
            );
            turns(&mut daemon, &mut client, 50);
            placed += 1;
        }
    });
    // The wait began within the opening turns, under a second in, so the bound falls on the
    // fourth quarter: the placement asked then answers in the bound's own tick, dispatched
    // before the turn's expiry judges the wait (`>=` in `gave_up`), and it is given up there.
    assert!(
        placed >= 4,
        "the store answered placements every quarter up to the bound ({placed} quarters)"
    );
    let lost = daemon.lost().to_vec();
    assert_eq!((lost.len(), reported), (1, 1), "{lost:#?}");
    assert!(
        lost[0].what.contains("answered nothing") && lost[0].what.contains("store ticket"),
        "{lost:#?}"
    );
    let now = daemon.driver().now();
    assert!(now >= deadline, "given up at {now:?}, before the bound at {deadline:?}");
    let took = now - start;
    assert!(took < DURABLE_WITHIN * 2, "given up at {took:?}, not at the bound");
    assert_eq!(dones(&client, id), 1, "{:#?}", client.of(id));
}

/// Review C1 and W3, at the boundary (`slow_writes` and `release_held` set to the deadline,
/// [`DURABLE_WITHIN`] from the wait's start): the commit's held syncs are let go to answer
/// exactly at the bound, in the same tick as placements, other store traffic, that wind the jobs up
/// as they are dispatched. Only [`Jobs::expire`](super::job::Jobs::expire), after every
/// completion of the turn, judges the time, so the step that answered counts and what the
/// build left settles with nothing lost. Judged inside a placement's dispatch (when it comes
/// first; the seed shuffles them) or before the turn's completions, it was given up.
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
        let id = client.send(
            &mut daemon,
            Request::Build { view: ViewRef::Head("/w".into()), targets: agent() },
        );
        let got = client.wait(&mut daemon, id);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
        turns(&mut daemon, &mut client, 3000);
        assert_eq!(daemon.jobs_settling(), 1, "seed {seed}: what it left waits for the commit");
        assert!(daemon.driver().held_syncs() > 0, "seed {seed}: the store's commit is held");
        // The syncs and the placements all answer at the deadline itself: the bound from the
        // tick the wait began, less what the clock has run since.
        let deadline = daemon.store_deadline().expect("what the build left waits for the store");
        let after = deadline - daemon.driver().now();
        assert!(
            after > DURABLE_WITHIN / 2 && after <= DURABLE_WITHIN,
            "seed {seed}: the wait's deadline is {after:?} away"
        );
        daemon.driver().record(vec![store_dir(), name("placed")]);
        // A place of a value's bytes is steps (blaze-cas `publish::Writing`): a clear of its
        // staged name, which takes the disk's own latency, a cut to its length, then its
        // writes. The cut and the writes are slowed; the writes, asked as the cuts end, are
        // timed from where the first clears ended so that they end at the deadline itself.
        let cut = after / 4;
        daemon.driver().slow_writes(name("placed"), cut);
        let asked = daemon.driver().now();
        for at in 0..4 {
            assert!(
                daemon.place_for_test(&kept_key("/w", 0), name(&format!("placed/{at}"))).is_some(),
                "a placement asked"
            );
        }
        for _ in 0..TURNS {
            if daemon.driver().now() > asked {
                break;
            }
            daemon.turn();
            client.take(&mut daemon);
        }
        let cleared = daemon.driver().now();
        assert!(
            cleared > asked && cleared - asked < cut,
            "seed {seed}: the first clears were told {:?} after the placements were asked",
            cleared - asked
        );
        daemon.driver().slow_writes(name("placed"), deadline - cleared - cut);
        daemon.driver().release_held(deadline - cleared);
        settle(&mut daemon, &mut client);
        assert!(daemon.lost().is_empty(), "seed {seed}: {:#?}", daemon.lost());
        assert_eq!(dones(&client, id), 1, "seed {seed}");
        let recorded = daemon.driver().recorded();
        let placed = name("placed");
        let sync_at =
            recorded.iter().position(|(_, file, sync)| *sync && file.is_under(&store_dir()));
        let sync_at = sync_at
            .unwrap_or_else(|| panic!("seed {seed}: no sync of the store was told: {recorded:?}"));
        let tick = recorded[sync_at].0;
        assert_eq!(
            tick, deadline,
            "seed {seed}: the released sync was told at {tick:?}, not at the deadline"
        );
        let before = recorded[..sync_at]
            .iter()
            .any(|(at, file, sync)| *at == tick && !*sync && file.is_under(&placed));
        placement_first += usize::from(before);
    }
    assert!(
        placement_first > 0,
        "no seed told a placement before the commit's sync in the deadline's tick: the test tests nothing"
    );
}

/// A store that breaks (its syncs fail until it gives up) never makes a build's puts durable.
/// The build is told once, whichever comes first: its answer, with what it left then given up
/// at once as a loss naming why (not at the bound); or, when the break is seen while it still
/// runs, the break as its error. The daemon then stops, failed naming its store.
#[test]
fn a_build_whose_store_breaks_is_answered_once_and_the_break_is_said() {
    let mut sim = Sim::new(195, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 195);
    let mut client = dial(&mut daemon);
    daemon.driver().fail_syncs(store_dir());
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let ((), reported) = stdx::assert::reported::expecting("settling", || {
        client.wait(&mut daemon, id);
        for _ in 0..TURNS {
            if daemon.store().broken().is_some() && daemon.jobs_settling() == 0 {
                break;
            }
            daemon.turn();
            client.take(&mut daemon);
        }
    });
    assert!(daemon.store().broken().is_some(), "the store broke");
    assert_eq!(daemon.jobs_settling(), 0, "nothing waits on a store that broke");
    let got = client.of(id);
    assert_eq!(dones(&client, id), 1, "{got:#?}");
    let lost = daemon.lost().to_vec();
    match got.last() {
        Some(Response::Done { ok: true }) => {
            assert_eq!((lost.len(), reported), (1, 1), "{lost:#?}");
            assert!(
                lost[0].checkout.as_deref() == Some("/w")
                    && lost[0].what.contains("Input/output error"),
                "{lost:#?}"
            );
        }
        Some(Response::Done { ok: false }) => {
            let named = |it: &Response| {
                matches!(it, Response::Error(error) if error.what.contains("the daemon's store")
                    && error.what.contains("Input/output error"))
            };
            assert!(got.iter().any(named), "{got:#?}");
        }
        other => panic!("the build was never answered: {other:?} of {got:#?}"),
    }
    // And the daemon stops, naming its store, as it does for the shared one: the next
    // request starts a fresh daemon rather than build on a store that can't commit.
    let failed = daemon.failed().map(ToString::to_string);
    assert!(
        failed.as_deref().is_some_and(
            |it| it.contains("the daemon's store") && it.contains("Input/output error")
        ),
        "{failed:?}"
    );
}

/// A snapshot is asked for durability, and still waits for it: asked after a build answered
/// with the store's commit held, it is not answered while what that build put is not durable,
/// and is once the commit is let go.
#[test]
fn a_snapshot_still_waits_for_what_answered_builds_put_to_be_durable() {
    let mut sim = Sim::new(196, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 196);
    let mut client = dial(&mut daemon);
    daemon.driver().hold_syncs(store_dir());
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    let snapshot = client.send(&mut daemon, Request::Snapshot);
    turns(&mut daemon, &mut client, 3000);
    assert!(daemon.driver().held_syncs() > 0, "the store's commit is held");
    assert!(
        !client.done(snapshot),
        "a snapshot answered with what the build put not durable: {:#?}",
        client.of(snapshot)
    );
    assert!(holds_kept(daemon.store(), "/w"), "the build's kept front end was stored meanwhile");
    daemon.driver().release_held(Duration::ZERO);
    let got = client.wait(&mut daemon, snapshot);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    assert!(got.iter().any(|it| matches!(it, Response::Snapshotted { .. })), "{got:#?}");
    settle(&mut daemon, &mut client);
    assert!(daemon.lost().is_empty(), "{:#?}", daemon.lost());
    let mut sim = daemon.into_driver();
    sim.crash();
    let daemon = start(sim, &steps, 1196);
    assert!(holds_kept(daemon.store(), "/w"), "a crash after the snapshot keeps what was put");
}

/// Review round 2, C1 and C2: what user code run by the job put (here the tests' run keeping
/// how they ran, through `Runs::take_puts` as `UnitRuns` forwards it) is durable once what
/// the job left has settled, so a crash after that keeps it. Across seeds, as a crash keeps a
/// random prefix of what was not synced.
#[test]
fn what_a_run_put_is_durable_once_its_job_settles() {
    TESTS_KEEP.set(true);
    for seed in 200..216 {
        let mut sim = Sim::new(seed, Faults::default());
        checkout(&mut sim, "[package]\nname = \"app\"\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        let mut client = dial(&mut daemon);
        let request = Request::Test {
            view: ViewRef::Head("/w".into()),
            targets: agent(),
            filter: Vec::new(),
            all: false,
            args: Vec::new(),
            ub: None,
        };
        let id = client.send(&mut daemon, request);
        let got = client.wait(&mut daemon, id);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
        assert!(
            daemon.store().get(&test_record_key()).unwrap().is_some(),
            "seed {seed}: the run kept nothing"
        );
        settle(&mut daemon, &mut client);
        assert!(daemon.lost().is_empty(), "seed {seed}: {:#?}", daemon.lost());
        let mut sim = daemon.into_driver();
        sim.crash();
        let daemon = start(sim, &steps, seed + 1000);
        let kept = daemon
            .store()
            .get(&test_record_key())
            .unwrap()
            .is_some_and(|it| it.bytes == TEST_RECORD);
        assert!(kept, "seed {seed}: the job settled and a crash lost what its run put");
    }
    TESTS_KEEP.set(false);
}

/// What a build of `/w` gives that a restart must not change: whether it is done, and the
/// bytes of its program and of each value it left for the store.
fn outcome(
    daemon: &mut SimDaemon,
    got: &[Response],
) -> (Option<Response>, Vec<u8>, Vec<Option<Vec<u8>>>) {
    let app = daemon.driver().contents(&name(&APP_AT[1..]));
    let keys = [kept_key("/w", 0), kept_key("/w", 1), shared_kept_key()];
    let kept = keys
        .iter()
        .map(|key| daemon.store().get(key).unwrap().map(|it| it.bytes.to_vec()))
        .collect();
    (got.last().cloned(), app, kept)
}

/// The daemon's side of the store model's invariant on answers sent before their results are
/// durable (stated, with its controls, in `crates/blaze-storemodel/src/tests.rs`): after a
/// crash at any turn, the restore serves no torn or half-kept value, and a build on it equals
/// a fresh build.
///
/// Rule 26, at every point: two checkouts build at once, so one's answer may rest on what the
/// other put and has not made durable (a value read from the store carries no ticket); the
/// daemon crashes after turn `k`, for every `k` up to both answered and settled, so at every
/// point between an answer and its results being durable. After each crash, once nothing was
/// still settling every checkout whose `Done` the client saw has all it left in the store; and
/// at every point, settled or not, a build of `/w` on the restored disk gives byte for byte
/// what a fresh daemon's gives: no torn or half-kept value is refused as a mismatch, and what
/// was lost is made again. `Sim::crash` keeps a seeded prefix of each file's unsynced writes; it does not
/// model directory entries (lane storemodel's `CrashFs` does).
#[test]
fn a_crash_after_any_turn_keeps_what_had_settled_and_restores_to_a_fresh_build() {
    OUTPUT.set(true);
    let fresh = {
        let mut sim = Sim::new(140, Faults::default());
        checkout(&mut sim, "[package]\nname = \"app\"\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, 140);
        let mut client = dial(&mut daemon);
        let id = client.send(
            &mut daemon,
            Request::Build { view: ViewRef::Head("/w".into()), targets: agent() },
        );
        let got = client.wait(&mut daemon, id);
        // What it left is in the store once it has settled, not as it answers.
        settle(&mut daemon, &mut client);
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
        let builds = ["/w", "/v"]
            .map(|head| Request::Build { view: ViewRef::Head(head.into()), targets: agent() });
        let ids = client.send_together(&mut daemon, builds.into());
        (daemon, client, ids)
    };
    // The turns an uncrashed run takes until both answer and what they left has settled: the
    // crash points.
    let turns = {
        let steps = Arc::new(Fake::default());
        let (mut daemon, mut client, ids) = begin(&steps);
        let mut turns = 0;
        while !(ids.iter().all(|id| client.done(*id)) && daemon.jobs_settling() == 0) {
            assert!(turns < TURNS, "the builds never ended and settled");
            daemon.turn();
            client.take(&mut daemon);
            turns += 1;
        }
        turns
    };
    // Crash points where a checkout had answered; of those, ones where the other had not, so
    // one's answer may rest on the other's puts not yet durable; ones where something still
    // settled, between an answer and its results being durable; and ones where all had
    // settled, so the check that it kept what it left bites.
    let (mut answered_any, mut answered_one) = (0, 0);
    let (mut unsettled, mut settled) = (0, 0);
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
        let all_settled = daemon.jobs_settling() == 0;
        unsettled += usize::from(!answered.is_empty() && !all_settled);
        settled += usize::from(!answered.is_empty() && all_settled);
        assert!(daemon.lost().is_empty(), "crash after turn {k}: {:#?}", daemon.lost());
        let mut sim = daemon.into_driver();
        sim.crash();
        let mut daemon = start(sim, &steps, seed + 1000 + k as u64);
        for head in answered.iter().filter(|_| all_settled) {
            assert!(
                holds_kept(daemon.store(), head),
                "crash after turn {k}: {head} answered and settled, and lost what it left"
            );
        }
        let mut client = dial(&mut daemon);
        let id = client.send(
            &mut daemon,
            Request::Build { view: ViewRef::Head("/w".into()), targets: agent() },
        );
        let got = client.wait(&mut daemon, id);
        settle(&mut daemon, &mut client);
        assert!(daemon.lost().is_empty(), "crash after turn {k}: {:#?}", daemon.lost());
        assert_eq!(outcome(&mut daemon, &got), fresh, "crash after turn {k}");
    }
    assert!(
        answered_any > 0,
        "no crash point came after an answer, of {turns} turns: the test checks nothing kept"
    );
    assert!(
        answered_one > 0,
        "no crash point came after one checkout answered and before the other did, of {turns} turns: the race it is for \
         was never crashed in"
    );
    assert!(
        unsettled > 0,
        "no crash point came between an answer and its results being durable, of {turns} turns: the window \
         the answer no longer waits out was never crashed in"
    );
    assert!(
        settled > 0,
        "no crash point came after an answer had settled, of {turns} turns: the test checks nothing kept"
    );
    eprintln!(
        "crash points: {} in all, {answered_any} after an answer, {answered_one} after one alone, {unsettled} \
         before it settled, {settled} after",
        turns + 1
    );
    OUTPUT.set(false);
}

/// A build's answer is sent in the winding up that first sees its plane request finished with
/// no output being placed and no run going: here with the task that stores its kept front end
/// held on the runner ([`Daemon::hold_tasks_for_test`], as a large front end's store runs on a
/// worker), so nothing it leaves is in the store yet, let alone durable, and its front end is
/// still out. That settles after the answer.
///
/// A build of the checkout asked meanwhile is no part of what settles: it waits only for the
/// front end's hand-back (its lowering held, named in a stall) and lowers into the front end
/// the store gives back, rather than lowering anew beside it; it answers with the first's
/// puts durable or not. Across seeds, for the order of everything else.
#[test]
fn a_build_answers_while_its_front_end_is_stored_and_the_next_waits_only_for_the_front_end() {
    for seed in 150..170 {
        let mut sim = Sim::new(seed, Faults::default());
        checkout(&mut sim, "[package]\nname = \"app\"\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        daemon.hold_tasks_for_test(blaze_graph::admission::kinds::STORE_KEPT, true);
        let mut client = dial(&mut daemon);
        let first = client.send(
            &mut daemon,
            Request::Build { view: ViewRef::Head("/w".into()), targets: agent() },
        );
        let got = client.wait(&mut daemon, first);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
        assert_eq!(daemon.storing(), 1, "seed {seed}: its kept front end is still being stored");
        assert_eq!(daemon.jobs_settling(), 1, "seed {seed}: what it left settles after the answer");
        assert!(
            !holds_kept(daemon.store(), "/w"),
            "seed {seed}: the held store put what the build left"
        );
        let (anew, into_kept) = (ANEW.get(), INTO_KEPT.get());
        let second = client.send(
            &mut daemon,
            Request::Build { view: ViewRef::Head("/w".into()), targets: agent() },
        );
        // Let go in the turn it is seen: with the store and the lowering both waiting, nothing
        // else is in flight, and the simulation's clock would jump to the stall's timer.
        turn_until(&mut daemon, |daemon| {
            client.take(daemon);
            let held =
                daemon.held_lowerings().iter().any(|it| it.checkout == "/w" && it.on.is_some());
            held || client.done(second)
        });
        assert!(
            !client.done(second),
            "seed {seed}: the second build never waited for the front end: {:#?}",
            client.of(second)
        );
        assert!(daemon.storing() > 0, "seed {seed}: the store ended while held");
        daemon.hold_tasks_for_test(blaze_graph::admission::kinds::STORE_KEPT, false);
        let got = client.wait(&mut daemon, second);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
        assert_eq!(
            (ANEW.get(), INTO_KEPT.get()),
            (anew, into_kept + 1),
            "seed {seed}: the second lowers into the stored front end"
        );
        settle(&mut daemon, &mut client);
        assert!(daemon.lost().is_empty(), "seed {seed}: {:#?}", daemon.lost());
        assert!(holds_kept(daemon.store(), "/w"), "seed {seed}: settled, and it is in the store");
        assert_eq!((dones(&client, first), dones(&client, second)), (1, 1), "seed {seed}");
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
        let id = client.send(
            &mut daemon,
            Request::Build { view: ViewRef::Head("/w".into()), targets: agent() },
        );
        assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }));
        assert_eq!(
            KEPT_READS.get(),
            reads,
            "the first build of a fresh daemon finds nothing stored"
        );
        // A stop waits for what the build left to settle; a crash keeps it only once it has.
        if crash {
            settle(&mut daemon, &mut client);
        }
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
        let id = client.send(
            &mut daemon,
            Request::Build { view: ViewRef::Head("/w".into()), targets: agent() },
        );
        assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }));
        assert_eq!(ANEW.get(), anew + 1, "a restarted daemon lowers anew");
        assert_eq!(
            KEPT_READS.get(),
            reads + 1,
            "and reads what the last build stored (crash: {crash})"
        );
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
        let id = client.send(
            &mut daemon,
            Request::Build { view: ViewRef::Head("/w".into()), targets: agent() },
        );
        assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }));
    }
    assert_eq!(
        (ANEW.get(), INTO_KEPT.get()),
        (anew + 1, into_kept + 1),
        "the second build lowers into the first's"
    );
    settle(&mut daemon, &mut client);
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
        Request::Run {
            view: ViewRef::Head("/w".into()),
            targets: agent(),
            bin: None,
            args: Vec::new(),
        },
    );
    let got = client.wait(&mut daemon, id);
    let at = |what: fn(&Response) -> bool| got.iter().position(what);
    let built = at(|it| matches!(it, Response::Built { .. }));
    let ran = at(|it| matches!(it, Response::Ran { code: Some(0), .. }));
    let worked = at(|it| matches!(it, Response::Work(_)));
    assert!(built.is_some() && built < ran && ran < worked, "{got:#?}");
    assert_eq!(got.last(), Some(&Response::Done { ok: true }));
}

/// B2 (2026-11-03): the plane finishes once the last unit's node ends, before the loop has
/// placed what it made, and a job's tests or run started then: on macOS every test of a
/// binary linked last failed at exec, "No such file or directory", its file not placed yet.
/// The program's placement slowed to 5 s here, the run still starts only once its job has no
/// placement in flight ([`super::job::Jobs::release_runs`]), and the job ends ok.
#[test]
fn a_run_starts_only_once_its_program_is_placed() {
    use super::job::RUNS_STARTED_PLACING;
    RUNS_STARTED_PLACING.with_borrow_mut(Vec::clear);
    OUTPUT.set(true);
    let mut sim = Sim::new(43, Faults::default());
    checkout(&mut sim, "[package]\n");
    sim.slow_writes(name("w/target"), Duration::from_secs(5));
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 43);
    let mut client = dial(&mut daemon);
    let id = client.send(
        &mut daemon,
        Request::Run {
            view: ViewRef::Head("/w".into()),
            targets: agent(),
            bin: None,
            args: Vec::new(),
        },
    );
    let got = client.wait(&mut daemon, id);
    OUTPUT.set(false);
    let started = RUNS_STARTED_PLACING.with_borrow_mut(std::mem::take);
    assert_eq!(
        started,
        [0],
        "the run starts once, with none of its job's placements in flight: {got:#?}"
    );
    assert_eq!(daemon.driver().contents(&name(&APP_AT[1..])), APP, "the program is placed whole");
    assert!(got.iter().any(|it| matches!(it, Response::Ran { code: Some(0), .. })), "{got:#?}");
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
}

/// What a build of `/w` whose program has a file its answer names ([`APP_AT`]) and one it
/// does not ([`SIDE_AT`]) looked like as its answer came, and once what it left had settled.
struct Answered {
    done: Option<Response>,
    /// The named file and the other one, as the answer came.
    app: Vec<u8>,
    side: Vec<u8>,
    /// How many answered jobs still had something settling as the answer came.
    settling: usize,
    /// The other file and its mode once all had settled, and what was lost.
    side_after: Vec<u8>,
    side_mode: Option<u32>,
    lost: Vec<String>,
}

/// One build of `/w` on a disk whose writes under `slow` take 5 s, and under `full`, if
/// given, find no room.
fn answered(seed: u64, slow: &str, full: Option<&str>) -> Answered {
    OUTPUT.set(true);
    SIDE_OUTPUT.set(true);
    let mut sim = Sim::new(seed, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    sim.slow_writes(name(slow), Duration::from_secs(5));
    if let Some(full) = full {
        sim.fill(name(full), 0);
    }
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, seed);
    let mut client = dial(&mut daemon);
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    let (app, side) = (name(&APP_AT[1..]), name(&SIDE_AT[1..]));
    let (app_then, side_then) = (daemon.driver().contents(&app), daemon.driver().contents(&side));
    let settling = daemon.jobs_settling();
    settle(&mut daemon, &mut client);
    OUTPUT.set(false);
    SIDE_OUTPUT.set(false);
    Answered {
        done: got.last().cloned(),
        app: app_then,
        side: side_then,
        settling,
        side_after: daemon.driver().contents(&side),
        side_mode: daemon.driver().mode(&side).map(|it| it & 0o777),
        lost: daemon.lost().iter().map(|it| it.what.clone()).collect(),
    }
}

/// The invariant: as a build is answered, every file its answer names is at its path whole;
/// an output it does not name holds the answer for nothing, and is at its path whole, with
/// its mode, once what the build left has settled, nothing lost.
///
/// The other output's directory is slow by 5 s: the answer comes with the named program
/// there and the other file not yet, which is the wait an edit's answer no longer has.
#[test]
fn a_builds_answer_waits_for_the_files_it_names_and_places_the_rest_after() {
    for seed in 150..158 {
        let it = answered(seed, "w/target/debug/deps", None);
        assert_eq!(it.done, Some(Response::Done { ok: true }), "seed {seed}");
        assert_eq!(it.app, APP, "seed {seed}: the file the answer names is whole as it is answered");
        assert!(
            it.side.is_empty() && it.settling > 0,
            "seed {seed}: the answer waited for an output it does not name ({} settling)",
            it.settling
        );
        assert_eq!(it.side_after, SIDE, "seed {seed}: the other output is placed after the answer");
        assert_eq!(it.side_mode, Some(0o755), "seed {seed}: with its mode, as under the job");
        assert!(it.lost.is_empty(), "seed {seed}: {:#?}", it.lost);
    }
}

/// The planted-bug control: an answer that waits for no placement at all is sent with the
/// program it names not yet at its path (its directory slow as the other's), which is what
/// a person's `./target/debug/app` right after the command would run into.
#[test]
fn the_oracle_catches_an_answer_sent_before_the_file_it_names_is_placed() {
    use super::job::PLANT_ANSWER_WAITS_FOR_NO_OUTPUT;
    let whole = answered(160, "w/target/debug", None);
    assert_eq!(whole.app, APP, "the daemon as it is waits for the named file, however slow");
    PLANT_ANSWER_WAITS_FOR_NO_OUTPUT.set(true);
    let planted = answered(160, "w/target/debug", None);
    PLANT_ANSWER_WAITS_FOR_NO_OUTPUT.set(false);
    assert_ne!(planted.app, APP, "the oracle missed an answer sent before its named file was placed");
}

/// An output that fails to be placed after the answer changes no answer and is never
/// silent: it is a loss naming the path and why, which `blaze status` shows and the
/// checkout's next `blaze why` says first ([`super::job::Jobs::lose`]).
#[test]
fn an_output_that_fails_to_be_placed_after_the_answer_is_a_loss_said() {
    let (it, reported) = stdx::assert::reported::expecting("settling", || {
        answered(170, "w/target/debug/deps", Some("w/target/debug/deps"))
    });
    assert_eq!(reported, 1, "the loss is reported once");
    assert_eq!(it.done, Some(Response::Done { ok: true }), "the answer was sent and stands");
    assert_eq!(it.app, APP);
    assert_ne!(it.side_after, SIDE, "the disk had no room for it");
    assert!(
        it.lost.len() == 1 && it.lost[0].contains("deps/app-0123"),
        "the failure is one loss naming the path: {:#?}",
        it.lost
    );
}

/// What a run of `/w` asked right after a build's answer, while the build's other output is
/// still being placed (its directory slow by 5 s), saw.
struct RanAfter {
    /// Still settling as the build was answered: the other output was in flight.
    settling: usize,
    /// The outputs the run's job queued behind an effect in flight at their path.
    queued: Vec<String>,
    /// The most effects in flight at one path at once.
    most: u32,
    /// How many of the run's job's placements were in flight as its program started.
    started: Vec<usize>,
    /// The other file as the run ended.
    side: Vec<u8>,
    ran: bool,
    lost: usize,
}

fn run_after_an_answer(seed: u64) -> RanAfter {
    use super::job::{MOST_AT_A_PATH, QUEUED_OUTPUTS, RUNS_STARTED_PLACING};
    RUNS_STARTED_PLACING.with_borrow_mut(Vec::clear);
    QUEUED_OUTPUTS.with_borrow_mut(Vec::clear);
    MOST_AT_A_PATH.set(0);
    OUTPUT.set(true);
    SIDE_OUTPUT.set(true);
    let mut sim = Sim::new(seed, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    sim.slow_writes(name("w/target/debug/deps"), Duration::from_secs(5));
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, seed);
    let mut client = dial(&mut daemon);
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
    let settling = daemon.jobs_settling();
    let id = client.send(
        &mut daemon,
        Request::Run {
            view: ViewRef::Head("/w".into()),
            targets: agent(),
            bin: None,
            args: Vec::new(),
        },
    );
    let got = client.wait(&mut daemon, id);
    let side = daemon.driver().contents(&name(&SIDE_AT[1..]));
    settle(&mut daemon, &mut client);
    OUTPUT.set(false);
    SIDE_OUTPUT.set(false);
    RanAfter {
        settling,
        queued: QUEUED_OUTPUTS.with_borrow_mut(std::mem::take),
        most: MOST_AT_A_PATH.get(),
        started: RUNS_STARTED_PLACING.with_borrow_mut(std::mem::take),
        side,
        ran: got.iter().any(|it| matches!(it, Response::Ran { code: Some(0), .. }))
            && got.last() == Some(&Response::Done { ok: true }),
        lost: daemon.lost().len(),
    }
}

/// The invariant: an output is placed at a path only once no effect of an earlier placement
/// is in flight there, so what the daemon starts after an answer (the next job's run) reads
/// a file that is whole and is the one its own job made, never one an answered build is
/// still writing: the run's job queues its output behind the answered build's, and its
/// program starts only once that and its own look at it have ended.
#[test]
fn a_run_asked_right_after_an_answer_waits_for_the_output_still_being_placed() {
    for seed in 180..188 {
        let it = run_after_an_answer(seed);
        assert!(it.settling > 0, "seed {seed}: the build's other output was not in flight as it was answered");
        assert_eq!(it.queued, [SIDE_AT], "seed {seed}: the run's output waited for the one in flight");
        assert_eq!(it.most, 1, "seed {seed}: two placements of one path were in flight at once");
        assert_eq!(it.started, [0], "seed {seed}: the run started with a placement of its job in flight");
        assert_eq!(it.side, SIDE, "seed {seed}: the file was whole as the run ended");
        assert!(it.ran && it.lost == 0, "seed {seed}");
    }
}

/// The planted-bug control: outputs placed at once whatever is in flight at their path, as
/// before an output could outlive its job's answer: the run's job places the file while the
/// answered build's placement of it is still in flight.
#[test]
fn the_oracle_catches_an_output_placed_beside_one_in_flight_at_its_path() {
    use super::job::PLANT_NO_QUEUE;
    PLANT_NO_QUEUE.set(true);
    let it = run_after_an_answer(180);
    PLANT_NO_QUEUE.set(false);
    assert!(it.queued.is_empty(), "the plant queues nothing");
    assert!(it.most > 1, "the oracle missed two placements of one path in flight at once");
}

#[test]
fn the_documentation_is_made_once_its_check_is_built_with_every_scripts_run_applied() {
    let mut sim = Sim::new(42, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 42);
    let mut client = dial(&mut daemon);
    let doc = Request::Doc {
        view: ViewRef::Head("/w".into()),
        targets: agent(),
        no_deps: true,
        private: false,
    };
    let id = client.send(&mut daemon, doc);
    let got = client.wait(&mut daemon, id);
    let at = |what: fn(&Response) -> bool| got.iter().position(what);
    let built = at(|it| matches!(it, Response::Built { .. }));
    let documented = at(|it| matches!(it, Response::Documented(_)));
    let page = at(|it| matches!(it, Response::DocPage { .. }));
    assert!(
        built.is_some() && built < documented && documented < page,
        "`Built`, `Documented`, then its pages: {got:#?}"
    );
    let made =
        Response::DocPage { path: "app/index.html".into(), page: b"true false [2]".to_vec() };
    assert!(
        got.contains(&made),
        "app's build script ran first, and the request's flags came through: {got:#?}"
    );
    assert_eq!(got.last(), Some(&Response::Done { ok: true }));

    let id = client
        .send(&mut daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
    let after = client.wait(&mut daemon, id);
    assert_eq!(
        after.last(),
        Some(&Response::Done { ok: true }),
        "the head builds after its documentation"
    );
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
        vec![
            ("a::adds".to_owned(), TestOutcome::Passed),
            ("a::overflows".to_owned(), TestOutcome::Failed)
        ],
        "{got:#?}"
    );
    let failed = got.iter().find_map(|it| match it {
        Response::TestResult(result) if result.outcome == TestOutcome::Failed => {
            Some(result.output.clone())
        }
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
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");
    let refused = got.iter().any(
        |it| matches!(it, Response::Error(error) if error.what.contains("refuses this manifest")),
    );
    assert!(refused, "the load's error is the build's: {got:#?}");
    assert!(!got.iter().any(|it| matches!(it, Response::Built { .. })), "{got:#?}");

    daemon.driver().change(&name("w/Cargo.toml"), b"[package]\n");
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
}

/// A build after one that failed says so before it does anything, and `blaze why` has it as a
/// row of that build; the build after a build that ended well says nothing of it. Before
/// 2026-11-11 the builds after a failed cold build did its work, exited 0 and said nothing, and
/// read as slow builds of an unchanged tree.
#[test]
fn a_build_after_one_that_failed_says_so_first_and_only_until_one_ends_well() {
    ENDED_LOST.with(|it| it.borrow_mut().clear());
    let mut sim = Sim::new(45, Faults::default());
    checkout(&mut sim, "fail\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 45);
    let mut client = dial(&mut daemon);
    let build = |daemon: &mut SimDaemon, client: &mut Client| {
        client.send(daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() })
    };
    // Where a build says its checkout's last build did not end well, among what it says.
    let noted = |got: &[Response]| {
        got.iter().position(|it| {
            matches!(it, Response::Diagnostic(note) if note.rendered.starts_with("note: the last build of /w (")
                && note.message.contains(") failed")
                && note.message.contains("this build first does the work that one left"))
        })
    };
    let id = build(&mut daemon, &mut client);
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");
    assert_eq!(noted(&got), None, "the first build of the checkout follows none: {got:#?}");

    daemon.driver().change(&name("w/Cargo.toml"), b"[package]\nname = \"app\"\n");
    let id = build(&mut daemon, &mut client);
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    let at = noted(&got).unwrap_or_else(|| panic!("it says the last build failed: {got:#?}"));
    let built = got.iter().position(|it| matches!(it, Response::Built { .. }));
    assert!(built.is_none_or(|built| at < built), "said before its work, not after: {got:#?}");
    let said = ENDED_LOST.with(|it| it.take());
    assert!(
        said.iter().any(|it| it.contains("the last build of /w") && it.contains("failed") && it.contains("it ran ")),
        "the build's row for `blaze why`: {said:#?}"
    );

    // The control: that build ended well, so the next has nothing to say of it.
    let id = build(&mut daemon, &mut client);
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    assert_eq!(noted(&got), None, "{got:#?}");
    let said = ENDED_LOST.with(|it| it.take());
    assert!(said.is_empty(), "{said:#?}");
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
        env: command_env(),
        args: vec!["--keep-going".into()],
        root: "/w".into(),
        color: false,
        files: Vec::new(),
        width: None,
        command: "build".into(),
        refused: None,
        verbose: 0,
        prebuilt: true,
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
    assert!(
        ran.contains(&(0, Phase::Store)),
        "dep's library, which needs no run, is built: {ran:?}"
    );
    assert!(
        !ran.iter().any(|it| it.0 == 3),
        "the program, after the run, is never compiled: {ran:?}"
    );

    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(
        got.last(),
        Some(&Response::Done { ok: true }),
        "the daemon builds the next: {got:#?}"
    );
}

/// A write to the front end that fails with an error as it is applied (as a real writer's does
/// on a proc macro it cannot load) fails only the units after it, naming the write and its
/// unit, while keep-going builds the rest: before, the error ended the whole job. The build
/// script's run (unit 2) fails as it is applied, the program (unit 3) is never compiled, and
/// dep's library (unit 0) is; the daemon answers the next request.
#[test]
fn a_write_that_fails_fails_only_the_units_after_it_and_keep_going_builds_the_rest() {
    use super::job::REFUSED_UNITS;
    PHASES.with(|it| it.borrow_mut().clear());
    REFUSED_UNITS.with_borrow_mut(Vec::clear);
    FAILS_RUN.set(Some(2));
    let mut sim = Sim::new(46, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 46);
    let mut client = dial(&mut daemon);
    let cargo = blaze_api::Cargo {
        cwd: "/w".into(),
        env: command_env(),
        args: vec!["--keep-going".into()],
        root: "/w".into(),
        color: false,
        files: Vec::new(),
        width: None,
        command: "build".into(),
        refused: None,
        verbose: 0,
        prebuilt: true,
    };
    let targets = Targets { cargo: Some(cargo), ..agent() };
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets });
    let got = client.wait(&mut daemon, id);
    FAILS_RUN.set(None);

    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");
    let named = got.iter().any(|it| {
        matches!(it, Response::Error(error) if error.what.starts_with("applying a build script's run of custom-build of ")
            && error.what.contains("it failed at serve-test's run: serve-test: the run's write fails"))
    });
    assert!(
        named,
        "the build's error names the write, its unit, where it failed and why: {got:#?}"
    );
    // The program, which depends on the run, is refused naming the write and its unit.
    let refused = REFUSED_UNITS.with_borrow_mut(std::mem::take);
    let program: Vec<&String> =
        refused.iter().filter(|(unit, _)| *unit == 3).map(|(_, why)| why).collect();
    assert!(!program.is_empty(), "the program is refused: {refused:#?}");
    for why in program {
        for part in [
            "a write it depends on broke",
            "applying a build script's run of custom-build of path+file://",
            "#app@0.1.0",
            "it failed at serve-test's run",
        ] {
            assert!(why.contains(part), "the program's refusal says `{part}`: {why}");
        }
    }
    assert!(
        refused.iter().all(|(unit, _)| *unit == 3),
        "only what depends on the run is refused: {refused:#?}"
    );
    let ran = PHASES.with(|it| it.take());
    assert!(
        ran.contains(&(0, Phase::Store)),
        "dep's library, which needs no run, is built: {ran:?}"
    );
    assert!(
        !ran.iter().any(|it| it.0 == 3),
        "the program, after the run, is never compiled: {ran:?}"
    );

    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(
        got.last(),
        Some(&Response::Done { ok: true }),
        "the daemon builds the next: {got:#?}"
    );
}

/// Under `--fail-fast`, a write that fails as it is applied ends the job at once, as a
/// writer's own error does. Before, it only failed the job's result, and the plane failed fast
/// once the program, which depends on the run, was posted, which waited for the program's other
/// dependency: here the second build script's run (unit 4), on a timer of an hour. The first
/// run's write (unit 2) fails; the job ends without that run ever finishing, well inside its
/// hour, with the write named, and the daemon answers the next request.
#[test]
fn under_fail_fast_a_write_that_fails_ends_the_job_at_once() {
    ASKED.set(0);
    FAILS_RUN.set(Some(2));
    SLOW_SCRIPT.set(Some((4, Duration::from_secs(3600))));
    let mut sim = Sim::new(47, Faults::default());
    checkout(&mut sim, "two scripts\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 47);
    let mut client = dial(&mut daemon);
    let cargo = blaze_api::Cargo {
        cwd: "/w".into(),
        env: command_env(),
        args: vec!["--fail-fast".into()],
        root: "/w".into(),
        color: false,
        files: Vec::new(),
        width: None,
        command: "build".into(),
        refused: None,
        verbose: 0,
        prebuilt: true,
    };
    let targets = Targets { cargo: Some(cargo), ..agent() };
    let began = daemon.driver().now();
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets });
    let got = client.wait(&mut daemon, id);
    let took = daemon.driver().now() - began;
    FAILS_RUN.set(None);
    SLOW_SCRIPT.set(None);

    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");
    let named = got.iter().any(|it| {
        matches!(it, Response::Error(error) if error.what.starts_with("applying a build script's run of custom-build of ")
            && error.what.contains("it failed at serve-test's run: serve-test: the run's write fails"))
    });
    assert!(named, "the build's error names the write, its unit and its error: {got:#?}");
    assert_eq!(ASKED.get(), 1, "the slow run never finished to ask its write: the job ended first");
    assert!(
        took < Duration::from_secs(3600),
        "the job ended without waiting for the slow run: it took {took:?}"
    );

    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(
        got.last(),
        Some(&Response::Done { ok: true }),
        "the daemon builds the next: {got:#?}"
    );
}

/// A build script's run refused by a write that broke never reaches the runner. The plane
/// hands a run to the loop directly, never through the post that refuses a task, so a run that
/// ended well at the plane let its dependent run start: here the program's build script run
/// (unit 4), after `dep`'s (unit 2) through the edge cargo makes for a `links` dependency.
/// `dep`'s run's write fails as it is applied (its run on a timer, so every compile is done by
/// then); under keep-going the program's run fails naming that write and its unit, and its user
/// code never runs.
#[test]
fn a_build_script_run_after_a_write_that_failed_never_reaches_the_runner() {
    use super::job::REFUSED_UNITS;
    REFUSED_UNITS.with_borrow_mut(Vec::clear);
    SCRIPTS.with_borrow_mut(Vec::clear);
    FAILS_RUN.set(Some(2));
    SLOW_SCRIPT.set(Some((2, Duration::from_secs(1))));
    let mut sim = Sim::new(48, Faults::default());
    checkout(&mut sim, "links scripts\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 48);
    let mut client = dial(&mut daemon);
    let cargo = blaze_api::Cargo {
        cwd: "/w".into(),
        env: command_env(),
        args: vec!["--keep-going".into()],
        root: "/w".into(),
        color: false,
        files: Vec::new(),
        width: None,
        command: "build".into(),
        refused: None,
        verbose: 0,
        prebuilt: true,
    };
    let targets = Targets { cargo: Some(cargo), ..agent() };
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets });
    let got = client.wait(&mut daemon, id);
    FAILS_RUN.set(None);
    SLOW_SCRIPT.set(None);
    let scripts = SCRIPTS.with_borrow_mut(std::mem::take);
    let refused = REFUSED_UNITS.with_borrow_mut(std::mem::take);

    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");
    assert_eq!(
        scripts,
        vec![2],
        "only dep's run reached the runner; the program's, after it, never did"
    );
    let run: Vec<&String> =
        refused.iter().filter(|(unit, _)| *unit == 4).map(|(_, why)| why).collect();
    let [why] = &run[..] else { panic!("the program's run is refused once: {refused:#?}") };
    for part in [
        "a write it depends on broke",
        "applying a build script's run of custom-build of registry+",
        "#dep@1.0.0",
        "it failed at serve-test's run",
    ] {
        assert!(why.contains(part), "the run's refusal says `{part}`: {why}");
    }

    daemon.driver().change(&name("w/Cargo.toml"), b"[package]\n");
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(
        got.last(),
        Some(&Response::Done { ok: true }),
        "the daemon builds the next: {got:#?}"
    );
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
        env: command_env(),
        args: vec!["--keep-going".into()],
        root: "/w".into(),
        color: false,
        files: Vec::new(),
        width: None,
        command: "build".into(),
        refused: None,
        verbose: 0,
        prebuilt: true,
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
    let job_failed = got
        .iter()
        .any(|it| matches!(it, Response::Error(error) if error.what.contains("panicked")));
    assert!(!job_failed, "the panic fails its unit, not the job: {got:#?}");
    let ran = PHASES.with(|it| it.take());
    assert!(ran.contains(&(0, Phase::Store)), "dep's library, the sibling, is built: {ran:?}");
    assert!(
        !ran.iter().any(|it| it.0 == 3),
        "the program, after the build script, is never compiled: {ran:?}"
    );

    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(
        got.last(),
        Some(&Response::Done { ok: true }),
        "the daemon builds the next: {got:#?}"
    );
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
        env: command_env(),
        args: vec!["--keep-going".into()],
        root: "/w".into(),
        color: false,
        files: Vec::new(),
        width: None,
        command: "build".into(),
        refused: None,
        verbose: 0,
        prebuilt: true,
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
    let job_failed = got
        .iter()
        .any(|it| matches!(it, Response::Error(error) if error.what.contains("lang_start")));
    assert!(!job_failed, "the error fails its unit, not the job: {got:#?}");
    let ran = PHASES.with(|it| it.take());
    assert!(ran.contains(&(0, Phase::Store)), "dep's library, the sibling, is built: {ran:?}");
    assert!(
        !ran.iter().any(|it| it.0 == 3),
        "the program, after the build script, is never compiled: {ran:?}"
    );

    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(
        got.last(),
        Some(&Response::Done { ok: true }),
        "the daemon builds the next: {got:#?}"
    );
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
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    PANICS_COMPILE.set(None);
    assert_eq!(got.last(), Some(&Response::Done { ok: false }), "{got:#?}");

    let image = name(&format!("{STATE}/db/image"));
    let mut daemon = restart(daemon, &steps, 47);
    assert!(daemon.driver().exists(&image), "the stop wrote the failed build's image");
    let started = daemon.started();
    assert!(started.mapped.is_some(), "the next start opens the stop's image: {started}");
    assert_eq!(started.unimaged, None, "{started}");
}

#[test]
fn a_build_cancelled_while_nothing_of_it_runs_ends_not_ok_and_the_daemon_answers_the_next() {
    let sim = Sim::new(43, Faults::default());
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 43);
    let mut client = dial(&mut daemon);
    let base = daemon.vfs().empty();
    let ops = vec![Op::Write {
        path: "Cargo.toml".into(),
        bytes: b"[package]\n".to_vec(),
        mode: blaze_vfs::Mode::Regular,
    }];
    let commit = client.send(&mut daemon, Request::Commit { head: "agents/one".into(), base, ops });
    assert!(matches!(client.wait(&mut daemon, commit).first(), Some(Response::Committed { .. })));

    // An agent's head waits for no watcher, so all three start in the turn they arrive: the
    // second build's standard library node joins the first's task, and the cancel finds
    // nothing of the second's request running.
    let build = || Request::Build { view: ViewRef::Head("agents/one".into()), targets: agent() };
    let (first, second) = (client.next, client.next + 1);
    let ids =
        client.send_together(&mut daemon, vec![build(), build(), Request::Cancel { id: second }]);
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
    let front = Arc::new(FakeFront { kind: JobKind::Check });
    let inputs = Arc::new(FakeInputs { runs: vec![4, 7], ..FakeInputs::default() });
    let reading =
        || Readers::<Fake> { this: Some(FakeReader(inputs.clone())), ..Readers::default() };
    let run = |does: Does<Fake>, readers: Readers<Fake>| match Step::of(&cancelled, does)
        .run(0, readers)
    {
        Some(Did { outcome: blaze_graph::Outcome::Built, value }) => value,
        _ => panic!("a step of the fakes is built at once"),
    };
    let answered = |value: Value<Fake>| match value {
        Value::Answer(Ok(response)) => response,
        _ => panic!("the step gave no answer"),
    };
    let ub = Ub { seed: Some(5), schedules: None };

    let from = Readers::<Fake> { from: Some(FakeReader(inputs.clone())), ..reading() };
    let reached = answered(run(
        Does::Reach { to: front.clone(), from: front.clone(), changes: Vec::new() },
        from,
    ));
    assert!(
        matches!(&reached, Response::Reached(Reach { changes, .. }) if changes.is_empty()),
        "{reached:?}"
    );

    let codegen = answered(run(Does::CodegenInputs { front: front.clone() }, reading()));
    assert_eq!(codegen, Response::CodegenInputs(Vec::new()));

    let writer = FakeWriter {
        inputs: Arc::new(FakeInputs { runs: vec![1, 2, 3], ..FakeInputs::default() }),
        asked: Vec::new(),
        broke: Vec::new(),
    };
    let tables = Does::Tables { front: front.clone(), images: Vec::new() };
    let tables = answered(run(tables, Readers { writer: Some(writer), ..Readers::default() }));
    assert_eq!(tables, Response::Tables(vec![Table { name: "run".into(), elements: 3, bytes: 0 }]));

    let db = Arc::new(Database::new(blaze_query::Config::memory()));
    let queries = db.open_with(blaze_vfs::View { root: ContentId([0; 32]) }, b"");
    let traced = answered(run(
        Does::Trace { db, queries, root: Fake::default().plan_key() },
        Readers::default(),
    ));
    assert_eq!(traced, Response::Trace(Vec::new()), "a view no query ran in traces nothing");

    let select = Does::Select {
        front: front.clone(),
        filter: vec!["a::".into()],
        args: Vec::new(),
        all: true,
        ub,
    };
    match run(select, reading()) {
        Value::Selected(Ok(tests)) => assert_eq!(tests, ["a::adds", "a::overflows"]),
        _ => panic!("the selection gave no tests"),
    }
    let evaluate =
        Does::Evaluate { front: front.clone(), tests: vec!["a::overflows".to_owned()].into(), ub };
    match run(evaluate, reading()) {
        Value::Evaluated(Ok(result)) => assert_eq!(
            (result.name.as_str(), result.outcome),
            ("a::overflows", TestOutcome::Failed)
        ),
        _ => panic!("the evaluation gave no result"),
    }

    let compile = || {
        Step::of(
            &cancelled,
            Does::Compile {
                front: Arc::new(FakeFront { kind: JobKind::Build }),
                back: Default::default(),
                hops: Arc::from(Vec::new()),
            },
        )
    };
    match compile().run(Phase::Names.arg(0), reading()) {
        Some(Did { outcome: blaze_graph::Outcome::Built, value: Value::Phase(0, images) }) => {
            assert!(images.is_empty())
        }
        _ => panic!("a phase before the last gives no compile"),
    }
    match compile().run(Phase::Store.arg(0), reading()) {
        Some(Did {
            outcome: blaze_graph::Outcome::Built,
            value: Value::Compiled(0, Ok(compiled), images),
        }) => {
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
    let id = client
        .send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
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
        let id = client.send(
            &mut daemon,
            Request::Build { view: ViewRef::Head("/w".into()), targets: agent() },
        );
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
        let id = client
            .send(daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
        let got = client.wait(daemon, id);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    };
    // The first build writes the lock file into the checkout, so the second's view is another.
    build(&mut client, &mut daemon);
    build(&mut client, &mut daemon);
    let (asks, anew, into_kept) = (ASKS.get(), ANEW.get(), INTO_KEPT.get());
    assert!(into_kept >= 1, "the second build lowers into the first's front end");
    build(&mut client, &mut daemon);
    assert_eq!(
        ASKS.get(),
        asks,
        "the view and its config are the last build's: its plan is not asked"
    );
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
        let id = client
            .send(daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
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
        assert!(
            matches!(of.last(), Some(Response::Done { .. })),
            "request {id} got something after its `Done`: {of:#?}"
        );
        for (at, it) in of.iter().enumerate() {
            if matches!(it, Response::Error(_)) {
                assert_eq!(
                    at + 2,
                    of.len(),
                    "request {id}'s error is not right before its `Done`: {of:#?}"
                );
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
        let faults = Faults {
            max_latency: std::time::Duration::from_millis(rng.below(4)),
            ..Faults::default()
        };
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
                    ops: vec![Op::Write {
                        path: "Cargo.toml".into(),
                        bytes: b"what if\n".to_vec(),
                        mode: blaze_vfs::Mode::Regular,
                    }],
                },
                _ => ViewRef::Head("agents/none".into()),
            };
            let request = match rng.below(5) {
                0 => Request::Check { view, targets: agent() },
                1 => Request::Build { view, targets: agent() },
                2 => Request::Test {
                    view,
                    targets: agent(),
                    filter: Vec::new(),
                    all: false,
                    args: Vec::new(),
                    ub: None,
                },
                3 => Request::Doc { view, targets: agent(), no_deps: false, private: true },
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
            let over =
                asked.iter().filter(|it| live(it.client)).all(|it| clients[it.client].done(it.id))
                    && cancels
                        .iter()
                        .filter(|it| live(it.0))
                        .all(|(client, id)| clients[*client].done(*id));
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
            let mut ids: Vec<u64> =
                asked.iter().filter(|it| it.client == at).map(|it| it.id).collect();
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
    let a = super::hold(state.path(), None, None).unwrap();
    let lock = state.path().join("serve.lock");
    assert_eq!(std::fs::read_to_string(&lock).unwrap(), std::process::id().to_string());
    let (mut heard, said) = std::io::pipe().unwrap();
    let said: std::os::fd::OwnedFd = said.into();
    let said = super::Ready::new(std::fs::File::from(said)).unwrap();
    let path = state.path().to_owned();
    let b = std::thread::spawn(move || super::hold(&path, None, Some(&said)).map(drop));
    let mut byte = [0u8; 1];
    heard.read_exact(&mut byte).unwrap();
    assert_eq!(super::Phase::of(byte[0]), Some(super::Phase::Waiting));
    assert!(!b.is_finished(), "B started while A still held the state");
    drop(a);
    b.join().unwrap().unwrap();
    assert_eq!(std::fs::read_to_string(&lock).unwrap(), std::process::id().to_string());
}

/// The invariant: while a daemon holds a state, anyone who asks the state's lock who holds it
/// is told that one does, and its pid, without a socket and without waiting; and is told none
/// does before any daemon ran there and after it let go. (agents.json A37: `stop` said no
/// daemon ran while one did; the socket's path follows `XDG_RUNTIME_DIR`, the state's does
/// not.)
#[cfg(unix)]
#[test]
fn the_states_lock_says_whether_a_daemon_runs_and_which() {
    let state = tempfile::tempdir().unwrap();
    assert_eq!(super::holder(state.path()).unwrap(), super::Holder::None, "no daemon ever ran here");
    assert!(!state.path().join("serve.lock").exists(), "asking creates nothing");
    let held = super::hold(state.path(), None, None).unwrap();
    let lock = state.path().join("serve.lock");
    assert_eq!(
        super::holder(state.path()).unwrap(),
        super::Holder::Daemon { pid: Some(std::process::id()), lock: lock.clone() }
    );
    // Asking does not take the daemon's place, nor leave a lock behind: asked twice, and a
    // daemon that starts after still waits for the one that holds the state.
    assert!(matches!(super::holder(state.path()).unwrap(), super::Holder::Daemon { .. }));
    drop(held);
    assert_eq!(super::holder(state.path()).unwrap(), super::Holder::None, "let go of as the daemon ends");
    // And asking left the lock free: a daemon starts on the state at once.
    let again = super::hold(state.path(), None, None).unwrap();
    assert!(matches!(again.1, super::Before::Ended(pid) if pid == std::process::id()));
}

#[test]
#[cfg(feature = "native")]
fn serve_takes_activated_alone_and_names_it_among_its_flags() {
    let parse = |args: &[&str]| super::parse(args.iter().map(std::ffi::OsString::from));
    let args = parse(&["--activated", "--socket", "/run/b.sock"]).unwrap();
    assert!(args.activated);
    assert_eq!(args.socket.as_deref(), Some(std::path::Path::new("/run/b.sock")));
    assert!(!parse(&[]).unwrap().activated);
    let refused = parse(&["--activated=yes"]).err().unwrap().to_string();
    assert!(refused.contains("`--activated`, `--ready-fd N`, `--stall-bound SECS` and `--memory-gb GB`, not `--activated=yes`"), "{refused}");
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

/// The loop's time between its turns is a `wait` span naming what ends it (rule 15). Before
/// 2026-11-03 it was in no span, and 108.9 s of a self-build check's critical path read as
/// unrecorded on the daemon's thread.
#[test]
fn the_loops_wait_between_turns_is_a_wait_span_naming_what_ends_it() {
    const THREAD: &str = "daemon loop wait test";
    blaze_profile::install();
    std::thread::Builder::new()
        .name(THREAD.into())
        .spawn(|| {
            let mut sim = Sim::new(5, Faults::default());
            checkout(&mut sim, "[package]\nname = \"app\"\n");
            let steps = Arc::new(Fake::default());
            let mut daemon = start(sim, &steps, 5);
            for _ in 0..100 {
                daemon.turn();
            }
        })
        .unwrap()
        .join()
        .unwrap();
    let mut read = blaze_profile::flight::Read::default();
    blaze_profile::flight::window(Duration::from_secs(60), &mut read);
    let awaits: Vec<String> = read
        .spans
        .iter()
        .filter(|row| blaze_profile::flight::name(row.thread_name).as_deref() == Some(THREAD))
        .filter_map(|row| blaze_profile::span::read::<blaze_profile::span::Wait>(row).unwrap())
        .filter_map(|it| it.fields.awaits.text().map(|it| it.to_string()))
        .collect();
    assert!(
        awaits.iter().any(|it| it.starts_with("io daemon loop: ")),
        "the idle loop's waits say what ends them: {awaits:?}"
    );
}

/// The limit the daemon runs under: `--memory-gb` first, else `BLAZE_DAEMON_MEMORY_GB`, each a
/// number of GiB above 0 (a fraction allowed), named as given; a text that is no such number is
/// an error naming where it came from, never the machine's in its place.
#[test]
fn the_memory_limit_is_told_by_flag_then_variable_and_a_bad_one_is_an_error() {
    use super::{MEMORY_VAR, told_limit, told_memory};
    const GIB: u64 = 1 << 30;
    let var = |it: &str| Some(std::ffi::OsString::from(it));
    assert_eq!(told_limit(None, None).unwrap(), None);
    assert_eq!(told_limit(None, var("48")).unwrap(), Some((48 * GIB, format!("{MEMORY_VAR}=48"))));
    assert_eq!(
        told_limit(None, var("1.5")).unwrap(),
        Some((3 * GIB / 2, format!("{MEMORY_VAR}=1.5")))
    );
    let flag = told_memory("--memory-gb", "24").unwrap();
    assert_eq!(flag, (24 * GIB, "--memory-gb=24".to_owned()));
    assert_eq!(told_limit(Some(flag.clone()), var("48")).unwrap(), Some(flag), "the flag wins");
    for bad in ["0", "-3", "abc", "", "inf", "NaN"] {
        let error = told_limit(None, var(bad)).unwrap_err().to_string();
        assert!(error.starts_with(MEMORY_VAR) && error.contains("GiB above 0"), "{bad:?}: {error}");
    }
}

/// A daemon started under a budget makes it the limit a build's release is decided against
/// and every subsystem's budget, before any job, and its start's line names the limit, what
/// told it and each share. B5, coord-105: the plane carried the budget and nothing set it, so
/// under `BLAZE_DAEMON_MEMORY_GB=48` no release was ever due and the daemon grew to 49 GiB.
#[test]
fn a_daemon_started_under_a_budget_makes_it_the_limit_releases_are_decided_against() {
    use blaze_graph::admission::{self, share};
    use blaze_mem::Tag;
    // Far past what any test holds, so no other daemon's release in this process is made due,
    // and no budget it sets (32 TiB at least) weighs on another test's sweep.
    const LIMIT: u64 = (1 << 50) + (64 << 20);
    let budget =
        blaze_graph::Budget { bytes: LIMIT, from: "a test's limit".to_owned(), kept_back: 0 };
    let said = super::limit_said(&budget);
    assert!(said.starts_with("memory limit 1048576.06 GiB (a test's limit): "), "{said}");
    assert!(said.contains("holds 917504.05 GiB") && said.contains("mir "), "{said}");
    let mut options = options();
    options.budget = Some(budget);
    let steps = Arc::new(Fake::default());
    let _daemon = Daemon::start(
        Sim::new(3, Faults::default()),
        options,
        steps,
        Runner::Inline(Inline::new(3)),
    )
    .unwrap_or_else(|why| panic!("{why}"));
    assert_eq!(admission::limit(), LIMIT, "the release's limit is the daemon's");
    assert_eq!(blaze_mem::tag::budget(Tag::Mir), LIMIT / 64 * share(Tag::Mir), "so is each budget");
}

/// A test command line's environment: the test process's own `BLAZE_CORES`, as a real
/// command's carries it (the daemon's budget is the process's, and the CLI compares the two
/// before any request, `super::settings`), wherever the tests run, `BLAZE_CORES=all` in CI too.
fn command_env() -> Vec<(String, String)> {
    let name = blaze_core::cores::VAR;
    // alloc: a test's request, once
    std::env::var(name).map(|value| vec![(name.to_owned(), value)]).unwrap_or_default()
}

mod ahead;
mod giving;
mod first_look;
mod launched_restart;
mod solo;
mod client_spans;
mod solo_rows;
mod unheld;

/// The checkout's one crate as the front end's carry is given it: rooted at `src/main.rs` of
/// the view's tree.
fn carry_crates() -> Vec<crate::serve::front_carry::CrateRoot> {
    vec![crate::serve::front_carry::CrateRoot::new(
        "w w bin",
        "src/main.rs".to_owned(),
        syntax::Edition::CURRENT,
        cfg::CfgOptions::default(),
        syntax::Extensions::NONE,
    )]
}

/// A check of the head of `/w`, ok, and the daemon turned until it settles: the carry's pass,
/// posted at the build's end, has run.
fn check_settled(client: &mut Client, daemon: &mut SimDaemon) {
    let id = client.send(daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
    assert_eq!(client.wait(daemon, id).last(), Some(&Response::Done { ok: true }));
    turn_until(daemon, |it| it.settled());
}

/// The front end's carry runs on a task of its own, never the loop (rule 14): over a seed
/// and a carried edit, every read of the carry's world is the task's, none the loop's.
#[test]
fn the_loop_runs_no_front_end_step_of_the_carry() {
    CARRY_CRATES.set(carry_crates());
    let mut sim = Sim::new(33, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 33);
    let mut client = dial(&mut daemon);
    check_settled(&mut client, &mut daemon);
    daemon.driver().change(&name("w/src/main.rs"), b"fn main() { let _ = 1; }\n");
    check_settled(&mut client, &mut daemon);
    let built = crate::serve::front_carry::sim::take_built();
    assert_eq!(built.iter().map(|it| it.first).collect::<Vec<_>>(), [true, false], "a seed, then a carry");
    assert!(built[0].files > 0, "the crate's root is in the view's tree: {built:?}");
    let (on_loop, on_task) = crate::serve::front_carry::sim::reads();
    assert_eq!(on_loop, 0, "no read of the carry's world on the loop");
    assert!(on_task > 0, "the carry's task read its world");
}

/// The session survives a restart (rule 26): seeded, carried over an edit, the daemon stopped
/// (its journal marks the session), started again, the edit undone and built. The restarted
/// daemon opens the marked session and carries it from that root, with the counts an edit of
/// that shape carries, rather than seeding; and none of it runs on the loop.
#[test]
fn a_restarted_daemon_carries_from_its_mark() {
    CARRY_CRATES.set(carry_crates());
    let mut sim = Sim::new(34, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 34);
    let mut client = dial(&mut daemon);
    check_settled(&mut client, &mut daemon);
    daemon.driver().change(&name("w/src/main.rs"), b"fn main() { let _ = 1; }\n");
    check_settled(&mut client, &mut daemon);
    let before = crate::serve::front_carry::sim::take_built();
    assert_eq!(before.len(), 2, "a seed, then a carry: {before:?}");
    assert!(before[0].files > 0, "the crate's root is in the view's tree: {before:?}");

    let mut daemon = restart(daemon, &steps, 35);
    assert_eq!(daemon.started().carries, None, "the marked sessions were used");
    let mut client = dial(&mut daemon);
    daemon.driver().change(&name("w/src/main.rs"), b"fn main() {}\n");
    check_settled(&mut client, &mut daemon);
    let after = crate::serve::front_carry::sim::take_built();
    assert_eq!(after.len(), 1, "one pass: {after:?}");
    assert!(!after[0].first, "the restarted daemon opened the marked session, not a seed");
    assert_eq!(after[0].paths, 1, "carried from the root the marked session is pinned at");
    assert_eq!(after[0].carried, before[1].carried, "an edit of the same shape carries the same");
    assert_eq!(after[0].files, before[1].files, "and holds the same roots");
    assert_eq!(crate::serve::front_carry::sim::reads().0, 0, "no read of the carry's world on the loop");
}

/// Whether the first pass of `/w`'s carry after a crash opened a session: seeded and carried
/// over an edit, a snapshot (the journal and the store durable, the journal rewritten onto
/// the image with its marks), a crash, a start and the edit undone. `drops_marks` plants the
/// bug the control catches: the start keeps no mark.
fn carries_after_a_crash(seed: u64, drops_marks: bool) -> (bool, Option<String>) {
    CARRY_CRATES.set(carry_crates());
    let mut sim = Sim::new(seed, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, seed);
    let mut client = dial(&mut daemon);
    check_settled(&mut client, &mut daemon);
    daemon.driver().change(&name("w/src/main.rs"), b"fn main() { let _ = 1; }\n");
    check_settled(&mut client, &mut daemon);
    _ = crate::serve::front_carry::sim::take_built();
    let mark = format!("{}/w", super::local::CARRY);
    turn_until(&mut daemon, |it| it.vfs().marked(&mark).is_some());
    let snapshot = client.send(&mut daemon, Request::Snapshot);
    let got = client.wait(&mut daemon, snapshot);
    assert!(got.iter().any(|it| matches!(it, Response::Snapshotted { .. })), "{got:#?}");
    let mut sim = daemon.into_driver();
    sim.crash();
    super::daemon::DROPS_MARKS.set(drops_marks);
    let mut daemon = start(sim, &steps, seed + 1000);
    super::daemon::DROPS_MARKS.set(false);
    let said = daemon.started().carries.clone();
    let mut client = dial(&mut daemon);
    daemon.driver().change(&name("w/src/main.rs"), b"fn main() {}\n");
    check_settled(&mut client, &mut daemon);
    let after = crate::serve::front_carry::sim::take_built();
    assert_eq!(after.len(), 1, "one pass: {after:?}");
    (!after[0].first, said)
}

/// The invariant: killed once a pass's puts and its mark are durable (no stop, so no image
/// is written at the end), the next build's pass opens the marked session (`Pass::Open`), not
/// a seed (`Pass::Seed`), across seeds of the crash's torn tails.
#[test]
fn a_carry_mark_survives_a_crash_and_opens_the_session() {
    for seed in [36, 37, 38, 39] {
        let (opened, said) = carries_after_a_crash(seed, false);
        assert_eq!(said, None, "seed {seed}: every mark was used");
        assert!(opened, "seed {seed}: the pass after the crash seeded");
    }
}

/// The control: a start that drops the marks seeds, which the oracle above catches.
#[test]
fn the_oracle_catches_a_start_that_keeps_no_carry_mark() {
    let (opened, _) = carries_after_a_crash(36, true);
    assert!(!opened, "the planted start opened a session it had no mark of");
}

/// A writer of the machine's sealed layer (`<std>/store`, [`Options::std`]) other than the
/// daemon's own, on a simulation of its own: what `blaze store install` or another daemon's
/// std writer commits. Its files, every one it wrote, as they are once committed.
fn committed_by_another_writer(key: blaze_cas::Key, bytes: &[u8]) -> Vec<(FileName, Vec<u8>)> {
    fn driven(sim: &mut Sim, other: &mut Store, effects: &mut Vec<Effect>) {
        blaze_core::run_until_idle(sim, effects, |completion, effects| {
            if other.owns(completion.tag) {
                other.complete(completion, effects);
            }
        })
        .expect("the effects end");
    }
    let mut sim = Sim::new(7, Faults::default());
    sim.record(vec![name(STD)]);
    let writer = Hasher::new("blaze v2 test: another writer of the layer").finish();
    let config = blaze_cas::SharedConfig {
        root: name(&format!("{STD}/store")),
        writer,
        tags: 1 << 40,
        load_tags: (1 << 40) + (1 << 32),
        loader: blaze_cas::Loader::Pumped,
    };
    let mut effects = Vec::new();
    let mut other = Store::open_shared(config, &mut effects);
    driven(&mut sim, &mut other, &mut effects);
    assert!(other.ready(), "{:?}", other.broken());
    other.put(key, blaze_cas::Kind::Code, bytes).unwrap();
    other.flush(&mut effects);
    driven(&mut sim, &mut other, &mut effects);
    let head = name(&format!("{STD}/store/w-{writer:x}/head"));
    let mut files: Vec<FileName> = sim.recorded().iter().map(|(_, file, _)| file.clone()).collect();
    files.push(head.clone());
    files.sort();
    files.dedup();
    // The head last, as a commit makes it: a reader that lists the layer meanwhile sees the
    // writer's last head and the packs it names.
    files.retain(|it| *it != head && sim.exists(it));
    files.push(head);
    files.into_iter().map(|file| (file.clone(), sim.contents(&file))).collect()
}

/// The invariant: a sealed record a writer of the machine's layer commits after the daemon
/// started is found through the home store by the next build: its lowering asks the home store
/// to look at its layers again (`job.look_home`), and the job's units wait for that look, so a
/// sealed unit's record committed meanwhile is replayed, never built again. The control: the
/// same record before any build, when the home store has looked only at its start, is not
/// found, so what the build finds is the look's doing.
#[test]
fn a_sealed_record_committed_to_the_layer_after_the_start_is_seen_by_the_next_build() {
    for seed in 0..4 {
        let mut sim = Sim::new(600 + seed, Faults::default());
        checkout(&mut sim, "[package]\nname = \"app\"\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        assert!(daemon.store().ready(), "seed {seed}: the home store opened over a layer directory it made");
        let key = blaze_cas::Key(Hasher::new("blaze v2 test: a sealed record").part(&seed.to_le_bytes()).finish());
        for (file, bytes) in committed_by_another_writer(key, b"a sealed unit's record") {
            daemon.driver().change(&file, &bytes);
        }
        assert!(
            daemon.store().get(&key).unwrap().is_none(),
            "seed {seed}: found before a look: the test would prove nothing"
        );
        let mut client = dial(&mut daemon);
        let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
        let got = client.wait(&mut daemon, id);
        assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
        let found = daemon.store().get(&key).unwrap().map(|entry| entry.bytes.to_vec());
        assert_eq!(found.as_deref(), Some(&b"a sealed unit's record"[..]), "seed {seed}: the build did not look at the layer again");
    }
}

/// What a build of `/w` with an edit after a stop and a start, or after a crash, saw of the body
/// entries' marks: each lowering's marks, and whether the store the edit's build read holds the
/// value each mark of its lowering names. `crashes`: the first build is answered and the daemon
/// loses power then, unsettled; else it settles and stops.
fn body_marks_after(seed: u64, crashes: bool) -> (Vec<BTreeMap<String, blaze_cas::Key>>, Result<(), String>) {
    const EDITED: &[u8] = b"fn main() { let _ = 2; }\n";
    BODY_SESSIONS.set(true);
    BODY_SAVES.set(0);
    BODY_MARKS_SEEN.with_borrow_mut(Vec::clear);
    OUTPUT.set(true);
    let mut sim = Sim::new(seed, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, seed);
    let mut daemon = match crashes {
        true => {
            let mut client = dial(&mut daemon);
            let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
            assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }), "seed {seed}");
            let mut sim = daemon.into_driver();
            sim.crash();
            start(sim, &steps, seed + 1000)
        }
        false => {
            assert_eq!(build_settled(&mut daemon).last(), Some(&Response::Done { ok: true }), "seed {seed}");
            restart(daemon, &steps, seed + 1)
        }
    };
    daemon.driver().change(&name("w/src/main.rs"), EDITED);
    assert_eq!(build_settled(&mut daemon).last(), Some(&Response::Done { ok: true }), "seed {seed}");
    let seen = BODY_MARKS_SEEN.with_borrow_mut(std::mem::take);
    let held = match seen.last() {
        None => Err(format!("seed {seed}: no lowering ran")),
        Some(marks) => marks
            .iter()
            .find(|(_, key)| daemon.store().get(key).unwrap().is_none())
            .map_or(Ok(()), |(krate, key)| Err(format!("seed {seed}: `{krate}`'s mark names {key:?}, which the store lost"))),
    };
    BODY_SESSIONS.set(false);
    OUTPUT.set(false);
    (seen, held)
}

/// Rule 26 for the body entries' sessions: the root a unit saved its crate's session under is
/// marked in the journal once the unit's puts are durable (`Jobs::body_sessions`), and after a
/// stop and a start the edit's job opens it (`JobInput::body_marks`, which a serve opens and
/// carries when its front end kept none).
///
/// The invariant: the first build's lowering sees no mark; the edit's build after the restart
/// sees exactly the first build's root for `app`, and the store holds the value it names. The
/// control is [`the_oracle_catches_a_mark_made_before_its_puts_are_durable`].
#[test]
fn a_restart_then_an_edit_opens_each_crates_marked_session() {
    for seed in [161, 163, 165] {
        let (seen, held) = body_marks_after(seed, false);
        assert_eq!(seen.first(), Some(&BTreeMap::new()), "seed {seed}: a checkout never built has no mark");
        let first = body_session(1).0;
        assert_eq!(seen.last(), Some(&BTreeMap::from([("app".to_owned(), first)])), "seed {seed}: {seen:?}");
        held.unwrap();
    }
}

/// The invariant: a mark never names a session whose puts a crash can lose: across seeds whose
/// crash keeps a random prefix of what was not synced, every mark the next daemon's job opens
/// names a value its store holds. The control: a loop that marks a session before its puts are
/// durable (`job::MARK_EARLY`) leaves, for some seed, a mark whose value the crash lost.
#[test]
fn the_oracle_catches_a_mark_made_before_its_puts_are_durable() {
    let seeds = 173..189;
    for seed in seeds.clone() {
        let (_, held) = body_marks_after(seed, true);
        held.unwrap();
    }
    super::job::MARK_EARLY.set(true);
    let planted: Vec<Result<(), String>> = seeds.map(|seed| body_marks_after(seed, true).1).collect();
    super::job::MARK_EARLY.set(false);
    assert!(planted.iter().any(Result::is_err), "no crash lost a session marked early: {planted:?}");
}

/// How many turns past its answer the first build of `/w` (on seed `seed`) takes until what it
/// left is settled: its kept front end stored, its puts durable, its outputs placed.
fn turns_to_settle(seed: u64) -> usize {
    OUTPUT.set(true);
    let mut sim = Sim::new(seed, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, seed);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }), "seed {seed}");
    let mut turns = 0;
    while !daemon.settled() {
        assert!(turns < TURNS, "seed {seed}: the build never settled");
        daemon.turn();
        turns += 1;
    }
    OUTPUT.set(false);
    turns
}

/// The first build of `/w` on seed `seed`, answered, then `after` turns, then the power lost
/// (the simulation keeps a random prefix of what was not synced); a daemon started on that disk
/// asked the same build. Whether it was answered from a certificate (no plan load ran: no job),
/// and which of the values the first build's kept front end put after its answer (the records
/// that certificate rests on, `Fake::store_kept`) that daemon's store lacks.
fn certificate_after_a_crash(seed: u64, after: usize) -> (bool, Vec<blaze_cas::Key>) {
    OUTPUT.set(true);
    let mut sim = Sim::new(seed, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, seed);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }), "seed {seed}");
    for _ in 0..after {
        daemon.turn();
    }
    let mut sim = daemon.into_driver();
    sim.crash();
    let mut daemon = start(sim, &steps, seed + 1000);
    PLANS.with(|it| it.set(0));
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}, crashed {after} turns past the answer: {got:#?}");
    let answered = PLANS.with(Cell::get) == 0;
    let missing = [kept_key("/w", 0), kept_key("/w", 1)]
        .into_iter()
        .filter(|key| daemon.store().get(key).unwrap().is_none())
        .collect();
    OUTPUT.set(false);
    (answered, missing)
}

/// Every crash between the answer and the commit of what the build put after it, across seeds:
/// for each, whether a certificate answered and what its records' store lacked.
fn crashes_after_the_answer(seeds: std::ops::Range<u64>) -> Vec<(u64, usize, bool, Vec<blaze_cas::Key>)> {
    let mut seen = Vec::new();
    for seed in seeds {
        // A few turns past settled: the certificate's own puts and its slot's write.
        for after in 0..=turns_to_settle(seed) + 4 {
            let (answered, missing) = certificate_after_a_crash(seed, after);
            seen.push((seed, after, answered, missing));
        }
    }
    seen
}

/// The answer before the record (/tmp/plan/12-edit-floor.md E3): what a build puts after its
/// answer (its kept front end's records, and its units' records once they move there) settles
/// later, and its request certificate is written by the loop only once that is durable
/// (`Jobs::certify_held`, `Jobs::certify_settled`), never as the build ends.
///
/// The invariant: crashed at every turn between the answer and well past its puts' commit, on
/// every seed, a restarted daemon that answers the same request from a certificate has every
/// record the first build put after its answer; and some crash late enough is answered from the
/// certificate, so the oracle is not vacuous. The control, the certificate written as the build
/// ends (`job::CERTIFY_FIRST`), leaves for some crash a certificate that answers over a record
/// the crash lost.
#[test]
fn a_crash_after_the_answer_leaves_no_certificate_without_its_record() {
    let seeds = 201..204;
    let seen = crashes_after_the_answer(seeds.clone());
    for (seed, after, answered, missing) in &seen {
        assert!(
            !answered || missing.is_empty(),
            "seed {seed}, crashed {after} turns past the answer: a certificate answered over records the crash lost: {missing:?}"
        );
    }
    assert!(seen.iter().any(|it| it.2), "no crash was answered from a certificate: the oracle proves nothing: {seen:?}");
    super::job::CERTIFY_FIRST.set(true);
    let planted = crashes_after_the_answer(seeds);
    super::job::CERTIFY_FIRST.set(false);
    assert!(
        planted.iter().any(|(_, _, answered, missing)| *answered && !missing.is_empty()),
        "the certificate written first never answered over a lost record: {planted:?}"
    );
}
