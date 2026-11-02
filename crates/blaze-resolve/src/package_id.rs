//! A package at a version in a source: `src/workspace/package_id.rs` of cargo 1.99.0
//! (5f94df4789).

use std::fmt;
use std::hash::{Hash, Hasher};
use std::ptr;
use std::sync::OnceLock;

use anyhow::{Result, anyhow};
use blaze_ids::{Domain, Interner, Sharded};

use crate::interning::InternedString;
use crate::package_id_spec::PackageIdSpec;
use crate::source_id::SourceId;

/// Interned like [`SourceId`], so it copies for free. Equal when the name, the version
/// (build metadata included) and the source (as [`SourceId`] compares, without its precise)
/// are.
#[derive(Clone, Copy, Eq, PartialOrd, Ord)]
pub struct PackageId {
    inner: &'static PackageIdInner,
}

#[derive(PartialOrd, Eq, Ord)]
struct PackageIdInner {
    name: InternedString,
    version: semver::Version,
    source_id: SourceId,
}

// The interning key compares the source fully, so that an id keeps the precise and URL it
// was made with.
impl PartialEq for PackageIdInner {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.version == other.version
            && self.source_id.full_eq(other.source_id)
    }
}

impl Hash for PackageIdInner {
    fn hash<S: Hasher>(&self, into: &mut S) {
        self.name.hash(into);
        self.version.hash(into);
        self.source_id.full_hash(into);
    }
}

/// Every package id, by the content ID of what [`PackageIdInner`]'s `Eq` compares (its `Hash`
/// covers exactly that, the source by address): a hit is lock-free probes and no allocation.
/// The ID is only compared within this process, so an address in it is sound.
static PACKAGE_IDS: OnceLock<Sharded<PackageIdInner>> = OnceLock::new();
static PACKAGE_ID: Domain = Domain::new("blaze-resolve package id 1");

impl PartialEq for PackageId {
    fn eq(&self, other: &PackageId) -> bool {
        if ptr::eq(self.inner, other.inner) {
            return true;
        }
        self.inner.name == other.inner.name
            && self.inner.version == other.inner.version
            && self.inner.source_id == other.inner.source_id
    }
}

impl Hash for PackageId {
    fn hash<S: Hasher>(&self, state: &mut S) {
        self.inner.name.hash(state);
        self.inner.version.hash(state);
        self.inner.source_id.hash(state);
    }
}

impl PackageId {
    pub fn try_new(
        name: impl Into<InternedString>,
        version: &str,
        sid: SourceId,
    ) -> Result<PackageId> {
        let v = version.parse()?;
        Ok(PackageId::new(name.into(), v, sid))
    }

    pub fn new(name: InternedString, version: semver::Version, source_id: SourceId) -> PackageId {
        let inner = PackageIdInner { name, version, source_id };
        let ids = PACKAGE_IDS.get_or_init(Sharded::new);
        let mut hasher = blaze_ids::Hasher::of(&PACKAGE_ID);
        hasher.std(&inner);
        let at = ids.intern(hasher.finish(), || inner);
        PackageId { inner: ids.get(at) }
    }

    pub fn name(self) -> InternedString {
        self.inner.name
    }

    pub fn version(self) -> &'static semver::Version {
        &self.inner.version
    }

    pub fn source_id(self) -> SourceId {
        self.inner.source_id
    }

    pub fn with_source_id(self, source: SourceId) -> PackageId {
        PackageId::new(self.inner.name, self.inner.version.clone(), source)
    }

    pub fn map_source(self, to_replace: SourceId, replace_with: SourceId) -> Self {
        if self.source_id() == to_replace { self.with_source_id(replace_with) } else { self }
    }

    /// `name-version.crate`.
    pub fn tarball_name(&self) -> String {
        format!("{}-{}.crate", self.name(), self.version())
    }

    /// The id as a spec with its version and source, which is how `cargo metadata` writes
    /// package ids: `registry+https://github.com/rust-lang/crates.io-index#serde@1.0.228`,
    /// `path+file:///ws/foo#0.1.0`.
    pub fn to_spec(&self) -> PackageIdSpec {
        PackageIdSpec::new(self.name().as_str().to_owned())
            .with_version(self.version().clone().into())
            .with_url(self.source_id().url().clone())
            .with_kind(self.source_id().kind().clone())
    }

    /// Reads `name version (source-url)`, the form of the old lock files' `[metadata]` keys
    /// and dependency lists, and of [`PackageId::serialized`].
    pub fn from_serialized(string: &str) -> Result<PackageId> {
        let invalid = || anyhow!("invalid serialized PackageId");
        let (name, rest) = string.split_once(' ').ok_or_else(invalid)?;
        let (version, rest) = rest.split_once(' ').ok_or_else(invalid)?;
        let version = version.parse()?;
        let url = rest.strip_prefix('(').and_then(|r| r.strip_suffix(')')).ok_or_else(invalid)?;
        let source_id = SourceId::from_url(url)?;
        Ok(PackageId::new(name.into(), version, source_id))
    }

    /// `name version (source-url)`.
    pub fn serialized(&self) -> String {
        format!("{} {} ({})", self.inner.name, self.inner.version, self.inner.source_id.as_url())
    }
}

/// `name v1.2.3`, with the source after it unless it is crates.io.
impl fmt::Display for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} v{}", self.inner.name, self.inner.version)?;
        if !self.inner.source_id.is_crates_io() {
            write!(f, " ({})", self.inner.source_id)?;
        }
        Ok(())
    }
}

impl fmt::Debug for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PackageId")
            .field("name", &self.inner.name)
            .field("version", &self.inner.version.to_string())
            .field("source", &self.inner.source_id.to_string())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_version_handled_nicely() {
        let repo = SourceId::crates_io();
        for bad in ["1.0", "1", "bar", ""] {
            assert!(PackageId::try_new("foo", bad, repo).is_err(), "{bad}");
        }
    }

    #[test]
    fn display() {
        let id = PackageId::try_new("foo", "1.0.0", SourceId::crates_io()).unwrap();
        assert_eq!(id.to_string(), "foo v1.0.0");
        let git = SourceId::from_url("git+https://example.com/foo#0123456789abcdef").unwrap();
        let id = PackageId::try_new("foo", "1.0.0", git).unwrap();
        assert_eq!(id.to_string(), "foo v1.0.0 (https://example.com/foo#01234567)");
    }

    #[test]
    fn unequal_build_metadata() {
        let repo = SourceId::crates_io();
        let first = PackageId::try_new("foo", "0.0.1+first", repo).unwrap();
        let second = PackageId::try_new("foo", "0.0.1+second", repo).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn equal_across_precise() {
        let locked = SourceId::crates_io().with_locked_precise();
        let a = PackageId::try_new("foo", "1.0.0", locked).unwrap();
        let b = PackageId::try_new("foo", "1.0.0", SourceId::crates_io()).unwrap();
        assert_eq!(a, b);
        assert!(a.source_id().has_locked_precise());
        assert!(!b.source_id().has_locked_precise());
    }

    #[test]
    fn serialized_roundtrip() {
        let s = "serde 1.0.228 (registry+https://github.com/rust-lang/crates.io-index)";
        let id = PackageId::from_serialized(s).unwrap();
        assert_eq!(id.name(), "serde");
        assert!(id.source_id().is_crates_io());
        assert_eq!(id.serialized(), s);
        assert!(PackageId::from_serialized("serde 1.0.228").is_err());
        assert!(PackageId::from_serialized("serde 1.0.228 registry+x").is_err());
    }
}
