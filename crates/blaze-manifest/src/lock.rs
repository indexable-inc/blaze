//! `Cargo.lock`, read sans-IO: the one reader of it (moved here from blaze-audit's
//! `cargo_lock.rs`, rule 30). Each package's name, version, source, checksum and the packages it
//! depends on, by index; the file's format version.
//!
//! `Cargo.lock` is a file blaze did not write, so its size is bounded before it is decoded, its
//! package count before the packages are read, and every dependency entry must name exactly one
//! package of the file. Every refusal names the package and the line of its `[[package]]` (rule
//! 19). Every owner is in the allocator the caller passes.
//!
//! What the formats say (cargo's `src/cargo/core/resolver/encode.rs`, the lock's documented
//! shape): `version = 3` or `4` at the top (none: format 1 or 2); a `[[package]]` per package
//! with `name`, `version`, `source` (none for a workspace member or a path dependency),
//! `checksum` (a registry package's sha256, hex; format 1 keeps them in `[metadata]` as
//! `"checksum NAME VERSION (SOURCE)" = "HEX"`), and `dependencies`, each `NAME`, or
//! `NAME VERSION` where the name alone is ambiguous, or `NAME VERSION (SOURCE)` where that is
//! too. Format 4 escapes a git reference's query; the text is kept as written here.
//!
//! `[patch]` and `[replace]` need no reading of their own: the lock already holds the package a
//! patch chose, under the source it came from, and the dependency entries name it.

use core::fmt;

use blaze_own::{Alloc, String, Vec};
use blaze_semver::Version;
use blaze_toml::{Document, Item, Limits};

/// Text in the caller's allocator.
pub type Text<'a> = String<Alloc<'a>>;

/// The largest `Cargo.lock` read: 64 MiB. The largest real ones (thousands of packages) are a
/// few MiB; past this is a file no workspace has, refused before TOML decodes it.
pub const MOST_LOCK_BYTES: usize = 64 << 20;

/// The most packages a lock may hold: 2^17, an order past any real workspace's graph.
pub const MOST_PACKAGES: usize = 1 << 17;

/// A package of the lock: its index in [`Lock::packages`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LockId(pub u32);

impl LockId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// The lock's format: the top-level `version`, or `None` when none is written (1 or 2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LockFormat(pub Option<u32>);

/// A registry package's checksum, or its absence named: a registry package with none is never
/// trusted silently; the caller decides (a fetch refuses it).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Checksum<'a> {
    /// The sha256 of the `.crate` file, as written (64 lowercase hex digits).
    Sha256(Text<'a>),
    /// None written.
    Absent,
}

/// A git source's reference, as the query of its URL says.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GitReference<'a> {
    DefaultBranch,
    Branch(Text<'a>),
    Tag(Text<'a>),
    Rev(Text<'a>),
}

/// Where a locked package comes from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LockSource<'a> {
    /// No `source`: a workspace member or a path dependency.
    Path,
    /// `registry+URL` (a git index) or `sparse+URL`: `url` without the prefix. crates.io is
    /// [`LockSource::is_crates_io`]; any other URL is an alternate registry, named by it.
    Registry { url: Text<'a>, sparse: bool },
    /// `git+URL?QUERY#COMMIT`: `url` without its query and fragment, the reference the query
    /// names, and the exact commit.
    Git { url: Text<'a>, reference: GitReference<'a>, commit: Text<'a> },
}

/// crates.io's index, as a git index and as a sparse one.
pub const CRATES_IO_GIT: &str = "https://github.com/rust-lang/crates.io-index";
pub const CRATES_IO_SPARSE: &str = "https://index.crates.io/";

impl LockSource<'_> {
    pub fn is_crates_io(&self) -> bool {
        match self {
            LockSource::Registry { url, .. } => url.as_str() == CRATES_IO_GIT || url.as_str() == CRATES_IO_SPARSE,
            LockSource::Path | LockSource::Git { .. } => false,
        }
    }
}

/// One `[[package]]`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LockPackage<'a> {
    pub name: Text<'a>,
    pub version: Version<Alloc<'a>>,
    pub source: LockSource<'a>,
    /// The source as written (`registry+https://..`), empty for [`LockSource::Path`]: what a
    /// dependency entry's `(SOURCE)` is compared with.
    pub source_text: Text<'a>,
    /// [`Checksum::Absent`] for a path or git package, where none is written.
    pub checksum: Checksum<'a>,
    /// What it depends on, sorted by index, each once.
    pub deps: Vec<LockId, Alloc<'a>>,
    /// The 1-based line of its `[[package]]`.
    pub line: u32,
}

/// A whole `Cargo.lock`.
pub struct Lock<'a> {
    pub format: LockFormat,
    /// In the file's order (cargo writes them sorted by name, version and source).
    pub packages: Vec<LockPackage<'a>, Alloc<'a>>,
    /// Every package's id sorted by name, for [`Lock::named`].
    by_name: Vec<LockId, Alloc<'a>>,
}

/// Why a `Cargo.lock` was not read: each names the package and its line, or the place.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LockRefused<'a> {
    TooBig { bytes: usize },
    TooMany { packages: usize },
    NotUtf8 { at: usize },
    NotToml(blaze_toml::Error),
    /// `[[package]]` at `line` (named `package`, empty when it has no name) is malformed: `why`
    /// says how, `value` the text that was refused (empty when none).
    Bad { line: u32, package: Text<'a>, why: &'static str, value: Text<'a> },
    /// The package at `line` depends on `dep`, which names no package of the lock, or more
    /// than one.
    Dangling { line: u32, package: Text<'a>, dep: Text<'a> },
}

impl fmt::Display for LockRefused<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockRefused::TooBig { bytes } => {
                write!(f, "Cargo.lock is {bytes} bytes, more than the {MOST_LOCK_BYTES} a lock may be")
            }
            LockRefused::TooMany { packages } => {
                write!(f, "Cargo.lock has {packages} packages, more than the {MOST_PACKAGES} a lock may")
            }
            LockRefused::NotUtf8 { at } => write!(f, "Cargo.lock is not UTF-8 at byte {at}"),
            LockRefused::NotToml(error) => write!(f, "Cargo.lock is not TOML: {error}"),
            LockRefused::Bad { line, package, why, value } => {
                write!(f, "Cargo.lock line {line}: package `{package}` {why}")?;
                match value.is_empty() {
                    true => Ok(()),
                    false => write!(f, ": `{value}`"),
                }
            }
            LockRefused::Dangling { line, package, dep } => write!(
                f,
                "Cargo.lock line {line}: package `{package}` depends on `{dep}`, which names no one package of the lock"
            ),
        }
    }
}

impl core::error::Error for LockRefused<'_> {}

type It<'d, 't, 'a> = Item<'d, 't, Alloc<'a>>;

impl<'a> Lock<'a> {
    /// `bytes` read as a `Cargo.lock`, every owner in `alloc`.
    pub fn read(bytes: &[u8], alloc: Alloc<'a>) -> Result<Lock<'a>, LockRefused<'a>> {
        if bytes.len() > MOST_LOCK_BYTES {
            return Err(LockRefused::TooBig { bytes: bytes.len() });
        }
        let text = match core::str::from_utf8(bytes) {
            Ok(text) => text,
            Err(error) => return Err(LockRefused::NotUtf8 { at: error.valid_up_to() }),
        };
        let doc = Document::parse_in(text, Limits::CARGO, alloc).map_err(LockRefused::NotToml)?;
        let root = doc.root();
        let format = LockFormat(root.get("version").and_then(|it| it.as_integer()).and_then(|it| u32::try_from(it).ok()));
        let listed = root.get("package");
        let count = listed.map_or(0, |it| it.len());
        if count > MOST_PACKAGES {
            return Err(LockRefused::TooMany { packages: count });
        }
        let text_in = |s: &str| String::from_str_in(s, alloc);
        let mut packages: Vec<LockPackage<'a>, Alloc<'a>> = Vec::with_capacity_in(count, alloc);
        // Each package's dependency entries, as written, read once the packages are all known.
        let mut wanted: Vec<Vec<&str, Alloc<'a>>, Alloc<'a>> = Vec::with_capacity_in(count, alloc);
        if let Some(listed) = listed {
            if !listed.is_array() {
                let (line, _) = listed.line_col();
                return Err(bad(line, "", "`package` is not an array of tables", "", alloc));
            }
            // Lines counted forward from the last table, never from the file's start: the
            // tables come in the text's order, so the whole count is one walk of the text (a
            // `line_col` per table was quadratic: 32 ms of tempo's 34 ms read).
            let mut lines = Lines { at: 0, line: 1 };
            for table in listed.children() {
                let line = lines.at(text, table.span().start);
                if !table.is_table() {
                    return Err(bad(line, "", "is not a table", "", alloc));
                }
                let field = |key: &str| table.get(key).and_then(|it| it.as_str());
                let Some(name) = field("name") else { return Err(bad(line, "", "has no `name`", "", alloc)) };
                let Some(version) = field("version") else { return Err(bad(line, name, "has no `version`", "", alloc)) };
                let version = match Version::parse_in(version, blaze_semver::Limits::CARGO, alloc) {
                    Ok(version) => version,
                    Err(_) => return Err(bad(line, name, "has a version that is not semver", version, alloc)),
                };
                let source_text = field("source").unwrap_or("");
                let source = source_of(source_text, alloc).ok_or_else(|| bad(line, name, "has a source that is no registry, sparse or git source", source_text, alloc))?;
                let checksum = match field("checksum") {
                    Some(sum) => Checksum::Sha256(text_in(sum)),
                    None => Checksum::Absent,
                };
                let mut deps = Vec::new_in(alloc);
                if let Some(list) = table.get("dependencies") {
                    if !list.is_array() {
                        return Err(bad(line, name, "has `dependencies` that is not an array", "", alloc));
                    }
                    for dep in list.children() {
                        match dep.as_str() {
                            Some(dep) => deps.push(dep),
                            None => return Err(bad(line, name, "has a dependency that is not a string", "", alloc)),
                        }
                    }
                }
                packages.push(LockPackage {
                    name: text_in(name),
                    version,
                    source,
                    source_text: text_in(source_text),
                    checksum,
                    deps: Vec::new_in(alloc),
                    line: narrow(line),
                });
                wanted.push(deps);
            }
        }
        // Format 1 and 2 keep checksums in `[metadata]`: `"checksum NAME VERSION (SOURCE)"`.
        if let Some(metadata) = root.get("metadata") {
            for entry in metadata.children() {
                let Some(rest) = entry.key().strip_prefix("checksum ") else { continue };
                let Some(sum) = entry.as_str() else { continue };
                if sum == "<none>" {
                    continue;
                }
                if let Some(at) = find_entry(&packages, rest) {
                    packages[at].checksum = Checksum::Sha256(text_in(sum));
                }
            }
        }
        let mut by_name = Vec::with_capacity_in(packages.len(), alloc);
        for at in 0..packages.len() {
            by_name.push(LockId(narrow(at)));
        }
        by_name.sort_by(|a, b| packages[a.index()].name.as_str().cmp(packages[b.index()].name.as_str()));
        let mut lock = Lock { format, packages, by_name };
        for (at, entries) in wanted.iter().enumerate() {
            let mut found = Vec::with_capacity_in(entries.len(), alloc);
            for entry in entries.iter() {
                match lock.entry(entry) {
                    Some(id) => found.push(id),
                    None => {
                        let package = &lock.packages[at];
                        return Err(LockRefused::Dangling { line: package.line, package: package.name.clone_in(alloc), dep: text_in(entry) });
                    }
                }
            }
            found.sort_unstable();
            found.dedup();
            lock.packages[at].deps = found;
        }
        Ok(lock)
    }

    /// Every package named `name`, in the file's order.
    pub fn named<'s>(&'s self, name: &'s str) -> impl Iterator<Item = LockId> + 's {
        let start = self.by_name.partition_point(|id| self.packages[id.index()].name.as_str() < name);
        self.by_name[start..].iter().copied().take_while(move |id| self.packages[id.index()].name.as_str() == name)
    }

    pub fn package(&self, id: LockId) -> &LockPackage<'a> {
        &self.packages[id.index()]
    }

    /// The one package a dependency entry names (`NAME`, `NAME VERSION` or
    /// `NAME VERSION (SOURCE)`); `None` for none or more than one.
    pub fn entry(&self, entry: &str) -> Option<LockId> {
        let (name, version, source) = split_entry(entry);
        let mut hits = self.named(name).filter(|id| {
            let package = &self.packages[id.index()];
            version.is_none_or(|v| version_is(&package.version, v)) && source.is_none_or(|s| source_is(&package.source_text, s))
        });
        let first = hits.next()?;
        match hits.next() {
            Some(_) => None,
            None => Some(first),
        }
    }
}

/// The line of a byte offset, counted on from the last one asked.
struct Lines {
    at: usize,
    line: usize,
}

impl Lines {
    fn at(&mut self, text: &str, offset: usize) -> usize {
        let offset = offset.min(text.len());
        if offset < self.at {
            // Asked behind the cursor: count again from the start.
            self.at = 0;
            self.line = 1;
        }
        self.line += text.as_bytes()[self.at..offset].iter().filter(|&&b| b == b'\n').count();
        self.at = offset;
        self.line
    }
}

fn bad<'a>(line: usize, package: &str, why: &'static str, value: &str, alloc: Alloc<'a>) -> LockRefused<'a> {
    LockRefused::Bad { line: narrow(line), package: String::from_str_in(package, alloc), why, value: String::from_str_in(value, alloc) }
}

fn narrow(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// `NAME [VERSION [(SOURCE)]]` taken apart.
fn split_entry(entry: &str) -> (&str, Option<&str>, Option<&str>) {
    let mut parts = entry.splitn(3, ' ');
    let name = parts.next().unwrap_or("");
    let version = parts.next();
    let source = parts.next().map(|it| it.trim_start_matches('(').trim_end_matches(')'));
    (name, version, source)
}

/// Whether `version` is the one written as `text`, compared as text so no allocation is made.
fn version_is(version: &Version<Alloc<'_>>, text: &str) -> bool {
    use core::fmt::Write;
    // A version's display is its text; compared piece by piece against `text`.
    struct Eq<'t> {
        rest: &'t str,
        same: bool,
    }
    impl Write for Eq<'_> {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            match self.rest.strip_prefix(s) {
                Some(rest) => self.rest = rest,
                None => self.same = false,
            }
            Ok(())
        }
    }
    let mut eq = Eq { rest: text, same: true };
    let _ = write!(eq, "{version}");
    eq.same && eq.rest.is_empty()
}

/// Whether a package whose source is written `written` is the one an entry's `(SOURCE)` names:
/// the same text, or for git the same but for the `#COMMIT` the entry may leave out.
fn source_is(written: &str, entry: &str) -> bool {
    written == entry || written.split_once('#').is_some_and(|(before, _)| before == entry)
}

/// The package a `[metadata]` checksum key's `NAME VERSION (SOURCE)` names.
fn find_entry(packages: &[LockPackage<'_>], entry: &str) -> Option<usize> {
    let (name, version, source) = split_entry(entry);
    packages.iter().position(|package| {
        package.name.as_str() == name
            && version.is_none_or(|v| version_is(&package.version, v))
            && source.is_none_or(|s| source_is(&package.source_text, s))
    })
}

/// `source` as written read, `Path` for none; `None` when it is no source cargo writes.
fn source_of<'a>(source: &str, alloc: Alloc<'a>) -> Option<LockSource<'a>> {
    if source.is_empty() {
        return Some(LockSource::Path);
    }
    if let Some(url) = source.strip_prefix("registry+") {
        return Some(LockSource::Registry { url: String::from_str_in(url, alloc), sparse: false });
    }
    if let Some(url) = source.strip_prefix("sparse+") {
        return Some(LockSource::Registry { url: String::from_str_in(url, alloc), sparse: true });
    }
    let rest = source.strip_prefix("git+")?;
    let (rest, commit) = rest.split_once('#').unwrap_or((rest, ""));
    let (url, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut reference = GitReference::DefaultBranch;
    for pair in query.split('&').filter(|it| !it.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = String::from_str_in(value, alloc);
        reference = match key {
            "branch" => GitReference::Branch(value),
            "tag" => GitReference::Tag(value),
            "rev" => GitReference::Rev(value),
            _ => return None,
        };
    }
    Some(LockSource::Git { url: String::from_str_in(url, alloc), reference, commit: String::from_str_in(commit, alloc) })
}

#[cfg(test)]
mod tests {

    use super::{Checksum, GitReference, Lock, LockFormat, LockRefused, LockSource, MOST_LOCK_BYTES};

    const LOCK: &str = r#"
version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "rand 0.7.3",
 "rand 0.8.5",
 "serde",
 "tool",
]

[[package]]
name = "rand"
version = "0.7.3"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "6a6b1679d49b24bbfe0c803429aa1874472f50d9b363131f0e89fc356b544d03"

[[package]]
name = "rand"
version = "0.8.5"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "34af8d1a0e25924bc5b7c43c079c942339d8f0a8b57c39049bef581b46327404"

[[package]]
name = "serde"
version = "1.0.200"
source = "sparse+https://my.registry.example/index/"

[[package]]
name = "tool"
version = "0.3.0"
source = "git+https://github.com/o/tool?rev=abc123#abc123def456"
"#;

    #[test]
    fn a_lock_reads_with_its_sources_checksums_and_entries() {
        let lock = Lock::read(LOCK.as_bytes(), blaze_own::testing::heap().alloc()).unwrap();
        assert_eq!(lock.format, LockFormat(Some(4)));
        assert_eq!(lock.packages.len(), 5);
        let app = &lock.packages[0];
        assert_eq!(app.source, LockSource::Path);
        let names: std::vec::Vec<std::string::String> =
            app.deps.iter().map(|id| std::format!("{} {}", lock.package(*id).name, lock.package(*id).version)).collect();
        assert_eq!(names, ["rand 0.7.3", "rand 0.8.5", "serde 1.0.200", "tool 0.3.0"]);
        assert!(lock.packages[1].source.is_crates_io());
        assert!(matches!(&lock.packages[1].checksum, Checksum::Sha256(sum) if sum.len() == 64));
        assert!(matches!(&lock.packages[3].source, LockSource::Registry { sparse: true, .. }));
        assert!(!lock.packages[3].source.is_crates_io());
        assert_eq!(lock.packages[3].checksum, Checksum::Absent);
        match &lock.packages[4].source {
            LockSource::Git { url, reference: GitReference::Rev(rev), commit } => {
                assert_eq!((url.as_str(), rev.as_str(), commit.as_str()), ("https://github.com/o/tool", "abc123", "abc123def456"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(lock.named("rand").count(), 2);
        assert_eq!(lock.entry("rand"), None, "two packages named rand: an entry by name alone is ambiguous");
    }

    #[test]
    fn a_dangling_entry_is_refused_with_its_line() {
        let text = "version = 3\n\n[[package]]\nname = \"a\"\nversion = \"1.0.0\"\ndependencies = [\"nope\"]\n";
        match Lock::read(text.as_bytes(), blaze_own::testing::heap().alloc()) {
            Err(LockRefused::Dangling { line, package, dep }) => {
                assert_eq!((line, package.as_str(), dep.as_str()), (3, "a", "nope"));
            }
            Err(other) => panic!("{other}"),
            Ok(_) => panic!("read"),
        }
    }

    #[test]
    fn a_lock_past_the_bound_is_refused_before_decoding() {
        let text = std::vec![b' '; MOST_LOCK_BYTES + 1];
        assert!(matches!(Lock::read(&text, blaze_own::testing::heap().alloc()), Err(LockRefused::TooBig { .. })));
    }
}
