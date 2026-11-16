//! The environment a script is given: exactly what the Cargo Book's "Environment variables
//! Cargo sets for build scripts" documents, computed from the unit graph, and nothing of the
//! machine. Paths are the sandbox's view: the package at `/workspace/<folder>` (the workspace
//! root for a member, the unpacked package for a dependency) and `OUT_DIR` an in-memory folder
//! at `/target/debug/build/<name>-<config>/out`, the shape scripts parse (tempo-node takes the
//! profile from its fourth part from the end).
//!
//! What is set, for `check` (the dev profile):
//!
//! - `CARGO_MANIFEST_DIR`, `CARGO_MANIFEST_PATH`, `CARGO_MANIFEST_LINKS` (with `links`);
//! - `CARGO_FEATURE_<NAME>` = `1` for each feature, uppercased, `-` as `_`;
//! - `CARGO_CFG_<NAME>` for each cfg of the target the crate is built for: a flag as an empty
//!   value, a name with values as the values joined by `,` (in the target description's
//!   sorted order); `test` and `proc_macro`, which no `--print cfg` gives, left out;
//! - `OUT_DIR`, `TARGET`, `HOST`, `NUM_JOBS` `1`, `OPT_LEVEL` `0`, `DEBUG` `true`, `PROFILE`
//!   `debug`;
//! - `RUSTC` `rustc` and `RUSTDOC` `rustdoc` (names the sandbox's process host answers, not
//!   paths), `CARGO` `cargo`, `CARGO_ENCODED_RUSTFLAGS` empty;
//! - `CARGO_PKG_*` from the manifest, absent values as `""`;
//! - `DEP_<LINKS>_<KEY>` from each direct dependency's `links` metadata.
//!
//! Not set: `RUSTC_WRAPPER`, `RUSTC_WORKSPACE_WRAPPER`, `RUSTC_LINKER`, `CARGO_MAKEFLAGS`, `PATH`,
//! `HOME`, and every variable of the user's: a switch such as `LIBC_CI` is unset.

#![blaze::sans_io]

use blaze_ids::Hasher;
use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_unit_graph::{CfgAtom, Text};

/// Where the package is served in the sandbox.
pub const WORKSPACE: &str = "/workspace";

/// The manifest's package facts a script is told (`CARGO_PKG_*`); an absent value is `""`.
#[derive(Clone, Copy, Debug, Default)]
pub struct PackageFacts<'s> {
    pub name: &'s str,
    /// The full version: `2.1.2+zstd.1.5.7`.
    pub version: &'s str,
    /// Already joined by `:`, as Cargo joins them.
    pub authors: &'s str,
    pub description: &'s str,
    pub homepage: &'s str,
    pub repository: &'s str,
    pub license: &'s str,
    pub license_file: &'s str,
    pub rust_version: &'s str,
    pub readme: &'s str,
    pub links: Option<&'s str>,
}

/// One `links` metadata pair of a direct dependency: `DEP_<LINKS>_<KEY>=value`.
#[derive(Clone, Copy, Debug)]
pub struct DepMetadata<'s> {
    pub links: &'s str,
    pub key: &'s str,
    pub value: &'s str,
}

/// What the environment is computed from.
#[derive(Clone, Copy, Debug)]
pub struct Given<'s, 'u> {
    pub package: PackageFacts<'s>,
    /// The package's folder under [`WORKSPACE`], `/`-separated, empty for the root.
    pub folder: &'s str,
    /// The unit's features, sorted.
    pub features: &'s [Text<'u>],
    /// The cfgs of the target the crate is built for, sorted (the library unit's).
    pub target_cfg: &'s [CfgAtom<'u>],
    pub target: &'s str,
    pub host: &'s str,
    pub deps: &'s [DepMetadata<'s>],
}

/// The variables, sorted by name, each once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Env<'a> {
    pub vars: AVec<'a, (AString<'a>, AString<'a>)>,
}

impl Env<'_> {
    pub fn get(&self, name: &str) -> Option<&str> {
        let at = self.vars.binary_search_by(|(n, _)| n.as_str().cmp(name)).ok()?;
        Some(self.vars[at].1.as_str())
    }

    /// Appends the bytes the key hashes: a count, then each name and value length-prefixed.
    pub fn encode(&self, out: &mut AVec<'_, u8>) {
        out.extend_from_slice(&(self.vars.len() as u64).to_le_bytes());
        for (name, value) in self.vars.iter() {
            out.extend_from_slice(&(name.len() as u64).to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&(value.len() as u64).to_le_bytes());
            out.extend_from_slice(value.as_bytes());
        }
    }
}

/// `-` as `_`, uppercased: how Cargo spells a feature or cfg name in a variable's name.
fn shout<'a>(prefix: &str, name: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::with_capacity_in(prefix.len() + name.len(), alloc);
    out.push_str(prefix);
    for ch in name.chars() {
        out.push(if ch == '-' { '_' } else { ch.to_ascii_uppercase() });
    }
    out
}

/// The 16 hex digits naming a configuration in `OUT_DIR`'s path: of the package's name and
/// version, the features and the target and its cfgs.
fn config(given: &Given<'_, '_>) -> [u8; 16] {
    let mut hasher = Hasher::new("blaze v2 build-script out-dir 1");
    hasher.part(given.package.name.as_bytes()).part(given.package.version.as_bytes()).part(given.target.as_bytes());
    for feature in given.features {
        hasher.part(feature.as_bytes());
    }
    for atom in given.target_cfg {
        hasher.part(atom.name.as_bytes()).opt_part(atom.value.as_ref().map(|v| v.as_bytes()));
    }
    let id = hasher.finish();
    let mut out = [0; 16];
    for (at, byte) in id.0[..8].iter().enumerate() {
        out[at * 2] = b"0123456789abcdef"[usize::from(byte >> 4)];
        out[at * 2 + 1] = b"0123456789abcdef"[usize::from(byte & 15)];
    }
    out
}

/// `major.minor.patch[-pre][+build]` split as Cargo's `CARGO_PKG_VERSION_*` give it.
fn version_parts(version: &str) -> (&str, &str, &str, &str) {
    let core_pre = version.split_once('+').map_or(version, |(v, _)| v);
    let (core, pre) = core_pre.split_once('-').unwrap_or((core_pre, ""));
    let mut parts = core.splitn(3, '.');
    (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""), pre)
}

/// The script's environment.
pub fn env<'a>(given: &Given<'_, '_>, alloc: Alloc<'a>) -> Env<'a> {
    let mut vars: AVec<'a, (AString<'a>, AString<'a>)> = Vec::new_in(alloc);
    let mut set = |name: AString<'a>, value: &str| vars.push((name, String::from_str_in(value, alloc)));
    let named = |name: &str| String::from_str_in(name, alloc);

    let mut dir = String::from_str_in(WORKSPACE, alloc);
    if !given.folder.is_empty() {
        dir.push('/');
        dir.push_str(given.folder);
    }
    let mut manifest = dir.clone_in(alloc);
    manifest.push_str("/Cargo.toml");
    set(named("CARGO_MANIFEST_DIR"), dir.as_str());
    set(named("CARGO_MANIFEST_PATH"), manifest.as_str());
    if let Some(links) = given.package.links {
        set(named("CARGO_MANIFEST_LINKS"), links);
    }

    for feature in given.features {
        set(shout("CARGO_FEATURE_", feature, alloc), "1");
    }

    // `CARGO_CFG_*`: atoms of one name joined; the atoms are sorted, so a name's are adjacent.
    let mut at = 0;
    let cfg = given.target_cfg;
    while at < cfg.len() {
        let name = cfg[at].name.as_str();
        let mut end = at;
        let mut joined = String::new_in(alloc);
        while end < cfg.len() && cfg[end].name.as_str() == name {
            if let Some(value) = &cfg[end].value {
                if !joined.is_empty() {
                    joined.push(',');
                }
                joined.push_str(value);
            }
            end += 1;
        }
        if name != "test" && name != "proc_macro" {
            set(shout("CARGO_CFG_", name, alloc), joined.as_str());
        }
        at = end;
    }

    let config = config(given);
    let mut out_dir = String::from_str_in("/target/debug/build/", alloc);
    out_dir.push_str(given.package.name);
    out_dir.push('-');
    out_dir.push_str(core::str::from_utf8(&config).unwrap_or(""));
    out_dir.push_str("/out");
    set(named("OUT_DIR"), out_dir.as_str());
    set(named("TARGET"), given.target);
    set(named("HOST"), given.host);
    set(named("NUM_JOBS"), "1");
    set(named("OPT_LEVEL"), "0");
    set(named("DEBUG"), "true");
    set(named("PROFILE"), "debug");
    set(named("RUSTC"), "rustc");
    set(named("RUSTDOC"), "rustdoc");
    set(named("CARGO"), "cargo");
    set(named("CARGO_ENCODED_RUSTFLAGS"), "");

    let p = &given.package;
    let (major, minor, patch, pre) = version_parts(p.version);
    for (name, value) in [
        ("CARGO_PKG_NAME", p.name),
        ("CARGO_PKG_VERSION", p.version),
        ("CARGO_PKG_VERSION_MAJOR", major),
        ("CARGO_PKG_VERSION_MINOR", minor),
        ("CARGO_PKG_VERSION_PATCH", patch),
        ("CARGO_PKG_VERSION_PRE", pre),
        ("CARGO_PKG_AUTHORS", p.authors),
        ("CARGO_PKG_DESCRIPTION", p.description),
        ("CARGO_PKG_HOMEPAGE", p.homepage),
        ("CARGO_PKG_REPOSITORY", p.repository),
        ("CARGO_PKG_LICENSE", p.license),
        ("CARGO_PKG_LICENSE_FILE", p.license_file),
        ("CARGO_PKG_RUST_VERSION", p.rust_version),
        ("CARGO_PKG_README", p.readme),
    ] {
        set(named(name), value);
    }

    for dep in given.deps {
        let mut name = shout("DEP_", dep.links, alloc);
        name.push('_');
        let key = shout("", dep.key, alloc);
        name.push_str(&key);
        set(name, dep.value);
    }

    // Sorted by name; a later value of one name (a `DEP_` pair printed twice) wins, as the
    // last `export` would.
    vars.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    let mut kept: AVec<'a, (AString<'a>, AString<'a>)> = Vec::with_capacity_in(vars.len(), alloc);
    for pair in vars {
        match kept.last_mut() {
            Some(last) if last.0 == pair.0 => *last = pair,
            _ => kept.push(pair),
        }
    }
    Env { vars: kept }
}
