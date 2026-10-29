//! What a build script's run is declared to be made of, Nix's way (Andrew, 2026-10-27): its
//! package's source, its `OUT_DIR`, the toolchain closure (clang, lld, a libc sysroot,
//! pkg-config and a shell's tools, pinned from nixpkgs) and the system libraries its crate is
//! declared to need ([`libs_for`]), and nothing else. On Linux the run sees only that, in a
//! mount namespace; what it is made of is then all its run is a function of, so the key of
//! the run is of that ([`package_identity`], the closure's hashes) and not of what it was seen
//! reading.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use sha2::{Digest as _, Sha256};

use super::closure::{self, StorePath};
use super::home::cargo_home_of;
use crate::sandbox::{Bind, View};

/// The table of system libraries by crate, shipped with blaze.
const LIBS: &str = include_str!("libs.toml");

/// The variable that has build scripts run in their closure alone (`on`), until the remote
/// cache has the closure for everyone ([`closure::PROJECT`]): then it is how every script runs
/// on Linux.
pub const VAR: &str = "BLAZE_SCRIPT_VIEW";

/// The table of the workspace's root manifest that gives crates their system libraries,
/// before blaze's own table.
pub const SYSTEM_DEPS: &str = "[workspace.metadata.blaze.system-deps]";

/// What every build script is given of the closure, whatever its crate: the C and C++
/// compilers, their linker and archiver, the target's libc and kernel headers, pkg-config, and the
/// tools a `configure` or a `Makefile` runs. By the root `closure/pins.txt` pins.
pub const TOOLCHAIN: [&str; 15] = [
    "clang",
    "lld",
    "llvmPackages.bintools",
    "glibc.out",
    "glibc.dev",
    "linuxHeaders",
    "pkg-config",
    "bash",
    "coreutils",
    "gnumake",
    "gnused",
    "gawk",
    "gnugrep",
    "diffutils",
    "findutils",
];

/// Whether a run with `env` is to see its closure alone, as [`VAR`] says.
pub fn wanted(env: &[(String, String)]) -> bool {
    env.iter().any(|(name, value)| name == VAR && matches!(value.as_str(), "on" | "1"))
}

/// A run that sees its closure alone: the closure, what of its package it does not see, and the
/// `OUT_DIR`s of other packages it reads, all of which its key is of ([`key_of`]).
#[derive(Clone, Debug)]
pub struct InView {
    pub closure: Closure,
    pub hidden: Vec<PathBuf>,
    pub deps: Vec<PathBuf>,
}

/// What a crate's build script runs with: the roots it was given, by attribute, and every
/// store path of their closure, each with where blaze laid it out.
#[derive(Clone, Debug)]
pub struct Closure {
    /// The roots, the toolchain's first, each by attribute and store path.
    pub roots: Vec<(String, String)>,
    /// The attributes of the crate's own libraries, from the workspace or blaze's table.
    pub libs: Vec<String>,
    pub paths: Vec<(StorePath, PathBuf)>,
}

/// The closure of the build script of the crate `name` at `version`, laid out: the toolchain
/// and the crate's libraries ([`libs_for`] with the workspace's `overrides`). `Err` names what
/// is missing: a host pins.txt has nothing for, a library it does not pin, a path neither the
/// store nor the remote has.
pub fn closure_for(
    name: &str,
    version: &str,
    overrides: &BTreeMap<String, Vec<String>>,
) -> anyhow::Result<Closure> {
    let host = closure::host().with_context(|| {
        format!(
            "a build script runs in its closure on Linux alone, not on {}",
            std::env::consts::OS
        )
    })?;
    let pinned = closure::roots(host);
    let libs = libs_for(name, version, overrides)?;
    let mut roots = Vec::new();
    for attribute in TOOLCHAIN.iter().copied().chain(libs.iter().map(String::as_str)) {
        let Some(root) = pinned.iter().find(|it| it.attribute == attribute) else {
            anyhow::bail!(
                "the build script of `{name}` is to be given `{attribute}`, which blaze does not \
                 pin for {host} (crates/blaze-cargo/src/cargo/closure/pins.txt, written by \
                 harness/closure_pins.py)"
            );
        };
        if !roots.iter().any(|(_, it): &(String, StorePath)| *it == root.path) {
            roots.push((attribute.to_owned(), root.path.clone()));
        }
    }
    let wanted: Vec<StorePath> = roots.iter().map(|it| it.1.clone()).collect();
    // The scripts of one build take the closure from the remote into one store at once, and a
    // pack another process is fetching is one this process lacks: one takes it, the others
    // wait for it and see what it brought.
    let remote = remote();
    let _held = match &remote {
        Some(_) => Some(held_by_one(closure::PROJECT)?),
        None => None,
    };
    if remote.is_some() {
        closure::refresh()?;
    }
    let mut all = closure::closure_of(&wanted);
    if all.is_err()
        && let Some(remote) = &remote
    {
        let dir = super::daemon::store().context("BLAZE_CACHE turns the store off")?;
        crate::remote::Shared::lazily(&dir, remote, closure::PROJECT)?.pull()?;
        all = closure::closure_of(&wanted);
    }
    let all = all.with_context(|| format!("the closure the build script of `{name}` runs with"))?;
    let mut paths = Vec::with_capacity(all.len());
    for path in all {
        let dir = closure::materialize(&path)?;
        paths.push((path, dir));
    }
    let roots = roots.into_iter().map(|(attribute, it)| (attribute, it.path)).collect();
    Ok(Closure { roots, libs, paths })
}

/// The remote cache `BLAZE_REMOTE` names, when it names one.
fn remote() -> Option<String> {
    std::env::var("BLAZE_REMOTE").ok().filter(|it| !it.is_empty() && it != "off")
}

/// A lock of blaze's store that one process holds at a time, beside the store's own files,
/// until it is dropped.
fn held_by_one(what: &str) -> anyhow::Result<std::fs::File> {
    let dir = super::daemon::store().context("BLAZE_CACHE turns the store off")?;
    std::fs::create_dir_all(&dir)?;
    let lock = std::fs::File::create(dir.join(format!("taking-{what}")))
        .with_context(|| format!("the lock of {} beside blaze's store", dir.display()))?;
    lock.lock()?;
    Ok(lock)
}

/// The crates `[workspace.metadata.blaze.system-deps]` of the root manifest at `manifest` gives
/// libraries, each its roots: nothing when there is none. Anything but a list of roots is not
/// built yet and an error that says so.
pub fn system_deps(manifest: &Path) -> anyhow::Result<BTreeMap<String, Vec<String>>> {
    let Ok(text) = std::fs::read_to_string(manifest) else { return Ok(BTreeMap::new()) };
    let value: toml::Table =
        toml::from_str(&text).with_context(|| format!("reading {}", manifest.display()))?;
    let table = ["workspace", "metadata", "blaze", "system-deps"]
        .iter()
        .try_fold(&value, |table, key| table.get(*key)?.as_table());
    let mut out = BTreeMap::new();
    for (name, value) in table.into_iter().flatten() {
        let roots = value.as_array().and_then(|it| {
            it.iter().map(|it| it.as_str().map(str::to_owned)).collect::<Option<Vec<_>>>()
        });
        let roots = roots.with_context(|| {
            format!(
                "{SYSTEM_DEPS} in {}: `{name}` is a list of the roots closure/pins.txt names \
                 (`[\"openssl.out\", \"openssl.dev\"]`); `use-feature` and `path` are not built \
                 yet",
                manifest.display()
            )
        })?;
        out.insert(name.clone(), roots);
    }
    Ok(out)
}

/// Where the program `name` is in the closure, as the run sees it: the first root, in order,
/// whose `bin` has it.
fn program(closure: &Closure, name: &str) -> Option<String> {
    closure.roots.iter().find_map(|(_, path)| {
        let (_, dir) = closure.paths.iter().find(|it| it.0.path == *path)?;
        dir.join("bin").join(name).exists().then(|| format!("{path}/bin/{name}"))
    })
}

/// The variables of cargo's for a build script that it is given in its closure: those the
/// Cargo book names for build scripts, blaze's (its `RUSTC` is blaze) and the jobserver's.
/// None names a program or a directory of the machine's, which the run does not see.
fn kept_in_view(name: &str) -> bool {
    const NAMES: [&str; 17] = [
        "CARGO",
        "OUT_DIR",
        "TARGET",
        "HOST",
        "NUM_JOBS",
        "OPT_LEVEL",
        "DEBUG",
        "PROFILE",
        "RUSTC",
        "RUSTDOC",
        "RUSTC_BOOTSTRAP",
        "SOURCE_DATE_EPOCH",
        "MAKEFLAGS",
        "MFLAGS",
        "CARGO_MAKEFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        crate::as_rustc::TOOLCHAIN,
    ];
    let cargo = name.starts_with("CARGO_") && name != "CARGO_HOME" && name != "CARGO_TARGET_DIR";
    NAMES.contains(&name) || cargo || name.starts_with("DEP_")
}

/// The environment of a run in `closure`: of `env`, what [`kept_in_view`] keeps and what the
/// workspace named (`also`), then the closure's programs and libraries, as a nixpkgs build
/// is given them: `PATH` of the roots' `bin`, the compilers, pkg-config and where it finds the
/// libraries' `.pc` files, their headers and libraries for the compiler wrapper, and Nix's home
/// and temporary directory.
pub fn env_in(
    env: &[(String, String)],
    also: &std::collections::BTreeSet<String>,
    closure: &Closure,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> =
        env.iter().filter(|(name, _)| kept_in_view(name) || also.contains(name)).cloned().collect();
    let laid = |path: &str| closure.paths.iter().find(|it| it.0.path == path).map(|it| &it.1);
    let below = |path: &str, rest: &str| laid(path).is_some_and(|it| it.join(rest).is_dir());
    let mut bin = Vec::new();
    for (_, path) in &closure.roots {
        if below(path, "bin") {
            bin.push(format!("{path}/bin"));
        }
    }
    let mut set = |name: &str, value: String| {
        out.retain(|it| it.0 != name);
        out.push((name.to_owned(), value));
    };
    set("PATH", bin.join(":"));
    set("HOME", "/homeless-shelter".to_owned());
    set("TMPDIR", "/tmp".to_owned());
    for (name, program_name) in [("CC", "clang"), ("CXX", "clang++"), ("PKG_CONFIG", "pkg-config")]
    {
        if let Some(it) = program(closure, program_name) {
            set(name, it);
        }
    }
    for (name, program_name) in [("AR", "ar"), ("RANLIB", "ranlib"), ("NM", "nm")] {
        let found = program(closure, program_name).or_else(|| {
            closure.paths.iter().find_map(|(path, dir)| {
                let bin = dir.join("bin").join(program_name);
                bin.exists().then(|| format!("{}/bin/{program_name}", path.path))
            })
        });
        if let Some(it) = found {
            set(name, it);
        }
    }
    let of_libs: Vec<&str> = closure
        .roots
        .iter()
        .filter(|(attribute, _)| closure.libs.contains(attribute))
        .map(|it| it.1.as_str())
        .collect();
    let mut pc = Vec::new();
    let mut cflags = Vec::new();
    let mut ldflags = Vec::new();
    for path in &of_libs {
        for rest in ["lib/pkgconfig", "share/pkgconfig"] {
            if below(path, rest) {
                pc.push(format!("{path}/{rest}"));
            }
        }
        if below(path, "include") {
            cflags.push(format!("-isystem {path}/include"));
        }
        if below(path, "lib") {
            ldflags.push(format!("-L{path}/lib"));
        }
    }
    if !pc.is_empty() {
        set("PKG_CONFIG_PATH", pc.join(":"));
        set("PKG_CONFIG_PATH_FOR_TARGET", pc.join(":"));
    }
    // What C compiles names of where it ran (debug info, `__FILE__`) names the package and the
    // target directory as the same on every machine, so that what it leaves in `OUT_DIR` is.
    // The last that matches is taken: the target directory, which may be in the package.
    let var = |name: &str| env.iter().find(|it| it.0 == name).map(|it| it.1.clone());
    if let Some(package) = var("CARGO_MANIFEST_DIR") {
        cflags.push(format!("-ffile-prefix-map={package}=/package"));
    }
    if let Some(out) = var("OUT_DIR")
        && let Some(at) = out.rfind("/build/")
    {
        cflags.push(format!("-ffile-prefix-map={}=/target", &out[..at]));
    }
    if !cflags.is_empty() {
        set("NIX_CFLAGS_COMPILE", cflags.join(" "));
    }
    if !ldflags.is_empty() {
        set("NIX_LDFLAGS", ldflags.join(" "));
    }
    out.sort();
    out
}

/// What a run in its closure sees of the machine: its package read-only but for the target
/// directory and `.git` in it (`hidden`), `OUT_DIR` written, the directories of `read`
/// read-only (the script, blaze and the toolchain its `RUSTC` answers for, the `OUT_DIR`s of
/// the packages its `DEP_` variables name), and the closure at `/nix/store`. Each is where it
/// is on the machine but the closure. A hidden directory is a skeleton of the directories
/// below it that are bound, made in `scratch`.
pub fn view(
    package: &Path,
    out_dir: &Path,
    read: &[PathBuf],
    closure: &Closure,
    scratch: &Path,
) -> anyhow::Result<(View, Vec<PathBuf>)> {
    let hidden = hidden_in(package, out_dir);
    let mut binds = vec![Bind::read(package, package), Bind::write(out_dir, out_dir)];
    let mut dirs: Vec<PathBuf> = read.iter().filter(|it| it.is_dir()).cloned().collect();
    dirs.sort();
    dirs.dedup();
    let all = dirs.clone();
    dirs.retain(|it| !all.iter().any(|other| other != it && it.starts_with(other)));
    dirs.retain(|it| it != package && it != out_dir);
    binds.extend(dirs.iter().map(|it| Bind::read(it, it)));
    for (at, dir) in hidden.iter().enumerate() {
        let skeleton = scratch.join("blaze-view").join(at.to_string());
        _ = std::fs::remove_dir_all(&skeleton);
        std::fs::create_dir_all(&skeleton)
            .with_context(|| format!("making {}", skeleton.display()))?;
        for bind in &binds {
            if let Ok(rest) = bind.inside.strip_prefix(dir) {
                std::fs::create_dir_all(skeleton.join(rest))?;
            }
        }
        binds.push(Bind::read(dir, &skeleton));
    }
    for (path, dir) in &closure.paths {
        binds.push(Bind::read(&path.path, dir));
    }
    // `/bin/sh` is the closure's bash, as a Nix build has it: what `configure`, `make` and
    // `system()` run. Nothing else of `/bin` or `/usr` is there.
    let bash = closure.roots.iter().find(|it| it.0 == "bash");
    let bash = bash.and_then(|(_, path)| closure.paths.iter().find(|it| it.0.path == *path));
    if let Some((_, dir)) = bash {
        binds.push(Bind::read("/bin/sh", dir.join("bin/bash")));
    }
    Ok((View { binds }, hidden))
}

/// What of `package` a run does not see: the target directory `out_dir` is in, when it is in
/// the package, as the directory of the package it is below; and `.git`.
fn hidden_in(package: &Path, out_dir: &Path) -> Vec<PathBuf> {
    let mut hidden = Vec::new();
    if let Ok(rest) = out_dir.strip_prefix(package)
        && let Some(first) = rest.components().next()
    {
        hidden.push(package.join(first));
    }
    let git = package.join(".git");
    if git.exists() && !hidden.contains(&git) {
        hidden.push(git);
    }
    hidden
}

/// The directories of `env`'s `DEP_` variables that are `OUT_DIR`s of other packages' runs:
/// what the run reads of them, which it is keyed by.
pub fn dep_out_dirs(env: &[(String, String)]) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for (name, value) in env {
        if !name.starts_with("DEP_") {
            continue;
        }
        for part in value.split(':') {
            let path = Path::new(part);
            // `build/<package>-<hash>/out`, or `build/<package>/<hash>/out` (cargo's
            // build-dir layout 2).
            let out = path.ancestors().find(|it| {
                let above = |n: usize| it.ancestors().nth(n).and_then(Path::file_name);
                it.file_name().is_some_and(|it| it == "out")
                    && (above(2) == Some("build".as_ref()) || above(3) == Some("build".as_ref()))
            });
            if path.is_absolute()
                && let Some(out) = out
                && out.is_dir()
            {
                dirs.push(out.to_owned());
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Adds to `key` what a run in `closure` is made of besides its script, its arguments and its
/// environment: its package's source ([`package_identity`]), each store path of the closure by
/// its NarHash, and what each `OUT_DIR` of `deps` holds, by its path as `unplace` names it.
pub fn key_of(
    key: &mut super::keyed::Key,
    package: &Path,
    hidden: &[PathBuf],
    closure: &Closure,
    deps: &[PathBuf],
    unplace: &dyn Fn(&[u8]) -> Vec<u8>,
) -> anyhow::Result<()> {
    key.field(package_identity(package, cargo_home_of(package).as_deref(), hidden)?.as_bytes());
    for (path, _) in &closure.paths {
        key.field(path.path.as_bytes());
        key.field(path.nar_hash.to_string().as_bytes());
    }
    for dir in deps {
        let mut digest = Sha256::new();
        tree_digest(dir, dir, &[], &mut digest)?;
        key.field(&unplace(dir.as_os_str().as_encoded_bytes()));
        key.field(&digest.finalize());
    }
    Ok(())
}

/// What a run in its closure printed, `stdout`, as cargo is given it: a directory of the
/// closure that a `rustc-link-search` or `rustc-flags` line names is where blaze laid it out,
/// for the link, which runs outside the run's namespace, where `/nix/store` may not be. The
/// store keeps the run as it printed it.
pub fn for_cargo(stdout: &[u8]) -> Vec<u8> {
    let Some(dir) = closure::dir() else { return stdout.to_vec() };
    let Some(dir) = dir.to_str() else { return stdout.to_vec() };
    let text = String::from_utf8_lossy(stdout);
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let directive = line.trim_start_matches("cargo:").trim_start_matches(':');
        if directive.starts_with("rustc-link-search=") || directive.starts_with("rustc-flags=") {
            out.push_str(&line.replace("/nix/store/", &format!("{dir}/")));
        } else {
            out.push_str(line);
        }
    }
    out.into_bytes()
}

/// What a run in its closure that failed with `stderr` is told: what it ran with, what it
/// looked for that was not there, as its tools said, and where to give it more.
pub fn failed_note(name: &str, closure: &Closure, stderr: &[u8], root: Option<&Path>) -> String {
    let text = String::from_utf8_lossy(stderr);
    let looked: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|it| {
            it.contains("was not found in the pkg-config search path")
                || it.contains("No such file or directory")
                || it.contains("cannot find -l")
                || it.contains("command not found")
                || it.contains("not found")
        })
        .take(4)
        .collect();
    let given = match closure.libs.is_empty() {
        true => "the toolchain and the libc sysroot alone".to_owned(),
        false => format!("the toolchain, the libc sysroot and {}", closure.libs.join(", ")),
    };
    let looked = match looked.is_empty() {
        true => String::new(),
        false => format!(", and looked for what is not there: {}", looked.join("; ")),
    };
    let root =
        root.map_or("the workspace's root manifest".to_owned(), |it| it.display().to_string());
    let failed = format!("the build script of `{name}` failed in its closure, which has {given}");
    if text.contains("Read-only file system") {
        return format!(
            "{failed}, and wrote outside OUT_DIR, which is the one place a run may write; a \
             script that writes its package's sources runs outside with `unsandboxed-build-scripts \
             = [\"{name}\"]` under [workspace.metadata.blaze.sandbox] in {root}, and is never \
             kept nor pushed"
        );
    }
    let unset: Vec<&str> = text
        .lines()
        .filter(|it| {
            it.contains("environment variable")
                && (it.contains("missing") || it.contains("not set") || it.contains("NotPresent"))
        })
        .map(str::trim)
        .take(2)
        .collect();
    if looked.is_empty() && !unset.is_empty() {
        return format!(
            "{failed}, and cargo's variables alone: it read one it is not given ({}); a variable \
             the workspace sets for build scripts is given by naming it in `build-script-env` \
             under [workspace.metadata.blaze.sandbox] in {root}",
            unset.join("; ")
        );
    }
    format!(
        "{failed}{looked}; a library it needs is given by `{name} = [\"<attr>.out\", \
         \"<attr>.dev\"]` under {SYSTEM_DEPS} in {root}, or by a line for `{name}` in blaze's \
         crates/blaze-cargo/src/cargo/hermetic/libs.toml"
    )
}

/// An entry of the table: a crate's libraries, for the versions `versions` allows.
#[derive(serde_derive::Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    versions: Option<String>,
    libs: Vec<String>,
}

/// The system libraries the build script of the crate `name` at `version` is given: the
/// workspace's own entry (`build-script-libs`, `overrides`) when it has one, else the shipped
/// table's for that version, else none. `Err` is a table that says it wrongly.
pub fn libs_for(
    name: &str,
    version: &str,
    overrides: &BTreeMap<String, Vec<String>>,
) -> anyhow::Result<Vec<String>> {
    if let Some(libs) = overrides.get(name) {
        return Ok(libs.clone());
    }
    let table: BTreeMap<String, Entry> = toml::from_str(LIBS).context("blaze's libs.toml")?;
    let Some(entry) = table.get(name) else { return Ok(Vec::new()) };
    let allowed = match &entry.versions {
        None => true,
        Some(range) => {
            let range = semver::VersionReq::parse(range)
                .with_context(|| format!("libs.toml: `{name}`'s versions `{range}`"))?;
            semver::Version::parse(version).is_ok_and(|it| range.matches(&it))
        }
    };
    Ok(match allowed {
        true => entry.libs.clone(),
        false => Vec::new(),
    })
}

/// What the source of the package in `dir` is, for the key of a run of its script: a
/// registry's package by its index, name, version and the digest of its `.crate` (what
/// `Cargo.lock`'s checksum is of); a git dependency's by its checkout, which cargo names by
/// the commit and never changes; any other by every file below `dir` but those in `hidden`,
/// which its run does not see. `cargo_home` is where cargo keeps the first two.
pub fn package_identity(
    dir: &Path,
    cargo_home: Option<&Path>,
    hidden: &[PathBuf],
) -> anyhow::Result<String> {
    if let Some(home) = cargo_home {
        if let Ok(rest) = dir.strip_prefix(home.join("registry").join("src")) {
            let mut parts = rest.components();
            let index = parts.next().context("a registry's package outside an index")?;
            let package = parts.next().context("a registry's index with no package")?;
            let (index, package) = (index.as_os_str(), package.as_os_str());
            let file = home.join("registry").join("cache").join(index);
            let file = file.join(format!("{}.crate", package.to_string_lossy()));
            let bytes = std::fs::read(&file)
                .with_context(|| format!("the package {} came from", file.display()))?;
            return Ok(format!(
                "registry {} {} {}",
                index.to_string_lossy(),
                package.to_string_lossy(),
                hex(&Sha256::digest(&bytes))
            ));
        }
        if let Ok(rest) = dir.strip_prefix(home.join("git").join("checkouts")) {
            return Ok(format!("git {}", rest.display()));
        }
    }
    let mut digest = Sha256::new();
    tree_digest(dir, dir, hidden, &mut digest)?;
    Ok(format!("tree {}", hex(&digest.finalize())))
}

/// Adds each file below `dir` but those in `hidden` to `digest`, in order: its path below
/// `root`, its kind and whether it is executable, and what it has or where it leads.
fn tree_digest(
    root: &Path,
    dir: &Path,
    hidden: &[PathBuf],
    digest: &mut Sha256,
) -> anyhow::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("listing {}", dir.display()))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("listing {}", dir.display()))?;
    entries.sort_by_key(|it| it.file_name());
    for entry in entries {
        let path = entry.path();
        if hidden.iter().any(|it| *it == path) {
            continue;
        }
        let kind = entry.file_type()?;
        let name = path.strip_prefix(root).unwrap_or(&path).as_os_str().as_encoded_bytes();
        let mut field = |bytes: &[u8]| {
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(bytes);
        };
        field(name);
        if kind.is_symlink() {
            field(b"link");
            field(std::fs::read_link(&path)?.as_os_str().as_encoded_bytes());
        } else if kind.is_dir() {
            field(b"dir");
            tree_digest(root, &path, hidden, digest)?;
        } else {
            let metadata = entry.metadata()?;
            field(if super::script::is_executable(&metadata) { b"exe" } else { b"file" });
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            field(&Sha256::digest(&bytes));
        }
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|it| format!("{it:02x}")).collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn the_table_gives_a_crate_its_libraries_and_the_workspace_wins() {
        let none = BTreeMap::new();
        let openssl = libs_for("openssl-sys", "0.9.109", &none).unwrap();
        assert_eq!(openssl, ["openssl.out", "openssl.dev"]);
        assert!(libs_for("serde", "1.0.228", &none).unwrap().is_empty());
        let ours = BTreeMap::from([("openssl-sys".to_owned(), vec!["libressl.dev".to_owned()])]);
        assert_eq!(libs_for("openssl-sys", "0.9.109", &ours).unwrap(), ["libressl.dev"]);
    }

    #[test]
    fn every_root_of_the_table_and_the_toolchain_is_pinned_for_both_hosts() {
        let table: BTreeMap<String, Entry> = toml::from_str(LIBS).unwrap();
        for host in ["x86_64-linux", "aarch64-linux"] {
            let pinned: Vec<&str> = closure::roots(host).iter().map(|it| it.attribute).collect();
            for root in TOOLCHAIN {
                assert!(pinned.contains(&root), "{host}: the toolchain's `{root}`");
            }
            for (name, entry) in &table {
                assert!(!entry.libs.is_empty(), "{name}");
                if let Some(range) = &entry.versions {
                    semver::VersionReq::parse(range).unwrap();
                }
                for lib in &entry.libs {
                    assert!(pinned.contains(&lib.as_str()), "{host}: `{name}`'s `{lib}`");
                }
            }
        }
    }

    fn closure_at(root: &Path) -> Closure {
        let path = |name: &str| StorePath {
            path: format!("/nix/store/{name}"),
            nar_hash: closure::Sha256([0; 32]),
            nar_size: 0,
        };
        let names = ["a-clang", "b-pkg-config", "c-openssl-dev", "d-binutils"];
        for name in names {
            std::fs::create_dir_all(root.join(name).join("bin")).unwrap();
        }
        std::fs::write(root.join("a-clang/bin/clang"), b"").unwrap();
        std::fs::write(root.join("b-pkg-config/bin/pkg-config"), b"").unwrap();
        std::fs::write(root.join("d-binutils/bin/ar"), b"").unwrap();
        std::fs::create_dir_all(root.join("c-openssl-dev/lib/pkgconfig")).unwrap();
        std::fs::create_dir_all(root.join("c-openssl-dev/include")).unwrap();
        Closure {
            roots: vec![
                ("clang".into(), "/nix/store/a-clang".into()),
                ("pkg-config".into(), "/nix/store/b-pkg-config".into()),
                ("openssl.dev".into(), "/nix/store/c-openssl-dev".into()),
            ],
            libs: vec!["openssl.dev".into()],
            paths: names.iter().map(|it| (path(it), root.join(it))).collect(),
        }
    }

    #[test]
    fn a_failed_run_is_told_what_it_lacked() {
        let closure = Closure { roots: Vec::new(), libs: Vec::new(), paths: Vec::new() };
        let note = |stderr: &str| failed_note("x-sys", &closure, stderr.as_bytes(), None);
        let lib = note(
            "The system library `libudev` required by crate `x-sys` was not found in the pkg-config search path.",
        );
        assert!(
            lib.contains("[workspace.metadata.blaze.system-deps]") && lib.contains("libudev"),
            "{lib}"
        );
        let wrote = note(
            "Error: Os { code: 30, kind: ReadOnlyFilesystem, message: \"Read-only file system\" }",
        );
        assert!(wrote.contains("unsandboxed-build-scripts = [\"x-sys\"]"), "{wrote}");
        let unset =
            note("panicked at src/input.rs:1:1:\ncargo environment variable `FOO` is missing");
        assert!(unset.contains("build-script-env") && unset.contains("`FOO`"), "{unset}");
    }

    #[test]
    fn a_run_in_its_closure_has_cargo_s_variables_and_the_closure_s_tools_alone() {
        let root = std::env::temp_dir().join(format!("blaze-hermetic-env-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        let closure = closure_at(&root);
        let pair = |name: &str, value: &str| (name.to_owned(), value.to_owned());
        let env = [
            pair("CARGO", "/home/me/.cargo/bin/cargo"),
            pair("CARGO_PKG_NAME", "openssl-sys"),
            pair("CARGO_HOME", "/home/me/.cargo"),
            pair("DEP_Z_INCLUDE", "/t/debug/build/z-1/out/include"),
            pair("OUT_DIR", "/t/debug/build/openssl-sys-2/out"),
            pair("CC", "/usr/bin/gcc"),
            pair("PKG_CONFIG_PATH", "/usr/lib/pkgconfig"),
            pair("AWS_SECRET_ACCESS_KEY", "hunter2"),
            pair("GITHUB_TOKEN", "ghp_x"),
            pair("HOME", "/home/me"),
            pair("OPENSSL_DIR", "/opt/openssl"),
        ];
        let also = std::collections::BTreeSet::from(["OPENSSL_DIR".to_owned()]);
        let out = env_in(&env, &also, &closure);
        let got = |name: &str| out.iter().find(|it| it.0 == name).map(|it| it.1.as_str());
        assert_eq!(got("CARGO"), Some("/home/me/.cargo/bin/cargo"));
        assert_eq!(got("CARGO_PKG_NAME"), Some("openssl-sys"));
        assert_eq!(got("DEP_Z_INCLUDE"), Some("/t/debug/build/z-1/out/include"));
        assert_eq!(got("OPENSSL_DIR"), Some("/opt/openssl"));
        for gone in ["CARGO_HOME", "AWS_SECRET_ACCESS_KEY", "GITHUB_TOKEN"] {
            assert_eq!(got(gone), None, "{gone}");
        }
        assert_eq!(got("CC"), Some("/nix/store/a-clang/bin/clang"));
        assert_eq!(got("AR"), Some("/nix/store/d-binutils/bin/ar"));
        assert_eq!(got("PKG_CONFIG"), Some("/nix/store/b-pkg-config/bin/pkg-config"));
        assert_eq!(got("PKG_CONFIG_PATH"), Some("/nix/store/c-openssl-dev/lib/pkgconfig"));
        assert_eq!(
            got("NIX_CFLAGS_COMPILE"),
            Some("-isystem /nix/store/c-openssl-dev/include -ffile-prefix-map=/t/debug=/target")
        );
        assert_eq!(got("HOME"), Some("/homeless-shelter"));
        assert_eq!(
            got("PATH"),
            Some("/nix/store/a-clang/bin:/nix/store/b-pkg-config/bin:/nix/store/c-openssl-dev/bin")
        );
        _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_view_hides_the_target_directory_and_git_but_what_is_bound_in_them() {
        let root = std::env::temp_dir().join(format!("blaze-hermetic-view-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        let package = root.join("ws");
        let out_dir = package.join("target/debug/build/ws-1/out");
        let script = package.join("target/debug/build/ws-0");
        std::fs::create_dir_all(&out_dir).unwrap();
        std::fs::create_dir_all(&script).unwrap();
        std::fs::create_dir_all(package.join(".git")).unwrap();
        let closure = closure_at(&root.join("nix"));
        let scratch = package.join("target/debug/build/ws-1");
        let (view, hidden) =
            view(&package, &out_dir, &[script.clone()], &closure, &scratch).unwrap();
        assert_eq!(hidden, [package.join("target"), package.join(".git")]);
        let skeleton = scratch.join("blaze-view/0");
        assert!(skeleton.join("debug/build/ws-1/out").is_dir());
        assert!(skeleton.join("debug/build/ws-0").is_dir());
        let at = |inside: &Path| view.binds.iter().find(|it| it.inside == inside).unwrap();
        assert_eq!(at(&package.join("target")).outside, skeleton);
        assert!(at(&out_dir).writable && !at(&package).writable);
        assert_eq!(at(Path::new("/nix/store/a-clang")).outside, root.join("nix/a-clang"));
        _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_registry_package_s_cargo_home_and_its_dependencies_out_dirs() {
        let home = Path::new("/h/.cargo");
        let src = home.join("registry/src/index.crates.io-1/zlib-1.0");
        assert_eq!(cargo_home_of(&src).as_deref(), Some(home));
        assert_eq!(cargo_home_of(&home.join("git/checkouts/x-1/abc")).as_deref(), Some(home));
        assert_eq!(cargo_home_of(Path::new("/w/pkg")), None);
        let root = std::env::temp_dir().join(format!("blaze-hermetic-dep-{}", std::process::id()));
        let out = root.join("target/debug/build/libz-sys-1/out");
        std::fs::create_dir_all(out.join("include")).unwrap();
        let env = [
            ("DEP_Z_INCLUDE".to_owned(), out.join("include").display().to_string()),
            ("DEP_Z_STATIC".to_owned(), "1".to_owned()),
            ("CARGO_PKG_NAME".to_owned(), "x".to_owned()),
        ];
        assert_eq!(dep_out_dirs(&env), [out]);
        let layout2 = root.join("target/debug/build/libz-sys/0123/out");
        std::fs::create_dir_all(layout2.join("include")).unwrap();
        let env = [("DEP_Z_INCLUDE".to_owned(), layout2.join("include").display().to_string())];
        assert_eq!(dep_out_dirs(&env), [layout2]);
        _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_package_is_its_crate_its_commit_or_its_files() {
        let root = std::env::temp_dir().join(format!("blaze-hermetic-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        let src = home.join("registry/src/index.crates.io-1949cf8c6b5b557f/zlib-sys-1.2.3");
        std::fs::create_dir_all(&src).unwrap();
        let cache = home.join("registry/cache/index.crates.io-1949cf8c6b5b557f");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("zlib-sys-1.2.3.crate"), b"the crate").unwrap();
        let registry = package_identity(&src, Some(&home), &[]).unwrap();
        assert!(registry.starts_with("registry index.crates.io-"), "{registry}");
        assert!(registry.contains(" zlib-sys-1.2.3 "), "{registry}");
        let git = home.join("git/checkouts/zed-abc/0a1c519/crates/webrtc-sys");
        std::fs::create_dir_all(&git).unwrap();
        assert_eq!(
            package_identity(&git, Some(&home), &[]).unwrap(),
            "git zed-abc/0a1c519/crates/webrtc-sys"
        );
        let ws = root.join("ws");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::create_dir_all(ws.join("target/debug")).unwrap();
        std::fs::write(ws.join("build.rs"), b"fn main() {}").unwrap();
        std::fs::write(ws.join("src/lib.rs"), b"").unwrap();
        let hidden = [ws.join("target")];
        let first = package_identity(&ws, Some(&home), &hidden).unwrap();
        std::fs::write(ws.join("target/debug/x"), b"built").unwrap();
        assert_eq!(package_identity(&ws, Some(&home), &hidden).unwrap(), first);
        std::fs::write(ws.join("src/lib.rs"), b"// edited").unwrap();
        assert_ne!(package_identity(&ws, Some(&home), &hidden).unwrap(), first);
        _ = std::fs::remove_dir_all(&root);
    }
}
