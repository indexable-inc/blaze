//! The standard library as one more crate graph (docs/v2/plan/lanes/cargo/notes.md section 5,
//! #86): `library/` (rust-lang/rust at `library/UPSTREAM`'s commit) is a root of its own in the
//! VFS, mounted at `/rustc/<commit>/library` as rustc's own paths into it are named, and loaded
//! by the same queries as a user's workspace ([`crate::load::Foreign`]), asked as cargo's
//! `-Zbuild-std` asks it (`core/compiler/standard_lib.rs`: `resolve_std`, `std_crates`,
//! `generate_std_roots`). Its key ([`StdKey`]) is everything it depends on and nothing of a
//! view, so every view and project shares its units.
//!
//! The target's facts come from blaze's own table ([`target`]), never `rustc --print`
//! (docs/no-rustc.md, step F).

#![forbid(unsafe_code)]

pub mod scripts;
pub mod target;
#[cfg(test)]
mod tests;

use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_query::{Cancelled, Cx, Database, Query, QueryKind};

use crate::load::{Foreign, ForeignAsk, ForeignProfile, key_id};
use crate::query::{
    CborBytes, LoadError, Mount, Origin, StdGraph, StdKey, StdPlanned, StdUnits, kinds, producer,
};
use crate::schema::Mode;

/// Where the library is named for a release of rustc at `commit`: rustc's own std has these
/// paths in its panics, `file!()` and debuginfo.
pub fn library_at(commit: &str) -> String {
    format!("/rustc/{commit}/library")
}

/// blaze's files in `library/`, which no nightly's `rust-src` has (`harness/library.py`'s
/// `OURS`).
pub const OURS: [&str; 2] = ["UPSTREAM", "DIGEST"];

/// Whether the file at `path` (relative to `library/`, `/`-separated) is one of the library's
/// as the VFS root has it: everything `library/` has, which is the nightly's `rust-src` byte for
/// byte (with `vendor/` and `.cargo/config.toml`), but blaze's own [`OURS`].
pub fn in_library(path: &str) -> bool {
    !path.is_empty() && !OURS.contains(&path)
}

/// The commit `library/UPSTREAM` says the library is a copy of, checked to be the one blaze's
/// rustc is ([`target::rustc_commit`]): a library of another release is refused, naming both.
pub fn library_commit(upstream: &str) -> Result<String, String> {
    let commit = upstream
        .lines()
        .find_map(|line| line.strip_prefix("commit "))
        .map(str::trim)
        .filter(|it| !it.is_empty())
        .ok_or_else(|| "library/UPSTREAM names no `commit`".to_owned())?;
    let ours = target::rustc_commit();
    if commit != ours {
        return Err(format!(
            "library/UPSTREAM is rust-lang/rust at {commit}, but blaze's rustc is {ours}: the standard \
             library is the release's (harness/library.py merge)"
        ));
    }
    Ok(commit.to_owned())
}

/// The library's mount, for [`crate::query::BuildConfig::mounts`] and [`StdKey::library`]: the
/// VFS root `root` made of `library/`'s files ([`in_library`]) at [`library_at`].
pub fn library_mount(root: ContentId, commit: &str) -> Mount {
    Mount { at: library_at(commit), origin: Origin::Root(root), under: String::new() }
}

/// The name of the VFS head that keeps the library's root durable, for the library known by
/// `digest` (`library/DIGEST`).
pub fn library_head(digest: &str) -> String {
    format!("blaze/library/{digest}")
}

/// Stands for the build's `target/[<triple>/]<profile dir>` in every path of a [`StdGraph`]'s
/// plans (`outputs`, `out_dir`, `OUT_DIR`), which are the view's: the merge puts the build's
/// directory in its place.
pub const STD_OUT: &str = "/@blaze-std-out";

/// The features of `-Zbuild-std` when `-Zbuild-std-features` names none (`resolve_std`).
pub fn default_features() -> Vec<String> {
    ["panic-unwind", "backtrace", "default"].map(str::to_owned).to_vec()
}

/// The crates asked for when `-Zbuild-std` names none: `std` for a target that has it, else
/// `core` (`std_crates`' default).
pub fn default_crates(triple: &str) -> Result<Vec<String>, String> {
    let facts = target::target_facts(triple)?;
    Ok(vec![if facts.has_std { "std" } else { "core" }.to_owned()])
}

/// The crates built, from those asked: `std` brings `core`, `alloc`, `proc_macro`,
/// `panic_unwind` and `compiler_builtins`; `core` brings `compiler_builtins`; `test` only when
/// asked (cargo's `std_crates`). Sorted.
pub fn std_crates(asked: &[String]) -> Vec<String> {
    let mut crates: Vec<String> = asked.to_vec();
    let has = |name: &str| asked.iter().any(|it| it == name);
    if has("std") {
        crates.extend(["core", "alloc", "proc_macro", "panic_unwind", "compiler_builtins"].map(str::to_owned));
    } else if has("core") {
        crates.push("compiler_builtins".to_owned());
    }
    crates.sort();
    crates.dedup();
    crates
}

/// The packages built with `panic = "abort"` whatever the profile says: library/Cargo.toml's
/// "panic_abort must always be compiled with panic=abort".
const ALWAYS_ABORT: [&str; 1] = ["panic_abort"];

fn failed(what: String, at: Option<String>) -> CborBytes<StdPlanned> {
    CborBytes::encode(&StdPlanned::Failed(LoadError { what, at }))
}

/// What [`StdUnits`] asks of the load for `key`, or why it can't be asked.
pub fn foreign_ask(key: &StdKey) -> Result<ForeignAsk, LoadError> {
    let error = |what: String| LoadError { what, at: None };
    if key.crates.is_empty() {
        return Err(error(format!(
            "the standard library for `{}` is asked for no crate: `-Zbuild-std`'s default is \
             crate::stdlib::default_crates",
            key.triple
        )));
    }
    if !matches!(key.profile_root.as_str(), "dev" | "release") {
        return Err(error(format!(
            "profile `{}` inherits from `{}`, which is neither `dev` nor `release`",
            key.profile, key.profile_root
        )));
    }
    if !matches!(key.panic.as_str(), "unwind" | "abort") {
        return Err(error(format!(
            "the standard library is built with `panic = \"unwind\"` or `\"abort\"`, not `\"{}\"`",
            key.panic
        )));
    }
    let flags = target::CfgArgs::from_flags(&key.rustflags).map_err(error)?;
    // rustc's own check of the profile's settings against the target (`-C panic=unwind` of a
    // target that only aborts).
    let compiled = target::CfgArgs {
        opt_level: Some(key.opt_level.clone()),
        debug_assertions: Some(key.debug_assertions),
        overflow_checks: Some(key.overflow_checks),
        panic: Some(key.panic.clone()),
        ..flags.clone()
    };
    target::target_cfg(&key.triple, &compiled).map_err(error)?;
    // What cargo resolves `[target.'cfg(..)']` and sets `CARGO_CFG_*` by: its probe of the
    // target, with the rustflags and `--crate-type proc-macro`, and no profile.
    let probe = |args: target::CfgArgs, triple: &str| {
        target::target_cfg(triple, &target::CfgArgs { proc_macro: true, ..args }).map_err(error)
    };
    let cfg = probe(flags, &key.triple)?;
    let host_cfg = probe(target::CfgArgs::default(), &key.host)?;
    let mut specs = key.crates.clone();
    specs.push("sysroot".to_owned());
    specs.sort();
    specs.dedup();
    Ok(ForeignAsk {
        mounts: vec![key.library.clone()],
        manifest: format!("{}/Cargo.toml", key.library.at),
        cwd: key.library.at.clone(),
        locked: true,
        specs,
        features: key.features.clone(),
        roots: std_crates(&key.crates),
        triple: key.triple.clone(),
        cfg,
        rustflags: key.rustflags.clone(),
        host: key.host.clone(),
        host_cfg,
        profile: ForeignProfile {
            name: key.profile.clone(),
            root: key.profile_root.clone(),
            opt_level: key.opt_level.clone(),
            debug_assertions: key.debug_assertions,
            overflow_checks: key.overflow_checks,
            debuginfo: key.debuginfo.clone(),
            panic: key.panic.clone(),
        },
        panic_abort: ALWAYS_ABORT.map(str::to_owned).to_vec(),
        is_std: true,
        out: STD_OUT.to_owned(),
    })
}

/// `graph` as the load made it, held to what a standard library's graph is: every unit
/// `is_std`, a plan per unit, each root the library unit of the package it names, and
/// `panic_abort` built to abort.
fn checked(ask: &ForeignAsk, graph: &StdGraph) -> Result<(), String> {
    if graph.plans.len() != graph.units.len() {
        return Err(format!("the load planned {} of the standard library's {} units", graph.plans.len(), graph.units.len()));
    }
    let mut names: HashMap<&str, &str> = HashMap::with_capacity_and_hasher(graph.packages.len(), Default::default());
    names.extend(graph.packages.iter().map(|it| (it.id.as_str(), it.name.as_str())));
    let name_of = |pkg_id: &str| names.get(pkg_id).copied();
    for unit in &graph.units {
        let name = name_of(&unit.pkg_id).ok_or_else(|| format!("the load gave no package `{}`", unit.pkg_id))?;
        if !unit.is_std {
            return Err(format!("the load gave a unit of `{name}` in the standard library that is not `is_std`"));
        }
        let script = unit.mode == Mode::RunCustomBuild || unit.target.is_custom_build();
        if ALWAYS_ABORT.contains(&name) && !script && unit.profile.panic != "abort" {
            return Err(format!("`{name}` is built with `panic = \"{}\"`, not `\"abort\"`", unit.profile.panic));
        }
    }
    if !graph.roots.iter().map(|(name, _)| name).eq(&ask.roots) {
        let named: Vec<&str> = graph.roots.iter().map(|(name, _)| name.as_str()).collect();
        return Err(format!("the load's roots are {named:?}, not the crates asked, {:?}", ask.roots));
    }
    for (name, index) in &graph.roots {
        let unit = graph.units.get(*index).ok_or_else(|| format!("root `{name}` is unit {index}, past the graph"))?;
        if name_of(&unit.pkg_id) != Some(name.as_str()) || !unit.target.is_lib() || unit.mode != Mode::Build {
            return Err(format!("root `{name}` is not the library unit of package `{name}`"));
        }
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl Query for StdUnits {
    const KIND: QueryKind = kinds::STD_UNITS;
    const NAME: &'static str = "std units";
    const PRODUCER: ContentId = producer("blaze v2 cargo std units 5");
    type Key = StdKey;
    /// `Foreign`'s bytes as it wrote them once checked: read as a `CborRef<StdPlanned>`, as a
    /// `Cbor<StdPlanned>` would be, with no second encoding.
    type Value = CborBytes<StdPlanned>;

    /// [`StdKey::id`], hashed without its allocation.
    fn input(key: &StdKey) -> ContentId {
        key_id(StdKey::DOMAIN, key)
    }

    fn execute(cx: &mut Cx<'_>, key: &StdKey) -> Result<CborBytes<StdPlanned>, Cancelled> {
        #[cfg(test)]
        RUNS.with(|it| it.set(it.get() + 1));
        let ask = match foreign_ask(key) {
            Ok(ask) => ask,
            Err(error) => return Ok(CborBytes::encode(&StdPlanned::Failed(error))),
        };
        let found = cx.get::<Foreign>(&ask)?;
        Ok(match found.get() {
            StdPlanned::Ready(graph) => match checked(&ask, &graph) {
                Ok(()) => CborBytes::of(found),
                Err(what) => failed(format!("the standard library's units: {what}"), Some(ask.manifest.clone())),
            },
            StdPlanned::Failed(_) => CborBytes::of(found),
        })
    }
}

/// Registers the standard library's queries.
pub fn register(db: &mut Database) {
    db.register::<StdUnits>();
}
