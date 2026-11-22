//! The one error of reading manifests: [`ManifestError`]. Each area's refusals are a typed
//! enum in its own module (a target's in `targets`, a profile's in `profiles`, ..); a manifest
//! that does not parse or does not fit the schema is a [`ManifestDiagnostic`] with its span;
//! what an outer step was doing is a [`Context`] around its cause, printed as cargo prints a
//! chain (`{:#}` joins it with `: `, `diagnostic::rendered` under `Caused by:`).

use std::fmt;
use std::path::PathBuf;

use crate::config::error::ConfigError;
use crate::dependency::DependencyError;
use crate::diagnostic::ManifestDiagnostic;
use crate::features::FeatureError;
use crate::manifest::InvalidManifest;
use crate::metadata::MetadataError;
use crate::profiles::ProfileError;
use crate::schema::UnresolvedError;
use crate::source::SourceRefusal;
use crate::targets::TargetError;
use crate::workspace::WorkspaceError;

/// What reading a manifest, a workspace or a profile refused, and where.
#[derive(Debug)]
pub enum ManifestError {
    /// A file that could not be read.
    Read { path: PathBuf, error: std::io::Error },
    /// A manifest that is not UTF-8.
    NotUtf8 { path: PathBuf },
    /// A manifest that is no TOML or does not fit the schema, with the span cargo marks.
    Diagnostic(Box<ManifestDiagnostic>),
    /// What was being done when `source` happened.
    Context { context: Context, source: Box<ManifestError> },
    Manifest(InvalidManifest),
    Target(TargetError),
    Workspace(WorkspaceError),
    Profile(ProfileError),
    Dependency(DependencyError),
    Feature(FeatureError),
    Metadata(MetadataError),
    /// Where a package comes from, refused here.
    Location(SourceRefusal),
    /// A field still `workspace = true` where a normalized one was wanted.
    Unresolved(UnresolvedError),
    Source(blaze_source::SourceError),
    Resolve(blaze_resolve::ResolveError),
    Config(ConfigError),
    Version(blaze_semver::Error),
    Cfg(blaze_resolve::CfgError),
    Pattern(crate::files::PatternError),
    Glob(crate::files::GlobError),
    Url(url::ParseError),
}

/// What an outer step was doing, in cargo's words.
#[derive(Debug)]
pub enum Context {
    /// `failed to parse manifest at `{path}``.
    ParseManifest { path: PathBuf },
    /// `could not parse pattern `{pattern}``.
    Pattern { pattern: String },
    /// `unable to match path to pattern `{pattern}``.
    PatternMatch { pattern: String },
    /// `resolving path for patch of ({package}) for source ({source})`.
    PatchPath { package: String, source: String },
    /// `error inheriting `{label}` from workspace root manifest's `workspace.package.{label}``.
    InheritField { label: String },
    /// `error inheriting `{name}` from workspace root manifest's `workspace.dependencies.{name}``.
    InheritDependency { name: String },
    /// `resolving path dependency {name}`.
    PathDependency { name: String },
    /// `error inheriting `lints` from workspace root manifest's `workspace.lints``.
    InheritLints,
    /// `failed to parse the `edition` key`.
    EditionKey,
    /// `failed to parse the version requirement `{version}` for dependency `{name}``.
    VersionRequirement { version: String, name: String },
    /// cargo's `failed searching for potential workspace` with its help.
    WorkspaceSearch { manifest: PathBuf, potential: PathBuf },
    /// `failed to load manifest for workspace member `{path}`` and who referenced it.
    WorkspaceMember { path: PathBuf, glob: Option<String>, root: PathBuf },
    /// `failed to load manifest for dependency `{name}``.
    DependencyManifest { name: String },
    /// `config profile `{name}` is not valid (defined in `{definition}`)`.
    ConfigProfile { name: String, definition: String },
    /// The unstable `im-a-teapot` key was used.
    ImATeapot,
    /// The unstable `package.default-target` or `package.forced-target` key was used.
    PerPackageTarget,
    /// A `[replace]` key that is no package id spec.
    ReplaceSpec { spec: String },
    /// A `[patch]` key that is neither a URL nor a registry name.
    PatchEntry { url: String },
}

impl fmt::Display for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Context::ParseManifest { path } => {
                write!(f, "failed to parse manifest at `{}`", path.display())
            }
            Context::Pattern { pattern } => write!(f, "could not parse pattern `{pattern}`"),
            Context::PatternMatch { pattern } => {
                write!(f, "unable to match path to pattern `{pattern}`")
            }
            Context::PatchPath { package, source } => {
                write!(f, "resolving path for patch of ({package}) for source ({source})")
            }
            Context::InheritField { label } => write!(
                f,
                "error inheriting `{label}` from workspace root manifest's \
                 `workspace.package.{label}`"
            ),
            Context::InheritDependency { name } => write!(
                f,
                "error inheriting `{name}` from workspace root manifest's \
                 `workspace.dependencies.{name}`"
            ),
            Context::PathDependency { name } => write!(f, "resolving path dependency {name}"),
            Context::InheritLints => f.write_str(
                "error inheriting `lints` from workspace root manifest's `workspace.lints`",
            ),
            Context::EditionKey => f.write_str("failed to parse the `edition` key"),
            Context::VersionRequirement { version, name } => write!(
                f,
                "failed to parse the version requirement `{version}` for dependency `{name}`"
            ),
            Context::WorkspaceSearch { manifest, potential } => write!(
                f,
                "failed searching for potential workspace\n\
                 package manifest: `{}`\n\
                 invalid potential workspace manifest: `{}`\n\
                 \n\
                 help: to avoid searching for a non-existent workspace, add `[workspace]` to \
                 the package manifest",
                manifest.display(),
                potential.display(),
            ),
            Context::WorkspaceMember { path, glob, root } => {
                write!(
                    f,
                    "failed to load manifest for workspace member `{}`\nreferenced",
                    path.display()
                )?;
                if let Some(glob) = glob {
                    write!(f, " via `{glob}`")?;
                }
                write!(f, " by workspace at `{}`", root.display())
            }
            Context::DependencyManifest { name } => {
                write!(f, "failed to load manifest for dependency `{name}`")
            }
            Context::ConfigProfile { name, definition } => {
                write!(f, "config profile `{name}` is not valid (defined in `{definition}`)")
            }
            Context::ImATeapot => f.write_str(
                "the `im-a-teapot` manifest key is unstable and may not work properly in England",
            ),
            Context::PerPackageTarget => f.write_str(
                "the `package.default-target` and `package.forced-target` manifest keys are \
                 unstable and may not work properly",
            ),
            Context::ReplaceSpec { spec } => write!(
                f,
                "replacements must specify a valid semver version to replace, but `{spec}` does not"
            ),
            Context::PatchEntry { url } => {
                write!(f, "[patch] entry `{url}` should be a URL or registry name")?;
                if url == "crates" {
                    f.write_str("\nFor crates.io, use [patch.crates-io] (with a dash)")?;
                }
                Ok(())
            }
        }
    }
}

impl ManifestError {
    /// `self` as the cause of what `context` names.
    pub fn context(self, context: Context) -> ManifestError {
        ManifestError::Context { context, source: Box::new(self) }
    }

    /// The diagnostic with a span this error is, or is caused by through contexts: what
    /// cargo prints alone, whatever wraps it.
    pub fn diagnostic(&self) -> Option<&ManifestDiagnostic> {
        match self {
            ManifestError::Diagnostic(d) => Some(d),
            ManifestError::Context { source, .. } => source.diagnostic(),
            _ => None,
        }
    }

    /// This error and each cause under it, outermost first.
    pub fn chain(&self) -> impl Iterator<Item = &(dyn core::error::Error + 'static)> {
        // dyn: std's error chain, walked only on an error path.
        let mut next: Option<&(dyn core::error::Error + 'static)> = Some(self);
        std::iter::from_fn(move || {
            let at = next?;
            next = at.source();
            Some(at)
        })
    }
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if f.alternate() {
            for (i, cause) in self.chain().enumerate() {
                if i > 0 {
                    f.write_str(": ")?;
                }
                // Each cause by its own words, not its chain again.
                write!(f, "{}", Head(cause))?;
            }
            return Ok(());
        }
        match self {
            ManifestError::Read { path, .. } => write!(f, "failed to read `{}`", path.display()),
            ManifestError::NotUtf8 { path } => {
                write!(f, "path at `{}` was not valid utf-8", path.display())
            }
            ManifestError::Diagnostic(d) => write!(f, "{d}"),
            ManifestError::Context { context, .. } => write!(f, "{context}"),
            ManifestError::Manifest(e) => write!(f, "{e}"),
            ManifestError::Target(e) => write!(f, "{e}"),
            ManifestError::Workspace(e) => write!(f, "{e}"),
            ManifestError::Profile(e) => write!(f, "{e}"),
            ManifestError::Dependency(e) => write!(f, "{e}"),
            ManifestError::Feature(e) => write!(f, "{e}"),
            ManifestError::Metadata(e) => write!(f, "{e}"),
            ManifestError::Location(e) => write!(f, "{e}"),
            ManifestError::Unresolved(e) => write!(f, "{e}"),
            ManifestError::Source(e) => write!(f, "{e}"),
            ManifestError::Resolve(e) => write!(f, "{e}"),
            ManifestError::Config(e) => write!(f, "{e}"),
            ManifestError::Version(e) => write!(f, "{e}"),
            ManifestError::Cfg(e) => write!(f, "{e}"),
            ManifestError::Pattern(e) => write!(f, "{e}"),
            ManifestError::Glob(e) => write!(f, "{e}"),
            ManifestError::Url(e) => write!(f, "{e}"),
        }
    }
}

/// An error's own message: for a [`ManifestError`] its plain Display, never the alternate
/// chain (which would repeat the causes).
struct Head<'e>(&'e (dyn core::error::Error + 'static));

impl fmt::Display for Head<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl core::error::Error for ManifestError {
    // dyn: std's error chain, read only on an error path.
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            ManifestError::Read { error, .. } => Some(error),
            ManifestError::Context { source, .. } => Some(&**source),
            ManifestError::NotUtf8 { .. } | ManifestError::Diagnostic(_) => None,
            // The rest print as the error they wrap, so its causes are theirs.
            ManifestError::Manifest(e) => e.source(),
            ManifestError::Target(e) => e.source(),
            ManifestError::Workspace(e) => e.source(),
            ManifestError::Profile(e) => e.source(),
            ManifestError::Dependency(e) => e.source(),
            ManifestError::Feature(e) => e.source(),
            ManifestError::Metadata(e) => e.source(),
            ManifestError::Location(e) => e.source(),
            ManifestError::Unresolved(e) => e.source(),
            ManifestError::Source(e) => e.source(),
            ManifestError::Resolve(e) => e.source(),
            ManifestError::Config(e) => e.source(),
            ManifestError::Version(e) => e.source(),
            ManifestError::Cfg(e) => e.source(),
            ManifestError::Pattern(e) => e.source(),
            ManifestError::Glob(e) => e.source(),
            ManifestError::Url(e) => e.source(),
        }
    }
}

macro_rules! from {
    ($($variant:ident($ty:ty)),* $(,)?) => {
        $(impl From<$ty> for ManifestError {
            fn from(error: $ty) -> ManifestError {
                ManifestError::$variant(error)
            }
        })*
    };
}

from!(
    Manifest(InvalidManifest),
    Target(TargetError),
    Workspace(WorkspaceError),
    Profile(ProfileError),
    Dependency(DependencyError),
    Feature(FeatureError),
    Metadata(MetadataError),
    Location(SourceRefusal),
    Unresolved(UnresolvedError),
    Source(blaze_source::SourceError),
    Resolve(blaze_resolve::ResolveError),
    Config(ConfigError),
    Version(blaze_semver::Error),
    Cfg(blaze_resolve::CfgError),
    Pattern(crate::files::PatternError),
    Glob(crate::files::GlobError),
    Url(url::ParseError),
);

impl From<ManifestDiagnostic> for ManifestError {
    fn from(d: ManifestDiagnostic) -> ManifestError {
        ManifestError::Diagnostic(Box::new(d))
    }
}

/// Adds a [`Context`] to the error of a `Result`, as anyhow's `with_context` did.
pub trait ResultExt<T> {
    fn context(self, context: impl FnOnce() -> Context) -> Result<T, ManifestError>;
}

impl<T, E: Into<ManifestError>> ResultExt<T> for Result<T, E> {
    fn context(self, context: impl FnOnce() -> Context) -> Result<T, ManifestError> {
        self.map_err(|e| e.into().context(context()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alternate_display_joins_the_chain_as_anyhow_did() {
        let inner = ManifestError::NotUtf8 { path: PathBuf::from("/w/Cargo.toml") };
        let outer = inner.context(Context::ParseManifest { path: PathBuf::from("/w/Cargo.toml") });
        assert_eq!(outer.to_string(), "failed to parse manifest at `/w/Cargo.toml`");
        assert_eq!(
            format!("{outer:#}"),
            "failed to parse manifest at `/w/Cargo.toml`: path at `/w/Cargo.toml` was not \
             valid utf-8"
        );
    }

    #[test]
    fn errors_cross_threads() {
        fn is_send_sync<T: Send + Sync + 'static>() {}
        is_send_sync::<ManifestError>();
    }
}
