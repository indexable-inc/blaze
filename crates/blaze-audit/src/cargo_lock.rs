//! A `Cargo.lock`, read for the audit: each package's name, version, source and the packages
//! it depends on, by index. `Cargo.lock` is a file blaze did not write, so its size and its
//! package count are bounded before it is decoded, and every dependency must name a package
//! the file has.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;
use std::fmt;

/// The largest `Cargo.lock` read: 64 MiB. The largest real ones (thousands of packages) are a
/// few MiB; past this is a file no workspace has, refused before TOML decodes it.
pub const MOST_LOCK_BYTES: usize = 64 << 20;

/// The most packages a lock may hold: 2^17, an order past any real workspace's graph.
pub const MOST_PACKAGES: usize = 1 << 17;

/// Where a locked package comes from, as `Cargo.lock` spells it.
#[derive(Clone, PartialEq, Eq, Debug, PartialOrd, Ord, Hash)]
pub enum Source {
    /// `registry+URL` or `sparse+URL`; `checksum` the sha256 hex `Cargo.lock` records.
    Registry { url: String, checksum: Option<String> },
    /// `git+URL?query#commit`: `url` without its query and fragment, `query` the reference
    /// cargo followed (`branch=main`, `rev=…`), `commit` the fragment.
    Git { url: String, query: String, commit: String },
    /// No `source`: a workspace member or a path dependency.
    Path,
}

/// One `[[package]]`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Package {
    pub name: String,
    pub version: semver::Version,
    pub source: Source,
    /// Indices into [`Lock::packages`], sorted by (name, version, source).
    pub deps: Vec<usize, Global>,
}

/// A whole `Cargo.lock`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Lock {
    pub packages: Vec<Package, Global>,
}

/// Why a `Cargo.lock` was not read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LockRefused {
    TooBig { bytes: usize },
    TooMany { packages: usize },
    NotToml(String),
    /// Package `package` (or `[[package]]` number `at` when it has no name) is malformed.
    Bad { package: String, why: String },
    /// `package` depends on `dep`, which names no package of the lock, or more than one.
    Dangling { package: String, dep: String },
}

impl fmt::Display for LockRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockRefused::TooBig { bytes } => {
                write!(f, "Cargo.lock is {bytes} bytes, more than the {MOST_LOCK_BYTES} a lock may be")
            }
            LockRefused::TooMany { packages } => {
                write!(f, "Cargo.lock has {packages} packages, more than the {MOST_PACKAGES} a lock may")
            }
            LockRefused::NotToml(why) => write!(f, "Cargo.lock is not TOML: {why}"),
            LockRefused::Bad { package, why } => write!(f, "Cargo.lock's package `{package}` {why}"),
            LockRefused::Dangling { package, dep } => write!(
                f,
                "Cargo.lock's package `{package}` depends on `{dep}`, which names no one package of the lock"
            ),
        }
    }
}

/// `git+URL?query#commit` taken apart, or `None` when `source` is not a git source.
pub fn git_source(source: &str) -> Option<(String, String, String)> {
    let rest = source.strip_prefix("git+")?;
    let (rest, commit) = rest.split_once('#').unwrap_or((rest, ""));
    let (url, query) = rest.split_once('?').unwrap_or((rest, ""));
    Some((url.to_owned(), query.to_owned(), commit.to_owned()))
}

fn source_of(text: Option<&str>, checksum: Option<String>, name: &str) -> Result<Source, LockRefused> {
    let bad = |why: String| LockRefused::Bad { package: name.to_owned(), why };
    let Some(text) = text else {
        return Ok(Source::Path);
    };
    if let Some((url, query, commit)) = git_source(text) {
        return Ok(Source::Git { url, query, commit });
    }
    for prefix in ["registry+", "sparse+"] {
        if text.starts_with(prefix) {
            return Ok(Source::Registry { url: text.to_owned(), checksum });
        }
    }
    Err(bad(format!("has the source `{text}`, which is no registry, sparse or git source")))
}

impl Lock {
    /// `bytes` read as a `Cargo.lock` (format 3 or 4: dependencies by name, or name and
    /// version, or name, version and source in parentheses, where one alone is ambiguous).
    pub fn parse(bytes: &[u8]) -> Result<Lock, LockRefused> {
        if bytes.len() > MOST_LOCK_BYTES {
            return Err(LockRefused::TooBig { bytes: bytes.len() });
        }
        let text = std::str::from_utf8(bytes).map_err(|it| LockRefused::NotToml(it.to_string()))?;
        let doc: toml::Table = toml::from_str(text).map_err(|it| LockRefused::NotToml(it.to_string()))?;
        let empty = toml::value::Array::new();
        let listed = match doc.get("package") {
            Some(toml::Value::Array(it)) => it,
            Some(_) => return Err(LockRefused::NotToml("`package` is not an array of tables".to_owned())),
            None => &empty,
        };
        if listed.len() > MOST_PACKAGES {
            return Err(LockRefused::TooMany { packages: listed.len() });
        }
        let mut packages: Vec<Package, Global> = Vec::with_capacity_in(listed.len(), Global);
        let mut wanted: Vec<Vec<String, Global>, Global> = Vec::with_capacity_in(listed.len(), Global);
        for (at, item) in listed.iter().enumerate() {
            let table = item
                .as_table()
                .ok_or_else(|| LockRefused::Bad { package: format!("#{at}"), why: "is not a table".to_owned() })?;
            let field = |key: &str| table.get(key).and_then(toml::Value::as_str);
            let name = field("name")
                .ok_or_else(|| LockRefused::Bad { package: format!("#{at}"), why: "has no `name`".to_owned() })?
                .to_owned();
            let bad = |why: String| LockRefused::Bad { package: name.clone(), why };
            let version = field("version").ok_or_else(|| bad("has no `version`".to_owned()))?;
            let version = semver::Version::parse(version)
                .map_err(|why| bad(format!("has the version `{version}`, which is not semver: {why}")))?;
            let source = source_of(field("source"), field("checksum").map(str::to_owned), &name)?;
            let mut deps = Vec::new_in(Global);
            if let Some(list) = table.get("dependencies") {
                let list = list.as_array().ok_or_else(|| bad("has `dependencies` that is not an array".to_owned()))?;
                for dep in list {
                    let dep = dep.as_str().ok_or_else(|| bad("has a dependency that is not a string".to_owned()))?;
                    deps.push(dep.to_owned());
                }
            }
            packages.push(Package { name, version, source, deps: Vec::new_in(Global) });
            wanted.push(deps);
        }
        for (at, deps) in wanted.into_iter().enumerate() {
            let mut found = Vec::with_capacity_in(deps.len(), Global);
            for dep in &deps {
                found.push(Self::find(&packages, &packages[at].name, dep)?);
            }
            found.sort_by(|a, b| {
                let (a, b) = (&packages[*a], &packages[*b]);
                (&a.name, &a.version, &a.source).cmp(&(&b.name, &b.version, &b.source))
            });
            found.dedup();
            packages[at].deps = found;
        }
        Ok(Lock { packages })
    }

    /// The one package `dep` (`name`, `name version` or `name version (source)`) names.
    fn find(packages: &[Package], of: &str, dep: &str) -> Result<usize, LockRefused> {
        let dangling = || LockRefused::Dangling { package: of.to_owned(), dep: dep.to_owned() };
        let mut parts = dep.splitn(3, ' ');
        let name = parts.next().unwrap_or_default();
        let version = parts.next().map(semver::Version::parse).transpose().map_err(|_| dangling())?;
        let source = parts.next().map(|it| it.trim_start_matches('(').trim_end_matches(')'));
        let mut hits = packages.iter().enumerate().filter(|(_, it)| {
            it.name == name
                && version.as_ref().is_none_or(|v| &it.version == v)
                && source.is_none_or(|s| match &it.source {
                    Source::Registry { url, .. } => url == s,
                    Source::Git { url, query, commit } => {
                        git_source(s).is_some_and(|(u, q, c)| {
                            &u == url && &q == query && (c.is_empty() || &c == commit)
                        })
                    }
                    Source::Path => false,
                })
        });
        let (at, _) = hits.next().ok_or_else(dangling)?;
        if hits.next().is_some() {
            return Err(dangling());
        }
        Ok(at)
    }
}
