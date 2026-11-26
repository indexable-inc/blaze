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

impl StartError {
    fn test_place(shown: &str) -> Self {
        Self::TestPlace { shown: shown.into() }
    }
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
    let flags = crate::parse_flags_in(args, cx.frame(), |name| packages.push(name))?;
    if flags.fix.is_some() {
        return Err(CliError::Usage("--fix is `blaze check`'s"));
    }
    if command != Command::Run && flags.open == blaze_run::viewer::OpenExplore::Open {
        return Err(CliError::Usage("--open is `blaze run`'s"));
    }
    if command != Command::Run && flags.quiet {
        return Err(CliError::Usage("-q / --quiet is `blaze run`'s"));
    }
    if flags.output.is_some() {
        return Err(CliError::Usage("-o / --output is `blaze build`'s"));
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
    let status_opts = crate::run_line::StatusOpts::for_run(screen_cmd, view, flags.quiet, line.terminal);
    let line = crate::run_line::LineOpts {
        live: line.live && matches!(command, Command::Run | Command::Test) && !flags.quiet,
        ..line
    };
    let start_scope = match command {
        Command::Run => crate::engine_check::StartScope::Bin(launch.select),
        Command::Test => crate::engine_check::StartScope::Test(launch.select),
        Command::Check => return Err(CliError::Usage("`check` is engine_check.rs's; engine_start.rs starts `run` and `test`")),
    };
    let checking_label = checking_target(&flags, env, cx.frame());
    let lined = crate::run_line::watched_run(cx, err, clock, line, status_opts, |watch, bg, status_out| {
        status_out.checking(checking_label.as_str());
        watch.see(blaze_screen::Step::Began(blaze_screen::Stage::Checking));
        let bg_live = if line.live { Some(bg) } else { None };
        check_for_start(cx, Some(args), fs, env, net, host, rng, words, err, phases, heap, &flags, command == Command::Run, start_scope, bg_live, |standing, phases, rng| {
            let (root, frame) = (standing.checkout, standing.frame);
            // The member's folder the command was given selects when no `-p` does (R3).
            let select = run_package::Select { member: standing.member, ..launch.select };
            let started = match command {
                Command::Run => match standing.picked {
                    Some(picked) => run_program(cx, standing, &picked, &launch, phases, watch, status_out),
                    None => run_package::pick(cx, root, select, frame).and_then(|picked| run_program(cx, standing, &picked, &launch, phases, watch, status_out)),
                },
                Command::Test => match standing.test_packages {
                    Some(packages) => run_tests(cx, standing, packages, select.bin, &launch, clock, rng, phases, watch),
                    None => run_package::test_packages(cx, root, select, frame).and_then(|packages| run_tests(cx, standing, &packages, select.bin, &launch, clock, rng, phases, watch)),
                },
                Command::Check => Err(CliError::Usage("`check` is engine_check.rs's; engine_start.rs starts `run` and `test`")),
            };
            write_start_profile(cx, root, phases, frame)?;
            started
        })
    });
    lined.map_err(CliError::RunLine)?
}

/// Concise label shown on `blaze run`'s initial `    Checking <label>` status line using only
/// parsed CLI flags and `PWD` (zero filesystem syscalls).
fn checking_target<'f>(
    flags: &crate::Flags<'_>,
    env: Option<&bstd::env::Env>,
    frame: &'f bstd::mem::Frame,
) -> bstd::mem::String<&'f bstd::mem::Frame> {
    match (flags.select.package, flags.select.bin) {
        (Some(pkg), Some(bin)) if bin != pkg => return bstd::mem::format_in!(frame, "{pkg} ({bin})"),
        (Some(pkg), _) => return bstd::mem::format_in!(frame, "{pkg}"),
        (None, Some(bin)) => return bstd::mem::format_in!(frame, "{bin}"),
        (None, None) => {}
    }
    if let Some(dir) = flags.dir {
        let trimmed = dir.trim_end_matches('/');
        if !trimmed.is_empty() && trimmed != "." {
            return bstd::mem::format_in!(frame, "{trimmed}");
        }
    }
    if let Some(env) = env
        && let Ok(name) = bstd::env::VarName::new("PWD")
        && let Ok(Some(pwd)) = env.get(&name)
    {
        let trimmed = pwd.trim_end_matches('/');
        if let Some(base) = trimmed.rsplit('/').next().filter(|s| !s.is_empty()) {
            return bstd::mem::format_in!(frame, "{base}");
        }
    }
    bstd::mem::format_in!(frame, "workspace")
}

/// Domain for folding a non-whole `CheckScope` into the `RunIds` table's certificate so two
/// scoped starts reusing the same whole-workspace stored answer root never collide on one table.
const SCOPED_IDS_CERT_DOMAIN: &str = "blaze-cli run ids scoped cert 1";

/// The certificate key for `RunIds` of `standing`: equal to `Key(root)` for whole-workspace runs,
/// and folded with `scope` for scoped runs so each scope owns a disjoint `RunIds` record.
pub(crate) fn body_ids_cert(root: ContentId, scope: crate::engine_check::CheckScope<'_>) -> Key {
    let put_len = |h: &mut blaze_ids::Hasher, n: usize| {
        h.update(&u64::try_from(n).unwrap_or(u64::MAX).to_le_bytes());
    };
    match scope {
        crate::engine_check::CheckScope::Whole => Key(root),
        crate::engine_check::CheckScope::Bin { package, root: bin_root } => {
            let mut h = blaze_ids::Hasher::new(SCOPED_IDS_CERT_DOMAIN);
            h.id(&root).update(&[1]);
            put_len(&mut h, package.len());
            h.update(package.as_bytes());
            put_len(&mut h, bin_root.len());
            h.update(bin_root.as_bytes());
            Key(h.finish())
        }
        crate::engine_check::CheckScope::Test { packages, bin, bin_roots } => {
            let mut h = blaze_ids::Hasher::new(SCOPED_IDS_CERT_DOMAIN);
            h.id(&root).update(&[2]);
            put_len(&mut h, packages.len());
            for package in packages {
                put_len(&mut h, package.len());
                h.update(package.as_bytes());
            }
            match bin {
                Some(name) => {
                    h.update(&[1]);
                    put_len(&mut h, name.len());
                    h.update(name.as_bytes());
                }
                None => {
                    h.update(&[0]);
                }
            }
            put_len(&mut h, bin_roots.len());
            for bin_root in bin_roots {
                put_len(&mut h, bin_root.len());
                h.update(bin_root.as_bytes());
            }
            Key(h.finish())
        }
    }
}

/// Tag bytes under [`SCOPED_IDS_CERT_DOMAIN`] for the last-`RunIds` delta anchor and pointers
/// (disjoint from `body_ids_cert`, which hashes 32-byte `root` followed by tag `1` or `2`).
const DELTA_ANCHOR_TAG: u8 = 3;
const DELTA_PTR_TAG: u8 = 4;
const DELTA_WS_PTR_TAG: u8 = 5;
const DELTA_ANCHOR_BYTES: usize = 64;
const WORKSPACE_ANCHOR_DOMAIN: &str = "blaze-cli run ids workspace anchor 1";

fn hash_scope(h: &mut blaze_ids::Hasher, scope: crate::engine_check::CheckScope<'_>) {
    let put_len = |h: &mut blaze_ids::Hasher, n: usize| {
        h.update(&u64::try_from(n).unwrap_or(u64::MAX).to_le_bytes());
    };
    match scope {
        crate::engine_check::CheckScope::Whole => {
            h.update(&[0]);
        }
        crate::engine_check::CheckScope::Bin { package, root: bin_root } => {
            h.update(&[1]);
            put_len(h, package.len());
            h.update(package.as_bytes());
            put_len(h, bin_root.len());
            h.update(bin_root.as_bytes());
        }
        crate::engine_check::CheckScope::Test { packages, bin, bin_roots } => {
            h.update(&[2]);
            put_len(h, packages.len());
            for package in packages {
                put_len(h, package.len());
                h.update(package.as_bytes());
            }
            match bin {
                Some(name) => {
                    h.update(&[1]);
                    put_len(h, name.len());
                    h.update(name.as_bytes());
                }
                None => {
                    h.update(&[0]);
                }
            }
            put_len(h, bin_roots.len());
            for bin_root in bin_roots {
                put_len(h, bin_root.len());
                h.update(bin_root.as_bytes());
            }
        }
    }
}

/// CAS pointer name for the checkout+scope's most recent `RunIds` delta anchor, keyed by
/// `(checkout, scope, toolchain, tools)` under [`SCOPED_IDS_CERT_DOMAIN`].
fn body_ids_last_ptr(
    checkout: &str,
    scope: crate::engine_check::CheckScope<'_>,
    toolchain: blaze_toolchain::ToolchainId,
    tools: &[Tool; KINDS],
) -> blaze_cas::Key {
    let mut h = blaze_ids::Hasher::new(SCOPED_IDS_CERT_DOMAIN);
    h.update(&[DELTA_PTR_TAG]);
    h.update(&u64::try_from(checkout.len()).unwrap_or(u64::MAX).to_le_bytes());
    h.update(checkout.as_bytes());
    hash_scope(&mut h, scope);
    h.update(&toolchain.0);
    for tool in tools {
        h.id(&tool.0);
    }
    blaze_cas::Key(h.finish())
}

/// Deterministic, checkout-path-free workspace identity derived from member `(name, /0/... root)`
/// entries in `list`, shared across worktrees of the same repository (rules 25, 36).
pub(crate) fn workspace_anchor_id(list: &blaze_world::crate_list::CrateList<'_>, alloc: Alloc<'_>) -> Option<ContentId> {
    use blaze_world::crate_list::CrateGraph as _;

    let mut members: blaze_own::AVec<'_, (&str, &str)> = blaze_own::Vec::new_in(alloc);
    for at in 0..list.len() {
        if list.member(at) {
            members.push((list.name(at), list.root(at)));
        }
    }
    if members.is_empty() {
        return None;
    }
    members.sort_unstable();
    members.dedup();
    let mut h = blaze_ids::Hasher::new(WORKSPACE_ANCHOR_DOMAIN);
    h.update(&u64::try_from(members.len()).unwrap_or(u64::MAX).to_le_bytes());
    for (name, root) in members.iter() {
        h.part(name.as_bytes()).part(root.as_bytes());
    }
    Some(h.finish())
}

/// Cross-worktree CAS pointer for the workspace+scope's most recent `RunIds` delta anchor, keyed
/// by `(workspace_anchor, scope, toolchain, tools)` without any checkout path so a fresh worktree
/// hits delta `BZRI` on its first edit after adopting a shared workspace base.
fn body_ids_workspace_ptr(
    workspace: ContentId,
    scope: crate::engine_check::CheckScope<'_>,
    toolchain: blaze_toolchain::ToolchainId,
    tools: &[Tool; KINDS],
) -> blaze_cas::Key {
    let mut h = blaze_ids::Hasher::new(SCOPED_IDS_CERT_DOMAIN);
    h.update(&[DELTA_WS_PTR_TAG]);
    h.id(&workspace);
    hash_scope(&mut h, scope);
    h.update(&toolchain.0);
    for tool in tools {
        h.id(&tool.0);
    }
    blaze_cas::Key(h.finish())
}

fn body_ids_anchor_bytes(root: ContentId, table: Key) -> [u8; DELTA_ANCHOR_BYTES] {
    let mut bytes = [0u8; DELTA_ANCHOR_BYTES];
    if let Some(head) = bytes.get_mut(..32) {
        head.copy_from_slice(&root.0);
    }
    if let Some(tail) = bytes.get_mut(32..64) {
        tail.copy_from_slice(&table.0.0);
    }
    bytes
}

fn body_ids_anchor_key(anchor: &[u8; DELTA_ANCHOR_BYTES]) -> blaze_cas::Key {
    let mut h = blaze_ids::Hasher::new(SCOPED_IDS_CERT_DOMAIN);
    h.update(&[DELTA_ANCHOR_TAG]);
    h.update(anchor);
    blaze_cas::Key(h.finish())
}

fn save_last_body_ids_if_moved<D: blaze_core::Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::plain_place::StoreAlloc>,
    ptr: &blaze_cas::Key,
    root: ContentId,
    table: Key,
) {
    let tag = blaze_keyed::kind::ReservedTag::Drive.tag();
    let anchor = body_ids_anchor_bytes(root, table);
    let anchor_key = body_ids_anchor_key(&anchor);
    if matches!(lent.pointed(tag, ptr), Ok(Some(current)) if current == anchor_key) {
        return;
    }
    if lent.put(&anchor_key, blaze_cas::plain::Namespace::LOCAL, tag, &anchor).is_ok() {
        let _ = lent.point(tag, ptr, &anchor_key);
    }
}

fn load_last_body_ids<'m, D: blaze_core::Driver>(
    lent: &'m blaze_cas::plain::Lent<D, crate::store::plain_place::StoreAlloc>,
    ptr: &blaze_cas::Key,
) -> Option<(ContentId, &'m [u8])> {
    let tag = blaze_keyed::kind::ReservedTag::Drive.tag();
    let anchor_key = match lent.pointed(tag, ptr) {
        Ok(Some(key)) => key,
        Ok(None) | Err(blaze_cas::plain::LentError::TargetGone { .. }) => return None,
        Err(_) => return None,
    };
    let anchor = lent.get(&anchor_key).ok()??;
    if anchor.bytes.len() != DELTA_ANCHOR_BYTES {
        return None;
    }
    let mut prev_root = [0u8; 32];
    let mut prev_table = [0u8; 32];
    prev_root.copy_from_slice(anchor.bytes.get(..32)?);
    prev_table.copy_from_slice(anchor.bytes.get(32..64)?);
    let prev_record = lent.get(&blaze_cas::Key(ContentId(prev_table))).ok()??;
    Some((ContentId(prev_root), prev_record.bytes))
}

/// Page-level reuse oracle comparing two engine answer [`blaze_engine::pages::Root`] directories.
/// Any `Name` whose page key is identical in `prev_root` and `curr_root` (and whose `PLACE_SKIPPED`
/// flag in `Root::places` did not flip) has mathematically identical `RowFacts`, record key, and
/// immutable CAS record bytes (rules 21, 25, 26).
struct PageReuseOracle<'a> {
    bits: u8,
    changed_pages: blaze_own::AVec<'a, u64>,
    skipped_changed: blaze_own::AVec<'a, blaze_drive::Name>,
}

impl PageReuseOracle<'_> {
    fn mark_changed(bits: &mut [u64], page: usize) {
        if let Some(word) = bits.get_mut(page / 64) {
            *word |= 1u64 << (page % 64);
        }
    }

    fn is_page_changed(bits: &[u64], page: usize) -> bool {
        bits.get(page / 64).is_none_or(|word| (*word & (1u64 << (page % 64))) != 0)
    }

    fn collect_skipped_diff<'a>(
        prev: &blaze_engine::pages::Root<'_>,
        curr: &blaze_engine::pages::Root<'_>,
        alloc: Alloc<'a>,
    ) -> blaze_own::AVec<'a, blaze_drive::Name> {
        let skipped = |place: Option<&blaze_engine::pages::FilePlace>| {
            place.is_some_and(|p| (p.flags & crate::engine_check::PLACE_SKIPPED) != 0)
        };
        let mut diff = blaze_own::Vec::new_in(alloc);
        for (&file, place) in curr.places.iter() {
            if skipped(Some(place)) != skipped(prev.places.get(&file)) {
                diff.push(file);
            }
        }
        for (&file, place) in prev.places.iter() {
            if skipped(Some(place)) && !curr.places.contains_key(&file) {
                diff.push(file);
            }
        }
        diff.sort_unstable();
        diff.dedup();
        diff
    }

    /// Sans-IO builder over flat page key slices (`!is_radix`), directly testable without a store.
    /// When `ug_row_unchanged` is `true`, a changed `ug_page` is treated as a hash-bucket
    /// collision (keeping `ug_page` marked changed so colliding tasks re-read fresh while allowing
    /// delta `BZRI` reuse across all untouched pages).
    fn from_flat_pages_with_ug<'a>(
        bits: u8,
        prev_pages: &[Key],
        curr_pages: &[Key],
        ug_row_unchanged: bool,
        skipped_changed: blaze_own::AVec<'a, blaze_drive::Name>,
        alloc: Alloc<'a>,
    ) -> Option<PageReuseOracle<'a>> {
        if prev_pages.len() != curr_pages.len() {
            return None;
        }
        let page_count = 1usize.checked_shl(u32::from(bits))?;
        if prev_pages.len() != page_count {
            return None;
        }
        let words = page_count.div_ceil(64).max(1);
        let mut changed_pages = blaze_own::Vec::with_capacity_in(words, alloc);
        changed_pages.resize(words, 0u64);
        for (idx, (pk, ck)) in prev_pages.iter().zip(curr_pages.iter()).enumerate() {
            if pk != ck {
                Self::mark_changed(changed_pages.as_mut_slice(), idx);
            }
        }
        let ug_page = usize::try_from(blaze_engine::pages::content_page_of(blaze_drive::Name::unit_graph(), bits)).unwrap_or(usize::MAX);
        if Self::is_page_changed(changed_pages.as_slice(), ug_page) && !ug_row_unchanged {
            return None;
        }
        Some(PageReuseOracle { bits, changed_pages, skipped_changed })
    }

    #[cfg(test)]
    fn from_flat_pages<'a>(
        bits: u8,
        prev_pages: &[Key],
        curr_pages: &[Key],
        skipped_changed: blaze_own::AVec<'a, blaze_drive::Name>,
        alloc: Alloc<'a>,
    ) -> Option<PageReuseOracle<'a>> {
        Self::from_flat_pages_with_ug(bits, prev_pages, curr_pages, false, skipped_changed, alloc)
    }

    /// Sans-IO builder for flat roots (`!curr.is_radix()`).
    fn from_flat_roots<'a>(
        prev: &blaze_engine::pages::Root<'_>,
        curr: &blaze_engine::pages::Root<'_>,
        ug_row_unchanged: bool,
        alloc: Alloc<'a>,
    ) -> Option<PageReuseOracle<'a>> {
        if prev.bits != curr.bits || prev.is_radix() || curr.is_radix() {
            return None;
        }
        let skipped_changed = Self::collect_skipped_diff(prev, curr, alloc);
        Self::from_flat_pages_with_ug(curr.bits, prev.pages.as_slice(), curr.pages.as_slice(), ug_row_unchanged, skipped_changed, alloc)
    }
}

impl crate::run_ids::ReuseOracle for PageReuseOracle<'_> {
    fn unchanged(&self, name: blaze_drive::Name) -> bool {
        let page = usize::try_from(blaze_engine::pages::content_page_of(name, self.bits)).unwrap_or(usize::MAX);
        if Self::is_page_changed(self.changed_pages.as_slice(), page) {
            return false;
        }
        if name.kind == Kind::FileItems && self.skipped_changed.binary_search(&name).is_ok() {
            return false;
        }
        true
    }
}

fn decode_root_from_store<'a, 'm>(
    cx: &Cx,
    store: &mut dyn crate::store::Store<'m>,
    key: Key,
    alloc: blaze_own::SharedAlloc<'a>,
) -> Result<Option<blaze_engine::pages::Root<'a>>, CliError> {
    let Some(held) = store.get(cx, key)? else { return Ok(None) };
    let mut store_err: Option<CliError> = None;
    let decoded = blaze_engine::pages::decode_root_with(
        blaze_engine::pages::Sealed::by_store(key, held.bytes, held.proof),
        |sub_key| match store.get(cx, sub_key) {
            Ok(found) => found.map(|h| Ok(blaze_engine::pages::Sealed::by_store(sub_key, h.bytes, h.proof))),
            Err(err) => {
                store_err.get_or_insert(err);
                None
            }
        },
        alloc,
    );
    if let Some(err) = store_err {
        return Err(err);
    }
    Ok(decoded.ok())
}

fn decode_shard_from_store<'a, 'm>(
    cx: &Cx,
    store: &mut dyn crate::store::Store<'m>,
    key: Key,
    shard: u32,
    bits: u8,
    alloc: blaze_own::SharedAlloc<'a>,
) -> Result<Option<blaze_engine::pages::RadixShard<'a>>, CliError> {
    let Some(held) = store.get(cx, key)? else { return Ok(None) };
    Ok(blaze_engine::pages::decode_radix_shard(
        blaze_engine::pages::Sealed::by_store(key, held.bytes, held.proof),
        shard,
        bits,
        alloc,
    )
    .ok())
}

/// Inspects `Name::unit_graph()` directly on `prev_page_key` and `curr_page_key` when `ug_page`'s
/// page key moved: returns `Ok(true)` iff the `UnitGraph` row's `out`, `key`, and stored record key
/// are bit-for-bit identical (meaning `ug_page` moved solely due to a hash-bucket collision with an
/// unrelated task, so delta `BZRI` remains 100% sound under rule 21).
fn unit_graph_row_unchanged<'m>(
    cx: &Cx,
    store: &mut dyn crate::store::Store<'m>,
    prev_page_key: Key,
    curr_page_key: Key,
    ug_page: u32,
    bits: u8,
) -> Result<bool, CliError> {
    if prev_page_key == Key::default() || curr_page_key == Key::default() {
        return Ok(false);
    }
    let Some(prev_held) = store.get(cx, prev_page_key)? else { return Ok(false) };
    let Some(curr_held) = store.get(cx, curr_page_key)? else { return Ok(false) };
    let Ok(prev_page) = blaze_engine::pages::VerifiedPage::open(
        blaze_engine::pages::Sealed::by_store(prev_page_key, prev_held.bytes, prev_held.proof),
        ug_page,
        bits,
    ) else {
        return Ok(false);
    };
    let Ok(curr_page) = blaze_engine::pages::VerifiedPage::open(
        blaze_engine::pages::Sealed::by_store(curr_page_key, curr_held.bytes, curr_held.proof),
        ug_page,
        bits,
    ) else {
        return Ok(false);
    };
    let ug = blaze_drive::Name::unit_graph();
    let Some(prev_row) = prev_page.row(ug) else { return Ok(false) };
    let Some(curr_row) = curr_page.row(ug) else { return Ok(false) };
    Ok(prev_row.facts.out == curr_row.facts.out
        && prev_row.facts.key == curr_row.facts.key
        && prev_page.record(ug) == curr_page.record(ug))
}

fn build_page_reuse_oracle<'a, 'm>(
    cx: &Cx,
    store: &mut dyn crate::store::Store<'m>,
    prev_root_id: ContentId,
    curr_root_id: ContentId,
    scratch: blaze_own::SharedAlloc<'static>,
    alloc: Alloc<'a>,
) -> Result<(Option<PageReuseOracle<'a>>, Option<blaze_engine::pages::Root<'static>>), CliError> {
    if prev_root_id == curr_root_id {
        return Ok((None, None));
    }
    let Some(prev) = decode_root_from_store(cx, store, Key(prev_root_id), scratch)? else { return Ok((None, None)) };
    let Some(curr) = decode_root_from_store(cx, store, Key(curr_root_id), scratch)? else { return Ok((None, None)) };
    if prev.bits != curr.bits {
        return Ok((None, Some(curr)));
    }
    let ug_page_u32 = blaze_engine::pages::content_page_of(blaze_drive::Name::unit_graph(), curr.bits);
    let ug_page = usize::try_from(ug_page_u32).unwrap_or(usize::MAX);
    if !curr.is_radix() {
        let prev_ug = prev.pages.get(ug_page).copied().unwrap_or_default();
        let curr_ug = curr.pages.get(ug_page).copied().unwrap_or_default();
        let ug_unchanged = if prev_ug != curr_ug {
            unit_graph_row_unchanged(cx, store, prev_ug, curr_ug, ug_page_u32, curr.bits)?
        } else {
            false
        };
        let oracle = PageReuseOracle::from_flat_roots(&prev, &curr, ug_unchanged, alloc);
        return Ok((oracle, Some(curr)));
    }
    if prev.page_shards.len() != curr.page_shards.len() {
        return Ok((None, Some(curr)));
    }
    let words = curr.page_count().div_ceil(64).max(1);
    let mut changed_pages = blaze_own::Vec::with_capacity_in(words, alloc);
    changed_pages.resize(words, 0u64);
    let ug_shard = ug_page / blaze_engine::pages::RADIX_LEAF_PAGES;
    let ug_slot = ug_page % blaze_engine::pages::RADIX_LEAF_PAGES;
    let mut prev_ug_key = Key::default();
    let mut curr_ug_key = Key::default();
    // In `BER6`, content entries live exclusively in the lower half of radix shards.
    let content_shards = prev.page_shards.len() / 2;
    for (shard_idx, (&prev_sk, &curr_sk)) in prev.page_shards.iter().zip(curr.page_shards.iter()).take(content_shards).enumerate() {
        if prev_sk == curr_sk {
            continue;
        }
        let Ok(shard_u32) = u32::try_from(shard_idx) else { return Ok((None, Some(curr))) };
        let prev_shard = if prev_sk == Key::default() {
            None
        } else {
            let Some(decoded) = decode_shard_from_store(cx, store, prev_sk, shard_u32, curr.bits, scratch)? else {
                return Ok((None, Some(curr)));
            };
            Some(decoded)
        };
        let curr_shard = if curr_sk == Key::default() {
            None
        } else {
            let Some(decoded) = decode_shard_from_store(cx, store, curr_sk, shard_u32, curr.bits, scratch)? else {
                return Ok((None, Some(curr)));
            };
            Some(decoded)
        };
        if shard_idx == ug_shard {
            prev_ug_key = prev_shard.as_ref().and_then(|s| s.keys.get(ug_slot).copied()).unwrap_or_default();
            curr_ug_key = curr_shard.as_ref().and_then(|s| s.keys.get(ug_slot).copied()).unwrap_or_default();
        }
        let base = shard_idx * blaze_engine::pages::RADIX_LEAF_PAGES;
        for slot in 0..blaze_engine::pages::RADIX_LEAF_PAGES {
            let pk = prev_shard.as_ref().and_then(|s| s.keys.get(slot).copied()).unwrap_or_default();
            let ck = curr_shard.as_ref().and_then(|s| s.keys.get(slot).copied()).unwrap_or_default();
            if pk != ck {
                PageReuseOracle::mark_changed(changed_pages.as_mut_slice(), base + slot);
            }
        }
    }
    if PageReuseOracle::is_page_changed(changed_pages.as_slice(), ug_page)
        && !unit_graph_row_unchanged(cx, store, prev_ug_key, curr_ug_key, ug_page_u32, curr.bits)?
    {
        return Ok((None, Some(curr)));
    }
    let skipped_changed = PageReuseOracle::collect_skipped_diff(&prev, &curr, alloc);
    Ok((Some(PageReuseOracle { bits: curr.bits, changed_pages, skipped_changed }), Some(curr)))
}

/// The body ids of the answer at its root (run_ids.rs): one get when a start under this root
/// wrote them, else incrementally updated from the prior `RunIds` record via page diff when
/// unchanged crates/files/checks stand (producing 100% byte-identical bytes), or walked fresh
/// over E2's answer and written at the root.
/// Either way they read each workspace body's reads BY NEED from the stored rows in place
/// (`Standing::rows`, run_ids.rs `BodyReads`), counted in `asked` for the profile's line.
/// COST: a hit, one get; an incremental miss, O(changed pages + changed checks) store reads and
/// one put; a cold miss, one store read per body (zero duplicate reads) and one put.
pub(crate) fn body_ids<'a, 'r: 'a, 'm: 'a>(
    cx: &Cx, standing: &mut Standing<'r, 'm>, list: &blaze_world::crate_list::CrateList<'_>, asked: &'a crate::run_ids::Asked, bump: blaze_own::Bump<'a>,
) -> Result<RunIds<'a>, CliError> {
    let (ids, _) = body_ids_with_root(cx, standing, list, asked, bump)?;
    Ok(ids)
}

/// Like [`body_ids`], also returning the decoded current engine [`blaze_engine::pages::Root`]
/// when the delta fast path decoded it, so `run_tests_status` reuses it without decoding
/// `standing.root` a second time (rule 77).
fn body_ids_with_root<'a, 'r: 'a, 'm: 'a>(
    cx: &Cx,
    standing: &mut Standing<'r, 'm>,
    list: &blaze_world::crate_list::CrateList<'_>,
    asked: &'a crate::run_ids::Asked,
    bump: blaze_own::Bump<'a>,
) -> Result<(RunIds<'a>, Option<blaze_engine::pages::Root<'static>>), CliError> {
    let alloc = bump.alloc();
    let frame = standing.frame;
    let refused = |error: RunIdsError<'_>| CliError::Start(StartError::BodyIds { shown: bstd::mem::format_in!(frame, "{error}").as_str().into() });
    let cert = body_ids_cert(standing.root, standing.scope);
    let (toolchain, tools): (_, [Tool; KINDS]) = (standing.toolchain, standing.tools);
    let table_cas_key = crate::run_ids::table_key(cert, toolchain, &tools);
    let lent = standing.lent;
    let checkout_id = standing.place.served.root_text().unwrap_or(standing.place.checkout);
    let last_ptr = body_ids_last_ptr(checkout_id, standing.scope, toolchain, &tools);
    let ws_ptr = workspace_anchor_id(list, alloc).map(|ws| body_ids_workspace_ptr(ws, standing.scope, toolchain, &tools));
    let save_anchors = |root: ContentId, table: Key| {
        save_last_body_ids_if_moved(lent, &last_ptr, root, table);
        if let Some(ref ptr) = ws_ptr {
            save_last_body_ids_if_moved(lent, ptr, root, table);
        }
    };
    let most = crate::run_ids::Most::USUAL;
    let from = crate::run_ids::ReadsFrom::new(standing.rows(), asked, most);
    let reading = |ids: RunIds<'a>| ids.reading::<crate::run_ids::Production>(from).map_err(|why| refused(RunIdsError::Unread(why)));
    if let Some(ids) = RunIds::from_record(cx, &mut *standing.store, cert, toolchain, &tools, bump).map_err(refused)? {
        save_anchors(standing.root, table_cas_key);
        return Ok((reading(ids)?, None));
    }
    // Each active member unit's root and view: the walk asks each unit's row of a body at its view (D191).
    let scoped = crate::engine_check::ScopedUnits::of(standing.scope, list, standing.heap.alloc());
    let units = crate::run_ids::unit_roots_scoped(list, scoped.as_ref(), alloc).map_err(refused)?;
    let walk_from = if scoped.is_some() { crate::run_ids::WalkFrom::UnitRoots } else { crate::run_ids::WalkFrom::Workspace };
    let body_tool = blaze_engine::Tools(tools).of(blaze_drive::Name { kind: Kind::BodyCheck, subject: blaze_ids::ContentId([0; 32]) });
    // Incremental delta fast path: check this checkout's own anchor first, then the cross-worktree
    // workspace anchor (so a fresh worktree adopting a shared base hits delta `BZRI` on its first
    // edit). Diff `prev_root` vs `curr_root` pages and copy all standing crates/files/checks
    // directly from `prev_ids` without opening their records.
    let mut cached_curr_root: Option<blaze_engine::pages::Root<'static>> = None;
    let prior_anchor = load_last_body_ids(lent, &last_ptr)
        .or_else(|| ws_ptr.as_ref().and_then(|ptr| load_last_body_ids(lent, ptr)));
    if let Some((prev_root_id, prev_bytes)) = prior_anchor {
        let prev_erased = blaze_own::Erased::new(frame);
        let prev_bump = prev_erased.bump();
        if let Ok(prev_ids) = RunIds::read(prev_bytes, prev_bump)
            && prev_ids.matches_build(toolchain, &tools)
        {
            let (oracle, decoded_curr) = build_page_reuse_oracle(cx, &mut *standing.store, prev_root_id, standing.root, standing.heap.shared(), alloc)?;
            cached_curr_root = decoded_curr;
            if let Some(oracle) = oracle {
                let mut answer = standing.answer();
                let walk = crate::run_ids::workspace_of_delta(cx, &mut answer, body_tool, &units, walk_from, &prev_ids, &oracle, alloc).map_err(refused)?;
                let ids = RunIds::from_walked(cx, toolchain, list, scoped.as_ref(), &tools, &walk, &mut answer, Some((&prev_ids, &oracle)), most, bump).map_err(refused)?;
                drop(answer);
                ids.put(cx, &mut *standing.store, cert, &tools, alloc).map_err(refused)?;
                save_anchors(standing.root, table_cas_key);
                return Ok((reading(ids)?, cached_curr_root));
            }
        }
    }
    let mut answer = standing.answer();
    let walk = crate::run_ids::workspace_of_scoped(cx, &mut answer, body_tool, &units, walk_from, None, alloc).map_err(refused)?;
    let ids = RunIds::from_walked(cx, toolchain, list, scoped.as_ref(), &tools, &walk, &mut answer, None, most, bump).map_err(refused)?;
    drop(answer);
    ids.put(cx, &mut *standing.store, cert, &tools, alloc).map_err(refused)?;
    save_anchors(standing.root, table_cas_key);
    Ok((reading(ids)?, cached_curr_root))
}

/// CAS memo domain for pure-Rust (`send: None`) native image results keyed by exact closure
/// `CrateScope` and `Script` record keys (rules 21, 77). Eliminates reading ~24 `CrateScope`
/// records (~29 MB) and mapping toolchain segments (~137 MB) on every incremental `blaze test`
/// / `blaze run` body edit when no crate's link table or build script changed.
const NATIVE_EMPTY_MEMO_DOMAIN: &str = "blaze-cli engine native empty memo 1";

/// Deterministic memo key covering every input to [`crate::engine_native::image_for`] for a
/// program or test unit without reading any `CrateScope` or `Script` record payload (rule 21).
fn native_empty_memo_key(
    platform: blaze_ladder::native_links::Platform,
    producer: &blaze_ids::producer::ProducerId<blaze_ids::producer::Whole>,
    sdk: Option<blaze_apple_sdk::SdkId>,
    program_root: &str,
    crates: &[(blaze_id::CrateItemKey, &str, Key, Option<(blaze_drive::Name, Key, ContentId)>)],
) -> blaze_cas::Key {
    let mut h = blaze_ids::Hasher::new(NATIVE_EMPTY_MEMO_DOMAIN);
    let platform_byte: u8 = match platform {
        blaze_ladder::native_links::Platform::MacosArm64 => 0,
        blaze_ladder::native_links::Platform::LinuxArm64 => 1,
    };
    h.update(&[platform_byte]).id(producer.id());
    match sdk {
        None => {
            h.update(&[0]);
        }
        Some(id) => {
            h.update(&[1]).update(&id.key());
        }
    }
    h.part(program_root.as_bytes());
    h.update(&u64::try_from(crates.len()).unwrap_or(u64::MAX).to_le_bytes());
    for &(key, name, scope_at, script) in crates {
        h.id(&key.0).part(name.as_bytes()).id(&scope_at.0);
        match script {
            None => {
                h.update(&[0]);
            }
            Some((script_name, run_at, run_out)) => {
                h.update(&[1])
                    .part(script_name.kind.name().as_bytes())
                    .id(&script_name.subject)
                    .id(&run_at.0)
                    .id(&run_out);
            }
        }
    }
    blaze_cas::Key(h.finish())
}

fn load_native_empty_memo<D: blaze_core::Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::plain_place::StoreAlloc>,
    memo_key: &blaze_cas::Key,
    require_record: bool,
) -> Option<Key> {
    let lend = lent.get(memo_key).ok()??;
    if lend.tag != blaze_keyed::kind::ReservedTag::Drive.tag() {
        return None;
    }
    let arr: [u8; 32] = lend.bytes.try_into().ok()?;
    let image_key = Key(ContentId(arr));
    if require_record && lent.get(&blaze_cas::Key(image_key.0)).ok().flatten().is_none() {
        return None;
    }
    Some(image_key)
}

fn save_native_empty_memo<D: blaze_core::Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::plain_place::StoreAlloc>,
    memo_key: &blaze_cas::Key,
    image_key: Key,
) {
    let _ = lent.put(
        memo_key,
        blaze_cas::plain::Namespace::LOCAL,
        blaze_keyed::kind::ReservedTag::Drive.tag(),
        &image_key.0.0,
    );
}

fn script_has_deferred_c<D: blaze_core::Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::plain_place::StoreAlloc>,
    script_at: Key,
) -> bool {
    let Ok(Some(lend)) = lent.get(&blaze_cas::Key(script_at.0)) else {
        return true;
    };
    let Ok(blaze_drive::record::Stored::Whole(view)) = blaze_drive::record::open(lend.bytes) else {
        return true;
    };
    let Some((_, record)) = crate::scripts::payload_parts(view.payload) else {
        return true;
    };
    let Ok(outputs) = blaze_build_script::outputs::OutputsRef::open(record) else {
        return true;
    };
    outputs.c_requests().next().is_some()
}

/// Memoized wrapper around [`crate::engine_native::image_for`] for pure-Rust programs and test
/// suites (`send: None`): hits in O(closure) using only `list` and `standing.names` (zero
/// `CrateScope` or `Script` store reads, zero segment mmaps). Any native-code suite (`send: Some`)
/// or missing input falls through directly to `engine_native::image_for`.
fn native_image_for_cached<'r>(
    cx: &Cx,
    standing: &mut Standing<'r, '_>,
    list: &blaze_world::crate_list::CrateList<'_>,
    closure: &[crate::engine_seeds::CrateAt],
    require_record: bool,
    watch: &dyn blaze_screen::Watch,
    frame: &bstd::mem::Frame,
) -> Result<crate::engine_native::ProgramImage<'r>, crate::engine_native::NativeError<'r>> {
    use blaze_world::crate_list::CrateGraph as _;

    let memo_info = (|| -> Option<(blaze_cas::Key, blaze_own::Vec<Key, &bstd::mem::Frame>)> {
        let platform = crate::native_image::platform()?;
        let sdk = match platform {
            blaze_ladder::native_links::Platform::MacosArm64 => Some(standing.resolve_sdk(cx).ok()?.identity()),
            blaze_ladder::native_links::Platform::LinuxArm64 => None,
        };
        let first = closure.first()?;
        let root = (0..list.len()).find(|&r| list.key(r) == first.key)?;
        let program_root = list.root(root).strip_prefix("/0/")?;
        if !crate::native_image::linked(list, root) {
            return None;
        }
        let mut handed: blaze_own::Vec<[u8; 32], &bstd::mem::Frame> = blaze_own::Vec::with_capacity_in(closure.len(), frame);
        handed.extend(closure.iter().map(|c| c.key.0.0));
        handed.sort_unstable();
        let order = crate::native_image::closure(list, root, handed.as_slice(), frame);
        let mut inputs = blaze_own::Vec::with_capacity_in(order.len(), frame);
        let mut script_ats: blaze_own::Vec<Key, &bstd::mem::Frame> = blaze_own::Vec::new_in(frame);
        for &r in order.iter() {
            let key = list.key(r);
            if handed.binary_search(&key.0.0).is_err() {
                return None;
            }
            let scope = blaze_drive::key::crate_scope_name(&key);
            let scope_in = (standing.names)(scope)?;
            let script = match list.script(r) {
                None => None,
                Some(bytes) => {
                    let name = crate::native_image::script_name(bytes)?;
                    let run = (standing.names)(name)?;
                    script_ats.push(run.at);
                    Some((name, run.at, run.out))
                }
            };
            inputs.push((key, list.name(r), scope_in.at, script));
        }
        let memo_key = native_empty_memo_key(platform, &standing.native.producer, sdk, program_root, inputs.as_slice());
        Some((memo_key, script_ats))
    })();

    if let Some((ref memo_key, _)) = memo_info
        && let Some(image_key) = load_native_empty_memo(standing.lent, memo_key, require_record)
    {
        return Ok(crate::engine_native::ProgramImage { key: image_key, send: None });
    }

    let image = crate::engine_native::image_for(cx, standing, closure, watch)?;
    if image.send.is_none()
        && let Some((memo_key, script_ats)) = memo_info
        && !script_ats.iter().any(|&at| script_has_deferred_c(standing.lent, at))
    {
        save_native_empty_memo(standing.lent, &memo_key, image.key);
    }
    Ok(image)
}

/// Builds `work::Facts` for `run`, `test`, and `build` from `standing` (the one home for
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
    cx: &Cx,
    mut standing: Standing<'_, '_>,
    picked: &run_package::Program<'_>,
    launch: &program::Launch<'_>,
    phases: &mut Phases,
    watch: &dyn blaze_screen::Watch,
    status_out: &crate::run_line::RunStatusOut<'_, '_>,
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
    // The start path does not run lints (`Standing::tools` already uses `Lints::NONE`);
    // skip reading and parsing the root `Cargo.toml` just to build unused lint rules.
    let mut facts = start_facts(&standing, launch.fs, phases, crate::lints::Lints::NONE);
    let answered = run_names::FromEngine { graph: standing.graph, names: standing.names };
    let at = run_names::program_row(cx, root, &answered, &mut *standing.store, &mut facts, picked.root, heap, frame).map_err(CliError::RunNames)?;
    let owner = run_names::Owner::At(at);
    let found = run_names::found_answered(cx, &answered, owner, &mut *standing.store, &mut facts.bodies, &mut read, heap, frame).map_err(CliError::RunNames)?;
    let program_found = found.program().ok_or(CliError::Start(StartError::NoProgram))?;
    let list = found.list().ok_or(CliError::Start(StartError::NoProgram))?;
    // The program's crate first, then its scope's: from the crate list the walk read, so the
    // unit graph is not read again here (pure-Rust programs hit the CAS memo with zero store reads).
    let closure = program_found.closure_in(alloc);
    let native = native_image_for_cached(cx, &mut standing, list, &closure, true, watch, frame)
        .map_err(|error| CliError::Start(StartError::Native { shown: bstd::mem::format_in!(frame, "{error}").as_str().into() }))?;
    phases.mark_cx(cx, "run_inventory", frame);
    watch.see(blaze_screen::Step::Began(blaze_screen::Stage::BodyIds));
    let asked = crate::run_ids::Asked::default();
    let began = phases.now();
    let ids = body_ids(cx, &mut standing, list, &asked, bump)?;
    let ids_ns = phases.since(began);
    phases.mark_cx(cx, "body_ids", frame);
    // The line's table half now: the table itself is handed to the parent below.
    let ids_line = ids.line(&asked, ids_ns);
    let mut made = Some(ids);
    // The start list (start_list.rs): under the answer's root, named by the program; its
    // tables are the root's body ids and the program's image, so both stay rooted beside it.
    let cert = body_ids_cert(standing.root, standing.scope);
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
    let scope_records = crate::front_load::ScopeRecords::new(found.snapshot(), blaze_world::crate_list::CrateGraph::len(list), heap);
    let store = &mut *standing.store;
    let (bodies, reads, seen_ref, dirs_ref) = (&mut facts.bodies, &mut read, &seen, &pack_dirs);
    let mut names = || {
        run_names::names_of_by_need(cx, root, &found, owner, &mut *store, &mut *bodies, &mut *reads, &scope_records, env, dirs_ref, procs, seen_ref, frame, heap)
            .map_err(|error| crate::run_parent::ParentError::Names { shown: std::boxed::Box::from(bstd::mem::format_in!(frame, "{error}").as_str()) })
    };
    let mut ids = || made.take().ok_or_else(|| crate::run_parent::ParentError::BodyIds { shown: std::boxed::Box::from("the body ids were asked a second time (run_parent.rs `CommandFront` asks once)") });
    status_out.set_launch(picked.package, picked.bin, launch.args);
    let ran = program::run(cx, root, launch, picked, units, native.send, producer, lent, today, &mut names, &mut ids, typed, &mut phases.code_records, watch, heap, frame);
    drop(names);
    read.add_by_need(scope_records.counts());
    end_procs(cx, &maker, standing.err, "run")?;
    phases.mark_cx(cx, "program_run", frame);
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

/// Parses both `[profile.dev]` `panic` and `overflow-checks` from root `Cargo.toml` text in a
/// single `blaze_toml::Document::parse_in` pass (matching `blaze_manifest::layout::panic::panic_strategy`
/// and `blaze_manifest::layout::profile::overflow_checks` identically).
fn parse_dev_profile_settings(
    text: &str,
    frame: &bstd::mem::Frame,
) -> Result<(blaze_manifest::layout::panic::ProfilePanic, blaze_ladder::run::OverflowChecks), CliError> {
    use blaze_manifest::layout::panic::{PanicError, ProfilePanic};
    use blaze_manifest::layout::profile::BuiltinProfile;
    use blaze_manifest::layout::{LayoutError, ManifestId, Span};
    use blaze_toml::{Document, Kind, Limits};

    const ROOT_MANIFEST: &str = "Cargo.toml";
    let refused = |why: &dyn core::fmt::Display| CliError::Program(crate::ManifestRefused::of(why, frame));
    let erased = blaze_own::Erased::new(frame);
    let alloc = erased.handle();
    let id = ManifestId(0);
    let profile = BuiltinProfile::Dev;
    let name = profile.name();
    let span_of = |r: core::ops::Range<usize>| Span {
        manifest: id,
        start: u32::try_from(r.start).unwrap_or(u32::MAX),
        end: u32::try_from(r.end).unwrap_or(u32::MAX),
    };
    let manifest = || blaze_own::String::from_str_in(ROOT_MANIFEST, alloc);
    let doc = match Document::parse_in(text, Limits::CARGO, alloc) {
        Ok(doc) => doc,
        Err(error) => return Err(refused(&LayoutError::Toml { manifest: manifest(), error })),
    };
    let default_overflow = if profile.default_overflow_checks() {
        blaze_ladder::run::OverflowChecks::On
    } else {
        blaze_ladder::run::OverflowChecks::Off
    };
    let Some(profiles) = doc.root().get("profile") else {
        return Ok((ProfilePanic::Unwind, default_overflow));
    };
    if profiles.kind() != Kind::Table {
        return Err(refused(&LayoutError::Expected { at: span_of(profiles.span()), key: "profile", kind: "a table" }));
    }
    let Some(table) = profiles.get(name) else {
        return Ok((ProfilePanic::Unwind, default_overflow));
    };
    if table.kind() != Kind::Table {
        return Err(refused(&LayoutError::Expected { at: span_of(table.span()), key: "profile.<name>", kind: "a table" }));
    }
    if let Some(inherits) = table.get("inherits") {
        return Err(refused(&PanicError::InheritsInRoot { manifest: manifest(), profile: name, at: span_of(inherits.span()) }));
    }
    if let Some(packages) = table.get("package") {
        if packages.kind() != Kind::Table {
            return Err(refused(&LayoutError::Expected { at: span_of(packages.span()), key: "profile.<name>.package", kind: "a table" }));
        }
        for package in packages.children() {
            if let Some(panic) = package.get("panic") {
                return Err(refused(&PanicError::InPackageOverride {
                    manifest: manifest(),
                    profile: name,
                    package: blaze_own::String::from_str_in(package.key(), alloc),
                    at: span_of(panic.span()),
                }));
            }
        }
    }
    let panic = match table.get("panic") {
        None => ProfilePanic::Unwind,
        Some(item) => match item.as_str() {
            Some("unwind") => ProfilePanic::Unwind,
            Some("abort") => ProfilePanic::Abort,
            Some("immediate-abort") => {
                return Err(refused(&PanicError::ImmediateAbort { manifest: manifest(), profile: name, at: span_of(item.span()) }));
            }
            Some(written) => {
                return Err(refused(&PanicError::Unknown {
                    manifest: manifest(),
                    profile: name,
                    written: blaze_own::String::from_str_in(written, alloc),
                    at: span_of(item.span()),
                }));
            }
            None => return Err(refused(&LayoutError::Expected { at: span_of(item.span()), key: "profile.<name>.panic", kind: "a string" })),
        },
    };
    let overflow_on = match table.get("overflow-checks") {
        None => profile.default_overflow_checks(),
        Some(item) => match item.as_bool() {
            Some(on) => on,
            None => {
                return Err(refused(&LayoutError::Expected {
                    at: span_of(item.span()),
                    key: "profile.<name>.overflow-checks",
                    kind: "a bool",
                }));
            }
        },
    };
    let overflow = if overflow_on { blaze_ladder::run::OverflowChecks::On } else { blaze_ladder::run::OverflowChecks::Off };
    Ok((panic, overflow))
}

/// Reads both `[profile.dev]` `panic` and `overflow-checks` from the root `Cargo.toml` in a
/// single read and single TOML parse on `blaze test`.
fn dev_profile_settings(
    cx: &Cx,
    root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
    frame: &bstd::mem::Frame,
) -> Result<(blaze_manifest::layout::panic::ProfilePanic, blaze_ladder::run::OverflowChecks), CliError> {
    const ROOT_MANIFEST: &str = "Cargo.toml";
    let mut bytes = bstd::mem::Bytes::new_in(frame, "the root manifest");
    root.read_all(cx, &bstd::path::Path::new(ROOT_MANIFEST)?, &mut bytes)?;
    let text = core::str::from_utf8(bytes.as_slice()).map_err(|_| CliError::Text { what: "the root manifest is not UTF-8" })?;
    parse_dev_profile_settings(text, frame)
}

const TEST_INVENTORY_PTR_DOMAIN: &str = "blaze-cli test inventory delta ptr 1";
const TEST_INVENTORY_REC_DOMAIN: &str = "blaze-cli test inventory delta rec 1";

fn test_inventory_ptr(graph_out: ContentId) -> blaze_cas::Key {
    let mut h = blaze_ids::Hasher::new(TEST_INVENTORY_PTR_DOMAIN);
    h.id(&graph_out);
    blaze_cas::Key(h.finish())
}

fn load_test_inventory_cache<'a, D: blaze_core::Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::plain_place::StoreAlloc>,
    graph_out: ContentId,
    bump: blaze_own::Bump<'a>,
) -> Option<&'a [u8]> {
    let tag = blaze_keyed::kind::ReservedTag::Drive.tag();
    let ptr = test_inventory_ptr(graph_out);
    let target = match lent.pointed(tag, &ptr) {
        Ok(Some(key)) => key,
        Ok(None) | Err(_) => return None,
    };
    let lend = lent.get(&target).ok()??;
    Some(blaze_scope::resolve::aligned(lend.bytes, bump))
}

fn save_test_inventory_cache<D: blaze_core::Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::plain_place::StoreAlloc>,
    graph_out: ContentId,
    bytes: &[u8],
) {
    let tag = blaze_keyed::kind::ReservedTag::Drive.tag();
    let ptr = test_inventory_ptr(graph_out);
    let rec_key = blaze_cas::Key(ContentId::of(TEST_INVENTORY_REC_DOMAIN, bytes));
    if matches!(lent.pointed(tag, &ptr), Ok(Some(current)) if current == rec_key) {
        return;
    }
    if lent.put(&rec_key, blaze_cas::plain::Namespace::LOCAL, tag, bytes).is_ok() {
        let _ = lent.point(tag, &ptr, &rec_key);
    }
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
    let workers = standing.workers.get();
    // The start path does not run lints (`Standing::tools` already uses `Lints::NONE`);
    // skip reading and parsing the root `Cargo.toml` just to build unused lint rules.
    let mut facts = start_facts(&standing, launch.fs, phases, crate::lints::Lints::NONE);
    let answered = run_names::FromEngine { graph: standing.graph, names: standing.names };
    let prev_inventory = load_test_inventory_cache(standing.lent, standing.graph.out, bump);
    let (inventory, updated_inventory) = run_names::found_tests_answered_cached(
        cx,
        root,
        &answered,
        &mut *standing.store,
        &mut facts,
        &mut read,
        packages,
        bin,
        prev_inventory,
        heap,
        frame,
    )
    .map_err(CliError::RunNames)?;
    if let Some(updated) = updated_inventory {
        save_test_inventory_cache(standing.lent, standing.graph.out, updated.as_slice());
    }
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
        let (profile, overflow) = dev_profile_settings(cx, root, frame)?;
        if let Some((test, dependency)) = abort_dependency(profile, list, inventory.suites.iter().map(|(_, unit)| unit.at)) {
            use blaze_world::crate_list::CrateGraph as _;
            let shown = |at: usize| -> std::boxed::Box<str> { std::boxed::Box::from(bstd::mem::format_in!(frame, "{} {}", list.name(at), list.version(at)).as_str()) };
            return Err(CliError::Start(StartError::AbortTests { test: shown(test), dependency: shown(dependency) }));
        }
        phases.mark_cx(cx, "test_inventory", frame);
        let began = phases.now();
        let (ids, decoded_curr_root) = body_ids_with_root(cx, &mut standing, list, &asked, bump)?;
        let ids_ns = phases.since(began);
        phases.mark_cx(cx, "body_ids", frame);
        let panic = blaze_ladder::panic_strategy::of_profile(blaze_world::panic::TEST_PROFILE);
        // Boxed in the process heap: the error outlives this frame to `main`'s print.
        let parent = move |why| CliError::OwnProcess(blaze_own::Box::new_in(crate::own_process::OwnError::Parent(why), heap.alloc()));
        let isa = crate::run_parent::isa().map_err(parent)?;
        let producer = standing.producers.get::<blaze_ids::producer::CodeBuilder>();
        let certificate = crate::start_list::Certificate::of_answer(&standing);
        let tools_digest = crate::test_cache::tools_digest(&standing.tools);
        let curr_root = match decoded_curr_root {
            Some(root) => Some(root),
            None => decode_root_from_store(cx, &mut *standing.store, Key(standing.root), standing.heap.shared())?,
        };
        let digest_ctx = crate::test_cache::SuiteDigestContext::new(curr_root.as_ref(), standing.errors, alloc);
        let mut specs = blaze_own::Vec::with_capacity_in(inventory.suites.len(), frame);
        for (_, unit) in &inventory.suites {
            let builder = crate::run_parent::builder_for(unit.edition.name(), overflow, panic, producer, isa).map_err(parent)?;
            let digests = crate::test_cache::compute_suite_digests_with_ctx(
                *unit,
                &builder,
                list,
                &ids,
                tools_digest,
                standing.root,
                curr_root.as_ref(),
                &digest_ctx,
                alloc,
            );
            specs.push(crate::test_drive::SuiteSpec { unit: *unit, builder, digests });
        }
        drop(digest_ctx);
        drop(curr_root);
        let lent = standing.lent;
        let prep_erased = blaze_own::Erased::new(frame);
        let prep_bump = prep_erased.bump();
        let prepared = crate::test_drive::prepare_suites(
            cx, clock, launch.out, specs.as_slice(), &ids, &certificate, tools_digest, config, launch.args, lent, watch, prep_bump,
        )
        .map_err(|error| CliError::Test(blaze_own::Box::new_in(error, heap.alloc())))?;
        phases.mark_cx(cx, "test_prepare", frame);
        match prepared {
            // Zero-spawn fast path: every targeted test hit `TestCache` (or was filtered/ignored);
            // skip native-image linking, crate names opening, `$TMPDIR`, code workers, and launcher spawns.
            crate::test_drive::PreparedSuites::Done(status) => {
                ids_line = Some(ids.line(&asked, ids_ns));
                status
            }
            crate::test_drive::PreparedSuites::Dirty(dirty) => {
                // Each dirty suite's native image, as `run` has its program's (engine_native.rs `image_for`):
                // clean cached suites skip native image linking completely, and pure-Rust dirty suites
                // hit the CAS memo with zero `CrateScope` reads and zero segment mmaps.
                let mut images = blaze_own::Vec::with_capacity_in(inventory.suites.len(), frame);
                for (at, (_, unit)) in inventory.suites.iter().enumerate() {
                    if !dirty.is_suite_dirty(at) || !crate::native_image::linked(list, unit.at) {
                        images.push(Ok(None));
                        continue;
                    }
                    let closure = suite_closure(list, unit.at, alloc);
                    let image = native_image_for_cached(cx, &mut standing, list, &closure, false, watch, frame)
                        .map(|image| image.send)
                        .map_err(|error| crate::work::held_str(bstd::mem::format_in!(frame, "{error}").as_str(), frame));
                    images.push(image);
                }
                // One runner for every dirty suite, made at the first proc-macro call any body meets
                // ([`RunProcs`]; C9). Before `runnable` and `bin_suites`, whose names records borrow it.
                let inputs = inventory.suites.first().map(|(found, _)| RunProcs { cx, root, snapshot: found.snapshot(), graph: standing.graph, env: &facts.fetch_env, frame });
                let maker = crate::job_texts::MadeOnce::new(|| match &inputs {
                    Some(inputs) => inputs.make(),
                    None => Err("no proc-macro runner: this command has no test suite (engine_start.rs `run_tests`)"),
                });
                let procs = crate::job_texts::Procs::ByNeed(&maker);
                // Command-wide lazy scope-record opener shared across every dirty suite, binary
                // stub, and `CodePool` worker: `test_open_suites` opens ZERO dependency/toolchain
                // crates upfront (rule 77), and touched crates share one `VerifiedModules` +
                // `FileItems` cache.
                let scope_records = inventory.suites.first().map(|(found, _)| {
                    crate::front_load::ScopeRecords::new(found.snapshot(), blaze_world::crate_list::CrateGraph::len(list), heap)
                });
                // Collect workspace `[[bin]]` targets exposed via `CARGO_BIN_EXE_<name>` by at least
                // one dirty integration-test/bench suite (`test.md` piece 17). Native-image failures
                // for a helper are isolated to that package's `expose_bins` suites.
                let mut needed_bins: blaze_own::Vec<(&str, &str), _> = blaze_own::Vec::new_in(frame);
                for (at, (_, unit)) in inventory.suites.iter().enumerate() {
                    if !dirty.is_suite_dirty(at) || !unit.expose_bins {
                        continue;
                    }
                    let env_facts = list.env_facts(unit.at);
                    if let Some((pkg, _)) = env_facts.package {
                        for bin_name in env_facts.bin_exes.names() {
                            if !needed_bins.iter().any(|&(p, b)| p == pkg && b == bin_name) {
                                needed_bins.push((pkg, bin_name));
                            }
                        }
                    }
                }
                let bin_panic = blaze_ladder::panic_strategy::of_profile(profile);
                let mut bin_suites = blaze_own::Vec::new_in(frame);
                let mut bin_native_errors: blaze_own::Vec<(&str, &str), _> = blaze_own::Vec::new_in(frame);
                for (found, bin_unit) in &inventory.bins {
                    if !needed_bins.iter().any(|&(p, b)| p == bin_unit.package && b == bin_unit.bin_name) {
                        continue;
                    }
                    let native = if crate::native_image::linked(list, bin_unit.at) {
                        let closure = suite_closure(list, bin_unit.at, alloc);
                        match crate::engine_native::image_for(cx, &mut standing, &closure, watch) {
                            Ok(image) => image.send,
                            Err(error) => {
                                let shown = crate::work::held_str(
                                    bstd::mem::format_in!(
                                        frame,
                                        "CARGO_BIN_EXE helper `{}/{}` native image: {error}",
                                        bin_unit.package,
                                        bin_unit.bin_name
                                    )
                                    .as_str(),
                                    frame,
                                );
                                if !bin_native_errors.iter().any(|&(p, _)| p == bin_unit.package) {
                                    bin_native_errors.push((bin_unit.package, shown));
                                }
                                continue;
                            }
                        }
                    } else {
                        None
                    };
                    let crates = match scope_records.as_ref() {
                        Some(sr) => run_names::crates_of_by_need(cx, found, run_names::Owner::At(bin_unit.at), &mut *standing.store, &mut facts.bodies, &mut read, sr, &facts.fetch_env, &pack_dirs, frame, heap),
                        None => run_names::crates_of(cx, found, run_names::Owner::At(bin_unit.at), &mut *standing.store, &mut facts.bodies, &mut read, &facts.fetch_env, &pack_dirs, frame, heap),
                    }
                    .map_err(CliError::RunNames)?;
                    let names = run_names::names_in(cx, root, crates, procs, &seen, frame).map_err(CliError::RunNames)?;
                    let builder = crate::run_parent::builder_for(bin_unit.edition.name(), overflow, bin_panic, producer, isa).map_err(parent)?;
                    bin_suites.push(crate::test_bins::BinSuite {
                        package: bin_unit.package,
                        bin_name: bin_unit.bin_name,
                        key: bin_unit.key,
                        names,
                        builder,
                        native,
                    });
                }
                let mut runnable = blaze_own::Vec::with_capacity_in(inventory.suites.len(), frame);
                // Each dirty test unit runs in its package's root, told its package's variables
                // (test_drive.rs `PackageRun`); clean cached suites skip crates/names loading.
                let checkout = standing.place.served.root_text();
                for (at, (found, unit)) in inventory.suites.iter().enumerate() {
                    if !dirty.is_suite_dirty(at) {
                        runnable.push(None);
                        continue;
                    }
                    let place = crate::test_drive::package_run(checkout, list.env_facts(unit.at), bump)
                        .map_err(|why| CliError::Start(StartError::test_place(bstd::mem::format_in!(frame, "{why}").as_str())))?;
                    let named = cx.span("run names: open the names records");
                    let crates = match scope_records.as_ref() {
                        Some(sr) => run_names::crates_of_by_need(&named, found, run_names::Owner::At(unit.at), &mut *standing.store, &mut facts.bodies, &mut read, sr, &facts.fetch_env, &pack_dirs, frame, heap),
                        None => run_names::crates_of(&named, found, run_names::Owner::At(unit.at), &mut *standing.store, &mut facts.bodies, &mut read, &facts.fetch_env, &pack_dirs, frame, heap),
                    }
                    .map_err(CliError::RunNames)?;
                    drop(named);
                    let names = run_names::names_in(cx, root, crates, procs, &seen, frame).map_err(CliError::RunNames)?;
                    let Some(spec) = specs.get(at) else {
                        return Err(CliError::Start(StartError::NoProgram));
                    };
                    let native = match images.get(at).copied().unwrap_or(Ok(None)) {
                        Err(why) => Err(why),
                        Ok(send) => match unit
                            .expose_bins
                            .then(|| bin_native_errors.iter().find(|&&(p, _)| p == unit.package).map(|&(_, why)| why))
                            .flatten()
                        {
                            Some(why) => Err(why),
                            None => Ok(send),
                        },
                    };
                    runnable.push(Some(crate::test_drive::Suite { names, crates, builder: spec.builder, unit: *unit, place, native }));
                }
                phases.mark_cx(cx, "test_open_suites", frame);
                let typed = crate::engine_body_record::TypedFrom { cx, lent, records: standing.records(), alloc: heap.alloc() };
                let ran = dirty.run(
                    cx, launch.fs, launch.env, launch.host, clock, rng, launch.out, launch.err,
                    runnable.as_slice(), bin_suites.as_slice(), &ids, typed, certificate, config, launch.args, lent, root, procs, heap, watch, frame,
                );
                drop(runnable);
                if let Some(ref sr) = scope_records {
                    read.add_by_need(sr.counts());
                }
                end_procs(cx, &maker, err, "test")?;
                phases.mark_cx(cx, "test_run", frame);
                ids_line = Some(ids.line(&asked, ids_ns));
                ran.map_err(|error| CliError::Test(blaze_own::Box::new_in(error, heap.alloc())))?
            }
        }
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

    #[test]
    fn scoped_body_ids_certs_are_disjoint_from_whole_and_each_other() {
        let root = blaze_ids::ContentId([9; 32]);
        let whole = super::body_ids_cert(root, crate::engine_check::CheckScope::Whole);
        assert_eq!(whole, blaze_drive::Key(root), "whole-workspace cert is unchanged");
        let a = super::body_ids_cert(root, crate::engine_check::CheckScope::Test { packages: &["a"], bin: None, bin_roots: &[] });
        let b = super::body_ids_cert(root, crate::engine_check::CheckScope::Test { packages: &["b"], bin: None, bin_roots: &[] });
        let bin = super::body_ids_cert(root, crate::engine_check::CheckScope::Bin { package: "a", root: "crates/a/src/main.rs" });
        assert_ne!(a, whole);
        assert_ne!(b, whole);
        assert_ne!(a, b);
        assert_ne!(bin, whole);
        assert_ne!(bin, a);
    }

    #[test]
    fn page_reuse_oracle_tracks_changed_pages_skipped_toggles_and_unit_graph() {
        use crate::run_ids::ReuseOracle as _;

        let alloc = blaze_own::testing::heap().alloc();
        let bits = 4u8; // 16 pages
        let prev = [blaze_drive::Key(blaze_ids::ContentId([0x10; 32])); 16];
        let mut curr = prev;
        let edited_file = blaze_drive::Name::file(b"crates/a/src/lib.rs");
        let skipped_file = blaze_drive::Name::file(b"crates/b/src/bin/extra.rs");
        let untouched_file = blaze_drive::Name::file(b"crates/c/src/lib.rs");
        let edited_page = usize::try_from(blaze_engine::pages::content_page_of(edited_file, bits)).unwrap_or(0);
        let ug_page = usize::try_from(blaze_engine::pages::content_page_of(blaze_drive::Name::unit_graph(), bits)).unwrap_or(0);
        let target_page = if edited_page == ug_page { (edited_page + 1) % 8 } else { edited_page };
        if let Some(slot) = curr.get_mut(target_page) {
            *slot = blaze_drive::Key(blaze_ids::ContentId([0x20; 32]));
        }

        let mut skipped = blaze_own::Vec::new_in(alloc);
        skipped.push(skipped_file);
        let oracle = super::PageReuseOracle::from_flat_pages(bits, &prev, &curr, skipped, alloc);
        assert!(oracle.is_some(), "oracle must construct when UnitGraph page is unchanged");
        if let Some(oracle) = oracle {
            if edited_page == target_page {
                assert!(!oracle.unchanged(edited_file), "changed page must report unchanged=false");
            }
            assert!(!oracle.unchanged(skipped_file), "PLACE_SKIPPED toggle must report unchanged=false");
            let untouched_page = usize::try_from(blaze_engine::pages::content_page_of(untouched_file, bits)).unwrap_or(0);
            if untouched_page != target_page {
                assert!(oracle.unchanged(untouched_file), "untouched page must report unchanged=true");
            }
        }

        // Control: when UnitGraph's page changed and UnitGraph's row moved, oracle returns None so full walk runs.
        if let Some(slot) = curr.get_mut(ug_page) {
            *slot = blaze_drive::Key(blaze_ids::ContentId([0x30; 32]));
        }
        assert!(
            super::PageReuseOracle::from_flat_pages(bits, &prev, &curr, blaze_own::Vec::new_in(alloc), alloc).is_none(),
            "changed UnitGraph page with moved UnitGraph row must disable delta reuse"
        );
        // Hash-bucket collision on `ug_page` when `Name::unit_graph()` row is unchanged:
        // oracle succeeds, marks `ug_page` changed (so colliding tasks re-read fresh), and keeps
        // all untouched pages reusable!
        let colliding = super::PageReuseOracle::from_flat_pages_with_ug(bits, &prev, &curr, true, blaze_own::Vec::new_in(alloc), alloc);
        assert!(colliding.is_some(), "ug_page hash-bucket collision with unchanged UnitGraph row must allow delta reuse");
        if let Some(oracle) = colliding {
            assert!(!oracle.unchanged(blaze_drive::Name::unit_graph()), "ug_page itself remains marked changed for colliding tasks");
            let untouched_page = usize::try_from(blaze_engine::pages::content_page_of(untouched_file, bits)).unwrap_or(0);
            if untouched_page != target_page && untouched_page != ug_page {
                assert!(oracle.unchanged(untouched_file), "untouched pages stay reusable across ug_page collision");
            }
        }
        // Control: mismatched slice length returns None.
        assert!(super::PageReuseOracle::from_flat_pages(bits, &prev[..8], &curr, blaze_own::Vec::new_in(alloc), alloc).is_none());
    }

    #[test]
    fn workspace_body_ids_anchor_shares_across_worktrees() {
        use blaze_cas::plain::{Namespace, Settings as StoreSettings};
        use blaze_core::FileName;
        use blaze_core::sim::{Faults, Sim};
        use crate::store::plain_place::{Facts, after_answer, config, open, test_cx};

        let alloc = blaze_own::testing::heap().alloc();
        let cx = test_cx();
        let open_lent = |sim: Sim, nonce: u64| {
            let Some(store_name) = FileName::new("store") else { panic!("store name") };
            let cfg = config(store_name, &StoreSettings::default(), Facts { boot: Some(1), nonce }, alloc);
            let Ok(lent) = open(sim, cfg, alloc) else { panic!("open sim lent {nonce}") };
            lent
        };
        let lent1 = open_lent(Sim::new(72, Faults::default()), 201);
        let tools: [blaze_drive::Tool; blaze_drive::key::KINDS] =
            core::array::from_fn(|at| blaze_drive::Tool(blaze_ids::ContentId([u8::try_from(at).unwrap_or(u8::MAX); 32])));
        let tc = blaze_toolchain::ToolchainId([0x66; 32]);
        let ws_id = blaze_ids::ContentId([0x42; 32]);

        let ptr_wt_a = super::body_ids_last_ptr("/wt/a", crate::engine_check::CheckScope::Whole, tc, &tools);
        let ptr_wt_b = super::body_ids_last_ptr("/wt/b", crate::engine_check::CheckScope::Whole, tc, &tools);
        let ws_ptr = super::body_ids_workspace_ptr(ws_id, crate::engine_check::CheckScope::Whole, tc, &tools);
        assert_ne!(ptr_wt_a, ptr_wt_b);
        assert_ne!(ws_ptr, ptr_wt_a);
        assert_ne!(ws_ptr, ptr_wt_b);

        // Worktree A saves its checkout anchor AND shared workspace anchor.
        let root_a = blaze_ids::ContentId([0x1a; 32]);
        let table_a = blaze_drive::Key(blaze_ids::ContentId([0x2b; 32]));
        let fake_bzri = b"BZRI-worktree-a";
        assert!(lent1.put(&blaze_cas::Key(table_a.0), Namespace::LOCAL, blaze_keyed::kind::ReservedTag::Drive.tag(), fake_bzri).is_ok());
        super::save_last_body_ids_if_moved(&lent1, &ptr_wt_a, root_a, table_a);
        super::save_last_body_ids_if_moved(&lent1, &ws_ptr, root_a, table_a);
        assert!(after_answer(&cx, &lent1, 0).is_ok());

        // Worktree B opens the shared store on its first edit: its checkout pointer misses, so it
        // adopts Worktree A's anchor through the workspace pointer and hits delta BZRI.
        let lent2 = open_lent(lent1.into_store().into_driver(), 202);
        assert!(super::load_last_body_ids(&lent2, &ptr_wt_b).is_none());
        let adopted = super::load_last_body_ids(&lent2, &ptr_wt_b)
            .or_else(|| super::load_last_body_ids(&lent2, &ws_ptr));
        assert_eq!(adopted, Some((root_a, &fake_bzri[..])));
    }

    #[test]
    fn last_body_ids_anchor_round_trips_and_skips_redundant_writes() {
        use blaze_cas::plain::{Namespace, Settings as StoreSettings};
        use blaze_core::FileName;
        use blaze_core::sim::{Faults, Sim};
        use crate::store::plain_place::{Facts, after_answer, config, open, test_cx};

        let alloc = blaze_own::testing::heap().alloc();
        let cx = test_cx();
        let open_lent = |sim: Sim, nonce: u64| {
            let Some(store_name) = FileName::new("store") else { panic!("store name") };
            let cfg = config(store_name, &StoreSettings::default(), Facts { boot: Some(1), nonce }, alloc);
            let Ok(lent) = open(sim, cfg, alloc) else { panic!("open sim lent {nonce}") };
            lent
        };
        let lent1 = open_lent(Sim::new(71, Faults::default()), 101);

        let tools: [blaze_drive::Tool; blaze_drive::key::KINDS] =
            core::array::from_fn(|at| blaze_drive::Tool(blaze_ids::ContentId([u8::try_from(at).unwrap_or(u8::MAX); 32])));
        let tc = blaze_toolchain::ToolchainId([0x55; 32]);
        let ptr_whole = super::body_ids_last_ptr("/ws/a", crate::engine_check::CheckScope::Whole, tc, &tools);
        let ptr_other = super::body_ids_last_ptr("/ws/b", crate::engine_check::CheckScope::Whole, tc, &tools);
        let ptr_scoped = super::body_ids_last_ptr("/ws/a", crate::engine_check::CheckScope::Test { packages: &["p"], bin: None, bin_roots: &[] }, tc, &tools);
        assert_ne!(ptr_whole, ptr_other);
        assert_ne!(ptr_whole, ptr_scoped);

        assert!(super::load_last_body_ids(&lent1, &ptr_whole).is_none(), "empty store has no prior anchor");

        let root1 = blaze_ids::ContentId([0xaa; 32]);
        let table_key1 = blaze_drive::Key(blaze_ids::ContentId([0xbb; 32]));
        let fake_bzri = b"BZRI-payload-bytes";
        assert!(lent1.put(&blaze_cas::Key(table_key1.0), Namespace::LOCAL, blaze_keyed::kind::ReservedTag::Drive.tag(), fake_bzri).is_ok());
        super::save_last_body_ids_if_moved(&lent1, &ptr_whole, root1, table_key1);
        let puts_after_first = lent1.counts().puts;
        // Repeat save with identical (root1, table_key1) must write nothing.
        super::save_last_body_ids_if_moved(&lent1, &ptr_whole, root1, table_key1);
        assert_eq!(lent1.counts().puts, puts_after_first, "unchanged anchor must not perform redundant puts");
        assert!(after_answer(&cx, &lent1, 0).is_ok());

        // Reload after restart: anchor must recover (root1, fake_bzri).
        let lent2 = open_lent(lent1.into_store().into_driver(), 102);
        let loaded = super::load_last_body_ids(&lent2, &ptr_whole);
        assert_eq!(loaded, Some((root1, &fake_bzri[..])));
        let puts_before_hit = lent2.counts().puts;
        super::save_last_body_ids_if_moved(&lent2, &ptr_whole, root1, table_key1);
        assert_eq!(lent2.counts().puts, puts_before_hit, "cache hit after restart must perform zero writes");
    }

    #[test]
    fn native_empty_memo_covers_every_input_and_round_trips() {
        use blaze_cas::plain::{Namespace, Settings as StoreSettings};
        use blaze_core::FileName;
        use blaze_core::sim::{Faults, Sim};
        use blaze_drive::{Key, Kind, Name};
        use blaze_ids::ContentId;
        use blaze_ladder::native_links::Platform;
        use crate::store::plain_place::{Facts, after_answer, config, open, test_cx};

        let alloc = blaze_own::testing::heap().alloc();
        let cx = test_cx();
        let open_lent = |sim: Sim, nonce: u64| {
            let Some(store_name) = FileName::new("store") else { panic!("store name") };
            let cfg = config(store_name, &StoreSettings::default(), Facts { boot: Some(1), nonce }, alloc);
            let Ok(lent) = open(sim, cfg, alloc) else { panic!("open sim lent {nonce}") };
            lent
        };

        let producer_a = blaze_ids::producer::Producers::for_tests(b"prod-a").get::<blaze_ids::producer::Whole>();
        let producer_b = blaze_ids::producer::Producers::for_tests(b"prod-b").get::<blaze_ids::producer::Whole>();
        let k0 = blaze_id::CrateItemKey(ContentId([1; 32]));
        let k1 = blaze_id::CrateItemKey(ContentId([2; 32]));
        let s0 = Key(ContentId([10; 32]));
        let s1 = Key(ContentId([11; 32]));
        let s1_moved = Key(ContentId([12; 32]));
        let script_name = Name::new(Kind::Script, ContentId([20; 32]));
        let script_a = Some((script_name, Key(ContentId([21; 32])), ContentId([22; 32])));
        let script_b = Some((script_name, Key(ContentId([21; 32])), ContentId([23; 32])));

        let base = super::native_empty_memo_key(Platform::MacosArm64, &producer_a, None, "src/lib.rs", &[(k0, "app", s0, None), (k1, "core", s1, None)]);
        let same = super::native_empty_memo_key(Platform::MacosArm64, &producer_a, None, "src/lib.rs", &[(k0, "app", s0, None), (k1, "core", s1, None)]);
        assert_eq!(base, same, "memo key must be deterministic");
        assert_ne!(base, super::native_empty_memo_key(Platform::LinuxArm64, &producer_a, None, "src/lib.rs", &[(k0, "app", s0, None), (k1, "core", s1, None)]), "platform must move key");
        assert_ne!(base, super::native_empty_memo_key(Platform::MacosArm64, &producer_b, None, "src/lib.rs", &[(k0, "app", s0, None), (k1, "core", s1, None)]), "producer must move key");
        assert_ne!(base, super::native_empty_memo_key(Platform::MacosArm64, &producer_a, None, "src/main.rs", &[(k0, "app", s0, None), (k1, "core", s1, None)]), "program root must move key");
        assert_ne!(base, super::native_empty_memo_key(Platform::MacosArm64, &producer_a, None, "src/lib.rs", &[(k0, "app", s0, None), (k1, "core", s1_moved, None)]), "CrateScope key must move key");
        let with_script_a = super::native_empty_memo_key(Platform::MacosArm64, &producer_a, None, "src/lib.rs", &[(k0, "app", s0, script_a), (k1, "core", s1, None)]);
        let with_script_b = super::native_empty_memo_key(Platform::MacosArm64, &producer_a, None, "src/lib.rs", &[(k0, "app", s0, script_b), (k1, "core", s1, None)]);
        assert_ne!(base, with_script_a, "adding a script must move key");
        assert_ne!(with_script_a, with_script_b, "changing script out must move key");

        let lent1 = open_lent(Sim::new(79, Faults::default()), 301);
        let img_key = Key(ContentId([0x77; 32]));
        assert_eq!(super::load_native_empty_memo(&lent1, &base, false), None);
        super::save_native_empty_memo(&lent1, &base, img_key);
        // Without the image record itself, `require_record = false` (test suites) hits, while
        // `require_record = true` (`run` start_list) stays a safe miss until the record is present.
        assert_eq!(super::load_native_empty_memo(&lent1, &base, false), Some(img_key));
        assert_eq!(super::load_native_empty_memo(&lent1, &base, true), None);
        assert!(lent1.put(&blaze_cas::Key(img_key.0), Namespace::LOCAL, blaze_keyed::kind::ReservedTag::Drive.tag(), b"empty-image").is_ok());
        assert_eq!(super::load_native_empty_memo(&lent1, &base, true), Some(img_key));
        assert!(after_answer(&cx, &lent1, 0).is_ok());

        let lent2 = open_lent(lent1.into_store().into_driver(), 302);
        assert_eq!(super::load_native_empty_memo(&lent2, &base, true), Some(img_key));
    }

    #[test]
    fn parse_dev_profile_settings_matches_manifest_rules_in_one_parse() {
        use blaze_ladder::run::OverflowChecks;
        use blaze_manifest::layout::panic::ProfilePanic;

        let frame = bstd::mem::Frame::new(bstd::mem::Tag::Other).expect("frame");
        assert_eq!(
            super::parse_dev_profile_settings("[package]\nname = \"a\"\n", &frame).expect("defaults"),
            (ProfilePanic::Unwind, OverflowChecks::On)
        );
        assert_eq!(
            super::parse_dev_profile_settings("[profile.dev]\npanic = \"abort\"\noverflow-checks = false\n", &frame).expect("custom dev"),
            (ProfilePanic::Abort, OverflowChecks::Off)
        );
        assert!(super::parse_dev_profile_settings("[profile.dev]\ninherits = \"release\"\n", &frame).is_err());
        assert!(super::parse_dev_profile_settings("[profile.dev]\npanic = \"immediate-abort\"\n", &frame).is_err());
        assert!(super::parse_dev_profile_settings("[profile.dev.package.foo]\npanic = \"abort\"\n", &frame).is_err());
        assert!(super::parse_dev_profile_settings("[profile.dev]\noverflow-checks = \"yes\"\n", &frame).is_err());
        assert!(super::parse_dev_profile_settings("not valid toml = = =", &frame).is_err());
    }

    #[test]
    fn test_inventory_delta_cache_persists_across_store_reopen() {
        use blaze_cas::plain::Settings as StoreSettings;
        use blaze_core::FileName;
        use blaze_core::sim::{Faults, Sim};
        use blaze_ids::ContentId;
        use crate::store::plain_place::{Facts, after_answer, config, open, test_cx};

        let alloc = blaze_own::testing::heap().alloc();
        let frame = bstd::mem::Frame::new(bstd::mem::Tag::Other).expect("frame");
        let erased = blaze_own::Erased::new(&frame);
        let bump = erased.bump();
        let cx = test_cx();
        let open_lent = |sim: Sim, nonce: u64| {
            let Some(store_name) = FileName::new("store") else { panic!("store name") };
            let cfg = config(store_name, &StoreSettings::default(), Facts { boot: Some(1), nonce }, alloc);
            let Ok(lent) = open(sim, cfg, alloc) else { panic!("open sim lent {nonce}") };
            lent
        };

        let graph_out = ContentId::of("test inventory graph", b"g1");
        let payload = crate::run_names::TestsDeltaCache::encode(&[], alloc).expect("encode empty BZTI");
        let lent1 = open_lent(Sim::new(83, Faults::default()), 401);
        assert!(super::load_test_inventory_cache(&lent1, graph_out, bump).is_none());
        super::save_test_inventory_cache(&lent1, graph_out, payload.as_slice());
        assert_eq!(super::load_test_inventory_cache(&lent1, graph_out, bump), Some(payload.as_slice()));
        assert!(after_answer(&cx, &lent1, 0).is_ok());

        let lent2 = open_lent(lent1.into_store().into_driver(), 402);
        let loaded = super::load_test_inventory_cache(&lent2, graph_out, bump).expect("reopened BZTI");
        assert_eq!(loaded, payload.as_slice());
        assert!(crate::run_names::TestsDeltaCache::open(loaded).is_some());
    }

    #[test]
    fn checking_target_and_quiet_flag_work_for_run() {
        use bstd::io::{Io, Limits, SimWorld};

        let frame = bstd::mem::Frame::new(bstd::mem::Tag::Other).expect("frame");
        let args_of = |words: &[&str]| {
            let mut world = SimWorld::new(51, Limits::USUAL);
            world.args = words.iter().map(|&s| s.into()).collect();
            Io::simulated(world).need_args().expect("args")
        };

        let a_short = args_of(&["blaze", "run", "-q", "-p", "scripts", "--bin", "rules"]);
        let quiet_short = crate::flags_in(&a_short, &frame).expect("short -q");
        assert!(quiet_short.quiet);
        assert_eq!(super::checking_target(&quiet_short, None, &frame).as_str(), "scripts (rules)");

        let a_long = args_of(&["blaze", "run", "--quiet", "-p", "hello"]);
        let quiet_long = crate::flags_in(&a_long, &frame).expect("long --quiet");
        assert!(quiet_long.quiet);
        assert_eq!(super::checking_target(&quiet_long, None, &frame).as_str(), "hello");

        let a_dir = args_of(&["blaze", "run", "crates/demo/"]);
        let dir_only = crate::flags_in(&a_dir, &frame).expect("dir");
        assert!(!dir_only.quiet);
        assert_eq!(super::checking_target(&dir_only, None, &frame).as_str(), "crates/demo");

        let a_bare = args_of(&["blaze", "run"]);
        let bare = crate::flags_in(&a_bare, &frame).expect("bare");
        assert_eq!(super::checking_target(&bare, None, &frame).as_str(), "workspace");

        let mut pwd_world = SimWorld::new(52, Limits::USUAL);
        pwd_world.env.push(("PWD".into(), "/Users/dev/projects/my-app/".into()));
        let pwd_io = Io::simulated(pwd_world);
        let pwd_env = pwd_io.env();
        assert_eq!(super::checking_target(&bare, pwd_env.as_ref(), &frame).as_str(), "my-app");
    }
}
