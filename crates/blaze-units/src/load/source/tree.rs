//! Packages found in a tree of files: a git checkout as cargo 1.99's `GitSource` finds them
//! (`RecursivePathSource::read_packages` in `src/sources/path.rs`), a `directory` source as its
//! `DirectorySource` does (`src/sources/directory.rs`). Copied from v1's
//! `crates/blaze-registry/src/{git,directory}.rs`, every read through the view's mounts.

use std::collections::BTreeMap;

use anyhow::anyhow;
use blaze_hash::HashSet;
use blaze_query::{Cancelled, Cx};
use blaze_resolve::PackageId;
use semver::Version;
use serde::Deserialize;

use crate::load::files::{Found, normal_text as normalize, read_dir, read_file};

/// A package of a checkout: its name, its version, and its directory.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Package {
    name: String,
    version: Version,
    dir: String,
}

/// What a walk read: the packages in the order cargo finds them, and the last manifest that
/// did not read, which is cargo's error when there are no packages.
#[derive(Default)]
struct Walk {
    packages: Vec<Package>,
    visited: HashSet<String>,
    errors: Vec<anyhow::Error>,
}

/// The `Cargo.toml` of the package `id` in the checkout at `root`: the first package of its
/// name and version cargo's walk finds. cargo keeps the first of two at one id too, warning of
/// the others (`first_package`).
pub(crate) fn checkout_package(
    cx: &mut Cx<'_>,
    root: &str,
    id: PackageId,
) -> Result<anyhow::Result<String>, Cancelled> {
    let mut walk = Walk::default();
    if let Err(error) = walk_dir(cx, root, root, &mut walk)? {
        return Ok(Err(error));
    }
    if walk.packages.is_empty() {
        return Ok(Err(match walk.errors.pop() {
            Some(error) => error,
            None => match read_file(cx, &format!("{root}/cargo.toml"))? {
                Found::Here(_) => anyhow!(
                    "Could not find Cargo.toml in `{root}`, but found cargo.toml please try to \
                     rename it to Cargo.toml"
                ),
                _ => anyhow!("Could not find Cargo.toml in `{root}`"),
            },
        }));
    }
    let found = walk
        .packages
        .iter()
        .find(|it| it.name == id.name().as_str() && &it.version == id.version());
    Ok(match found {
        Some(package) => Ok(format!("{}/Cargo.toml", package.dir)),
        None => Err(anyhow!("failed to find {id} in path source")),
    })
}

fn is_here<T>(found: &Found<T>) -> bool {
    matches!(found, Found::Here(_))
}

/// cargo's `walk` with `read_packages`'s callback: hidden directories and submodules but at the
/// root, and `target` beside a manifest, are not looked in.
fn walk_dir(
    cx: &mut Cx<'_>,
    root: &str,
    dir: &str,
    walk: &mut Walk,
) -> Result<anyhow::Result<()>, Cancelled> {
    if dir != root {
        if base_name(dir).starts_with('.') {
            return Ok(Ok(()));
        }
        let git = format!("{dir}/.git");
        if is_here(&read_file(cx, &git)?) || is_here(&read_dir(cx, &git)?) {
            return Ok(Ok(()));
        }
    }
    if base_name(dir) == "target"
        && let Some((parent, _)) = dir.rsplit_once('/')
        && is_here(&read_file(cx, &format!("{parent}/Cargo.toml"))?)
    {
        return Ok(Ok(()));
    }
    if is_here(&read_file(cx, &format!("{dir}/Cargo.toml"))?) {
        read_nested(cx, root, dir, walk)?;
    }
    let names = match read_dir(cx, dir)? {
        Found::Here(names) => names,
        Found::Absent => return Ok(Ok(())),
        Found::Unmounted => return Ok(Err(anyhow!("`{dir}` is under no mount of the view"))),
    };
    for (name, is_dir) in names {
        if is_dir && let Err(error) = walk_dir(cx, root, &format!("{dir}/{name}"), walk)? {
            return Ok(Err(error));
        }
    }
    Ok(Ok(()))
}

fn base_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// cargo's `read_nested_packages`: the package of `dir` and of each `path` dependency it names,
/// once each. A manifest that does not read is skipped, as cargo skips one in a git source.
fn read_nested(
    cx: &mut Cx<'_>,
    root: &str,
    dir: &str,
    walk: &mut Walk,
) -> Result<(), Cancelled> {
    if !walk.visited.insert(dir.to_owned()) {
        return Ok(());
    }
    let manifest_path = format!("{dir}/Cargo.toml");
    let manifest = match read_table(cx, &manifest_path)? {
        Ok(Some(manifest)) => manifest,
        Ok(None) => {
            walk.errors.push(anyhow!("failed to read `{manifest_path}`"));
            return Ok(());
        }
        Err(error) => {
            walk.errors.push(error);
            return Ok(());
        }
    };
    let Some(package) = manifest.get("package").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    let Some(name) = package.get("name").and_then(toml::Value::as_str) else {
        walk.errors.push(anyhow!("failed to parse manifest at `{manifest_path}`"));
        return Ok(());
    };
    let workspace = match workspace_root(cx, root, dir, package)? {
        Ok(workspace) => workspace,
        Err(error) => {
            walk.errors.push(error);
            return Ok(());
        }
    };
    let version = match package.get("version") {
        None => Some(Version::new(0, 0, 0)),
        Some(toml::Value::String(it)) => Version::parse(it).ok(),
        Some(toml::Value::Table(it)) if it.get("workspace").and_then(toml::Value::as_bool) == Some(true) => {
            workspace.as_ref().and_then(|(_, table)| {
                let version = table.get("workspace")?.get("package")?.get("version")?.as_str()?;
                Version::parse(version).ok()
            })
        }
        Some(_) => None,
    };
    let Some(version) = version else {
        walk.errors.push(anyhow!("failed to parse manifest at `{manifest_path}`"));
        return Ok(());
    };
    walk.packages.push(Package { name: name.to_owned(), version, dir: dir.to_owned() });
    for path in nested_paths(&manifest, dir, workspace.as_ref()) {
        read_nested(cx, root, &path, walk)?;
    }
    Ok(())
}

/// A TOML file: `None` when there is none.
fn read_table(
    cx: &mut Cx<'_>,
    path: &str,
) -> Result<anyhow::Result<Option<toml::Table>>, Cancelled> {
    Ok(match read_file(cx, path)? {
        Found::Here(bytes) => match String::from_utf8(bytes) {
            Ok(text) => match toml::from_str::<toml::Table>(&text) {
                Ok(table) => Ok(Some(table)),
                Err(error) => {
                    Err(anyhow::Error::new(error).context(format!("failed to parse manifest at `{path}`")))
                }
            },
            Err(_) => Err(anyhow!("failed to read `{path}`: the file is not UTF-8")),
        },
        Found::Absent => Ok(None),
        Found::Unmounted => Err(anyhow!("`{path}` is under no mount of the view")),
    })
}

/// The workspace the package in `dir` is a member of, its directory and manifest: the one
/// `package.workspace` names, or the nearest manifest with a `[workspace]` above it within
/// `root` (cargo's `find_workspace_root`, which a checkout's root bounds).
fn workspace_root(
    cx: &mut Cx<'_>,
    root: &str,
    dir: &str,
    package: &toml::Table,
) -> Result<anyhow::Result<Option<(String, toml::Table)>>, Cancelled> {
    if let Some(explicit) = package.get("workspace").and_then(toml::Value::as_str) {
        let at = normalize(&format!("{dir}/{explicit}"));
        return Ok(match read_table(cx, &format!("{at}/Cargo.toml"))? {
            Ok(Some(table)) => Ok(Some((at, table))),
            Ok(None) => Err(anyhow!("failed to read `{at}/Cargo.toml`")),
            Err(error) => Err(error),
        });
    }
    let mut at = dir.to_owned();
    loop {
        match read_table(cx, &format!("{at}/Cargo.toml"))? {
            Ok(Some(table)) if table.contains_key("workspace") => return Ok(Ok(Some((at, table)))),
            Ok(_) => {}
            Err(error) if at != dir => return Ok(Err(error)),
            Err(_) => {}
        }
        if at == root {
            return Ok(Ok(None));
        }
        match at.rsplit_once('/') {
            Some((above, _)) if above.len() >= root.len() => at = above.to_owned(),
            _ => return Ok(Ok(None)),
        }
    }
}

/// The directories of the `path` dependencies of every dependency table and every
/// `[target.<cfg>]` one, an inherited one's from its workspace's `[workspace.dependencies]`
/// (cargo's `nested_paths` of the normalized manifest).
fn nested_paths(
    manifest: &toml::Table,
    dir: &str,
    workspace: Option<&(String, toml::Table)>,
) -> Vec<String> {
    const TABLES: [&str; 5] = [
        "dependencies",
        "build-dependencies",
        "build_dependencies",
        "dev-dependencies",
        "dev_dependencies",
    ];
    let mut tables: Vec<&toml::Table> =
        TABLES.iter().filter_map(|it| manifest.get(*it).and_then(toml::Value::as_table)).collect();
    let targets = manifest.get("target").and_then(toml::Value::as_table);
    for target in targets.into_iter().flat_map(|it| it.values()) {
        tables.extend(TABLES.iter().filter_map(|it| target.get(*it).and_then(toml::Value::as_table)));
    }
    let inherited = workspace.and_then(|(at, table)| {
        let deps = table.get("workspace")?.get("dependencies")?.as_table()?;
        Some((at, deps))
    });
    let mut out = Vec::new();
    for (name, dep) in tables.into_iter().flat_map(|it| it.iter()) {
        let Some(dep) = dep.as_table() else { continue };
        if dep.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
            let path = inherited.and_then(|(at, deps)| {
                let path = deps.get(name)?.get("path")?.as_str()?;
                Some(normalize(&format!("{at}/{path}")))
            });
            out.extend(path);
        } else if let Some(path) = dep.get("path").and_then(toml::Value::as_str) {
            out.push(normalize(&format!("{dir}/{path}")));
        }
    }
    out
}

#[derive(Deserialize)]
struct Checksums {
    package: Option<String>,
    // BTreeMap: the check reports the first changed file in path order, the same on every run.
    files: BTreeMap<String, String>,
}

/// A package of a directory source.
struct Vendored {
    name: String,
    version: Version,
    dir: String,
    checksums: Checksums,
}

/// The `Cargo.toml` of the package `id` in the directory source at `root`, once its files are
/// what its `.cargo-checksum.json` lists and the `.crate` it came from is the one the lock file
/// names (`checksum`). `source` is how cargo names the directory source in its messages.
pub(crate) fn vendored_package(
    cx: &mut Cx<'_>,
    root: &str,
    source: &str,
    id: PackageId,
    checksum: Option<&str>,
) -> Result<anyhow::Result<String>, Cancelled> {
    let names = match read_dir(cx, root)? {
        Found::Here(names) => names,
        Found::Absent => {
            return Ok(Err(anyhow!("No such file or directory (os error 2)")
                .context(format!("failed to read root of directory source: {root}"))));
        }
        Found::Unmounted => {
            return Ok(Err(anyhow!(
                "blaze reads the directory source `{root}` in place, and it is under no mount of \
                 the view"
            )));
        }
    };
    let mut packages = Vec::new();
    for (name, _) in names {
        if name.starts_with('.') {
            continue;
        }
        let dir = format!("{root}/{name}");
        match read_vendored(cx, &dir)? {
            Ok(Some(package)) => packages.push(package),
            Ok(None) => {}
            Err(error) => return Ok(Err(error)),
        }
    }
    let Some(found) = packages
        .iter()
        .find(|it| it.name == id.name().as_str() && &it.version == id.version())
    else {
        return Ok(Err(anyhow!(
            "failed to find package with id: {} v{} ({source})",
            id.name(),
            id.version()
        )));
    };
    let package = format!("{} v{}", id.name(), id.version());
    let locked = checksum.filter(|it| !it.is_empty());
    match (locked, found.checksums.package.as_deref()) {
        (Some(locked), Some(found)) if locked != found => {
            return Ok(Err(anyhow!(
                "checksum for `{package}` changed between lock files\n\nthis could be indicative \
                 of a few possible errors:\n\n    * the lock file is corrupt\n    * a replacement \
                 source in use (e.g., a mirror) returned a different checksum\n    * the source \
                 itself may be corrupt in one way or another\n\nunable to verify that `{package}` \
                 is the same as when the lockfile was generated"
            )));
        }
        (Some(_), None) => {
            return Ok(Err(anyhow!(
                "checksum for `{package}` could not be calculated, but a checksum is listed in \
                 the existing lock file\n\nthis could be indicative of a few possible \
                 situations:\n\n    * the source `{source}` supports checksums,\n      but was \
                 replaced with one that doesn't\n    * the lock file is corrupt\n\nunable to \
                 verify that `{package}` is the same as when the lockfile was generated"
            )));
        }
        _ => {}
    }
    if let Err(error) = verify(cx, found)? {
        return Ok(Err(error));
    }
    Ok(Ok(format!("{}/Cargo.toml", found.dir)))
}

/// A subdirectory of a directory source: a package when it has a `Cargo.toml`.
fn read_vendored(cx: &mut Cx<'_>, dir: &str) -> Result<anyhow::Result<Option<Vendored>>, Cancelled> {
    let manifest_path = format!("{dir}/Cargo.toml");
    let manifest = match read_table(cx, &manifest_path)? {
        Ok(Some(manifest)) => manifest,
        Ok(None) => return Ok(Ok(None)),
        Err(error) => return Ok(Err(error)),
    };
    let package = manifest.get("package").and_then(toml::Value::as_table);
    let Some(name) = package.and_then(|it| it.get("name")).and_then(toml::Value::as_str) else {
        return Ok(Err(anyhow!(
            "failed to parse manifest at `{manifest_path}`: a vendored package has a [package] \
             with a name"
        )));
    };
    let version = match package.and_then(|it| it.get("version")) {
        None => Version::new(0, 0, 0),
        Some(toml::Value::String(it)) => match Version::parse(it) {
            Ok(version) => version,
            Err(error) => {
                return Ok(Err(anyhow::Error::new(error)
                    .context(format!("failed to parse manifest at `{manifest_path}`"))));
            }
        },
        Some(_) => {
            return Ok(Err(anyhow!(
                "failed to parse manifest at `{manifest_path}`: `package.version` of a vendored \
                 package is a version"
            )));
        }
    };
    let name = name.to_owned();
    let checksum_path = format!("{dir}/.cargo-checksum.json");
    let text = match read_file(cx, &checksum_path)? {
        Found::Here(bytes) => bytes,
        _ => {
            return Ok(Err(anyhow!("failed to read `{checksum_path}`").context(format!(
                "failed to load checksum `.cargo-checksum.json` of {name} v{version}"
            ))));
        }
    };
    let checksums: Checksums = match serde_json::from_slice(&text) {
        Ok(it) => it,
        Err(error) => {
            return Ok(Err(anyhow::Error::new(error).context(format!(
                "failed to decode `.cargo-checksum.json` of {name} v{version}"
            ))));
        }
    };
    Ok(Ok(Some(Vendored { name, version, dir: dir.to_owned(), checksums })))
}

/// cargo's `DirectorySource::verify`: every file listed has the sha256 listed.
fn verify(cx: &mut Cx<'_>, package: &Vendored) -> Result<anyhow::Result<()>, Cancelled> {
    for (file, expected) in &package.checksums.files {
        let path = format!("{}/{file}", package.dir);
        let Found::Here(bytes) = read_file(cx, &path)? else {
            return Ok(Err(anyhow!("No such file or directory (os error 2)")
                .context(format!("failed to calculate checksum of: {path}"))));
        };
        let actual: String = blaze_sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect();
        if &actual != expected {
            return Ok(Err(anyhow!(
                "the listed checksum of `{path}` has changed:\nexpected: {expected}\nactual:   \
                 {actual}\n\ndirectory sources are not intended to be edited, if modifications \
                 are required then it is recommended that `[patch]` is used with a forked copy \
                 of the source"
            )));
        }
    }
    Ok(Ok(()))
}
