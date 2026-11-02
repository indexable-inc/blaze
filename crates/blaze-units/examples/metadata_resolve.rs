//! Reads the JSON of `cargo metadata --format-version 1` on stdin and prints the `resolve` that
//! blaze-resolve makes of its packages: the workspace resolved as `cargo metadata` resolves it
//! (all members with their default features, after the resolve `Cargo.lock` is written from),
//! against a registry of the packages listed, with the `[patch]` tables of the workspace's
//! root manifest. With `--lock FILE`, that lock file (the workspace's `Cargo.lock` as cargo
//! found it) guides the resolve, as it guided cargo's. The first resolve takes every feature,
//! so it needs packages the output asked for does not list: `--packages FILE` adds those of
//! another `cargo metadata` output (that of `--all-features`). `harness/resolve_graph.py`
//! compares it with cargo's.
//!
//! The `[patch]` tables are read by blaze-units' `read` module, the load's own copy. Packages are
//! read here from `cargo metadata`'s JSON, since `read::read` takes the typed form the load makes.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use blaze_hash::HashMap;
use blaze_resolve::lockfile::{self, PathDeps};
use blaze_resolve::metadata::{MetadataPackages, resolve_graph};
use blaze_resolve::resolver::{CliFeatures, CompileKind, HasDevUnits, Members, VersionOrdering};
use blaze_resolve::workspace_resolve::{PackageRegistry, Patch, Workspace, resolve_ws};
use blaze_resolve::{
    DepKind, Dependency, IndexSummary, InternedString, MemRegistry, PackageId, PackageIdSpec,
    Platform, QueryKind, Registry, ResolveVersion, SourceId, Summary,
};
use blaze_units::read::{self, Root};
use serde_json::Value;
use url::Url;

fn str_of<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key].as_str().ok_or_else(|| anyhow!("no `{key}` in {value}"))
}

fn package_source(package: &Value) -> Result<SourceId> {
    match package["source"].as_str() {
        Some(source) => SourceId::from_url(source),
        None => SourceId::for_path(
            Path::new(str_of(package, "manifest_path")?).parent().context("a manifest's dir")?,
        ),
    }
}

fn dependency(dep: &Value) -> Result<Dependency> {
    let source = match (dep["source"].as_str(), dep["path"].as_str()) {
        (Some(source), _) => SourceId::from_url(source)?,
        (None, Some(path)) => SourceId::for_path(Path::new(path))?,
        (None, None) => SourceId::crates_io(),
    };
    // A path or git dependency without a `version` is written `*` but, unlike `version = "*"`,
    // matches prereleases.
    let req = match str_of(dep, "req")? {
        "*" if !source.is_registry() => None,
        req => Some(req),
    };
    let mut d = Dependency::parse(str_of(dep, "name")?, req, source)?;
    d.set_kind(match dep["kind"].as_str() {
        None => DepKind::Normal,
        Some("dev") => DepKind::Development,
        Some("build") => DepKind::Build,
        Some(kind) => return Err(anyhow!("dependency kind `{kind}`")),
    });
    if let Some(rename) = dep["rename"].as_str() {
        d.set_explicit_name_in_toml(rename);
    }
    d.set_optional(dep["optional"].as_bool().unwrap_or(false));
    d.set_default_features(dep["uses_default_features"].as_bool().unwrap_or(true));
    let features: Vec<&str> =
        dep["features"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    d.set_features(features);
    if let Some(target) = dep["target"].as_str() {
        d.set_platform(Some(target.parse::<Platform>()?));
    }
    Ok(d)
}

/// The root manifest's `[patch]` (blaze-units' `read::patch_tables`), without entries whose
/// package `cargo metadata` does not list (a patch the graph does not use).
fn patch_tables(root: &Path, registry: &MemRegistry) -> Result<Vec<(Url, Vec<Patch>)>> {
    let text = std::fs::read_to_string(root.join("Cargo.toml"))?;
    let mut tables = read::patch_tables(&Root::new(root, &text)?)?;
    for (_, patches) in &mut tables {
        let mut kept = Vec::with_capacity(patches.len());
        for patch in patches.drain(..) {
            if !ready(registry.query_vec(&patch.dep, QueryKind::Exact))?.is_empty() {
                kept.push(patch);
            }
        }
        *patches = kept;
    }
    Ok(tables)
}

/// The output of a future that is ready at once, as [`MemRegistry`]'s are.
fn ready<T>(future: impl std::future::Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    let cx = &mut std::task::Context::from_waker(std::task::Waker::noop());
    match future.as_mut().poll(cx) {
        std::task::Poll::Ready(output) => output,
        std::task::Poll::Pending => unreachable!("a MemRegistry query is ready at once"),
    }
}

struct Packages {
    lib_names: BTreeMap<PackageId, InternedString>,
}

impl MetadataPackages for Packages {
    fn package_id(&self, id: PackageId) -> PackageId {
        id
    }

    fn lib_crate_name(&self, id: PackageId) -> Option<InternedString> {
        self.lib_names.get(&id).copied()
    }

    fn platform_activated(&self, _platform: &Platform, _kind: CompileKind) -> bool {
        true
    }
}

const LIB_KINDS: &[&str] = &["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];

fn main() -> Result<()> {
    let usage = "usage: metadata_resolve [--lock FILE] [--packages FILE]...";
    let mut lock_path = None;
    let mut more_packages = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--lock" => lock_path = Some(args.next().context(usage)?),
            "--packages" => more_packages.push(args.next().context(usage)?),
            _ => return Err(anyhow!("{usage}, not `{arg}`")),
        }
    }
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let metadata: Value = serde_json::from_str(&input)?;
    let mut sources = vec![metadata.clone()];
    for path in &more_packages {
        let text = std::fs::read_to_string(path).with_context(|| path.clone())?;
        sources.push(serde_json::from_str(&text)?);
    }

    let mut by_spec = BTreeMap::new();
    let mut lib_names = BTreeMap::new();
    for package in sources.iter().flat_map(|m| m["packages"].as_array().into_iter().flatten()) {
        if by_spec.contains_key(str_of(package, "id")?) {
            continue;
        }
        let id = PackageId::try_new(
            str_of(package, "name")?,
            str_of(package, "version")?,
            package_source(package)?,
        )?;
        let deps = package["dependencies"]
            .as_array()
            .into_iter()
            .flatten()
            .map(dependency)
            .collect::<Result<Vec<_>>>()?;
        let features: BTreeMap<InternedString, Vec<InternedString>> = package["features"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, values)| {
                let values = values
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(InternedString::new)
                    .collect();
                (InternedString::new(name), values)
            })
            .collect();
        let summary = Summary::new(id, deps, &features, package["links"].as_str(), None)?;
        let lib = package["targets"].as_array().into_iter().flatten().find(|target| {
            target["kind"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|k| k.as_str().is_some_and(|k| LIB_KINDS.contains(&k)))
        });
        if let Some(lib) = lib {
            lib_names.insert(id, InternedString::new(&str_of(lib, "name")?.replace('-', "_")));
        }
        by_spec.insert(str_of(package, "id")?.to_owned(), summary);
    }

    let workspace_root = Path::new(str_of(&metadata, "workspace_root")?);
    let path_packages: HashMap<SourceId, Summary> = by_spec
        .values()
        .filter(|s| s.source_id().is_path())
        .map(|s| (s.source_id(), s.clone()))
        .collect();

    let lock = if let Some(path) = &lock_path {
        let text = std::fs::read_to_string(path).with_context(|| path.clone())?;
        let mut path_deps = PathDeps::default();
        for s in path_packages.values() {
            path_deps
                .entry(s.name().to_string())
                .or_default()
                .insert(s.version().clone(), s.source_id());
        }
        Some(lockfile::into_resolve(lockfile::parse(&text)?, &text, &path_deps)?)
    } else {
        None
    };
    // The checksums a registry's index would give, which must agree with the lock file's.
    if let Some(lock) = &lock {
        for summary in by_spec.values_mut() {
            if let Some(Some(checksum)) = lock.checksums().get(&summary.package_id()) {
                summary.set_checksum(checksum.clone());
            }
        }
    }
    let mut registry = MemRegistry::new();
    for summary in by_spec.values() {
        registry.add(IndexSummary::Candidate(summary.clone()));
    }
    let patch = patch_tables(workspace_root, &registry)?;

    let member = |spec: &Value| -> Result<&Summary> {
        let spec = spec.as_str().context("a member's id")?;
        by_spec.get(spec).ok_or_else(|| anyhow!("no package {spec}"))
    };
    let members = metadata["workspace_members"]
        .as_array()
        .context("no workspace_members")?
        .iter()
        .map(member)
        .collect::<Result<Vec<_>>>()?;
    let root = match &metadata["resolve"]["root"] {
        Value::Null => None,
        spec => Some(member(spec)?.package_id()),
    };
    let ws = Workspace {
        members: Members { members: &members, current: root, new_cli_feature_behavior: true },
        path_packages: &path_packages,
        patch: &patch,
        replace: &[],
        require_optional_deps: true,
        version_ordering: VersionOrdering::MaximumVersionsFirst,
        rust_versions: None,
        resolve_version: ResolveVersion::default(),
        offline_flag: None,
    };
    let specs: Vec<PackageIdSpec> = members.iter().map(|m| m.package_id().to_spec()).collect();
    let mut registry = PackageRegistry::new(registry);
    let (_, resolve) = resolve_ws(
        &mut registry,
        &ws,
        lock.as_ref(),
        &CliFeatures::new_all(false),
        &specs,
        HasDevUnits::Yes,
    )?;

    let member_ids: Vec<PackageId> = members.iter().map(|m| m.package_id()).collect();
    let graph =
        resolve_graph(&resolve, &member_ids, root, &Packages { lib_names }, &[CompileKind::Host])?;
    writeln!(std::io::stdout(), "{}", serde_json::to_string(&graph)?)?;
    Ok(())
}
