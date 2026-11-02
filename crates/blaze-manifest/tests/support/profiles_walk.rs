//! The profile of every unit of a unit graph cargo printed (`--unit-graph`), recomputed with
//! blaze-manifest's `Profiles`: what each unit is built for is derived from the graph's edges
//! as cargo's `unit_dependencies` derives it, and the strip and debuginfo adjustments of cargo's
//! `rebuild_unit_graph_shared` are applied over the graph's dependencies.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow, bail};
use blaze_hash::HashMap;
use blaze_manifest::config::CompileKind;
use blaze_manifest::diagnostic::report;
use blaze_manifest::files::Disk;
use blaze_manifest::manifest::PackageId as Pkg;
use blaze_manifest::metadata::root_manifest;
use blaze_manifest::profiles::{Profile, Profiles, UnitFor, requested_profile};
use blaze_manifest::source::{self, SourceId};
use blaze_manifest::{Config, Context, Workspace};
use cargo_util_schemas::core::PackageIdSpec;
use cargo_util_schemas::lockfile::TomlLockfile;
use serde_json::Value;

/// What the profiles of `cargo <args> --unit-graph` were made from.
pub(crate) struct Output {
    pub(crate) code: u8,
    /// The profile of each unit of the graph, a JSON array.
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

/// The cargo command the graph is of, as far as profiles go.
struct Command {
    /// `build`, `check`, `test`, `bench`.
    name: String,
    requested_profile: String,
    /// The first `--target`, which keeps host units apart from target units.
    target: Option<String>,
    config: Vec<String>,
    manifest_path: Option<PathBuf>,
}

fn parse_args(args: &[String]) -> anyhow::Result<Command> {
    let mut args = args.iter().cloned();
    let Some(name) = args.next() else { bail!("no cargo command given") };
    let mut command = Command {
        requested_profile: String::new(),
        target: None,
        config: Vec::new(),
        manifest_path: None,
        name,
    };
    let mut release = false;
    let mut profile = None;
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_owned(), Some(v.to_owned())),
            _ => (arg.clone(), None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| args.next())
                .ok_or_else(|| anyhow!("a value is required for '{flag} <VALUE>'"))
        };
        match flag.as_str() {
            "--release" | "-r" => release = true,
            "--profile" => profile = Some(value()?),
            "--target" => {
                let target = value()?;
                command.target.get_or_insert(target);
            }
            "--config" => command.config.push(value()?),
            "--manifest-path" => command.manifest_path = Some(PathBuf::from(value()?)),
            "-p" | "--package" | "--exclude" | "--features" | "-F" | "--bin" | "--example"
            | "--test" | "--bench" | "-j" | "--jobs" => {
                value()?;
            }
            _ => {}
        }
    }
    let default = match command.name.as_str() {
        "test" | "bench" => command.name.as_str(),
        _ => "dev",
    };
    command.requested_profile = match (command.name.as_str(), profile.as_deref()) {
        ("check", Some("test")) => "test".to_owned(),
        _ => requested_profile(default, release, profile.as_deref())?,
    };
    Ok(command)
}

/// `cargo <args>`'s profiles from cwd, the unit graph cargo printed for it in `graph`.
pub(crate) fn main(
    cwd: &Path,
    env: impl IntoIterator<Item = (String, String)>,
    host: &str,
    args: &[String],
    graph: &str,
) -> Output {
    let mut config = match Config::load(cwd, env, &[]) {
        Ok(config) => config,
        Err(err) => {
            return Output { code: 101, stdout: String::new(), stderr: report(&err, cwd) + "\n" };
        }
    };
    let mut stderr = String::new();
    let result = parse_args(args).and_then(|command| {
        let started = config.start(&command.name, &command.config);
        for warning in config.take_warnings() {
            let _ = writeln!(stderr, "warning: {warning}");
        }
        started?;
        let ctx = Context::new(config);
        let result = run(&ctx, host, &command, graph, &mut stderr);
        for warning in ctx.config.take_warnings() {
            let _ = writeln!(stderr, "warning: {warning}");
        }
        result
    });
    match result {
        Ok(stdout) => Output { code: 0, stdout, stderr },
        Err(err) => {
            let _ = writeln!(stderr, "{}", report(&err, cwd));
            Output { code: 101, stdout: String::new(), stderr }
        }
    }
}

fn run(
    ctx: &Context<Disk>,
    host: &str,
    command: &Command,
    graph: &str,
    stderr: &mut String,
) -> anyhow::Result<String> {
    let root = root_manifest(command.manifest_path.as_deref(), ctx)?;
    let ws = Workspace::new(&root, ctx)?;
    for warning in ws.workspace_warnings() {
        let _ = writeln!(stderr, "warning: {warning}");
    }
    for (path, warning) in ws.warnings() {
        let _ = writeln!(stderr, "warning: {}: {warning}", path.display());
    }
    let profiles = Profiles::new(
        ws.profiles(),
        ws.unstable_features(),
        &ctx.config,
        &command.requested_profile,
        host,
    )?;
    let graph: Value = serde_json::from_str(graph).context("cargo's unit graph")?;
    let profiles = profiles_of(&ws, &profiles, command, &graph, stderr)?;
    Ok(serde_json::to_string_pretty(&profiles)? + "\n")
}

fn package(pkg_id: &str) -> anyhow::Result<Pkg> {
    let spec = PackageIdSpec::parse(pkg_id).with_context(|| format!("package id `{pkg_id}`"))?;
    let version = spec.version().ok_or_else(|| anyhow!("package id `{pkg_id}` has no version"))?;
    let url = spec.url().ok_or_else(|| anyhow!("package id `{pkg_id}` has no source"))?;
    let kind = spec.kind().ok_or_else(|| anyhow!("package id `{pkg_id}` has no source kind"))?;
    let source_id = source::from_spec(kind, url)?;
    Ok(Pkg { name: spec.name().to_owned(), version, source_id })
}

/// The packages of the workspace's `Cargo.lock`, which cargo checks the package overrides
/// against: every package of the resolve, built or not. A path package's directory is that of
/// the unit or member of its name and version.
fn lock_packages(ws: &Workspace<'_, Disk>, units: &[Unit]) -> anyhow::Result<Option<Vec<Pkg>>> {
    let path = ws.root().join("Cargo.lock");
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok(None) };
    let lock: TomlLockfile =
        toml::from_str(&text).with_context(|| format!("failed to parse `{}`", path.display()))?;
    let mut packages = Vec::new();
    for package in lock.package.into_iter().flatten() {
        let version: semver::Version = package.version.parse()?;
        let source_id = match &package.source {
            Some(locked) => source::from_spec(locked.kind(), locked.url())?,
            None => {
                let unit = units.iter().find(|u| {
                    u.pkg.name == package.name
                        && u.pkg.version == version
                        && u.pkg.source_id.is_path()
                });
                let member = ws.members().find(|(_, m)| {
                    m.package_id.name == package.name && m.package_id.version == version
                });
                match (unit, member) {
                    (Some(u), _) => u.pkg.source_id,
                    (None, Some((_, m))) => m.package_id.source_id,
                    (None, None) => SourceId::for_path(ws.root())?,
                }
            }
        };
        packages.push(Pkg { name: package.name, version, source_id });
    }
    Ok(Some(packages))
}

struct Unit {
    pkg: Pkg,
    is_member: bool,
    kinds: Vec<String>,
    custom_build: bool,
    proc_macro: bool,
    mode: String,
    kind: CompileKind,
    /// The unit as cargo interns it, less its profile: what a probe of cargo's compares.
    identity: String,
    deps: Vec<usize>,
}

impl Unit {
    fn for_host(&self) -> bool {
        self.custom_build || self.proc_macro
    }
    fn is_any_test(&self) -> bool {
        matches!(self.mode.as_str(), "test" | "doctest")
    }
}

/// A unit as cargo interns it before it shares the graph: the graph's unit, whether its kind
/// is the host's own (`CompileKind::Host`), the profile it was first interned with, and its
/// dependencies, computed the first time it was reached.
struct Interned {
    unit: usize,
    kind_host: bool,
    profile: Profile,
    deps: Option<Vec<Edge>>,
}

#[derive(Clone, Copy)]
enum Edge {
    To {
        to: usize,
        unit_for: UnitFor,
    },
    /// The run of a `links` dependency's build script, by the graph's unit.
    Links(usize),
}

/// The profiles of `graph`'s units, in its order, and the warnings of the package overrides
/// that match none of its packages.
fn profiles_of(
    ws: &Workspace<'_, Disk>,
    profiles: &Profiles,
    command: &Command,
    graph: &Value,
    stderr: &mut String,
) -> anyhow::Result<Vec<Profile>> {
    let units = graph["units"].as_array().ok_or_else(|| anyhow!("no `units` in the unit graph"))?;
    let mut parsed = Vec::new();
    for unit in units {
        let pkg = package(unit["pkg_id"].as_str().unwrap_or_default())?;
        let is_member = pkg.source_id.is_path()
            && pkg.source_id.url().to_file_path().is_ok_and(|p| ws.is_member(&p.join("Cargo.toml")));
        let kinds: Vec<&str> = unit["target"]["kind"]
            .as_array()
            .map(|k| k.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let kind = match unit["platform"].as_str() {
            Some(t) => CompileKind::Target(t.to_owned()),
            None => CompileKind::Host,
        };
        let identity = serde_json::to_string(&[
            &unit["pkg_id"],
            &unit["target"],
            &unit["mode"],
            &unit["features"],
        ])?;
        parsed.push(Unit {
            pkg,
            is_member,
            kinds: kinds.iter().map(|k| (*k).to_owned()).collect(),
            custom_build: kinds.contains(&"custom-build"),
            proc_macro: kinds.contains(&"proc-macro"),
            mode: unit["mode"].as_str().unwrap_or_default().to_owned(),
            kind,
            identity,
            deps: unit["dependencies"]
                .as_array()
                .map(|d| d.iter().filter_map(|d| d["index"].as_u64()).map(|i| i as usize).collect())
                .unwrap_or_default(),
        });
    }
    let locked = lock_packages(ws, &parsed)?;
    let mut packages: Vec<&Pkg> = Vec::new();
    if let Some(locked) = &locked {
        packages.extend(locked.iter());
    } else {
        let mut seen = blaze_hash::HashSet::default();
        for (unit, value) in parsed.iter().zip(units) {
            if seen.insert(value["pkg_id"].as_str()) {
                packages.push(&unit.pkg);
            }
        }
    }
    let mut warnings = Vec::new();
    profiles.validate_packages(ws.profiles(), &packages, &mut warnings)?;
    for warning in warnings {
        let _ = writeln!(stderr, "warning: {warning}");
    }
    let mut walk =
        Walk { units: &parsed, profiles, interned: Vec::new(), by_unit: HashMap::default() };
    let roots = graph["roots"].as_array().ok_or_else(|| anyhow!("no `roots` in the unit graph"))?;
    let is_test_command = matches!(command.name.as_str(), "test" | "bench");
    let requested = match &command.target {
        Some(t) => CompileKind::Target(t.clone()),
        None => CompileKind::Host,
    };
    let mut root_units = Vec::new();
    for root in roots {
        let i = root.as_u64().ok_or_else(|| anyhow!("a root is not an index"))? as usize;
        let unit = parsed.get(i).ok_or_else(|| anyhow!("root {i} is not a unit"))?;
        let profile_for = if unit.is_any_test() || is_test_command {
            UnitFor::new_test()
        } else if unit.for_host() {
            UnitFor::new_compiler()
        } else {
            UnitFor::new_normal()
        };
        let deps_for = if unit.is_any_test() || is_test_command {
            if unit.proc_macro { UnitFor::new_host_test() } else { UnitFor::new_test() }
        } else if unit.custom_build {
            UnitFor::new_host(false)
        } else if unit.proc_macro {
            UnitFor::new_host(true)
        } else if unit.for_host() {
            UnitFor::new_compiler()
        } else {
            UnitFor::new_normal()
        };
        let profile = walk.profile(i, profile_for, &requested);
        root_units.push((walk.intern(i, unit.for_host(), profile), deps_for));
    }
    for &(n, deps_for) in &root_units {
        walk.deps_of(n, deps_for);
    }
    let to_host = command.target.is_none();
    let mut shared: Vec<Option<Profile>> = vec![None; walk.interned.len()];
    for &(n, _) in &root_units {
        walk.share(n, false, to_host, &mut shared);
    }
    let mut out: Vec<Option<Profile>> = vec![None; parsed.len()];
    for (n, interned) in walk.interned.iter().enumerate() {
        let Some(profile) = shared[n].take() else { continue };
        match &out[interned.unit] {
            Some(prev) if serde_json::to_string(prev)? != serde_json::to_string(&profile)? => {
                bail!(
                    "unit {} ({}) gets two profiles:\n{}\n{}",
                    interned.unit,
                    parsed[interned.unit].identity,
                    serde_json::to_string(prev)?,
                    serde_json::to_string(&profile)?
                )
            }
            _ => out[interned.unit] = Some(profile),
        }
    }
    out.into_iter()
        .enumerate()
        .map(|(i, v)| v.ok_or_else(|| anyhow!("unit {i} is reached from no root")))
        .collect()
}

struct Walk<'a> {
    units: &'a [Unit],
    profiles: &'a Profiles,
    interned: Vec<Interned>,
    by_unit: HashMap<usize, Vec<usize>>,
}

impl Walk<'_> {
    fn profile(&self, i: usize, unit_for: UnitFor, kind: &CompileKind) -> Profile {
        let unit = &self.units[i];
        let is_local = unit.pkg.source_id.is_path();
        self.profiles.get_profile(&unit.pkg, unit.is_member, is_local, unit_for, kind)
    }

    /// cargo's interner: a unit equal to one interned before is that one, its profile included,
    /// and profiles compare without whether their debuginfo is deferred.
    fn intern(&mut self, unit: usize, kind_host: bool, profile: Profile) -> usize {
        let same = self.by_unit.get(&unit).and_then(|ns| {
            ns.iter().copied().find(|&n| {
                self.interned[n].kind_host == kind_host && self.interned[n].profile == profile
            })
        });
        if let Some(n) = same {
            return n;
        }
        let n = self.interned.len();
        self.interned.push(Interned { unit, kind_host, profile, deps: None });
        self.by_unit.entry(unit).or_default().push(n);
        n
    }

    /// cargo's `deps_of`: a unit's dependencies all interned, then each walked, once per unit.
    fn deps_of(&mut self, n: usize, unit_for: UnitFor) {
        if self.interned[n].deps.is_some() {
            return;
        }
        let i = self.interned[n].unit;
        let kind_host = self.interned[n].kind_host;
        let profile = self.interned[n].profile.clone();
        let unit = &self.units[i];
        let mut edges = Vec::new();
        for &d in &unit.deps {
            let dep = &self.units[d];
            let (dep_for, dep_profile, dep_kind_host) = if dep.mode == "run-custom-build" {
                if unit.mode == "run-custom-build" {
                    edges.push(Edge::Links(d));
                    continue;
                }
                let dep_for = unit_for.for_custom_build();
                (dep_for, self.profiles.get_profile_run_custom_build(&profile), kind_host)
            } else if unit.mode == "run-custom-build" {
                let dep_for = unit_for.for_custom_build();
                (dep_for, self.profile(d, dep_for, &CompileKind::Host), true)
            } else {
                let dep_for = if unit.mode == "test"
                    && unit.kinds.iter().any(|k| k == "test" || k == "bench")
                    && dep.kinds.iter().any(|k| k == "bin")
                {
                    // The binaries an integration test runs, built as the user would build them.
                    UnitFor::new_normal()
                } else {
                    unit_for.with_dependency(unit.custom_build, dep.for_host(), dep.proc_macro)
                };
                let dep_kind_host = kind_host || dep.for_host();
                let kind = if dep_kind_host { CompileKind::Host } else { dep.kind.clone() };
                (dep_for, self.profile(d, dep_for, &kind), dep_kind_host)
            };
            let to = self.intern(d, dep_kind_host, dep_profile);
            edges.push(Edge::To { to, unit_for: dep_for });
        }
        self.interned[n].deps = Some(edges.clone());
        for edge in edges {
            if let Edge::To { to, unit_for } = edge {
                self.deps_of(to, unit_for);
            }
        }
    }

    /// cargo's `traverse_and_share` as far as profiles go: `strip` lowered to debuginfo when
    /// neither the unit nor its dependencies have any, and the deferred debuginfo of a unit
    /// for the host lowered to none unless the same unit is built for the target too. The first
    /// way a unit is reached decides.
    fn share(&self, n: usize, for_host: bool, to_host: bool, shared: &mut Vec<Option<Profile>>) {
        if shared[n].is_some() {
            return;
        }
        let mut deps = Vec::new();
        for edge in self.interned[n].deps.iter().flatten() {
            let (to, unit_for) = match *edge {
                Edge::To { to, unit_for } => (to, unit_for),
                Edge::Links(d) => match self.by_unit.get(&d).and_then(|ns| ns.first()) {
                    Some(&to) => (to, UnitFor::new_host(false)),
                    None => continue,
                },
            };
            self.share(to, unit_for.is_for_host(), to_host, shared);
            deps.push(to);
        }
        let interned = &self.interned[n];
        let mut profile = interned.profile.clone();
        if profile.strip.is_deferred()
            && !profile.debuginfo.is_turned_on()
            && deps.iter().all(|&d| !shared[d].as_ref().is_some_and(|p| p.debuginfo.is_turned_on()))
        {
            profile.strip = profile.strip.strip_debuginfo();
        }
        if for_host && to_host && profile.debuginfo.is_deferred() {
            let canonical = profile.debuginfo.finalize();
            let mut probe = profile.clone();
            probe.debuginfo = canonical;
            let identity = &self.units[interned.unit].identity;
            let exists = self.interned.iter().any(|m| {
                !m.kind_host && self.units[m.unit].identity == *identity && m.profile == probe
            });
            profile.debuginfo = if exists { canonical } else { canonical.weaken() };
        }
        shared[n] = Some(profile);
    }
}
