//! A package id specification (`-p`, `[replace]`, `cargo metadata`'s ids):
//! `cargo-util-schemas`' `core/package_id_spec.rs` and `src/workspace/package_id_spec.rs` of
//! cargo 1.99.0 (5f94df4789).

use std::fmt;

use anyhow::{Context as _, Result, anyhow, bail};
use url::Url;

use crate::names::validate_package_name;
use crate::package_id::PackageId;
use crate::source_id::{GitReference, SourceKind};
use crate::version::PartialVersion;

/// A name with an optional partial version, source URL and source kind.
#[derive(Clone, PartialEq, Eq, Debug, Hash, Ord, PartialOrd)]
pub struct PackageIdSpec {
    name: String,
    version: Option<PartialVersion>,
    url: Option<Url>,
    kind: Option<SourceKind>,
}

impl PackageIdSpec {
    pub fn new(name: String) -> Self {
        PackageIdSpec { name, version: None, url: None, kind: None }
    }

    pub fn with_version(mut self, version: PartialVersion) -> Self {
        self.version = Some(version);
        self
    }

    pub fn with_url(mut self, url: Url) -> Self {
        self.url = Some(url);
        self
    }

    pub fn with_kind(mut self, kind: SourceKind) -> Self {
        self.kind = Some(kind);
        self
    }

    /// Reads `foo`, `foo@1.4`, `foo:1.4.3`, `https://github.com/rust-lang/crates.io-index#foo@1.4.3`,
    /// `git+ssh://git@host/foo.git?branch=dev#foo@1.4.3`, `path+file:///ws/foo#1.1.8`.
    pub fn parse(spec: &str) -> Result<PackageIdSpec> {
        if spec.contains("://") {
            if let Ok(url) = Url::parse(spec) {
                return PackageIdSpec::from_url(url);
            }
        } else if spec.contains('/') || spec.contains('\\') {
            let abs = std::env::current_dir().unwrap_or_default().join(spec);
            if abs.exists() {
                let maybe_url = blaze_source::file_url(abs)
                    .map_or_else(|_| "a file:// URL".to_owned(), |url| url.to_string());
                bail!(
                    "package ID specification `{spec}` looks like a file path, maybe try {maybe_url}"
                );
            }
        }
        let (name, version) = parse_spec(spec)?.unwrap_or_else(|| (spec.to_owned(), None));
        validate_package_name(&name)?;
        Ok(PackageIdSpec { name, version, url: None, kind: None })
    }

    fn from_url(mut url: Url) -> Result<PackageIdSpec> {
        let mut kind = None;
        if let Some((kind_str, scheme)) = url.scheme().split_once('+') {
            match kind_str {
                "git" => {
                    let git_ref = GitReference::from_query(url.query_pairs());
                    url.set_query(None);
                    kind = Some(SourceKind::Git(git_ref));
                    url = strip_url_protocol(&url);
                }
                "registry" => {
                    if url.query().is_some() {
                        bail!("cannot have a query string in a pkgid: {url}");
                    }
                    kind = Some(SourceKind::Registry);
                    url = strip_url_protocol(&url);
                }
                // `sparse+` stays part of the URL, as in a `SourceId`.
                "sparse" => {
                    if url.query().is_some() {
                        bail!("cannot have a query string in a pkgid: {url}");
                    }
                    kind = Some(SourceKind::SparseRegistry);
                }
                "path" => {
                    if url.query().is_some() {
                        bail!("cannot have a query string in a pkgid: {url}");
                    }
                    if scheme != "file" {
                        bail!(
                            "`path+{scheme}` is unsupported; `path+file` and `file` schemes are supported"
                        );
                    }
                    kind = Some(SourceKind::Path);
                    url = strip_url_protocol(&url);
                }
                kind => bail!("unsupported source protocol: {kind}"),
            }
        } else if url.query().is_some() {
            bail!("cannot have a query string in a pkgid: {url}");
        }

        let frag = url.fragment().map(|s| s.to_owned());
        url.set_fragment(None);

        let (name, version) = {
            let Some(path_name) = url.path_segments().and_then(|mut p| p.next_back()) else {
                bail!("pkgid urls must have at least one path component: {url}");
            };
            match frag {
                Some(fragment) => match parse_spec(&fragment)? {
                    Some((name, ver)) => (name, ver),
                    None => {
                        let Some(f) = fragment.chars().next() else {
                            bail!("pkgid url cannot have an empty fragment");
                        };
                        if f.is_alphabetic() {
                            (fragment, None)
                        } else {
                            (path_name.to_owned(), Some(fragment.parse::<PartialVersion>()?))
                        }
                    }
                },
                None => (path_name.to_owned(), None),
            }
        };
        validate_package_name(&name)?;
        Ok(PackageIdSpec { name, version, url: Some(url), kind })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn version(&self) -> Option<semver::Version> {
        self.version.as_ref().and_then(|v| v.to_version())
    }

    pub fn partial_version(&self) -> Option<&PartialVersion> {
        self.version.as_ref()
    }

    pub fn url(&self) -> Option<&Url> {
        self.url.as_ref()
    }

    pub fn kind(&self) -> Option<&SourceKind> {
        self.kind.as_ref()
    }

    pub fn matches(&self, package_id: PackageId) -> bool {
        if self.name() != package_id.name().as_str() {
            return false;
        }
        if let Some(v) = &self.version
            && !v.matches(package_id.version())
        {
            return false;
        }
        if let Some(u) = &self.url
            && u != package_id.source_id().url()
        {
            return false;
        }
        if let Some(k) = &self.kind
            && k != package_id.source_id().kind()
        {
            return false;
        }
        true
    }

    /// The package of `all_ids` given by `spec`, a spec yet to be parsed: cargo's
    /// `PackageIdSpecQuery::query_str`.
    pub fn query_str(spec: &str, all_ids: &[PackageId]) -> Result<PackageId> {
        let parsed = PackageIdSpec::parse(spec).with_context(|| {
            let suggestion =
                crate::closest_msg(spec, all_ids.iter(), |id| id.name().as_str(), "package");
            format!("invalid package ID specification: `{spec}`{suggestion}")
        })?;
        parsed.query(all_ids)
    }

    /// The one of `all_ids` this spec matches, or why there is not one: cargo's
    /// `PackageIdSpecQuery::query`.
    pub fn query(&self, all_ids: &[PackageId]) -> Result<PackageId> {
        let spec = self;
        let mut ids = all_ids.iter().copied().filter(|&id| spec.matches(id));
        let Some(ret) = ids.next() else {
            let mut suggestion = String::new();
            let try_spec = |tried: PackageIdSpec, suggestion: &mut String| {
                let try_matches: Vec<PackageId> =
                    all_ids.iter().copied().filter(|&id| tried.matches(id)).collect();
                if !try_matches.is_empty() {
                    suggestion.push_str("\nhelp: there are similar package ID specifications:\n");
                    minimize(suggestion, &try_matches, spec);
                }
            };
            if spec.url().is_some() {
                let tried = PackageIdSpec::new(spec.name().to_owned());
                let tried = match spec.partial_version().cloned() {
                    Some(version) => tried.with_version(version),
                    None => tried,
                };
                try_spec(tried, &mut suggestion);
            }
            if suggestion.is_empty() && spec.version().is_some() {
                try_spec(PackageIdSpec::new(spec.name().to_owned()), &mut suggestion);
            }
            if suggestion.is_empty() {
                suggestion.push_str(&crate::closest_msg(
                    spec.name(),
                    all_ids.iter(),
                    |id| id.name().as_str(),
                    "package",
                ));
            }
            bail!("package ID specification `{spec}` did not match any packages{suggestion}");
        };
        match ids.next() {
            Some(other) => {
                let mut msg = format!(
                    "specification `{spec}` is ambiguous\nhelp: re-run this command with one of the following specifications",
                );
                let mut all = vec![ret, other];
                all.extend(ids);
                minimize(&mut msg, &all, spec);
                Err(anyhow!("{msg}"))
            }
            None => Ok(ret),
        }
    }
}

/// Of the ids `spec` matches, each by the least that tells it apart.
fn minimize(msg: &mut String, ids: &[PackageId], spec: &PackageIdSpec) {
    let mut version_cnt: blaze_hash::HashMap<&semver::Version, usize, std::alloc::Global> =
        Default::default();
    for id in ids {
        *version_cnt.entry(id.version()).or_insert(0) += 1;
    }
    for id in ids {
        if version_cnt[id.version()] == 1 {
            msg.push_str(&format!("\n  {}@{}", spec.name(), id.version()));
        } else {
            msg.push_str(&format!("\n  {}", id.to_spec()));
        }
    }
}

fn parse_spec(spec: &str) -> Result<Option<(String, Option<PartialVersion>)>> {
    let Some((name, ver)) =
        spec.rsplit_once('@').or_else(|| spec.rsplit_once(':').filter(|(n, _)| !n.ends_with(':')))
    else {
        return Ok(None);
    };
    Ok(Some((name.to_owned(), Some(ver.parse::<PartialVersion>()?))))
}

fn strip_url_protocol(url: &Url) -> Url {
    // `Url::set_scheme` refuses to change to http or https.
    let raw = url.to_string();
    raw.split_once('+').unwrap().1.parse().unwrap()
}

impl serde::Serialize for PackageIdSpec {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        self.to_string().serialize(s)
    }
}

impl fmt::Display for PackageIdSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut printed_name = false;
        match self.url {
            Some(ref url) => {
                if let Some(protocol) = self.kind.as_ref().and_then(|k| k.protocol()) {
                    write!(f, "{protocol}+")?;
                }
                write!(f, "{}", url)?;
                if let Some(SourceKind::Git(git_ref)) = self.kind.as_ref()
                    && let Some(pretty) = git_ref.pretty_ref(true)
                {
                    write!(f, "?{}", pretty)?;
                }
                if url.path_segments().unwrap().next_back().unwrap() != self.name {
                    printed_name = true;
                    write!(f, "#{}", self.name)?;
                }
            }
            None => {
                printed_name = true;
                write!(f, "{}", self.name)?;
            }
        }
        if let Some(ref v) = self.version {
            write!(f, "{}{}", if printed_name { "@" } else { "#" }, v)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_id::SourceId;

    // cargo-util-schemas' `good_parsing`, a selection with every shape.
    #[test]
    fn good_parsing() {
        #[track_caller]
        fn ok(spec: &str, expected_display: &str) {
            let parsed = PackageIdSpec::parse(spec).unwrap();
            assert_eq!(parsed.to_string(), expected_display, "{spec}");
            let reparsed = PackageIdSpec::parse(&parsed.to_string()).unwrap();
            assert_eq!(parsed, reparsed, "{spec}");
        }
        ok("https://crates.io/foo", "https://crates.io/foo");
        ok("https://crates.io/foo#1.2.3", "https://crates.io/foo#1.2.3");
        ok("https://crates.io/foo#1.2", "https://crates.io/foo#1.2");
        ok("https://crates.io/foo#bar:1.2.3", "https://crates.io/foo#bar@1.2.3");
        ok("https://crates.io/foo#bar@1.2.3", "https://crates.io/foo#bar@1.2.3");
        ok("https://crates.io/foo#bar", "https://crates.io/foo#bar");
        ok("registry+https://crates.io/foo#bar@1.2.3", "registry+https://crates.io/foo#bar@1.2.3");
        ok("sparse+https://crates.io/foo#bar@1.2.3", "sparse+https://crates.io/foo#bar@1.2.3");
        ok("foo", "foo");
        ok("foo::bar", "foo::bar");
        ok("foo:1.2.3", "foo@1.2.3");
        ok("foo@1.2.3", "foo@1.2.3");
        ok("foo@1.2", "foo@1.2");
        ok(
            "git+ssh://git@host/foo?branch=dev#foo@1.2.3",
            "git+ssh://git@host/foo?branch=dev#1.2.3",
        );
        ok("git+ssh://git@host/foo?tag=v1#bar@1.2.3", "git+ssh://git@host/foo?tag=v1#bar@1.2.3");
        ok("file:///path/to/my/project/foo", "file:///path/to/my/project/foo");
        ok(
            "path+file:///path/to/my/project/foo#1.1.8",
            "path+file:///path/to/my/project/foo#1.1.8",
        );
        ok("path+file:///path/to/my/project/foo#bar", "path+file:///path/to/my/project/foo#bar");
    }

    #[test]
    fn bad_parsing() {
        for bad in [
            "baz:",
            "baz:*",
            "baz@",
            "baz@*",
            "baz@^1.0",
            "https://baz:1.0",
            "https://#baz:1.0",
            "foobar+https://github.com/rust-lang/crates.io-index",
            "path+https://github.com/rust-lang/crates.io-index",
            "registry+https://github.com/rust-lang/crates.io-index?a=b",
            "https://crates.io/foo#",
            "@1.2.3",
        ] {
            assert!(PackageIdSpec::parse(bad).is_err(), "{bad}");
        }
    }

    // cargo's `matching`.
    #[test]
    fn matching() {
        let sid = SourceId::crates_io();
        let foo = PackageId::try_new("foo", "1.2.3", sid).unwrap();
        let pre = PackageId::try_new("foo", "1.2.3-alpha.0", sid).unwrap();
        let meta = PackageId::try_new("meta", "1.2.3+hello", sid).unwrap();
        let m = |spec: &str, id| PackageIdSpec::parse(spec).unwrap().matches(id);

        assert!(m("foo", foo));
        assert!(!m("bar", foo));
        assert!(m("foo:1.2.3", foo));
        assert!(!m("foo:1.2.2", foo));
        assert!(m("foo@1.2", foo));
        assert!(m("foo@1", foo));
        assert!(!m("foo@1.2", pre));
        assert!(m("foo@1.2.3-alpha.0", pre));
        assert!(m("meta@1.2.3", meta));
        assert!(m("meta@1.2.3+hello", meta));
        assert!(!m("meta@1.2.3+bye", meta));
        assert!(m("https://github.com/rust-lang/crates.io-index#foo@1.2.3", foo));
        assert!(m("registry+https://github.com/rust-lang/crates.io-index#foo@1.2.3", foo));
        assert!(!m("sparse+https://github.com/rust-lang/crates.io-index#foo@1.2.3", foo));
        assert!(!m("https://example.com#foo", foo));
    }

    #[cfg(unix)]
    #[test]
    fn package_id_to_spec() {
        let crates_io = PackageId::try_new("serde", "1.0.228", SourceId::crates_io()).unwrap();
        assert_eq!(
            crates_io.to_spec().to_string(),
            "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.228"
        );
        let path = SourceId::for_path(std::path::Path::new("/ws/foo")).unwrap();
        let id = PackageId::try_new("foo", "0.1.0", path).unwrap();
        assert_eq!(id.to_spec().to_string(), "path+file:///ws/foo#0.1.0");
        let id = PackageId::try_new("bar", "0.1.0", path).unwrap();
        assert_eq!(id.to_spec().to_string(), "path+file:///ws/foo#bar@0.1.0");
        let git = SourceId::from_url("git+https://github.com/a/b?branch=x y#0123abc").unwrap();
        let id = PackageId::try_new("b", "0.2.0", git).unwrap();
        assert_eq!(id.to_spec().to_string(), "git+https://github.com/a/b?branch=x+y#0.2.0");
    }
}
