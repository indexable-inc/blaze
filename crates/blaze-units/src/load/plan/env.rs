//! The variables cargo sets: for a compile (`compilation.rs`'s `fill_env` and
//! `fill_rustc_tool_env`, `prepare_rustc`, `build_deps_args`, `artifact::get_env`) and for a build
//! script's run (`custom_build.rs`'s `build_work`, through `host_process`'s `fill_env`). Each
//! command gets `[env]` for the variables cargo sets none of (`apply_env_config`).
//!
//! Left to the daemon, as blaze's own: `CARGO`, `RUSTC` and `RUSTDOC` (the programs cargo names),
//! the dynamic library search path, and the jobserver.

use std::path::Path;

use blaze_manifest::manifest::Manifest;
use blaze_resolve::PackageId;

use crate::query::LoadError;
use crate::stdlib::target as platform;

/// A command's variables as cargo sets them, a later one replacing an earlier one.
#[derive(Default)]
pub(super) struct Env(Vec<(String, String)>);

impl Env {
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.0.push((name.into(), value.into()));
    }

    pub fn extend(&mut self, pairs: &[(String, String)]) {
        self.0.extend(pairs.iter().cloned());
    }

    /// Sorted by name, with `[env]`'s (`config_env`, sorted) for the names cargo set none of.
    pub fn finish(self, config_env: &[(String, String)]) -> Vec<(String, String)> {
        let mut set = self.0;
        // Stable, so of one name the last set is the last of its run.
        set.sort_by(|a, b| a.0.cmp(&b.0));
        let mut out: Vec<(String, String)> = Vec::with_capacity(set.len() + config_env.len());
        for pair in set {
            match out.last_mut() {
                Some(last) if last.0 == pair.0 => *last = pair,
                _ => out.push(pair),
            }
        }
        let cargo_set = out.len();
        for (name, value) in config_env {
            if out[..cargo_set].binary_search_by(|it| it.0.as_str().cmp(name)).is_err() {
                out.push((name.clone(), value.clone()));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

/// What `fill_env` sets of a package for every command of it: `CARGO_MANIFEST_DIR`,
/// `CARGO_MANIFEST_PATH`, `CARGO_PKG_*` (the metadata's of `ManifestMetadata::env_vars` too).
pub(super) fn package_env(id: PackageId, manifest: &Manifest, manifest_path: &Path) -> Result<Vec<(String, String)>, LoadError> {
    let error = |what: String| LoadError { what, at: Some(manifest_path.display().to_string()) };
    let root = manifest_path
        .parent()
        .ok_or_else(|| error(format!("the manifest `{}` is in no directory", manifest_path.display())))?;
    let version = id.version();
    let meta = &manifest.metadata;
    let text = |it: &Option<String>| it.clone().unwrap_or_default();
    let pairs = [
        ("CARGO_MANIFEST_DIR", root.display().to_string()),
        ("CARGO_MANIFEST_PATH", manifest_path.display().to_string()),
        ("CARGO_PKG_VERSION_MAJOR", version.major.to_string()),
        ("CARGO_PKG_VERSION_MINOR", version.minor.to_string()),
        ("CARGO_PKG_VERSION_PATCH", version.patch.to_string()),
        ("CARGO_PKG_VERSION_PRE", version.pre.as_str().to_owned()),
        ("CARGO_PKG_VERSION", version.to_string()),
        ("CARGO_PKG_NAME", id.name().as_str().to_owned()),
        ("CARGO_PKG_DESCRIPTION", text(&meta.description)),
        ("CARGO_PKG_HOMEPAGE", text(&meta.homepage)),
        ("CARGO_PKG_REPOSITORY", text(&meta.repository)),
        ("CARGO_PKG_LICENSE", text(&meta.license)),
        ("CARGO_PKG_LICENSE_FILE", text(&meta.license_file)),
        ("CARGO_PKG_AUTHORS", meta.authors.join(":")),
        ("CARGO_PKG_RUST_VERSION", meta.rust_version.as_ref().map(ToString::to_string).unwrap_or_default()),
        ("CARGO_PKG_README", text(&meta.readme)),
    ];
    Ok(pairs.into_iter().map(|(name, value)| (name.to_owned(), value)).collect())
}

/// cargo's `envify`.
pub(super) fn envify(name: &str) -> String {
    name.chars().flat_map(char::to_uppercase).map(|c| if c == '-' { '_' } else { c }).collect()
}

/// What a compile's variables are made of beyond its package's.
pub(super) struct CompileEnv<'a> {
    pub package: &'a [(String, String)],
    pub crate_name: &'a str,
    /// `CARGO_BIN_NAME`, of an executable.
    pub bin_name: Option<&'a str>,
    pub primary: bool,
    /// `CARGO_TARGET_TMPDIR`, of a test or bench target.
    pub tmpdir: Option<String>,
    /// The `OUT_DIR` of the unit's build script's run.
    pub out_dir: Option<&'a str>,
    /// `CARGO_BIN_EXE_<name>`, of a test or bench target.
    pub bin_exes: Vec<(String, String)>,
}

pub(super) fn compile_env(it: CompileEnv<'_>, config_env: &[(String, String)]) -> Vec<(String, String)> {
    let mut env = Env::default();
    if let Some(name) = it.bin_name {
        env.set("CARGO_BIN_NAME", name);
    }
    env.set("CARGO_CRATE_NAME", it.crate_name);
    env.extend(it.package);
    if let Some(dir) = it.out_dir {
        env.set("OUT_DIR", dir);
    }
    for (name, path) in it.bin_exes {
        env.set(name, path);
    }
    if it.primary {
        env.set("CARGO_PRIMARY_PACKAGE", "1");
    }
    if let Some(dir) = it.tmpdir {
        env.set("CARGO_TARGET_TMPDIR", dir);
    }
    env.finish(config_env)
}

/// What a build script's run's variables are made of beyond its package's.
pub(super) struct RunEnv<'a> {
    pub package: &'a [(String, String)],
    pub out_dir: &'a str,
    pub jobs: Option<u32>,
    /// The triple the run's unit is for (`TARGET`), and the host's.
    pub target: &'a str,
    pub host: &'a str,
    pub debug: bool,
    pub opt_level: &'a str,
    /// `release` or `debug`, of the profile's root.
    pub profile: &'a str,
    pub linker: Option<&'a str>,
    pub links: Option<&'a str>,
    pub features: &'a [String],
    pub debug_assertions: bool,
    /// `rustc --print cfg` of the run's kind.
    pub cfg_lines: &'a [String],
    /// The kind's rustflags.
    pub rustflags: &'a [String],
}

pub(super) fn run_env(it: RunEnv<'_>, config_env: &[(String, String)]) -> Vec<(String, String)> {
    let mut env = Env::default();
    env.extend(it.package);
    env.set("OUT_DIR", it.out_dir);
    if let Some(jobs) = it.jobs {
        env.set("NUM_JOBS", jobs.to_string());
    }
    env.set("TARGET", it.target);
    env.set("DEBUG", it.debug.to_string());
    env.set("OPT_LEVEL", it.opt_level);
    env.set("PROFILE", it.profile);
    env.set("HOST", it.host);
    if let Some(linker) = it.linker {
        env.set("RUSTC_LINKER", linker);
    }
    if let Some(links) = it.links {
        env.set("CARGO_MANIFEST_LINKS", links);
    }
    // `trim-paths` is refused unless `none`, whose remap is empty.
    env.set("CARGO_TRIM_PATHS_SCOPE", "none");
    env.set("CARGO_TRIM_PATHS_REMAP", "");
    for feature in it.features {
        env.set(format!("CARGO_FEATURE_{}", envify(feature)), "1");
    }
    // cargo's `cfg_map`: the features first under `feature`, `debug_assertions` of the
    // profile, then the target's cfgs (less its `debug_assertions`).
    let mut feature = it.features.join(",");
    for (name, value) in platform::cargo_cfg_env(it.cfg_lines) {
        if name == "CARGO_CFG_FEATURE" {
            if !feature.is_empty() && !value.is_empty() {
                feature.push(',');
            }
            feature.push_str(&value);
        } else {
            env.set(name, value);
        }
    }
    env.set("CARGO_CFG_FEATURE", feature);
    if it.debug_assertions {
        env.set("CARGO_CFG_DEBUG_ASSERTIONS", "");
    }
    env.set("CARGO_ENCODED_RUSTFLAGS", it.rustflags.join("\x1f"));
    // Removed after `[env]` is applied: blaze has no rustc wrapper.
    let removed = ["RUSTFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"];
    let config_env: Vec<(String, String)> =
        config_env.iter().filter(|(name, _)| !removed.contains(&name.as_str())).cloned().collect();
    env.finish(&config_env)
}
