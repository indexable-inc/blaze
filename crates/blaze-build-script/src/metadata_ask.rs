//! `cargo metadata`, spawned by a build script, read as one typed question (rule 1: never a
//! cargo that runs; the answer is made from the unit graph and the lock blaze already holds).
//! Sans-IO: the arguments in, a typed ask or a refusal by index out, as probe_ask.rs reads
//! `rustc`'s line.
//!
//! The case: turso's `hegeltest-c 0.30.1` build script runs cbindgen 0.29.4
//! (`Builder::with_crate(..).generate()`), whose `Cargo::load` spawns
//! `cargo metadata --all-features --format-version 1 --manifest-path /package/Cargo.toml`
//! (cbindgen src/bindgen/cargo/cargo_metadata.rs `metadata`; `--filter-platform <TARGET>` too
//! when the config sets `only_target_dependencies`). blaze refused it
//! (`NoSandbox { program: "cargo", .. OwnerHasNotAllowed }`, c1814 check.out:21), and the
//! header check that follows never ran.
//!
//! - [`is_cargo_metadata`]: whether a spawn is this question (`cargo` and `metadata` first).
//! - [`classify`]: its flags read into [`MetadataAsk`]: every flag of `cargo metadata` that can
//!   move the answer is read (features, `--no-deps`, `--filter-platform`, `--manifest-path`,
//!   `--format-version`); those that only move where or how cargo prints (`-q`, `-v`,
//!   `--color`) or whether it may touch the network or the lock (`--offline`, `--locked`,
//!   `--frozen`: blaze reads the lock and fetches nothing, so each holds) are taken; any other
//!   (`--config`, `-Z`, `-C`, `--lockfile-path`, an unknown one) is [`Refused`] by its index:
//!   an unread flag can change the answer.
//! - [`FIELDS`]: the format-1 fields an answer writes and which reader reads each, so the
//!   answerer (not yet written; the report's handoff) is held to what readers parse. A reader
//!   that deserializes with serde stops on a missing field it does not default, so a field
//!   cbindgen reads is never left out of any package.

#![blaze::sans_io]

use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_unit_graph::{UnitGraph, UnitId, UnitKind};

/// The `--format-version` `cargo metadata` has (cargo's `metadata.rs`: "only version 1 is
/// supported").
pub const FORMAT_VERSION: &str = "1";

/// Whether a spawn of `program` with `args` is `cargo metadata`: the program's last path
/// component is `cargo` and its first argument `metadata` (a `+toolchain` before it is
/// rustup's, which the sandbox's cargo is not, so it is not this question).
pub fn is_cargo_metadata(program: &str, args: &[&str]) -> bool {
    let name = program.rsplit('/').next().unwrap_or(program);
    name == "cargo" && args.first() == Some(&"metadata")
}

/// Which features are on in the packages the question selects (the manifest's package, or
/// every workspace member when it is a virtual manifest's).
#[derive(Debug, PartialEq, Eq)]
pub struct FeatureAsk<'a, 's> {
    /// `--all-features`.
    pub all: bool,
    /// `--no-default-features`.
    pub no_default: bool,
    /// `--features` / `-F` values, split at commas and spaces, sorted, each once.
    pub listed: AVec<'a, &'s str>,
}

/// What a `cargo metadata` asks: everything its answer is a function of besides the workspace's
/// manifests and lock.
#[derive(Debug, PartialEq, Eq)]
pub struct MetadataAsk<'a, 's> {
    /// `--manifest-path`: the manifest whose workspace is described. `None`: the working
    /// directory's (a script runs in its package's directory, `CARGO_MANIFEST_DIR`).
    pub manifest_path: Option<&'s str>,
    pub features: FeatureAsk<'a, 's>,
    /// `--no-deps`: the workspace members alone, and no `resolve`.
    pub no_deps: bool,
    /// `--filter-platform` values, sorted, each once: only the dependencies that build for one of
    /// them are in `packages` and `resolve`. Empty: every platform's.
    pub platforms: AVec<'a, &'s str>,
    /// `--format-version` was given. Not given, cargo writes version 1 and warns on stderr
    /// ("please specify `--format-version` flag explicitly to avoid compatibility problems"),
    /// which the answer writes too.
    pub format_given: bool,
}

/// Why a `cargo metadata` line is not read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// The spawn is not `cargo metadata` ([`is_cargo_metadata`]).
    NotMetadata,
    /// A flag `cargo metadata` may take that this reader does not, or one it reads and does
    /// not model (`--config`, `-Z`, `-C`, `--lockfile-path`): its answer is not known to hold.
    UnknownFlag,
    /// A flag that takes a value, last on the line.
    NoValue,
    /// A `--format-version` other than [`FORMAT_VERSION`].
    FormatVersion,
    /// A flag cargo takes once, given again (`--manifest-path`, `--format-version`).
    Repeated,
    /// A word that is no flag: `cargo metadata` takes no positional argument.
    Positional,
}

impl core::fmt::Display for Why {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Why::NotMetadata => "a spawn that is not `cargo metadata`",
            Why::UnknownFlag => "a flag blaze's reader of `cargo metadata` does not read (an unread flag can change the answer)",
            Why::NoValue => "a flag whose value is missing",
            Why::FormatVersion => "a `--format-version` other than 1, the one cargo has",
            Why::Repeated => "a flag cargo takes once, given twice",
            Why::Positional => "a word that is no flag: `cargo metadata` takes no positional argument",
        })
    }
}

/// A refusal: the argument at `at` (an index into the arguments after the program, so `0` is
/// `metadata` itself) and why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Refused {
    pub at: usize,
    pub why: Why,
}

impl core::fmt::Display for Refused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "`cargo metadata`, argument {}: {} (blaze-build-script metadata_ask.rs)", self.at, self.why)
    }
}

/// The flags read, by what they are.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Flag {
    AllFeatures,
    NoDefault,
    Features,
    NoDeps,
    Platform,
    ManifestPath,
    Format,
    /// Where or how cargo prints, or whether it may touch the network or the lock: no part of
    /// the answer blaze writes.
    Quiet,
}

/// The long flags: name, meaning, whether a value follows (`--x v` or `--x=v`).
const LONG: &[(&str, Flag, bool)] = &[
    ("--all-features", Flag::AllFeatures, false),
    ("--no-default-features", Flag::NoDefault, false),
    ("--features", Flag::Features, true),
    ("--no-deps", Flag::NoDeps, false),
    ("--filter-platform", Flag::Platform, true),
    ("--manifest-path", Flag::ManifestPath, true),
    ("--format-version", Flag::Format, true),
    ("--quiet", Flag::Quiet, false),
    ("--verbose", Flag::Quiet, false),
    ("--color", Flag::Quiet, true),
    ("--offline", Flag::Quiet, false),
    ("--locked", Flag::Quiet, false),
    ("--frozen", Flag::Quiet, false),
];

/// The short flags: letter, meaning, whether a value follows (`-F v` or `-Fv`).
const SHORT: &[(u8, Flag, bool)] = &[(b'F', Flag::Features, true), (b'q', Flag::Quiet, false), (b'v', Flag::Quiet, false)];

/// The argument at `at`: its flag, its value (`""` for none) and how many words it took.
fn read<'s>(args: &[&'s str], at: usize) -> Result<(Flag, &'s str, usize), Refused> {
    let arg = args[at];
    let valued = |flag: Flag, joined: Option<&'s str>| match joined {
        Some(value) => Ok((flag, value, 1)),
        None => args.get(at + 1).map(|value| (flag, *value, 2)).ok_or(Refused { at, why: Why::NoValue }),
    };
    if !arg.starts_with('-') || arg == "-" {
        return Err(Refused { at, why: Why::Positional });
    }
    if arg.starts_with("--") {
        let (name, joined) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (arg, None),
        };
        let Some((_, flag, takes)) = LONG.iter().find(|(long, _, _)| *long == name) else {
            return Err(Refused { at, why: Why::UnknownFlag });
        };
        return match (*takes, joined) {
            (true, _) => valued(*flag, joined),
            (false, None) => Ok((*flag, "", 1)),
            (false, Some(_)) => Err(Refused { at, why: Why::UnknownFlag }),
        };
    }
    let bytes = arg.as_bytes();
    let Some((_, flag, takes)) = bytes.get(1).and_then(|letter| SHORT.iter().find(|(short, _, _)| short == letter)) else {
        return Err(Refused { at, why: Why::UnknownFlag });
    };
    match *takes {
        true => valued(*flag, (arg.len() > 2).then(|| &arg[2..])),
        // `-qv` and `-vv`: letters without values, each one of them.
        false if bytes[1..].iter().all(|letter| SHORT.iter().any(|(short, _, takes)| short == letter && !takes)) => Ok((*flag, "", 1)),
        false => Err(Refused { at, why: Why::UnknownFlag }),
    }
}

/// Reads a spawn of `cargo` with `args` (after the program) as one `cargo metadata` question.
pub fn classify<'a, 's>(program: &str, args: &[&'s str], alloc: Alloc<'a>) -> Result<MetadataAsk<'a, 's>, Refused> {
    if !is_cargo_metadata(program, args) {
        return Err(Refused { at: 0, why: Why::NotMetadata });
    }
    let mut ask = MetadataAsk {
        manifest_path: None,
        features: FeatureAsk { all: false, no_default: false, listed: Vec::new_in(alloc.clone()) },
        no_deps: false,
        platforms: Vec::new_in(alloc),
        format_given: false,
    };
    let mut at = 1;
    while at < args.len() {
        let (flag, value, took) = read(args, at)?;
        match flag {
            Flag::AllFeatures => ask.features.all = true,
            Flag::NoDefault => ask.features.no_default = true,
            // Cargo's `--features`: "space or comma separated list of features".
            Flag::Features => ask.features.listed.extend(value.split([',', ' ']).filter(|one| !one.is_empty())),
            Flag::NoDeps => ask.no_deps = true,
            Flag::Platform => ask.platforms.push(value),
            Flag::ManifestPath if ask.manifest_path.is_some() => return Err(Refused { at, why: Why::Repeated }),
            Flag::ManifestPath => ask.manifest_path = Some(value),
            Flag::Format if ask.format_given => return Err(Refused { at, why: Why::Repeated }),
            Flag::Format if value != FORMAT_VERSION => return Err(Refused { at, why: Why::FormatVersion }),
            Flag::Format => ask.format_given = true,
            Flag::Quiet => {}
        }
        at += took;
    }
    ask.features.listed.sort_unstable();
    ask.features.listed.dedup();
    ask.platforms.sort_unstable();
    ask.platforms.dedup();
    Ok(ask)
}

/// Who reads a field of the answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reader {
    /// cbindgen 0.29.4's fork of `cargo_metadata` (src/bindgen/cargo/cargo_metadata.rs), with
    /// serde: a field it reads without `Option` or `#[serde(default)]` stops the parse when it is
    /// missing.
    Cbindgen,
    /// Written because format 1 has it; no reader met so far reads it.
    Format,
}

/// One field of the format-1 answer: its path (`packages[].targets[].src_path`), who reads it,
/// whether a missing one stops that reader, and what blaze writes it from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Field {
    pub path: &'static str,
    pub reader: Reader,
    pub required: bool,
    pub from: &'static str,
}

/// THE FIELDS AN ANSWER WRITES, each with its reader and where it comes from. Paths are the
/// sandbox's (the script's package at `/package`, each other package at its mount: rule 25, no
/// path of the machine).
pub const FIELDS: &[Field] = &[
    Field { path: "version", reader: Reader::Cbindgen, required: true, from: "1" },
    Field { path: "workspace_root", reader: Reader::Cbindgen, required: true, from: "the workspace root's mount; cbindgen joins `Cargo.lock` to it when `parse_deps` is on" },
    Field { path: "packages[]", reader: Reader::Cbindgen, required: true, from: "the unit graph's packages under the asked features and platforms (all of them; `--no-deps`: the members)" },
    Field { path: "packages[].name", reader: Reader::Cbindgen, required: true, from: "the lock's package name" },
    Field { path: "packages[].version", reader: Reader::Cbindgen, required: false, from: "the lock's package version" },
    Field { path: "packages[].id", reader: Reader::Cbindgen, required: true, from: "the package id spec (`registry+https://..#name@version`, `path+file:///..#version`) of the lock row" },
    Field { path: "packages[].source", reader: Reader::Cbindgen, required: false, from: "the lock's `source` (null for a path package)" },
    Field { path: "packages[].manifest_path", reader: Reader::Cbindgen, required: true, from: "the package's Cargo.toml at its mount; cbindgen compares it to `<crate_dir>/Cargo.toml` to find the binding crate" },
    Field { path: "packages[].features", reader: Reader::Cbindgen, required: true, from: "the manifest's `[features]` table, with the implicit features of optional dependencies" },
    Field { path: "packages[].dependencies[]", reader: Reader::Cbindgen, required: true, from: "the manifest's dependency tables (normal, dev, build; each `[target.'cfg'.*]`)" },
    Field { path: "packages[].dependencies[].name", reader: Reader::Cbindgen, required: true, from: "the dependency's package name (`package = ..`, else its key)" },
    Field { path: "packages[].dependencies[].source", reader: Reader::Cbindgen, required: false, from: "the dependency's source" },
    Field { path: "packages[].dependencies[].req", reader: Reader::Cbindgen, required: true, from: "the version requirement as cargo normalizes it (`^1.0`; `*` when none)" },
    Field { path: "packages[].dependencies[].kind", reader: Reader::Cbindgen, required: false, from: "null, `dev` or `build`" },
    Field { path: "packages[].dependencies[].optional", reader: Reader::Cbindgen, required: true, from: "`optional = true`" },
    Field { path: "packages[].dependencies[].uses_default_features", reader: Reader::Cbindgen, required: true, from: "`default-features`" },
    Field { path: "packages[].dependencies[].features", reader: Reader::Cbindgen, required: true, from: "`features = [..]`" },
    Field { path: "packages[].dependencies[].target", reader: Reader::Cbindgen, required: false, from: "the `[target.X]` key (cbindgen reads it as a cfg, `Cfg::load_metadata`)" },
    Field { path: "packages[].dependencies[].rename", reader: Reader::Format, required: false, from: "the dependency's key when `package = ..` renames it" },
    Field { path: "packages[].dependencies[].registry", reader: Reader::Format, required: false, from: "null (crates.io)" },
    Field { path: "packages[].dependencies[].path", reader: Reader::Format, required: false, from: "a path dependency's directory at its mount" },
    Field { path: "packages[].targets[]", reader: Reader::Cbindgen, required: true, from: "the manifest's targets, inferred ones too (the unit graph's targets)" },
    Field { path: "packages[].targets[].name", reader: Reader::Cbindgen, required: true, from: "the target's name" },
    Field { path: "packages[].targets[].kind", reader: Reader::Cbindgen, required: true, from: "`lib` (or its crate types), `bin`, `example`, `test`, `bench`, `custom-build`; cbindgen takes the first lib-like one's `src_path` as the crate to parse" },
    Field { path: "packages[].targets[].crate_types", reader: Reader::Cbindgen, required: false, from: "the target's crate types" },
    Field { path: "packages[].targets[].src_path", reader: Reader::Cbindgen, required: true, from: "the target's root at its mount" },
    Field { path: "packages[].targets[].edition", reader: Reader::Format, required: false, from: "the target's edition" },
    Field { path: "packages[].targets[].required-features", reader: Reader::Format, required: false, from: "the target's `required-features`, when it has any" },
    Field { path: "packages[].targets[].doctest", reader: Reader::Format, required: false, from: "whether its doc tests run" },
    Field { path: "packages[].targets[].test", reader: Reader::Format, required: false, from: "whether `cargo test` builds it" },
    Field { path: "packages[].edition", reader: Reader::Format, required: false, from: "the manifest's edition" },
    Field { path: "packages[].links", reader: Reader::Format, required: false, from: "the manifest's `links`" },
    Field { path: "packages[].metadata", reader: Reader::Format, required: false, from: "the manifest's `[package.metadata]`, as JSON" },
    Field { path: "workspace_members", reader: Reader::Format, required: false, from: "the members' package ids" },
    Field { path: "workspace_default_members", reader: Reader::Format, required: false, from: "the default members' package ids" },
    Field { path: "resolve", reader: Reader::Format, required: false, from: "the unit graph's edges (null under `--no-deps`); cbindgen's fork removed it (\"was causing parse failures\")" },
    Field { path: "target_directory", reader: Reader::Format, required: false, from: "a stated path in the sandbox with nothing in it (no target directory exists: rule 1)" },
];

/// Normalizes a `--manifest-path` value against `cwd` (resolving relative paths and `.` / `..`
/// components cleanly without touching the filesystem, sans-IO).
pub fn normalize_manifest_path<'a>(raw: &str, cwd: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut parts: AVec<'a, &str> = Vec::new_in(alloc);
    if !raw.starts_with('/') {
        for seg in cwd.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    let _ = parts.pop();
                }
                other => parts.push(other),
            }
        }
    }
    for seg in raw.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                let _ = parts.pop();
            }
            other => parts.push(other),
        }
    }
    let mut out = String::new_in(alloc);
    if parts.is_empty() {
        out.push('/');
        return out;
    }
    for part in parts.iter() {
        out.push('/');
        out.push_str(part);
    }
    out
}

/// Inputs for serializing a deterministic Cargo format-1 `cargo metadata` JSON payload from
/// Blaze's resolved [`UnitGraph`] without spawning `cargo` (Rule 1).
pub struct MetadataSpec<'s> {
    pub graph: &'s UnitGraph<'s>,
    /// The served unit (library/binary) whose package is the focal package of the build script.
    pub package: UnitId,
    /// Directory of `package` (`CARGO_MANIFEST_DIR`, e.g. real host path or `/package` for keying).
    pub package_dir: &'s str,
    /// Workspace root directory (same as `package_dir` for non-workspace packages).
    pub workspace_root: &'s str,
    /// Stated target directory path.
    pub target_dir: &'s str,
}

fn push_json_str(out: &mut AString<'_>, s: &str) {
    use core::fmt::Write as _;
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn push_pkg_id(out: &mut AString<'_>, is_focal: bool, package_dir: &str, name: &str, version: &str) {
    out.push('"');
    if is_focal {
        out.push_str("path+file://");
        for ch in package_dir.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c => out.push(c),
            }
        }
        out.push('#');
        out.push_str(name);
        out.push('@');
        out.push_str(version);
    } else {
        out.push_str("registry+https://github.com/rust-lang/crates.io-index#");
        out.push_str(name);
        out.push('@');
        out.push_str(version);
    }
    out.push('"');
}

fn target_kind_str<'u>(kind: &'u UnitKind<'u>) -> (&'static str, &'static str, bool, bool) {
    match kind {
        UnitKind::Lib => ("lib", "lib", true, true),
        UnitKind::ProcMacro => ("proc-macro", "proc-macro", true, true),
        UnitKind::Bin(_) => ("bin", "bin", false, true),
        UnitKind::BuildScript => ("custom-build", "bin", false, false),
        UnitKind::Test { .. } | UnitKind::IntegrationTest(_) => ("test", "bin", false, true),
        UnitKind::Example(_) => ("example", "bin", false, false),
        UnitKind::Bench(_) => ("bench", "bin", false, false),
    }
}

/// Formats deterministic Cargo format-1 `cargo metadata` JSON for `spec` covering every field
/// required by `cbindgen` and `cargo_metadata` ([`FIELDS`]) without spawning `cargo` (Rule 1).
pub fn format_metadata_json<'a>(spec: &MetadataSpec<'_>, no_deps: bool, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::new_in(alloc);
    let Some(focal_unit) = spec.graph.units.get(spec.package.index()) else {
        out.push_str("{\"packages\":[],\"workspace_members\":[],\"workspace_default_members\":[],\"resolve\":null,\"target_directory\":");
        push_json_str(&mut out, spec.target_dir);
        out.push_str(",\"version\":1,\"workspace_root\":");
        push_json_str(&mut out, spec.workspace_root);
        out.push_str(",\"metadata\":null}");
        return out;
    };
    let focal_pkg = focal_unit.package;

    // Collect distinct packages in deterministic order: focal package only when `no_deps`, else
    // every package reachable from `focal_pkg`'s units in `spec.graph`.
    let mut reachable_units: AVec<'a, usize> = Vec::new_in(alloc);
    let mut stack: AVec<'a, usize> = Vec::new_in(alloc);
    for (idx, unit) in spec.graph.units.iter().enumerate() {
        if unit.package == focal_pkg {
            reachable_units.push(idx);
            if !no_deps {
                stack.push(idx);
            }
        }
    }
    while let Some(cur) = stack.pop() {
        let Some(unit) = spec.graph.units.get(cur) else { continue };
        for dep in unit.deps.iter() {
            let dep_idx = dep.unit.index();
            if reachable_units.scan_for(&dep_idx).is_none() {
                reachable_units.push(dep_idx);
                stack.push(dep_idx);
            }
        }
        if let Some(bs) = unit.build_script {
            let bs_idx = bs.index();
            if reachable_units.scan_for(&bs_idx).is_none() {
                reachable_units.push(bs_idx);
                stack.push(bs_idx);
            }
        }
    }

    let mut pkg_reps: AVec<'a, usize> = Vec::new_in(alloc);
    for &idx in reachable_units.iter() {
        let Some(unit) = spec.graph.units.get(idx) else { continue };
        let seen = pkg_reps.iter().any(|&rep| spec.graph.units.get(rep).is_some_and(|u| u.package == unit.package));
        if !seen {
            pkg_reps.push(idx);
        }
    }
    pkg_reps.sort_unstable_by(|&a, &b| {
        let ua = spec.graph.units.get(a);
        let ub = spec.graph.units.get(b);
        match (ua, ub) {
            (Some(ua), Some(ub)) => (ua.name.as_str(), ua.version.as_str(), ua.package).cmp(&(ub.name.as_str(), ub.version.as_str(), ub.package)),
            _ => a.cmp(&b),
        }
    });

    out.push_str("{\"packages\":[");
    for (pkg_at, &rep_idx) in pkg_reps.iter().enumerate() {
        let Some(rep) = spec.graph.units.get(rep_idx) else { continue };
        if pkg_at > 0 {
            out.push(',');
        }
        let is_focal = rep.package == focal_pkg;
        let pkg_base = if is_focal { spec.package_dir } else { "/deps" };
        out.push_str("{\"name\":");
        push_json_str(&mut out, rep.name.as_str());
        out.push_str(",\"version\":");
        push_json_str(&mut out, rep.version.as_str());
        out.push_str(",\"id\":");
        push_pkg_id(&mut out, is_focal, spec.package_dir, rep.name.as_str(), rep.version.as_str());
        out.push_str(",\"source\":");
        if is_focal {
            out.push_str("null");
        } else {
            out.push_str("\"registry+https://github.com/rust-lang/crates.io-index\"");
        }

        // Dependencies from all units of this package in the unit graph (deduplicated by dep pkg + kind + rename).
        out.push_str(",\"dependencies\":[");
        let mut dep_count = 0usize;
        let mut seen_deps: AVec<'a, (&str, Option<&str>, Option<&str>)> = Vec::new_in(alloc);
        for unit in spec.graph.units.iter().filter(|u| u.package == rep.package) {
            let dep_kind: Option<&str> = match unit.kind {
                UnitKind::BuildScript => Some("build"),
                UnitKind::Test { .. } | UnitKind::IntegrationTest(_) | UnitKind::Example(_) | UnitKind::Bench(_) => Some("dev"),
                UnitKind::Lib | UnitKind::ProcMacro | UnitKind::Bin(_) => None,
            };
            for dep in unit.deps.iter() {
                let Some(dep_unit) = spec.graph.units.get(dep.unit.index()) else { continue };
                if dep_unit.package == rep.package {
                    continue;
                }
                let dep_pkg_name = dep_unit.name.as_str();
                let rename = (dep.name.as_str() != dep_unit.crate_name.as_str()).then_some(dep.name.as_str());
                let key = (dep_pkg_name, dep_kind, rename);
                if seen_deps.scan_for(&key).is_some() {
                    continue;
                }
                seen_deps.push(key);
                if dep_count > 0 {
                    out.push(',');
                }
                dep_count += 1;
                out.push_str("{\"name\":");
                push_json_str(&mut out, dep_pkg_name);
                out.push_str(",\"source\":\"registry+https://github.com/rust-lang/crates.io-index\",\"req\":\"*\",\"kind\":");
                match dep_kind {
                    Some(k) => push_json_str(&mut out, k),
                    None => out.push_str("null"),
                }
                out.push_str(",\"rename\":");
                match rename {
                    Some(r) => push_json_str(&mut out, r),
                    None => out.push_str("null"),
                }
                out.push_str(",\"optional\":false,\"uses_default_features\":true,\"features\":[],\"target\":null,\"registry\":null,\"path\":null}");
            }
        }
        out.push(']');

        // Targets of this package (Lib/ProcMacro/Bin/BuildScript), deduplicated by (kind, name, root).
        out.push_str(",\"targets\":[");
        let mut tgt_count = 0usize;
        let mut seen_tgts: AVec<'a, (&str, &str, &str)> = Vec::new_in(alloc);
        for unit in spec.graph.units.iter().filter(|u| u.package == rep.package) {
            if matches!(unit.kind, UnitKind::Test { .. }) {
                continue;
            }
            let (kind_str, crate_type, doctest, test) = target_kind_str(&unit.kind);
            let tgt_name = match &unit.kind {
                UnitKind::Bin(bin_name) => bin_name.as_str(),
                UnitKind::BuildScript => "build-script-build",
                _ => unit.name.as_str(),
            };
            let root_rel = unit.root.as_str();
            let key = (kind_str, tgt_name, root_rel);
            if seen_tgts.scan_for(&key).is_some() {
                continue;
            }
            seen_tgts.push(key);
            if tgt_count > 0 {
                out.push(',');
            }
            tgt_count += 1;
            out.push_str("{\"kind\":[");
            push_json_str(&mut out, kind_str);
            out.push_str("],\"crate_types\":[");
            push_json_str(&mut out, crate_type);
            out.push_str("],\"name\":");
            push_json_str(&mut out, tgt_name);
            out.push_str(",\"src_path\":");
            let mut src_path = String::from_str_in(pkg_base, alloc);
            if !is_focal {
                src_path.push('/');
                src_path.push_str(rep.name.as_str());
                src_path.push('-');
                src_path.push_str(rep.version.as_str());
            }
            src_path.push('/');
            src_path.push_str(root_rel);
            push_json_str(&mut out, src_path.as_str());
            out.push_str(",\"edition\":");
            if let Some(blaze) = unit.edition.blaze {
                push_json_str(&mut out, blaze.manifest());
            } else {
                use core::fmt::Write as _;
                out.push('"');
                let _ = write!(out, "{}", unit.edition.syntax);
                out.push('"');
            }
            out.push_str(",\"doctest\":");
            out.push_str(if doctest { "true" } else { "false" });
            out.push_str(",\"test\":");
            out.push_str(if test { "true" } else { "false" });
            out.push('}');
        }
        out.push(']');

        // Features map: sorted unique feature names across units of this package.
        let mut feat_names: AVec<'a, &str> = Vec::new_in(alloc);
        for unit in spec.graph.units.iter().filter(|u| u.package == rep.package) {
            for feat in unit.features.iter() {
                if feat_names.scan_for(&feat.as_str()).is_none() {
                    feat_names.push(feat.as_str());
                }
            }
        }
        feat_names.sort_unstable();
        out.push_str(",\"features\":{");
        for (fi, feat) in feat_names.iter().enumerate() {
            if fi > 0 {
                out.push(',');
            }
            push_json_str(&mut out, feat);
            out.push_str(":[]");
        }
        out.push('}');

        // Manifest path.
        out.push_str(",\"manifest_path\":");
        let mut manifest_path = String::from_str_in(pkg_base, alloc);
        if !is_focal {
            manifest_path.push('/');
            manifest_path.push_str(rep.name.as_str());
            manifest_path.push('-');
            manifest_path.push_str(rep.version.as_str());
        }
        manifest_path.push_str("/Cargo.toml");
        push_json_str(&mut out, manifest_path.as_str());

        out.push_str(",\"metadata\":null,\"edition\":");
        if let Some(blaze) = rep.edition.blaze {
            push_json_str(&mut out, blaze.manifest());
        } else {
            use core::fmt::Write as _;
            out.push('"');
            let _ = write!(out, "{}", rep.edition.syntax);
            out.push('"');
        }
        out.push_str(",\"links\":");
        match rep.links.as_ref() {
            Some(links) => push_json_str(&mut out, links.as_str()),
            None => out.push_str("null"),
        }
        out.push('}');
    }
    out.push_str("],\"workspace_members\":[");
    push_pkg_id(&mut out, true, spec.package_dir, focal_unit.name.as_str(), focal_unit.version.as_str());
    out.push_str("],\"workspace_default_members\":[");
    push_pkg_id(&mut out, true, spec.package_dir, focal_unit.name.as_str(), focal_unit.version.as_str());
    out.push_str("],\"resolve\":null,\"target_directory\":");
    push_json_str(&mut out, spec.target_dir);
    out.push_str(",\"version\":1,\"workspace_root\":");
    push_json_str(&mut out, spec.workspace_root);
    out.push_str(",\"metadata\":null}");
    out
}
