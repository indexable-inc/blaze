//! Spike: keep a workspace loaded in rust-analyzer's database and compile
//! functions to native code that links against rustc-built artifacts.

#![allow(clippy::print_stderr, reason = "a command line program: stderr is where it speaks")]

// The cargo drop-in and what it stands on are blaze-cargo's; here they are the engine's modules
// as they were, `crate::cargo::...`.
use blaze_cargo::{
    RUN_TOOL_TARGET, Told, answers, cargo, dep_info, deps, end_with_reader, errors, lock, memory,
    plan, process, refused_error, remote, renamed, replace, sandbox, socket, stub, trace, tree,
    version, weaken,
};

mod admit;
mod as_rustc;
mod borrowck;
mod check;
mod consteval;
mod content;
mod doc;
mod doctest;
mod events;
mod fns;
mod frozen;
mod inputs;
mod link;
mod live;
mod macro_list;
mod macro_sandbox;
mod manifest;
mod mirdump;
mod outline;
mod pin;
mod shipped;
use blaze_splice as splice;
mod splicer;
mod symbols;
mod test_harness;
mod tier;
mod versions;
mod whole;

/// The daemon holds millions of small allocations and frees a file's worth of them on each
/// edit, which the system allocator is slow at. jemalloc gives the system back more of what
/// a build freed than mimalloc does. It is not built for Windows.
#[cfg(unix)]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(windows)]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use ide_db::base_db::{CrateOrigin, LangCrateOrigin, SourceDatabase as _, TargetKind};
use std::{
    collections::BTreeSet,
    io::{BufRead as _, BufReader, Write as _},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, bail};
use hir::{AsAssocItem, AssocItemContainer, Crate, Module, ModuleDef, db::HirDatabase};
use hir_def::{
    AssocItemId, DefWithBodyId, FunctionId, ItemContainerId, Lookup, ModuleDefId, StaticId,
    expr_store::Body, item_scope::ItemScope,
};
use hir_ty_codegen::{
    Backend, CodegenError, CodegenUnit, ContentId, ParallelWork, Rejections, Roots, SymbolResolver,
    exported_static, exported_symbol, linked_symbol, tier as codegen_tier,
};
use ide_db::{ChangeWithProcMacros, RootDatabase};
use load_cargo::{LoadCargoConfig, ProcMacroServerChoice};
use project_model::{
    CargoConfig, CargoFeatures, CargoWorkspace, ProjectManifest, ProjectWorkspace,
    ProjectWorkspaceKind, RustLibSource, TargetDirectoryConfig, WorkspaceBuildScripts,
};
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{
    AstNode, SyntaxNode,
    ast::{self, HasName},
    match_ast,
};
use vfs::{AbsPathBuf, Vfs, VfsPath};

#[cfg(unix)]
use std::os::unix::net::UnixStream;
// The same sockets, which the standard library does not have on Windows.
#[cfg(windows)]
use uds_windows::UnixStream;

use crate::{
    splice::{Baseline, Linked, Spliced},
    symbols::SymbolTable,
};

#[derive(Clone)]
struct Args {
    project: PathBuf,
    krate: String,
    /// More crates to compile from the same loaded workspace, into the same object. When `out`
    /// is a directory, each crate is a program of its own instead: `out` and `report` get a
    /// file per crate, and a crate that fails gets a `.error` file instead of ending the run.
    batch: Vec<String>,
    /// The root files of the crates the daemon builds without having been asked to: the ones
    /// the crates asked for use, whose source is no longer what rustc compiled.
    widened: Vec<String>,
    out: PathBuf,
    symbol_files: Vec<PathBuf>,
    watch: bool,
    /// A Unix socket to take build requests on, instead of building when a file changes.
    socket: Option<PathBuf>,
    link_cmd: Option<String>,
    /// Where to write the object that amends `out` after an edit, for the link to take ahead of
    /// `out`. Without it every edit writes `out` again.
    patch: Option<PathBuf>,
    /// The executable `link_cmd` writes. With it an edit that fits a patch is added to the
    /// executable where it is, and `link_cmd` only runs when that can't be done.
    splice: Option<PathBuf>,
    /// Tab-separated per-function outcome, for the harness.
    report: Option<PathBuf>,
    /// File of function paths to leave to rustc, for bisecting miscompiles.
    deny: Option<PathBuf>,
    /// Directory that gets one object per compiled function, for testing each in isolation.
    isolate: Option<PathBuf>,
    /// An object of the standard library blaze built before, in a build with nothing of
    /// rustc's: the standard library's crates are not compiled, and what the program calls of
    /// them is called by the names that object defines.
    prebuilt: Option<PathBuf>,
    /// The executable to link `out` into, with the standard library's object and the native
    /// libraries the crates ask for, in a build with nothing of rustc's ([`link::executable`]).
    exe: Option<PathBuf>,
    /// `exe` is a shared library of the crate asked for, as a proc macro's is
    /// (docs/no-rustc.md, step J2), not an executable.
    shared: bool,
    /// `exe` is the archive of a `staticlib`, with `shared`: what the shared library would
    /// have, archived ([`link::archive`]).
    staticlib: bool,
    /// Where to write, after `exe` is linked, the dep-info rustc would have of the crate asked
    /// for: the files of its modules, as cargo reads them to know when to build it again.
    dep_info: Option<PathBuf>,
    /// Where to write, after `exe` is linked, every file the build read for a crate that is
    /// not the toolchain's, one a line, and each variable of this process `env!` read, as
    /// [`cargo::keyed::var`] names it: what a build script's or a proc macro's build is kept
    /// in the store by (`cargo/own_script.rs`).
    inputs: Option<PathBuf>,
    /// The build is of the standard library alone ([`std_unit`]): a crate it names is the
    /// library's, not another of that name the program has too.
    library: bool,
    /// The program is a build script for wasm32-wasip1 whose commands run on the host: its
    /// standard library has blaze's `std::process` ([`run_tool_std`]).
    run_tool: bool,
    /// Load the workspace as `cargo test` builds it: with every crate of its build, each
    /// library's and binary's unit tests a crate of their own with `cfg(test)`.
    cfg_test: bool,
    /// The program is the unit tests of the target `krate` names, its crate with `cfg(test)`,
    /// and not the target. Only of a `cfg_test` load, which has them.
    unit_test: bool,
    /// Where the build of the tests puts its programs and the directory it gives its
    /// integration tests, which those are told as `CARGO_BIN_EXE_<name>` and
    /// `CARGO_TARGET_TMPDIR`.
    executables: Option<AbsPathBuf>,
    scratch: Option<AbsPathBuf>,
    /// Run build scripts and build proc macros in a directory of blaze's own under the
    /// target directory, so that nothing here waits for a cargo that is building there.
    own_target: bool,
    /// What cargo said of the build scripts and proc macros of the build of `project`, a
    /// line each: the workspace is loaded with that, and nothing is run to find it out.
    scripts_from: Option<PathBuf>,
    /// Which build of the package this is: its profile, features, packages and target. The
    /// target is the triple to emit code for, when it is not the host: the workspace is
    /// loaded as cargo builds it for that target, and the objects are of its format and
    /// architecture.
    selection: plan::Selection,
    /// Other checkouts of projects to load into the same database, for requests that name
    /// one as their `--project`. What they have in common with `project` is there once.
    also: Vec<PathBuf>,
    /// What the code has a debugger told: what `profile` asks for.
    debug: plan::Described,
    /// Whether the flags rustc is given cap the lints, so that a lint the source denies may
    /// not fail its build.
    lints_capped: bool,
    /// What the flags rustc is given pass to the linker (`-C link-arg`, `-C link-args`):
    /// the daemon's own link line has them, as rustc's has.
    link_args: Vec<String>,
    /// The root of the workspace `project` is in. Another checkout has crates of the same
    /// names, and the ones to build are the ones below this.
    checkout: PathBuf,
    /// The loaded checkout that `checkout` reads the packages from that are the same in both.
    base: Option<PathBuf>,
    /// The crates of `checkout`. One that is the same in another checkout is of both.
    members: Arc<FxHashSet<ide_db::base_db::Crate>>,
}

impl Args {
    /// What `path` is called in the checkout, when it is a file of it.
    fn inside<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        let base = self.base.as_deref().and_then(|base| path.strip_prefix(base).ok());
        path.strip_prefix(&self.checkout).ok().or(base)
    }

    /// The crates that are built into the program's object, the one asked for first.
    fn crates(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.krate).chain(&self.batch).chain(&self.widened)
    }

    /// Whether the file is one the checkout is built from. A file of the checkout it shares
    /// packages with is, when a crate of this one has it.
    fn owns(&self, db: &RootDatabase, file: vfs::FileId, path: &Path) -> bool {
        path.starts_with(&self.checkout)
            || self.base.as_ref().is_some_and(|base| {
                path.starts_with(base)
                    && ide_db::base_db::relevant_crates(db, file)
                        .iter()
                        .any(|it| self.members.contains(it))
            })
    }

    /// What the paths in cargo's dep-info files are relative to: the root of the workspace
    /// the project is in, where cargo runs rustc. A package below it, as `cargo build -p`
    /// of one member asks for, is not.
    fn root(&self) -> anyhow::Result<PathBuf> {
        let mut root = dunce::canonicalize(&self.project)?;
        if root.is_file() {
            root.pop();
        }
        let declares_workspace = |dir: &Path| {
            let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap_or_default();
            manifest
                .lines()
                .map(str::trim)
                .any(|it| it == "[workspace]" || it.starts_with("[workspace."))
        };
        Ok(root.ancestors().find(|it| declares_workspace(it)).unwrap_or(&root).to_owned())
    }

    fn parse(mut args: impl Iterator<Item = String>) -> anyhow::Result<Args> {
        let mut project = None;
        let mut krate = None;
        let mut batch = Vec::new();
        let mut out = None;
        let mut symbol_files = Vec::new();
        let mut watch = false;
        let mut socket = None;
        let mut link_cmd = None;
        let mut patch = None;
        let mut splice = None;
        let mut report = None;
        let mut deny = None;
        let mut isolate = None;
        let mut prebuilt = None;
        let mut exe = None;
        let mut shared = false;
        let mut staticlib = false;
        let mut run_tool = false;
        let mut dep_info = None;
        let mut inputs = None;
        let mut cfg_test = false;
        let mut unit_test = false;
        let (mut executables, mut scratch) = (None, None);
        let mut dev_deps = false;
        let mut own_target = false;
        let mut scripts_from = None;
        let mut features = Vec::new();
        let mut no_default_features = false;
        let (mut packages, mut workspace) = (Vec::new(), false);
        let mut all_features = false;
        let mut profile = None;
        let mut config = Vec::new();
        let (mut excludes, mut unstable) = (Vec::new(), Vec::new());
        let mut asked_in = None;
        let mut also = Vec::new();
        let mut target = None;
        while let Some(arg) = args.next() {
            let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "--crate" if krate.is_some() => batch.push(value()?),
                "--crate" => krate = Some(value()?),
                "--out" => out = Some(PathBuf::from(value()?)),
                "--symbols" => symbol_files.push(PathBuf::from(value()?)),
                "--symbols-in" => {
                    let listed = value()?;
                    let listed = std::fs::read_to_string(&listed)
                        .with_context(|| format!("reading the symbol files in {listed}"))?;
                    symbol_files.extend(listed.lines().map(PathBuf::from));
                }
                "--link-cmd" => link_cmd = Some(value()?),
                "--patch" => patch = Some(PathBuf::from(value()?)),
                "--binary" | "--splice" => splice = Some(PathBuf::from(value()?)),
                "--report" => report = Some(PathBuf::from(value()?)),
                "--deny" => deny = Some(PathBuf::from(value()?)),
                "--isolate" => isolate = Some(PathBuf::from(value()?)),
                "--prebuilt" => prebuilt = Some(PathBuf::from(value()?)),
                "--exe" => exe = Some(PathBuf::from(value()?)),
                "--shared" => shared = true,
                "--staticlib" => staticlib = true,
                "--run-tool" => run_tool = true,
                "--dep-info" => dep_info = Some(PathBuf::from(value()?)),
                "--inputs" => inputs = Some(PathBuf::from(value()?)),
                "--watch" => watch = true,
                "--socket" => socket = Some(PathBuf::from(value()?)),
                "--cfg-test" => cfg_test = true,
                // A unit test is built of a load that has it.
                "--unit-test" => {
                    cfg_test = true;
                    unit_test = true;
                }
                "--executables" => executables = Some(absolute(&arg, value()?)?),
                "--scratch" => scratch = Some(absolute(&arg, value()?)?),
                "--dev-deps" => dev_deps = true,
                "--own-target" => own_target = true,
                "--scripts-from" => scripts_from = Some(PathBuf::from(value()?)),
                "--also" => also.push(PathBuf::from(value()?)),
                "--target" => target = Some(value()?),
                "--features" => features.extend(value()?.split(',').map(str::to_owned)),
                "--no-default-features" => no_default_features = true,
                "--package" | "-p" => packages.push(value()?),
                "--workspace" => workspace = true,
                "--exclude" => excludes.push(value()?),
                "-Z" => unstable.push(value()?),
                "--all-features" => all_features = true,
                "--release" | "--profile" if profile.is_some() => {
                    bail!("the profile is given twice")
                }
                "--release" => profile = Some("release".to_owned()),
                "--profile" => profile = Some(value()?),
                "--config" => config.push(value()?),
                "--asked-in" => asked_in = Some(PathBuf::from(value()?)),
                _ if project.is_none() => project = Some(PathBuf::from(arg)),
                _ => bail!("unexpected argument {arg}"),
            }
        }
        if shared && exe.is_none() {
            bail!("--shared says what --exe links, and there is no --exe");
        }
        if staticlib && !shared {
            bail!("--staticlib archives what --shared links, and there is no --shared");
        }
        if run_tool && target.as_deref() != Some(RUN_TOOL_TARGET) {
            bail!("--run-tool is for build scripts compiled for --target {RUN_TOOL_TARGET}");
        }
        Ok(Args {
            project: project.context("missing project path")?,
            krate: krate.context("missing --crate")?,
            batch,
            widened: Vec::new(),
            out: out.context("missing --out")?,
            symbol_files,
            watch,
            socket,
            link_cmd,
            patch,
            splice,
            report,
            deny,
            isolate,
            prebuilt,
            exe,
            shared,
            staticlib,
            dep_info,
            inputs,
            library: false,
            run_tool,
            cfg_test,
            unit_test,
            executables,
            scratch,
            own_target,
            scripts_from,
            selection: plan::Selection {
                profile: profile.unwrap_or_else(|| "dev".to_owned()),
                features,
                no_default_features,
                all_features,
                packages,
                workspace,
                excludes,
                unstable,
                target,
                dev_deps: cfg_test || dev_deps,
                config,
                asked_in,
            },
            also,
            debug: plan::Described::NOTHING,
            lints_capped: false,
            link_args: Vec::new(),
            checkout: PathBuf::new(),
            base: None,
            members: Arc::default(),
        })
    }
}

struct Resolver<'a> {
    db: &'a RootDatabase,
    table: &'a SymbolTable,
    /// What panic locations in the workspace are relative to.
    root: PathBuf,
    /// The checkout that the packages which are the same in both are read from.
    base: Option<PathBuf>,
    contents: &'a content::Contents,
    /// What the code has a debugger told.
    debug: plan::Described,
    /// The object the entries a debugger reads are kept in, when they are kept apart from
    /// the program.
    apart: Option<PathBuf>,
    listed: &'a Listed,
    /// For each crate asked about, the paths that functions written in its bodies share.
    /// Every thread of the build asks about the same crates, and one of them looks.
    shared_paths: SharedPaths,
    /// The symbols of the functions asked about, for every thread of the build: the threads
    /// that compile find them ahead of the one that puts the functions in the object.
    named: Arc<std::sync::Mutex<FxHashMap<FunctionId, Option<String>>>>,
    /// The places of the closures and statics of the definitions gone through, for every
    /// thread of the build.
    places: Places,
    /// What the executable a whole build's patch goes into has kept of its link.
    own_linked: Option<Arc<FxHashSet<String>>>,
    /// Set once the build turns out to be one in full, which `own_linked` is not for.
    in_full: Arc<std::sync::atomic::AtomicBool>,
    /// The standard library blaze built before, when the program is linked with it instead
    /// of compiling its crates.
    prebuilt: Option<Arc<Prebuilt>>,
    /// The crates of the workspace, which rustc compiled with a symbol for each function
    /// when `linked` is there.
    members: Arc<FxHashSet<ide_db::base_db::Crate>>,
    /// The crates the programs are made of, which a file that several crates share is read as
    /// a module of.
    programs: Option<Arc<FxHashSet<ide_db::base_db::Crate>>>,
    /// The object is the standard library's, which is split into parts for the linker
    /// ([`std_unit`]).
    library: bool,
}

/// A [`Resolver`] without its database, to be taken to another thread and put over that
/// thread's.
struct ResolverParts<'a> {
    table: &'a SymbolTable,
    root: PathBuf,
    base: Option<PathBuf>,
    contents: &'a content::Contents,
    debug: plan::Described,
    apart: Option<PathBuf>,
    listed: &'a Listed,
    shared_paths: SharedPaths,
    named: Arc<std::sync::Mutex<FxHashMap<FunctionId, Option<String>>>>,
    places: Places,
    own_linked: Option<Arc<FxHashSet<String>>>,
    in_full: Arc<std::sync::atomic::AtomicBool>,
    prebuilt: Option<Arc<Prebuilt>>,
    members: Arc<FxHashSet<ide_db::base_db::Crate>>,
    programs: Option<Arc<FxHashSet<ide_db::base_db::Crate>>>,
    library: bool,
}

impl<'a> Resolver<'a> {
    fn parts(&self) -> ResolverParts<'a> {
        ResolverParts {
            table: self.table,
            root: self.root.clone(),
            base: self.base.clone(),
            contents: self.contents,
            debug: self.debug,
            apart: self.apart.clone(),
            listed: self.listed,
            shared_paths: self.shared_paths.clone(),
            named: self.named.clone(),
            places: self.places.clone(),
            own_linked: self.own_linked.clone(),
            in_full: self.in_full.clone(),
            prebuilt: self.prebuilt.clone(),
            members: self.members.clone(),
            programs: self.programs.clone(),
            library: self.library,
        }
    }
}

impl<'a> ResolverParts<'a> {
    fn over<'b>(self, db: &'b RootDatabase) -> Resolver<'b>
    where
        'a: 'b,
    {
        let ResolverParts {
            table,
            root,
            base,
            contents,
            debug,
            apart,
            listed,
            shared_paths,
            named,
            places,
            own_linked,
            in_full,
            prebuilt,
            members,
            programs,
            library,
        } = self;
        Resolver {
            db,
            table,
            root,
            base,
            contents,
            debug,
            apart,
            listed,
            shared_paths,
            named,
            places,
            own_linked,
            in_full,
            prebuilt,
            members,
            programs,
            library,
        }
    }
}

/// The standard library blaze built before the program ([`std_unit`]).
struct Prebuilt {
    /// What its object defines for others to link to.
    defined: Arc<FxHashSet<String>>,
    /// Its crates, as [`library_unit`] has them.
    crates: FxHashSet<ide_db::base_db::Crate>,
}

/// The crates of the standard library a program links, which blaze builds apart from it
/// ([`std_unit`]): what the program's crates take of the library ([`library_linked_by`]), and
/// everything that depends on, whoever's it is (rust-analyzer gives the library a crate of the
/// workspace's where both use one), but `test`, `proc_macro` and the panic runtime the program
/// does not link ([`unlinked_panic_runtime`]). rust-analyzer's graph has every crate depend on
/// the first two, which a program links only as a test or a macro.
fn library_unit(db: &RootDatabase, program: &[Crate]) -> FxHashSet<ide_db::base_db::Crate> {
    use ide_db::base_db::LangCrateOrigin::{ProcMacro, Test};
    let unlinked = unlinked_panic_runtime(db, program);
    let left_out = |it: ide_db::base_db::Crate| match it.data(db).origin {
        CrateOrigin::Lang(ProcMacro | Test) => true,
        _ => Crate::from(it)
            .display_name(db)
            .is_some_and(|name| name.crate_name().as_str() == unlinked),
    };
    let deps = |it: ide_db::base_db::Crate| it.data(db).dependencies.iter().map(|dep| dep.crate_id);
    // The program's crates: a proc macro it depends on runs in the compiler, not in it.
    let mut own: FxHashSet<ide_db::base_db::Crate> = program.iter().map(|&it| it.into()).collect();
    let mut next: Vec<ide_db::base_db::Crate> = own.iter().flat_map(|&it| deps(it)).collect();
    while let Some(krate) = next.pop() {
        let data = krate.data(db);
        if !data.origin.is_lang() && !data.target_kind.is_proc_macro() && own.insert(krate) {
            next.extend(deps(krate));
        }
    }
    let mut unit = FxHashSet::default();
    let mut next: Vec<ide_db::base_db::Crate> =
        own.iter().flat_map(|&it| library_linked_by(db, it)).collect();
    while let Some(krate) = next.pop() {
        if !left_out(krate) && unit.insert(krate) {
            next.extend(deps(krate));
        }
    }
    unit
}

/// The standard library's crates `krate` links, as rustc puts them in: `std`, or `core` and
/// `compiler_builtins` under `#![no_std]`, none under `#![no_core]`, and those it names with
/// `extern crate`. A `#![no_std]` crate with a `#[panic_handler]` of its own does not get
/// std's. One of the library's own that a program links, as `proc_macro`, links what it
/// depends on.
fn library_linked_by(
    db: &RootDatabase,
    krate: ide_db::base_db::Crate,
) -> Vec<ide_db::base_db::Crate> {
    if krate.data(db).origin.is_lang() {
        return (krate.data(db).dependencies.iter())
            .map(|dep| dep.crate_id)
            .filter(|it| it.data(db).origin.is_lang())
            .collect();
    }
    let def_map = hir_def::nameres::crate_def_map(db, krate);
    let injected: &[&str] = if def_map.is_no_core() {
        &[]
    } else if def_map.is_no_std() {
        &["core", "compiler_builtins"]
    } else {
        &["std"]
    };
    let name = |it: ide_db::base_db::Crate| Crate::from(it).display_name(db);
    let mut linked: Vec<ide_db::base_db::Crate> = (krate.transitive_deps(db).into_iter())
        .filter(|&it| it.data(db).origin.is_lang())
        .filter(|&it| name(it).is_some_and(|it| injected.contains(&it.crate_name().as_str())))
        .collect();
    linked.extend(
        (Crate::from(krate).modules(db).into_iter())
            .flat_map(|module| module.extern_crate_decls(db))
            .filter_map(|it| it.resolved_crate(db))
            .map(ide_db::base_db::Crate::from)
            .filter(|it| it.data(db).origin.is_lang()),
    );
    linked
}

/// What a program of tests compiles of the standard library's crates beside its own:
/// `test`, and what only it links, which [`library_unit`] leaves out. None for another.
fn tests_linked(
    db: &RootDatabase,
    program: &[Crate],
    tests: bool,
) -> FxHashSet<ide_db::base_db::Crate> {
    use ide_db::base_db::LangCrateOrigin::Test;
    let deps = |it: ide_db::base_db::Crate| it.data(db).dependencies.iter().map(|dep| dep.crate_id);
    let mut next: Vec<ide_db::base_db::Crate> = match tests {
        true => program.iter().flat_map(|&it| deps(it.into())).collect(),
        false => Vec::new(),
    };
    next.retain(|it| matches!(it.data(db).origin, CrateOrigin::Lang(Test)));
    beside_library(db, program, next)
}

/// What a program compiles of the standard library's crates for `proc_macro` beside its own:
/// `proc_macro`, and what only it links, when a crate of `linked` that is not a proc macro
/// names it with `extern crate`, as `proc-macro2` does in a build script or a program (rustc
/// links `libproc_macro` there, whose functions then say no macro is running). None for
/// another: rust-analyzer's graph has every crate depend on it ([`library_unit`]).
fn macros_linked(
    db: &RootDatabase,
    program: &[Crate],
    linked: &FxHashSet<ide_db::base_db::Crate>,
) -> FxHashSet<ide_db::base_db::Crate> {
    use ide_db::base_db::LangCrateOrigin::ProcMacro;
    let named = linked
        .iter()
        .filter(|it| !it.data(db).target_kind.is_proc_macro())
        .flat_map(|&it| Crate::from(it).modules(db))
        .flat_map(|module| module.extern_crate_decls(db))
        .filter_map(|it| it.resolved_crate(db))
        .map(ide_db::base_db::Crate::from)
        .filter(|it| matches!(it.data(db).origin, CrateOrigin::Lang(ProcMacro)));
    beside_library(db, program, named.collect())
}

/// `next` and everything they depend on that the program's standard library unit
/// ([`library_unit`]) does not have, but the panic runtime it does not link.
fn beside_library(
    db: &RootDatabase,
    program: &[Crate],
    mut next: Vec<ide_db::base_db::Crate>,
) -> FxHashSet<ide_db::base_db::Crate> {
    if next.is_empty() {
        return FxHashSet::default();
    }
    let library = library_unit(db, program);
    let unlinked = unlinked_panic_runtime(db, program);
    let deps = |it: ide_db::base_db::Crate| it.data(db).dependencies.iter().map(|dep| dep.crate_id);
    let mut beside = FxHashSet::default();
    while let Some(krate) = next.pop() {
        let name = Crate::from(krate).display_name(db);
        let unlinked = name.is_some_and(|it| it.crate_name().as_str() == unlinked);
        if !library.contains(&krate) && !unlinked && beside.insert(krate) {
            next.extend(deps(krate));
        }
    }
    beside
}

/// The panic runtime a program does not link: `panic_unwind` when its crate is built with
/// `panic = "abort"`, which the profile gives it as `cfg(panic = "abort")`, else
/// `panic_abort`. std depends on both, and rustc links the one the strategy names.
pub(crate) fn unlinked_panic_runtime(db: &RootDatabase, program: &[Crate]) -> &'static str {
    let abort = program.first().is_some_and(|&it| {
        let cfg = ide_db::base_db::Crate::from(it).cfg_options(db);
        cfg.get_cfg_values("panic").any(|it| it.as_str() == "abort")
    });
    if abort { "panic_unwind" } else { "panic_abort" }
}

/// [`defined_symbols`] of the object at `path`, read again only when the file is not the one it
/// was read from: every build of the program asks for the standard library's unit, which is
/// tens of megabytes of object.
fn defined_symbols_kept(path: &Path) -> anyhow::Result<Arc<FxHashSet<String>>> {
    type Kept = FxHashMap<PathBuf, ((std::time::SystemTime, u64), Arc<FxHashSet<String>>)>;
    static KEPT: std::sync::LazyLock<std::sync::Mutex<Kept>> =
        std::sync::LazyLock::new(Default::default);
    let file = std::fs::metadata(path).ok();
    let key = file.and_then(|it| Some((it.modified().ok()?, it.len())));
    if let Some(key) = key
        && let Some((was, names)) = lock(&KEPT).get(path)
        && *was == key
    {
        return Ok(names.clone());
    }
    let names = Arc::new(defined_symbols(path)?);
    if let Some(key) = key {
        lock(&KEPT).insert(path.to_owned(), (key, names.clone()));
    }
    Ok(names)
}

/// The names an object defines for others to link to.
fn defined_symbols(path: &Path) -> anyhow::Result<FxHashSet<String>> {
    use object::{Object as _, ObjectSymbol as _, SymbolScope};
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let objects: Vec<&[u8]> = if link::split::is_archive(&bytes) {
        let archive = object::read::archive::ArchiveFile::parse(&*bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        archive
            .members()
            .map(|it| Ok(it?.data(&*bytes)?))
            .collect::<object::Result<_>>()
            .with_context(|| format!("parsing {}", path.display()))?
    } else {
        vec![&bytes]
    };
    let mut names = FxHashSet::default();
    for bytes in objects {
        let file =
            object::File::parse(bytes).with_context(|| format!("parsing {}", path.display()))?;
        // A hidden one stands in for a function blaze could not compile, which a program
        // compiles itself to be told why. A thread-local is not a "definition" to `object`.
        // A COFF external symbol is `SymbolScope::Linkage` in `object` (`Dynamic` is for a DLL's
        // exports), and COFF has no hidden symbols to tell apart: all of them count, or the
        // prebuilt standard library defines nothing on Windows and a program compiles std's
        // startup itself, up to the first writable static (`CLEANUP`).
        let coff = matches!(file.format(), object::BinaryFormat::Coff | object::BinaryFormat::Pe);
        let defined = file.symbols().filter(|it| {
            !it.is_undefined()
                && (it.scope() == SymbolScope::Dynamic
                    || coff && it.scope() == SymbolScope::Linkage)
        });
        // Mach-O spells every name with an underscore in front, which the names looked up
        // ([`Resolver::in_prebuilt`]) don't have: with it kept, no function or static of the
        // prebuilt std was found on macOS, a build script compiled std's itself and stopped
        // at its first writable static (`GLOBAL_PANIC_COUNT`).
        let macho = file.format() == object::BinaryFormat::MachO;
        names.extend(defined.filter_map(|it| {
            let name = it.name().ok()?;
            Some(if macho { name.strip_prefix('_').unwrap_or(name) } else { name }.to_owned())
        }));
    }
    Ok(names)
}

/// The [`SymbolTable::generation`] and the checkout that paths were found with.
type SharedKey = (u64, PathBuf, Option<PathBuf>);

type SharedPaths =
    Arc<std::sync::Mutex<FxHashMap<Crate, Arc<std::sync::OnceLock<FxHashSet<String>>>>>>;

/// What rustc is told a file is called: its place in the checkout when it is a file of it.
fn source_path(
    db: &dyn HirDatabase,
    root: &Path,
    base: Option<&Path>,
    file: vfs::FileId,
) -> Option<String> {
    let source_root = db.file_source_root(file).source_root_id(db);
    let source_root = db.source_root(source_root).source_root(db);
    let path = source_root.path_for_file(&file)?.as_path()?.to_string();
    // The library is named where it is, as in a toolchain, also when it is in the checkout
    // built (blaze's own): what the store keeps of it is named as built (`library_sources`).
    if cargo::toolchain::library::has(Path::new(&path)) {
        return Some(path);
    }
    let base = base.and_then(|base| Path::new(&path).strip_prefix(base).ok());
    let relative = Path::new(&path).strip_prefix(root).ok().or(base);
    Some(relative.map_or(path.clone(), |it| it.to_string_lossy().into_owned()))
}

impl<'a> Resolver<'a> {
    /// The symbols of the program of `args`, which panic locations are named for from now on.
    fn of(
        db: &'a RootDatabase,
        table: &'a SymbolTable,
        args: &Args,
        contents: &'a content::Contents,
        listed: &'a Listed,
    ) -> anyhow::Result<Resolver<'a>> {
        let prebuilt = args.prebuilt.as_deref().filter(|_| table.is_empty()).map(|path| {
            let defined = defined_symbols_kept(path).context("the standard library blaze built")?;
            let program: Vec<Crate> =
                args.crates().filter_map(|it| crates_named(db, it, args).ok()).flatten().collect();
            let crates = library_unit(db, &program);
            anyhow::Ok(Arc::new(Prebuilt { defined, crates }))
        });
        let prebuilt = prebuilt.transpose()?;
        Ok(Resolver::with(db, table, args, contents, listed, prebuilt))
    }

    /// [`Resolver::of`] for what is asked of what rustc linked alone, which the standard
    /// library blaze built is no part of: its symbols are not read.
    fn of_rustcs(
        db: &'a RootDatabase,
        table: &'a SymbolTable,
        args: &Args,
        contents: &'a content::Contents,
        listed: &'a Listed,
    ) -> Resolver<'a> {
        Resolver::with(db, table, args, contents, listed, None)
    }

    fn with(
        db: &'a RootDatabase,
        table: &'a SymbolTable,
        args: &Args,
        contents: &'a content::Contents,
        listed: &'a Listed,
        prebuilt: Option<Arc<Prebuilt>>,
    ) -> Resolver<'a> {
        let own_linked = args
            .splice
            .as_deref()
            .filter(|_| table.is_empty() && args.patch.is_some())
            .and_then(splice::kept_names);
        let (root, base) = (args.checkout.clone(), args.base.clone());
        let debug = args.debug;
        let apart = (debug.kept != plan::Kept::Linked).then(|| args.out.clone());
        let named = (root.clone(), base.clone());
        hir_ty_codegen::name_sources(move |db, file| {
            source_path(db, &named.0, named.1.as_deref(), file)
        });
        Resolver {
            db,
            table,
            root,
            base,
            contents,
            debug,
            apart,
            listed,
            shared_paths: Default::default(),
            named: Default::default(),
            places: Places::default(),
            own_linked,
            in_full: Arc::default(),
            prebuilt,
            members: args.members.clone(),
            programs: listed.programs(db),
            library: args.library,
        }
    }

    /// What reads the syntax, with each file a module of the crates of the programs.
    fn sema(&self) -> hir::Semantics<'a, RootDatabase> {
        match &self.programs {
            Some(crates) => hir::Semantics::new_within(self.db, crates.clone()),
            None => hir::Semantics::new(self.db),
        }
    }

    /// Whether the crate is of the standard library blaze built before.
    fn prebuilt(&self, krate: ide_db::base_db::Crate) -> bool {
        self.prebuilt.as_ref().is_some_and(|it| it.crates.contains(&krate))
    }

    /// The name of a function or static of the prebuilt standard library, when it defines it.
    fn in_prebuilt(&self, symbol: String) -> Option<String> {
        self.prebuilt.as_ref().is_some_and(|it| it.defined.contains(&symbol)).then_some(symbol)
    }
}

impl Resolver<'_> {
    /// What `path` is called in the checkout, when it is a file of it.
    fn inside<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        let base = self.base.as_deref().and_then(|base| path.strip_prefix(base).ok());
        path.strip_prefix(&self.root).ok().or(base)
    }

    /// The function's path as `rustc-demangle` prints its v0 symbol.
    fn path(&self, func: FunctionId) -> Option<String> {
        let ModuleDef::Function(func) = ModuleDef::from(ModuleDefId::FunctionId(func)) else {
            return None;
        };
        let name = func.name(self.db);
        let Some(assoc) = func.as_assoc_item(self.db) else {
            return self.item_path(func.module(self.db), name.as_str());
        };
        // A method a trait provides is named after the trait, as what its body declares is.
        let imp = match assoc.container(self.db) {
            AssocItemContainer::Impl(imp) => imp,
            AssocItemContainer::Trait(trait_) => {
                let trait_ = self.item_path(trait_.module(self.db), trait_.name(self.db).as_str());
                return Some(format!("{}::{}", trait_?, name.as_str()));
            }
        };
        let self_ty = self.type_path(&imp.self_ty(self.db))?;
        match imp.trait_(self.db) {
            Some(trait_) => {
                let mut path =
                    self.item_path(trait_.module(self.db), trait_.name(self.db).as_str())?;
                // `impl PartialEq<str> for String` and `impl PartialEq<&str> for String` are
                // told apart by the trait's arguments alone.
                let trait_ref = imp.trait_ref(self.db)?;
                let params = hir::GenericDef::from(trait_).type_or_const_params(self.db);
                let mut args = Vec::new();
                for (idx, param) in params.iter().enumerate().skip(1) {
                    let arg = trait_ref.get_type_argument(idx)?;
                    let default = param.as_type_param(self.db).and_then(|it| it.default(self.db));
                    // FIXME: a default that names a parameter is understood only as `= Self`.
                    let is_default = default.as_ref().is_some_and(|default| {
                        if default.as_type_param(self.db).is_some() {
                            return hir::attach_db(self.db, || arg == trait_ref.self_ty());
                        }
                        let default = self.type_path(default);
                        default.is_some() && default == self.type_path(&arg)
                    });
                    if idx + 1 == params.len() && is_default {
                        break;
                    }
                    args.push(self.type_path(&arg)?);
                }
                if !args.is_empty() {
                    path = format!("{path}<{}>", args.join(", "));
                }
                let trait_ = path;
                Some(format!("<{self_ty} as {trait_}>::{}", name.as_str()))
            }
            None => Some(format!("<{self_ty}>::{}", name.as_str())),
        }
    }

    /// The symbol of `func`, when it has one.
    fn function_named(&self, func: FunctionId) -> Option<String> {
        if let Some(symbol) = exported_symbol(self.db, func) {
            return Some(symbol);
        }
        // rustc built nothing of blaze's test harness.
        if in_harness(self.db, func) {
            return Some(format!("blaze.harness.{}", self.path(func)?));
        }
        let krate = hir_def::HasModule::krate(&func, self.db);
        if let Some(symbol) = linked_symbol(self.db, func)
            && (!self.rustc_built(krate) || self.prebuilt(krate))
        {
            return Some(symbol);
        }
        if self.prebuilt(krate) {
            if self.is_generic(func)
                || matches!(func.lookup(self.db).container, ItemContainerId::ExternBlockId(_))
            {
                return None;
            }
            return self.in_prebuilt(self.own_function_name(func)?);
        }
        // Not a function until it or the `impl<T>` it is in is given its types: there is
        // nothing to name, the more so in a crate of such alone, which rustc builds nothing
        // of. One of a name that another in the same function has is not to be taken for it.
        if self.is_generic(func) {
            return None;
        }
        // The symbol of a path is the one rustc gave a function it built under it, which is
        // not known to be this one when a body has two of the path. One of a crate rustc
        // built nothing of is named by where it is written as well.
        if self.rustc_built(krate) && self.shares_path(func) {
            return None;
        }
        let extern_block =
            matches!(func.lookup(self.db).container, ItemContainerId::ExternBlockId(_));
        let Some(path) = self.path(func) else {
            // No path to look up in what rustc built, but one of a crate rustc built nothing of
            // is ours to name all the same, by where it is written.
            if extern_block || self.rustc_built(krate) {
                return None;
            }
            return self.own_function_name(func);
        };
        // A test is the second of its name where it is written: rustc puts the constant
        // that describes it to the harness before it, under the same name.
        let function = hir::Function::from(func);
        let symbol = if function.is_test(self.db) || function.is_bench(self.db) {
            self.table.get_at(&path, 1, &[])
        } else {
            self.table.get(&path)
        };
        if let Some(symbol) = symbol {
            return Some(symbol.to_owned());
        }
        // rustc has built nothing of the crate: the function is ours to name, unless it is
        // declared in an `extern` block, where what it links to names it.
        if extern_block {
            return None;
        }
        if self.rustc_built(krate) {
            if !self.rustc_has_none(func) {
                return None;
            }
            return self.function_named_in(func, self.stable_crate_id(krate)?);
        }
        self.own_function_name(func)
    }

    /// The name of a non-generic function of a crate rustc built nothing of.
    fn own_function_name(&self, func: FunctionId) -> Option<String> {
        let krate = hir_def::HasModule::krate(&func, self.db);
        self.function_named_in(func, self.own_crate_id(krate)?)
    }

    /// The name blaze gives a non-generic function of the crate `id` names: where it is, and
    /// for one in a body where in the source that is.
    fn function_named_in(&self, func: FunctionId, id: u64) -> Option<String> {
        let Some(path) = self.path(func) else {
            // No path to write (the `impl` is for `!`, `dyn Trait`, a type taking a constant, or
            // the function sits where `item_path` cannot reach): its name and where it is
            // written say which function it is just as well.
            let name = hir::Function::from(func).name(self.db);
            let place = hir_ty_codegen::function_place(self.db, self, func)?;
            return Some(format!("blaze.{id:x}.{}.{:x}", name.as_str(), short_hash(&place)));
        };
        if hir_def::HasModule::module(&func, self.db).block(self.db).is_none() {
            return Some(format!("blaze.{id:x}.{path}"));
        }
        let place = hir_ty_codegen::block_place(self.db, self, func)?;
        Some(format!("blaze.{id:x}.{path}.{:x}", short_hash(&place)))
    }

    /// The name a static of a crate rustc built nothing of is defined under, in a build with
    /// nothing of rustc's: the one it is declared to have, else where it is, as
    /// [`Self::own_function_name`] has it of a function.
    fn own_static_name(&self, id: hir_def::StaticId) -> Option<String> {
        let db = self.db;
        if let Some(name) = exported_static(db, id) {
            return Some(name.as_str().to_owned());
        }
        let ModuleDef::Static(it) = ModuleDef::from(ModuleDefId::StaticId(id)) else {
            return None;
        };
        let crate_id = self.own_crate_id(it.module(db).krate(db).into())?;
        let (module, name) = (it.module(db), it.name(db));
        if module.nearest_non_block_module(db) == module {
            return Some(format!("blaze.{crate_id:x}.{}", self.item_path(module, name.as_str())?));
        }
        let place = hir_ty_codegen::static_place(db, self, id)?;
        Some(format!("blaze.{crate_id:x}.{}.{:x}", name.as_str(), short_hash(&place)))
    }

    /// Whether the function, or the `impl` it is in, takes types or constants. A method a
    /// trait provides takes `Self`.
    fn is_generic(&self, func: FunctionId) -> bool {
        is_generic_function(self.db, func)
    }

    /// Whether another function written in a body of the crate has this one's path. rustc
    /// tells such functions apart by a number that the path does not show, so its symbol for
    /// the path may be the other's.
    fn shares_path(&self, func: FunctionId) -> bool {
        let module = hir::Function::from(func).module(self.db);
        if !is_in_body(self.db, module) {
            return false;
        }
        let Some(path) = self.path(func) else { return false };
        let krate = module.krate(self.db);
        let listed = lock(&self.listed.functions).get(&krate).cloned();
        // Kept with the listing, which an edit to more than bodies forgets, for the builds
        // after this one that read the same symbols.
        let key = (self.table.generation(), self.root.clone(), self.base.clone());
        let shared = match &listed {
            Some(listed) if key.0 != 0 => {
                let mut kept = lock(&listed.shared_paths);
                match kept.iter().find(|it| it.0 == key) {
                    Some(it) => it.1.clone(),
                    None => {
                        let shared = Arc::<std::sync::OnceLock<_>>::default();
                        kept.push((key, shared.clone()));
                        shared
                    }
                }
            }
            _ => lock(&self.shared_paths).entry(krate).or_default().clone(),
        };
        shared.get_or_init(|| self.paths_shared_in(krate, listed.as_deref())).contains(&path)
    }

    /// The paths that more than one of the functions written in the bodies of a crate have.
    fn paths_shared_in(&self, krate: Crate, listed: Option<&ListedCrate>) -> FxHashSet<String> {
        let mut seen = FxHashSet::default();
        let mut shared = FxHashSet::default();
        // Asked for without listing the crate as one that is built.
        let functions = match listed {
            Some(listed) => listed.functions.clone(),
            None => declared_in(self.db, krate).functions,
        };
        for func in functions {
            if !is_in_body(self.db, hir::Function::from(func).module(self.db)) {
                continue;
            }
            let Some(path) = self.path(func) else { continue };
            if !seen.insert(path.clone()) {
                shared.insert(path);
            }
        }
        shared
    }

    /// rustc's `StableCrateId` of a crate it built something of. A crate of the checkout is
    /// one rustc built when a file of the link is its own: the standard library has private
    /// copies of crates (memchr, hashbrown) whose names a dependency at a path has too.
    fn rustc_crate_id(&self, krate: ide_db::base_db::Crate) -> Option<u64> {
        let ours = !krate.data(self.db).origin.is_lang();
        let krate = Crate::from(krate);
        let name = krate.display_name(self.db)?;
        let root = self.crate_root(krate)?;
        if ours && !self.table.links(name.crate_name().as_str(), &root) {
            return None;
        }
        self.table.stable_crate_id(name.crate_name().as_str(), &root)
    }

    /// How the paths of symbols spell a crate. One of the checkout that rustc built nothing
    /// of is spelled with its own id, so that no symbol of another crate of its name (the
    /// standard library's memchr) is taken for one of its items.
    fn crate_path(&self, krate: Crate) -> Option<String> {
        let name = krate.display_name(self.db)?;
        if self.table.is_empty() && krate.origin(self.db).is_lang() {
            return Some(name.crate_name().to_string());
        }
        match self.own_crate_id(krate.into()) {
            Some(own) => Some(format!("{}[{own:x}]", name.crate_name())),
            None => self.table.crate_path(name.crate_name().as_str(), &self.crate_root(krate)?),
        }
    }

    /// What stands for a crate of the checkout that rustc built nothing of, the same after
    /// every edit: its name and where in the checkout its source starts. One of the standard
    /// library, when the build has nothing of rustc's, by its name alone: a program has one
    /// of each name, wherever its source is. One of a registry or of git by what it is built
    /// with too ([`content::Contents::library`]): its source does not change, but its features,
    /// cfgs and what its build script set and wrote do, and code of one is not the other's.
    fn own_crate_id(&self, krate: ide_db::base_db::Crate) -> Option<u64> {
        let origin = &krate.data(self.db).origin;
        let (lang, local) = (origin.is_lang(), origin.is_local());
        if lang && !self.table.is_empty() || self.rustc_crate_id(krate).is_some() {
            return None;
        }
        let name = Crate::from(krate).display_name(self.db)?;
        if lang {
            return Some(short_hash(&format!("blaze crate {}", name.crate_name())));
        }
        let root = self.crate_root(krate.into())?;
        let home = self.contents.cargo_home(self.db);
        let place = crate_place(&root, self.inside(&root), home.as_deref());
        if local {
            return Some(short_hash(&format!("blaze crate {} {place}", name.crate_name())));
        }
        let built = self.contents.library(
            self.db,
            krate,
            &root,
            &|krate| Some((self.stable_crate_id(krate), self.crate_root(krate.into())?)),
            &|krate| process_reads(self.db, krate),
        );
        let named = format!("blaze crate {} {place} {built}", name.crate_name());
        Some(short_hash(&named))
    }

    /// The file the crate's source starts at.
    fn crate_root(&self, krate: Crate) -> Option<PathBuf> {
        crate_root_file(self.db, krate)
    }

    fn item_path(&self, module: Module, name: &str) -> Option<String> {
        let mut path = self.scope_path(module)?;
        path.push_str("::");
        path.push_str(name);
        Some(path)
    }

    /// The function written at `func`, which holds the block that `scope` is the module of.
    ///
    /// `Semantics::to_def` answers the same, but starts from every crate the file is part of,
    /// which for a package with tests and examples analyses all its dev-dependencies.
    fn function_at(
        &self,
        sema: &hir::Semantics<'_, RootDatabase>,
        scope: Module,
        func: &ast::Fn,
    ) -> Option<hir::Function> {
        let db = self.db;
        let file = sema.hir_file_for(func.syntax());
        let written = hir::InFile::new(file, file.ast_id_map(db).ast_id(func));
        let is_it = |it: &hir::Function| {
            FunctionId::try_from(*it).is_ok_and(|it| it.lookup(db).id == written)
        };
        let among = |items: Vec<hir::AssocItem>| {
            items.into_iter().filter_map(hir::AssocItem::as_function).find(is_it)
        };
        let mut module = Some(scope);
        while let Some(it) = module {
            let found = it
                .declarations(db)
                .into_iter()
                .find_map(|def| match def {
                    ModuleDef::Function(func) => Some(func).filter(is_it),
                    ModuleDef::Trait(trait_) => among(trait_.items(db)),
                    _ => None,
                })
                .or_else(|| it.impl_defs(db).into_iter().find_map(|imp| among(imp.items(db))));
            if found.is_some() || it.nearest_non_block_module(db) == it {
                return found;
            }
            module = it.parent(db);
        }
        None
    }

    /// The path of what holds a module's items: the module, or for the items of a block the
    /// function, closure, constant or static the block is in, which only the syntax knows.
    fn scope_path(&self, module: Module) -> Option<String> {
        if module.nearest_non_block_module(self.db) == module {
            let mut path = self.crate_path(module.krate(self.db))?;
            for segment in module.path_segments(self.db) {
                path.push_str("::");
                path.push_str(segment.as_str());
            }
            return Some(path);
        }
        let sema = self.sema();
        let source = module.definition_source(self.db);
        let hir::ModuleSource::BlockExpr(block) = source.value else { return None };
        let range = block.syntax().text_range();
        let block = sema
            .parse_or_expand(source.file_id)
            .covering_element(range)
            .ancestors()
            .find(|it| it.text_range() == range && ast::BlockExpr::can_cast(it.kind()))?;
        let mut inner = Vec::new();
        let mut owner = None;
        for node in sema.ancestors_with_macros(block) {
            if is_closure(&node) {
                inner.push(format!("{{closure#{}}}", closure_number(&sema, &self.places, &node)?));
                continue;
            }
            if let Some(func) = ast::Fn::cast(node.clone()) {
                // The body of an `async fn` is a closure to rustc.
                if func.async_token().is_some() {
                    inner.push("{closure#0}".to_owned());
                }
                let func = self.function_at(&sema, module, &func)?;
                owner = Some(self.path(func.try_into().ok()?)?);
                break;
            }
            match_ast! {
                match node {
                    ast::Const(it) => inner.push(it.name().map_or("_".to_owned(), |it| it.text().to_owned())),
                    ast::Static(it) => inner.push(it.name()?.text().to_owned()),
                    // Anonymous constants have numbers we don't know.
                    ast::BlockExpr(it) => if it.const_token().is_some() { return None },
                    ast::ConstArg(_) => return None,
                    _ => {},
                }
            }
        }
        let mut path = match owner {
            Some(owner) => owner,
            None => {
                let outer = inner.pop()?;
                self.item_path(module.nearest_non_block_module(self.db), &outer)?
            }
        };
        for segment in inner.iter().rev() {
            path.push_str("::");
            path.push_str(segment);
        }
        Some(path)
    }

    /// A type the way rustc writes it in a symbol.
    fn type_path(&self, ty: &hir::Type<'_>) -> Option<String> {
        if let Some((inner, mutability)) = ty.as_reference() {
            let inner = self.type_path(&inner)?;
            return Some(match mutability {
                hir::Mutability::Shared => format!("&{inner}"),
                hir::Mutability::Mut => format!("&mut {inner}"),
            });
        }
        if let Some(elem) = ty.as_slice() {
            return Some(format!("[{}]", self.type_path(&elem)?));
        }
        if let Some(builtin) = ty.as_builtin() {
            return Some(builtin.name().as_str().to_owned());
        }
        if ty.is_tuple() {
            let fields: Option<Vec<_>> =
                ty.tuple_fields(self.db).iter().map(|it| self.type_path(it)).collect();
            return Some(match &*fields? {
                [only] => format!("({only},)"),
                fields => format!("({})", fields.join(", ")),
            });
        }
        // A parameter of the `impl` an item is in, where the item is not generic itself: a
        // static, or what a closure or constant in a generic function holds.
        if ty.as_type_param(self.db).is_some() {
            return Some("_".to_owned());
        }
        let adt = ty.as_adt()?;
        let path = self.item_path(adt.module(self.db), adt.name(self.db).as_str())?;
        let generics = hir::GenericDef::from(adt);
        let params = generics.type_or_const_params(self.db);
        if params.is_empty() {
            return Some(path);
        }
        // rustc leaves a defaulted const argument out of the name, so `Bump` may be `Bump<1>`.
        // FIXME: name those too.
        if !params.iter().all(|it| it.as_type_param(self.db).is_some()) {
            return None;
        }
        // Lifetimes are erased, and rustc leaves erased ones out of the name.
        let args: Option<Vec<_>> = ty.type_arguments().map(|arg| self.type_path(&arg)).collect();
        Some(format!("{path}<{}>", args?.join(", ")))
    }
}

/// What tells the standard library a build compiles against from any other: the digest of
/// its source ([`source_identity`]), and, of a program rustc's library is linked into, the name
/// of the built library it links and what the metadata of its `core`, `alloc` and `std` says
/// of each (`metadata_hash`). `None` when it has none of them.
fn toolchain_identity(symbol_files: &[PathBuf]) -> Option<String> {
    let mut identity = library_identity(symbol_files);
    identity.extend(source_identity());
    identity.sort_unstable();
    (!identity.is_empty()).then(|| identity.join(" "))
}

/// What [`toolchain_identity`] will say of a build with nothing of rustc's, before it is
/// loaded: the source of the library it loads.
fn toolchain_ahead() -> Option<String> {
    let identity = source_identity();
    (!identity.is_empty()).then(|| identity.join(" "))
}

/// The rlibs of the standard library whose metadata [`toolchain_identity`] reads. rustc's link
/// line has all of them, and a build told ahead of cargo's finds the same in the toolchain
/// ([`built_library`]): one toolchain is then one identity whichever way a daemon starts, and
/// a daemon started after cargo's build finds what the one told ahead of it stored.
const LIBRARY: [&str; 3] = ["libcore-", "liballoc-", "libstd-"];

/// What of [`toolchain_identity`] the library's rlibs among `files` say: the name of `std`'s
/// and what the metadata of each of [`LIBRARY`] says.
fn library_identity(files: &[PathBuf]) -> Vec<String> {
    let mut identity: Vec<String> = files
        .iter()
        .filter_map(|it| it.file_name()?.to_str())
        .filter(|it| it.starts_with("libstd-"))
        .map(str::to_owned)
        .collect();
    for rlib in files {
        let Some(name) = rlib.file_name().and_then(|it| it.to_str()) else { continue };
        if !LIBRARY.iter().any(|it| name.starts_with(it)) || !name.ends_with(".rlib") {
            continue;
        }
        let Ok(file) = std::fs::File::open(rlib) else { continue };
        // SAFETY: a library of the toolchain is not written once it is installed, nor one cargo
        // built while a build links it.
        let Ok(bytes) = (unsafe { memmap2::Mmap::map(&file) }) else { continue };
        if let Some(hash) = metadata_hash(&bytes) {
            identity.push(format!("{name}={hash}"));
        }
    }
    identity
}

/// What of [`toolchain_identity`] the source of the standard library says: its digest
/// (`library/DIGEST`), the same on any machine with that library. The whole of it for a build
/// with nothing of rustc's. Nothing of the downloaded toolchain is: blaze compiles neither its
/// `rust-src` nor with its rustc.
pub(crate) fn source_identity() -> Vec<String> {
    let library = cargo::toolchain::library::found();
    library.map(|it| format!("library={}", it.digest)).into_iter().collect()
}

/// A hash of what the metadata of the crate in the rlib `archive` says it is: the header,
/// and the start of the crate's root, which holds its target and the hash rustc gives the
/// crate (its SVH). That hash is another for other sources or other flags, which the name of
/// the file need not show: a library rebuilt under the same toolchain, by `-Zbuild-std` or
/// into a patched sysroot, keeps its name. The rlibs of a toolchain hold a stub of their
/// metadata with the root in it, and others all of it.
fn metadata_hash(archive: &[u8]) -> Option<String> {
    use object::{Object as _, ObjectSection as _};
    use sha2::{Digest, Sha256};
    /// How much of the root is hashed: the crate's header, of a few dozen bytes, and more.
    const ROOT: usize = 256;
    let parsed = object::read::archive::ArchiveFile::parse(archive).ok()?;
    let member = parsed.members().filter_map(Result::ok).find(|it| it.name() == b"lib.rmeta")?;
    let member = member.data(archive).ok()?;
    let metadata = match object::File::parse(member) {
        Ok(object) => object.section_by_name(".rmeta")?.data().ok()?,
        Err(_) => member,
    };
    if !metadata.starts_with(b"rust\0\0\0") {
        return None;
    }
    let root = u64::from_le_bytes(metadata.get(8..16)?.try_into().ok()?);
    let root = metadata.get(usize::try_from(root).ok()?..)?;
    let mut hasher = Sha256::new();
    hasher.update(&metadata[..16]);
    hasher.update(&root[..root.len().min(ROOT)]);
    let hash = hasher.finalize();
    Some(hash[..8].iter().map(|it| format!("{it:02x}")).collect())
}

/// The built rlibs of [`LIBRARY`] of blaze's toolchain, for `triple`.
fn built_library(triple: &str) -> Vec<PathBuf> {
    let Some(toolchain) = cargo::toolchain::sysroot() else { return Vec::new() };
    library_in(&toolchain.join(format!("lib/rustlib/{triple}/lib")))
}

/// The rlibs of [`LIBRARY`] in the directory of a toolchain's built library.
fn library_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let named =
        |name: &str| LIBRARY.iter().any(|it| name.starts_with(it)) && name.ends_with(".rlib");
    let names = entries.filter_map(|it| it.ok()?.file_name().into_string().ok());
    names.filter(|it| named(it)).map(|it| dir.join(it)).collect()
}

impl SymbolResolver for Resolver<'_> {
    fn named_dependencies(&self, krate: ide_db::base_db::Crate) -> Vec<ide_db::base_db::Crate> {
        self.listed.named(self.db, krate.into()).iter().map(|&it| it.into()).collect()
    }

    fn in_parallel_beside(&self, count: usize, work: &ParallelWork<'_>, beside: &mut dyn FnMut()) {
        let threads = std::thread::available_parallelism().map_or(1, |it| it.get()).min(count);
        // One alone is as soon done by whoever asks for what it computes.
        if threads < 2 {
            return beside();
        }
        let parallel = |name: &'static str| {
            tracing::info_span!(target: "blaze", "parallel", subject = name).entered()
        };
        let mut join = None;
        let next = std::sync::atomic::AtomicUsize::new(0);
        // What the threads compile, the build that asked counts.
        let counted = hir_ty_codegen::counted::now();
        // A worker's panic goes on as itself, a cancellation first: the scope would end in one
        // of its own, which callers can't tell from a bug.
        let stopped = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    let (db, next, parts) = (self.db.clone(), &next, self.parts());
                    let counted = counted.clone();
                    scope.spawn(move || {
                        let _worker = parallel("worker");
                        let resolver = parts.over(&db);
                        hir_ty_codegen::counted::within(counted, || {
                            hir::attach_db(&db, || {
                                loop {
                                    let at =
                                        next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    if at >= count {
                                        break;
                                    }
                                    work(&db, &resolver, at);
                                }
                            })
                        })
                    })
                })
                .collect();
            {
                let _caller = parallel("caller");
                beside();
            }
            // From when the caller is done to when the last thread is.
            join = Some(parallel("join"));
            let mut stopped: Option<Box<dyn std::any::Any + Send>> = None;
            for worker in workers {
                let Err(panic) = worker.join() else { continue };
                let cancelled = panic.is::<ide_db::base_db::salsa::Cancelled>();
                if stopped
                    .as_ref()
                    .is_none_or(|it| cancelled && !it.is::<ide_db::base_db::salsa::Cancelled>())
                {
                    stopped = Some(panic);
                }
            }
            stopped
        });
        drop(join);
        if let Some(panic) = stopped {
            std::panic::resume_unwind(panic);
        }
    }

    fn source_path(&self, file: vfs::FileId) -> Option<String> {
        source_path(self.db, &self.root, self.base.as_deref(), file)
    }

    fn debug_directory(&self) -> Option<String> {
        (self.debug.level != plan::Level::Nothing).then(|| self.root.to_string_lossy().into_owned())
    }

    fn describes_variables(&self) -> bool {
        self.debug.level == plan::Level::Full
    }

    fn keeps_frame_pointers(&self) -> bool {
        self.debug.frame_pointers
    }

    fn debug_apart(&self) -> Option<String> {
        self.apart.as_ref().map(|it| it.to_string_lossy().into_owned())
    }

    fn debug_grouped(&self) -> bool {
        self.library
    }

    fn function(&self, func: FunctionId) -> Option<String> {
        if let Some(named) = self.named.lock().unwrap_or_else(|e| e.into_inner()).get(&func) {
            return named.clone();
        }
        let named = self.function_named(func);
        self.named.lock().unwrap_or_else(|e| e.into_inner()).insert(func, named.clone());
        named
    }

    fn private(&self, func: FunctionId) -> bool {
        !self.is_generic(func) && self.path(func).is_some_and(|path| self.table.private(&path))
    }

    fn defines(&self, symbol: &str) -> bool {
        self.table.defines(symbol)
    }

    fn provides(&self, symbol: &str) -> bool {
        self.table.defines(symbol)
            || self.prebuilt.as_ref().is_some_and(|it| it.defined.contains(symbol))
    }

    fn linked(&self, symbol: &str) -> bool {
        let in_full = self.in_full.load(std::sync::atomic::Ordering::Relaxed);
        in_full || self.own_linked.as_ref().is_none_or(|it| it.contains(symbol))
    }

    fn in_full(&self) {
        self.in_full.store(true, std::sync::atomic::Ordering::Relaxed);
        // Some were named after what only the executable has.
        self.named.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    fn closure(&self, owner: FunctionId, expr: hir_def::hir::ExprId) -> Option<String> {
        let source_map = &Body::with_source_map(self.db, owner.into()).1;
        let written = source_map.expr_syntax(expr).ok()?;
        let sema = self.sema();
        let node = written.value.to_node(&sema.parse_or_expand(written.file_id));
        let mut inner = Vec::new();
        for node in sema.ancestors_with_macros(node.syntax().clone()) {
            if is_closure(&node) {
                inner.push(format!("{{closure#{}}}", closure_number(&sema, &self.places, &node)?));
                continue;
            }
            if let Some(func) = ast::Fn::cast(node) {
                // The body of an `async fn` is a closure of its own to rustc.
                if func.async_token().is_some() {
                    return None;
                }
                break;
            }
        }
        let mut path = self.path(owner)?;
        for segment in inner.iter().rev() {
            path.push_str("::");
            path.push_str(segment);
        }
        self.table.get(&path).map(str::to_owned)
    }

    fn makers(&self, function: &str) -> Vec<u64> {
        self.table.makers(function).collect()
    }

    fn copies(&self, symbol: &str) -> Vec<(String, u64)> {
        self.table.copies(symbol).map(|(copy, of)| (copy.to_owned(), of)).collect()
    }

    /// A crate rustc built may have no symbol that says which it is: one of generics alone,
    /// when nothing rustc built gives them their types. Its functions are still not ours to
    /// name.
    fn rustc_built(&self, krate: ide_db::base_db::Crate) -> bool {
        // The standard library is rustc's prebuilt one, unless the build has nothing of rustc's,
        // where it is blaze's, compiled with the program or before it.
        if self.prebuilt(krate) {
            return true;
        }
        // What the prebuilt unit leaves out ([`library_unit`]), `proc_macro` and `test`, is
        // compiled with the program that links it.
        if krate.data(self.db).origin.is_lang() {
            return !self.table.is_empty();
        }
        if self.own_crate_id(krate).is_none() {
            return true;
        }
        let named = Crate::from(krate).display_name(self.db);
        let root = self.crate_root(krate.into());
        named
            .zip(root)
            .is_some_and(|(name, root)| self.table.links(name.crate_name().as_str(), &root))
    }

    fn stable_crate_id(&self, krate: ide_db::base_db::Crate) -> Option<u64> {
        self.rustc_crate_id(krate).or_else(|| self.own_crate_id(krate))
    }

    fn content_id(&self, krate: ide_db::base_db::Crate) -> Option<ContentId> {
        let places = (&*self.root, self.base.as_deref());
        if !krate.data(self.db).origin.is_local() {
            let root = self.crate_root(krate.into())?;
            return Some(self.contents.library(
                self.db,
                krate,
                &root,
                &|krate| Some((self.stable_crate_id(krate), self.crate_root(krate.into())?)),
                &|krate| process_reads(self.db, krate),
            ));
        }
        self.contents.id(
            self.db,
            places,
            krate,
            &|krate| self.stable_crate_id(krate),
            // Every body of the crate has been looked through once it is listed, so that
            // every variable it asks for is known.
            &|krate| {
                self.listed.of(self.db, krate.into());
                process_reads(self.db, krate)
            },
        )
    }

    fn cargo_home(&self) -> Option<String> {
        self.contents.cargo_home(self.db)
    }

    fn library_sources(&self) -> Option<hir_ty_codegen::LibrarySources> {
        self.contents.library_sources()
    }

    fn names_place(&self, bytes: &[u8]) -> bool {
        self.contents.names_place((&*self.root, self.base.as_deref()), bytes)
    }

    fn declared_in(&self, adt: hir_def::AdtId) -> Option<FunctionId> {
        let adt = hir::Adt::from(adt);
        let sema = self.sema();
        let source = sema.source(adt)?;
        let mut around = sema.ancestors_with_macros(source.value.syntax().clone()).skip(1);
        let func = ast::Fn::cast(around.find(is_definition)?)?;
        // The body of an `async fn` is a closure to rustc.
        if func.async_token().is_some() {
            return None;
        }
        let name = adt.name(self.db);
        let named_so = func.syntax().descendants().filter_map(ast::Name::cast).filter(|it| {
            it.text() == name.as_str()
                && it.syntax().parent().is_some_and(|it| {
                    ast::Adt::can_cast(it.kind())
                        || ast::Trait::can_cast(it.kind())
                        || ast::TypeAlias::can_cast(it.kind())
                        || ast::Module::can_cast(it.kind())
                })
        });
        if named_so.count() != 1 {
            return None;
        }
        self.function_at(&sema, adt.module(self.db), &func)?.try_into().ok()
    }

    fn impl_place(&self, id: hir_def::ImplId) -> Option<u32> {
        use hir::HasSource;
        let db = self.db;
        let it = hir::Impl::from(id);
        let source = it.source(db)?;
        // FIXME: where rustc puts the `impl`s a macro makes among the ones written out.
        if source.file_id.is_macro() {
            return None;
        }
        let start = |it: &hir::InFile<ast::Impl>| it.value.syntax().text_range().start();
        let mut earlier = 0;
        for other in it.module(db).impl_defs(db) {
            let other = other.source(db)?;
            // rustc numbers what a module has written out before what its macros and derives
            // make, which it only has once they are expanded.
            if other.file_id.is_macro() {
                continue;
            }
            if other.file_id != source.file_id {
                return None;
            }
            earlier += u32::from(start(&other) < start(&source));
        }
        Some(earlier)
    }

    fn nothing_of_rustc(&self) -> bool {
        self.table.is_empty()
    }

    fn own_static(&self, id: StaticId) -> Option<String> {
        self.table.is_empty().then(|| self.own_static_name(id)).flatten()
    }

    fn static_copy(&self, symbol: &str) -> Option<hir_ty_codegen::RustcStaticCopy> {
        self.table.static_copy(symbol).cloned()
    }

    fn static_(&self, id: StaticId) -> Option<String> {
        if self.prebuilt(hir_def::HasModule::krate(&id, self.db)) {
            return self.in_prebuilt(self.own_static_name(id)?);
        }
        let ModuleDef::Static(it) = ModuleDef::from(ModuleDefId::StaticId(id)) else {
            return None;
        };
        let module = it.module(self.db);
        let name = it.name(self.db);
        if module.nearest_non_block_module(self.db) == module {
            let path = self.item_path(module, name.as_str())?;
            return self.table.get(&path).map(str::to_owned);
        }
        // A static in a body is named after the items around it, which only the syntax knows.
        let sema = self.sema();
        let source = sema.source(it)?;
        let mut names = vec![name.as_str().to_owned()];
        let mut closures = Vec::new();
        let mut around = Vec::new();
        let mut owner = None;
        for node in sema.ancestors_with_macros(source.value.syntax().clone()).skip(1) {
            if is_closure(&node) {
                closures.push(closure_number(&sema, &self.places, &node)?);
                continue;
            }
            if let Some(func) = ast::Fn::cast(node.clone()) {
                if func.async_token().is_some() {
                    closures.push(0);
                }
                let func = self.function_at(&sema, module, &func)?;
                owner = Some(self.path(func.try_into().ok()?)?);
                break;
            }
            if let Some(name) = name_in_path(&node) {
                let named = |other: &SyntaxNode| name_in_path(other).as_ref() == Some(&name);
                around.push(place_in_definition(&sema, &node, &named)?.into());
                names.push(name);
            }
        }
        let owner = match owner {
            Some(owner) => owner,
            None => {
                around.pop();
                let outer = names.pop()?;
                self.item_path(module.nearest_non_block_module(self.db), &outer)?
            }
        };
        names.push(owner);
        names.reverse();
        closures.reverse();
        around.reverse();
        let path = names.join("::");
        // Only a path through a closure or an anonymous constant has segments left out of it.
        let named = |node: &SyntaxNode| {
            ast::Static::cast(node.clone())
                .is_some_and(|it| it.name().is_some_and(|it| it.text() == name.as_str()))
        };
        let counted = Counted::Statics(name.as_str().to_owned());
        let place =
            place_counted(&sema, &self.places, counted, source.value.syntax(), &named)?.into();
        let symbol = self
            .table
            .get_nested(&path, &closures, place, &around)
            .or_else(|| self.table.get_at(&path, place, &around));
        symbol.map(str::to_owned)
    }

    fn path(&self, path: &str) -> Option<String> {
        self.table.get(path).map(str::to_owned)
    }

    fn item(&self, item: ModuleDefId) -> Option<String> {
        let item = ModuleDef::from(item);
        self.item_path(item.module(self.db)?, item.name(self.db)?.as_str())
    }

    fn adt_at(&self, path: &str) -> Option<hir_def::AdtId> {
        let db = self.db;
        let mut segments = path.split("::");
        let krate = segments.next()?;
        let mut segments: Vec<&str> = segments.collect();
        let name = segments.pop()?;
        Crate::all(db)
            .into_iter()
            .filter(|&it| self.crate_path(it).as_deref() == Some(krate))
            .find_map(|it| {
                let mut module = it.root_module(db);
                for segment in &segments {
                    let child = |it: &Module| it.name(db).is_some_and(|it| it.as_str() == *segment);
                    module = module.children(db).find(child)?;
                }
                let adt = module.declarations(db).into_iter().find_map(|it| match it {
                    ModuleDef::Adt(adt) if adt.name(db).as_str() == name => Some(adt),
                    _ => None,
                })?;
                let id = hir_def::AdtId::from(adt);
                (self.item(id.into()).as_deref() == Some(path)).then_some(id)
            })
    }
}

/// The segment a constant or static that items are written in adds to their path, as rustc
/// prints it: its name, or `_` for an unnamed constant. objc2's `declare_class!` keeps a
/// class's ivar offsets in statics in a `const _: () = { .. }`, which rustc exports as
/// `<module>::_::__OBJC2_IVAR_OFFSET`.
fn name_in_path(node: &SyntaxNode) -> Option<String> {
    match_ast! {
        match node {
            ast::Const(it) => it
                .name()
                .map(|it| it.text().to_string())
                .or_else(|| it.underscore_token().map(|_| "_".to_owned())),
            ast::Static(it) => it.name().map(|it| it.text().to_string()),
            _ => None,
        }
    }
}

/// Whether what the module declares is written in a body.
fn is_in_body(db: &RootDatabase, module: Module) -> bool {
    module.nearest_non_block_module(db) != module
}

/// What names the crate whose root is `root` in the id blaze gives it, the same on every
/// machine: its path in the checkout (`inside`), or below where cargo keeps what it fetched
/// (`cargo_home`), or else where it is.
fn crate_place(root: &Path, inside: Option<&Path>, cargo_home: Option<&str>) -> String {
    if let Some(inside) = inside {
        return inside.display().to_string();
    }
    let fetched = root.to_str().zip(cargo_home);
    match fetched
        .and_then(|(root, home)| hir_ty_codegen::moved(root, home, hir_ty_codegen::CARGO_HOME))
    {
        Some(moved) => moved,
        None => root.display().to_string(),
    }
}

/// The first eight bytes of the SHA-256 of a text.
fn short_hash(text: &str) -> u64 {
    let hash = <sha2::Sha256 as sha2::Digest>::digest(text.as_bytes());
    u64::from_le_bytes(hash[..8].try_into().expect("a hash is 32 bytes"))
}

/// Whether rustc numbers the node as a closure: a closure, or an `async` block.
fn is_closure(node: &SyntaxNode) -> bool {
    ast::ClosureExpr::can_cast(node.kind())
        || ast::BlockExpr::cast(node.clone()).is_some_and(|it| it.async_token().is_some())
}

fn is_definition(node: &SyntaxNode) -> bool {
    !ast::MacroCall::can_cast(node.kind())
        && (ast::Item::can_cast(node.kind())
            || ast::AssocItem::can_cast(node.kind())
            || ast::ConstArg::can_cast(node.kind())
            || is_closure(node)
            || ast::BlockExpr::cast(node.clone()).is_some_and(|it| it.const_token().is_some()))
}

/// The place of a node among those of the definition it is written in that `counts` holds
/// of, in the order rustc comes across them, or none when that order cannot be told. This is
/// how rustc tells apart the closures of a function, and the statics of one name that a
/// macro used twice in it declares.
fn place_in_definition(
    sema: &hir::Semantics<'_, RootDatabase>,
    target: &SyntaxNode,
    counts: &dyn Fn(&SyntaxNode) -> bool,
) -> Option<u32> {
    let along: Vec<_> = sema.ancestors_with_macros(target.clone()).collect();
    let owner = owner_of(&along)?;
    let mut count = Count::new(sema, Some(target), along, counts, &owner);
    count.all_in(&owner)?;
    count.waiting_ones()?;
    count.earlier
}

/// The definition a node is written in, as [`place_in_definition`] counts in: the first of
/// `along` but the node that is one, or the last.
fn owner_of(along: &[SyntaxNode]) -> Option<SyntaxNode> {
    let mut root = None;
    let owner = along.iter().skip(1).cloned().find(|node| {
        root = Some(node.clone());
        is_definition(node)
    });
    owner.or(root)
}

/// [`place_in_definition`] of what `counted` names, every node of the definition counted
/// once and kept in `places`: a definition with many closures, or a macro used many times
/// that declares a static each time (tracing's `META` and `__CALLSITE`), is gone through
/// once instead of once a node, which made a build in full of a large program wait on one
/// thread (#259). A node whose place the count does not find is counted alone as before.
fn place_counted(
    sema: &hir::Semantics<'_, RootDatabase>,
    places: &Places,
    counted: Counted,
    target: &SyntaxNode,
    counts: &dyn Fn(&SyntaxNode) -> bool,
) -> Option<u32> {
    let along: Vec<_> = sema.ancestors_with_macros(target.clone()).collect();
    let owner = owner_of(&along)?;
    if let Some(spot) = spot_in(&along, &owner) {
        let of = PlacesOf {
            file: sema.hir_file_for(&owner),
            range: owner.text_range(),
            kind: owner.kind(),
            counted,
        };
        let known = places.0.lock().unwrap_or_else(|e| e.into_inner()).get(&of).cloned();
        let all = known.unwrap_or_else(|| {
            let all = all_places(sema, &owner, counts).map(Arc::new);
            places.0.lock().unwrap_or_else(|e| e.into_inner()).insert(of, all.clone());
            all
        });
        if let Some(&place) = all.as_ref().and_then(|all| all.get(&spot)) {
            return Some(place);
        }
    }
    place_in_definition(sema, target, counts)
}

/// The places of all the nodes of the definition `owner` that `counts` holds of, by where
/// each is ([`Spot`]), or none when the order rustc numbers them in is not known.
fn all_places(
    sema: &hir::Semantics<'_, RootDatabase>,
    owner: &SyntaxNode,
    counts: &dyn Fn(&SyntaxNode) -> bool,
) -> Option<FxHashMap<Spot, u32>> {
    let mut count = Count::new(sema, None, Vec::new(), counts, owner);
    count.places = Some(FxHashMap::default());
    count.all_in(owner)?;
    count.waiting_ones()?;
    let mut places = count.places?;
    // Two nodes in one spot are told apart by nothing a spot has.
    for spot in count.twice {
        places.remove(&spot);
    }
    Some(places)
}

/// Where `along`'s first node is in the definition `owner`, which `along` goes up to: the
/// macros it is expanded from and the expansion of each, or none when an attribute or a
/// derive made a node on the way, which the count does not go into.
fn spot_in(along: &[SyntaxNode], owner: &SyntaxNode) -> Option<Spot> {
    let at_owner = along.iter().position(|it| it == owner)?;
    let mut calls = Vec::new();
    for at in (1..at_owner).rev() {
        let node = &along[at];
        if ast::MacroCall::can_cast(node.kind()) {
            let root = &along[at - 1];
            if root.parent().is_some() {
                return None;
            }
            calls.push((node.text_range(), root.kind()));
        } else if node.parent().is_none() && !ast::MacroCall::can_cast(along[at + 1].kind()) {
            // An expansion that no macro call along is of.
            return None;
        }
    }
    if along.first()?.parent().is_none() {
        return None;
    }
    Some((calls, along.first()?.text_range()))
}

/// The places of the counted nodes of the definitions gone through ([`place_counted`]).
#[derive(Clone, Default)]
struct Places(Arc<std::sync::Mutex<FxHashMap<PlacesOf, Option<Arc<FxHashMap<Spot, u32>>>>>>);

/// A definition, and what is counted in it.
#[derive(Clone, PartialEq, Eq, Hash)]
struct PlacesOf {
    file: hir::HirFileId,
    range: syntax::TextRange,
    kind: syntax::SyntaxKind,
    counted: Counted,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Counted {
    Closures,
    /// The statics of a name.
    Statics(String),
}

/// Where a node is in a definition: the macros it is expanded from, the outermost first,
/// each by where it is used and what it is expanded to (a macro used in a body is expanded
/// both to items and to statements, of the same text), and the node's range in the last.
type Spot = (Vec<(syntax::TextRange, syntax::SyntaxKind)>, syntax::TextRange);

/// The imports of the scope of a definition that rustc has not resolved when it first comes
/// to the macros used in the definition: those a macro expanded to. It resolves the imports
/// it has, then expands every macro it can, and only then resolves the imports that those
/// expanded to.
#[derive(Default)]
struct LateImports {
    names: FxHashSet<String>,
    glob: bool,
}

impl LateImports {
    fn around(sema: &hir::Semantics<'_, RootDatabase>, owner: &SyntaxNode) -> LateImports {
        let mut late = LateImports::default();
        let module_expanded = module_is_expanded(sema, owner);
        let expanded = |node: &SyntaxNode| {
            module_expanded
                || sema.hir_file_for(node).is_macro()
                || sema
                    .ancestors_with_macros(node.clone())
                    .filter_map(ast::Item::cast)
                    .any(|item| has_attribute_macro(sema, &item))
        };
        if expanded(owner) {
            late.add_written_in(owner);
        }
        for scope in sema.ancestors_with_macros(owner.clone()) {
            if expanded(&scope) {
                let imports = scope.children().filter_map(ast::Use::cast);
                imports.filter_map(|it| it.use_tree()).for_each(|tree| late.add(&tree, None));
            }
            if ast::Module::can_cast(scope.kind()) {
                break;
            }
        }
        late
    }

    /// Adds the imports written in the blocks of a definition, those of the definitions in
    /// it apart.
    fn add_written_in(&mut self, node: &SyntaxNode) {
        for child in node.children() {
            if let Some(tree) = ast::Use::cast(child.clone()).and_then(|it| it.use_tree()) {
                self.add(&tree, None);
            } else if !is_definition(&child) {
                self.add_written_in(&child);
            }
        }
    }

    /// Adds the names the tree brings in, `prefix` being the last name of the path before it.
    fn add(&mut self, tree: &ast::UseTree, prefix: Option<String>) {
        let last = tree.path().and_then(|it| it.segment()).map(|it| it.syntax().text().to_string());
        let own = if last.as_deref() == Some("self") { prefix } else { last };
        if let Some(list) = tree.use_tree_list() {
            list.use_trees().for_each(|it| self.add(&it, own.clone()));
        } else if tree.star_token().is_some() {
            self.glob = true;
        } else if let Some(name) = tree.rename().and_then(|it| it.name()) {
            self.names.insert(name.text().to_owned());
        } else {
            self.names.extend(own);
        }
    }
}

/// Whether an attribute of the item is a macro, which rustc expands the item with: `#[test]`
/// is one, though it leaves the item as it is written.
fn has_attribute_macro(sema: &hir::Semantics<'_, RootDatabase>, item: &ast::Item) -> bool {
    sema.resolve_attr_macro_call(item).is_some()
        || ast::HasAttrs::attrs(item).filter_map(|it| it.path()).any(|path| {
            matches!(
                sema.resolve_path(&path),
                Some(hir::PathResolution::Def(ModuleDef::Macro(found))) if found.is_attr(sema.db)
            )
        })
}

/// Whether a macro expanded to the module the node is in, or to one around it.
fn module_is_expanded(sema: &hir::Semantics<'_, RootDatabase>, node: &SyntaxNode) -> bool {
    let mut module = sema.scope(node).map(|it| it.module());
    while let Some(it) = module {
        if it.declaration_source(sema.db).is_some_and(|it| it.file_id.is_macro()) {
            return true;
        }
        module = it.parent(sema.db);
    }
    false
}

/// When rustc expands a macro used in a definition.
enum Turn {
    /// As it comes to it.
    Now,
    /// After every macro it can expand at first, the last such first.
    Later,
}

struct Count<'a, 'db> {
    sema: &'a hir::Semantics<'db, RootDatabase>,
    target: Option<&'a SyntaxNode>,
    /// The target and what it is in, the macros it is expanded from with their expansions.
    along: Vec<SyntaxNode>,
    counts: &'a dyn Fn(&SyntaxNode) -> bool,
    late: LateImports,
    seen: u32,
    earlier: Option<u32>,
    /// The macros to expand after the others, each with the macros it is in.
    waiting: Vec<(ast::MacroCall, Vec<(syntax::TextRange, syntax::SyntaxKind)>)>,
    /// The macros the nodes being counted are in, as a [`Spot`] has them.
    calls: Vec<(syntax::TextRange, syntax::SyntaxKind)>,
    /// The place of every counted node, when they are all wanted.
    places: Option<FxHashMap<Spot, u32>>,
    twice: Vec<Spot>,
}

impl<'a, 'db> Count<'a, 'db> {
    fn new(
        sema: &'a hir::Semantics<'db, RootDatabase>,
        target: Option<&'a SyntaxNode>,
        along: Vec<SyntaxNode>,
        counts: &'a dyn Fn(&SyntaxNode) -> bool,
        owner: &SyntaxNode,
    ) -> Self {
        let late = LateImports::around(sema, owner);
        Count {
            sema,
            target,
            along,
            counts,
            late,
            seen: 0,
            earlier: None,
            waiting: Vec::new(),
            calls: Vec::new(),
            places: None,
            twice: Vec::new(),
        }
    }

    /// Expands the macros that wait for the others, the last come to first.
    fn waiting_ones(&mut self) -> Option<()> {
        while let Some((call, calls)) = self.waiting.pop() {
            self.calls = calls;
            self.expansion_of(&call)?;
        }
        Some(())
    }

    /// Counts the nodes of the definition `node` is of, or none when the order rustc numbers
    /// them in is not known.
    ///
    /// rustc numbers what is written in the definition before what the macros used in it
    /// expand to, and then goes through the macros in turn, each the same way.
    fn all_in(&mut self, node: &SyntaxNode) -> Option<()> {
        let mut calls = Vec::new();
        self.written_in(node, &mut calls);
        for call in calls {
            match self.turn(&call) {
                Some(Turn::Now) => self.expansion_of(&call)?,
                Some(Turn::Later) => self.waiting.push((call, self.calls.clone())),
                None => self.nothing_in(&call)?,
            }
        }
        Some(())
    }

    fn expansion_of(&mut self, call: &ast::MacroCall) -> Option<()> {
        // A macro used in a body is expanded twice, to items and to statements, and of the
        // same text: the target is in one of them, which need not be the one sema gives.
        let at = self.along.iter().position(|it| it == call.syntax());
        let along = at.and_then(|at| at.checked_sub(1)).map(|at| self.along[at].clone());
        let root = match along.filter(|root| root.parent().is_none()) {
            Some(root) => root,
            None => match self.sema.expand_macro_call(call) {
                Some(expansion) => expansion.value,
                None => return Some(()),
            },
        };
        self.calls.push((call.syntax().text_range(), root.kind()));
        self.all_in(&root)?;
        self.calls.pop();
        Some(())
    }

    /// That the macro expands to nothing that is counted, so that when it is expanded is
    /// the same to the count.
    fn nothing_in(&mut self, call: &ast::MacroCall) -> Option<()> {
        let (seen, waiting) = (self.seen, self.waiting.len());
        self.expansion_of(call)?;
        (self.seen == seen && self.waiting.len() == waiting).then_some(())
    }

    /// As `all_in`, of what is not in a macro: `calls` gets the macros come across.
    fn written_in(&mut self, node: &SyntaxNode, calls: &mut Vec<ast::MacroCall>) {
        for child in node.children() {
            if let Some(call) = ast::MacroCall::cast(child.clone()) {
                calls.push(call);
                continue;
            }
            let target = self.target == Some(&child);
            if target {
                self.earlier = Some(self.seen);
            }
            let counted = (self.counts)(&child);
            if counted && let Some(places) = &mut self.places {
                let spot = (self.calls.clone(), child.text_range());
                if places.insert(spot.clone(), self.seen).is_some() {
                    self.twice.push(spot);
                }
            }
            if target || counted {
                self.seen += 1;
            }
            if !target && !is_definition(&child) {
                self.written_in(&child, calls);
            }
        }
    }

    /// When rustc expands the macro, or none when that is not known: a macro it finds by a
    /// late import waits for the import.
    fn turn(&self, call: &ast::MacroCall) -> Option<Turn> {
        let Some(path) = call.path() else { return Some(Turn::Now) };
        let Some(first) = path.first_segment().and_then(|it| it.name_ref()) else {
            return Some(Turn::Now);
        };
        let alone = path.qualifier().is_none();
        let imported = self.late.names.contains(first.text());
        if !alone && !imported {
            return self.reached_by_path(call).then_some(Turn::Now);
        }
        if !imported && !self.late.glob {
            return Some(Turn::Now);
        }
        if alone && self.in_textual_scope(call)? {
            return Some(Turn::Now);
        }
        if imported {
            return Some(Turn::Later);
        }
        // A name alone may be found by the glob import: not one of this crate or of the
        // standard library, which the macros in scope everywhere are.
        let home = self.sema.resolve_macro_call(call)?.module(self.sema.db).krate(self.sema.db);
        let here = self.sema.scope(call.syntax())?.krate();
        (home == here || home.origin(self.sema.db).is_lang()).then_some(Turn::Now)
    }

    /// Whether a path finds the macro with no late import: any but a `macro_rules!` one of
    /// this crate that is not exported, which is in no module but by an import of it, and is
    /// found at once only through an import written in the module the path names.
    fn reached_by_path(&self, call: &ast::MacroCall) -> bool {
        let sema = self.sema;
        let Some(found) = sema.resolve_macro_call(call) else { return true };
        let here = sema.scope(call.syntax()).map(|it| it.krate());
        if Some(found.module(sema.db).krate(sema.db)) != here {
            return true;
        }
        match sema.source(found).and_then(|it| it.value.left()) {
            Some(ast::Macro::MacroRules(rules)) => {
                ast::HasAttrs::attrs(&rules)
                    .any(|it| it.simple_name().is_some_and(|name| name == "macro_export"))
                    || self.imported_where_written(call)
            }
            _ => true,
        }
    }

    /// Whether the module that the path of `call` names imports the macro by a `use` written
    /// in it, which no macro made: rustc resolves those before it expands any macro, as
    /// `crate::tracing::debug!` finds salsa's `pub(crate) use debug;`.
    fn imported_where_written(&self, call: &ast::MacroCall) -> bool {
        let sema = self.sema;
        let Some(path) = call.path() else { return false };
        let (Some(qualifier), Some(name)) =
            (path.qualifier(), path.segment().and_then(|it| it.name_ref()))
        else {
            return false;
        };
        let Some(hir::PathResolution::Def(ModuleDef::Module(module))) =
            sema.resolve_path(&qualifier)
        else {
            return false;
        };
        let mut around = Some(module);
        while let Some(it) = around {
            if it.declaration_source(sema.db).is_some_and(|it| it.file_id.is_macro()) {
                return false;
            }
            around = it.parent(sema.db);
        }
        let written = sema.module_definition_node(module);
        if written.file_id.is_macro() {
            return false;
        }
        let items = match ast::Module::cast(written.value.clone()) {
            Some(inline) => inline.item_list().map(|it| it.syntax().clone()),
            None => ast::SourceFile::can_cast(written.value.kind()).then_some(written.value),
        };
        let uses = items.iter().flat_map(|it| it.children()).filter_map(ast::Use::cast);
        uses.filter(|it| !has_attribute_macro(sema, &ast::Item::Use(it.clone()))).any(|it| {
            let mut imported = LateImports::default();
            it.use_tree().into_iter().for_each(|tree| imported.add(&tree, None));
            imported.names.contains(&*name.text())
        })
    }

    /// Whether the macro is a `macro_rules!` one found by where it is written, which is
    /// before an import of the name is looked for: one written earlier, beside something
    /// the use is in, or in a `#[macro_use]` module that is. None when that is not known:
    /// a macro expanded to it or to a module around it.
    fn in_textual_scope(&self, call: &ast::MacroCall) -> Option<bool> {
        let sema = self.sema;
        let Some(found) = sema.resolve_macro_call(call) else { return Some(false) };
        let Some(source) = sema.source(found) else { return Some(false) };
        let Some(ast::Macro::MacroRules(rules)) = source.value.left() else {
            return Some(false);
        };
        let named = rules.name().map(|it| it.text().to_string());
        let used = call.path().and_then(|it| it.segment()).map(|it| it.syntax().text().to_string());
        // Another crate's are in scope by `#[macro_use]`, which is looked in after imports.
        let home = found.module(sema.db).krate(sema.db);
        if named != used || Some(home) != sema.scope(call.syntax()).map(|it| it.krate()) {
            return Some(false);
        }
        let mut module = found.module(sema.db);
        let mut written = hir::InFile::new(source.file_id, rules.syntax().clone());
        loop {
            if written.file_id.is_macro() {
                return None;
            }
            let beside = sema.ancestors_with_macros(call.syntax().clone()).any(|around| {
                around.parent() == written.value.parent()
                    && sema.hir_file_for(&around) == written.file_id
                    && written.value.text_range().end() <= around.text_range().start()
            });
            if beside {
                return Some(true);
            }
            // The scope goes on after a module that is `#[macro_use]`.
            let inline = written.value.ancestors().skip(1).find_map(ast::Module::cast);
            let around = match inline {
                Some(inline) => written.with_value(inline),
                None => match module.declaration_source(sema.db) {
                    Some(declared) => declared,
                    None => return Some(false),
                },
            };
            let goes_on = ast::HasAttrs::attrs(&around.value)
                .any(|it| it.simple_name().is_some_and(|name| name == "macro_use"));
            if !goes_on {
                return Some(false);
            }
            module = match module.parent(sema.db) {
                Some(parent) => parent,
                None => return Some(false),
            };
            written = around.map(|it| it.syntax().clone());
        }
    }
}

/// The `n` of the `{closure#n}` rustc names a closure: its place among the closures of the
/// definition it is written in, a closure's own closures being counted apart.
fn closure_number(
    sema: &hir::Semantics<'_, RootDatabase>,
    places: &Places,
    closure: &SyntaxNode,
) -> Option<u32> {
    place_counted(sema, places, Counted::Closures, closure, &is_closure)
}

/// The crates called `krate` that a build of the checkout at `checkout` compiles: a package's
/// library and its binary may share the name, and both are being built. A `krate` that is the
/// path of a `.rs` file is the one target with that root file, which tells a test from an
/// example of the same name.
/// What the records of a build call the crate `args` builds ([`events`]): its name, or what
/// `--crate` says.
fn shown_crate(db: &RootDatabase, args: &Args) -> String {
    let named = crates_named(db, &args.krate, args).unwrap_or_default();
    let name = named.first().and_then(|it| it.display_name(db));
    name.map_or_else(|| args.krate.clone(), |it| it.to_string())
}

fn crates_named(db: &RootDatabase, krate: &str, args: &Args) -> anyhow::Result<Vec<Crate>> {
    let mut found = crates_called(db, krate, args);
    if args.library {
        found.retain(|it| ide_db::base_db::Crate::from(*it).data(db).origin.is_lang());
    }
    // A dependency may be another version of the crate being built, and another checkout
    // has a crate of its name too.
    let of_checkout = |krate: &Crate| args.members.contains(&(*krate).into());
    if found.iter().any(of_checkout) {
        found.retain(of_checkout);
    }
    // The standard library vendors private copies of crates (libc, hashbrown) that are
    // libstd's, and a program's crate of the same name is the one asked for.
    let of_program = |krate: &Crate| !krate.origin(db).is_lang();
    if found.iter().any(of_program) {
        found.retain(of_program);
    }
    // One of the crates after the first is one the first links: a binary of another package
    // may have the name too, as bun's `bun-sema` has the library `bun_sema`'s.
    if let Some(first) = args.crates().next().filter(|&it| it != krate) {
        let program = crates_named(db, first, args)?;
        let program: Vec<_> = program.into_iter().map(|it| deps_of(db, it.into())).collect();
        let linked = |krate: &Crate| program.iter().any(|it| it.contains(&(*krate).into()));
        if found.iter().any(linked) {
            found.retain(linked);
        }
    }
    anyhow::ensure!(!found.is_empty(), "crate {krate} not found in workspace");
    Ok(found)
}

/// Why what rustc built does not say which copy of a function a call reaches any more, in a
/// program whose crates differ in `-C overflow-checks`: rustc decides by the source which
/// crate compiles a function, and a source was written after it did.
fn stale_copies(db: &RootDatabase, table: &SymbolTable, args: &Args) -> Option<String> {
    let written = table.written_since_built()?;
    let built = args.crates().filter_map(|it| crates_named(db, it, args).ok()).flatten();
    let built: Vec<Crate> = built.collect();
    checks_differ(db, &built).then(|| {
        format!(
            "the crates of the program differ in `overflow-checks` and {} was written after \
             rustc built it: which crate's copy of a function a call reaches is read from \
             what rustc built, so cargo has to build first",
            args.inside(written).unwrap_or(written).display()
        )
    })
}

/// Whether the crates cargo builds of the program of `built` do not all have
/// `-C overflow-checks` alike.
fn checks_differ(db: &RootDatabase, built: &[Crate]) -> bool {
    let flag = cfg::CfgAtom::Flag(intern::sym::overflow_checks);
    let program: FxHashSet<ide_db::base_db::Crate> = built
        .iter()
        .flat_map(|&it| ide_db::base_db::Crate::from(it).transitive_deps(db))
        .filter(|it| !it.data(db).origin.is_lang())
        .collect();
    let mut checks = program.iter().map(|it| it.cfg_options(db).check_atom(&flag));
    checks.next().is_some_and(|first| checks.any(|it| it != first))
}

/// Fails when rustc linked none of the libraries blaze builds a program with `lto` from:
/// rustc then compiled the whole program from bitcode as it linked it, and the functions
/// blaze leaves to rustc are not there to be linked against by name.
fn linked_from_libraries(db: &RootDatabase, args: &Args) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.link_cmd.as_deref().is_none_or(links_core),
        "the link command has no library of `core`, as rustc's has not with `lto`: blaze \
         links against a build with `lto = \"off\"`"
    );
    if args.symbol_files.is_empty() {
        return Ok(());
    }
    let mut built = Vec::new();
    for named in args.crates() {
        built.extend(crates_named(db, named, args)?);
    }
    let used: FxHashSet<Crate> =
        built.iter().flat_map(|it| it.dependencies(db)).map(|it| it.krate).collect();
    // A proc macro is a library of the compiler's, not of the program.
    let is_proc_macro =
        |krate: &Crate| ide_db::base_db::Crate::from(*krate).data(db).target_kind.is_proc_macro();
    let libraries: Vec<String> = built
        .iter()
        .filter(|it| used.contains(it) && !is_proc_macro(it))
        .filter_map(|it| Some(format!("lib{}", it.display_name(db)?.crate_name().as_str())))
        .collect();
    let linked = |library: &String| {
        let mut files = args.symbol_files.iter().filter_map(|it| it.file_name()?.to_str());
        files.any(|it| it.starts_with(library.as_str()))
    };
    anyhow::ensure!(
        libraries.is_empty() || libraries.iter().any(linked),
        "rustc linked the program without a library of `{}`, as it does with `lto`: blaze \
         links against a build with `lto = \"off\"`",
        libraries[0].trim_start_matches("lib")
    );
    Ok(())
}

/// Whether a link command names `core` as a library, by itself or in the shared library of
/// std. One that reads its files from elsewhere is taken to.
fn links_core(command: &str) -> bool {
    let mut files = command.split_whitespace().map(|it| it.trim_matches(['"', '\'']));
    files.any(|it| {
        let name = it.rsplit(['/', '\\']).next().unwrap_or(it);
        it.starts_with('@') || name.starts_with("libcore-") || name.starts_with("libstd-")
    })
}

/// Every crate called `krate`, of the checkout or not.
fn crates_called(db: &RootDatabase, krate: &str, args: &Args) -> Vec<Crate> {
    let is_file = krate.ends_with(".rs");
    // A path that is written as its parts are ends the text of every path it is the end of,
    // which is all that most crates have to be asked.
    let plain = !krate.contains("//") && !krate.contains("./");
    let mut found: Vec<Crate> = named_crates(db)
        .iter()
        .filter(|it| match (is_file, &it.root) {
            // By its parts: on Windows the root's are apart by `\` and the asked `examples/a.rs`'s
            // may be by `/`.
            (true, Some(root)) if plain && !Path::new(&**root).ends_with(krate) => false,
            // Named from the checkout, or in full.
            (true, Some(root)) => {
                let root = Path::new(&**root);
                root == Path::new(krate) || args.inside(root) == Some(Path::new(krate))
            }
            (true, None) => false,
            (false, _) => it.name.as_deref() == Some(krate),
        })
        .map(|it| it.krate)
        .collect();
    // Built as `cargo test` builds them, a library or a binary and its unit tests are two
    // crates of one root file, the tests alone with `cfg(test)`: the crate asked for first is
    // the program's, its unit tests when it is those, and the others are what it links. An
    // integration test has `cfg(test)` and is no target's unit tests.
    if args.cfg_test {
        let unit_tests = |it: &Crate| {
            let data = ide_db::base_db::Crate::from(*it).data(db);
            matches!(data.target_kind, TargetKind::Lib { .. } | TargetKind::Bin)
                && it.cfg(db).check_atom(&cfg::CfgAtom::Flag(intern::sym::test))
        };
        // Unit tests asked for are those or none, never the target in their place: a load
        // that has one copy of a crate gives it to what links it.
        let wanted = *krate == args.krate && args.unit_test;
        if wanted || found.iter().any(|it| !unit_tests(it)) {
            found.retain(|it| unit_tests(it) == wanted);
        }
    }
    found
}

/// A crate of the graph as naming one finds it.
#[derive(PartialEq, Eq)]
struct Named {
    krate: Crate,
    /// What code calls it by.
    name: Option<Box<str>>,
    /// The text of the path of its root file.
    root: Option<Box<str>>,
}

/// Every crate of the graph, in its order, as naming one finds it: a build names its crates
/// many times, and the graph is asked once until it changes.
#[salsa::tracked(returns(ref))]
fn named_crates(db: &dyn HirDatabase) -> Vec<Named> {
    let root = |krate: Crate| {
        let file = krate.root_file(db);
        let source_root = db.file_source_root(file).source_root_id(db);
        let source_root = db.source_root(source_root).source_root(db);
        Some(source_root.path_for_file(&file)?.as_path()?.as_str().into())
    };
    let name = |krate: Crate| Some(krate.display_name(db)?.crate_name().as_str().into());
    Crate::all(db)
        .into_iter()
        .map(|krate| Named { krate, name: name(krate), root: root(krate) })
        .collect()
}

/// The crates `krate` depends on, itself and all, from the graph.
#[salsa::tracked(returns(ref))]
fn deps_of(
    db: &dyn HirDatabase,
    krate: ide_db::base_db::Crate,
) -> FxHashSet<ide_db::base_db::Crate> {
    krate.transitive_deps(db).into_iter().collect()
}

/// Whether the program `args` builds is of one of `crates`: one of its crates is, or one it
/// links. A proc macro is a library of the compiler's, not of the program, built to its
/// module apart from this database, so neither it nor what it links is. A program whose
/// crates are not found may be.
fn program_reads_any(
    db: &RootDatabase,
    args: &Args,
    crates: &FxHashSet<ide_db::base_db::Crate>,
) -> bool {
    let mut seen = FxHashSet::default();
    let mut next = Vec::new();
    for krate in args.crates() {
        let Ok(found) = crates_named(db, krate, args) else { return true };
        next.extend(found.into_iter().map(ide_db::base_db::Crate::from));
    }
    while let Some(krate) = next.pop() {
        if !seen.insert(krate) {
            continue;
        }
        if crates.contains(&krate) {
            return true;
        }
        let linked = krate.data(db).dependencies.iter().map(|it| it.crate_id);
        next.extend(linked.filter(|it| !it.data(db).target_kind.is_proc_macro()));
    }
    false
}

/// The crates called `krate` to build: the ones rustc built into this program, when it built
/// any, which may be a crate of the checkout and a dependency of the same name both. Otherwise
/// what `crates_named` finds.
fn crates_to_build(
    db: &RootDatabase,
    table: &SymbolTable,
    krate: &str,
    args: &Args,
) -> anyhow::Result<Vec<Crate>> {
    let mut called = crates_called(db, krate, args);
    called.retain(|&it| links(db, table, it));
    if called.is_empty() { crates_named(db, krate, args) } else { Ok(called) }
}

/// Whether rustc linked the crate into the program the table is of.
fn links(db: &RootDatabase, table: &SymbolTable, krate: Crate) -> bool {
    let name = krate.display_name(db);
    let root = crate_root_file(db, krate);
    name.zip(root).is_some_and(|(name, root)| table.links(name.crate_name().as_str(), &root))
}

fn crate_root_file(db: &RootDatabase, krate: Crate) -> Option<PathBuf> {
    let file = krate.root_file(db);
    let source_root = db.file_source_root(file).source_root_id(db);
    let source_root = db.source_root(source_root).source_root(db);
    Some(PathBuf::from(source_root.path_for_file(&file)?.as_path()?.to_string()))
}

/// Resolves the names and collects the impls of the crates being built and of everything they
/// depend on, a crate per thread. Every build needs them, and one thread would do them in turn.
/// `load_workspace_at`, with each package's features those of a build of the package at
/// `project` alone, as the artifacts we link against were built.
fn load(
    projects: &[&Path],
    cargo_config: &CargoConfig,
    load_config: LoadCargoConfig,
    selection: plan::Selection,
    beside: Option<Beside>,
    scripts_said: Option<Vec<String>>,
) -> anyhow::Result<(RootDatabase, vfs::Vfs, Checkouts)> {
    let mut checkouts = Checkouts {
        compared: Default::default(),
        roots: Vec::new(),
        splits: Vec::new(),
        loaded: load_cargo::Loaded::default(),
        cargo_config: cargo_config.clone(),
        load_config,
        selection,
        inputs: inputs::Inputs::default(),
        debug: Vec::new(),
        beside,
        scripts_said,
    };
    let mut db = RootDatabase::new(None);
    let mut vfs = vfs::Vfs::default();
    checkouts.add(projects, &mut db, &mut vfs)?;
    Ok((db, vfs, checkouts))
}

/// A file of a checkout that was compared with the one it is in a loaded checkout: the size
/// and the time of the last write of both, and whether the two were alike. They are not read
/// again until one is written.
type Compared = std::sync::Mutex<FxHashMap<PathBuf, ((u64, SystemTime), (u64, SystemTime), bool)>>;

/// The checkouts the daemon has loaded.
struct Checkouts {
    compared: Compared,
    /// The project a build names, and the root of the workspace it is in.
    roots: Vec<(PathBuf, PathBuf)>,
    /// For each of `roots`, what it shares with another when it is not loaded in full.
    splits: Vec<Option<Split>>,
    loaded: load_cargo::Loaded,
    cargo_config: CargoConfig,
    load_config: LoadCargoConfig,
    /// What the checkouts loaded in full were loaded from besides their source.
    inputs: inputs::Inputs,
    /// Which build of each project is loaded.
    selection: plan::Selection,
    /// For each of `roots`, what its plan says of the program as a whole.
    debug: Vec<plan::Whole>,
    /// Where cargo's own build says what the scripts and proc macros of the next checkout
    /// are, when it is loaded beside that build: nothing is then run to find out.
    beside: Option<Beside>,
    /// The same of a build of the first checkout that is over, a line of cargo's each.
    scripts_said: Option<Vec<String>>,
}

impl Checkouts {
    fn root_of(&self, project: &Path) -> anyhow::Result<PathBuf> {
        let project = dunce::canonicalize(project)?;
        let root = self.roots.iter().find(|(it, _)| *it == project).map(|(_, root)| root.clone());
        root.with_context(|| format!("{} is not a project the daemon loaded", project.display()))
    }

    /// The checkout of `project`: its root, what it shares and its crates.
    fn parts(&self, project: &Path) -> anyhow::Result<Parts> {
        let root = self.root_of(project)?;
        let at = self.roots.iter().position(|(_, it)| *it == root).expect("it has a root");
        let base = self.splits[at].as_ref().map(|it| it.base.clone());
        let members = Arc::new(self.loaded.crates(at).iter().copied().collect());
        // An override for measuring one against the other.
        let level = match std::env::var("BLAZE_DEBUGINFO").ok().as_deref() {
            Some("0") => plan::Level::Nothing,
            Some("lines") => plan::Level::Lines,
            Some(_) => plan::Level::Full,
            None => self.debug[at].debug.level,
        };
        let debug = plan::Described { level, ..self.debug[at].debug };
        let (lints_capped, link_args) =
            (self.debug[at].lints_capped, self.debug[at].link_args.clone());
        Ok(Parts { root, base, members, debug, lints_capped, link_args })
    }

    /// Whether the program of the checkout `project` is of is one rustc links from bitcode.
    fn lto(&self, project: &Path) -> anyhow::Result<bool> {
        let root = self.root_of(project)?;
        Ok(self.roots.iter().zip(&self.debug).any(|((_, it), whole)| *it == root && whole.lto))
    }

    /// The loaded checkout that `project` is a checkout of the same repository as, and the
    /// root of its own. `None` when the two are not built alike.
    fn alike(&self, project: &Path) -> Option<(usize, PathBuf)> {
        let project = dunce::canonicalize(project).ok()?;
        // The path of a loaded file is UTF-8: one that is not is refused when it is loaded.
        project.to_str()?;
        (0..self.roots.len()).find_map(|at| {
            let (loaded, base) = &self.roots[at];
            if self.splits[at].is_some() {
                return None;
            }
            let inside = loaded.strip_prefix(base).ok()?;
            let mut over = project.clone();
            for part in inside.components().rev() {
                if over.file_name() != Some(part.as_os_str()) {
                    return None;
                }
                over.pop();
            }
            self.builds_alike(base, &over).then_some((at, over))
        })
    }

    /// The directories of the member packages of the checkout at `at`, whether each is of a
    /// procedural macro, and whether it has an integration test or a benchmark told a place in
    /// the checkout (`CARGO_BIN_EXE_<name>`, `CARGO_TARGET_TMPDIR`), which another checkout
    /// tells its own.
    fn packages(&self, at: usize) -> Vec<(PathBuf, bool, bool)> {
        let ProjectWorkspaceKind::Cargo { cargo, .. } = &self.loaded.workspaces()[at].kind else {
            return Vec::new();
        };
        let told = self.tells_places(at);
        cargo
            .packages()
            .filter(|&it| cargo[it].is_member)
            .map(|it| {
                let package = &cargo[it];
                let dir = AsRef::<Path>::as_ref(package.manifest.parent()).to_owned();
                let kinds = || package.targets.iter().map(|&it| cargo[it].kind);
                let tests = kinds().any(|it| matches!(it, TargetKind::Test | TargetKind::Bench));
                (dir, kinds().any(|it| it.is_proc_macro()), told && tests)
            })
            .collect()
    }

    /// Whether the integration tests and benchmarks of the checkout at `at` are told places in
    /// it (`CARGO_BIN_EXE_<name>`, `CARGO_TARGET_TMPDIR` in its target directory), which those
    /// of another checkout are told in that one.
    fn tells_places(&self, at: usize) -> bool {
        let ProjectWorkspaceKind::Cargo { cargo, .. } = &self.loaded.workspaces()[at].kind else {
            return false;
        };
        let root = &self.roots[at].1;
        [cargo.executables(), cargo.scratch()]
            .into_iter()
            .flatten()
            .any(|it| AsRef::<Path>::as_ref(it).starts_with(root))
    }

    /// The packages of the checkout at `over` whose files are not those of the loaded one at
    /// `base`.
    fn differing(&self, base: usize, over: &Path) -> BTreeSet<PathBuf> {
        let packages = self.packages(base);
        let base = &self.roots[base].1;
        let nested: FxHashSet<&Path> = packages.iter().map(|it| &*it.0).collect();
        // A procedural macro is rustc's to build: one that is not the same is another build.
        // A test told a place in its checkout is the other checkout's own.
        packages
            .iter()
            .filter(|(dir, proc_macro, told)| {
                !proc_macro && (*told || !package_alike(dir, base, over, &nested, &self.compared))
            })
            .map(|it| &it.0)
            .filter_map(|dir| Some(dir.strip_prefix(base).ok()?.to_owned()))
            .collect()
    }

    /// The workspace of the checkout at `over`: that of the loaded one at `base`, with the
    /// packages of `own` where `over` has them.
    fn split(&self, base: usize, over: &Path, own: &BTreeSet<PathBuf>) -> ProjectWorkspace {
        let mut workspace = self.loaded.workspaces()[base].clone();
        let root = AbsPathBuf::assert_utf8(self.roots[base].1.clone());
        let over = AbsPathBuf::assert_utf8(over.to_owned());
        if let ProjectWorkspaceKind::Cargo { cargo, .. } = &mut workspace.kind {
            *cargo = cargo.placed(&root, &over, &|dir| {
                dir.strip_prefix(&root).is_some_and(|it| own.contains(AsRef::<Path>::as_ref(&it)))
            });
        }
        workspace
    }

    /// The checkouts that share other packages with a loaded one than the ones that are the
    /// same in both now: each with the one it shares with and the packages of its own.
    fn differences(&self) -> anyhow::Result<Vec<(usize, usize, BTreeSet<PathBuf>)>> {
        let mut differences = Vec::new();
        for (at, split) in self.splits.iter().enumerate() {
            let Some(split) = split else { continue };
            let base = self.roots.iter().position(|(_, it)| *it == split.base);
            let base = base.context("a checkout shares with a loaded one")?;
            let own = self.differing(base, &self.roots[at].1);
            if own != split.own {
                differences.push((at, base, own));
            }
        }
        Ok(differences)
    }

    /// Gives the checkouts that share packages with a loaded one the packages of their own
    /// that [`Checkouts::differences`] found.
    fn share(
        &mut self,
        db: &mut RootDatabase,
        vfs: &mut vfs::Vfs,
        differences: Vec<(usize, usize, BTreeSet<PathBuf>)>,
    ) -> anyhow::Result<()> {
        for (at, base, own) in differences {
            let over = self.roots[at].1.clone();
            eprintln!("split {}: its own are {own:?}", over.display());
            self.loaded.replace(at, self.split(base, &over, &own));
            self.splits[at] = Some(Split { base: self.roots[base].1.clone(), own });
        }
        self.loaded.add(Vec::new(), &self.cargo_config.extra_env, &self.load_config, vfs, db)?;
        *lock(&UNEDITED) = Unedited::of(self.loaded.workspaces());
        prepare_macro_sessions(db);
        Ok(())
    }

    /// How the daemon builds `project`: as itself when its checkout is loaded, and as its
    /// place in a loaded checkout with the files of its own when `overlay` allows that and
    /// the two lock the same dependencies. `None` when its checkout has to be loaded.
    fn view_of(
        &self,
        project: &Path,
        overlay: Overlay,
        quick: &dyn Fn(&Path, &Path) -> bool,
    ) -> Option<View> {
        let project = dunce::canonicalize(project).ok()?;
        if let Some((_, root)) = self.roots.iter().find(|(it, _)| *it == project) {
            return Some(View { base: root.clone(), over: root.clone(), project });
        }
        if overlay == Overlay::Never {
            return None;
        }
        // One that shares packages with another has those where the other is. Not one whose
        // tests are told places in it: read over another, they would be told the other's.
        let whole = (0..self.roots.len())
            .filter(|&at| self.splits[at].is_none() && !self.tells_places(at))
            .map(|at| &self.roots[at]);
        whole.into_iter().find_map(|(loaded, base)| {
            let inside = loaded.strip_prefix(base).ok()?;
            let mut over = project.clone();
            for part in inside.components().rev() {
                if over.file_name() != Some(part.as_os_str()) {
                    return None;
                }
                over.pop();
            }
            let alike = (overlay == Overlay::Always || quick(base, &over))
                && self.builds_alike(base, &over);
            alike.then(|| View { base: base.clone(), over, project: loaded.clone() })
        })
    }

    /// The checkout that the one at `root` has packages of, when it has.
    fn shares_with(&self, root: &Path) -> Option<&Path> {
        let at = self.roots.iter().position(|it| it.1 == root)?;
        self.splits[at].as_ref().map(|it| &*it.base)
    }

    /// Whether what cargo and rustc did for the checkout at `base` is what they would do for
    /// the one at `over`: the same crates with the same features, and the same build scripts
    /// and proc macros, which the daemon has the output of and does not run.
    // FIXME: a build script or a proc macro that reads other files of its checkout is not
    // noticed, and neither is a crate outside the checkout that the manifests name by path.
    fn builds_alike(&self, base: &Path, over: &Path) -> bool {
        let alike = |path: &Path| {
            let Ok(inside) = path.strip_prefix(base) else { return true };
            std::fs::read(path).ok() == std::fs::read(over.join(inside)).ok()
        };
        let top = ["Cargo.lock", "Cargo.toml", ".cargo/config.toml", "rust-toolchain.toml"];
        if !top.iter().all(|it| alike(&base.join(it))) {
            return false;
        }
        let cargo = self.loaded.workspaces().iter().find_map(|it| match &it.kind {
            ProjectWorkspaceKind::Cargo { cargo, .. }
                if AsRef::<Path>::as_ref(cargo.workspace_root()) == base =>
            {
                Some(cargo)
            }
            _ => None,
        });
        let Some(cargo) = cargo else { return false };
        cargo.packages().map(|it| &cargo[it]).filter(|it| it.is_member).all(|package| {
            let manifest: &Path = package.manifest.as_ref();
            alike(manifest)
                && package.targets.iter().map(|&it| &cargo[it]).all(|target| {
                    let root: &Path = target.root.as_ref();
                    match target.kind {
                        TargetKind::BuildScript => alike(root),
                        TargetKind::Lib { is_proc_macro: true } => {
                            root.parent().is_some_and(|it| tree_alike(it, base, over))
                        }
                        _ => true,
                    }
                })
        })
    }

    /// Loads the checkouts of `projects` into `db`, next to those there are.
    fn add(
        &mut self,
        projects: &[&Path],
        db: &mut RootDatabase,
        vfs: &mut vfs::Vfs,
    ) -> anyhow::Result<()> {
        let mut workspaces = Vec::new();
        let mut roots = Vec::new();
        let (mut splits, mut debug) = (Vec::new(), Vec::new());
        let span = tracing::info_span!(target: "blaze", "load workspaces").entered();
        // A checkout like a loaded one shares the packages they have alike, unless told not to.
        let share = std::env::var_os("BLAZE_SPLIT").is_none_or(|it| it != "0");
        for project in projects {
            if share && let Some((base, over)) = self.alike(project) {
                let own = self.differing(base, &over);
                eprintln!("split {}: its own are {own:?}", over.display());
                workspaces.push(self.split(base, &over, &own));
                splits.push(Some(Split { base: self.roots[base].1.clone(), own }));
                debug.push(self.debug[base].clone());
                roots.push((dunce::canonicalize(project)?, over));
                continue;
            }
            splits.push(None);
            let beside = self.beside.as_mut().filter(|it| it.at == usize::MAX);
            let beside = beside.map(|it| {
                it.at = self.roots.len() + roots.len();
                it
            });
            // Cargo's lines are of the build of the project asked for first.
            let said =
                if Some(project) == projects.first() { self.scripts_said.take() } else { None };
            let (workspace, told) = workspace_at(
                project,
                &self.cargo_config,
                &self.load_config,
                &self.selection,
                beside,
                said,
            )?;
            debug.push(told);
            // The checkouts after the first run their build scripts where the first did. A
            // dependency is then told the same `OUT_DIR` in each, which it has to be to be one
            // crate for all.
            if let ProjectWorkspaceKind::Cargo { cargo, .. } = &workspace.kind
                && !matches!(
                    self.cargo_config.target_dir_config,
                    TargetDirectoryConfig::Directory(_)
                )
            {
                let target: &paths::Utf8Path = cargo.target_directory().as_ref();
                let scripts = self.cargo_config.target_dir_config.target_dir(Some(target));
                self.cargo_config.target_dir_config = TargetDirectoryConfig::Directory(
                    scripts.map_or_else(|| target.to_owned(), |it| it.into_owned()),
                );
            }
            let root = PathBuf::from(workspace.workspace_root().to_string());
            self.inputs.watch(&root, &workspace, &self.cargo_config);
            roots.push((dunce::canonicalize(project)?, root));
            workspaces.push(workspace);
        }
        drop(span);
        let span = tracing::info_span!(target: "blaze", "load files").entered();
        self.loaded.add(workspaces, &self.cargo_config.extra_env, &self.load_config, vfs, db)?;
        drop(span);
        *lock(&UNEDITED) = Unedited::of(self.loaded.workspaces());
        tracing::info_span!(target: "blaze", "macro sessions")
            .in_scope(|| prepare_macro_sessions(db));
        self.roots.extend(roots);
        self.splits.extend(splits);
        self.debug.extend(debug);
        Ok(())
    }

    /// The checkouts that are no longer what they were loaded from, for a build of
    /// `project` by a process with the variables `env`. A checkout loaded in full is one
    /// when its manifests, cargo's configuration or what its build scripts say changed, and
    /// when the build is of it and the process has other flags for rustc. One that shares
    /// packages with another is one when the build is of it and the two no longer build alike.
    fn stale(&mut self, project: &Path, env: &ide_db::base_db::Env) -> anyhow::Result<Vec<usize>> {
        let project = dunce::canonicalize(project).ok();
        let asked = self.roots.iter().position(|(it, _)| Some(it) == project.as_ref());
        // The checkout whose variables are those of the build: any, when it is not loaded.
        let built = match asked {
            Some(at) => Some(self.splits[at].as_ref().map_or(&self.roots[at].1, |it| &it.base)),
            None => None,
        };
        let mut known: FxHashMap<&Path, bool> = FxHashMap::default();
        let mut stale = Vec::new();
        for (at, (_, root)) in self.roots.iter().enumerate() {
            let changed = match &self.splits[at] {
                Some(split) => asked == Some(at) && !self.builds_alike(&split.base, root),
                None => match known.get(&**root) {
                    Some(&changed) => changed,
                    None => {
                        let env = (asked.is_none() || built == Some(root)).then_some(env);
                        let changed = self.inputs.stale(root, env)?;
                        known.insert(root, changed);
                        changed
                    }
                },
            };
            if changed {
                stale.push(at);
            }
        }
        Ok(stale)
    }

    /// Loads the checkouts at `stale` again, as a process with the variables `env` builds
    /// them. Every other keeps the workspace it has, and the crates that are the same in the
    /// workspace loaded again stay the crates they are. Nothing is changed when cargo turns
    /// one of them down. A checkout that shares packages with one of `stale` shares what is
    /// alike now, and is loaded in full when the two no longer build alike.
    fn reload(
        &mut self,
        stale: &[usize],
        env: &ide_db::base_db::Env,
        db: &mut RootDatabase,
        vfs: &mut vfs::Vfs,
    ) -> anyhow::Result<()> {
        let mut loaded = Vec::new();
        for &at in stale {
            loaded.push(self.loaded_again(at, env)?);
        }
        for (&at, (workspace, debug, config)) in stale.iter().zip(loaded) {
            self.place(at, workspace, debug, &config);
        }
        pause_mid_reload();
        let mut failed = Ok(());
        for at in 0..self.roots.len() {
            let Some(base) = self.splits[at].as_ref().map(|it| it.base.clone()) else { continue };
            let Some(base) = stale.iter().copied().find(|&it| self.roots[it].1 == base) else {
                continue;
            };
            let over = self.roots[at].1.clone();
            if !self.builds_alike(&self.roots[base].1, &over) {
                match self.loaded_again(at, env) {
                    Ok((workspace, debug, config)) => {
                        self.place(at, workspace, debug, &config);
                        continue;
                    }
                    Err(error) => failed = Err(error),
                }
            }
            let own = self.differing(base, &over);
            self.loaded.replace(at, self.split(base, &over, &own));
            self.splits[at] = Some(Split { base: self.roots[base].1.clone(), own });
            self.debug[at] = self.debug[base].clone();
        }
        self.loaded.add(Vec::new(), &self.cargo_config.extra_env, &self.load_config, vfs, db)?;
        *lock(&UNEDITED) = Unedited::of(self.loaded.workspaces());
        prepare_macro_sessions(db);
        failed
    }

    /// The workspace of the checkout at `at` as cargo has it now for a process with the
    /// variables `env`, what a build of it has a debugger told, and how it was loaded.
    fn loaded_again(
        &self,
        at: usize,
        env: &ide_db::base_db::Env,
    ) -> anyhow::Result<(ProjectWorkspace, plan::Whole, CargoConfig)> {
        let (project, root) = &self.roots[at];
        let mut config = self.cargo_config.clone();
        config.extra_env.extend(self.inputs.variables(root, env));
        let (workspace, debug) =
            workspace_at(project, &config, &self.load_config, &self.selection, None, None)?;
        Ok((workspace, debug, config))
    }

    /// Has the checkout at `at` be `workspace`, loaded in full with `config`.
    fn place(
        &mut self,
        at: usize,
        workspace: ProjectWorkspace,
        debug: plan::Whole,
        config: &CargoConfig,
    ) {
        let root = PathBuf::from(workspace.workspace_root().to_string());
        self.inputs.watch(&root, &workspace, config);
        self.loaded.replace(at, workspace);
        self.roots[at].1 = root;
        self.splits[at] = None;
        self.debug[at] = debug;
    }
}

/// Waits for `BLAZE_RELOAD_PAUSE_MS` where a reload has the new workspaces of its checkouts
/// and the crates are still the old ones, and says so: what `harness/buildmodel.py` holds a
/// reload at while it sends the requests that must not see the two mixed.
fn pause_mid_reload() {
    let pause = std::env::var("BLAZE_RELOAD_PAUSE_MS").ok().and_then(|it| it.parse().ok());
    if let Some(pause) = pause {
        eprintln!("reload: paused");
        std::thread::sleep(Duration::from_millis(pause));
    }
}

/// Waits for `BLAZE_LOAD_PAUSE_MS` where the workspace's files are read and their timestamps
/// not taken yet, and says so: what `harness/loadedits.py` writes a file at.
fn pause_after_load() {
    let pause = std::env::var("BLAZE_LOAD_PAUSE_MS").ok().and_then(|it| it.parse().ok());
    if let Some(pause) = pause {
        eprintln!("load: paused");
        std::thread::sleep(Duration::from_millis(pause));
    }
}

/// The absolute path `flag` names.
fn absolute(flag: &str, path: String) -> anyhow::Result<AbsPathBuf> {
    AbsPathBuf::try_from(path.as_str())
        .map_err(|it| anyhow::anyhow!("{flag} names a directory by its absolute path, not `{it}`"))
}

/// The directory cargo put the executables of the build in: the profile's directory the
/// libraries of the build are written below, `deps` in cargo's first layout and
/// `build/<package>/<hash>/out` in its second (`cargo::layout`). The standard library's,
/// in the toolchain, are below neither.
fn executables_of(symbol_files: &[PathBuf]) -> Option<AbsPathBuf> {
    let built = symbol_files.iter().find_map(|file| {
        let dir = file.parent()?;
        let first = dir.file_name().is_some_and(|name| name == "deps");
        match first {
            true => dir.parent(),
            false => cargo::layout::unit_dir(dir).filter(|it| it.dir.join("out") == dir)?.profile(),
        }
    })?;
    AbsPathBuf::try_from(built.to_str()?).ok()
}

/// The parts of a checkout that a build of it is told.
struct Parts {
    root: PathBuf,
    base: Option<PathBuf>,
    members: Arc<FxHashSet<ide_db::base_db::Crate>>,
    /// What the build has a debugger told.
    debug: plan::Described,
    lints_capped: bool,
    link_args: Vec<String>,
}

/// Whether the files of a package under `dir`, a directory of the checkout at `base`, are
/// those of the checkout at `over`. The packages in `nested` are not part of it,
/// and neither is what a build wrote.
fn package_alike(
    dir: &Path,
    base: &Path,
    over: &Path,
    nested: &FxHashSet<&Path>,
    compared: &Compared,
) -> bool {
    let Ok(inside) = dir.strip_prefix(base) else { return true };
    let theirs = over.join(inside);
    let skipped = |inside: &Path, name: &std::ffi::OsStr| {
        name == ".git"
            || inside.join(name).join("CACHEDIR.TAG").exists()
            || nested.contains(&*dir.join(name))
    };
    let names = |inside: &Path| {
        let mut names: Vec<_> = std::fs::read_dir(inside)
            .ok()?
            .filter_map(|it| Some(it.ok()?.file_name()))
            .filter(|name| !skipped(inside, name))
            .collect();
        names.sort_unstable();
        Some(names)
    };
    let Some(ours) = names(dir) else { return false };
    names(&theirs).is_some_and(|it| it == ours)
        && ours.iter().all(|name| {
            let (path, their) = (dir.join(name), theirs.join(name));
            if path.is_dir() {
                return package_alike(&path, base, over, nested, compared);
            }
            let written = |path: &Path| {
                let file = std::fs::metadata(path).ok()?;
                Some((file.len(), file.modified().ok()?))
            };
            let (Some(ours), Some(theirs)) = (written(&path), written(&their)) else {
                return false;
            };
            if ours.0 != theirs.0 {
                return false;
            }
            let known = compared.lock().unwrap_or_else(|e| e.into_inner()).get(&their).copied();
            if let Some((_, _, alike)) = known.filter(|it| (it.0, it.1) == (ours, theirs)) {
                return alike;
            }
            let alike = match (std::fs::read(&path), std::fs::read(&their)) {
                (Ok(ours), Ok(theirs)) => {
                    // Code that asks where its package is does not say the same in two
                    // checkouts.
                    let asks = path.extension().is_some_and(|it| it == "rs")
                        && memchr::memmem::find_iter(&ours, b"env!(").any(|at| {
                            ours[at + 5..].trim_ascii_start().starts_with(b"\"CARGO_MANIFEST_DIR\"")
                        });
                    ours == theirs && !asks
                }
                _ => false,
            };
            compared.lock().unwrap_or_else(|e| e.into_inner()).insert(their, (ours, theirs, alike));
            alike
        })
}

/// Whether the files under `dir`, a directory of the checkout at `base`, are those of the
/// checkout at `over`.
fn tree_alike(dir: &Path, base: &Path, over: &Path) -> bool {
    let Ok(inside) = dir.strip_prefix(base) else { return true };
    let names = |dir: &Path| {
        let mut names: Vec<_> =
            std::fs::read_dir(dir).ok()?.filter_map(|it| Some(it.ok()?.file_name())).collect();
        names.sort_unstable();
        Some(names)
    };
    let theirs = over.join(inside);
    let Some(ours) = names(dir) else { return false };
    names(&theirs).is_some_and(|it| it == ours)
        && ours.iter().all(|name| {
            let path = dir.join(name);
            if path.is_dir() {
                tree_alike(&path, base, over)
            } else {
                std::fs::read(&path).ok() == std::fs::read(theirs.join(name)).ok()
            }
        })
}

/// A checkout that is a loaded one where its files are the same: the packages that are the
/// same in both are the crates of the loaded one, and only the others are crates of its own.
#[derive(Clone, PartialEq)]
struct Split {
    /// The root of the loaded checkout.
    base: PathBuf,
    /// The directories of the packages it has other files in, as they are called in both.
    own: BTreeSet<PathBuf>,
}

/// When a checkout that is not loaded is read as a loaded one with other files.
#[derive(Clone, Copy, PartialEq)]
enum Overlay {
    Never,
    /// For as long as taking in what differs is quick: the checkouts read as one are built
    /// in turn, so one that keeps taking long is loaded with its own crates instead.
    Quick,
    Always,
}

/// A checkout as the daemon looks at it.
struct View {
    /// The loaded checkout it is built as.
    base: PathBuf,
    /// Where its files are read from: `base`, or a checkout that is `base` with other contents.
    over: PathBuf,
    /// The project asked for, in `base`.
    project: PathBuf,
}

fn workspace_at(
    project: &Path,
    cargo_config: &CargoConfig,
    load_config: &LoadCargoConfig,
    selection: &plan::Selection,
    beside: Option<&mut Beside>,
    said: Option<Vec<String>>,
) -> anyhow::Result<(ProjectWorkspace, plan::Whole)> {
    let _load = plan::Load::start();
    let span = tracing::info_span!(target: "blaze", "cargo metadata").entered();
    let root = AbsPathBuf::assert_utf8(std::env::current_dir()?.join(project));
    let manifest = ProjectManifest::discover_single(&root)?;
    let mut workspace = ProjectWorkspace::load(manifest.clone(), cargo_config, &|_| {})?;
    let described: &Path = manifest.manifest_path().as_ref();
    if workspace.is_json()
        && selection.target.as_deref() == Some(cargo::own_macro::WASM_TARGET)
        && let Some(overrides) =
            cargo::own_macro::host_side_cfg(described).map_err(anyhow::Error::msg)?
    {
        workspace.cfg_overrides = overrides;
    }
    drop(span);
    let span = tracing::info_span!(target: "blaze", "plan").entered();
    // Without the standard library's source every call into it is unresolved, and each
    // function making one would be refused for a reason that says nothing of the toolchain.
    if workspace.sysroot.is_rust_lib_src_empty() {
        // A library with its `Cargo.toml` is there and could not be read; without one it is
        // not installed, whatever the stitched library's error says of it.
        let read = workspace
            .sysroot
            .rust_lib_src_root()
            .is_some_and(|it| std::fs::metadata(it.join("Cargo.toml")).is_ok());
        if let Some(error) = workspace.sysroot.error().filter(|_| read) {
            anyhow::bail!("{}", error.trim_end());
        }
        let named = std::env::var(cargo::toolchain::library::VAR).unwrap_or_default();
        let named = match named.is_empty() {
            true => "blaze".to_owned(),
            false => format!("{} (`{}`)", named, cargo::toolchain::library::VAR),
        };
        anyhow::bail!(
            "{named} has no source of the standard library, which blaze compiles from: \
             `blaze toolchain` downloads blaze's, digest {} ({})",
            cargo::toolchain::library::digest(),
            cargo::toolchain::library::why_not()
        );
    }
    let mut debug = plan::Whole {
        debug: plan::Described::NOTHING,
        lto: false,
        links_dead_code: false,
        lints_capped: false,
        link_args: Vec::new(),
    };
    let mut scripts = None;
    if let ProjectWorkspaceKind::Cargo { cargo, .. } = &mut workspace.kind {
        if !selection.dev_deps {
            cargo.drop_dev_dependencies();
        }
        // `cargo metadata` unifies features over every member of the workspace and knows no
        // profile. The build of this one package has both.
        let dir = manifest.manifest_path().parent();
        let triple = project_model::toolchain_info::target_tuple::get(
            project_model::toolchain_info::QueryConfig::Rustc(&workspace.sysroot, dir.as_ref()),
            selection.target.as_deref(),
            &cargo_config.extra_env,
        )?;
        let triple = triple.first().context("no target to build for")?;
        let asked = plan::Asked { triple, cfg: &workspace.rustc_cfg, env: &cargo_config.extra_env };
        let plan = plan::Plan::of(manifest.manifest_path().as_ref(), selection, &asked)?;
        debug = plan.whole()?;
        hir_ty_codegen::rustc_links_dead_code(debug.links_dead_code);
        let packages: Vec<_> = cargo.packages().collect();
        if let Some(beside) = beside {
            let built = packages.iter().filter(|it| plan.package(&cargo[**it].id.repr).is_some());
            beside.awaits(cargo, built.copied());
            // Nothing cargo has yet to say is waited for here: what there is of it so far
            // is what the crates that need no more are read with.
            let (lines, _) = beside.said.so_far();
            beside.applied = lines.len();
            scripts =
                Some(WorkspaceBuildScripts::of_messages(cargo, lines.iter().map(String::as_str)));
        }
        let target = plan.target_profile();
        for package in packages {
            let built = plan.package(&cargo[package].id.repr);
            // `cargo metadata` has one graph for the host and the target, so a package built
            // only for a build script or proc macro, or not at all, can still be a dependency
            // of one of the program's: as such it is built as they are.
            let profile = built.filter(|it| it.for_target).map(|it| &it.profile).or(target);
            if let Some(profile) = profile {
                cargo.set_profile_cfg(package, profile.cfg());
            }
            let Some(built) = built else { continue };
            cargo.set_active_features(package, built.features.iter().cloned().collect());
            cargo.set_primary(package, built.primary);
        }
    }
    drop(span);
    let _span = tracing::info_span!(target: "blaze", "build scripts").entered();
    if let Some(scripts) = scripts {
        workspace.set_build_scripts(scripts);
    } else if load_config.load_out_dirs_from_check {
        let build_scripts = match (&workspace.kind, said) {
            // Cargo's build that is over said it all: the scripts it ran said what they did,
            // and it built the proc macros where they are.
            (ProjectWorkspaceKind::Cargo { cargo, .. }, Some(said)) => {
                WorkspaceBuildScripts::of_messages(cargo, said.iter().map(String::as_str))
            }
            (kind, _) => {
                // blaze's own target directory is one per mode of its proc macros
                // (`own_macro::own_dir`): with them, cargo is given empty files for the
                // libraries, which rustc would read when they are turned off. blaze's own
                // build of them (`cargo/compile_time.rs`) reads nothing of cargo's.
                let own_config;
                let mut config = cargo_config;
                if let ProjectWorkspaceKind::Cargo { cargo, .. } = kind
                    && cargo_config.target_dir_config == TargetDirectoryConfig::UseSubdirectory
                    && cargo_config.own_build_scripts.is_none()
                {
                    let target = cargo.target_directory();
                    let own = target.join(cargo::own_macro::own_dir());
                    seed_own_target(target.as_ref(), own.as_ref());
                    own_config = CargoConfig {
                        target_dir_config: TargetDirectoryConfig::Directory(own.into()),
                        ..cargo_config.clone()
                    };
                    config = &own_config;
                }
                events::within(events::Step::BuildScripts, None, || {
                    workspace.run_build_scripts(config, &|_| {})
                })?
            }
        };
        // A crate whose script did not run is read without what it writes: thiserror's
        // `include!` of its `OUT_DIR` then leaves every derived `Error` unimplemented, and a
        // daemon that loaded that builds wrongly for as long as it runs.
        if let Some(error) = build_scripts.error() {
            anyhow::bail!("a build script failed: {}", error.trim().replace('\n', " "));
        }
        workspace.set_build_scripts(build_scripts);
    }
    Ok((workspace, debug))
}

/// Starts blaze's own target directory from what cargo built in `target`, so that the build
/// scripts and proc macros cargo has are not built again. Without it the first load is a
/// second build of those.
fn seed_own_target(target: &Path, own: &Path) {
    if own.exists() {
        return;
    }
    let mut seeding = own.as_os_str().to_owned();
    seeding.push(format!(".{}", std::process::id()));
    let seeding = PathBuf::from(seeding);
    // cargo's first layout has `.fingerprint`, `build` and `deps` in a profile's directory;
    // its second has every unit in `build/<package>/<hash>`, with `fingerprint`, `out` and a
    // build script's `run` in it.
    let second = |profile: &Path| profile.join("build").is_dir() && !profile.join("deps").exists();
    let profiles = |dir: &Path| -> Vec<PathBuf> {
        let dirs = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|it| it.path());
        dirs.filter(|it| it.join(".fingerprint").is_dir() || second(it)).collect()
    };
    let mut found = profiles(target);
    for triple in std::fs::read_dir(target).into_iter().flatten().flatten() {
        // blaze's own directories, of either mode, and one being seeded, are not cargo's.
        let name = triple.file_name();
        if name.to_str().is_some_and(|it| it.starts_with(cargo::own_macro::OWN_DIR)) {
            continue;
        }
        found.extend(profiles(&triple.path()));
    }
    let entries = |dir: &Path| -> Vec<std::fs::DirEntry> {
        std::fs::read_dir(dir).into_iter().flatten().flatten().collect()
    };
    for profile in found {
        let Ok(within) = profile.strip_prefix(target) else { continue };
        let to = seeding.join(within);
        // A fingerprint says its artifact is whole, so the fingerprints are read before the
        // artifacts are: cargo may be building here.
        if !second(&profile) {
            _ = copy_tree(&profile.join(".fingerprint"), &to.join(".fingerprint"), false);
            // A build script writes to its directory in place; rustc and the linker replace
            // what they write, as cargo's own links out of `deps` rely on.
            _ = copy_tree(&profile.join("build"), &to.join("build"), false);
            _ = copy_tree(&profile.join("deps"), &to.join("deps"), true);
            continue;
        }
        let units: Vec<PathBuf> = entries(&profile.join("build"))
            .iter()
            .flat_map(|package| entries(&package.path()))
            .map(|unit| unit.path())
            .collect();
        for unit in &units {
            let Ok(unit_within) = unit.strip_prefix(&profile) else { continue };
            _ = copy_tree(
                &unit.join("fingerprint"),
                &to.join(unit_within).join("fingerprint"),
                false,
            );
        }
        for unit in &units {
            let Ok(unit_within) = unit.strip_prefix(&profile) else { continue };
            for entry in entries(unit) {
                let name = entry.file_name();
                if name == "fingerprint" {
                    continue;
                }
                let to = to.join(unit_within).join(&name);
                match entry.file_type().is_ok_and(|it| it.is_dir()) {
                    // A script's `out` and `run` are written in place; a library's `out` is
                    // what rustc replaces, which can be linked to.
                    true => {
                        _ = copy_tree(
                            &entry.path(),
                            &to,
                            name == "out" && !unit.join("run").exists(),
                        )
                    }
                    false => {
                        _ = to.parent().map(std::fs::create_dir_all);
                        _ = std::fs::copy(entry.path(), to);
                    }
                }
            }
        }
    }
    if std::fs::rename(&seeding, &own).is_err() {
        _ = std::fs::remove_dir_all(&seeding);
    }
}

fn copy_tree(from: &Path, to: &Path, link: bool) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (from, to) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type()?.is_dir() {
            copy_tree(&from, &to, link)?;
        } else if !link || std::fs::hard_link(&from, &to).is_err() {
            std::fs::copy(&from, &to)?;
            // Cargo takes what is newer than its fingerprint to have changed.
            let modified = entry.metadata()?.modified()?;
            std::fs::File::options().write(true).open(&to)?.set_modified(modified)?;
        }
    }
    Ok(())
}

/// What the checkouts loaded have in common, and what holding them costs.
fn report_sharing(db: &RootDatabase) {
    let crates = Crate::all(db);
    let local = crates.iter().filter(|it| it.origin(db).is_local()).count();
    eprintln!(
        "sharing: {} crates, {local} of the checkouts' own, {} MB",
        crates.len(),
        memory::resident().unwrap_or(0)
    );
}

fn resolve_crates(db: &RootDatabase, args: &Args, threads: usize) -> anyhow::Result<usize> {
    let crates = built_crates(db, args)?;
    resolve_all(db, &crates, threads)?;
    Ok(crates.len())
}

/// The crates a build of `args` compiles and every crate they depend on, each after its
/// dependencies.
fn built_crates(db: &RootDatabase, args: &Args) -> anyhow::Result<Vec<ide_db::base_db::Crate>> {
    let mut seen = FxHashSet::default();
    let mut crates = Vec::new();
    for name in args.crates() {
        for krate in crates_named(db, name, args)? {
            let krate: ide_db::base_db::Crate = krate.into();
            crates.extend(krate.transitive_deps(db).into_iter().filter(|&dep| seen.insert(dep)));
        }
    }
    Ok(crates)
}

/// Resolves the names of `crates` again after an edit to many files, on every core: whoever
/// asks first otherwise reads every file that changed on one.
fn resolve_names(db: &RootDatabase, crates: &[ide_db::base_db::Crate]) {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
    std::thread::scope(|scope| {
        for _ in 0..threads.min(crates.len()) {
            let (db, next) = (db.clone(), &next);
            scope.spawn(move || {
                while let Some(&krate) =
                    crates.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
                {
                    hir::attach_db(&db, || _ = hir::crate_def_map(&db, krate));
                }
            });
        }
    });
}

/// Establishes declaration-order macro state before incremental queries validate bodies.
fn prepare_macro_sessions(db: &RootDatabase) {
    hir::attach_db(db, || {
        for &krate in &*ide_db::base_db::all_crates(db) {
            if hir_def::nameres::may_have_session_macros(db, krate) {
                _ = hir::crate_def_map(db, krate);
            }
        }
    });
}

/// Resolves the names and finds the impls of `crates`, on `threads`: each crate is a node of a
/// build graph (docs/driver.md) that runs once the crates it depends on are resolved, so no
/// thread waits inside a query for another's. While fewer crates are ready than there are
/// threads, the idle ones help with a crate's impls and files.
fn resolve_all(
    db: &RootDatabase,
    crates: &[ide_db::base_db::Crate],
    threads: usize,
) -> anyhow::Result<()> {
    use blaze_graph::{Config, Machine, NodeId, Pool, Spec, exec};
    use blaze_protocol::events::NodeKind;
    let first = exec::ids(crates.len());
    let id = |at: usize| NodeId(first + at as u32);
    let index: FxHashMap<ide_db::base_db::Crate, usize> =
        crates.iter().enumerate().map(|(at, &krate)| (krate, at)).collect();
    let nodes = crates
        .iter()
        .enumerate()
        .map(|(at, &krate)| {
            let mut deps: Vec<NodeId> = krate
                .data(db)
                .dependencies
                .iter()
                .filter_map(|dep| index.get(&dep.crate_id))
                .map(|&it| id(it))
                .collect();
            deps.sort();
            deps.dedup();
            let name = krate.extra_data(db).display_name.as_ref();
            let name = name.map(|it| it.to_string()).unwrap_or_default();
            Spec { id: id(at), kind: NodeKind::Names, name, deps, pool: Pool::Work, cost: 1 }
        })
        .collect();
    let threads = threads.max(1);
    let mut machine = Machine::new(Config { io: 1, work: threads, keep_going: false })?;
    // The threads at work: on a crate, or helping with one.
    let busy = std::sync::atomic::AtomicUsize::new(0);
    let counting = events::Counting::new(events::Step::Resolve, crates.len(), threads);
    // The slot a crate under way says what it works on in: at most `threads` are under way.
    let free: Mutex<Vec<usize>> = Mutex::new((0..threads).rev().collect());
    // `RootDatabase` is `Send` but not `Sync`: each node clones its own from this one.
    let shared = Mutex::new(db.clone());
    let work = |node: NodeId| {
        let krate = crates[(node.0 - first) as usize];
        let db = lock(&shared).clone();
        let slot = lock(&free).pop().unwrap_or_default();
        busy.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        counting.on(slot, || crate_shown(&db, krate));
        events::crate_is(krate, events::CrateState::Resolving);
        hir::attach_db(&db, || resolve_one(&db, krate, &busy, threads));
        events::crate_is(krate, events::CrateState::Resolved);
        counting.done(slot);
        busy.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        lock(&free).push(slot);
        blaze_graph::exec::Did::built()
    };
    // The crates' nodes are said by `events::graph`, with the names the build's view shows.
    let ok = exec::drive(&mut machine, nodes, &work, &mut |_| {}, None)?;
    anyhow::ensure!(ok, "resolving the names of a crate failed");
    Ok(())
}

/// Resolves the names and finds the impls of `krate`, whose dependencies are resolved.
fn resolve_one(
    db: &RootDatabase,
    krate: ide_db::base_db::Crate,
    busy: &std::sync::atomic::AtomicUsize,
    threads: usize,
) {
    let before = memory::live();
    let subject = || crate_shown(db, krate);
    tracing::info_span!(target: "blaze", "resolve names", subject = %subject()).in_scope(|| {
        _ = hir::crate_def_map(db, krate);
    });
    let _span =
        tracing::info_span!(target: "blaze", "resolve impls", subject = %subject()).entered();
    let frozen = hir_ty::method_resolution::frozen::served(db, krate);
    if !frozen {
        warm_impls(db, krate, busy, threads);
    }
    _ = hir::TraitImpls::for_crate(db, krate);
    // Which of its dependencies the crate names is read off its files, and the first body
    // inferred would wait for that of every crate.
    _ = hir::TraitImpls::for_crate_and_deps(db, krate);
    if frozen {
        warm_files(db, krate, busy, threads);
    } else if krate.data(db).origin.is_lang() {
        // A build asks which functions written in the bodies of the standard library share a
        // path, which every body of the crate is lowered for: threads are idle here, waiting
        // for the crates that come next.
        _ = declared_in(db, krate.into());
    }
    // What the heap grew by is this crate's when one thread resolves.
    let grew = memory::live().zip(before).map(|(now, before)| now as i64 - before as i64);
    if let Some(grew) = grew.filter(|_| std::env::var_os("BLAZE_STATS").is_some()) {
        let origin = &krate.data(db).origin;
        eprintln!("resolved {}: {grew:+} bytes {origin:?}", subject());
    }
}

/// A crate as a person names it: its name, or `std` for the standard library's.
fn crate_shown(db: &RootDatabase, krate: ide_db::base_db::Crate) -> String {
    match krate.data(db).origin.is_lang() {
        true => "std".to_owned(),
        false => krate
            .extra_data(db)
            .display_name
            .as_ref()
            .map_or_else(|| "?".to_owned(), |it| it.to_string()),
    }
}

/// Does what finding the impls of `krate` asks of each impl and each body that may hold one,
/// with the threads no other crate has helping: the crates late in a chain of dependencies are
/// resolved one at a time, and the other cores are idle then.
fn warm_impls(
    db: &RootDatabase,
    krate: ide_db::base_db::Crate,
    busy: &std::sync::atomic::AtomicUsize,
    threads: usize,
) {
    let work = hir_ty::method_resolution::impl_work(db, krate);
    on_idle_threads(db, &work, 64, busy, threads, |db, work| work.run(db));
}

/// Asks for the impls of each of `crates` on every core, without waiting for it. After an edit
/// the first type check asks for those of every crate it sees, one crate after another, and
/// the other threads wait for it: an impl may be written in a body, so each crate's must be
/// verified again.
fn verify_impls<'scope>(
    scope: &'scope std::thread::Scope<'scope, '_>,
    db: &RootDatabase,
    crates: &FxHashSet<ide_db::base_db::Crate>,
) {
    use hir_ty::method_resolution::{InherentImpls, TraitImpls};
    let crates: Arc<[ide_db::base_db::Crate]> = crates.iter().copied().collect();
    let next = Arc::new(AtomicUsize::new(0));
    let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
    for _ in 0..threads.min(crates.len()) {
        let (db, crates, next) = (db.clone(), crates.clone(), next.clone());
        scope.spawn(move || {
            // A query that is cancelled unwinds, and those that wait for it see that.
            _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                hir::attach_db(&db, || {
                    let at = || next.fetch_add(1, Ordering::Relaxed);
                    while let Some(&krate) = crates.get(at()) {
                        _ = TraitImpls::for_crate(&db, krate);
                        _ = InherentImpls::for_crate(&db, krate);
                    }
                })
            }));
        });
    }
}

/// Parses the files of the modules of `krate`, whose names and impls were read rather than
/// found, with the threads no other crate has: a signature or body of the crate that a build
/// lowers parses its whole file first, one file after another.
fn warm_files(
    db: &RootDatabase,
    krate: ide_db::base_db::Crate,
    busy: &std::sync::atomic::AtomicUsize,
    threads: usize,
) {
    let def_map = hir_def::nameres::crate_def_map(db, krate);
    let mut seen = FxHashSet::default();
    let files: Vec<_> = def_map
        .modules(db)
        .filter_map(|(_, data)| data.origin.file_id())
        .filter(|&file| seen.insert(file))
        .collect();
    on_idle_threads(db, &files, 4, busy, threads, |db, &file| {
        _ = hir::HirFileId::from(file).ast_id_map(db);
    });
}

/// Runs `run` on each of `work` here and on the threads no other crate has, one for each
/// `per` items. `busy` counts the threads at work, this one among them.
fn on_idle_threads<T: Sync>(
    db: &RootDatabase,
    work: &[T],
    per: usize,
    busy: &std::sync::atomic::AtomicUsize,
    threads: usize,
    run: impl Fn(&RootDatabase, &T) + Sync,
) {
    use std::sync::atomic::Ordering::Relaxed;
    let wanted = (work.len() / per).min(threads - 1);
    let mut helpers = 0;
    _ = busy.try_update(Relaxed, Relaxed, |now| {
        helpers = wanted.min(threads.saturating_sub(now));
        Some(now + helpers)
    });
    if helpers == 0 {
        return;
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let each = |db: &RootDatabase| {
        while let Some(item) = work.get(next.fetch_add(1, Relaxed)) {
            run(db, item);
        }
    };
    std::thread::scope(|scope| {
        for _ in 0..helpers {
            let db = db.clone();
            let each = &each;
            scope.spawn(move || hir::attach_db(&db, || each(&db)));
        }
        each(db);
    });
    busy.fetch_sub(helpers, Relaxed);
}

/// The functions of the crates being built, listed once and again for the crates an edit can
/// have changed the items of.
#[derive(Default)]
struct Listed {
    functions: std::sync::Mutex<FxHashMap<Crate, std::sync::Arc<ListedCrate>>>,
    /// Every crate that was listed or that a build began, also once an edit has it listed
    /// again.
    built: std::sync::Mutex<FxHashSet<Crate>>,
    /// What [`Listed::in_files`] found written in a file, by its [`outline::skeleton`] then.
    files: std::sync::Mutex<FxHashMap<vfs::FileId, (u64, Vec<FunctionId>)>>,
    /// What [`hir::named_dependencies`] found of each crate asked about.
    named: std::sync::Mutex<FxHashMap<Crate, Arc<[Crate]>>>,
}

struct ListedCrate {
    functions: Vec<FunctionId>,
    /// What [`Resolver::paths_shared_in`] found, by the symbols and the checkout it was for.
    shared_paths: std::sync::Mutex<Vec<(SharedKey, Arc<std::sync::OnceLock<FxHashSet<String>>>)>>,
    /// The statics a linker or a runtime finds though no code names them.
    kept: Vec<StaticId>,
    /// Every static, in a body or not.
    statics: Vec<StaticId>,
    /// See [`Declared::in_provided`].
    in_provided: Vec<FunctionId>,
    /// For each, whether a runtime looks it up by name.
    exported: Vec<bool>,
    /// What a build takes of `functions` for its roots, each with whether a runtime looks it up
    /// by name and whether it is generic: those only the const evaluator runs are left out.
    /// Kept with the crate, as a query of the attributes of every function of every crate is
    /// most of what an edit would otherwise wait for.
    rooted: Vec<(FunctionId, bool, bool)>,
    /// The same of `in_provided`: each with whether it is generic.
    provided: Vec<(FunctionId, bool)>,
    main: Option<FunctionId>,
}

/// Whether the function, or the `impl` it is in, takes types or constants. A method a trait
/// provides takes `Self`.
fn is_generic_function(db: &RootDatabase, func: FunctionId) -> bool {
    let generic = |def: hir::GenericDef| !def.type_or_const_params(db).is_empty();
    if generic(hir::Function::from(func).into()) {
        return true;
    }
    match func.lookup(db).container {
        ItemContainerId::ImplId(imp) => generic(hir::Impl::from(imp).into()),
        ItemContainerId::TraitId(_) => true,
        _ => false,
    }
}

fn is_bodiless_intrinsic(db: &RootDatabase, id: FunctionId) -> bool {
    let signature = hir_def::signatures::FunctionSignature::of(db, id);
    signature.flags.contains(hir_def::signatures::FnFlags::RUSTC_INTRINSIC) && !signature.has_body()
}

/// Whether `func` is of blaze's test harness.
fn in_harness(db: &RootDatabase, func: FunctionId) -> bool {
    let module = hir::Function::from(func).module(db);
    module.name(db).is_some_and(|it| it.as_str() == test_harness::MODULE)
        && module.parent(db).is_some_and(|it| it.is_crate_root(db))
}

/// The `main` of blaze's test harness among the functions of a crate, when it has one.
fn harness_main(db: &RootDatabase, functions: &[FunctionId]) -> Option<FunctionId> {
    functions
        .iter()
        .copied()
        .find(|&it| hir::Function::from(it).name(db).as_str() == "main" && in_harness(db, it))
}

/// Whether the program of `args` is one of tests, which starts in blaze's test harness where
/// blaze writes one: a unit test, built with `cfg(test)`, or one the plan says is a test.
fn of_tests(args: &Args) -> bool {
    args.unit_test || args.out.parent().is_some_and(|it| it.join(cargo::plan::TESTS).is_file())
}

/// Why the program of `args` is right only with blaze's test harness, when the plan says it
/// is: rustc's calls copies of the tests the daemon's can't take the place of.
fn harness_alone(args: &Args) -> Option<String> {
    let file = args.out.parent()?.join(cargo::plan::TESTS);
    std::fs::read_to_string(file).ok().filter(|it| !it.is_empty())
}

/// Writes [`cargo::plan::REACH`] beside the program of tests of `args` when a client asked for
/// it, and takes away the one of an older build: `built` is whether this build is the program.
fn write_reach(db: &RootDatabase, table: &SymbolTable, listed: &Listed, args: &Args, built: bool) {
    let Some(dir) = args.out.parent() else { return };
    let file = dir.join(cargo::plan::REACH);
    if !built || !dir.join(cargo::plan::REACH_WANTED).is_file() {
        _ = std::fs::remove_file(&file);
        return;
    }
    let _span = tracing::info_span!(target: "blaze", "write tests reach").entered();
    let text = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match of_tests(args) {
        true => hir::attach_db(db, || {
            let contents = content::Contents::default();
            match Resolver::of(db, table, args, &contents, listed) {
                Ok(resolver) => reach_of_tests(db, &resolver, listed, args),
                Err(error) => format!("*\t{error:#}\n"),
            }
        }),
        // A test target cargo builds without libtest's harness says what it runs itself.
        false => "*\tit is not built with libtest's harness\n".to_owned(),
    }));
    let text = match text {
        Ok(text) => text,
        Err(panic) if panic.is::<ide_db::base_db::salsa::Cancelled>() => {
            std::panic::resume_unwind(panic)
        }
        Err(panic) => format!("*\tblaze panicked: {}\n", panic_message(&*panic)),
    };
    let writing = dir.join(format!("{}.writing", cargo::plan::REACH));
    let by = dir.join(cargo::plan::REACH_BY);
    let written = std::fs::write(&by, reach_by())
        .and_then(|()| std::fs::write(&writing, text))
        .and_then(|()| std::fs::rename(&writing, &file));
    if written.is_err() {
        _ = std::fs::remove_file(&file);
    }
}

/// Whether the program whose object is `out` is to be built again for its
/// [`cargo::plan::REACH`] alone: a client asked for it since the program was last built, or
/// another build of blaze wrote it.
fn reach_owed(out: &Path) -> bool {
    out.parent().is_some_and(|dir| {
        dir.join(cargo::plan::REACH_WANTED).is_file()
            && (!dir.join(cargo::plan::REACH).is_file()
                || std::fs::read_to_string(dir.join(cargo::plan::REACH_BY))
                    .map_or(true, |it| it != reach_by()))
    })
}

/// What [`cargo::plan::REACH_BY`] says of this build of blaze.
fn reach_by() -> String {
    blaze_store::build_name().unwrap_or_default()
}

/// The text of [`cargo::plan::REACH`] for the program of tests of `args`. A test's hash is of
/// its attributes, which say how it is to end, as well as its code and all it reaches.
fn reach_of_tests(
    db: &RootDatabase,
    resolver: &Resolver<'_>,
    listed: &Listed,
    args: &Args,
) -> String {
    use hir::HasSource as _;
    use sha2::Digest as _;
    use syntax::ast::HasAttrs as _;
    let crates = crates_named(db, &args.krate, args).unwrap_or_default();
    let Some(&krate) = crates.first() else { return String::new() };
    let functions = &listed.of(db, krate).functions;
    let tests: Vec<FunctionId> =
        functions.iter().copied().filter(|&it| hir::Function::from(it).is_test(db)).collect();
    // `cargo test` runs a `#[bench]` once as a test.
    let benches = functions.iter().map(|&it| hir::Function::from(it)).filter(|it| it.is_bench(db));
    let mut text = format!("{}\n", cargo::plan::REACH_FORMAT);
    for bench in benches {
        let Some(name) = test_harness::name(db, krate, bench) else { continue };
        text.push_str(&format!("-\t{name}\tit is a benchmark\n"));
    }
    let reaches = tracing::info_span!(target: "blaze", "tests reach")
        .in_scope(|| hir_ty_codegen::tests_reach(db, resolver, &tests));
    for (&id, reach) in tests.iter().zip(reaches) {
        let function = hir::Function::from(id);
        let Some(name) = test_harness::name(db, krate, function) else { continue };
        let attrs = function.source(db).map(|it| {
            let attrs = it.value.attrs().map(|it| it.syntax().text().to_string());
            attrs.collect::<Vec<_>>().join("\n")
        });
        let identity = format!("{:032x}\n{}", reach.hash, attrs.unwrap_or_default());
        let identity = sha2::Sha256::digest(identity.as_bytes());
        let hex: String = identity.iter().map(|it| format!("{it:02x}")).collect();
        let blocking = reach.found.iter().filter(|it| it.kind.blocks());
        let recorded =
            blocking.clone().all(|it| cargo::plan::recorded_by_a_sandbox(it.kind.name()));
        let mark = match (reach.hermetic(), recorded) {
            (true, _) => "",
            (false, true) => "~",
            (false, false) => "!",
        };
        let found: Vec<cargo::plan::Reason> = reach
            .found
            .iter()
            .map(|it| cargo::plan::Reason {
                kind: it.kind.name().to_owned(),
                what: it.what.clone(),
                why: it.why.clone(),
                chain: it
                    .path
                    .iter()
                    .map(|call| cargo::plan::Call::new(call.name.clone(), call.at.as_deref()))
                    .collect(),
            })
            .collect();
        let (blocking, others): (Vec<_>, Vec<_>) = found.into_iter().partition(|it| it.blocks());
        let json = |it: &[cargo::plan::Reason]| {
            serde_json::to_string(it).unwrap_or_else(|_| "[]".to_owned())
        };
        text.push_str(&format!("{mark}{hex}\t{name}\t{}\t{}\n", json(&blocking), json(&others)));
    }
    text
}

/// The text of the root file of the crate of each program of tests of `programs` with blaze's
/// test harness as the tests are now at its end, or without it where blaze writes none, for
/// those that differ from the text they have. Only the crates among `of` when it is given.
/// A proc-macro crate linked as a shared library has the list of its macros there instead
/// ([`macro_list`]).
fn harnesses<'a>(
    db: &RootDatabase,
    programs: impl IntoIterator<Item = &'a Args>,
    of: Option<&FxHashSet<Crate>>,
) -> Vec<(vfs::FileId, String)> {
    let mut texts: Vec<(vfs::FileId, String)> = Vec::new();
    for args in programs.into_iter().filter(|it| of_tests(it) || it.shared) {
        for krate in crates_named(db, &args.krate, args).unwrap_or_default().into_iter().take(1) {
            let file = krate.root_file(db);
            if of.is_some_and(|it| !it.contains(&krate)) || texts.iter().any(|it| it.0 == file) {
                continue;
            }
            let text = db.file_text(file).text(db).clone();
            if !of_tests(args) {
                let data = ide_db::base_db::Crate::from(krate).data(db);
                if !data.target_kind.is_proc_macro() {
                    continue;
                }
                let wasm =
                    args.selection.target.as_deref().is_some_and(|it| it.starts_with("wasm"));
                let module = hir::attach_db(db, || macro_list::module(db, krate, wasm));
                let wanted = format!("{}{module}", test_harness::without(&text));
                if *text != *wanted {
                    texts.push((file, wanted));
                }
                if let Some((file, shim)) = wasm.then(|| macro_list::shim(db, krate)).flatten()
                    && **db.file_text(file).text(db) != *shim
                {
                    texts.push((file, shim));
                }
                continue;
            }
            let module = hir::attach_db(db, || {
                let functions = krate.modules(db).into_iter().flat_map(|it| it.declarations(db));
                let functions = functions.filter_map(|it| match it {
                    ModuleDef::Function(it) => Some(it),
                    _ => None,
                });
                let name = |file| source_path(db, &args.checkout, args.base.as_deref(), file);
                test_harness::module(db, krate, functions, &name)
            });
            let module = module.map(|it| it.text).unwrap_or_default();
            // rustc empties a crate whose root's `#![cfg(...)]` is false, its crate attributes
            // too, and then writes its harness: such a root is the harness alone, with no tests,
            // or the `#![cfg]` would leave the program without the harness's `main` too.
            let configured_out = hir::attach_db(db, || krate.is_configured_out(db));
            let kept = if configured_out { "" } else { test_harness::without(&text) };
            let wanted = format!("{kept}{module}");
            if *text != *wanted {
                texts.push((file, wanted));
            }
        }
    }
    texts
}

/// Gives the files the texts [`harnesses`] wrote. The files.
fn write_harnesses(db: &mut RootDatabase, texts: Vec<(vfs::FileId, String)>) -> Vec<vfs::FileId> {
    use ide_db::base_db::salsa::{Durability, Setter as _};
    for (file, text) in &texts {
        db.set_file_text_with_durability(*file, text, Durability::LOW);
        // What the crate's `#![cfg]` leaves of a root it takes away.
        let module = test_harness::appended(text);
        let input = db.file_text(*file);
        if input.appended_root_module(db) != module {
            input.set_appended_root_module(db).with_durability(Durability::LOW).to(module);
        }
    }
    prepare_macro_sessions(db);
    texts.into_iter().map(|it| it.0).collect()
}

/// Gives the standard library of a build with `--run-tool` blaze's `std::process` for
/// wasm32-wasip1 build scripts ([`blaze_wasi::STD_PROCESS`]) in place of the toolchain's
/// unsupported one. Refused when the toolchain's file is not the one blaze's was written
/// against: blaze's uses that module's private interface to the rest of std.
fn run_tool_std(db: &mut RootDatabase) -> anyhow::Result<()> {
    use ide_db::base_db::{LangCrateOrigin, salsa::Durability};
    use sha2::{Digest, Sha256};
    let std = ide_db::base_db::all_crates(db)
        .iter()
        .copied()
        .find(|it| matches!(it.data(db).origin, CrateOrigin::Lang(LangCrateOrigin::Std)))
        .context("--run-tool: the workspace has no standard library")?;
    let root = Crate::from(std).root_file(db);
    let source_root = db.file_source_root(root).source_root_id(db);
    let source_root = db.source_root(source_root).source_root(db);
    let at = source_root
        .path_for_file(&root)
        .and_then(|it| it.parent())
        .and_then(|it| it.join(blaze_wasi::STD_PROCESS_AT))
        .context("--run-tool: std's root has no directory")?;
    let file = source_root
        .file_for_path(&at)
        .copied()
        .with_context(|| format!("--run-tool: the standard library has no {at}"))?;
    let digest = Sha256::digest(db.file_text(file).text(db).as_bytes());
    let digest: String = digest.iter().map(|it| format!("{it:02x}")).collect();
    anyhow::ensure!(
        digest == blaze_wasi::STD_PROCESS_FOR,
        "--run-tool: blaze's std::process for build scripts (crates/blaze-wasi/std/process.rs) \
         was written against another {at} (sha256 {}) than this toolchain's (sha256 {digest}): \
         it is to be written again for this toolchain",
        blaze_wasi::STD_PROCESS_FOR
    );
    db.set_file_text_with_durability(file, blaze_wasi::STD_PROCESS, Durability::HIGH);
    frozen::overlaid(std, blaze_wasi::STD_PROCESS);
    Ok(())
}

/// Whether the root of `krate` says `#![no_main]`.
fn has_no_main(db: &RootDatabase, krate: Crate) -> bool {
    let root = ide_db::base_db::EditionedFileId::new(db, krate.root_file(db), krate.edition(db));
    let attrs = syntax::ast::HasAttrs::attrs(&root.parse(db).tree());
    attrs.filter_map(|it| it.as_simple_atom()).any(|it| it == "no_main")
}

impl ListedCrate {
    fn new(db: &RootDatabase, krate: Crate) -> ListedCrate {
        let Declared { functions, statics, in_provided, .. } = declared_in(db, krate);
        let kept = statics
            .iter()
            .copied()
            .filter(|&it| {
                hir_def::attrs::AttrFlags::query(db, it.into())
                    .contains(hir_def::attrs::AttrFlags::USED)
                    || hir_def::attrs::AttrFlags::link_section(db, it.into()).is_some()
                    || exported_static(db, it).is_some()
            })
            .collect();
        // A crate with `#![no_main]` has no `main` of Rust's: one it exports is the runtime's.
        let main = functions
            .iter()
            .copied()
            .find(|&it| hir::Function::from(it).is_main(db))
            .filter(|_| !has_no_main(db, krate));
        let exported = exported(db, &functions);
        let shared_paths = Default::default();
        let rooted = functions
            .iter()
            .zip(&exported)
            .filter(|&(&it, _)| !hir_ty_codegen::compile_time_only(db, it))
            .map(|(&it, &exported)| (it, exported, is_generic_function(db, it)))
            .collect();
        let provided = in_provided
            .iter()
            .filter(|&&it| !hir_ty_codegen::compile_time_only(db, it))
            .map(|&it| (it, is_generic_function(db, it)))
            .collect();
        ListedCrate {
            functions,
            shared_paths,
            kept,
            statics,
            in_provided,
            exported,
            rooted,
            provided,
            main,
        }
    }
}

/// For each of `functions`, whether a runtime looks it up by name. Its attributes say, which
/// are read off the source: on every core, as a build waits for this of every crate it builds.
fn exported(db: &RootDatabase, functions: &[FunctionId]) -> Vec<bool> {
    let exported = |db: &RootDatabase, functions: &[FunctionId]| -> Vec<bool> {
        let exported = |&it| {
            exported_symbol(db, it).is_some()
                || linked_symbol(db, it).is_some()
                || serves_allocator(db, it)
        };
        functions.iter().map(exported).collect()
    };
    let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
    let each = functions.len().div_ceil(threads).max(256);
    if functions.len() <= each {
        return exported(db, functions);
    }
    std::thread::scope(|scope| {
        let parts: Vec<_> = functions
            .chunks(each)
            .map(|part| {
                let db = db.clone();
                scope.spawn(move || hir::attach_db(&db, || exported(&db, part)))
            })
            .collect();
        // A query that is cancelled unwinds, and goes on unwinding here as what it is.
        let joined = |it: std::thread::ScopedJoinHandle<'_, Vec<bool>>| {
            it.join().unwrap_or_else(|e| std::panic::resume_unwind(e))
        };
        parts.into_iter().flat_map(joined).collect()
    })
}

/// Whether a function is a method of an `impl GlobalAlloc`. The program calls the allocator
/// through symbols rustc writes for the `#[global_allocator]`, so no call in the source names
/// these methods, and they run all the same.
fn serves_allocator(db: &RootDatabase, func: FunctionId) -> bool {
    let Some(item) = hir::Function::from(func).as_assoc_item(db) else { return false };
    let AssocItemContainer::Impl(implementation) = item.container(db) else { return false };
    implementation.trait_(db).is_some_and(|it| {
        let core = CrateOrigin::Lang(LangCrateOrigin::Core);
        it.name(db).as_str() == "GlobalAlloc" && it.module(db).krate(db).origin(db) == core
    })
}

impl Listed {
    /// Two builds that ask for a crate not listed yet both list it: neither waits for the other.
    fn of(&self, db: &RootDatabase, krate: Crate) -> std::sync::Arc<ListedCrate> {
        lock(&self.built).insert(krate);
        if let Some(listed) = lock(&self.functions).get(&krate) {
            return listed.clone();
        }
        let listed = std::sync::Arc::new(ListedCrate::new(db, krate));
        // A file of the crate that is a module of another too has more functions now.
        lock(&self.files).clear();
        lock(&self.functions).entry(krate).or_insert(listed).clone()
    }

    /// Lists `crates` on every core, a crate to a thread at a time.
    fn of_all(&self, db: &RootDatabase, crates: &[Crate]) {
        let unlisted: Vec<Crate> = {
            let functions = lock(&self.functions);
            crates.iter().copied().filter(|it| !functions.contains_key(it)).collect()
        };
        on_every_core(db, &unlisted, |db, krate| {
            self.of(db, krate);
        });
    }

    /// The dependencies `krate` names ([`hir::named_dependencies`]), found once.
    fn named(&self, db: &RootDatabase, krate: Crate) -> Arc<[Crate]> {
        if let Some(named) = lock(&self.named).get(&krate) {
            return named.clone();
        }
        let named: Arc<[Crate]> = hir::named_dependencies(db, krate).into();
        lock(&self.named).entry(krate).or_insert(named).clone()
    }

    /// The crates a program of `crates` links, themselves and all, as rustc loads them: what
    /// their code names, and what that names in turn, but not through the standard library's
    /// crates, which are built apart, nor through a proc macro, a library of the compiler's.
    /// Read a level of the graph at a time, on every core.
    fn program(&self, db: &RootDatabase, crates: &[Crate]) -> FxHashSet<Crate> {
        let mut program: FxHashSet<Crate> = crates.iter().copied().collect();
        let mut level: Vec<Crate> = crates.to_vec();
        while !level.is_empty() {
            on_every_core(db, &level, |db, krate| {
                self.named(db, krate);
            });
            let named: Vec<Crate> =
                level.iter().flat_map(|&it| self.named(db, it).to_vec()).collect();
            let linked = |it: &Crate| {
                let data = ide_db::base_db::Crate::from(*it).data(db);
                !data.origin.is_lang() && !data.target_kind.is_proc_macro()
            };
            level = named.into_iter().filter(linked).filter(|&it| program.insert(it)).collect();
        }
        program
    }

    /// Notes that a build of `crates` began: the files they read are a program's before the
    /// build lists them, as the check of its edited files has it.
    fn building(&self, crates: impl IntoIterator<Item = Crate>) {
        lock(&self.built).extend(crates);
    }

    /// Notes that a file changed: one of `crates`, or with `None` one we can't place.
    fn touch(&self, db: &RootDatabase, crates: Option<&[ide_db::base_db::Crate]>) {
        lock(&self.files).clear();
        let Some(crates) = crates else {
            lock(&self.functions).clear();
            lock(&self.named).clear();
            return;
        };
        // Asked before the lock is taken: a query that is cancelled unwinds, and would poison it.
        let seeing: Vec<Crate> = crates
            .iter()
            .flat_map(|krate| krate.transitive_rev_deps(db).into_iter().map(Crate::from))
            .collect();
        let (mut functions, mut named) = (lock(&self.functions), lock(&self.named));
        for krate in seeing {
            functions.remove(&krate);
            named.remove(&krate);
        }
    }
}

/// Runs `each` of `crates` on every core, a crate to a thread at a time.
fn on_every_core(db: &RootDatabase, crates: &[Crate], each: impl Fn(&RootDatabase, Crate) + Sync) {
    if crates.len() < 2 {
        for &krate in crates {
            each(db, krate);
        }
        return;
    }
    let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let parts: Vec<_> = (0..threads.min(crates.len()))
            .map(|_| {
                let (db, next, each) = (db.clone(), &next, &each);
                scope.spawn(move || {
                    hir::attach_db(&db, || {
                        let at = || next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        while let Some(&krate) = crates.get(at()) {
                            each(&db, krate);
                        }
                    })
                })
            })
            .collect();
        // A query that is cancelled unwinds, and goes on unwinding here as what it is.
        for part in parts {
            part.join().unwrap_or_else(|e| std::panic::resume_unwind(e));
        }
    });
}

fn declared_in(db: &RootDatabase, krate: Crate) -> Declared {
    let _span = tracing::info_span!(target: "blaze", "list functions").entered();
    // The first thing asked of a crate after an edit: name resolution is redone here.
    hir_def::nameres::crate_def_map(db, krate.into());
    let mut declared = Declared::default();
    declared.in_modules(db, krate);
    // A body's blocks include those of the blocks in it, but not those of the items in them.
    while !declared.bodies.is_empty() || !declared.provided.is_empty() {
        for under_provided in [false, true] {
            let bodies = match under_provided {
                true => std::mem::take(&mut declared.provided),
                false => std::mem::take(&mut declared.bodies),
            };
            declared.under_provided = under_provided;
            for body in lower(db, bodies) {
                for (_, def_map) in Body::of(db, body).blocks(db) {
                    for (_, module) in def_map.modules(db) {
                        declared.in_scope(db, module.scope(db));
                    }
                }
            }
        }
    }
    declared
}

/// The functions of a crate that no body declares, as its modules are now. The ones listed
/// for a build are from before the edits since, and the bodies of those may be gone.
fn functions_of_modules(db: &RootDatabase, krate: Crate) -> Vec<FunctionId> {
    let mut declared = Declared::default();
    declared.in_modules(db, krate);
    declared.functions
}

/// The file at `path` as the filesystem names it, through no link. rustc's dep-info names a
/// file of the workspace where it is, and the workspace's files are named through the path it
/// was given (`/tmp` is a link on macOS).
fn real_path(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

/// The sources of `files` that are among `written`, which rustc's dep-info names.
fn written_sources<'a>(
    files: &[WatchedFile],
    written: impl IntoIterator<Item = &'a Path>,
) -> FxHashMap<vfs::FileId, PathBuf> {
    let written: FxHashSet<PathBuf> = written.into_iter().map(real_path).collect();
    if written.is_empty() {
        return FxHashMap::default();
    }
    let source = |it: &WatchedFile| match it.file {
        Watched::Source(id) if written.contains(&real_path(&it.path)) => {
            Some((id, it.path.clone()))
        }
        _ => None,
    };
    files.iter().filter_map(source).collect()
}

/// Has each program that links with rustc's code note the first values of the statics with
/// state of the crates `files` are of, which are about to change, and of those that depend on
/// them (`Rejections::take_first_values`).
fn take_first_values(db: &RootDatabase, programs: &mut [Program], files: &[vfs::FileId]) {
    let crates: Vec<_> = files
        .iter()
        .flat_map(|&file| ide_db::base_db::relevant_crates(db, file).iter().copied())
        .collect();
    take_first_values_of(db, programs, &crates);
}

fn take_first_values_of(
    db: &RootDatabase,
    programs: &mut [Program],
    crates: &[ide_db::base_db::Crate],
) {
    if crates.is_empty() {
        return;
    }
    hir::attach_db(db, || {
        for program in programs.iter_mut().filter(|it| !it.table.is_empty()) {
            program.known.take_first_values(db, crates);
        }
    });
}

/// The crates that include one of `paths` with `include!`, `include_str!` or `include_bytes!`.
fn includers(db: &RootDatabase, paths: &[AbsPathBuf]) -> Vec<ide_db::base_db::Crate> {
    let all = ide_db::base_db::all_crates(db);
    let includes = |krate: ide_db::base_db::Crate| {
        db.included_by(krate).iter().any(|(path, _)| paths.contains(path))
    };
    all.iter().copied().filter(|&krate| includes(krate)).collect()
}

/// Has each program whose edits are patches of what rustc linked take in what rustc compiled
/// of `files`, which are about to change: the functions written in them and the statics
/// their bodies declare. A static declared outside a body is no edit of bodies alone.
fn home_in_rustcs(
    db: &RootDatabase,
    listed: &Listed,
    programs: &mut [Program],
    files: &[vfs::FileId],
) {
    if files.is_empty() || !programs.iter().any(|it| it.known.homes()) {
        return;
    }
    hir::attach_db(db, || {
        let functions = listed.in_files(db, files);
        let found: Vec<_> = files
            .iter()
            .zip(functions)
            .filter_map(|(&file, functions)| {
                let functions = functions?;
                let mut declared = Declared {
                    bodies: functions.iter().map(|&it| it.into()).collect(),
                    ..Declared::default()
                };
                while !declared.bodies.is_empty() {
                    for body in std::mem::take(&mut declared.bodies) {
                        for (_, def_map) in Body::of(db, body).blocks(db) {
                            for (_, module) in def_map.modules(db) {
                                declared.in_scope(db, module.scope(db));
                            }
                        }
                    }
                }
                Some((file, functions, declared.statics))
            })
            .collect();
        let homing = |file: vfs::FileId| programs.iter().any(|it| it.known.homes_file(file));
        let found: Vec<_> = found
            .into_iter()
            .map(|(file, functions, statics)| {
                let callers =
                    if homing(file) { callers_of_generic(db, &functions) } else { vec![] };
                (file, functions, statics, callers)
            })
            .collect();
        let written: Vec<FunctionId> = found
            .iter()
            .filter(|(file, ..)| homing(*file))
            .flat_map(|(_, functions, _, callers)| functions.iter().chain(callers).copied())
            .collect();
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..cores().min(written.len()) {
                let (db, written, next) = (db.clone(), &written, &next);
                scope.spawn(move || {
                    hir::attach_db(&db, || {
                        while let Some(&func) = written.get(next.fetch_add(1, Ordering::Relaxed)) {
                            hir_ty_codegen::compile_written(&db, func);
                        }
                    })
                });
            }
        });
        let contents = content::Contents::default();
        for program in programs.iter_mut().filter(|it| it.known.homes()) {
            let Program { args, table, known, .. } = program;
            let resolver = Resolver::of_rustcs(db, table, args, &contents, listed);
            for (file, functions, statics, callers) in &found {
                known.home(db, &resolver, *file, functions, statics, callers);
            }
        }
    });
}

/// The functions written elsewhere that call the generic functions among `functions`, or call
/// a generic one that does, as far as [`CALLERS_SEARCHED`] and [`CALLERS_COMPILED`] go: rustc
/// copied those into them. A method of a trait's impl is called through the trait's method as
/// well. A call outside a function's body is left out, as are the callers past the bound; the
/// copies they have are then ones no patch is known to reach.
fn callers_of_generic(db: &RootDatabase, functions: &[FunctionId]) -> Vec<FunctionId> {
    let sema = hir::Semantics::new(db);
    let of_trait = |func: FunctionId| {
        let container = hir::Function::from(func).as_assoc_item(db).map(|it| it.container(db));
        matches!(container, Some(hir::AssocItemContainer::Trait(_)))
    };
    let named = |func: FunctionId| {
        let func = hir::Function::from(func);
        let trait_ = func.as_assoc_item(db).and_then(|it| it.implemented_trait(db));
        std::iter::once(func).chain(trait_.and_then(|it| it.function(db, func.name(db))))
    };
    let mut seen: FxHashSet<FunctionId> = functions.iter().copied().collect();
    let mut queue: Vec<FunctionId> = functions
        .iter()
        .copied()
        .filter(|&it| !of_trait(it) && hir_ty_codegen::copied_by_rustc(db, it))
        .collect();
    let (mut callers, mut searched) = (Vec::new(), 0);
    while let Some(func) = queue.pop() {
        searched += 1;
        if searched > CALLERS_SEARCHED || callers.len() > CALLERS_COMPILED {
            break;
        }
        let usages =
            named(func).flat_map(|it| ide_db::defs::Definition::Function(it).usages(&sema).all());
        for reference in usages.flat_map(|(_, references)| references) {
            if reference.category.contains(ide_db::search::ReferenceCategory::IMPORT) {
                continue;
            }
            let ide_db::search::FileReferenceNode::NameRef(name) = reference.name else { continue };
            let caller = sema.ancestors_with_macros(name.syntax().clone()).find_map(ast::Fn::cast);
            let caller = caller.and_then(|it| sema.to_def(&it));
            let Some(caller) = caller.and_then(|it| FunctionId::try_from(it).ok()) else {
                continue;
            };
            if !seen.insert(caller) {
                continue;
            }
            callers.push(caller);
            if hir_ty_codegen::copied_by_rustc(db, caller) {
                queue.push(caller);
            }
        }
    }
    callers
}

/// How many generic functions [`callers_of_generic`] finds the callers of, and about how many
/// callers it finds, which are compiled before the edit.
const CALLERS_SEARCHED: usize = 16;
const CALLERS_COMPILED: usize = 512;

/// Whether the executable `program`'s edits patch has no copy of the functions written in
/// `file` with one of the `names`, which rustc would have copied into each crate that calls
/// them.
fn none_linked(
    db: &RootDatabase,
    listed: &Listed,
    program: &Program,
    file: vfs::FileId,
    names: &[String],
) -> bool {
    hir::attach_db(db, || {
        let Some(Some(functions)) = listed.in_files(db, &[file]).into_iter().next() else {
            return false;
        };
        let contents = content::Contents::default();
        let Program { args, table, .. } = program;
        let resolver = Resolver::of_rustcs(db, table, args, &contents, listed);
        let named = |func: &FunctionId| {
            let name = hir::Function::from(*func).name(db);
            names.iter().any(|it| it.as_str() == name.as_str())
        };
        functions.iter().filter(|it| named(it)).all(|&func| resolver.rustc_has_none(func))
    })
}

#[derive(Default)]
struct Declared {
    functions: Vec<FunctionId>,
    statics: Vec<StaticId>,
    /// The bodies whose blocks declare more scopes.
    bodies: Vec<DefWithBodyId>,
    /// Functions declared in the body of a trait's provided method, or in theirs: generic
    /// over nothing, though rustc compiles one only where it instantiates the method, so
    /// that what rustc built has no symbol of it.
    in_provided: Vec<FunctionId>,
    /// The bodies of provided methods and of what is declared in them.
    provided: Vec<DefWithBodyId>,
    /// Whether the scopes looked through are of [`Declared::provided`] bodies.
    under_provided: bool,
}

impl Declared {
    fn in_modules(&mut self, db: &RootDatabase, krate: Crate) {
        let mut modules: Vec<Module> = vec![krate.root_module(db)];
        while let Some(module) = modules.pop() {
            modules.extend(module.children(db));
            let id = hir_def::ModuleId::from(module);
            self.in_scope(db, id.def_map(db).module(db, id).scope(db));
        }
    }

    fn in_scope(&mut self, db: &RootDatabase, scope: &ItemScope) {
        for def in scope.declarations() {
            match def {
                // A function declared in an `extern` block has no body to compile, nor has an
                // intrinsic declared without one: every call to it is the compiler's own code.
                ModuleDefId::FunctionId(id)
                    if !matches!(id.lookup(db).container, ItemContainerId::ExternBlockId(_))
                        && !is_bodiless_intrinsic(db, id) =>
                {
                    self.function(id);
                }
                ModuleDefId::ConstId(id) => self.body(id.into()),
                ModuleDefId::StaticId(id) => {
                    self.statics.push(id);
                    self.body(id.into());
                }
                ModuleDefId::TraitId(id) => {
                    for &(_, item) in id.trait_items(db).items.iter() {
                        match item {
                            AssocItemId::FunctionId(id) => self.provided.push(id.into()),
                            AssocItemId::ConstId(id) => self.provided.push(id.into()),
                            AssocItemId::TypeAliasId(_) => {}
                        }
                    }
                }
                _ => {}
            }
        }
        for id in scope.unnamed_consts() {
            self.body(id.into());
        }
        for imp in scope.impls() {
            for &(_, item) in imp.impl_items(db).items.iter() {
                match item {
                    AssocItemId::FunctionId(id) => self.function(id),
                    AssocItemId::ConstId(id) => self.body(id.into()),
                    AssocItemId::TypeAliasId(_) => {}
                }
            }
        }
    }

    fn function(&mut self, id: FunctionId) {
        match self.under_provided {
            true => self.in_provided.push(id),
            false => self.functions.push(id),
        }
        self.body(id.into());
    }

    fn body(&mut self, id: DefWithBodyId) {
        match self.under_provided {
            true => self.provided.push(id),
            false => self.bodies.push(id),
        }
    }
}

/// Lowers the bodies on all threads and hands them back. An edit to a file has every body in
/// it lowered again, which is most of the work of looking through them.
fn lower(db: &RootDatabase, bodies: Vec<DefWithBodyId>) -> Vec<DefWithBodyId> {
    // A thread costs about what lowering a few small bodies does: the bodies of one edited
    // file are lowered where they are asked for.
    const PER_THREAD: usize = 16;
    let threads = std::thread::available_parallelism()
        .map_or(1, |it| it.get())
        .min(bodies.len().div_ceil(PER_THREAD));
    if threads <= 1 {
        hir::attach_db(db, || {
            for &body in &bodies {
                Body::of(db, body);
            }
        });
        return bodies;
    }
    std::thread::scope(|scope| {
        // The bodies of one file are next to each other, and the edited file's are the slow
        // ones: each thread takes every nth, so that they are spread over all of them.
        let probing = PROBING.get();
        for part in 0..threads.min(bodies.len()) {
            let (db, bodies) = (db.clone(), &bodies);
            scope.spawn(move || {
                PROBING.set(probing);
                hir::attach_db(&db, || {
                    for &body in bodies.iter().skip(part).step_by(threads) {
                        Body::of(&db, body);
                    }
                })
            });
        }
    });
    bodies
}

struct Build {
    compiled: usize,
    skipped: Vec<(String, CodegenError)>,
    /// Of [`Build::skipped`], by where each is in it, those the program reaches: with nothing
    /// of rustc's nothing else compiles them, and the build stops before its link
    /// ([`Build::refused_reached`]).
    reached: Vec<usize>,
    /// The build has nothing of rustc's, which would compile what blaze refuses.
    nothing_of_rustc: bool,
    /// Of [`Build::skipped`], in a library's build with nothing of rustc's, those exported by
    /// name, each `(symbol, "name: why")`: a program can declare one in an `extern` block
    /// and call it, which the unit has no stand-in for when nothing in it calls the function,
    /// and the link then fails on the name ([`RefusedExports`]).
    refused_exports: Vec<(String, String)>,
    instances: usize,
    codegen: Duration,
    emit: Duration,
    /// How long finding the roots, checking what is cached and compiling took, in that order.
    steps: [Duration; 3],
    /// Within the checking, the longest a thread took over each kind of cached thing.
    looked: hir_ty_codegen::StaleSteps,
    /// Why rustc does not compile what was built, when `env!` asks for a variable nobody set.
    unset: Option<String>,
    /// The files of a full build made for an edit, in the order they are written: they stand
    /// for the program once the build is kept ([`Build::keep`]), and one that is undone leaves
    /// those of the build before.
    unwritten: Vec<(PathBuf, Vec<u8>)>,
    /// Where [`Build::keep`] wrote them.
    wrote: Vec<PathBuf>,
}

impl Build {
    /// Writes the files of the build: what was built is the program from now on.
    fn keep(&mut self, known: &mut Rejections) -> anyhow::Result<()> {
        known.keep_built();
        for (path, bytes) in std::mem::take(&mut self.unwritten) {
            let _writing = whole::writing(&path);
            replace(&path, &bytes)?;
            self.wrote.push(path);
        }
        Ok(())
    }

    /// The build whose files a manifest of the store has ([`manifest`]): nothing compiled.
    fn of_manifest(unwritten: Vec<(PathBuf, Vec<u8>)>) -> Build {
        Build {
            compiled: 0,
            skipped: Vec::new(),
            reached: Vec::new(),
            nothing_of_rustc: true,
            refused_exports: Vec::new(),
            instances: 0,
            codegen: Duration::ZERO,
            emit: Duration::ZERO,
            steps: [Duration::ZERO; 3],
            looked: Default::default(),
            unset: None,
            unwritten,
            wrote: Vec::new(),
        }
    }
}

thread_local! {
    /// Set on a thread that asks about what an edit may have removed. A query about something
    /// that is gone panics, which is the answer and not worth a report.
    static PROBING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the edits of the build are patches of an object that has the standard library in
/// it. A whole build's (`--exe`) are patched over the library it links as a unit of its own.
fn patches_its_library(args: &Args) -> bool {
    args.patch.is_some() && args.exe.is_none()
}

/// The standard library of a build with nothing of rustc's, compiled before the program into
/// an object of its own beside `--out` (docs/no-rustc.md, step F): the store's when it has the
/// one of these crates, cfgs and target, otherwise built here and kept there. `None` when the
/// build has something of rustc's, is given the library, or compiles it with the program.
fn std_unit(
    db: &RootDatabase,
    table: &SymbolTable,
    args: &Args,
    listed: &Listed,
) -> anyhow::Result<Option<PathBuf>> {
    std_unit_at(db, table, args, listed, args.out.with_extension("std.o"))
}

/// The [`UnitStamps`] of each standard library's unit this process wrote, by its path.
static UNITS_WRITTEN: std::sync::LazyLock<Mutex<FxHashMap<PathBuf, UnitStamps>>> =
    std::sync::LazyLock::new(Default::default);

/// The length and write time of a standard library's unit and of its list of what it refused.
type UnitStamps = [Option<(u64, SystemTime)>; 2];

fn unit_stamps(unit: &Path) -> UnitStamps {
    let stamp = |path: &Path| {
        let meta = std::fs::metadata(path).ok()?;
        Some((meta.len(), meta.modified().ok()?))
    };
    [stamp(unit), stamp(&refused_exports_of(unit))]
}

/// Writes the standard library's unit at `args.prebuilt` again, from the store or built, when
/// what [`std_unit`] wrote there is gone or another file since: a worker keeps the path for as
/// long as it runs, and a `cargo clean` or `rm -rf target` under it tells it nothing.
fn std_unit_again(
    db: &RootDatabase,
    table: &SymbolTable,
    args: &Args,
    listed: &Listed,
) -> anyhow::Result<()> {
    static AGAIN: Mutex<()> = Mutex::new(());
    let Some(unit) = &args.prebuilt else { return Ok(()) };
    let _again = lock(&AGAIN);
    let Some(written) = lock(&UNITS_WRITTEN).get(unit).copied() else { return Ok(()) };
    if written.iter().all(Option::is_some) && unit_stamps(unit) == written {
        return Ok(());
    }
    eprintln!(
        "standard library: {} is not the file blaze wrote there, written again",
        unit.display()
    );
    // As the worker had it when it wrote the unit first, before it had a line to link with.
    let of_unit = Args { prebuilt: None, link_cmd: None, splice: None, ..args.clone() };
    match std_unit_at(db, table, &of_unit, listed, unit.with_extension("o"))? {
        Some(again) if again == *unit => Ok(()),
        again => bail!(
            "the standard library blaze built at {} is gone, and was written again as {again:?}",
            unit.display()
        ),
    }
}

/// [`std_unit`], with the unit at `object`, or beside it as an archive.
fn std_unit_at(
    db: &RootDatabase,
    table: &SymbolTable,
    args: &Args,
    listed: &Listed,
    object: PathBuf,
) -> anyhow::Result<Option<PathBuf>> {
    use sha2::{Digest, Sha256};
    if !table.is_empty() || args.prebuilt.is_some() || patches_its_library(args) {
        return Ok(None);
    }
    let mut program = Vec::new();
    for named in args.crates() {
        program.extend(crates_named(db, named, args)?);
    }
    // `proc_macro` and `test` are the program's when it links them: the unit leaves them out.
    let of_library = |it: &Crate| {
        use ide_db::base_db::LangCrateOrigin::{ProcMacro, Test};
        match &ide_db::base_db::Crate::from(*it).data(db).origin {
            CrateOrigin::Lang(ProcMacro | Test) => false,
            origin => origin.is_lang(),
        }
    };
    if program.iter().any(of_library) {
        return Ok(None);
    }
    let members = library_unit(db, &program);
    let lang = members.iter().copied().filter(|it| it.data(db).origin.is_lang());
    let named = |it: ide_db::base_db::Crate| {
        Some((Crate::from(it).display_name(db)?.crate_name().as_str().to_owned(), it))
    };
    let mut unit: Vec<_> = lang.filter_map(named).collect();
    unit.sort_unstable();
    // One that only renames another, as `rustc_std_workspace_core` does `core`, is not asked
    // for by its name: it comes with the crates that use it.
    unit.retain(|(name, _)| crates_named(db, name, args).is_ok());
    let Some(((first, _), rest)) = unit.split_first() else { return Ok(None) };
    // Lines at most, as rustc's own standard library is described: the variables of every
    // function of it would be most of the object, and the linker reads all of it.
    let debug = plan::Described { level: args.debug.level.min(plan::Level::Lines), ..args.debug };
    let mut hasher = Sha256::new();
    let mut field = |bytes: &[u8]| {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    field(b"blaze standard library 5");
    field(format!("{:?} {debug:?}", args.selection.target).as_bytes());
    // A file of one of its crates whose text is blaze's, not the toolchain's.
    if args.run_tool {
        field(blaze_wasi::STD_PROCESS_AT.as_bytes());
        field(blaze_wasi::STD_PROCESS.as_bytes());
    }
    // An object of one section is linked whole, with what the program never reaches.
    if !hir_ty_codegen::sections_of_their_own() {
        field(b"one section");
    }
    // A crate that is not the library's own is told by where its source is.
    let mut described: Vec<String> = members
        .iter()
        .map(|&krate| {
            let mut cfgs: Vec<String> =
                krate.cfg_options(db).into_iter().map(|it| it.to_string()).collect();
            cfgs.sort_unstable();
            let name = Crate::from(krate).display_name(db).map(|it| it.to_string());
            let root =
                (!krate.data(db).origin.is_lang()).then(|| crate_root_file(db, krate.into()));
            format!("{name:?} {root:?} {:?} {}", krate.data(db).edition, cfgs.join(" "))
        })
        .collect();
    described.sort_unstable();
    for it in &described {
        field(it.as_bytes());
    }
    let key: [u8; 32] = hasher.finalize().into();
    // What it refused that a program can call by name is kept beside it ([`refused_exports_of`]).
    let refused_key: [u8; 32] =
        Sha256::new().chain_update(key).chain_update(b"refused").finalize().into();
    let archive = object.with_extension("a");
    let refused = refused_exports_of(&object);
    let written = |path: &Path| {
        lock(&UNITS_WRITTEN).insert(path.to_owned(), unit_stamps(path));
    };
    let store = blaze_store::Store::global();
    let stored = || -> anyhow::Result<Option<PathBuf>> {
        let Some(store) = store else { return Ok(None) };
        let Some((bytes, listed)) = store.get(&key).zip(store.get(&refused_key)) else {
            return Ok(None);
        };
        let path = if link::split::is_archive(&bytes) { &archive } else { &object };
        std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
        std::fs::write(&refused, listed)
            .with_context(|| format!("writing {}", refused.display()))?;
        eprintln!("standard library: from the store, {} crates", members.len());
        written(path);
        Ok(Some(path.clone()))
    };
    if let Some(path) = stored()? {
        return Ok(Some(path));
    }
    let _making = match store.map(|it| it.making(&key)).transpose() {
        Ok(making) => making,
        Err(e) => {
            eprintln!("standard library: building it beside any other process that is: {e}");
            None
        }
    };
    if let Some(path) = stored()? {
        return Ok(Some(path));
    }
    let of_unit = Args {
        krate: first.clone(),
        batch: rest.iter().map(|it| it.0.clone()).collect(),
        widened: Vec::new(),
        out: object.clone(),
        report: None,
        exe: None,
        shared: false,
        library: true,
        // The unit is the standard library's crates, never the program's unit tests: asked as
        // those, `crates_called` keeps only crates with `cfg(test)`, which no crate of it has.
        unit_test: false,
        debug,
        ..args.clone()
    };
    events::at(events::Step::Std, None);
    let built = build_unheld(db, table, &of_unit, &mut Rejections::default(), listed)
        .with_context(|| {
            let names: Vec<&str> = unit.iter().map(|it| it.0.as_str()).collect();
            format!("building the standard library ({})", names.join(", "))
        })?;
    eprint!("standard library: {} crates, ", members.len());
    report(&built);
    let listed = refused_exports_text(&built.refused_exports);
    std::fs::write(&refused, &listed).with_context(|| format!("writing {}", refused.display()))?;
    let mut path = object;
    let mut bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    if object::FileKind::parse(&*bytes).is_ok_and(|it| it == object::FileKind::Elf64) {
        let start = Instant::now();
        match link::split::split(&bytes, link::split::PARTS) {
            Ok(parts) => {
                eprintln!(
                    "standard library: {} parts in {:.1?}",
                    link::split::PARTS + 1,
                    start.elapsed()
                );
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
                path = archive;
                std::fs::write(&path, &parts)
                    .with_context(|| format!("writing {}", path.display()))?;
                bytes = parts;
            }
            Err(e) => eprintln!("standard library: kept as one object: {e:#}"),
        }
    }
    if let Some(store) = store {
        store.settled();
        // The list first: a unit from the store comes with it.
        store.insert(&refused_key, listed.as_bytes());
        store.insert(&key, &bytes);
        store.flush();
    }
    written(&path);
    Ok(Some(path))
}

/// The sysroot of the workspace loaded first, which an executable with nothing of rustc's
/// is linked against.
static SYSROOT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

/// Links the executable of a build with nothing of rustc's: `--out` and the standard
/// library's object, with the native libraries the crates ask for.
fn link_executable(
    db: &RootDatabase,
    table: &SymbolTable,
    listed: &Listed,
    args: &Args,
    sysroot: Option<&Path>,
    exe: &Path,
) -> anyhow::Result<()> {
    if !table.is_empty() {
        bail!("--exe links a build with nothing of rustc's, and this one has rustc's symbols");
    }
    let mut program = Vec::new();
    for named in args.crates() {
        program.extend(crates_named(db, named, args)?);
    }
    let objects: Vec<&Path> =
        std::iter::once(args.out.as_path()).chain(args.prebuilt.as_deref()).collect();
    // A shared library exports what the crates it links ask for names with `#[no_mangle]`
    // or `#[export_name]`, as rustc's `cdylib` does; a proc macro's only what its own crate
    // names. The standard library's crates name none a program may call.
    let exports = args.shared.then(|| -> anyhow::Result<Vec<String>> {
        let mut exports = Vec::new();
        let mut crates = FxHashSet::default();
        for krate in crates_named(db, &args.krate, args)? {
            crates.insert(krate);
            let data = ide_db::base_db::Crate::from(krate).data(db);
            if data.target_kind.is_proc_macro() {
                continue;
            }
            crates.extend(listed.program(db, &[krate]));
        }
        for krate in crates {
            let of = listed.of(db, krate);
            let functions = of.functions.iter().filter_map(|&it| exported_symbol(db, it));
            let statics = of.statics.iter().filter_map(|&it| exported_static(db, it));
            exports.extend(functions.chain(statics.map(|it| it.as_str().to_owned())));
        }
        exports.sort_unstable();
        exports.dedup();
        Ok(exports)
    });
    let exports = exports.transpose()?;
    blaze_macro_wasm::flush();
    // A `staticlib` is linked as the `cdylib` of it would be, beside it, which leaves out
    // what its exports do not reach: the stand-ins left are what a program linking the
    // archive can reach, and a link that fails is one that program's would.
    let linked = match args.staticlib {
        true => {
            let mut beside = exe.as_os_str().to_owned();
            beside.push(".shared");
            PathBuf::from(beside)
        }
        false => exe.to_path_buf(),
    };
    let took = link::executable(
        db,
        &program,
        &objects,
        sysroot,
        &linked,
        exports.as_deref(),
        &args.link_args,
        args.debug.level,
    )
    .map_err(|it| refused_linked(it, args))?;
    let stand_ins = link::stand_ins(&linked);
    if args.staticlib {
        _ = std::fs::remove_file(&linked);
    }
    let stand_ins = stand_ins?;
    eprintln!(
        "linked {} in {took:.1?}, {} functions in it blaze could not compile",
        linked.display(),
        stand_ins.len()
    );
    // What the link kept of the stand-ins is what the program can reach, the standard
    // library's unit's among them, each saying why its unit refused it.
    // COFF keeps every stand-in in one section (the objects have no COMDAT sections for
    // `/OPT:REF` to take out what nothing reaches), so on Windows the stand-ins in the
    // executable are not the ones the program reaches: one that is called says so when it runs.
    if !stand_ins.is_empty() && !cfg!(windows) {
        let said = stand_ins.iter().map(|it| {
            let it = it.trim().trim_start_matches(hir_ty_codegen::STAND_IN_SAYS);
            cargo::whole::without_hashes(it)
        });
        bail!("{}", refused_error(&args.krate, stand_ins.len(), said));
    }
    if args.staticlib {
        let took = link::archive(db, &program, &objects, exe)?;
        eprintln!("archived {} in {took:.1?}", exe.display());
    }
    Ok(())
}

/// What a whole build's daemon links the program at `--exe` with and splices its edits into:
/// G's line. `None` when it is not one, or has it already.
fn own_link_line(db: &RootDatabase, args: &Args) -> anyhow::Result<Option<(String, PathBuf)>> {
    let Some(exe) = args.exe.as_ref().filter(|_| args.patch.is_some() && args.link_cmd.is_none())
    else {
        return Ok(None);
    };
    let mut program = Vec::new();
    for named in args.crates() {
        program.extend(crates_named(db, named, args)?);
    }
    let objects: Vec<&Path> =
        std::iter::once(args.out.as_path()).chain(args.prebuilt.as_deref()).collect();
    let sysroot = SYSROOT.get().and_then(Option::as_deref);
    let cmd =
        link::command(db, &program, &objects, sysroot, exe, &args.link_args, args.debug.level)?;
    Ok(Some((cmd, exe.clone())))
}

/// Writes to `path` the dep-info rustc writes of the crate `args` asks for, linked as `exe`:
/// one rule naming the files of its modules, the root first, and a rule of no files for each.
fn write_dep_info(
    db: &RootDatabase,
    args: &Args,
    vfs: &vfs::Vfs,
    exe: &Path,
    path: &Path,
) -> anyhow::Result<()> {
    let mut files = Vec::new();
    let krates = crates_named(db, &args.krate, args)?;
    for &krate in &krates {
        let modules = krate.modules(db).into_iter().map(|it| it.definition_source_file_id(db));
        files.extend(modules.map(|it| it.original_file(db).file_id(db)));
        let included = hir_def::include_macro_invoc(db, krate.into()).iter();
        files.extend(included.map(|(_, file)| file.file_id(db)));
    }
    let named = |id: &vfs::FileId| Some(vfs.file_path(*id).as_path()?.to_string());
    let mut paths: Vec<String> = Vec::new();
    // What `include_str!` and `include_bytes!` read from outside the VFS, as rustc names it.
    let read = krates.iter().flat_map(|it| db.included_by((*it).into()));
    let read: Vec<String> = read.map(|(path, _)| path.to_string()).collect();
    for path in files.iter().filter_map(named).chain(read) {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    anyhow::ensure!(!paths.is_empty(), "no file of `{}` was loaded", args.krate);
    let escaped: Vec<String> = paths.iter().map(|it| dep_info::escaped(it)).collect();
    let target = dep_info::escaped(&exe.display().to_string());
    let mut text = format!("{target}: {}\n\n", escaped.join(" "));
    for file in &escaped {
        text += &format!("{file}:\n");
    }
    // The variables `env!` and `option_env!` read, with what each was, so that cargo builds
    // again when one changes.
    let mut env: Vec<(String, Option<String>)> = db
        .env_read()
        .into_iter()
        .filter(|(krate, _)| krates.iter().any(|it| ide_db::base_db::Crate::from(*it) == *krate))
        .map(|(krate, key)| {
            let value = krate.env(db).get(&key);
            let value = value.or_else(|| ide_db::base_db::process_env_var(db, &key));
            (key, value)
        })
        .collect();
    env.sort_unstable();
    env.dedup();
    if !env.is_empty() {
        text.push('\n');
    }
    for (key, value) in env {
        let key = dep_info::escaped_variable(&key);
        match value {
            Some(value) => {
                text += &format!("# env-dep:{key}={}\n", dep_info::escaped_variable(&value))
            }
            None => text += &format!("# env-dep:{key}\n"),
        }
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// Writes to `path` what [`Args::inputs`] says: the files of the modules of every crate that
/// is not the toolchain's, what `include!` and its kin read for them, and the variables of
/// this process `env!` and `option_env!` read where the crate's description had none.
fn write_inputs(db: &RootDatabase, vfs: &vfs::Vfs, path: &Path) -> anyhow::Result<()> {
    let krates: Vec<Crate> =
        Crate::all(db).into_iter().filter(|it| !it.origin(db).is_lang()).collect();
    let mut files = Vec::new();
    for &krate in &krates {
        let modules = krate.modules(db).into_iter().map(|it| it.definition_source_file_id(db));
        files.extend(modules.map(|it| it.original_file(db).file_id(db)));
        let included = hir_def::include_macro_invoc(db, krate.into()).iter();
        files.extend(included.map(|(_, file)| file.file_id(db)));
    }
    let mut lines: Vec<String> =
        files.iter().filter_map(|id| Some(vfs.file_path(*id).as_path()?.to_string())).collect();
    lines.extend(
        krates.iter().flat_map(|it| db.included_by((*it).into())).map(|it| it.0.to_string()),
    );
    for (krate, key) in db.env_read() {
        let ours = krates.iter().any(|it| ide_db::base_db::Crate::from(*it) == krate);
        if ours && krate.env(db).get(&key).is_none() {
            lines.push(cargo::keyed::var(&key));
        }
    }
    lines.sort();
    lines.dedup();
    anyhow::ensure!(!lines.is_empty(), "no file was loaded");
    std::fs::write(path, lines.join("\n")).with_context(|| format!("writing {}", path.display()))
}

/// [`build`] of a build that is never held.
fn build_unheld(
    db: &RootDatabase,
    table: &SymbolTable,
    args: &Args,
    known: &mut Rejections,
    listed: &Listed,
) -> anyhow::Result<Build> {
    let built = build(db, table, args, known, listed, None)?;
    Ok(built.expect("a build that is never held is made"))
}

/// Builds the program, as a patch of the last full build where it can be. With `hold`, the
/// build is of an edit: where it can't be a patch and `hold` says to hold it, nothing is built
/// (`None`, see [`CodegenUnit::compile_or_hold`]), and a full build is undoable, its files
/// left to [`Build::keep`].
fn build(
    db: &RootDatabase,
    table: &SymbolTable,
    args: &Args,
    known: &mut Rejections,
    listed: &Listed,
    hold: Option<&dyn Fn() -> bool>,
) -> anyhow::Result<Option<Build>> {
    let start = Instant::now();
    // What this build compiles, here and on the threads that work for it: not what builds of
    // other programs compile beside it.
    let counting = hir_ty_codegen::counted::Counting::start();
    let contents = &content::Contents::default();
    let resolver = Resolver::of(db, table, args, contents, listed)?;
    let hir_db: &dyn HirDatabase = db;
    let mut functions = Vec::new();
    let mut kept = Vec::new();
    let mut every_static: Vec<StaticId> = Vec::new();
    // The `main` of the crate asked for: another program of the workspace among the crates
    // is only here for what this one uses of it.
    let mut main = None;
    let mut local = Vec::new();
    let asked = args.crates().map(|it| crates_to_build(db, table, it, args));
    let asked: Vec<Vec<Crate>> = asked.collect::<anyhow::Result<_>>()?;
    // On every core: an edit to a crate others depend on has each of them listed again, and
    // the build waits for all of it.
    listed.of_all(db, &asked.iter().flatten().copied().collect::<Vec<_>>());
    for (at, crates) in asked.into_iter().enumerate() {
        for krate in crates {
            local.push(krate);
            let listed = listed.of(db, krate);
            // A program of tests starts in blaze's test harness, which calls every test, or
            // in rustc's when blaze wrote none: the crate's own `main` is one function among
            // the others.
            if at == 0 && !of_tests(args) {
                main = main.or(listed.main);
            } else if at == 0 {
                main = main.or_else(|| harness_main(db, &listed.functions));
            }
            // With nothing of rustc's there is no copy of a generic function to name: each
            // instance is compiled with the caller that asks for it.
            let kept_rooted = |it: &&(FunctionId, bool, bool)| !(table.is_empty() && it.2);
            functions.extend(
                listed.rooted.iter().filter(kept_rooted).map(|&(it, exported, _)| (it, exported)),
            );
            // rustc's copy of the crate has them when rustc built it.
            let own = resolver.own_crate_id(krate.into()).is_some();
            if table.is_empty() || own {
                let kept_provided = |it: &&(FunctionId, bool)| !(table.is_empty() && it.1);
                functions.extend(
                    listed.provided.iter().filter(kept_provided).map(|&(it, _)| (it, false)),
                );
            }
            if own {
                kept.extend(&listed.kept);
            }
            every_static.extend(&listed.statics);
        }
    }
    // With nothing of rustc's every crate the program links is blaze's to compile; the
    // standard library is built apart ([`std_unit`]) or asked for by name.
    if table.is_empty() {
        let asked: FxHashSet<Crate> = local.iter().copied().collect();
        let tested = tests_linked(db, &local, of_tests(args));
        let own: FxHashSet<ide_db::base_db::Crate> =
            listed.program(db, &local).into_iter().map(ide_db::base_db::Crate::from).collect();
        let macros = macros_linked(db, &local, &own);
        let linked: FxHashSet<Crate> = own
            .into_iter()
            .filter(|it| !resolver.prebuilt(*it))
            .chain(tested)
            .chain(macros)
            .map(Crate::from)
            .filter(|it| !asked.contains(it))
            .collect();
        listed.of_all(db, &linked.iter().copied().collect::<Vec<_>>());
        for krate in linked {
            local.push(krate);
            let listed = listed.of(db, krate);
            functions.extend(
                listed.rooted.iter().filter(|it| !it.2).map(|&(it, exported, _)| (it, exported)),
            );
            functions.extend(listed.provided.iter().filter(|it| !it.1).map(|&(it, _)| (it, false)));
            kept.extend(&listed.kept);
            every_static.extend(&listed.statics);
        }
    }
    // A library built before the programs that use it has every static a generic function
    // of it uses, for the instances they compile.
    let offered: Vec<StaticId> = match main.is_none() && table.is_empty() {
        true => every_static
            .into_iter()
            .filter(|id| !matches!(id.lookup(db).container, ItemContainerId::ExternBlockId(_)))
            .collect(),
        false => Vec::new(),
    };
    // A generic function is not a function until its types are given, and with nothing of
    // rustc's there is no copy of it to name: each instance a caller asks for is compiled with
    // the caller. As a root it would only be counted as left out ("no rustc symbol"), which is
    // not something blaze could not compile.
    let list = start.elapsed();
    // With a program to start from, only what it reaches is compiled, and what a runtime
    // looks up by name. Without one any function may be the one called.
    let (entries, on_demand): (Vec<_>, Vec<_>) = functions
        .into_iter()
        .partition(|&(func, exported)| exported || main.is_none() || main == Some(func));
    let unzip = |it: Vec<(FunctionId, bool)>| it.into_iter().map(|it| it.0).collect::<Vec<_>>();
    let (entries, on_demand) = (unzip(entries), unzip(on_demand));
    // The program's `main` is called by one rustc wrote, when rustc built the crate, except
    // the `main` of blaze's test harness, which rustc's does not call.
    let own = |main: &FunctionId| resolver.own_crate_id(hir_def::HasModule::krate(main, db));
    let roots = Roots {
        entries: &entries,
        on_demand: &on_demand,
        start: main.filter(|it| own(it).is_some() || in_harness(db, *it)),
        shared: args.shared,
        whole: local.iter().all(|&krate| resolver.own_crate_id(krate.into()).is_some()),
        kept: &kept,
        offered: &offered,
    };
    let setup = start.elapsed();
    if std::env::var_os("BLAZE_STATS").is_some() {
        eprintln!("entries: {:.1?}", setup - list);
    }
    // Not dropped from the roots: what they share with the others has to stay with rustc too.
    let mut denied = FxHashSet::default();
    if let Some(deny) = &args.deny {
        let deny = std::fs::read_to_string(deny)?;
        let deny: FxHashSet<&str> = deny.lines().collect();
        denied.extend(
            entries.iter().chain(&on_demand).copied().filter(|&func| {
                resolver.path(func).is_some_and(|path| deny.contains(path.as_str()))
            }),
        );
    }
    let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
    // What every question about a crate's code comes down to first, asked one crate to a
    // thread: left to the questions, whichever gets there has the others wait for all of it.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let stale: Option<Vec<_>> = std::thread::scope(|scope| {
        let parts: Vec<_> = (0..threads)
            .map(|part| {
                let (db, known, local, next) = (db.clone(), &*known, &local, &next);
                let parts = resolver.parts();
                let counted = Some(counting.counted().clone());
                scope.spawn(move || {
                    PROBING.set(true);
                    let resolver = parts.over(&db);
                    // What an edit changed is compiled again to tell what changed.
                    hir_ty_codegen::counted::within(counted, || {
                        hir::attach_db(&db, || {
                            while let Some(&krate) =
                                local.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
                            {
                                hir_ty::method_resolution::TraitImpls::for_crate(&db, krate.into());
                                hir_ty::method_resolution::InherentImpls::for_crate(
                                    &db,
                                    krate.into(),
                                );
                            }
                            known.stale(&db, Some(&resolver), part, threads)
                        })
                    })
                })
            })
            .collect();
        parts.into_iter().map(|it| it.join().ok().flatten()).collect()
    });
    hir::attach_db(db, || known.set_stale(hir_db, stale.unwrap_or_default()));
    let looked = known.stale_steps();
    let revalidate = start.elapsed();
    // Salsa attaches the database for the duration of the closure; the solver needs it.
    let undoable = hold.is_some();
    let hold = hold.unwrap_or(&|| false);
    let unit = hir::attach_db(db, || {
        let counted = counting.counted();
        CodegenUnit::compile_or_hold(
            hir_db, &resolver, roots, &denied, known, hold, undoable, counted,
        )
    })
    .map_err(anyhow::Error::msg)?;
    let Some(mut unit) = unit else { return Ok(None) };
    let name = |func| resolver.path(func).unwrap_or_else(|| "<assoc fn>".to_owned());
    // Looking for the macro costs a walk of every macro call of the crate, so it is done once
    // a crate, and not for the standard library's own build, whose refusals are only counted.
    let mut failed_macros = FxHashMap::default();
    let mut explained = |func: FunctionId, error: CodegenError| {
        if args.library {
            return error;
        }
        with_failed_macro(db, func, error, |krate| {
            failed_macros.entry(krate).or_insert_with(|| failed_macro(db, krate)).clone()
        })
    };
    let codegen = start.elapsed();
    if std::env::var_os("BLAZE_STATS").is_some() {
        eprintln!(
            "roots: {setup:.1?}, revalidate: {:.1?}, compile: {:.1?}",
            revalidate - setup,
            codegen - revalidate
        );
    }
    let start = Instant::now();
    let mut unwritten = Vec::new();
    if let Some(mut object) = unit.object.take() {
        // A patch defines again what the object does, and the linker of Mach-O takes two
        // definitions of a name only when one of them gives way. The standard library's unit
        // gives way to nothing: it is linked with nothing of rustc's, and a program over it
        // takes its definitions by name ([`Prebuilt`]), which a private external, what
        // weakening makes of a name of Rust's, is not to `defined_symbols` (#255).
        if !args.library {
            weaken::ours(&mut object);
        }
        // The files a debugger is sent to are there before the object that sends it.
        for file in std::mem::take(&mut unit.apart) {
            unwritten.push((PathBuf::from(file.name), file.bytes));
        }
        unwritten.push((args.out.clone(), object));
    }
    if let (Some(path), Some(patch)) = (&args.patch, unit.patch.take()) {
        unwritten.push((path.clone(), patch));
    }
    // What a program with nothing of rustc's reaches of what was refused: its start, and what
    // only code of the object calls, which is compiled only once it is called. An entry the
    // program does not start from is there whether or not anything calls it, and its stand-in
    // says, once linked, whether the program can reach it ([`link_executable`]).
    let reaches = |func: FunctionId| {
        table.is_empty()
            && !args.library
            && main.is_some()
            && (main == Some(func) || !entries.contains(&func))
    };
    let reached = unit.skipped.iter().enumerate().filter(|(_, (func, _))| reaches(*func));
    let reached = reached.map(|(at, _)| at).collect();
    // The standard library's unit names the functions it refused that a program can call by
    // name ([`RefusedExports`]).
    let refused_exports = if table.is_empty() && args.library {
        let exported = unit.skipped.iter().filter_map(|(func, e)| {
            let symbol = exported_symbol(hir_db, *func)?;
            let why = e.to_string().replace('\n', " ");
            Some((symbol, format!("{}: {why}", cargo::whole::without_hashes(&name(*func)))))
        });
        exported.collect()
    } else {
        Vec::new()
    };
    let skipped =
        unit.skipped.into_iter().map(|(func, e)| (name(func), explained(func, e))).collect();
    let mut build = Build {
        compiled: unit.compiled.len(),
        skipped,
        reached,
        nothing_of_rustc: table.is_empty(),
        refused_exports,
        instances: unit.instances,
        codegen,
        emit: start.elapsed(),
        steps: [setup, revalidate - setup, codegen - revalidate],
        looked,
        unset: unset_in(db, &local),
        unwritten,
        wrote: Vec::new(),
    };
    // A patch is of the last full build and replaces nothing the next edit is built from.
    if !undoable || known.stats.patched.is_some() {
        build.keep(known)?;
    }
    if let Some(path) = &args.report {
        let compiled: Vec<String> = unit.compiled.iter().map(|&(func, _)| name(func)).collect();
        build.write_report(path, &compiled)?;
    }
    if let Some(dir) = &args.isolate {
        std::fs::create_dir_all(dir)?;
        let mut index = String::new();
        for (i, &(func, _)) in unit.compiled.iter().enumerate() {
            let single = hir::attach_db(db, || {
                let roots = Roots {
                    entries: &[func],
                    on_demand: &[],
                    start: None,
                    shared: false,
                    whole: false,
                    kept: &[],
                    offered: &[],
                };
                CodegenUnit::compile(hir_db, &resolver, roots, &Default::default(), known)
            })
            .map_err(anyhow::Error::msg)?;
            std::fs::write(dir.join(format!("{i}.o")), single.object.unwrap_or_default())?;
            index.push_str(&format!("{i}\t{}\n", name(func)));
        }
        std::fs::write(dir.join("index.tsv"), index)?;
    }
    Ok(Some(build))
}

/// Where the standard library's unit at `unit` lists the functions it refused that are
/// exported by name ([`Build::refused_exports`]), a line each, `symbol\tname: why`. A program
/// that declares one in an `extern` block and calls it, when nothing in the library calls it,
/// finds no stand-in for it in the unit: one exported under the name could take the place of
/// a definition the program links from elsewhere (libgcc's soft float is named as the
/// library's is), so the unit has none, and the link fails on the name ([`refused_linked`]).
fn refused_exports_of(unit: &Path) -> PathBuf {
    unit.with_extension("refused")
}

/// The lines of [`refused_exports_of`].
fn refused_exports_text(exports: &[(String, String)]) -> String {
    exports.iter().map(|(symbol, why)| format!("{symbol}\t{why}\n")).collect()
}

/// Of the names a link that failed says nothing defines, `said`, those the standard library's
/// unit at `unit` refused ([`refused_exports_of`]), each `name: why`, in the order the linker
/// names them. Every linker names a symbol as a word of its own, Apple's with the `_` before
/// it, and the names are looked up a word at a time, whatever else the linker says around them.
pub(crate) fn refused_undefined(said: &str, unit: &Path) -> Vec<String> {
    if !LinkFailed::UNDEFINED.iter().any(|it| said.contains(it)) {
        return Vec::new();
    }
    let Ok(listed) = std::fs::read_to_string(refused_exports_of(unit)) else { return Vec::new() };
    let refused: FxHashMap<&str, &str> =
        listed.lines().filter_map(|it| it.split_once('\t')).collect();
    let mut named = FxHashSet::default();
    let words =
        said.split(|it: char| !(it.is_ascii_alphanumeric() || matches!(it, '_' | '$' | '.')));
    let found = words.filter_map(|word| {
        let (symbol, why) = refused
            .get_key_value(word)
            .or_else(|| refused.get_key_value(word.strip_prefix('_')?))?;
        named.insert(*symbol).then(|| (*why).to_owned())
    });
    found.collect()
}

/// Why the program of `args` is not linked, when its link failed, `failed`, on names the
/// standard library's unit refused ([`refused_undefined`]): the error of a program that
/// reaches a function blaze refused ([`refused_error`]).
fn refused_linked(failed: anyhow::Error, args: &Args) -> anyhow::Error {
    let Some(unit) = &args.prebuilt else { return failed };
    let said = match failed.downcast_ref::<LinkFailed>() {
        Some(it) => it.said.clone(),
        None => format!("{failed:#}"),
    };
    let refused = refused_undefined(&said, unit);
    if refused.is_empty() {
        return failed;
    }
    anyhow::anyhow!("{}", refused_error(&args.krate, refused.len(), refused.into_iter()))
}

impl Build {
    /// Why the program of `args` is not linked, when it reaches a function this build refused
    /// ([`Build::reached`]).
    fn refused_reached(&self, args: &Args) -> Option<String> {
        if self.reached.is_empty() {
            return None;
        }
        let said = self.reached.iter().map(|&at| {
            let (name, error) = &self.skipped[at];
            let error = error.to_string().replace('\n', " ");
            format!("{}: {error}", cargo::whole::without_hashes(name))
        });
        Some(refused_error(&args.krate, self.reached.len(), said))
    }

    fn write_report(&self, path: &Path, compiled: &[String]) -> anyhow::Result<()> {
        use std::fmt::Write;
        let mut out = String::new();
        writeln!(
            out,
            "#\tinstances\t{}\tcodegen_us\t{}",
            self.instances,
            self.codegen.as_micros()
        )?;
        for name in compiled {
            writeln!(out, "compiled\t{name}\t")?;
        }
        for (name, error) in &self.skipped {
            let error = error.to_string().replace(['\t', '\n'], " ");
            writeln!(out, "skipped\t{name}\t{error}")?;
        }
        Ok(std::fs::write(path, out)?)
    }
}

/// `error` with the macro of the function's crate that did not expand, when the frontend is
/// what turned the function down: what such a macro would have written is what is then
/// missing, and the name or method that does not resolve says nothing of it.
fn with_failed_macro(
    db: &RootDatabase,
    func: FunctionId,
    error: CodegenError,
    failed_macro: impl FnOnce(Crate) -> Option<String>,
) -> CodegenError {
    let frontend = match &error {
        CodegenError::Frontend(_) => true,
        CodegenError::Unsupported(why) => why.contains("frontend:"),
        CodegenError::Backend(_)
        | CodegenError::NoSymbol
        | CodegenError::Private
        | CodegenError::Refused(_) => false,
    };
    if !frontend {
        return error;
    }
    let krate = hir::Function::from(func).module(db).krate(db);
    let Some(failed) = failed_macro(krate) else { return error };
    match error {
        CodegenError::Frontend(why) => CodegenError::Frontend(format!("{why} ({failed})")),
        CodegenError::Unsupported(why) => CodegenError::Unsupported(format!("{why} ({failed})")),
        error => error,
    }
}

/// What the first macro written among the items of `krate` that did not expand said.
fn failed_macro(db: &RootDatabase, krate: Crate) -> Option<String> {
    let modules = hir_def::nameres::crate_def_map(db, krate.into()).modules(db);
    let said = modules
        .flat_map(|(_, module)| module.scope(db).all_macro_calls())
        .find_map(|call| call.parse_macro_expansion_error(db)?.err)?;
    Some(format!("a macro of the crate did not expand: {}", said.render_to_string(db).message))
}

fn report(build: &Build) {
    eprintln!(
        "compiled {} of {} fns (+{} instances) in {:.1?} (+{:.1?} emit)",
        build.compiled,
        build.compiled + build.skipped.len(),
        build.instances,
        build.codegen,
        build.emit,
    );
    if let Some(path) = std::env::var_os("BLAZE_SKIPPED") {
        let all: String =
            build.skipped.iter().map(|(name, error)| format!("{name}: {error}\n")).collect();
        if let Err(error) = std::fs::write(&path, all) {
            eprintln!("blaze: cannot write {path:?}: {error}");
        }
    }
    if build.skipped.len() <= 8 {
        // With nothing of rustc's, nothing compiles what blaze refuses.
        let left = if build.nothing_of_rustc { "refused" } else { "left to rustc" };
        for (name, error) in &build.skipped {
            eprintln!("  {left}: {name}: {error}");
        }
        return;
    }
    // Bucket by the innermost reason, without the payload.
    let mut reasons: FxHashMap<String, (usize, &str)> = FxHashMap::default();
    for (name, error) in &build.skipped {
        let error = error.to_string();
        let reason = error.rsplit("`: ").next().unwrap_or(&error);
        let reason = reason.split(['(', '{']).next().unwrap_or(reason).trim();
        let entry = reasons.entry(reason.to_owned()).or_insert((0, name));
        entry.0 += 1;
    }
    let mut reasons: Vec<_> = reasons.into_iter().collect();
    reasons.sort_by_key(|(_, (count, _))| std::cmp::Reverse(*count));
    for (reason, (count, example)) in reasons {
        eprintln!("  {count:>5}  {reason}  (e.g. {example})");
    }
}

/// The executables that edits are spliced into, by where each is. One program has as many as
/// there are checkouts read as its own, and a link may run after the build that asked for it.
pub(crate) static LINKED: std::sync::LazyLock<std::sync::Mutex<FxHashMap<PathBuf, Executable>>> =
    std::sync::LazyLock::new(Default::default);

static SPLICING: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| std::env::var_os("BLAZE_SPLICE").is_none_or(|it| it != "0"));

/// Why a patch was not spliced into the executable whose link line does not have it: the
/// program is built in full and linked instead.
#[derive(Debug)]
struct PatchUnlinked {
    why: String,
}

impl std::fmt::Display for PatchUnlinked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the patch was not spliced into the program{}", self.why)
    }
}

impl std::error::Error for PatchUnlinked {}

/// Brings an executable up to date with its objects. An edit that left the whole object alone
/// is spliced into the executable at `splice`, which `--binary` names, when that is known from
/// its last link; anything else, and whatever the splice declines, runs the link command, whose
/// result is then known. `BLAZE_SPLICE=0` has every edit linked in full. With `packs` the object
/// is kept packed after the link ([`whole`]). A patch the splice declines whose line is the
/// worker's own (`own_line`, [`own_link_line`]) fails with [`PatchUnlinked`]. `prebuilt` is the
/// standard library the line links besides `object`, which the room of a first link counts
/// ([`headerpad_ahead`]).
#[expect(clippy::too_many_arguments)]
fn link(
    cmd: Option<&str>,
    splice: Option<&Path>,
    patch: Option<&Path>,
    object: &Path,
    prebuilt: Option<&Path>,
    patched: bool,
    defer: bool,
    packs: bool,
    own_line: bool,
) -> anyhow::Result<()> {
    let splice = splice.filter(|_| *SPLICING);
    // An older edit's splice under way is given up on. It gives the executable back, or, to an
    // edit whose splice waits for the edits to stop anyway, what to publish it with meanwhile.
    let deferred = defer && patched && cmd.is_some();
    let view = match (splice, patch) {
        (Some(binary), Some(patch)) => splicer::claim(binary, !deferred).map(|it| (it, patch)),
        (Some(binary), None) => {
            _ = splicer::claim(binary, true);
            None
        }
        (None, _) => None,
    };
    if let (Some(binary), Some((view, patch))) = (splice, view) {
        let start = Instant::now();
        live::tell_superseded();
        live::publish(&view, patch);
        splicer::defer(binary, std::fs::read(patch)?);
        eprintln!(
            "link: {:.1?} (the splice waits for the edits to stop; the one under way gives up)",
            start.elapsed()
        );
        return Ok(());
    }
    // An executable linked with the object as it was is not one to add to, also when the
    // object was written for a request that does not link.
    let rewritten = match splice {
        Some(binary) if !patched => lock(&LINKED).remove(binary),
        _ => None,
    };
    // So is a program that runs it, which can only be started again.
    let rewritten = match rewritten.map(Executable::baseline) {
        Some(old) => {
            if let Ok(old) = old {
                live::tell_superseded();
                live::restart(&old, "the edit changes what the rest of the program sees");
            }
            true
        }
        None => false,
    };
    let Some(cmd) = cmd else { return Ok(()) };
    // This splice or link has the edits of the one that waits.
    if let Some(binary) = splice {
        splicer::forget(binary);
    }
    let start = Instant::now();
    let mut declined = String::new();
    // No executable known of the whole object: the worker's own line links one, and the
    // patch is spliced into that.
    let mut unknown = false;
    if let (Some(binary), Some(patch)) = (splice, patch) {
        let known = lock(&LINKED).remove(binary).map(Executable::baseline);
        unknown = known.is_none() && !rewritten;
        // A program that runs the executable takes the edit in memory too.
        let old = known.as_ref().and_then(|it| it.as_ref().ok());
        live::tell_superseded();
        if let Some(baseline) = old.filter(|it| !rewritten && it.is_of(binary)) {
            live::publish(baseline, patch);
        }
        let superseded = old.filter(|_| !rewritten).and_then(Baseline::uuid);
        let span = tracing::info_span!(target: "blaze", "splice").entered();
        let spliced = match known {
            _ if rewritten => Err(anyhow::anyhow!("the whole object was written again")),
            Some(Ok(mut baseline)) if baseline.is_of(binary) => match std::fs::read(patch) {
                // The program has the edit: the executable on disk is for its next start,
                // and is spliced once the edits stop (see `splicer`).
                Ok(patch) if defer && baseline.uuid().is_some() => {
                    lock(&LINKED).insert(binary.to_owned(), Executable::Read(Box::new(baseline)));
                    splicer::defer(binary, patch);
                    eprintln!(
                        "link: {:.1?} (the splice waits for the edits to stop)",
                        start.elapsed()
                    );
                    return Ok(());
                }
                read => read
                    .map_err(anyhow::Error::from)
                    .and_then(|patch| baseline.splice(&patch, &AtomicBool::new(false)))
                    .map(|spliced| (baseline, spliced)),
            },
            Some(Ok(_)) => Err(anyhow::anyhow!("another executable is asked for")),
            Some(Err(why)) => Err(why),
            None => Err(anyhow::anyhow!("no executable to add to")),
        };
        drop(span);
        match spliced {
            Ok((baseline, Spliced { redirected, appended, copied })) => {
                if redirected != 0 && baseline.leaves_debug_info() {
                    eprintln!(
                        "debug: the PDB is of the code before the splice: the {redirected} functions redirected have no lines in it until the next full link"
                    );
                }
                let read = Executable::Read(Box::new(baseline));
                lock(&LINKED).insert(binary.to_owned(), read);
                eprintln!(
                    "link: {:.1?} (spliced: {redirected} functions redirected, {appended} bytes, {copied} copied)",
                    start.elapsed()
                );
                return Ok(());
            }
            Err(why) => {
                if let Some(uuid) = superseded {
                    live::superseded(uuid, format!("the program was linked again: {why:#}"));
                }
                declined = format!("; not spliced: {why:#}");
            }
        }
    }
    // The worker's own line has the object and not the patch, which alone has the edit.
    let relinked = own_line && patched && patch.is_some();
    if relinked && !unknown {
        return Err(PatchUnlinked { why: declined }.into());
    }
    let reading = whole::read(object)?;
    // The command was made before a clean of the target directory may have taken the file of
    // the room it reads.
    cargo::plan::headerpad_again(cmd).context("writing the room the link reads again")?;
    if let Some(binary) = splice {
        set_aside(binary)?;
        let objects: Vec<&Path> = [Some(object), prebuilt, patch].into_iter().flatten().collect();
        headerpad_ahead(binary, &objects)?;
    }
    run_link(cmd, splice, start, &declined)?;
    if let Some(binary) = splice
        && let Some(pad) = headerpad_short(binary)
    {
        cargo::plan::write_headerpad(binary, pad)?;
        let again = format!("; again with -Wl,-headerpad,{pad:#x}, for the splices to fit");
        run_link(cmd, splice, Instant::now(), &again)?;
    }
    drop(reading);
    if let (Some(binary), Some(patch), true) = (splice, patch, relinked) {
        let spliced = Linked::read_bare(binary.to_owned(), object).and_then(|mut baseline| {
            let patch = std::fs::read(patch)?;
            let spliced = baseline.splice(&patch, &AtomicBool::new(false))?;
            Ok((baseline, spliced))
        });
        let (baseline, Spliced { redirected, appended, copied }) = spliced.map_err(|why| {
            PatchUnlinked { why: format!("; not spliced into the object linked again: {why:#}") }
        })?;
        lock(&LINKED).insert(binary.to_owned(), Executable::Read(Box::new(baseline)));
        eprintln!(
            "link: {:.1?} (linked again from the whole object, which it was not, and spliced: {redirected} functions redirected, {appended} bytes, {copied} copied)",
            start.elapsed()
        );
    } else if let (Some(binary), Some(patch)) = (splice, patch) {
        let linked = Executable::Linked(Linked::read(binary.to_owned(), patch, object));
        lock(&LINKED).insert(binary.to_owned(), linked);
    }
    if packs {
        let start = Instant::now();
        match whole::pack(object) {
            Ok(true) => eprintln!("link: the whole object packed in {:.1?}", start.elapsed()),
            Ok(false) => {}
            Err(e) => eprintln!("link: the whole object is kept as it is: {e}"),
        }
    }
    Ok(())
}

/// Whether the object with all of the program is kept packed between links ([`whole`]): a
/// daemon's, whose executable has the debug information, and that nothing links but it.
fn packs_whole(args: &Args) -> bool {
    args.socket.is_some()
        && args.isolate.is_none()
        && args.exe.is_none()
        && args.debug.kept == plan::Kept::Linked
}

/// Runs the link command `cmd` and says how long it took since `start` and how it ended, with
/// `note` after. The executable `binary` it writes is linked under a directory of its own
/// beside it and renamed into place: lld moves the file there aside as it starts to write, and
/// GNU ld removes it, so a client that installs the last build's while another client's
/// request links it again would find none.
fn run_link(cmd: &str, binary: Option<&Path>, start: Instant, note: &str) -> anyhow::Result<()> {
    let aside = binary.and_then(|binary| Some((binary, linked_aside(cmd, binary)?)));
    let line = aside.as_ref().map_or(cmd, |(_, (line, _))| line);
    let linked = trace::link_output(&mut shell(line), "link");
    let placed = match (&aside, &linked) {
        (Some((binary, (_, at))), Ok(linked)) if linked.status.success() => {
            std::fs::rename(at, binary)
                .with_context(|| format!("putting the executable linked at {}", binary.display()))
        }
        _ => Ok(()),
    };
    if let Some(dir) = aside.as_ref().and_then(|(_, (_, at))| at.parent()) {
        _ = std::fs::remove_dir_all(dir);
    }
    let linked = linked?;
    let status = linked.status;
    // `link.exe` says what failed on its standard output.
    let said = [&linked.stdout, &linked.stderr].map(|it| String::from_utf8_lossy(it)).concat();
    eprint!("{said}");
    eprintln!("link: {:.1?} ({status}{note})", start.elapsed());
    if !status.success() {
        return Err(LinkFailed { status, said }.into());
    }
    placed
}

/// The link line `cmd` with the executable it names after `-o`, `binary`, written in a
/// directory of its own beside it under the same name (which Apple's linker signs it by), and
/// where that is. `None` when the line names `binary` otherwise, or not once.
fn linked_aside(cmd: &str, binary: &Path) -> Option<(String, PathBuf)> {
    if cfg!(windows) {
        return None;
    }
    let mut args = shlex::split(cmd)?;
    let named = binary.to_str()?;
    let mut found = args.windows(2).enumerate().filter(|(_, it)| it[0] == "-o" && it[1] == named);
    let at = found.next().filter(|_| found.next().is_none())?.0;
    let mut dir = binary.as_os_str().to_owned();
    dir.push(format!(".linking.{}", std::process::id()));
    let dir = PathBuf::from(dir);
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).ok()?;
    let aside = dir.join(binary.file_name()?);
    args[at + 1] = aside.to_str()?.to_owned();
    let line = shlex::try_join(args.iter().map(String::as_str)).ok()?;
    Some((line, aside))
}

/// A link that ended in failure, with what the linker said.
#[derive(Debug)]
struct LinkFailed {
    status: std::process::ExitStatus,
    said: String,
}

impl LinkFailed {
    /// How the linkers say that something uses a name nothing defines: lld, GNU ld, Apple's
    /// and `link.exe`.
    const UNDEFINED: [&str; 4] = [
        "undefined symbol:",
        "undefined reference to",
        "Undefined symbols for architecture",
        "unresolved external symbol",
    ];
}

impl std::fmt::Display for LinkFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the link failed ({}): {}", self.status, last_lines(&self.said, 6))
    }
}

impl std::error::Error for LinkFailed {}

/// Tells the link of the program at `binary` the room after the load commands it will want for
/// splices, when that is more than the link is told, so that it is not linked a second time
/// with more ([`headerpad_short`]): what the executable there, of the last link, wants, and
/// what the unwind information of the `objects` it links will take, which is all a first
/// link has to go by (#173).
fn headerpad_ahead(binary: &Path, objects: &[&Path]) -> anyhow::Result<()> {
    let Some(kept) = cargo::plan::headerpad_of(binary) else { return Ok(()) };
    let before = match splice::headerpad_wanted(binary) {
        Ok(Some((pad, _))) => pad,
        _ => 0,
    };
    let linked = splice::headerpad_of_objects(objects)?.unwrap_or(0);
    let pad = before.max(linked);
    if pad > kept {
        cargo::plan::write_headerpad(binary, pad)?;
    }
    Ok(())
}

/// The room after the load commands to link the executable at `binary` again with, when the
/// link left too little for an edit to be spliced into it: a program whose unwind table is
/// larger than the room the link was told to leave. Only a link that reads the room from
/// [`cargo::plan::headerpad_file`] is told more; one given otherwise keeps what its line says.
fn headerpad_short(binary: &Path) -> Option<u64> {
    let kept = cargo::plan::headerpad_of(binary)?;
    let pad = splice::headerpad_short(binary).ok()??;
    (pad > kept).then_some(pad)
}

/// Moves the executable at `binary` aside when a program runs it, which Windows lets nothing
/// write: it runs on there, and the link writes a new one ([`splice::move_aside`]).
#[cfg(windows)]
fn set_aside(binary: &Path) -> anyhow::Result<()> {
    const SHARING_VIOLATION: i32 = 32;
    match std::fs::OpenOptions::new().write(true).open(binary) {
        Err(e) if e.raw_os_error() == Some(SHARING_VIOLATION) => {
            splice::move_aside(binary).context("moving the running executable aside")
        }
        _ => Ok(()),
    }
}

/// Nothing on Unix: the linker writes a new file in place of one a program runs.
#[cfg(unix)]
fn set_aside(_: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// The last `count` lines of `text` that say something, on one line: what a linker that
/// failed said last is why.
fn last_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|it| !it.is_empty()).collect();
    lines[lines.len().saturating_sub(count)..].join(" | ")
}

/// The command that runs the line `cmd` as the platform's shell would.
#[cfg(unix)]
fn shell(cmd: &str) -> Command {
    let mut shell = Command::new("sh");
    shell.arg("-c").arg(cmd);
    shell
}

/// The command that runs the line `cmd`. It is the command line of the process as it is:
/// `cmd.exe` takes no line as long as the one a link has.
#[cfg(windows)]
fn shell(cmd: &str) -> Command {
    use std::os::windows::process::CommandExt as _;
    let cmd = cmd.trim_start();
    let (program, rest) = match cmd.strip_prefix('"').and_then(|it| it.split_once('"')) {
        Some(quoted) => quoted,
        None => cmd.split_once(char::is_whitespace).unwrap_or((cmd, "")),
    };
    let mut command = Command::new(program);
    command.raw_arg(rest);
    command
}

/// Whether a patch of `binary` can call the C symbol `name`; yes while its executable is being
/// read, which a splice then says.
fn can_call(binary: &Path, name: &str) -> bool {
    match lock(&LINKED).get(binary) {
        Some(Executable::Read(baseline)) => baseline.can_call(name),
        _ => true,
    }
}

/// A symbol the patch at `patch` refers to that the executable at `binary` does not give it;
/// none while the executable is being read, which a splice then says.
fn missing_in(binary: &Path, patch: &Path) -> Option<String> {
    let linked = lock(&LINKED);
    let Some(Executable::Read(baseline)) = linked.get(binary) else { return None };
    baseline.missing_in(&std::fs::read(patch).ok()?)
}

/// The executable of the last full link, for splicing edits into.
enum Executable {
    /// Just linked, and being read.
    Linked(Linked),
    Read(Box<Baseline>),
}

impl Executable {
    fn baseline(self) -> anyhow::Result<Baseline> {
        match self {
            Executable::Linked(linked) => linked.baseline(),
            Executable::Read(baseline) => Ok(*baseline),
        }
    }
}

/// The directories of the checkout at `base` that builds write to. They are not part of the
/// checkout: another checkout read in its place has the same ones.
fn built_in(db: &RootDatabase, base: &Path) -> Vec<PathBuf> {
    let crates = ide_db::base_db::all_crates(db);
    let scripts = crates.iter().filter_map(|krate| {
        let out = krate.env(db).get("OUT_DIR")?;
        let below = Path::new(&out).strip_prefix(base).ok()?.components().next()?;
        Some(base.join(below))
    });
    // Cargo tags the directories it builds in.
    let tagged = std::fs::read_dir(base)
        .into_iter()
        .flatten()
        .filter_map(|it| Some(it.ok()?.path()))
        .filter(|it| it.join("CACHEDIR.TAG").exists());
    let mut built: Vec<PathBuf> = scripts.chain(tagged).collect();
    built.sort_unstable();
    built.dedup();
    built
}

/// The variables of the process the build is for that `env!` and `option_env!` in `krate` have
/// asked for and that are set, sorted. Those the crate's own variables answer are not asked of
/// the process.
fn process_reads(db: &RootDatabase, krate: ide_db::base_db::Crate) -> Vec<(String, String)> {
    let env = krate.env(db);
    let mut reads: Vec<(String, String)> = db
        .env_read()
        .into_iter()
        .filter(|(of, key)| *of == krate && !env.contains_key(key))
        .filter_map(|(_, key)| {
            let value = ide_db::base_db::process_env_var(db, &key)?;
            Some((key, value))
        })
        .collect();
    reads.sort_unstable();
    reads
}

/// A variable `env!` asks for in one of `crates` that neither cargo nor the process the build
/// is for sets: rustc does not compile the crate.
fn unset_in(db: &RootDatabase, crates: &[Crate]) -> Option<String> {
    let mut unset: Vec<String> = db
        .env_unset()
        .into_iter()
        .filter(|(krate, key)| {
            crates.iter().any(|it| ide_db::base_db::Crate::from(*it) == *krate)
                && !krate.env(db).contains_key(key)
                && ide_db::base_db::process_env_var(db, key).is_none()
        })
        .map(|(krate, key)| match &krate.extra_data(db).display_name {
            Some(name) => format!(
                "`env!(\"{key}\")` in {} names a variable that is not set",
                name.canonical_name()
            ),
            None => format!("`env!(\"{key}\")` names a variable that is not set"),
        })
        .collect();
    unset.sort_unstable();
    unset.into_iter().next()
}

/// The checkout a crate was loaded in, and the one its variables name now.
type Placed = FxHashMap<ide_db::base_db::Crate, (PathBuf, PathBuf)>;

/// Has the crates of the checkout at `base` that ask `env!` for a place in it be told the
/// place in `over`: `CARGO_MANIFEST_DIR` is where the checkout read is. The result is the
/// crates told another place than before.
fn place(
    db: &mut RootDatabase,
    base: &Path,
    over: &Path,
    placed: &mut Placed,
) -> Vec<ide_db::base_db::Crate> {
    let built = built_in(db, base);
    let mut asking: FxHashMap<_, Vec<String>> = FxHashMap::default();
    for (krate, key) in db.env_read() {
        asking.entry(krate).or_default().push(key);
    }
    let mut moved = Vec::new();
    for (krate, keys) in asking {
        let env = krate.env(db);
        let Some(manifest) = env.get("CARGO_MANIFEST_DIR") else { continue };
        let manifest = Path::new(&manifest);
        // A checkout that joins has the crates there are loaded again, as they first were.
        let from = match placed.get(&krate) {
            Some((of, from)) if of == base && manifest.starts_with(from) => from.clone(),
            Some((of, _)) if of != base => continue,
            _ if manifest.starts_with(base) => base.to_owned(),
            _ => continue,
        };
        if from == over {
            continue;
        }
        let moved_to = |value: &str| {
            let path = Path::new(value);
            let inside = path.strip_prefix(&from).ok()?;
            if built.iter().any(|it| path.starts_with(it)) {
                return None;
            }
            Some(if inside.as_os_str().is_empty() { over.to_owned() } else { over.join(inside) })
        };
        if !keys.iter().any(|key| env.get(key).is_some_and(|it| moved_to(&it).is_some())) {
            continue;
        }
        let mut new = env.clone();
        for (key, value) in env {
            if let Some(path) = moved_to(value) {
                new.set(key, path.to_string_lossy());
            }
        }
        db.set_env(krate, new);
        placed.insert(krate, (base.to_owned(), over.to_owned()));
        moved.push(krate);
    }
    if !moved.is_empty() {
        eprintln!("placed: {} crates in {}", moved.len(), over.display());
    }
    moved
}

/// The arguments to cargo that have it run the build scripts of the build `args` asks for.
fn selected(args: &Args) -> Vec<String> {
    let selection = &args.selection;
    let mut selected = vec!["--profile".to_owned(), selection.profile.clone()];
    for package in &selection.packages {
        selected.extend(["--package".to_owned(), package.clone()]);
    }
    if selection.workspace {
        selected.push("--workspace".to_owned());
    }
    for package in &selection.excludes {
        selected.extend(["--exclude".to_owned(), package.clone()]);
    }
    for unstable in &selection.unstable {
        selected.extend(["-Z".to_owned(), unstable.clone()]);
    }
    for config in &selection.config {
        selected.extend(["--config".to_owned(), config.clone()]);
    }
    selected
}

/// What cargo said of every crate when the checkouts were last loaded.
struct CratesAsLoaded(FxHashMap<ide_db::base_db::Crate, CrateAsLoaded>);

type CrateAsLoaded = (
    ide_db::base_db::BuiltCrateData,
    ide_db::base_db::ExtraCrateData,
    cfg::CfgOptions,
    ide_db::base_db::Env,
);

impl CratesAsLoaded {
    fn of(db: &RootDatabase) -> CratesAsLoaded {
        let crates = ide_db::base_db::all_crates(db);
        CratesAsLoaded(crates.iter().map(|&it| (it, Self::said(db, it))).collect())
    }

    fn said(db: &RootDatabase, krate: ide_db::base_db::Crate) -> CrateAsLoaded {
        (
            krate.data(db).clone(),
            krate.extra_data(db).clone(),
            krate.cfg_options(db).clone(),
            krate.env(db).clone(),
        )
    }

    /// The crates of `db` that cargo says something else of now, or that were not there.
    fn changed(&self, db: &RootDatabase) -> Vec<ide_db::base_db::Crate> {
        let crates = ide_db::base_db::all_crates(db);
        let same = |krate| self.0.get(&krate).is_some_and(|was| *was == Self::said(db, krate));
        crates.iter().copied().filter(|&it| !same(it)).collect()
    }
}

/// The source files the daemon watches.
struct Sources {
    paths: FxHashMap<vfs::FileId, PathBuf>,
}

/// Why the daemon did not build a program.
enum Refused {
    /// The source has errors: the build fails with them, as rustc's would, and no rustc runs
    /// to say them again.
    Error(errors::Said),
    /// The source is one blaze does not build as it is, or blaze failed to: cargo builds it.
    Cargos(String),
}

impl Refused {
    /// The line that tells a client.
    fn reply(&self) -> String {
        match self {
            Refused::Error(said) => said.reply(),
            Refused::Cargos(why) => format!("cargo: {}", why.replace('\n', " ")),
        }
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Error(said) => said.fmt(f),
            Refused::Cargos(why) => f.write_str(why),
        }
    }
}

/// Why the source as it is can't be built by the daemon, when it can't: the proc-macro server
/// failed an expansion, then a file that does not parse, then the errors found in the edited
/// files, then a function left uncompiled.
/// Paths are shown from `root`, the workspace's, as rustc's errors show them.
fn refusal(
    db: &RootDatabase,
    sources: &[vfs::FileId],
    watched: &Sources,
    root: &Path,
    broken: Option<errors::Said>,
    out_of_date: Option<hir_ty_codegen::OutOfDate>,
) -> Option<Refused> {
    if let Some(failed) = blaze_expand::server_failed() {
        return Some(Refused::Error(errors::Said::one(errors::of_none(failed))));
    }
    if let Some(unparsed) = unparsed(db, sources, watched, root) {
        return Some(Refused::Error(errors::Said::one(unparsed)));
    }
    if let Some(broken) = broken {
        return Some(Refused::Error(broken));
    }
    let hir_ty_codegen::OutOfDate { func, why, left_from_the_start, calls } = out_of_date?;
    let named =
        |func| hir::Function::from(func).name(db).display(db, syntax::Edition::CURRENT).to_string();
    let name = func.map_or("drop glue or a shim".to_owned(), |it| format!("`{}`", named(it)));
    if let Some(calls) = calls {
        let calls = match calls {
            Some(it) => format!("calls rustc's copy of `{}`, which is", named(it)),
            None => "may call code of rustc's that is".to_owned(),
        };
        return Some(Refused::Cargos(format!(
            "{name} is not compiled by blaze, and rustc's copy of it {calls} from before the \
             edit: {why} (no-rustc E, #85)"
        )));
    }
    // What the frontend turns down as code that does not compile, rustc would too, unless
    // the frontend turned the function down when rustc compiled it. Code rustc compiles
    // that blaze takes for wrong is blaze's to fix, and not for rustc to build.
    if does_not_compile(&why) && !left_from_the_start {
        let message = format!("{name} has an error blaze cannot say more of: {why}");
        let said = errors::Said::one(written_at(db, func, watched, root, &message));
        return Some(Refused::Error(said));
    }
    Some(Refused::Cargos(format!(
        "{name} is not compiled by blaze and rustc's copy is from before the edit: {why} \
         (no-rustc E, #85)"
    )))
}

/// What the frontend says of a function that does not compile, of its reasons: the rest are
/// code blaze does not compile yet, or its own failures.
const DOES_NOT_COMPILE: &[&str] = &[
    "IncompleteExpr",
    "IncompletePattern",
    "UnresolvedName",
    "RecordLiteralWithoutPath",
    "UnresolvedMethod",
    "UnresolvedField",
    "MissingFunctionDefinition",
    "HasErrors",
    "TypeError",
    "ContinueWithoutLoop",
    "BreakWithoutLoop",
    "MutatingRvalue",
    "UnresolvedLabel",
];

/// Whether `why`, a reason a function is not compiled, says it does not compile.
fn does_not_compile(why: &str) -> bool {
    why.strip_prefix("frontend: mir: ").is_some_and(|it| {
        let variant = it.split(|c: char| !c.is_alphanumeric()).next().unwrap_or(it);
        DOES_NOT_COMPILE.contains(&variant)
    })
}

/// `message` as an error at the name of `func`, or of no place when there is none (drop glue,
/// a shim) or its file is not one of `watched`.
fn written_at(
    db: &RootDatabase,
    func: Option<hir_def::FunctionId>,
    watched: &Sources,
    root: &Path,
    message: &str,
) -> errors::Diagnostic {
    let at = func.and_then(|func| hir::HasSource::source(hir::Function::from(func), db)).and_then(
        |source| {
            let name = source.value.name()?;
            Some(source.with_value(name.syntax()).original_file_range_rooted(db))
        },
    );
    match at {
        Some(at) if watched.paths.contains_key(&at.file_id.file_id(db)) => {
            let file = at.file_id.file_id(db);
            let text = db.file_text(file).text(db);
            errors::at(None, message, &shown(watched, file, root), text, at.range)
        }
        _ => errors::of_none(message),
    }
}

/// `file` as rustc names it in an error: from `root` when it is below it.
fn shown(watched: &Sources, file: vfs::FileId, root: &Path) -> String {
    let Some(path) = watched.paths.get(&file) else { return String::new() };
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

/// The first error of the first of `sources` that does not parse, where it is.
fn unparsed(
    db: &RootDatabase,
    sources: &[vfs::FileId],
    watched: &Sources,
    root: &Path,
) -> Option<errors::Diagnostic> {
    let Sources { paths } = watched;
    for &file in sources {
        let path = paths.get(&file);
        // A manifest is cargo's to read: the checkout is loaded again when one changes.
        if path.is_some_and(|it| it.file_name().is_some_and(|it| it == "Cargo.toml")) {
            continue;
        }
        let edition = ide_db::base_db::relevant_crates(db, file)
            .first()
            .map_or(syntax::Edition::CURRENT, |it| it.data(db).edition);
        let parse = ide_db::base_db::EditionedFileId::new(db, file, edition).parse(db);
        if let Some(error) = parse.errors().first() {
            let text = db.file_text(file).text(db);
            // A file that is included where an expression goes is not one of items.
            if syntax::ast::Expr::parse(text, edition).errors().is_empty() {
                continue;
            }
            let range = syntax::TextRange::empty(error.range().start());
            return Some(errors::at(
                None,
                &error.to_string(),
                &shown(watched, file, root),
                text,
                range,
            ));
        }
    }
    None
}

/// The errors rust-analyzer is as sure of as rustc is: a wrong one fails a build that is
/// right.
const SURE: &[&str] = &[
    "E0004",
    "E0005",
    "E0023",
    "E0046",
    "E0057",
    "E0061",
    "E0063",
    "E0308",
    "E0407",
    "E0425",
    "E0432",
    "E0559",
    "E0560",
    "E0583",
    "E0599",
    "E0665",
    "builtin-derive-error",
    "unresolved-macro-call",
];

/// The borrow checker's errors, which fail a build unless `BLAZE_BORROWCK_REFUSE=0`:
/// `borrowck` is rustc's errors without a code, as "lifetime may not live long enough".
const BORROWCK: &[&str] = &[
    "E0309", "E0310", "E0311", "E0373", "E0381", "E0382", "E0384", "E0499", "E0500", "E0501",
    "E0502", "E0503", "E0505", "E0506", "E0507", "E0508", "E0509", "E0515", "E0521", "E0524",
    "E0594", "E0596", "E0597", "E0621", "E0716", "borrowck",
];

/// The lints rust-analyzer finds where rustc does, and nowhere else, as `lints_ui.py` scores
/// them against rustc's UI tests and its probes of each place a lint applies. None is denied
/// by default, so one at the level of an error is one a source denies, which no flag but
/// `--cap-lints` or `--force-warn` undoes.
const DENIED: &[&str] = &[
    "non_snake_case",
    "non_camel_case_types",
    "non_upper_case_globals",
    "unused_must_use",
    "deprecated_safe_2024",
    "elided_lifetimes_in_paths",
    "unsafe_op_in_unsafe_fn",
];

/// The lints of [`DENIED`]'s kind that the borrow checker gives, off with its errors.
const DENIED_BORROWCK: &[&str] = &["unused_mut"];

/// The errors written in the edited files, as rust-analyzer finds them, with paths from
/// `root` as rustc's are. Nothing may reach them, so the build never looks at them.
fn broken_in(
    db: &RootDatabase,
    edited: &[vfs::FileId],
    watched: &Sources,
    root: &Path,
    within: Option<Arc<FxHashSet<ide_db::base_db::Crate>>>,
    lints: bool,
) -> Option<errors::Said> {
    let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
    let config = ide_diagnostics::DiagnosticsConfig::test_sample();
    let refuse_borrowck = std::env::var_os("BLAZE_BORROWCK_REFUSE").is_none_or(|it| it != "0");
    // The check of one file is an edit's, over before anyone would look.
    let counting = match edited.len() {
        0 | 1 => events::Counting::none(),
        files => events::Counting::new(events::Step::Check, files, threads.min(files)),
    };
    let mut found: Vec<_> = std::thread::scope(|scope| {
        let checks: Vec<_> = (0..threads.min(edited.len()))
            .map(|part| {
                let (db, config, within, counting) = (db.clone(), &config, &within, &counting);
                scope.spawn(move || {
                    let files = edited.iter().skip(part).step_by(threads);
                    hir::attach_db(&db, || {
                        files
                            .flat_map(|&file| {
                                counting.on(part, || shown(watched, file, root));
                                // A library's files are its unit tests' too, with what is
                                // under `cfg(test)`: each crate built of them says what it
                                // finds, where rust-analyzer would ask the first.
                                let crates: Vec<_> = ide_db::base_db::relevant_crates(&db, file)
                                    .iter()
                                    .copied()
                                    .filter(|it| within.as_ref().is_none_or(|w| w.contains(it)))
                                    .collect();
                                let each: Vec<_> = match crates.len() {
                                    0 | 1 => vec![within.clone()],
                                    _ => crates
                                        .into_iter()
                                        .map(|it| Some(Arc::new(FxHashSet::from_iter([it]))))
                                        .collect(),
                                };
                                let db = &db;
                                let rules = (lints, refuse_borrowck);
                                let each = each.into_iter().flat_map(move |within| {
                                    errors_of(db, config, file, within, rules)
                                });
                                let found = each.collect::<Vec<_>>();
                                counting.done(part);
                                found
                            })
                            .collect::<Vec<_>>()
                    })
                })
            })
            .collect();
        checks.into_iter().flat_map(|it| it.join().unwrap_or_default()).collect()
    });
    if found.is_empty() {
        return None;
    }
    found.sort_by(|(a, a_code, a_said), (b, b_code, b_said)| {
        (a.file_id, a.range.start(), a.range.end(), a_code, a_said).cmp(&(
            b.file_id,
            b.range.start(),
            b.range.end(),
            b_code,
            b_said,
        ))
    });
    found.dedup();
    let said = found
        .iter()
        .map(|(at, code, message)| {
            let text = db.file_text(at.file_id).text(db);
            // rustc's codes are `E` and four digits; rust-analyzer's own names are not codes.
            let rustcs = code
                .strip_prefix('E')
                .is_some_and(|it| !it.is_empty() && it.bytes().all(|b| b.is_ascii_digit()));
            let code = rustcs.then_some(*code);
            let path = shown(watched, at.file_id, root);
            errors::at(code, message, &path, text, at.range)
        })
        .collect();
    Some(errors::Said(said))
}

/// What the last check of a file found nothing wrong in: the file's [`outline::skeleton`] then,
/// and a hash of how each of its functions was written. An edit that leaves the skeleton as it
/// was changed no more than bodies nothing else sees into, and a body's errors are its own:
/// the next check asks only for the bodies whose functions are written otherwise.
struct Clean {
    skeleton: u64,
    functions: FxHashMap<hir::Function, u64>,
}

/// The [`Clean`] checks, by the file, the crates it was read as a module of (`within`, by a
/// hash of the set), and the rules of what is an error.
type CleanKey = (vfs::FileId, Option<(usize, u64)>, (bool, bool));

static CLEAN: std::sync::LazyLock<std::sync::Mutex<FxHashMap<CleanKey, Clean>>> =
    std::sync::LazyLock::new(Default::default);

/// The errors the check of `file` as a module of the crates `within` finds, by the rules
/// (lints denied, borrowck refused) of [`broken_in`]. After a check that found none, the next
/// asks rust-analyzer only for the bodies an edit can have changed ([`Clean`]); every body is
/// asked for when the file is more than bodies apart from that check, or it was not clean.
fn errors_of(
    db: &RootDatabase,
    config: &ide_diagnostics::DiagnosticsConfig,
    file: vfs::FileId,
    within: Option<Arc<FxHashSet<ide_db::base_db::Crate>>>,
    (lints, refuse_borrowck): (bool, bool),
) -> Vec<(ide_db::FileRange, &'static str, String)> {
    use std::hash::BuildHasher as _;
    let crates = within.as_ref().map(|set| {
        let hashes = set.iter().map(|it| rustc_hash::FxBuildHasher.hash_one(it));
        (set.len(), hashes.fold(0, |all, it| all ^ it))
    });
    let key = (file, crates, (lints, refuse_borrowck));
    let skeleton = outline::skeleton(db, file);
    let was = skeleton.and_then(|now| lock(&CLEAN).remove(&key).filter(|it| it.skeleton == now));
    let functions = std::cell::RefCell::new(FxHashMap::default());
    let asked_of = std::cell::Cell::new((0usize, 0usize));
    let asked = |body: hir::DefWithBody| {
        let hir::DefWithBody::Function(func) = body else { return was.is_none() };
        let written = hir::HasSource::source(func, db)
            .map(|it| rustc_hash::FxBuildHasher.hash_one(it.value.syntax().text().to_string()));
        let Some(written) = written else { return true };
        functions.borrow_mut().insert(func, written);
        let asked = was.as_ref().is_none_or(|was| was.functions.get(&func) != Some(&written));
        let (n, of) = asked_of.get();
        asked_of.set((n + usize::from(asked), of + 1));
        asked
    };
    let found: Vec<_> = ide_diagnostics::semantic_diagnostics_of_bodies(
        db,
        config,
        &ide_db::assists::AssistResolveStrategy::None,
        file,
        within,
        &asked,
    )
    .into_iter()
    .filter(|it| {
        let code = it.code.as_str();
        // rustc says nothing of most lints in what another crate's macro expands to.
        let expanded = it.main_node.is_some_and(|it| it.file_id.is_macro());
        it.severity == ide_db::Severity::Error
            && (SURE.contains(&code)
                || refuse_borrowck && BORROWCK.contains(&code)
                || lints
                    && !expanded
                    && (DENIED.contains(&code)
                        || refuse_borrowck && DENIED_BORROWCK.contains(&code)))
    })
    .map(|it| (it.range, it.code.as_str(), it.message))
    .collect();
    if std::env::var_os("BLAZE_STATS").is_some() {
        let (n, of) = asked_of.get();
        eprintln!("check: {file:?}: the bodies of {n} of {of} functions asked");
    }
    if let Some(skeleton) = skeleton.filter(|_| found.is_empty()) {
        lock(&CLEAN).insert(key, Clean { skeleton, functions: functions.into_inner() });
    }
    found
}

/// The text of the source file at `path` as the database has it.
fn source_on_disk(path: &Path) -> std::io::Result<String> {
    let text = std::fs::read_to_string(path)?;
    let rust = path.extension().is_some_and(|it| it == "rs");
    Ok(if rust { load_cargo::source_text(text) } else { text })
}

/// A file whose changes the daemon picks up.
enum Watched {
    Source(vfs::FileId),
    /// Named by an `include_bytes!` or `include_str!`, and not a source file.
    Included(AbsPathBuf),
}

struct WatchedFile {
    file: Watched,
    path: PathBuf,
    /// `None` for one taken for a changed one, or an input that is not there.
    mtime: Option<SystemTime>,
}

/// What [`WatchedFile::include_sources`] took in.
#[derive(Clone, Copy, PartialEq)]
enum Loaded {
    Nothing,
    /// Files that rustc read as they are now: what it linked is of them.
    AsRustcRead,
    New,
}

/// The registry directories the loaded workspaces' dependencies were installed in, their
/// sysroots' sources, and the target directories of those workspaces: what
/// [`WatchedFile::immutable`] does not watch. They are the directories themselves, not their
/// names, since a package can have modules of its own in a directory named `registry`,
/// `rustlib` or `target`.
#[derive(Default)]
struct Unedited {
    installed: Vec<PathBuf>,
    targets: Vec<PathBuf>,
}

static UNEDITED: Mutex<Unedited> =
    Mutex::new(Unedited { installed: Vec::new(), targets: Vec::new() });

impl Unedited {
    fn of(workspaces: &[ProjectWorkspace]) -> Unedited {
        let mut unedited = Unedited::default();
        let install = |installed: &mut Vec<PathBuf>, dir: PathBuf| {
            if !installed.contains(&dir) {
                installed.push(dir);
            }
        };
        for workspace in workspaces {
            // Only the library's sources: the sysroot itself can be `/usr`, with projects below.
            if let Some(library) = workspace.sysroot.rust_lib_src_root() {
                install(&mut unedited.installed, PathBuf::from(library.to_string()));
            }
            // The sysroot's own dependencies are among the roots too, not only the workspace's.
            for root in workspace.to_roots().into_iter().filter(|it| !it.is_local) {
                for dir in root.include.iter().map(|it| PathBuf::from(it.to_string())) {
                    // A registry by its layout, `<home>/registry/src/<index>/<package>`: a
                    // directory only named `registry` above a vendored package can be the
                    // project's own.
                    let Some(home) = cargo::home::cargo_home_of(&dir) else { continue };
                    let registry = home.join("registry");
                    if dir.starts_with(&registry) {
                        install(&mut unedited.installed, registry);
                    }
                }
            }
            if let ProjectWorkspaceKind::Cargo { cargo, .. } = &workspace.kind {
                unedited.targets.push(PathBuf::from(cargo.target_directory().to_string()));
            }
        }
        unedited
    }
}

impl WatchedFile {
    /// Whether the file is never written again: everything is watched but the sysroot, the
    /// registry, the Nix store (crates vendored by Nix, which `nix run` of the demo reads its
    /// thousands of files from) and what a build made, as path dependencies live outside the
    /// project. What a build script wrote is written again when it runs again. Each watched
    /// file is looked at on every edit, so one too many is time on every keystroke.
    fn immutable(path: &Path) -> bool {
        let names: Vec<&str> = path.components().filter_map(|c| c.as_os_str().to_str()).collect();
        let unedited = lock(&UNEDITED);
        path.starts_with("/nix/store")
            || cargo::toolchain::library::has(path)
            || unedited.installed.iter().any(|it| path.starts_with(it))
            || WatchedFile::fetched(&names)
            || unedited.targets.iter().any(|it| path.starts_with(it))
                && !cargo::script::in_out_dir(path)
    }

    /// Whether a file is in what cargo fetched: `registry/src/<index>-<hash>/` or
    /// `git/checkouts/<repository>-<hash>/`. A directory of the checkout that is only named
    /// `registry` is edited like any other (cargo's own `src/cargo/sources/registry`).
    fn fetched(names: &[&str]) -> bool {
        let hashed = |name: &str| {
            name.rsplit_once('-').is_some_and(|(_, hash)| {
                hash.len() == 16 && hash.bytes().all(|it| it.is_ascii_hexdigit())
            })
        };
        names.windows(3).any(|it| {
            matches!((it[0], it[1]), ("registry", "src") | ("git", "checkouts")) && hashed(it[2])
        })
    }

    /// `path` is what the file is known as, `on_disk` where it is read from.
    fn new(file: Watched, path: &Path, on_disk: &Path) -> Option<WatchedFile> {
        if WatchedFile::immutable(path) {
            return None;
        }
        let mtime = std::fs::metadata(on_disk).ok()?.modified().ok()?;
        Some(WatchedFile { file, path: path.to_owned(), mtime: Some(mtime) })
    }

    /// The files of the VFS that are text. One that is not UTF-8 can only be named by an
    /// `include_bytes!`, and is watched as [`Watched::Included`] once it is.
    ///
    /// They were read from `read_from` on, and a file written since may have been read before
    /// the write and have its timestamp taken after it: one whose text is not the database's
    /// is taken for a changed one, or every build after would be of the text before the write.
    fn sources(vfs: &Vfs, db: &RootDatabase, read_from: SystemTime) -> Vec<WatchedFile> {
        // A timestamp can be before the clock's time of the write: some file systems keep
        // whole or even seconds, and Linux takes it from a clock that ticks every few
        // milliseconds.
        let since = read_from.checked_sub(Duration::from_secs(2)).unwrap_or(SystemTime::UNIX_EPOCH);
        vfs.iter()
            .filter(|&(id, _)| db.has_file_text(id))
            .filter_map(|(id, path)| {
                let path: &Path = path.as_path()?.as_ref();
                let mut watched = WatchedFile::new(Watched::Source(id), path, path)?;
                if watched.mtime.is_some_and(|at| at >= since)
                    && source_on_disk(path).is_ok_and(|text| *text != **db.file_text(id).text(db))
                {
                    watched.mtime = None;
                }
                Some(watched)
            })
            .collect()
    }

    /// Puts the files an `include!` named that were not loaded in their source roots, and
    /// watches them: whether there were any, and whether each was what rustc read.
    fn include_sources(
        files: &mut Vec<WatchedFile>,
        tree: &mut tree::Tree,
        db: &mut RootDatabase,
    ) -> Loaded {
        let added = tree.included(db);
        if added.is_empty() {
            return Loaded::Nothing;
        }
        prepare_macro_sessions(db);
        let mut was = FxHashMap::default();
        files.retain(|it| {
            let watched = matches!(it.file, Watched::Included(_))
                && added.iter().any(|(_, path)| *path == it.path);
            if watched {
                was.insert(it.path.clone(), it.mtime);
            }
            !watched
        });
        let mut as_rustc_read = true;
        for (file, path) in &added {
            let now = WatchedFile::new(Watched::Source(*file), path, &db.on_disk(path));
            as_rustc_read &= now.as_ref().is_some_and(|it| was.get(path) == Some(&it.mtime));
            files.extend(now);
        }
        if as_rustc_read { Loaded::AsRustcRead } else { Loaded::New }
    }

    /// Whether a build included a file that is not watched yet.
    fn unwatched(files: &[WatchedFile], db: &RootDatabase) -> bool {
        let watched: FxHashSet<&Path> = files.iter().map(|it| &*it.path).collect();
        db.included_files()
            .iter()
            .map(AsRef::<Path>::as_ref)
            .any(|it| !watched.contains(it) && !WatchedFile::immutable(it))
    }

    /// Watches the files the last build included that are not watched yet, writing nothing to
    /// the database, so the builds still under way go on. Each is compared with what the build
    /// read after its timestamp is taken, so a write between the build's read and now is not
    /// lost: one that differs is watched as changed ([`WatchedFile::mtime`] `None`), and the
    /// scan for changed files reads it in once the builds are settled.
    fn watch_included(files: &mut Vec<WatchedFile>, db: &RootDatabase) {
        let watched: FxHashSet<&Path> = files.iter().map(|it| &*it.path).collect();
        let new: Vec<AbsPathBuf> = db
            .included_files()
            .into_iter()
            .filter(|it| !watched.contains(AsRef::<Path>::as_ref(it)))
            .collect();
        for path in new {
            if WatchedFile::immutable(path.as_ref()) {
                continue;
            }
            // One that is not there is watched for when it is, as a new file is.
            let on_disk = db.on_disk(path.as_ref());
            let mtime = std::fs::metadata(&on_disk).and_then(|it| it.modified()).ok();
            let mtime = mtime.filter(|_| db.included_is_read(&path));
            files.push(WatchedFile {
                file: Watched::Included(path.clone()),
                path: AsRef::<Path>::as_ref(&path).to_owned(),
                mtime,
            });
        }
    }
}

/// Compiles every crate asked for, each as it would be alone.
/// Builds each crate into `<out>/<crate>.o`. The crates are separate programs that share only
/// the workspace, so each one sees the symbols of its own rlib and of the rlibs that belong to
/// none of them: a symbol from another crate's rlib would not be there at link time.
fn build_batch(db: &RootDatabase, args: Args) -> anyhow::Result<()> {
    let (out, report) = (args.out.clone(), args.report.clone().context("a batch needs --report")?);
    let crates: Vec<_> =
        std::iter::once(args.krate.clone()).chain(args.batch.iter().cloned()).collect();
    let rlib_of = |file: &Path| {
        let name = file.file_name()?.to_str()?.strip_prefix("lib")?;
        let name = name.rsplit_once('-').map_or(name, |(name, _)| name);
        crates.iter().find(|krate| krate.as_str() == name)
    };
    for krate in &crates {
        let symbol_files: Vec<_> = args
            .symbol_files
            .iter()
            .filter(|file| rlib_of(file).is_none_or(|owner| owner == krate))
            .cloned()
            .collect();
        let table = &SymbolTable::load(&symbol_files, &args.root()?)?;
        let args = Args {
            project: args.project.clone(),
            krate: krate.clone(),
            batch: Vec::new(),
            widened: Vec::new(),
            out: out.join(format!("{krate}.o")),
            symbol_files: Vec::new(),
            watch: false,
            socket: None,
            link_cmd: None,
            patch: None,
            splice: None,
            report: Some(report.join(format!("{krate}.tsv"))),
            deny: None,
            isolate: None,
            prebuilt: args.prebuilt.clone(),
            exe: args.exe.clone(),
            shared: args.shared,
            staticlib: args.staticlib,
            dep_info: None,
            inputs: None,
            library: args.library,
            run_tool: args.run_tool,
            cfg_test: args.cfg_test,
            unit_test: args.unit_test,
            executables: args.executables.clone(),
            scratch: args.scratch.clone(),
            own_target: args.own_target,
            scripts_from: None,
            selection: args.selection.clone(),
            also: Vec::new(),
            debug: args.debug,
            lints_capped: args.lints_capped,
            link_args: args.link_args.clone(),
            checkout: args.checkout.clone(),
            base: args.base.clone(),
            members: args.members.clone(),
        };
        let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            build_unheld(db, table, &args, &mut Rejections::default(), &mut Listed::default())
        }));
        let error = match built {
            Ok(Ok(_)) => continue,
            Ok(Err(error)) => format!("error: {error:#}"),
            Err(panic) => format!("panicked: {}", panic_message(&*panic)),
        };
        std::fs::write(report.join(format!("{krate}.error")), error)?;
    }
    Ok(())
}

/// Leaves `said`, the errors in the source a worker found as it loaded the workspace, for
/// the daemon to fail each build with.
fn errors_for_the_daemon(args: &Args, said: &errors::Said) -> anyhow::Result<()> {
    // Whole or not at all: the daemon reads the file as soon as it is there.
    if let Some(socket) = &args.socket {
        let errors = cargo::daemon::errors_beside(socket);
        let writing = errors.with_extension("writing");
        std::fs::write(&writing, serde_json::to_string(said)?)?;
        std::fs::rename(&writing, &errors)?;
    }
    Ok(())
}

/// What a client asks of the daemon, as the one line it sends.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Request {
    /// Bring the objects and the linked program up to date with the files on disk.
    Sync,
    /// Bring the objects up to date and leave the linking to the client, which loads the
    /// patch object into a program that is running.
    Patch,
    /// An editor's text, built as `Sync` is: the program that runs has the edit before the
    /// executable on disk does, which is written once the edits stop.
    Edit,
}

impl Request {
    fn parse(line: &str) -> Option<Request> {
        match line {
            "sync" => Some(Request::Sync),
            "patch" => Some(Request::Patch),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Request::Sync => "sync",
            Request::Patch => "patch",
            Request::Edit => "edit",
        }
    }
}

/// The program of the workspace a request is for, and where its objects go.
#[derive(Clone)]
struct Target {
    /// The checkout it is a program of. `None` is the one the daemon was started in.
    project: Option<PathBuf>,
    krate: String,
    /// The other crates of the workspace that are in it, when the client says: the names
    /// after the first in `--crate a,b,c`. Otherwise the ones the daemon was started with.
    batch: Option<Vec<String>>,
    out: PathBuf,
    patch: Option<PathBuf>,
    link_cmd: Option<String>,
    splice: Option<PathBuf>,
    /// The executable blaze links the program into itself, with nothing of rustc's: a build
    /// from nothing (docs/no-rustc.md, step K).
    exe: Option<PathBuf>,
    /// `exe` is a shared library, a `cdylib` (#262).
    shared: bool,
    /// `exe` is the archive of a `staticlib`, with `shared` ([`Args::staticlib`]).
    staticlib: bool,
    /// It is the unit tests of the target `krate` names ([`Args::unit_test`]).
    unit_test: bool,
    /// What its link takes that rustc compiled. Code rustc instantiated for one program is in
    /// that program's own objects, so another has to be told where its copies are. Empty for
    /// the files the daemon was started with.
    symbol_files: Vec<PathBuf>,
}

impl Target {
    /// What a client would send to have the program of `args` built.
    fn of(args: &Args) -> Target {
        Target {
            project: Some(args.project.clone()),
            krate: args.krate.clone(),
            batch: Some(args.batch.clone()),
            out: args.out.clone(),
            patch: args.patch.clone(),
            link_cmd: args.link_cmd.clone(),
            splice: args.splice.clone(),
            exe: args.exe.clone(),
            shared: args.shared,
            staticlib: args.staticlib,
            unit_test: args.unit_test,
            symbol_files: args.symbol_files.clone(),
        }
    }

    fn is(&self, args: &Args) -> bool {
        self.of_crate(args) && self.out == args.out
    }

    /// Whether `args` builds the crate this program does: a binary and its unit tests are two
    /// programs of one root file.
    fn of_crate(&self, args: &Args) -> bool {
        self.krate == args.krate && self.unit_test == args.unit_test
    }

    /// The daemon's arguments, were it started for this program.
    fn args(self, daemon: &Args, checkouts: &Checkouts) -> anyhow::Result<Args> {
        let Target {
            project,
            krate,
            batch,
            out,
            patch,
            link_cmd,
            splice,
            exe,
            shared,
            staticlib,
            unit_test,
            symbol_files,
        } = self;
        // Only a load with `cfg(test)` has the crate of a target's unit tests.
        anyhow::ensure!(
            !unit_test || daemon.cfg_test,
            "the unit tests of `{krate}` are asked of a worker that loaded the workspace \
             without `cfg(test)`"
        );
        let project = project.unwrap_or_else(|| daemon.project.clone());
        let Parts { root: checkout, base, members, debug, lints_capped, link_args } =
            checkouts.parts(&project)?;
        let symbol_files =
            if symbol_files.is_empty() { daemon.symbol_files.clone() } else { symbol_files };
        let batch = batch.unwrap_or_else(|| {
            let started_with = std::iter::once(&daemon.krate).chain(&daemon.batch);
            started_with.filter(|&it| *it != krate).cloned().collect()
        });
        Ok(Args {
            project,
            checkout,
            base,
            members,
            debug,
            lints_capped,
            link_args,
            krate,
            batch,
            widened: Vec::new(),
            out,
            patch,
            link_cmd,
            splice,
            exe,
            shared,
            staticlib,
            unit_test,
            symbol_files,
            ..daemon.clone()
        })
    }

    /// From what follows the socket on the command line of a client.
    fn from_flags(mut flags: impl Iterator<Item = String>) -> anyhow::Result<Option<Target>> {
        let (mut krate, mut out, mut patch, mut link_cmd) = (None, None, None, None);
        let (mut project, mut splice, mut exe) = (None, None, None);
        let mut symbol_files = Vec::new();
        let mut unit_test = false;
        while let Some(flag) = flags.next() {
            if flag == "--unit-test" {
                unit_test = true;
                continue;
            }
            let value = flags.next().with_context(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--crate" => krate = Some(value),
                "--project" => project = Some(PathBuf::from(value)),
                "--out" => out = Some(PathBuf::from(value)),
                "--patch" => patch = Some(PathBuf::from(value)),
                "--link-cmd" => link_cmd = Some(value),
                "--binary" | "--splice" => splice = Some(PathBuf::from(value)),
                "--exe" => exe = Some(PathBuf::from(value)),
                "--symbols" => symbol_files.push(PathBuf::from(value)),
                _ => bail!("unexpected argument {flag}"),
            }
        }
        match (krate, out) {
            (Some(krate), Some(out)) => {
                let (krate, batch) = Target::crates(&krate);
                Ok(Some(Target {
                    project,
                    krate,
                    batch,
                    out,
                    patch,
                    link_cmd,
                    splice,
                    exe,
                    shared: false,
                    staticlib: false,
                    unit_test,
                    symbol_files,
                }))
            }
            (None, None)
                if patch.is_none()
                    && !unit_test
                    && link_cmd.is_none()
                    && splice.is_none()
                    && exe.is_none()
                    && symbol_files.is_empty()
                    && project.is_none() =>
            {
                Ok(None)
            }
            _ => bail!("a program is named by --crate and --out together"),
        }
    }

    /// The fields after the request's name on its line, each after a tab. Empty is absent.
    fn parse(fields: &[&str]) -> Option<Target> {
        let &[
            project,
            krate,
            out,
            patch,
            link_cmd,
            splice,
            exe,
            shared,
            unit_test,
            ref symbol_files @ ..,
        ] = fields
        else {
            return None;
        };
        let given = |it: &str| (!it.is_empty()).then(|| it.to_owned());
        let (krate, batch) = Target::crates(krate);
        Some(Target {
            project: given(project).map(PathBuf::from),
            krate,
            batch,
            out: PathBuf::from(out),
            patch: given(patch).map(PathBuf::from),
            link_cmd: given(link_cmd),
            splice: given(splice).map(PathBuf::from),
            exe: given(exe).map(PathBuf::from),
            shared: shared == "shared" || shared == "staticlib",
            staticlib: shared == "staticlib",
            unit_test: unit_test == cargo::plan::UNIT_TEST,
            symbol_files: symbol_files.iter().map(PathBuf::from).collect(),
        })
    }

    /// The program's crate and the others in it, from `a,b,c`. A name with no comma says
    /// nothing of the others, and `a,` says there are none.
    fn crates(named: &str) -> (String, Option<Vec<String>>) {
        match named.split_once(',') {
            Some((krate, batch)) => {
                let batch = batch.split(',').filter(|it| !it.is_empty()).map(str::to_owned);
                (krate.to_owned(), Some(batch.collect()))
            }
            None => (named.to_owned(), None),
        }
    }

    fn fields(&self) -> String {
        let crates = match &self.batch {
            Some(batch) => format!("{},{}", self.krate, batch.join(",")),
            None => self.krate.clone(),
        };
        let mut fields = format!(
            "\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.project.as_ref().map(|it| it.display().to_string()).unwrap_or_default(),
            crates,
            self.out.display(),
            self.patch.as_ref().map(|it| it.display().to_string()).unwrap_or_default(),
            self.link_cmd.as_deref().unwrap_or_default(),
            self.splice.as_ref().map(|it| it.display().to_string()).unwrap_or_default(),
            self.exe.as_ref().map(|it| it.display().to_string()).unwrap_or_default(),
            match (self.shared, self.staticlib) {
                (true, true) => "staticlib",
                (true, false) => "shared",
                (false, _) => "",
            },
            if self.unit_test { cargo::plan::UNIT_TEST } else { "" }
        );
        for file in &self.symbol_files {
            fields.push('\t');
            fields.push_str(&file.display().to_string());
        }
        fields
    }
}

/// What the daemon keeps for a program it builds. The code compiled for one program is there
/// for the others: only the objects and what went into them are its own.
struct Program {
    args: Args,
    table: Arc<SymbolTable>,
    known: Rejections,
    unbuilt: Unbuilt,
    /// The object the last build was asked for.
    wrote: Option<PathBuf>,
    /// The object with all of the program, which a patch is the difference from.
    whole: Option<PathBuf>,
    /// How many of its builds a change to the source has interrupted since one finished.
    interrupted: u32,
    /// Whether its crates are others than the last build was of, which is then no patch.
    moved: bool,
    /// The tests of its crate when it was first built, which is how rustc built it.
    tests: Option<Tests>,
    /// The tests last found, with the listings of the crates they were found in. A listing
    /// stays until an edit to more than bodies, which is all that changes a test.
    tests_now: Option<(Vec<Arc<ListedCrate>>, Tests)>,
    /// A whole build's own link line for each executable it links the program at: one for
    /// each checkout read as the one that was loaded.
    own_lines: FxHashMap<PathBuf, String>,
}

/// The functions of a program's own crate that a test harness runs, each with its
/// attributes. The harness of a test executable is rustc's: it lists the tests by name and
/// by what `#[ignore]` and `#[should_panic]` say of them, so a build after the list changed
/// would run the tests of before.
#[derive(Clone, PartialEq)]
struct Tests(Vec<(String, String)>);

impl Tests {
    /// The tests as the crates are listed now: those `found` when the listings are the same.
    fn of(
        db: &RootDatabase,
        listed: &Listed,
        args: &Args,
        found: &mut Option<(Vec<Arc<ListedCrate>>, Tests)>,
    ) -> Tests {
        use hir::HasSource as _;
        use syntax::ast::HasAttrs as _;
        let crates = crates_named(db, &args.krate, args).unwrap_or_default();
        let listings: Vec<_> = crates.into_iter().map(|it| listed.of(db, it)).collect();
        if let Some((was, tests)) = found.as_ref() {
            let same = |(a, b): (&Arc<ListedCrate>, &Arc<ListedCrate>)| Arc::ptr_eq(a, b);
            if was.len() == listings.len() && was.iter().zip(&listings).all(same) {
                return tests.clone();
            }
        }
        let mut tests = Vec::new();
        for listing in &listings {
            for &id in &listing.functions {
                let function = hir::Function::from(id);
                if !function.is_test(db) && !function.is_bench(db) {
                    continue;
                }
                let module = function.module(db);
                let mut path: Vec<String> = module
                    .path_to_root(db)
                    .into_iter()
                    .rev()
                    .filter_map(|it| Some(it.name(db)?.as_str().to_owned()))
                    .collect();
                path.push(function.name(db).as_str().to_owned());
                let attrs = function.source(db).map(|it| {
                    let attrs = it.value.attrs().map(|it| it.syntax().text().to_string());
                    attrs.collect::<Vec<_>>().join(" ")
                });
                tests.push((path.join("::"), attrs.unwrap_or_default()));
            }
        }
        tests.sort();
        let tests = Tests(tests);
        *found = Some((listings, tests.clone()));
        tests
    }

    /// Why the program can't be built by the daemon, when its tests are not these.
    fn changed(&self, now: &Tests) -> Option<String> {
        if self == now {
            return None;
        }
        let differs =
            |a: &Tests, b: &Tests| a.0.iter().find(|it| !b.0.contains(it)).map(|it| it.0.clone());
        let which = differs(now, self).or_else(|| differs(self, now)).unwrap_or_default();
        Some(format!(
            "the test `{which}` was added, removed or given other attributes, and the list of \
             tests is rustc's to write: blaze wrote no test harness for the program (no-rustc H, \
             #88)"
        ))
    }
}

/// A request, the program it is for, and who waits for the answer.
type Asked = (Request, Option<Target>, Client);

/// Who asked for a build, and the variables of its process.
struct Client {
    stream: UnixStream,
    env: ide_db::base_db::Env,
}

impl Client {
    /// A client that hears nothing: the one a build the daemon asks for itself answers.
    #[cfg(unix)]
    fn nobody() -> Option<Client> {
        let (stream, _) = UnixStream::pair().ok()?;
        Some(Client { stream, env: ide_db::base_db::Env::default() })
    }

    #[cfg(not(unix))]
    fn nobody() -> Option<Client> {
        None
    }
}

/// Compiles what the tier watcher asked for of the program at `at`, on a thread of its own,
/// and sends what it compiled as an [`Event::Tiered`].
fn compile_tiered(
    db: &RootDatabase,
    known: &Rejections,
    at: usize,
    ask: tier::Ask,
    sender: std::sync::mpsc::Sender<Event>,
) {
    use codegen_tier::Level;
    let tier::Ask { binary, wants, hot, mut counters } = ask;
    let hot: Vec<codegen_tier::Node> = hot.iter().filter_map(|it| known.tier_node(it)).collect();
    let mut asked = Vec::new();
    for want in wants {
        let Some(node) = known.tier_node(&want.symbol) else {
            tier::theirs(&binary, &want.symbol, "rustc's code, which blaze does not tier");
            continue;
        };
        let asked_for = codegen_tier::Asked {
            node,
            level: want.level,
            hot: hot.clone(),
            profile: want.profile,
        };
        asked.push((want.symbol, asked_for));
    }
    if asked.is_empty() {
        tier::done(&binary, Vec::new(), counters);
        return;
    }
    let (db, tiering) = (db.clone(), Tiering::start());
    let asked_of = binary.clone();
    // Each body built in or shown is built inside the one that calls it, up to ten deep, with
    // the frames of a whole function's build each; a thread's default 2 MiB does not hold that.
    let thread = std::thread::Builder::new().name("tier".to_owned()).stack_size(64 << 20);
    let spawned = thread.spawn(move || {
        // The IR of every function is built with the database, which is let go of before
        // LLVM runs: a change to the source waits for the database to be let go of, and
        // so for nothing LLVM does.
        let prepared: Vec<_> = asked
            .into_iter()
            .map(|(symbol, asked)| {
                let start = Instant::now();
                let prepared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    hir::attach_db(&db, || codegen_tier::prepare(&db, &asked))
                }))
                .unwrap_or_else(|_| Err("an edit came first".to_owned()));
                (symbol, asked.node, asked.level, prepared, start.elapsed())
            })
            .collect();
        drop((db, tiering));
        let (mut code, mut done) = (Vec::new(), Vec::new());
        for (symbol, node, level, prepared, with_db) in prepared {
            let room = blaze_live::format::COUNTERS as u32;
            let counting =
                (level == Level::Llvm && tier::highest() == Level::Pgo).then_some((counters, room));
            let start = Instant::now();
            let compiled = prepared.and_then(|prepared| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| prepared.finish(counting)))
                    .unwrap_or_else(|_| Err("LLVM's compile of it panicked".to_owned()))
            });
            eprintln!(
                "tier-timing: {symbol}: IR {with_db:.1?} with the database, LLVM {:.1?} without",
                start.elapsed()
            );
            // The executable may not link what LLVM's code calls (`memcmp` where rustc's calls
            // `bcmp`), and a patch that calls it would leave the program taking none at all.
            let compiled = compiled.and_then(|tiered| {
                let missing = tiered.symbols().find(|it| !can_call(&binary, it)).map(str::to_owned);
                match missing {
                    Some(name) => {
                        Err(format!("it calls {name}, which the executable does not link"))
                    }
                    None => Ok(tiered),
                }
            });
            let outcome = compiled.map(|tiered| {
                let used = tiered.counters_used();
                if let Some(first) = tiered.counters {
                    counters = first + used;
                }
                let compiled = tier::Compiled {
                    took: tiered.took,
                    inlined: tiered.inlined_count,
                    branches: tiered.branches,
                    counters: tiered.counters.map(|first| (first, used)),
                };
                code.push((node, tiered));
                compiled
            });
            done.push(tier::Done { symbol, level, outcome });
        }
        let _ = sender.send(Event::Tiered(at, Tiered { binary, code, done, counters }));
    });
    if let Err(why) = spawned {
        eprintln!("tier: no thread to compile on: {why}");
        tier::done(&asked_of, Vec::new(), counters);
    }
}

/// A compile at a higher level that holds the database, which [`let_go`] waits to have none.
struct Tiering;

static TIERING: AtomicUsize = AtomicUsize::new(0);

impl Tiering {
    fn start() -> Self {
        TIERING.fetch_add(1, Ordering::Relaxed);
        Tiering
    }
}

impl Drop for Tiering {
    fn drop(&mut self) {
        TIERING.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Lets go now of what the queries with an `lru` keep past it, the trees parsed and the bodies
/// lowered for the builds that are over, which would otherwise wait for the next edit. Only
/// when nothing else holds the database: this cancels what does.
fn let_go(db: &mut RootDatabase, builds: &Builds, programs: &[Program]) {
    use ide_db::base_db::salsa::Database;
    // An own base an edit interrupted is built again from what it had computed, which letting
    // it go would have it compute again (#261): only once it is built, or given up.
    let base_to_come = (programs.iter().enumerate())
        .any(|(at, it)| !builds.given_up.contains(&at) && it.known.wants_base());
    if builds.running.is_empty() && TIERING.load(Ordering::Relaxed) == 0 && !base_to_come {
        db.trigger_lru_eviction();
        // An idle worker allocates nothing, and jemalloc returns freed pages only as it
        // allocates, so what the build and the eviction freed would stay resident.
        memory::give_back();
    }
}

/// How many values of the queries that hold the most of a build in full salsa keeps past the
/// revision of the build: the lowered and the runtime MIR of each body (and their
/// monomorphized copies), the code of each instance and the bodies to inline. A worker's own
/// base computes all of them for the whole program; kept, they were about 4.5 GB of reth's
/// 13 GB live (#261). One let go is computed again if an edit asks for it, which only the
/// first edit after a build in full does much of. `BLAZE_LRU` (`mir=N,runtime=N,compiled=N,
/// inlinable=N`) sets any of them; 0 keeps every value.
fn memo_capacities(db: &mut RootDatabase) -> anyhow::Result<()> {
    let (mut mir, mut runtime, mut compiled, mut inlinable) = (2048, 2048, 16384, 8192);
    let said = match std::env::var("BLAZE_LRU") {
        Ok(said) => said,
        Err(std::env::VarError::NotPresent) => String::new(),
        Err(why) => bail!("BLAZE_LRU: {why}"),
    };
    for it in said.split(',').filter(|it| !it.trim().is_empty()) {
        let parsed =
            it.split_once('=').and_then(|(name, n)| Some((name.trim(), n.trim().parse().ok()?)));
        match parsed {
            Some(("mir", n)) => mir = n,
            Some(("runtime", n)) => runtime = n,
            Some(("compiled", n)) => compiled = n,
            Some(("inlinable", n)) => inlinable = n,
            _ => bail!(
                "BLAZE_LRU: `{it}` is not one of mir, runtime, compiled or inlinable = a number"
            ),
        }
    }
    hir_ty_mir::mir::set_mir_capacity(db, mir);
    hir_ty_mir::mir::set_runtime_mir_capacity(db, runtime);
    hir_ty_codegen::set_code_capacity(db, compiled, inlinable);
    Ok(())
}

impl std::io::Write for &Client {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        (&self.stream).write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        (&self.stream).flush()
    }
}

/// What the daemon waits for.
enum Event {
    Asked(Asked),
    /// The text of a source file as an editor has it, which it has not written yet.
    Edited(Edited),
    /// The build of a program is over, with what to tell its clients: nothing when a change
    /// to the source interrupted it.
    Built(usize, Box<Program>, Option<anyhow::Result<Result<String, Refused>>>),
    /// The link of a program's last build failed, after the build was over.
    Unlinked(usize),
    /// The tier watcher asks for functions of a running program at a higher level.
    Tier(tier::Ask),
    /// Those functions are compiled, for the program at the index: what to patch it with,
    /// and what to tell the watcher.
    Tiered(usize, Tiered),
    /// The daemon's own base of the program at the index holds more memory than it may.
    OverBudget(usize),
}

/// The functions of a program compiled at a higher level, for its next patch.
struct Tiered {
    binary: PathBuf,
    code: Vec<(codegen_tier::Node, codegen_tier::Tiered)>,
    done: Vec<tier::Done>,
    counters: u32,
}

/// An `edit` request: the programs that link the file are built from `text` at once, with
/// no look at the disk, and the client is answered as one that asked for a build of them.
struct Edited {
    path: PathBuf,
    text: String,
    client: Client,
    /// When it was read.
    at: Instant,
}

/// Set while the builds under way are told to stop for a change to the source.
static INTERRUPTING: AtomicBool = AtomicBool::new(false);

/// How many builds have begun to link. An interruption waits for them: a link is not stopped,
/// and once it has begun the executable it writes is the program, which a build interrupted
/// after it would leave the daemon without. The daemon's own base, once it links, has
/// replaced the executable rustc linked.
static LINKING: Mutex<usize> = Mutex::new(0);

thread_local! {
    /// Whether the build on this thread is one of [`LINKING`].
    static LINKS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Called before a build links, which it does not once an interruption has begun: it then
/// unwinds as an interrupted build does.
fn begin_link() {
    if LINKS.get() {
        return;
    }
    let mut linking = lock(&LINKING);
    if INTERRUPTING.load(Ordering::SeqCst) {
        drop(linking);
        std::panic::resume_unwind(Box::new(ide_db::base_db::salsa::Cancelled::PendingWrite));
    }
    *linking += 1;
    LINKS.set(true);
}

/// After a build on this thread is over: it holds up no interruption.
fn end_link() {
    if LINKS.replace(false) {
        *lock(&LINKING) -= 1;
    }
}

/// Reading a loaded checkout from another place is long when the build takes more than this,
/// or than `BLAZE_SLOW_SWITCH_MS`.
const SLOW_SWITCH: Duration = Duration::from_secs(2);

/// A checkout that is long to turn to or from this many times running is loaded instead.
const SLOW_SWITCHES: u32 = 3;

/// A build that is interrupted this many times is waited for the next time.
const INTERRUPTIONS: u32 = 3;

/// The clients of a program that is being built.
struct Running {
    clients: Vec<Asked>,
    /// Whether a change to the source waits for the build: one that patches is short, and
    /// has nothing to patch once it is interrupted.
    kept: bool,
    /// Whether it is the build in full that nobody asked for, of a base of the daemon's own
    /// while edits patch what rustc linked: whoever asks for the program interrupts it.
    own_base: bool,
}

/// How long the daemon waits with nothing asked of it before it builds a base of its own
/// for edits that patch what rustc linked.
const OWN_BASE_AFTER: Duration = Duration::from_secs(1);

/// Bytes the daemon may hold while it builds a base of its own, which no edit waits for:
/// what `BLAZE_OWN_BASE_MEMORY` says (a size, or `off` for no bound), else a quarter of the
/// machine's memory. What the build computed stays, so the bound is on what the daemon keeps
/// after it.
fn own_base_budget() -> Option<u64> {
    match std::env::var("BLAZE_OWN_BASE_MEMORY") {
        Ok(it) if it == "off" => None,
        it => it
            .ok()
            .and_then(|it| blaze_store::parse_size(&it))
            .or_else(|| memory::of_the_machine().map(|it| it / 4)),
    }
}

/// The builds under way and the requests that wait for them. A build reads the database on
/// threads of its own, so requests for other programs are not kept waiting by it.
struct Builds {
    events: std::sync::mpsc::Receiver<Event>,
    running: FxHashMap<usize, Running>,
    /// Requests that could not be built with the ones before them.
    waiting: std::collections::VecDeque<Asked>,
    /// Whether the requests that wait can only be built once something else happens.
    stuck: bool,
    /// The edits that wait, the newest of each file: one typed over before it was built is
    /// answered as superseded.
    edits: Vec<Edited>,
    /// What the tier watcher asked for that waits for its program's build to be over.
    tier_asks: Vec<tier::Ask>,
    /// Compiled functions that wait for their program's build to be over, and the programs
    /// to patch with what was compiled.
    tiered: Vec<(usize, Tiered)>,
    /// The programs to build again though no client asked: to patch in what was compiled at
    /// a higher level, or to compile what their last build took of the checkout from the
    /// store, which the first edit would otherwise wait for.
    unasked: Vec<usize>,
    /// The programs whose own base holds more memory than it may, to be interrupted.
    over_budget: Vec<usize>,
    /// The programs whose own base is not built again: one held more memory than it may.
    given_up: FxHashSet<usize>,
}

impl Builds {
    /// Gives the programs that are not being built what was compiled of them at a higher
    /// level, and has each patched with it.
    fn install_tiered(&mut self, programs: &mut [Program]) {
        let waiting = std::mem::take(&mut self.tiered);
        for (at, tiered) in waiting {
            if self.running.contains_key(&at) {
                self.tiered.push((at, tiered));
                continue;
            }
            let Tiered { binary, code, done, counters } = tiered;
            let patched = !code.is_empty();
            for (node, code) in code {
                programs[at].known.tiers.install(node, code);
            }
            tier::done(&binary, done, counters);
            if patched {
                programs[at].unbuilt.stale = true;
                if !self.unasked.contains(&at) {
                    self.unasked.push(at);
                }
            }
        }
    }

    /// Tells the tier watcher which functions the builds since the last call sent back to
    /// level 1.
    fn tell_demoted(&self, programs: &mut [Program]) {
        for (at, program) in programs.iter_mut().enumerate() {
            if self.running.contains_key(&at) {
                continue;
            }
            let demoted = program.known.tiers.take_demoted();
            let refused = program.known.tiers.take_refused();
            let Some(binary) = program.args.splice.clone() else { continue };
            for (node, why) in demoted {
                if let Some(symbol) = program.known.tier_symbol(&node) {
                    tier::demoted(&binary, &symbol, &why);
                }
            }
            for (node, level, why) in refused {
                if let Some(symbol) = program.known.tier_symbol(&node) {
                    tier::refused(&binary, &symbol, level, &why);
                }
            }
        }
    }

    /// Queues a request, or takes back the program of a build that is over and answers its
    /// clients, which is when it returns true. Those of a build that was interrupted ask again,
    /// ahead of the others.
    fn take(&mut self, event: Event, programs: &mut [Program]) -> anyhow::Result<bool> {
        self.stuck = false;
        let (at, program, answer) = match event {
            Event::Asked(asked) => {
                self.waiting.push_back(asked);
                return Ok(false);
            }
            Event::Edited(edited) => {
                if let Some(older) = self.edits.iter_mut().find(|it| it.path == edited.path) {
                    let _ = writeln!(&older.client, "ok superseded");
                    *older = edited;
                } else {
                    self.edits.push(edited);
                }
                return Ok(false);
            }
            Event::Built(at, program, answer) => (at, program, answer),
            Event::Unlinked(at) => {
                programs[at].unbuilt.stale = true;
                programs[at].wrote = None;
                return Ok(false);
            }
            Event::Tier(ask) => {
                self.tier_asks.push(ask);
                return Ok(false);
            }
            Event::Tiered(at, tiered) => {
                self.tiered.push((at, tiered));
                self.install_tiered(programs);
                return Ok(false);
            }
            Event::OverBudget(at) => {
                if self.running.get(&at).is_some_and(|it| it.own_base) {
                    self.over_budget.push(at);
                }
                return Ok(false);
            }
        };
        let Some(Running { clients, own_base, .. }) = self.running.remove(&at) else {
            return Ok(false);
        };
        programs[at] = *program;
        let program = &mut programs[at];
        let Some(answer) = answer else {
            eprintln!("interrupted: {}", program.args.krate);
            program.known.start_again();
            program.whole = None;
            program.unbuilt.stale = true;
            program.wrote = None;
            if own_base {
                // Nobody asked for it, and it is built again once nobody asks for anything,
                // unless it held more memory than it may. An edit that came as it started is
                // still to be built.
                if let Some(over) = self.over_budget.iter().position(|&it| it == at) {
                    self.given_up.insert(self.over_budget.swap_remove(over));
                }
                return Ok(false);
            }
            program.interrupted += 1;
            clients.into_iter().rev().for_each(|it| self.waiting.push_front(it));
            return Ok(false);
        };
        (program.interrupted, program.moved) = (0, false);
        self.over_budget.retain(|&it| it != at);
        let answer = answer?;
        if answer.is_ok() && program.dehydrate() && !self.unasked.contains(&at) {
            self.unasked.push(at);
        }
        // Before the answer: the client puts the parts of the profile together once it has it.
        trace::write_part();
        for (_, _, client) in clients {
            // A client that hangs up before the answer is not the daemon's problem.
            let _ = match &answer {
                Ok(built) => writeln!(&client, "ok {built}"),
                Err(why) => writeln!(&client, "{}", why.reply()),
            };
        }
        Ok(true)
    }

    /// Leaves no build under way, for the database to be written to: a write stops every
    /// thread that reads it. The builds that are kept are waited for, the others interrupted.
    #[track_caller]
    fn settle(&mut self, db: &mut RootDatabase, programs: &mut [Program]) -> anyhow::Result<()> {
        self.settle_waiting(db, programs, &FxHashSet::default())
    }

    /// [`Self::settle`], waiting for the builds of `waited` too, as for those that are kept.
    #[track_caller]
    fn settle_waiting(
        &mut self,
        db: &mut RootDatabase,
        programs: &mut [Program],
        waited: &FxHashSet<usize>,
    ) -> anyhow::Result<()> {
        use ide_db::base_db::salsa::Database as _;
        while self.running.iter().any(|(at, it)| it.kept || waited.contains(at)) {
            let event = self.events.recv()?;
            self.take(event, programs)?;
        }
        loop {
            if self.running.is_empty() {
                return Ok(());
            }
            let linking = lock(&LINKING);
            if *linking == 0 {
                eprintln!("settle: interrupting from {}", std::panic::Location::caller());
                INTERRUPTING.store(true, Ordering::SeqCst);
                break;
            }
            drop(linking);
            let event = self.events.recv()?;
            self.take(event, programs)?;
        }
        db.trigger_cancellation();
        while !self.running.is_empty() {
            let event = self.events.recv()?;
            self.take(event, programs)?;
        }
        INTERRUPTING.store(false, Ordering::SeqCst);
        Ok(())
    }
}

/// Where a build of a program goes. A checkout looked at as another has files of its own for
/// the program they share, a whole build's executable among them.
#[derive(Clone, PartialEq)]
struct Outputs {
    out: PathBuf,
    patch: Option<PathBuf>,
    link_cmd: Option<String>,
    splice: Option<PathBuf>,
    exe: Option<PathBuf>,
}

impl Outputs {
    fn of(args: &Args) -> Outputs {
        Outputs {
            out: args.out.clone(),
            patch: args.patch.clone(),
            link_cmd: args.link_cmd.clone(),
            splice: args.splice.clone(),
            exe: args.exe.clone(),
        }
    }
}

/// What a program's objects are behind by.
#[derive(Default)]
struct Unbuilt {
    /// Files edited since the last build that was not refused.
    edited: Vec<vfs::FileId>,
    /// Whether there is anything to build even when no file has changed since.
    stale: bool,
    /// Whether the last build was for a request that does not link, which leaves the
    /// executable to the next that does.
    unlinked: bool,
    /// Whether a client has named what rustc built of the checkout since the edits, as one
    /// does when the checkout is loaded again: the crates left to rustc are then as edited.
    named_anew: bool,
}

/// Sends `request` to the daemon listening on `socket` and waits for it. The answer is one
/// line: `ok unchanged`, or `ok patch=PATH ...` when the object at PATH holds the functions that
/// differ from the first build's, or `ok object=PATH ...` when the whole object was written
/// again.
fn ask(request: Request, socket: &Path, target: Option<Target>) -> anyhow::Result<()> {
    let mut stream =
        socket::connect(socket).with_context(|| format!("no daemon at {}", socket.display()))?;
    let target = target.map(|it| it.fields()).unwrap_or_default();
    let env = process::encode(&process::own());
    stream.write_all(format!("{}{target}\n{env}\n", request.name()).as_bytes())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    let reply = reply.trim_end();
    anyhow::ensure!(reply.starts_with("ok"), "daemon: {reply}");
    println!("{}", trace::without_spans(reply));
    Ok(())
}

/// Loads the checkout of `project` into the database of a daemon that is running, and
/// watches its files.
fn join(
    project: &Path,
    checkouts: &mut Checkouts,
    db: &mut RootDatabase,
    tree: &mut tree::Tree,
    files: &mut Vec<WatchedFile>,
) -> anyhow::Result<()> {
    let read_from = SystemTime::now();
    checkouts.add(&[project], db, tree.vfs())?;
    watch_loaded(db, tree, files, read_from);
    Ok(())
}

/// Watches the source files that were loaded since the last were, which were read from
/// `read_from` on.
fn watch_loaded(
    db: &mut RootDatabase,
    tree: &mut tree::Tree,
    files: &mut Vec<WatchedFile>,
    read_from: SystemTime,
) {
    let watched: FxHashSet<vfs::FileId> = files
        .iter()
        .filter_map(|it| match it.file {
            Watched::Source(id) => Some(id),
            Watched::Included(_) => None,
        })
        .collect();
    let new: Vec<WatchedFile> = WatchedFile::sources(tree.vfs(), db, read_from)
        .into_iter()
        .filter(|it| !matches!(it.file, Watched::Source(id) if watched.contains(&id)))
        .collect();
    let sources: Vec<_> = new
        .iter()
        .filter_map(|it| match it.file {
            Watched::Source(id) => Some((id, &*it.path)),
            Watched::Included(_) => None,
        })
        .collect();
    tree.watch(db, &sources);
    files.extend(new);
}

/// Reads the request a client sent on `stream`, and the variables of its process: the
/// result is where to send the outcome. One that is not a request is told so and is `None`.
fn requested(stream: UnixStream) -> Option<Event> {
    let [request, env] = match socket::read_request_and_env(&stream) {
        Ok(lines) => lines,
        Err(error) => {
            // A client that hangs up before the answer is not the daemon's problem.
            let _ = writeln!(&stream, "failed: {error}");
            return None;
        }
    };
    trace::arrived();
    let (name, rest) = request.split_once('\t').unwrap_or((&request, ""));
    eprintln!("request: {name} at_us={}", live::epoch_us());
    if name == "edit"
        && let Some((path, text)) = rest.split_once('\t')
    {
        let path = PathBuf::from(blaze_live::format::unescape(path));
        let text = blaze_live::format::unescape(text);
        let client = Client { stream, env: clients_env(&env) };
        return Some(Event::Edited(Edited { path, text, client, at: Instant::now() }));
    }
    let fields: Vec<&str> = request.split('\t').collect();
    let target = Target::parse(&fields[1..]);
    if let Some(request) = Request::parse(fields[0])
        && (fields.len() == 1 || target.is_some())
    {
        let client = Client { stream, env: clients_env(&env) };
        return Some(Event::Asked((request, target, client)));
    }
    let _ = writeln!(&stream, "failed: unknown request {request:?}");
    None
}

/// The variables of the process a request is for, from their line: [`trace::VAR`] among them
/// is not one of them, but has this worker record a profile from now on.
fn clients_env(line: &str) -> ide_db::base_db::Env {
    let (profile, env): (Vec<_>, Vec<_>) =
        Vec::from(process::decode(line)).into_iter().partition(|(name, _)| name == trace::VAR);
    if let Some((_, dir)) = profile.first() {
        trace::start(Path::new(dir));
    }
    env.into_iter().collect()
}

/// Ends the process when its standard input ends: the daemon that started this worker holds
/// the other end for as long as it lives, however it dies.
fn end_with_input() {
    while input_line().is_some() {}
    // The executables on disk get the last edits before the worker goes, and the store what
    // the last build kept.
    splicer::flush();
    blaze_store::settled();
    // What it started goes with it, as when the daemon kills it: the daemon started it in a
    // group of its own.
    #[cfg(unix)]
    // SAFETY: neither takes a pointer.
    unsafe {
        if libc::getpgrp() == libc::getpid() {
            libc::killpg(0, libc::SIGKILL);
        }
    }
    std::process::exit(0);
}

/// The lines of the standard input, read on a thread of their own from the first use on. The
/// daemon writes what cargo says to a spare worker as cargo says it, and waits while the
/// pipe is full: cargo then waits for the daemon.
static INPUT: std::sync::LazyLock<Mutex<std::sync::mpsc::Receiver<String>>> =
    std::sync::LazyLock::new(|| {
        let (line, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut input = std::io::stdin().lock();
            loop {
                let mut text = Vec::new();
                match std::io::BufRead::read_until(&mut input, b'\n', &mut text) {
                    Ok(len) if len > 0 => {
                        let text = String::from_utf8(text)
                            .unwrap_or_else(|it| String::from_utf8_lossy(it.as_bytes()).into());
                        if line.send(text).is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        });
        Mutex::new(lines)
    });

/// The next line of the standard input, `None` once it has ended.
fn input_line() -> Option<String> {
    INPUT.lock().unwrap_or_else(|e| e.into_inner()).recv().ok()
}

const HELP: &str = "\
blaze's engine: a daemon that keeps Rust workspaces compiled and builds edits from memory.
`blaze <command>` is what to run; this program is what it runs.

usage: blaze <command>

  status            what the daemon has loaded, the store, how the builds went
  stop              ends the daemon; the next build starts one
  cargo <command>   what `blaze <command>` runs: build, run, test, check
  cache <verb>      the disk store: stats, gc, verify, push, pull, keygen
                    (`blaze cache clear` forgets what `#[blaze::cache]` kept)
  closure fetch|materialize [--host <host>] [<root>...]
                    the nixpkgs store paths build scripts run with (docs/closure.md)
  mir <dir> --crate <name> [--level 0|1|2] [--time]
                    a crate's MIR as blaze compiles it, written as rustc's -Zunpretty=mir
  --help, --version

`blaze build|run|test|check` takes cargo's flags. The guide: https://github.com/indexable-inc/blaze/blob/main/docs/cargo.md";

/// The program `blaze-engine`, whose `src/main.rs` is only this.
pub fn main() -> anyhow::Result<()> {
    // Before anything that could start a thread: the sandbox forks.
    if std::env::args_os().nth(1).is_some_and(|it| it == sandbox::COMMAND) {
        sandbox::command(std::env::args_os().skip(2));
    }
    blaze_cargo::engine::set(blaze_cargo::engine::Engine {
        doc: doc::command,
        stand_ins: link::stand_ins,
        pin: versions::pin,
        why_this_one: versions::why_this_one,
        version: pin::OURS,
        version_line: env!("BLAZE_VERSION_LINE"),
    });
    // Before anything that could start a thread: it sets the environment. After the engine
    // is handed over: a library downloaded is the release's, which the engine's version names.
    cargo::toolchain::library::publish();
    trace::began();
    renamed::refuse_old_settings();
    trace::install();
    if let Some(sources) = option_env!("BLAZE_SOURCES") {
        blaze_store::built_from(sources);
    }
    syntax::set_green_trees(std::env::var_os("BLAZE_GREEN_TREES").is_some());
    project_model::answer_toolchain_questions_with(answers::ask);
    // A build script runs blaze as its `RUSTC`, whose load asks what it asks
    // rustc of [`answers`].
    if as_rustc::is_me() {
        as_rustc::command();
    }
    let stands_in_for_rustc = std::env::var_os("BLAZE_WRAPS_RUSTC").is_some();
    if let Some(replayed) = doctest::replay() {
        return replayed;
    }
    // A build script's name, linked to this executable, comes before the commands: cargo runs
    // it with no arguments, which are the help.
    #[cfg(windows)]
    if let Some(script) = cargo::script::stood_in_front_of() {
        return cargo::script::run_as_cargo_does(script, std::env::args_os().skip(1));
    }
    match std::env::args().nth(1).as_deref() {
        _ if stands_in_for_rustc => {}
        None | Some("--help" | "-h" | "help") => {
            end_with_reader();
            println!("{HELP}");
            return Ok(());
        }
        Some("--version" | "-V") => {
            println!("{}", version());
            return Ok(());
        }
        Some("cache") => return remote::command(std::env::args().skip(2)),
        Some("std") => return shipped::command(std::env::args().skip(2)),
        Some("version-fetch") => return versions::command(std::env::args().skip(2)),
        Some("version-resolve") => return versions::resolve_command(std::env::args().skip(2)),
        #[cfg(unix)]
        Some("closure") => {
            end_with_reader();
            return cargo::closure::command(std::env::args().skip(2));
        }
        _ => {}
    }
    // A build script runs with the variables of the cargo that runs it, the wrapper's too.
    if std::env::args().nth(1).as_deref() == Some("build-script") {
        return cargo::script::command(std::env::args_os().skip(2));
    }
    match std::env::args().nth(1).as_deref() {
        Some("rustc-wrapper") => return cargo::load_wrapper::command(std::env::args_os().skip(2)),
        // `cargo-answer <program> <args>`: what blaze answers for the cargo `<program>` run
        // with `<args>`, of the toolchain or the configuration, for `harness/resolve.py
        // toolchain`: printed as cargo prints it, ending as cargo ends. Nothing runs it.
        Some("cargo-answer") => {
            let program = std::env::args_os().nth(2).context("cargo-answer needs a cargo")?;
            let mut asked = std::process::Command::new(program);
            // `--rustflags <triple>`: the flags the plan reads (`plan.rs`, `Flags::of`), as JSON.
            if std::env::args().nth(3).as_deref() == Some("--rustflags") {
                let triple = std::env::args().nth(4).context("--rustflags needs a target")?;
                let flags = answers::cargo_rustflags(&asked, &triple)?;
                println!("{}", serde_json::to_string(&flags)?);
                return Ok(());
            }
            asked.args(std::env::args_os().skip(3));
            let output = answers::cargo_answer(&asked)
                .context("blaze does not answer this question of cargo")??;
            use std::io::Write as _;
            std::io::stdout().write_all(&output.stdout)?;
            std::io::stderr().write_all(&output.stderr)?;
            std::process::exit(output.status.code().unwrap_or(101));
        }
        // `proc-macro-wasm <module.wasm> --macro <name> --input <file> ...`: one macro of a
        // module compiled to WebAssembly expanded and timed (`blaze-macro-wasm/src/bench.rs`).
        Some("proc-macro-wasm") => {
            return blaze_macro_wasm::bench::command(std::env::args().skip(2))
                .map_err(anyhow::Error::msg);
        }
        Some(cargo::own_macro::COMMAND) => {
            return cargo::own_macro::command(std::env::args_os().skip(2));
        }
        Some("cargo") => return cargo::command(std::env::args().skip(2)),
        Some("status" | "stop") => {
            return cargo::command(std::env::args().skip(1));
        }
        Some("daemon") => return cargo::daemon::serve(),
        _ => {}
    }
    if std::env::args().nth(1).as_deref() == Some("--spare") {
        let dir = std::env::args().nth(2).context("--spare needs a directory")?;
        return spare(Path::new(&dir));
    }
    if std::env::var_os(cargo::daemon::ENDS_WITH_INPUT).is_some() {
        std::thread::spawn(end_with_input);
    }
    if std::env::var_os("BLAZE_WRAPS_RUSTC").is_some() {
        return stub::command(std::env::args_os().skip(1));
    }
    if std::env::args().nth(1).as_deref() == Some("--check") {
        return check::serve(Args::parse(std::env::args().skip(2))?);
    }
    if std::env::args().nth(1).as_deref() == Some("deps") {
        return deps::command(std::env::args().skip(2));
    }
    if std::env::args().nth(1).as_deref() == Some("borrowck") {
        return borrowck::command(std::env::args().skip(2));
    }
    if std::env::args().nth(1).as_deref() == Some("consteval") {
        return consteval::command(std::env::args().skip(2));
    }
    if std::env::args().nth(1).as_deref() == Some("compile-time-deps") {
        return compile_time_deps(std::env::args().skip(2));
    }
    if std::env::args().nth(1).as_deref() == Some("weaken") {
        return weaken::command(std::env::args().skip(2));
    }
    if std::env::args().nth(1).as_deref() == Some("mir") {
        return mirdump::command(std::env::args().skip(2));
    }
    if let Some((request, socket)) =
        std::env::args().nth(1).and_then(|it| Request::parse(&it)).zip(std::env::args().nth(2))
    {
        return ask(request, Path::new(&socket), Target::from_flags(std::env::args().skip(3))?);
    }
    let args = Args::parse(std::env::args().skip(1))?;
    // A worker the daemon started answers its builds' profiles for the client to put
    // together; one started directly puts its first build's together itself.
    if args.socket.is_none()
        && std::env::var_os(cargo::daemon::ENDS_WITH_INPUT).is_none()
        && trace::on()
    {
        FIRST_BUILD_PROFILED.store(true, std::sync::atomic::Ordering::Relaxed);
        if trace::followed_const().is_some() {
            hir_ty::consteval::trace_consts_with(followed_const);
        }
    }
    let result = run(args, None);
    first_build_profiled();
    result
}

/// Whether `def` is the constant [`trace::CONST_VAR`] names, `crate::module::NAME`: its
/// crate, every module and its name, so one of the same name elsewhere, in an impl or in a
/// block is not. What to note for it, a request or a run of its query.
fn followed_const(
    db: &dyn hir_ty::db::HirDatabase,
    def: hir_def::ConstId,
    executing: bool,
) -> Option<fn(Instant)> {
    use hir_def::{HasModule as _, Lookup as _, signatures::ConstSignature};
    let mut path = trace::followed_const()?.rsplit("::");
    if ConstSignature::of(db, def).name.as_ref()?.as_str() != path.next()? {
        return None;
    }
    let ItemContainerId::ModuleId(mut module) = def.lookup(db).container else { return None };
    while let Some(parent) = module.containing_module(db) {
        // A block's module has no name, so no path names a constant in a block.
        if module.name(db)?.as_str() != path.next()? {
            return None;
        }
        module = parent;
    }
    let krate = def.krate(db).extra_data(db).display_name.as_ref()?.crate_name();
    if krate.as_str() != path.next()? || path.next().is_some() {
        return None;
    }
    Some(if executing { trace::const_execution } else { trace::const_request })
}

/// Whether this is a worker started directly with a profile whose first build is not yet
/// put together: no client does it for it.
static FIRST_BUILD_PROFILED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Puts the profile of a directly started worker's first build together, once: when it
/// starts watching or its one build ends.
fn first_build_profiled() {
    if FIRST_BUILD_PROFILED.swap(false, std::sync::atomic::Ordering::Relaxed) {
        trace::finish();
    }
}

/// Compiles the crates `args` names on `threads` threads until `done`, giving up on what is
/// being compiled then: the symbols a build waits for are not needed for it. With `follow`
/// the generic instances their functions reach are compiled too.
fn lower_until(
    db: &mut RootDatabase,
    args: &Args,
    listed: &Listed,
    threads: usize,
    follow: bool,
    done: &(dyn Fn() -> bool + Sync),
) -> anyhow::Result<()> {
    use ide_db::base_db::salsa::Database as _;
    if done() {
        return Ok(());
    }
    let start = Instant::now();
    let mut functions = Vec::new();
    let mut crates = Vec::new();
    for name in args.crates() {
        for krate in crates_named(db, name, args)? {
            crates.push(krate.into());
            functions.extend(listed.of(db, krate).functions.iter().copied().filter(|&func| {
                let generic = |def: hir::GenericDef| !def.type_or_const_params(db).is_empty();
                let container = match func.lookup(db).container {
                    ItemContainerId::ImplId(it) => !generic(hir::Impl::from(it).into()),
                    ItemContainerId::TraitId(_) => false,
                    _ => true,
                };
                container && !generic(hir::Function::from(func).into())
            }));
        }
    }
    let count = functions.len();
    let unnamed = match follow {
        true => hir_ty_codegen::Unnamed::new(functions),
        false => hir_ty_codegen::Unnamed::alone(functions),
    };
    std::thread::scope(|scope| {
        let compiling: Vec<_> = (0..threads)
            .map(|_| {
                let (db, unnamed) = (db.clone(), &unnamed);
                scope.spawn(move || {
                    let compile = || hir_ty_codegen::compile_unnamed(&db, unnamed, done);
                    let compile = std::panic::AssertUnwindSafe(|| hir::attach_db(&db, compile));
                    let _ = std::panic::catch_unwind(compile);
                })
            })
            .collect();
        // What is being compiled once the build stops waiting is given up on, not finished:
        // one function can take minutes, and readiness would wait for it. A thread's own
        // cancellation is not enough: salsa ignores it inside a query that recovers from
        // cycles, as inference and MIR building do, and so for most of what takes long.
        while compiling.iter().any(|thread| !thread.is_finished()) {
            if done() {
                db.trigger_cancellation();
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    let db = &*db;
    // What the build lists of the crates to name what it keeps, while there is still time.
    let lower = |bodies: &[hir_def::DefWithBodyId]| {
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                let (db, next) = (db.clone(), &next);
                scope.spawn(move || {
                    hir::attach_db(&db, || {
                        while let Some(&body) = bodies.get(next.fetch_add(1, Ordering::Relaxed))
                            && !done()
                        {
                            hir_def::expr_store::Body::of(&db, body);
                        }
                    })
                });
            }
        });
    };
    if !done() {
        hir_ty_codegen::lower_declared(db, &crates, &lower);
    }
    let compiled = unnamed.compiled();
    eprintln!("compiled ahead: {compiled} from {count} functions in {:.1?}", start.elapsed());
    Ok(())
}

fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, |it| it.get())
}

/// The threads that work beside a build of cargo's: a quarter of the cores, the rest being
/// cargo's.
fn beside_cargo() -> usize {
    (cores() / 4).max(1)
}

/// How cargo is asked about the workspace of `args`.
fn cargo_config(args: &Args) -> CargoConfig {
    // The library is read as the toolchain that built the program has it: one the directory
    // would pick now may be another, whose library has names the built one does not.
    let built_with = args.symbol_files.iter().find_map(|file| {
        let name = file.file_name()?.to_str()?;
        let (toolchain, _) = file.to_str()?.split_once("/lib/rustlib/")?;
        (name.starts_with("libstd-") && name.ends_with(".rlib")).then_some(toolchain)
    });
    let sysroot = match built_with.map(AbsPathBuf::try_from) {
        Some(Ok(toolchain)) => RustLibSource::Path(toolchain),
        _ => RustLibSource::Discover,
    };
    CargoConfig {
        // The variables of whoever asks for a build: those of crates are read as rustc does.
        extra_env_for_commands_only: true,
        sysroot: Some(sysroot),
        target: args.selection.target.clone(),
        set_test: args.cfg_test,
        // Tests, benches and examples are what dev-dependencies are for: cargo runs the
        // build scripts and the proc macros of those only for a build that has them.
        all_targets: args.cfg_test || args.selection.dev_deps,
        features: match args.selection.all_features {
            true => CargoFeatures::All,
            false => CargoFeatures::Selected {
                features: args.selection.features.clone(),
                no_default_features: args.selection.no_default_features,
            },
        },
        // The build scripts say what the build asked for says, not what `cargo check` of the
        // workspace does.
        extra_args: selected(&args),
        only_manifest_package: true,
        // A whole build of tests says where it puts its programs; one over rustc's, where
        // cargo put them.
        executables: match &args.executables {
            Some(dir) => Some(dir.clone()),
            None => executables_of(&args.symbol_files),
        },
        scratch: args.scratch.clone(),
        own_metadata: cargo::metadata::own(&args.selection),
        // The build scripts and proc macros of a load in blaze's own target directory are
        // built and run by blaze (`cargo/compile_time.rs`), not `cargo check
        // --compile-time-deps`; a build cargo already ran says what its scripts did.
        own_build_scripts: (args.own_target && !cargo::own_macro::turned_off())
            .then(|| cargo::compile_time::own(&args.selection)),
        target_dir_config: match args.own_target {
            true => TargetDirectoryConfig::UseSubdirectory,
            false => TargetDirectoryConfig::None,
        },
        // The build scripts are run with cargo told it is a nightly, for
        // `--compile-time-deps`. Cargo then also takes an `[unstable]` table of the user's
        // configuration at its word, and `checksum-freshness` hands rustc a `-Z` flag the
        // stable rustc refuses, so that no build script runs. The wrapper is told when the
        // target directory is blaze's own, which cargo's builds do not read: the proc macros
        // built there can be blaze's (`cargo/own_macro.rs`), for the server the load found.
        extra_env: [("CARGO_UNSTABLE_CHECKSUM_FRESHNESS".to_owned(), Some("false".to_owned()))]
            .into_iter()
            .chain(
                args.own_target
                    .then(|| {
                        // Turned off, the wrapper is told so; with none found, it is told none.
                        let server = cargo::own_macro::server()
                            .map(|it| it.display().to_string())
                            .or_else(|| cargo::own_macro::turned_off().then(|| "0".to_owned()));
                        [
                            (cargo::own_macro::OWN_TARGET.to_owned(), Some("1".to_owned())),
                            (cargo::own_macro::SERVER.to_owned(), server),
                        ]
                    })
                    .into_iter()
                    .flatten(),
            )
            .collect(),
        ..Default::default()
    }
}

/// How a workspace is loaded, whichever it is.
/// `blaze-engine compile-time-deps PROJECT [build flags]`: what a build's load in blaze's own target
/// directory reads of the build scripts and proc macros of PROJECT, as blaze built and ran them
/// (`cargo/compile_time.rs`): a JSON object of each package that has any, by its id, with its
/// script's `cfgs` and `envs`, its `out_dir`, and `proc_macro`, its library. The flags are the
/// build's (`--release`, `--features`, `--cfg-test`, ...). `harness/resolve.py ctdeps` holds it
/// to rust-analyzer's `cargo check --compile-time-deps`.
fn compile_time_deps(args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let fixed = ["--own-target", "--crate", "-", "--out", "/dev/null"].map(str::to_owned);
    let args = Args::parse(args.chain(fixed))?;
    let config = cargo_config(&args);
    let (workspace, _) =
        workspace_at(&args.project, &config, &load_config(), &args.selection, None, None)?;
    let ProjectWorkspaceKind::Cargo { cargo, build_scripts, .. } = &workspace.kind else {
        bail!("{} is not a cargo workspace", args.project.display());
    };
    let mut packages = serde_json::Map::new();
    for package in cargo.packages() {
        let Some(read) = build_scripts.read_of(package) else { continue };
        if read == project_model::OwnBuildOutput::default() {
            continue;
        }
        let path = |it: &Option<AbsPathBuf>| it.as_ref().map(ToString::to_string);
        packages.insert(
            cargo[package].id.repr.clone(),
            serde_json::json!({
                "cfgs": read.cfgs,
                "envs": read.envs,
                "out_dir": path(&read.out_dir),
                "proc_macro": path(&read.proc_macro_dylib),
            }),
        );
    }
    println!("{}", serde_json::to_string_pretty(&packages)?);
    Ok(())
}

fn load_config() -> LoadCargoConfig {
    LoadCargoConfig {
        load_out_dirs_from_check: true,
        with_proc_macro_server: ProcMacroServerChoice::Sysroot,
        prefill_caches: false,
        num_worker_threads: 1,
        proc_macro_processes: 1,
    }
}

/// What a process loaded before it was told its build: a package of nothing, and with it the
/// library of the toolchain, its names resolved.
struct Held {
    db: RootDatabase,
    vfs: Vfs,
    checkouts: Checkouts,
}

impl Held {
    /// Loads an empty package in `dir` and resolves what it depends on, which is the library.
    fn load(dir: &Path) -> anyhow::Result<Held> {
        let start = Instant::now();
        std::fs::create_dir_all(dir.join("src"))?;
        let manifest =
            "[package]\nname = \"blaze-spare\"\nversion = \"0.0.0\"\nedition = \"2024\"\n";
        std::fs::write(dir.join("Cargo.toml"), manifest)?;
        std::fs::write(dir.join("src/lib.rs"), "")?;
        // The crate and the object are ones nothing asks for.
        let asked = ["--crate", "blaze_spare", "--out", "none"].map(str::to_owned);
        let asked = std::iter::once(dir.display().to_string()).chain(asked);
        let args = Args::parse(asked)?;
        held_wide();
        // An empty package has no build script, and asking cargo for one waits for the lock
        // of the target directory, which a build that started beside this holds until it ends.
        let scriptless = LoadCargoConfig { load_out_dirs_from_check: false, ..load_config() };
        let (db, vfs, mut checkouts) =
            load(&[dir], &cargo_config(&args), scriptless, args.selection, None, None)?;
        checkouts.load_config = load_config();
        let all: Vec<ide_db::base_db::Crate> =
            Crate::all(&db).into_iter().map(Into::into).collect();
        // The spare starts with the daemon, which the first build starts as cargo does.
        resolve_all(&db, &all, beside_cargo())?;
        eprintln!("spare: {} crates in {:.1?}", all.len(), start.elapsed());
        Ok(Held { db, vfs, checkouts })
    }

    /// This with `projects` loaded next to the empty package, as a build of `selection` has
    /// them. `None` when their library is another toolchain's: nothing held is of use then,
    /// and the proc macros would be expanded by the wrong server.
    fn with(
        mut self,
        projects: &[&Path],
        cargo_config: &CargoConfig,
        selection: &plan::Selection,
    ) -> Option<Held> {
        let start = Instant::now();
        // The toolchain of the build is asked for before its workspace is loaded for nothing.
        let held = self.checkouts.loaded.workspaces().first()?.sysroot.root()?.to_owned();
        let asked = match &cargo_config.sysroot {
            Some(RustLibSource::Path(toolchain)) => Some(toolchain.clone()),
            _ => {
                let dir = AbsPathBuf::assert_utf8(std::env::current_dir().ok()?.join(projects[0]));
                let sysroot = project_model::Sysroot::discover(&dir, &cargo_config.extra_env);
                sysroot.root().map(ToOwned::to_owned)
            }
        };
        if asked.as_ref() != Some(&held) {
            eprintln!(
                "spare: not used: it holds the library of {held:?}, the build is of {asked:?}"
            );
            return None;
        }
        self.checkouts.cargo_config = cargo_config.clone();
        self.checkouts.selection = selection.clone();
        if let Err(why) = self.checkouts.add(projects, &mut self.db, &mut self.vfs) {
            eprintln!("spare: not used: {why:#}");
            return None;
        }
        let workspaces = self.checkouts.loaded.workspaces();
        let library = |it: &ProjectWorkspace| it.sysroot.root().map(ToOwned::to_owned);
        let (held, asked) = (library(workspaces.first()?), library(workspaces.last()?));
        if held != asked {
            eprintln!(
                "spare: not used: it holds the library of {held:?}, the build is of {asked:?}"
            );
            return None;
        }
        eprintln!("spare: workspace added in {:.1?}", start.elapsed());
        Some(self)
    }
}

/// A line a worker told a build ahead of cargo's reads while cargo runs.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub(crate) enum Line {
    /// One cargo printed of a build script it ran or a proc macro it built.
    Said { said: String },
    /// What cargo built, which ends its build.
    Whole(Told),
}

/// What cargo says of the build scripts and the proc macros of a build as it runs.
#[derive(Default)]
struct Said {
    lines: Mutex<Lines>,
    more: std::sync::Condvar,
}

#[derive(Default)]
struct Lines {
    said: Vec<String>,
    /// Cargo's build is over: nothing more is said.
    ended: bool,
}

impl Said {
    /// Reads the lines of the standard input on a thread: what cargo said, then on the
    /// second what it built. The process ends when its input does without that: there is
    /// no build.
    fn read() -> (Arc<Said>, std::sync::mpsc::Receiver<anyhow::Result<Args>>) {
        let said = Arc::new(Said::default());
        let (built, whole) = std::sync::mpsc::channel();
        let read = said.clone();
        std::thread::spawn(move || {
            let tell = |told: &dyn Fn(&mut Lines)| {
                told(&mut read.lines.lock().unwrap_or_else(|e| e.into_inner()));
                read.more.notify_all();
            };
            loop {
                let line = input_line().and_then(|text| serde_json::from_str::<Line>(&text).ok());
                match line {
                    // A library is read for its symbols, and is nothing the workspace is
                    // loaded from.
                    Some(Line::Said { said: line }) => match libraries_in(&line) {
                        Some(files) => files
                            .into_iter()
                            .map(|it| cargo::Record::kept_copy(&it).unwrap_or(it))
                            .for_each(symbols::read_ahead),
                        None => tell(&|it| it.said.push(line.clone())),
                    },
                    Some(Line::Whole(told)) => {
                        tell(&|it| it.ended = true);
                        _ = built.send(Args::parse(told.args.into_iter()));
                        return end_with_input();
                    }
                    None => break,
                }
            }
            std::process::exit(0);
        });
        (said, whole)
    }

    /// The lines so far, and whether they are all there will be.
    fn so_far(&self) -> (Vec<String>, bool) {
        let lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        (lines.said.clone(), lines.ended)
    }

    /// Whether there is more than the first `seen` lines to take in.
    fn more_than(&self, seen: usize) -> bool {
        let lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        lines.said.len() > seen || lines.ended
    }

    /// Waits until [`Self::more_than`] `seen`.
    fn wait_more_than(&self, seen: usize) {
        let lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        let more = |it: &mut Lines| it.said.len() <= seen && !it.ended;
        drop(self.more.wait_while(lines, more));
    }
}

/// A script cargo runs or a proc macro it builds for a package, by the id of the package.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Awaited {
    Script(String),
    ProcMacro(String),
}

impl Awaited {
    /// The one a line of cargo's says is there, when it says that.
    fn arrived(line: &str) -> Option<Awaited> {
        #[derive(serde::Deserialize)]
        struct Message {
            reason: String,
            package_id: String,
            target: Option<Target>,
        }
        #[derive(serde::Deserialize)]
        struct Target {
            kind: Vec<String>,
        }
        let message: Message = serde_json::from_str(line).ok()?;
        let proc_macro =
            message.target.is_some_and(|it| it.kind.iter().any(|it| it == "proc-macro"));
        match &*message.reason {
            "build-script-executed" => Some(Awaited::Script(message.package_id)),
            "compiler-artifact" if proc_macro => Some(Awaited::ProcMacro(message.package_id)),
            _ => None,
        }
    }
}

/// The libraries cargo says it built in `line`, when it is of a crate that is not a proc
/// macro.
fn libraries_in(line: &str) -> Option<Vec<PathBuf>> {
    #[derive(serde::Deserialize)]
    struct Message {
        reason: String,
        target: Target,
        filenames: Vec<PathBuf>,
    }
    #[derive(serde::Deserialize)]
    struct Target {
        kind: Vec<String>,
    }
    let message: Message = serde_json::from_str(line).ok()?;
    let proc_macro = message.target.kind.iter().any(|it| it == "proc-macro");
    (message.reason == "compiler-artifact" && !proc_macro).then(|| {
        let rlib = |it: &PathBuf| it.extension().is_some_and(|it| it == "rlib");
        message.filenames.into_iter().filter(rlib).collect()
    })
}

/// A checkout that is loaded while cargo builds it. What its build scripts say and where
/// its proc macros are is what cargo says as it gets to each, and a crate is read once
/// cargo has said that of every package it is built from.
struct Beside {
    said: Arc<Said>,
    /// Which of the loaded workspaces it is.
    at: usize,
    /// What cargo has to say for the whole of it to be read.
    awaited: FxHashSet<Awaited>,
    /// The packages cargo builds, by id: a member of the workspace that the build leaves
    /// out is not read for it, nor is what only that member depends on.
    built: FxHashSet<String>,
    /// How many lines of cargo's the loaded workspace has taken in.
    applied: usize,
}

impl Beside {
    /// Takes note of what a build of the packages `built` of `cargo` has to say.
    fn awaits(
        &mut self,
        cargo: &CargoWorkspace,
        built: impl Iterator<Item = project_model::Package>,
    ) {
        for package in built {
            let id = &cargo[package].id.repr;
            self.built.insert(id.clone());
            for &target in &cargo[package].targets {
                match cargo[target].kind {
                    TargetKind::BuildScript => _ = self.awaited.insert(Awaited::Script(id.clone())),
                    TargetKind::Lib { is_proc_macro: true } => {
                        _ = self.awaited.insert(Awaited::ProcMacro(id.clone()))
                    }
                    _ => {}
                }
            }
        }
    }

    /// The crates of the workspace's own packages that cargo builds, by the file each starts
    /// from.
    fn built_roots(&self, cargo: &CargoWorkspace) -> FxHashSet<PathBuf> {
        let built = cargo
            .packages()
            .filter(|&it| cargo[it].is_member && self.built.contains(&cargo[it].id.repr));
        built
            .flat_map(|it| {
                cargo[it].targets.iter().map(|&it| PathBuf::from(cargo[it].root.clone()))
            })
            .collect()
    }

    /// The crates of the workspace's own packages cargo builds that `lines` of cargo's say
    /// everything of, by the file each starts from, and whether those are all its crates.
    fn ready(&self, cargo: &CargoWorkspace, lines: &[String]) -> (FxHashSet<PathBuf>, bool) {
        let arrived: FxHashSet<Awaited> =
            lines.iter().filter_map(|it| Awaited::arrived(it)).collect();
        let waits = |package: project_model::Package| {
            let id = &cargo[package].id.repr;
            [Awaited::Script(id.clone()), Awaited::ProcMacro(id.clone())]
                .iter()
                .any(|it| self.awaited.contains(it) && !arrived.contains(it))
        };
        // A package is read with every package it depends on.
        let mut waiting: FxHashMap<project_model::Package, bool> = FxHashMap::default();
        fn any_waits(
            cargo: &CargoWorkspace,
            package: project_model::Package,
            waits: &dyn Fn(project_model::Package) -> bool,
            waiting: &mut FxHashMap<project_model::Package, bool>,
        ) -> bool {
            if let Some(&known) = waiting.get(&package) {
                return known;
            }
            // One that depends on itself through another waits for no more than the other.
            waiting.insert(package, false);
            let any = waits(package)
                || cargo[package]
                    .dependencies
                    .iter()
                    .any(|it| any_waits(cargo, it.pkg, waits, waiting));
            waiting.insert(package, any);
            any
        }
        let mut ready = FxHashSet::default();
        let mut all = true;
        let built = |it: &project_model::Package| self.built.contains(&cargo[*it].id.repr);
        for package in cargo.packages().filter(|&it| cargo[it].is_member).filter(built) {
            if any_waits(cargo, package, &waits, &mut waiting) {
                all = false;
                continue;
            }
            let roots = cargo[package].targets.iter().map(|&it| cargo[it].root.clone());
            ready.extend(roots.map(PathBuf::from));
        }
        (ready, all)
    }
}

/// Compiles the functions of the crates of the checkout loaded beside cargo's build that
/// cargo has said everything of, and takes in what cargo says next, until its build is over.
/// The crates of the packages cargo builds are what it comes back with, by the file each
/// starts from. The generic instances those functions reach are left to the edits: on a
/// quarter of the cores they made cargo's build of nushell a twentieth slower.
fn follow_cargo(
    db: &mut RootDatabase,
    vfs: &mut Vfs,
    checkouts: &mut Checkouts,
    args: &Args,
    listed: &Listed,
) -> anyhow::Result<Option<FxHashSet<PathBuf>>> {
    let Some(mut beside) = checkouts.beside.take() else { return Ok(None) };
    let mut ready = FxHashSet::default();
    let mut all_applied = false;
    loop {
        let (lines, ended) = beside.said.so_far();
        let workspace = &checkouts.loaded.workspaces()[beside.at];
        let ProjectWorkspaceKind::Cargo { cargo, .. } = &workspace.kind else { return Ok(None) };
        let (now, all) = beside.ready(cargo, &lines);
        let all = all || ended;
        if lines.len() > beside.applied && (now != ready || all && !all_applied) {
            let scripts =
                WorkspaceBuildScripts::of_messages(cargo, lines.iter().map(String::as_str));
            let mut workspace = workspace.clone();
            workspace.set_build_scripts(scripts);
            checkouts.loaded.replace(beside.at, workspace);
            let Checkouts { loaded, cargo_config, load_config, .. } = checkouts;
            loaded.add(Vec::new(), &cargo_config.extra_env, load_config, vfs, db)?;
            prepare_macro_sessions(db);
            beside.applied = lines.len();
            all_applied = all;
        }
        if ended {
            let workspace = &checkouts.loaded.workspaces()[beside.at];
            let ProjectWorkspaceKind::Cargo { cargo, .. } = &workspace.kind else {
                return Ok(None);
            };
            return Ok(Some(beside.built_roots(cargo)));
        }
        // With all of it said, cargo still compiles and links the last crates: the cores
        // are cargo's until its build is over.
        ready = now;
        // Whether cargo has said what makes another crate ready, or that its build is over,
        // asked once a line.
        let seen = Mutex::new((lines.len(), false));
        let more = || {
            let mut seen = seen.lock().unwrap_or_else(|e| e.into_inner());
            if !seen.1 && beside.said.more_than(seen.0) {
                let (lines, ended) = beside.said.so_far();
                let workspace = &checkouts.loaded.workspaces()[beside.at];
                let ProjectWorkspaceKind::Cargo { cargo, .. } = &workspace.kind else {
                    return true;
                };
                *seen = (lines.len(), ended || beside.ready(cargo, &lines).0 != ready);
            }
            seen.1
        };
        let Parts { root, base, members, debug, lints_capped, link_args } =
            checkouts.parts(&args.project)?;
        let mut names = args.crates().filter(|it| ready.contains(&root.join(it.as_str()))).cloned();
        if let Some(krate) = names.next() {
            let of_ready = Args {
                krate,
                batch: names.collect(),
                widened: Vec::new(),
                checkout: root,
                base,
                members,
                debug,
                lints_capped,
                link_args,
                ..args.clone()
            };
            resolve_crates(db, &of_ready, beside_cargo())?;
            lower_until(db, &of_ready, listed, beside_cargo(), false, &more)?;
        }
        while !more() {
            let seen = seen.lock().unwrap_or_else(|e| e.into_inner()).0;
            beside.said.wait_more_than(seen);
        }
    }
}

/// Loads the library of the toolchain, waits to be told a build, and runs as the worker of
/// it. The process ends when its input does without a build.
fn spare(dir: &Path) -> anyhow::Result<()> {
    // A build may start while the library loads, and its lines are read as they come.
    std::sync::LazyLock::force(&INPUT);
    let held = Held::load(dir).inspect_err(|why| eprintln!("spare: nothing held: {why:#}")).ok();
    let Some(line) = input_line() else { return Ok(()) };
    let told: Told = serde_json::from_str(&line).context("what the spare worker is told")?;
    for (name, value) in &told.env {
        // SAFETY: the threads that loaded the library have ended, and the ones that remain
        // wait for files to change or for a proc macro to expand: none reads a variable.
        unsafe { std::env::set_var(name, value) };
    }
    // The spare was started with the daemon's variables: one the build leaves unset is unset.
    for name in cargo::cli::not_the_builds(&told.env) {
        // SAFETY: as above.
        unsafe { std::env::remove_var(name) };
    }
    trace::from_env();
    std::env::set_current_dir(&told.cwd)?;
    if !told.ahead {
        std::thread::spawn(end_with_input);
    }
    run_holding(Args::parse(told.args.into_iter())?, None, held, told.ahead)
}

/// What is said once for the process about how every workspace is read.
fn process_wide() {
    held_wide();
    // Which proc-macro server expands is read from the build's variables, which a spare is
    // told after it loaded the library ([`held_wide`]), and is kept from then on.
    let server = cargo::own_macro::server();
    if let Some(Ok(server)) = server.and_then(Path::to_str).map(AbsPathBuf::try_from) {
        load_cargo::serve_listed_with(server);
    }
    // A proc macro rustc compiled, as cargo's builds do, is expanded with blaze's module or
    // library of it.
    if server.is_some() || cargo::own_macro::wasm() {
        load_cargo::own_libraries_with(Box::new(|sysroot, workspace, libraries| {
            let libraries: Vec<_> = libraries
                .iter()
                .map(|it| cargo::own_macro::Compiled {
                    name: it.name.clone(),
                    library: it.library.clone().into(),
                    description: it.description.clone(),
                })
                .collect();
            let sysroot = sysroot.map(AsRef::<Path>::as_ref);
            let made = cargo::own_macro::libraries_of(sysroot, workspace.as_ref(), &libraries);
            let path = |it: PathBuf| match it.to_str().map(AbsPathBuf::try_from) {
                Some(Ok(it)) => Ok(it),
                _ => Err(format!("blaze made {} where a path is not text", it.display())),
            };
            made.into_iter().map(|it| it?.map(path).transpose()).collect()
        }));
    }
}

/// What of [`process_wide`] a spare says before it is told its build: nothing that depends
/// on the build's variables.
fn held_wide() {
    hir_def::nameres::EXPAND_BUILTIN_DERIVES.store(true, std::sync::atomic::Ordering::Relaxed);
    project_model::SYSROOT_AS_BUILT.store(true, std::sync::atomic::Ordering::Relaxed);
    blaze_expand::install();
    load_cargo::wasm_macros_with(Box::new(blaze_macro_wasm::load));
    // Every proc-macro server is started in the sandbox.
    macro_sandbox::install();
}

/// Loads the workspace `args` names and builds what they ask for, then serves it when they
/// ask for that. With `doc` the build is of the libraries whose examples are then run.
fn run(args: Args, doc: Option<&doctest::Libraries>) -> anyhow::Result<()> {
    run_holding(args, doc, None, false)
}

/// The remote `BLAZE_REMOTE` names once the pull a load starts beside itself is over.
fn pulled(
    pulling: std::thread::JoinHandle<anyhow::Result<Option<remote::Shared>>>,
) -> anyhow::Result<Option<remote::Shared>> {
    pulling.join().map_err(|_| anyhow::anyhow!("fetching the store panicked"))?
}

/// [`run`], with what a process started before its build was known has loaded already.
fn run_holding(
    mut args: Args,
    doc: Option<&doctest::Libraries>,
    held: Option<Held>,
    ahead: bool,
) -> anyhow::Result<()> {
    // As cargo does: the flag, then the environment, then the host.
    if args.selection.target.is_none() {
        args.selection.target =
            std::env::var("CARGO_BUILD_TARGET").ok().filter(|it| !it.is_empty());
    }
    let emits = match &args.selection.target {
        Some(triple) => hir_ty_codegen::Target::select(triple),
        None => hir_ty_codegen::Target::selected(),
    };
    let emits = emits.map_err(anyhow::Error::msg)?;
    if args.selection.target.is_some() {
        eprintln!("target: {}", emits.triple());
    }
    let backend = Backend::from_env().map_err(anyhow::Error::msg)?;
    if backend != Backend::Cranelift {
        eprintln!("backend: {backend}");
    }
    process_wide();
    if args.socket.is_some() || args.watch {
        hir_ty_codegen::builds_edits();
    }
    if args.exe.is_some() {
        hir_ty_codegen::links_own();
    }
    // Nothing asks for a symbol before the first build, so they load next to the workspace.
    let (symbol_files, root) = (args.symbol_files.clone(), args.root()?);
    let (said, whole) = match ahead {
        true => {
            let (said, whole) = Said::read();
            (Some(said), Some(whole))
        }
        false => (None, None),
    };
    let table = std::thread::spawn(move || {
        // The files are cargo's to write first, when the build was told ahead of them.
        let whole = whole.map(|it| it.recv()?).transpose()?;
        let symbol_files = whole.as_ref().map_or(&symbol_files, |it| &it.symbol_files);
        let start = Instant::now();
        let table = SymbolTable::load(symbol_files, &root)?;
        eprintln!("symbols: {} in {:.1?}", table.len(), start.elapsed());
        Ok::<_, anyhow::Error>((table, whole))
    });

    // The store keeps the first toolchain it is told of, and the build scripts the load runs
    // in this process tell it a name of their own: a build with nothing of rustc's tells it
    // the toolchain whose library it loads before then, or its store is not that toolchain's
    // and the standard library a release ships is never found (#775).
    let cargo_config = cargo_config(&args);
    if !ahead
        && args.symbol_files.is_empty()
        && let Some(toolchain) = toolchain_ahead()
    {
        blaze_store::compiles_against(&toolchain);
    }
    // A build with nothing of rustc's starts from the standard library, which a release ships:
    // fetched while the workspace loads, it is in before the names are resolved, and the
    // standard library of the build scripts the load builds is made of it.
    let nothing_of_rustc = !ahead && args.symbol_files.is_empty();
    if nothing_of_rustc && args.prebuilt.is_none() && !patches_its_library(&args) {
        shipped::fetch_ahead(emits.triple(), &args.project)?;
    }

    // What other machines compiled is fetched beside the build, which never waits for it: the
    // store reads what arrived once it is in, and the build finds it from then on.
    let shared = std::thread::spawn(|| {
        let shared = remote::Shared::from_env()?;
        if let Some(shared) = &shared {
            shared.pull()?;
            // Opened before the toolchain is known, the store of this process is none for good;
            // opened after, it reads what the pull brought anyway.
            if blaze_store::toolchain().is_some()
                && let Some(store) = blaze_store::Store::global()
            {
                store.refresh();
            }
        }
        anyhow::Ok(shared)
    });

    if let Some(socket) = &args.socket {
        events::said_beside(socket);
        fns::said();
    }
    events::at(events::Step::LoadWorkspace, None);
    events::cache_ahead();
    events::flights();
    let start = Instant::now();
    let span = tracing::info_span!(target: "blaze", "load").entered();
    let selection = args.selection.clone();
    blaze_store::identify_ahead();
    let projects: Vec<&Path> =
        std::iter::once(&args.project).chain(&args.also).map(PathBuf::as_path).collect();
    let beside = |said: &Arc<Said>| Beside {
        said: said.clone(),
        at: usize::MAX,
        awaited: FxHashSet::default(),
        built: FxHashSet::default(),
        applied: 0,
    };
    // A build that is over said what its scripts did and where its proc macros are once,
    // and loading does not build them again to find out.
    let scripts_said = match (&said, &args.scripts_from) {
        (None, Some(file)) => match std::fs::read_to_string(file) {
            Ok(text) => Some(text.lines().map(str::to_owned).collect::<Vec<_>>()),
            Err(why) => {
                eprintln!("build scripts: {} not read, they run: {why}", file.display());
                None
            }
        },
        _ => None,
    };
    let read_from = SystemTime::now();
    let held = held.and_then(|mut held| {
        held.checkouts.beside = said.as_ref().map(beside);
        held.checkouts.scripts_said = scripts_said.clone();
        held.with(&projects, &cargo_config, &selection)
    });
    let (mut db, mut vfs, mut checkouts) = match held {
        Some(Held { db, vfs, checkouts }) => (db, vfs, checkouts),
        None => {
            let beside = said.as_ref().map(beside);
            load(&projects, &cargo_config, load_config(), selection, beside, scripts_said)?
        }
    };
    drop(span);
    let span = tracing::info_span!(target: "blaze", "toolchain").entered();
    let listed = Arc::new(Listed::default());
    ide_db::base_db::set_process_env(&mut db, process::own());
    // What is compiled ahead is stored too, so the store is told the toolchain first. A build
    // told ahead of cargo's links the library of the toolchain that was loaded, or is not the
    // daemon's.
    let library = ahead.then(|| built_library(emits.triple()));
    match toolchain_identity(library.as_ref().unwrap_or(&args.symbol_files)) {
        Some(toolchain) => {
            blaze_store::compiles_against(&toolchain);
            if let Some(told) = blaze_store::toolchain().filter(|it| *it != toolchain) {
                eprintln!("store: of `{told}`, not of the toolchain loaded, `{toolchain}`");
            }
            std::thread::spawn(blaze_store::check_packs);
        }
        None => eprintln!("store: the toolchain of the library cannot be told from another"),
    }
    drop(span);
    let built = tracing::info_span!(target: "blaze", "follow cargo")
        .in_scope(|| follow_cargo(&mut db, &mut vfs, &mut checkouts, &args, &listed))?;
    let beside = built.is_some();
    let Parts { root, base, members, debug, lints_capped, link_args } =
        checkouts.parts(&args.project)?;
    (args.checkout, args.base, args.members, args.debug) = (root, base, members, debug);
    (args.lints_capped, args.link_args) = (lints_capped, link_args);
    // The names are resolved before cargo says which program it built: those of the crates
    // it built are, and not of every crate of the workspace.
    if let Some(built) = built {
        let mut names =
            args.crates().filter(|it| built.contains(&args.checkout.join(it.as_str()))).cloned();
        if let Some(krate) = names.next() {
            let batch = names.collect();
            (args.krate, args.batch, args.widened) = (krate, batch, Vec::new());
        }
    }
    eprintln!("load workspace: {:.1?}", start.elapsed());
    memory::checkpoint("load");
    if args.run_tool {
        run_tool_std(&mut db)?;
    }
    prepare_macro_sessions(&db);

    if args.out.is_dir() {
        let texts = harnesses(&db, [&args], None);
        write_harnesses(&mut db, texts);
        return build_batch(&db, args);
    }

    pause_after_load();
    let mut files = WatchedFile::sources(&vfs, &db, read_from);
    let sources: Vec<_> = files
        .iter()
        .filter_map(|it| match it.file {
            Watched::Source(id) => Some((id, &*it.path)),
            Watched::Included(_) => None,
        })
        .collect();
    let mut tree = tree::Tree::new(&db, vfs, &sources);

    let start = Instant::now();
    if checkouts.lto(&args.project)? {
        linked_from_libraries(&db, &args)?;
    }
    // Only a remote the build requires (BLAZE_REMOTE_REQUIRED) is waited for: its failure is
    // the build's.
    let (mut pulling, mut shared) = (Some(shared), None);
    if remote::required() {
        shared = pulled(pulling.take().expect("not joined yet"))?;
    }
    events::at(events::Step::Resolve, None);
    let built = built_crates(&db, &args)?;
    events::graph(&db, &built);
    let mut served = frozen::serve(&mut db, &built);
    // What is frozen names the files its `include!`s read: those not loaded yet are, and
    // what named them is read again.
    if WatchedFile::include_sources(&mut files, &mut tree, &mut db) != Loaded::Nothing {
        listed.touch(&db, None);
        served = frozen::serve(&mut db, &built);
    }
    let mut resolved = resolve_crates(&db, &args, cores())?;
    // Resolving the names finds the files `include!` names that were not loaded. With those
    // in, there may be more.
    while WatchedFile::include_sources(&mut files, &mut tree, &mut db) != Loaded::Nothing {
        // A crate listed beside cargo's build was listed without what they declare.
        listed.touch(&db, None);
        resolved = resolve_crates(&db, &args, cores())?;
    }
    // A tests' program starts in blaze's harness.
    let texts = harnesses(&db, [&args], None);
    write_harnesses(&mut db, texts);
    eprintln!("resolve: {resolved} crates in {:.1?}", start.elapsed());
    events::at(events::Step::Store, Some("the resolved names"));
    frozen::store(&db, &served)?;
    blaze_macro_wasm::flush();
    for said in [blaze_expand::report(), blaze_macro_wasm::report()].into_iter().flatten() {
        eprintln!("{said}");
    }
    memory::checkpoint("resolve");
    // What the other checkouts don't share with this one, before a request waits for it.
    for also in &args.also {
        let start = Instant::now();
        let Parts { root: checkout, base, members, debug, lints_capped, link_args } =
            checkouts.parts(also)?;
        let of_it =
            Args { checkout, base, members, debug, lints_capped, link_args, ..args.clone() };
        let resolved = resolve_crates(&db, &of_it, cores())?;
        eprintln!("resolve {}: {resolved} crates in {:.1?}", also.display(), start.elapsed());
    }
    if !args.also.is_empty() {
        report_sharing(&db);
    }

    anyhow::ensure!(args.patch.is_none() || args.isolate.is_none(), "--patch with --isolate");
    anyhow::ensure!(
        args.splice.is_none() || (args.patch.is_some() && args.link_cmd.is_some()),
        "--binary without --patch and --link-cmd"
    );
    let report_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if !PROBING.get() && !INTERRUPTING.load(Ordering::SeqCst) {
            report_panic(info);
        }
    }));
    let mut known = Rejections::default();
    known.patching = args.patch.is_some();
    let mut outlines = outline::Outlines::default();
    // Beside cargo the symbols were read as cargo wrote them, and are there a moment after
    // the names are resolved: a compile started now would hold up readiness past that.
    if !beside {
        events::at(events::Step::Codegen, Some("ahead"));
        lower_until(&mut db, &args, &listed, cores(), true, &|| table.is_finished())?;
    }
    let (table, whole) =
        table.join().map_err(|_| anyhow::anyhow!("loading the symbols panicked"))??;
    if let Some(whole) = whole {
        // What was loaded is the workspace of both; the rest is as cargo built it.
        args = Args {
            checkout: args.checkout,
            base: args.base,
            members: args.members,
            debug: args.debug,
            lints_capped: args.lints_capped,
            link_args: args.link_args,
            selection: args.selection,
            ..whole
        };
        let links = |file: &PathBuf| args.symbol_files.contains(file);
        anyhow::ensure!(
            library.is_some_and(|it| !it.is_empty() && it.iter().all(links)),
            "cargo built with another toolchain than the one that was loaded"
        );
        known.patching = args.patch.is_some();
    }
    if let Some(libraries) = doc {
        let workspace = checkouts.loaded.workspaces().first();
        let sysroot: Option<PathBuf> =
            workspace.and_then(|it| it.sysroot.root()).map(|it| AsRef::<Path>::as_ref(it).into());
        let building = doctest::Building {
            table: &table,
            args: &args,
            listed: &listed,
            sysroot: sysroot.as_deref(),
        };
        let code = doctest::run_all(&mut db, &building, libraries, emits.triple());
        blaze_store::settled();
        std::process::exit(code);
    }
    if let Some(stale) = stale_copies(&db, &table, &args) {
        anyhow::bail!(stale);
    }
    // What rustc built stands in for a function that is not compiled, and it is not of a
    // source written since: an error there fails the build rather than run the old code.
    let written: FxHashSet<&Path> = table.all_written_since_built().collect();
    // The sources read here that rustc compiled other text of: its copies of what they write
    // are of the text before.
    let mut rewritten: Option<(Vec<vfs::FileId>, Sources)> = None;
    if !written.is_empty() {
        let paths = written_sources(&files, written.iter().copied());
        let mut edited: Vec<_> = paths.keys().copied().collect();
        edited.sort_unstable();
        let sources = Sources { paths };
        let what = match edited.len() {
            1 => "the edited file".to_owned(),
            files => format!("{files} edited files"),
        };
        events::at(events::Step::Check, Some(&what));
        let broken = broken_in(&db, &edited, &sources, &args.checkout, None, !args.lints_capped);
        let why =
            hir::attach_db(&db, || refusal(&db, &edited, &sources, &args.checkout, broken, None));
        if let Some(why) = why {
            if let Refused::Error(said) = &why {
                errors_for_the_daemon(&args, said)?;
            }
            bail!("{why}");
        }
        let crates: Vec<_> = edited
            .iter()
            .flat_map(|&id| ide_db::base_db::relevant_crates(&db, id).to_vec())
            .collect();
        known.distrust(&db, &crates);
        rewritten = Some((edited, sources));
    }
    let sysroot = checkouts.loaded.workspaces().first().and_then(|it| it.sysroot.root());
    let _ = SYSROOT.set(sysroot.map(|it| AsRef::<Path>::as_ref(it).to_owned()));
    let from_manifest;
    // Whether the store is keeping the program as this build compiled it from cold: a worker
    // started once it has settled would load it from there.
    let mut kept = false;
    // What the compile of the program counted, for what is said of it once it is built.
    let mut counted = None;
    let cold = {
        if let Some(unit) = std_unit(&db, &table, &args, &listed)? {
            args.prebuilt = Some(unit);
        }
        if let Some((cmd, exe)) = own_link_line(&db, &args)? {
            (args.link_cmd, args.splice) = (Some(cmd), Some(exe));
        }
        let start = Instant::now();
        let manifest_mode = manifest::mode();
        let manifest_key = manifest_mode.and_then(|_| manifest::key(&db, &args, table.is_empty()));
        let files_of_manifest = match (manifest_mode, &manifest_key) {
            (Some(manifest::Mode::Use), Some(key)) => match manifest::lookup(&db, &args, key) {
                Ok(files) => Some(files),
                Err(why) => {
                    eprintln!("manifest: none used, {why} ({:.1?})", start.elapsed());
                    None
                }
            },
            _ => None,
        };
        let served = files_of_manifest.is_some();
        from_manifest = served;
        let cold = match files_of_manifest {
            Some(files) => {
                let mut cold = Build::of_manifest(files);
                cold.keep(&mut known)?;
                eprintln!(
                    "manifest: {} files of the program from the store in {:.1?}, nothing compiled",
                    cold.wrote.len(),
                    start.elapsed()
                );
                cold
            }
            None => {
                fns::building();
                let compiling = events::compiling(&shown_crate(&db, &args));
                counted = compiling.counted();
                let mut cold = build_unheld(&db, &table, &args, &mut known, &listed)?;
                // An `include!` in a body is first seen when the body is compiled. Only the
                // crates that named the file, and what depends on them, compiled without it:
                // touching every crate compiles all of a program from the store again to tell
                // what changed.
                let including = tree.including(&db);
                if WatchedFile::include_sources(&mut files, &mut tree, &mut db) != Loaded::Nothing {
                    let named: Vec<String> = including
                        .iter()
                        .map(|&it| {
                            let name = it.extra_data(&db).display_name.as_ref();
                            name.map_or_else(|| "?".to_owned(), ToString::to_string)
                        })
                        .collect();
                    if including.is_empty() {
                        bail!(
                            "a file an `include!` or a module named was loaded late, with no crate noted as naming it"
                        );
                    }
                    eprintln!("included late, by {}", named.join(", "));
                    known.touch(&db, Some(&including));
                    listed.touch(&db, Some(&including));
                    cold = build_unheld(&db, &table, &args, &mut known, &listed)?;
                }
                cold
            }
        };
        if let Some(unset) = &cold.unset {
            errors_for_the_daemon(&args, &errors::Said::one(errors::of_none(unset)))?;
            bail!("{unset}");
        }
        if let Some(failed) = blaze_expand::server_failed() {
            errors_for_the_daemon(&args, &errors::Said::one(errors::of_none(failed)))?;
            bail!("{failed}");
        }
        if !served {
            eprint!("cold: ");
            report(&cold);
        }
        for said in [blaze_expand::report(), blaze_macro_wasm::report()].into_iter().flatten() {
            eprintln!("{said}");
        }
        if let Some(why) = cold.refused_reached(&args) {
            bail!("{why}");
        }
        if events::heard() {
            events::built(&db, &shown_crate(&db, &args), &known.stats, None, counted.as_deref());
            if !served {
                events::cache(&known.stats);
            }
        }
        // No rustc checked the source of a build with nothing of rustc's: the worker
        // does, all of it the build compiles and the code nothing reaches too, as rustc
        // would. An error fails every build of the checkout until a source is written
        // (docs/no-rustc.md, D). Only the modules of those crates: the workspace's
        // `rustfmt.toml` and READMEs are loaded too, and so are its integration tests,
        // whose dev-dependencies a build has none of. A program from the store listed
        // no crate.
        if nothing_of_rustc && written.is_empty() {
            let checking = Instant::now();
            let within = match listed.programs(&db) {
                Some(within) => within,
                None => {
                    let mut within = FxHashSet::default();
                    for krate in args.crates() {
                        for krate in crates_to_build(&db, &table, krate, &args)? {
                            let krate: ide_db::base_db::Crate = krate.into();
                            within.extend(krate.transitive_deps(&db));
                        }
                    }
                    Arc::new(within)
                }
            };
            let module_of = |id: vfs::FileId| {
                let relevant = ide_db::base_db::relevant_crates(&db, id);
                relevant.iter().filter(|it| within.contains(*it)).any(|&krate| {
                    hir_def::nameres::crate_def_map(&db, krate)
                        .modules_for_file(&db, id)
                        .next()
                        .is_some()
                })
            };
            let paths: FxHashMap<vfs::FileId, PathBuf> = files
                .iter()
                .filter_map(|it| match it.file {
                    Watched::Source(id) if args.owns(&db, id, &it.path) && module_of(id) => {
                        Some((id, it.path.clone()))
                    }
                    _ => None,
                })
                .collect();
            let mut all: Vec<_> = paths.keys().copied().collect();
            all.sort_unstable();
            events::at(events::Step::Check, Some(&format!("{} files", all.len())));
            let sources = Sources { paths };
            let lints = !args.lints_capped;
            let within = Some(within);
            let broken = broken_in(&db, &all, &sources, &args.checkout, within, lints);
            let why =
                hir::attach_db(&db, || refusal(&db, &all, &sources, &args.checkout, broken, None));
            eprintln!("check: {} files in {:.1?}", all.len(), checking.elapsed());
            if let Some(why) = why {
                if let Refused::Error(said) = &why {
                    errors_for_the_daemon(&args, said)?;
                }
                bail!("{why}");
            }
        }
        memory::report(&mut db, "build")?;
        report_stats(&known.stats);
        let named = shown_crate(&db, &args);
        events::at(events::Step::Link, Some(&named));
        link(
            args.link_cmd.as_deref(),
            args.splice.as_deref(),
            args.patch.as_deref(),
            &args.out,
            args.prebuilt.as_deref(),
            false,
            false,
            packs_whole(&args),
            args.exe.is_some(),
        )
        .map_err(|it| refused_linked(it, &args))?;
        if let Some(exe) = &args.exe {
            // A daemon's link line linked it already.
            if args.splice.is_none() {
                let sysroot = SYSROOT.get().and_then(Option::as_deref);
                // An executable that reaches a stand-in is not the program.
                link_executable(&db, &table, &listed, &args, sysroot, exe)
                    .inspect_err(|_| _ = std::fs::remove_file(exe))?;
            }
            if let Some(path) = &args.dep_info {
                write_dep_info(&db, &args, tree.vfs(), exe, path)?;
            }
            if let Some(path) = &args.inputs {
                write_inputs(&db, tree.vfs(), path)?;
            }
        }
        if let (Some(key), false) = (&manifest_key, served) {
            events::at(events::Step::Store, Some("the manifest"));
            let start = Instant::now();
            let checked = match manifest_mode {
                Some(manifest::Mode::Check) => manifest::check(&db, &args, key, &cold.wrote)
                    .map_err(|why| anyhow::anyhow!("manifest: check: {why}"))?,
                _ => false,
            };
            // What was loaded since the key was made, an `include!` in a body, is not in it.
            let again = manifest::key(&db, &args, table.is_empty());
            match () {
                _ if checked => eprintln!(
                    "manifest: check: the build wrote what the store's manifest has ({:.1?})",
                    start.elapsed()
                ),
                _ if again.as_ref() != Some(key) => {
                    eprintln!("manifest: not kept, the build loaded files its key was made without")
                }
                _ => match manifest::keeping(&db, &args, key, &cold.wrote) {
                    Ok(keeping) => {
                        kept = true;
                        let (files, read) = (cold.wrote.len(), start.elapsed());
                        keeping.settle(move |bytes, took| {
                            eprintln!(
                                "manifest: kept, {files} files, {bytes} bytes ({read:.1?} \
                                 read, {took:.1?} kept beside what came next)"
                            )
                        });
                    }
                    Err(why) => eprintln!("manifest: not kept: {why}"),
                },
            }
        }
        Some(cold)
    };
    // What this build added goes to the other machines while the daemon waits for edits; a
    // build that is not a daemon's has nothing to wait for but this. A daemon's keeps them in
    // step once the pull is over, which its builds do not wait for.
    let once = args.socket.is_none() && !args.watch;
    if once && let Some(pulling) = pulling.take() {
        events::at(events::Step::Remote, None);
        shared = pulled(pulling)?;
    }
    match (shared, pulling) {
        (Some(shared), _) if once => {
            blaze_store::settled();
            shared.push()?
        }
        (Some(shared), _) => shared.keep(),
        (None, Some(pulling)) => {
            std::thread::spawn(move || match pulled(pulling) {
                Ok(Some(shared)) => shared.keep(),
                Ok(None) => {}
                Err(e) => eprintln!("remote: {e:#}"),
            });
        }
        (None, None) => {}
    }
    let out_of_date = hir::attach_db(&db, || known.out_of_date(&db));
    if let Some((edited, sources)) = &rewritten {
        let why = hir::attach_db(&db, || {
            refusal(&db, edited, sources, &args.checkout, None, out_of_date)
        });
        if let Some(why) = why {
            if let Refused::Error(said) = &why {
                errors_for_the_daemon(&args, said)?;
            }
            bail!("{why}");
        }
    }
    // What rustc compiled, by where in the checkout it is: the files as this build read them,
    // but for those written after rustc read them.
    let as_compiled: FxHashMap<PathBuf, triomphe::Arc<str>> = files
        .iter()
        .filter_map(|it| {
            let Watched::Source(id) = it.file else { return None };
            if rewritten.as_ref().is_some_and(|(edited, _)| edited.contains(&id)) {
                return None;
            }
            let inside = it.path.strip_prefix(&args.checkout).ok()?;
            Some((inside.to_owned(), db.file_text(id).text(&db).clone()))
        })
        .collect();
    events::idle();
    let listener = match &args.socket {
        Some(path) => match socket::Served::bind(path) {
            Ok(served) => Some(served),
            Err(socket::NotServed::Taken) => {
                bail!("another daemon serves {}", path.display())
            }
            Err(socket::NotServed::Failed(error)) => {
                // On one line: the daemon reads a worker's log for its last line that
                // starts `Error`, and says it as why the worker ended.
                bail!("listening on {}: {error}", path.display());
            }
        },
        None if args.watch => None,
        None => {
            blaze_store::settled();
            // The process is ending: what these hold is not freed one allocation at a time.
            std::mem::forget((db, table, known, cold, as_compiled));
            return Ok(());
        }
    };

    // The first change after a build of everything waits for what that build left behind.
    // Paid here, the first edit does not.
    {
        use ide_db::base_db::salsa::{Database, Durability};
        db.synthetic_write(Durability::LOW);
        prepare_macro_sessions(&db);
    }
    // Only a worker that serves builds again and lets go after; the bookkeeping of an `lru`
    // costs every fetch of its queries.
    if listener.is_some() {
        memo_capacities(&mut db)?;
    }
    memory::report(&mut db, "warm-up")?;
    first_build_profiled();
    eprintln!("watching {} files", files.len());
    ide_db::executions::take(&db);
    // Requests wait here while a build is under way, and those that have come by the time it
    // is done are built together.
    let (sender, events) = std::sync::mpsc::channel();
    let serving = listener.is_some();
    if let Some(served) = listener {
        let sender = sender.clone();
        // Each connection is read by a thread of its own: one that sends nothing, or
        // slowly, keeps no other from being heard.
        std::thread::spawn(move || {
            for (stream, open) in served.incoming() {
                let sender = sender.clone();
                std::thread::spawn(move || {
                    let _open = open;
                    if let Some(event) = requested(stream) {
                        // The daemon is ending when nothing takes requests any more.
                        let _ = sender.send(event);
                    }
                });
            }
        });
    }
    if serving && tier::enabled() {
        let sender = sender.clone();
        tier::watch(move |ask| _ = sender.send(Event::Tier(ask)));
    } else if serving {
        eprintln!(
            "tier: off (BLAZE_TIER=off, blaze built without `--features llvm`, or not macOS \
             on AArch64)"
        );
    }
    // The first is the program asked for when a request names none.
    // Patches of what rustc linked have no object with all of the program.
    let (wrote, whole) = (Some(args.out.clone()), cold.as_ref().map(|_| args.out.clone()));
    let mut tests_now = None;
    let tests = Some(hir::attach_db(&db, || Tests::of(&db, &listed, &args, &mut tests_now)));
    let mut programs = vec![Program {
        tests,
        tests_now,
        args,
        table: Arc::new(table),
        known,
        unbuilt: Unbuilt::default(),
        wrote,
        whole,
        interrupted: 0,
        moved: false,
        own_lines: FxHashMap::default(),
    }];
    let overlay = match std::env::var("BLAZE_OVERLAY").as_deref() {
        Ok("0") => Overlay::Never,
        Ok(_) => Overlay::Always,
        Err(_) => Overlay::Quick,
    };
    let mut builds = Builds {
        events,
        running: FxHashMap::default(),
        waiting: Default::default(),
        stuck: false,
        edits: Vec::new(),
        tier_asks: Vec::new(),
        tiered: Vec::new(),
        unasked: Vec::new(),
        over_budget: Vec::new(),
        given_up: FxHashSet::default(),
    };
    if from_manifest {
        programs[0].build_from_store();
        builds.unasked.push(0);
    } else if programs[0].dehydrate() {
        builds.unasked.push(0);
    } else if kept
        && let Some(socket) = &programs[0].args.socket
        && let Some(store) = blaze_store::Store::global()
    {
        // Once the store has it all, the daemon starts a worker from it in this one's place,
        // which holds what a warm start holds (`Daemon::settle`).
        let settles = cargo::daemon::settles_beside(socket);
        store.settle_with(move || {
            if let Err(why) = std::fs::write(&settles, b"") {
                eprintln!("{}: {why}", settles.display());
            }
        });
    }
    // How many times running each checkout that is read as a loaded one has been long to turn
    // to or from.
    let mut slow: FxHashMap<PathBuf, u32> = FxHashMap::default();
    // The ones that are loaded instead from the next time on.
    let mut given_up: FxHashSet<PathBuf> = FxHashSet::default();
    // The ones whose link failed: what rustc built of one lacks what the object of the
    // checkout it was read as takes from what rustc built of that.
    let unlinked: Arc<std::sync::Mutex<Vec<PathBuf>>> = Arc::default();
    let slow_switch = std::env::var("BLAZE_SLOW_SWITCH_MS")
        .ok()
        .and_then(|it| it.parse().ok())
        .map_or(SLOW_SWITCH, Duration::from_millis);
    // The links under way that no build waits for, by the object each is of.
    let mut links: FxHashMap<PathBuf, std::thread::JoinHandle<()>> = FxHashMap::default();
    // What the files were like when each object was last built and linked.
    let as_built: Arc<std::sync::Mutex<FxHashMap<PathBuf, Settled>>> = Arc::default();
    // The checkout each loaded one is read from, when that is not itself.
    let mut viewed: FxHashMap<PathBuf, PathBuf> = FxHashMap::default();
    // The crates whose variables name another checkout than the one they were loaded in.
    let mut placed = Placed::default();
    // The programs whose tests blaze's harness lists, by their object.
    let mut harnessed: FxHashSet<PathBuf> = FxHashSet::default();
    loop {
        WatchedFile::watch_included(&mut files, &db);
        // An `include!` in a body is first seen when the body is compiled. Only the crates
        // that named the file, and what depends on them, compiled without it: the builds of
        // the other programs go on to their end.
        let including = tree.includes_more(&db).then(|| tree.including(&db));
        if let Some(including) = &including {
            let named: Vec<String> = (including.iter())
                .map(|&it| {
                    let name = it.extra_data(&db).display_name.as_ref();
                    name.map_or_else(|| "?".to_owned(), ToString::to_string)
                })
                .collect();
            eprintln!("included late, by {}", named.join(", "));
            // With no crate noted as naming it, any may have.
            let named: FxHashSet<ide_db::base_db::Crate> = including.iter().copied().collect();
            let waited = (builds.running.keys().copied())
                .filter(|&at| {
                    !named.is_empty() && !program_reads_any(&db, &programs[at].args, &named)
                })
                .collect();
            builds.settle_waiting(&mut db, &mut programs, &waited)?;
        }
        let including = including.filter(|it| !it.is_empty());
        let loaded = WatchedFile::include_sources(&mut files, &mut tree, &mut db);
        match loaded {
            Loaded::Nothing => {}
            // A build of the daemon's own compiled without them; what rustc linked is of them.
            Loaded::AsRustcRead => programs.iter_mut().for_each(|it| it.known.touch_own(&db)),
            Loaded::New => {
                programs.iter_mut().for_each(|it| it.known.touch(&db, including.as_deref()))
            }
        }
        // What they declare is in the crates now, and a listing from before has none of it.
        if loaded != Loaded::Nothing {
            listed.touch(&db, including.as_deref());
        }
        // The program each client asks for.
        let mut clients: Vec<(usize, Asked)> = Vec::new();
        // Where each of those programs is to be written.
        let mut outputs: FxHashMap<usize, Outputs> = FxHashMap::default();
        // The checkout each loaded one has to be read from for them.
        let mut looked: FxHashMap<PathBuf, PathBuf> = FxHashMap::default();
        // The text editors sent of files they have not written yet, and when it came.
        let mut sent: FxHashMap<vfs::FileId, (String, Instant)> = FxHashMap::default();
        match serving {
            true => {
                let mut later = Vec::new();
                let idle = builds.waiting.is_empty()
                    && builds.edits.is_empty()
                    && builds.tier_asks.is_empty()
                    && builds.unasked.is_empty();
                // A base of its own is built in the background, which edits interrupt.
                let wants_base = (overlay != Overlay::Always && viewed.is_empty())
                    .then(|| {
                        (0..programs.len()).find(|&at| {
                            let program = &programs[at];
                            !builds.running.contains_key(&at)
                                && !builds.given_up.contains(&at)
                                && !program.unbuilt.stale
                                && program.known.wants_base()
                        })
                    })
                    .flatten();
                let (mut own_base, mut interrupt) = (None, false);
                match wants_base {
                    Some(at) if idle => match builds.events.recv_timeout(OWN_BASE_AFTER) {
                        Ok(event) => {
                            if builds.take(event, &mut programs)? {
                                let_go(&mut db, &builds, &programs);
                                memory::served(&mut db)?;
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => own_base = Some(at),
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            bail!("nothing takes requests any more")
                        }
                    },
                    _ if builds.stuck || idle => {
                        let event = builds.events.recv()?;
                        if builds.take(event, &mut programs)? {
                            let_go(&mut db, &builds, &programs);
                            memory::served(&mut db)?;
                        }
                    }
                    _ => {}
                }
                if !builds.over_budget.is_empty() {
                    builds.settle(&mut db, &mut programs)?;
                }
                loop {
                    // Asked again after each: a checkout that joins takes long enough for the
                    // requests sent with its own to arrive.
                    while let Ok(event) = builds.events.try_recv() {
                        if builds.take(event, &mut programs)? {
                            let_go(&mut db, &builds, &programs);
                            memory::served(&mut db)?;
                        }
                    }
                    let Some((request, target, stream)) = builds.waiting.pop_front() else {
                        break;
                    };
                    // What a client asks of is the executable on disk: the splices that wait
                    // for the edits to stop are done first.
                    for binary in splicer::flush() {
                        let linked_at =
                            |it: &&mut Program| it.args.splice.as_deref() == Some(&*binary);
                        programs
                            .iter_mut()
                            .filter(linked_at)
                            .for_each(|it| it.unbuilt.stale = true);
                    }
                    let asked = target.clone();
                    let mut target = target;
                    // A checkout that is not what it was loaded from is loaded again before
                    // anything is built of it. The builds under way and the ones already
                    // taken for this turn are of the checkout as it was.
                    let project = target.as_ref().and_then(|it| it.project.as_ref());
                    let project = project.unwrap_or(&programs[0].args.project);
                    match checkouts.stale(project, &stream.env) {
                        Ok(stale) if stale.is_empty() => {}
                        Ok(_) if !clients.is_empty() => {
                            later.push((request, asked, stream));
                            continue;
                        }
                        Ok(stale) => {
                            builds.settle(&mut db, &mut programs)?;
                            let start = Instant::now();
                            let before = CratesAsLoaded::of(&db);
                            let read_from = SystemTime::now();
                            let reloaded =
                                checkouts.reload(&stale, &stream.env, &mut db, tree.vfs());
                            watch_loaded(&mut db, &mut tree, &mut files, read_from);
                            let new = before.changed(&db);
                            eprintln!(
                                "reload: {} crates are others in {:.1?}",
                                new.len(),
                                start.elapsed()
                            );
                            let mut started = Ok(());
                            for program in &mut programs {
                                let parts = checkouts.parts(&program.args.project)?;
                                let linked = target
                                    .as_ref()
                                    .filter(|it| it.is(&program.args))
                                    .filter(|it| !it.symbol_files.is_empty());
                                let same = parts.members == program.args.members
                                    && parts.debug == program.args.debug;
                                if same && (linked.is_none() || new.is_empty()) {
                                    program.known.touch(&db, Some(&new));
                                    program.unbuilt.stale |= !new.is_empty();
                                    continue;
                                }
                                let again = program.start_over(&db, parts, linked, &new, &files);
                                started = started.and(again);
                            }
                            listed.touch(&db, Some(&new));
                            lock(&as_built).clear();
                            if let Err(error) = reloaded.and(started) {
                                let _ = writeln!(&stream, "failed: {error:#}");
                                continue;
                            }
                        }
                        Err(error) => {
                            let _ = writeln!(&stream, "failed: {error:#}");
                            continue;
                        }
                    }
                    // One build is for one process: its variables are what `env!` reads.
                    if process::differs(&db, &stream.env) {
                        if !clients.is_empty() {
                            later.push((request, asked, stream));
                            continue;
                        }
                        builds.settle(&mut db, &mut programs)?;
                        let reading = process::turn(&mut db, stream.env.clone());
                        prepare_macro_sessions(&db);
                        if !reading.is_empty() {
                            for program in &mut programs {
                                program.known.touch(&db, Some(&reading));
                                program.unbuilt.stale = true;
                            }
                            listed.touch(&db, Some(&reading));
                            lock(&as_built).clear();
                        }
                    }
                    given_up.extend(lock(&unlinked).drain(..));
                    let view = match target.as_ref().and_then(|it| it.project.as_ref()) {
                        Some(project) => {
                            // What a checkout reads is known once a build of it is over.
                            let pending = WatchedFile::unwatched(&files, &db);
                            let built: Vec<&Path> = programs
                                .iter()
                                .enumerate()
                                .filter(|(at, it)| {
                                    it.whole.is_some() && !builds.running.contains_key(at)
                                })
                                .map(|(_, it)| &*it.args.checkout)
                                .collect();
                            let quick = |base: &Path, over: &Path| {
                                !pending && built.contains(&base) && !given_up.contains(over)
                            };
                            checkouts.view_of(project, overlay, &quick)
                        }
                        None => {
                            let base = programs[0].args.checkout.clone();
                            let project = programs[0].args.project.clone();
                            Some(View { over: base.clone(), base, project })
                        }
                    };
                    // A checkout read as another that is as it was built takes no turn.
                    if let (Some(view), Some(target)) = (&view, &target) {
                        let unchanged = links.get(&target.out).is_none_or(|it| it.is_finished())
                            && lock(&as_built)
                                .get(&target.out)
                                .is_some_and(|it| it.still(&files, &db, view, target, request));
                        // Nor is it unchanged when rustc linked the program again since.
                        let relinked = programs.iter().any(|it| {
                            target.is(&it.args)
                                && !target.symbol_files.is_empty()
                                && target.symbol_files != it.args.symbol_files
                        });
                        if unchanged && !relinked && !reach_owed(&target.out) {
                            let _ = writeln!(&stream, "ok unchanged");
                            continue;
                        }
                    }
                    // One build reads a checkout from one place, and one that has packages of
                    // another reads that one as it is.
                    let shares = view
                        .as_ref()
                        .and_then(|it| checkouts.shares_with(&it.base))
                        .map(Path::to_owned);
                    if let Some(view) = &view
                        && (looked.get(&view.base).is_some_and(|over| *over != view.over)
                            || shares.as_ref().is_some_and(|base| {
                                looked.get(base).is_some_and(|over| over != base)
                            }))
                    {
                        later.push((request, target, stream));
                        continue;
                    }
                    if let (Some(view), Some(target)) = (&view, &mut target)
                        && target.project.is_some()
                    {
                        target.project = Some(view.project.clone());
                    }
                    let shared = view.as_ref().is_some_and(|view| {
                        view.base != view.over || viewed.contains_key(&view.base)
                    });
                    let known = match (&target, &view) {
                        (Some(target), _) if programs.iter().any(|it| target.is(&it.args)) => {
                            programs.iter().position(|it| target.is(&it.args))
                        }
                        // The checkouts read as one share its programs.
                        (Some(target), Some(view)) if shared => programs.iter().position(|it| {
                            target.of_crate(&it.args) && it.args.checkout == view.base
                        }),
                        (Some(_), _) => None,
                        (None, _) => Some(0),
                    };
                    let wanted = match &target {
                        Some(target) => Outputs {
                            out: target.out.clone(),
                            patch: target.patch.clone(),
                            link_cmd: target.link_cmd.clone(),
                            splice: target.splice.clone(),
                            exe: target.exe.clone(),
                        },
                        None => Outputs::of(&programs[0].args),
                    };
                    if let Some(at) = known {
                        if builds.running.contains_key(&at)
                            || outputs.get(&at).is_some_and(|it| *it != wanted)
                        {
                            interrupt |= builds.running.get(&at).is_some_and(|it| it.own_base);
                            later.push((request, asked, stream));
                            continue;
                        }
                        // rustc linked the program again since it was built here, as cargo
                        // does of one the daemon did not build while it builds the others,
                        // which the daemon keeps (`Checkout::kept`): what was built of it is
                        // of rustc's copies from before.
                        if let Some(target) = &target
                            && target.is(&programs[at].args)
                            && !target.symbol_files.is_empty()
                            && target.symbol_files != programs[at].args.symbol_files
                        {
                            let again =
                                checkouts.parts(&programs[at].args.project).and_then(|parts| {
                                    programs[at].start_over(&db, parts, Some(target), &[], &files)
                                });
                            if let Err(error) = again {
                                let _ = writeln!(&stream, "failed: {error:#}");
                                continue;
                            }
                            eprintln!("rustc linked {} again: built over", programs[at].args.krate);
                        }
                        outputs.insert(at, wanted);
                        if let Some(view) = view {
                            looked.insert(view.base, view.over);
                        }
                        if let Some(base) = shares {
                            looked.insert(base.clone(), base);
                        }
                        clients.push((at, (request, asked, stream)));
                        continue;
                    }
                    let Some(target) = target else { continue };
                    // A checkout the daemon has not seen joins the ones it has.
                    let joined = match &target.project {
                        Some(project) if view.is_none() => {
                            builds.settle(&mut db, &mut programs)?;
                            let start = Instant::now();
                            let joined =
                                join(project, &mut checkouts, &mut db, &mut tree, &mut files);
                            eprintln!("join {}: {:.1?}", project.display(), start.elapsed());
                            joined
                        }
                        _ => Ok(()),
                    };
                    if let Some(view) = view {
                        looked.insert(view.base, view.over);
                    }
                    let shares = target
                        .project
                        .as_ref()
                        .and_then(|it| checkouts.parts(it).ok())
                        .and_then(|it| checkouts.shares_with(&it.root).map(Path::to_owned));
                    if let Some(base) = shares {
                        // Whoever else reads it in this build reads it so.
                        if looked.get(&base).is_some_and(|over| *over != base) {
                            later.push((request, asked, stream));
                            continue;
                        }
                        looked.insert(base.clone(), base);
                    }
                    let mut known = Rejections::default();
                    known.patching = target.patch.is_some();
                    let loaded = joined
                        .and_then(|()| target.args(&programs[0].args, &checkouts))
                        .and_then(|args| {
                            // The symbols of a checkout are those of the one it shares
                            // packages with, which rustc built.
                            let shared = programs.iter().find(|it| {
                                it.args.symbol_files == args.symbol_files
                                    && (it.args.checkout == args.checkout
                                        || Some(&it.args.checkout) == args.base.as_ref())
                            });
                            let table = match shared {
                                Some(program) => program.table.clone(),
                                None => {
                                    Arc::new(SymbolTable::load(&args.symbol_files, &args.root()?)?)
                                }
                            };
                            Ok((args, table))
                        });
                    match loaded {
                        Ok((args, table)) => {
                            // Never built: every file is as good as edited, but those were
                            // checked when the programs before it were built, all but the
                            // ones rustc did not read, which `distrust_rustc` adds.
                            let unbuilt = Unbuilt {
                                edited: programs[0].unbuilt.edited.clone(),
                                stale: true,
                                unlinked: false,
                                named_anew: true,
                            };
                            let mut program = Program {
                                tests: None,
                                tests_now: None,
                                args,
                                table,
                                known,
                                unbuilt,
                                wrote: None,
                                whole: None,
                                interrupted: 0,
                                moved: false,
                                own_lines: FxHashMap::default(),
                            };
                            if let Some(built) =
                                programs.iter().find(|it| it.same_as(&db, &program))
                            {
                                program.known = built.known.clone();
                                program.unbuilt.stale = false;
                                program.whole = built.whole.clone();
                            }
                            let first = program.args.symbol_files == programs[0].args.symbol_files;
                            program.distrust_rustc(&db, &files, first.then_some(&as_compiled));
                            clients.push((programs.len(), (request, asked, stream)));
                            outputs.insert(programs.len(), wanted);
                            programs.push(program);
                        }
                        // A request for what can't be built is the client's mistake.
                        Err(e) => {
                            let _ = writeln!(&stream, "failed: {e:#}");
                        }
                    }
                }
                builds.waiting.extend(later);
                builds.tell_demoted(&mut programs);
                builds.install_tiered(&mut programs);
                for ask in std::mem::take(&mut builds.tier_asks) {
                    let found = programs
                        .iter()
                        .position(|it| it.args.splice.as_deref() == Some(&*ask.binary));
                    let Some(at) = found else {
                        tier::declined(&ask.binary, "the worker has no program of it");
                        continue;
                    };
                    if builds.running.contains_key(&at) {
                        builds.tier_asks.push(ask);
                        continue;
                    }
                    compile_tiered(&db, &programs[at].known, at, ask, sender.clone());
                }
                // A program with code compiled at a higher level is patched with it, and one
                // with code of the checkout from the store is built without it, as a client of
                // it would ask for an edit.
                for at in std::mem::take(&mut builds.unasked) {
                    let busy = builds.running.contains_key(&at)
                        || outputs
                            .get(&at)
                            .is_some_and(|it| *it != Outputs::of(&programs[at].args));
                    if busy {
                        builds.unasked.push(at);
                        continue;
                    }
                    let Some(client) = Client::nobody() else { continue };
                    let target = (at != 0).then(|| Target::of(&programs[at].args));
                    outputs.insert(at, Outputs::of(&programs[at].args));
                    clients.push((at, (Request::Sync, target, client)));
                }
                // Text an editor sent before it wrote it: the programs that link the file
                // are built from it, with no look at the disk.
                for mut edited in std::mem::take(&mut builds.edits) {
                    let source = |it: &WatchedFile| match it.file {
                        Watched::Source(id) => Some((id, it.path.clone())),
                        Watched::Included(_) => None,
                    };
                    let found =
                        files.iter().filter(|it| it.path == edited.path).find_map(source).or_else(
                            || {
                                let path = dunce::canonicalize(&edited.path).ok()?;
                                files.iter().find_map(|it| {
                                    source(it).filter(|_| {
                                        dunce::canonicalize(&it.path).is_ok_and(|it| it == path)
                                    })
                                })
                            },
                        );
                    let Some((id, path)) = found else {
                        let shown = edited.path.display();
                        let _ = writeln!(&edited.client, "error: {shown} is not a source file");
                        continue;
                    };
                    if viewed.keys().any(|base| path.starts_with(base)) {
                        let shown = path.display();
                        let _ = writeln!(&edited.client, "error: {shown} is read from elsewhere");
                        continue;
                    }
                    // One being built stands in as unbuilt meanwhile: the edit waits for it.
                    let owners: Vec<usize> = (0..programs.len())
                        .filter(|&at| {
                            programs[at].wrote.is_some() || builds.running.contains_key(&at)
                        })
                        .filter(|&at| programs[at].args.owns(&db, id, &path))
                        .collect();
                    if owners.is_empty() {
                        let shown = path.display();
                        let _ = writeln!(&edited.client, "error: no program built has {shown}");
                        continue;
                    }
                    let busy = owners.iter().any(|at| {
                        builds.running.contains_key(at)
                            || outputs
                                .get(at)
                                .is_some_and(|it| *it != Outputs::of(&programs[*at].args))
                    });
                    if busy {
                        interrupt |= owners
                            .iter()
                            .any(|at| builds.running.get(at).is_some_and(|it| it.own_base));
                        builds.edits.push(edited);
                        continue;
                    }
                    let rust = path.extension().is_some_and(|it| it == "rs");
                    let text = std::mem::take(&mut edited.text);
                    let text = if rust { load_cargo::source_text(text) } else { text };
                    if *text != **db.file_text(id).text(&db) {
                        sent.insert(id, (text, edited.at));
                    }
                    // Built for the process the last build was for.
                    edited.client.env = ide_db::base_db::ProcessEnv::try_get(&db)
                        .map(|it| it.vars(&db).clone())
                        .unwrap_or_default();
                    // Asked as a client of each would: one interrupted asks again.
                    let target = |at: usize| (at != 0).then(|| Target::of(&programs[at].args));
                    for &at in &owners[1..] {
                        outputs.insert(at, Outputs::of(&programs[at].args));
                        if let Ok(stream) = edited.client.stream.try_clone() {
                            let client = Client { stream, env: edited.client.env.clone() };
                            clients.push((at, (Request::Edit, target(at), client)));
                        }
                    }
                    outputs.insert(owners[0], Outputs::of(&programs[owners[0]].args));
                    clients.push((owners[0], (Request::Edit, target(owners[0]), edited.client)));
                }
                if interrupt {
                    builds.settle(&mut db, &mut programs)?;
                }
                if let Some(at) = own_base.filter(|_| clients.is_empty())
                    && let Some(client) = Client::nobody()
                {
                    programs[at].build_own_base();
                    let target = (at != 0).then(|| Target::of(&programs[at].args));
                    outputs.insert(at, Outputs::of(&programs[at].args));
                    clients.push((at, (Request::Sync, target, client)));
                }
                // What waited for the build that was interrupted is asked again at once.
                builds.stuck = clients.is_empty() && !interrupt;
                if clients.is_empty() {
                    continue;
                }
                // A package that was the same in two checkouts may have been edited in one.
                let start = Instant::now();
                let differences = checkouts.differences()?;
                if !differences.is_empty() {
                    builds.settle(&mut db, &mut programs)?;
                    let read_from = SystemTime::now();
                    checkouts.share(&mut db, tree.vfs(), differences)?;
                    watch_loaded(&mut db, &mut tree, &mut files, read_from);
                    for program in &mut programs {
                        let Parts { base, members, .. } = checkouts.parts(&program.args.project)?;
                        program.moved |= program.args.members != members;
                        (program.args.base, program.args.members) = (base, members);
                        program.unbuilt.stale = true;
                        program.known.touch(&db, None);
                    }
                    listed.touch(&db, None);
                    eprintln!("split again: {:.1?}", start.elapsed());
                }
            }
            false => std::thread::sleep(Duration::from_millis(5)),
        }
        // The checkouts that are read from somewhere else than for the last build. Their files
        // have the times of another checkout, so they are compared by what is in them.
        let mut switched = Vec::new();
        // The checkouts this build stops or starts reading a loaded one from.
        let mut turned = Vec::new();
        // The crates that are told another place than for the last build.
        let mut moved = Vec::new();
        for (base, over) in looked {
            if *viewed.get(&base).unwrap_or(&base) != over {
                eprintln!("view: {} as {}", over.display(), base.display());
                builds.settle(&mut db, &mut programs)?;
                turned.extend(viewed.get(&base).map(|it| (base.clone(), it.clone())));
                turned.extend((base != over).then(|| (base.clone(), over.clone())));
                db.set_view(&base, &over, built_in(&db, &base));
                moved.extend(place(&mut db, &base, &over, &mut placed));
                tree.relist(&base);
                if base == over {
                    viewed.remove(&base);
                } else {
                    viewed.insert(base.clone(), over);
                }
                switched.push(base);
            }
        }
        // A checkout read as another is read so by every build, so those are waited for.
        let background = serving && overlay != Overlay::Always && viewed.is_empty();
        // Checkouts read as one are built in turn, so their links don't take the turn.
        let detached = serving && !viewed.is_empty();
        // A full build of one forgets what the others' executables were linked from, which
        // their links under way would tell again after it.
        if detached {
            for (_, link) in links.drain() {
                _ = link.join();
            }
        }
        for (&at, wanted) in &outputs {
            let program = &mut programs[at];
            program.unbuilt.stale |= program.wrote.as_ref() != Some(&wanted.out);
            // An executable of its own that was taken away is linked again.
            program.unbuilt.stale |= wanted.exe.as_ref().is_some_and(|it| !it.exists());
            // The files a link reads and writes are not written again under it.
            if let Some(link) = links.remove(&wanted.out) {
                _ = link.join();
            }
            lock(&as_built).remove(&wanted.out);
        }
        let mut noticed = None;
        // Keep scanning until a pass finds nothing new, so one save of several files is one build.
        // Files are read after that: a timestamp can be that of a write still under way, and
        // the rest of that write may not change it again. Whoever asks for a build has finished
        // writing, so one pass is enough then.
        let mut touched = FxHashSet::default();
        // Source files that are no longer there.
        let mut gone = FxHashSet::default();
        for (index, file) in files.iter().enumerate() {
            if let Watched::Source(id) = file.file
                && let Some((_, at)) = sent.get(&id)
            {
                touched.insert(index);
                noticed.get_or_insert((*at, Duration::ZERO));
            }
        }
        loop {
            if tree.relisted(&db) {
                builds.settle(&mut db, &mut programs)?;
            }
            // A new file has no timestamp yet, so the pass below takes it for a changed one.
            for (file, path) in tree.added(&mut db).added {
                files.push(WatchedFile { file: Watched::Source(file), path, mtime: None });
            }
            let before = touched.len() + gone.len();
            for (index, file) in files.iter_mut().enumerate() {
                let on_disk = db.on_disk(&file.path);
                let Ok(now) = std::fs::metadata(&on_disk).and_then(|m| m.modified()) else {
                    let changed = match file.file {
                        Watched::Source(_) => {
                            touched.remove(&index);
                            gone.insert(index)
                        }
                        // Still watched, so the input put back invalidates the macro again.
                        Watched::Included(_) => {
                            file.mtime.take().is_some() && touched.insert(index)
                        }
                    };
                    if changed {
                        noticed.get_or_insert((Instant::now(), Duration::ZERO));
                    }
                    continue;
                };
                gone.remove(&index);
                if switched.iter().any(|base| file.path.starts_with(base)) {
                    file.mtime = Some(now);
                    let same = match file.file {
                        Watched::Source(id) => source_on_disk(&on_disk)
                            .is_ok_and(|text| *text == **db.file_text(id).text(&db)),
                        Watched::Included(ref path) => db.included_is_read(path),
                    };
                    if !same {
                        touched.insert(index);
                        noticed.get_or_insert((Instant::now(), Duration::ZERO));
                    }
                } else if Some(now) != file.mtime {
                    let was = file.mtime.replace(now);
                    // An editor that sent its text writes it after: the same text again.
                    let built = match file.file {
                        Watched::Source(id) if was.is_some() => {
                            !sent.contains_key(&id)
                                && source_on_disk(&on_disk)
                                    .is_ok_and(|text| *text == **db.file_text(id).text(&db))
                        }
                        _ => false,
                    };
                    if !built {
                        touched.insert(index);
                        noticed.get_or_insert((Instant::now(), now.elapsed().unwrap_or_default()));
                    }
                }
            }
            switched.clear();
            if touched.len() + gone.len() == before || !clients.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        prepare_macro_sessions(&db);
        // Without clients, the first program is built whenever a file changes.
        let building: FxHashSet<usize> = match serving {
            true => clients.iter().map(|it| it.0).collect(),
            false => FxHashSet::from_iter([0]),
        };
        for (at, (request, ..)) in &clients {
            let unbuilt = &mut programs[*at].unbuilt;
            unbuilt.stale |= unbuilt.unlinked && *request != Request::Patch;
        }
        for &at in &building {
            programs[at].unbuilt.stale |= reach_owed(&programs[at].args.out);
        }
        // A program of tests asked for the first time is built with blaze's harness then, while
        // nobody edits: the first edit is a patch of it.
        let unharnessed = |at: &usize| {
            of_tests(&programs[*at].args) && !harnessed.contains(&programs[*at].args.out)
        };
        let stale = building.iter().any(|&at| programs[at].unbuilt.stale)
            || building.iter().any(unharnessed)
            || !moved.is_empty();
        let (noticed, age) = match noticed {
            Some(noticed) => noticed,
            None if stale => (Instant::now(), Duration::ZERO),
            None => {
                for (_, (_, _, client)) in clients {
                    let _ = writeln!(&client, "ok unchanged");
                }
                continue;
            }
        };
        if touched.len() + gone.len() != 0 {
            builds.settle(&mut db, &mut programs)?;
        }
        let settled = noticed.elapsed();
        let read = Instant::now();
        let mut change = ChangeWithProcMacros::default();
        let changed = touched.len() + gone.len();
        let mut included = Vec::new();
        let mut sources = Vec::new();
        let mut removed = Vec::new();
        let editing = gone.iter().chain(&touched).filter_map(|&index| match files[index].file {
            Watched::Source(id) => Some(id),
            Watched::Included(_) => None,
        });
        let editing: Vec<_> = editing.collect();
        let mut texts = Vec::new();
        for index in touched {
            let WatchedFile { file, path, .. } = &files[index];
            match file {
                Watched::Source(id) => {
                    let text = match sent.remove(id) {
                        Some((text, _)) => text,
                        None => source_on_disk(&db.on_disk(path))?,
                    };
                    let text = test_harness::kept(&db.file_text(*id).text(&db), text);
                    texts.push((*id, text));
                }
                Watched::Included(path) => included.push(path.clone()),
            }
        }
        // An edit that adds functions to a crate, or takes them away, is a patch when what
        // rustc compiled of every file of the crate is known from before it.
        let homing = programs.iter().any(|it| it.known.homes());
        let mut homed = outlines.adding(&db, &listed, if homing { &texts } else { &[] });
        homed.extend(&editing);
        homed.sort_unstable();
        homed.dedup();
        take_first_values(&db, &mut programs, &homed);
        home_in_rustcs(&db, &listed, &mut programs, &homed);
        outlines.before_edit(&db, &listed, &editing);
        for &index in &gone {
            if let Watched::Source(id) = files[index].file {
                change.change_file(id, None);
                sources.push(id);
                removed.push(id);
            }
        }
        for (id, text) in texts {
            change.change_file(id, Some(text));
            sources.push(id);
        }
        let read = read.elapsed();
        eprintln!("change: {changed} files");
        let start = Instant::now();
        // A write stops the builds under way, so nothing is written when nothing changed.
        if changed != 0 || !background {
            db.apply_change(change);
            prepare_macro_sessions(&db);
        }
        // The tests of a crate an edited file is of are listed again after the edit, and those
        // of a program asked for the first time; a root file the edit did not touch is noted
        // as it was before its harness is written.
        let mut wanted: FxHashSet<Crate> = editing
            .iter()
            .flat_map(|&file| ide_db::base_db::relevant_crates(&db, file).iter().copied())
            .map(Crate::from)
            .collect();
        for program in programs.iter().filter(|it| harnessed.insert(it.args.out.clone())) {
            let args = &program.args;
            wanted.extend(crates_named(&db, &args.krate, args).unwrap_or_default().first());
        }
        let texts = harnesses(&db, programs.iter().map(|it| &it.args), Some(&wanted));
        let mut untouched: Vec<vfs::FileId> = Vec::new();
        if !texts.is_empty() {
            let roots: Vec<_> = texts.iter().map(|it| it.0).collect();
            untouched = roots.iter().copied().filter(|it| !editing.contains(it)).collect();
            take_first_values(&db, &mut programs, &untouched);
            home_in_rustcs(&db, &listed, &mut programs, &untouched);
            outlines.before_edit(&db, &listed, &untouched);
            for file in write_harnesses(&mut db, texts) {
                if !sources.contains(&file) {
                    sources.push(file);
                }
            }
        }
        let applied_change = start.elapsed();
        if !removed.is_empty() {
            tree.remove(&mut db, &removed);
            prepare_macro_sessions(&db);
            let mut index = 0;
            files.retain(|_| {
                index += 1;
                !gone.contains(&(index - 1))
            });
        }
        // Asked after the change: a file can have joined a crate with it.
        if !included.is_empty() {
            let includers = includers(&db, &included);
            take_first_values_of(&db, &mut programs, &includers);
            programs.iter_mut().for_each(|it| it.known.touch_included(&db, &includers));
            listed.touch(&db, None);
        }
        for path in included {
            db.reload_included_file(&path);
        }
        prepare_macro_sessions(&db);
        // Asked of every file before any is noted: noting one forgets the functions listed.
        for program in &mut programs {
            // A harness written into the root of a library is an edit of the program of its
            // tests alone: the others link the library, which runs none of it.
            let args = &program.args;
            let own = crates_named(&db, &args.krate, args).ok();
            let own = own.and_then(|it| it.first().map(|it| it.root_file(&db)));
            let edited = sources.iter().filter(|it| !untouched.contains(it) || own == Some(**it));
            let unbuilt = &mut program.unbuilt;
            unbuilt.stale |= changed != 0;
            unbuilt.edited.extend(edited);
            unbuilt.edited.sort_unstable();
            unbuilt.edited.dedup();
        }
        let paths: FxHashMap<_, _> = files
            .iter()
            .filter_map(|it| match it.file {
                Watched::Source(id) => Some((id, it.path.clone())),
                Watched::Included(_) => None,
            })
            .collect();
        let paths = Arc::new(Sources { paths });
        let before_outlines = start.elapsed();
        let sources = outlines.after_edit(&db, &listed, sources);
        let outlined = start.elapsed();
        for outline::Changed { file, bodies_only, copied, generics, inline, added } in sources {
            if copied || !generics.is_empty() || !inline.is_empty() {
                let why = "an edit of a generic, inline or default trait function, which rustc copies into its callers";
                let inline_unlinked = |program: &Program| {
                    inline.is_empty() || none_linked(&db, &listed, program, file, &inline)
                };
                let unknown = |it: &Program| {
                    copied || !it.known.copies_homed(file, &generics) || !inline_unlinked(it)
                };
                programs
                    .iter_mut()
                    .filter(|it| unknown(it))
                    .for_each(|it| it.known.forget_rustc_for(why));
            }
            if bodies_only {
                if std::env::var_os("BLAZE_STATS").is_some() {
                    eprintln!("bodies only: {file:?}");
                }
                programs.iter_mut().for_each(|it| it.known.touch_bodies(&db, file));
                continue;
            }
            if let Some((crates, files)) = added {
                if std::env::var_os("BLAZE_STATS").is_some() {
                    eprintln!("functions added: {file:?}");
                }
                programs.iter_mut().for_each(|it| it.known.touch_added(&db, &crates, &files));
                listed.touch(&db, Some(&crates));
                continue;
            }
            let crates = ide_db::base_db::relevant_crates(&db, file);
            let crates = (!crates.is_empty()).then_some(crates);
            programs.iter_mut().for_each(|it| it.known.touch(&db, crates));
            listed.touch(&db, crates);
        }
        if !moved.is_empty() {
            programs.iter_mut().for_each(|it| {
                it.known.touch(&db, Some(&moved));
                it.unbuilt.stale = true;
            });
            listed.touch(&db, Some(&moved));
        }
        let edit = Edit { start, age, settled, read, apply: start.elapsed(), changed };
        if std::env::var_os("BLAZE_STATS").is_some() {
            eprintln!(
                "apply-us: change={} outlines={} rest={}",
                applied_change.as_micros(),
                (outlined - before_outlines).as_micros(),
                (edit.apply - applied_change - (outlined - before_outlines)).as_micros()
            );
        }
        if background {
            let mut asked: FxHashMap<usize, Vec<Asked>> = FxHashMap::default();
            for (at, client) in clients {
                asked.entry(at).or_default().push(client);
            }
            for (at, clients) in asked {
                // A program's first build knows what the others over the same code of rustc's
                // found can't be compiled: it can't be in this one either. Without it each
                // program of a whole build after the first compiled a round of every function
                // only to find the same instance fail and start over.
                if programs[at].wrote.is_none() {
                    let (table, cfg_test) =
                        (programs[at].table.clone(), programs[at].args.cfg_test);
                    let learnt: Vec<Rejections> = (programs.iter().enumerate())
                        .filter(|&(other, it)| {
                            other != at
                                && Arc::ptr_eq(&it.table, &table)
                                && it.args.cfg_test == cfg_test
                        })
                        .map(|(_, it)| it.known.rejections())
                        .collect();
                    for other in &learnt {
                        programs[at].known.learn_from(other);
                    }
                }
                let program = &mut programs[at];
                if !program.unbuilt.stale {
                    for (_, _, client) in clients {
                        let _ = writeln!(&client, "ok unchanged");
                    }
                    continue;
                }
                // The program is linked unless every client of it loads the patch.
                let link = clients.iter().any(|it| it.0 != Request::Patch);
                let defer = clients.iter().all(|it| it.0 == Request::Edit);
                let patches = program.known.patches() && !program.moved;
                let own_base = program.known.builds_base();
                let kept = !own_base && (patches || program.interrupted >= INTERRUPTIONS);
                // What stands in for the program while it is built says what program it is.
                let away = Program {
                    args: program.args.clone(),
                    table: program.table.clone(),
                    known: Rejections::default(),
                    unbuilt: Unbuilt::default(),
                    wrote: None,
                    whole: None,
                    interrupted: program.interrupted,
                    moved: program.moved,
                    tests: None,
                    tests_now: None,
                    own_lines: FxHashMap::default(),
                };
                let mut program = Box::new(std::mem::replace(program, away));
                builds.running.insert(at, Running { clients, kept, own_base });
                let (db, listed, paths, sender) =
                    (db.clone(), listed.clone(), paths.clone(), sender.clone());
                let wanted = outputs.remove(&at);
                let done = Arc::new(AtomicBool::new(false));
                if own_base && let Some(budget) = own_base_budget() {
                    let (done, sender) = (done.clone(), sender.clone());
                    let krate = program.args.krate.clone();
                    std::thread::spawn(move || {
                        let over = || memory::held_by(std::process::id()).filter(|&it| it > budget);
                        while !done.load(Ordering::SeqCst) {
                            if over().is_some() {
                                memory::give_back();
                            }
                            if let Some(held) = over() {
                                eprintln!(
                                    "not compiling the program for edits to patch any more: it holds {} MB, more than the {} MB it may (BLAZE_OWN_BASE_MEMORY), and edits go on patching what rustc linked: {krate}",
                                    held >> 20,
                                    budget >> 20
                                );
                                let _ = sender.send(Event::OverBudget(at));
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    });
                }
                std::thread::spawn(move || {
                    let answer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let out = wanted.as_ref().map_or(&program.args.out, |it| &it.out).clone();
                        let answer = admit::admit(&db, &out).and_then(|_admitted| {
                            program.sync(&db, &listed, &paths, &edit, wanted.as_ref(), link, defer)
                        });
                        program.refused_in_codegen(answer)
                    }));
                    done.store(true, Ordering::SeqCst);
                    // The write that interrupts waits for this.
                    drop(db);
                    let answer = match answer {
                        _ if INTERRUPTING.load(Ordering::SeqCst) => None,
                        Ok(answer) => Some(answer),
                        Err(panic) => Some(Ok(program.panicked(&*panic))),
                    };
                    end_link();
                    events::idle();
                    let _ = sender.send(Event::Built(at, program, answer));
                });
            }
            continue;
        }
        // The programs asked for are built side by side: nothing is written to the database
        // until all of them are done, so none is interrupted.
        let answers = loop {
            let answers = std::thread::scope(|scope| {
                let builds: Vec<_> = programs
                    .iter_mut()
                    .enumerate()
                    .filter(|(at, program)| building.contains(at) && program.unbuilt.stale)
                    .map(|(at, program)| {
                        // The program is linked unless every client of it loads the patch.
                        let link = !clients.iter().any(|it| it.0 == at)
                            || clients.iter().any(|it| it.0 == at && it.1.0 != Request::Patch);
                        // A whole build's patch that its executable does not take is built in
                        // full, which only its build can do: it is linked there.
                        let link = link && (!detached || program.args.exe.is_some());
                        let mut theirs = clients.iter().filter(|it| it.0 == at).peekable();
                        let defer =
                            theirs.peek().is_some() && theirs.all(|it| it.1.0 == Request::Edit);
                        let (db, listed, paths, edit) = (db.clone(), &*listed, &*paths, &edit);
                        let wanted = outputs.get(&at);
                        let build = scope.spawn(move || {
                            let answer =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    let out =
                                        wanted.map_or(&program.args.out, |it| &it.out).clone();
                                    let answer = admit::admit(&db, &out).and_then(|_admitted| {
                                        program.sync(&db, listed, paths, edit, wanted, link, defer)
                                    });
                                    program.refused_in_codegen(answer)
                                }));
                            end_link();
                            // Each build's steps are of its own thread ([`events::at`]).
                            events::idle();
                            answer.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                        });
                        (at, build)
                    })
                    .collect();
                builds.into_iter().map(|(at, build)| (at, build.join())).collect::<Vec<_>>()
            });
            let answers: Vec<(usize, anyhow::Result<Result<String, Refused>>)> = answers
                .into_iter()
                .map(|(at, answer)| match answer {
                    Ok(answer) => (at, answer),
                    Err(panic) => (at, Ok(programs[at].panicked(&*panic))),
                })
                .collect();
            // An `env!` the builds were the first to see was told the place of the loaded
            // checkout.
            let late: Vec<_> = viewed
                .iter()
                .flat_map(|(base, over)| place(&mut db, base, over, &mut placed))
                .collect();
            if late.is_empty() {
                break answers;
            }
            prepare_macro_sessions(&db);
            for program in &mut programs {
                program.known.touch(&db, Some(&late));
                program.unbuilt.stale = true;
            }
            listed.touch(&db, Some(&late));
        };
        events::idle();
        if std::env::var_os("BLAZE_QUERIES").is_some() {
            let ran: Vec<String> = ide_db::executions::take(&db)
                .iter()
                .take(400)
                .map(|(query, count)| format!("{query}={count}"))
                .collect();
            eprintln!("ran: {}", ran.join(" "));
            eprintln!("order: {}", ide_db::executions::order(&db).join(" > ").replace(' ', ""));
        }
        // Turning from one that is given up on is the last time it is paid for.
        if !turned.iter().any(|it| given_up.contains(&it.1)) {
            let long = edit.start.elapsed() > slow_switch;
            for (_, over) in &turned {
                let times = slow.entry(over.clone()).or_default();
                *times = if long { *times + 1 } else { 0 };
            }
            if turned.iter().any(|it| slow[&it.1] >= SLOW_SWITCHES) {
                // Turning between two is as long for the one as for the other: the one that
                // is further from what is loaded is what takes long.
                let far = turned.iter().max_by_key(|(base, over)| {
                    let differs = |file: &&WatchedFile| {
                        let Ok(inside) = file.path.strip_prefix(base) else { return false };
                        std::fs::read(&file.path).ok() != std::fs::read(over.join(inside)).ok()
                    };
                    files.iter().filter(differs).count()
                });
                given_up.extend(far.map(|it| it.1.clone()));
                slow.clear();
            }
        }
        let mut replies = FxHashMap::default();
        for (at, answer) in answers {
            replies.insert(at, answer?);
        }
        let mut linking: FxHashMap<usize, Vec<Asked>> = FxHashMap::default();
        trace::write_part();
        for (at, asked) in clients {
            let mut client = &asked.2;
            let _ = match replies.get(&at) {
                Some(Ok(_)) if detached && programs[at].args.exe.is_none() => {
                    linking.entry(at).or_default().push(asked);
                    continue;
                }
                Some(Ok(built)) => writeln!(client, "ok {built}"),
                Some(Err(why)) => writeln!(client, "{}", why.reply()),
                None => writeln!(client, "ok unchanged"),
            };
        }
        for (at, clients) in linking {
            let Some(Ok(built)) = replies.remove(&at) else { continue };
            let wanted = outputs.remove(&at);
            let Outputs { out, link_cmd, patch, splice, .. } =
                match programs[at].outputs(&db, wanted.as_ref()) {
                    Ok(outputs) => outputs,
                    Err(why) => {
                        for (_, _, client) in clients {
                            let _ = writeln!(&client, "failed: {why:#}");
                        }
                        continue;
                    }
                };
            // The answer names a patch when the whole object is as it was.
            let patched = built.starts_with("patch=");
            let spliced = patch.clone();
            let link_cmd = link_cmd.filter(|_| clients.iter().any(|it| it.0 != Request::Patch));
            let defer = clients.iter().all(|it| it.0 == Request::Edit);
            let sender = sender.clone();
            let base = &programs[at].args.checkout;
            let over = viewed.get(base).unwrap_or(base);
            let from = Settled::of(&files, &db, base, over, patch, link_cmd.clone());
            let (as_built, object) = (as_built.clone(), out.clone());
            let packs = packs_whole(&programs[at].args);
            let own_line = programs[at].args.exe.is_some();
            let prebuilt = programs[at].args.prebuilt.clone();
            let read_as = (over != base).then(|| (over.clone(), unlinked.clone()));
            let linked = std::thread::spawn(move || {
                let failed = link(
                    link_cmd.as_deref(),
                    splice.as_deref(),
                    spliced.as_deref(),
                    &object,
                    prebuilt.as_deref(),
                    patched,
                    defer,
                    packs,
                    own_line,
                )
                .err();
                if failed.is_none() {
                    lock(&as_built).insert(object, from);
                }
                trace::write_part();
                for (_, _, client) in clients {
                    let _ = match &failed {
                        None => writeln!(&client, "ok {built}"),
                        Some(why) => writeln!(&client, "failed: {why}"),
                    };
                }
                if let (Some(_), Some((over, unlinked))) = (&failed, read_as) {
                    eprintln!("view: {} is loaded from now on", over.display());
                    lock(&unlinked).push(over);
                }
                if failed.is_some() {
                    let _ = sender.send(Event::Unlinked(at));
                }
            });
            links.insert(out, linked);
        }
        let_go(&mut db, &builds, &programs);
        memory::served(&mut db)?;
    }
}

/// What an object of a checkout read as another was built from, to tell whether it still is.
struct Settled {
    over: PathBuf,
    patch: Option<PathBuf>,
    /// `None` when it was not linked.
    link_cmd: Option<String>,
    /// The checkout's own files and their directories, with when each was written. Another
    /// checkout may have other files, and a file that is new changes none of those there are.
    own: Vec<(PathBuf, Option<SystemTime>)>,
    /// When each file outside the checkout was written.
    others: u64,
}

impl Settled {
    /// To be asked once the build is over: the files are as old as they were when the build
    /// started to read them.
    fn of(
        files: &[WatchedFile],
        db: &RootDatabase,
        base: &Path,
        over: &Path,
        patch: Option<PathBuf>,
        link_cmd: Option<String>,
    ) -> Settled {
        let mut own = Vec::new();
        let mut directories = FxHashSet::default();
        for file in files.iter().filter(|it| it.path.starts_with(base)) {
            let path = db.on_disk(&file.path);
            if let Some(directory) = path.parent()
                && path.starts_with(over)
                && directories.insert(directory.to_owned())
            {
                own.push((directory.to_owned(), written(directory)));
            }
            own.push((path, file.mtime));
        }
        let others = Settled::others(files, base, |file| file.mtime);
        Settled { over: over.to_owned(), patch, link_cmd, own, others }
    }

    fn others(
        files: &[WatchedFile],
        base: &Path,
        written: impl Fn(&WatchedFile) -> Option<SystemTime>,
    ) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = rustc_hash::FxHasher::default();
        for file in files.iter().filter(|it| !it.path.starts_with(base)) {
            (&file.path, written(file)).hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Whether a build for `request` would find the files as they were and write what is
    /// there.
    fn still(
        &self,
        files: &[WatchedFile],
        db: &RootDatabase,
        view: &View,
        target: &Target,
        request: Request,
    ) -> bool {
        let linked = request == Request::Patch || self.link_cmd.is_some();
        // A file outside every checkout is read from where it is.
        let outside = |file: &WatchedFile| written(&db.on_disk(&file.path));
        self.over == view.over
            && self.patch == target.patch
            && linked
            && (self.link_cmd.is_none() || self.link_cmd == target.link_cmd)
            && target.out.exists()
            && self.patch.as_ref().is_none_or(|it| it.exists())
            && self.own.iter().all(|(path, at)| written(path) == *at)
            && self.others == Settled::others(files, &view.base, outside)
    }
}

fn written(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|it| it.modified()).ok()
}

/// A change to the files, and how long each step up to the builds took.
#[derive(Clone, Copy)]
struct Edit {
    /// When the change was applied.
    start: Instant,
    age: Duration,
    settled: Duration,
    read: Duration,
    apply: Duration,
    /// How many files changed.
    changed: usize,
}

/// What a panic said, from what it unwound with.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("no message")
}

impl Program {
    /// Whether to build it again with nobody asking, for the code of the checkout its last
    /// build took from the store: no patch replaces that, so the first edit would wait for
    /// all of it to be compiled.
    fn dehydrate(&mut self) -> bool {
        if !self.known.dehydrate() {
            return false;
        }
        eprintln!(
            "compiling what the store had of the checkout, for the first edit to patch: {}",
            self.args.krate
        );
        self.unbuilt.stale = true;
        true
    }

    /// Has the next build be the one a start makes without a manifest, while nobody asks: a
    /// start from a manifest wrote the program with nothing analysed and holds no base, so
    /// the first edit would be a full build.
    fn build_from_store(&mut self) {
        eprintln!(
            "taking the program's code from the store, for the first edit to patch: {}",
            self.args.krate
        );
        self.unbuilt.stale = true;
    }

    /// Has the next build be one in full of the program, for edits to patch from then on
    /// rather than what rustc linked.
    fn build_own_base(&mut self) {
        eprintln!(
            "compiling the program for edits to patch, while nobody asks: {}",
            self.args.krate
        );
        self.known.build_base();
        self.unbuilt.stale = true;
    }

    /// Takes a build that panicked as the build's error, not the worker's: the clients are
    /// told why, the worker stays, and the next build is a whole one, since what the build
    /// left of the patch base and of the executable may be half written.
    fn panicked(&mut self, panic: &(dyn std::any::Any + Send)) -> Result<String, Refused> {
        self.known.start_again();
        self.unbuilt.stale = true;
        (self.wrote, self.whole) = (None, None);
        let why = format!("a build of {} panicked: {}", self.args.krate, panic_message(panic));
        eprintln!("{why}");
        Err(Refused::Cargos(why))
    }

    /// Takes a build that code generation refused (`CodegenError::Refused`: what it would
    /// link would not be the program the source says) as that build's refusal, not the
    /// worker's error, as [`Program::panicked`] does: the clients are told why and the worker
    /// stays. Nothing else is built in its place.
    fn refused_in_codegen(
        &mut self,
        answer: anyhow::Result<Result<String, Refused>>,
    ) -> anyhow::Result<Result<String, Refused>> {
        let why = match &answer {
            Err(error) => match error.downcast_ref::<CodegenError>() {
                Some(CodegenError::Refused(why)) => why.clone(),
                _ => return answer,
            },
            Ok(_) => return answer,
        };
        self.known.start_again();
        self.unbuilt.stale = true;
        (self.wrote, self.whole) = (None, None);
        eprintln!("refused: {why}");
        Ok(Err(Refused::Cargos(why)))
    }

    /// Notes the crates rustc did not compile from the files this program is of, when it joins
    /// linked with what rustc built: of the first checkout, whose files as the daemon read them
    /// are `as_compiled`, or of its own checkout as it was when rustc built it.
    fn distrust_rustc(
        &mut self,
        db: &RootDatabase,
        files: &[WatchedFile],
        as_compiled: Option<&FxHashMap<PathBuf, triomphe::Arc<str>>>,
    ) {
        let built = self
            .args
            .symbol_files
            .iter()
            .filter(|it| !it.components().any(|it| it.as_os_str() == "rustlib"))
            .filter_map(|it| std::fs::metadata(it).ok()?.modified().ok())
            .max();
        let compiled_from = |file: &WatchedFile, id: vfs::FileId| {
            let inside = file.path.strip_prefix(&self.args.checkout).ok()?;
            Some(match as_compiled {
                Some(as_compiled) => {
                    let text = db.file_text(id).text(db);
                    as_compiled.get(inside).is_some_and(|was| **was == **text)
                }
                // No timestamp is a file taken for a changed one, which rustc did not read.
                None => match file.mtime {
                    Some(written) => written <= built?,
                    None => false,
                },
            })
        };
        let unread: Vec<vfs::FileId> = files
            .iter()
            .filter_map(|it| {
                let Watched::Source(id) = it.file else { return None };
                (!compiled_from(it, id)?).then_some(id)
            })
            .collect();
        let other: Vec<_> = unread
            .iter()
            .flat_map(|&id| ide_db::base_db::relevant_crates(db, id).to_vec())
            .collect();
        self.known.distrust(db, &other);
        // A file rustc did not read is edited for this program: its build checks it.
        self.unbuilt.edited.extend(unread);
        self.unbuilt.edited.sort_unstable();
        self.unbuilt.edited.dedup();
    }

    /// Has the program be one of its checkout as it was loaded again: of the crates and the
    /// debug information of `parts`, and never built. It is linked as `linked` says, when
    /// that names what rustc built of the checkout since, and else with what it was linked
    /// with, where the crates in `new` are not the ones rustc compiled.
    fn start_over(
        &mut self,
        db: &RootDatabase,
        parts: Parts,
        linked: Option<&Target>,
        new: &[ide_db::base_db::Crate],
        files: &[WatchedFile],
    ) -> anyhow::Result<()> {
        let Parts { root, base, members, debug, lints_capped, link_args } = parts;
        (self.args.checkout, self.args.base) = (root, base);
        (self.args.members, self.args.debug) = (members, debug);
        (self.args.lints_capped, self.args.link_args) = (lints_capped, link_args);
        (self.whole, self.moved) = (None, true);
        self.unbuilt.stale = true;
        match linked {
            // rustc has built the checkout since: its copies are new ones.
            Some(linked) => {
                let patching = self.known.patching;
                self.known = Rejections::default();
                self.known.patching = patching;
                self.args.symbol_files = linked.symbol_files.clone();
                if linked.link_cmd.is_some() {
                    self.args.link_cmd = linked.link_cmd.clone();
                }
                self.table =
                    Arc::new(SymbolTable::load(&self.args.symbol_files, &self.args.root()?)?);
                self.distrust_rustc(db, files, None);
                self.unbuilt.named_anew = true;
                self.args.widened.clear();
                // The tests listed were of the program rustc linked before.
                (self.tests, self.tests_now) = (None, None);
            }
            None => {
                self.known.start_again();
                self.known.distrust(db, new);
            }
        }
        Ok(())
    }

    /// Takes over from rustc the crates that an edit since the last build is to, of those the
    /// program links and the daemon was not asked to build, and the crates between them and
    /// the ones it was: rustc's copies of them are from before the edit. A crate the program
    /// depends on but rustc did not link (an integration test's package's dependencies it does
    /// not use) has nothing in it an edit changes. The result is why one can't be taken over,
    /// when one can't.
    fn widen(&mut self, db: &RootDatabase) -> Option<String> {
        // Every crate of a whole build is the daemon's: rustc has no copy of any.
        if self.args.exe.is_some() {
            return None;
        }
        let built: Vec<ide_db::base_db::Crate> = self
            .args
            .crates()
            .filter_map(|name| crates_named(db, name, &self.args).ok())
            .flatten()
            .map(Into::into)
            .collect();
        let table = &self.table;
        let used: FxHashSet<_> = built
            .iter()
            .flat_map(|it| it.transitive_deps(db))
            .filter(|&it| table.is_empty() || links(db, table, it.into()))
            .collect();
        let edited: Vec<_> = self
            .unbuilt
            .edited
            .iter()
            .filter_map(|&file| outline::left_to_rustc(db, &built, &used, file))
            .collect();
        if edited.is_empty() {
            return None;
        }
        let mut stale: Vec<_> = edited.iter().flat_map(|it| it.transitive_rev_deps(db)).collect();
        stale.retain(|it| used.contains(it) && !built.contains(it));
        stale.sort_unstable();
        stale.dedup();
        for &krate in &stale {
            let Some(root) = crate_root_file(db, krate.into()) else {
                let name = krate.extra_data(db).display_name.as_ref().map(|it| it.to_string());
                return Some(format!(
                    "an edit is to `{}`, which the program uses and the daemon was not asked to \
                     build: its root file has no path to build it by",
                    name.unwrap_or_default()
                ));
            };
            let root = root.display().to_string();
            eprintln!("widened: {root} is built here from now on, rustc's copy is of before");
            self.args.widened.push(root);
        }
        self.known.distrust(db, &edited);
        self.known.forget_base();
        (self.whole, self.moved) = (None, true);
        None
    }

    /// Reads what rustc built again when it has built since, in a program whose crates
    /// differ in `-C overflow-checks`: which crate holds a copy of a function decides
    /// whether a call of it checks, and what was compiled by the copies of before is not
    /// kept.
    fn read_copies_again(&mut self, db: &RootDatabase) -> anyhow::Result<()> {
        let args = &self.args;
        let built: Vec<Crate> =
            args.crates().filter_map(|it| crates_named(db, it, args).ok()).flatten().collect();
        if !self.table.built_since_read() || !checks_differ(db, &built) {
            return Ok(());
        }
        self.table = Arc::new(SymbolTable::load(&args.symbol_files, &args.root()?)?);
        let patching = self.known.patching;
        self.known = Rejections::default();
        self.known.patching = patching;
        (self.whole, self.moved) = (None, true);
        self.unbuilt.stale = true;
        Ok(())
    }

    /// Whether the last build of this program is one of `other`, which has had none: the two
    /// are of the same crates, as checkouts that share them have it. What `other` is then
    /// asked for is a patch of that build.
    fn same_as(&self, db: &RootDatabase, other: &Program) -> bool {
        let (ours, theirs) = (&self.args, &other.args);
        let crates = |args: &Args| {
            let names = args.crates();
            names.map(|it| crates_named(db, it, args).ok()).collect::<Option<Vec<_>>>()
        };
        !self.unbuilt.stale
            && self.whole.is_some()
            && self.known.patches()
            && self.known.patching == other.known.patching
            && Arc::ptr_eq(&self.table, &other.table)
            && (ours.cfg_test, ours.unit_test, &ours.selection)
                == (theirs.cfg_test, theirs.unit_test, &theirs.selection)
            && crates(ours).is_some_and(|it| Some(it) == crates(theirs))
    }

    /// Where a build for `wanted` goes, the program's own place without it. A whole build is
    /// linked by the worker's own line, which no client names: the one of the executable
    /// asked for, from the object asked for, which a checkout read as another has its own of.
    fn outputs(&mut self, db: &RootDatabase, wanted: Option<&Outputs>) -> anyhow::Result<Outputs> {
        let mut wanted = wanted.cloned().unwrap_or_else(|| Outputs::of(&self.args));
        let Some(exe) = wanted.exe.clone() else { return Ok(wanted) };
        if let Some(cmd) = &wanted.link_cmd {
            self.own_lines.entry(exe).or_insert_with(|| cmd.clone());
            return Ok(wanted);
        }
        let cmd = match self.own_lines.get(&exe) {
            Some(cmd) => cmd.clone(),
            None => {
                let args = Args {
                    out: wanted.out.clone(),
                    patch: wanted.patch.clone(),
                    link_cmd: None,
                    exe: Some(exe.clone()),
                    ..self.args.clone()
                };
                let Some((cmd, _)) = own_link_line(db, &args)? else { return Ok(wanted) };
                self.own_lines.insert(exe.clone(), cmd.clone());
                cmd
            }
        };
        (wanted.link_cmd, wanted.splice) = (Some(cmd), Some(exe));
        Ok(wanted)
    }

    /// Brings the program up to date with the database. The result is what to tell its
    /// clients: what they have to load, or why the source can't be built.
    fn sync(
        &mut self,
        db: &RootDatabase,
        listed: &Listed,
        paths: &Sources,
        edit: &Edit,
        wanted: Option<&Outputs>,
        link_it: bool,
        defer: bool,
    ) -> anyhow::Result<Result<String, Refused>> {
        // The crates left to rustc are as edited when a client has named what it built since.
        let named_anew = std::mem::take(&mut self.unbuilt.named_anew);
        let unnamed = if named_anew { None } else { self.widen(db) };
        self.read_copies_again(db)?;
        std_unit_again(db, &self.table, &self.args, listed)?;
        // What the worker wrote under the target directory is taken away by a clean under it,
        // which tells it nothing: with the object of the program gone, there is nothing to
        // patch.
        if let Some(gone) = self.whole.clone().filter(|it| !whole::is_there(it)) {
            eprintln!("{} is gone: built in full", gone.display());
            self.known.start_again();
            self.unbuilt.stale = true;
            (self.wrote, self.whole) = (None, None);
        }
        let Outputs { out, patch, link_cmd, splice, exe } = self.outputs(db, wanted)?;
        let Program {
            args, table, known, unbuilt, wrote, whole, tests, tests_now, own_lines, ..
        } = self;
        let args = &Args { out, patch, link_cmd, splice, exe, ..args.clone() };
        // A patch is for the object with all of the program, which this build may be the
        // first to be asked for at this place.
        if let Some(whole) = whole.as_ref().filter(|it| **it != args.out) {
            let _reading = whole::read(whole)?;
            if !same_file::is_same_file(whole, &args.out).unwrap_or(false) {
                let _writing = whole::writing(&args.out);
                _ = std::fs::remove_file(&args.out);
                if std::fs::hard_link(whole, &args.out).is_err() {
                    std::fs::copy(whole, &args.out)?;
                }
            }
        }
        let &Edit { start, age, settled, read, apply, changed } = edit;
        let named = shown_crate(db, args);
        events::flights();
        fns::building();
        let compiling = events::compiling(&named);
        listed.building(args.crates().filter_map(|it| crates_named(db, it, args).ok()).flatten());
        // Another checkout's files are not this program's to be refused for.
        let mut edited = unbuilt.edited.clone();
        edited.retain(|file| paths.paths.get(file).is_none_or(|it| args.owns(db, *file, it)));
        // Neither is what no crate that is built reads: an example, or a test.
        edited.retain(|&file| listed.sees(db, file));
        // The check of the edited files does not wait for the build: it reads other things. A
        // build that can't be a patch waits for it, and a text that does not compile is not
        // built whole: what it would make of the program is not what runs, and the next edit
        // that compiles is to be a patch of what does.
        let (warm, broken) = std::thread::scope(|scope| {
            let within = listed.programs(db);
            if let Some(crates) = &within {
                verify_impls(scope, db, crates);
            }
            let check = {
                let (db, edited, root) = (db.clone(), &edited, &args.checkout);
                let lints = !args.lints_capped;
                scope.spawn(move || {
                    tracing::info_span!(target: "blaze", "check edited")
                        .in_scope(|| broken_in(&db, edited, paths, root, within, lints))
                })
            };
            let (check, checked) = (std::sync::Mutex::new(Some(check)), std::sync::OnceLock::new());
            let broken = || {
                let joined = || lock(&check).take().and_then(|it| it.join().ok()).flatten();
                checked.get_or_init(joined).clone()
            };
            let hold =
                || unparsed(db, &edited, paths, &args.checkout).is_some() || broken().is_some();
            let warm = build(db, table, args, known, listed, Some(&hold));
            (warm, broken())
        });
        let hold = || unparsed(db, &edited, paths, &args.checkout).is_some() || broken.is_some();
        let held = |known: &Rejections| {
            let why = refusal(db, &edited, paths, &args.checkout, broken.clone(), None);
            let why = why.unwrap_or_else(|| {
                let said = errors::of_none("the source does not compile");
                Refused::Error(errors::Said::one(said))
            });
            eprintln!(
                "edit (apply {apply:.1?}): not built: no patch, and the source does not compile; \
                 the next edit is a patch of the last build"
            );
            report_stats(&known.stats);
            eprintln!("refused: {why}");
            eprintln!("edit-to-binary: {:.1?}", start.elapsed());
            why
        };
        let Some(mut warm) = warm? else { return Ok(Err(held(known))) };
        // Code compiled at a higher level can call what rustc built into its callers and left
        // no symbol of. The executable can't take a patch with it, and a full link would start
        // the program over, so the code goes instead; of several functions installed
        // together, the others are installed again one by one, a build each.
        loop {
            let missing = match (&args.splice, &args.patch) {
                (Some(binary), Some(patch)) if known.stats.patched.is_some() => {
                    missing_in(binary, patch)
                }
                _ => None,
            };
            let again = match missing {
                Some(missing) => known.tiers.refuse(&format!(
                    "no patch can hold its code: {missing} is not in the executable"
                )),
                None => known.tiers.held(),
            };
            if !again {
                break;
            }
            match build(db, table, args, known, listed, Some(&hold))? {
                Some(again) => warm = again,
                None => return Ok(Err(held(known))),
            }
        }
        let full = known.stats.patched.is_none();
        *wrote = Some(args.out.clone());
        let built = start.elapsed();
        eprint!("edit (apply {apply:.1?}): ");
        report(&warm);
        report_stats(&known.stats);
        let counted = compiling.counted();
        drop(compiling);
        let edited_paths: FxHashMap<vfs::FileId, PathBuf> = edited
            .iter()
            .filter_map(|file| {
                let path = paths.paths.get(file)?;
                Some((*file, path.strip_prefix(&args.checkout).unwrap_or(path).to_owned()))
            })
            .collect();
        events::built(db, &named, &known.stats, Some(&edited_paths), counted.as_deref());
        let reported = start.elapsed();
        let span = tracing::info_span!(target: "blaze", "out of date").entered();
        let out_of_date = hir::attach_db(db, || known.out_of_date(db));
        drop(span);
        let span = tracing::info_span!(target: "blaze", "refusal").entered();
        let mut failed = unnamed
            .map(Refused::Cargos)
            .or_else(|| refusal(db, &edited, paths, &args.checkout, broken, out_of_date))
            .or_else(|| {
                let unset = warm.unset.as_deref()?;
                Some(Refused::Error(errors::Said::one(errors::of_none(unset))))
            })
            .or_else(|| warm.refused_reached(args).map(Refused::Cargos));
        drop(span);
        let span = tracing::info_span!(target: "blaze", "stale copies").entered();
        failed = failed.or_else(|| stale_copies(db, table, args).map(Refused::Cargos));
        drop(span);
        let now = tracing::info_span!(target: "blaze", "tests now")
            .in_scope(|| hir::attach_db(db, || Tests::of(db, listed, args, tests_now)));
        // blaze's harness lists the tests as they are now, and rustc's as they were.
        let harnessed = of_tests(args)
            && crates_named(db, &args.krate, args)
                .unwrap_or_default()
                .first()
                .is_some_and(|&krate| harness_main(db, &listed.of(db, krate).functions).is_some());
        if let Some(why) = harness_alone(args)
            && !harnessed
        {
            let why = format!("{why}, and blaze wrote no test harness for it (no-rustc H, #88)");
            failed = failed.or(Some(Refused::Cargos(why)));
        }
        match tests {
            Some(_) if harnessed => {}
            Some(first) => failed = failed.or_else(|| first.changed(&now).map(Refused::Cargos)),
            // The first build of the program is of the source rustc built it from.
            None if failed.is_none() => *tests = Some(now),
            None => {}
        }
        let checked = start.elapsed();
        // A full build that is refused is undone, when there is a build before it: that one
        // stays the program, and what the next edit is a patch of.
        let undone = failed.is_some() && known.undo_built();
        if undone {
            eprintln!(
                "undone: the build is refused, and the next edit is a patch of the one before"
            );
        } else {
            warm.keep(known)?;
            if full {
                *whole = Some(args.out.clone());
                // The executables of the other checkouts were linked with the whole object of
                // before: a patch of this one is not spliced into them, they are linked again.
                for exe in own_lines.keys().filter(|it| Some(*it) != args.exe.as_ref()) {
                    splicer::forget(exe);
                    _ = splicer::claim(exe, true);
                    lock(&LINKED).remove(exe);
                }
            }
        }
        if failed.is_none() || full && !undone {
            let patched = !full;
            let (splice, patch) = (args.splice.as_deref(), args.patch.as_deref());
            let cmd = args.link_cmd.as_deref().filter(|_| failed.is_none() && link_it);
            begin_link();
            events::at(events::Step::Link, Some(&named));
            let packs = packs_whole(args);
            let own_line = args.exe.is_some();
            let unlinked = link(
                cmd,
                splice,
                patch,
                &args.out,
                args.prebuilt.as_deref(),
                patched,
                defer,
                packs,
                own_line,
            )
            .err();
            if let Some(lost) = unlinked.as_ref().and_then(|it| it.downcast_ref::<PatchUnlinked>())
            {
                eprintln!("{lost}: built in full");
                known.start_again();
                return self.sync(db, listed, paths, edit, wanted, link_it, defer);
            }
            failed = failed.or(unlinked.map(|it| Refused::Cargos(it.to_string())));
            let unspliced = || failed.is_none() && link_it && full && args.splice.is_none();
            if let Some(exe) = args.exe.as_deref().filter(|_| unspliced()) {
                let sysroot = SYSROOT.get().and_then(Option::as_deref);
                let linked = link_executable(db, table, listed, args, sysroot, exe);
                failed = linked.err().map(|it| Refused::Cargos(format!("{it:#}")));
            }
        }
        if let Some(why) = &failed {
            eprintln!("refused: {why}");
        } else {
            let unlinked = args.link_cmd.is_some() && !link_it;
            *unbuilt = Unbuilt { unlinked, ..Unbuilt::default() };
        }
        let total = start.elapsed();
        let steps = {
            let [roots, revalidate, compile] = warm.steps;
            let phases = known.stats.phases;
            // In the order they happen, from the save on. Each is a duration of its own.
            [
                ("notice", age),
                ("settle", settled),
                ("read", read),
                ("apply", apply),
                ("roots", roots),
                // All of the checking: threads look at the kinds one after another.
                ("check", revalidate),
                ("reach", phases.compile),
                ("resolve", phases.resolve),
                ("define", phases.define),
                ("object", phases.object),
                ("discarded", phases.discarded),
                ("statics", phases.statics),
                ("setup", phases.setup),
                (
                    "compile-rest",
                    compile.saturating_sub(
                        phases.compile
                            + phases.resolve
                            + phases.define
                            + phases.object
                            + phases.discarded
                            + phases.statics
                            + phases.setup,
                    ),
                ),
                ("write", warm.emit),
                ("build-rest", built.saturating_sub(apply + warm.codegen + warm.emit)),
                ("report", reported - built),
                // What could refuse the edit: types that changed, tests, stale copies.
                ("refuse", checked - reported),
                ("link", total - checked),
            ]
        };
        if std::env::var_os("BLAZE_STATS").is_some() {
            let steps: Vec<String> =
                steps.iter().map(|(name, took)| format!("{name}={}", took.as_micros())).collect();
            eprintln!("timeline-us: {}", steps.join(" "));
            eprintln!(
                "asked: {} roots, {} instances, slowest {:.1?}; unchanged by their items: {} roots, \
                 {} instances; longest on a thread: roots {:.1?}, instances {:.1?}, statics \
                 {:.1?}, left {:.1?}",
                warm.looked.roots_asked,
                warm.looked.instances_asked,
                warm.looked.slowest,
                warm.looked.roots_unchanged,
                warm.looked.instances_unchanged,
                warm.looked.roots,
                warm.looked.instances,
                warm.looked.statics,
                warm.looked.left,
            );
            let macros = [blaze_expand::report(), blaze_macro_wasm::report()];
            for said in macros.into_iter().flatten() {
                eprintln!("{said}");
            }
        }
        eprintln!("edit-to-binary: {total:.1?}");
        write_reach(db, table, listed, args, failed.is_none());
        if let Some(why) = failed {
            return Ok(Err(why));
        }
        // What the caller has to load to be up to date: the functions that differ from the
        // first object when the edit could be a patch, and all of them when it could not.
        let (kind, object) = match (&args.patch, known.stats.patched) {
            (Some(patch), Some(_)) => ("patch", patch),
            _ => ("object", &args.out),
        };
        let answer = format!(
            "{kind}={} functions={} files={changed} ms={}",
            object.display(),
            known.stats.patched.unwrap_or(known.stats.compiled),
            total.as_millis()
        );
        Ok(Ok(trace::with_spans(answer, &worker_spans(start, settled, read, &steps))))
    }
}

/// The steps of a build for the client: the worker's wait from the request to the look at
/// the files, then the steps of the timeline one after another from `start`.
fn worker_spans(
    start: Instant,
    settled: Duration,
    read: Duration,
    steps: &[(&str, Duration)],
) -> Vec<trace::Span> {
    let level = Some(trace::WORKER);
    let read_from = start.checked_sub(read).unwrap_or(start);
    let noticed = read_from.checked_sub(settled).unwrap_or(read_from);
    let mut spans = Vec::new();
    if let Some(arrived) = trace::last_arrival().filter(|it| *it < noticed) {
        spans.push(trace::span("worker wait", level, arrived, noticed));
    }
    spans.push(trace::span("worker settle", level, noticed, read_from));
    spans.push(trace::span("worker read", level, read_from, start));
    let mut at = start;
    for &(name, took) in steps.iter().skip_while(|it| it.0 != "apply") {
        spans.push(trace::span(format!("worker {name}"), level, at, at + took));
        at += took;
    }
    spans
}

#[allow(dead_code)]
fn abs(path: &Path) -> anyhow::Result<VfsPath> {
    Ok(VfsPath::from(AbsPathBuf::assert_utf8(dunce::canonicalize(path)?)))
}

fn report_stats(stats: &hir_ty_codegen::BuildStats) {
    if stats.bodies == 0 {
        return;
    }
    eprintln!(
        "stats: {} of {} functions compiled, {} reused",
        stats.compiled, stats.bodies, stats.reused
    );
    if let Some(patched) = stats.patched {
        eprintln!("patch: {patched} functions");
    }
    if let Some(why) = stats.declined {
        eprintln!("no patch: {why}");
    }
    let phases = stats.phases;
    eprintln!(
        "phases: compile {:.1?}, resolve {:.1?}, define {:.1?}, fallback {:.1?}, object {:.1?}, {} rounds",
        phases.compile, phases.resolve, phases.define, phases.fallback, phases.object, stats.rounds
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_splice_names_statics_as_the_code_generator_does() {
        assert_eq!(splice::LASTING_STATIC, hir_ty_codegen::LASTING_STATIC);
        assert_eq!(splice::MOVED_STATIC, hir_ty_codegen::MOVED_STATIC);
    }

    #[test]
    fn a_directory_of_the_checkout_named_registry_is_watched() {
        let immutable = |path: &str| WatchedFile::immutable(Path::new(path));
        let home = "/home/one/.cargo";
        assert!(immutable(&format!(
            "{home}/registry/src/index.crates.io-1949cf8c6b5b557f/memchr-2.7.4/src/lib.rs"
        )));
        assert!(immutable(&format!(
            "{home}/git/checkouts/gix-0f1c5b2bd2cb9a5e/1a2b3c4/src/lib.rs"
        )));
        assert!(!immutable("/ws/cargo/src/cargo/ops/registry/publish.rs"));
        assert!(!immutable("/ws/cargo/src/cargo/sources/registry/index/mod.rs"));
        assert!(!immutable("/ws/crates/registry/src/lib.rs"));
        assert!(!immutable("/ws/crates/git/checkouts.rs"));
    }

    #[test]
    fn a_fetched_crate_is_named_the_same_wherever_cargo_keeps_it() {
        let fetched = "registry/src/index.crates.io-1949cf8c6b5b557f/memchr-2.7.4/src/lib.rs";
        let at =
            |home: &str| crate_place(Path::new(&format!("{home}/{fetched}")), None, Some(home));
        assert_eq!(at("/home/one/.cargo"), at("/Users/other/.cargo"));
        assert_eq!(at("/home/one/.cargo"), format!("/cargo/{fetched}"));
        let elsewhere = Path::new("/opt/crates/lib.rs");
        assert_eq!(crate_place(elsewhere, None, Some("/home/one/.cargo")), "/opt/crates/lib.rs");
        let inside = Path::new("/ws/app/src/main.rs");
        assert_eq!(
            crate_place(inside, Some(Path::new("app/src/main.rs")), None),
            "app/src/main.rs"
        );
    }

    /// A link writes the executable beside its place under the same name, for it to be renamed
    /// there: a client that reads it meanwhile finds the last one, never none.
    #[cfg(unix)]
    #[test]
    fn a_link_writes_the_executable_aside_under_its_name() {
        let dir = std::env::temp_dir().join(format!("blaze-aside-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("my app");
        let line = format!("cc -B/x -fuse-ld=lld a.o -o '{}' -lc", binary.display());
        let (aside, at) = linked_aside(&line, &binary).unwrap();
        assert_eq!(at.file_name(), binary.file_name());
        assert_eq!(at.parent().unwrap().parent(), Some(&*dir));
        assert!(at.parent().unwrap().is_dir());
        let args = shlex::split(&aside).unwrap();
        assert_eq!(args[args.len() - 3..], ["-o", at.to_str().unwrap(), "-lc"]);
        assert!(linked_aside("cc a.o -o elsewhere", &binary).is_none());
        let twice = format!("{line} -o '{}'", binary.display());
        assert!(linked_aside(&twice, &binary).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An rlib whose `lib.rmeta` is `metadata`, as an archive of one member.
    fn rlib(metadata: &[u8]) -> Vec<u8> {
        let mut archive = b"!<arch>\n".to_vec();
        // The name, then what of the member is not read, then its size.
        let size = metadata.len();
        let header = format!("{:<48}{size:<10}`\n", "lib.rmeta/");
        archive.extend_from_slice(header.as_bytes());
        archive.extend_from_slice(metadata);
        if metadata.len() % 2 == 1 {
            archive.push(b'\n');
        }
        archive
    }

    /// Metadata with its root at 32: the crate's header, then `rest`.
    fn metadata(svh: [u8; 16], rest: &[u8]) -> Vec<u8> {
        let mut bytes = b"rust\0\0\0\x0a".to_vec();
        bytes.extend_from_slice(&32u64.to_le_bytes());
        bytes.extend_from_slice(&[0xc1; 16]);
        bytes.extend_from_slice(b"\0\x14aarch64-apple-darwin\xc1");
        bytes.extend_from_slice(&svh);
        bytes.extend_from_slice(rest);
        bytes
    }

    #[test]
    fn a_library_is_told_apart_by_the_hash_its_metadata_gives_it() {
        let built = metadata([1; 16], &[0; 8]);
        let hash = metadata_hash(&rlib(&built));
        assert!(hash.is_some());
        assert_eq!(metadata_hash(&rlib(&built)), hash);
        assert_ne!(metadata_hash(&rlib(&metadata([2; 16], &[0; 8]))), hash);
        // Past the start of the root is what the rest of the metadata is, which the hash of
        // the crate covers.
        let long = |end| metadata([1; 16], &[[0; 512].as_slice(), &[end]].concat());
        assert_eq!(metadata_hash(&rlib(&long(1))), metadata_hash(&rlib(&long(2))));
        assert_eq!(metadata_hash(&rlib(b"not metadata")), None);
    }

    /// A build told ahead of cargo's finds the library in the toolchain; the daemon started
    /// once cargo built is given rustc's link line. Both have to key the store alike, or the
    /// daemon's first start after a build finds nothing that build stored (#41).
    #[test]
    fn the_toolchain_is_named_alike_ahead_of_cargo_and_from_rustcs_link_line() {
        let dir = std::env::temp_dir().join(format!("blaze-library-{}", std::process::id()));
        let library = dir.join("lib/rustlib/x86_64-unknown-linux-gnu/lib");
        let deps = dir.join("target/debug/deps");
        std::fs::create_dir_all(&library).unwrap();
        std::fs::create_dir_all(&deps).unwrap();
        let mut written = |at: &Path, name: &str, svh: u8| {
            let path = at.join(name);
            std::fs::write(&path, rlib(&metadata([svh; 16], &[0; 8]))).unwrap();
            path
        };
        let core = written(&library, "libcore-1a.rlib", 1);
        let alloc = written(&library, "liballoc-2b.rlib", 2);
        let std = written(&library, "libstd-3c.rlib", 3);
        let unwind = written(&library, "libpanic_unwind-4d.rlib", 4);
        written(&library, "libtest-5e.rlib", 5);
        std::fs::write(library.join("libstd-3c.so"), b"a dynamic std").unwrap();
        let dep = written(&deps, "libdep-6f.rlib", 6);

        let link_line = [dep, std, unwind, alloc, core];
        let mut ahead = library_identity(&library_in(&library));
        let mut linked = library_identity(&link_line);
        ahead.sort_unstable();
        linked.sort_unstable();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(ahead, linked);
        assert_eq!(ahead.iter().filter(|it| it.contains(".rlib=")).count(), LIBRARY.len());
    }

    /// A file of a package is in the source root of each of its targets, and a program reads
    /// it as a module of its own crates only; a file in none of them is read in all.
    #[test]
    fn a_file_shared_by_targets_is_read_within_the_crates_given() {
        use test_fixture::WithFixture;
        let (db, files) = RootDatabase::with_many_files(
            r#"
//- /src/main.rs crate:app
mod shared;
fn main() {}
//- /src/shared.rs
pub fn f() {}
//- /examples/other.rs crate:other
#[path = "../src/shared.rs"]
mod shared;
fn main() {}
"#,
        );
        let [main, shared, other] = [0, 1, 2].map(|it| files[it].file_id(&db));
        hir::attach_db(&db, || {
            let crates_of = |sema: &hir::Semantics<'_, RootDatabase>, file| {
                let modules = sema.file_to_module_defs(file);
                modules.map(|it| ide_db::base_db::Crate::from(it.krate(&db))).collect::<Vec<_>>()
            };
            let all = hir::Semantics::new(&db);
            let (app, example) = (crates_of(&all, main)[0], crates_of(&all, other)[0]);
            assert_eq!(crates_of(&all, shared), [app, example]);
            let within = |crates: &[_]| {
                hir::Semantics::new_within(&db, Arc::new(crates.iter().copied().collect()))
            };
            assert_eq!(crates_of(&within(&[example]), shared), [example]);
            assert_eq!(crates_of(&within(&[app]), shared), [app]);
            assert_eq!(crates_of(&within(&[example]), main), [app]);
        });
    }

    /// A crate names the dependencies its paths look up in the extern prelude, at the level of
    /// items, in bodies and in what macros expand to, and those it names with `extern crate`:
    /// not one a false `#[cfg]` leaves out, a local of the same name, or one never written.
    #[test]
    fn a_crate_names_the_dependencies_rustc_loads_for_it() {
        use test_fixture::WithFixture;
        let (db, files) = RootDatabase::with_many_files(
            r#"
//- /main.rs crate:main deps:imported,called,externed,mac,expanded,tokens,cfgd,value,unused
use imported::Item;
extern crate externed;
#[cfg(any())]
use cfgd as _;
mac::make!();
macro_rules! local {
    () => { tokens::T };
}
fn f() -> u32 {
    let value = called::g();
    value
}
//- /imported.rs crate:imported
pub struct Item;
//- /called.rs crate:called
pub fn g() -> u32 { 0 }
//- /externed.rs crate:externed
//- /mac.rs crate:mac
#[macro_export]
macro_rules! make {
    () => { pub use expanded::E; };
}
//- /expanded.rs crate:expanded
pub struct E;
//- /tokens.rs crate:tokens
//- /cfgd.rs crate:cfgd
//- /value.rs crate:value
//- /unused.rs crate:unused
"#,
        );
        let main = files[0].file_id(&db);
        hir::attach_db(&db, || {
            let sema = hir::Semantics::new(&db);
            let krate = sema.file_to_module_def(main).unwrap().krate(&db);
            let mut named: Vec<String> = hir::named_dependencies(&db, krate)
                .into_iter()
                .map(|it| it.display_name(&db).unwrap().to_string())
                .collect();
            named.sort();
            assert_eq!(named, ["called", "expanded", "externed", "imported", "mac", "tokens"]);
        });
    }

    /// What a build roots of a crate is what it always was: its functions with whether a runtime
    /// looks each up by name, but those only the const evaluator runs, and with whether each is
    /// generic, which a build with nothing of rustc's leaves out. They are listed with the
    /// crate, so an edit does not ask the attributes of every function of every crate again.
    #[test]
    fn the_roots_of_a_listed_crate_are_its_functions_but_those_only_the_const_evaluator_runs() {
        use test_fixture::WithFixture;
        let (db, file) = RootDatabase::with_single_file(
            r#"
fn plain() {}
fn generic<T>(_: T) {}
#[rustc_comptime]
fn comptime() {}
struct S;
impl S { fn method(&self) {} }
impl<T> Tr for T { fn provided(&self) {} }
trait Tr { fn required(&self); fn default(&self) { fn inner() {} } }
#[no_mangle]
extern "C" fn exported() {}
"#,
        );
        hir::attach_db(&db, || {
            let sema = hir::Semantics::new(&db);
            let module = sema.file_to_module_defs(file.file_id(&db)).next().unwrap();
            let listed = ListedCrate::new(&db, module.krate(&db));
            let name = |func: FunctionId| hir::Function::from(func).name(&db).as_str().to_owned();
            let before: Vec<(FunctionId, bool)> = listed
                .functions
                .iter()
                .copied()
                .zip(listed.exported.iter().copied())
                .filter(|&(it, _)| !hir_ty_codegen::compile_time_only(&db, it))
                .collect();
            let rooted: Vec<_> =
                listed.rooted.iter().map(|&(it, exported, _)| (it, exported)).collect();
            assert_eq!(rooted, before);
            assert!(listed.functions.iter().any(|&it| name(it) == "comptime"));
            assert!(listed.rooted.iter().all(|&(it, _, _)| name(it) != "comptime"));
            let flags: Vec<_> = listed
                .rooted
                .iter()
                .map(|&(it, exported, generic)| (name(it), exported, generic))
                .collect();
            for (wanted, exported, generic) in [
                ("plain", false, false),
                ("generic", false, true),
                ("method", false, false),
                ("provided", false, true),
                ("exported", true, false),
            ] {
                assert!(
                    flags.contains(&(wanted.to_owned(), exported, generic)),
                    "{wanted}: {flags:?}"
                );
            }
            assert!(listed.provided.iter().any(|&(it, generic)| name(it) == "inner" && !generic));
            for &(it, generic) in &listed.provided {
                assert_eq!(generic, is_generic_function(&db, it));
                assert!(!hir_ty_codegen::compile_time_only(&db, it));
            }
        });
    }

    /// The symbols of the standard library's object are read once for as long as the file is the
    /// one they were read from.
    #[test]
    fn the_symbols_of_an_object_are_read_again_only_when_the_file_changed() {
        use object::write::{Object, Symbol, SymbolSection};
        let dir = std::env::temp_dir().join(format!("blaze-defined-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("std.o");
        let write = |names: &[&str]| {
            let mut object = Object::new(
                object::BinaryFormat::Elf,
                object::Architecture::X86_64,
                object::Endianness::Little,
            );
            let section = object.section_id(object::write::StandardSection::Text);
            for name in names {
                let symbol = Symbol {
                    name: name.as_bytes().to_vec(),
                    value: 0,
                    size: 0,
                    kind: object::SymbolKind::Text,
                    scope: object::SymbolScope::Dynamic,
                    weak: false,
                    section: SymbolSection::Section(section),
                    flags: object::SymbolFlags::None,
                };
                object.add_symbol(symbol);
            }
            object.append_section_data(section, &[0x90], 1);
            std::fs::write(&path, object.write().unwrap()).unwrap();
        };
        write(&["one"]);
        let first = defined_symbols_kept(&path).unwrap();
        assert!(first.contains("one"));
        assert!(Arc::ptr_eq(&first, &defined_symbols_kept(&path).unwrap()));
        write(&["one", "another_and_longer"]);
        let second = defined_symbols_kept(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(second.contains("another_and_longer"));
        assert!(!Arc::ptr_eq(&first, &second));
    }

    /// A COFF object has no hidden symbols to tell apart from the others, and `object` calls
    /// the external ones `Linkage`, not `Dynamic`: they are what the object defines for others
    /// to link to, as an ELF object's default ones are.
    #[test]
    fn the_external_symbols_of_a_coff_object_are_the_ones_it_defines() {
        use object::write::{Object, Symbol, SymbolSection};
        let mut object = Object::new(
            object::BinaryFormat::Coff,
            object::Architecture::X86_64,
            object::Endianness::Little,
        );
        let section = object.section_id(object::write::StandardSection::Text);
        for (name, scope) in [
            ("external", object::SymbolScope::Linkage),
            ("local", object::SymbolScope::Compilation),
        ] {
            object.add_symbol(Symbol {
                name: name.as_bytes().to_vec(),
                value: 0,
                size: 0,
                kind: object::SymbolKind::Text,
                scope,
                weak: false,
                section: SymbolSection::Section(section),
                flags: object::SymbolFlags::None,
            });
        }
        object.append_section_data(section, &[0x90], 1);
        let path = std::env::temp_dir().join(format!("blaze-coff-{}.obj", std::process::id()));
        std::fs::write(&path, object.write().unwrap()).unwrap();
        let defined = defined_symbols(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(defined.contains("external"));
        assert!(!defined.contains("local"));
    }

    /// A `macro` names the modules of where it is defined, and a module private to the one it
    /// is in is not visible where the macro is used: std's `utf16!` writes
    /// `crate::sys::pal::windows::api::utf16_len` for `sys::path::windows`, from where `windows`
    /// is private. The path is looked up where the macro is defined.
    #[test]
    fn a_macro_names_the_private_modules_of_where_it_is_defined() {
        use test_fixture::WithFixture;
        let (db, file) = RootDatabase::with_single_file(
            r#"
#![feature(decl_macro)]
mod outer {
    mod private {
        pub const fn three() -> usize { 3 }
        pub macro size() { crate::outer::private::three() }
    }
    pub use self::private::size;
}
mod user {
    pub const N: usize = crate::outer::size!();
}
"#,
        );
        hir::attach_db(&db, || {
            let sema = hir::Semantics::new(&db);
            let root = sema.file_to_module_defs(file.file_id(&db)).next().unwrap();
            let user =
                root.children(&db).find(|it| it.name(&db).is_some_and(|it| it.as_str() == "user"));
            let hir::ModuleDef::Const(n) = user
                .unwrap()
                .declarations(&db)
                .into_iter()
                .find(|it| matches!(it, hir::ModuleDef::Const(_)))
                .unwrap()
            else {
                unreachable!()
            };
            let value = n.eval(&db).expect("the constant is evaluated");
            assert_eq!(value.render_debug(&db).unwrap(), "3");
        });
    }

    #[test]
    fn a_link_command_of_lto_has_no_core() {
        let ordinary = r#"cc "/t/deps/app-1.app.0.rcgu.o" "/sysroot/lib/libcore-667b96598095cbda.rlib" -o /t/app"#;
        let lto = r#"cc "/t/deps/app-1.app.0.rcgu.o" "/sysroot/lib/libcompiler_builtins-2ae2.rlib" -o /t/app"#;
        assert!(links_core(ordinary));
        assert!(links_core("cc @/t/link.args"));
        assert!(links_core("cc /sysroot/lib/libstd-44675f186029a4cd.so"));
        assert!(!links_core(lto));
    }

    #[test]
    fn a_link_that_failed_says_what_the_linker_said_last() {
        let said = "note: one\n\nld: cannot find -lmissing\n  collect2: error: ld returned 1\n";
        assert_eq!(
            last_lines(said, 2),
            "ld: cannot find -lmissing | collect2: error: ld returned 1"
        );
        assert_eq!(last_lines("", 2), "");
    }

    #[test]
    fn package_sources_can_be_named_like_installed_sources_and_build_outputs() {
        let ws = std::env::temp_dir().join("watch-paths");
        let before = std::mem::replace(
            &mut *lock(&UNEDITED),
            Unedited {
                installed: vec![
                    ws.join(".cargo/registry"),
                    ws.join("toolchain/lib/rustlib/src/rust/library"),
                ],
                targets: vec![ws.join("custom-output")],
            },
        );
        for source in [
            "src/registry/rule_set.rs",
            "src/rustlib/mod.rs",
            "src/target/opcode.rs",
            "custom-output/debug/build/pkg-hash/out/generated.rs",
        ] {
            assert!(!WatchedFile::immutable(&ws.join(source)), "{source}");
        }
        for cached in [
            ".cargo/registry/src/index/pkg/src/lib.rs",
            "toolchain/lib/rustlib/src/rust/library/core/src/lib.rs",
            "custom-output/debug/deps/old.rs",
        ] {
            assert!(WatchedFile::immutable(&ws.join(cached)), "{cached}");
        }
        *lock(&UNEDITED) = before;
    }

    #[test]
    fn a_file_written_after_it_was_loaded_is_taken_for_a_changed_one() {
        let dir = std::env::temp_dir().join(format!("blaze-loaded-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let read_from = SystemTime::now();
        let (mut db, mut vfs) = (RootDatabase::new(None), vfs::Vfs::default());
        let (mut change, mut root) =
            (ChangeWithProcMacros::default(), vfs::file_set::FileSet::default());
        for name in ["same.rs", "written.rs"] {
            let path = dir.join(name);
            std::fs::write(&path, "fn f() {}\n").unwrap();
            let at = vfs::VfsPath::from(AbsPathBuf::assert_utf8(path));
            vfs.set_file_contents(at.clone(), Some(b"fn f() {}\n".to_vec()));
            let id = vfs.file_id(&at).unwrap().0;
            root.insert(id, at);
            change.change_file(id, Some("fn f() {}\n".to_owned()));
        }
        change.set_roots(vec![ide_db::base_db::SourceRoot::new_local(root)]);
        db.apply_change(change);
        std::fs::write(dir.join("written.rs"), "fn f() { g() }\n").unwrap();
        let files = WatchedFile::sources(&vfs, &db, read_from);
        let changed = |name: &str| {
            let file = files.iter().find(|it| it.path.ends_with(name)).unwrap();
            file.mtime.is_none()
        };
        assert!(!changed("same.rs"));
        assert!(changed("written.rs"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A workspace given through a link (`/tmp` on macOS) has its files named through it, and
    /// rustc's dep-info names them where they are: a file written after rustc built it is
    /// the workspace's file all the same, and an error in it fails the build (#50).
    #[cfg(unix)]
    #[test]
    fn a_source_written_since_rustc_built_it_is_found_through_a_link() {
        let dir = std::env::temp_dir().join(format!("blaze-written-{}", std::process::id()));
        let (real, link) = (dir.join("real"), dir.join("link"));
        std::fs::create_dir_all(real.join("src")).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let (written, same) = (real.join("src/lib.rs"), real.join("src/same.rs"));
        let mut files = Vec::new();
        for (id, path) in [(0, &written), (1, &same)] {
            std::fs::write(path, "fn f() {}\n").unwrap();
            let watched = Watched::Source(vfs::FileId::from_raw(id));
            let named = link.join(path.strip_prefix(&real).unwrap());
            files.extend(WatchedFile::new(watched, &named, &named));
        }
        let found = written_sources(&files, [&*real_path(&written)]);
        let found: Vec<_> = found.into_iter().collect();
        assert_eq!(found, [(vfs::FileId::from_raw(0), link.join("src/lib.rs"))]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_lock_a_thread_panicked_with_is_still_taken() {
        let held = std::sync::Arc::new(std::sync::Mutex::new(1));
        let theirs = held.clone();
        let panicked = std::thread::spawn(move || {
            let _guard = lock(&theirs);
            panic!("unwinds as an interrupted build does");
        });
        assert!(panicked.join().is_err());
        assert!(held.is_poisoned());
        *lock(&held) += 1;
        assert_eq!(*lock(&held), 2);
    }

    /// A load of `cargo test`'s build has every crate of it, as one worker builds them all: a
    /// binary and a library each without `cfg(test)` and as their unit tests, the unit tests
    /// linking the library without, an example without `cfg(test)`, and only the integration
    /// test told `CARGO_BIN_EXE_<name>` and `CARGO_TARGET_TMPDIR`.
    #[cfg(unix)]
    #[test]
    fn a_load_of_the_tests_has_every_crate_cargo_test_builds() {
        use project_model::{
            CargoWorkspace, ManifestPath, ProjectWorkspace, ProjectWorkspaceKind, Sysroot,
            WorkspaceBuildScripts,
        };
        project_model::SYSROOT_AS_BUILT.store(true, std::sync::atomic::Ordering::Relaxed);
        let json = include_str!("../../project-model/test_data/hello-world-metadata.json");
        let meta: project_model::Metadata =
            serde_json::from_str(&json.replace("$ROOT$", "/ROOT/")).unwrap();
        let manifest = ManifestPath::try_from(AbsPathBuf::assert_utf8("/ROOT/".into())).unwrap();
        let mut cargo = CargoWorkspace::new(meta, manifest, Default::default(), false);
        cargo.set_executables(Some(AbsPathBuf::assert_utf8("/ROOT/target/debug".into())));
        cargo.set_scratch(Some(AbsPathBuf::assert_utf8("/ROOT/target/tmp".into())));
        let workspace = ProjectWorkspace {
            kind: ProjectWorkspaceKind::Cargo {
                cargo,
                build_scripts: WorkspaceBuildScripts::default(),
                rustc: Err(None),
                error: None,
            },
            cfg_overrides: Default::default(),
            sysroot: Sysroot::empty(),
            rustc_cfg: Vec::new(),
            toolchain: None,
            target: Err("no target data".into()),
            extra_includes: Vec::new(),
            set_test: true,
            const_eval: Default::default(),
        };
        let mut files = FxHashMap::default();
        let (graph, _) = workspace.to_crate_graph(
            &mut |path| {
                let next = ide_db::FileId::from_raw(files.len() as u32 + 1);
                Some(*files.entry(path.to_path_buf()).or_insert(next))
            },
            &Default::default(),
        );
        let test = cfg::CfgAtom::Flag(intern::sym::test);
        let of = |name: &str, kind: TargetKind| -> Vec<_> {
            graph
                .iter()
                .filter(|&it| graph[it].basic.target_kind == kind)
                .filter(|&it| {
                    let named = graph[it].extra.display_name.as_ref();
                    named.is_some_and(|it| it.canonical_name().as_str() == name)
                })
                .collect()
        };
        let lib = TargetKind::Lib { is_proc_macro: false };
        let tested = |it: ide_db::base_db::CrateBuilderId| graph[it].cfg_options.check_atom(&test);
        let told = |it: ide_db::base_db::CrateBuilderId, var: &str| graph[it].env.get(var);
        let libs = of("hello-world", lib);
        let bins = of("hello-world", TargetKind::Bin);
        let (examples, tests) = (of("an-example", TargetKind::Example), of("it", TargetKind::Test));
        assert_eq!((libs.len(), bins.len(), examples.len(), tests.len()), (2, 2, 1, 1));
        let library = *libs.iter().find(|&&it| !tested(it)).unwrap();
        for unit in [&libs, &bins] {
            assert_eq!(unit.iter().filter(|&&it| tested(it)).count(), 1, "{unit:?}");
        }
        for &bin in &bins {
            let deps = &graph[bin].basic.dependencies;
            assert!(deps.iter().any(|it| it.crate_id == library), "a binary links the library");
        }
        assert!(!tested(examples[0]) && tested(tests[0]));
        assert_eq!(
            told(tests[0], "CARGO_BIN_EXE_hello-world").as_deref(),
            Some("/ROOT/target/debug/hello-world")
        );
        assert_eq!(told(tests[0], "CARGO_TARGET_TMPDIR").as_deref(), Some("/ROOT/target/tmp"));
        for &it in libs.iter().chain(&bins).chain(&examples) {
            assert_eq!(told(it, "CARGO_BIN_EXE_hello-world"), None);
            assert_eq!(told(it, "CARGO_TARGET_TMPDIR"), None);
        }
    }
}
