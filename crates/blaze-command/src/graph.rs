//! A checkout's unit graph for the keyed driver: the workspace resolved by blaze-units' own
//! load (the [`blaze_units::query::Plan`] query the daemon runs, over a lazy tree of the disk),
//! then each unit the front end checks turned into a [`Unit`] in dependency order, the
//! standard library's crates included.
//!
//! What the graph holds, and what it leaves out:
//!
//! - every unit `check` asks the front end for: libraries, programs, tests, build-script crates
//!   and proc macros, each the crate it is with its cfg, edition, root file, dependencies and
//!   the variables cargo sets for it;
//! - not a build script's run (`RunCustomBuild`) nor a unit built for the macro host: neither
//!   is a crate the front end checks. A unit whose crate needs a script's output (its `OUT_DIR`,
//!   its `rustc-cfg`s), a proc macro, a crate that uses one, and a build-script crate stay in the
//!   graph with [`Unit::blocked`] saying why: the command stops before any task, naming it
//!   ([`crate::NOT_BUILT_YET`]). The standard library's scripts are blaze's ports
//!   ([`blaze_units::stdlib::scripts`]), never run: their output is applied here, so std never
//!   stops the command.
//!
//! The invariant: every unit's dependencies have smaller indexes; every unit's `owner` is the
//! first eight bytes of its `key`, unique in the graph and not reserved; `cfg` is sorted and
//! each atom is once; `env` is sorted by name, each name once; [`Graph::identity`] covers
//! every field of every unit and the graph's own, so two graphs with one identity are one.
//!
//! Reuse, and what is not: the load is blaze-units' (`query::register`, the `Plan` query),
//! driven as `crates/hir-firewall/tests/real.rs` drives it. A unit's cfg lines, std's ported
//! scripts' output with them, come from blaze-units' `stdlib::cfg` (`unit_cfg`, `std_cfg`),
//! the one source the std tests key with too; [`cfg_of`] only parses them and adds blaze's
//! contract atom. The daemon's mounts of fetched sources are read by its VFS: [`DiskTree`]
//! reads them where they are on disk, lazily, the way `real.rs` reads them eagerly.
//!
//! Floors (rule 67). The graph is made once a command. Its floor is the load's own reads: the
//! workspace's manifests, its lock file, its config files, the standard library's manifests,
//! one directory listing per directory the load lists, plus the plan's own time. [`DiskTree`]
//! reads exactly those, each once a process (blaze-query keeps what it read), so the load is at
//! its floor for a command that loads; above the floor of a no-op it costs the whole load
//! every command, since no graph is kept between commands yet (what is missing, in order: a
//! graph record keyed by the load's reads, found by the same certificate rule as the units').

use std::{
    alloc::{Allocator, Global},
    cmp::Ordering,
    fmt,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use blaze_core::lock::Lock;
use blaze_ids::{ContentId, Hasher};
use blaze_keyed::{
    driver::Bodies,
    kind::{Namespace, Origin as PackageOrigin, namespace_of},
    record::Owner,
};
use blaze_query::{Config as DbConfig, Database, Names, Tree};
use blaze_units::{
    query::{self, BuildConfig, FetchKind, Invocation, Mount, Origin, PackagePlan, Plan as LoadPlan, Planned, Units},
    schema::{self, Mode},
    stdlib,
};
use blaze_vfs::{Bytes, Corrupt, Mode as FileMode, View, watch::fixed_out};
use span::{BlazeEdition, Edition};

use crate::{CommandError, boxed};

/// A checkout's units in dependency order: every unit's dependencies have smaller indexes.
pub struct Graph<A: Allocator + Clone> {
    pub units: Vec<Unit<A>, A>,
    /// The toolchain's identity (blaze, its pinned nightly, the target): what every key takes as `tool`.
    pub tool: blaze_ids::ContentId,
    /// The command (`check`, or one that links) and every unit's settings, as one id: what the
    /// graph's identity takes, and no unit's key (each takes its own `Unit::settings`).
    pub settings: blaze_ids::ContentId,
    /// The target's identity: [`blaze_keyed::kind::ToolId::target`]. Every unit of the graph is
    /// built for this one triple (refused by name otherwise).
    pub target: ContentId,
    /// The target triple every unit is built for (`aarch64-apple-darwin`): the host's, as the
    /// load resolved it (another is refused by name until the front end takes this).
    pub triple: Box<str, A>,
    /// The checkout's directory, absolute and UTF-8: what the request's outside reads are
    /// relative to.
    pub checkout: Box<str, A>,
    /// Every field above and of every unit, in one id: the certificate's base.
    pub identity: ContentId,
    /// How long [`load`] took, by part: what the command line's `load` time is made of. In no
    /// key and not in [`Graph::identity`].
    pub load: LoadTimes,
    /// The programs a `build` or `run` links, in unit order: every member's binary unit the
    /// load planned; none for `check`.
    pub programs: Vec<Program<A>, A>,
    /// How std's files are named to the program ([`Graph::std_name`]).
    pub library: LibraryNames<A>,
}

/// Where std's files are on disk and the name the program prints for that directory: the
/// pinned toolchain's own std prints its files (`file!()`, a panic's location, debug info) as
/// `/rustc/<commit>/library/<rest>` ([`stdlib::library_at`], the rule the daemon's mounts
/// name std by), and this graph's std units read `<dir>/<rest>` ([`on_disk`]).
pub struct LibraryNames<A: Allocator + Clone> {
    /// The library's directory on disk, absolute: the machine's, in no key or record.
    pub dir: Box<str, A>,
    /// `/rustc/<commit>/library`: nothing of the machine; in [`Graph::identity`].
    pub printed: Box<str, A>,
}

/// A std file's name as the program prints it: [`LibraryNames::printed`] then `rest` (which is
/// empty or starts with `/`). Two borrows, no allocation; [`fmt::Display`] writes it whole.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StdName<'g, 'p> {
    pub library: &'g str,
    pub rest: &'p str,
}

impl std::fmt::Display for StdName<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", self.library, self.rest)
    }
}

impl<A: Allocator + Clone> Graph<A> {
    /// The name the program prints for the file at absolute `path` of unit `unit` when the unit
    /// is std's (its package's origin the toolchain): `/rustc/<commit>/library/<rest>`. `None`
    /// for a unit that is not std's, or a path not under the library's directory.
    pub fn std_name<'g, 'p>(&'g self, unit: u32, path: &'p str) -> Option<StdName<'g, 'p>> {
        let unit = self.units.get(unit as usize)?;
        if !matches!(unit.origin, PackageOrigin::Toolchain) {
            return None;
        }
        let rest = path.strip_prefix(&*self.library.dir).filter(|rest| rest.is_empty() || rest.starts_with('/'))?;
        Some(StdName { library: &self.library.printed, rest })
    }
}

/// A program `build` or `run` links: a member's binary unit, and what of its profile its code
/// is made under. What makes the code, the toolchain, is the command's to find on the machine
/// (`run::build`): only the profile and the flags are the graph's.
pub struct Program<A: Allocator + Clone> {
    /// Its unit in [`Graph::units`].
    pub unit: u32,
    /// The executable's name, as cargo names its file: the target's (`hello-world`, no `_`).
    pub name: Box<str, A>,
    /// The unit's profile as its code is made: its opt level's tier zero (the edit goal), its
    /// checks, its panic strategy; one profile for every crate linked in ([`program_of`]).
    pub profile: blaze_codegen::plan::Profile,
    /// The unit's rustflags: what the toolchain reads `-C target-cpu` and `-C target-feature`
    /// from (`hir_ty_codegen::v2::toolchain`).
    pub rustflags: Vec<String, Global>,
}

/// A program's profile asks for debug info: line tables are made from the code records' side
/// tables, which no record files yet (`program::code` drops them).
pub const NO_SPAN_TABLES: &str = "a program's profile: debug info is asked (`debug` is not 0), and no code record files its span tables yet (`program::code` drops the side table); set `[profile.dev] debug = 0` (or `CARGO_PROFILE_DEV_DEBUG=0`)";
/// A program's profile names a backend blaze has none of, or Cranelift past its stencils.
pub const BACKEND: &str = "a program's profile: its `codegen-backend` is none blaze makes native code with (`llvm`, `stencil`; `cranelift` only where stencils compile)";
/// A program's profile or flags blaze reads no value of.
pub const PROFILE_VALUE: &str = "a program's profile: an opt level, debug info or panic strategy rustc takes no such value of";
/// A program's flags ask for instrumented code.
pub const INSTRUMENTED: &str = "a program's flags: a sanitizer or profile-guided code (`-Z sanitizer`, `-C profile-generate`, `-C profile-use`) is asked, which the no-daemon command does not make yet";
/// A crate linked into a program has another profile than the program's.
pub const PACKAGE_PROFILE: &str = "a program's profile: a crate linked into it is given another opt level, checks, panic strategy or debug info (`[profile.*.package.*]`), and the no-daemon command makes one profile per program (`Profile::packages` is empty)";

/// The parts of [`load`], each the wall time of one stretch: their sum is the load's time but
/// for the moves between them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LoadTimes {
    /// blaze-units' `Plan` query, every round of it: the workspace's manifests, lock file and
    /// config read, and std's.
    pub resolve: Duration,
    /// The plan's units turned into the graph's, but for `scripts` and `keys`.
    pub units: Duration,
    /// std's ported build scripts run (`stdlib::cfg::std_cfg`), all of them.
    pub scripts: Duration,
    /// Each unit's crate key, the front end's ([`own`]).
    pub keys: Duration,
}

pub struct Unit<A: Allocator + Clone> {
    /// The crate's name as code names it (`serde_json`).
    pub name: Box<str, A>,
    /// The package, as a person names it: `serde_json 1.0.1`.
    pub package: Box<str, A>,
    pub edition: span::Edition,
    /// The crate's root file, absolute, UTF-8 (refused by name otherwise). A standard library
    /// file is where this blaze's library is on disk, not its `/rustc/<commit>` name.
    pub root: Box<str, A>,
    pub kind: UnitKind,
    /// Sorted, each once.
    pub cfg: Vec<CfgAtom<A>, A>,
    pub deps: Vec<Dep<A>, A>,
    /// The variables cargo sets for the crate (`CARGO_PKG_*`, `OUT_DIR`, a std script's
    /// `rustc-env`), sorted by name, each once.
    pub env: Vec<(Box<str, A>, Box<str, A>), A>,
    /// A member of the workspace.
    pub member: bool,
    pub owner: blaze_keyed::record::Owner,
    pub namespace: blaze_keyed::kind::Namespace,
    /// The crate key: the front end's (`crate::snapshot::crate_keys`, `base_db::crate_item_key`
    /// of the unit's crate), and `owner` is its first eight bytes ([`Owner::of_subject`]), so it
    /// is the subject every fact the unit owns is stamped from.
    pub key: ContentId,
    /// Where the unit's package came from: a registry (by its checksum), the toolchain (std
    /// and its dependencies), or anything else (a member, a path, git). `member` says which
    /// of the last are the workspace's.
    pub origin: PackageOrigin,
    /// Whether `check` asks the unit's bodies: a member's, never a dependency's or std's
    /// ([`blaze_keyed::driver::UnitPlan::bodies`]).
    pub bodies: Bodies,
    /// The blaze edition the target opted into, when it did ([`BlazeEdition::from_manifest`]);
    /// `edition` is then its Rust edition.
    pub blaze_edition: Option<BlazeEdition>,
    /// What of the unit's profile a body's lowering reads beside its cfg (overflow checks,
    /// panic strategy, contract checks, opt level, debug assertions), as one id.
    pub settings: ContentId,
    /// Why the command cannot build the unit yet; `None` for one it can.
    pub blocked: Option<Blocked>,
}

pub struct Dep<A: Allocator + Clone> {
    /// The name the dependent's code gives it (`extern crate` name, renames applied).
    pub name: Box<str, A>,
    pub unit: u32,
    /// The edge's marks in the front end's crate graph, by the one rule
    /// (`blaze_units::schema::UnitDep::marks`, as blaze-lower's are): cargo's `noprelude`
    /// resolved here, so nothing downstream derives them again. An input of the dependent's
    /// crate key (`base_db::crate_item_key` hashes each dependency's marks).
    pub marks: blaze_units::schema::EdgeMarks,
}

/// What a unit is built as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnitKind {
    Lib,
    /// A program: a `bin`, or an example or bench checked as one.
    Bin,
    ProcMacro,
    /// A package's `build.rs`, compiled.
    BuildScript,
    /// Checked with `--test`.
    Test,
}

impl UnitKind {
    fn byte(self) -> u8 {
        match self {
            UnitKind::Lib => 0,
            UnitKind::Bin => 1,
            UnitKind::ProcMacro => 2,
            UnitKind::BuildScript => 3,
            UnitKind::Test => 4,
        }
    }
}

/// One cfg atom: `unix`, or `target_os = "linux"`.
#[derive(Clone, Debug)]
pub enum CfgAtom<A: Allocator + Clone> {
    Flag(Box<str, A>),
    KeyValue { key: Box<str, A>, value: Box<str, A> },
}

impl<A: Allocator + Clone> CfgAtom<A> {
    pub fn key(&self) -> &str {
        match self {
            CfgAtom::Flag(key) | CfgAtom::KeyValue { key, .. } => key,
        }
    }

    pub fn value(&self) -> Option<&str> {
        match self {
            CfgAtom::Flag(_) => None,
            CfgAtom::KeyValue { value, .. } => Some(value),
        }
    }

    /// The graph's order of atoms: by key, a flag before every value of its key, then by value.
    pub fn order(&self, other: &CfgAtom<A>) -> Ordering {
        self.key().cmp(other.key()).then_with(|| self.value().cmp(&other.value()))
    }

    /// An atom as `rustc --print cfg` prints one (`name` or `name="value"`), and as a script's
    /// `rustc-cfg` says one; anything else is refused, naming it.
    fn parse(line: &str, alloc: A) -> Result<CfgAtom<A>, &'static str> {
        let ident = |text: &str| {
            !text.is_empty() && text.split("::").all(|part| !part.is_empty() && part.chars().all(|c| c == '_' || c.is_alphanumeric()))
        };
        match line.split_once('=') {
            None if ident(line) => Ok(CfgAtom::Flag(boxed(line, alloc))),
            None => Err("a cfg that is neither `name` nor `name=\"value\"`"),
            Some((key, quoted)) => {
                let key = key.trim_end();
                let quoted = quoted.trim_start();
                let value = quoted
                    .strip_prefix('"')
                    .and_then(|it| it.strip_suffix('"'))
                    .filter(|it| !it.contains(['"', '\\']))
                    .ok_or("a cfg value that is not one plain quoted string")?;
                if !ident(key) {
                    return Err("a cfg key that is not a name");
                }
                Ok(CfgAtom::KeyValue { key: boxed(key, alloc.clone()), value: boxed(value, alloc) })
            }
        }
    }
}

/// Why a unit cannot be built by this command yet: each stops it before any task.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Blocked {
    /// A package's build script, which the keyed driver neither compiles nor runs yet.
    BuildScript,
    /// The crate reads its package's build-script output (`OUT_DIR`, `rustc-cfg`, `rustc-env`).
    ScriptOutput,
    /// A proc-macro crate: its macros are expanded by Wasmtime, not under the keyed driver yet.
    ProcMacro,
    /// The crate uses a proc macro.
    Expansion,
    /// Built for the GPU device (`artifact = "gpu"`), which this command does not lower.
    Device,
}

impl fmt::Display for Blocked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Blocked::BuildScript => "a build script",
            Blocked::ScriptOutput => "its crate reads its build script's output",
            Blocked::ProcMacro => "a proc macro",
            Blocked::Expansion => "its crate expands a proc macro",
            Blocked::Device => "built for the GPU device",
        })
    }
}

impl Blocked {
    fn byte(self) -> u8 {
        match self {
            Blocked::BuildScript => 1,
            Blocked::ScriptOutput => 2,
            Blocked::ProcMacro => 3,
            Blocked::Expansion => 4,
            Blocked::Device => 5,
        }
    }
}

/// Where this blaze's standard library is.
#[derive(Clone, Copy, Debug)]
pub struct Library<'i> {
    /// The `library/` directory, on disk.
    pub dir: &'i Path,
    /// Its digest (`library/DIGEST`): the pinned nightly's library, in the tool's identity.
    pub digest: &'i str,
    /// The rust-lang/rust commit it is of: its files are named `/rustc/<commit>/library/...`.
    pub commit: &'i str,
}

/// What a graph is loaded from, every piece of it given by the command line's driver.
pub struct Inputs<'i> {
    /// cargo's command line as the load reads it: the subcommand, its words (blaze's own flags
    /// taken out), the workspace root, the directory, the environment and the config files.
    pub invocation: Invocation,
    /// The machine's triple.
    pub host: &'i str,
    pub library: Library<'i>,
    /// Where the plan names the standard library's builds, builds and fetched sources
    /// ([`BuildConfig`]'s fields of those names).
    pub std: &'i str,
    pub builds: &'i str,
    pub sources: &'i str,
    /// The version of this blaze ([`BuildConfig::blaze`]).
    pub blaze: &'i str,
    /// The front end's code identity: the part of the tool that is blaze.
    pub front_end: ContentId,
    /// The directory of blaze's stand-ins for `rustc` and `rustdoc`, which a std script's
    /// environment names (it never runs them: the scripts are ported).
    pub tools: &'i Path,
    /// The store's organisation namespace, and whether this machine is a trusted writer
    /// ([`namespace_of`]).
    pub org: Option<Namespace>,
    pub trusted_writer: bool,
}

/// How many times the load may ask for sources before it is called stuck: each round mounts
/// at least one new place, and a workspace names finitely many.
pub const LOAD_ROUNDS: usize = 256;

/// The graph of the workspace `inputs` names, in `alloc`.
pub fn load<A: Allocator + Clone>(inputs: &Inputs<'_>, alloc: A) -> Result<Graph<A>, CommandError> {
    let (units, resolve) = resolve(inputs)?;
    let mut graph = from_units(&units, inputs, alloc)?;
    graph.load.resolve = resolve;
    Ok(graph)
}

/// The units graph of the workspace `inputs` names, as blaze-units resolves it, and how long
/// that took: what [`load`] makes the graph of, which a test reads beside the graph.
pub(crate) fn resolve(inputs: &Inputs<'_>) -> Result<(Units, Duration), CommandError> {
    let checkout = inputs.invocation.root.as_str();
    let tree = Arc::new(DiskTree::new());
    let view = tree.add(Hasher::new("blaze-command view 1").part(checkout.as_bytes()).finish(), DiskRoot::View(PathBuf::from(checkout)));
    let library_root = tree.add(
        Hasher::new("blaze-command library 1").part(inputs.library.digest.as_bytes()).finish(),
        DiskRoot::Dir(inputs.library.dir.to_path_buf()),
    );
    // dyn: blaze-query takes its tree as a trait object, chosen once per database.
    let shared: Arc<dyn Tree> = tree.clone();
    let mut db = Database::new(DbConfig { tree: Some(shared), dir: None, tags: 0 });
    query::register(&mut db);
    let mut mounts = Vec::new_in(Global);
    mounts.push(Mount { at: checkout.to_owned(), origin: Origin::View, under: String::new() });
    mounts.push(stdlib::library_mount(library_root, inputs.library.commit));
    let mut mounted: Vec<String, Global> = Vec::new_in(Global);
    let mut planned = None;
    let began = Instant::now();
    let span = tracing::info_span!(target: "blaze", "command: resolve the workspace into units").entered();
    for _ in 0..LOAD_ROUNDS {
        mounts.sort();
        let config = BuildConfig {
            invocation: inputs.invocation.clone(),
            mounts: mounts.clone(),
            host: inputs.host.to_owned(),
            library: stdlib::library_at(inputs.library.commit),
            std: inputs.std.to_owned(),
            builds: inputs.builds.to_owned(),
            sources: inputs.sources.to_owned(),
            blaze: inputs.blaze.to_owned(),
        };
        let opened = db.open_with(View { root: view }, &config.encode());
        let answer = db
            .pin()
            .get::<LoadPlan>(opened, &())
            .map_err(|error| load_error(format!("resolving the workspace at {checkout}: {error:?}")))?
            .get();
        tree.failed()?;
        let needs = match answer {
            Planned::Ready(units) => {
                planned = Some((units, config));
                break;
            }
            Planned::Failed(error) => return Err(load_error(format!("resolving the workspace at {checkout}: {}", error.what))),
            Planned::Needs(needs) => needs,
        };
        for fetch in needs {
            if mounted.contains(&fetch.at) {
                return Err(load_error(format!("the load needs {} again once it is mounted at {}", fetch.what, fetch.at)));
            }
            mounts.push(mount(&tree, &fetch)?);
            mounted.push(fetch.at);
        }
    }
    drop(span);
    let resolve = began.elapsed();
    let Some((units, _config)) = planned else {
        return Err(load_error(format!("the load of {checkout} asked for sources {LOAD_ROUNDS} times without a plan")));
    };
    Ok((units, resolve))
}

fn load_error(what: impl fmt::Display) -> CommandError {
    CommandError::Load { what: what.to_string().into_boxed_str() }
}

/// The mount of what one fetch names, read where it is on disk. Nothing is fetched: a source
/// that is not there is refused by name, as is a path outside the checkout, which no sweep
/// covers.
fn mount(tree: &DiskTree, fetch: &query::Fetch) -> Result<Mount, CommandError> {
    let at = Path::new(&fetch.at);
    let root = |kind: &[u8], pin: &[&str]| {
        let mut hasher = Hasher::new("blaze-command source 1");
        hasher.part(kind).part(fetch.at.as_bytes());
        for part in pin {
            hasher.part(part.as_bytes());
        }
        hasher.finish()
    };
    let id = match &fetch.kind {
        FetchKind::Crate { sha256 } => tree.add(root(b"crate", &[sha256]), DiskRoot::Dir(at.to_path_buf())),
        FetchKind::Git { commit, tree: pinned } => tree.add(root(b"git", &[commit, pinned]), DiskRoot::Dir(at.to_path_buf())),
        FetchKind::IndexFile => {
            return Err(load_error(format!(
                "the load reads the index file {}: the lock file does not pin {}, and `check --no-daemon` fetches nothing",
                fetch.url, fetch.what
            )));
        }
        FetchKind::Local { probe: false } => {
            return Err(load_error(format!(
                "{} is outside the checkout ({}): no sweep covers its files, so no certificate could; `check --no-daemon` builds what is under the checkout",
                fetch.what, fetch.at
            )));
        }
        FetchKind::Local { probe: true } => match std::fs::symlink_metadata(at) {
            Ok(meta) if meta.is_file() => {
                let (Some(dir), Some(name)) = (at.parent(), at.file_name().and_then(|it| it.to_str())) else {
                    return Err(load_error(format!("{} names no file in a directory", fetch.at)));
                };
                let id = tree.add(root(b"one", &[name]), DiskRoot::One { dir: dir.to_path_buf(), name: name.to_owned() });
                return Ok(Mount { at: fetch.at.clone(), origin: Origin::Root(id), under: name.to_owned() });
            }
            Ok(_) => return Err(load_error(format!("{} is there and is no file", fetch.at))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => tree.add(root(b"absent", &[]), DiskRoot::Absent),
            Err(error) => return Err(CommandError::File { path: fetch.at.clone().into_boxed_str(), what: error.to_string().into_boxed_str() }),
        },
    };
    if matches!(fetch.kind, FetchKind::Crate { .. } | FetchKind::Git { .. }) && !at.is_dir() {
        return Err(load_error(format!(
            "{} is not at {}: `check --no-daemon` fetches nothing; a `blaze check` with the daemon fetches it once",
            fetch.what, fetch.at
        )));
    }
    Ok(Mount { at: fetch.at.clone(), origin: Origin::Root(id), under: String::new() })
}

// --- the units --------------------------------------------------------------------------------

/// The graph of a plan's units.
/// Which units of `units` are crates of the graph, by their index there, and those crates in
/// the graph's order: graph unit `i` is units' unit `order[i]`.
pub(crate) fn crates_of<A: Allocator + Clone>(units: &Units, alloc: A) -> Result<(Vec<bool, A>, Vec<usize, A>), CommandError> {
    let all = &units.graph.units;
    let mut kept = Vec::with_capacity_in(all.len(), alloc.clone());
    for (at, unit) in all.iter().enumerate() {
        let script_run = unit.mode == Mode::RunCustomBuild;
        let std_script = unit.is_std && unit.target.is_custom_build();
        kept.push(!script_run && !unit.macro_wasm && !std_script);
        if kept[at] && matches!(unit.mode, Mode::Doc | Mode::Doctest | Mode::Docscrape) {
            return Err(unit_error(describe(units, at), "check builds no documentation", format!("a {:?} unit", unit.mode)));
        }
    }
    let order = dependency_order(units, &kept, alloc)?;
    Ok((kept, order))
}

pub(crate) fn from_units<A: Allocator + Clone>(units: &Units, inputs: &Inputs<'_>, alloc: A) -> Result<Graph<A>, CommandError> {
    let began = Instant::now();
    let mut scripts = Duration::ZERO;
    let all = &units.graph.units;
    if units.plans.len() != all.len() {
        return Err(load_error(format!("the plan has {} units and {} unit plans", all.len(), units.plans.len())));
    }
    let what = |at: usize| describe(units, at);
    let (kept, order) = crates_of(units, Global)?;
    let links = matches!(inputs.invocation.command, query::Command::Build | query::Command::Run);
    let mut index = Vec::with_capacity_in(all.len(), Global);
    index.resize(all.len(), u32::MAX);
    for (new, &old) in order.iter().enumerate() {
        index[old] = u32::try_from(new).map_err(|_| load_error(format!("{} units, more than a graph indexes", order.len())))?;
    }
    let triple_of = |unit: &schema::Unit| unit.platform.clone().unwrap_or_else(|| inputs.host.to_owned());
    let triple = match order.first() {
        Some(&first) => triple_of(&all[first]),
        None => return Err(load_error(format!("the workspace at {} has no unit to check", inputs.invocation.root))),
    };
    let mut graph_units = Vec::with_capacity_in(order.len(), alloc.clone());
    let mut programs = Vec::new_in(alloc.clone());
    // Each unit's code settings ([`code_settings_of`]), in graph order: what a program's
    // crates are held to its own by ([`PACKAGE_PROFILE`]). Empty for `check`.
    let mut code_settings: Vec<Option<ContentId>, Global> = Vec::new_in(Global);
    for &old in order.iter() {
        let unit = &all[old];
        let plan = &units.plans[old];
        let package = package_of(units, unit)?;
        if triple_of(unit) != triple {
            return Err(unit_error(what(old), "a graph is built for one target", format!("built for {} beside {triple}", triple_of(unit))));
        }
        let mut blocked = None;
        let mut deps = Vec::with_capacity_in(unit.dependencies.len(), alloc.clone());
        let mut script = None;
        for dep in &unit.dependencies {
            let Some(target) = all.get(dep.index) else {
                return Err(load_error(format!("{} depends on unit {}, which the plan does not have", what(old), dep.index)));
            };
            if target.mode == Mode::RunCustomBuild {
                script = Some(dep.index);
                continue;
            }
            if !kept[dep.index] {
                // Only a proc macro's own dependencies are built for the macro host, and a proc
                // macro is blocked: nothing of its is asked.
                if unit.target.is_proc_macro() {
                    continue;
                }
                return Err(load_error(format!("{} depends on {}, which is no crate of the graph", what(old), what(dep.index))));
            }
            if target.target.is_proc_macro() {
                blocked = blocked.or(Some(Blocked::Expansion));
            }
            deps.push(Dep {
                name: boxed(&dep.extern_crate_name, alloc.clone()),
                unit: index[dep.index],
                marks: dep.marks(unit, target),
            });
        }
        let kind = kind_of(unit, plan);
        if kind == UnitKind::BuildScript {
            blocked = Some(Blocked::BuildScript);
        } else if kind == UnitKind::ProcMacro {
            blocked = Some(Blocked::ProcMacro);
        } else if unit.for_device() {
            blocked = Some(Blocked::Device);
        }
        let label = what(old).to_string();
        let mut env = Vec::with_capacity_in(plan.env.len(), alloc.clone());
        for (name, value) in &plan.env {
            env.push((boxed(name, alloc.clone()), boxed(value, alloc.clone())));
        }
        // The unit's cfg lines from blaze-units' one source (`stdlib::cfg`): a std unit's with
        // its ported script's cfgs and variables, the tests' too.
        let refused = |why: String| unit_error(&label, "a unit's cfg is the target's with its flags (`blaze_units::stdlib::cfg`)", why);
        let lines = match (script, unit.is_std) {
            (Some(_), true) => {
                let ran_at = Instant::now();
                let library = stdlib::library_at(inputs.library.commit);
                let jobs = blaze_core::cores::get().map_err(|error| refused(error.to_string()))?.split(blaze_core::cores::NESTED).get();
                let run = stdlib::cfg::ScriptRun {
                    library: &library,
                    dir: inputs.library.dir,
                    host: inputs.host,
                    tools: inputs.tools,
                    inherited: &inputs.invocation.env,
                    jobs: u32::try_from(jobs).unwrap_or(u32::MAX),
                };
                let std = stdlib::cfg::std_cfg(all, &units.plans, &units.packages, old, &triple, &run)
                    .map_err(|why| refused(why.to_string()))?;
                scripts += ran_at.elapsed();
                for (name, value) in &std.env {
                    env.push((boxed(name, alloc.clone()), boxed(value, alloc.clone())));
                }
                std.lines
            }
            (Some(_), false) => {
                blocked = blocked.or(Some(Blocked::ScriptOutput));
                stdlib::cfg::unit_cfg(unit, plan, &triple).map_err(refused)?
            }
            (None, _) => stdlib::cfg::unit_cfg(unit, plan, &triple).map_err(refused)?,
        };
        let mut cfg = cfg_of(unit, &lines, &label, alloc.clone())?;
        cfg.sort_by(CfgAtom::order);
        cfg.dedup_by(|later, first| later.order(first) == Ordering::Equal);
        env.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some(pair) = env.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            return Err(unit_error(what(old), "a crate's variable has one value", format!("`{}` is set to `{}` and to `{}`", pair[0].0, pair[0].1, pair[1].1)));
        }
        let (edition, blaze_edition) = edition_of(unit, &label)?;
        let root = root_of(unit, inputs, &label, alloc.clone())?;
        let origin = match (unit.is_std, package.checksum) {
            (true, _) => PackageOrigin::Toolchain,
            (false, Some(checksum)) => PackageOrigin::Registry { checksum: ContentId(checksum) },
            (false, None) => PackageOrigin::Private,
        };
        let namespace = match (inputs.trusted_writer, inputs.org, origin) {
            (true, None, PackageOrigin::Private) => {
                return Err(unit_error(
                    what(old),
                    "a trusted writer files private code under its organisation",
                    "the store names no organisation (`org` in store.json)".to_owned(),
                ));
            }
            (_, org, origin) => namespace_of(origin, org.unwrap_or(Namespace::LOCAL), inputs.trusted_writer),
        };
        let (member, bodies) = asked(package, unit);
        if links {
            // std is built under the program's profile whatever cargo prints for it.
            code_settings.push((!unit.is_std).then(|| code_settings_of(&unit.profile)));
            if kind == UnitKind::Bin && member && unit.mode == Mode::Build {
                let at = u32::try_from(graph_units.len()).map_err(|_| load_error("more units than a graph indexes"))?;
                programs.push(program_of(unit, plan, at, &label, alloc.clone())?);
            }
        }
        graph_units.push(Unit {
            name: boxed(&unit.target.name.replace('-', "_"), alloc.clone()),
            package: boxed(&format!("{} {}", package.name, package.version), alloc.clone()),
            edition,
            root,
            kind,
            cfg,
            deps,
            env,
            member,
            // Set below from the front end's crate keys, once the graph is whole.
            owner: Owner([0; 8]),
            namespace,
            key: ContentId([0; 32]),
            origin,
            bodies,
            blaze_edition,
            settings: settings_of(&unit.profile),
            blocked,
        });
    }
    // The front end's databases are made for this machine's target (`front::workspace`): a
    // graph for another would be keyed against the wrong target data.
    if triple != inputs.host {
        return Err(load_error(format!(
            "the workspace is built for {triple}, and the front end checks for this machine's {} alone (rule: one target, the host's, until `Command` takes `Graph::triple`)",
            inputs.host
        )));
    }
    let tool = Hasher::new("blaze-command tool 1")
        .id(&inputs.front_end)
        .part(inputs.library.digest.as_bytes())
        .part(inputs.blaze.as_bytes())
        .finish();
    let target = Hasher::new("blaze-command target 1").part(triple.as_bytes()).finish();
    for program in programs.iter() {
        same_profile(&graph_units, &code_settings, program.unit)?;
    }
    let mut settings = Hasher::new("blaze-command settings 1");
    // The graph's identity alone takes this (`identity_of`: the certificate's base), where a
    // command that links is another graph than a check. No unit's key takes it: a unit's
    // manifest and surface are one under `check`, `build` and `run` (`front::Command::new`).
    settings.part(match links {
        false => b"check".as_slice(),
        true => b"build".as_slice(),
    });
    for unit in graph_units.iter() {
        settings.id(&unit.settings);
    }
    let settings = settings.finish();
    let checkout = boxed(&inputs.invocation.root, alloc.clone());
    let triple = boxed(&triple, alloc.clone());
    let library_dir = inputs.library.dir.to_str().ok_or_else(|| {
        unit_error("the library".to_owned(), "the library's directory is named in UTF-8", format!("{}", inputs.library.dir.display()))
    })?;
    let library = LibraryNames {
        dir: boxed(library_dir.trim_end_matches('/'), alloc.clone()),
        printed: boxed(&stdlib::library_at(inputs.library.commit), alloc.clone()),
    };
    let mut graph = Graph {
        units: graph_units,
        tool,
        settings,
        target,
        triple,
        checkout,
        identity: ContentId([0; 32]),
        load: LoadTimes::default(),
        programs,
        library,
    };
    let keys_at = Instant::now();
    own(&mut graph, alloc)?;
    let keys = keys_at.elapsed();
    graph.identity = identity_of(&graph.units, tool, settings, target, &graph.checkout, &graph.library.printed);
    let units = began.elapsed().saturating_sub(scripts + keys);
    graph.load = LoadTimes { resolve: Duration::ZERO, units, scripts, keys };
    Ok(graph)
}

/// Each unit's crate key and owner: the front end's own crate key
/// ([`crate::snapshot::crate_keys`], the key `Command::new` checks every owner against), so the
/// owner is computed once, in one place, and the graph cannot disagree with the front end.
fn own<A: Allocator + Clone>(graph: &mut Graph<A>, alloc: A) -> Result<(), CommandError> {
    let workspace = triomphe::Arc::new(crate::front::workspace()?);
    let mut cfgs = Vec::with_capacity_in(graph.units.len(), alloc.clone());
    cfgs.extend(graph.units.iter().map(|unit| crate::front::cfg_of(&unit.cfg)));
    let keys = crate::snapshot::crate_keys(graph, &workspace, &cfgs, alloc)?;
    for (unit, key) in graph.units.iter_mut().zip(keys.iter()) {
        let owner = Owner::of_subject(key);
        if !owner.is_crate() {
            return Err(unit_error(
                format!("`{}` ({})", unit.name, unit.package),
                "an owner is no reserved prefix",
                format!("crate key {key:?}"),
            ));
        }
        unit.key = *key;
        unit.owner = owner;
    }
    check_owners(&graph.units)
}

fn unit_error(unit: impl fmt::Display, rule: &'static str, what: impl fmt::Display) -> CommandError {
    CommandError::Graph { unit: unit.to_string().into_boxed_str(), rule, what: what.to_string().into_boxed_str() }
}

/// How a unit is named in an error: its package and target, and what it is built as.
#[derive(Clone, Copy)]
struct Described<'u> {
    units: &'u Units,
    at: usize,
}

impl fmt::Display for Described<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.units.graph.units.get(self.at) {
            Some(unit) => write!(f, "`{}` ({}, {:?})", unit.target.name, unit.pkg_id, unit.mode),
            None => write!(f, "unit {}", self.at),
        }
    }
}

fn describe(units: &Units, at: usize) -> Described<'_> {
    Described { units, at }
}

fn package_of<'u>(units: &'u Units, unit: &schema::Unit) -> Result<&'u PackagePlan, CommandError> {
    units
        .packages
        .iter()
        .find(|it| it.id == unit.pkg_id)
        .ok_or_else(|| load_error(format!("the plan names package {} and has no plan of it", unit.pkg_id)))
}

/// The kept units in dependency order: a depth-first walk from each in plan order, a unit
/// after everything it depends on, so the order is the plan's wherever it can be. A cycle is
/// refused by name; the walk is bounded by twice the edges.
fn dependency_order<A: Allocator + Clone>(units: &Units, kept: &[bool], alloc: A) -> Result<Vec<usize, A>, CommandError> {
    let all = &units.graph.units;
    // 0: not reached, 1: on the walk's path, 2: placed.
    let mut state = Vec::with_capacity_in(all.len(), alloc.clone());
    state.resize(all.len(), 0u8);
    let mut order = Vec::with_capacity_in(all.len(), alloc.clone());
    let mut stack: Vec<(usize, usize), A> = Vec::new_in(alloc);
    for start in 0..all.len() {
        if !kept[start] || state[start] != 0 {
            continue;
        }
        stack.push((start, 0));
        state[start] = 1;
        while let Some(&mut (at, ref mut next)) = stack.last_mut() {
            let deps = &all[at].dependencies;
            if *next == deps.len() {
                stack.pop();
                state[at] = 2;
                order.push(at);
                continue;
            }
            let dep = deps[*next].index;
            *next += 1;
            if dep >= all.len() || !kept[dep] {
                continue;
            }
            match state[dep] {
                0 => {
                    state[dep] = 1;
                    stack.push((dep, 0));
                }
                1 => return Err(load_error(format!("{} depends on itself through {}", describe(units, dep), describe(units, at)))),
                _ => {}
            }
        }
    }
    Ok(order)
}

fn kind_of(unit: &schema::Unit, plan: &query::UnitPlan) -> UnitKind {
    if unit.mode == Mode::Test || plan.checked_as_test {
        UnitKind::Test
    } else if unit.target.is_custom_build() {
        UnitKind::BuildScript
    } else if unit.target.is_proc_macro() {
        UnitKind::ProcMacro
    } else if unit.target.is_lib() {
        UnitKind::Lib
    } else {
        UnitKind::Bin
    }
}

/// The edition the front end reads the crate in, and the blaze edition it opted into.
fn edition_of(unit: &schema::Unit, what: &str) -> Result<(Edition, Option<BlazeEdition>), CommandError> {
    let blaze = BlazeEdition::from_manifest(&unit.target.edition)
        .map_err(|unknown| unit_error(what.to_owned(), "an edition is one blaze knows", unknown.to_string()))?;
    match blaze {
        Some(blaze) => Ok((blaze.rust(), Some(blaze))),
        None => unit
            .target
            .edition
            .parse::<Edition>()
            .map(|it| (it, None))
            .map_err(|_| unit_error(what.to_owned(), "an edition is one blaze knows", format!("edition `{}`", unit.target.edition))),
    }
}

/// The root file on disk, absolute and UTF-8: a std file mapped from its `/rustc/<commit>`
/// name to this blaze's library.
fn root_of<A: Allocator + Clone>(unit: &schema::Unit, inputs: &Inputs<'_>, what: &str, alloc: A) -> Result<Box<str, A>, CommandError> {
    let named = unit
        .target
        .src_path
        .to_str()
        .ok_or_else(|| unit_error(what, "a root file is named in UTF-8", unit.target.src_path.display()))?;
    if !unit.is_std {
        if !named.starts_with('/') {
            return Err(unit_error(what, "a root file is absolute", named));
        }
        return Ok(boxed(named, alloc));
    }
    on_disk(named, inputs, what, alloc)
}

/// A std path named `/rustc/<commit>/library/<rest>`, where the library is on disk.
fn on_disk<A: Allocator + Clone>(named: &str, inputs: &Inputs<'_>, what: &str, alloc: A) -> Result<Box<str, A>, CommandError> {
    let at = stdlib::library_at(inputs.library.commit);
    let rest = named
        .strip_prefix(at.as_str())
        .filter(|rest| rest.is_empty() || rest.starts_with('/'))
        .ok_or_else(|| unit_error(what.to_owned(), "a std file is under the library", format!("{named} is not under {at}")))?;
    let dir = inputs
        .library
        .dir
        .to_str()
        .ok_or_else(|| unit_error(what.to_owned(), "the library's directory is named in UTF-8", format!("{}", inputs.library.dir.display())))?;
    let mut path = blaze_mem::String::with_capacity_in(dir.len() + rest.len(), alloc);
    path.push_str(dir);
    path.push_str(rest);
    Ok(path.into_boxed_str())
}

/// Whether `unit` of `package` is the checkout's own, and so whether `check` asks its bodies:
/// a member package's unit that is not std's. std's packages are members of the library's own
/// workspace (blaze-units loads `library/` as one, `load::Foreign`, and its `PackagePlan`s say
/// `is_member` of that), never of the checkout's: read alone, `is_member` asked every body of
/// core (job merge25: 15,607 body tasks of core, each refused).
fn asked(package: &PackagePlan, unit: &schema::Unit) -> (bool, Bodies) {
    match package.is_member && !unit.is_std {
        true => (true, Bodies::Asked),
        false => (false, Bodies::NotAsked),
    }
}

/// The unit's cfg atoms: its `lines` (blaze-units' `stdlib::cfg`, the one source of what rustc
/// is given), and blaze's contract atom when the profile checks contracts, which is blaze's own
/// and no line of rustc's.
fn cfg_of<A: Allocator + Clone, L: AsRef<str>>(unit: &schema::Unit, lines: &[L], what: &str, alloc: A) -> Result<Vec<CfgAtom<A>, A>, CommandError> {
    let profile = &unit.profile;
    let mut atoms = Vec::with_capacity_in(lines.len() + 1, alloc.clone());
    for line in lines {
        let line = line.as_ref();
        atoms.push(CfgAtom::parse(line, alloc.clone()).map_err(|why| unit_error(what, why, line))?);
    }
    if profile.contract_checks {
        atoms.push(CfgAtom::Flag(boxed(cfg::CONTRACT_CHECKS, alloc)));
    }
    Ok(atoms)
}

/// What of a profile a body's lowering reads beside the cfg.
fn settings_of(profile: &schema::Profile) -> ContentId {
    let mut hasher = Hasher::new("blaze-command unit settings 1");
    hasher.part(profile.opt_level.as_bytes()).part(profile.panic.as_bytes());
    hasher.update(&[u8::from(profile.debug_assertions), u8::from(profile.overflow_checks), u8::from(profile.contract_checks)]);
    hasher.finish()
}

/// What of a unit's profile its code is made under, beside [`settings_of`]'s: its debug info,
/// as one id, to hold a program's crates to its own profile ([`same_profile`]).
fn code_settings_of(profile: &schema::Profile) -> ContentId {
    let mut hasher = Hasher::new("blaze-command code settings 1");
    hasher.id(&settings_of(profile));
    match &profile.debuginfo {
        schema::DebugInfo::Level(level) => hasher.part(&[0, *level]),
        schema::DebugInfo::Named(name) => hasher.part(&[1]).part(name.as_bytes()),
    };
    hasher.finish()
}

/// Every non-std crate linked into the program at `unit` has the program's code settings:
/// one profile per program ([`PACKAGE_PROFILE`]). Floor: one walk down the program's closure.
fn same_profile<A: Allocator + Clone>(units: &[Unit<A>], code_settings: &[Option<ContentId>], unit: u32) -> Result<(), CommandError> {
    let own = code_settings.get(unit as usize).copied().flatten();
    let mut seen: Vec<bool, Global> = Vec::with_capacity_in(unit as usize + 1, Global);
    seen.resize(unit as usize + 1, false);
    seen[unit as usize] = true;
    for down in (0..=unit as usize).rev() {
        if !seen[down] {
            continue;
        }
        for dep in units[down].deps.iter() {
            if let Some(it) = seen.get_mut(dep.unit as usize) {
                *it = true;
            }
        }
        if let Some(theirs) = code_settings.get(down).copied().flatten()
            && Some(theirs) != own
        {
            let named = &units[down];
            return Err(unit_error(format!("`{}` ({})", named.name, named.package), PACKAGE_PROFILE, format!("linked into `{}`", units[unit as usize].name)));
        }
    }
    Ok(())
}

/// The program of `unit`, a member's binary at graph index `at`: its profile as blaze makes
/// code under it, each value refused by name where the no-daemon command makes no code of it.
///
/// The derivation mirrors blaze-cargo's (`serve/unit/codegen.rs`, `policy`, `opt_level`,
/// `debuginfo`, `panic`, `codegen_backend`), which are private to that crate: two homes of one
/// rule until it moves to a crate both take (agents/annoyances/command-policy-two-homes.json).
fn program_of<A: Allocator + Clone>(unit: &schema::Unit, plan: &query::UnitPlan, at: u32, what: &str, alloc: A) -> Result<Program<A>, CommandError> {
    use blaze_codegen::{
        Backend,
        plan::{OptLevel, Override, Profile},
        settings::{DebugInfo, Panic},
    };
    let profile = &unit.profile;
    let refused = |rule: &'static str, value: &str| unit_error(what.to_owned(), rule, value.to_owned());
    let opt = match profile.opt_level.as_str() {
        "0" => OptLevel::O0,
        "1" => OptLevel::O1,
        "2" => OptLevel::O2,
        "3" => OptLevel::O3,
        "s" => OptLevel::S,
        "z" => OptLevel::Z,
        other => return Err(refused(PROFILE_VALUE, &format!("opt-level `{other}`"))),
    };
    // The edit goal: the profile's code at its tier zero. The optimized tier (`--goal max`,
    // a release profile's own level) is refused where the walker asks it
    // (`program::NO_RELEASE_TIER`).
    let level = opt.tier_zero();
    let stencils = match profile.codegen_backend.as_deref() {
        None | Some("llvm") => false,
        Some("stencil") => true,
        Some("cranelift") if level.backend(false) == Some(Backend::Stencil) => false,
        Some(other) => return Err(refused(BACKEND, &format!("codegen-backend `{other}` at opt-level {}", profile.opt_level))),
    };
    let debuginfo = match &profile.debuginfo {
        schema::DebugInfo::Level(0) => DebugInfo::None,
        schema::DebugInfo::Named(name) if name == "none" || name == "false" => DebugInfo::None,
        schema::DebugInfo::Level(level) => return Err(refused(NO_SPAN_TABLES, &format!("debug = {level}"))),
        schema::DebugInfo::Named(name) => return Err(refused(NO_SPAN_TABLES, &format!("debug = \"{name}\""))),
    };
    let panic = match profile.panic.as_str() {
        "unwind" => Panic::Unwind,
        "abort" => Panic::Abort,
        other => return Err(refused(PROFILE_VALUE, &format!("panic = \"{other}\""))),
    };
    if let Some(flag) =
        plan.rustflags.iter().find(|flag| flag.contains("sanitizer") || flag.contains("profile-generate") || flag.contains("profile-use"))
    {
        return Err(refused(INSTRUMENTED, flag));
    }
    let mut rustflags = Vec::with_capacity_in(plan.rustflags.len(), Global);
    rustflags.extend(plan.rustflags.iter().cloned());
    Ok(Program {
        unit: at,
        name: boxed(&unit.target.name, alloc),
        profile: Profile {
            opt_level: level,
            overflow_checks: profile.overflow_checks,
            debug_assertions: profile.debug_assertions,
            debuginfo,
            panic,
            // One profile per program ([`same_profile`]).
            packages: Default::default(),
            build_override: Override::default(),
            stencils,
        },
        rustflags,
    })
}

/// Two units with one owner would answer each other's facts: refused, naming both.
fn check_owners<A: Allocator + Clone>(units: &[Unit<A>]) -> Result<(), CommandError> {
    let mut owners: Vec<(Owner, usize), Global> = Vec::with_capacity_in(units.len(), Global);
    owners.extend(units.iter().enumerate().map(|(at, unit)| (unit.owner, at)));
    owners.sort_by_key(|it| it.0.0);
    match owners.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        Some(pair) => Err(unit_error(
            format!("`{}` ({})", units[pair[1].1].name, units[pair[1].1].package),
            "every unit owns its own facts",
            format!("its owner is that of `{}` ({})", units[pair[0].1].name, units[pair[0].1].package),
        )),
        None => Ok(()),
    }
}

/// The certificate's base: every field of the graph, in order.
fn identity_of<A: Allocator + Clone>(
    units: &[Unit<A>],
    tool: ContentId,
    settings: ContentId,
    target: ContentId,
    checkout: &str,
    library: &str,
) -> ContentId {
    let mut hasher = Hasher::new("blaze-command graph 1");
    hasher.id(&tool).id(&settings).id(&target).part(checkout.as_bytes()).part(library.as_bytes());
    hasher.update(&(units.len() as u64).to_le_bytes());
    for unit in units {
        hasher.part(unit.name.as_bytes()).part(unit.package.as_bytes()).part(format!("{:?}", unit.edition).as_bytes());
        hasher.opt_part(unit.blaze_edition.map(|it| it.manifest()));
        hasher.part(unit.root.as_bytes()).update(&[unit.kind.byte(), u8::from(unit.member)]);
        hasher.update(&(unit.cfg.len() as u64).to_le_bytes());
        for atom in unit.cfg.iter() {
            hasher.part(atom.key().as_bytes()).opt_part(atom.value());
        }
        hasher.update(&(unit.deps.len() as u64).to_le_bytes());
        for dep in unit.deps.iter() {
            hasher.part(dep.name.as_bytes()).update(&dep.unit.to_le_bytes());
        }
        hasher.update(&(unit.env.len() as u64).to_le_bytes());
        for (name, value) in unit.env.iter() {
            hasher.part(name.as_bytes()).part(value.as_bytes());
        }
        hasher.update(&unit.owner.0).update(&unit.namespace.0.to_le_bytes()).id(&unit.key).id(&unit.settings);
        match unit.origin {
            PackageOrigin::Registry { checksum } => hasher.update(&[1]).id(&checksum),
            PackageOrigin::Toolchain => hasher.update(&[2]),
            PackageOrigin::Private => hasher.update(&[3]),
        };
        hasher.update(&[u8::from(unit.bodies == Bodies::Asked), unit.blocked.map_or(0, Blocked::byte)]);
    }
    hasher.finish()
}

// --- the tree the load reads ------------------------------------------------------------------

/// One root of [`DiskTree`].
enum DiskRoot {
    /// The checkout: its files as the sweep's view has them (no root `target`, no `.git`).
    View(PathBuf),
    /// A directory read as it is: the library, a fetched source.
    Dir(PathBuf),
    /// One file of a directory, alone (a manifest the workspace search asks for).
    One { dir: PathBuf, name: String },
    /// Nothing: a path the workspace search asks for that is not there.
    Absent,
}

/// The files the load reads, read from the disk where they are when it asks, each once a
/// process (blaze-query keeps every answer for a root). A root's id names its place and pin
/// for this process alone: the database is made and dropped by one command, so no id outlives
/// what it names, and the graph's identity is of what the load made, never of these ids.
///
/// A look that fails (not "not there", which is an answer) cannot be said through
/// [`Tree::file`], whose only error is an image's [`Corrupt`]: it is kept and the load is
/// failed by it right after the plan answers ([`DiskTree::failed`]), so nothing made from the
/// answer it stood in for is used.
struct DiskTree {
    roots: Lock<Vec<(ContentId, Arc<DiskRoot>), Global>>,
    failure: Lock<Option<(String, String)>>,
}

impl DiskTree {
    fn new() -> DiskTree {
        DiskTree { roots: Lock::new("command.tree.roots", Vec::new_in(Global)), failure: Lock::new("command.tree.failure", None) }
    }

    /// `root` added under `id`; the id, for the mount.
    fn add(&self, id: ContentId, root: DiskRoot) -> ContentId {
        let mut roots = self.roots.lock();
        if !roots.iter().any(|it| it.0 == id) {
            roots.push((id, Arc::new(root)));
        }
        id
    }

    /// `read` of the root under `id`, or `None` for an id with none: the root is shared out of
    /// the lock (the guard drops with the `let`), so `read` reads the disk unlocked.
    fn with_root<R>(&self, id: ContentId, read: impl FnOnce(&DiskRoot) -> R) -> Option<R> {
        let root = self.roots.lock().iter().find(|it| it.0 == id).map(|it| Arc::clone(&it.1))?;
        Some(read(&root))
    }

    fn fail(&self, path: &Path, error: &std::io::Error) {
        let mut failure = self.failure.lock();
        if failure.is_none() {
            *failure = Some((path.display().to_string(), error.to_string()));
        }
    }

    fn failed(&self) -> Result<(), CommandError> {
        match self.failure.lock().take() {
            Some((path, what)) => Err(CommandError::File { path: path.into_boxed_str(), what: what.into_boxed_str() }),
            None => Ok(()),
        }
    }

    /// Where `path` of `root` is on disk, or `None` for a path the root does not have.
    fn place(root: &DiskRoot, path: &str) -> Option<PathBuf> {
        match root {
            DiskRoot::View(_) if !path.is_empty() && fixed_out(path) => None,
            DiskRoot::View(dir) | DiskRoot::Dir(dir) => Some(if path.is_empty() { dir.clone() } else { dir.join(path) }),
            DiskRoot::One { dir, name } if path == name => Some(dir.join(name)),
            DiskRoot::One { .. } | DiskRoot::Absent => None,
        }
    }
}

impl Tree for DiskTree {
    fn file(&self, root: ContentId, path: &str) -> Result<Option<(ContentId, FileMode, Bytes)>, Corrupt> {
        let Some(place) = self.with_root(root, |root| DiskTree::place(root, path)).flatten() else { return Ok(None) };
        let meta = match std::fs::symlink_metadata(&place) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                self.fail(&place, &error);
                return Ok(None);
            }
        };
        let (mode, bytes) = if meta.file_type().is_symlink() {
            match std::fs::read_link(&place) {
                Ok(to) => (FileMode::Symlink, to.into_os_string().into_encoded_bytes()),
                Err(error) => {
                    self.fail(&place, &error);
                    return Ok(None);
                }
            }
        } else if meta.is_file() {
            match std::fs::read(&place) {
                Ok(bytes) => (if meta.permissions().mode() & 0o111 != 0 { FileMode::Executable } else { FileMode::Regular }, bytes),
                Err(error) => {
                    self.fail(&place, &error);
                    return Ok(None);
                }
            }
        } else {
            return Ok(None);
        };
        let id = ContentId::of(blaze_vfs::FILE_DOMAIN, &bytes);
        let shared: Arc<[u8]> = Arc::from(bytes);
        Ok(Some((id, mode, Bytes::from(shared))))
    }

    fn read_dir(&self, root: ContentId, path: &str, read: &mut dyn FnMut(Names<'_, '_>)) -> bool {
        // dyn: see `Tree::read_dir`
        self.with_root(root, |root| self.list(root, path, read)).unwrap_or(false)
    }

    fn dir(&self, root: ContentId, path: &str) -> Option<ContentId> {
        let place = self.with_root(root, |found| DiskTree::place(found, path))??;
        match std::fs::symlink_metadata(&place) {
            Ok(meta) if meta.is_dir() => Some(Hasher::new("blaze-command dir 1").id(&root).part(path.as_bytes()).finish()),
            Ok(_) => None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                self.fail(&place, &error);
                None
            }
        }
    }
}

impl DiskTree {
    /// [`Tree::read_dir`] of `path` under `root`.
    fn list(&self, root: &DiskRoot, path: &str, read: &mut dyn FnMut(Names<'_, '_>)) -> bool {
        // dyn: see `Tree::read_dir`
        if let DiskRoot::One { name, .. } = root {
            if !path.is_empty() {
                return false;
            }
            read(&mut std::iter::once((name.as_str(), false)));
            return true;
        }
        let Some(place) = DiskTree::place(root, path) else { return false };
        let entries = match std::fs::read_dir(&place) {
            Ok(entries) => entries,
            Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => return false,
            Err(error) => {
                self.fail(&place, &error);
                return false;
            }
        };
        let view = matches!(root, DiskRoot::View(_));
        let mut names: Vec<(String, bool), Global> = Vec::new_in(Global);
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.fail(&place, &error);
                    return false;
                }
            };
            let Ok(name) = entry.file_name().into_string() else {
                self.fail(&entry.path(), &std::io::Error::new(std::io::ErrorKind::InvalidData, "a name that is not UTF-8"));
                return false;
            };
            let below = if path.is_empty() { name.clone() } else { format!("{path}/{name}") };
            if view && fixed_out(&below) {
                continue;
            }
            let dir = match entry.file_type() {
                Ok(kind) => kind.is_dir(),
                Err(error) => {
                    self.fail(&entry.path(), &error);
                    return false;
                }
            };
            names.push((name, dir));
        }
        names.sort();
        read(&mut names.iter().map(|(name, dir)| (name.as_str(), *dir)));
        true
    }
}
