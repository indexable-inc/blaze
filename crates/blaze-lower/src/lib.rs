//! A build's units lowered into the front end (docs/v2/plan/lanes/cargo/notes.md sections 2
//! and 6): base-db's crate graph, source roots and files, from `blaze_units::query::Units` and
//! the files of the view and the sources mounted beside it, read through the tree and never
//! from the disk.
//!
//! - **Crates**: one per unit that compiles (every mode but `RunCustomBuild`), so a lib's
//!   `Test` unit is a crate of its own with `cfg(test)`, and a package cargo builds for the host
//!   and for the target is two crates. Deps are the unit's, by `extern_crate_name`, in its
//!   order; cfgs are the target's (blaze's target table, never `rustc --print`) with what the
//!   profile, the rustflags and `UnitPlan::cfgs` decide; env is `UnitPlan::env`.
//! - **Files**: every file under each package's directory (and the directory of a target root
//!   outside it), by its absolute path as cargo names it, from the root of its longest mount.
//!   One source root per package, but the standard library's packages share one, the whole of
//!   `library/` (core reads `library/stdarch` and `library/portable-simd` by `#[path]`):
//!   library (`Durability::HIGH`) for the standard library, registry and git packages; local
//!   (`Durability::LOW`) for path packages, as rust-analyzer has them, so an edit of a member's
//!   file invalidates no query of std. A build script run's `OUT_DIR` is in its package's
//!   source root, so what it writes there is read by that package's crates and no other's.
//! - **Build script runs** are applied by [`Lowered::ran`]; a proc macro's built expanders by
//!   [`Lowered::macros`].
//! - **Another view** of the same units is applied by [`Lowered::refresh`]: what it read from
//!   the view read again and applied as a change, the database and its memos kept.
//! - **What a running proc macro asks** of the database (a span's text, file, line or parent)
//!   is answered by [`answer`], whichever runtime runs it.
//!
//! The database is shared by reference, never cloned (docs/v2/plan/process/decisions.md): a
//! task reads it through [`Lowered::db`] while it holds a shared reference to the
//! [`Lowered`], and a write takes `&mut Lowered`, which no reader can hold at the same time.

#![forbid(unsafe_code)]
#![feature(allocator_api)]

mod answer;
mod const_macros;
mod crates;
mod doctests;
mod files;
mod harness;
#[cfg(test)]
mod tests;

use std::{cell::Cell, collections::HashMap, fmt};
use std::alloc::Global;

use base_db::{FileSetConfig, Trigger};
use base_db::{
    AbsPathBuf, ConstEvalBudget, ContentId, Crate, CrateWorkspaceData, Env, FileId, FileSet,
    LibraryRoots, LocalRoots, SourceRoot, SourceRootId, VfsPath, ViewChange, ViewFile, ViewRoot,
};
use blaze_hash::HashSet;
use blaze_ids::{Hasher, radix};
use blaze_query::db::{Database as _, Durability, Setter};
use blaze_units::{
    query::{BuildConfig, PackagePlan, Units},
    schema::{Mode, Unit},
    stdlib::target::HostCpu,
};
use cfg::{CfgAtom, CfgOptions};
use hir_expand::proc_macro::ProcMacros;
use triomphe::Arc;

pub use base_db::{Db, ProcMacroLoadingError};
pub use hir_expand::proc_macro::{ProcMacro, ProcMacroLoadResult};

pub use self::answer::{SubRequest, SubResponse, answer};
pub use self::crates::{lint_flags, local, target_data};
pub use self::doctests::DoctestProgram;
pub use self::harness::{Appended, macros_loaded, wasm_macro};

/// What [`lower`] lowers.
pub struct Lower<'a, T> {
    pub units: &'a Units,
    pub config: &'a BuildConfig,
    /// The view's root and every mount's root, readable from any thread: the VFS's
    /// `blaze_vfs::Reader` (`&job.files`), or a `blaze_query::MemTree` in a test.
    pub files: &'a T,
    /// The view's root (what `Origin::View` mounts are of).
    pub root: ContentId,
    /// The machine's CPU, for a `-C target-cpu=native` in the rustflags; without it such a
    /// flag is an error naming it.
    pub host_cpu: Option<&'a HostCpu>,
}

/// The part of `now` a front end lowered from `kept` (with the same units) would read
/// differently, by name; `None` when [`Lowered::refresh`] can move it to `now`. A front end
/// reads the mounts, the host, the library's and the stores' paths and the checkout's root. It
/// never reads the command, its arguments, the working directory or cargo's config files:
/// those only make the plan, whose units the caller compares whole, so `build` then
/// `build --keep-going` or `-v` lowers into the same front end. Every field is named here, so a
/// field added to [`BuildConfig`] is a decision.
///
/// The process environment is not compared whole: a front end is keyed by the variables it
/// read, as it read them. Its crates' `env!` and `option_env!` reads are
/// [`Lowered::env_moved`]'s; what the daemon read of the environment into what it keeps with
/// the front end (its stores' modes) is the daemon's own (`blaze_units::query::EnvReads`).
/// Comparing it whole let a kept front end go on every variable a shell or systemd sets per
/// run (`SHLVL`, `OLDPWD`, `TERM_SESSION_ID`, `INVOCATION_ID`), so a no-op build lowered all
/// again: 86 s in place of about 1 s.
pub fn config_differs(kept: &BuildConfig, now: &BuildConfig) -> Option<String> {
    let BuildConfig { invocation, mounts, host, library, std, builds, sources } = kept;
    let blaze_units::query::Invocation { command: _, args: _, root, cwd: _, env: _, files: _ } = invocation;
    if mounts != &now.mounts {
        return Some("its mounts".to_owned()); // alloc: a reason, once per front end let go
    }
    if host != &now.host {
        return Some("its host".to_owned()); // alloc: a reason, once per front end let go
    }
    if library != &now.library {
        return Some("where its library is named".to_owned()); // alloc: a reason, once per front end let go
    }
    if std != &now.std {
        return Some("where its standard library's builds are placed".to_owned()); // alloc: a reason, once per front end let go
    }
    if builds != &now.builds {
        return Some("where its build directories are placed".to_owned()); // alloc: a reason, once per front end let go
    }
    if sources != &now.sources {
        return Some("where its fetched packages are kept".to_owned()); // alloc: a reason, once per front end let go
    }
    if root != &now.invocation.root {
        return Some("its checkout's root".to_owned()); // alloc: a reason, once per front end let go
    }
    None
}

/// A crate by the name it was given, for a person, or its id where it has none.
fn crate_display(db: &Db, krate: Crate) -> String {
    match &krate.extra_data(db).display_name {
        Some(name) => name.canonical_name().as_str().to_owned(), // alloc: a name, once per front end let go
        None => format!("{krate:?}"),                             // alloc: as above
    }
}

/// Why units could not be lowered, or a run applied: what, and the path or unit it is about.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LowerError {
    pub what: String,
    pub at: Option<String>,
}

impl LowerError {
    fn new(what: impl Into<String>) -> LowerError {
        LowerError { what: what.into(), at: None }
    }

    fn at(what: impl Into<String>, at: impl Into<String>) -> LowerError {
        LowerError { what: what.into(), at: Some(at.into()) }
    }
}

impl fmt::Display for LowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.at {
            Some(at) => write!(f, "{}\n  at {at}", self.what),
            None => f.write_str(&self.what),
        }
    }
}

impl std::error::Error for LowerError {}

/// What a build script's run gave: what [`Lowered::ran`] applies to the units depending on it.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Ran {
    /// As `--cfg` takes them (`foo`, `foo="bar"`).
    pub cfgs: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Absolute, as cargo names it.
    pub out_dir: String,
    /// What it left in `OUT_DIR`, by `/`-separated path under it.
    pub files: Vec<(String, Vec<u8>)>,
}

/// A build's units in the front end, owned by the daemon's loop. Tasks read it through a
/// shared reference ([`Lowered::db`], [`Lowered::crates`], [`Lowered::paths`]); its writes
/// ([`Lowered::ran`], [`Lowered::macros`]) take `&mut self`, so the loop makes one only once
/// no task holds a reference. Nothing in it is locked, and its database is never cloned.
pub struct Lowered {
    db: Db,
    crates: std::sync::Arc<Crates>,
    /// By unit: the runs applied to it, by the run's unit.
    applied: Vec<Vec<(usize, Applied)>>,
    /// Every file of the front end by its absolute path as cargo names it. A task may hold it
    /// past its read of the database (the code generator's names); a write changes it in
    /// place when no task does.
    paths: std::sync::Arc<HashMap<FileId, String>>,
    /// Each crate's identity worked out since the last write ([`Lowered::identity`]).
    identities: blaze_hash::HashMap<Crate, ContentId, Global>,
    /// Each root (an alias) [`Lowered::append_roots`] made since the last [`Lowered::refresh`],
    /// with the unit it was first made for and the content ID of the module it was given.
    appended: blaze_hash::HashMap<FileId, (usize, ContentId), Global>,
    /// Each directory read into the front end, as [`Lowered::refresh`] reads it again.
    loads: Vec<Load>,
    /// The root of the view its files were read from.
    view: ContentId,
    /// The workspace's root as that view has it, which every crate is made with.
    workspace_root: crates::Root,
    /// Each package's const macros, listed again by [`Lowered::refresh`] when their file
    /// changes.
    const_macros: Vec<const_macros::Loaded>,
    /// The crates of each `Doctest` unit's examples and runner ([`Lowered::doctests`]).
    doctests: Vec<doctests::OfUnit>,
}

/// A directory read into a package's source root.
struct Load {
    dir: String,
    set: usize,
    /// The paths under it that are another load's or the build's, not read.
    skipped: Vec<String>,
    built: Vec<AbsPathBuf>,
    /// Whether what it read came from the view, not only from mounts of fixed roots.
    view: bool,
}

/// What every task of a build reads and nothing writes once [`lower`] made it: each unit's
/// crate.
pub struct Crates {
    units: Vec<Lowering>,
    /// How many source roots of packages there are: `SourceRootId(0..sets)`, then the one of
    /// files of no package.
    sets: usize,
}

impl Crates {
    /// The front end's crate of unit `unit` (an index of `units.graph.units`); none for a
    /// `RunCustomBuild` unit.
    pub fn krate(&self, unit: usize) -> Option<Crate> {
        self.units.get(unit)?.krate
    }

    /// Whether unit `unit`'s crate is a proc macro's dylib, whose expanders
    /// [`Lowered::macros`] sets once it is built.
    pub fn is_proc_macro(&self, unit: usize) -> bool {
        self.units.get(unit).is_some_and(|it| it.proc_macro)
    }
}

/// One unit as lowered.
struct Lowering {
    krate: Option<Crate>,
    /// `foo v0.1.0 (lib)`, for errors.
    what: String,
    /// Its package's source root.
    set: usize,
    /// A `RunCustomBuild` unit's `OUT_DIR`.
    out_dir: Option<String>,
    /// The units depending on it directly.
    dependents: Vec<usize>,
    /// Its cfg and env before any run applied.
    cfg: CfgOptions,
    env: Vec<(String, String)>,
    /// Whether its crate is a proc macro's dylib (a `proc-macro` target built without `--test`).
    proc_macro: bool,
    /// For a unit built to run in the macro host, whether its crate links `proc_macro`, as its
    /// root read when lowered.
    macro_host: Option<MacroHost>,
}

/// What a macro-host crate's cfg was decided by ([`crates::host_side`]): its root, the root's
/// text when read and its edition, and whether it links `proc_macro`: always, for a proc
/// macro, whatever its root says.
struct MacroHost {
    root: FileId,
    content: ContentId,
    edition: span::Edition,
    proc_macro: bool,
    links: bool,
}

struct Applied {
    cfgs: Vec<CfgAtom>,
    env: Vec<(String, String)>,
    out_dir: String,
}

/// The crate graph, source roots and files of `lower.units` in a new database.
pub fn lower<T: blaze_query::Tree>(lower: Lower<'_, T>) -> Result<Lowered, LowerError> {
    lower_into(Db::new(), lower)
}

/// [`lower`] into `db`, a build's database just made and never written ([`Db::new`], or
/// [`Db::logged`] for a harness that counts what each request executes): every input written
/// as one change, one batch, so the maintainers run once over all of it. From then on the
/// database takes its writes only in batches ([`Db::require_batches`]): each later change of
/// the build ([`Lowered::ran`], [`Lowered::refresh`], ...) is one.
pub fn lower_into<T: blaze_query::Tree>(mut db: Db, lower: Lower<'_, T>) -> Result<Lowered, LowerError> {
    let setup = db.batch(Trigger::named("the build lowered"), |db| lower_writes(db, lower))?;
    db.require_batches();
    let Setup { unit_count, lowerings, sets, loads, view, workspace_root, const_macros } = setup;
    let paths = paths_of(&db, sets);
    Ok(Lowered {
        db,
        applied: (0..unit_count).map(|_| Vec::new()).collect(),
        crates: std::sync::Arc::new(Crates { units: lowerings, sets }),
        paths: std::sync::Arc::new(paths),
        identities: blaze_hash::HashMap::new_in(Global),
        appended: blaze_hash::HashMap::new_in(Global),
        loads,
        view,
        workspace_root,
        const_macros,
        doctests: Vec::new(),
    })
}

/// What [`lower_writes`] made beside the inputs it wrote.
struct Setup {
    unit_count: usize,
    lowerings: Vec<Lowering>,
    /// [`Crates::sets`].
    sets: usize,
    loads: Vec<Load>,
    view: ContentId,
    workspace_root: crates::Root,
    const_macros: Vec<const_macros::Loaded>,
}

/// The writes of [`lower_into`], in its batch: the crate graph, source roots and files of
/// `lower.units`, and what the database is told of the build.
fn lower_writes<T: blaze_query::Tree>(db: &mut Db, lower: Lower<'_, T>) -> Result<Setup, LowerError> {
    let Lower { units, config, files, root, host_cpu } = lower;
    let unit_count = units.graph.units.len();
    if units.plans.len() != unit_count {
        return Err(LowerError::new(format!(
            "the build has {unit_count} units and {} plans of them",
            units.plans.len()
        )));
    }
    let packages: blaze_hash::HashMap<&str, &PackagePlan, Global> =
        units.packages.iter().map(|it| (it.id.as_str(), it)).collect();
    let package_of = |unit: &Unit| {
        packages.get(unit.pkg_id.as_str()).copied().ok_or_else(|| {
            LowerError::at(
                format!("unit `{}` of package `{}` is of no package of the build", unit.target.name, unit.pkg_id),
                unit.pkg_id.clone(),
            )
        })
    };
    let mut whats = Vec::with_capacity(unit_count);
    for unit in &units.graph.units {
        whats.push(crates::describe(package_of(unit)?, unit));
    }
    let package_index: blaze_hash::HashMap<&str, usize, Global> =
        units.packages.iter().enumerate().map(|(index, it)| (it.id.as_str(), index)).collect();
    let mut const_plan = const_macros::plan(units, &package_index, &whats)?;

    // One source root per package with a unit, in the order of `units.packages`, but one for
    // every package of the standard library; each directory read is one root's, the innermost
    // when one is in another's.
    let mut used: blaze_hash::HashMap<&str, bool, Global> = blaze_hash::HashMap::new_in(Global);
    for unit in &units.graph.units {
        *used.entry(unit.pkg_id.as_str()).or_default() |= unit.is_std;
    }
    let mut library: Vec<bool> = Vec::new();
    let mut set_of: blaze_hash::HashMap<&str, usize, Global> = blaze_hash::HashMap::new_in(Global);
    let mut loads: blaze_hash::HashMap<String, usize, Global> = blaze_hash::HashMap::new_in(Global);
    let mut std_set = None;
    for package in &units.packages {
        let Some(&is_std) = used.get(package.id.as_str()) else { continue };
        let set = match (is_std, std_set) {
            (true, Some(set)) => set,
            (true, None) => {
                let set = library.len();
                library.push(true);
                loads.insert(config.library.trim_end_matches('/').to_owned(), set);
                std_set = Some(set);
                set
            }
            (false, _) => {
                let set = library.len();
                library.push(!crates::is_path(package));
                loads.entry(files::parent(&package.manifest_path).to_owned()).or_insert(set);
                set
            }
        };
        set_of.insert(package.id.as_str(), set);
    }
    // blaze's copy of `const-ast`, a library's root of its own, when a package has const macros.
    let const_ast_set = (!const_plan.is_empty()).then(|| {
        library.push(true);
        library.len() - 1
    });
    let mut excluded: HashSet<String, Global> = HashSet::new_in(Global);
    excluded.insert(units.target_dir.clone());
    excluded.insert(units.build_dir.clone());
    for (index, unit) in units.graph.units.iter().enumerate() {
        if unit.mode == Mode::RunCustomBuild {
            let out_dir = units.plans[index].out_dir.clone().ok_or_else(|| {
                LowerError::at("a build script's run has no `OUT_DIR`", whats[index].clone())
            })?;
            excluded.insert(out_dir);
            continue;
        }
        let src = crates::src_path(unit, &whats[index])?;
        let dir = files::parent(&src);
        if !loads.keys().any(|load| files::rest(dir, load).is_some()) {
            loads.insert(dir.to_owned(), set_of[unit.pkg_id.as_str()]);
        }
    }
    for group in &const_plan.groups {
        let dir = files::parent(&group.file);
        if !loads.keys().any(|load| files::rest(dir, load).is_some()) {
            loads.insert(dir.to_owned(), set_of[units.packages[group.package].id.as_str()]);
        }
    }
    // Walked by path: the order files get their ids in.
    let mut loads: Vec<(String, usize)> = loads.into_iter().collect();
    loads.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
    let mut excluded: Vec<String> = excluded.into_iter().collect();
    excluded.sort_unstable();

    let tree = files::Mounts { mounts: &config.mounts, view: root, tree: files, read_view: Cell::new(false) };
    ProcMacros::init_default(db, Durability::HIGH);
    // rustc compiles, checks and evaluates what a builtin derive expands to: the impls it
    // writes have bodies, which recording them without expanding (rust-analyzer's fast path)
    // leaves out.
    hir_def::nameres::set_expand_builtin_derives(db, true);
    for (index, &is_library) in library.iter().enumerate() {
        let source_root = match is_library {
            true => SourceRoot::new_library(FileSet::default()),
            false => SourceRoot::new_local(FileSet::default()),
        };
        let durability = if is_library { Durability::HIGH } else { Durability::LOW };
        db.set_source_root_with_durability(SourceRootId(index as u32), Arc::new(source_root), durability);
    }
    let other = SourceRootId(library.len() as u32);
    db.set_source_root_with_durability(other, Arc::new(SourceRoot::new_local(FileSet::default())), Durability::LOW);
    let roots = |library_too: bool| -> HashSet<SourceRootId, Global> {
        (0..=library.len())
            .filter(|&it| library.get(it).copied().unwrap_or(false) == library_too)
            .map(|it| SourceRootId(it as u32))
            .collect()
    };
    _ = LibraryRoots::builder(roots(true)).durability(Durability::MEDIUM).new(db);
    _ = LocalRoots::builder(roots(false)).durability(Durability::MEDIUM).new(db);

    let mut loaded = Vec::new();
    let mut read = Vec::with_capacity(loads.len());
    for (load, set) in &loads {
        let set = *set;
        // What is under another load, or written by the build, is not this one's.
        let skipped: Vec<String> = loads
            .iter()
            .map(|(it, _)| it)
            .chain(excluded.iter())
            .filter(|it| *it != load && files::rest(it, load).is_some())
            .cloned()
            .collect();
        loaded.clear();
        tree.read_view.set(false);
        tree.walk(load, &skipped, &mut loaded)?;
        let built: Vec<AbsPathBuf> = skipped.iter().map(|it| abs(it)).collect::<Result<_, _>>()?;
        set_files(db, load, &built, set, library.len(), &loaded)?;
        read.push(Load { dir: load.clone(), set, skipped, built, view: tree.read_view.get() });
    }

    let const_ast_dir = const_macros::const_ast_dir(&config.library);
    let const_ast_root = match const_ast_set {
        Some(set) => {
            let loaded = const_macros::const_ast_files(&const_ast_dir);
            set_files(db, &const_ast_dir, &[], set, library.len(), &loaded)?;
            let root = VfsPath::from(abs(&format!("{const_ast_dir}/lib.rs"))?);
            db.source_root(SourceRootId(set as u32)).source_root(&*db).file_for_path(&root).copied()
        }
        None => None,
    };

    let workspace_root = crates::root(units, &tree)?;
    let toolchain = crates::toolchain()?;
    // By triple and whether the crates are the standard library's.
    let mut workspaces: HashMap<(&str, bool), Arc<CrateWorkspaceData>> = HashMap::new();
    let mut lowerings: Vec<Lowering> = Vec::with_capacity(unit_count);
    for (index, unit) in units.graph.units.iter().enumerate() {
        let plan = &units.plans[index];
        let compiles = unit.mode != Mode::RunCustomBuild;
        lowerings.push(Lowering {
            krate: None,
            what: whats[index].clone(),
            set: set_of[unit.pkg_id.as_str()],
            out_dir: plan.out_dir.clone().filter(|_| !compiles),
            dependents: Vec::new(),
            cfg: CfgOptions::default(),
            env: Vec::new(),
            proc_macro: compiles && unit.target.is_proc_macro() && !crates::is_test(unit, plan),
            macro_host: None,
        });
    }
    for (index, unit) in units.graph.units.iter().enumerate() {
        for dep in &unit.dependencies {
            let Some(lowering) = lowerings.get_mut(dep.index) else {
                return Err(LowerError::at(
                    format!("a dependency `{}` on unit {}, which the build lacks", dep.extern_crate_name, dep.index),
                    whats[index].clone(),
                ));
            };
            if !lowering.dependents.contains(&index) {
                lowering.dependents.push(index);
            }
        }
    }

    let order = crates::topological(units, &whats, &const_plan.after)?;
    let mut all = Vec::with_capacity(order.len());
    // alloc: once a lowering, the crates made by their root file, for the units after to share.
    let mut made: blaze_hash::HashMap<FileId, Vec<Made>, Global> = blaze_hash::HashMap::new_in(Global);
    let mut const_listed = Vec::new();
    let mut const_made = Vec::new();
    for index in order {
        let unit = &units.graph.units[index];
        let plan = &units.plans[index];
        let what = &whats[index];
        let package = package_of(unit)?;
        let triple = unit.platform.as_deref().unwrap_or(&config.host);
        let workspace = match workspaces.get(&(triple, unit.is_std)) {
            Some(it) => it.clone(),
            None => {
                // The workspace's `const-eval` budget is for its own crates: the standard
                // library's are evaluated under blaze's, the same in every workspace.
                let const_eval = if unit.is_std { ConstEvalBudget::default() } else { workspace_root.const_eval };
                let made = crates::workspace_data(triple, &toolchain, const_eval)?;
                workspaces.insert((triple, unit.is_std), made.clone());
                made
            }
        };
        let mut cfg = crates::cfg(unit, plan, triple, host_cpu, what)?;
        if const_plan.group_of[index].is_some() {
            cfg.insert_atom(intern::Symbol::intern(const_macros::CFG));
        }
        let durability = if library[set_of[unit.pkg_id.as_str()]] { Durability::HIGH } else { Durability::MEDIUM };
        let macros = match const_plan.group_of[index] {
            None => None,
            Some(group) => {
                let file = const_plan.groups[group].file.clone();
                let name = const_macros::dependency_name(&file)?;
                let krate = match const_plan.groups[group].krate {
                    Some(krate) => krate,
                    None => {
                        let file_path = VfsPath::from(abs(&file)?);
                        let file_set = loads
                            .iter()
                            .filter(|(load, _)| files::rest(&file, load).is_some())
                            .max_by_key(|(load, _)| load.len())
                            .map_or(other, |(_, set)| SourceRootId(*set as u32));
                        let macros_root = db.source_root(file_set).source_root(&*db).file_for_path(&file_path).copied();
                        let cx = const_macros::Cx {
                            units,
                            whats: &whats,
                            lowerings: &lowerings,
                            workspace: workspace.clone(),
                            triple,
                            host_cpu,
                            root: macros_root,
                            const_ast_root,
                            const_ast_dir: &const_ast_dir,
                            proc_macro_cwd: abs(&crates::proc_macro_cwd(&workspace_root, package))?,
                            durability,
                        };
                        let (made, listed) = const_macros::make(db, &mut const_plan, group, cx, &mut all)?;
                        let krate = made.krate;
                        const_listed.push((krate, listed));
                        const_made.push(made);
                        krate
                    }
                };
                Some((name, krate))
            }
        };
        let src = crates::src_path(unit, what)?;
        let src_path = VfsPath::from(abs(&src)?);
        let set = loads
            .iter()
            .filter(|(load, _)| files::rest(&src, load).is_some())
            .max_by_key(|(load, _)| load.len())
            .map_or(other, |(_, set)| SourceRootId(*set as u32));
        let root_file = db.source_root(set).source_root(&*db).file_for_path(&src_path).copied();
        let Some(root_file) = root_file else {
            return Err(LowerError::at(
                format!("the root file `{src}` of {what} is not among its package's files"),
                src,
            ));
        };
        // A crate of the macro host that links `proc_macro` is told of the host's side, as
        // rustc builds it for the host (`crates::host_side`). Decided from the root's text,
        // which `Lowered::refresh` reads again.
        let macro_host = match plan.macro_wasm {
            true => {
                let read: &Db = &*db;
                let text = read.file_text(root_file);
                let (edition, proc_macro) = (crates::edition(unit, what)?, unit.target.is_proc_macro());
                let links = crates::links_proc_macro(proc_macro, &**text.text(read), edition);
                if links {
                    crates::host_side(&mut cfg);
                }
                Some(MacroHost { root: root_file, content: text.content(read), edition, proc_macro, links })
            }
            false => None,
        };
        let mut dependencies = Vec::with_capacity(unit.dependencies.len());
        for dep in &unit.dependencies {
            let on = &units.graph.units[dep.index];
            if on.mode == Mode::RunCustomBuild {
                continue;
            }
            let Some(krate) = lowerings[dep.index].krate else {
                return Err(LowerError::at(
                    format!("{what} depends on {}, which has no crate", whats[dep.index]),
                    what.clone(),
                ));
            };
            dependencies.push(crates::dependency(unit, on, dep, krate, what)?);
        }
        if let Some((name, krate)) = macros {
            dependencies.push(base_db::BuiltDependency::with_prelude(name, krate, true, false));
        }
        let data = base_db::BuiltCrateData {
            root_file_id: root_file,
            edition: crates::edition(unit, what)?,
            dependencies,
            origin: crates::origin(package, unit),
            crate_attrs: Box::new([]),
            target_kind: crates::target_kind(unit, plan, what)?,
            manifest_kind: crates::manifest_kind(unit, what)?,
            crate_types: crates::crate_types(unit, plan, what)?,
            proc_macro_cwd: Arc::new(abs(&crates::proc_macro_cwd(&workspace_root, package))?),
        };
        let extra = base_db::ExtraCrateData {
            version: Some(package.version.clone()),
            display_name: Some(base_db::CrateDisplayName::from_canonical_name(&unit.target.name)),
            potential_cfg_options: None,
            built_for: crates::built_for(unit.built_for()),
        };
        let env: Env = plan.env.iter().cloned().collect();
        let runs: Vec<usize> = unit
            .dependencies
            .iter()
            .filter(|dep| units.graph.units[dep.index].mode == Mode::RunCustomBuild)
            .map(|dep| dep.index)
            .collect();
        // A crate whose root blaze appends a module to ([`Lowered::append_roots`]: a test's
        // harness, a wasm proc macro's list of macros and bridge glue) has a root of its own,
        // an alias of the file on disk, so no other crate of that file reads the module.
        let aliased = (crates::is_test(unit, plan) && plan.harness) || wasm_macro(unit, plan);
        // Units cargo keeps apart for what the front end does not read (a profile's opt-level
        // or debuginfo) are one crate: two crates of one key would name each other's items.
        let made_of_root = made.entry(root_file).or_default();
        let same = made_of_root.iter().find(|it| {
            it.data == data
                && it.aliased == aliased
                && it.extra == extra
                && Arc::ptr_eq(&it.workspace, &workspace)
                && it.cfg == cfg
                && it.env == env
                && it.durability == durability
                && it.runs == runs
        });
        let krate = match same {
            Some(it) => it.krate,
            None => {
                // clone: once per crate made, kept to compare the units after with
                let mut made_of = data.clone();
                if aliased {
                    made_of.root_file_id = db.alias(root_file).map_err(|error| {
                        LowerError::at(format!("the root file `{src}` of {what} has no root of its own: {error}"), src.clone())
                    })?;
                }
                // clone: as `made_of`
                let krate = Crate::create(db, made_of, extra.clone(), workspace.clone(), cfg.clone(), env.clone(), durability);
                made_of_root.push(Made { data, aliased, extra, workspace, cfg: cfg.clone(), env, durability, runs, krate });
                all.push(krate);
                krate
            }
        };
        let lowering = &mut lowerings[index];
        lowering.krate = Some(krate);
        lowering.cfg = cfg;
        lowering.macro_host = macro_host;
        lowering.env = plan.env.clone();
    }
    base_db::set_all_crates_with_durability(db, all, Durability::MEDIUM);

    let not_built = lowerings
        .iter()
        .filter(|it| it.proc_macro)
        .filter_map(|it| it.krate)
        .map(|krate| (krate, Err(base_db::ProcMacroLoadingError::NotYetBuilt)));
    ProcMacros::give(db, not_built);
    const_macros::set_macros(db, const_listed);
    base_db::set_process_env(db, config.invocation.env.iter().cloned().collect());
    name_sources(db, workspace_root.dir.as_deref(), &config.library);

    Ok(Setup {
        unit_count,
        lowerings,
        sets: library.len(),
        loads: read,
        view: root,
        workspace_root,
        const_macros: const_made,
    })
}

/// What a crate of [`lower_into`] was made of, and the build scripts' runs its unit is given:
/// a later unit of all the same is lowered to it.
struct Made {
    /// With the root file on disk, which an aliased crate's root is an alias of.
    data: base_db::BuiltCrateData,
    /// Whether its root is an alias of the file on disk, its own.
    aliased: bool,
    extra: base_db::ExtraCrateData,
    workspace: Arc<CrateWorkspaceData>,
    cfg: CfgOptions,
    env: Env,
    durability: Durability,
    runs: Vec<usize>,
    krate: Crate,
}

impl Lowered {
    /// [`Crates::krate`].
    pub fn krate(&self, unit: usize) -> Option<Crate> {
        self.crates.krate(unit)
    }

    /// Runs `f`'s writes as one change, `trigger` ([`Db::batch`]): the maintainers run once
    /// when it ends, over everything it wrote. A write of `f` that is itself a change
    /// ([`Lowered::ran`], [`Lowered::macros`], ...) is part of this one. Its database takes
    /// writes only in batches, so a change made of several writes is one of these.
    pub fn batch<R>(&mut self, trigger: Trigger<'_>, f: impl FnOnce(&mut Lowered) -> R) -> R {
        Db::batch_in(self, |lowered| &mut lowered.db, trigger, f)
    }

    /// `foo v0.1.0 (lib)`: unit `unit` as an error or a profile names it; empty for a unit the
    /// build lacks.
    pub fn what(&self, unit: usize) -> &str {
        self.crates.units.get(unit).map_or("", |it| it.what.as_str())
    }

    /// What every task reads: each unit's crate. Made once by [`lower`] and never written.
    pub fn crates(&self) -> &std::sync::Arc<Crates> {
        &self.crates
    }

    /// The database, for a read through the shared reference a task holds: every front-end
    /// query takes `&Db`, and each a task asks runs with its own query stack.
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Every file of the front end by its absolute path as cargo names it, with every run
    /// applied so far.
    pub fn paths(&self) -> &std::sync::Arc<HashMap<FileId, String>> {
        &self.paths
    }

    /// Whether [`Lowered::ran`] would take `ran` as the run of unit `run`: a build script's run
    /// of this build, its `OUT_DIR` the one cargo names, every cfg as `--cfg` takes it. Asked
    /// as the run ends, before its write waits for the readers to finish.
    pub fn check_ran(&self, run: usize, ran: &Ran) -> Result<(), LowerError> {
        checked(&self.crates, run, ran).map(drop)
    }

    /// Unit `unit`'s identity in the front end: of its crate (its root file's path, data,
    /// cfgs, env, edition, target), its root's text and appended module (an alias's, which is
    /// not among its source root's files), the files of its source root by path and content (an
    /// applied run's `OUT_DIR` among them), and each crate it depends on by its identity, so the
    /// identity of a crate covers everything it is made from before it is resolved. None for a
    /// unit with no crate. Worked out once per crate between writes.
    ///
    /// The process's environment is not in it: a crate reads a variable of it only through
    /// `env!` and `option_env!`, which only resolving finds, and each read is noted in the
    /// database's `env!` log (`Files::env_read`). Whoever files a value made from the crate
    /// keys it by those reads and their values, as `blaze-cargo`'s unit records do; hashing the
    /// whole environment here made every record miss whenever any variable changed, such as
    /// a per-run `BLAZE_PROFILE`.
    pub fn identity(&mut self, unit: usize) -> Option<ContentId> {
        let krate = self.crates.krate(unit)?;
        Some(identity_of(&self.db, krate, &mut self.identities))
    }

    /// The first variable of the process that a crate of the front end read through `env!` or
    /// `option_env!` ([`base_db::process_env_reads`], sorted by crate and name) whose value in
    /// `now`, a request's environment, is not the one the database holds, with the crate that
    /// read it: why the front end does not take a build for `now`. `None` whatever else of
    /// `now` differs, and [`Lowered::refresh`] then sets `now` as the process's variables, which
    /// runs no maintainer.
    pub fn env_moved(&self, now: &[(String, String)]) -> Option<blaze_units::query::EnvMoved> {
        let db = &self.db;
        base_db::process_env_reads(db).into_iter().find_map(|(krate, name)| {
            let was = base_db::process_env_var(db, &name);
            let value = blaze_units::query::env_var(now, &name);
            if was.as_deref() == value {
                return None;
            }
            let by = match self.crates.units.iter().find(|it| it.krate == Some(krate)) {
                // alloc: a reason, once per front end let go
                Some(lowering) => format!("`env!` or `option_env!` in {}", lowering.what),
                // alloc: as above; a crate of no unit (a const macro's)
                None => format!("`env!` or `option_env!` in the crate {}", crate_display(db, krate)),
            };
            Some(blaze_units::query::EnvMoved { name, by, was, now: value.map(str::to_owned) })
        })
    }

    /// The run of build script unit `run` applied to every unit that depends on it directly:
    /// its `rustc-cfg`s added to their cfgs, its `rustc-env`s to their env, `OUT_DIR` set, and
    /// the files it left in `OUT_DIR` put into the front end's files (its package's source
    /// root), so `include!` and `env!("OUT_DIR")` read them. Applying a run again replaces
    /// what it applied before, files it no longer left included.
    ///
    /// A write: `&mut self` means no task holds a reference to read through, so nothing is
    /// cancelled or waited for here; tasks that read after it read the run applied. One change:
    /// its files, cfgs and envs are one batch, and inside [`Lowered::batch`] part of that one.
    pub fn ran(&mut self, run: usize, ran: &Ran) -> Result<(), LowerError> {
        let crates = self.crates.clone();
        let (lowering, cfgs) = checked(&crates, run, ran)?;
        let trigger = Trigger::of("a build script's run applied", &lowering.what);
        self.batch(trigger, |this| this.apply_run(run, ran, &crates, lowering, cfgs))
    }

    /// [`Lowered::ran`]'s writes, in its batch: `run` checked against `crates`, its unit
    /// `lowering`, its cfgs `cfgs`.
    fn apply_run(
        &mut self,
        run: usize,
        ran: &Ran,
        crates: &Crates,
        lowering: &Lowering,
        cfgs: Vec<CfgAtom>,
    ) -> Result<(), LowerError> {
        let out_dir = &ran.out_dir;
        let mut loaded = Vec::with_capacity(ran.files.len());
        for (path, bytes) in &ran.files {
            let id = ContentId::of(blaze_vfs::FILE_DOMAIN, bytes);
            let bytes = blaze_vfs::Bytes::from(std::sync::Arc::<[u8]>::from(bytes.as_slice()));
            loaded.push(files::Loaded { path: format!("{out_dir}/{path}"), id, bytes });
        }

        self.identities.clear();
        let db = &mut self.db;
        let change = set_files(db, out_dir, &[], lowering.set, crates.sets, &loaded)?;
        note(&mut self.paths, db, lowering.set, &change);
        for &dependent in &lowering.dependents {
            let Some(krate) = crates.units[dependent].krate else { continue };
            let runs = &mut self.applied[dependent];
            runs.retain(|(it, _)| *it != run);
            let applying = Applied { cfgs: cfgs.clone(), env: ran.env.clone(), out_dir: out_dir.clone() };
            runs.push((run, applying));
            radix::sort_by_key(runs, |(it, _)| *it as u64);
            let base = &crates.units[dependent];
            let mut cfg = base.cfg.clone();
            let mut env: Env = base.env.iter().cloned().collect();
            for (_, applying) in runs.iter() {
                cfg.extend(applying.cfgs.iter().cloned());
                env.extend(applying.env.iter().cloned());
                env.set("OUT_DIR", applying.out_dir.clone());
            }
            if *krate.cfg_options(&*db) != cfg {
                krate.set_cfg_options(db, cfg);
            }
            if *krate.env(&*db) != env {
                krate.set_env(db, env);
            }
        }
        Ok(())
    }

    /// The front end moved to the view `lower` names, `lower`'s units and config being the
    /// ones it was lowered from (the caller compares them): each directory whose files were
    /// read from the view read again and applied as the change it is (O(its files), the
    /// mounts' fixed roots not read again), so the database's memos stay and only what the
    /// change reaches runs again. The runs and proc macros applied stay until applied again.
    /// `Ok(false)`, nothing changed, when the view moves the workspace's root or what its
    /// manifest sets, which every crate is made with, or whether a crate of the macro host
    /// links `proc_macro` ([`MacroHost`]), which its cfg is made with: it is to be [`lower`]ed
    /// anew. In the last case the view's files are already written, to a front end let go.
    ///
    /// `seed` runs in the same write once the new texts are set, before the write's
    /// maintainers parse them: where an edit's trees are handed over
    /// ([`base_db::EditionedFileId::seed_parse`]), as after it each file has its parse. It does
    /// not run when the view did not move (nothing is written, so nothing is parsed again) or
    /// on `Ok(false)`.
    ///
    /// A write, as [`Lowered::ran`] is.
    pub fn refresh<T: blaze_query::Tree>(
        &mut self,
        lower: Lower<'_, T>,
        seed: impl FnOnce(&Db) -> Result<(), LowerError>,
    ) -> Result<bool, LowerError> {
        let Lower { units, config, files, root, .. } = lower;
        // The request's variables, which the caller checked moved none a crate read
        // ([`Lowered::env_moved`]): a later read reads them, and no maintainer runs. Its own
        // batch: the database takes writes only in batches, and this one notes nothing.
        let vars: Env = config.invocation.env.iter().cloned().collect();
        self.db.batch(Trigger::named("the process environment set"), |db| base_db::set_process_env(db, vars));
        // A new job: its units' roots are appended to again, and may be made other than before.
        self.appended.clear();
        if root == self.view {
            return Ok(true);
        }
        let tree = files::Mounts { mounts: &config.mounts, view: root, tree: files, read_view: Cell::new(false) };
        if crates::root(units, &tree)? != self.workspace_root {
            return Ok(false);
        }
        self.identities.clear();
        self.db.admit_parses();
        // One change: the view's files, the seeds and the const macros they list, one batch.
        self.batch(Trigger::named("the view moved"), |this| this.refresh_writes(&tree, root, seed))?;
        // A crate of the macro host whose root now links `proc_macro` or no longer does is
        // made with another cfg (`crates::host_side`): lowered anew, as a moved root is.
        Ok(!self.macro_host_moved())
    }

    /// Whether a macro-host crate's root, read again, links `proc_macro` other than it did
    /// when lowered ([`MacroHost`]).
    fn macro_host_moved(&self) -> bool {
        let db = &self.db;
        self.crates.units.iter().filter_map(|it| it.macro_host.as_ref()).any(|host| {
            let text = db.file_text(host.root);
            !host.proc_macro
                && text.content(db) != host.content
                && crates::links_proc_macro(false, &**text.text(db), host.edition) != host.links
        })
    }

    /// [`Lowered::refresh`]'s writes, in its batch: the view `root` read through `tree`.
    fn refresh_writes<T: blaze_query::Tree>(
        &mut self,
        tree: &files::Mounts<'_, T>,
        root: ContentId,
        seed: impl FnOnce(&Db) -> Result<(), LowerError>,
    ) -> Result<(), LowerError> {
        let sets = self.crates.sets;
        let mut loaded = Vec::new();
        let mut removed = false;
        let (loads, paths, db) = (&mut self.loads, &mut self.paths, &mut self.db);
        for load in loads.iter_mut() {
            if !load.view {
                continue;
            }
            loaded.clear();
            tree.read_view.set(false);
            tree.walk(&load.dir, &load.skipped, &mut loaded)?;
            load.view = tree.read_view.get();
            let change = set_files(db, &load.dir, &load.built, load.set, sets, &loaded)?;
            removed |= !change.removed.is_empty();
            note(paths, db, load.set, &change);
        }
        seed(db)?;
        self.view = root;
        if removed {
            for lowering in &self.crates.units {
                let Some(krate) = lowering.krate else { continue };
                let file = krate.data(&self.db).root_file_id;
                if !self.paths.contains_key(&file) {
                    return Err(LowerError::at(
                        "the root file of the crate is gone from its package's files",
                        lowering.what.clone(),
                    ));
                }
            }
        }
        let mut listed = Vec::new();
        for made in &mut self.const_macros {
            if let Some(macros) = made.reload(&self.db, &self.paths)? {
                listed.push((made.krate, macros));
            }
        }
        const_macros::set_macros(&mut self.db, listed);
        Ok(())
    }

    /// The front end's tables by owner and name: how many each holds and their bytes, as
    /// the query engine counts them. It takes `&mut self`, as a write does: the counts are of a
    /// database no reader is adding to.
    pub fn tables(&mut self) -> Vec<(&'static str, u64, u64)> {
        tables(&mut self.db)
    }

    /// Whether [`Lowered::macros`] would take expanders for unit `unit`: a proc macro's crate.
    pub fn check_macros(&self, unit: usize) -> Result<(), LowerError> {
        let Some(lowering) = self.crates.units.get(unit) else {
            return Err(LowerError::new(format!("the build has no unit {unit}")));
        };
        match (lowering.krate, lowering.proc_macro) {
            (Some(_), true) => Ok(()),
            _ => Err(LowerError::at("not a proc macro's crate", lowering.what.clone())),
        }
    }

    /// The expanders of proc-macro unit `unit` once it is built (or why it has none), for the
    /// crates depending on it to expand its macros with. Until then they are
    /// `ProcMacroLoadingError::NotYetBuilt`. A write, as [`Lowered::ran`] is.
    pub fn macros(&mut self, unit: usize, macros: ProcMacroLoadResult) -> Result<(), LowerError> {
        self.check_macros(unit)?;
        let Some(krate) = self.crates.krate(unit) else {
            unreachable!("checked: a proc macro's unit has a crate")
        };
        let macros = macros.map(|mut macros| {
            macros.sort_unstable_by(|a, b| a.name.cmp_order(&b.name).then_with(|| a.kind.cmp(&b.kind)));
            macros
        });
        self.identities.clear();
        let trigger = Trigger::of("a proc macro's expanders loaded", &self.crates.units[unit].what);
        Db::batch_in(&mut self.db, |db| db, trigger, |db| ProcMacros::give(db, [(krate, macros)]));
        Ok(())
    }

    /// `crates` and their dependencies that are dependencies of the build
    /// ([`hir_def::deps::eligible`]) read from the images `images` keeps, by `build`, the blaze
    /// reading them ([`hir_def::deps::serve`]): under [`hir_def::deps::Mode::Serve`] an image
    /// replaces resolving its crate. Only crates of units every run of whose closure is applied
    /// may be named; one depending on a proc macro not built yet is not eligible. A crate served
    /// before whose key a later write moved (a run applied again with other cfgs, env or
    /// `OUT_DIR` files, a proc macro built into other code) is fetched again under its new key,
    /// and loses its image when there is none, so its caller serves again after every such
    /// write. An image that does not read is an error naming its crate. A write, as
    /// [`Lowered::ran`] is.
    pub fn serve_deps(
        &mut self,
        crates: &[Crate],
        build: ContentId,
        // dyn: the driver's store, one call per crate per front end
        images: &dyn hir_def::deps::ImageStore,
        mode: hir_def::deps::Mode,
    ) -> Result<hir_def::deps::Served, LowerError> {
        let trigger = Trigger::named("the dependencies' images served");
        self.db.batch(trigger, |db| hir_def::deps::serve(db, crates, build, images, mode)).map_err(LowerError::new)
    }

    /// The images of the crates `served` keyed and found none of, written to `images` from
    /// what this front end resolves them to ([`hir_def::deps::store`]), with each crate refused
    /// and why.
    pub fn store_deps(
        &self,
        served: &hir_def::deps::Served,
        // dyn: as in `serve_deps`
        images: &dyn hir_def::deps::ImageStore,
    ) -> Result<hir_def::deps::Stored, LowerError> {
        hir_def::deps::store(&self.db, served, images).map_err(LowerError::new)
    }

    /// The database to give what else the store served for the crates [`Lowered::serve_deps`]
    /// served (their checked bodies, their constants' values), right after it: a write, as
    /// [`Lowered::ran`] is.
    pub fn served_db(&mut self) -> &mut Db {
        &mut self.db
    }
}

/// Unit `run` as a build script's run that `ran` is of, and `ran`'s cfgs parsed.
fn checked<'a>(crates: &'a Crates, run: usize, ran: &Ran) -> Result<(&'a Lowering, Vec<CfgAtom>), LowerError> {
    let Some(lowering) = crates.units.get(run) else {
        return Err(LowerError::new(format!("the build has no unit {run}")));
    };
    let Some(out_dir) = &lowering.out_dir else {
        return Err(LowerError::at("not a build script's run", lowering.what.clone()));
    };
    if *out_dir != ran.out_dir {
        return Err(LowerError::at(
            format!("the run's `OUT_DIR` is `{}`, and cargo names it `{out_dir}`", ran.out_dir),
            lowering.what.clone(),
        ));
    }
    let cfgs = ran
        .cfgs
        .iter()
        .map(|it| crates::atom(it).map_err(|what| LowerError::at(what, lowering.what.clone())))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((lowering, cfgs))
}

const IDENTITY: &str = "blaze v2 cargo unit identity 4";

/// [`Lowered::identity`] of `krate`, each crate of its closure worked out once into `known`,
/// dependencies first, with a stack of its own rather than the thread's.
fn identity_of(db: &Db, krate: Crate, known: &mut blaze_hash::HashMap<Crate, ContentId, Global>) -> ContentId {
    if let Some(identity) = known.get(&krate) {
        return *identity;
    }
    let mut stack = vec![(krate, false)];
    while let Some((at, deps_known)) = stack.pop() {
        if known.contains_key(&at) {
            continue;
        }
        let data = at.data(db);
        if !deps_known {
            stack.push((at, true));
            stack.extend(data.dependencies.iter().filter(|it| !known.contains_key(&it.crate_id)).map(|it| (it.crate_id, false)));
            continue;
        }
        let root = db.file_source_root(data.root_file_id).source_root_id(db);
        let mut hasher = Hasher::new(IDENTITY);
        hasher.id(&at.content(db)).id(&db.file_text(data.root_file_id).content(db)).id(&db.source_root(root).content(db));
        hasher.update(&(data.dependencies.len() as u64).to_le_bytes());
        for dep in &data.dependencies {
            hasher.id(&known[&dep.crate_id]);
        }
        known.insert(at, hasher.finish());
    }
    known[&krate]
}

/// Tells the evaluator of constants what the build calls each file (`file!()` in a constant's
/// value, such as tracing's callsite statics), as cargo has rustc call it: a file of the
/// standard library (under `library`) by its path as it is, one under the workspace's root
/// `workspace` by its path from there, any other by its absolute path. The key names both
/// directories, so telling it again for the same workspace evaluates nothing again.
fn name_sources(db: &mut Db, workspace: Option<&str>, library: &str) {
    let library = format!("{}/", library.trim_end_matches('/'));
    let workspace = workspace.map(|it| format!("{}/", it.trim_end_matches('/')));
    let key = format!("{}\0{library}", workspace.as_deref().unwrap_or(""));
    hir_ty_mir::mir::location::name_sources(db, &key, move |db, file| {
        let root = db.file_source_root(file).source_root_id(db);
        let root = db.source_root(root).source_root(db);
        let path = root.path_for_file(&file)?.as_path()?.as_str();
        if path.starts_with(&library) {
            return Some(path.to_owned());
        }
        let relative = workspace.as_deref().and_then(|it| path.strip_prefix(it));
        Some(relative.unwrap_or(path).to_owned())
    });
}

/// The front end's tables, each of the database's structs and memoized queries by its name,
/// with how many rows it holds and their bytes (the rows and what their values hold), as
/// [`blaze_query::db::Storage::memory_usage`] counts them, with the database exclusive.
fn tables(db: &mut Db) -> Vec<(&'static str, u64, u64)> {
    // alloc: a report, off any build's path
    let mut usage = Vec::new();
    db.storage().memory_usage(&mut usage);
    usage.into_iter().map(|it| (it.name, it.rows, it.row_bytes + it.value_bytes)).collect()
}

/// `change` of source root `set` made in `paths` too.
fn note(paths: &mut std::sync::Arc<HashMap<FileId, String>>, db: &Db, set: usize, change: &ViewChange) {
    if change.added.is_empty() && change.removed.is_empty() {
        return;
    }
    // No task holds the map now, so this edits it in place.
    let map = std::sync::Arc::make_mut(paths);
    for id in &change.removed {
        map.remove(id);
    }
    let root = db.source_root(SourceRootId(set as u32)).source_root(db);
    for id in &change.added {
        if let Some(path) = root.path_for_file(id).and_then(VfsPath::as_path) {
            map.insert(*id, path.as_str().to_owned());
        }
    }
}

/// Every file of the source roots `0..=sets` by its absolute path, and each alias of one by
/// the path of the file it is an alias of.
fn paths_of(db: &Db, sets: usize) -> HashMap<FileId, String> {
    let mut paths = HashMap::new();
    for set in 0..=sets {
        let root = db.source_root(SourceRootId(set as u32)).source_root(db);
        for file in root.iter().chain(root.aliases().map(|(alias, _)| alias)) {
            if let Some(path) = root.path_for_file(&file).and_then(VfsPath::as_path) {
                paths.insert(file, path.as_str().to_owned());
            }
        }
    }
    paths
}

/// Puts `loaded`, the files under `base`, into the source root `set` of the `sets` packages'.
/// Files under `base` it lacks and none of `built` has are taken out.
fn set_files(
    db: &mut Db,
    base: &str,
    built: &[AbsPathBuf],
    set: usize,
    sets: usize,
    loaded: &[files::Loaded],
) -> Result<ViewChange, LowerError> {
    let base_path = abs(base)?;
    // Every file of `loaded` is under `base`, so the one prefix puts each in `set`: a partition
    // of every package's prefixes would put `/w/ab/x.rs` in `/w/a`'s, its prefixes being bytes.
    let mut partition = FileSetConfig::builder();
    for index in 0..sets {
        partition.add_file_set(if index == set { vec![VfsPath::from(base_path.clone())] } else { Vec::new() });
    }
    let partition = partition.build();
    let other = SourceRootId(sets as u32);
    let mut view = Vec::with_capacity(loaded.len());
    for file in loaded {
        let Some(path) = files::rest(&file.path, base).filter(|it| !it.is_empty()) else {
            return Err(LowerError::at(format!("`{}` is not under `{base}`", file.path), file.path.clone()));
        };
        view.push(ViewFile { path, id: file.id, bytes: &file.bytes });
    }
    let root = ViewRoot { base: &base_path, built, partition: &partition, other, files: &view };
    db.set_view_root(root).map_err(|error| {
        let at = match &error {
            base_db::ViewError::BadPath(path)
            | base_db::ViewError::Duplicate(path)
            | base_db::ViewError::Mismatch { path, .. }
            | base_db::ViewError::NoSourceRoot { path, .. }
            | base_db::ViewError::NoFileId(path) => format!("{base}/{path}"),
        };
        LowerError::at(error.to_string(), at)
    })
}

fn abs(path: &str) -> Result<AbsPathBuf, LowerError> {
    AbsPathBuf::try_from(path)
        .map_err(|_| LowerError::at(format!("`{path}` is not an absolute path"), path))
}
