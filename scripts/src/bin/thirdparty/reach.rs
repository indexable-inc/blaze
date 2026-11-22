//! The third-party packages the `blaze` binary and the workspace reach, by cargo's own resolve
//! (`reach-check`, `reach-write`, `reach-report`; decision D180 in
//! docs/v2/plan/design/end-state.md). The target is zero, printed on every run. Until it is
//! reached the counts are a ratchet in `tools/ratchets/thirdparty.txt` that only falls; once that
//! file is absent or empty (no pair, both counts 0) the check is a plain refusal of any
//! third-party package anywhere in the graph, with no ratchet at all.
//!
//! The input is cargo's: `cargo metadata --format-version 1 --offline --locked`, written to a
//! file by the gate row (tools/gate.txt) and named by the `metadata` field. `--offline` reads the
//! local registry cache and never the network; `--locked` makes a Cargo.lock that the manifests
//! have outgrown a refusal by cargo naming the lock, never a quiet update. This file only reads
//! the JSON: it starts no cargo itself.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::Value;

use super::{DepsError, Fixture, Tree, expect, get_table, line_of, read_toml};

pub(super) const REACH_DOC: &str = r#"
    command   reach-check   the third-party packages cargo's resolve reaches (input: `metadata`,
                            the path of `cargo metadata --format-version 1 --offline --locked`'s
                            output) held against tools/ratchets/thirdparty.txt: `(reached)` the
                            packages with a `source` (a registry or git) the `blaze` binary
                            reaches by normal and build edges on every platform; `(with tests)`
                            the same from every member with dev edges too; and one line
                            `<our crate><TAB><name>` per crate of ours the binary reaches and
                            third-party package it names directly. A package under vendor/ is
                            third-party until tools/ratchets/vendored.txt names it with why. A
                            pair not in the file is refused with D180's row for the name; a
                            count over the file's is refused; one under it, or a pair gone, asks
                            for `reach-write` (`just lint-write`). Test-only names (named in a
                            member's dev-dependencies and by no reached crate's normal ones) are
                            held the same way in tools/ratchets/thirdparty-tests.txt, a line
                            `<name><TAB><crate><TAB><why>`, which only a person adds to. The
                            target, 0, is printed on every run. When thirdparty.txt is absent or
                            holds no pair and two zero counts, every third-party package in the
                            graph, a `[patch]` entry with no `path`, a `[source]` with a
                            `registry` or `git`, and a root `[workspace.dependencies]` entry
                            with no `path` are refused by name
              reach-write   lower both ratchet files to the tree; refuses what a write would raise
              reach-report  D180's table: each name, the crates naming it, its closure (the
                            third-party packages it reaches, itself included) and its exclusive
                            closure (those the binary reaches only through it)
"#;

/// The ratchet, the test-only list and the vendored list, from the root.
pub(super) const RATCHET: &str = "tools/ratchets/thirdparty.txt";
pub(super) const TESTS: &str = "tools/ratchets/thirdparty-tests.txt";
pub(super) const VENDORED: &str = "tools/ratchets/vendored.txt";
/// Where `[source]` tables live.
const CONFIG: &str = ".cargo/config.toml";
const ROOT: &str = "Cargo.toml";
/// The bin target whose package is the root of the binary's reach.
const BINARY: &str = "blaze";
/// The ratchet's two count lines.
const REACHED: &str = "(reached)";
const WITH_TESTS: &str = "(with tests)";
/// What every refusal ends with.
const RULE: &str = "docs/v2/plan/design/end-state.md, D180: no package from a registry; the count only falls, to 0";

/// How each name D180 lists goes, from its table (2026-11-20). windows-sys: the owner's word of
/// 2026-11-20 replaces D180's row: blaze-sys gains its own Windows surface.
const HOW: [(&str, &str); 15] = [
    ("wasmtime", "D175: compile-time code runs natively; deleted at the switch"),
    ("sonic-rs", "the legacy report goes; JSON out by blaze's own writer"),
    ("quinn-proto", "LAST: blaze's own TLS 1.3 and HTTP client; until then vendored and pinned"),
    ("rustls", "LAST: blaze's own TLS 1.3 and HTTP client; until then vendored and pinned"),
    ("bytes", "LAST: blaze's own TLS 1.3 and HTTP client; until then vendored and pinned"),
    ("ring", "the remote bucket is parked (goal revision 2026-11-17): the crate leaves the binary"),
    ("stacker", "stdx is deleted (D172); deep recursion is a frame with a stated bound"),
    ("url", "the cargo layer is parked (D172); manifests are read by blaze-toml and typed paths"),
    ("serde_json", "the cargo layer is parked (D172); manifests are read by blaze-toml and typed paths"),
    ("imbl", "the cargo layer is parked (D172); manifests are read by blaze-toml and typed paths"),
    ("rustc-stable-hash", "the cargo layer is parked (D172); manifests are read by blaze-toml and typed paths"),
    ("windows-sys", "blaze-sys gains its own Windows surface (the owner, 2026-11-20); the crates call it and the package goes"),
    ("serde", "records are blaze-format and CBOR; the derives go with blaze-serde"),
    ("libc", "blaze-sys is the one home of kernel calls (D170)"),
    ("tracing", "spans are bstd's (D178)"),
];

/// What a name not in [`HOW`] is told.
const NEW_NAME: &str = "no row of D180: a third-party name not in its table is new, and D180 refuses a new name";

/// D180's row for `name`.
fn how_of(name: &str) -> &'static str {
    HOW.iter().find(|(it, _)| *it == name).map_or(NEW_NAME, |(_, how)| how)
}

// ---------------------------------------------------------------------------------------------
// Cargo's resolve, read from its JSON.

/// One package of the metadata.
#[derive(Debug, Clone)]
struct Pkg {
    name: String,
    version: String,
    /// Its `source`: a registry or git; `None` for a path package.
    source: Option<String>,
    /// A path package whose manifest is under a `/vendor/` directory.
    vendor: bool,
    member: bool,
}

/// The kinds of one edge.
#[derive(Debug, Clone, Copy, Default)]
struct Kinds {
    /// A normal or a build dependency, on any target.
    built: bool,
    dev: bool,
}

/// The resolve: packages by id and each one's edges.
#[derive(Debug)]
pub(super) struct Graph {
    pkgs: BTreeMap<String, Pkg>,
    edges: BTreeMap<String, Vec<(String, Kinds)>>,
    /// The package of the `blaze` bin target.
    binary: String,
}

/// Why the metadata or a list is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReachError {
    /// The metadata file could not be read.
    Unread { path: String, why: String },
    /// It is not JSON.
    NotJson { path: String, why: String },
    /// A field is missing or not of its shape.
    Shape { path: String, what: &'static str, at: String },
    /// An edge or a member names an id no package has.
    UnknownId { path: String, id: String },
    /// No package, or two, has the `blaze` bin target.
    Binary { path: String, found: usize },
    /// A line of a list is not of its form.
    Line { file: &'static str, line: usize, text: String, what: &'static str },
    /// A manifest or config did not read.
    Deps(DepsError),
}

impl fmt::Display for ReachError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReachError::Unread { path, why } => write!(f, "the metadata {path} could not be read: {why}"),
            ReachError::NotJson { path, why } => write!(f, "the metadata {path} is not JSON: {why}"),
            ReachError::Shape { path, what, at } => {
                write!(f, "the metadata {path}: {at} has no {what}: it is not `cargo metadata --format-version 1`'s output (the resolve is needed: no `--no-deps`)")
            }
            ReachError::UnknownId { path, id } => write!(f, "the metadata {path}: the id {id:?} is no package's"),
            ReachError::Binary { path, found } => {
                write!(f, "the metadata {path}: {found} packages have a bin target `{BINARY}`, not one (blaze-cli's)")
            }
            ReachError::Line { file, line, text, what } => write!(f, "{file}:{line}: {text:?} is {what}"),
            ReachError::Deps(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ReachError {}

fn field<'v>(value: &'v Value, key: &str, path: &str, what: &'static str, at: &str) -> Result<&'v Value, ReachError> {
    value.get(key).ok_or_else(|| ReachError::Shape { path: path.to_owned(), what, at: at.to_owned() })
}

fn string<'v>(value: &'v Value, key: &str, path: &str, what: &'static str, at: &str) -> Result<&'v str, ReachError> {
    field(value, key, path, what, at)?.as_str().ok_or_else(|| ReachError::Shape { path: path.to_owned(), what, at: at.to_owned() })
}

fn array<'v>(value: &'v Value, key: &str, path: &str, what: &'static str, at: &str) -> Result<&'v Vec<Value>, ReachError> {
    field(value, key, path, what, at)?.as_array().ok_or_else(|| ReachError::Shape { path: path.to_owned(), what, at: at.to_owned() })
}

/// The resolve in `text`, the metadata at `path` (named in errors).
pub(super) fn graph(path: &str, text: &str) -> Result<Graph, ReachError> {
    let doc: Value = serde_json::from_str(text).map_err(|e| ReachError::NotJson { path: path.to_owned(), why: e.to_string() })?;
    let mut pkgs = BTreeMap::new();
    let mut binaries = Vec::new();
    for pkg in array(&doc, "packages", path, "`packages` array", "the top level")? {
        let id = string(pkg, "id", path, "string `id`", "a package")?.to_owned();
        let name = string(pkg, "name", path, "string `name`", &id)?.to_owned();
        let version = string(pkg, "version", path, "string `version`", &id)?.to_owned();
        let source = match field(pkg, "source", path, "`source`", &id)? {
            Value::Null => None,
            Value::String(it) => Some(it.clone()),
            _ => return Err(ReachError::Shape { path: path.to_owned(), what: "`source` string or null", at: id }),
        };
        let manifest = string(pkg, "manifest_path", path, "string `manifest_path`", &id)?.replace('\\', "/");
        let vendor = source.is_none() && manifest.contains("/vendor/");
        for target in array(pkg, "targets", path, "`targets` array", &id)? {
            let kinds = array(target, "kind", path, "`kind` array", &id)?;
            if kinds.iter().any(|it| it.as_str() == Some("bin")) && string(target, "name", path, "string target `name`", &id)? == BINARY {
                binaries.push(id.clone());
            }
        }
        pkgs.insert(id, Pkg { name, version, source, vendor, member: false });
    }
    for member in array(&doc, "workspace_members", path, "`workspace_members` array", "the top level")? {
        let id = member.as_str().ok_or_else(|| ReachError::Shape { path: path.to_owned(), what: "string id", at: "`workspace_members`".to_owned() })?;
        let pkg = pkgs.get_mut(id).ok_or_else(|| ReachError::UnknownId { path: path.to_owned(), id: id.to_owned() })?;
        pkg.member = true;
    }
    let resolve = field(&doc, "resolve", path, "`resolve`", "the top level")?;
    let mut edges: BTreeMap<String, Vec<(String, Kinds)>> = BTreeMap::new();
    for node in array(resolve, "nodes", path, "`nodes` array", "`resolve`")? {
        let id = string(node, "id", path, "string `id`", "a node")?;
        if !pkgs.contains_key(id) {
            return Err(ReachError::UnknownId { path: path.to_owned(), id: id.to_owned() });
        }
        let mut out = Vec::new();
        for dep in array(node, "deps", path, "`deps` array", id)? {
            let to = string(dep, "pkg", path, "string `pkg`", id)?;
            if !pkgs.contains_key(to) {
                return Err(ReachError::UnknownId { path: path.to_owned(), id: to.to_owned() });
            }
            let mut kinds = Kinds::default();
            for kind in array(dep, "dep_kinds", path, "`dep_kinds` array", id)? {
                match field(kind, "kind", path, "`kind`", id)? {
                    Value::Null => kinds.built = true,
                    Value::String(it) if it.as_str() == "build" => kinds.built = true,
                    Value::String(it) if it.as_str() == "dev" => kinds.dev = true,
                    _ => return Err(ReachError::Shape { path: path.to_owned(), what: "`kind` of null, \"build\" or \"dev\"", at: id.to_owned() }),
                }
            }
            out.push((to.to_owned(), kinds));
        }
        edges.insert(id.to_owned(), out);
    }
    match binaries.as_slice() {
        [one] => Ok(Graph { binary: one.clone(), pkgs, edges }),
        _ => Err(ReachError::Binary { path: path.to_owned(), found: binaries.len() }),
    }
}

// ---------------------------------------------------------------------------------------------
// What is reached.

/// The tree's numbers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Reach {
    /// Packages with a source the binary reaches by normal and build edges.
    reached: usize,
    /// The same from every member, dev edges too.
    with_tests: usize,
    /// `(our crate, third-party name)`: what each crate of ours the binary reaches names directly.
    pairs: BTreeSet<(String, String)>,
    /// `(name, member)`: third-party names a member names in dev-dependencies and no pair names.
    tests: BTreeSet<(String, String)>,
    /// Our crates the binary reaches, and the vendored packages it reaches.
    ours: usize,
    vendored: usize,
}

impl Graph {
    /// Whether `pkg` counts as third-party: from a source, or under vendor/ and not in `kept`.
    fn third(&self, pkg: &Pkg, kept: &BTreeSet<String>) -> bool {
        pkg.source.is_some() || (pkg.vendor && !kept.contains(&pkg.name))
    }

    /// The ids reached from `starts` (themselves included) over built edges, and dev edges when
    /// `dev`, never entering a package `skip` says.
    fn walk<'g>(&'g self, starts: impl Iterator<Item = &'g str>, dev: bool, skip: &dyn Fn(&Pkg) -> bool) -> BTreeSet<&'g str> {
        let mut seen = BTreeSet::new();
        let mut todo: Vec<&str> = starts.filter(|id| self.pkgs.get(*id).is_some_and(|p| !skip(p))).collect();
        while let Some(id) = todo.pop() {
            if !seen.insert(id) {
                continue;
            }
            for (to, kinds) in self.edges.get(id).into_iter().flatten() {
                let pkg = &self.pkgs[to];
                if (kinds.built || (dev && kinds.dev)) && !skip(pkg) && !seen.contains(to.as_str()) {
                    todo.push(to);
                }
            }
        }
        seen
    }

    fn members(&self) -> impl Iterator<Item = &str> {
        self.pkgs.iter().filter(|(_, p)| p.member).map(|(id, _)| id.as_str())
    }

    /// The numbers, with `kept` the vendored packages vendored.txt names.
    fn reach(&self, kept: &BTreeSet<String>) -> Reach {
        let none = |_: &Pkg| false;
        let from_binary = self.walk([self.binary.as_str()].into_iter(), false, &none);
        let all = self.walk(self.members(), true, &none);
        let sourced = |ids: &BTreeSet<&str>| ids.iter().filter(|id| self.pkgs[**id].source.is_some()).count();
        let mut pairs = BTreeSet::new();
        for id in &from_binary {
            let pkg = &self.pkgs[*id];
            if self.third(pkg, kept) || pkg.vendor {
                continue;
            }
            for (to, kinds) in self.edges.get(*id).into_iter().flatten() {
                let dep = &self.pkgs[to];
                if kinds.built && self.third(dep, kept) {
                    pairs.insert((pkg.name.clone(), dep.name.clone()));
                }
            }
        }
        let named: BTreeSet<&str> = pairs.iter().map(|(_, name)| name.as_str()).collect();
        let mut tests = BTreeSet::new();
        for id in self.members() {
            let pkg = &self.pkgs[id];
            for (to, kinds) in self.edges.get(id).into_iter().flatten() {
                let dep = &self.pkgs[to];
                if kinds.dev && self.third(dep, kept) && !named.contains(dep.name.as_str()) {
                    tests.insert((dep.name.clone(), pkg.name.clone()));
                }
            }
        }
        Reach {
            reached: sourced(&from_binary),
            with_tests: sourced(&all),
            pairs,
            tests,
            ours: from_binary.iter().filter(|id| { let p = &self.pkgs[**id]; p.source.is_none() && !p.vendor }).count(),
            vendored: from_binary.iter().filter(|id| self.pkgs[**id].vendor).count(),
        }
    }

    /// Per name the reached crates name: the crates, the closure and the exclusive closure.
    fn table(&self, kept: &BTreeSet<String>, now: &Reach) -> Vec<(String, Vec<String>, usize, usize)> {
        let none = |_: &Pkg| false;
        let mut by: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for (krate, name) in &now.pairs {
            by.entry(name.as_str()).or_default().push(krate.clone());
        }
        let from_binary = self.walk([self.binary.as_str()].into_iter(), false, &none);
        let mut out = Vec::new();
        for (name, crates) in by {
            let of_name = |p: &Pkg| p.name == name;
            let starts = from_binary.iter().copied().filter(|id| of_name(&self.pkgs[*id]));
            let closure: BTreeSet<&str> = self.walk(starts, false, &none).into_iter().filter(|id| self.third(&self.pkgs[*id], kept)).collect();
            let without = self.walk([self.binary.as_str()].into_iter(), false, &of_name);
            let exclusive = closure.iter().filter(|id| !without.contains(**id)).count();
            out.push((name.to_owned(), crates, closure.len(), exclusive));
        }
        out
    }

    /// Every third-party package in the graph, with the crates of ours naming it: the end
    /// state's refusals.
    fn every_third(&self, kept: &BTreeSet<String>) -> Vec<Finding> {
        let mut out = Vec::new();
        for (id, pkg) in &self.pkgs {
            if !self.third(pkg, kept) {
                continue;
            }
            let by: BTreeSet<String> = self
                .edges
                .iter()
                .filter(|(from, deps)| !self.third(&self.pkgs[*from], kept) && !self.pkgs[*from].vendor && deps.iter().any(|(to, _)| to == id))
                .map(|(from, _)| self.pkgs[from].name.clone())
                .collect();
            let from = pkg.source.clone().unwrap_or_else(|| "vendor/, not in tools/ratchets/vendored.txt".to_owned());
            out.push(Finding::ThirdParty { name: pkg.name.clone(), version: pkg.version.clone(), from, by: by.into_iter().collect() });
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// The lists.

/// thirdparty.txt as read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Held {
    reached: usize,
    with_tests: usize,
    pairs: BTreeSet<(String, String)>,
}

impl Held {
    /// The end state: nothing held.
    fn is_zero(&self) -> bool {
        self.reached == 0 && self.with_tests == 0 && self.pairs.is_empty()
    }
}

/// The lines of `text` that are not comments or blank, with their line numbers, split at tabs.
fn rows(text: &str) -> impl Iterator<Item = (usize, &str, Vec<&str>)> {
    text.split('\n').enumerate().filter(|(_, l)| !l.is_empty() && !l.starts_with('#')).map(|(at, l)| (at + 1, l, l.split('\t').collect()))
}

pub(super) fn parse_held(text: &str) -> Result<Held, ReachError> {
    let bad = |line, text: &str, what| ReachError::Line { file: RATCHET, line, text: text.to_owned(), what };
    let (mut reached, mut with_tests) = (None, None);
    let mut pairs = BTreeSet::new();
    for (line, raw, fields) in rows(text) {
        let [first, second] = fields.as_slice() else { return Err(bad(line, raw, "not two fields split by a tab")) };
        if first.starts_with('(') {
            let n: usize = second.parse().map_err(|_| bad(line, raw, "a count line whose count is no whole number"))?;
            let slot = match *first {
                REACHED => &mut reached,
                WITH_TESTS => &mut with_tests,
                _ => return Err(bad(line, raw, "a count line that is neither `(reached)` nor `(with tests)`")),
            };
            if slot.replace(n).is_some() {
                return Err(bad(line, raw, "a second line of its count"));
            }
        } else if first.is_empty() || second.is_empty() || !pairs.insert(((*first).to_owned(), (*second).to_owned())) {
            return Err(bad(line, raw, "an empty or repeated pair"));
        }
    }
    let empty = pairs.is_empty() && reached.is_none() && with_tests.is_none();
    match (reached, with_tests) {
        (Some(reached), Some(with_tests)) => Ok(Held { reached, with_tests, pairs }),
        _ if empty => Ok(Held::default()),
        _ => Err(bad(0, "", "a list without both count lines (`(reached)` and `(with tests)`)")),
    }
}

/// thirdparty-tests.txt: `(name, crate)` to why.
pub(super) fn parse_tests(text: &str) -> Result<BTreeMap<(String, String), String>, ReachError> {
    let mut out = BTreeMap::new();
    for (line, raw, fields) in rows(text) {
        let [name, krate, why] = fields.as_slice() else {
            return Err(ReachError::Line { file: TESTS, line, text: raw.to_owned(), what: "not `<name><TAB><crate><TAB><why>`" });
        };
        if name.is_empty() || krate.is_empty() || why.trim().is_empty() || out.insert(((*name).to_owned(), (*krate).to_owned()), (*why).to_owned()).is_some() {
            return Err(ReachError::Line { file: TESTS, line, text: raw.to_owned(), what: "a line with an empty field or a repeated pair" });
        }
    }
    Ok(out)
}

/// vendored.txt: a package under vendor/ to why it is still there.
pub(super) fn parse_vendored(text: &str) -> Result<BTreeMap<String, String>, ReachError> {
    let mut out = BTreeMap::new();
    for (line, raw, fields) in rows(text) {
        let [name, why] = fields.as_slice() else {
            return Err(ReachError::Line { file: VENDORED, line, text: raw.to_owned(), what: "not `<package><TAB><why>`" });
        };
        if name.is_empty() || why.trim().is_empty() || out.insert((*name).to_owned(), (*why).to_owned()).is_some() {
            return Err(ReachError::Line { file: VENDORED, line, text: raw.to_owned(), what: "a line with an empty field or a repeated package" });
        }
    }
    Ok(out)
}

const HELD_HEADER: &str = "# The third-party packages the `blaze` binary reaches (docs/v2/plan/design/end-state.md, D180).\n\
                           # Target 0. `(reached)`: packages with a source (registry or git) reached from blaze-cli's\n\
                           # `blaze` by normal and build edges, every platform; `(with tests)`: from every member, dev\n\
                           # edges too; then `<our crate><TAB><third-party name>` for each name a reached crate of ours\n\
                           # names directly (a package under vendor/ is third-party unless tools/ratchets/vendored.txt\n\
                           # names it). Only falls: `just lint-write` lowers it; a new pair or a higher count is refused.\n\
                           # Empty or absent, the gate refuses every third-party package outright. Checked by\n\
                           # scripts/src/bin/thirdparty.rs (`reach-check`), a row of tools/gate.txt.\n";

const TESTS_HEADER: &str = "# The third-party packages only tests, benches and examples name, each with why (D180, \"Test-only\n\
                            # packages\"): `<name><TAB><member><TAB><why>`. A name here is in a member's dev-dependencies and\n\
                            # named by no crate the binary reaches. Only falls: a person adds a line, with its reason; a\n\
                            # comparison that has done its job is deleted with its package. Checked by `reach-check`.\n";

fn render_held(now: &Reach) -> String {
    let mut out = format!("{HELD_HEADER}{REACHED}\t{}\n{WITH_TESTS}\t{}\n", now.reached, now.with_tests);
    for (krate, name) in &now.pairs {
        out.push_str(&format!("{krate}\t{name}\n"));
    }
    out
}

fn render_tests(now: &Reach, why: &BTreeMap<(String, String), String>) -> String {
    let mut out = TESTS_HEADER.to_owned();
    for pair in &now.tests {
        let reason = why.get(pair).map_or("", String::as_str);
        out.push_str(&format!("{}\t{}\t{reason}\n", pair.0, pair.1));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The verdict.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Finding {
    /// A pair the ratchet does not hold.
    NewPair { krate: String, name: String },
    /// A count over the ratchet's.
    Rose { what: &'static str, now: usize, held: usize },
    /// A count under it: lower it.
    Fell { what: &'static str, now: usize, held: usize },
    /// A held pair the tree no longer has: remove it.
    PairGone { krate: String, name: String },
    /// A test-only name the list does not hold: a person adds it with why.
    NewTest { name: String, krate: String },
    /// A test-only line the tree no longer has.
    TestGone { name: String, krate: String },
    /// A vendored.txt line naming no package under vendor/ in the graph.
    VendoredGone { name: String },
    /// The end state: a third-party package anywhere.
    ThirdParty { name: String, version: String, from: String, by: Vec<String> },
    /// The end state: a fetched `[patch]`, `[source]` or root entry.
    Fetched { file: &'static str, line: usize, key: String, what: &'static str },
}

impl Finding {
    /// Whether a write cannot settle it.
    fn raises(&self) -> bool {
        !matches!(self, Finding::Fell { .. } | Finding::PairGone { .. } | Finding::TestGone { .. } | Finding::VendoredGone { .. })
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Finding::NewPair { krate, name } => {
                write!(f, "`{krate}` names the third-party package `{name}`, a pair {RATCHET} does not hold; D180 for `{name}`: {} ({RULE})", how_of(name))
            }
            Finding::Rose { what, now, held } => write!(f, "{what} is {now}, over the {held} {RATCHET} holds ({RULE})"),
            Finding::Fell { what, now, held } => write!(f, "{what} is {now}, under the {held} {RATCHET} holds: lower it (`just lint-write`); it only falls"),
            Finding::PairGone { krate, name } => write!(f, "`{krate}` no longer names `{name}`: remove the pair from {RATCHET} (`just lint-write`)"),
            Finding::NewTest { name, krate } => write!(
                f,
                "`{krate}`'s dev-dependencies name the third-party package `{name}`, which {TESTS} does not hold: a test-only package is added by a person, with why it is there ({RULE})"
            ),
            Finding::TestGone { name, krate } => write!(f, "`{krate}` no longer names `{name}` in its dev-dependencies: remove its line from {TESTS} (`just lint-write`)"),
            Finding::VendoredGone { name } => write!(f, "{VENDORED} names `{name}`, which is no package under vendor/ in the graph: remove its line"),
            Finding::ThirdParty { name, version, from, by } => {
                let by = if by.is_empty() { "named by no crate of ours: reached through another package".to_owned() } else { format!("named by {}", by.join(", ")) };
                write!(f, "the third-party package `{name}` {version} ({from}) is in the graph, {by}; D180 for it: {} ({RULE})", how_of(name))
            }
            Finding::Fetched { file, line, key, what } => write!(f, "{file}:{line}: `{key}` {what} ({RULE})"),
        }
    }
}

/// What a run decides: the numbers, and what is wrong.
pub(super) struct Verdict {
    now: Reach,
    findings: Vec<Finding>,
    /// Whether the end state's branch was taken (the ratchet absent or empty).
    end_state: bool,
    tests_why: BTreeMap<(String, String), String>,
    held_text: Option<String>,
    tests_text: Option<String>,
}

/// The end state's reading of the root manifest and the cargo config: a `[patch]` entry with
/// no `path`, a `[source]` with a `registry` or `git`, a root dependency with no `path`.
fn fetched(tree: &dyn Tree) -> Result<Vec<Finding>, ReachError> {
    let mut out = Vec::new();
    if let Some(text) = tree.read(ROOT).map_err(ReachError::Deps)? {
        let doc = read_toml(ROOT, &text).map_err(ReachError::Deps)?;
        let root = doc.root();
        if let Some(patch) = get_table(root, "patch", ROOT).map_err(ReachError::Deps)? {
            for source in patch.children() {
                for entry in source.children() {
                    if !entry.is_table() || entry.get("path").is_none() {
                        let key = format!("patch.{}.{}", source.key(), entry.key());
                        out.push(Finding::Fetched { file: ROOT, line: line_of(entry), key, what: "is a patch with no `path`: it is fetched" });
                    }
                }
            }
        }
        let deps = get_table(root, "workspace", ROOT).map_err(ReachError::Deps)?.map(|ws| get_table(ws, "dependencies", ROOT)).transpose().map_err(ReachError::Deps)?.flatten();
        for entry in deps.into_iter().flat_map(|it| it.children()) {
            if !entry.is_table() || entry.get("path").is_none() {
                let key = format!("workspace.dependencies.{}", entry.key());
                out.push(Finding::Fetched { file: ROOT, line: line_of(entry), key, what: "has a version requirement and no `path`: a package from a registry or git" });
            }
        }
    }
    if let Some(text) = tree.read(CONFIG).map_err(ReachError::Deps)? {
        let doc = read_toml(CONFIG, &text).map_err(ReachError::Deps)?;
        if let Some(sources) = get_table(doc.root(), "source", CONFIG).map_err(ReachError::Deps)? {
            for source in sources.children() {
                for key in ["registry", "git"] {
                    if let Some(item) = source.get(key) {
                        out.push(Finding::Fetched { file: CONFIG, line: line_of(item), key: format!("source.{}.{key}", source.key()), what: "points a source at a registry or git" });
                    }
                }
            }
        }
    }
    Ok(out)
}

/// The verdict on the graph `g` with the lists of `tree`.
pub(super) fn judge(g: &Graph, tree: &dyn Tree) -> Result<Verdict, ReachError> {
    let vendored = match tree.read(VENDORED).map_err(ReachError::Deps)? {
        Some(text) => parse_vendored(&text)?,
        None => BTreeMap::new(),
    };
    let kept: BTreeSet<String> = vendored.keys().cloned().collect();
    let now = g.reach(&kept);
    let mut findings = Vec::new();
    for name in &kept {
        if !g.pkgs.values().any(|p| p.vendor && p.name == *name) {
            findings.push(Finding::VendoredGone { name: name.clone() });
        }
    }
    let held_text = tree.read(RATCHET).map_err(ReachError::Deps)?;
    let held = match &held_text {
        Some(text) => parse_held(text)?,
        None => Held::default(),
    };
    let tests_text = tree.read(TESTS).map_err(ReachError::Deps)?;
    let tests_why = match &tests_text {
        Some(text) => parse_tests(text)?,
        None => BTreeMap::new(),
    };
    let end_state = held.is_zero();
    if end_state {
        findings.extend(g.every_third(&kept));
        findings.extend(fetched(tree)?);
    } else {
        for (krate, name) in now.pairs.difference(&held.pairs) {
            findings.push(Finding::NewPair { krate: krate.clone(), name: name.clone() });
        }
        for (krate, name) in held.pairs.difference(&now.pairs) {
            findings.push(Finding::PairGone { krate: krate.clone(), name: name.clone() });
        }
        for (what, n, h) in [(REACHED, now.reached, held.reached), (WITH_TESTS, now.with_tests, held.with_tests)] {
            if n > h {
                findings.push(Finding::Rose { what, now: n, held: h });
            } else if n < h {
                findings.push(Finding::Fell { what, now: n, held: h });
            }
        }
    }
    for pair in &now.tests {
        if !tests_why.contains_key(pair) {
            findings.push(Finding::NewTest { name: pair.0.clone(), krate: pair.1.clone() });
        }
    }
    for pair in tests_why.keys() {
        if !now.tests.contains(pair) {
            findings.push(Finding::TestGone { name: pair.0.clone(), krate: pair.1.clone() });
        }
    }
    Ok(Verdict { now, findings, end_state, tests_why, held_text, tests_text })
}

/// The line every run prints: where the count is, and its target.
fn status(now: &Reach) -> String {
    let names: BTreeSet<&str> = now.pairs.iter().map(|(_, n)| n.as_str()).collect();
    let crates: BTreeSet<&str> = now.pairs.iter().map(|(c, _)| c.as_str()).collect();
    format!(
        "third party: {} names in {} crates, {} packages reached ({} with tests; {} crates of ours, {} vendored); target 0",
        names.len(),
        crates.len(),
        now.reached,
        now.with_tests,
        now.ours,
        now.vendored
    )
}

/// The report: D180's table.
fn report(g: &Graph, tree: &dyn Tree, now: &Reach) -> Result<String, ReachError> {
    let kept: BTreeSet<String> = match tree.read(VENDORED).map_err(ReachError::Deps)? {
        Some(text) => parse_vendored(&text)?.into_keys().collect(),
        None => BTreeSet::new(),
    };
    let mut out = format!("{:<20} {:>8} {:>10}  by\n", "named directly", "closure", "exclusive");
    for (name, crates, closure, exclusive) in g.table(&kept, now) {
        out.push_str(&format!("{name:<20} {closure:>8} {exclusive:>10}  {}\n", crates.join(", ")));
    }
    out.push_str(&format!("{}\n", status(now)));
    Ok(out)
}

/// What the command `reach-<what>` does at `root` with the metadata at `metadata`.
pub(super) fn run(tree: &dyn Tree, root: &std::path::Path, what: &str, metadata: Option<&str>) -> Result<(), String> {
    let Some(path) = metadata else {
        return Err("`reach-*` takes `metadata`: the path of `cargo metadata --format-version 1 --offline --locked`'s output (tools/gate.txt's row writes it)".to_owned());
    };
    let text = std::fs::read_to_string(path).map_err(|e| ReachError::Unread { path: path.to_owned(), why: e.to_string() }.to_string())?;
    let g = graph(path, &text).map_err(|e| e.to_string())?;
    let verdict = judge(&g, tree).map_err(|e| e.to_string())?;
    println!("thirdparty: {}", status(&verdict.now));
    match what {
        "report" => {
            print!("{}", report(&g, tree, &verdict.now).map_err(|e| e.to_string())?);
            Ok(())
        }
        "write" => {
            let raising: Vec<String> = verdict.findings.iter().filter(|f| f.raises()).map(|f| format!("thirdparty: {f}")).collect();
            if !raising.is_empty() {
                return Err(format!("{}\nthirdparty: a write only lowers the lists; {} findings above it cannot", raising.join("\n"), raising.len()));
            }
            if !verdict.end_state {
                std::fs::write(root.join(RATCHET), render_held(&verdict.now)).map_err(|e| format!("{RATCHET} is not written: {e}"))?;
            }
            std::fs::write(root.join(TESTS), render_tests(&verdict.now, &verdict.tests_why)).map_err(|e| format!("{TESTS} is not written: {e}"))?;
            println!("thirdparty: wrote {TESTS}{}", if verdict.end_state { "" } else { " and the ratchet" });
            Ok(())
        }
        _ => {
            let mut wrong: Vec<String> = verdict.findings.iter().map(|f| format!("thirdparty: {f}")).collect();
            if wrong.is_empty() && !verdict.end_state && verdict.held_text.as_deref() != Some(render_held(&verdict.now).as_str()) {
                wrong.push(format!("thirdparty: {RATCHET} is not as `reach-write` writes it (its header, the two counts, the pairs sorted): run `just lint-write`"));
            }
            if wrong.is_empty() && verdict.tests_text.as_deref().unwrap_or(TESTS_HEADER) != render_tests(&verdict.now, &verdict.tests_why) {
                wrong.push(format!("thirdparty: {TESTS} is not as `reach-write` writes it (its header, the lines sorted): run `just lint-write`"));
            }
            if !wrong.is_empty() {
                return Err(format!("{}\nthirdparty: {} third-party findings", wrong.join("\n"), wrong.len()));
            }
            println!("thirdparty: reach holds{}", if verdict.end_state { ": no third-party package anywhere" } else { " at the ratchet" });
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The selftest.

const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";

/// The selftest's packages: `(name, source, manifest dir, member, has the bin target)`.
const PKGS: [(&str, Option<&str>, &str, bool, bool); 7] = [
    ("blaze-cli", None, "/r/crates/blaze-cli", true, true),
    ("blaze-a", None, "/r/crates/blaze-a", true, false),
    ("blaze-t", None, "/r/crates/blaze-t", true, false),
    ("vend", None, "/r/vendor/vend", false, false),
    ("reg-x", Some(CRATES_IO), "/home/.cargo/registry/src/reg-x-1.0.0", false, false),
    ("reg-y", Some(CRATES_IO), "/home/.cargo/registry/src/reg-y-1.0.0", false, false),
    ("reg-z", Some(CRATES_IO), "/home/.cargo/registry/src/reg-z-1.0.0", false, false),
];

/// The base graph's edges: `(from, to, kind)`, kind `None` normal.
const EDGES: [(&str, &str, Option<&str>); 7] = [
    ("blaze-cli", "blaze-a", None),
    ("blaze-cli", "vend", None),
    ("blaze-a", "reg-x", None),
    ("vend", "reg-y", Some("build")),
    ("reg-x", "reg-y", None),
    ("blaze-a", "reg-z", Some("dev")),
    ("blaze-t", "reg-z", Some("dev")),
];

fn metadata(edges: &[(&str, &str, Option<&str>)]) -> String {
    let id = |name: &str| format!("{name}@1.0.0");
    let used = |name: &str, member: bool| member || edges.iter().any(|(from, to, _)| *from == name || *to == name);
    let packages: Vec<Value> = PKGS
        .iter()
        .filter(|(name, _, _, member, _)| used(name, *member))
        .map(|(name, source, dir, _, bin)| {
            let targets = if *bin { serde_json::json!([{"kind": ["bin"], "name": BINARY}]) } else { serde_json::json!([{"kind": ["lib"], "name": name}]) };
            serde_json::json!({"id": id(name), "name": name, "version": "1.0.0", "source": source, "manifest_path": format!("{dir}/Cargo.toml"), "targets": targets})
        })
        .collect();
    let members: Vec<String> = PKGS.iter().filter(|it| it.3).map(|it| id(it.0)).collect();
    let nodes: Vec<Value> = PKGS
        .iter()
        .filter(|(name, _, _, member, _)| used(name, *member))
        .map(|(name, ..)| {
            let deps: Vec<Value> = edges
                .iter()
                .filter(|(from, ..)| from == name)
                .map(|(_, to, kind)| serde_json::json!({"name": to, "pkg": id(to), "dep_kinds": [{"kind": kind, "target": null}]}))
                .collect();
            serde_json::json!({"id": id(name), "deps": deps})
        })
        .collect();
    serde_json::json!({"packages": packages, "workspace_members": members, "resolve": {"nodes": nodes, "root": null}}).to_string()
}

fn lists(held: &str, tests: &str, vendored: Option<&str>) -> Fixture {
    let mut files = BTreeMap::new();
    files.insert(RATCHET.to_owned(), held.to_owned());
    files.insert(TESTS.to_owned(), tests.to_owned());
    if let Some(v) = vendored {
        files.insert(VENDORED.to_owned(), v.to_owned());
    }
    Fixture { files }
}

const BASE_HELD: &str = "(reached)\t2\n(with tests)\t3\nblaze-a\treg-x\nblaze-cli\tvend\n";
const BASE_TESTS: &str = "reg-z\tblaze-a\tthe reference\nreg-z\tblaze-t\tthe reference\n";

fn verdict(edges: &[(&str, &str, Option<&str>)], held: &str, tests: &str, vendored: Option<&str>) -> Verdict {
    let g = graph("m.json", &metadata(edges)).unwrap_or_else(|e| super::fail("thirdparty selftest", &e.to_string()));
    judge(&g, &lists(held, tests, vendored)).unwrap_or_else(|e| super::fail("thirdparty selftest", &e.to_string()))
}

pub(super) fn selftest() {
    let base = verdict(&EDGES, BASE_HELD, BASE_TESTS, None);
    expect(base.findings.is_empty(), "the base graph holds at its lists");
    expect(base.now.reached == 2 && base.now.with_tests == 3, "two registry packages reached from the binary, three with dev edges");
    expect(
        base.now.pairs.contains(&("blaze-cli".to_owned(), "vend".to_owned())) && base.now.vendored == 1 && base.now.ours == 2,
        "a path package under vendor/ is third-party: a pair, and not a registry package",
    );
    expect(!base.now.pairs.iter().any(|(_, n)| n == "reg-y"), "what a vendored package reaches is no pair of ours");
    expect(parse_held(&render_held(&base.now)).map(|h| h.pairs) == Ok(base.now.pairs.clone()), "the ratchet round-trips");

    // A dev-only edge: reg-z only through dev-dependencies, so it is in `(with tests)` alone,
    // and a test-only name.
    expect(
        base.now.tests == BTreeSet::from([("reg-z".to_owned(), "blaze-a".to_owned()), ("reg-z".to_owned(), "blaze-t".to_owned())]),
        "a dev-only edge is a test-only name, not a pair",
    );
    let mut more_tests: Vec<(&str, &str, Option<&str>)> = EDGES.to_vec();
    more_tests.push(("blaze-t", "reg-y", Some("dev")));
    let v = verdict(&more_tests, BASE_HELD, BASE_TESTS, None);
    expect(
        v.findings == vec![Finding::NewTest { name: "reg-y".to_owned(), krate: "blaze-t".to_owned() }] && v.findings.iter().all(Finding::raises),
        "a new test-only package is refused until a person adds it with why",
    );
    let fewer: Vec<(&str, &str, Option<&str>)> = EDGES.iter().copied().filter(|e| *e != ("blaze-t", "reg-z", Some("dev"))).collect();
    let v = verdict(&fewer, BASE_HELD, BASE_TESTS, None);
    expect(
        v.findings == vec![Finding::TestGone { name: "reg-z".to_owned(), krate: "blaze-t".to_owned() }] && !v.findings[0].raises(),
        "a test-only line gone asks for a write",
    );

    // A new pair: blaze-a names reg-y.
    let mut new_pair: Vec<(&str, &str, Option<&str>)> = EDGES.to_vec();
    new_pair.push(("blaze-a", "reg-y", None));
    let v = verdict(&new_pair, BASE_HELD, BASE_TESTS, None);
    expect(v.findings == vec![Finding::NewPair { krate: "blaze-a".to_owned(), name: "reg-y".to_owned() }], "a new pair is refused by name");
    expect(v.findings[0].to_string().contains(NEW_NAME) && how_of("libc").contains("blaze-sys"), "a refused pair carries D180's row, or says it has none");

    // A rising count, and a falling one.
    let v = verdict(&EDGES, "(reached)\t1\n(with tests)\t3\nblaze-a\treg-x\nblaze-cli\tvend\n", BASE_TESTS, None);
    expect(v.findings == vec![Finding::Rose { what: REACHED, now: 2, held: 1 }] && v.findings[0].raises(), "a count over the ratchet is refused");
    let v = verdict(&EDGES, "(reached)\t2\n(with tests)\t9\nblaze-a\treg-x\nblaze-cli\tvend\n", BASE_TESTS, None);
    expect(v.findings == vec![Finding::Fell { what: WITH_TESTS, now: 3, held: 9 }] && !v.findings[0].raises(), "a count under the ratchet asks for a write");

    // vendored.txt keeps a vendored package out of the count; a line naming nothing is refused.
    let v = verdict(&EDGES, "(reached)\t2\n(with tests)\t3\nblaze-a\treg-x\n", BASE_TESTS, Some("vend\tthe TLS closure\n"));
    expect(v.findings.is_empty(), "a vendored package vendored.txt names is no pair");
    let v = verdict(&EDGES, BASE_HELD, BASE_TESTS, Some("gone\twhy\n"));
    expect(v.findings == vec![Finding::VendoredGone { name: "gone".to_owned() }], "a vendored.txt line naming no vendored package is refused");

    // The end state: an empty ratchet refuses every third-party package by name.
    let v = verdict(&EDGES, "", BASE_TESTS, None);
    let named: BTreeSet<String> = v.findings.iter().filter_map(|f| if let Finding::ThirdParty { name, .. } = f { Some(name.clone()) } else { None }).collect();
    expect(v.end_state && named == BTreeSet::from(["reg-x", "reg-y", "reg-z", "vend"].map(str::to_owned)), "an empty ratchet refuses every third-party package");
    let clean: Vec<(&str, &str, Option<&str>)> = vec![("blaze-cli", "blaze-a", None)];
    let v = verdict(&clean, "", "", None);
    expect(v.end_state && v.findings.is_empty(), "with no third-party package the end state holds");
    let mut files = BTreeMap::new();
    files.insert(ROOT.to_owned(), "[workspace.dependencies]\nours = { path = \"crates/ours\" }\nfar = \"1.0\"\n[patch.crates-io]\nnear = { path = \"vendor/near\" }\nfetched = { git = \"https://example.invalid/x\" }\n".to_owned());
    files.insert(CONFIG.to_owned(), "[source.crates-io]\nreplace-with = \"mirror\"\n[source.mirror]\nregistry = \"sparse+https://example.invalid/\"\n".to_owned());
    let keys: Vec<String> = fetched(&Fixture { files }).unwrap_or_default().into_iter().filter_map(|f| if let Finding::Fetched { key, .. } = f { Some(key) } else { None }).collect();
    expect(
        keys == ["patch.crates-io.fetched", "workspace.dependencies.far", "source.mirror.registry"],
        "the end state refuses a fetched patch, a root entry with no path and a source at a registry",
    );

    // The closures: reg-x reaches reg-y, which vend reaches too.
    let g = graph("m.json", &metadata(&EDGES)).unwrap_or_else(|e| super::fail("thirdparty selftest", &e.to_string()));
    let table = g.table(&BTreeSet::new(), &base.now);
    expect(
        table == vec![("reg-x".to_owned(), vec!["blaze-a".to_owned()], 2, 1), ("vend".to_owned(), vec!["blaze-cli".to_owned()], 2, 1)],
        "the closure counts what a name reaches; the exclusive closure what nothing else reaches",
    );

    // Refusals of the input.
    expect(matches!(graph("m.json", "{"), Err(ReachError::NotJson { .. })), "metadata that is no JSON is refused");
    expect(
        matches!(graph("m.json", r#"{"packages": [], "workspace_members": []}"#), Err(ReachError::Shape { what: "`resolve`", .. })),
        "metadata without the resolve (`--no-deps`) is refused",
    );
    expect(parse_held("a\tb\tc\n").is_err() && parse_held("(reached)\t1\na\tb\n").is_err() && parse_held("(other)\t1\n").is_err(), "a bad ratchet line is refused");
    expect(parse_tests("a\tb\t \n").is_err() && parse_vendored("a\n").is_err(), "a list line without why is refused");
}
