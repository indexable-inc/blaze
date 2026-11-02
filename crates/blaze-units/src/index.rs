//! The lines of a registry's index as cargo 1.99 reads them (`IndexSummary::parse` in
//! `src/sources/registry/index/mod.rs`, cargo-util-schemas' `IndexPackage`), each made the
//! resolver's summary as `index_package_to_summary` and `registry_dependency_into_dep` make it,
//! and the `config.json` at an index's root. One copy for the load (`load::source`, reading the
//! file as served) and v1's blaze-registry (reading it through cargo's cache of it).

use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::ops::Range;
use std::str::FromStr;

use anyhow::{Context as _, bail};
use blaze_hash::HashMap;
use blaze_resolve::{
    DepKind, Dependency, IndexSummary, InternedString, PackageId, Platform, RustVersion, SourceId,
    Summary,
};
use semver::{Version, VersionReq};
use serde::Deserialize;
use serde::de::{self, Deserializer};

pub use blaze_manifest::dependency::Artifact;

#[cfg(test)]
pub(crate) mod tests;

/// The newest schema version of an index line (its `v`) that cargo 1.99 reads without
/// `-Zbindeps`. A line of a newer one is [`Status::Unsupported`].
pub const INDEX_V_MAX: u32 = 2;

/// One version of a package, as a line of the index says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub name: String,
    pub version: Version,
    pub deps: Vec<IndexDependency>,
    /// `features` with `features2` added, as cargo adds them. Unordered: the summary made of
    /// the entry sorts them.
    pub features: HashMap<String, Vec<String>>,
    /// The sha256 of the `.crate` file, in hex.
    pub checksum: String,
    pub links: Option<String>,
    pub rust_version: Option<RustVersion>,
    /// When it was published, `yyyy-mm-ddThh:mm:ssZ`.
    pub pubtime: Option<String>,
}

/// What cargo does with a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Candidate,
    Yanked,
    /// Of a schema newer than [`INDEX_V_MAX`]: skipped by the resolver.
    Unsupported(u32),
    /// Not what an index line is, past its name and version, which are all the entry has:
    /// skipped by the resolver.
    Invalid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexLine {
    pub entry: IndexEntry,
    pub status: Status,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexDependency {
    /// The name the depending package knows it by: the key of its `[dependencies]` entry.
    pub name_in_toml: String,
    /// The package's own name, which differs from `name_in_toml` for a renamed dependency.
    pub package: String,
    pub req: VersionReq,
    /// `req` as the index wrote it, which the resolver's messages quote.
    pub req_text: String,
    /// With the empty features older registries wrote taken out, as cargo takes them out.
    pub features: Vec<String>,
    pub optional: bool,
    pub default_features: bool,
    pub platform: Option<Platform>,
    pub kind: DepKind,
    /// The registry the dependency is from, when it is not this one.
    pub registry: Option<SourceId>,
    pub public: bool,
    /// An artifact dependency (`-Zbindeps`), checked as cargo checks one.
    pub artifact: Option<Artifact>,
}

/// The `config.json` at the root of an index.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct RegistryConfig {
    /// Where a `.crate` file is: a template of `{crate}`, `{version}`, `{prefix}`,
    /// `{lowerprefix}` and `{sha256-checksum}`, or, with none of them, a URL that
    /// `/{crate}/{version}/download` is added to.
    pub dl: String,
    pub api: Option<String>,
    #[serde(default)]
    pub auth_required: bool,
}

impl RegistryConfig {
    pub const NAME: &str = "config.json";
}

/// An index line as JSON: cargo-util-schemas's `IndexPackage`, field for field, with what
/// serde does to a missing, extra or repeated field.
#[derive(Deserialize)]
struct IndexPackage {
    name: String,
    vers: Version,
    deps: Vec<RegistryDependency>,
    #[serde(default)]
    features: HashMap<String, Vec<String>>,
    features2: Option<HashMap<String, Vec<String>>>,
    cksum: String,
    yanked: Option<bool>,
    links: Option<String>,
    #[serde(default, deserialize_with = "rust_version")]
    rust_version: Option<RustVersion>,
    #[serde(default, deserialize_with = "pubtime")]
    pubtime: Option<String>,
    v: Option<u32>,
}

#[derive(Deserialize)]
struct RegistryDependency {
    name: String,
    req: String,
    #[serde(default)]
    features: Vec<String>,
    #[serde(default)]
    optional: bool,
    #[serde(default = "yes")]
    default_features: bool,
    target: Option<String>,
    kind: Option<String>,
    registry: Option<String>,
    package: Option<String>,
    public: Option<bool>,
    artifact: Option<Vec<String>>,
    bindep_target: Option<String>,
    #[serde(default)]
    lib: bool,
}

fn yes() -> bool {
    true
}

/// A string, parsed as a [`RustVersion`]; anything else is an error, as cargo's deserializer
/// takes strings only.
fn rust_version<'de, D: Deserializer<'de>>(de: D) -> Result<Option<RustVersion>, D::Error> {
    let text: Option<String> = Deserialize::deserialize(de)?;
    text.map(|it| it.parse().map_err(de::Error::custom)).transpose()
}

fn pubtime<'de, D: Deserializer<'de>>(de: D) -> Result<Option<String>, D::Error> {
    let text: Option<String> = Deserialize::deserialize(de)?;
    match text {
        Some(text) if !is_pubtime(&text) => {
            Err(de::Error::custom(format!("invalid publish time `{text}`")))
        }
        it => Ok(it),
    }
}

/// Whether `text` is a time as cargo's `parse_pubtime` takes one: `%Y-%m-%dT%H:%M:%SZ`, padded,
/// of a day that exists.
fn is_pubtime(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 20 {
        return false;
    }
    let digits = |range: Range<usize>| -> Option<u32> {
        let part = &text[range];
        part.bytes().all(|it| it.is_ascii_digit()).then(|| part.parse().ok())?
    };
    let shape = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':'), (19, b'Z')];
    if shape.iter().any(|&(at, byte)| bytes[at] != byte) {
        return false;
    }
    let parts = (digits(0..4), digits(5..7), digits(8..10), digits(11..13), digits(14..16));
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute)) = parts else {
        return false;
    };
    let Some(second) = digits(17..19) else { return false };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day) && hour < 24 && minute < 60 && second < 60
}

#[derive(Deserialize)]
struct IndexPackageMinimum {
    name: String,
    vers: Version,
}

#[derive(Deserialize, Default)]
struct IndexPackageRustVersion {
    #[serde(default, deserialize_with = "rust_version")]
    rust_version: Option<RustVersion>,
}

#[derive(Deserialize, Default)]
struct IndexPackageV {
    v: Option<u32>,
}

/// Reads one line of an index file. An error is a line that has no name and version: cargo
/// leaves it out of the file's versions and of its cache.
pub fn parse_line(line: &[u8]) -> anyhow::Result<IndexLine> {
    let full =
        serde_json::from_slice::<IndexPackage>(line).map_err(anyhow::Error::from).and_then(|it| {
            let (v, yanked) = (it.v, it.yanked);
            Ok((v, yanked, entry(it)?))
        });
    let (v, yanked, entry, valid) = match full {
        Ok((v, yanked, entry)) => (v, yanked, entry, true),
        Err(err) => {
            let Ok(IndexPackageMinimum { name, vers }) = serde_json::from_slice(line) else {
                return Err(err);
            };
            let IndexPackageRustVersion { rust_version } =
                serde_json::from_slice(line).unwrap_or_default();
            let IndexPackageV { v } = serde_json::from_slice(line).unwrap_or_default();
            let entry = IndexEntry {
                name,
                version: vers,
                deps: Vec::new(),
                features: HashMap::default(),
                checksum: String::new(),
                links: None,
                rust_version,
                pubtime: None,
            };
            (v, None, entry, false)
        }
    };
    let v = v.unwrap_or(1);
    let status = if v > INDEX_V_MAX {
        Status::Unsupported(v)
    } else if !valid {
        Status::Invalid
    } else if yanked.unwrap_or(false) {
        Status::Yanked
    } else {
        Status::Candidate
    };
    Ok(IndexLine { entry, status })
}

#[derive(Deserialize)]
struct IndexPackageVersion<'a> {
    /// Read only to leave out a line without a name, as [`parse_line`] does.
    #[serde(borrow, rename = "name")]
    _name: std::borrow::Cow<'a, str>,
    vers: Version,
}

/// The version of an index line, read without the rest of it: `Some` exactly when
/// [`parse_line`] reads the line, with the version it reads.
pub fn line_version(line: &[u8]) -> Option<Version> {
    serde_json::from_slice::<IndexPackageVersion<'_>>(line).ok().map(|it| it.vers)
}

/// An index file, each line's version read and the rest of it parsed only when a query
/// matches that version, as cargo parses its cached index (`MaybeIndexSummary::Unparsed`).
/// Lines are split at `\n` only, as cargo splits them; a line with no name and version is
/// left out, as [`parse_line`] leaves it out.
pub struct Lines {
    raw: Vec<u8>,
    versions: Vec<Version>,
    spans: Vec<Range<usize>>,
    parsed: Vec<OnceCell<Option<IndexLine>>>,
}

impl Lines {
    pub fn new(raw: Vec<u8>) -> Lines {
        let count = raw.iter().filter(|&&it| it == b'\n').count() + 1;
        let mut versions = Vec::with_capacity(count);
        let mut spans = Vec::with_capacity(count);
        let mut start = 0;
        for line in raw.split(|&it| it == b'\n') {
            let span = start..start + line.len();
            start = span.end + 1;
            if line.is_empty() {
                continue;
            }
            if let Some(version) = line_version(line) {
                versions.push(version);
                spans.push(span);
            }
        }
        let parsed = (0..versions.len()).map(|_| OnceCell::new()).collect();
        Lines { raw, versions, spans, parsed }
    }

    pub fn empty() -> Lines {
        Lines { raw: Vec::new(), versions: Vec::new(), spans: Vec::new(), parsed: Vec::new() }
    }

    #[cfg(test)]
    pub(crate) fn parsed_count(&self) -> usize {
        self.parsed.iter().filter(|it| it.get().is_some()).count()
    }

    /// The lines whose version `matches` accepts, in the file's order, each parsed once.
    pub fn matching<'s>(
        &'s self,
        mut matches: impl FnMut(&Version) -> bool + 's,
    ) -> impl Iterator<Item = &'s IndexLine> + 's {
        self.versions.iter().enumerate().filter(move |(_, version)| matches(version)).filter_map(
            move |(at, _)| {
                self.parsed[at].get_or_init(|| parse_line(&self.raw[self.spans[at].clone()]).ok()).as_ref()
            },
        )
    }
}

fn entry(package: IndexPackage) -> anyhow::Result<IndexEntry> {
    let deps = package.deps.into_iter().map(dependency).collect::<anyhow::Result<Vec<_>>>()?;
    let mut features = package.features;
    for (name, values) in package.features2.into_iter().flatten() {
        features.entry(name).or_default().extend(values);
    }
    Ok(IndexEntry {
        name: package.name,
        version: package.vers,
        deps,
        features,
        checksum: package.cksum,
        links: package.links,
        rust_version: package.rust_version,
        pubtime: package.pubtime,
    })
}

/// As cargo's `registry_dependency_into_dep`, to the errors it can return.
fn dependency(dep: RegistryDependency) -> anyhow::Result<IndexDependency> {
    let registry = match dep.registry.as_deref() {
        Some(url) => Some(SourceId::for_registry(
            &url::Url::parse(url).with_context(|| format!("`{url}` is not a URL"))?,
        )?),
        None => None,
    };
    let package = dep.package.unwrap_or_else(|| dep.name.clone());
    let req = VersionReq::parse(&dep.req).with_context(|| {
        format!("failed to parse the version requirement `{}` for dependency `{package}`", dep.req)
    })?;
    let kind = match dep.kind.as_deref().unwrap_or("") {
        "dev" => DepKind::Development,
        "build" => DepKind::Build,
        _ => DepKind::Normal,
    };
    let platform = dep.target.as_deref().map(Platform::from_str).transpose()?;
    let mut features = dep.features;
    features.retain(|it| !it.is_empty());
    let artifact = dep
        .artifact
        .map(|kinds| Artifact::parse(&kinds, dep.lib, dep.bindep_target.as_deref()))
        .transpose()?;
    Ok(IndexDependency {
        name_in_toml: dep.name,
        package,
        req,
        req_text: dep.req,
        features,
        optional: dep.optional,
        default_features: dep.default_features,
        platform,
        kind,
        registry: registry.filter(|it| !it.is_crates_io()),
        public: dep.public.unwrap_or(false),
        artifact,
    })
}

/// The path of a package's file in an index, from its name as given: `1/a`, `2/ab`, `3/a/abc`,
/// `ab/cd/abcd` (cargo-util's `make_dep_path`). With `prefix_only`, the directory alone, which a
/// `dl` template's `{prefix}` is.
pub fn dep_path(name: &str, prefix_only: bool) -> String {
    let (slash, rest) = if prefix_only { ("", "") } else { ("/", name) };
    match name.len() {
        1 => format!("1{slash}{rest}"),
        2 => format!("2{slash}{rest}"),
        3 => format!("3/{}{slash}{rest}", &name[..1]),
        _ => format!("{}/{}{slash}{rest}", &name[0..2], &name[2..4]),
    }
}

/// The path of a package's index file: [`dep_path`] of its name in lower case, as cargo's
/// `RegistryIndex::load_summaries` names it.
pub fn index_path(name: &str) -> String {
    dep_path(&name.to_lowercase(), false)
}

/// The URL of a `.crate` file from a registry's `dl` template (cargo-util's `crate_url`).
pub fn crate_url(template: &str, name: &str, version: &str, checksum: &str) -> String {
    const MARKERS: [&str; 5] =
        ["{crate}", "{version}", "{prefix}", "{lowerprefix}", "{sha256-checksum}"];
    if !MARKERS.iter().any(|it| template.contains(it)) {
        return format!("{template}/{name}/{version}/download");
    }
    let prefix = dep_path(name, true);
    template
        .replace("{crate}", name)
        .replace("{version}", version)
        .replace("{prefix}", &prefix)
        .replace("{lowerprefix}", &prefix.to_lowercase())
        .replace("{sha256-checksum}", checksum)
}

/// A line of an index of `source` as the resolver takes it, as cargo's `IndexSummary::parse`:
/// a line whose summary cannot be made is [`IndexSummary::Invalid`], with its name, version and
/// `rust-version` alone. None for a name or version no summary can be made of.
pub fn index_summary(line: &IndexLine, source: SourceId) -> Option<IndexSummary> {
    let made = match line.status {
        Status::Invalid => None,
        _ => summary(&line.entry, source).ok(),
    };
    let Some(summary) = made else {
        let IndexEntry { name, version, rust_version, .. } = &line.entry;
        let id = PackageId::new(InternedString::new(name), version.clone(), source);
        let none = BTreeMap::new();
        return Summary::new(id, Vec::new(), &none, None::<&str>, rust_version.clone())
            .ok()
            .map(IndexSummary::Invalid);
    };
    Some(match line.status {
        Status::Candidate => IndexSummary::Candidate(summary),
        Status::Yanked => IndexSummary::Yanked(summary),
        Status::Unsupported(v) => IndexSummary::Unsupported(summary, v),
        Status::Invalid => IndexSummary::Invalid(summary),
    })
}

/// cargo's `index_package_to_summary`.
fn summary(entry: &IndexEntry, source: SourceId) -> anyhow::Result<Summary> {
    let id = PackageId::new(InternedString::new(&entry.name), entry.version.clone(), source);
    let deps = entry
        .deps
        .iter()
        .map(|it| summary_dependency(it, source))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let features: BTreeMap<InternedString, Vec<InternedString>> = entry
        .features
        .iter()
        .map(|(name, values)| {
            (InternedString::new(name), values.iter().map(|it| InternedString::new(it)).collect())
        })
        .collect();
    let mut summary =
        Summary::new(id, deps, &features, entry.links.as_deref(), entry.rust_version.clone())?;
    summary.set_checksum(entry.checksum.clone());
    if let Some(pubtime) = &entry.pubtime {
        summary.set_pubtime(pubtime.clone());
    }
    Ok(summary)
}

/// A dependency of a package of the registry `default`, as the resolver takes it.
fn summary_dependency(dep: &IndexDependency, default: SourceId) -> anyhow::Result<Dependency> {
    if dep.artifact.is_some() {
        bail!(
            "`{}` is an artifact dependency (`-Zbindeps`), which blaze does not read",
            dep.package
        );
    }
    if dep.public && dep.kind != DepKind::Normal {
        bail!("`{}` is public but not a normal dependency", dep.package);
    }
    let id = dep.registry.unwrap_or(default);
    let mut it = Dependency::parse(dep.package.as_str(), Some(dep.req_text.as_str()), id)?;
    if dep.name_in_toml != dep.package {
        it.set_explicit_name_in_toml(dep.name_in_toml.as_str());
    }
    if !id.is_crates_io() {
        it.set_registry_id(id);
    }
    it.set_optional(dep.optional)
        .set_default_features(dep.default_features)
        .set_features(dep.features.iter().map(String::as_str))
        .set_platform(dep.platform.clone())
        .set_kind(dep.kind)
        .set_public(dep.public);
    Ok(it)
}
