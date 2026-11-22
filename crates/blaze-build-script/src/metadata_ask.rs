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

use blaze_own::{AVec, Alloc, Vec};

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
