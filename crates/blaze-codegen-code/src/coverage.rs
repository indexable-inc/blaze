//! The dep-info check of an identity: what building the closure's crates read, as cargo's
//! dep-info and build-script output in a target directory say, held to what the identity is of.
//! A test of a package the program's closure is built for calls [`hold`] on its own target
//! directory: hir-ty-codegen's for [`crate::CODEGEN`], blaze-cargo's for [`crate::FRONT_END`] and
//! [`crate::RECORD`].

use std::path::{Path, PathBuf};

use crate::{Worked, Workspace};

/// What [`hold`] found: every read not covered, and what it could not check.
#[derive(Debug, Default)]
pub struct Held {
    /// The units the test executable was built from.
    pub units: usize,
    /// The dep-infos of the closure's crates read, and the files they say were read.
    pub crates: usize,
    pub read: usize,
    /// The closure's build scripts whose output was read, and their rerun directives.
    pub scripts: usize,
    pub directives: usize,
    /// Each file or variable read that the identity is not of, named with where it was read.
    pub missing: Vec<String>,
    /// The closure's crates of the checkout with no dep-info among the units: not checked.
    pub unchecked: Vec<String>,
}

impl Held {
    /// Says what was checked and what was not, and fails naming every read the identity is
    /// not of.
    pub fn assert_covered(&self, what: &str) {
        eprintln!(
            "{} units; {} dep-infos of {what}'s crates, {} files read; {} build scripts, {} rerun directives",
            self.units, self.crates, self.read, self.scripts, self.directives
        );
        if !self.unchecked.is_empty() {
            eprintln!("not built for this test, so not checked: {}", self.unchecked.join(", "));
        }
        assert!(
            self.crates > 0 && self.read > 0,
            "no dep-info of {what}'s crates is in the target directory"
        );
        assert!(
            self.missing.is_empty(),
            "{what} read what its code identity is not of:\n{}",
            self.missing.join("\n")
        );
    }
}

/// Holds `worked`, an identity of `workspace`'s program, to what building it read: every file
/// and variable rustc read building the closure's crates, as their dep-info (`<crate>-<hash>.d`
/// in `made`, the target directory, among the units the test executable `exe` was built from)
/// says, is a file of a package it hashes, of a registry package it keys by checksum, or one a
/// build script made (from hashed inputs); a variable it hashes, one cargo derives from the
/// manifest, or one a build script set. What the closure's build scripts say they read
/// (`rerun-if-env-changed`, `rerun-if-changed`) is an input too. No build script of the closure
/// works out an identity (only the program's does, a leaf past every closure), so none may read
/// or watch the lock. Anything else would be an input missing from every key the identity is in
/// (AGENTS.md rule 21), and is named in [`Held::missing`].
pub fn hold(
    workspace: &Workspace,
    worked: &Worked,
    made: &Path,
    exe: &Path,
) -> Result<Held, String> {
    // The target directory, the parent of the `tmp` cargo gives integration tests (the
    // executable may run from a copy elsewhere): every unit's dep-info is under it, in either of
    // cargo's layouts, and so is everything build scripts made.
    let mut dep_infos = Vec::new();
    let mut outputs = Vec::new();
    let mut fingerprints = Vec::new();
    let mut stack = vec![made.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().is_ok_and(|it| it.is_dir()) {
                if name != "incremental" {
                    stack.push(path);
                }
            } else if dir.file_name().is_some_and(|it| it == "fingerprint")
                || dir.parent().and_then(|it| it.file_name()).is_some_and(|it| it == ".fingerprint")
            {
                fingerprints.push(path);
            } else if name.ends_with(".d") {
                dep_infos.push(path);
            } else if name == "output" || (name == "stdout" && dir.ends_with("run")) {
                outputs.push(path);
            }
        }
    }
    // The unit (cargo's metadata hash) a file of the target directory is of: in the new layout
    // `build/<package>/<unit>/{fingerprint,out,run}/..`; in the old one
    // `.fingerprint/<package>-<unit>/..`, `build/<package>-<unit>/..` or `deps/<crate>-<unit>.d`.
    let unit_of = |path: &Path| -> Option<String> {
        let below = path.strip_prefix(made).ok()?;
        let parts: Vec<&str> = below.iter().map(|it| it.to_str()).collect::<Option<_>>()?;
        let at = parts.iter().position(|it| matches!(*it, "build" | ".fingerprint" | "deps"))?;
        let hex = |it: &str| it.len() == 16 && it.bytes().all(|b| b.is_ascii_hexdigit());
        let suffix = |it: &str| {
            it.rsplit_once('-').map(|(_, unit)| unit.to_owned()).filter(|unit| hex(unit))
        };
        match parts[at] {
            "build" => match (parts.get(at + 2), parts.get(at + 3)) {
                (Some(unit), Some(&("fingerprint" | "out" | "run"))) if hex(unit) => {
                    Some((*unit).to_owned())
                }
                _ => suffix(parts.get(at + 1)?),
            },
            ".fingerprint" => suffix(parts.get(at + 1)?),
            _ => suffix(parts.get(at + 1)?.strip_suffix(".d")?),
        }
    };
    // The units this test's build made it of, and nothing else in the shared target directory
    // (a test of the identity's library, built or being built by another run, is no part
    // of blaze): a unit's fingerprint (`<kind>-<name>.json`) lists the fingerprint of each unit
    // it was built from, which that unit's `<kind>-<name>` file holds (the hex of its
    // little-endian bytes). Walked from this test's own unit, they are the identity's
    // library and everything it was built from, as the build that links it into blaze compiles
    // them.
    let mut by_fingerprint: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();
    let mut jsons: std::collections::BTreeMap<String, Vec<&Path>> =
        std::collections::BTreeMap::new();
    for path in &fingerprints {
        let (Some(name), Some(unit)) = (path.file_name().and_then(|it| it.to_str()), unit_of(path))
        else {
            continue;
        };
        if name.ends_with(".json") {
            jsons.entry(unit).or_default().push(path);
        } else if !(name.starts_with("dep-")
            || name.starts_with("output-")
            || name == "invoked.timestamp")
        {
            let Ok(hash) = std::fs::read_to_string(path) else { continue };
            by_fingerprint.entry(hash.trim().to_owned()).or_default().insert(unit);
        }
    }
    let own_unit = exe
        .file_stem()
        .and_then(|it| it.to_str())
        .and_then(|it| it.rsplit_once('-'))
        .map(|(_, unit)| unit.to_owned())
        .ok_or_else(|| format!("no unit hash in the test's name {}", exe.display()))?;
    if !jsons.contains_key(&own_unit) {
        return Err(format!("the test's unit {own_unit} has no fingerprint in {}", made.display()));
    }
    let mut units = std::collections::BTreeSet::new();
    let mut queue = vec![own_unit.clone()];
    while let Some(unit) = queue.pop() {
        for json in jsons.get(&unit).into_iter().flatten() {
            let text =
                std::fs::read_to_string(json).map_err(|e| format!("{}: {e}", json.display()))?;
            let deps = dep_fingerprints(&text)
                .ok_or_else(|| format!("no `deps` of cargo's form in {}", json.display()))?;
            for (name, fingerprint) in deps {
                let hex = format!("{:016x}", fingerprint.swap_bytes());
                let Some(made_by) = by_fingerprint.get(&hex) else {
                    return Err(format!(
                        "{name}, which {} was built from, has no unit of fingerprint {hex} in {}: the target \
                         directory no longer holds what the test was built from",
                        json.display(),
                        made.display()
                    ));
                };
                for dep in made_by {
                    if units.insert(dep.clone()) {
                        queue.push(dep.clone());
                    }
                }
            }
        }
    }
    // `build/<package>/<hash>/run/stdout`, or `build/<package>-<hash>/output`.
    let package_of = |output: &Path| {
        if output.ends_with("run/stdout") {
            output
                .ancestors()
                .nth(3)
                .and_then(|it| it.file_name())
                .and_then(|it| it.to_str())
                .map(str::to_owned)
        } else {
            output
                .parent()
                .and_then(|it| it.file_name())
                .and_then(|it| it.to_str())
                .and_then(|it| it.rsplit_once('-'))
                .map(|(name, _)| name.to_owned())
        }
    };
    // N5: only the closure's build scripts set what the closure's crates read.
    let mut set_by_scripts = std::collections::BTreeSet::new();
    outputs.retain(|it| unit_of(it).is_some_and(|unit| units.contains(&unit)));
    for output in &outputs {
        if !package_of(output).is_some_and(|it| worked.packages.contains(&it)) {
            continue;
        }
        let Ok(output) = std::fs::read_to_string(output) else { continue };
        for line in output.lines() {
            let set = line
                .strip_prefix("cargo:rustc-env=")
                .or_else(|| line.strip_prefix("cargo::rustc-env="));
            if let Some((name, _)) = set.and_then(|it| it.split_once('=')) {
                set_by_scripts.insert(name.to_owned());
            }
        }
    }
    // `CARGO_MANIFEST_LINKS` is the manifest's `links` (ring's build script says it reads it).
    let derived = |name: &str| {
        name.starts_with("CARGO_PKG_")
            || name.starts_with("CARGO_CFG_")
            || matches!(
                name,
                "CARGO_MANIFEST_DIR"
                    | "CARGO_MANIFEST_PATH"
                    | "CARGO_MANIFEST_LINKS"
                    | "CARGO_CRATE_NAME"
                    | "CARGO_PRIMARY_PACKAGE"
                    | "OUT_DIR"
            )
    };
    let hashed_env: std::collections::BTreeSet<&str> =
        worked.inputs.iter().filter(|it| it.kind == "env").map(|it| it.name.as_str()).collect();

    let files: std::collections::BTreeSet<PathBuf> =
        worked.files.iter().filter_map(|it| it.canonicalize().ok()).collect();
    // A file of a package of the checkout counts only if hashed (not one of its skipped
    // `tests/` or dotfiles); one of a registry package (`<registry>/src/<index>/<name>-<version>/`)
    // by its checksum; one of the target directory only as what a closure package's build script
    // made (`build/<package>-<hash>/out/` or `build/<package>/<hash>/out/`, from hashed inputs);
    // one of the pinned LLVM's prefix, checked to be the pin, by the pin.
    let registry: std::collections::BTreeSet<String> =
        worked.registry.iter().map(|(name, version)| format!("{name}-{version}")).collect();
    let in_registry = |path: &Path| {
        let parts: Vec<&std::ffi::OsStr> = path.iter().collect();
        parts.windows(4).any(|it| {
            it[0] == "registry"
                && it[1] == "src"
                && it[3].to_str().is_some_and(|package| registry.contains(package))
        })
    };
    let made_dir = made.canonicalize().map_err(|why| format!("{}: {why}", made.display()))?;
    let made_by_closure = |path: &Path| {
        let Ok(below) = path.strip_prefix(&made_dir) else { return false };
        let parts: Vec<&str> = below.iter().filter_map(|it| it.to_str()).collect();
        parts.iter().position(|it| *it == "build").is_some_and(|at| {
            let (package, out) = match (parts.get(at + 1), parts.get(at + 2)) {
                (Some(unit), Some(&"out")) => {
                    (unit.rsplit_once('-').map_or(*unit, |(name, _)| name), at + 2)
                }
                (Some(package), Some(_)) => (*package, at + 3),
                _ => return false,
            };
            worked.packages.contains(package) && parts.get(out) == Some(&"out")
        })
    };
    let llvm_prefix = worked.llvm_prefix.as_ref().and_then(|it| it.canonicalize().ok());
    let dirs: Vec<PathBuf> = worked.dirs.iter().filter_map(|it| it.canonicalize().ok()).collect();
    let covered = |path: &Path| {
        files.contains(path)
            || in_registry(path)
            || made_by_closure(path)
            || llvm_prefix.as_ref().is_some_and(|it| path.starts_with(it))
    };
    // A path that is not there is hashed once made if it is in a package of the checkout and
    // in no part the hash skips (a dot name, or `target`, `tests`, `benches`, `examples` at the
    // package's root).
    let hashed_once_made = |path: &Path| {
        dirs.iter().any(|dir| {
            path.strip_prefix(dir).is_ok_and(|below| {
                let parts: Vec<&str> = below.iter().filter_map(|it| it.to_str()).collect();
                !parts.iter().any(|it| it.starts_with('.'))
                    && !parts.first().is_some_and(|it| {
                        matches!(*it, "target" | "tests" | "benches" | "examples")
                    })
            })
        })
    };
    let mut read = 0usize;
    let mut crates = 0usize;
    let mut seen = std::collections::BTreeSet::new();
    let mut missing: Vec<String> = Vec::new();
    dep_infos.sort();
    for dep_info in dep_infos {
        let Some(name) = dep_info.file_name().and_then(|it| it.to_str()) else { continue };
        // The test's own build, and a test of a crate, read what blaze's never does (fixtures,
        // `Cargo.lock`); a unit of another build is not what blaze links.
        if !unit_of(&dep_info).is_some_and(|unit| units.contains(&unit)) {
            continue;
        }
        let Some(stem) = name.strip_suffix(".d") else { continue };
        // A build script's is `build_script_build[-<hash>].d`, in `build/<package>/<hash>/out/`
        // or `build/<package>-<hash>/`: it is its package's.
        let krate = if stem.starts_with("build_script_build") {
            let package = dep_info.ancestors().nth(3).zip(dep_info.parent()).map(|(new, old)| {
                if dep_info.parent().is_some_and(|it| it.ends_with("out")) { new } else { old }
            });
            let Some(package) = package.and_then(|it| it.file_name()).and_then(|it| it.to_str())
            else {
                continue;
            };
            let package = if dep_info.parent().is_some_and(|it| it.ends_with("out")) {
                package.to_owned()
            } else {
                package.rsplit_once('-').map_or(package, |(name, _)| name).to_owned()
            };
            package.replace('-', "_")
        } else {
            let Some((krate, _)) = stem.rsplit_once('-') else { continue };
            krate.to_owned()
        };
        let krate = krate.as_str();
        if !worked.crates.contains(krate) {
            continue;
        }
        let text = std::fs::read_to_string(&dep_info)
            .map_err(|why| format!("{}: {why}", dep_info.display()))?;
        let mut reads = Vec::new();
        let mut vars = Vec::new();
        for line in text.lines() {
            if let Some(var) = line.strip_prefix("# env-dep:") {
                vars.push(var.split_once('=').map_or(var, |(name, _)| name).to_owned());
                continue;
            }
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            // `<path>:` lines, one per file read (make's phony targets).
            let Some(path) = line.strip_suffix(':') else { continue };
            if path.contains(": ") {
                continue;
            }
            let path = PathBuf::from(path.replace("\\ ", " "));
            let path = if path.is_absolute() { path } else { workspace.root.join(path) };
            // One left from a build of an older tree is gone.
            if let Ok(path) = path.canonicalize() {
                reads.push(path);
            }
        }
        // Another package whose library has the name (another version, built for a crate
        // outside the identity) reads nothing of the closure's packages.
        if !reads
            .iter()
            .any(|path| in_registry(path) || dirs.iter().any(|dir| path.starts_with(dir)))
        {
            continue;
        }
        crates += 1;
        seen.insert(krate.to_owned());
        for var in vars {
            if !(derived(&var)
                || hashed_env.contains(var.as_str())
                || set_by_scripts.contains(&var))
            {
                missing.push(format!("{krate} reads ${var} ({})", dep_info.display()));
            }
        }
        for path in reads {
            read += 1;
            if !covered(&path) {
                missing.push(format!("{krate} reads {} ({})", path.display(), dep_info.display()));
            }
        }
    }
    // C5 of the review: what the closure's build scripts say they read (`rerun-if-env-changed`,
    // `rerun-if-changed`, in their newest output) is an input too, or a variable named no input
    // (the C compiler's) or derived by cargo.
    let checkout: std::collections::BTreeMap<&str, &Path> =
        workspace.checkout().into_iter().collect();
    let mut newest: std::collections::BTreeMap<String, (std::time::SystemTime, PathBuf)> =
        std::collections::BTreeMap::new();
    for output in &outputs {
        let Some(package) = package_of(output).filter(|it| worked.packages.contains(it)) else {
            continue;
        };
        let Ok(modified) = std::fs::metadata(output).and_then(|it| it.modified()) else { continue };
        if newest.get(&package).is_none_or(|(was, _)| *was < modified) {
            newest.insert(package, (modified, output.clone()));
        }
    }
    let mut directives = 0usize;
    for (package, (_, output)) in &newest {
        let text = std::fs::read_to_string(output).unwrap_or_default();
        for line in text.lines() {
            let Some(directive) =
                line.strip_prefix("cargo::").or_else(|| line.strip_prefix("cargo:"))
            else {
                continue;
            };
            if let Some(var) = directive.strip_prefix("rerun-if-env-changed=") {
                directives += 1;
                let keyed = hashed_env.contains(var)
                    || derived(var)
                    || var.starts_with("CARGO_FEATURE_")
                    || (var == crate::LLVM_PREFIX
                        && worked.inputs.iter().any(|it| it.kind == "llvm"))
                    || crate::unkeyed_env(var).is_some();
                if !keyed {
                    missing.push(format!(
                        "{package}'s build script reads ${var} ({})",
                        output.display()
                    ));
                }
            } else if let Some(path) = directive.strip_prefix("rerun-if-changed=") {
                directives += 1;
                let path = Path::new(path);
                let path = match (path.is_absolute(), checkout.get(package.as_str())) {
                    (true, _) => path.to_path_buf(),
                    (false, Some(dir)) => dir.join(path),
                    // Relative to a registry package: of its files, keyed by its checksum.
                    (false, None) => continue,
                };
                // N5: one that is not there is read once made, so it must be hashed then; a
                // directory, by every file under it.
                let Ok(path) = path.canonicalize() else {
                    if !hashed_once_made(path.as_path()) {
                        missing.push(format!(
                            "{package}'s build script watches {}, which is not there and would not be hashed once made ({})",
                            path.display(),
                            output.display()
                        ));
                    }
                    continue;
                };
                let mut under = Vec::new();
                if path.is_dir() {
                    let mut stack = vec![path.clone()];
                    while let Some(dir) = stack.pop() {
                        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                            let entry = entry.path();
                            if entry.is_dir() { stack.push(entry) } else { under.push(entry) }
                        }
                    }
                } else {
                    under.push(path.clone());
                }
                for file in under {
                    let file = file.canonicalize().unwrap_or(file);
                    if !covered(&file) {
                        missing.push(format!(
                            "{package}'s build script reads {} ({})",
                            file.display(),
                            output.display()
                        ));
                    }
                }
            }
        }
    }
    let mut unchecked: Vec<String> = worked
        .crates
        .iter()
        .filter(|it| {
            !seen.contains(it.as_str())
                && !worked.registry.iter().any(|(name, _)| name.replace('-', "_") == **it)
        })
        .cloned()
        .collect();
    unchecked.sort();
    missing.sort();
    missing.dedup();
    Ok(Held {
        units: units.len(),
        crates,
        read,
        scripts: newest.len(),
        directives,
        missing,
        unchecked,
    })
}

/// The `(name, fingerprint)` of each unit a cargo fingerprint (`<kind>-<name>.json`) lists in
/// `deps`, as `[<package id>, "<name>", <public>, <fingerprint>]`.
fn dep_fingerprints(json: &str) -> Option<Vec<(String, u64)>> {
    let mut rest = &json[json.find("\"deps\":[")? + "\"deps\":[".len()..];
    let mut deps = Vec::new();
    loop {
        rest = rest.trim_start_matches(',');
        if rest.starts_with(']') {
            return Some(deps);
        }
        let entry = rest.strip_prefix('[')?;
        let end = entry.find(']')?;
        let fields: Vec<&str> = entry[..end].split(',').collect();
        let [_, name, _, fingerprint] = fields[..] else { return None };
        deps.push((name.trim_matches('"').to_owned(), fingerprint.trim().parse().ok()?));
        rest = &entry[end + 1..];
    }
}

/// The tuple a test is built for and runs on, as cargo names it to a build script.
pub fn host() -> &'static str {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("x86_64", "windows") => "x86_64-pc-windows-msvc",
        (arch, os) => panic!("the code identity's check knows no tuple of {arch} {os}"),
    }
}
