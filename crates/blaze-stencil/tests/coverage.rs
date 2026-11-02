//! The stencil library's coverage over real code: every function of a workspace's members,
//! its build scripts and proc macros, and the standard library, reached as a build reaches
//! them, lowered to the IR copy-and-patch compiles and surveyed ([`blaze_stencil::survey`]).
//! The table it writes lists each opcode and type no stencil writes, with how many
//! instructions and functions hit it, then every other refusal. It fails while the table has
//! a row: a gap is a build error, never another backend.
//!
//! `BLAZE_STENCIL_COVERAGE` is one JSON value, inline or a path to a file holding it:
//!
//! ```json
//! {"projects": [{"dir": "/abs/blaze", "cargo_home": "/abs/cargo-home"}],
//!  "out": "/abs/coverage.md"}
//! ```
//!
//! Each project is built once with `blaze build` first, so its build scripts have run (the
//! test reads each run's `output` and `OUT_DIR` from blaze's build directory), and its registry
//! packages are fetched into `cargo_home` (`cargo fetch --locked`). `"unbuilt": true` on a
//! project lowers it without its build scripts' runs instead: crates that read them lower
//! without their generated code and cfgs, and the table says so. Run it with `--release`: the
//! front end lowers every function of the workspace and std. It prints one line per crate and
//! rewrites `out` after each, so a run that dies still leaves its numbers. The test is
//! `#[ignore]`d, as it reads the machine's checkouts: the harness runs it with `--ignored`.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt::Write as _,
    path::Path,
    sync::{Arc, OnceLock},
    time::Instant,
};

use base_db::{Crate, Db, FileId};
use blaze_codegen::{
    Backend, Instance as UnitInstance, InstanceKey, Unit,
    backend::Frontend as _,
    driver::Build,
    memo::Memo,
    plan::{Mode as PlanMode, OptLevel, Origin as PlanOrigin, Override, Package, Policy, Profile},
    settings::{DebugInfo, Panic, Pgo, Relocation, Sanitize, Spec},
};
use blaze_ids::ContentId;
use blaze_lower::{Lower, Lowered, Ran};
use blaze_query::{Config, Database, MemTree};
use blaze_units::{
    query::{self, BuildConfig, Command, FetchKind, Invocation, Mount, Origin, Plan, Planned, Units},
    schema::Mode,
    stdlib,
};
use blaze_vfs::View;
use cranelift_codegen::ir;
use hir_ty_codegen::v2::{
    Backends, BuildTime, Databases, HashSource, HirFrontend, HirInstance, ItemHashes, Lowering, Runs, lowered,
};
use serde::Deserialize;

const LIBRARY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../library");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    projects: Vec<Project>,
    /// Where the table is written, besides stderr.
    out: Option<String>,
    /// Print each function before it is lowered and surveyed: where a crash was.
    #[serde(default)]
    trace: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Project {
    /// The workspace's directory, absolute.
    dir: String,
    /// Where its registry packages were fetched.
    cargo_home: String,
    /// Lower it without its build scripts' runs, for a workspace blaze has not built.
    #[serde(default)]
    unbuilt: bool,
}

#[test]
#[ignore = "reads the projects' checkouts: run with --ignored and BLAZE_STENCIL_COVERAGE set"]
fn every_function_of_the_projects_and_std_compiles_by_copy_and_patch() {
    // The front end recurses as deep as the code it lowers: a test thread's stack is too small
    // for every body of std and a workspace.
    std::thread::Builder::new()
        .name("coverage".to_owned())
        .stack_size(1 << 30)
        .spawn(coverage)
        .expect("the coverage thread")
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

fn coverage() {
    let input = std::env::var("BLAZE_STENCIL_COVERAGE")
        .expect("BLAZE_STENCIL_COVERAGE names the projects: see the head of crates/blaze-stencil/tests/coverage.rs");
    let text = if input.trim_start().starts_with('{') {
        input
    } else {
        std::fs::read_to_string(&input).unwrap_or_else(|error| panic!("BLAZE_STENCIL_COVERAGE: {input}: {error}"))
    };
    let input: Input = serde_json::from_str(&text).unwrap_or_else(|error| panic!("BLAZE_STENCIL_COVERAGE: {error}"));
    let (policy, stencils) = policy().unwrap_or_else(|error| panic!("{error}"));
    let mut tally = Tally::default();
    let out = input.out.as_deref();
    tally.notes.push(
        "std is planned as shipped (opt-level 3, its MIR inlined) and compiled by copy-and-patch; \
         \"a relocation at a reference of a body built in\" is a reference inside an inlined body, \
         which should go once std is planned at opt-level 0 (codegen queue 01466)"
            .to_owned(),
    );
    for project in &input.projects {
        if project.unbuilt {
            tally.notes.push(format!("{}: lowered without its build scripts' runs", project.dir));
        }
        let (lowered, units) = lower(project).unwrap_or_else(|error| panic!("{error}"));
        let crates = crates(&units, &lowered);
        eprintln!("{}: {} crates to survey", project.dir, crates.len());
        survey(lowered.db(), lowered.paths(), &crates, &policy, &stencils, &mut tally, out, input.trace)
            .unwrap_or_else(|error| panic!("{}: {error}", project.dir));
    }
    let table = tally.table();
    eprintln!("{table}");
    write_out(out, &table);
    assert!(tally.is_clean(), "functions copy-and-patch does not compile: the table is above");
}

/// What the survey found, over every project.
#[derive(Default)]
struct Tally {
    functions: usize,
    compiled: usize,
    /// Each opcode and type no stencil writes: instructions, functions, a function's name.
    gaps: BTreeMap<(String, String), Row>,
    /// Every other refusal past lowering, by its message with the instruction left out.
    refused: BTreeMap<String, Row>,
    /// What `lowered` refuses before there is IR (`asm!`, naked functions), by its kind.
    unlowered: BTreeMap<String, Row>,
    /// Roots the front end can't reach from (a body that fails to lower), and crates whose
    /// survey panicked, by the message: not the stencil library's, but not surveyed.
    unreached: BTreeMap<String, Row>,
    /// Said above the table: what the run left out.
    notes: Vec<String>,
}

#[derive(Default)]
struct Row {
    instructions: usize,
    functions: usize,
    example: String,
}

impl Row {
    fn hit(&mut self, instructions: usize, name: &str) {
        self.instructions += instructions;
        self.functions += 1;
        if self.example.is_empty() {
            name.clone_into(&mut self.example);
        }
    }
}

impl Tally {
    fn is_clean(&self) -> bool {
        self.gaps.is_empty() && self.refused.is_empty() && self.unlowered.is_empty() && self.unreached.is_empty()
    }

    fn function(&mut self, name: &str, errors: &[blaze_stencil::Error]) {
        self.functions += 1;
        if errors.is_empty() {
            self.compiled += 1;
            return;
        }
        let mut gaps: HashMap<(String, String), usize> = HashMap::new();
        let mut refused: HashMap<String, usize> = HashMap::new();
        for error in errors {
            match error.gap {
                Some(gap) => *gaps.entry((gap.opcode.to_string(), ty_name(gap.ty))).or_default() += 1,
                None => *refused.entry(error.what.clone()).or_default() += 1,
            }
        }
        for (key, count) in gaps {
            let row = self.gaps.entry(key).or_insert_with_key(|key| {
                eprintln!("  gap: `{}` of {} in `{name}`", key.0, key.1);
                Row::default()
            });
            row.hit(count, name);
        }
        for (key, count) in refused {
            self.refused.entry(key).or_default().hit(count, name);
        }
    }

    fn unlowered(&mut self, name: &str, what: &str) {
        self.functions += 1;
        let kind = if what.contains("`asm!`") {
            "a body with `asm!` statements".to_owned()
        } else if what.contains("naked") {
            "a naked function".to_owned()
        } else {
            message(what)
        };
        self.unlowered.entry(kind).or_default().hit(1, name);
    }

    fn unreached(&mut self, name: &str, what: &str) {
        self.unreached.entry(message(what)).or_default().hit(1, name);
    }

    /// The report's table, most functions first.
    fn table(&self) -> String {
        let mut out = String::new();
        for note in &self.notes {
            let _ = writeln!(out, "Note: {note}.");
        }
        let _ = writeln!(
            out,
            "{} functions surveyed, {} compile by copy-and-patch, {} do not.\n",
            self.functions,
            self.compiled,
            self.functions - self.compiled
        );
        let _ = writeln!(out, "| opcode | type | functions | instructions | e.g. |\n|---|---|---:|---:|---|");
        for ((opcode, ty), row) in sorted(&self.gaps) {
            let _ = writeln!(out, "| `{opcode}` | {ty} | {} | {} | `{}` |", row.functions, row.instructions, row.example);
        }
        if !self.refused.is_empty() {
            let _ = writeln!(out, "\n| other refusal | functions | e.g. |\n|---|---:|---|");
            for (what, row) in sorted(&self.refused) {
                let _ = writeln!(out, "| {what} | {} | `{}` |", row.functions, row.example);
            }
        }
        if !self.unlowered.is_empty() {
            let _ = writeln!(out, "\n| not lowered | functions | e.g. |\n|---|---:|---|");
            for (what, row) in sorted(&self.unlowered) {
                let _ = writeln!(out, "| {what} | {} | `{}` |", row.functions, row.example);
            }
        }
        if !self.unreached.is_empty() {
            let _ = writeln!(out, "\n| not reached (front end) | roots | e.g. |\n|---|---:|---|");
            for (what, row) in sorted(&self.unreached) {
                let _ = writeln!(out, "| {what} | {} | `{}` |", row.functions, row.example);
            }
        }
        out
    }
}

/// `what` without the instance a codegen error starts with (`HirInstance(..): `), so one
/// refusal is one row, and with no `|` or newline to break the table.
fn message(what: &str) -> String {
    let what = match what.strip_prefix("HirInstance(") {
        Some(_) => what.split_once("): ").map_or(what, |it| it.1),
        None => what,
    };
    what.replace('|', "\\|").replace('\n', " ")
}

/// `rows`, most functions first, then most instructions.
fn sorted<K>(rows: &BTreeMap<K, Row>) -> Vec<(&K, &Row)> {
    let mut rows: Vec<_> = rows.iter().collect();
    rows.sort_by(|a, b| b.1.functions.cmp(&a.1.functions).then(b.1.instructions.cmp(&a.1.instructions)));
    rows
}

fn ty_name(ty: ir::Type) -> String {
    if ty == ir::types::INVALID { "none".to_owned() } else { ty.to_string() }
}

/// The policy: a dev build of the host with static stencil code, whose units [`tier_spec`]
/// makes copy-and-patch's. Static, as `lowered` hands out only static IR; position-independent
/// code differs in its glue, not in its opcodes.
fn policy() -> Result<(Policy, blaze_stencil::Target), String> {
    let target = hir_ty_codegen::Target::selected()?;
    let mut stencils = target.stencil().ok_or_else(|| format!("{} has no stencils", target.triple()))?;
    stencils.pic = false;
    let library = blaze_stencil::library_id(&stencils).map_err(|error| error.to_string())?;
    let mut toolchain = hir_ty_codegen::v2::toolchain(hir_ty_codegen::Target::host(), &[], Runs::Here)?;
    hir_ty_codegen::v2::select(&toolchain)?;
    toolchain.relocation = Relocation::Static;
    toolchain.stencils = Some(blaze_format::hex::encode(&library));
    // Std's shipped layer is opt-level 3, which a plan refuses in a blaze without LLVM; every
    // unit here is compiled by copy-and-patch whatever its level (`tier_spec`), so the plan
    // is told of an LLVM it never reaches.
    toolchain.llvm.get_or_insert_with(|| "none: the stencil survey compiles every unit by copy-and-patch".to_owned());
    let policy = Policy {
        toolchain,
        profile: Profile {
            opt_level: OptLevel::O0,
            overflow_checks: true,
            debug_assertions: true,
            debuginfo: DebugInfo::None,
            panic: Panic::Unwind,
            packages: Default::default(),
            build_override: Override::default(),
        },
        mode: PlanMode::Output,
        fold: false,
        sanitize: Sanitize::default(),
        pgo: Pgo::None,
    };
    Ok((policy, stencils))
}

fn tier_spec(mut spec: Spec) -> Spec {
    spec.backend = Backend::Stencil;
    spec.counted = false;
    spec.fold = false;
    spec.debuginfo = DebugInfo::None;
    spec.pgo = Pgo::None;
    spec.toolchain.relocation = Relocation::Static;
    spec
}

/// Every crate to survey, once each, with the origin its code is asked as: the members'
/// crates of every kind (libraries, binaries, tests, build scripts, proc macros) and the
/// standard library's.
fn crates(units: &Units, lowered: &Lowered) -> Vec<(Crate, PlanOrigin, String)> {
    let members: HashMap<&str, &str> =
        units.packages.iter().filter(|it| it.is_member).map(|it| (it.id.as_str(), it.name.as_str())).collect();
    let mut out: Vec<(Crate, PlanOrigin, String)> = Vec::new();
    for (unit, it) in units.graph.units.iter().enumerate() {
        if it.mode == Mode::RunCustomBuild {
            continue;
        }
        let origin = if it.is_std {
            PlanOrigin::Std
        } else if let Some(name) = members.get(it.pkg_id.as_str()) {
            PlanOrigin::Member(Package::new(name))
        } else {
            continue;
        };
        let Some(krate) = lowered.krate(unit) else { continue };
        if !out.iter().any(|(seen, ..)| *seen == krate) {
            out.push((krate, origin, it.target.name.clone()));
        }
    }
    out.sort_by_key(|it| !matches!(it.1, PlanOrigin::Std));
    out
}

/// Surveys every function `crates` reach into `tally`, a crate at a time (std's first), each
/// instance once: a line per crate on stderr, and the table so far written to `out`.
fn survey(
    db: &Db,
    paths: &Arc<HashMap<FileId, String>>,
    crates: &[(Crate, PlanOrigin, String)],
    policy: &Policy,
    stencils: &blaze_stencil::Target,
    tally: &mut Tally,
    out: Option<&str>,
    trace: bool,
) -> Result<(), String> {
    let names = Arc::clone(paths);
    let hashes = ItemHashes::new(Box::new(move |file: FileId| {
        Some(names.get(&file).cloned().unwrap_or_else(|| format!("file{}.rs", file.index())))
    }));
    let build_time = Arc::new(BuildTime(Default::default()));
    let frontend = HirFrontend::new(Databases::new(db), HashSource::Items(hashes), build_time).with_policy(policy);
    let backends = Backends::default();
    let memo = Memo::new();
    let requester = requester(crates);
    let mut seen: HashSet<InstanceKey> = HashSet::new();
    let started = Instant::now();
    for (n, (krate, origin, name)) in crates.iter().enumerate() {
        let (before, compiled) = (tally.functions, tally.compiled);
        eprintln!("[{}/{}] {name}: surveying", n + 1, crates.len());
        let surveyed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            survey_crate(&frontend, &backends, &memo, policy, stencils, &requester, *krate, *origin, &mut seen, tally, trace)
        }));
        let (roots, reached) = match surveyed {
            Ok(Ok(counts)) => counts,
            Ok(Err(error)) => return Err(format!("`{name}`: {error}")),
            Err(panic) => {
                let what = panic
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("a panic");
                tally.unreached(name, &format!("the survey of the crate panicked: {what}"));
                (0, 0)
            }
        };
        eprintln!(
            "[{}/{}] {name}: {} roots, {} reached, {} new, {} compile; {} gaps so far; {:.0?}",
            n + 1,
            crates.len(),
            roots,
            reached,
            tally.functions - before,
            tally.compiled - compiled,
            tally.gaps.len(),
            started.elapsed()
        );
        write_out(out, &tally.table());
    }
    Ok(())
}

/// Surveys what `krate`'s functions reach and `seen` has not, into `tally`: how many roots and
/// reached functions there were. A crate whose reach fails is reached a root at a time, each
/// root that fails a row of [`Tally::unreached`].
#[expect(clippy::too_many_arguments, reason = "the survey's state, passed down once a crate")]
fn survey_crate(
    frontend: &HirFrontend<'_>,
    backends: &Backends,
    memo: &Memo,
    policy: &Policy,
    stencils: &blaze_stencil::Target,
    requester: &PlanOrigin,
    krate: Crate,
    origin: PlanOrigin,
    seen: &mut HashSet<InstanceKey>,
    tally: &mut Tally,
    trace: bool,
) -> Result<(usize, usize), String> {
    let roots: Vec<(HirInstance, PlanOrigin)> = frontend.every_function(krate).into_iter().map(|it| (it, origin)).collect();
    let build = Build::new(backends, frontend, policy, memo);
    let reached = match build.reach(&roots, &[]) {
        Ok(reached) => vec![reached],
        Err(_) => {
            let mut each = Vec::new();
            for root in &roots {
                match build.reach(std::slice::from_ref(root), &[]) {
                    Ok(reached) => each.push(reached),
                    Err(errors) => {
                        let name = frontend.describe(&root.0, requester).map_or_else(|_| format!("{:?}", root.0), |it| it.name);
                        let what = errors.first().map_or("no error given", |it| it.what.as_str());
                        tally.unreached(&name, what);
                    }
                }
            }
            each
        }
    };
    let mut count = 0;
    for reached in &reached {
        count += reached.functions.len();
        for ((instance, unit), refs) in reached.functions.iter().zip(&reached.refs) {
            if !seen.insert(unit.instance.key()) {
                continue;
            }
            let described = match frontend.describe(instance, requester) {
                Ok(described) => described,
                Err(error) => {
                    tally.unreached(&format!("{instance:?}"), &error.what);
                    continue;
                }
            };
            let spec = match policy.choose(&described.origin, described.overflow_checks) {
                Ok(choice) => tier_spec(choice.first),
                Err(why) => {
                    tally.unreached(&described.name, &why.to_string());
                    continue;
                }
            };
            let unit = Unit {
                instance: UnitInstance { settings: spec.settings(), ..unit.instance },
                backend: Backend::Stencil,
                ..unit.clone()
            };
            if trace {
                eprintln!("  {}", described.name);
            }
            match lowered(frontend, &unit, &spec, instance, refs) {
                Ok(Lowering::Ir { func, .. }) => {
                    let errors = blaze_stencil::survey(&func, stencils).map_err(|error| error.to_string())?;
                    tally.function(&described.name, &errors);
                }
                Ok(Lowering::Asm(_) | Lowering::Allocator(_)) => {}
                Err(error) => tally.unlowered(&described.name, &error.what),
            }
        }
    }
    Ok((roots.len(), count))
}

fn write_out(out: Option<&str>, table: &str) {
    if let Some(out) = out {
        std::fs::write(out, table).unwrap_or_else(|error| panic!("{out}: {error}"));
    }
}

/// Whose code asks for each instance: a member's when there is one, as a generic is compiled
/// as a member of a dev build asks for it.
fn requester(crates: &[(Crate, PlanOrigin, String)]) -> PlanOrigin {
    crates.iter().map(|it| it.1).find(|it| matches!(it, PlanOrigin::Member(_))).unwrap_or(PlanOrigin::Std)
}

/// Every root the test reads, in one tree.
fn tree() -> &'static Arc<MemTree> {
    static TREE: OnceLock<Arc<MemTree>> = OnceLock::new();
    TREE.get_or_init(|| Arc::new(MemTree::default()))
}

struct Library {
    root: ContentId,
    commit: String,
}

/// This checkout's `library/` as the daemon mounts it.
fn library() -> Result<&'static Library, String> {
    static READ: OnceLock<Result<Library, String>> = OnceLock::new();
    let read = READ.get_or_init(|| {
        let dir = Path::new(LIBRARY);
        let upstream = std::fs::read_to_string(dir.join("UPSTREAM"))
            .map_err(|error| format!("{}/UPSTREAM: {error}", dir.display()))?;
        let commit = stdlib::library_commit(&upstream)?;
        let files = read_tree(dir, &stdlib::in_library, &[])?;
        Ok(Library { root: root_of(&files), commit })
    });
    read.as_ref().map_err(Clone::clone)
}

/// Every file under `dir` that `keep` keeps, by its `/`-separated path, but under `.git`, a
/// top-level `target*` directory (build output: `target`, `target-cargo`) and `skipped`.
fn read_tree(dir: &Path, keep: &dyn Fn(&str) -> bool, skipped: &[&str]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    let mut dirs = vec![(dir.to_path_buf(), String::new())];
    while let Some((at, under)) = dirs.pop() {
        let entries = std::fs::read_dir(&at).map_err(|error| format!("{}: {error}", at.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("{}: {error}", at.display()))?;
            let name = entry.file_name().into_string().map_err(|it| format!("{}: {it:?} is not UTF-8", at.display()))?;
            let path = if under.is_empty() { name.clone() } else { format!("{under}/{name}") };
            let metadata = std::fs::metadata(entry.path()).map_err(|error| format!("{}: {error}", entry.path().display()))?;
            if metadata.is_dir() {
                let output = under.is_empty() && name.starts_with("target");
                if name != ".git" && !output && !skipped.contains(&path.as_str()) {
                    dirs.push((entry.path(), path));
                }
            } else if keep(&path) {
                let bytes = std::fs::read(entry.path()).map_err(|error| format!("{}: {error}", entry.path().display()))?;
                files.push((path, bytes));
            }
        }
    }
    Ok(files)
}

fn root_of(files: &[(String, Vec<u8>)]) -> ContentId {
    let files: Vec<(&str, &[u8])> = files.iter().map(|(path, bytes)| (path.as_str(), bytes.as_slice())).collect();
    tree().root(&files)
}

fn host() -> Result<&'static str, String> {
    let musl = cfg!(target_env = "musl");
    match (std::env::consts::ARCH, std::env::consts::OS, musl) {
        ("x86_64", "linux", false) => Ok("x86_64-unknown-linux-gnu"),
        ("x86_64", "linux", true) => Ok("x86_64-unknown-linux-musl"),
        ("aarch64", "linux", false) => Ok("aarch64-unknown-linux-gnu"),
        ("aarch64", "linux", true) => Ok("aarch64-unknown-linux-musl"),
        ("aarch64", "macos", _) => Ok("aarch64-apple-darwin"),
        ("x86_64", "macos", _) => Ok("x86_64-apple-darwin"),
        (arch, os, _) => Err(format!("the coverage test knows no triple of {arch} {os}")),
    }
}

/// The units of the workspace at `dir` as `blaze build` plans them, each package it needs
/// mounted from where cargo fetched it.
fn plan(db: &Database, dir: &str, view: ContentId, cargo_home: &str) -> Result<(Units, BuildConfig), String> {
    let library = library()?;
    let home = std::env::var("HOME").map_err(|_| "HOME is not set, and cargo reads it".to_owned())?;
    let mut env = vec![("CARGO_HOME".to_owned(), cargo_home.to_owned()), ("HOME".to_owned(), home)];
    env.sort();
    let config_toml = format!("{cargo_home}/config.toml");
    let files = match std::fs::read(&config_toml) {
        Ok(bytes) => vec![(config_toml, bytes)],
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(format!("{config_toml}: {error}")),
    };
    let mut mounts = vec![
        Mount { at: dir.to_owned(), origin: Origin::View, under: String::new() },
        stdlib::library_mount(library.root, &library.commit),
    ];
    let mut fetched: Vec<String> = Vec::new();
    loop {
        mounts.sort();
        let config = BuildConfig {
            invocation: Invocation {
                command: Command::Build,
                args: Vec::new(),
                root: dir.to_owned(),
                cwd: dir.to_owned(),
                env: env.clone(),
                files: files.clone(),
            },
            mounts: mounts.clone(),
            host: host()?.to_owned(),
            library: stdlib::library_at(&library.commit),
            std: "/s/std".to_owned(),
        };
        let opened = db.open_with(View { root: view }, &config.encode());
        let planned = db.pin().get::<Plan>(opened, &()).map_err(|error| format!("planning {dir}: {error:?}"))?.get();
        let needs = match planned {
            Planned::Ready(units) => return Ok((units, config)),
            Planned::Failed(error) => return Err(format!("planning {dir}: {}", error.what)),
            Planned::Needs(needs) => needs,
        };
        for fetch in needs {
            if fetched.contains(&fetch.at) {
                return Err(format!("planning {dir} needs {} again once it is mounted at {}", fetch.what, fetch.at));
            }
            if matches!(fetch.kind, FetchKind::IndexFile) {
                return Err(format!("planning {dir} reads the index file {}: its Cargo.lock does not pin {}", fetch.url, fetch.what));
            }
            let at = Path::new(&fetch.at);
            if !at.is_dir() {
                return Err(format!(
                    "{} is not at {}: `cargo fetch --locked --manifest-path {dir}/Cargo.toml` with CARGO_HOME={cargo_home} first",
                    fetch.what, fetch.at
                ));
            }
            let files = read_tree(at, &|_: &str| true, &[])?;
            mounts.push(Mount { at: fetch.at.clone(), origin: Origin::Root(root_of(&files)), under: String::new() });
            fetched.push(fetch.at);
        }
    }
}

/// The run of build-script unit `unit` as `blaze build` left it: `output` beside its
/// `OUT_DIR`, and the files in `OUT_DIR`.
fn ran(units: &Units, unit: usize) -> Result<Ran, String> {
    let out_dir = units.plans[unit].out_dir.clone().ok_or_else(|| format!("build script run {unit} has no OUT_DIR"))?;
    let output_at = Path::new(&out_dir).parent().map(|it| it.join("output")).ok_or_else(|| format!("{out_dir} has no parent"))?;
    let output = std::fs::read(&output_at).map_err(|error| {
        format!("no build script's run at {}: {error}; build the project with `blaze build` first", output_at.display())
    })?;
    let mut ran = Ran { out_dir: out_dir.clone(), ..Ran::default() };
    for line in String::from_utf8_lossy(&output).lines() {
        let Some(directive) = line.strip_prefix("cargo::").or_else(|| line.strip_prefix("cargo:")) else { continue };
        let Some((key, value)) = directive.split_once('=') else { continue };
        match key {
            "rustc-cfg" => ran.cfgs.push(value.to_owned()),
            "rustc-env" => {
                let (name, value) = value
                    .split_once('=')
                    .ok_or_else(|| format!("{}: `{line}` sets no variable", output_at.display()))?;
                ran.env.push((name.to_owned(), value.to_owned()));
            }
            _ => {}
        }
    }
    ran.files = read_tree(Path::new(&out_dir), &|_: &str| true, &[])?;
    Ok(ran)
}

/// The workspace at `dir` lowered as a build lowers it, each build script's run and each
/// proc-macro crate's macros applied.
fn lower(project: &Project) -> Result<(Lowered, Units), String> {
    let (dir, cargo_home) = (project.dir.as_str(), project.cargo_home.as_str());
    let tree = tree();
    let started = Instant::now();
    let files = read_tree(Path::new(dir), &|_: &str| true, &[])?;
    let bytes: usize = files.iter().map(|it| it.1.len()).sum();
    eprintln!("{dir}: read {} files, {} MiB, in {:.0?}", files.len(), bytes >> 20, started.elapsed());
    let view = root_of(&files);
    let mut db = Database::new(Config { tree: Some(tree.clone()), dir: None, tags: 0 });
    query::register(&mut db);
    let (units, config) = plan(&db, dir, view, cargo_home)?;
    let lower = Lower { units: &units, config: &config, files: &**tree, root: view, host_cpu: None };
    let mut lowered = blaze_lower::lower_into(Db::logged(), lower).map_err(|error| format!("lowering {dir}: {error}"))?;
    eprintln!("{dir}: planned {} units and lowered them in {:.0?}", units.graph.units.len(), started.elapsed());
    for (unit, it) in units.graph.units.iter().enumerate() {
        if it.mode == Mode::RunCustomBuild && !project.unbuilt {
            let run = ran(&units, unit)?;
            lowered.ran(unit, &run).map_err(|error| format!("applying build script run {unit} of {dir}: {error}"))?;
        }
    }
    let toolchain = ContentId::of("blaze v2 stencil coverage toolchain 1", library()?.commit.as_bytes());
    for unit in 0..units.graph.units.len() {
        if !lowered.crates().is_proc_macro(unit) {
            continue;
        }
        let name = &units.graph.units[unit].target.name;
        let krate = lowered.krate(unit).ok_or_else(|| format!("proc macro `{name}` has no crate"))?;
        let macros = blaze_macro_eval::load(lowered.db(), krate, toolchain);
        lowered.macros(unit, macros).map_err(|error| format!("the macros of `{name}`: {error}"))?;
    }
    Ok((lowered, units))
}
