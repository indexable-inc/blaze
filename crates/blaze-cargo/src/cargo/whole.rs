//! `blaze build` and `run` (docs/no-rustc.md, step K, #91): each program of the build is
//! built whole, every crate from source over the shipped standard library and linked by blaze
//! (step G), and put where cargo would. No `cargo build` runs, and what blaze cannot compile
//! or plan fails the build, with what it is.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use anyhow::Context as _;
use cargo_metadata::{Metadata, Package};

use blaze_metadata::Kind;

use crate::plan::Selection;

use super::{
    FAILED,
    cli::{Invocation, Subcommand, worker_env},
    client::{self, Start},
    daemon::{Checkout, NotBuilt, Reply, Request, TestsTold, WorkerConfig},
    plan::Plan as Linked,
    run, say,
    state::{configured_env, library_path, place, repository, run_env},
};

/// How many of the functions blaze could not compile a failed build names.
const SHOWN: usize = 8;

/// Why blaze does not build what `asked` names yet. Nothing else builds it: the build fails
/// with this.
fn refused(asked: &Invocation) -> Option<String> {
    let flags = &asked.flags;
    let not_yet = |what: &str| Some(format!("blaze does not build {what} yet"));
    match () {
        _ if flags.cargos_plan => Some(
            "`--build-plan` and `--unit-graph` are cargo's plans, and blaze makes its own"
                .to_owned(),
        ),
        _ if !flags.message_format.is_empty() => not_yet("with `--message-format`"),
        _ if !flags.unstable.is_empty() => not_yet("with cargo's unstable flags (`-Z`)"),
        _ if flags.targets.len() > 1 => not_yet("for several targets at once"),
        _ if asked.typed("--target-dir") => {
            not_yet("into a `--target-dir`; `CARGO_TARGET_DIR` is read")
        }
        _ if asked.values_of("--test").len() > 1 => not_yet("several `--test` targets at once"),
        _ => match (asked.kinds_of_targets(), asked.subcommand) {
            (Some(flag), Subcommand::Test) => not_yet(&format!(
                "the targets `{flag}` picks; a test of the default targets, or of those `--lib`, `--bin`, `--example` and `--test` name, is"
            )),
            (Some(flag), _) => not_yet(&format!(
                "the targets `{flag}` picks; a build of the default programs, or of one named by `--bin` or `--example`, is"
            )),
            (None, Subcommand::Test) if asked.target().is_some_and(|it| is_wasm(&it)) => {
                not_yet("or run the tests of a wasm target")
            }
            (None, _) => asked.target_not_linked(),
        },
    }
}

/// A program of the build, as cargo plans it.
struct Program {
    /// The name of the executable.
    name: String,
    /// The root file of its crate.
    root: PathBuf,
    package: Package,
    kind: Kind,
    /// Whether it is a test `cargo test` runs, and whether that starts in libtest's harness.
    test: bool,
    harness: bool,
    /// Where cargo puts it.
    executable: PathBuf,
    /// The directory of the daemon's object of it.
    dir: PathBuf,
}

/// Why the workspace is newer than this blaze's Rust ([`super::toolchain::too_new`]), unless
/// `--ignore-rust-version` says to build it anyway, as cargo does.
fn too_new(asked: &Invocation, metadata: &Metadata) -> Option<String> {
    if asked.typed("--ignore-rust-version") {
        return None;
    }
    super::toolchain::too_new(&metadata.workspace_packages())
}

/// `build` and `run`: builds the programs of `asked` whole and runs the one `run` asks for.
/// A build of no program, of libraries alone, is the daemon's check of them: blaze writes no
/// library for cargo, which builds nothing.
pub fn build(asked: &Invocation) -> anyhow::Result<i32> {
    if let Some(why) = refused(asked) {
        say::error(&why);
        return Ok(FAILED);
    }
    let started = Instant::now();
    let cwd = std::env::current_dir()?;
    let metadata = super::state::metadata(asked);
    if let Some(why) = too_new(asked, &metadata) {
        say::error(&why);
        return Ok(FAILED);
    }
    let mut programs = match planned(asked, &cwd, &metadata, None) {
        Ok(programs) => programs,
        Err(why) => {
            say::error(&format!("blaze could not plan the build: {why:#}"));
            return Ok(FAILED);
        }
    };
    // `cargo run` builds the binary it runs, not a library's `cdylib` or `staticlib`.
    if asked.subcommand == Subcommand::Run {
        programs.retain(|it| !it.kind.linked_library());
    }
    if let Some(it) = programs.iter().find(|it| it.kind == Kind::Dylib) {
        let kind = format!("{:?}", it.kind).to_lowercase();
        say::error(&format!(
            "blaze does not link the `{kind}` of `{}` (target `{}`) yet, so it would write no `{}` (#262)",
            it.package.name,
            it.name,
            it.executable.file_name().unwrap_or_default().to_string_lossy()
        ));
        return Ok(FAILED);
    }
    if programs.is_empty() && asked.subcommand == Subcommand::Run {
        say::error("a bin target must be available for `cargo run`");
        return Ok(FAILED);
    }
    if programs.is_empty() {
        return super::check(asked);
    }
    let mut said = Vec::new();
    compiling(asked, programs.iter().filter(|it| !it.executable.exists()), &mut said);
    let checkout = checkout(asked, &cwd, &metadata, &programs, None);
    match whole(&cwd, &checkout, &programs) {
        Ok(rebuilt) => compiling(asked, rebuilt.into_iter().map(|at| &programs[at]), &mut said),
        Err(why) => {
            why.say();
            return Ok(FAILED);
        }
    }
    finished(asked, asked.profile(), started);
    crate::trace::finish();
    if let Some(failed) = super::remote_failed_it() {
        return Ok(failed);
    }
    if asked.subcommand != Subcommand::Run {
        return Ok(0);
    }
    ran(asked, &cwd, &programs)
}

/// `test`: builds whole the tests of `asked`, and what they run, and runs them as cargo
/// does. One worker loads the workspace as cargo builds it for the tests, once: each library
/// and binary without `cfg(test)` and its unit tests apart with it, the integration tests,
/// and the examples (`cargo_to_crate_graph`), and builds every program of it. The doc tests
/// are made after.
pub fn test(asked: &Invocation) -> anyhow::Result<i32> {
    if let Some(why) = refused(asked) {
        say::error(&why);
        return Ok(FAILED);
    }
    let started = Instant::now();
    let cwd = std::env::current_dir()?;
    let metadata = super::state::metadata(asked);
    if let Some(why) = too_new(asked, &metadata) {
        say::error(&why);
        return Ok(FAILED);
    }
    let named = asked.values_of("--test").first().copied();
    let tests = blaze_metadata::Tests { lib: asked.typed("--lib"), test: named };
    let programs = match planned(asked, &cwd, &metadata, Some(tests)) {
        Ok(programs) => programs,
        Err(why) => {
            say::error(&format!("blaze could not plan the build: {why:#}"));
            return Ok(FAILED);
        }
    };
    // The doc tests alone build nothing before their examples.
    if asked.flags.doc {
        let libraries: Vec<&Program> = programs.iter().filter(|it| it.kind == Kind::Lib).collect();
        return doc_tests(asked, &cwd, metadata.workspace_root.as_std_path(), &libraries);
    }
    let unit = |it: &Program| it.test && matches!(it.kind, Kind::Lib | Kind::Bin);
    if let Some(it) = programs.iter().find(|it| unit(it) && !it.harness) {
        let why = "without libtest's harness (`harness = false`) yet";
        say::error(&format!("blaze does not build the unit tests of `{}` {why}", it.name));
        return Ok(FAILED);
    }
    let mut said = Vec::new();
    compiling(asked, programs.iter().filter(|it| !it.executable.exists()), &mut said);
    // The scratch directory cargo makes for the integration tests and benchmarks.
    let scratch = metadata.target_directory.as_std_path().join("tmp");
    _ = std::fs::create_dir_all(&scratch);
    let told = TestsTold { executables: programs_dir(asked, &metadata), scratch };
    let checkout = checkout(asked, &cwd, &metadata, &programs, Some(told));
    let profile = match asked.profile() {
        "dev" => "test",
        other => other,
    };
    // A cargo since rust-lang/cargo#16421 also sets `CARGO_BIN_EXE_<name>` of each program of
    // its package when it runs an integration test: assert_cmd's `cargo_bin` reads it at run
    // time, and finds no program beside the test in cargo's second layout.
    let programs_named = |test: &Program| -> Vec<(String, String)> {
        if test.kind != Kind::Test {
            return Vec::new();
        }
        let named =
            |it: &&Program| it.kind == Kind::Bin && !it.test && it.package.id == test.package.id;
        programs
            .iter()
            .filter(named)
            .map(|it| (format!("CARGO_BIN_EXE_{}", it.name), it.executable.display().to_string()))
            .collect()
    };
    let mut ran = Vec::new();
    for (index, program) in programs.iter().enumerate().filter(|(_, it)| it.test) {
        ran.push(super::state::Program::of_whole(
            program.name.clone(),
            state_kind(program.kind),
            program.package.name.to_string(),
            program.root.clone(),
            super::state::package_dir(&program.package),
            program.executable.clone(),
            (0, index),
            run_env(&program.package, &[])
                .into_iter()
                .chain(library_path(&program.executable, None))
                .chain(programs_named(program))
                .collect(),
        ));
    }
    // Cargo runs a package's tests as it orders its targets, and stops at the first that fails.
    let rank = |kind: &super::state::Kind| {
        use super::state::Kind::*;
        match kind {
            Lib => 0,
            Bin => 1,
            Test => 2,
            _ => 3,
        }
    };
    ran.sort_by(|a, b| {
        (&a.package, rank(&a.kind), &a.name).cmp(&(&b.package, rank(&b.kind), &b.name))
    });
    let tested_library = |it: &&Program| unit(it) && it.kind == Kind::Lib;
    let documented = programs.iter().any(|it| tested_library(&it));
    let root = metadata.workspace_root.as_std_path().to_owned();
    let state = super::state::State::of_whole(
        checkout.key.clone(),
        root,
        profile.to_owned(),
        ran,
        vec![checkout.clone()],
        configured_env(&cwd),
    );
    super::affected::ask(&state);
    for (plan, program) in checkout.programs.iter().zip(&programs) {
        let tests = plan.object.with_file_name(super::plan::TESTS);
        // An integration test, or an example that is tests (`test = true`), which cargo
        // builds with `--test`: the load has it with `cfg(test)`.
        let harnessed = matches!(program.kind, Kind::Test | Kind::Example) && program.test;
        match harnessed && program.harness {
            true => std::fs::write(&tests, "")?,
            false => _ = std::fs::remove_file(&tests),
        }
    }
    match whole(&cwd, &checkout, &programs) {
        Ok(rebuilt) => compiling(asked, rebuilt.into_iter().map(|at| &programs[at]), &mut said),
        Err(why) => {
            why.say();
            return Ok(FAILED);
        }
    }
    finished(asked, profile, started);
    crate::trace::finish();
    if let Some(failed) = super::remote_failed_it() {
        return Ok(failed);
    }
    if asked.flags.no_run {
        return Ok(0);
    }
    let code = run::tests(asked, &state, &cwd, None, metadata.target_directory.as_std_path())?;
    if asked.flags.why {
        return Ok(code);
    }
    let stopped = code != 0 && !asked.flags.no_fail_fast;
    // Cargo runs no doc tests when tests are picked by target or by name.
    let picked = asked.flags.selects_targets || !asked.positional.is_empty();
    if stopped || picked || !documented {
        return Ok(code);
    }
    let libraries: Vec<&Program> = programs.iter().filter(tested_library).collect();
    Ok(code.max(doc_tests(asked, &cwd, metadata.workspace_root.as_std_path(), &libraries)?))
}

/// Runs the examples in the documentation of `libraries`, as `cargo test --doc` would and with
/// no cargo: blaze writes down the run of rustdoc cargo would make of each library, and one
/// run of blaze makes them all, over one load of the workspace.
fn doc_tests(
    asked: &Invocation,
    cwd: &Path,
    root: &Path,
    libraries: &[&Program],
) -> anyhow::Result<i32> {
    use std::ffi::OsString;
    let replay = crate::doctest::Replay {
        dir: std::env::temp_dir().join(format!("blaze-doctests-{}", std::process::id())),
        no_fail_fast: asked.flags.no_fail_fast,
        quiet: asked.flags.quiet,
    };
    std::fs::create_dir_all(&replay.dir)?;
    let asked_in = asked.flags.manifest_path.as_ref().map_or(cwd.to_owned(), |it| cwd.join(it));
    let build = Selection { asked_in: Some(asked_in), ..asked.selection() };
    let build = serde_json::to_string(&build)?;
    // Cargo's `-q` is the harness's too: it shows the results tersely.
    let quiet = asked.flags.quiet.then_some("--quiet");
    let test_args =
        shlex::try_join(quiet.into_iter().chain(asked.trailing.iter().map(String::as_str)))?;
    for library in libraries {
        let package = &library.package;
        let target = package.targets.iter().find(|it| it.src_path.as_std_path() == library.root);
        let Some(target) = target.filter(|it| it.doctest) else { continue };
        let mut args: Vec<OsString> = vec![
            "--test".into(),
            target.src_path.as_std_path().into(),
            "--crate-name".into(),
            target.name.replace('-', "_").into(),
            "--edition".into(),
            target.edition.as_str().into(),
            "--test-run-directory".into(),
            super::state::package_dir(package).into(),
        ];
        // The examples' programs are kept from run to run beside the build's tests.
        if let Some(deps) = library.executable.parent() {
            args.extend(["-L".into(), format!("dependency={}", deps.display()).into()]);
        }
        if !test_args.is_empty() {
            args.extend(["--test-args".into(), test_args.clone().into()]);
        }
        if let Some(triple) = &asked.selection().target {
            args.extend(["--target".into(), triple.into()]);
        }
        let mut env: Vec<(OsString, OsString)> =
            std::env::vars_os().filter(|(name, _)| name != "BLAZE_CACHE").collect();
        env.extend(
            run_env(package, &[]).into_iter().map(|(name, value)| (name.into(), value.into())),
        );
        env.push((crate::doctest::BUILD.into(), build.clone().into()));
        // The daemon's store, as its workers have it: what the examples link of the standard
        // library is compiled once, not for each library each run.
        if let Some(store) = super::daemon::store() {
            env.push(("BLAZE_CACHE".into(), store.into()));
        }
        crate::doctest::write_run(&replay.dir, args, root.to_owned(), env)?;
    }
    let ran = replay.command().and_then(|blaze| {
        blaze
            .map(|mut it| Ok(run::exit_code(it.status().context("running the doc tests")?)))
            .transpose()
    });
    _ = std::fs::remove_dir_all(&replay.dir);
    Ok(ran?.unwrap_or(0))
}

/// What the state of cargo's builds calls a kind of target.
fn state_kind(kind: Kind) -> super::state::Kind {
    match kind {
        Kind::Lib => super::state::Kind::Lib,
        Kind::Bin => super::state::Kind::Bin,
        Kind::Example => super::state::Kind::Example,
        Kind::Test => super::state::Kind::Test,
        Kind::Cdylib | Kind::Staticlib | Kind::Dylib => super::state::Kind::Lib,
    }
}

/// Says that each package of `programs` is compiled, once in a command: as cargo, of what is
/// built and not of what was up to date. A program not built before is said before the build,
/// which may be long; one the daemon rebuilt, after it answers.
fn compiling<'a>(
    asked: &Invocation,
    programs: impl Iterator<Item = &'a Program>,
    said: &mut Vec<cargo_metadata::PackageId>,
) {
    for program in programs {
        let package = &program.package;
        if said.contains(&package.id) {
            continue;
        }
        said.push(package.id.clone());
        let at = super::state::package_dir(package);
        say::status(
            asked.flags.quiet,
            "Compiling",
            &format!("{} v{} ({})", package.name, package.version, at.display()),
        );
    }
}

/// Says the build of `profile` is done.
fn finished(asked: &Invocation, profile: &str, started: Instant) {
    let took = started.elapsed().as_secs_f64();
    let finished = format!("`{profile}` profile [{}] target(s) in {took:.2}s", super::blaze_tag());
    say::status(asked.flags.quiet, "Finished", &finished);
}

/// Why a whole build failed: the errors the daemon found in the source, written as rustc
/// and cargo write them, or what else kept a program from being built.
enum Unbuilt {
    Said(String),
    Why(String),
}

impl From<String> for Unbuilt {
    fn from(why: String) -> Unbuilt {
        Unbuilt::Why(why)
    }
}

impl Unbuilt {
    fn say(self) {
        match self {
            Unbuilt::Said(written) => eprintln!("{written}"),
            Unbuilt::Why(why) => say::error(&why),
        }
    }
}

/// The errors `said` of `program`, as cargo ends a build rustc turned down: with the target
/// that did not compile.
fn turned_down(program: &Program, said: &crate::errors::Said) -> String {
    let name = &program.name;
    let target = match (program.kind, program.test) {
        (Kind::Lib | Kind::Cdylib | Kind::Staticlib | Kind::Dylib, false) => "lib".to_owned(),
        (Kind::Lib | Kind::Cdylib | Kind::Staticlib | Kind::Dylib, true) => "lib test".to_owned(),
        (Kind::Bin, false) => format!("bin \"{name}\""),
        (Kind::Bin, true) => format!("bin \"{name}\" test"),
        (Kind::Example, _) => format!("example \"{name}\""),
        (Kind::Test, _) => format!("test \"{name}\""),
    };
    super::state::turned_down(said, &program.package.name, &target)
}

/// Has the daemon build the `programs` of `checkout` whole, and puts each where cargo would.
/// A worker of the daemon loads the workspace and links each program before it answers, and
/// builds them again on what it has loaded when asked next. `Ok` names the programs, by
/// index, the daemon built rather than found up to date; `Err` says why not.
fn whole(cwd: &Path, checkout: &Checkout, programs: &[Program]) -> Result<Vec<usize>, Unbuilt> {
    let asking_at = Instant::now();
    let reply = client::ask(Request::Build(super::asking(checkout)), Start::IfMissing)
        .map_err(|it| format!("{it:#}"))?;
    crate::trace::note("client↔daemon", crate::trace::CLIENT, asking_at);
    let built = match reply {
        Some(Reply::Synced { programs, spans }) => {
            crate::trace::note_all(spans);
            for line in programs.iter().flatten() {
                crate::trace::note_all(crate::trace::split(line).1);
            }
            programs
        }
        Some(Reply::Cold { reason }) => return Err(not_loaded(cwd, checkout, &reason).into()),
        None => return Err(client::why_none().into()),
        Some(other) => return Err(format!("the daemon answered {other:?}").into()),
    };
    let mut rebuilt = Vec::new();
    let pairs = std::iter::zip(programs, &checkout.programs).zip(built).enumerate();
    for (at, ((program, plan), built)) in pairs {
        match built {
            Ok(said) if said == "ok unchanged" => {}
            Ok(_) => rebuilt.push(at),
            Err(NotBuilt::Error(said)) => return Err(Unbuilt::Said(turned_down(program, &said))),
            Err(why) => {
                return Err(format!("could not build `{}`: {}", program.name, why.why()).into());
            }
        }
        let _unwritten = crate::splice::unwritten(&plan.binary);
        let stand_ins = crate::link::stand_ins(&plan.binary).map_err(|it| format!("{it:#}"))?;
        // On Windows the executable keeps every stand-in (see `main.rs`, `link_whole`).
        if !stand_ins.is_empty() && !cfg!(windows) {
            let mut reached: Vec<String> = stand_ins
                .iter()
                .take(SHOWN)
                .map(|it| it.trim().trim_start_matches(hir_ty_codegen::STAND_IN_SAYS))
                .map(without_hashes)
                .collect();
            if stand_ins.len() > SHOWN {
                reached.push(format!("and {} more", stand_ins.len() - SHOWN));
            }
            return Err(format!(
                "`{}` can reach functions blaze could not compile, so it is not built:\n  {}",
                program.name,
                reached.join("\n  ")
            )
            .into());
        }
        if let Some(parent) = program.executable.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|it| format!("creating {}: {it}", parent.display()))?;
        }
        place(&plan.binary, &program.executable)
            .map_err(|it| format!("installing {}: {it}", program.executable.display()))?;
    }
    Ok(rebuilt)
}

/// The daemon's checkout of a whole build of `programs`, each linked in its `dir`. Of the
/// tests (`tests`, which says what the integration tests are told), by a worker that loads
/// the workspace as `cargo test` builds it, with every program of it.
fn checkout(
    asked: &Invocation,
    cwd: &Path,
    metadata: &Metadata,
    programs: &[Program],
    tests: Option<TestsTold>,
) -> Checkout {
    let root = metadata.workspace_root.as_std_path();
    let asked_in = asked.flags.manifest_path.as_ref().map_or(cwd.to_owned(), |it| cwd.join(it));
    // From the root of the checkout, as `project` is: the same build in a worktree of the
    // repository is loaded by the same worker (docs/cargo.md).
    let asked_in = asked_in.strip_prefix(root).map_or(asked_in.clone(), Path::to_owned);
    let project = programs.first().map(|it| super::state::package_dir(&it.package));
    let project = project.as_deref().unwrap_or(root);
    let inside = |it: PathBuf| it.strip_prefix(root).map_or(it.clone(), Path::to_owned);
    let key = match tests {
        Some(_) => "whole-test",
        None => "whole",
    };
    let config = WorkerConfig {
        repository: repository(root),
        project: project.strip_prefix(root).unwrap_or(Path::new("")).to_owned(),
        cfg_test: tests.is_some(),
        selection: Selection { asked_in: Some(asked_in), ..asked.selection() },
        env: worker_env(),
        command_env: BTreeMap::new(),
        whole: true,
        tests_told: tests.map(|it| TestsTold {
            executables: inside(it.executables),
            scratch: inside(it.scratch),
        }),
    };
    // A worker links its programs where it was started to and builds again only what was
    // edited: another worker's build, of other features or flags, is not in its files.
    let worker = config.id();
    let dirs: Vec<PathBuf> = programs.iter().map(|it| it.dir.join(&worker)).collect();
    let plans = programs.iter().zip(&dirs).map(|(program, dir)| {
        // A `cdylib` or `staticlib` is linked under the name of its file, which a binary of
        // the same name beside it does not have.
        let staticlib = program.kind == Kind::Staticlib;
        let shared = program.kind == Kind::Cdylib || staticlib;
        let name = match shared {
            true => program.executable.file_name().unwrap_or_default().to_string_lossy().into(),
            false => program.name.clone(),
        };
        Linked {
            crates: vec![program.root.clone()],
            object: dir.join(format!("{name}.o")),
            patch: dir.join(format!("{name}.patch.o")),
            binary: dir.join(&name),
            link_cmd: String::new(),
            symbol_files: Vec::new(),
            // A library's or a binary's unit tests: an integration test is its own target.
            unit_test: program.test && matches!(program.kind, Kind::Lib | Kind::Bin),
            linked_as_built: Vec::new(),
            copies: Vec::new(),
            package: None,
            stamp: 0,
            whole: true,
            shared,
            staticlib,
        }
    });
    for dir in &dirs {
        _ = std::fs::create_dir_all(dir);
    }
    let members = metadata.workspace_packages();
    let manifests = members.iter().map(|it| it.manifest_path.as_std_path().to_owned());
    let lock = root.join("Cargo.lock");
    // A worker that loaded other manifests loads them again; one with the same is asked to
    // build again what was edited since.
    let generation = manifests.clone().chain([lock]).filter_map(|it| written_at(&it)).max();
    let sources = members.iter().flat_map(|it| sources_in(&super::state::package_dir(it)));
    Checkout {
        key: format!("{}-{key}", asked.key(cwd)),
        generation: generation.unwrap_or(0),
        kept: false,
        built_at: 0,
        config,
        root: root.to_owned(),
        programs: plans.collect(),
        sources: manifests.chain(sources).collect(),
        // What `env!` reads that cargo sets is each crate's own, in the load.
        env: BTreeMap::new(),
        scripts: None,
    }
}

/// When `file` was last written, as a stamp.
fn written_at(file: &Path) -> Option<u128> {
    let written = std::fs::metadata(file).and_then(|it| it.modified()).ok()?;
    Some(written.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos())
}

/// The Rust files of the package in `dir`, but not those of a target directory or of a
/// package within it.
fn sources_in(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut left = vec![dir.to_owned()];
    while let Some(dir) = left.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                let hidden = name.to_string_lossy().starts_with('.');
                if !hidden && name != "target" && !path.join("Cargo.toml").exists() {
                    left.push(path);
                }
            } else if path.extension().is_some_and(|it| it == "rs") {
                found.push(path);
            }
        }
    }
    found
}

/// Why the daemon did not load the workspace, from what its worker said: the functions blaze
/// could not compile, or its last error.
fn not_loaded(cwd: &Path, checkout: &Checkout, reason: &str) -> String {
    let named = checkout.programs.iter().map(|it| it.binary.file_name().unwrap_or_default());
    let named: Vec<String> = named.map(|it| format!("`{}`", it.to_string_lossy())).collect();
    let named = named.join(", ");
    let Some(log) = worker_log(&checkout.root.join(&checkout.config.project)) else {
        return format!("could not build {named}: {reason}");
    };
    let said = std::fs::read_to_string(&log).unwrap_or_default();
    let shown = log.strip_prefix(cwd).unwrap_or(&log).display().to_string();
    // The functions of the program blaze could not compile, which nothing else compiles: the
    // lines after the error's first ([`crate::refused_error`]).
    let refused: Vec<&str> = said
        .lines()
        .skip_while(|it| !(it.starts_with("Error: ") && it.contains(crate::REFUSED_SAYS)))
        .skip(1)
        .take_while(|it| it.starts_with("  "))
        .map(str::trim)
        .collect();
    if !refused.is_empty() {
        return format!(
            "{named} reaches functions blaze could not compile, so it is not built:\n  {}\n(what blaze said is in {shown})",
            refused.join("\n  ")
        );
    }
    let last = super::daemon::last_error(&said);
    let last = last.as_deref().unwrap_or(reason).replace('\n', "\n  ");
    format!("could not build {named}: {last}\n(what blaze said is in {shown})")
}

/// The log of the daemon's worker for the package in `project`.
fn worker_log(project: &Path) -> Option<PathBuf> {
    let Ok(Some(Reply::Status(status))) = client::ask(Request::Status, Start::Never) else {
        return None;
    };
    status.workers.into_iter().find(|it| it.project == project).map(|it| it.log)
}

/// `name` with the `[0123456789abcdef]` after each crate's name, and the `blaze.<hash>.` of
/// a symbol, taken out.
pub fn without_hashes(name: &str) -> String {
    let is_hash = |it: &str| it.len() == 16 && it.bytes().all(|it| it.is_ascii_hexdigit());
    let mut name = name.to_owned();
    while let Some(at) = name.find("blaze.")
        && let Some(hash) = name.get(at + 7..at + 23)
        && is_hash(hash)
        && name[at + 23..].starts_with('.')
    {
        name.replace_range(at..at + 24, "");
    }
    let mut out = String::with_capacity(name.len());
    let mut rest = name.as_str();
    while let Some(at) = rest.find('[') {
        let hash = rest[at + 1..].split_once(']').map(|(it, _)| it);
        out.push_str(&rest[..at]);
        match hash {
            Some(hash) if is_hash(hash) => {
                rest = &rest[at + hash.len() + 2..];
            }
            _ => {
                out.push('[');
                rest = &rest[at + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Runs the program `asked` names, as `cargo run` does.
fn ran(asked: &Invocation, cwd: &Path, programs: &[Program]) -> anyhow::Result<i32> {
    let named = asked.flags.bin.as_ref().or(asked.flags.example.as_ref());
    let mut chosen: Vec<&Program> =
        programs.iter().filter(|it| named.is_none_or(|name| it.name == *name)).collect();
    if chosen.len() > 1 {
        let default = |it: &&Program| it.package.default_run.as_deref() == Some(it.name.as_str());
        chosen.retain(default);
    }
    let [program] = chosen[..] else {
        let names: Vec<&str> = programs.iter().map(|it| it.name.as_str()).collect();
        say::error(&format!(
            "`cargo run` could not determine which binary to run. Use the `--bin` option to specify a binary, \
             or the `default-run` manifest key.\navailable binaries: {}",
            names.join(", ")
        ));
        return Ok(FAILED);
    };
    let shown =
        program.executable.strip_prefix(cwd).unwrap_or(&program.executable).display().to_string();
    let target = asked.selection().target;
    let runner = match &target {
        Some(triple) => runner(asked, cwd, triple)?,
        None => None,
    };
    let mut command = match runner {
        Some(mut runner) => {
            runner.arg(&program.executable);
            runner
        }
        None if target.as_deref().is_some_and(is_wasm) => {
            let triple = target.unwrap_or_default();
            let variable =
                format!("CARGO_TARGET_{}_RUNNER", triple.replace('-', "_").to_uppercase());
            say::error(&format!(
                "`{shown}` is a wasm module, which runs in a runtime: name one with `target.{triple}.runner` in the configuration or `{variable}` (`wasmtime run`, say), as cargo runs it"
            ));
            return Ok(FAILED);
        }
        None => Command::new(&program.executable),
    };
    let args = asked.program_args().map(|it| format!(" {it}")).collect::<String>();
    let ran = std::iter::once(command.get_program()).chain(command.get_args());
    let ran: Vec<String> = ran.map(|it| it.to_string_lossy().into_owned()).collect();
    let ran = match ran.len() {
        1 => shown.clone(),
        _ => {
            format!("{} {shown}", shlex::try_join(ran[..ran.len() - 1].iter().map(String::as_str))?)
        }
    };
    say::status(asked.flags.quiet, "Running", &format!("`{ran}{args}`"));
    for it in configured_env(cwd) {
        if it.force || std::env::var_os(&it.name).is_none() {
            command.env(&it.name, &it.value);
        }
    }
    command.args(asked.program_args()).envs(run_env(&program.package, &[]));
    command.envs(library_path(&program.executable, None));
    let status = command.status().with_context(|| format!("could not execute {shown}"))?;
    Ok(run::exit_code(status))
}

/// The programs `asked` builds, each executable root of the build, as blaze plans it with its
/// own resolution; of `cargo test` (`tests`), the tests it runs and what it builds beside.
fn planned(
    asked: &Invocation,
    cwd: &Path,
    metadata: &Metadata,
    tests: Option<blaze_metadata::Tests<'_>>,
) -> anyhow::Result<Vec<Program>> {
    let manifest = asked.flags.manifest_path.as_ref().map(|it| cwd.join(it));
    let dir = manifest.as_deref().and_then(Path::parent).unwrap_or(cwd);
    let selection = asked.selection();
    let named = blaze_metadata::Targets {
        bin: asked.flags.bin.as_deref(),
        example: asked.flags.example.as_deref(),
        lib: tests.is_none() && asked.typed("--lib"),
        test: tests,
    };
    let target = metadata.target_directory.as_std_path();
    let kept_in = target.join("blaze/plans");
    let planned = super::metadata::programs(&selection, dir, manifest.as_deref(), named, &kept_in)?;
    let profile = profile_dir(&selection);
    let out = programs_dir(asked, metadata);
    // A test is in the `deps` of cargo's build directory (`build.build-dir`), where stable
    // cargo puts it, also where nightly's second layout would put it in its unit's directory:
    // a test that finds its package's programs from its own path (`current_exe()`'s
    // grandparent, the `executable-path` crate, assert_cmd's `cargo_bin`) finds them only
    // from `deps`.
    let build_dir = metadata.build_directory.as_ref().map_or(target, |it| it.as_std_path());
    super::layout::make_tagged(target);
    super::layout::make_tagged(build_dir);
    let mut built = build_dir.to_owned();
    if let Some(triple) = &selection.target {
        built.push(triple);
    }
    built.push(profile);
    let mut programs = Vec::new();
    for program in planned {
        let Some(package) = metadata.packages.iter().find(|it| it.id.repr == program.package)
        else {
            anyhow::bail!(
                "the plan of the build names {}, which the workspace does not",
                program.package
            );
        };
        let named = FileNames::of(selection.target.as_deref());
        let exe = |name: &str| format!("{name}{}", named.exe_suffix);
        let kind = format!("{:?}", program.kind).to_lowercase();
        // A library's file is named for its crate, as rustc names it.
        let library = |prefix: &str, suffix: &str| {
            format!("{prefix}{}{suffix}", program.name.replace('-', "_"))
        };
        let executable = match (program.kind, program.test) {
            (Kind::Example, _) => out.join("examples").join(exe(&program.name)),
            (Kind::Cdylib | Kind::Dylib, _) => {
                out.join(library(named.dll_prefix, named.dll_suffix))
            }
            (Kind::Staticlib, _) => out.join(match cfg!(windows) {
                true => library("", ".lib"),
                false => library("lib", ".a"),
            }),
            (_, false) => out.join(exe(&program.name)),
            // Cargo names a test by its target and a hash of the unit; blaze by a hash of
            // what tells its tests apart.
            (_, true) => {
                let hash = test_hash(&package.id.repr, &kind, &program.name, &built);
                built.join("deps").join(exe(&format!("{}-{hash}", program.name)))
            }
        };
        // A test's directory has the file that says it starts in the harness, and a build of
        // `cargo test` is another worker's than that of `cargo build`.
        let dir = match tests {
            Some(_) => {
                let test = if program.test { "-test" } else { "" };
                let named = format!("{}-{kind}-{}{test}", package.name, program.name);
                target.join("blaze/whole-test").join(named)
            }
            None => target.join("blaze/whole"),
        };
        programs.push(Program {
            executable,
            dir,
            name: program.name,
            root: program.root,
            package: package.clone(),
            kind: program.kind,
            test: program.test,
            harness: program.harness,
        });
    }
    Ok(programs)
}

/// cargo's directory of the profile of `selection`: `dev`'s is `debug`.
fn profile_dir(selection: &Selection) -> &str {
    match selection.profile.as_str() {
        "dev" => "debug",
        other => other,
    }
}

/// Where cargo puts the programs of the build `asked` that are not tests: the directory of its
/// profile in the target directory, in that of the target when one is named.
fn programs_dir(asked: &Invocation, metadata: &Metadata) -> PathBuf {
    let selection = asked.selection();
    let mut out = metadata.target_directory.as_std_path().to_owned();
    if let Some(triple) = &selection.target {
        out.push(triple);
    }
    out.push(profile_dir(&selection));
    out
}

/// Sixteen hexadecimal digits, as cargo names a unit by, of the test `name` of `kind` in
/// `package` built in the profile directory `profile`.
fn test_hash(package: &str, kind: &str, name: &str, profile: &Path) -> String {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = rustc_hash::FxHasher::default();
    (package, kind, name, profile).hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// How rustc names the files of the programs it links for a target (its spec's
/// `exe-suffix`, `dll-prefix` and `dll-suffix`), which cargo puts as they are.
struct FileNames {
    exe_suffix: &'static str,
    dll_prefix: &'static str,
    dll_suffix: &'static str,
}

impl FileNames {
    /// Of the target `triple`, the host when it is none.
    fn of(triple: Option<&str>) -> FileNames {
        match triple {
            Some(triple) if is_wasm(triple) => {
                FileNames { exe_suffix: ".wasm", dll_prefix: "", dll_suffix: ".wasm" }
            }
            _ => FileNames {
                exe_suffix: std::env::consts::EXE_SUFFIX,
                dll_prefix: std::env::consts::DLL_PREFIX,
                dll_suffix: std::env::consts::DLL_SUFFIX,
            },
        }
    }
}

/// Whether `triple` is a WebAssembly target, whose programs run in a runtime.
fn is_wasm(triple: &str) -> bool {
    triple.starts_with("wasm32-") || triple.starts_with("wasm64-")
}

/// The program `target.<triple>.runner` (or a `target.'cfg(..)'.runner` that matches, or
/// `CARGO_TARGET_<TRIPLE>_RUNNER`) names to run what is built for `triple`, and its
/// arguments, as cargo finds it for `cargo run`.
fn runner(asked: &Invocation, cwd: &Path, triple: &str) -> anyhow::Result<Option<Command>> {
    use blaze_manifest::config::CompileKind;
    let config = blaze_manifest::Config::load(cwd, std::env::vars(), &asked.flags.config)?;
    let host = super::own::manifest::host_triple()?;
    let cfg = super::own::manifest::target_cfg(triple)?;
    let kind = CompileKind::Target(triple.to_owned());
    let Some((program, args)) = config.target_runner(&kind, &host, &cfg)? else {
        return Ok(None);
    };
    let mut command = Command::new(program);
    command.args(args);
    Ok(Some(command))
}
