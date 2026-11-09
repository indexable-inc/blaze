//! The workspace load and the unit graph as queries of blaze's database (docs/v2/plan.md;
//! #1450's 238 s load): pure functions of a view's `Cargo.toml`s, lock file and config, and of
//! the sources mounted beside it, so nothing is loaded again on start (the daemon's image has
//! it) and an edit reruns only what read what changed.
//!
//! - A build's view is opened with a [`BuildConfig`] (`blaze_query::Database::open_with`): the
//!   cargo command line, and [`Mount`]s saying where each directory cargo names is.
//! - Every file is read by its absolute path as cargo names it: under a mount of the view
//!   through `Cx::file`, under a mount of another root (a registry or git package, the standard
//!   library) through `Cx::file_at`, each an edge. A path under no mount is never read: the load
//!   asks the daemon to mount it ([`FetchKind::Local`], read from the disk where it is) and is
//!   asked again.
//! - The daemon asks one query per build, [`Plan`]: the units to build ([`Planned::Ready`]),
//!   the sources to fetch and mount first ([`Planned::Needs`]), or cargo's error.
//! - The standard library is one more crate graph loaded the same way, from its own root
//!   ([`StdUnits`]), keyed by nothing of the view, so every view and project shares it.

use std::marker::PhantomData;

use blaze_format::{FormatError, Persist, SchemaHash, Slice, Writer};
use blaze_ids::ContentId;
use blaze_protocol::cbor;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::schema::{Unit, UnitGraph};

/// Lane cargo's query kinds, in `blaze_query::kinds::CARGO` (`0x6000..0x6800`).
pub mod kinds {
    use blaze_query::QueryKind;

    /// [`super::Plan`].
    pub const PLAN: QueryKind = QueryKind(0x6000);
    /// [`super::StdUnits`].
    pub const STD_UNITS: QueryKind = QueryKind(0x6001);
    /// The workspace load's own queries (manifests, config, lock, resolve, units):
    /// `0x6010..0x6400`.
    pub const LOAD: std::ops::Range<u16> = 0x6010..0x6400;
    /// The standard library's own queries (target cfg, std's packages): `0x6400..0x6800`.
    pub const STD: std::ops::Range<u16> = 0x6400..0x6800;
}

/// A value kept as deterministic CBOR in one slice of a buffer: the workspace load's values,
/// read once per build, not hot columns.
pub trait Versioned: Serialize + DeserializeOwned + Send + Sync + 'static {
    /// A new text for any change to the type's shape.
    const SCHEMA: SchemaHash;
}

/// A [`Versioned`] value as a query returns it.
pub struct Cbor<T>(pub T);

/// A [`Cbor`] value read in place: its bytes, decoded on [`CborRef::get`].
pub struct CborRef<'a, T> {
    bytes: &'a [u8],
    of: PhantomData<fn() -> T>,
}

impl<T> Clone for CborRef<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for CborRef<'_, T> {}

impl<'a, T: Versioned> CborRef<'a, T> {
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// The value. Its bytes were written by [`Cbor`]'s `Persist::write` under the same schema,
    /// so a value that does not decode is a bug in the writer, not an input.
    pub fn get(&self) -> T {
        cbor::from_slice(self.bytes).unwrap_or_else(|error| {
            panic!("a kept {} does not decode: {error}", std::any::type_name::<T>())
        })
    }
}

impl<T: Versioned> Persist for Cbor<T> {
    const SCHEMA: SchemaHash = T::SCHEMA;
    type Ref<'a> = CborRef<'a, T>;

    fn write(&self, out: &mut Writer) -> u64 {
        let bytes = cbor::to_vec(&self.0).unwrap_or_else(|error| {
            panic!("a {} has no deterministic CBOR: {error}", std::any::type_name::<T>())
        });
        write_bytes(out, &bytes)
    }

    fn read(buffer: &[u8]) -> Result<CborRef<'_, T>, FormatError> {
        read_bytes(buffer)
    }
}

/// A [`Versioned`] value already encoded, as another query's [`CborRef::bytes`] gave it: the
/// same schema and layout as [`Cbor<T>`], so readers get the same [`CborRef`]. A query that
/// passes on another's value (`StdUnits` of `Foreign`'s) copies its bytes once instead of
/// decoding and encoding it again.
pub struct CborBytes<T> {
    bytes: Vec<u8>,
    of: PhantomData<fn() -> T>,
}

impl<T: Versioned> CborBytes<T> {
    /// `value`'s bytes, as written under `T`'s schema.
    pub fn of(value: CborRef<'_, T>) -> CborBytes<T> {
        CborBytes { bytes: value.bytes.to_vec(), of: PhantomData }
    }

    /// A value encoded here, for a query whose answer is sometimes its own (an error).
    pub fn encode(value: &T) -> CborBytes<T> {
        let bytes = cbor::to_vec(value).unwrap_or_else(|error| {
            panic!("a {} has no deterministic CBOR: {error}", std::any::type_name::<T>())
        });
        CborBytes { bytes, of: PhantomData }
    }
}

impl<T: Versioned> Persist for CborBytes<T> {
    const SCHEMA: SchemaHash = T::SCHEMA;
    type Ref<'a> = CborRef<'a, T>;

    fn write(&self, out: &mut Writer) -> u64 {
        write_bytes(out, &self.bytes)
    }

    fn read(buffer: &[u8]) -> Result<CborRef<'_, T>, FormatError> {
        read_bytes(buffer)
    }
}

fn write_bytes(out: &mut Writer, bytes: &[u8]) -> u64 {
    let data = out.push_slice::<u8>(bytes);
    let at = out.next::<Slice<u8>>();
    out.push(&Slice::<u8>::between(at, data, bytes.len()))
}

fn read_bytes<T: Versioned>(buffer: &[u8]) -> Result<CborRef<'_, T>, FormatError> {
    let slice = blaze_format::open_as::<Slice<u8>>(buffer, T::SCHEMA)?;
    Ok(CborRef { bytes: slice.follow(buffer)?, of: PhantomData })
}

/// Where a mounted directory's files are.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Origin {
    /// The view's own files (`Cx::file`).
    View,
    /// An immutable root of the database's tree (`Cx::file_at`): a registry or git package's
    /// source, a sparse index's file, the standard library.
    Root(ContentId),
}

/// A path as cargo names it, absolute, and where its files are.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Mount {
    /// Absolute, `/`-separated, without a trailing `/`. A path is under the mount when it is
    /// `at` or starts with `at` and a `/`; the longest such mount is the path's.
    pub at: String,
    pub origin: Origin,
    /// What `at` is under the origin's root, `/`-separated ("" for the root itself): a
    /// directory, or a file for a mount of one file.
    pub under: String,
}

/// What a build's database view is opened with (`Database::open_with`) and `Cx::config` reads.
/// Only the workspace load's first query reads it; every other query reads what that one made
/// of it, so a change to a variable no one reads reruns that query alone.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct BuildConfig {
    pub invocation: Invocation,
    /// Sorted by `at`: the view's own root (where the client's checkout is), each fetched
    /// source, the standard library's.
    pub mounts: Vec<Mount>,
    /// The machine's triple, which build scripts and proc macros are built for.
    pub host: String,
    /// Where the standard library's `library/` is named (`/rustc/<commit>/library`, as
    /// rustc's paths into it are), a mount of `mounts`.
    pub library: String,
    /// Where the daemon places the standard library's builds, absolute: the machine's for the
    /// blaze build (`<user cache>/blaze/std/<build id>`), one directory of each [`StdKey`],
    /// the same whatever workspace or `BLAZE_HOME` asks for it.
    pub std: String,
    /// Where the daemon places workspaces' build directories when nothing names one, absolute:
    /// `build` under blaze's root (`blaze_manifest::home::builds`), each workspace's in a
    /// directory of its root's id (`blaze_manifest::home::build_dir`). Nothing of a build is
    /// written into the checkout.
    pub builds: String,
    /// Where the daemon keeps the packages it fetched, absolute: `sources` under blaze's root
    /// (`blaze_manifest::home::sources`). A registry package is at
    /// `<sources>/registry/src/<registry dir>/<name>-<version>` and a git package's checkout
    /// at `<sources>/git/checkouts/<ident>/<commit>` (the whole commit): mounted there, and on
    /// disk there for what reads it outside the VFS (a build script, the C compiler it runs).
    /// Never cargo's `$CARGO_HOME`, where blaze keeps no packages.
    pub sources: String,
    /// The version of the blaze that serves the build: a release's `0.YYYYMMDD.N`, or
    /// `0.0.0-dev` for a build of the tree. `blaze.lock` records it when it is a release run as
    /// the workspace pins it ([`crate::lock`]).
    #[serde(default)]
    pub blaze: String,
}

impl BuildConfig {
    pub fn encode(&self) -> Vec<u8> {
        cbor::to_vec(self).unwrap_or_else(|error| panic!("a build config has no CBOR: {error}"))
    }

    pub fn decode(bytes: &[u8]) -> Result<BuildConfig, String> {
        cbor::from_slice(bytes).map_err(|error| format!("the view's build configuration: {error}"))
    }
}

/// cargo's subcommands that build.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Command {
    Build,
    Check,
    Test,
    Bench,
    /// Builds what `build` does, and the client runs it.
    Run,
    /// Checks what `doc` documents, and the daemon writes its pages from the front end
    /// (`blaze-doc`): no rustdoc runs.
    Doc,
}

impl Command {
    /// The command a word names, with cargo's aliases.
    pub fn named(word: &str) -> Option<Command> {
        match word {
            "build" | "b" => Some(Command::Build),
            "check" | "c" => Some(Command::Check),
            "run" | "r" => Some(Command::Run),
            "test" | "t" => Some(Command::Test),
            "bench" => Some(Command::Bench),
            "doc" | "d" => Some(Command::Doc),
            _ => None,
        }
    }

    /// The command as cargo names it, for `alias.<command>` and its messages.
    pub fn name(self) -> &'static str {
        match self {
            Command::Build => "build",
            Command::Check => "check",
            Command::Run => "run",
            Command::Test => "test",
            Command::Bench => "bench",
            Command::Doc => "doc",
        }
    }

    /// The profile cargo builds with when none is named.
    pub fn profile(self) -> &'static str {
        match self {
            Command::Build | Command::Check | Command::Run | Command::Doc => "dev",
            Command::Test => "test",
            Command::Bench => "bench",
        }
    }
}

/// A cargo command line as the client was run (`blaze_api::Cargo`), or one the daemon made of
/// an agent's `blaze_api::Targets`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Invocation {
    pub command: Command,
    /// After the subcommand, before `--`, as typed.
    pub args: Vec<String>,
    /// Where the view's root is, absolute.
    pub root: String,
    pub cwd: String,
    /// Sorted by name. Never in a query's value but for the variables cargo reads by name.
    pub env: Vec<(String, String)>,
    /// Files outside every mount that cargo reads, by absolute path (`$CARGO_HOME/config.toml`).
    pub files: Vec<(String, Vec<u8>)>,
}

/// The variable `name` of a request's environment ([`Invocation::env`]).
pub fn env_var<'e>(env: &'e [(String, String)], name: &str) -> Option<&'e str> {
    env.iter().find(|(it, _)| it == name).map(|(_, value)| value.as_str())
}

/// The variables of a request's environment ([`Invocation::env`]) that something the daemon
/// keeps from one request to the next read, each by name with the value it read (`None`:
/// unset) and what read it, noted at the read ([`EnvReads::var`]). It is what the kept thing
/// is keyed by in the environment: a request whose other variables differ (a shell's `SHLVL`,
/// `OLDPWD` or `TERM_SESSION_ID`, systemd's `INVOCATION_ID`) reuses it, and one in which a
/// variable it read moved does not (rule 21 both ways). Sorted by name, each once.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct EnvReads {
    read: Vec<(String, Option<String>, &'static str)>,
}

impl EnvReads {
    /// The variable `name` of `env`, noted read by `by` (what a person is told read it: "the
    /// item store's mode"). A name read before with another value is an error naming it: what
    /// is kept would be made from two values of one variable, and its key would hold one.
    pub fn var<'e>(
        &mut self,
        env: &'e [(String, String)],
        name: &str,
        by: &'static str,
    ) -> Result<Option<&'e str>, String> {
        let value = env_var(env, name);
        match self.read.binary_search_by(|it| it.0.as_str().cmp(name)) {
            Ok(at) if self.read[at].1.as_deref() == value => Ok(value),
            Ok(at) => Err(format!(
                // alloc: an error, once
                "`{name}` is read by {by} with another value than {} read it with, into one kept front end: \
                 its key holds the first read alone (rule 21)",
                self.read[at].2
            )),
            Err(at) => {
                // alloc: a variable read, a few per front end
                self.read.insert(at, (name.to_owned(), value.map(str::to_owned), by));
                Ok(value)
            }
        }
    }

    /// The first variable read (by name) whose value in `now` is not the one read: why what
    /// read it can't serve a request with `now`. `None` whatever else of `now` differs.
    pub fn moved(&self, now: &[(String, String)]) -> Option<EnvMoved> {
        self.read.iter().find_map(|(name, was, by)| {
            let value = env_var(now, name);
            (value != was.as_deref()).then(|| EnvMoved {
                name: name.clone(),            // clone: a reason, once per front end let go
                by: (*by).to_owned(),          // alloc: as above
                was: was.clone(),              // clone: as above
                now: value.map(str::to_owned), // alloc: as above
            })
        })
    }

    /// Each variable read, by name, with what read it.
    pub fn names(&self) -> impl Iterator<Item = (&str, &'static str)> {
        self.read.iter().map(|(name, _, by)| (name.as_str(), *by))
    }
}

/// Never the values: a variable may hold a secret.
impl std::fmt::Debug for EnvReads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.read.iter().map(|(name, value, by)| (name, value.is_some(), by)))
            .finish()
    }
}

/// A variable that something kept read with one value, which a request has another of (or
/// has unset, or sets where it was unset): the reason the kept thing does not serve it.
#[derive(Clone, PartialEq, Eq)]
pub struct EnvMoved {
    pub name: String,
    /// What read it, for a person: "`env!` in a v0.1.0 (lib)", "the item store's mode".
    pub by: String,
    pub was: Option<String>,
    pub now: Option<String>,
}

impl std::fmt::Display for EnvMoved {
    /// By name and what read it, never its values: a variable may hold a secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let how = match (&self.was, &self.now) {
            (Some(_), Some(_)) => "has another value now",
            (Some(_), None) => "is unset now",
            (None, _) => "is set now, and was unset",
        };
        write!(f, "the variable `{}`, which {} read, {how}", self.name, self.by)
    }
}

/// As [`Display`](std::fmt::Display): never the values.
impl std::fmt::Debug for EnvMoved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EnvMoved({self})")
    }
}

/// The build of the view, as its [`BuildConfig`] asks: the one query the daemon asks per job.
/// `impl Query for Plan` (cargo-load, `crate::load`): key `()`, value `Cbor<Planned>`, kind
/// [`kinds::PLAN`].
pub struct Plan;

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum Planned {
    /// What the load would read that no mount covers yet: the daemon fetches each, mounts it
    /// at its `at`, opens the view again with the new mounts and asks again.
    Needs(Vec<Fetch>),
    Ready(Units),
    /// cargo's error, as cargo says it.
    Failed(LoadError),
}

impl Versioned for Planned {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo planned 9");
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Fetch {
    /// What it is, as cargo's `Downloaded serde v1.0.219` and `Updating crates.io index` name
    /// it: every file of one index is the index, which cargo reports updating once.
    pub what: String,
    pub url: String,
    pub kind: FetchKind,
    /// Where cargo keeps it, absolute: the mount's `at` once fetched.
    pub at: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum FetchKind {
    /// A `.crate`, checked by its sha256 (hex) and unpacked: a directory.
    Crate { sha256: String },
    /// One file of a sparse index (`config.json`, a package's entry), as served: a file.
    IndexFile,
    /// A git repository's tree at `commit`: a directory.
    Git { commit: String },
    /// A path outside the view and every source, read from the disk where it is: what a
    /// `path` dependency outside the checkout is, or a manifest above one that its workspace
    /// search looks for. `at` is the path the load read. Its files are a root keyed by their
    /// bytes, as a fetched source's are. The daemon mounts:
    ///
    /// - a directory at `at`: the whole tree under it (a root `target/` and every `.git` left
    ///   out, as a checkout's view leaves them);
    /// - a `Cargo.toml` at `at` read for its contents (`probe` false): the whole tree of its
    ///   directory, the package's;
    /// - any other file at `at`, or a `Cargo.toml` only asked whether it is there (`probe`
    ///   true, the workspace search above a package): that file alone;
    /// - nothing at `at`: `at` as absent.
    Local { probe: bool },
}

/// What cargo says when it can't: its message with its causes (`error: ...`, then
/// `Caused by:` and each cause indented, as cargo prints an error), and where.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct LoadError {
    pub what: String,
    /// The manifest, lock file or package it is about.
    pub at: Option<String>,
}

/// A build's units: the graph as `--unit-graph` prints it, with the standard library's units
/// in it (`is_std`), and what cargo passes and writes for each.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Units {
    pub graph: UnitGraph,
    /// One per unit of `graph.units`, in its order.
    pub plans: Vec<UnitPlan>,
    /// Every package of the graph, sorted by id.
    pub packages: Vec<PackagePlan>,
    /// The lock files the build writes (`Cargo.lock` as cargo would write it, `blaze.lock`),
    /// when one changes and the lock policy lets the build change it ([`crate::lock`]).
    pub lock: Option<LockWrite>,
    /// cargo's warnings of the load, each as cargo prints it after `warning: `.
    pub warnings: Vec<String>,
    pub target_dir: String,
    pub build_dir: String,
    /// The profile as cargo's `Finished` line names it: "`dev` profile [unoptimized + debuginfo]".
    pub finished: String,
    /// The workspace's `build-script-link-dirs`, absolute or `${NAME}/rest` for a run to expand
    /// (`blaze_cargo::directives::link_dirs`).
    pub link_dirs: Vec<String>,
    /// The workspace's root manifest lists `sandboxed-tests` in `experimental` under
    /// `[workspace.metadata.blaze]`: every member's tests run sandboxed but those
    /// [`TestSandbox`] takes out.
    pub sandboxed_tests: bool,
    /// The pinned native inputs (`[workspace.metadata.blaze.native.<name>]`, docs/cargo.md,
    /// "Pinned native inputs") some build script run of this graph is given: one per input and
    /// target, sorted by name then target. The daemon fetches and lays out each before any
    /// script runs.
    pub native: Vec<NativeInput>,
}

impl Versioned for Units {
    // 13: `CARGO_PRIMARY_PACKAGE` left the plans' variables ([`Units::is_primary`]).
    // 14: the pinned native inputs ([`Units::native`]).
    // 15: [`LockWrite`] writes `blaze.lock` too, and says what moved.
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo units 15");
}

/// One pinned native input for one target: an archive named by its digest, which blaze fetches,
/// unpacks read-only under its home, and hands to the build scripts of `packages` in the
/// variable `env` (docs/cargo.md, "Pinned native inputs").
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct NativeInput {
    /// The table's name, `llvm` of `[workspace.metadata.blaze.native.llvm]`.
    pub name: String,
    /// The target triple whose archive this is: the platform of the script runs that get it.
    pub target: String,
    /// The variable the scripts read the unpacked directory from.
    pub env: String,
    /// The packages whose build scripts are given it, sorted: each a name, or `name/feature`
    /// for that package's script only when the run has the feature on.
    pub packages: Vec<String>,
    /// Where the archive is downloaded from: `https://`.
    pub url: String,
    /// The archive's sha256, 64 lowercase hex digits, as the pin file says.
    pub sha256: String,
    /// The pin file the URL and digest were read from, absolute: what errors name.
    pub pin: String,
}

impl NativeInput {
    /// Whether the build script of the package named `package`, run with `features` on, is
    /// given this input.
    pub fn gives(&self, package: &str, features: &[String]) -> bool {
        names_script(&self.packages, package, features)
    }
}

/// Whether `packages`, as [`NativeInput::packages`] lists them (`name` or `name/feature`), name
/// the build script of `package` run with `features` on.
pub fn names_script(packages: &[String], package: &str, features: &[String]) -> bool {
    packages.iter().any(|it| match it.split_once('/') {
        Some((name, feature)) => name == package && features.iter().any(|on| on == feature),
        None => it == package,
    })
}

impl Units {
    /// Whether unit `unit` is of a primary package: one a root unit (what the request asked
    /// for) is of, as cargo's `is_primary_package` has it. Its crates see
    /// `CARGO_PRIMARY_PACKAGE=1`. It is of the request alone, never in a plan: a record is
    /// keyed by it only through a crate that read it, so `--workspace` then `-p one` reuses
    /// every unit `one` depends on (rule 21).
    pub fn is_primary(&self, unit: usize) -> bool {
        let pkg = &self.graph.units[unit].pkg_id;
        self.primary_package(pkg)
    }

    /// Whether the package whose manifest is `manifest_path` (a crate's `CARGO_MANIFEST_PATH`)
    /// is primary ([`Units::is_primary`]): how a record's reads, which name a crate by its
    /// package's manifest, are valued with no front end.
    pub fn is_primary_manifest(&self, manifest_path: &str) -> bool {
        self.packages
            .iter()
            .find(|it| it.manifest_path == manifest_path)
            .is_some_and(|it| self.primary_package(&it.id))
    }

    fn primary_package(&self, pkg_id: &str) -> bool {
        self.graph.roots.iter().any(|&root| self.graph.units[root].pkg_id == pkg_id)
    }
}

/// What cargo passes and writes for one unit.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct UnitPlan {
    /// The unit's `checked_as_test`, which the graph's JSON leaves out.
    pub checked_as_test: bool,
    /// cargo's `-C metadata` and `-C extra-filename` (`-0123456789abcdef`, or "").
    pub metadata: String,
    pub extra_filename: String,
    /// cargo's `Metadata::unit_id`, 16 hex digits: what dependents hash, and what names the
    /// unit's `-C extra-filename` and package directory when it has them (a build script's
    /// has no extra filename, so this is the only place a plan read back finds it).
    pub unit_id: String,
    /// The `--cfg`s cargo passes (`feature="x"`, those of the rustflags) and `--check-cfg`s.
    pub cfgs: Vec<String>,
    pub check_cfg: Vec<String>,
    /// The rustc flags of the profile and the config (`[build] rustflags`, `RUSTFLAGS`, the
    /// profile's `rustflags`), as cargo passes them.
    pub rustflags: Vec<String>,
    /// The `[lints]` table as cargo passes it (`--warn=unused_qualifications`, ...).
    pub lints: Vec<String>,
    /// The variables cargo sets for the unit (`CARGO_PKG_*`, `CARGO_MANIFEST_DIR`,
    /// `CARGO_CRATE_NAME`, `CARGO_BIN_EXE_*`, `OUT_DIR`, and a build script's run's), sorted.
    pub env: Vec<(String, String)>,
    /// Every file cargo writes for it, absolute: in `deps/`, `build/`, and the copies it
    /// uplifts (`target/debug/foo`).
    pub outputs: Vec<String>,
    /// A build script's run's `OUT_DIR`, absolute.
    pub out_dir: Option<String>,
    /// The package's `links`.
    pub links: Option<String>,
    /// The package is named in the workspace's `unsandboxed-build-scripts` (or a member says
    /// `build-script = false`): the documented opt-out of docs/cargo.md, nothing else.
    pub unsandboxed: bool,
    /// `target.<triple>.runner` or `CARGO_TARGET_<TRIPLE>_RUNNER` of the unit's platform,
    /// program then arguments; empty for none.
    pub runner: Vec<String>,
    /// The target's `harness`.
    pub harness: bool,
    /// What `check` lints the unit's crate with; `None` for a crate it does not lint (any
    /// other command, a package outside the workspace, the standard library, a build script's
    /// run).
    pub clippy: Option<Clippy>,
    /// The unit is built to run in the macro host ([`crate::schema::Unit::macro_wasm`], which
    /// this follows), a proc macro's or the standard library's for it: its code calls the macro
    /// host's bridge (`proc_macro_bridge`), and a proc macro's links as a WASI reactor the host
    /// loads.
    pub macro_wasm: bool,
}

/// What `check` lints a crate with beyond its compile, as cargo-clippy gives clippy's driver;
/// `--cfg clippy` is among [`UnitPlan::cfgs`].
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Clippy {
    /// The configuration file clippy's search finds (`.clippy.toml` or `clippy.toml`, from
    /// `CLIPPY_CONF_DIR` or the package's directory up), absolute, with its text.
    pub conf: Option<(String, String)>,
    /// What `[workspace.metadata.blaze.lints]` says of the profile the profile lints read
    /// (`blaze_lints::profile`); `clippy.toml`'s keys override it.
    pub profile: Option<LintProfile>,
}

/// The profile keys of `[workspace.metadata.blaze.lints]` (`profile`, `profile-hot-percent`,
/// `profile-cold-percent`): the path absolute against the workspace's root, each percent as
/// its TOML number. Only the path is in the plan; the profile's bytes are keyed where the
/// lints run (`blaze_cargo`'s record key).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct LintProfile {
    pub path: Option<String>,
    pub hot_percent: Option<String>,
    pub cold_percent: Option<String>,
    /// The workspace's root manifest, for errors.
    pub manifest: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct PackagePlan {
    /// As `cargo metadata` writes it.
    pub id: String,
    pub name: String,
    pub version: String,
    pub manifest_path: String,
    /// What cargo prints after the version in `Compiling`: `(/abs/path)`, `(url#rev)`, none.
    pub source: Option<String>,
    pub is_member: bool,
    /// The manifest's `package.default-run`.
    pub default_run: Option<String>,
    /// For a crates.io package, the SHA-256 of its `.crate` as the lock file names it (what the
    /// download was checked against); `None` for any other source. How `blaze_global::Rule`
    /// names a public crate (`Origin::CratesIo`).
    pub checksum: Option<[u8; 32]>,
    /// What a member's `[package.metadata.blaze.sandbox]` says of its tests; nothing for any
    /// other package.
    pub tests: TestSandbox,
    /// The file `[package.metadata.blaze] const-macros` names, absolute: a crate of macros the
    /// front end runs, which every crate of the package depends on (docs/future/const-macros.md,
    /// the workspace's experiment `const-macros`). Whether it is there is the front end's to say.
    pub const_macros: Option<String>,
}

impl PackagePlan {
    /// Whether the package's tests run sandboxed, when the build runs any so: `tests = true`
    /// or the workspace's `sandboxed-tests` experiment ([`Units::sandboxed_tests`]), unless
    /// `tests = false`.
    pub fn tests_sandboxed(&self, experiment: bool) -> bool {
        !self.tests.all && (self.tests.opted_in || experiment)
    }
}

/// The tests keys of a member's `[package.metadata.blaze.sandbox]` (docs/cargo.md).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Default, Debug)]
pub struct TestSandbox {
    /// `tests = false`: every test of the package runs outside.
    pub all: bool,
    /// `tests = true`: every test of the package runs sandboxed but those of `tests`, whether
    /// or not the workspace opts in.
    pub opted_in: bool,
    /// `unsandboxed-tests`: the tests, by libtest's names, that run outside; sorted, each once.
    pub tests: Vec<String>,
}

impl TestSandbox {
    /// Whether `unsandboxed-tests` names `test`.
    pub fn outside(&self, test: &str) -> bool {
        self.tests.binary_search_by(|it| it.as_str().cmp(test)).is_ok()
    }
}

/// The lock files a plan writes (docs/cargo.md, "Lock files"; [`crate::lock`]), and what the
/// build says of them.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct LockWrite {
    /// Each lock file that changes, whole: `Cargo.lock` before `blaze.lock`.
    pub files: Vec<LockFile>,
    /// What the build says, each a line: what it wrote, then each move of a file that was there.
    pub said: Vec<String>,
}

impl LockWrite {
    /// The file of `name` (`Cargo.lock`, `blaze.lock`) it writes, if it writes it.
    pub fn file(&self, name: &str) -> Option<&LockFile> {
        self.files.iter().find(|it| it.path.rsplit('/').next() == Some(name))
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct LockFile {
    /// Absolute.
    pub path: String,
    pub text: String,
}

/// The standard library's units for one platform and profile, loaded from `library/` as one
/// more workspace (`-Zbuild-std`'s graph), keyed by nothing of the view.
/// `impl Query for StdUnits` (`crate::stdlib`): key [`StdKey`], value `CborBytes<StdPlanned>`
/// (`Foreign`'s bytes passed on once checked), kind [`kinds::STD_UNITS`].
pub struct StdUnits;

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub struct StdKey {
    /// The library's root and where it is named ([`BuildConfig::library`]).
    pub library: Mount,
    pub triple: String,
    /// The profile of the build, by name, and its settings the standard library is built with
    /// (`panic`, `opt-level`, `debug-assertions`, `overflow-checks`, `debug`, `strip`), as the
    /// user's profile has them.
    pub profile: String,
    pub panic: String,
    pub opt_level: String,
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub debuginfo: String,
    /// The crates asked for, by name: `std`, `core`, `alloc`, `proc_macro`, `test`, or a
    /// `#![no_std]` build's `core` and `alloc` alone.
    pub crates: Vec<String>,
    /// std's features (`panic-unwind`, `backtrace`, ...), as `-Zbuild-std-features` names them.
    pub features: Vec<String>,
    /// The machine build scripts are compiled for and run on ([`BuildConfig::host`]): std's
    /// build scripts' units are host units, and cargo hashes the host into their metadata.
    pub host: String,
    /// The target's rustflags as cargo passes them to every unit of the platform, std's too
    /// (`RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, `[build]`, `[target.<triple>]`): their `-C
    /// target-feature`, `-C target-cpu` and `--cfg` decide the cfg std is resolved and built
    /// with.
    pub rustflags: Vec<String>,
    /// `dev` or `release`: what the build's profile inherits from at the root, whose
    /// `[profile.<root>.package.*]` tables of library/Cargo.toml apply to std.
    pub profile_root: String,
    /// The std of the wasm proc macros ([`crate::schema::Unit::macro_wasm`]), whose code calls
    /// the macro host's bridge: never the same std as a `--target wasm32-wasip1` build's, even
    /// where every other field is equal, so the two are never placed in one directory.
    pub macro_wasm: bool,
    /// Whether its code runs on the machine that builds it ([`crate::schema::runs_here`] of
    /// the platform and profile, as each of its units' codegen reads it): a dev build's host
    /// std is compiled for this machine's own CPU, the std of a `--target` of the host's own
    /// triple for the target's baseline. Two stds, never one directory; a release build's
    /// two are one.
    pub runs_here: bool,
    /// The build's profile's `codegen-backend`: `stencil` compiles std's own functions as
    /// copy-and-patch stencils too (blaze-codegen's `Profile::stencils`), not at the shipped
    /// std's O3. Left out of the key's bytes when unset, so every other std's id stays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codegen_backend: Option<String>,
}

impl StdKey {
    pub const DOMAIN: &'static str = "blaze v2 cargo std key 5";

    pub fn id(&self) -> ContentId {
        crate::load::key_id(StdKey::DOMAIN, self)
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum StdPlanned {
    Ready(StdGraph),
    Failed(LoadError),
}

impl Versioned for StdPlanned {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo std planned 7");
}

/// The standard library's units: a graph of their own, whose units the user's units depend on
/// by `roots`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct StdGraph {
    /// `is_std` set on each; edges by index into `units`.
    pub units: Vec<Unit>,
    pub plans: Vec<UnitPlan>,
    pub packages: Vec<PackagePlan>,
    /// Each crate of [`StdKey::crates`] by name, with its unit's index.
    pub roots: Vec<(String, usize)>,
}

/// Registers every query of the load and of the standard library's graph.
pub fn register(db: &mut blaze_query::Database) {
    crate::load::register(db);
    crate::stdlib::register(db);
}

/// A query's `blaze_query::Query::PRODUCER`: its name and a version, `"blaze v2 cargo <query>
/// <n>"`, the number raised with every change to what the query computes, so no memo of the
/// old code is taken for the new.
pub const fn producer(what: &str) -> ContentId {
    blaze_format::schema(what).0
}

/// A query key's content ID (`blaze_query::Query::input`): its deterministic CBOR under
/// `domain`, hashed in this thread's buffer ([`crate::load::key_id`]), so it allocates nothing.
pub fn key_input<K: Serialize>(domain: &'static str, key: &K) -> ContentId {
    crate::load::key_id(domain, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &[(&str, &str)]) -> Vec<(String, String)> {
        vars.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect()
    }

    /// What a kept front end read of a request's environment is all it is keyed by there: a
    /// variable nothing read moving (a shell's or systemd's per-run ones) keeps it, a variable
    /// read with another value, unset, or set where it was unset lets it go, named and never
    /// valued. A second read of one variable with another value is refused.
    #[test]
    fn env_reads_move_only_with_a_variable_read() {
        let first = env(&[("BLAZE_ITEM_STORE", "verify"), ("SHLVL", "1"), ("SECRET", "hunter2")]);
        let mut reads = EnvReads::default();
        assert_eq!(
            reads.var(&first, "BLAZE_ITEM_STORE", "the item store's mode"),
            Ok(Some("verify"))
        );
        assert_eq!(reads.var(&first, "BLAZE_DEP_IMAGES", "the dependency images' mode"), Ok(None));
        let unrelated = env(&[
            ("BLAZE_ITEM_STORE", "verify"),
            ("SHLVL", "3"),
            ("INVOCATION_ID", "9f"),
            ("OLDPWD", "/x"),
        ]);
        assert_eq!(reads.moved(&unrelated), None);
        let other = env(&[("BLAZE_ITEM_STORE", "off"), ("SECRET", "hunter3")]);
        let moved = reads.moved(&other).expect("a variable read has another value");
        assert_eq!(
            (moved.name.as_str(), moved.by.as_str()),
            ("BLAZE_ITEM_STORE", "the item store's mode")
        );
        let said = format!("{moved} {moved:?} {reads:?}");
        assert!(
            said.contains("has another value now")
                && !said.contains("verify")
                && !said.contains("off"),
            "{said}"
        );
        let set = env(&[("BLAZE_ITEM_STORE", "verify"), ("BLAZE_DEP_IMAGES", "check")]);
        assert_eq!(
            reads.moved(&set).map(|it| it.name),
            Some("BLAZE_DEP_IMAGES".to_owned()),
            "set where it was unset"
        );
        assert!(reads.moved(&env(&[])).is_some_and(|it| it.to_string().contains("is unset now")));
        let refused = reads
            .var(&other, "BLAZE_ITEM_STORE", "a later serve")
            .expect_err("two values of one variable");
        assert!(
            refused.contains("`BLAZE_ITEM_STORE`") && refused.contains("the item store's mode"),
            "{refused}"
        );
    }

    #[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
    struct Words(Vec<String>);

    impl Versioned for Words {
        const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo test words 1");
    }

    fn round_trip(words: &[&str]) {
        let value = Cbor(Words(words.iter().map(|it| (*it).to_owned()).collect()));
        let bytes = blaze_format::to_bytes(&value);
        let aligned = blaze_format::Aligned::copy_from(&bytes);
        let read = <Cbor<Words> as Persist>::read(&aligned).unwrap();
        assert_eq!(read.get(), value.0);
        assert_eq!(read.bytes(), cbor::to_vec(&value.0).unwrap());
    }

    #[test]
    fn a_value_reads_back_as_written() {
        round_trip(&[]);
        round_trip(&["serde", "v1.0.219"]);
        round_trip(&["x"; 1000]);
    }

    #[test]
    fn passed_on_bytes_are_the_value_as_written() {
        let value = Cbor(Words(vec!["core".to_owned(), "alloc".to_owned()]));
        let bytes = blaze_format::to_bytes(&value);
        let aligned = blaze_format::Aligned::copy_from(&bytes);
        let read = <Cbor<Words> as Persist>::read(&aligned).unwrap();
        assert_eq!(blaze_format::to_bytes(&CborBytes::of(read)), bytes);
        assert_eq!(blaze_format::to_bytes(&CborBytes::encode(&value.0)), bytes);
    }

    #[test]
    fn another_schema_is_refused() {
        let bytes = blaze_format::to_bytes(&Cbor(Words(vec!["a".to_owned()])));
        let aligned = blaze_format::Aligned::copy_from(&bytes);
        assert!(matches!(
            <Cbor<Planned> as Persist>::read(&aligned),
            Err(FormatError::Schema { .. })
        ));
    }

    #[test]
    fn a_build_config_round_trips() {
        let config = BuildConfig {
            invocation: Invocation {
                command: Command::Test,
                args: vec!["-p".to_owned(), "a".to_owned()],
                root: "/w".to_owned(),
                cwd: "/w/a".to_owned(),
                env: vec![("RUSTFLAGS".to_owned(), "-Dwarnings".to_owned())],
                files: vec![("/home/u/.cargo/config.toml".to_owned(), b"[build]\n".to_vec())],
            },
            mounts: vec![
                Mount {
                    at: "/rustc/0123/library".to_owned(),
                    origin: Origin::Root(ContentId([7; 32])),
                    under: String::new(),
                },
                Mount { at: "/w".to_owned(), origin: Origin::View, under: String::new() },
            ],
            host: "aarch64-unknown-linux-gnu".to_owned(),
            library: "/rustc/0123/library".to_owned(),
            std: "/s/std".to_owned(),
            builds: "/s/build".to_owned(),
            sources: "/s/sources".to_owned(),
            blaze: String::new(),
        };
        assert_eq!(BuildConfig::decode(&config.encode()).unwrap(), config);
        assert!(BuildConfig::decode(b"\xff").is_err());
    }

    #[test]
    fn std_keys_differ_by_what_they_ask() {
        let key = StdKey {
            library: Mount {
                at: "/rustc/0123/library".to_owned(),
                origin: Origin::Root(ContentId([7; 32])),
                under: String::new(),
            },
            triple: "x86_64-unknown-linux-gnu".to_owned(),
            profile: "dev".to_owned(),
            panic: "unwind".to_owned(),
            opt_level: "0".to_owned(),
            debug_assertions: true,
            overflow_checks: true,
            debuginfo: "2".to_owned(),
            crates: vec!["std".to_owned()],
            features: vec![],
            host: "x86_64-unknown-linux-gnu".to_owned(),
            rustflags: vec![],
            profile_root: "dev".to_owned(),
            macro_wasm: false,
            runs_here: true,
            codegen_backend: None,
        };
        let abort = StdKey { panic: "abort".to_owned(), ..key.clone() };
        assert_ne!(key.id(), abort.id());
        let macros = StdKey { macro_wasm: true, ..key.clone() };
        assert_ne!(key.id(), macros.id(), "the wasm macros' std is never the target's");
        let shipped = StdKey { runs_here: false, ..key.clone() };
        assert_ne!(key.id(), shipped.id(), "a std for this machine's CPU is never the baseline's");
        let avx = StdKey { rustflags: vec!["-Ctarget-feature=+avx2".to_owned()], ..key.clone() };
        assert_ne!(key.id(), avx.id());
        assert_eq!(key.id(), key.clone().id());
    }
}
