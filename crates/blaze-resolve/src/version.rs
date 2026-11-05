//! Version requirements as cargo holds them (`src/util/semver_ext.rs`) and the partial
//! versions of `rust-version` and package id specs (`cargo-util-schemas`'
//! `core/partial_version.rs` and `manifest/rust_version.rs`), of cargo 1.99.0 (5f94df4789).

use std::fmt;

use anyhow::{Result, bail};
use semver::{Comparator, Op, Version, VersionReq};

/// A dependency's version requirement, and what a lock file or `cargo update --precise` pinned
/// it to.
#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub enum OptVersionReq {
    /// No `version` was written (a path or git dependency).
    Any,
    Req(VersionReq),
    /// The version the lock file has, and the requirement as written.
    Locked(Version, VersionReq),
    /// The version `cargo update --precise` asked for, and the requirement as written.
    Precise(Version, VersionReq),
}

pub(crate) fn to_req(version: &Version, op: Op) -> VersionReq {
    VersionReq {
        comparators: vec![Comparator {
            op,
            major: version.major,
            minor: Some(version.minor),
            patch: Some(version.patch),
            pre: version.pre.clone(),
        }],
    }
}

impl OptVersionReq {
    pub fn exact(version: &Version) -> Self {
        OptVersionReq::Req(to_req(version, Op::Exact))
    }

    /// Some registries have versions that differ only in build metadata, which `=x.y.z` does
    /// not tell apart; a locked requirement does.
    pub fn lock_to_exact(version: &Version) -> Self {
        OptVersionReq::Locked(version.clone(), to_req(version, Op::Exact))
    }

    pub fn is_exact(&self) -> bool {
        match self {
            OptVersionReq::Any => false,
            OptVersionReq::Req(req) | OptVersionReq::Precise(_, req) => {
                req.comparators.len() == 1 && {
                    let cmp = &req.comparators[0];
                    cmp.op == Op::Exact && cmp.minor.is_some() && cmp.patch.is_some()
                }
            }
            OptVersionReq::Locked(..) => true,
        }
    }

    pub fn lock_to(&mut self, version: &Version) {
        assert!(self.matches(version), "cannot lock {} to {}", self, version);
        let version = version.clone();
        *self = match self {
            OptVersionReq::Any => OptVersionReq::Locked(version, VersionReq::STAR),
            OptVersionReq::Req(req)
            | OptVersionReq::Locked(_, req)
            | OptVersionReq::Precise(_, req) => OptVersionReq::Locked(version, req.clone()),
        };
    }

    pub fn precise_to(&mut self, version: &Version) {
        let version = version.clone();
        *self = match self {
            OptVersionReq::Any => OptVersionReq::Precise(version, VersionReq::STAR),
            OptVersionReq::Req(req)
            | OptVersionReq::Locked(_, req)
            | OptVersionReq::Precise(_, req) => OptVersionReq::Precise(version, req.clone()),
        };
    }

    pub fn is_precise(&self) -> bool {
        matches!(self, OptVersionReq::Precise(..))
    }

    pub fn precise_version(&self) -> Option<&Version> {
        match self {
            OptVersionReq::Precise(version, _) => Some(version),
            _ => None,
        }
    }

    pub fn is_locked(&self) -> bool {
        matches!(self, OptVersionReq::Locked(..))
    }

    pub fn locked_version(&self) -> Option<&Version> {
        match self {
            OptVersionReq::Locked(version, _) => Some(version),
            _ => None,
        }
    }

    pub fn matches(&self, version: &Version) -> bool {
        match self {
            OptVersionReq::Any => true,
            OptVersionReq::Req(req) => req.matches(version),
            // A lock file is reproduced exactly, build metadata included: `1.0.0+bar` locked
            // never becomes `1.0.0+foo`.
            OptVersionReq::Locked(v, _) => v == version,
            // `--precise` without build metadata takes any; with it, only that.
            OptVersionReq::Precise(v, _) => {
                v.major == version.major
                    && v.minor == version.minor
                    && v.patch == version.patch
                    && v.pre == version.pre
                    && (v.build == version.build || v.build.is_empty())
            }
        }
    }
}

impl fmt::Display for OptVersionReq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OptVersionReq::Any => f.write_str("*"),
            OptVersionReq::Req(req)
            | OptVersionReq::Locked(_, req)
            | OptVersionReq::Precise(_, req) => fmt::Display::fmt(req, f),
        }
    }
}

impl From<VersionReq> for OptVersionReq {
    fn from(req: VersionReq) -> Self {
        OptVersionReq::Req(req)
    }
}

/// A version with the minor and patch optional, as in `foo@1.2` and `rust-version = "1.70"`.
#[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Debug)]
pub struct PartialVersion {
    pub major: u64,
    pub minor: Option<u64>,
    pub patch: Option<u64>,
    pub pre: Option<semver::Prerelease>,
    pub build: Option<semver::BuildMetadata>,
}

impl PartialVersion {
    pub fn to_version(&self) -> Option<Version> {
        Some(Version {
            major: self.major,
            minor: self.minor?,
            patch: self.patch?,
            pre: self.pre.clone().unwrap_or_default(),
            build: self.build.clone().unwrap_or_default(),
        })
    }

    pub fn to_caret_req(&self) -> VersionReq {
        VersionReq {
            comparators: vec![Comparator {
                op: Op::Caret,
                major: self.major,
                minor: self.minor,
                patch: self.patch,
                pre: self.pre.clone().unwrap_or_default(),
            }],
        }
    }

    /// Whether `version` is this one, build metadata included where this has some. A
    /// prerelease matches only a version that names a prerelease.
    pub fn matches(&self, version: &Version) -> bool {
        if !version.pre.is_empty() && self.pre.is_none() {
            return false;
        }
        self.major == version.major
            && self.minor.is_none_or(|f| f == version.minor)
            && self.patch.is_none_or(|f| f == version.patch)
            && self.pre.as_ref().is_none_or(|f| f == &version.pre)
            && self.build.as_ref().is_none_or(|f| f == &version.build)
    }
}

impl From<Version> for PartialVersion {
    fn from(ver: Version) -> Self {
        PartialVersion {
            major: ver.major,
            minor: Some(ver.minor),
            patch: Some(ver.patch),
            pre: (!ver.pre.is_empty()).then_some(ver.pre),
            build: (!ver.build.is_empty()).then_some(ver.build),
        }
    }
}

impl std::str::FromStr for PartialVersion {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        if let Ok(ver) = Version::parse(value) {
            return Ok(ver.into());
        }
        // A one-comparator caret requirement without the `^` is a partial version.
        let mut version_req = match VersionReq::parse(value) {
            Ok(req) => req,
            Err(_) if value.contains('-') => {
                bail!("unexpected prerelease field, expected a version like \"1.32\"")
            }
            Err(_) if value.contains('+') => {
                bail!("unexpected build field, expected a version like \"1.32\"")
            }
            Err(_) => bail!("expected a version like \"1.32\""),
        };
        if version_req.comparators.len() != 1 {
            bail!("unexpected version requirement, expected a version like \"1.32\"")
        }
        let comp = version_req.comparators.pop().unwrap();
        if comp.op != Op::Caret || value.starts_with('^') {
            bail!("unexpected version requirement, expected a version like \"1.32\"")
        }
        Ok(PartialVersion {
            major: comp.major,
            minor: comp.minor,
            patch: comp.patch,
            pre: (!comp.pre.is_empty()).then_some(comp.pre),
            build: None,
        })
    }
}

impl fmt::Display for PartialVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.major)?;
        if let Some(minor) = self.minor {
            write!(f, ".{minor}")?;
        }
        if let Some(patch) = self.patch {
            write!(f, ".{patch}")?;
        }
        if let Some(pre) = self.pre.as_ref() {
            write!(f, "-{pre}")?;
        }
        if let Some(build) = self.build.as_ref() {
            write!(f, "+{build}")?;
        }
        Ok(())
    }
}

/// A package's `rust-version`: a partial version with no prerelease or build metadata.
#[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Debug)]
pub struct RustVersion {
    major: u64,
    minor: Option<u64>,
    patch: Option<u64>,
}

impl RustVersion {
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        RustVersion { major, minor: Some(minor), patch: Some(patch) }
    }

    /// Whether a toolchain of version `rustc` (its prerelease ignored) meets this.
    pub fn is_compatible_with(&self, rustc: &PartialVersion) -> bool {
        let msrv = self.to_partial().to_caret_req();
        let rustc = Version {
            major: rustc.major,
            minor: rustc.minor.unwrap_or_default(),
            patch: rustc.patch.unwrap_or_default(),
            pre: Default::default(),
            build: Default::default(),
        };
        msrv.matches(&rustc)
    }

    pub fn to_partial(&self) -> PartialVersion {
        PartialVersion {
            major: self.major,
            minor: self.minor,
            patch: self.patch,
            pre: None,
            build: None,
        }
    }
}

impl std::str::FromStr for RustVersion {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let partial: PartialVersion = value.parse()?;
        if partial.pre.is_some() {
            bail!("unexpected prerelease field, expected a version like \"1.32\"")
        }
        if partial.build.is_some() {
            bail!("unexpected build field, expected a version like \"1.32\"")
        }
        Ok(RustVersion { major: partial.major, minor: partial.minor, patch: partial.patch })
    }
}

impl fmt::Display for RustVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_partial().fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }

    #[test]
    fn locked_matches_build_metadata_exactly() {
        let mut req = OptVersionReq::Req("^1".parse().unwrap());
        req.lock_to(&v("1.0.0+bar"));
        assert!(req.is_locked() && req.is_exact());
        assert!(req.matches(&v("1.0.0+bar")));
        assert!(!req.matches(&v("1.0.0+foo")));
        assert_eq!(req.to_string(), "^1");
        assert_eq!(req.locked_version(), Some(&v("1.0.0+bar")));
    }

    #[test]
    fn precise_ignores_metadata_unless_given() {
        let mut req = OptVersionReq::Any;
        req.precise_to(&v("1.0.0"));
        assert!(req.matches(&v("1.0.0+anything")));
        assert_eq!(req.to_string(), "*");
        req.precise_to(&v("1.0.0+a"));
        assert!(req.matches(&v("1.0.0+a")));
        assert!(!req.matches(&v("1.0.0+b")));
    }

    #[test]
    #[should_panic(expected = "cannot lock")]
    fn lock_outside_requirement() {
        OptVersionReq::Req("^1".parse().unwrap()).lock_to(&v("2.0.0"));
    }

    #[test]
    fn exactness() {
        assert!(OptVersionReq::exact(&v("1.2.3")).is_exact());
        assert!(!OptVersionReq::Req("=1.2".parse().unwrap()).is_exact());
        assert!(!OptVersionReq::Any.is_exact());
    }

    // cargo's `is_compatible_with_rustc`.
    #[test]
    fn rust_version_compatibility() {
        let cases = [
            ("1", "1.70.0", true),
            ("1.30", "1.70.0", true),
            ("1.30.10", "1.70.0", true),
            ("1.70", "1.70.0", true),
            ("1.70.0", "1.70.0", true),
            ("1.70.1", "1.70.0", false),
            ("1.70", "1.70.0-nightly", true),
            ("1.70.0", "1.70.0-nightly", true),
            ("1.71", "1.70.0", false),
            ("2", "1.70.0", false),
        ];
        for (msrv, rustc, expected) in cases {
            let msrv: RustVersion = msrv.parse().unwrap();
            let rustc = PartialVersion::from(v(rustc));
            assert_eq!(msrv.is_compatible_with(&rustc), expected, "{msrv} with {rustc}");
        }
    }

    #[test]
    fn partial_versions() {
        for ok in ["1", "1.2", "1.2.3", "1.2.3-alpha.1", "1.2.3+build"] {
            assert_eq!(ok.parse::<PartialVersion>().unwrap().to_string(), ok);
        }
        for bad in ["^1.2", "1.2, 1.3", ">=1.2", "x", "1.2-", ""] {
            assert!(bad.parse::<PartialVersion>().is_err(), "{bad}");
        }
        assert!("1.70-beta".parse::<RustVersion>().is_err());
        assert!("1.70.0+x".parse::<RustVersion>().is_err());
        assert_eq!("1.70".parse::<RustVersion>().unwrap().to_string(), "1.70");
    }

    #[test]
    fn partial_version_matches() {
        let p: PartialVersion = "1.2".parse().unwrap();
        assert!(p.matches(&v("1.2.9")));
        assert!(!p.matches(&v("1.3.0")));
        assert!(!p.matches(&v("1.2.0-alpha")));
        let p: PartialVersion = "1.2.0-alpha".parse().unwrap();
        assert!(p.matches(&v("1.2.0-alpha")));
    }
}
