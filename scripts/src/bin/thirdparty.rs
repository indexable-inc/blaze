//! deps: the third-party packages each workspace crate depends on only go down, to none
//! (docs/v2/plan/design/bottom.md, "Also decided on 2026-11-15"). `DOC` below is what its usage
//! error prints; it says what each count is, what a workspace crate is, how a dependency is
//! resolved, what the TOML reader refuses, and the rule.
//!
//!     blaze run -q -p scripts --bin thirdparty-- '{"command": "check", "base": "origin/main"}'
//!     blaze run -q -p scripts --bin thirdparty-- '{"command": "write"}'
//!     blaze run -q -p scripts --bin thirdparty-- selftest
//!
//! Why (2026-11-15): Andrew decided the compiler has no third-party crates ("we should have 0
//! third party"). Measured on main that day: 413 third-party packages in the lock graph, 102 named
//! directly by workspace crates. This holds the count per crate and lets it only fall, as
//! `allocs` and `untyped` hold theirs: a gate step of the `scripts` job
//! (.github/workflows/blaze.yaml). docs/v2/plan/third-party.md says what replaces each package.
//!
//! It adds nothing to the graph it counts: it starts no cargo and reads the manifests and
//! Cargo.lock with blaze-toml (through blaze-script's `toml`), the workspace's own reader.

/// What the usage error prints.
const DOC: &str = r#"deps: the third-party packages each workspace crate depends on only go down, to none (docs/v2/plan/design/bottom.md, "Also decided on 2026-11-15").

    blaze run -q -p scripts --bin thirdparty-- '{"command": "check", "base": "<rev>"}'
    blaze run -q -p scripts --bin thirdparty-- '{"command": "write"}'
    blaze run -q -p scripts --bin thirdparty-- selftest

    command   check   each count of every workspace crate, and of `(all)`, equals its row's in
                      the list: one over its row is refused, naming each package that is new
                      since `base` and the manifest line it came in by (without `base`, a
                      `direct` or `dev` count over its row names all its packages); one under
                      it must be lowered with `write`; a crate with no row is held to 0; with
                      `base`, no count of a row is above the base's (CI)
              write   write the list from the tree as it is
    root      the checkout (".")
    base      the commit the change is against, or null: its list (`git show`) holds every
              row down, and its manifests and Cargo.lock, read the same way, tell which
              packages are new

The list's row is `<crate><TAB><direct><TAB><dev><TAB><closure>`, one per workspace crate with a
count above 0, by its package name, sorted, then `(all)`, the workspace's, each package once:
  direct   the third-party packages named in the crate's `[dependencies]` and
           `[build-dependencies]`, and in their `[target.<cfg>.*]` forms (every target's)
  dev      the third-party packages named in its `[dev-dependencies]` and not in `direct`; for
           `(all)`, the ones no crate names in `direct`: what only tests and benches need
  closure  the third-party packages its normal and build dependencies reach: through the
           workspace's crates by their manifests (their normal and build dependencies again,
           never dev), and through each third-party package by its entry in Cargo.lock
A package is a name and a version: two versions of one name count twice.

A workspace crate is a package whose manifest is in the repository and not under vendor/: a
member (`[workspace] members`, each a path or `<dir>/*`, every directory of <dir> holding a
Cargo.toml, less `exclude`), or a package that a `path` dependency or a `[patch]` reaches from
one. Every other package is third-party: one from a registry or git (it has a `source` in
Cargo.lock), and a fork under vendor/, which is upstream's code with our patches. A fork whose
manifest says `[package.metadata.blaze] ours = true` is a workspace crate; none says so on
2026-11-15.

A dependency resolves as cargo resolved it: to its package (`package = ".."`, else its key),
through the root's `[workspace.dependencies]` for `workspace = true`, to the manifest at its
`path`, and otherwise to the entry of that name among the crate's own dependencies in Cargo.lock
(where the crate has two versions of it, by its requirement: `=x.y.z`, `^x.y.z` or `x.y.z`).
An optional dependency Cargo.lock does not list (no feature anywhere turns it on) is in no
build and not counted; one it lists counts, whichever crate's features turn it on. A crate that
is not a member has no dev-dependencies read: cargo resolves none for it.

The manifests and Cargo.lock are read as TOML 1.0 by blaze-toml, which refuses what is not TOML
naming the file, line and column. A key whose value is not of the shape it must be is refused
naming the file and line. A Cargo.lock that does not list a dependency is refused, naming the
crate, the dependency and its manifest line: update the lock.
"#;

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
};

use blaze_script::{
    fail, git,
    input::{self, Fields, Input, InputError},
    read_text,
    toml,
    universal_newlines,
};

/// The manifests' shape: every dependency through the root's `[workspace.dependencies]`.
#[path = "thirdparty/manifests.rs"]
mod manifests;

/// The packages cargo's resolve reaches, held at D180's ratchet, target 0.
#[path = "thirdparty/reach.rs"]
mod reach;

const SCRIPT: &str = "thirdparty";
/// The list, from the root.
const LIST: &str = "tools/ratchets/third-party.txt";
/// The list's row of the whole workspace: no package's name, which has no parentheses.
const TOTAL: &str = "(all)";
/// The directory of forks of upstream crates: third-party unless a manifest says [`OURS`].
const VENDOR: &str = "vendor";
/// The key of `[package.metadata.blaze]` that makes a fork under vendor/ a workspace crate.
const OURS: &str = "ours";
/// The root manifest and the lock, from the root.
const ROOT: &str = "Cargo.toml";
const LOCK: &str = "Cargo.lock";
/// What every message about a count over its row ends with.
const RULE: &str = "docs/v2/plan/design/bottom.md, \"Also decided on 2026-11-15\": no third-party crates in the compiler; \
                    every crate depends only on crates of this workspace, and a new crate starts at zero";
/// What to do instead, for a message.
const INSTEAD: &str = "use the workspace's own crate for what the package does (docs/v2/plan/third-party.md says what \
                       replaces each), or write it where it is needed; a workspace crate brings its own closure with it, \
                       so take the need from one at zero";
/// The list's first lines.
const HEADER: &str = "# The third-party packages each workspace crate depends on, by its package name\n\
                      # (docs/v2/plan/design/bottom.md, \"Also decided on 2026-11-15\": none, in the end). `direct`:\n\
                      # named in its normal and build dependencies; `dev`: in its dev-dependencies only; `closure`:\n\
                      # reached by its normal and build dependencies, through the workspace's crates and Cargo.lock.\n\
                      # A package is a name and a version. `(all)` is the workspace's, each package once. Only\n\
                      # shrinks; a crate with no row is held to 0. Written by scripts/src/bin/thirdparty.rs\n\
                      # (`{\"command\": \"write\"}`), checked in CI; docs/v2/plan/third-party.md says what replaces each.\n\
                      # crate\tdirect\tdev\tclosure\n";

struct Args {
    command: String,
    root: String,
    base: Option<String>,
    /// For `reach-*`: the path of `cargo metadata --format-version 1 --offline --locked`'s output.
    metadata: Option<String>,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["command", "root", "base", "metadata"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args {
            command: f.one_of(
                "command",
                &["check", "write", "manifests-check", "manifests-report", "reach-check", "reach-write", "reach-report"],
                None,
            )?,
            root: f.str("root", Some("."))?,
            base: f.str_or_null("base")?,
            metadata: f.str_or_null("metadata")?,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// TOML: blaze-toml, through blaze-script.

/// The TOML text `text` of the file `file` (named in errors), as a document.
fn read_toml<'t>(file: &str, text: &'t str) -> Result<toml::Doc<'t>, DepsError> {
    toml::read(text).map_err(|e| {
        let (line, col) = e.line_col(text);
        DepsError::Toml { at: Place { file: file.to_owned(), line }, col, what: e.what }
    })
}

/// The line (from 1) a key, table or value of a document starts at.
fn line_of(item: toml::Node<'_, '_>) -> usize {
    item.line_col().0
}

// ---------------------------------------------------------------------------------------------
// The workspace: its manifests, read from a tree.

/// A file and a line (from 1) in it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Place {
    file: String,
    line: usize,
}

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.file, self.line)
    }
}

/// The graph could not be read: what and where.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DepsError {
    /// A manifest or the lock is not TOML: the file, line and column, and what is wrong there.
    Toml { at: Place, col: usize, what: toml::What },
    /// A file the graph needs is not there: which, and what it is.
    Missing { path: String, what: String },
    /// A file could not be read: which, and the system's reason.
    Unreadable { path: String, why: String },
    /// A key whose value is not of the shape it must be.
    Shape { at: Place, key: String, want: &'static str },
    /// A `members` pattern that is not a path or `<dir>/*`.
    Glob { at: Place, pattern: String },
    /// A path that leaves the repository.
    Outside { at: Place, path: String },
    /// `workspace = true` with no entry of its key in the root's `[workspace.dependencies]`.
    NoWorkspaceDep { at: Place, key: String },
    /// Two manifests of one package name.
    TwoPackages { name: String, first: String, second: String },
    /// A workspace crate with no entry in Cargo.lock.
    CrateNotLocked { krate: String, manifest: String },
    /// A dependency the crate's entry in Cargo.lock does not list.
    NotInLock { krate: String, package: String, at: Place },
    /// A dependency that two or more entries of the crate's in Cargo.lock could be.
    Ambiguous { krate: String, package: String, at: Place, versions: Vec<String> },
    /// A dependency of an entry of Cargo.lock that names not exactly one of its packages.
    LockDep { package: String, text: String, found: usize },
    /// A package of Cargo.lock with no source that is no manifest reached from the workspace.
    UnknownPath { name: String },
    /// The base's tree could not be read.
    Base(BaseError),
}

impl fmt::Display for DepsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DepsError::Toml { at, col, what } => write!(f, "{at}:{col}: {what}"),
            DepsError::Missing { path, what } => write!(f, "{path}, {what}, is not there"),
            DepsError::Unreadable { path, why } => write!(f, "{path} could not be read: {why}"),
            DepsError::Shape { at, key, want } => write!(f, "{at}: `{key}` is not {want}"),
            DepsError::Glob { at, pattern } => {
                write!(f, "{at}: the member pattern {pattern:?} is neither a path nor `<dir>/*`, the two forms scripts/src/bin/thirdparty.rs expands")
            }
            DepsError::Outside { at, path } => write!(f, "{at}: the path {path:?} leaves the repository"),
            DepsError::NoWorkspaceDep { at, key } => {
                write!(f, "{at}: `{key}` says `workspace = true`, and the root's `[workspace.dependencies]` has no `{key}`")
            }
            DepsError::TwoPackages { name, first, second } => write!(f, "{first} and {second} are both the package `{name}`"),
            DepsError::CrateNotLocked { krate, manifest } => {
                write!(f, "{LOCK} has no entry for the workspace crate `{krate}` ({manifest}): update the lock")
            }
            DepsError::NotInLock { krate, package, at } => write!(
                f,
                "{at}: `{krate}` depends on `{package}`, which its entry in {LOCK} does not list: update the lock (`package = ..` names the package a key renames)"
            ),
            DepsError::Ambiguous { krate, package, at, versions } => write!(
                f,
                "{at}: `{krate}`'s dependency `{package}` could be any of its versions {} in {LOCK}: write its requirement as `=x.y.z` or `x.y.z`",
                versions.join(", ")
            ),
            DepsError::LockDep { package, text, found } => {
                write!(f, "{LOCK}: `{package}`'s dependency {text:?} names {found} of its packages, not one")
            }
            DepsError::UnknownPath { name } => write!(
                f,
                "{LOCK}: the package `{name}` has no source, so its manifest is in the repository, and no member, path dependency or patch reaches it"
            ),
            DepsError::Base(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DepsError {}

/// Where the files are read from: the checkout, the base commit, or a selftest's strings.
trait Tree {
    /// The text of the file `path` (from the root, `/`-separated), or `None` when there is none.
    fn read(&self, path: &str) -> Result<Option<String>, DepsError>;
    /// The directories directly under `dir` that hold a Cargo.toml, as paths from the root,
    /// sorted.
    fn manifest_dirs(&self, dir: &str) -> Result<Vec<String>, DepsError>;
}

/// The checkout on disk.
struct Disk {
    root: PathBuf,
}

impl Tree for Disk {
    fn read(&self, path: &str) -> Result<Option<String>, DepsError> {
        match std::fs::read(self.root.join(path)) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(|text| Some(universal_newlines(text)))
                .map_err(|_| DepsError::Unreadable { path: path.to_owned(), why: "it is not UTF-8".to_owned() }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(DepsError::Unreadable { path: path.to_owned(), why: e.to_string() }),
        }
    }

    fn manifest_dirs(&self, dir: &str) -> Result<Vec<String>, DepsError> {
        let unreadable = |e: std::io::Error| DepsError::Unreadable { path: dir.to_owned(), why: e.to_string() };
        let mut out = Vec::new();
        for entry in std::fs::read_dir(self.root.join(dir)).map_err(unreadable)? {
            let entry = entry.map_err(unreadable)?;
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir && entry.path().join("Cargo.toml").is_file() {
                out.push(format!("{dir}/{}", entry.file_name().to_string_lossy()));
            }
        }
        out.sort();
        Ok(out)
    }
}

/// The tree of a commit, read with git.
struct AtBase {
    root: PathBuf,
    base: String,
    /// Every file of its tree, from the root.
    files: BTreeSet<String>,
}

impl AtBase {
    fn new(root: &Path, base: &str) -> Result<AtBase, BaseError> {
        let dir = root.to_string_lossy().into_owned();
        let listed = git(root, &["-C", dir.as_str(), "ls-tree", "-r", "--name-only", base])
            .map_err(|why| BaseError::Git { base: base.to_owned(), what: "list its tree", why })?;
        Ok(AtBase { root: root.to_path_buf(), base: base.to_owned(), files: listed.lines().map(str::to_owned).collect() })
    }
}

impl Tree for AtBase {
    fn read(&self, path: &str) -> Result<Option<String>, DepsError> {
        if !self.files.contains(path) {
            return Ok(None);
        }
        let dir = self.root.to_string_lossy().into_owned();
        let spec = format!("{}:{path}", self.base);
        let text = git(&self.root, &["-C", dir.as_str(), "show", spec.as_str()])
            .map_err(|why| DepsError::Base(BaseError::Git { base: self.base.clone(), what: "show a manifest or the lock", why }))?;
        Ok(Some(universal_newlines(text)))
    }

    fn manifest_dirs(&self, dir: &str) -> Result<Vec<String>, DepsError> {
        Ok(dirs_with_manifest(self.files.iter(), dir))
    }
}

/// A selftest's files, by path.
struct Fixture {
    files: BTreeMap<String, String>,
}

impl Tree for Fixture {
    fn read(&self, path: &str) -> Result<Option<String>, DepsError> {
        Ok(self.files.get(path).cloned())
    }

    fn manifest_dirs(&self, dir: &str) -> Result<Vec<String>, DepsError> {
        Ok(dirs_with_manifest(self.files.keys(), dir))
    }
}

/// Of the file paths `files`, the directories directly under `dir` with a Cargo.toml, sorted.
fn dirs_with_manifest<'f>(files: impl Iterator<Item = &'f String>, dir: &str) -> Vec<String> {
    let prefix = format!("{dir}/");
    let mut out: Vec<String> = files
        .filter_map(|path| {
            let name = path.strip_prefix(prefix.as_str())?.strip_suffix("/Cargo.toml")?;
            (!name.is_empty() && !name.contains('/')).then(|| format!("{dir}/{name}"))
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// `rel` from the directory `dir` (both from the root; `""` is the root), lexically; `None` when
/// it is absolute or leaves the root.
fn join(dir: &str, rel: &str) -> Option<String> {
    if rel.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|it| !it.is_empty()).collect();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

/// The manifest of the directory `dir`, from the root.
fn manifest_path(dir: &str) -> String {
    if dir.is_empty() { ROOT.to_owned() } else { format!("{dir}/Cargo.toml") }
}

/// The key `key` of `table` is not `want`: refused at its line.
fn shape(item: toml::Node<'_, '_>, key: &str, file: &str, want: &'static str) -> DepsError {
    DepsError::Shape { at: Place { file: file.to_owned(), line: line_of(item) }, key: key.to_owned(), want }
}

fn get_str(table: toml::Node<'_, '_>, key: &str, file: &str) -> Result<Option<String>, DepsError> {
    let Some(item) = table.get(key) else { return Ok(None) };
    item.as_str().map(|it| Some(it.to_owned())).ok_or_else(|| shape(item, key, file, "a string"))
}

fn get_bool(table: toml::Node<'_, '_>, key: &str, file: &str) -> Result<Option<bool>, DepsError> {
    let Some(item) = table.get(key) else { return Ok(None) };
    item.as_bool().map(Some).ok_or_else(|| shape(item, key, file, "a boolean"))
}

fn get_table<'d, 't>(table: toml::Node<'d, 't>, key: &str, file: &str) -> Result<Option<toml::Node<'d, 't>>, DepsError> {
    match table.get(key) {
        None => Ok(None),
        Some(item) if item.is_table() => Ok(Some(item)),
        Some(item) => Err(shape(item, key, file, "a table")),
    }
}

/// An array of strings, with the line of its key.
fn get_strs(table: toml::Node<'_, '_>, key: &str, file: &str) -> Result<Option<(usize, Vec<String>)>, DepsError> {
    let Some(item) = table.get(key) else { return Ok(None) };
    let refused = || shape(item, key, file, "an array of strings");
    if !item.is_array() {
        return Err(refused());
    }
    let mut out = Vec::new();
    for it in item.children() {
        out.push(it.as_str().ok_or_else(refused)?.to_owned());
    }
    Ok(Some((line_of(item), out)))
}

/// Which table names a dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DepKind {
    Normal,
    Build,
    Dev,
}

/// The dependency tables of a manifest (and of each `[target.<cfg>]`), with the underscore
/// forms cargo still reads.
const DEP_TABLES: [(&str, DepKind); 5] = [
    ("dependencies", DepKind::Normal),
    ("build-dependencies", DepKind::Build),
    ("build_dependencies", DepKind::Build),
    ("dev-dependencies", DepKind::Dev),
    ("dev_dependencies", DepKind::Dev),
];

/// One entry of a dependency table, as far as the count needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Dep {
    /// Its key: the name the code uses.
    key: String,
    /// The package it is: `package = ".."`, else the key.
    package: String,
    /// The directory of its manifest, from the root, for a `path` dependency.
    path: Option<String>,
    /// Its version requirement, which tells two versions of a package in the lock apart.
    version: Option<String>,
    optional: bool,
    kind: DepKind,
    /// The manifest line that names it.
    at: Place,
}

/// The dependency `key = value` at `at` of a manifest in `dir`; `root_deps` the root's
/// `[workspace.dependencies]`, which `workspace = true` reads (its paths from the root).
fn dep_of(key: &str, value: toml::Node<'_, '_>, kind: DepKind, at: &Place, dir: &str, root_deps: Option<toml::Node<'_, '_>>) -> Result<Dep, DepsError> {
    let mut dep = Dep { key: key.to_owned(), package: key.to_owned(), path: None, version: None, optional: false, kind, at: at.clone() };
    let shape = |want: &'static str| DepsError::Shape { at: at.clone(), key: key.to_owned(), want };
    let file = at.file.as_str();
    match value.as_str() {
        Some(version) => dep.version = Some(version.to_owned()),
        None if value.is_table() => {
            let table = value;
            dep.optional = get_bool(table, "optional", file)?.unwrap_or(false);
            if get_bool(table, "workspace", file)? == Some(true) {
                if ["package", "path", "version", "git"].iter().any(|it| table.get(it).is_some()) {
                    return Err(shape("`workspace = true` with no `package`, `path`, `version` or `git` of its own"));
                }
                let Some(inherited) = root_deps.and_then(|it| it.get(key)) else {
                    return Err(DepsError::NoWorkspaceDep { at: at.clone(), key: key.to_owned() });
                };
                let from_root = dep_of(key, inherited, kind, at, "", None)?;
                dep.package = from_root.package;
                dep.path = from_root.path;
                dep.version = from_root.version;
            } else {
                if let Some(package) = get_str(table, "package", file)? {
                    dep.package = package;
                }
                dep.version = get_str(table, "version", file)?;
                if let Some(path) = get_str(table, "path", file)? {
                    let Some(joined) = join(dir, &path) else { return Err(DepsError::Outside { at: at.clone(), path }) };
                    dep.path = Some(joined);
                }
            }
        }
        None => return Err(shape("a version string or a table")),
    }
    Ok(dep)
}

/// Every dependency the manifest `table` (the file `file`, in `dir`) names, in line order.
fn deps_of(file: &str, dir: &str, table: toml::Node<'_, '_>, root_deps: Option<toml::Node<'_, '_>>) -> Result<Vec<Dep>, DepsError> {
    let mut tables: Vec<(toml::Node<'_, '_>, DepKind)> = Vec::new();
    for (name, kind) in DEP_TABLES {
        if let Some(it) = get_table(table, name, file)? {
            tables.push((it, kind));
        }
    }
    if let Some(targets) = get_table(table, "target", file)? {
        for target in targets.children() {
            if !target.is_table() {
                return Err(shape(target, &format!("target.{}", target.key()), file, "a table"));
            }
            for (name, kind) in DEP_TABLES {
                if let Some(it) = get_table(target, name, file)? {
                    tables.push((it, kind));
                }
            }
        }
    }
    let mut out = Vec::new();
    for (deps, kind) in tables {
        for value in deps.children() {
            out.push(dep_of(value.key(), value, kind, &Place { file: file.to_owned(), line: line_of(value) }, dir, root_deps)?);
        }
    }
    out.sort_by(|a, b| a.at.cmp(&b.at));
    Ok(out)
}

/// A package with a manifest in the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Manifest {
    /// Its directory and manifest, from the root.
    dir: String,
    file: String,
    name: String,
    /// A workspace crate: not under vendor/, or a fork there that says it is ours.
    ours: bool,
    /// Its dependencies in line order; a non-member's without its dev-dependencies.
    deps: Vec<Dep>,
}

/// Whether the `[package]` table `package` says `[package.metadata.blaze] ours = true`.
fn fork_is_ours(package: toml::Node<'_, '_>, file: &str) -> Result<bool, DepsError> {
    let Some(metadata) = get_table(package, "metadata", file)? else { return Ok(false) };
    let Some(blaze) = get_table(metadata, "blaze", file)? else { return Ok(false) };
    Ok(get_bool(blaze, OURS, file)? == Some(true))
}

/// Every package with a manifest in the repository that the workspace reaches, by name: the
/// members, and what their `path` dependencies and the root's `[patch]`es reach.
fn workspace(tree: &dyn Tree) -> Result<BTreeMap<String, Manifest>, DepsError> {
    let Some(text) = tree.read(ROOT)? else { return Err(DepsError::Missing { path: ROOT.to_owned(), what: "the root manifest".to_owned() }) };
    let root_doc = read_toml(ROOT, &text)?;
    let root = root_doc.root();
    let Some(ws) = get_table(root, "workspace", ROOT)? else {
        return Err(DepsError::Shape { at: Place { file: ROOT.to_owned(), line: 1 }, key: "workspace".to_owned(), want: "a table: the root is no workspace" });
    };
    let root_deps = get_table(ws, "dependencies", ROOT)?;
    let members = members_of(tree, root, ws)?;
    // Each directory to read, and the line that named it (none for a member).
    let mut todo: Vec<(String, Option<Place>)> = members.iter().rev().map(|dir| (dir.clone(), None)).collect();
    if let Some(patches) = get_table(root, "patch", ROOT)? {
        for patch in patches.children() {
            if !patch.is_table() {
                return Err(shape(patch, &format!("patch.{}", patch.key()), ROOT, "a table"));
            }
            for value in patch.children() {
                let at = Place { file: ROOT.to_owned(), line: line_of(value) };
                if let Some(path) = dep_of(value.key(), value, DepKind::Normal, &at, "", None)?.path {
                    todo.push((path, Some(at)));
                }
            }
        }
    }
    workspace_from(tree, root, root_deps, &members, todo)
}

/// The members' directories, from the root (`""` for a root package): `[workspace] members`, each
/// a path or `<dir>/*` (every directory of `<dir>` with a Cargo.toml), less `exclude`.
fn members_of(tree: &dyn Tree, root: toml::Node<'_, '_>, ws: toml::Node<'_, '_>) -> Result<BTreeSet<String>, DepsError> {
    let excluded: BTreeSet<String> = get_strs(ws, "exclude", ROOT)?
        .map_or_else(Vec::new, |(_, them)| them)
        .iter()
        .map(|it| it.trim_end_matches('/').to_owned())
        .collect();
    let globbed = |text: &str| text.contains(|c: char| matches!(c, '*' | '?' | '[' | ']'));
    let mut members: BTreeSet<String> = BTreeSet::new();
    if root.get("package").is_some() {
        members.insert(String::new());
    }
    if let Some((line, patterns)) = get_strs(ws, "members", ROOT)? {
        let at = Place { file: ROOT.to_owned(), line };
        for pattern in patterns {
            let pattern = pattern.trim_end_matches('/');
            match pattern.strip_suffix("/*") {
                Some(dir) if !globbed(dir) => {
                    members.extend(tree.manifest_dirs(dir)?.into_iter().filter(|it| !excluded.contains(it)));
                }
                _ if globbed(pattern) => return Err(DepsError::Glob { at, pattern: pattern.to_owned() }),
                _ => {
                    let Some(dir) = join("", pattern) else { return Err(DepsError::Outside { at, path: pattern.to_owned() }) };
                    members.insert(dir);
                }
            }
        }
    }
    Ok(members)
}

/// [`workspace`] from its members and the directories `todo` to read.
fn workspace_from(
    tree: &dyn Tree,
    root: toml::Node<'_, '_>,
    root_deps: Option<toml::Node<'_, '_>>,
    members: &BTreeSet<String>,
    mut todo: Vec<(String, Option<Place>)>,
) -> Result<BTreeMap<String, Manifest>, DepsError> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: BTreeMap<String, Manifest> = BTreeMap::new();
    // Bounded: a directory is read once, and each read adds only the directories it names.
    while let Some((dir, by)) = todo.pop() {
        if !seen.insert(dir.clone()) {
            continue;
        }
        let file = manifest_path(&dir);
        // A member's text and document, read here and done with by the end of the turn.
        let member_text: String;
        let doc: toml::Doc<'_>;
        let table = if dir.is_empty() {
            root
        } else {
            let Some(read) = tree.read(&file)? else {
                let what = match &by {
                    Some(at) => format!("the manifest of the package {at} names"),
                    None => "a member's manifest".to_owned(),
                };
                return Err(DepsError::Missing { path: file, what });
            };
            member_text = read;
            doc = read_toml(&file, &member_text)?;
            doc.root()
        };
        let Some(package) = get_table(table, "package", &file)? else {
            return Err(DepsError::Shape { at: Place { file, line: 1 }, key: "package".to_owned(), want: "a table: the manifest has no `[package]`" });
        };
        let Some(name) = get_str(package, "name", &file)? else {
            return Err(DepsError::Shape { at: Place { file, line: 1 }, key: "package.name".to_owned(), want: "a string: the package has no name" });
        };
        let member = members.contains(&dir);
        let vendored = dir == VENDOR || dir.starts_with("vendor/");
        let ours = !vendored || fork_is_ours(package, &file)?;
        let mut deps = deps_of(&file, &dir, table, root_deps)?;
        if !member {
            deps.retain(|it| it.kind != DepKind::Dev);
        }
        for dep in &deps {
            if let Some(path) = &dep.path {
                todo.push((path.clone(), Some(dep.at.clone())));
            }
        }
        if let Some(first) = out.get(&name) {
            return Err(DepsError::TwoPackages { name, first: first.file.clone(), second: file });
        }
        out.insert(name.clone(), Manifest { dir, file, name, ours, deps });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Cargo.lock, and the count.

/// One `[[package]]` of Cargo.lock.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Locked {
    name: String,
    version: String,
    /// None for a package with a manifest in the repository.
    source: Option<String>,
    /// As written: `name`, `name version` or `name version (source)`.
    deps: Vec<String>,
}

fn read_lock(tree: &dyn Tree) -> Result<Vec<Locked>, DepsError> {
    let Some(text) = tree.read(LOCK)? else { return Err(DepsError::Missing { path: LOCK.to_owned(), what: "the lock".to_owned() }) };
    let doc = read_toml(LOCK, &text)?;
    let want = "an array of tables, `[[package]]`";
    let packages = match doc.root().get("package") {
        None => return Ok(Vec::new()),
        Some(them) if them.is_array() => them,
        Some(it) => return Err(shape(it, "package", LOCK, want)),
    };
    let mut out = Vec::new();
    for package in packages.children() {
        if !package.is_table() {
            return Err(shape(package, "package", LOCK, want));
        }
        let line = line_of(package);
        let need = |key: &str| DepsError::Shape { at: Place { file: LOCK.to_owned(), line }, key: key.to_owned(), want: "a string, which every package has" };
        let Some(name) = get_str(package, "name", LOCK)? else { return Err(need("name")) };
        let Some(version) = get_str(package, "version", LOCK)? else { return Err(need("version")) };
        let source = get_str(package, "source", LOCK)?;
        let deps = get_strs(package, "dependencies", LOCK)?.map_or_else(Vec::new, |(_, them)| them);
        out.push(Locked { name, version, source, deps });
    }
    Ok(out)
}

/// The entry of `lock` that the dependency `text` of the package `of` names: `name`, `name
/// version` or `name version (source)`, which Cargo.lock writes when the shorter form is two.
fn lock_index(lock: &[Locked], text: &str, of: &str) -> Result<usize, DepsError> {
    let (name, rest) = text.split_once(' ').unwrap_or((text, ""));
    let (version, source) = match rest.split_once(' ') {
        Some((version, source)) => (Some(version), Some(source.strip_prefix('(').and_then(|it| it.strip_suffix(')')).unwrap_or(source))),
        None if rest.is_empty() => (None, None),
        None => (Some(rest), None),
    };
    let found: Vec<usize> = lock
        .iter()
        .enumerate()
        .filter(|(_, it)| {
            it.name == name && version.is_none_or(|v| it.version == v) && source.is_none_or(|s| it.source.as_deref() == Some(s))
        })
        .map(|(at, _)| at)
        .collect();
    match found.as_slice() {
        [one] => Ok(*one),
        _ => Err(DepsError::LockDep { package: of.to_owned(), text: text.to_owned(), found: found.len() }),
    }
}

/// Whether `version` meets the requirement `req`, as far as this reads one: `=x.y.z` exactly
/// (as many parts as it gives), `^x.y.z` or a bare `x.y.z` as cargo's caret (equal to its
/// leftmost nonzero part, and not below it). `None` for another form (`~`, `>=`, `*`, a list).
fn may_be(req: &str, version: &str) -> Option<bool> {
    let req = req.trim();
    let (exact, req) = match req.strip_prefix('=') {
        Some(rest) => (true, rest.trim()),
        None => (false, req.strip_prefix('^').unwrap_or(req).trim()),
    };
    let parts = |text: &str| -> Option<Vec<u64>> { text.split('.').map(|it| it.parse::<u64>().ok()).collect() };
    let want = parts(req)?;
    let have = parts(version.split(|c: char| c == '-' || c == '+').next()?)?;
    if want.is_empty() || want.len() > 3 || have.len() != 3 {
        return None;
    }
    let equal = |n: usize| want[..n].iter().zip(&have).all(|(a, b)| a == b);
    if exact {
        return Some(equal(want.len()));
    }
    let fixed = want.iter().position(|it| *it != 0).map_or(want.len(), |at| at + 1);
    Some(equal(fixed) && have[..want.len()] >= want[..])
}

/// A third-party package: a name and a version.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Pkg {
    name: String,
    version: String,
}

impl fmt::Display for Pkg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.name, self.version)
    }
}

/// The direct dependency a package of a closure came in by.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Via {
    key: String,
    at: Place,
}

/// One workspace crate's third-party packages, each with the manifest line it came in by (the
/// first, in line order, where two do).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Counted {
    direct: BTreeMap<Pkg, Place>,
    dev: BTreeMap<Pkg, Place>,
    closure: BTreeMap<Pkg, Via>,
}

/// Every workspace crate's count, by its package name.
type Census = BTreeMap<String, Counted>;

/// A package of the graph: a workspace crate by name, or an entry of Cargo.lock.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Node {
    Ours(String),
    Third(usize),
}

/// The graph being walked, and what each node reaches, once known.
struct Graph<'w> {
    manifests: &'w BTreeMap<String, Manifest>,
    lock: &'w [Locked],
    reached: BTreeMap<Node, BTreeSet<Pkg>>,
}

impl<'w> Graph<'w> {
    fn pkg(&self, at: usize) -> Pkg {
        Pkg { name: self.lock[at].name.clone(), version: self.lock[at].version.clone() }
    }

    /// What the entry `at` of the lock is.
    fn node_of_lock(&self, at: usize) -> Result<Node, DepsError> {
        let it = &self.lock[at];
        if it.source.is_some() {
            return Ok(Node::Third(at));
        }
        match self.manifests.get(&it.name) {
            Some(m) if m.ours => Ok(Node::Ours(it.name.clone())),
            Some(_) => Ok(Node::Third(at)),
            None => Err(DepsError::UnknownPath { name: it.name.clone() }),
        }
    }

    /// The lock's entry of a package with a manifest in the repository.
    fn path_entry(&self, name: &str) -> Option<usize> {
        self.lock.iter().position(|it| it.name == name && it.source.is_none())
    }

    /// What the dependency `dep` of the crate `m` is; `None` for an optional one the lock does
    /// not list.
    fn node_of_dep(&self, m: &Manifest, dep: &Dep) -> Result<Option<Node>, DepsError> {
        let not_locked = || DepsError::NotInLock { krate: m.name.clone(), package: dep.package.clone(), at: dep.at.clone() };
        if let Some(dir) = &dep.path {
            let Some(target) = self.manifests.values().find(|it| &it.dir == dir) else {
                return Err(DepsError::UnknownPath { name: dep.package.clone() });
            };
            if target.ours {
                return Ok(Some(Node::Ours(target.name.clone())));
            }
            return match self.path_entry(&target.name) {
                Some(at) => Ok(Some(Node::Third(at))),
                None if dep.optional => Ok(None),
                None => Err(not_locked()),
            };
        }
        let Some(own) = self.path_entry(&m.name) else {
            return Err(DepsError::CrateNotLocked { krate: m.name.clone(), manifest: m.file.clone() });
        };
        let mut found = Vec::new();
        for text in &self.lock[own].deps {
            let at = lock_index(self.lock, text, &m.name)?;
            if self.lock[at].name == dep.package {
                found.push(at);
            }
        }
        if found.len() > 1
            && let Some(req) = &dep.version
        {
            found.retain(|at| may_be(req, &self.lock[*at].version) != Some(false));
        }
        match found.as_slice() {
            [] if dep.optional => Ok(None),
            [] => Err(not_locked()),
            [one] => self.node_of_lock(*one).map(Some),
            _ => Err(DepsError::Ambiguous {
                krate: m.name.clone(),
                package: dep.package.clone(),
                at: dep.at.clone(),
                versions: found.iter().map(|at| self.lock[*at].version.clone()).collect(),
            }),
        }
    }

    /// The third-party packages `node` is, and those its normal and build dependencies reach.
    fn reach(&mut self, node: &Node) -> Result<BTreeSet<Pkg>, DepsError> {
        if let Some(done) = self.reached.get(node) {
            return Ok(done.clone());
        }
        // A cycle, which cargo's graph has none of through normal and build dependencies, would
        // read this node's set as empty rather than recurse forever.
        self.reached.insert(node.clone(), BTreeSet::new());
        let manifests = self.manifests;
        let lock = self.lock;
        let mut out = BTreeSet::new();
        match node {
            Node::Ours(name) => {
                let Some(m) = manifests.get(name) else { return Err(DepsError::UnknownPath { name: name.clone() }) };
                for dep in m.deps.iter().filter(|it| it.kind != DepKind::Dev) {
                    if let Some(next) = self.node_of_dep(m, dep)? {
                        out.extend(self.reach(&next)?);
                    }
                }
            }
            Node::Third(at) => {
                out.insert(self.pkg(*at));
                for text in &lock[*at].deps {
                    let next = self.node_of_lock(lock_index(lock, text, &lock[*at].name)?)?;
                    out.extend(self.reach(&next)?);
                }
            }
        }
        self.reached.insert(node.clone(), out.clone());
        Ok(out)
    }
}

/// Every workspace crate's third-party packages, from the tree's manifests and Cargo.lock.
fn census(tree: &dyn Tree) -> Result<Census, DepsError> {
    let manifests = workspace(tree)?;
    let lock = read_lock(tree)?;
    let mut graph = Graph { manifests: &manifests, lock: &lock, reached: BTreeMap::new() };
    let mut out = Census::new();
    for m in manifests.values().filter(|it| it.ours) {
        let mut counted = Counted::default();
        let mut dev: Vec<(Pkg, Place)> = Vec::new();
        for dep in &m.deps {
            let Some(node) = graph.node_of_dep(m, dep)? else { continue };
            if dep.kind == DepKind::Dev {
                if let Node::Third(at) = node {
                    dev.push((graph.pkg(at), dep.at.clone()));
                }
                continue;
            }
            if let Node::Third(at) = &node {
                counted.direct.entry(graph.pkg(*at)).or_insert_with(|| dep.at.clone());
            }
            for pkg in graph.reach(&node)? {
                counted.closure.entry(pkg).or_insert_with(|| Via { key: dep.key.clone(), at: dep.at.clone() });
            }
        }
        for (pkg, at) in dev {
            if !counted.direct.contains_key(&pkg) {
                counted.dev.entry(pkg).or_insert(at);
            }
        }
        out.insert(m.name.clone(), counted);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// The crates the binaries reach (D173): written here, the one home of the manifests' graph, and
// read by the ambient tool's no_std check (ambient/sansio.rs, the `bare` column).

/// The packages whose binaries ship: `blaze` (blaze-cli) and the launcher.
const BINARIES: [&str; 2] = ["blaze-cli", "blaze-launcher"];

/// The list of the workspace crates the binaries reach, from the root.
const REACHED_LIST: &str = "tools/ratchets/reached.txt";

const REACHED_HEADER: &str = "# The workspace crates the shipped binaries (blaze-cli's `blaze`, blaze-launcher's) reach\n\
                              # through normal and build dependencies (never dev), every target's and every optional one\n\
                              # the lock lists, from the manifests and Cargo.lock: the crate's directory, its third-party\n\
                              # packages named directly, and the workspace crates it names directly (`-` for none). Written\n\
                              # by scripts/src/bin/thirdparty.rs (`{\"command\": \"write\"}`), and its `check` refuses a\n\
                              # list that is not as `write` writes it; read by the ambient tool's no_std check (D173).\n\
                              # crate\tthird_party\tdepends_on\n";

/// One reached workspace crate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Reached {
    /// Its third-party packages named directly by a normal or build dependency.
    direct: BTreeSet<Pkg>,
    /// The directories of the workspace crates it names by a normal or build dependency.
    deps: BTreeSet<String>,
}

/// Every workspace crate [`BINARIES`] reach, by its directory, from the tree's manifests and lock.
fn reached(tree: &dyn Tree) -> Result<BTreeMap<String, Reached>, DepsError> {
    reached_from(tree, &BINARIES)
}

/// Every workspace crate the packages `roots` reach, by its directory, the roots with them.
fn reached_from(tree: &dyn Tree, roots: &[&str]) -> Result<BTreeMap<String, Reached>, DepsError> {
    let manifests = workspace(tree)?;
    let lock = read_lock(tree)?;
    let graph = Graph { manifests: &manifests, lock: &lock, reached: BTreeMap::new() };
    let mut out: BTreeMap<String, Reached> = BTreeMap::new();
    let mut todo: Vec<String> = roots.iter().map(|it| (*it).to_owned()).collect();
    // Bounded: a crate is expanded once (by its directory), and only an expanded crate pushes.
    while let Some(name) = todo.pop() {
        let Some(m) = manifests.get(&name) else { return Err(DepsError::UnknownPath { name }) };
        if out.contains_key(&m.dir) {
            continue;
        }
        let mut row = Reached::default();
        for dep in m.deps.iter().filter(|it| it.kind != DepKind::Dev) {
            match graph.node_of_dep(m, dep)? {
                Some(Node::Ours(next)) => {
                    if let Some(target) = manifests.get(&next) {
                        row.deps.insert(target.dir.clone());
                    }
                    todo.push(next);
                }
                Some(Node::Third(at)) => {
                    row.direct.insert(graph.pkg(at));
                }
                None => {}
            }
        }
        out.insert(m.dir.clone(), row);
    }
    Ok(out)
}

/// The reached list's text: its header, then a row per crate, sorted.
fn render_reached(reached: &BTreeMap<String, Reached>) -> String {
    let mut out = REACHED_HEADER.to_owned();
    for (dir, row) in reached {
        let deps: Vec<&str> = row.deps.iter().map(String::as_str).collect();
        let deps = if deps.is_empty() { "-".to_owned() } else { deps.join(",") };
        out.push_str(&format!("{dir}\t{}\t{deps}\n", row.direct.len()));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The list.

/// A column of the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    Direct,
    Dev,
    Closure,
}

impl Column {
    const ALL: [Column; 3] = [Column::Direct, Column::Dev, Column::Closure];

    const fn index(self) -> usize {
        match self {
            Column::Direct => 0,
            Column::Dev => 1,
            Column::Closure => 2,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Column::Direct => "direct",
            Column::Dev => "dev",
            Column::Closure => "closure",
        }
    }

    /// What its packages are, for a message: `<n> <what>`.
    const fn what(self) -> &'static str {
        match self {
            Column::Direct => "third-party packages in its normal and build dependencies",
            Column::Dev => "third-party packages in its dev-dependencies only",
            Column::Closure => "third-party packages its normal and build dependencies reach",
        }
    }
}

/// A row's counts, by [`Column::index`].
type Row = [usize; 3];

/// The packages of `column` of the crate `krate` (or of [`TOTAL`]), each with where it came in,
/// for a message: `at <file>:<line>`, `through `<key>` at ..`, or for the total `in `<crate>` ..`
/// (the first crate, by name).
fn column(census: &Census, krate: &str, col: Column) -> BTreeMap<Pkg, String> {
    if krate != TOTAL {
        let Some(c) = census.get(krate) else { return BTreeMap::new() };
        return match col {
            Column::Direct => c.direct.iter().map(|(pkg, at)| (pkg.clone(), format!("at {at}"))).collect(),
            Column::Dev => c.dev.iter().map(|(pkg, at)| (pkg.clone(), format!("at {at}"))).collect(),
            Column::Closure => c.closure.iter().map(|(pkg, via)| (pkg.clone(), format!("through `{}` at {}", via.key, via.at))).collect(),
        };
    }
    let mut out = BTreeMap::new();
    for name in census.keys() {
        for (pkg, how) in column(census, name, col) {
            out.entry(pkg).or_insert_with(|| format!("in `{name}` {how}"));
        }
    }
    if col == Column::Dev {
        let direct = column(census, TOTAL, Column::Direct);
        out.retain(|pkg, _| !direct.contains_key(pkg));
    }
    out
}

/// The row of `krate` (or of [`TOTAL`]).
fn row_of(census: &Census, krate: &str) -> Row {
    Column::ALL.map(|col| column(census, krate, col).len())
}

/// The list as `write` writes it: the header, a row per crate with a count above 0, sorted, and
/// the total's.
fn render(census: &Census) -> String {
    let mut out = HEADER.to_owned();
    for name in census.keys() {
        let [direct, dev, closure] = row_of(census, name);
        if [direct, dev, closure] != [0; 3] {
            out.push_str(&format!("{name}\t{direct}\t{dev}\t{closure}\n"));
        }
    }
    let [direct, dev, closure] = row_of(census, TOTAL);
    out.push_str(&format!("{TOTAL}\t{direct}\t{dev}\t{closure}\n"));
    out
}

/// The list read wrong: where, and what is wrong there.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ListError {
    /// A line (from 1) that is not a row, a comment or blank.
    NotARow { line: usize, text: String },
    /// A crate's second row, at its line (from 1).
    TwoRows { line: usize, krate: String },
}

impl fmt::Display for ListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ListError::NotARow { line, text } => write!(f, "{LIST}:{line}: {text:?} is not `<crate><TAB><direct><TAB><dev><TAB><closure>`"),
            ListError::TwoRows { line, krate } => write!(f, "{LIST}:{line}: `{krate}` has two rows"),
        }
    }
}

impl std::error::Error for ListError {}

/// The base could not be read: which base, and what of it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BaseError {
    /// `base` names no commit of the checkout; `why` is git's message.
    NoCommit { base: String, why: String },
    /// A git command about `base` failed: what it was to do, and git's message.
    Git { base: String, what: &'static str, why: String },
    /// The base's list does not read.
    List { base: String, error: ListError },
}

impl fmt::Display for BaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BaseError::NoCommit { base, why } => write!(f, "the base {base} is no commit here (fetch it): {why}"),
            BaseError::Git { base, what, why } => write!(f, "git could not {what} at the base {base}: {why}"),
            BaseError::List { base, error } => write!(f, "at the base {base}: {error}"),
        }
    }
}

impl std::error::Error for BaseError {}

/// The list's rows, the total's included.
fn parse_rows(text: &str) -> Result<BTreeMap<String, Row>, ListError> {
    let mut rows = BTreeMap::new();
    for (at, line) in text.split('\n').enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let row = match fields.as_slice() {
            [krate, rest @ ..] if !krate.is_empty() && rest.len() == 3 => {
                let mut n: Row = [0; 3];
                let read = rest.iter().zip(&mut n).all(|(field, slot)| field.parse::<usize>().map(|it| *slot = it).is_ok());
                read.then_some((*krate, n))
            }
            _ => None,
        };
        let Some((krate, n)) = row else { return Err(ListError::NotARow { line: at + 1, text: line.to_owned() }) };
        if rows.insert(krate.to_owned(), n).is_some() {
            return Err(ListError::TwoRows { line: at + 1, krate: krate.to_owned() });
        }
    }
    Ok(rows)
}

/// The base's rows: none when it has no list, before the ratchet began.
fn rows_at_base(root: &Path, base: &str) -> Result<Option<BTreeMap<String, Row>>, BaseError> {
    let root_arg = root.to_string_lossy();
    let dir = root_arg.as_ref();
    let commit = format!("{base}^{{commit}}");
    git(root, &["-C", dir, "rev-parse", "--verify", "--quiet", commit.as_str()])
        .map_err(|why| BaseError::NoCommit { base: base.to_owned(), why })?;
    let failed = |what: &'static str| move |why: String| BaseError::Git { base: base.to_owned(), what, why };
    let listed = git(root, &["-C", dir, "ls-tree", "--name-only", base, "--", LIST]).map_err(failed("list its tree"))?;
    if listed.trim().is_empty() {
        return Ok(None);
    }
    let spec = format!("{base}:{LIST}");
    let text = git(root, &["-C", dir, "show", spec.as_str()]).map_err(failed("show the list"))?;
    parse_rows(&text).map(Some).map_err(|error| BaseError::List { base: base.to_owned(), error })
}

/// The names a check goes over: every crate counted or listed, then the total.
fn names<'c>(census: &'c Census, rows: &'c BTreeMap<String, Row>) -> Vec<&'c str> {
    let crates: BTreeSet<&str> = census.keys().chain(rows.keys()).map(String::as_str).filter(|it| *it != TOTAL).collect();
    crates.into_iter().chain([TOTAL]).collect()
}

/// Whether a count is over its row: then the base's tree is read, to name what is new.
fn over(census: &Census, rows: &BTreeMap<String, Row>) -> bool {
    names(census, rows).into_iter().any(|name| {
        let held = rows.get(name).copied().unwrap_or([0; 3]);
        row_of(census, name).iter().zip(held).any(|(now, held)| *now > held)
    })
}

/// What is wrong: `now` the tree's counts, `rows` the list's, `base_rows` the base's list (when
/// it had one), `then` the base's counts (read when a count is over its row and a base given).
fn problems(now: &Census, rows: &BTreeMap<String, Row>, base_rows: Option<&BTreeMap<String, Row>>, then: Option<&Census>) -> Vec<String> {
    let mut out = Vec::new();
    for name in names(now, rows) {
        let row = rows.get(name).copied();
        let entries = Column::ALL.map(|col| column(now, name, col));
        let now_row: Row = [entries[0].len(), entries[1].len(), entries[2].len()];
        let subject = if name == TOTAL { "the workspace (`(all)`)".to_owned() } else { format!("`{name}`") };
        for col in Column::ALL {
            let n = now_row[col.index()];
            let held = row.map_or(0, |it| it[col.index()]);
            let (what, column_name) = (col.what(), col.name());
            if n > held {
                let shown = match row {
                    Some(_) => format!("its row's `{column_name}` {held}"),
                    None => "0, as a crate with no row is held".to_owned(),
                };
                let listed = |pkgs: Vec<String>| pkgs.join(", ");
                let named = match then {
                    Some(then) => {
                        let before = column(then, name, col);
                        let new: Vec<String> =
                            entries[col.index()].iter().filter(|(pkg, _)| !before.contains_key(*pkg)).map(|(pkg, how)| format!("`{pkg}` {how}")).collect();
                        if new.is_empty() {
                            "none is new since the base: the base's count was over its row too".to_owned()
                        } else {
                            format!("new: {}", listed(new))
                        }
                    }
                    None if col != Column::Closure && name != TOTAL => {
                        let all: Vec<String> = entries[col.index()].iter().map(|(pkg, how)| format!("`{pkg}` {how}")).collect();
                        format!("its packages: {}; give `base` to have the new ones named", listed(all))
                    }
                    None => "give `base`, the commit the change is against, to have the new ones named".to_owned(),
                };
                out.push(format!("{subject} has {n} {what}, over {shown} in {LIST} ({named}): {INSTEAD} ({RULE})"));
            } else if n < held {
                let lower = if now_row == [0; 3] && name != TOTAL { "remove the row".to_owned() } else { format!("lower its `{column_name}` to {n}") };
                out.push(format!("{subject} has {n} {what}, under its row's `{column_name}` {held} in {LIST}: {lower} (command `write`); the list only shrinks"));
            }
        }
    }
    if let Some(base) = base_rows {
        for (name, n) in rows {
            let was = base.get(name).copied().unwrap_or([0; 3]);
            for col in Column::ALL {
                let (now, then) = (n[col.index()], was[col.index()]);
                if now > then {
                    out.push(format!("{LIST}: the row `{name}`'s `{}` is {now} and was {then} at the base: the list only shrinks ({RULE})", col.name()));
                }
            }
        }
    }
    out
}

/// The census's totals, for a line of output.
fn summary(census: &Census) -> String {
    let [direct, dev, closure] = row_of(census, TOTAL);
    let depending = census.keys().filter(|name| row_of(census, name) != [0; 3]).count();
    format!(
        "{direct} third-party packages named directly, {dev} more only by tests and benches, {closure} reached; \
         {depending} of {} workspace crates depend on one",
        census.len()
    )
}

fn main_with(it: &Args) {
    // `Path(root).resolve()`.
    let root: PathBuf =
        std::fs::canonicalize(&it.root).unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&it.root));
    if let Some(what) = it.command.strip_prefix("reach-") {
        if let Err(why) = reach::run(&Disk { root: root.clone() }, &root, what, it.metadata.as_deref()) {
            eprintln!("{why}");
            std::process::exit(1);
        }
        return;
    }
    if let Some(what) = it.command.strip_prefix("manifests-") {
        let shape = manifests::shape(&Disk { root: root.clone() }).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
        if what == "report" {
            print!("{}", manifests::report(&shape));
            return;
        }
        if !shape.problems.is_empty() {
            let lines: Vec<String> = shape.problems.iter().map(|w| format!("{SCRIPT}: {w}")).collect();
            eprintln!("{}\n{SCRIPT}: {} manifest problems", lines.join("\n"), shape.problems.len());
            std::process::exit(1);
        }
        print!("{SCRIPT}: manifests hold: {}", manifests::report(&shape).lines().last().map_or(String::new(), |it| format!("{it}\n")));
        return;
    }
    let now = census(&Disk { root: root.clone() }).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    let text = render(&now);
    let reached_now = reached(&Disk { root: root.clone() }).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    let reached_text = render_reached(&reached_now);
    if it.command == "write" {
        if let Err(e) = std::fs::write(root.join(LIST), &text) {
            fail(SCRIPT, &format!("{LIST} is not written: {e}"));
        }
        if let Err(e) = std::fs::write(root.join(REACHED_LIST), &reached_text) {
            fail(SCRIPT, &format!("{REACHED_LIST} is not written: {e}"));
        }
        println!("{SCRIPT}: wrote {LIST}: {}; wrote {REACHED_LIST}: {} crates reached", summary(&now), reached_now.len());
        return;
    }
    if read_text(&root.join(REACHED_LIST)).as_deref() != Some(reached_text.as_str()) {
        fail(SCRIPT, &format!("{REACHED_LIST} is not as command `write` writes it from the manifests (the crates the binaries reach): run it"));
    }
    let Some(listed) = read_text(&root.join(LIST)) else {
        fail(SCRIPT, &format!("{LIST} could not be read: every crate is held to its row there (command `write` makes it)"))
    };
    let rows = parse_rows(&listed).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    let base_rows = match &it.base {
        Some(base) => {
            let rows = rows_at_base(&root, base).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
            if rows.is_none() {
                println!("{SCRIPT}: {base} has no {LIST}: the list begins with this change, so no row is compared with the base");
            }
            rows
        }
        None => None,
    };
    let then = match &it.base {
        Some(base) if over(&now, &rows) => {
            let tree = AtBase::new(&root, base).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
            let then = census(&tree).unwrap_or_else(|e| {
                fail(SCRIPT, &format!("the base {base}'s manifests and {LOCK}, read to name the new packages, do not read: {e}"))
            });
            Some(then)
        }
        _ => None,
    };
    let mut wrong = problems(&now, &rows, base_rows.as_ref(), then.as_ref());
    if wrong.is_empty() && listed != text {
        wrong.push(format!(
            "{LIST} is not as command `write` writes it (its header, a sorted `<crate><TAB><direct><TAB><dev><TAB><closure>` row per crate with a count, then `{TOTAL}`): run it"
        ));
    }
    if !wrong.is_empty() {
        let lines: Vec<String> = wrong.iter().map(|w| format!("{SCRIPT}: {w}")).collect();
        eprintln!("{}", lines.join("\n"));
        std::process::exit(1);
    }
    println!("{SCRIPT}: {}, each crate at its row in {LIST}", summary(&now));
}

// ---------------------------------------------------------------------------------------------
// The selftest.

fn expect(holds: bool, what: &str) {
    blaze_script::expect(SCRIPT, holds, what);
}

/// The selftest's root manifest: members by a pattern and a path, one excluded, the
/// workspace's dependencies (a path, a version, a rename to a pinned version) and two patches.
const FIXTURE_ROOT: &str = r#"# The selftest's workspace: four crates, one parked, a vendored fork and a fork that is ours.
[workspace]
members = [
    "crates/*", # every directory of crates/ with a manifest
    "tools/t",
]
exclude = ["crates/parked"]

[workspace.dependencies]
b = { path = "crates/b" }
serde = { version = "1.0", features = ["derive"] }
uw = { version = "=0.1.14", package = "unicode-width" }

[patch.crates-io]
forked = { path = "vendor/forked" }
mine = { path = "vendor/mine" }
"#;

/// Crate `a`, line by line: 6 `b` through the root (a workspace crate's path), 7 `serde` through
/// the root, 8 `uw` through the root's rename and pin (unicode-width 0.1.14), 9 the other
/// unicode-width (0.2.1), 10 `forked` (patched to vendor/, third-party), 11 `mine` (patched to
/// vendor/, ours); dev 14 `moved` (dev only), 15 `serde` (also normal: not dev only).
const FIXTURE_A: &str = r#"[package]
name = "a"
version.workspace = true

[dependencies]
b.workspace = true
serde = { workspace = true, features = ["derive"] }
uw.workspace = true
unicode-width = "0.2"
forked = "1"
mine = "1"

[dev-dependencies]
moved = "1"
serde = "1"
"#;

/// Crate `b`: a multi-line description with escapes, 9 `libc` for one target, 10 an optional
/// dependency no feature turns on (Cargo.lock lists it not), 12 `cc` as a table header.
const FIXTURE_B: &str = r#"[package]
name = "b"
version = "0.0.0"
description = """
One line, \
  then the next: \u00e9."""

[target.'cfg(unix)'.dependencies]
libc = "0.2"
extra = { version = "2", optional = true }

[build-dependencies.cc]
version = "1"
"#;

/// Crate `t`, by a path member: 5 a renamed package, 6 `a` by a relative path.
const FIXTURE_T: &str = r#"[package]
name = "t"

[dependencies]
alias = { version = "0.3", package = "real-name" }
a = { path = "../../crates/a" }
"#;

const FIXTURE_FORKED: &str = "[package]\nname = \"forked\"\nversion = \"1.0.0\"\n\n[dependencies]\nlibc = \"0.2\"\n";
const FIXTURE_MINE: &str = "[package]\nname = \"mine\"\nversion = \"1.0.0\"\n\n[package.metadata.blaze]\nours = true\n";
/// Excluded: if it were read, it would be refused, as Cargo.lock has no entry for it.
const FIXTURE_PARKED: &str = "[package]\nname = \"parked\"\n\n[dependencies]\nhuge = \"1\"\n";

const FIXTURE_LOCK: &str = r#"# This file is automatically @generated by Cargo.
version = 4

[[package]]
name = "a"
version = "0.0.0"
dependencies = [
 "b",
 "forked",
 "mine",
 "moved",
 "serde",
 "unicode-width 0.1.14",
 "unicode-width 0.2.1",
]

[[package]]
name = "b"
version = "0.0.0"
dependencies = [
 "cc",
 "libc",
]

[[package]]
name = "c"
version = "0.0.0"

[[package]]
name = "cc"
version = "1.2.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "00"

[[package]]
name = "forked"
version = "1.0.0"
dependencies = [
 "libc",
]

[[package]]
name = "libc"
version = "0.2.100"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "mine"
version = "1.0.0"

[[package]]
name = "moved"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "real-name"
version = "0.3.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "serde"
version = "1.0.200"
source = "registry+https://github.com/rust-lang/crates.io-index"
dependencies = [
 "serde_derive",
]

[[package]]
name = "serde_derive"
version = "1.0.200"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "t"
version = "0.0.0"
dependencies = [
 "a",
 "real-name",
]

[[package]]
name = "unicode-width"
version = "0.1.14"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "unicode-width"
version = "0.2.1"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;

/// The selftest's tree, with each `(path, from, to)` edit made.
fn fixture(edits: &[(&str, &str, &str)]) -> Fixture {
    let mut files: BTreeMap<String, String> = [
        (ROOT, FIXTURE_ROOT),
        (LOCK, FIXTURE_LOCK),
        ("crates/a/Cargo.toml", FIXTURE_A),
        ("crates/a/src/lib.rs", ""),
        ("crates/b/Cargo.toml", FIXTURE_B),
        ("crates/c/Cargo.toml", "[package]\nname = \"c\"\n"),
        ("crates/parked/Cargo.toml", FIXTURE_PARKED),
        ("crates/notes/README.md", "a directory of crates/ with no manifest is no member\n"),
        ("tools/t/Cargo.toml", FIXTURE_T),
        ("vendor/forked/Cargo.toml", FIXTURE_FORKED),
        ("vendor/mine/Cargo.toml", FIXTURE_MINE),
    ]
    .into_iter()
    .map(|(path, text)| (path.to_owned(), text.to_owned()))
    .collect();
    for (path, from, to) in edits {
        let Some(text) = files.get_mut(*path) else { fail(&format!("{SCRIPT} selftest"), &format!("the fixture has no {path}")) };
        expect(text.contains(from), &format!("the fixture's {path} has {from:?} to replace"));
        *text = text.replace(from, to);
    }
    Fixture { files }
}

fn pkg(name: &str, version: &str) -> Pkg {
    Pkg { name: name.to_owned(), version: version.to_owned() }
}

fn place(file: &str, line: usize) -> Place {
    Place { file: file.to_owned(), line }
}

/// The census of the fixture with `edits`, or the selftest fails naming why.
fn census_of(edits: &[(&str, &str, &str)]) -> Census {
    census(&fixture(edits)).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &format!("the fixture's census: {e}")))
}

/// What the fixture with `edits` is refused with.
fn refused(edits: &[(&str, &str, &str)]) -> Option<DepsError> {
    census(&fixture(edits)).err()
}

fn selftest() {
    // The reach of the shipped binaries' crates: from `t`, through normal and build
    // dependencies only (`a`'s dev `moved` is none), each crate once by its directory, and a
    // crate nothing reaches (`c`) is not there.
    let from_t = reached_from(&fixture(&[]), &["t"]).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &format!("the fixture's reach: {e}")));
    let dirs: Vec<&str> = from_t.keys().map(String::as_str).collect();
    expect(dirs.contains(&"tools/t") && dirs.contains(&"crates/a") && dirs.contains(&"crates/b") && !dirs.contains(&"crates/c"), &format!("the reach: {dirs:?}"));
    expect(from_t.get("tools/t").is_some_and(|it| it.deps.contains("crates/a")), "t names a");
    expect(from_t.get("crates/a").is_some_and(|it| it.deps.contains("crates/b") && !it.direct.iter().any(|p| p.name == "moved")), "a names b, and not its dev packages");
    let text = render_reached(&from_t);
    expect(text.starts_with(REACHED_HEADER) && text.contains("\ntools/t\t1\tcrates/a\n"), &format!("the reached list: {text}"));
    // TOML, read by blaze-toml (whose own tests hold the language): a refusal names the file
    // and line, and a key of the wrong shape its line.
    let twice = read_toml("t.toml", "x = 1\na = 1\na = 2\n").err();
    expect(
        matches!(&twice, Some(DepsError::Toml { at, what: toml::What::Duplicate { .. }, .. }) if *at == place("t.toml", 3)),
        &format!("a key written twice is refused at its line: {twice:?}"),
    );
    let text = "[t]\nname = 1\ndeps = [\n  \"a\",\n  2,\n]\n";
    match read_toml("t.toml", text) {
        Ok(doc) => {
            let t = get_table(doc.root(), "t", "t.toml").ok().flatten();
            let name = t.map(|it| get_str(it, "name", "t.toml"));
            let want = DepsError::Shape { at: place("t.toml", 2), key: "name".to_owned(), want: "a string" };
            expect(name == Some(Err(want)), &format!("a number where a string must be, at its line: {name:?}"));
            let deps = t.map(|it| get_strs(it, "deps", "t.toml"));
            let want = DepsError::Shape { at: place("t.toml", 3), key: "deps".to_owned(), want: "an array of strings" };
            expect(deps == Some(Err(want)), &format!("a number in an array of strings, at the array's line: {deps:?}"));
        }
        Err(e) => expect(false, &format!("blaze-toml reads {text:?}: {e}")),
    }

    // The pieces: paths, the lock's dependency forms, version requirements.
    expect(join("crates/x", "../y/./z") == Some("crates/y/z".to_owned()) && join("tools/t", "../../../up").is_none(), "paths join lexically");
    let lock = read_lock(&fixture(&[])).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    let version_of = |text: &str| lock_index(&lock, text, "x").map(|at| lock[at].version.clone());
    expect(version_of("unicode-width 0.2.1") == Ok("0.2.1".to_owned()), "`name version` picks that version");
    expect(
        version_of("serde 1.0.200 (registry+https://github.com/rust-lang/crates.io-index)") == Ok("1.0.200".to_owned()),
        "`name version (source)` picks that entry",
    );
    expect(
        version_of("unicode-width") == Err(DepsError::LockDep { package: "x".to_owned(), text: "unicode-width".to_owned(), found: 2 }),
        "a bare name of two versions names no one entry",
    );
    expect(
        may_be("=0.1.14", "0.1.14") == Some(true)
            && may_be("0.2", "0.1.14") == Some(false)
            && may_be("0.2", "0.2.1") == Some(true)
            && may_be("^1", "1.9.0") == Some(true)
            && may_be("1.2", "1.1.9") == Some(false)
            && may_be("0.0.3", "0.0.4") == Some(false)
            && may_be("~1", "1.0.0").is_none(),
        "version requirements: exact and caret, others unread",
    );

    // The census of the fixture: every rule.
    let now = census_of(&[]);
    let a_toml = "crates/a/Cargo.toml";
    let b_toml = "crates/b/Cargo.toml";
    let t_toml = "tools/t/Cargo.toml";
    let via = |key: &str, file: &str, line: usize| Via { key: key.to_owned(), at: place(file, line) };
    let want_a = Counted {
        direct: BTreeMap::from([
            (pkg("forked", "1.0.0"), place(a_toml, 10)),
            (pkg("serde", "1.0.200"), place(a_toml, 7)),
            (pkg("unicode-width", "0.1.14"), place(a_toml, 8)),
            (pkg("unicode-width", "0.2.1"), place(a_toml, 9)),
        ]),
        dev: BTreeMap::from([(pkg("moved", "1.0.0"), place(a_toml, 14))]),
        closure: BTreeMap::from([
            (pkg("cc", "1.2.0"), via("b", a_toml, 6)),
            (pkg("forked", "1.0.0"), via("forked", a_toml, 10)),
            (pkg("libc", "0.2.100"), via("b", a_toml, 6)),
            (pkg("serde", "1.0.200"), via("serde", a_toml, 7)),
            (pkg("serde_derive", "1.0.200"), via("serde", a_toml, 7)),
            (pkg("unicode-width", "0.1.14"), via("uw", a_toml, 8)),
            (pkg("unicode-width", "0.2.1"), via("unicode-width", a_toml, 9)),
        ]),
    };
    let want_b = Counted {
        direct: BTreeMap::from([(pkg("libc", "0.2.100"), place(b_toml, 9)), (pkg("cc", "1.2.0"), place(b_toml, 12))]),
        dev: BTreeMap::new(),
        closure: BTreeMap::from([(pkg("libc", "0.2.100"), via("libc", b_toml, 9)), (pkg("cc", "1.2.0"), via("cc", b_toml, 12))]),
    };
    let mut want_t = Counted {
        direct: BTreeMap::from([(pkg("real-name", "0.3.0"), place(t_toml, 5))]),
        dev: BTreeMap::new(),
        closure: BTreeMap::from([(pkg("real-name", "0.3.0"), via("alias", t_toml, 5))]),
    };
    want_t.closure.extend(want_a.closure.keys().map(|it| (it.clone(), via("a", t_toml, 6))));
    let want = Census::from([
        ("a".to_owned(), want_a),
        ("b".to_owned(), want_b),
        ("c".to_owned(), Counted::default()),
        ("mine".to_owned(), Counted::default()),
        ("t".to_owned(), want_t),
    ]);
    expect(now == want, &format!("the fixture's census:\n  found  {now:?}\n  wanted {want:?}"));
    let description = fixture(&[])
        .read(b_toml)
        .ok()
        .flatten()
        .and_then(|text| {
            let doc = read_toml(b_toml, &text).ok()?;
            get_table(doc.root(), "package", b_toml).ok().flatten().and_then(|it| get_str(it, "description", b_toml).ok().flatten())
        });
    expect(description.as_deref() == Some("One line, then the next: \u{e9}."), &format!("a multi-line string's line-ending backslash: {description:?}"));
    expect(row_of(&now, TOTAL) == [7, 1, 8], &format!("the total, each package once: {:?}", row_of(&now, TOTAL)));

    // Planted: a dependency moved from dev to normal is seen in `direct` and `closure`, at its
    // new line, and leaves `dev`.
    let moved_edits = [(a_toml, "mine = \"1\"\n", "mine = \"1\"\nmoved = \"1\"\n"), (a_toml, "[dev-dependencies]\nmoved = \"1\"\n", "[dev-dependencies]\n")];
    let moved = census_of(&moved_edits);
    let moved_a = moved.get("a");
    expect(
        moved_a.is_some_and(|it| it.direct.get(&pkg("moved", "1.0.0")) == Some(&place(a_toml, 12)) && it.dev.is_empty())
            && row_of(&moved, "a") == [5, 0, 8]
            && row_of(&moved, "t") == [1, 0, 9],
        &format!("a dependency moved from dev to normal: {moved_a:?}"),
    );
    // Planted: the rename taken out, the key is no package of the lock and is refused at its line.
    expect(
        refused(&[(t_toml, ", package = \"real-name\"", "")])
            == Some(DepsError::NotInLock { krate: "t".to_owned(), package: "alias".to_owned(), at: place(t_toml, 5) }),
        "a renamed dependency is counted as its package, and the rename is what makes it one",
    );
    // Planted: the root's entry taken out, `workspace = true` has nothing to resolve through.
    expect(
        refused(&[(ROOT, "b = { path = \"crates/b\" }\n", "")]) == Some(DepsError::NoWorkspaceDep { at: place(a_toml, 6), key: "b".to_owned() }),
        "`workspace = true` resolves through the root's table",
    );
    // Planted: the optional dependency made required, the lock's not listing it is refused.
    expect(
        refused(&[(b_toml, "extra = { version = \"2\", optional = true }", "extra = { version = \"2\" }")])
            == Some(DepsError::NotInLock { krate: "b".to_owned(), package: "extra".to_owned(), at: place(b_toml, 10) }),
        "an optional dependency the lock lists not is out of the count, and only an optional one",
    );
    // Planted: the fork under vendor/ that is ours, made upstream's: third-party, and no crate.
    let upstream = census_of(&[("vendor/mine/Cargo.toml", "[package.metadata.blaze]\nours = true\n", "")]);
    expect(
        upstream.get("a").is_some_and(|it| it.direct.get(&pkg("mine", "1.0.0")) == Some(&place(a_toml, 11))) && !upstream.contains_key("mine"),
        "a fork under vendor/ is third-party unless its manifest says it is ours",
    );
    // Planted: the exclusion taken out, the parked crate is read, and refused: the lock has no
    // entry for it.
    expect(
        matches!(refused(&[(ROOT, "exclude = [\"crates/parked\"]\n", "")]), Some(DepsError::CrateNotLocked { krate, .. }) if krate == "parked"),
        "an excluded member is not read",
    );
    expect(
        matches!(refused(&[(ROOT, "\"crates/*\"", "\"crates/a*\"")]), Some(DepsError::Glob { pattern, .. }) if pattern == "crates/a*"),
        "a member pattern other than `<dir>/*` is refused",
    );

    // The ratchet. As recorded, nothing is wrong, and the list reads back as it was written.
    let written = render(&now);
    expect(written.ends_with("a\t4\t1\t7\nb\t2\t0\t2\nt\t1\t0\t8\n(all)\t7\t1\t8\n"), &format!("the list: {written}"));
    let rows = parse_rows(&written).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    expect(problems(&now, &rows, Some(&rows), None).is_empty(), "the tree as recorded is right");
    // Planted: the move from dev to normal, against the list and with the base's census: each
    // count over its row names the new package and the line it came in by.
    let said = problems(&moved, &rows, None, Some(&now));
    let says = |parts: &[&str]| said.iter().any(|it| parts.iter().all(|part| it.contains(part)));
    expect(
        said.len() == 7
            && says(&["`a` has 5 third-party packages in its normal and build dependencies", "over its row's `direct` 4", "new: `moved 1.0.0` at crates/a/Cargo.toml:12", "bottom.md"])
            && says(&["`a` has 0 third-party packages in its dev-dependencies only", "lower its `dev` to 0"])
            && says(&["`a` has 8", "new: `moved 1.0.0` through `moved` at crates/a/Cargo.toml:12"])
            && says(&["`t` has 9", "new: `moved 1.0.0` through `a` at tools/t/Cargo.toml:6"])
            && says(&["the workspace (`(all)`) has 8", "new: `moved 1.0.0` in `a` at crates/a/Cargo.toml:12"]),
        &format!("a dependency moved from dev to normal is caught and named: {said:#?}"),
    );
    expect(over(&moved, &rows) && !over(&now, &rows), "a count over its row is told apart");
    // Planted: a crate with no row is held to 0, its packages named.
    let mut no_b = rows.clone();
    no_b.remove("b");
    let said = problems(&now, &no_b, None, None);
    expect(
        said.len() == 2
            && said[0].contains("`b` has 2")
            && said[0].contains("0, as a crate with no row is held")
            && said[0].contains("`libc 0.2.100` at crates/b/Cargo.toml:9")
            && said[1].contains("give `base`"),
        &format!("a crate with no row starts at 0: {said:?}"),
    );
    // Planted: a crate gone, its row must go; a count down, its row must come down.
    let mut gone = rows.clone();
    gone.insert("old".to_owned(), [1, 0, 0]);
    let said = problems(&now, &gone, None, None);
    expect(said.len() == 1 && said[0].contains("`old`") && said[0].contains("remove the row"), &format!("a crate gone: {said:?}"));
    let mut more = rows.clone();
    more.insert("b".to_owned(), [2, 0, 3]);
    let said = problems(&now, &more, None, None);
    expect(said.len() == 1 && said[0].contains("lower its `closure` to 2"), &format!("a count down: {said:?}"));
    // Planted: a row raised above the base's, and a new crate's row where the base had none.
    let mut lower = rows.clone();
    lower.insert("a".to_owned(), [3, 1, 7]);
    let said = problems(&now, &rows, Some(&lower), None);
    expect(said.len() == 1 && said[0].contains("the row `a`'s `direct` is 4 and was 3 at the base"), &format!("a row raised: {said:?}"));
    let mut no_t = rows.clone();
    no_t.remove("t");
    let said = problems(&now, &rows, Some(&no_t), None);
    expect(
        said.len() == 2 && said.iter().all(|it| it.contains("the row `t`'s") && it.contains("was 0 at the base")),
        &format!("a new crate's row: {said:?}"),
    );
    expect(
        parse_rows("a\t1\t0\t0\nb 2\n") == Err(ListError::NotARow { line: 2, text: "b 2".to_owned() })
            && matches!(parse_rows("a\t1\t0\t0\na\t2\t0\t0\n"), Err(ListError::TwoRows { line: 2, .. }))
            && parse_rows("a\t1\t0\n").is_err()
            && parse_rows("a\t1\t0\tx\n").is_err()
            && parse_rows("\t1\t0\t0\n").is_err(),
        "a bad row is refused, naming its line",
    );
    manifests::selftest();
    reach::selftest();
    println!("{SCRIPT} selftest: ok");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [one] if one == "selftest" => selftest(),
        [one] => main_with(&input::parse::<Args>(one, SCRIPT)),
        _ => {
            eprintln!("usage: {SCRIPT} '<json>' | selftest\n\n{DOC}{}{}", manifests::MANIFESTS_DOC, reach::REACH_DOC);
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The script's own cases run under `blaze test` and `cargo test` too: one that fails ends
    /// the test process with status 1, which fails the run.
    #[test]
    fn the_selftest_holds() {
        selftest();
    }
}
