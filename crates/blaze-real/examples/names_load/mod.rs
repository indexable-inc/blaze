//! A workspace's names, as blaze-scope's `workspace` example makes them (its phases 1 to 3:
//! the unit graph from `Cargo.lock` and the members' manifests, the toolchain from the shelf,
//! each crate's files from the checkout, the fetch store's records or the toolchain's packs,
//! then `resolve_crates` on one thread), for stage h's body calls resolved through
//! `Names::item_at`. Everything is leaked for the run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use blaze_fetch::{PackView, SourceId};
use blaze_manifest::layout::{Entry, Layout, Need, Pass as LayoutPass};
use blaze_manifest::lock::{Lock, LockId, LockSource};
use blaze_own::{AVec, Alloc};
use blaze_scope::resolve::{Names, SourceTexts, resolve_crates};
use blaze_scope::task::Serial;
use blaze_scope::units::{Member, Origin, UnitCrates, unit_crates};
use blaze_toolchain::{Shelf, ToolchainView};
use blaze_unit_graph::{Ask, CfgAtom, Package, Pass, Records, Resolution, Resolver, StoreSources, TargetDesc};

const TRIPLE: &str = "aarch64-apple-darwin";

/// The target's cfg without `panic`: that atom is the program's strategy, added by [`mac`].
const MAC_CFG: &[(&str, Option<&str>)] = &[
    ("target_abi", Some("")),
    ("target_arch", Some("aarch64")),
    ("target_endian", Some("little")),
    ("target_env", Some("")),
    ("target_family", Some("unix")),
    ("target_feature", Some("aes")),
    ("target_feature", Some("crc")),
    ("target_feature", Some("dit")),
    ("target_feature", Some("dotprod")),
    ("target_feature", Some("dpb")),
    ("target_feature", Some("dpb2")),
    ("target_feature", Some("fcma")),
    ("target_feature", Some("fhm")),
    ("target_feature", Some("flagm")),
    ("target_feature", Some("fp16")),
    ("target_feature", Some("frintts")),
    ("target_feature", Some("jsconv")),
    ("target_feature", Some("lor")),
    ("target_feature", Some("lse")),
    ("target_feature", Some("neon")),
    ("target_feature", Some("paca")),
    ("target_feature", Some("pacg")),
    ("target_feature", Some("pan")),
    ("target_feature", Some("pmuv3")),
    ("target_feature", Some("ras")),
    ("target_feature", Some("rcpc")),
    ("target_feature", Some("rcpc2")),
    ("target_feature", Some("rdm")),
    ("target_feature", Some("sb")),
    ("target_feature", Some("sha2")),
    ("target_feature", Some("sha3")),
    ("target_feature", Some("ssbs")),
    ("target_feature", Some("vh")),
    ("target_has_atomic", Some("128")),
    ("target_has_atomic", Some("16")),
    ("target_has_atomic", Some("32")),
    ("target_has_atomic", Some("64")),
    ("target_has_atomic", Some("8")),
    ("target_has_atomic", Some("ptr")),
    ("target_os", Some("macos")),
    ("target_pointer_width", Some("64")),
    ("target_vendor", Some("apple")),
    ("unix", None),
];

/// What [`load`] made.
pub struct Loaded {
    pub names: &'static Names<'static>,
    pub crates: &'static UnitCrates<'static>,
    /// By crate index: whether it is a workspace member's (its files are the checkout's).
    pub member: Vec<bool>,
    pub resolve_ms: f64,
    /// The proc-macro runner, when a module store was given.
    pub procs: Option<&'static Procs>,
    /// By crate index: its build script's outputs record (blaze-world's `ScriptRecords`, filled
    /// from a folder of records), `None` for a crate with no script or no record given.
    pub scripts: Vec<Option<blaze_build_script::OutputsRef<'static>>>,
    /// The unit graph the crates were made from: a crate of [`Origin::Unit`] is the unit at
    /// that index (its package's name, its kind: lib, bin, test unit, integration test).
    pub graph: &'static blaze_unit_graph::UnitGraph<'static>,
    texts: &'static Texts,
}

impl Loaded {
    /// The sources a body's macro calls expand with (front2's `Store::sources`): the same
    /// texts, environment and proc-macro runner the names' items were expanded with.
    pub fn body_sources(&self) -> &'static dyn blaze_ladder::front2::body_host::BodySources {
        leak(blaze_ladder::front2::body_host::Sourced(self.texts))
    }

    /// The bytes of the file `path` (the names' path space: [`blaze_scope::units::UnitCrate::shown_from`]
    /// then the path in the package) of crate `krate`, `None` when there is no such file.
    pub fn file_bytes(&self, krate: usize, path: &str) -> Option<&'static [u8]> {
        self.texts.bytes(krate, path)
    }

    /// What bounds crate `krate`'s includes (D60): its origin and its package's folder under
    /// its root.
    pub fn read_root(&self, krate: usize) -> Option<blaze_expand::ReadRoot<'static>> {
        let texts: &'static Texts = self.texts;
        let (origin, package, _) = texts.roots.get(krate)?;
        Some(blaze_expand::ReadRoot { origin: *origin, package: package.as_str() })
    }

    /// The bytes of the file `path` under crate `krate`'s root ([`Self::read_root`]).
    pub fn root_bytes(&self, krate: usize, path: &str) -> Option<&'static [u8]> {
        let (_, _, place) = self.texts.roots.get(krate)?;
        match place {
            Place::Disk(dir) => std::fs::read(dir.join(path)).ok().map(|b| &*Box::leak(b.into_boxed_slice())),
            Place::Pack(view, dir) => view.find(&format!("{dir}{path}")).map(|file| file.bytes),
        }
    }
}

/// Why the names could not be made: what and where.
#[derive(Debug)]
pub enum LoadError {
    Read { path: PathBuf, error: std::io::Error },
    Lock(String),
    Layout(String),
    Graph(String),
    Toolchain(String),
    Crates(String),
    Names(String),
    /// The proc-macro runner could not be made: what and where.
    Procs(String),
    /// The infer frame could not be made.
    Frame(blaze_mem::MemError),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Read { path, error } => write!(f, "{}: {error}", path.display()),
            LoadError::Lock(e) => write!(f, "Cargo.lock: {e}"),
            LoadError::Layout(e) => write!(f, "the workspace's layout: {e}"),
            LoadError::Graph(e) => write!(f, "the unit graph: {e}"),
            LoadError::Toolchain(e) => write!(f, "the toolchain: {e}"),
            LoadError::Crates(e) => write!(f, "the workspace's crates: {e}"),
            LoadError::Names(e) => write!(f, "the names: {e}"),
            LoadError::Procs(e) => write!(f, "the proc-macro runner: {e}"),
            LoadError::Frame(e) => write!(f, "the infer frame: {e:?}"),
        }
    }
}

fn leak<T>(value: T) -> &'static T {
    Box::leak(Box::new(value))
}

fn leak_str(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

/// The target's desc for the program's units, `panic` the program's strategy (its `cfg(panic)`).
fn mac(alloc: Alloc<'static>, panic: blaze_ladder::run::Panic) -> TargetDesc<'static> {
    let mut cfg = AVec::new_in(alloc);
    for (name, value) in MAC_CFG {
        cfg.push(match value {
            Some(v) => CfgAtom::pair(name, v, alloc),
            None => CfgAtom::flag(name, alloc),
        });
    }
    cfg.push(CfgAtom::pair(blaze_ladder::panic_strategy::CFG_KEY, blaze_ladder::panic_strategy::cfg_value(panic), alloc));
    TargetDesc::new(TRIPLE, cfg, alloc)
}

fn layout(anchor: &Path, alloc: Alloc<'static>) -> Result<blaze_manifest::layout::Units<'static>, LoadError> {
    let mut reader = Layout::new("", alloc).map_err(|e| LoadError::Layout(e.to_string()))?;
    for _ in 0..64 {
        let needs = match reader.pass() {
            Ok(LayoutPass::Done(units)) => return Ok(units),
            Ok(LayoutPass::Need(needs)) => needs,
            Err(e) => return Err(LoadError::Layout(e.to_string())),
        };
        for need in needs.iter() {
            match need {
                Need::Manifest(path) => {
                    let text = std::fs::read_to_string(anchor.join(path.as_str())).ok();
                    reader.manifest(path, text.as_deref()).map_err(|e| LoadError::Layout(format!("{}: {e}", path.as_str())))?;
                }
                Need::Listing(dir) => {
                    let listed: Option<Vec<(String, bool)>> = std::fs::read_dir(anchor.join(dir.as_str())).ok().map(|listed| {
                        let mut entries: Vec<(String, bool)> = listed
                            .flatten()
                            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.file_type().is_ok_and(|t| t.is_dir())))
                            .collect();
                        entries.sort();
                        entries
                    });
                    let entries: Option<Vec<Entry<'_>>> =
                        listed.as_ref().map(|l| l.iter().map(|(name, is_dir)| Entry { name, is_dir: *is_dir }).collect());
                    reader.listing(dir, entries.as_deref());
                }
            }
        }
    }
    Err(LoadError::Layout("no layout after 64 passes".to_string()))
}

struct Folder {
    records: PathBuf,
}

impl Records<'static> for Folder {
    fn record(&mut self, source: &SourceId<'_>) -> Option<&'static [u8]> {
        let name: String = source.key().0.iter().map(|b| format!("{b:02x}")).collect();
        let path = self.records.join(name);
        match std::fs::read(&path) {
            Ok(bytes) => Some(Box::leak(bytes.into_boxed_slice())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => panic!("{}: {e}", path.display()),
        }
    }
}

struct DirShelf {
    place: String,
}

impl Shelf for DirShelf {
    type Error = std::io::Error;
    type Bytes = Vec<u8>;

    fn place(&self) -> &str {
        &self.place
    }

    fn size(&mut self, name: &str) -> Result<Option<u64>, std::io::Error> {
        match std::fs::metadata(format!("{}/{name}", self.place)) {
            Ok(m) => Ok(Some(m.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn read(&mut self, name: &str, _size: u64) -> Result<Vec<u8>, std::io::Error> {
        std::fs::read(format!("{}/{name}", self.place))
    }
}

enum Place {
    Disk(PathBuf),
    Pack(PackView<'static>, String),
}

struct Texts {
    places: Vec<(String, Place)>,
    /// By crate index: what bounds its includes (D60) and the place its root's files are read
    /// from (a member's source root on disk, a fetched record's root).
    roots: Vec<(blaze_expand::SourceOrigin, String, Place)>,
    procs: Option<&'static Procs>,
    /// By crate index: the environment `env!` reads in its items and bodies alike
    /// (`UnitCrates::env_of`, after the scripts' records are joined).
    envs: Vec<blaze_scope::crate_env::CrateEnv<'static>>,
}

/// The source root of the workspace at `root` (decision D60): the nearest folder at or above it
/// holding `.git`, else the workspace's root itself. The driver's one look at the machine for
/// it, made when the workspace is loaded.
fn source_root(root: &Path) -> (blaze_expand::SourceOrigin, PathBuf) {
    let mut at = Some(root);
    while let Some(dir) = at {
        if dir.join(".git").exists() {
            return (blaze_expand::SourceOrigin::Checkout, dir.to_path_buf());
        }
        at = dir.parent();
    }
    (blaze_expand::SourceOrigin::Workspace, root.to_path_buf())
}

/// The proc-macro runner of one run: blaze-proc-macro's hook over a module store, each
/// proc-macro crate registered by its lock identity, and by crate index the package and
/// edition a call written in that crate runs in.
pub struct Procs {
    pub procs: blaze_proc_macro::hook::ProcMacros<'static>,
    /// By crate index: the key the runner knows a proc-macro crate by (`None`: not one, or not
    /// registered).
    keys: Vec<Option<blaze_id::CrateItemKey>>,
    pub editions: Vec<blaze_surface::Edition>,
    /// By crate index: the package a call written in that crate is expanded in (its folder,
    /// and its environment set: [`unit_env`]).
    pub packages: Vec<std::sync::Arc<blaze_proc_macro::sandbox::Package>>,
}

impl Procs {
    /// Whether crate `krate` is a proc-macro crate the runner registered (its module found or
    /// compiled): `false` for one it could not register, whose macros' calls are refused.
    pub fn registered(&self, krate: usize) -> bool {
        self.keys.get(krate).is_some_and(Option::is_some)
    }
}

/// The variables cargo sets for compiling a unit that the manifest decides (the cargo book,
/// "Environment variables Cargo sets for crates"): `CARGO_PKG_NAME`, `CARGO_PKG_VERSION` and
/// its `_MAJOR`, `_MINOR`, `_PATCH`, `_PRE` parts, `CARGO_CRATE_NAME`, and
/// `CARGO_MANIFEST_DIR` / `CARGO_MANIFEST_PATH`, derived from `seen`, the package's mount row (D96:
/// `/package` for a fetched package, `/workspace/<dir>` for a member), never the machine's path, so
/// no key holds where the checkout is. The `[package]` metadata family (`CARGO_PKG_AUTHORS`,
/// `_DESCRIPTION`, `_HOMEPAGE`, `_REPOSITORY`, `_LICENSE`, `_LICENSE_FILE`, `_RUST_VERSION`,
/// `_README`) is not read from the manifest yet: a call asking for one is refused, never
/// answered unset ([`CARGO_SET`]).
fn unit_env(package: &str, version: &str, crate_name: &str, seen: blaze_expand::seen::SeenRoot<'_>) -> Vec<(String, String)> {
    let dir = seen.to_string();
    let (core, pre) = version.split_once('-').unwrap_or((version, ""));
    let core = core.split_once('+').map_or(core, |(c, _)| c);
    let mut parts = core.splitn(3, '.');
    let mut env = vec![
        ("CARGO_PKG_NAME".to_string(), package.to_string()),
        ("CARGO_PKG_VERSION".to_string(), version.to_string()),
        ("CARGO_PKG_VERSION_MAJOR".to_string(), parts.next().unwrap_or("").to_string()),
        ("CARGO_PKG_VERSION_MINOR".to_string(), parts.next().unwrap_or("").to_string()),
        ("CARGO_PKG_VERSION_PATCH".to_string(), parts.next().unwrap_or("").to_string()),
        ("CARGO_PKG_VERSION_PRE".to_string(), pre.split_once('+').map_or(pre, |(p, _)| p).to_string()),
        ("CARGO_CRATE_NAME".to_string(), crate_name.replace('-', "_")),
        ("CARGO_MANIFEST_PATH".to_string(), format!("{dir}/Cargo.toml")),
        ("CARGO_MANIFEST_DIR".to_string(), dir),
    ];
    env.sort();
    env
}

/// Every variable cargo sets for a unit whatever its manifest says (the cargo book's list for
/// crates; `OUT_DIR` only with a build script): one of these missing from a unit's set is a
/// fact this run does not know, so `env!` of it is refused, not answered unset.
/// (`CARGO` is unset by rule, as blaze-scope crate_env.rs `CARGO_ALWAYS` says.)
pub const CARGO_SET: &[&str] = &[
    "CARGO_PKG_AUTHORS",
    "CARGO_PKG_DESCRIPTION",
    "CARGO_PKG_HOMEPAGE",
    "CARGO_PKG_REPOSITORY",
    "CARGO_PKG_LICENSE",
    "CARGO_PKG_LICENSE_FILE",
    "CARGO_PKG_RUST_VERSION",
    "CARGO_PKG_README",
    "CARGO_PRIMARY_PACKAGE",
    "CARGO_BIN_NAME",
    "OUT_DIR",
];

/// The runner over the module store `modules`, compiling modules and keeping records under
/// `out`: each proc-macro unit registered by its lock identity (a registry checksum, a path
/// package's files as they are now, a git commit and the folder the store's module identity
/// names, as the lock does not say it). A crate it cannot register is printed and left
/// without a key: its macros' calls are refused, one note per crate.
/// Each crate's calls are shown its OWN package (blaze-world `procs::files`, the rule's one
/// home): `places[i]` is crate `i`'s place as [`load`] read its sources, so a member gets its
/// folder and a fetched package its own fetch record's files and manifest; never the
/// workspace's root (whose manifest, a virtual workspace's, has no `[dependencies]`:
/// proc-macro-crate's lookup in jsonrpsee's `#[rpc(server)]` failed on it).
#[expect(clippy::too_many_arguments, reason = "the run's facts, each read once by `load`")]
fn procs_of(
    root: &Path,
    modules: &Path,
    out: &Path,
    crates: &UnitCrates<'static>,
    graph: &blaze_unit_graph::UnitGraph<'static>,
    lock: &Lock<'static>,
    member_dirs: &BTreeMap<u32, PathBuf>,
    places: &[(String, Place)],
    heap: &'static blaze_own::Heap,
) -> Result<Procs, LoadError> {
    use blaze_proc_macro::hook::ProcMacros;
    use blaze_world::procs::files::{PackageAt, sandbox_files};
    let runtime: &'static blaze_proc_macro::host::Runtime =
        leak(blaze_proc_macro::host::Runtime::new(heap).map_err(|e| LoadError::Procs(format!("the proc-macro runtime: {e}")))?);
    let records: &'static blaze_proc_macro::record::DirStore = leak(blaze_proc_macro::record::DirStore::new(out));
    let mut procs = ProcMacros::new(runtime, records, blaze_proc_macro::host::Limits::STANDARD);
    let mut macro_units = Vec::new();
    let mut keys = vec![None; crates.crates.len()];
    let mut packages = Vec::with_capacity(crates.crates.len());
    for (i, c) in crates.crates.iter().enumerate() {
        let unit = match c.origin {
            Origin::Unit(u) => Some(&graph.units[u.index()]),
            Origin::Toolchain { .. } => None,
        };
        let version = unit.map_or("", |u| u.version.as_str());
        let package = unit.map_or(c.name, |u| u.name.as_str());
        let (_, place) = places
            .get(i)
            .ok_or_else(|| LoadError::Procs(format!("crate {} ({i}) has no place: {} places for {} crates", c.name, places.len(), crates.crates.len())))?;
        let files = match (unit, place) {
            (None, _) => sandbox_files(PackageAt::Toolchain),
            // A member (D96 (3)): its workspace served at `/workspace`, the member at its dir in
            // it (`load` placed a member at `root.join(<its dir>)`), so a macro's
            // `$CARGO_MANIFEST_DIR/../sibling/x` lands in the workspace (D111), never refused as
            // leaving `/package`.
            (Some(_), Place::Disk(dir)) => {
                let rel = dir
                    .strip_prefix(root)
                    .ok()
                    .and_then(Path::to_str)
                    .ok_or_else(|| LoadError::Procs(format!("crate {}: the member at {} is not a UTF-8 folder under the workspace {}", c.name, dir.display(), root.display())))?;
                let member = blaze_proc_macro::script_sandbox::ScriptRoot::member(rel).map_err(|e| LoadError::Procs(format!("crate {}: {e}", c.name)))?;
                blaze_proc_macro::sandbox::Files::Member { workspace: root.to_path_buf(), root: member }
            }
            (Some(_), Place::Pack(view, dir)) => sandbox_files(PackageAt::Pack { view: *view, dir: dir.as_str() }),
        };
        let env = unit_env(package, version, c.name, files.seen());
        let unknown: Vec<String> = CARGO_SET.iter().filter(|name| !env.iter().any(|(set, _)| set == *name)).map(|name| (*name).to_string()).collect();
        let env = blaze_proc_macro::package_env::PackageEnv::new(env, unknown, Vec::new())
            .map_err(|e| LoadError::Procs(format!("crate {} ({i}): its variables: {e}", c.name)))?;
        let shown = blaze_proc_macro::sandbox::Package::new(files, env).map_err(|e| LoadError::Procs(format!("crate {} ({i}): {e}", c.name)))?;
        packages.push(std::sync::Arc::new(shown));
        if let Some(unit) = unit
            && matches!(unit.kind, blaze_unit_graph::UnitKind::ProcMacro)
        {
            macro_units.push((i, unit));
        }
    }
    let path_dir = |id: blaze_manifest::lock::LockId| member_dirs.get(&id.0).cloned();
    let made = blaze_world::procs::register_macro_crates(&mut procs, modules, lock, macro_units, path_dir).map_err(|e| LoadError::Procs(e.to_string()))?;
    for (i, why) in &made.unregistered {
        println!("  procs: crate {}: not registered: {why:?}", crates.crates[*i].name);
    }
    for (i, key) in &made.keys {
        keys[*i] = Some(*key);
    }
    let editions = crates.crates.iter().map(|c| c.edition).collect();
    Ok(Procs { procs, keys, editions, packages })
}

impl Texts {
    /// A file's bytes, for `include_bytes!` and the other includes (read as they are, never
    /// as text first).
    fn bytes(&self, krate: usize, path: &str) -> Option<&'static [u8]> {
        let (prefix, place) = self.places.get(krate)?;
        let rest = path.strip_prefix(prefix.as_str())?;
        match place {
            Place::Disk(dir) => std::fs::read(dir.join(rest)).ok().map(|b| &*Box::leak(b.into_boxed_slice())),
            Place::Pack(view, dir) => view.find(&format!("{dir}{rest}")).map(|file| file.bytes),
        }
    }
}

impl SourceTexts<'static> for Texts {
    fn crate_env(&self, krate: usize) -> Option<&blaze_scope::crate_env::CrateEnv<'static>> {
        self.envs.get(krate)
    }

    fn proc_macro_key(&self, krate: usize) -> Option<blaze_id::CrateItemKey> {
        self.procs?.keys.get(krate).copied().flatten()
    }

    fn run_proc_macro<'b>(
        &self,
        krate: usize,
        def: &blaze_expand::MacroDefinition<'_>,
        call: blaze_expand::ProcCall<'_>,
        alloc: Alloc<'b>,
    ) -> Result<blaze_expand::Produced<'b>, blaze_expand::HostError> {
        let refused = blaze_expand::HostError { ask: blaze_expand::HostAsk::ProcMacro, why: blaze_expand::HostWhy::Refused };
        let Some(p) = self.procs else { return Err(refused) };
        p.procs.run(def, &call, p.editions[krate], &p.packages[krate], alloc)
    }

    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        let (prefix, place) = self.places.get(krate)?;
        let rest = path.strip_prefix(prefix.as_str())?;
        match place {
            Place::Disk(dir) => std::fs::read_to_string(dir.join(rest)).ok().map(leak_str),
            Place::Pack(view, dir) => {
                let file = view.find(&format!("{dir}{rest}"))?;
                core::str::from_utf8(file.bytes).ok()
            }
        }
    }
}

/// The names of the workspace at `root`, its dependencies read from the fetch store `store`
/// (the folder holding `records/`), the toolchain from `$HOME/.blaze/v2/toolchain`.
/// With `modules` (the proc-macro module store, and a folder of this run's for compiled modules
/// and expansion records), each proc-macro crate gets the runner ([`Procs`]); without it every
/// proc macro's call is refused.
/// `tests`: each member's library is also a test unit (Cargo's "Development dependencies": its
/// `#[cfg(test)]` modules walked, its dev-dependencies in scope), beside its library unit.
pub fn load(root: &Path, store: &Path, modules: Option<(&Path, &Path)>, scripts: Option<&Path>, tests: bool, heap: &'static blaze_own::Heap) -> Result<Loaded, LoadError> {
    let alloc = heap.alloc();
    let lock_path = root.join("Cargo.lock");
    let bytes = std::fs::read(&lock_path).map_err(|error| LoadError::Read { path: lock_path.clone(), error })?;
    let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    let lock: &'static Lock<'static> = leak(Lock::read(bytes, alloc).map_err(|e| LoadError::Lock(e.to_string()))?);
    let ws = layout(root, alloc)?;
    let written = ws.resolver.as_ref().map(|(r, _)| r.as_str());
    let resolver = Resolver::of(written, ws.root_edition).map_err(|_| LoadError::Layout(format!("unknown resolver {written:?}")))?;
    let mut roots = AVec::new_in(alloc);
    let mut member_ids: Vec<LockId> = Vec::new();
    let mut members: Vec<Member<'static>> = Vec::new();
    let mut member_dirs: BTreeMap<u32, PathBuf> = BTreeMap::new();
    let mut facts = Vec::new();
    for m in ws.members.iter() {
        let Some(id) = lock.named(&m.name).find(|id| matches!(lock.package(*id).source, LockSource::Path)) else { continue };
        roots.push(id);
        member_ids.push(id);
        let dir: &'static str = Box::leak(m.dir.as_str().to_owned().into_boxed_str());
        members.push(Member::new(id, dir).map_err(|e| LoadError::Crates(e.to_string()))?);
        member_dirs.insert(id.0, root.join(m.dir.as_str()));
        facts.push((id, Package::from_member(m, alloc)));
    }
    let ask: &'static Ask<'static> =
        leak(Ask { roots, features: AVec::new_in(alloc), all_features: false, no_default_features: false, tests });
    // The program's panic strategy: tempo's `[profile.dev]` writes no `panic`, and this loader
    // reads no profile, so Rust's default, named (manifest-panic replaces it with the profile's).
    let target: &'static TargetDesc<'static> = leak(mac(alloc, blaze_ladder::run::Panic::Unwind));
    let mut res = Resolution::new(lock, target, target, ask, resolver, alloc);
    for (id, p) in facts {
        res.member(id, p).map_err(|e| LoadError::Graph(e.to_string()))?;
    }
    let folder = Folder { records: store.join("records") };
    let mut sources = StoreSources::new(lock, folder, PackView::open_source_put, alloc).map_err(|e| LoadError::Graph(e.to_string()))?;
    let mut passes = 0;
    let graph = loop {
        passes += 1;
        if passes > 64 {
            return Err(LoadError::Graph("no graph after 64 passes".to_string()));
        }
        match res.pass().map_err(|e| LoadError::Graph(e.to_string()))? {
            Pass::Done(graph) => break graph,
            Pass::Partial(partial) => return Err(LoadError::Graph(format!("the manifests of {:?} are missing: no unit graph", partial.missing()))),
            Pass::Need(needs) => {
                for id in needs.iter() {
                    let facts = sources.package(*id).map_err(|e| LoadError::Graph(e.to_string()))?;
                    res.package(*id, Some(facts)).map_err(|e| LoadError::Graph(e.to_string()))?;
                }
            }
        }
    };
    let graph = leak(graph);

    let home = std::env::var("HOME").map_err(|_| LoadError::Toolchain("HOME is not set".to_string()))?;
    let mut shelf = DirShelf { place: format!("{home}/.blaze/v2/toolchain") };
    let id = blaze_toolchain::PINNED;
    let manifest = blaze_toolchain::shelf::open_manifest(&mut shelf, id).map_err(|e| LoadError::Toolchain(format!("{}: {e}", shelf.place)))?;
    let manifest: &'static [u8] = Box::leak(manifest.into_boxed_slice());
    let view = ToolchainView::open(manifest, id).map_err(|e| LoadError::Toolchain(e.to_string()))?;
    // Each build script's record is joined to its crate BEFORE anything reads the list: the
    // script's `rustc-cfg` atoms are part of the cfg the crate's names are resolved under
    // (nix's `apple_targets`, rustix's `libc`, curve25519-dalek's `curve25519_dalek_bits="64"`)
    // and of its key.
    let mut unit_list = unit_crates(graph, lock, &members, view, TRIPLE, alloc).map_err(|e| LoadError::Crates(e.to_string()))?;
    // A crate refused alone (a member proc macro with no wasm32-wasip1 cut) is still in the
    // list, with no deps: its paths would read as NotFound, so say why first.
    for r in unit_list.refused.iter() {
        println!("  units: crate {} ({}): refused: {}", unit_list.crates[r.krate].name, r.krate, r.why);
    }
    let script_list = script_records(scripts, graph, &unit_list)?;
    unit_list.join_scripts(&script_list).map_err(|e| LoadError::Crates(e.to_string()))?;
    let crates = leak(unit_list);
    let mut places = Vec::with_capacity(crates.crates.len());
    let mut read_roots = Vec::with_capacity(crates.crates.len());
    let (member_origin, checkout) = source_root(root);
    let mut member = Vec::with_capacity(crates.crates.len());
    let mut packs: BTreeMap<u32, PackView<'static>> = BTreeMap::new();
    for c in crates.crates.iter() {
        let place = match c.origin {
            Origin::Toolchain { krate, .. } => {
                let view = match packs.get(&krate) {
                    Some(view) => *view,
                    None => {
                        let row = view.krate(krate);
                        let bytes = blaze_toolchain::shelf::open_pack(&mut shelf, id, &row).map_err(|e| LoadError::Toolchain(e.to_string()))?;
                        let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
                        let opened = PackView::open_verified(bytes)
                            .map_err(|_| LoadError::Toolchain(format!("the pack of `{}` is no pack", row.name)))?;
                        packs.insert(krate, opened);
                        opened
                    }
                };
                member.push(false);
                read_roots.push((blaze_expand::SourceOrigin::Package, String::new(), Place::Pack(view, String::new())));
                Place::Pack(view, String::new())
            }
            Origin::Unit(u) => {
                let package = graph.units[u.index()].package;
                match member_dirs.get(&package.0) {
                    Some(dir) => {
                        member.push(true);
                        let under = dir.strip_prefix(&checkout).map_err(|_| {
                            LoadError::Layout(format!("the member {} is not under the source root {}", dir.display(), checkout.display()))
                        })?;
                        let under = under.to_str().ok_or_else(|| LoadError::Layout(format!("the member {} is not UTF-8", dir.display())))?;
                        read_roots.push((member_origin, under.to_string(), Place::Disk(checkout.clone())));
                        Place::Disk(dir.clone())
                    }
                    None => {
                        member.push(false);
                        let (view, dir) = sources.files(package).map_err(|e| LoadError::Graph(e.to_string()))?;
                        let origin = match lock.package(package).source {
                            LockSource::Git { .. } => blaze_expand::SourceOrigin::GitCheckout,
                            LockSource::Registry { .. } | LockSource::Path => blaze_expand::SourceOrigin::Package,
                        };
                        read_roots.push((origin, dir.as_str().to_string(), Place::Pack(view, String::new())));
                        let dir = if dir.is_empty() { String::new() } else { format!("{}/", dir.as_str()) };
                        Place::Pack(view, dir)
                    }
                }
            }
        };
        places.push((c.shown_from.as_str().to_string(), place));
    }
    let procs = match modules {
        Some((modules, out)) => {
            Some(&*leak(procs_of(root, modules, out, crates, graph, lock, &member_dirs, &places, heap)?))
        }
        None => None,
    };
    let frame: &'static blaze_mem::Frame = leak(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).map_err(LoadError::Frame)?);
    let bump = frame.alloc(blaze_own::Erased::new(frame)).bump();
    let mut envs = Vec::with_capacity(crates.crates.len());
    for at in 0..crates.crates.len() {
        envs.push(crates.env_of(at, graph, bump).map_err(|e| LoadError::Crates(e.to_string()))?);
    }
    let texts: &'static Texts = leak(Texts { places, roots: read_roots, procs, envs });
    // `inputs` carries each joined record (`UnitCrates::join_scripts`, above).
    let scripts = script_list;
    // A refused crate (a member proc macro the shelf has no wasm32-wasip1 side for) is never
    // resolved with no dependencies (rule 3): the load stops, naming it and the fix.
    let inputs = leak(crates.checked_inputs().map_err(|refused| LoadError::Crates(refused.why.to_string()))?);
    let start = Instant::now();
    let names = resolve_crates(inputs, crates.sysroot, texts, &Serial, bump).map_err(|e| LoadError::Names(e.to_string()))?;
    let resolve_ms = start.elapsed().as_secs_f64() * 1e3;
    Ok(Loaded { names: leak(names), crates, member, resolve_ms, procs, scripts, graph, texts })
}

/// By crate index, the outputs record of the build script serving it, from the folder `dir`
/// of records blaze-proc-macro's `tempo_scripts export` writes
/// (`<name>@<version>@<host|target>.outputs`, the naming blaze-world's `examples/names.rs
/// --scripts` reads), opened through blaze-world's `ScriptRecords` (the one reader of them).
fn script_records(
    dir: Option<&Path>,
    graph: &'static blaze_unit_graph::UnitGraph<'static>,
    crates: &UnitCrates<'static>,
) -> Result<Vec<Option<blaze_build_script::OutputsRef<'static>>>, LoadError> {
    let Some(dir) = dir else { return Ok(vec![None; crates.crates.len()]) };
    let mut given = Vec::new();
    // A macro-side crate (`UnitCrates::macro_from`) is built for wasm32-wasip1: its build
    // script's record would be that run's, which no exporter writes yet. The host's record is
    // another side's facts (rule 21), so none is joined: the crate is counted apart in
    // `build_scripts`.
    for c in crates.crates.iter().take(crates.macro_from) {
        let Origin::Unit(u) = c.origin else { continue };
        let unit = &graph.units[u.index()];
        if unit.build_script.is_none() {
            continue;
        }
        let side = if unit.for_host { "host" } else { "target" };
        let path = dir.join(format!("{}@{}@{side}.outputs", unit.name.as_str(), unit.version.as_str()));
        match std::fs::read(&path) {
            Ok(bytes) => given.push((u, &*Box::leak(bytes.into_boxed_slice()))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(LoadError::Read { path, error }),
        }
    }
    let records = blaze_world::ScriptRecords::new(given, Alloc::global()).map_err(|e| LoadError::Graph(e.to_string()))?;
    // A native script's stored files are beside the records, in `<dir>/files/<sha256 hex>`
    // (`tempo_scripts export`): mapped when an `include!` reads one. A folder without them
    // gives no area, and such an include is refused naming the missing area (`NoArea`).
    let files = dir.join("files");
    let area = leak(blaze_world::StoredFiles::new(files.is_dir().then_some(files), records.stored_digests()));
    let records = leak(records.with_area(area));
    Ok(crates
        .crates
        .iter()
        .enumerate()
        .map(|(at, c)| match c.origin {
            Origin::Unit(u) if !crates.macro_side(at) => records.outputs(u),
            _ => None,
        })
        .collect())
}
