//! A dependency as a manifest or an index entry asks for it: `src/workspace/dependency.rs` of
//! cargo 1.99.0 (5f94df4789). Artifact dependencies (`-Z bindeps`) are not modelled.

use std::fmt;
use std::sync::Arc;

use anyhow::Result;
use cargo_platform::Platform;
use semver::VersionReq;

use crate::interning::InternedString;
use crate::package_id::PackageId;
use crate::source_id::SourceId;
use crate::summary::Summary;
use crate::version::OptVersionReq;

/// Cheap to clone; changed through its setters, which copy on write.
#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub struct Dependency {
    inner: Arc<Inner>,
}

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
struct Inner {
    /// The name of the package depended on (the `package` key if renamed).
    name: InternedString,
    source_id: SourceId,
    /// The registry the manifest named (`registry = "..."`), where it differs from
    /// `source_id`, as when a path dependency also names a registry to publish against.
    registry_id: Option<SourceId>,
    req: OptVersionReq,
    specified_req: bool,
    kind: DepKind,
    /// Matches any version from any source: an override of `[replace]`.
    only_match_name: bool,
    explicit_name_in_toml: Option<InternedString>,
    optional: bool,
    public: bool,
    default_features: bool,
    features: Vec<InternedString>,
    /// `None` is every platform.
    platform: Option<Platform>,
}

#[derive(PartialEq, Eq, Hash, Ord, PartialOrd, Clone, Debug, Copy)]
pub enum DepKind {
    Normal,
    Development,
    Build,
}

impl serde::Serialize for DepKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match *self {
            DepKind::Normal => None,
            DepKind::Development => Some("dev"),
            DepKind::Build => Some("build"),
        }
        .serialize(s)
    }
}

impl DepKind {
    pub fn kind_table(&self) -> &'static str {
        match self {
            DepKind::Normal => "dependencies",
            DepKind::Development => "dev-dependencies",
            DepKind::Build => "build-dependencies",
        }
    }

    /// `null`, `"dev"` or `"build"`, as `cargo metadata` writes a dependency's kind.
    pub fn metadata_name(&self) -> Option<&'static str> {
        match self {
            DepKind::Normal => None,
            DepKind::Development => Some("dev"),
            DepKind::Build => Some("build"),
        }
    }
}

impl Dependency {
    /// A dependency on `name` (the package's name, not a rename) with the requirement
    /// `version` as written, or any version if there is none.
    pub fn parse(
        name: impl Into<InternedString>,
        version: Option<&str>,
        source_id: SourceId,
    ) -> Result<Dependency> {
        let name = name.into();
        let (specified_req, version_req) = match version {
            Some(v) => match VersionReq::parse(v) {
                Ok(req) => (true, OptVersionReq::Req(req)),
                Err(err) => {
                    return Err(anyhow::Error::new(err).context(format!(
                        "failed to parse the version requirement `{}` for dependency `{}`",
                        v, name,
                    )));
                }
            },
            None => (false, OptVersionReq::Any),
        };

        let mut ret = Dependency::new_override(name, source_id);
        {
            let ptr = Arc::make_mut(&mut ret.inner);
            ptr.only_match_name = false;
            ptr.req = version_req;
            ptr.specified_req = specified_req;
        }
        Ok(ret)
    }

    /// A dependency matching `name` at any version from any source.
    pub fn new_override(name: InternedString, source_id: SourceId) -> Dependency {
        assert!(!name.is_empty());
        Dependency {
            inner: Arc::new(Inner {
                name,
                source_id,
                registry_id: None,
                req: OptVersionReq::Any,
                kind: DepKind::Normal,
                only_match_name: true,
                optional: false,
                public: false,
                features: Vec::new(),
                default_features: true,
                specified_req: false,
                platform: None,
                explicit_name_in_toml: None,
            }),
        }
    }

    pub fn version_req(&self) -> &OptVersionReq {
        &self.inner.req
    }

    /// The key in `[dependencies]`: the rename if there is one, else the package's name.
    pub fn name_in_toml(&self) -> InternedString {
        self.explicit_name_in_toml().unwrap_or(self.inner.name)
    }

    /// The name of the package depended on.
    pub fn package_name(&self) -> InternedString {
        self.inner.name
    }

    pub fn source_id(&self) -> SourceId {
        self.inner.source_id
    }

    pub fn registry_id(&self) -> Option<SourceId> {
        self.inner.registry_id
    }

    pub fn set_registry_id(&mut self, registry_id: SourceId) -> &mut Dependency {
        Arc::make_mut(&mut self.inner).registry_id = Some(registry_id);
        self
    }

    pub fn kind(&self) -> DepKind {
        self.inner.kind
    }

    pub fn is_public(&self) -> bool {
        self.inner.public
    }

    pub fn set_public(&mut self, public: bool) -> &mut Dependency {
        if public {
            assert_eq!(self.kind(), DepKind::Normal);
        }
        Arc::make_mut(&mut self.inner).public = public;
        self
    }

    /// Whether the manifest wrote a version requirement.
    pub fn specified_req(&self) -> bool {
        self.inner.specified_req
    }

    pub fn platform(&self) -> Option<&Platform> {
        self.inner.platform.as_ref()
    }

    pub fn explicit_name_in_toml(&self) -> Option<InternedString> {
        self.inner.explicit_name_in_toml
    }

    /// The keys of this entry in `Cargo.toml`, as `["target", "cfg(unix)", "dependencies", "foo"]`.
    pub fn toml_path(&self) -> Vec<String> {
        let mut path = Vec::new();
        if let Some(platform) = self.platform() {
            path.push("target".to_owned());
            path.push(platform.to_string());
        }
        path.push(self.kind().kind_table().to_owned());
        path.push(self.name_in_toml().as_str().to_owned());
        path
    }

    pub fn set_kind(&mut self, kind: DepKind) -> &mut Dependency {
        if self.is_public() {
            assert_eq!(kind, DepKind::Normal);
        }
        Arc::make_mut(&mut self.inner).kind = kind;
        self
    }

    pub fn set_features(
        &mut self,
        features: impl IntoIterator<Item = impl Into<InternedString>>,
    ) -> &mut Dependency {
        Arc::make_mut(&mut self.inner).features = features.into_iter().map(|s| s.into()).collect();
        self
    }

    pub fn set_default_features(&mut self, default_features: bool) -> &mut Dependency {
        Arc::make_mut(&mut self.inner).default_features = default_features;
        self
    }

    pub fn set_optional(&mut self, optional: bool) -> &mut Dependency {
        Arc::make_mut(&mut self.inner).optional = optional;
        self
    }

    pub fn set_source_id(&mut self, id: SourceId) -> &mut Dependency {
        Arc::make_mut(&mut self.inner).source_id = id;
        self
    }

    pub fn set_version_req(&mut self, req: OptVersionReq) -> &mut Dependency {
        Arc::make_mut(&mut self.inner).req = req;
        self
    }

    pub fn set_platform(&mut self, platform: Option<Platform>) -> &mut Dependency {
        Arc::make_mut(&mut self.inner).platform = platform;
        self
    }

    pub fn set_explicit_name_in_toml(
        &mut self,
        name: impl Into<InternedString>,
    ) -> &mut Dependency {
        Arc::make_mut(&mut self.inner).explicit_name_in_toml = Some(name.into());
        self
    }

    /// Pins this dependency to `id`, as a lock file does: its version exactly, and its source's
    /// precise revision.
    pub fn lock_to(&mut self, id: PackageId) -> &mut Dependency {
        assert_eq!(self.inner.source_id, id.source_id());
        let me = Arc::make_mut(&mut self.inner);
        me.req.lock_to(id.version());
        me.source_id = me.source_id.with_precise_from(id.source_id());
        self
    }

    /// Pins the version only, as `[patch]` and `[replace]` do.
    pub fn lock_version(&mut self, version: &semver::Version) -> &mut Dependency {
        Arc::make_mut(&mut self.inner).req.lock_to(version);
        self
    }

    pub fn is_locked(&self) -> bool {
        self.inner.req.is_locked()
    }

    /// Whether it is needed by those depending on this package: not a dev-dependency.
    pub fn is_transitive(&self) -> bool {
        match self.inner.kind {
            DepKind::Normal | DepKind::Build => true,
            DepKind::Development => false,
        }
    }

    pub fn is_build(&self) -> bool {
        matches!(self.inner.kind, DepKind::Build)
    }

    pub fn is_optional(&self) -> bool {
        self.inner.optional
    }

    pub fn uses_default_features(&self) -> bool {
        self.inner.default_features
    }

    pub fn features(&self) -> &[InternedString] {
        &self.inner.features
    }

    pub fn matches(&self, sum: &Summary) -> bool {
        self.matches_id(sum.package_id())
    }

    pub fn matches_ignoring_source(&self, id: PackageId) -> bool {
        self.package_name() == id.name() && self.version_req().matches(id.version())
    }

    pub fn matches_id(&self, id: PackageId) -> bool {
        self.inner.name == id.name()
            && (self.inner.only_match_name
                || (self.inner.req.matches(id.version()) && self.inner.source_id == id.source_id()))
    }

    pub fn map_source(mut self, to_replace: SourceId, replace_with: SourceId) -> Dependency {
        if self.source_id() == to_replace {
            self.set_source_id(replace_with);
        }
        self
    }
}

impl fmt::Display for DepKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DepKind::Normal => "normal",
            DepKind::Development => "dev",
            DepKind::Build => "build",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renamed_dependency() {
        let mut dep = Dependency::parse("serde", Some("1.0"), SourceId::crates_io()).unwrap();
        assert_eq!(dep.name_in_toml(), "serde");
        dep.set_explicit_name_in_toml("serde1");
        assert_eq!(dep.name_in_toml(), "serde1");
        assert_eq!(dep.package_name(), "serde");
        assert!(dep.specified_req());
        assert_eq!(dep.version_req().to_string(), "^1.0");
    }

    #[test]
    fn bad_requirement() {
        let err = Dependency::parse("foo", Some("1.0.0.0"), SourceId::crates_io()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "failed to parse the version requirement `1.0.0.0` for dependency `foo`"
        );
        // semver accepts what cargo always has: `1.*`, `>= 1.2, < 1.5`, bare `1`.
        for ok in ["1.*", ">= 1.2, < 1.5", "1", "*", "~1.2.3", "=1.0.0-alpha.1"] {
            Dependency::parse("foo", Some(ok), SourceId::crates_io()).unwrap();
        }
    }

    #[test]
    fn matching_and_locking() {
        let sid = SourceId::crates_io();
        let mut dep = Dependency::parse("foo", Some("^1.2"), sid).unwrap();
        let v13 = PackageId::try_new("foo", "1.3.0", sid).unwrap();
        let v20 = PackageId::try_new("foo", "2.0.0", sid).unwrap();
        let other = PackageId::try_new("foo", "1.3.0", SourceId::crates_io_sparse()).unwrap();
        assert!(dep.matches_id(v13));
        assert!(!dep.matches_id(v20));
        assert!(!dep.matches_id(other));
        assert!(dep.matches_ignoring_source(other));

        let locked = PackageId::try_new("foo", "1.3.0", sid.with_locked_precise()).unwrap();
        dep.lock_to(locked);
        assert!(dep.is_locked());
        assert!(dep.source_id().has_locked_precise());
        assert!(!dep.matches_id(PackageId::try_new("foo", "1.4.0", sid).unwrap()));
        assert_eq!(dep.version_req().to_string(), "^1.2");
    }

    #[test]
    fn override_matches_any_source() {
        let dep = Dependency::new_override("foo".into(), SourceId::crates_io());
        assert!(
            dep.matches_id(
                PackageId::try_new("foo", "9.0.0", SourceId::crates_io_sparse()).unwrap()
            )
        );
    }

    #[test]
    fn toml_path() {
        let mut dep = Dependency::parse("foo", None, SourceId::crates_io()).unwrap();
        dep.set_kind(DepKind::Build).set_platform(Some("cfg(unix)".parse().unwrap()));
        assert_eq!(dep.toml_path(), ["target", "cfg(unix)", "build-dependencies", "foo"]);
        assert_eq!(dep.version_req(), &OptVersionReq::Any);
        assert!(dep.is_transitive());
    }
}
