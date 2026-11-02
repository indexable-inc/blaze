//! Where a package comes from, as cargo names it: `src/workspace/source_id.rs`,
//! `src/util/canonical_url.rs` and `cargo-util-schemas`' `core/source_kind.rs` of cargo 1.99.0
//! (5f94df4789).

use std::cmp::{self, Ordering};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::OnceLock;

use anyhow::{Result, anyhow, bail};
use blaze_ids::{ContentId, Domain, Interner, Sharded};
use url::Url;

use crate::interning::InternedString;

/// The git index of crates.io, which is what `Cargo.lock` and package ids name crates.io by,
/// whichever protocol fetches it.
pub const CRATES_IO_INDEX: &str = "https://github.com/rust-lang/crates.io-index";
/// The sparse index of crates.io.
pub const CRATES_IO_HTTP_INDEX: &str = "sparse+https://index.crates.io/";
/// The name of crates.io in `[registries]` and `registry = "..."`.
pub const CRATES_IO_REGISTRY: &str = "crates-io";
pub const CRATES_IO_DOMAIN: &str = "crates.io";

/// What kind of place a source is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    Git(GitReference),
    Path,
    /// A registry with a git index.
    Registry,
    /// A registry with a sparse (HTTP) index. Its URL keeps the `sparse+` prefix.
    SparseRegistry,
    /// A `local-registry` source of `[source]`.
    LocalRegistry,
    /// A `directory` source of `[source]`, such as a vendored directory.
    Directory,
}

impl Hash for SourceKind {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        if let SourceKind::Git(git) = self {
            git.hash(state);
        }
    }
}

impl SourceKind {
    /// The prefix of the kind in a source URL (`git+`, `registry+`, ...). A sparse registry has
    /// none: `sparse+` is part of its URL.
    pub fn protocol(&self) -> Option<&'static str> {
        match self {
            SourceKind::Path => Some("path"),
            SourceKind::Git(_) => Some("git"),
            SourceKind::Registry => Some("registry"),
            SourceKind::SparseRegistry => None,
            SourceKind::LocalRegistry => Some("local-registry"),
            SourceKind::Directory => Some("directory"),
        }
    }
}

// This order is the order of packages in a lock file: path, then registries, then git.
impl Ord for SourceKind {
    fn cmp(&self, other: &SourceKind) -> Ordering {
        fn rank(kind: &SourceKind) -> u8 {
            match kind {
                SourceKind::Path => 0,
                SourceKind::Registry => 1,
                SourceKind::SparseRegistry => 2,
                SourceKind::LocalRegistry => 3,
                SourceKind::Directory => 4,
                SourceKind::Git(_) => 5,
            }
        }
        match (self, other) {
            (SourceKind::Git(a), SourceKind::Git(b)) => a.cmp(b),
            _ => rank(self).cmp(&rank(other)),
        }
    }
}

impl PartialOrd for SourceKind {
    fn partial_cmp(&self, other: &SourceKind) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Which commit of a git repository a dependency asks for. The order of the variants is
/// cargo's and decides the order of git packages in a lock file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GitReference {
    Tag(String),
    Branch(String),
    /// A commit hash (short or full) or a named reference like `refs/pull/493/head`.
    Rev(String),
    /// The reference named `HEAD`.
    DefaultBranch,
}

impl GitReference {
    /// Reads `?branch=`, `?tag=` and `?rev=` (and the old `?ref=`, a branch) of a source URL;
    /// the last one wins.
    pub fn from_query(
        query_pairs: impl Iterator<Item = (impl AsRef<str>, impl AsRef<str>)>,
    ) -> Self {
        let mut reference = GitReference::DefaultBranch;
        for (k, v) in query_pairs {
            let v = v.as_ref();
            match k.as_ref() {
                "branch" | "ref" => reference = GitReference::Branch(v.to_owned()),
                "rev" => reference = GitReference::Rev(v.to_owned()),
                "tag" => reference = GitReference::Tag(v.to_owned()),
                _ => {}
            }
        }
        reference
    }

    /// `branch=x`, `tag=x` or `rev=x`, form-urlencoded if `url_encoded`; `None` for the default
    /// branch.
    pub fn pretty_ref(&self, url_encoded: bool) -> Option<PrettyRef<'_>> {
        match self {
            GitReference::DefaultBranch => None,
            _ => Some(PrettyRef { inner: self, url_encoded }),
        }
    }
}

pub struct PrettyRef<'a> {
    inner: &'a GitReference,
    url_encoded: bool,
}

impl fmt::Display for PrettyRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self.inner {
            GitReference::Branch(s) => {
                f.write_str("branch=")?;
                s
            }
            GitReference::Tag(s) => {
                f.write_str("tag=")?;
                s
            }
            GitReference::Rev(s) => {
                f.write_str("rev=")?;
                s
            }
            GitReference::DefaultBranch => unreachable!(),
        };
        if self.url_encoded {
            for value in url::form_urlencoded::byte_serialize(value.as_bytes()) {
                f.write_str(value)?;
            }
        } else {
            f.write_str(value)?;
        }
        Ok(())
    }
}

/// A URL normalized for comparing sources only, never for fetching: no trailing slash, and
/// for git (URLs without a `+` in the scheme) no `.git` suffix, with github.com lowercased and
/// on https.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct CanonicalUrl(Url);

impl CanonicalUrl {
    pub fn new(url: &Url) -> Result<CanonicalUrl> {
        let mut url = url.clone();

        if url.cannot_be_a_base() {
            bail!("invalid url `{}`: cannot-be-a-base-URLs are not supported", url)
        }

        if url.path().ends_with('/') {
            url.path_segments_mut().unwrap().pop_if_empty();
        }

        if !url.scheme().contains('+') {
            if url.host_str() == Some("github.com") {
                url = format!("https{}", &url[url::Position::AfterScheme..]).parse().unwrap();
                let path = url.path().to_lowercase();
                url.set_path(&path);
            }

            if url.path().ends_with(".git") {
                let last = {
                    let last = url.path_segments().unwrap().next_back().unwrap();
                    last[..last.len() - 4].to_owned()
                };
                url.path_segments_mut().unwrap().pop().push(&last);
            }
        }

        Ok(CanonicalUrl(url))
    }

    pub fn raw_canonicalized_url(&self) -> &Url {
        &self.0
    }
}

impl Hash for CanonicalUrl {
    fn hash<S: Hasher>(&self, into: &mut S) {
        self.0.as_str().hash(into);
    }
}

/// A source of packages. Two ids are equal when their kind and canonical URL are, whatever
/// their precise revision; [`SourceId::full_eq`] compares the precise too.
///
/// Ids are interned for the life of the process, as cargo's are, so they copy for free. The
/// interning key is the kind, the precise and the canonical URL, so the first URL seen for a
/// canonical URL is the one every later id with it displays, as in cargo.
#[derive(Clone, Copy, Eq)]
pub struct SourceId {
    inner: &'static SourceIdInner,
}

#[derive(Eq, Clone, Debug)]
struct SourceIdInner {
    url: Url,
    canonical_url: CanonicalUrl,
    kind: SourceKind,
    precise: Option<Precise>,
    /// The name of the registry, where it is known. Not part of the identity: a registry read
    /// back from a lock file has no name.
    registry_key: Option<KeyOf>,
}

impl Hash for SourceIdInner {
    fn hash<S: Hasher>(&self, into: &mut S) {
        self.kind.hash(into);
        self.precise.hash(into);
        self.canonical_url.hash(into);
    }
}

impl PartialEq for SourceIdInner {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.precise == other.precise
            && self.canonical_url == other.canonical_url
    }
}

/// Every source id, by the content ID of what [`SourceIdInner`]'s `Eq` compares (its `Hash`
/// covers exactly that): a hit is lock-free probes and no allocation.
static SOURCE_IDS: OnceLock<Sharded<SourceIdInner>> = OnceLock::new();
static SOURCE_ID: Domain = Domain::new("blaze-source source id 1");

/// The first name each registry was given, by the content ID of its kind and canonical URL,
/// written once and read with no lock. An id interned before its named twin (a lock file's
/// registry read before the manifest's) has no name of its own; cargo always makes the named
/// one first, and its messages name the registry.
static REGISTRY_NAMES: OnceLock<Sharded<KeyOf>> = OnceLock::new();
static REGISTRY_NAME: Domain = Domain::new("blaze-source registry name 1");

fn registry_names() -> &'static Sharded<KeyOf> {
    REGISTRY_NAMES.get_or_init(Sharded::new)
}

/// The key of a registry in [`REGISTRY_NAMES`]. `std` hashing is only ever compared within
/// this process, so the hashes `Hash` writes need not be stable across runs.
fn registry_name_id(kind: &SourceKind, canonical_url: &CanonicalUrl) -> ContentId {
    let mut hasher = blaze_ids::Hasher::of(&REGISTRY_NAME);
    hasher.std(kind).std(canonical_url);
    hasher.finish()
}

/// What pins a source: a lock file's entry, a `cargo update --precise` request, or a git
/// commit.
#[derive(Eq, PartialEq, Clone, Debug, Hash)]
pub enum Precise {
    Locked,
    Updated { name: InternedString, from: semver::Version, to: semver::Version },
    GitUrlFragment(String),
}

impl fmt::Display for Precise {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Precise::Locked => f.write_str("locked"),
            Precise::Updated { name, from, to } => write!(f, "{name}={from}->{to}"),
            Precise::GitUrlFragment(s) => f.write_str(s),
        }
    }
}

/// Where the name of a registry was defined.
#[derive(Debug, Clone, PartialEq, Eq)]
enum KeyOf {
    /// `[registries]` or the built-in `crates-io`.
    Registry(String),
    /// A `[source]` replacement.
    Source(String),
}

impl KeyOf {
    fn key(&self) -> &str {
        match self {
            KeyOf::Registry(k) | KeyOf::Source(k) => k,
        }
    }
}

fn path_to_url(path: &Path) -> Result<Url> {
    Url::from_file_path(path).map_err(|()| anyhow!("invalid path url `{}`", path.display()))
}

fn str_to_url(s: &str) -> Result<Url> {
    Url::parse(s).map_err(|e| anyhow!("invalid url `{}`: {}", s, e))
}

impl SourceId {
    fn new(kind: SourceKind, url: Url, key: Option<KeyOf>) -> Result<SourceId> {
        if kind == SourceKind::SparseRegistry {
            assert!(url.as_str().starts_with("sparse+"));
        }
        let canonical_url = CanonicalUrl::new(&url)?;
        if let Some(key) = &key {
            // alloc: once per registry name for the life of the process.
            registry_names().intern(registry_name_id(&kind, &canonical_url), || key.clone());
        }
        Ok(SourceId::wrap(SourceIdInner {
            kind,
            canonical_url,
            url,
            precise: None,
            registry_key: key,
        }))
    }

    fn wrap(inner: SourceIdInner) -> SourceId {
        let ids = SOURCE_IDS.get_or_init(Sharded::new);
        let mut hasher = blaze_ids::Hasher::of(&SOURCE_ID);
        hasher.std(&inner);
        let at = ids.intern(hasher.finish(), || inner);
        SourceId { inner: ids.get(at) }
    }

    fn remote_source_kind(url: &Url) -> SourceKind {
        if url.as_str().starts_with("sparse+") {
            SourceKind::SparseRegistry
        } else {
            SourceKind::Registry
        }
    }

    /// Reads a source URL as a lock file or a package id writes it: `git+URL?branch=x#commit`,
    /// `registry+URL`, `sparse+URL`, `path+file://...`. Registry ids come back locked.
    pub fn from_url(string: &str) -> Result<SourceId> {
        let (kind, url) =
            string.split_once('+').ok_or_else(|| anyhow!("invalid source `{}`", string))?;

        match kind {
            "git" => {
                let mut url = str_to_url(url)?;
                let reference = GitReference::from_query(url.query_pairs());
                let precise = url.fragment().map(|s| s.to_owned());
                url.set_fragment(None);
                url.set_query(None);
                Ok(SourceId::for_git(&url, reference)?.with_git_precise(precise))
            }
            "registry" => {
                let url = str_to_url(url)?;
                Ok(SourceId::new(SourceKind::Registry, url, None)?.with_locked_precise())
            }
            "sparse" => {
                let url = str_to_url(string)?;
                Ok(SourceId::new(SourceKind::SparseRegistry, url, None)?.with_locked_precise())
            }
            "path" => {
                let url = str_to_url(url)?;
                SourceId::new(SourceKind::Path, url, None)
            }
            kind => Err(anyhow!("unsupported source protocol: {}", kind)),
        }
    }

    /// The id as a URL with its kind's prefix, as `Cargo.lock` and `cargo metadata` write it.
    pub fn as_url(&self) -> SourceIdAsUrl<'_> {
        SourceIdAsUrl { inner: self.inner, encoded: false }
    }

    /// Like [`SourceId::as_url`], with the git reference form-urlencoded, as lock files from
    /// version 4 write it.
    pub fn as_encoded_url(&self) -> SourceIdAsUrl<'_> {
        SourceIdAsUrl { inner: self.inner, encoded: true }
    }

    /// The source of the package in the directory `path`, which is absolute.
    pub fn for_path(path: &Path) -> Result<SourceId> {
        SourceId::new(SourceKind::Path, path_to_url(path)?, None)
    }

    pub fn for_git(url: &Url, reference: GitReference) -> Result<SourceId> {
        SourceId::new(SourceKind::Git(reference), url.clone(), None)
    }

    /// A registry named only by its URL, as `--index` and lock files name one.
    pub fn for_registry(url: &Url) -> Result<SourceId> {
        SourceId::new(Self::remote_source_kind(url), url.to_owned(), None)
    }

    /// A registry of `[registries]`, or crates.io.
    pub fn for_alt_registry(url: &Url, key: &str) -> Result<SourceId> {
        SourceId::new(
            Self::remote_source_kind(url),
            url.to_owned(),
            Some(KeyOf::Registry(key.into())),
        )
    }

    /// A registry of a `[source]` replacement.
    pub fn for_source_replacement_registry(url: &Url, key: &str) -> Result<SourceId> {
        SourceId::new(
            Self::remote_source_kind(url),
            url.to_owned(),
            Some(KeyOf::Source(key.into())),
        )
    }

    pub fn for_local_registry(path: &Path) -> Result<SourceId> {
        SourceId::new(SourceKind::LocalRegistry, path_to_url(path)?, None)
    }

    pub fn for_directory(path: &Path) -> Result<SourceId> {
        SourceId::new(SourceKind::Directory, path_to_url(path)?, None)
    }

    /// crates.io as cargo names it when no `[registry] index` or `[source]` changes it: its
    /// git index URL, named `crates-io`. The sparse protocol is a replacement of this source.
    pub fn crates_io() -> SourceId {
        static ID: OnceLock<SourceId> = OnceLock::new();
        *ID.get_or_init(|| {
            let url = Url::parse(CRATES_IO_INDEX).unwrap();
            SourceId::for_alt_registry(&url, CRATES_IO_REGISTRY).unwrap()
        })
    }

    /// crates.io's sparse index, named `crates-io`.
    pub fn crates_io_sparse() -> SourceId {
        static ID: OnceLock<SourceId> = OnceLock::new();
        *ID.get_or_init(|| {
            let url = Url::parse(CRATES_IO_HTTP_INDEX).unwrap();
            SourceId::for_alt_registry(&url, CRATES_IO_REGISTRY).unwrap()
        })
    }

    pub fn url(&self) -> &Url {
        &self.inner.url
    }

    pub fn canonical_url(&self) -> &CanonicalUrl {
        &self.inner.canonical_url
    }

    pub fn kind(&self) -> &'static SourceKind {
        &self.inner.kind
    }

    /// `crates.io index` or `` `name` index``, for messages.
    pub fn display_index(self) -> String {
        if self.is_crates_io() {
            format!("{} index", CRATES_IO_DOMAIN)
        } else {
            format!("`{}` index", self.display_registry_name())
        }
    }

    /// The registry's name if it has one, else its URL.
    pub fn display_registry_name(self) -> String {
        if let Some(key) = self.inner.registry_key.as_ref().map(|k| k.key()) {
            return key.into();
        }
        let names = registry_names();
        match names.find(&registry_name_id(&self.inner.kind, &self.inner.canonical_url)) {
            Some(at) => names.get(at).key().to_owned(),
            None if self.is_crates_io() => CRATES_IO_REGISTRY.to_owned(),
            None => url_display(self.url()),
        }
    }

    /// The name of the registry if it came from `[registries]` (or is `crates-io`).
    pub fn alt_registry_key(&self) -> Option<&str> {
        match self.inner.registry_key.as_ref()? {
            KeyOf::Registry(k) => Some(k),
            KeyOf::Source(_) => None,
        }
    }

    pub fn is_path(self) -> bool {
        self.inner.kind == SourceKind::Path
    }

    pub fn local_path(self) -> Option<PathBuf> {
        if self.inner.kind != SourceKind::Path {
            return None;
        }
        Some(self.inner.url.to_file_path().unwrap())
    }

    /// A registry, remote or local.
    pub fn is_registry(self) -> bool {
        matches!(
            self.inner.kind,
            SourceKind::Registry | SourceKind::SparseRegistry | SourceKind::LocalRegistry
        )
    }

    pub fn is_sparse(self) -> bool {
        matches!(self.inner.kind, SourceKind::SparseRegistry)
    }

    /// A registry with a git or sparse index (which may still be a `file://` URL).
    pub fn is_remote_registry(self) -> bool {
        matches!(self.inner.kind, SourceKind::Registry | SourceKind::SparseRegistry)
    }

    pub fn is_git(self) -> bool {
        matches!(self.inner.kind, SourceKind::Git(_))
    }

    pub fn git_reference(self) -> Option<&'static GitReference> {
        match self.inner.kind {
            SourceKind::Git(ref s) => Some(s),
            _ => None,
        }
    }

    pub fn precise(self) -> Option<&'static Precise> {
        self.inner.precise.as_ref()
    }

    pub fn has_precise(self) -> bool {
        self.inner.precise.is_some()
    }

    pub fn has_locked_precise(self) -> bool {
        self.inner.precise == Some(Precise::Locked)
    }

    pub fn has_same_precise_as(self, other: Self) -> bool {
        self.inner.precise == other.inner.precise
    }

    /// The version in the lock file and the one to update to, if `cargo update --precise`
    /// asked for `pkg` from this source.
    pub fn precise_registry_version(
        self,
        pkg: &str,
    ) -> Option<(&'static semver::Version, &'static semver::Version)> {
        match &self.inner.precise {
            Some(Precise::Updated { name, from, to }) if name == pkg => Some((from, to)),
            _ => None,
        }
    }

    /// The commit a git source is pinned to.
    pub fn precise_git_fragment(self) -> Option<&'static str> {
        match &self.inner.precise {
            Some(Precise::GitUrlFragment(s)) => Some(s),
            _ => None,
        }
    }

    pub fn with_git_precise(self, fragment: Option<String>) -> SourceId {
        self.with_precise(&fragment.map(Precise::GitUrlFragment))
    }

    pub fn without_precise(self) -> SourceId {
        self.with_precise(&None)
    }

    pub fn with_locked_precise(self) -> SourceId {
        self.with_precise(&Some(Precise::Locked))
    }

    pub fn with_precise_from(self, v: Self) -> SourceId {
        self.with_precise(&v.inner.precise)
    }

    fn with_precise(self, precise: &Option<Precise>) -> SourceId {
        if &self.inner.precise == precise {
            self
        } else {
            SourceId::wrap(SourceIdInner { precise: precise.clone(), ..self.inner.clone() })
        }
    }

    /// `cargo update -p name --precise version`: records that `name` at `version` in the lock
    /// file is to be updated to `precise`.
    pub fn with_precise_registry_version(
        self,
        name: InternedString,
        version: semver::Version,
        precise: &str,
    ) -> Result<SourceId> {
        let to = semver::Version::parse(precise).map_err(|e| {
            let message = if let Some(stripped) = precise.strip_prefix('v') {
                format!(
                    "the version provided, `{precise}` is not a valid SemVer version\n\n\
                     help: try changing the version to `{stripped}`"
                )
            } else {
                format!("invalid version format for precise version `{precise}`")
            };
            anyhow::Error::new(e).context(message)
        })?;
        Ok(SourceId::wrap(SourceIdInner {
            precise: Some(Precise::Updated { name, from: version, to }),
            ..self.inner.clone()
        }))
    }

    /// The registry is crates.io: its git or its sparse index.
    pub fn is_crates_io(self) -> bool {
        match self.inner.kind {
            SourceKind::Registry | SourceKind::SparseRegistry => {}
            _ => return false,
        }
        let url = self.inner.url.as_str();
        url == CRATES_IO_INDEX || url == CRATES_IO_HTTP_INDEX
    }

    /// Equal including the precise revision and the URL as written.
    pub fn full_eq(self, other: SourceId) -> bool {
        ptr::eq(self.inner, other.inner)
    }

    pub fn full_hash<S: Hasher>(self, into: &mut S) {
        ptr::NonNull::from(self.inner).hash(into)
    }

    /// The name of the directories under `$CARGO_HOME/registry/{index,cache,src}` a registry
    /// is kept in: the host of its URL and a hash of the source, `index.crates.io-1949cf8c6b5b557f`
    /// for crates.io's sparse index (cargo's `short_name` in `src/sources/registry/mod.rs`).
    pub fn registry_dir_name(self) -> String {
        let host = self.inner.url.host_str().unwrap_or("");
        format!("{host}-{}", short_hash(&self))
    }

    /// `<name>-<hash>`, the name of a git repository's database and of its checkouts'
    /// directory under `$CARGO_HOME/git`: the last segment of its canonical URL and a hash of
    /// that URL (cargo's `ident` in `src/sources/git/source.rs`).
    pub fn git_ident(self) -> String {
        let url = self.inner.canonical_url.raw_canonicalized_url();
        let last = url.path_segments().and_then(|mut it| it.next_back()).unwrap_or("");
        let last = if last.is_empty() { "_empty" } else { last };
        format!("{last}-{}", short_hash(url.as_str()))
    }
}

/// Sixteen hex digits of a hash of `value` that are the same on every platform and release, as
/// cargo's `util::hex::short_hash`: the little-endian bytes of a 64-bit SipHash-1-3 of 128 bits.
pub fn short_hash<H: Hash + ?Sized>(value: &H) -> String {
    let mut hasher = rustc_stable_hash::StableSipHasher128::new();
    value.hash(&mut hasher);
    Hasher::finish(&hasher).to_le_bytes().iter().map(|byte| format!("{byte:02x}")).collect()
}

impl PartialEq for SourceId {
    fn eq(&self, other: &SourceId) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl PartialOrd for SourceId {
    fn partial_cmp(&self, other: &SourceId) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SourceId {
    fn cmp(&self, other: &SourceId) -> Ordering {
        if ptr::eq(self.inner, other.inner) {
            return Ordering::Equal;
        }
        self.inner
            .kind
            .cmp(&other.inner.kind)
            .then_with(|| self.inner.canonical_url.cmp(&other.inner.canonical_url))
    }
}

impl Hash for SourceId {
    fn hash<S: Hasher>(&self, into: &mut S) {
        self.inner.kind.hash(into);
        self.inner.canonical_url.hash(into);
    }
}

fn url_display(url: &Url) -> String {
    if url.scheme() == "file"
        && let Ok(path) = url.to_file_path()
        && let Some(path_str) = path.to_str()
    {
        return path_str.to_owned();
    }
    url.as_str().to_owned()
}

/// How cargo shows a source in messages: a git URL with its reference and the first eight
/// characters of its commit, a path, `registry `name``, `dir /path`.
impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.inner.kind {
            SourceKind::Git(ref reference) => {
                write!(f, "{}", self.inner.url)?;
                if let Some(pretty) = reference.pretty_ref(true) {
                    write!(f, "?{}", pretty)?;
                }
                if let Some(s) = &self.inner.precise {
                    let s = s.to_string();
                    let len = cmp::min(s.len(), 8);
                    write!(f, "#{}", &s[..len])?;
                }
                Ok(())
            }
            SourceKind::Path => f.write_str(&url_display(&self.inner.url)),
            SourceKind::Registry | SourceKind::SparseRegistry => {
                write!(f, "registry `{}`", self.display_registry_name())
            }
            SourceKind::LocalRegistry => {
                write!(f, "registry `{}`", url_display(&self.inner.url))
            }
            SourceKind::Directory => write!(f, "dir {}", url_display(&self.inner.url)),
        }
    }
}

impl fmt::Debug for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceId")
            .field("url", &self.inner.url.as_str())
            .field("kind", &self.inner.kind)
            .field("precise", &self.inner.precise)
            .finish()
    }
}

pub struct SourceIdAsUrl<'a> {
    inner: &'a SourceIdInner,
    encoded: bool,
}

impl fmt::Display for SourceIdAsUrl<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(protocol) = self.inner.kind.protocol() {
            write!(f, "{protocol}+")?;
        }
        write!(f, "{}", self.inner.url)?;
        if let SourceKind::Git(ref reference) = self.inner.kind {
            if let Some(pretty) = reference.pretty_ref(self.encoded) {
                write!(f, "?{}", pretty)?;
            }
            if let Some(precise) = self.inner.precise.as_ref() {
                write!(f, "#{}", precise)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    // cargo's `github_sources_equal`.
    #[test]
    fn github_sources_equal() {
        let default = SourceKind::Git(GitReference::DefaultBranch);
        let s1 = SourceId::new(default.clone(), url("https://github.com/foo/bar"), None).unwrap();
        let s2 = SourceId::new(default, url("git://github.com/foo/bar"), None).unwrap();
        assert_eq!(s1, s2);

        let foo = SourceKind::Git(GitReference::Branch("foo".to_owned()));
        let s3 = SourceId::new(foo, url("git://github.com/foo/bar"), None).unwrap();
        assert_ne!(s1, s3);
    }

    #[test]
    fn canonical_urls() {
        let canon = |s: &str| CanonicalUrl::new(&url(s)).unwrap().0.to_string();
        assert_eq!(canon("https://github.com/Foo/Bar.git"), "https://github.com/foo/bar");
        assert_eq!(canon("http://github.com/foo/bar/"), "https://github.com/foo/bar");
        assert_eq!(canon("https://gitlab.com/Foo/Bar.git/"), "https://gitlab.com/Foo/Bar");
        // A `+` in the scheme is a registry: no `.git` stripping, no lowercasing.
        assert_eq!(
            canon("sparse+https://github.com/Foo/x.git"),
            "sparse+https://github.com/Foo/x.git"
        );
        // A path of only `/` keeps it: the url crate has no empty path for a URL with a host.
        assert_eq!(canon("sparse+https://index.crates.io/"), "sparse+https://index.crates.io/");
        assert_eq!(canon("sparse+https://example.com/index/"), "sparse+https://example.com/index");
        assert!(CanonicalUrl::new(&url("mailto:x@y")).is_err());
    }

    // cargo's `serde_roundtrip`.
    #[test]
    fn sparse_roundtrip() {
        let source_id = SourceId::for_registry(&url("sparse+https://my-crates.io/")).unwrap();
        let formatted = source_id.as_url().to_string();
        assert_eq!(formatted, "sparse+https://my-crates.io/");
        let deserialized = SourceId::from_url(&formatted).unwrap();
        assert_eq!(source_id, deserialized);
        assert!(deserialized.is_sparse());
        assert!(deserialized.has_locked_precise());
    }

    // cargo's `gitrefs_roundtrip`.
    #[test]
    fn gitrefs_roundtrip() {
        let base = url("https://host/path");
        let branch = GitReference::Branch("*-._+20%30 Z/z#foo=bar&zap[]?to\\()'\"".to_owned());
        let s1 = SourceId::for_git(&base, branch).unwrap();
        let ser1 = s1.as_encoded_url().to_string();
        let s2 = SourceId::from_url(&ser1).unwrap();
        let ser2 = s2.as_encoded_url().to_string();
        assert_eq!(ser1, ser2);
        assert_eq!(s1, s2);
        assert_eq!(
            ser1,
            "git+https://host/path?branch=*-._%2B20%2530+Z%2Fz%23foo%3Dbar%26zap%5B%5D%3Fto%5C%28%29%27%22"
        );
    }

    #[test]
    fn git_precise_and_display() {
        let id = SourceId::from_url(
            "git+https://github.com/alexcrichton/libssh2-static-sys?rev=80e71a3#80e71a3021618eb05656c58fb7c5ef5f12bc747f",
        )
        .unwrap();
        assert_eq!(id.git_reference(), Some(&GitReference::Rev("80e71a3".to_owned())));
        assert_eq!(id.precise_git_fragment(), Some("80e71a3021618eb05656c58fb7c5ef5f12bc747f"));
        assert_eq!(
            id.as_url().to_string(),
            "git+https://github.com/alexcrichton/libssh2-static-sys?rev=80e71a3#80e71a3021618eb05656c58fb7c5ef5f12bc747f"
        );
        assert_eq!(
            id.to_string(),
            "https://github.com/alexcrichton/libssh2-static-sys?rev=80e71a3#80e71a30"
        );
        // Equal without the precise, not fully equal.
        assert_eq!(id, id.without_precise());
        assert!(!id.full_eq(id.without_precise()));
    }

    #[test]
    fn old_ref_query_is_a_branch() {
        let id = SourceId::from_url("git+https://example.com/a?ref=dev").unwrap();
        assert_eq!(id.git_reference(), Some(&GitReference::Branch("dev".to_owned())));
        assert_eq!(id.as_url().to_string(), "git+https://example.com/a?branch=dev");
    }

    #[test]
    fn registry_display() {
        let crates_io = SourceId::crates_io();
        assert!(crates_io.is_crates_io());
        assert_eq!(
            crates_io.as_url().to_string(),
            "registry+https://github.com/rust-lang/crates.io-index"
        );
        assert_eq!(crates_io.to_string(), "registry `crates-io`");
        assert_eq!(crates_io.display_index(), "crates.io index");
        // A lock file's registry has no name, but the id created with one gives it.
        let locked =
            SourceId::from_url("registry+https://github.com/rust-lang/crates.io-index").unwrap();
        assert_eq!(locked, crates_io);
        assert_eq!(locked.to_string(), "registry `crates-io`");
        assert!(SourceId::crates_io_sparse().is_crates_io());
        assert_ne!(SourceId::crates_io_sparse(), crates_io);

        // A registry read before its named twin is made still shows the name.
        let early = SourceId::from_url("registry+https://named-late.example/index").unwrap();
        SourceId::for_alt_registry(&url("https://named-late.example/index"), "late").unwrap();
        assert_eq!(early.to_string(), "registry `late`");
        assert_eq!(early.without_precise().to_string(), "registry `late`");

        let alt = SourceId::from_url("registry+https://example.com/index").unwrap();
        assert_eq!(alt.to_string(), "registry `https://example.com/index`");
        assert_eq!(alt.display_index(), "`https://example.com/index` index");
    }

    #[cfg(unix)]
    #[test]
    fn local_sources() {
        let path = SourceId::for_path(Path::new("/tmp/ws/crate")).unwrap();
        assert_eq!(path.as_url().to_string(), "path+file:///tmp/ws/crate");
        assert_eq!(path.to_string(), "/tmp/ws/crate");
        assert_eq!(path.local_path(), Some(PathBuf::from("/tmp/ws/crate")));
        assert_eq!(SourceId::from_url("path+file:///tmp/ws/crate").unwrap(), path);

        let dir = SourceId::for_directory(Path::new("/tmp/vendor")).unwrap();
        assert_eq!(dir.as_url().to_string(), "directory+file:///tmp/vendor");
        assert_eq!(dir.to_string(), "dir /tmp/vendor");
        let local = SourceId::for_local_registry(Path::new("/tmp/reg")).unwrap();
        assert_eq!(local.as_url().to_string(), "local-registry+file:///tmp/reg");
        assert_eq!(local.to_string(), "registry `/tmp/reg`");
        assert!(local.is_registry() && !local.is_remote_registry());
    }

    #[test]
    fn kinds_order_as_in_lock_files() {
        let mut kinds = vec![
            SourceKind::Git(GitReference::DefaultBranch),
            SourceKind::Directory,
            SourceKind::Git(GitReference::Tag("v1".to_owned())),
            SourceKind::Registry,
            SourceKind::Path,
            SourceKind::SparseRegistry,
            SourceKind::LocalRegistry,
            SourceKind::Git(GitReference::Branch("b".to_owned())),
        ];
        kinds.sort();
        assert_eq!(
            kinds,
            vec![
                SourceKind::Path,
                SourceKind::Registry,
                SourceKind::SparseRegistry,
                SourceKind::LocalRegistry,
                SourceKind::Directory,
                SourceKind::Git(GitReference::Tag("v1".to_owned())),
                SourceKind::Git(GitReference::Branch("b".to_owned())),
                SourceKind::Git(GitReference::DefaultBranch),
            ]
        );
    }

    #[test]
    fn bad_sources() {
        assert!(SourceId::from_url("https://example.com").is_err());
        assert!(SourceId::from_url("svn+https://example.com").is_err());
        assert!(SourceId::from_url("registry+not a url").is_err());
    }

    /// The values of cargo 1.99's `test_stable_hash` (`src/workspace/source_id.rs`), and the
    /// directories cargo 1.99 makes for crates.io.
    #[test]
    fn names_directories_as_cargo() {
        let mine = SourceId::for_registry(&url("https://my-crates.io")).unwrap();
        assert_eq!(short_hash(&SourceId::crates_io()), "25cdd57fae9f0462");
        assert_eq!(short_hash(&mine), "b2d65deb64f05373");
        assert_eq!(
            SourceId::crates_io_sparse().registry_dir_name(),
            "index.crates.io-1949cf8c6b5b557f"
        );
        assert_eq!(SourceId::crates_io().registry_dir_name(), "github.com-25cdd57fae9f0462");
        // The precise of a lock file's source is not part of the name.
        let locked = SourceId::from_url("sparse+https://index.crates.io/").unwrap();
        assert_eq!(locked.registry_dir_name(), "index.crates.io-1949cf8c6b5b557f");

        // `gen_hash` of cargo's test: the source's `stable_hash`, which for these is its kind
        // and its URL as written, finished as a number.
        let stable = |id: SourceId| {
            let mut hasher = rustc_stable_hash::StableSipHasher128::new();
            id.kind().hash(&mut hasher);
            id.url().as_str().hash(&mut hasher);
            Hasher::finish(&hasher)
        };
        assert_eq!(stable(SourceId::crates_io()), 7062945687441624357);
        assert_eq!(stable(mine), 8310250053664888498);
        let yours = SourceId::for_registry(&url("https://your-crates.io")).unwrap();
        assert_eq!(stable(yours), 14149534903000258933);
    }

    /// The directory cargo 1.99 made for zed's `async-process` dependency
    /// (`git+https://github.com/zed-industries/async-process.git?rev=0b6d671…`).
    #[test]
    fn names_git_directories_as_cargo() {
        let base = url("https://github.com/zed-industries/async-process.git");
        let rev = GitReference::Rev("0b6d6713570af61806e1e5cb40e0f757cb93fd9d".to_owned());
        assert_eq!(
            SourceId::for_git(&base, rev).unwrap().git_ident(),
            "async-process-b80486ce8f827270"
        );
        let root = SourceId::for_git(&url("https://example.com/"), GitReference::DefaultBranch);
        assert!(root.unwrap().git_ident().starts_with("_empty-"));
    }

    #[test]
    fn precise_update() {
        let id = SourceId::crates_io()
            .with_precise_registry_version("serde".into(), "1.0.0".parse().unwrap(), "1.0.5")
            .unwrap();
        let (from, to) = id.precise_registry_version("serde").unwrap();
        assert_eq!((from.to_string(), to.to_string()), ("1.0.0".to_owned(), "1.0.5".to_owned()));
        assert!(id.precise_registry_version("log").is_none());
        let err = SourceId::crates_io()
            .with_precise_registry_version("serde".into(), "1.0.0".parse().unwrap(), "v1.0.5")
            .unwrap_err();
        assert!(format!("{err:#}").contains("try changing the version to `1.0.5`"));
    }
}
