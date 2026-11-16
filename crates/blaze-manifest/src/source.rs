//! Where a package comes from: cargo's `SourceId`, which is blaze-source's, plus the two
//! constructors that read the configuration and the source a `PackageIdSpec`'s kind names.

use std::fmt;
use std::path::{Path, PathBuf};

use url::Url;

use crate::config::Config;
use crate::error::ManifestError;
use crate::files::Files;

pub use blaze_source::{
    CRATES_IO_HTTP_INDEX, CRATES_IO_INDEX, CRATES_IO_REGISTRY, GitReference, SourceId, SourceKind,
};

/// A place a package cannot come from, in cargo's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceRefusal {
    /// A manifest path with no directory above it.
    NoDirectory { manifest: PathBuf },
    /// A URL that names no local path where one is wanted.
    NotAPath { url: Url },
    /// A URL that does not parse; `ssh` is the `ssh://` form to suggest for `git@host:path`.
    BadUrl { text: String, error: url::ParseError, ssh: Option<String> },
}

impl fmt::Display for SourceRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceRefusal::NoDirectory { manifest } => {
                write!(f, "the manifest path `{}` has no directory", manifest.display())
            }
            SourceRefusal::NotAPath { url } => write!(f, "`{url}` is not a path"),
            SourceRefusal::BadUrl { text, error, ssh } => {
                write!(f, "invalid url `{text}`: {error}")?;
                if let Some(ssh) = ssh {
                    write!(f, "; try using `{ssh}` instead")?;
                }
                Ok(())
            }
        }
    }
}

impl core::error::Error for SourceRefusal {}

/// The source of the package whose manifest is at `manifest_path`: its directory.
pub fn for_manifest_path(manifest_path: &Path) -> Result<SourceId, ManifestError> {
    let dir = manifest_path.parent().ok_or_else(|| SourceRefusal::NoDirectory {
        manifest: manifest_path.to_owned(),
    })?;
    Ok(SourceId::for_path(dir)?)
}

/// crates.io as cargo names it in metadata: its git index, whatever protocol fetches it.
pub fn crates_io<F: Files>(config: &Config<F>) -> Result<SourceId, ManifestError> {
    config.check_registry_index_not_set()?;
    Ok(SourceId::crates_io())
}

/// A registry by its name in `[registries]` (cargo's `SourceId::alt_registry`).
pub fn alt_registry<F: Files>(config: &Config<F>, key: &str) -> Result<SourceId, ManifestError> {
    if key == CRATES_IO_REGISTRY {
        return crates_io(config);
    }
    let url = config.registry_index(key)?;
    Ok(SourceId::for_alt_registry(&url, key)?)
}

/// The source a `PackageIdSpec`'s kind and URL name.
pub fn from_spec(kind: &SourceKind, url: &Url) -> Result<SourceId, ManifestError> {
    let path = || {
        blaze_source::url_file(url).map_err(|()| SourceRefusal::NotAPath { url: url.clone() })
    };
    Ok(match kind {
        // clone: the interned source owns it, once per spec
        SourceKind::Git(reference) => SourceId::for_git(url, reference.clone())?,
        SourceKind::Path => SourceId::for_path(&path()?)?,
        SourceKind::Registry | SourceKind::SparseRegistry => SourceId::for_registry(url)?,
        SourceKind::LocalRegistry => SourceId::for_local_registry(&path()?)?,
        SourceKind::Directory => SourceId::for_directory(&path()?)?,
    })
}

/// A URL as cargo's `IntoUrl` parses one, with its hint for scp-like git addresses.
pub(crate) fn into_url(s: &str) -> Result<Url, SourceRefusal> {
    Url::parse(s).map_err(|error| SourceRefusal::BadUrl {
        text: s.to_owned(),
        error,
        ssh: s.starts_with("git@").then(|| format!("ssh://{}", s.replacen(':', "/", 1))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::PackageId;

    fn id(source_id: SourceId) -> String {
        PackageId { name: "a".to_owned(), version: blaze_semver::Version::new(1, 2, 3), source_id }
            .to_string()
    }

    fn url(s: &str) -> Url {
        s.parse().unwrap()
    }

    /// Each kind of source as cargo's `PackageId` display names it in a message
    /// (`src/cargo/core/source_id.rs`: a registry is ``registry `name` ``).
    #[test]
    fn a_package_reads_as_cargo_names_it() {
        let crates_io =
            SourceId::for_alt_registry(&url(CRATES_IO_INDEX), CRATES_IO_REGISTRY).unwrap();
        assert!(crates_io.is_crates_io());
        assert_eq!(id(crates_io), "a v1.2.3");
        let sparse = SourceId::for_registry(&url(CRATES_IO_HTTP_INDEX)).unwrap();
        assert!(sparse.is_crates_io());
        assert_eq!(id(sparse), "a v1.2.3");
        let named = SourceId::for_alt_registry(&url("https://r.example/index"), "mine").unwrap();
        assert_eq!(id(named), "a v1.2.3 (registry `mine`)");
        let unnamed = SourceId::for_registry(&url("https://r.example/unnamed")).unwrap();
        assert_eq!(id(unnamed), "a v1.2.3 (registry `https://r.example/unnamed`)");
        let branch = GitReference::Branch("dev".to_owned());
        assert_eq!(
            id(SourceId::for_git(&url("https://g.example/a"), branch).unwrap()),
            "a v1.2.3 (https://g.example/a?branch=dev)"
        );
        let default = GitReference::DefaultBranch;
        assert_eq!(
            id(SourceId::for_git(&url("https://g.example/a"), default).unwrap()),
            "a v1.2.3 (https://g.example/a)"
        );
        #[cfg(unix)]
        assert_eq!(id(SourceId::for_path(Path::new("/w/a")).unwrap()), "a v1.2.3 (/w/a)");
    }

    /// A spec's git kind names the source of that kind and URL.
    #[test]
    fn a_spec_kind_names_its_source() {
        let git = url("https://g.example/a");
        let branch = SourceKind::Git(GitReference::Branch("dev".to_owned()));
        let id = from_spec(&branch, &git).unwrap();
        assert_eq!(id.kind(), &branch);
        assert_eq!(id.url(), &git);
    }

    #[test]
    fn an_scp_like_address_is_refused_with_the_ssh_form() {
        let error = into_url("git@g.example:a/b").unwrap_err().to_string();
        assert!(error.ends_with("; try using `ssh://git@g.example/a/b` instead"), "{error}");
    }
}
