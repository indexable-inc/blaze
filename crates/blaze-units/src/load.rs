//! The workspace load as queries of blaze's database (docs/v2/plan/lanes/cargo/notes.md): the
//! view's invocation ([`invocation`]), every file through the mounts ([`files`]), the config
//! chain ([`config`]), each manifest and the workspace ([`manifest`]), the resolve
//! ([`resolve`], packages located by [`source`]), the units ([`units`]) and their plans
//! ([`plan`]), and the two queries asked from outside: [`crate::query::Plan`] of the view, and
//! [`Foreign`], the same load of a workspace keyed by nothing of the view (the standard
//! library's, [`crate::stdlib`]).
//!
//! Each step reads each file once, through the database, when it asks for it; a TOML file is
//! read in its canonical form (re-serialized), so an edit no one reads (a comment, a file's
//! contents a listing does not show) cuts off where it is first read. Only
//! [`invocation::Invocation`] reads `Cx::config`.

mod config;
mod files;
mod invocation;
mod manifest;
mod plan;
mod resolve;
mod sandbox;
mod source;
#[cfg(test)]
pub(crate) mod tests;
mod units;
mod view;

use std::cell::RefCell;

use blaze_ids::ContentId;
use blaze_protocol::cbor;
use blaze_query::{Cancelled, Cx, Database, Query, QueryKind};
use serde::{Deserialize, Serialize};

use self::config::ConfigOf;
use self::invocation::{Asked, Asking};
use self::manifest::{WorkspaceKey, WorkspaceQuery, in_workspace};
use self::units::Built;
use crate::query::{
    Cbor, Command, Fetch, LoadError, Mount, Plan, Planned, StdGraph, StdPlanned, Units, kinds, producer,
};
use crate::{Filter, Intent};

pub use self::files::Snap;
pub use self::files::normal_text;
pub use crate::Read;
pub use self::view::{ResolveAsk, Resolving, ViewResolved, platform_cfg, resolve_view};

thread_local! {
    // alloc: grown once to the largest key the thread hashes, then reused by every key.
    static KEY_BYTES: RefCell<(cbor::Writer, Vec<u8>)> = RefCell::new((cbor::Writer::new(), Vec::new()));
}

/// [`crate::query::key_input`] without its allocation: the key's deterministic CBOR is written
/// into this thread's buffer and hashed there, so a memo hit, which asks for the key's ID
/// first, allocates nothing. The same ID as `key_input` of the same domain and key.
pub fn key_id<K: Serialize>(domain: &'static str, key: &K) -> ContentId {
    KEY_BYTES.with(|held| {
        let mut held = held.borrow_mut();
        let (writer, bytes) = &mut *held;
        bytes.clear();
        writer.append(bytes, key).unwrap_or_else(|error| panic!("a query key has no CBOR: {error}"));
        ContentId::of(domain, bytes)
    })
}

/// What a load step came to: its value, what it would read that no mount covers yet, or
/// cargo's error.
pub(crate) enum Staged<T> {
    Done(T),
    Needs(Vec<Fetch>),
    Failed(LoadError),
}

/// cargo's error as it prints it after `error: `: the message, then each cause after a blank
/// line and `Caused by:`, indented two spaces.
pub fn load_error(error: &anyhow::Error, at: Option<String>) -> LoadError {
    let mut what = error.to_string();
    for cause in error.chain().skip(1) {
        what.push_str("\n\nCaused by:");
        for line in cause.to_string().lines() {
            what.push_str("\n  ");
            what.push_str(line);
        }
    }
    LoadError { what, at }
}

/// A path the load would read that is under no mount: not fetchable, so an error naming it.
pub(crate) fn unmounted_error(path: &str) -> LoadError {
    LoadError { what: format!("`{path}` is under no mount of the view"), at: Some(path.to_owned()) }
}

/// The packages a command selects (cargo's `Packages`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Select {
    Default,
    Packages(Vec<String>),
    All,
    OptOut(Vec<String>),
}

/// Everything a load is asked by, whichever query asks it.
pub(crate) struct Spec {
    pub workspace: WorkspaceKey,
    pub select: Select,
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
    /// `--locked` or `--frozen`: the lock file is never written.
    pub locked: Option<&'static str>,
    /// `--offline` or `--frozen` (`net.offline` is read with the config).
    pub offline: Option<&'static str>,
    pub require_optional_deps: bool,
    pub intent: Intent,
    pub filter: Filter,
    /// `cargo run`: one executable among the roots.
    pub run: bool,
    /// `check`: the workspace's crates are linted by rustc's, clippy's and blaze's lints.
    pub lints: bool,
    pub release: bool,
    pub profile: Option<String>,
    pub host: String,
    pub target: Option<String>,
    /// `--target-dir`, absolute.
    pub target_dir: Option<String>,
    pub jobs: Option<u32>,
    /// rustc's release, as `rustc -vV` says it.
    pub rustc_version: String,
    /// Each unit's package's `rust-version` is checked against [`Spec::rustc_version`]: not
    /// under `--ignore-rust-version`.
    pub honor_rust_version: bool,
    /// The standard library's mount, where the view has one at [`Spec::library_at`].
    pub library: Option<Mount>,
    pub library_at: String,
    /// [`crate::query::BuildConfig::std`]: where the standard library's units place what they
    /// make, in a directory of their [`crate::query::StdKey`]'s.
    pub std: String,
    /// `-Zbuild-std=` and `-Zbuild-std-features=`.
    pub build_std: Option<Vec<String>>,
    pub build_std_features: Option<Vec<String>>,
    /// A foreign workspace's ask: its units are a standard library's.
    pub foreign: Option<ForeignAsk>,
}

/// The view's spec, from its command line: as cargo's `Packages::from_flags`,
/// `CompileFilter::from_raw_arguments` and the commands read it. `library` is the view's mount
/// at `asked.library`, if it has one.
fn view_spec(asked: &Asked, library: Option<Mount>) -> Result<Spec, LoadError> {
    let error = |what: &str| LoadError { what: what.to_owned(), at: None };
    let select = match (asked.workspace, asked.exclude.is_empty(), asked.packages.is_empty()) {
        (false, true, true) => Select::Default,
        (false, true, false) => Select::Packages(asked.packages.clone()),
        (false, false, _) => return Err(error("--exclude can only be used together with --workspace")),
        (true, true, _) => Select::All,
        (true, false, _) => Select::OptOut(asked.exclude.clone()),
    };
    if asked.doc {
        return Err(error("blaze does not run doctests yet (`cargo test --doc`)"));
    }
    let intent = match asked.command() {
        Command::Build | Command::Run => Intent::Build,
        Command::Check => Intent::Check { test: asked.profile.as_deref() == Some("test") },
        Command::Test => Intent::Test,
        Command::Bench => Intent::Bench,
        Command::Doc => Intent::Doc { open: asked.open },
    };
    let filter = Filter::from_flags(
        asked.lib,
        asked.bins.clone(),
        asked.all_bins,
        asked.tests.clone(),
        asked.all_tests,
        asked.examples.clone(),
        asked.all_examples,
        asked.benches.clone(),
        asked.all_benches,
        asked.all_targets,
    );
    let target = match asked.targets.as_slice() {
        [] => None,
        [one] => Some(one.clone()),
        _ => return Err(error("blaze builds for one `--target` at a time")),
    };
    let mut build_std = None;
    let mut build_std_features = None;
    for flag in &asked.unstable {
        let (name, value) = flag.split_once('=').unwrap_or((flag.as_str(), ""));
        let list = || value.split(',').filter(|it| !it.is_empty()).map(str::to_owned).collect::<Vec<_>>();
        match name {
            "unstable-options" | "bindeps" | "codegen-backend" => {}
            "build-std" => build_std = Some(list()),
            "build-std-features" => build_std_features = Some(list()),
            _ => return Err(error(&format!("blaze does not read `-Z {name}` yet"))),
        }
    }
    let absolute = |path: &str| match path.starts_with('/') {
        true => path.to_owned(),
        false => format!("{}/{path}", asked.cwd),
    };
    Ok(Spec {
        workspace: WorkspaceKey::view(),
        select,
        features: asked.features.clone(),
        all_features: asked.all_features,
        no_default_features: asked.no_default_features,
        locked: asked.locked_flag(),
        offline: asked.offline_flag(false),
        require_optional_deps: true,
        intent,
        filter,
        run: asked.command() == Command::Run,
        lints: asked.command() == Command::Check,
        release: asked.release,
        profile: asked.profile.clone(),
        host: asked.host.clone(),
        target,
        target_dir: asked.target_dir.as_deref().map(absolute),
        jobs: asked.jobs,
        rustc_version: crate::stdlib::target::rustc_version().to_owned(),
        honor_rust_version: !asked.ignore_rust_version,
        library,
        library_at: asked.library.clone(),
        std: asked.std.clone(),
        build_std,
        build_std_features,
        foreign: None,
    })
}

/// Stands for `$CARGO_HOME` of a foreign workspace's configuration: under no mount, so its
/// config files are absent and nothing is read from it.
const NO_CARGO_HOME: &str = "/@blaze-no-cargo-home";

fn foreign_spec(ask: &ForeignAsk) -> Spec {
    let config = ConfigOf::Fixed {
        mounts: ask.mounts.clone(),
        cwd: ask.cwd.clone(),
        env: vec![("CARGO_HOME".to_owned(), NO_CARGO_HOME.to_owned())],
    };
    Spec {
        workspace: WorkspaceKey { manifest: Some(ask.manifest.clone()), config },
        select: Select::Packages(ask.specs.clone()),
        features: ask.features.clone(),
        all_features: false,
        no_default_features: true,
        locked: ask.locked.then_some("--locked"),
        offline: Some("--offline"),
        require_optional_deps: false,
        intent: Intent::Build,
        filter: Filter::Only {
            all_targets: false,
            lib: crate::LibRule::True,
            bins: crate::Rule::none(),
            examples: crate::Rule::none(),
            tests: crate::Rule::none(),
            benches: crate::Rule::none(),
        },
        run: false,
        lints: false,
        release: false,
        profile: None,
        host: ask.host.clone(),
        target: Some(ask.triple.clone()),
        target_dir: None,
        jobs: None,
        rustc_version: crate::stdlib::target::rustc_version().to_owned(),
        // The standard library is the toolchain's own, of its release.
        honor_rust_version: false,
        library: None,
        library_at: String::new(),
        // Its units are a standard library's, placed where the workspace that asks puts them.
        std: String::new(),
        build_std: None,
        build_std_features: None,
        foreign: Some(ask.clone()),
    }
}

/// Loads the workspace `spec` names and makes its units.
fn load(cx: &mut Cx<'_>, spec: &Spec) -> Result<Staged<Built>, Cancelled> {
    let read = match cx.get::<WorkspaceQuery>(&spec.workspace)?.get() {
        Ok(read) => read,
        Err(error) => return Ok(Staged::Failed(error)),
    };
    let outcome = in_workspace(cx, &read, &spec.workspace.config, |ctx, ws| {
        resolve::resolved(ctx, ws, spec, |resolved| units::units(ctx, ws, spec, &read, resolved))
    })?;
    Ok(match outcome {
        Ok(staged) => staged,
        Err(error) => Staged::Failed(error),
    })
}

impl Query for Plan {
    const KIND: QueryKind = kinds::PLAN;
    const NAME: &'static str = "cargo plan";
    const PRODUCER: ContentId = producer("blaze v2 cargo plan 5");
    type Key = ();
    type Value = Cbor<Planned>;

    fn input(key: &()) -> ContentId {
        key_id("blaze v2 cargo plan key 1", key)
    }

    fn execute(cx: &mut Cx<'_>, _: &()) -> Result<Cbor<Planned>, Cancelled> {
        let asked = match cx.get::<Asking>(&())?.get() {
            Ok(asked) => asked,
            Err(error) => return Ok(Cbor(Planned::Failed(error))),
        };
        let library = cx.get::<files::Mounts>(&())?.get(&asked.library);
        let spec = match view_spec(&asked, library) {
            Ok(spec) => spec,
            Err(error) => return Ok(Cbor(Planned::Failed(error))),
        };
        Ok(Cbor(match load(cx, &spec)? {
            Staged::Done(built) => Planned::Ready(Units {
                graph: built.graph,
                plans: built.plans,
                packages: built.packages,
                lock: built.lock,
                warnings: built.warnings,
                target_dir: built.target_dir,
                build_dir: built.build_dir,
                finished: built.finished,
                link_dirs: built.link_dirs,
                sandboxed_tests: built.sandboxed_tests,
            }),
            Staged::Needs(fetches) => Planned::Needs(fetches),
            Staged::Failed(error) => Planned::Failed(error),
        }))
    }
}

/// The units of a workspace other than the view's, by everything that decides them: the
/// standard library's ([`crate::stdlib`]). Reads nothing of the view and never `Cx::config`.
pub struct Foreign;

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub struct ForeignAsk {
    /// Every file it reads is under these (the library's mount alone); a path under none fails
    /// naming it (nothing is fetched: library/ vendors its registry packages).
    pub mounts: Vec<Mount>,
    /// The workspace's root manifest, absolute: `<library>/Cargo.toml`.
    pub manifest: String,
    /// The config chain: `.cargo/config.toml` of `cwd` and each directory above it (library/'s
    /// replaces crates.io with `vendor/`); no `$CARGO_HOME`, no environment, no `--config`.
    pub cwd: String,
    /// `--locked`: `Cargo.lock` as it is; one the manifests disagree with is cargo's error.
    pub locked: bool,
    /// cargo's `resolve_ws_with_opts` of these specs (`-p` names), with `features` as
    /// `--features` says them, `--no-default-features`, `HasDevUnits::No`, and
    /// `require_optional_deps = false` (cargo's `resolve_std`).
    pub specs: Vec<String>,
    pub features: Vec<String>,
    /// The packages whose `lib` units are the graph's roots, by name; `StdGraph::roots` names
    /// each with its unit's index, in this order.
    pub roots: Vec<String>,
    /// What every unit but build scripts' is built for (`CompileKind::Target(triple)`, so
    /// `Unit::platform` is `Some(triple)`), with its cfg.
    pub triple: String,
    pub cfg: Vec<String>,
    /// The rustflags of every unit for `triple`, as cargo passes them; host units have none.
    pub rustflags: Vec<String>,
    /// What build scripts are compiled for and run on, with its cfg.
    pub host: String,
    pub host_cfg: Vec<String>,
    /// The profile of every unit: the workspace's own `[profile.<root>]` with its top-level
    /// settings replaced by these, then its `package.*` and `build-override` tables applied
    /// as cargo applies them. `Profile::name` is `name`.
    pub profile: ForeignProfile,
    /// Packages whose units are built with `panic = "abort"` whatever the profile says.
    pub panic_abort: Vec<String>,
    /// `Unit::is_std` of every unit.
    pub is_std: bool,
    /// Stands for the build's `target/[<triple>/]<profile dir>` in every path of the plans
    /// (`outputs`, `out_dir`, `OUT_DIR` in `env`).
    pub out: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub struct ForeignProfile {
    /// The build's profile, as each unit's `Profile::name` says it.
    pub name: String,
    /// `dev` or `release`: the workspace's table the settings go on.
    pub root: String,
    pub opt_level: String,
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub debuginfo: String,
    pub panic: String,
}

impl Query for Foreign {
    const KIND: QueryKind = QueryKind(0x601a);
    const NAME: &'static str = "cargo foreign";
    const PRODUCER: ContentId = producer("blaze v2 cargo foreign 5");
    type Key = ForeignAsk;
    type Value = Cbor<StdPlanned>;

    fn input(key: &ForeignAsk) -> ContentId {
        key_id("blaze v2 cargo foreign key 2", key)
    }

    fn execute(cx: &mut Cx<'_>, ask: &ForeignAsk) -> Result<Cbor<StdPlanned>, Cancelled> {
        Ok(Cbor(match load(cx, &foreign_spec(ask))? {
            Staged::Done(built) => StdPlanned::Ready(StdGraph {
                units: built.graph.units,
                plans: built.plans,
                packages: built.packages,
                roots: built.roots,
            }),
            Staged::Needs(fetches) => {
                let what: Vec<String> = fetches.iter().map(|it| format!("`{}` at `{}`", it.what, it.at)).collect();
                StdPlanned::Failed(LoadError {
                    what: format!(
                        "the workspace at `{}` reads what nothing fetches for it: {}",
                        ask.manifest,
                        what.join(", ")
                    ),
                    at: Some(ask.manifest.clone()),
                })
            }
            Staged::Failed(error) => StdPlanned::Failed(error),
        }))
    }
}

/// Registers every query of the load.
pub fn register(db: &mut Database) {
    db.register::<invocation::Invocation>();
    db.register::<invocation::Asking>();
    db.register::<files::Mounts>();
    db.register::<files::LoadFile>();
    db.register::<files::LoadDir>();
    db.register::<files::FixedFile>();
    db.register::<files::FixedDir>();
    db.register::<files::TomlFile>();
    db.register::<config::ConfigChain>();
    db.register::<manifest::ManifestQuery>();
    db.register::<manifest::WorkspaceQuery>();
    db.register::<Plan>();
    db.register::<Foreign>();
}
