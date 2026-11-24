//! `blaze run` and `blaze test` start from the engine's answer (docs/v2/plan/design/run.md,
//! "Run and test read the engine's answer", D147; "THE CONTRACT OF E3", piece R1).
//!
//! The command is the engine's check ([`check_for_start`]), its answer printed, then the start
//! over what that check stored ([`Standing`]): what main.rs's `Effect::StartProgram` and
//! `Effect::StartTests` arms did over the old request, with the request's parts replaced by
//! the answer's:
//!
//! - the certificate is the answer's root (`Standing::root`): the start list's and the run
//!   tables' (run_ids.rs `table_key`);
//! - each crate's names row is the answer's (`Standing::names`, run_names.rs `FromEngine`);
//! - the body ids are walked over E2's answer (engine_run_answer.rs `EngineAnswer`), or read
//!   at the root by one get when a start under this root already wrote them;
//! - the native image is the record keyed by its inputs (engine_native.rs `image_for`, R2).
//!
//! The gate is the call: nothing here refuses the start for an error the check printed. A body
//! the program calls is refused by the parent when its check does not run (D151, run_ids.rs
//! `Checked::runs`: types whole and no error; run_parent.rs
//! `ParentError::NotChecked`, with the body's finding); a body nothing calls is never looked at.

use blaze_drive::Key;
use blaze_drive::key::KINDS;
use blaze_drive::{Command, Kind, Tool};
use blaze_ids::ContentId;
use blaze_own::Alloc;
use bstd::entry::Status;
use bstd::out::Out;
use bstd::Cx;
use core::fmt;

use crate::body_types::{SyncPackDirs, TreesSeen};
use crate::engine_check::{Standing, check_for_start};
use crate::engine_records::EngineRecords;
use crate::phases::Phases;
use crate::run_ids::{RunIds, RunIdsError};
use crate::{CliError, program, run_names, run_package};

/// The start list's shape for `run` of the program at `root` (start_list.rs `ListName::of`): the
/// command and the program's crate root, never the checkout, so two worktrees of one program
/// share one list.
const RUN_SHAPE: &str = "blaze run: the start list of a program 1";

/// The start's profile beside the check's (print.rs `PROFILE`, which the check wrote): the
/// command's phases with the run's lines (`run  start list: placed ..`, phases.rs), written after
/// the start whether it ended well or not, so why a start did what it did is read (rule 17).
/// Named from clean.rs's one list of a checkout's entries, so `blaze clean` knows it.
pub(crate) const START_PROFILE: &str = crate::clean::CheckoutEntry::Start.path();

/// Writes [`START_PROFILE`] under the checkout, in place, never synced (as the check's profile).
pub(crate) fn write_start_profile(cx: &Cx, root: &bstd::fs::Dir<bstd::fs::ReadWrite>, phases: &Phases, frame: &bstd::mem::Frame) -> Result<(), CliError> {
    let text = bstd::mem::format_in!(frame, "{phases}");
    crate::print::ensure_checkout_ignore(cx, root)?;
    root.publish(cx, &bstd::path::Path::new(START_PROFILE)?, bstd::fs::Content::bytes(text.as_bytes()), bstd::fs::Durable::No)?;
    Ok(())
}

/// Why `run` or `test` did not start from the engine's answer, naming the thing (rule 19).
#[derive(Debug)]
pub(crate) enum StartError {
    /// The program's native image could not be had (engine_native.rs `NativeError`): its words,
    /// kept in the command's frame-free form because the error outlives the start's frame.
    Native { shown: std::boxed::Box<str> },
    /// The body ids of the answer could not be had (run_ids.rs).
    BodyIds { shown: std::boxed::Box<str> },
    /// A walk found no program (run_names.rs `found_answered` always walks: blaze's bug).
    NoProgram,
    /// The answer has no `Units` row with a record: the program's entry has no crate list.
    NoUnits,
    /// A test unit's run place (its package root and run-time variables) could not be made
    /// (test_drive.rs `package_run`): its words, frame-free as [`StartError::BodyIds`]'s.
    TestPlace { shown: std::boxed::Box<str> },
    /// The root manifest's `[profile.dev]` says `panic = "abort"`, and the test program of
    /// `test` would link `dependency`, a crate the unit graph built under that profile's
    /// `panic = "abort"`. Cargo builds every crate of a test program to unwind (the Cargo Book,
    /// Profiles, `panic`: "Tests, benchmarks, build scripts, and proc macros ignore the `panic`
    /// setting"); blaze's unit graph has one unit per dependency for the bin's graph and the
    /// test's alike, so the unwinding one is not built yet (rule 3: said, never linked silently).
    AbortTests { test: std::boxed::Box<str>, dependency: std::boxed::Box<str> },
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartError::Native { shown } => write!(f, "the program's native image: {shown} (engine_native.rs `image_for`)"),
            StartError::BodyIds { shown } => write!(f, "the body ids of the answer: {shown} (run_ids.rs, engine_start.rs `body_ids`)"),
            StartError::NoProgram => write!(f, "the walk of the answer found no program (run_names.rs `found_answered`, engine_start.rs `run_program`)"),
            StartError::NoUnits => write!(f, "the checkout's answer has no `Units` record: the program's entry cannot be found (engine_start.rs `run_program`)"),
            StartError::TestPlace { shown } => write!(f, "a test unit's run place: {shown} (engine_start.rs `run_tests`)"),
            StartError::AbortTests { test, dependency } => write!(
                f,
                "{ABORT_TESTS}: `Cargo.toml` `[profile.dev]` sets `panic = \"abort\"`, and the tests of `{test}` link `{dependency}`, built under it (engine_start.rs `abort_dependency`)"
            ),
        }
    }
}

/// The words of [`StartError::AbortTests`], said first.
pub(crate) const ABORT_TESTS: &str = "tests of a profile with panic = \"abort\" need their dependencies built to unwind: not built yet";

/// The first test unit of `tests` (crate rows of `list`) that would link a dependency built
/// under the profile's `panic = "abort"`, and that dependency: `None` when the profile unwinds,
/// or no test unit has such a dependency. A dependency so built is a target-side crate that is
/// neither a proc macro (its strategy is the macro side's cut, blaze-world panic.rs
/// `unit_panic`) nor the toolchain's (`core`, `alloc`, `std`, bstd: precompiled, not built by the
/// profile); a test unit's own cfg unwinds (blaze-unit-graph `ignores_profile_panic`). Its
/// direct dependencies suffice: a target-side crate's dependencies are target-side or the
/// toolchain's, and a proc macro's are the macro side's.
pub(crate) fn abort_dependency<'b>(
    profile: blaze_manifest::layout::panic::ProfilePanic,
    list: &blaze_world::crate_list::CrateList<'b>,
    tests: impl IntoIterator<Item = usize>,
) -> Option<(usize, usize)> {
    use blaze_world::crate_list::CrateGraph as _;
    if profile == blaze_manifest::layout::panic::ProfilePanic::Unwind {
        return None;
    }
    tests.into_iter().find_map(|test| {
        list.deps(test)
            .find(|&dep| list.side(dep) == blaze_world::Side::Target && !list.proc_macro(dep) && !list.toolchain(dep))
            .map(|dep| (test, dep))
    })
}

/// What `blaze run` and `blaze test` make their proc-macro runner from, at the FIRST proc-macro
/// call a body the run lowers meets (job_texts.rs `MadeOnce` over [`RunProcs::make`]), never at
/// setup: a run whose bodies call no proc macro lays no units and makes no names world. Before
/// (C9, wave-1119 t1515: 31 tempo tests NOT RUN), both passed no runner, so EVERY such call was
/// refused (`Host(HostError { ask: ProcMacro, why: Refused })`; tempo: ruint's `uint!` in a
/// dependency's `const`). It is `check`'s runner by the one maker (proc_runner.rs `make`: the
/// sandboxed wasm host, rules 9 and 10) over the names world made by the one maker of it
/// (names.rs `world`) from the unit graph's record in the snapshot the check committed, so each
/// call is keyed as `check` keys it and an expansion `check` kept is read by its key, never run
/// again (rule 77); this command's new answers are kept at its end ([`end_procs`]).
/// Every input is held by reference and borrows nothing the run holds mutably (the store is
/// read only through the committed snapshot). COST: none until the first call; then one
/// units layout, one names world and one runner, once a command.
pub(crate) struct RunProcs<'n, 'e> {
    pub(crate) cx: &'n Cx,
    pub(crate) root: &'n bstd::fs::Dir<bstd::fs::ReadWrite>,
    pub(crate) snapshot: &'n crate::store::StoreSnapshot,
    /// The answer's `UnitGraph` row (`Standing::graph`).
    pub(crate) graph: blaze_drive::Input,
    pub(crate) env: &'n crate::fetch::FetchEnv<'e>,
    pub(crate) frame: &'n bstd::mem::Frame,
}

impl<'n> RunProcs<'n, '_> {
    /// The runner, as engine_check.rs makes it on a Names miss (its `MacroSide`, engine_check.rs
    /// `checking`), or why it could not be made, in the frame (rule 19).
    pub(crate) fn make(&self) -> crate::job_texts::MadeProcs<'n> {
        let (cx, frame, env) = (self.cx, self.frame, self.env);
        let _cx = cx.span("run: the proc-macro runner, made at the first proc-macro call");
        let said = |what: fmt::Arguments<'_>| -> &'n str { crate::work::held_str(bstd::mem::format_in!(frame, "{}", what).as_str(), frame) };
        let bytes = match self.snapshot.get(self.graph.at) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Err(said(format_args!("no proc-macro runner: the unit graph's record is not in the store's snapshot (engine_start.rs `RunProcs`)"))),
            Err(_) => return Err(said(format_args!("no proc-macro runner: the unit graph's record could not be read from the store's snapshot (engine_start.rs `RunProcs`)"))),
        };
        let payload = match blaze_drive::record::open(bytes) {
            Ok(blaze_drive::record::Stored::Whole(view)) => view.payload,
            Ok(blaze_drive::record::Stored::Shape(_)) => return Err(said(format_args!("no proc-macro runner: the unit graph's record is a shape, not a record"))),
            Err(error) => return Err(said(format_args!("no proc-macro runner: the unit graph's record is malformed: {error:?}"))),
        };
        let graph = crate::graph_payload::sections(payload).map_err(|error| said(format_args!("no proc-macro runner: the unit graph's record: {error}")))?.graph;
        let stamps = crate::sweep::FromMeta::now(&env.clock);
        let mut rows = crate::sweep::Rows::new_in(frame);
        let erased = blaze_own::Erased::new(frame);
        let laid = crate::units::lay(cx, self.root, &stamps, &mut rows, env.toolchain, env.heap, frame, erased.handle())
            .map_err(|error| said(format_args!("no proc-macro runner: the workspace's layout: {error}")))?;
        let Some(lockfile) = laid.lock.as_ref() else {
            return Err(said(format_args!("no proc-macro runner: the workspace has no `Cargo.lock` the units read, so no macro crate is located (units.rs `Laid::lock`)")));
        };
        let members = crate::names::member_dirs(Some(&laid));
        let names = crate::names::world(cx, self.root, graph, Some(lockfile.lock), members, env)
            .map_err(|error| said(format_args!("no proc-macro runner: the names world: {error}")))?;
        let world = names.made().ok_or_else(|| said(format_args!("no proc-macro runner: the names world was made and not kept (names.rs `world`)")))?;
        let side = crate::proc_runner::MacroSide { lock: lockfile, place: crate::macros::Place { home: env.home, checkout: env.checkout, members, rows: &rows }, heap: env.heap };
        crate::proc_runner::make(world, *names.list(), &side).map_err(|error| said(format_args!("no proc-macro runner: {error} (proc_runner.rs `make`)")))
    }
}

/// The command's end for its runner by need: this command's new proc-macro answers kept to
/// their store (proc_runner.rs `keep`, as engine_check.rs keeps the check's), so the next command
/// reads each by its key; or, when a call asked for a runner and none could be made, why, said
/// once on `err` (each call's refusal is its body's, with C8 that test's NOT RUN). Nothing when
/// no call asked.
pub(crate) fn end_procs<'n, F: Fn() -> crate::job_texts::MadeProcs<'n>>(cx: &Cx, maker: &crate::job_texts::MadeOnce<'n, F>, err: &Out, command: &str) -> Result<(), CliError> {
    match maker.made() {
        None => {}
        Some(Ok(procs)) => {
            procs.keep().map_err(CliError::ProcAnswers)?;
        }
        Some(Err(words)) => {
            err.line(cx, format_args!("blaze {command}: every proc-macro call a body met was refused: {words}")).map_err(CliError::Out)?;
        }
    }
    Ok(())
}

/// main.rs's `run` and `test` arms: the arguments, the engine's check, then the start. `run`'s
/// words go to stderr (its stdout is its program's), `test`'s to stdout (main.rs `words`).
#[expect(clippy::too_many_arguments, reason = "the command's edge: each argument one specific capability")]
pub(crate) fn command(
    cx: &Cx,
    args: &bstd::args::Args,
    fs: &bstd::fs::Fs,
    env: Option<&bstd::env::Env>,
    net: Option<&bstd::net::Net>,
    host: &bstd::host::Host,
    clock: &bstd::clock::Clock,
    rng: &mut bstd::rng::Rng,
    out: &Out,
    err: &Out,
    phases: &mut Phases,
    heap: &'static blaze_own::Heap,
    command: Command,
) -> Result<Status, CliError> {
    let mut packages = blaze_own::Vec::new_in(cx.frame());
    // `-p` picks from the engine's unit graph for `run` and `test` alike (R3).
    let flags = crate::parse_flags(args, |name| packages.push(name))?;
    if flags.fix.is_some() {
        return Err(CliError::Usage("--fix is `blaze check`'s"));
    }
    if command != Command::Run && flags.open == blaze_run::viewer::OpenExplore::Open {
        return Err(CliError::Usage("--open is `blaze run`'s"));
    }
    if flags.output.is_some() {
        return Err(CliError::Usage("-o / --output is `blaze export`'s"));
    }
    let select = match command {
        Command::Test => run_package::Select { test_packages: packages.as_slice(), bin: flags.select.bin, ..run_package::Select::NONE },
        Command::Run | Command::Check => run_package::Select { test_packages: packages.as_slice(), ..flags.select },
    };
    let launch = program::Launch { fs, env, host, out, err, select, open: flags.open, grants: &flags.grants, args: &flags.program };
    let words = crate::words(command, out, err);
    // `blaze run` / `blaze test`'s live inline TUI on a terminal's stderr (run_line.rs).
    let screen_cmd = match command {
        Command::Run => blaze_screen::Command::Run,
        Command::Test => blaze_screen::Command::Test,
        Command::Check => blaze_screen::Command::Check,
    };
    let view = flags.view.unwrap_or(blaze_report::frame::plain::View::Person);
    let line = crate::run_line::LineOpts::from_env(env, err, flags.color, screen_cmd, view, flags.status_every.is_some());
    let line = crate::run_line::LineOpts { live: line.live && matches!(command, Command::Run | Command::Test), ..line };
    let lined = crate::run_line::watched(cx, err, clock, line, |watch, bg| {
        watch.see(blaze_screen::Step::Began(blaze_screen::Stage::Checking));
        let bg_live = if line.live { Some(bg) } else { None };
        check_for_start(cx, Some(args), fs, env, net, host, rng, words, err, phases, heap, &flags, command == Command::Run, bg_live, |standing, phases, rng| {
            let (root, frame) = (standing.checkout, standing.frame);
            // The member's folder the command was given selects when no `-p` does (R3).
            let select = run_package::Select { member: standing.member, ..launch.select };
            let started = match command {
                Command::Run => run_package::pick(cx, root, select, frame).and_then(|picked| run_program(cx, standing, &picked, &launch, phases, watch)),
                Command::Test => run_package::test_packages(cx, root, select, frame).and_then(|packages| run_tests(cx, standing, &packages, select.bin, &launch, clock, rng, phases, watch)),
                Command::Check => Err(CliError::Usage("`check` is engine_check.rs's; engine_start.rs starts `run` and `test`")),
            };
            write_start_profile(cx, root, phases, frame)?;
            started
        })
    });
    lined.map_err(CliError::RunLine)?
}

/// The body ids of the answer at its root (run_ids.rs): one get when a start under this root
/// wrote them, else walked over E2's answer from the workspace's bodies and written at the root.
/// Either way they read each workspace body's reads BY NEED from the stored rows in place
/// (`Standing::rows`, run_ids.rs `BodyReads`), counted in `asked` for the profile's line.
/// COST: a hit, one get; a miss, O(workspace bodies) row reads and one put; no read list.
pub(crate) fn body_ids<'a, 'r: 'a, 'm: 'a>(
    cx: &Cx, standing: &mut Standing<'r, 'm>, list: &blaze_world::crate_list::CrateList<'_>, asked: &'a crate::run_ids::Asked, bump: blaze_own::Bump<'a>,
) -> Result<RunIds<'a>, CliError> {
    let alloc = bump.alloc();
    let frame = standing.frame;
    let refused = |error: RunIdsError<'_>| CliError::Start(StartError::BodyIds { shown: std::boxed::Box::from(bstd::mem::format_in!(frame, "{error}").as_str()) });
    let cert = Key(standing.root);
    let (toolchain, tools): (_, [Tool; KINDS]) = (standing.toolchain, standing.tools);
    let most = crate::run_ids::Most::USUAL;
    let from = crate::run_ids::ReadsFrom::new(standing.rows(), asked, most);
    let reading = |ids: RunIds<'a>| ids.reading::<crate::run_ids::Production>(from).map_err(|why| refused(RunIdsError::Unread(why)));
    if let Some(ids) = RunIds::from_record(cx, &mut *standing.store, cert, toolchain, &tools, bump).map_err(refused)? {
        return reading(ids);
    }
    // Each member unit's root and view: the walk asks each unit's row of a body at its view (D191).
    let units = crate::run_ids::unit_roots(list, alloc).map_err(refused)?;
    let mut answer = standing.answer();
    let walk = crate::run_ids::workspace_of(cx, &mut answer, tools[Kind::BodyCheck.index()], &units, None, alloc).map_err(refused)?;
    let ids = RunIds::new(cx, toolchain, list, &tools, &walk.bodies, &walk.edges, &mut answer, None, most, bump).map_err(refused)?;
    drop(answer);
    ids.put(cx, &mut *standing.store, cert, &tools, alloc).map_err(refused)?;
    reading(ids)
}

/// Builds `work::Facts` for `run`, `test`, and `export` from `standing` (the one home for
/// `Facts` on the start path, rule 30).
pub(crate) fn start_facts<'f>(
    standing: &Standing<'f, '_>,
    fs: &bstd::fs::Fs,
    phases: &Phases,
    lints: crate::lints::Lints,
) -> crate::work::Facts<'f, crate::sweep::FromMeta> {
    let (frame, heap) = (standing.frame, standing.heap);
    crate::work::Facts {
        stamps: crate::sweep::FromMeta::now(phases.clock()),
        rows: crate::sweep::Rows::new_in(frame),
        found: crate::sweep::Found::new_in(frame),
        laid: None,
        plant: crate::work::Sizes::USUAL.plant,
        workers: standing.workers.get(),
        fetch_env: standing.place.fetch_env(phases.clock().clone(), standing.toolchain, fs.clone(), heap),
        macros: None,
        names: None,
        bodies: crate::body_types::Kept::default(),
        scripts: None,
        producer: standing.producers.get::<blaze_ids::producer::Whole>(),
        lints: lints.rules,
    }
}

/// `blaze run` over the check's answer: the program's crates from the answer's rows, its native
/// image by key, its body ids, then the program under the lazy parent (program.rs `run`), which
/// builds what it calls and refuses a called body whose check does not run (D151).
pub(crate) fn run_program(
    cx: &Cx, mut standing: Standing<'_, '_>, picked: &run_package::Program<'_>, launch: &program::Launch<'_>, phases: &mut Phases, watch: &dyn blaze_screen::Watch,
) -> Result<Status, CliError> {
    let (frame, heap) = (standing.frame, standing.heap);
    let erased = blaze_own::Erased::new(frame);
    let bump = erased.bump();
    let alloc = bump.alloc();
    watch.see(blaze_screen::Step::Began(blaze_screen::Stage::Crates));
    let mut read = EngineRecords::new(heap.alloc());
    let seen = TreesSeen::default();
    let pack_dirs = SyncPackDirs::default();
    let root = standing.checkout;
    let lints = crate::lints::of_root(cx, root, frame)?;
    // The names world's facts, made only on the unit map's miss (run_names.rs `unit_map`):
    // the program's crate is its `Bin` unit's, found through that map (`program_row`).
    let mut facts = start_facts(&standing, launch.fs, phases, lints);
    let answered = run_names::FromEngine { graph: standing.graph, names: standing.names };
    let at = run_names::program_row(cx, root, &answered, &mut *standing.store, &mut facts, picked.root, heap, frame).map_err(CliError::RunNames)?;
    let owner = run_names::Owner::At(at);
    let found = run_names::found_answered(cx, &answered, owner, &mut *standing.store, &mut facts.bodies, &mut read, heap, frame).map_err(CliError::RunNames)?;
    let program_found = found.program().ok_or(CliError::Start(StartError::NoProgram))?;
    // The program's crate first, then its scope's: from the crate list the walk read, so the
    // unit graph is not read again here (engine_native.rs reads it once more inside).
    let closure = program_found.closure_in(alloc);
    let native = crate::engine_native::image_for(cx, &mut standing, &closure, watch)
        .map_err(|error| CliError::Start(StartError::Native { shown: std::boxed::Box::from(bstd::mem::format_in!(frame, "{error}").as_str()) }))?;
    watch.see(blaze_screen::Step::Began(blaze_screen::Stage::BodyIds));
    let list = found.list().ok_or(CliError::Start(StartError::NoProgram))?;
    let asked = crate::run_ids::Asked::default();
    let began = phases.now();
    let ids = body_ids(cx, &mut standing, list, &asked, bump)?;
    let ids_ns = phases.since(began);
    // The line's table half now: the table itself is handed to the parent below.
    let ids_line = ids.line(&asked, ids_ns);
    let mut made = Some(ids);
    // The start list (start_list.rs): under the answer's root, named by the program; its
    // tables are the root's body ids and the program's image, so both stay rooted beside it.
    let cert = Key(standing.root);
    let shape = ContentId::of(RUN_SHAPE, picked.root.as_bytes());
    let today = crate::start_list::Today {
        certificate: crate::start_list::Certificate::of_answer(&standing),
        name: crate::start_list::ListName::of(&shape),
        tables: [blaze_cas::Key(crate::run_ids::table_key(cert, standing.toolchain, &standing.tools).0), blaze_cas::Key(native.key.0)],
    };
    // The units' record: the entry's crate list, as the effect's `units` key was.
    let units = (standing.names)(blaze_drive::Name::units()).ok_or(CliError::Start(StartError::NoUnits))?.at;
    let producer = standing.producers.get::<blaze_ids::producer::CodeBuilder>();
    let lent = standing.lent;
    // What the run reads each body's checked columns from (engine_body_record.rs `TypedFrom`):
    // the check's typed records, found through the answer's records in the command's store,
    // decoded into the process's heap, where the lazy program's tables live.
    let typed = crate::engine_body_record::TypedFrom { cx, lent, records: standing.records(), alloc: heap.alloc() };
    let env = &facts.fetch_env;
    // The runner, made at a body's first proc-macro call ([`RunProcs`]; C9).
    let inputs = RunProcs { cx, root, snapshot: found.snapshot(), graph: standing.graph, env, frame };
    let maker = crate::job_texts::MadeOnce::new(|| inputs.make());
    let procs = crate::job_texts::Procs::ByNeed(&maker);
    let store = &mut *standing.store;
    let (bodies, reads, seen_ref, dirs_ref) = (&mut facts.bodies, &mut read, &seen, &pack_dirs);
    let mut names = || {
        run_names::names_of(cx, root, &found, owner, &mut *store, &mut *bodies, &mut *reads, env, dirs_ref, procs, seen_ref, frame, heap)
            .map_err(|error| crate::run_parent::ParentError::Names { shown: std::boxed::Box::from(bstd::mem::format_in!(frame, "{error}").as_str()) })
    };
    let mut ids = || made.take().ok_or_else(|| crate::run_parent::ParentError::BodyIds { shown: std::boxed::Box::from("the body ids were asked a second time (run_parent.rs `CommandFront` asks once)") });
    let ran = program::run(cx, root, launch, picked, units, native.send, producer, lent, today, &mut names, &mut ids, typed, &mut phases.code_records, watch, heap, frame);
    end_procs(cx, &maker, standing.err, "run")?;
    let ids_line = crate::run_ids::IdsLine { read: asked.bodies(), folded: asked.reads(), ..ids_line };
    phases.run_names = Some(run_names::NamesRead::of(read.counts(), &seen).with_ids(ids_line));
    if let Some(why) = seen.refused() {
        return Err(CliError::RunNames(run_names::RunNamesError::Trees { why }));
    }
    match ran {
        Ok(status) => Ok(Status(status)),
        // A body the program called that was not checked clean, or did not lower: the program
        // did not run; its words are the parent's, said once (main.rs `run`).
        Err(CliError::Program(error)) => Err(CliError::Ran { command: "run", error }),
        Err(error) => Err(error),
    }
}

/// The crates a test program may link, its own crate first (engine_native.rs `image_for`'s
/// `closure`): every crate of the answer's crate list. The image walks the test unit's own
/// dependencies from it (native_image.rs `closure`, link order its own), so a crate the test
/// does not reach is never read; the run's closure (run_names.rs `closure_in`) is a body walk's,
/// which a test program has none of before its tests start. COST: O(crates) keys, per suite.
fn suite_closure<'a>(list: &blaze_world::crate_list::CrateList<'_>, test: usize, alloc: Alloc<'a>) -> blaze_own::AVec<'a, crate::engine_seeds::CrateAt> {
    use blaze_world::crate_list::CrateGraph as _;
    // Only `key` is read (`image_for` checks the walk's crates against the keys handed).
    let at_of = |at: usize| {
        let key = list.key(at);
        crate::engine_seeds::CrateAt { prefix: key.prefix(), key, names_at: None, member: list.member(at), closed: !list.member(at), toolchain: list.toolchain(at) }
    };
    let mut closure: blaze_own::AVec<'a, crate::engine_seeds::CrateAt> = blaze_own::Vec::with_capacity_in(list.len(), alloc);
    closure.push(at_of(test));
    closure.extend((0..list.len()).filter(|at| *at != test).map(at_of));
    closure
}

/// `blaze test` over the check's answer: the selected packages' suites from the answer's rows,
/// the body ids, then each suite under the lazy parent (test_drive.rs `run_suites`). A test
/// that calls a body whose check does not run (D151) is NOT RUN naming that body; the others run.
#[expect(clippy::too_many_arguments, reason = "the test runner's edge: each argument one specific capability or record")]
pub(crate) fn run_tests(
    cx: &Cx,
    standing: Standing<'_, '_>,
    packages: &[&str],
    bin: Option<&str>,
    launch: &program::Launch<'_>,
    clock: &bstd::clock::Clock,
    rng: &mut bstd::rng::Rng,
    phases: &mut Phases,
    watch: &dyn blaze_screen::Watch,
) -> Result<Status, CliError> {
    let status = run_tests_status(cx, standing, packages, bin, launch, clock, rng, phases, watch)?;
    Ok(if status.passed() { Status::OK } else { Status::FAILED })
}

/// [`run_tests`], returning the structured [`crate::test_loop::Status`] (passed/failed/ignored/not_run
/// counts) for `blaze ci` without duplicating the suite runner.
#[expect(clippy::too_many_arguments, reason = "the test runner's edge: each argument one specific capability or record")]
pub(crate) fn run_tests_status(
    cx: &Cx,
    mut standing: Standing<'_, '_>,
    packages: &[&str],
    bin: Option<&str>,
    launch: &program::Launch<'_>,
    clock: &bstd::clock::Clock,
    rng: &mut bstd::rng::Rng,
    phases: &mut Phases,
    watch: &dyn blaze_screen::Watch,
) -> Result<crate::test_loop::Status, CliError> {
    let (frame, heap) = (standing.frame, standing.heap);
    let erased = blaze_own::Erased::new(frame);
    let bump = erased.bump();
    let alloc = bump.alloc();
    let out = launch.out;
    let err = standing.err;
    watch.see(blaze_screen::Step::Began(blaze_screen::Stage::Crates));
    let mut read = EngineRecords::new(heap.alloc());
    let seen = TreesSeen::default();
    let pack_dirs = SyncPackDirs::default();
    let root = standing.checkout;
    let lints = crate::lints::of_root(cx, root, frame)?;
    let workers = standing.workers.get();
    // The names world's facts, made only on a test map's miss (run_names.rs `tests_of`).
    let mut facts = start_facts(&standing, launch.fs, phases, lints);
    let answered = run_names::FromEngine { graph: standing.graph, names: standing.names };
    let inventory = run_names::found_tests_answered(cx, root, &answered, &mut *standing.store, &mut facts, &mut read, packages, bin, heap, frame).map_err(CliError::RunNames)?;
    // A custom harness owns its arguments. Listing is a command operation, even for a selection
    // consisting entirely of custom harnesses.
    let harness_args: &[&str] = if inventory.suites.iter().any(|(_, unit)| unit.tests.harnessed()) || inventory.suites.is_empty() {
        launch.args
    } else if launch.args.contains(&"--list") {
        &["--list"]
    } else {
        &[]
    };
    let args = match crate::test_args::parse(harness_args, alloc) {
        Ok(args) => args,
        Err(error) => {
            err.line(cx, format_args!("blaze test: {error}")).map_err(CliError::Out)?;
            return Err(CliError::TestArgs);
        }
    };
    let mut plan = args.config(workers).map_err(CliError::TestPool)?;
    plan.width = plan.width.min(workers.max(1));
    let width = out.size().map_or(80, |size| size.cols.clamp(4, 120));
    let style = match args.format {
        blaze_run::report::Format::Text => blaze_run::report::Style::text(width, out.is_terminal()),
        blaze_run::report::Format::Json => blaze_run::report::Style::json(),
    };
    let suites = u32::try_from(inventory.suites.len()).map_err(|_| CliError::Text { what: "more than u32::MAX test units in this request" })?;
    let config = crate::test_loop::LoopConfig { plan, style, list: args.list, run: blaze_run::CheckoutKey(standing.root.0), suites };
    let asked = crate::run_ids::Asked::default();
    let mut ids_line = None;
    let status = if args.list {
        crate::test_drive::list(cx, &inventory.suites, config, &out, frame).map_err(|error| CliError::Test(blaze_own::Box::new_in(error, heap.alloc())))?
    } else {
        watch.see(blaze_screen::Step::Began(blaze_screen::Stage::BodyIds));
        let list = inventory.list().ok_or(CliError::Start(StartError::NoProgram))?;
        // Up front, before a body id is walked: a test program over dependencies built to abort
        // is refused by name (rule 3), never linked into an unwinding image.
        let profile = program::dev_profile_panic(cx, root, frame)?;
        if let Some((test, dependency)) = abort_dependency(profile, list, inventory.suites.iter().map(|(_, unit)| unit.at)) {
            use blaze_world::crate_list::CrateGraph as _;
            let shown = |at: usize| -> std::boxed::Box<str> { std::boxed::Box::from(bstd::mem::format_in!(frame, "{} {}", list.name(at), list.version(at)).as_str()) };
            return Err(CliError::Start(StartError::AbortTests { test: shown(test), dependency: shown(dependency) }));
        }
        let began = phases.now();
        let ids = body_ids(cx, &mut standing, list, &asked, bump)?;
        let ids_ns = phases.since(began);
        // Each suite's native image, as `run` has its program's (engine_native.rs `image_for`):
        // the C of every crate its test program links (a build script's deferred C built by
        // native_runner_c.rs), made once and found by its key after, told to every test's start
        // before any of its code (test_drive.rs `Launched`). One that cannot be had leaves that
        // suite's tests NOT RUN naming why (test_drive.rs `EntryCheck`), the others run. A crate
        // whose C is refused does not stop the image: its archives are left out, said by name,
        // and only a test whose code calls one of its C names stops (engine_native.rs `image_for`).
        // NOT DONE: a proc-macro crate's tests (the macro side, native_image.rs `linked`) get no
        // image, as `image_for` links target-side crates only; a C name one calls stays its
        // test's `dlsym` failure, named by the launcher.
        let mut images = blaze_own::Vec::with_capacity_in(inventory.suites.len(), frame);
        for (_, unit) in &inventory.suites {
            if !crate::native_image::linked(list, unit.at) {
                images.push(Ok(None));
                continue;
            }
            let closure = suite_closure(list, unit.at, alloc);
            let image = crate::engine_native::image_for(cx, &mut standing, &closure, watch)
                .map(|image| image.send)
                .map_err(|error| crate::work::held_str(bstd::mem::format_in!(frame, "{error}").as_str(), frame));
            images.push(image);
        }
        let overflow = program::dev_overflow(cx, root, frame)?;
        let panic = blaze_ladder::panic_strategy::of_profile(blaze_world::panic::TEST_PROFILE);
        // Boxed in the process heap: the error outlives this frame to `main`'s print.
        let parent = move |why| CliError::OwnProcess(blaze_own::Box::new_in(crate::own_process::OwnError::Parent(why), heap.alloc()));
        let isa = crate::run_parent::isa().map_err(parent)?;
        let producer = standing.producers.get::<blaze_ids::producer::CodeBuilder>();
        // One runner for every suite, made at the first proc-macro call any suite's body meets
        // ([`RunProcs`]; C9). Every suite's names records read one committed snapshot. Before
        // `runnable`, whose names records borrow it, so it outlives them.
        let inputs = inventory.suites.first().map(|(found, _)| RunProcs { cx, root, snapshot: found.snapshot(), graph: standing.graph, env: &facts.fetch_env, frame });
        let maker = crate::job_texts::MadeOnce::new(|| match &inputs {
            Some(inputs) => inputs.make(),
            // No suite: no names records, so nothing asks.
            None => Err("no proc-macro runner: this command has no test suite (engine_start.rs `run_tests`)"),
        });
        let procs = crate::job_texts::Procs::ByNeed(&maker);
        let mut runnable = blaze_own::Vec::with_capacity_in(inventory.suites.len(), frame);
        // Each test unit runs in its package's root, told its package's variables (test_drive.rs
        // `PackageRun`): the checkout's absolute root as the edge resolved it.
        let checkout = standing.place.served.root_text();
        for (at, (found, unit)) in inventory.suites.iter().enumerate() {
            let place = crate::test_drive::package_run(checkout, list.env_facts(unit.at), bump)
                .map_err(|why| CliError::Start(StartError::TestPlace { shown: std::boxed::Box::from(bstd::mem::format_in!(frame, "{why}").as_str()) }))?;
            // What the suite's names records are made of, kept for the command's code workers
            // (test_drive.rs `pooled`, D194), then the command thread's own names records of it.
            let named = cx.span("run names: open the names records");
            let crates = run_names::crates_of(&named, found, run_names::Owner::At(unit.at), &mut *standing.store, &mut facts.bodies, &mut read, &facts.fetch_env, &pack_dirs, frame, heap)
                .map_err(CliError::RunNames)?;
            drop(named);
            let names = run_names::names_in(cx, root, crates, procs, &seen, frame).map_err(CliError::RunNames)?;
            let builder = crate::run_parent::builder_for(unit.edition.name(), overflow, panic, producer, isa).map_err(parent)?;
            runnable.push(crate::test_drive::Suite { names, crates, builder, unit: *unit, place, native: images[at] });
        }
        let ran = crate::test_drive::run_suites(cx, launch.fs, launch.env, launch.host, clock, rng, launch.out, launch.err, &runnable, &ids, config, launch.args, standing.lent, root, procs, heap, watch, frame);
        end_procs(cx, &maker, err, "test")?;
        ids_line = Some(ids.line(&asked, ids_ns));
        ran.map_err(|error| CliError::Test(blaze_own::Box::new_in(error, heap.alloc())))?
    };
    let names_read = run_names::NamesRead::of(read.counts(), &seen);
    phases.run_names = Some(match ids_line {
        Some(line) => names_read.with_ids(line),
        None => names_read,
    });
    if let Some(why) = seen.refused() {
        return Err(CliError::RunNames(run_names::RunNamesError::Trees { why }));
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use crate::engine_body_facts::{Class, class_of};
    use crate::run_ids::Checked;

    /// The gate is the call (D151, `Checked::runs`): a body whose types were not checked whole is
    /// classed `Untyped`, which the body ids give as `Incomplete`, and is never run; a body whose
    /// types were checked whole and whose borrow check was refused is `Typed` and runs while
    /// borrows are notes. Planted control: the same body with no finding is `Clean` and runs.
    #[test]
    fn an_untyped_body_never_runs_and_a_typed_one_does() {
        let untyped = class_of([crate::body_types::TYPES_NOT_CHECKED]);
        let Class::Untyped { rule } = untyped else { panic!("a body whose types were not checked is {untyped:?}") };
        let body = blaze_typeck::ItemKey(7);
        assert!(Checked::Incomplete { rule }.runs(body).is_err(), "a body with no typed body to lower is not run");
        let typed = class_of([crate::body_types::BORROWS_NOT_CHECKED]);
        if !crate::body_types::BORROWS_ARE_ERRORS {
            let Class::Typed { rule } = typed else { panic!("a body whose borrows alone were not checked is {typed:?}") };
            assert!(Checked::Typed { rule }.runs(body).is_ok(), "types whole, borrows unread: run (D151)");
        }
        let errors = Checked::Errors { errors: core::num::NonZeroU32::MIN, first: 0 };
        assert_eq!(errors.runs(body), Err(crate::run_ids::NotRunnable::Checked(errors)), "a body with an error is not run, its verdict said");
        assert!(matches!(class_of(core::iter::empty::<u16>()), Class::Clean), "control: a body with no finding is Clean");
        assert!(Checked::Clean.runs(body).is_ok(), "control: a clean body runs");
    }
}
