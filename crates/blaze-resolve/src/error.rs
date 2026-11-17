//! [`ResolveError`]: why a name, version, lock file, feature map or resolve was refused, each
//! naming the package, feature, file or key it was about, in the words cargo 1.99.0
//! (5f94df4789) uses for the same refusal.

use std::fmt;

use blaze_core::wait::Unfinished;
use url::Url;

use crate::interning::InternedString;
use crate::lockfile::LOCKFILE_NAME;
use crate::package_id::PackageId;
use crate::package_id_spec::PackageIdSpec;
use crate::source_id::SourceId;
use crate::summary::{FeatureValue, MissingDependencyError};
use crate::version::{OptVersionReq, Version};
use blaze_source::SourceError;

/// What a partial version (`rust-version`, the version of a package id spec) had that it may
/// not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartialWhy {
    Prerelease,
    Build,
    /// Not a version at all.
    NotAVersion,
    /// A requirement (`^1.2`, `>=1.2`, `1.2, 1.3`) where a version was wanted.
    Requirement,
}

/// Why blaze-resolve refused something.
#[derive(Debug)]
pub enum ResolveError {
    /// A source that could not be named.
    Source(SourceError),
    /// A registry's own failure to answer a query ([`Registry`](crate::Registry) implementors
    /// outside this crate box theirs here).
    // dyn: an implementor's own error type, on the error path.
    Registry(Box<dyn core::error::Error + Send + Sync>),
    /// A driver's wait for a registry's answer did not end (an answer of
    /// [`Event::Answered`](crate::resolving::Event::Answered) may be this).
    Wait(Unfinished),

    // Names.
    /// An empty package, registry or feature name; `what` says which.
    EmptyName { what: &'static str },
    /// A character a name of `what` may not have there.
    NameChar { ch: char, what: &'static str, name: String, reason: &'static str },
    /// A feature named `dep:...`.
    FeatureNameDep { name: String },

    // Versions.
    /// A version that does not parse; `what` names whose it is.
    Version { what: String, text: String, error: blaze_semver::Error },
    /// A dependency's version requirement that does not parse.
    DependencyReq { name: InternedString, req: String, error: blaze_semver::Error },
    /// A partial version (`rust-version = "1.70"`, `foo@1.2`) that is not one.
    PartialVersion { text: String, why: PartialWhy },
    /// `name version (source)` that is not.
    SerializedPackageId { text: String },

    // Package id specs.
    SpecLooksLikePath { spec: String, url: Option<Url> },
    SpecQueryString { url: Url },
    SpecPathScheme { scheme: String },
    SpecProtocol { kind: String },
    SpecNoPath { url: Url },
    SpecEmptyFragment,
    /// A spec that does not parse, with what it might have meant.
    InvalidSpec { spec: String, suggestion: String, cause: Box<ResolveError> },
    SpecNoMatch { spec: PackageIdSpec, suggestion: String },
    /// A spec more than one package matches; `options` lists them, a line each.
    SpecAmbiguous { spec: PackageIdSpec, options: String },

    // Lock files.
    /// `Cargo.lock` is not TOML.
    LockfileToml { error: blaze_toml::Error, line: usize, column: usize },
    /// A key of `Cargo.lock` that holds the wrong kind of value.
    LockfileType { key: String, line: usize, expected: &'static str, found: &'static str },
    /// A key every entry of its table must have.
    LockfileMissing { key: String, line: usize },
    /// A `source` (or an edge's) that names no source.
    LockfileSource { key: String, line: usize, error: SourceError },
    /// An edge that is not `name`, `name version` or `name version (source)`.
    LockfilePackageId { key: String, line: usize, text: String },
    /// A `version = N` newer than this resolver reads.
    LockfileFuture { version: i64 },
    LockfileTwice { name: String },
    /// A `[metadata]` checksum key that is no package id.
    LockfileChecksumKey { key: String, cause: Box<ResolveError> },
    /// A lock file that lists two different packages the same way.
    LockfileCollision { first: PackageId, second: PackageId },
    ChecksumNotPreviouslyCalculated { package: PackageId },
    ChecksumUncomputable { package: PackageId },
    ChecksumChanged { package: PackageId },

    // Summaries and their features.
    OptionalDevDependency { dep: InternedString },
    FeatureUnknown { feature: InternedString, value: FeatureValue, closest: String },
    /// `feature = ["dep"]` where `dep` is optional and only enabled with `dep:dep`.
    FeatureNoImplicit { feature: InternedString, value: FeatureValue, dep: InternedString },
    /// A feature (`dep` or `dep:dep`) that names a dependency that is not optional.
    FeatureNotOptional { feature: InternedString, value: FeatureValue, dep: InternedString },
    FeatureDepNotListed { feature: InternedString, value: FeatureValue, dep: InternedString },
    FeatureSlashes { feature: InternedString, value: FeatureValue },
    /// `dep:a/b`; `help` holds `a` and `b` where the help applies.
    FeatureDepAndSlash {
        feature: InternedString,
        value: FeatureValue,
        help: Option<(InternedString, InternedString)>,
    },
    MissingDependency(MissingDependencyError),
    FeatureWeakNotOptional { feature: InternedString, value: FeatureValue, dep: InternedString },
    OptionalUnused { dep: InternedString },
    /// `--features dep:x`.
    CliDepSyntax { feature: FeatureValue },
    CliSlashes { feature: FeatureValue },
    /// Features asked of packages that do not have them; `report` says which and what is near.
    UnknownFeatures { report: String },
    /// `--features` with `-p` of a package outside the workspace.
    FeaturesOutsideWorkspace { hint: String },
    ResolverSetting { value: String },

    // Resolving.
    /// Why the resolve failed (`report`), and the packages from the one whose requirements
    /// could not be met up to the root.
    Unresolvable { report: String, package_path: Vec<PackageId> },
    /// A failure to query a dependency of the package `path` describes.
    DependencyQuery { dep: InternedString, path: String, cause: Box<ResolveError> },
    /// A failure to load the source of a dependency.
    LoadSource { dep: InternedString, source: SourceId, cause: Box<ResolveError> },
    NoFeature { package: PackageId, feature: InternedString, closest: String },
    /// A feature of the name of an optional dependency only `dep:` enables; `suggestion` lists
    /// the features that enable it.
    NonImplicitFeature { package: PackageId, feature: InternedString, suggestion: String },
    RequiredDependencyAsFeature { package: PackageId, feature: InternedString },
    NoDependencyNamed { package: PackageId, dep: InternedString },
    FeatureCycle { feature: InternedString },
    PackageCycle { package: PackageId, path: String },
    LinksTwice { links: InternedString },
    CrateNamedTwice { from: PackageId, to: PackageId },
    NotPinned { dep: InternedString, source: String },
    /// The resolve machine ran its most rounds and still met queries no answer held.
    Unsettled { rounds: u32, asks: usize },

    // `[replace]`.
    OverrideNotFound { spec: PackageIdSpec, source: SourceId, req: OptVersionReq },
    OverrideAmbiguous { spec: PackageIdSpec, first: PackageId, rest: Vec<PackageId> },
    OverrideVersion { spec: PackageIdSpec, version: Version, with: Version },
    OverrideOverlap { first: PackageIdSpec, second: PackageIdSpec, package: PackageId },

    // `[patch]`.
    PatchSameSource { dep: InternedString, url: Url, loc: String },
    PatchSameVersion { name: InternedString, version: Version, url: Url, locations: Vec<String> },
    PatchManyCandidates {
        dep: InternedString,
        source: SourceId,
        versions: Vec<String>,
        url: Url,
        loc: String,
    },
    PatchNoMatch { dep: InternedString, source: SourceId, url: Url, loc: String },
    /// `found` is `version `x`` or `versions `x, y``.
    PatchVersionMismatch {
        dep: InternedString,
        found: String,
        req: OptVersionReq,
        source: SourceId,
        url: Url,
        loc: String,
    },
}

impl From<SourceError> for ResolveError {
    fn from(error: SourceError) -> ResolveError {
        ResolveError::Source(error)
    }
}

impl From<MissingDependencyError> for ResolveError {
    fn from(error: MissingDependencyError) -> ResolveError {
        ResolveError::MissingDependency(error)
    }
}

impl From<Unfinished> for ResolveError {
    fn from(error: Unfinished) -> ResolveError {
        ResolveError::Wait(error)
    }
}

// Callers box it into errors that cross threads.
const _: () = {
    const fn send_sync<T: Send + Sync + 'static>() {}
    send_sync::<ResolveError>();
};

impl ResolveError {
    /// The packages from the one whose requirements could not be met up to the root, for an
    /// unresolvable graph; empty otherwise.
    pub fn package_path(&self) -> &[PackageId] {
        match self {
            ResolveError::Unresolvable { package_path, .. } => package_path,
            _ => &[],
        }
    }
}

const NON_OPTIONAL_HELP: &str = "A non-optional dependency of the same name is defined; \
                                 consider adding `optional = true` to its definition.";

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ResolveError as E;
        match self {
            E::Source(error) => error.fmt(f),
            E::Registry(error) => error.fmt(f),
            E::Wait(error) => error.fmt(f),

            E::EmptyName { what } => write!(f, "{what} cannot be empty"),
            E::NameChar { ch, what, name, reason } => {
                write!(f, "invalid character `{ch}` in {what}: `{name}`, {reason}")
            }
            E::FeatureNameDep { name } => {
                write!(f, "feature named `{name}` is not allowed to start with `dep:`")
            }

            E::Version { what, text, error } => {
                write!(f, "invalid version `{text}` of {what}: {error}")
            }
            E::DependencyReq { name, req, error: _ } => write!(
                f,
                "failed to parse the version requirement `{req}` for dependency `{name}`"
            ),
            E::PartialVersion { text: _, why } => f.write_str(match why {
                PartialWhy::Prerelease => {
                    "unexpected prerelease field, expected a version like \"1.32\""
                }
                PartialWhy::Build => "unexpected build field, expected a version like \"1.32\"",
                PartialWhy::NotAVersion => "expected a version like \"1.32\"",
                PartialWhy::Requirement => {
                    "unexpected version requirement, expected a version like \"1.32\""
                }
            }),
            E::SerializedPackageId { text } => {
                write!(f, "invalid serialized PackageId `{text}`")
            }

            E::SpecLooksLikePath { spec, url } => {
                write!(f, "package ID specification `{spec}` looks like a file path, maybe try ")?;
                match url {
                    Some(url) => write!(f, "{url}"),
                    None => f.write_str("a file:// URL"),
                }
            }
            E::SpecQueryString { url } => write!(f, "cannot have a query string in a pkgid: {url}"),
            E::SpecPathScheme { scheme } => write!(
                f,
                "`path+{scheme}` is unsupported; `path+file` and `file` schemes are supported"
            ),
            E::SpecProtocol { kind } => write!(f, "unsupported source protocol: {kind}"),
            E::SpecNoPath { url } => {
                write!(f, "pkgid urls must have at least one path component: {url}")
            }
            E::SpecEmptyFragment => f.write_str("pkgid url cannot have an empty fragment"),
            E::InvalidSpec { spec, suggestion, cause: _ } => {
                write!(f, "invalid package ID specification: `{spec}`{suggestion}")
            }
            E::SpecNoMatch { spec, suggestion } => write!(
                f,
                "package ID specification `{spec}` did not match any packages{suggestion}"
            ),
            E::SpecAmbiguous { spec, options } => write!(
                f,
                "specification `{spec}` is ambiguous\n\
                 help: re-run this command with one of the following specifications{options}"
            ),

            E::LockfileToml { error, line, column } => {
                write!(f, "{LOCKFILE_NAME}, line {line}, column {column}: {}", error.what)
            }
            E::LockfileType { key, line, expected, found } => write!(
                f,
                "{LOCKFILE_NAME}, line {line}: `{key}` is {found}, where {expected} was wanted"
            ),
            E::LockfileMissing { key, line } => {
                write!(f, "{LOCKFILE_NAME}, line {line}: missing field `{key}`")
            }
            E::LockfileSource { key, line, error } => {
                write!(f, "{LOCKFILE_NAME}, line {line}: `{key}`: {error}")
            }
            E::LockfilePackageId { key, line, text } => write!(
                f,
                "{LOCKFILE_NAME}, line {line}: `{key}`: invalid serialized PackageId `{text}`"
            ),
            E::LockfileFuture { version } => write!(
                f,
                "lock file version `{version}` was found, but this version of Cargo does not \
                 understand this lock file, perhaps Cargo needs to be updated?"
            ),
            E::LockfileTwice { name } => {
                write!(f, "package `{name}` is specified twice in the lockfile")
            }
            E::LockfileChecksumKey { key, cause: _ } => write!(
                f,
                "internal error: invalid encoding of checksum in lockfile: `{key}`"
            ),
            E::LockfileCollision { first, second } => write!(
                f,
                "package collision in the lockfile: packages {first} and {second} are different, \
                 but only one can be written to lockfile unambiguously"
            ),
            E::ChecksumNotPreviouslyCalculated { package } => write!(
                f,
                "\
checksum for `{package}` was not previously calculated, but a checksum could now \
be calculated

this could be indicative of a few possible situations:

    * the source `{}` did not previously support checksums,
      but was replaced with one that does
    * newer Cargo implementations know how to checksum this source, but this
      older implementation does not
    * the lock file is corrupt
",
                package.source_id()
            ),
            E::ChecksumUncomputable { package } => write!(
                f,
                "\
checksum for `{package}` could not be calculated, but a checksum is listed in \
the existing lock file

this could be indicative of a few possible situations:

    * the source `{}` supports checksums,
      but was replaced with one that doesn't
    * the lock file is corrupt

unable to verify that `{package}` is the same as when the lockfile was generated
",
                package.source_id()
            ),
            E::ChecksumChanged { package } => write!(
                f,
                "\
checksum for `{package}` changed between lock files

this could be indicative of a few possible errors:

    * the lock file is corrupt
    * a replacement source in use (e.g., a mirror) returned a different checksum
    * the source itself may be corrupt in one way or another

unable to verify that `{package}` is the same as when the lockfile was generated
"
            ),

            E::OptionalDevDependency { dep } => {
                write!(f, "dev-dependencies are not allowed to be optional: `{dep}`")
            }
            E::FeatureUnknown { feature, value, closest } => write!(
                f,
                "feature `{feature}` includes `{value}` which is neither a dependency nor \
                 another feature{closest}"
            ),
            E::FeatureNoImplicit { feature, value, dep } => write!(
                f,
                "feature `{feature}` includes `{value}`, but `{dep}` is an optional dependency \
                 without an implicit feature\nUse `dep:{dep}` to enable the dependency."
            ),
            E::FeatureNotOptional { feature, value, dep } => write!(
                f,
                "feature `{feature}` includes `{value}`, but `{dep}` is not an optional \
                 dependency\n{NON_OPTIONAL_HELP}"
            ),
            E::FeatureDepNotListed { feature, value, dep } => write!(
                f,
                "feature `{feature}` includes `{value}`, but `{dep}` is not listed as a dependency"
            ),
            E::FeatureSlashes { feature, value } => write!(
                f,
                "multiple slashes in feature `{value}` (included by feature `{feature}`) are not \
                 allowed"
            ),
            E::FeatureDepAndSlash { feature, value, help } => {
                write!(
                    f,
                    "feature `{feature}` includes `{value}` with both `dep:` and `/`\n\
                     To fix this, remove the `dep:` prefix."
                )?;
                match help {
                    Some((dep, dep_feature)) => write!(
                        f,
                        "\nIf the intent is to avoid creating an implicit feature `{dep}` for an \
                         optional dependency, then consider replacing this with two values:\n    \
                         \"dep:{dep}\", \"{dep}/{dep_feature}\""
                    ),
                    None => Ok(()),
                }
            }
            E::MissingDependency(error) => error.fmt(f),
            E::FeatureWeakNotOptional { feature, value, dep } => write!(
                f,
                "feature `{feature}` includes `{value}` with a `?`, but `{dep}` is not an \
                 optional dependency\nA non-optional dependency of the same name is defined; \
                 consider removing the `?` or changing the dependency to be optional"
            ),
            E::OptionalUnused { dep } => write!(
                f,
                "optional dependency `{dep}` is not included in any feature\n\
                 Make sure that `dep:{dep}` is included in one of features in the [features] \
                 table."
            ),
            E::CliDepSyntax { feature } => {
                write!(f, "feature `{feature}` is not allowed to use explicit `dep:` syntax")
            }
            E::CliSlashes { feature } => {
                write!(f, "multiple slashes in feature `{feature}` is not allowed")
            }
            E::UnknownFeatures { report } => f.write_str(report),
            E::FeaturesOutsideWorkspace { hint } => {
                write!(f, "cannot specify features for packages outside of workspace{hint}")
            }
            E::ResolverSetting { value } => write!(
                f,
                "`resolver` setting `{value}` is not valid, valid options are \"1\", \"2\" or \"3\""
            ),

            E::Unresolvable { report, package_path: _ } => f.write_str(report),
            E::DependencyQuery { dep, path, cause: _ } => {
                write!(f, "failed to get `{dep}` as a dependency of {path}")
            }
            E::LoadSource { dep, source: _, cause: _ } => {
                write!(f, "failed to load source for dependency `{dep}`")
            }
            E::NoFeature { package, feature, closest } => {
                write!(f, "package `{package}` does not have the feature `{feature}`{closest}")
            }
            E::NonImplicitFeature { package, feature, suggestion } => write!(
                f,
                "package `{package}` does not have feature `{feature}`\n\n\
                 help: an optional dependency with that name exists, but the `features` table \
                 includes it with the \"dep:\" syntax so it does not have an implicit feature \
                 with that name{suggestion}"
            ),
            E::RequiredDependencyAsFeature { package, feature } => write!(
                f,
                "package `{package}` does not have feature `{feature}`\n\n\
                 help: a dependency with that name exists but it is required dependency and \
                 only optional dependencies can be used as features."
            ),
            E::NoDependencyNamed { package, dep } => {
                write!(f, "package `{package}` does not have a dependency named `{dep}`")
            }
            E::FeatureCycle { feature } => {
                write!(f, "cyclic feature dependency: feature `{feature}` depends on itself")
            }
            E::PackageCycle { package, path } => write!(
                f,
                "cyclic package dependency: package `{package}` depends on itself. Cycle:\n{path}"
            ),
            E::LinksTwice { links } => write!(
                f,
                "Attempting to resolve a dependency with more than one crate with links={links}.\n\
                 This will not build as is. Consider rebuilding the .lock file."
            ),
            E::CrateNamedTwice { from, to } => write!(
                f,
                "the crate `{from}` depends on crate `{to}` multiple times with different names"
            ),
            E::NotPinned { dep, source } => write!(
                f,
                "`{dep}` of {source} is not pinned by Cargo.lock, and blaze reads that source \
                 only at the versions Cargo.lock pins"
            ),
            E::Unsettled { rounds, asks } => write!(
                f,
                "the resolve still asked the registry for {asks} more answers after {rounds} \
                 rounds of them (blaze-resolve's `resolving::MOST_ROUNDS`, rule 18)"
            ),

            E::OverrideNotFound { spec, source, req } => write!(
                f,
                "no matching package for override `{spec}` found\n\
                 location searched: {source}\n\
                 version required: {req}"
            ),
            E::OverrideAmbiguous { spec, first, rest } => {
                write!(
                    f,
                    "the replacement specification `{spec}` matched multiple packages:\n  * {first}\n"
                )?;
                for (i, id) in rest.iter().enumerate() {
                    if i > 0 {
                        f.write_str("\n")?;
                    }
                    write!(f, "  * {id}")?;
                }
                Ok(())
            }
            E::OverrideVersion { spec, version, with } => write!(
                f,
                "replacement specification `{spec}` matched {version} and tried to override it \
                 with {with}\navoid matching unrelated packages by being more specific"
            ),
            E::OverrideOverlap { first, second, package } => write!(
                f,
                "overlapping replacement specifications found:\n\n  * {first}\n  * {second}\n\n\
                 both specifications match: {package}"
            ),

            E::PatchSameSource { dep, url, loc } => write!(
                f,
                "patch for `{dep}` points to the same source, but patches must point to \
                 different sources\nhelp: check `{dep}` patch definition for `{url}` in `{loc}`"
            ),
            E::PatchSameVersion { name, version, url, locations } => write!(
                f,
                "several `[patch]` entries resolving to same version `{name} v{version}`\n\
                 help: check `{name}` patch definitions for `{url}` in {}",
                locations.join(", ")
            ),
            E::PatchManyCandidates { dep, source, versions, url, loc } => write!(
                f,
                "patch for `{dep}` in `{source}` resolved to more than one candidate\n\
                 note: found versions: {}\n\
                 help: check `{dep}` patch definition for `{url}` in `{loc}`\n\
                 help: select only one package using `version = \"={}\"`",
                versions.join(", "),
                versions.last().map_or("", String::as_str)
            ),
            E::PatchNoMatch { dep, source, url, loc } => write!(
                f,
                "patch location `{source}` does not contain packages matching `{dep}`\n\
                 help: check `{dep}` patch definition for `{url}` in `{loc}`"
            ),
            E::PatchVersionMismatch { dep, found, req, source, url, loc } => write!(
                f,
                "patch `{dep}` version mismatch\n\
                 note: patch location contains {found}, but patch definition requires `{req}`\n\
                 help: check patch location `{source}`\n\
                 help: check `{dep}` patch definition for `{url}` in `{loc}`"
            ),
        }
    }
}

impl core::error::Error for ResolveError {
    // dyn: `core::error::Error::source`'s own signature, on the error path.
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        use ResolveError as E;
        match self {
            E::Source(error) => Some(error),
            E::Registry(error) => Some(&**error),
            E::Wait(error) => Some(error),
            E::Version { error, .. } | E::DependencyReq { error, .. } => Some(error),
            E::LockfileToml { error, .. } => Some(error),
            E::LockfileSource { error, .. } => Some(error),
            E::InvalidSpec { cause, .. }
            | E::LockfileChecksumKey { cause, .. }
            | E::DependencyQuery { cause, .. }
            | E::LoadSource { cause, .. } => Some(&**cause),
            _ => None,
        }
    }
}
