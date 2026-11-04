//! The pinned native inputs a workspace's root manifest declares (docs/cargo.md, "Pinned native
//! inputs"): `[workspace.metadata.blaze.native.<name>]`, each naming a pin file that says, per
//! target, which archive to fetch and its sha256, the variable a build script reads the unpacked
//! directory from, and the packages whose build scripts are given it.
//!
//! ```toml
//! [workspace.metadata.blaze.native.llvm]
//! pin = "crates/hir-ty-codegen/llvm.json"
//! env = "LLVM_SYS_231_PREFIX"
//! packages = ["llvm-sys", "blaze-stencil/llvm-stencils"]
//! ```
//!
//! A package named `name/feature` is given the input only when its build script runs with
//! that feature on, as cargo names a feature of a dependency: a build without it (blaze's seed,
//! for wasm, which has no LLVM) asks for no archive.
//!
//! The pin file is the one home of the pin: a JSON object with `release` (the `https://` URL
//! the archives are under) and `targets`, each target triple's `{ "name", "sha256" }`; any other
//! top-level key is the pin's own (llvm.json's `version`, `commit`, ...). The load reads it
//! through the view, so an edit to it is an edit to the load, and hands on one
//! [`NativeInput`] per input and target some build script run of the graph needs. Anything else
//! is an error naming the table, the pin file and what is wrong; there is no default.

use std::{collections::BTreeMap, path::Path};

use blaze_manifest::files::Files;
use serde::Deserialize;

use crate::query::{LoadError, NativeInput};

/// The table, under the workspace's root manifest, each input is a table of.
pub(crate) const TABLE: &str = "[workspace.metadata.blaze.native]";

/// The variables an input may not be handed in: cargo's and blaze's own, and those a loader,
/// shell, make, compiler driver or reproducible build reads. Each is either set by cargo for
/// every script (so the input would be overridden, or override cargo) or changes how programs
/// run.
const RESERVED: &[&str] = &[
    "OUT_DIR",
    "TARGET",
    "HOST",
    "NUM_JOBS",
    "OPT_LEVEL",
    "DEBUG",
    "PROFILE",
    "CARGO",
    "PATH",
    "HOME",
    "TMP",
    "TEMP",
    "TMPDIR",
    "SHELL",
    "BASH_ENV",
    "ENV",
    "IFS",
    "MAKEFLAGS",
    "MFLAGS",
    "SDKROOT",
    "SOURCE_DATE_EPOCH",
    "MACOSX_DEPLOYMENT_TARGET",
    // What the `cc` and `pkg-config` crates and build systems read to pick and drive the
    // compiler: an input named so would swap the toolchain a script builds C with.
    "CC",
    "CXX",
    "AR",
    "CFLAGS",
    "CXXFLAGS",
    "CPPFLAGS",
    "LDFLAGS",
];

/// Prefixes no input's variable may start with, for the same reasons as [`RESERVED`]: `RUST`
/// covers `RUSTC`, `RUSTDOC`, `RUSTFLAGS` and `RUSTDOCFLAGS`; `LD_` and `DYLD_` the loaders';
/// `PKG_CONFIG` every `PKG_CONFIG*`; `CC_`, `CXX_`, `AR_`, `CFLAGS_`, `CXXFLAGS_`, `LDFLAGS_`,
/// `TARGET_` and `HOST_` the `cc` crate's per-target spellings (`CC_x86_64-...`, `TARGET_CC`,
/// `HOST_CFLAGS`).
const RESERVED_PREFIXES: &[&str] = &[
    "CARGO_",
    "DEP_",
    "RUST",
    "BLAZE_",
    "__BLAZE",
    "LD_",
    "DYLD_",
    "PKG_CONFIG",
    "CC_",
    "CXX_",
    "AR_",
    "CFLAGS_",
    "CXXFLAGS_",
    "LDFLAGS_",
    "TARGET_",
    "HOST_",
];

/// One build script run of the graph: its package's name, the platform it is built for (none
/// for the host) and its features.
pub(crate) struct ScriptRun<'a> {
    pub package: &'a str,
    pub platform: Option<&'a str>,
    pub features: &'a [String],
}

/// One input as the manifest declares it.
struct Declared {
    name: String,
    pin: String,
    env: String,
    packages: Vec<String>,
}

/// One target's archive in a pin file: its file name under `release`, and its sha256. The pin
/// file's other top-level keys are its own, so only this is read with serde.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PinTarget {
    name: String,
    sha256: String,
}

fn wrong(manifest: &Path, what: &str) -> LoadError {
    LoadError {
        what: format!("{TABLE} in {}: {what}", manifest.display()),
        at: Some(manifest.display().to_string()),
    }
}

/// The inputs `metadata` (the root manifest's `[workspace.metadata]`) declares, sorted by name.
fn declared(metadata: Option<&toml::Value>, manifest: &Path) -> Result<Vec<Declared>, LoadError> {
    let Some(table) = metadata.and_then(|it| it.get("blaze")).and_then(|it| it.get("native"))
    else {
        return Ok(Vec::new());
    };
    let table = table.as_table().ok_or_else(|| wrong(manifest, "is a table of inputs by name"))?;
    let root = manifest.parent().unwrap_or(Path::new("/"));
    let mut out: Vec<Declared> = Vec::with_capacity(table.len()); // alloc: a manifest's few inputs, once a load
    for (name, input) in table {
        let wrong = |what: &str| wrong(manifest, &format!("input `{name}`: {what}"));
        let input =
            input.as_table().ok_or_else(|| wrong("is a table with `pin`, `env` and `packages`"))?;
        let (mut pin, mut env, mut packages) = (None, None, None);
        for (key, value) in input {
            match key.as_str() {
                "pin" => {
                    let path =
                        value.as_str().ok_or_else(|| wrong("`pin` is a path, as a string"))?;
                    let absolute = super::files::normal(&root.join(path)).ok_or_else(|| {
                        wrong(&format!("`pin` names `{path}`, which is not a UTF-8 path"))
                    })?;
                    pin = Some(absolute);
                }
                "env" => {
                    let var = value
                        .as_str()
                        .ok_or_else(|| wrong("`env` is a variable's name, as a string"))?;
                    env = Some(
                        variable(var)
                            .map_err(|why| wrong(&format!("`env` is `{var}`, which {why}")))?,
                    );
                }
                "packages" => {
                    let list = value
                        .as_array()
                        .ok_or_else(|| wrong("`packages` is a list of package names"))?;
                    let mut names = list
                        .iter()
                        .map(|it| {
                            let name = it.as_str().ok_or_else(|| wrong("`packages` lists names, as strings"))?;
                            let (package, feature) = name.split_once('/').unwrap_or((name, "x"));
                            if package.is_empty() || feature.is_empty() || feature.contains('/') {
                                return Err(wrong(&format!("`packages` lists `{name}`, which is no `name` or `name/feature`")));
                            }
                            Ok(name.to_owned()) // alloc: a package's name, once per name the input lists
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    names.sort_unstable();
                    names.dedup();
                    if names.is_empty() {
                        return Err(wrong(
                            "`packages` is empty: it names the packages whose build scripts are given the input",
                        ));
                    }
                    packages = Some(names);
                }
                other => {
                    return Err(wrong(&format!(
                        "`{other}` is not known (`pin`, `env` and `packages` are)"
                    )));
                }
            }
        }
        let (Some(pin), Some(env), Some(packages)) = (pin, env, packages) else {
            return Err(wrong("needs all of `pin`, `env` and `packages`"));
        };
        out.push(Declared { name: name.clone(), pin, env, packages });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    // Two inputs in one variable of one script: which one it reads would be whichever was set last.
    let package = |it: &String| it.split_once('/').map_or(it.as_str(), |(name, _)| name).to_owned();
    for (i, a) in out.iter().enumerate() {
        for b in &out[i + 1..] {
            let theirs: Vec<String> = b.packages.iter().map(package).collect(); // alloc: an error check, once a pair
            if a.env == b.env
                && let Some(shared) = a.packages.iter().map(package).find(|it| theirs.contains(it))
            {
                return Err(wrong(
                    manifest,
                    &format!(
                        "inputs `{}` and `{}` both give `{shared}`'s build script `{}`",
                        a.name, b.name, a.env
                    ),
                ));
            }
        }
    }
    Ok(out)
}

/// `var` as a variable an input may be handed in: `[A-Z_][A-Z0-9_]*`, none of [`RESERVED`].
fn variable(var: &str) -> Result<String, &'static str> {
    let mut chars = var.chars();
    let first_ok = chars.next().is_some_and(|it| it.is_ascii_uppercase() || it == '_');
    if !first_ok || !chars.all(|it| it.is_ascii_uppercase() || it.is_ascii_digit() || it == '_') {
        return Err("is not of upper-case letters, digits and `_`, starting with a letter or `_`");
    }
    if RESERVED.contains(&var) || RESERVED_PREFIXES.iter().any(|it| var.starts_with(it)) {
        return Err("cargo, blaze or the loader sets or reads itself");
    }
    Ok(var.to_owned())
}

/// Whether `sha256` is 64 lowercase hex digits.
fn is_sha256(sha256: &str) -> bool {
    sha256.len() == 64
        && sha256.bytes().all(|it| it.is_ascii_digit() || (b'a'..=b'f').contains(&it))
}

/// The inputs the root manifest at `manifest` (its `[workspace.metadata]` is `metadata`)
/// declares that some run of `runs` is given, each for the target that run is built for (`host`
/// when it names none), read from its pin through `files`: sorted by name then target. A run
/// whose target its pin has no archive for is an error naming the input, the target, the pin
/// and the package.
pub(crate) fn needed<F: Files>(
    files: &F,
    metadata: Option<&toml::Value>,
    manifest: &Path,
    host: &str,
    runs: &[ScriptRun<'_>],
) -> Result<Vec<NativeInput>, LoadError> {
    let declared = declared(metadata, manifest)?;
    let mut out: Vec<NativeInput> = Vec::new();
    for input in &declared {
        // alloc: each target some run of the input's packages is built for, once a load
        let mut targets: Vec<(&str, &str)> = runs
            .iter()
            .filter(|run| crate::query::names_script(&input.packages, run.package, run.features))
            .map(|run| (run.platform.unwrap_or(host), run.package))
            .collect();
        if targets.is_empty() {
            continue;
        }
        targets.sort_unstable();
        targets.dedup_by(|a, b| a.0 == b.0);
        let bad = bad_pin(input, manifest);
        let (release, archives) = read_pin(files, input, manifest)?;
        for (target, package) in targets {
            let Some(archive) = archives.get(target) else {
                return Err(bad(format!(
                    "has no archive for target `{target}`, which `{package}`'s build script is built for (it has {})",
                    archives.keys().map(|it| format!("`{it}`")).collect::<Vec<_>>().join(", ")
                )));
            };
            out.push(NativeInput {
                name: input.name.clone(),
                target: target.to_owned(),
                env: input.env.clone(),
                packages: input.packages.clone(),
                url: format!("{release}/{}", archive.name),
                sha256: archive.sha256.clone(),
                pin: input.pin.clone(),
            });
        }
    }
    out.sort();
    Ok(out)
}

/// How an error about the pin of `input` (declared in `manifest`) says it.
fn bad_pin<'a>(input: &'a Declared, manifest: &'a Path) -> impl Fn(String) -> LoadError + 'a {
    move |what: String| LoadError {
        what: format!("the pin of native input `{}` ({TABLE} in {}), {}: {what}", input.name, manifest.display(), input.pin),
        at: Some(input.pin.clone()),
    }
}

/// The pin file of `input` read through `files`: its `release` URL without a trailing `/`, and
/// each target's archive, every one checked (one file name, a sha256).
fn read_pin<F: Files>(
    files: &F,
    input: &Declared,
    manifest: &Path,
) -> Result<(String, BTreeMap<String, PinTarget>), LoadError> {
    let bad = bad_pin(input, manifest);
    // Plain matches, not `map_err`/`ok_or_else` chains: stage 0 (0.20261030.1) can't resolve
    // those methods here; return to the chains after the next stage-0 promotion.
    let pin_path = Path::new(&input.pin);
    let bytes = match files.read(pin_path) {
        Ok(it) => it,
        Err(error) => return Err(bad(format!("can't be read: {error}"))),
    };
    let shape = "JSON with `release` and `targets` of `name` and `sha256`";
    let pin: serde_json::Map<String, serde_json::Value> = match serde_json::from_slice(&bytes) {
        Ok(it) => it,
        Err(error) => return Err(bad(format!("is not {shape}: {error}"))),
    };
    let release_raw = match pin.get("release") {
        Some(serde_json::Value::String(it)) => it.as_str(),
        _ => return Err(bad(format!("has no `release` string ({shape})"))),
    };
    let archives: BTreeMap<String, PinTarget> = match pin.get("targets") {
        None => return Err(bad(format!("has no `targets` ({shape})"))),
        Some(value) => match BTreeMap::<String, PinTarget>::deserialize(value) {
            Ok(it) => it,
            Err(error) => return Err(bad(format!("`targets` is not {shape}: {error}"))),
        },
    };
    let release = release_raw.trim_end_matches('/');
    if !release.starts_with("https://") || release.len() == "https://".len() {
        return Err(bad(format!("`release` is `{release_raw}`, not an `https://` URL")));
    }
    for (target, archive) in &archives {
        let one_part = !archive.name.is_empty()
            && archive.name != "."
            && archive.name != ".."
            && !archive.name.contains(['/', '\\', '?', '#', '%', '\0']);
        if !one_part {
            return Err(bad(format!("target `{target}`'s `name` is `{}`, not one file name", archive.name)));
        }
        if !is_sha256(&archive.sha256) {
            return Err(bad(format!("target `{target}`'s `sha256` is `{}`, not 64 lowercase hex digits", archive.sha256)));
        }
    }
    Ok((release.to_owned(), archives)) // alloc: a URL, once per pin read
}

/// Every input the root manifest at `manifest` (its `[workspace.metadata]` is `metadata`)
/// declares, with every target its pin file names, as `blaze.lock` pins them: whatever this
/// build needs, so the lock does not depend on what was built. Each pin is named relative to
/// `root`, the workspace's root, when it is under it.
pub(crate) fn locked_pins<F: Files>(
    files: &F,
    metadata: Option<&toml::Value>,
    manifest: &Path,
    root: &Path,
) -> Result<Vec<crate::lock::NativePin>, LoadError> {
    let declared = declared(metadata, manifest)?;
    let mut out = Vec::with_capacity(declared.len()); // alloc: a manifest's few inputs, once a load
    for input in &declared {
        let (_, archives) = read_pin(files, input, manifest)?;
        let pin = Path::new(&input.pin);
        let pin = pin.strip_prefix(root).unwrap_or(pin).display().to_string();
        let targets = archives.into_iter().map(|(target, archive)| (target, archive.sha256)).collect();
        out.push(crate::lock::NativePin { name: input.name.clone(), pin, targets });
    }
    Ok(out)
}
