//! Where a package comes from: cargo's `SourceId`, which is blaze-source's, plus the two
//! constructors that read the configuration and the match of a `PackageIdSpec`'s kind.

use std::path::Path;

use anyhow::anyhow;
use cargo_util_schemas::core as spec;
use url::Url;

use crate::config::Config;
use crate::files::Files;

pub use blaze_source::{
    CRATES_IO_HTTP_INDEX, CRATES_IO_INDEX, CRATES_IO_REGISTRY, GitReference, SourceId, SourceKind,
};

/// The source of the package whose manifest is at `manifest_path`: its directory.
pub fn for_manifest_path(manifest_path: &Path) -> anyhow::Result<SourceId> {
    let dir = manifest_path
        .parent()
        .ok_or_else(|| anyhow!("the manifest path `{}` has no directory", manifest_path.display()))?;
    SourceId::for_path(dir)
}

/// crates.io as cargo names it in metadata: its git index, whatever protocol fetches it.
pub fn crates_io<F: Files>(config: &Config<F>) -> anyhow::Result<SourceId> {
    config.check_registry_index_not_set()?;
    Ok(SourceId::crates_io())
}

/// A registry by its name in `[registries]` (cargo's `SourceId::alt_registry`).
pub fn alt_registry<F: Files>(config: &Config<F>, key: &str) -> anyhow::Result<SourceId> {
    if key == CRATES_IO_REGISTRY {
        return crates_io(config);
    }
    let url = config.registry_index(key)?;
    SourceId::for_alt_registry(&url, key)
}

/// The kind as cargo's `PackageIdSpec` (cargo-util-schemas) names it, for `with_kind`.
pub fn spec_kind(kind: &SourceKind) -> spec::SourceKind {
    match kind {
        SourceKind::Git(reference) => spec::SourceKind::Git(match reference {
            GitReference::Tag(it) => spec::GitReference::Tag(it.clone()), // clone: the spec owns it, once per spec
            GitReference::Branch(it) => spec::GitReference::Branch(it.clone()), // clone: as above
            GitReference::Rev(it) => spec::GitReference::Rev(it.clone()), // clone: as above
            GitReference::DefaultBranch => spec::GitReference::DefaultBranch,
        }),
        SourceKind::Path => spec::SourceKind::Path,
        SourceKind::Registry => spec::SourceKind::Registry,
        SourceKind::SparseRegistry => spec::SourceKind::SparseRegistry,
        SourceKind::LocalRegistry => spec::SourceKind::LocalRegistry,
        SourceKind::Directory => spec::SourceKind::Directory,
    }
}

/// Whether a `PackageIdSpec`'s kind is `kind`, compared in place.
pub fn spec_kind_is(want: &spec::SourceKind, kind: &SourceKind) -> bool {
    match (want, kind) {
        (spec::SourceKind::Git(want), SourceKind::Git(reference)) => match (want, reference) {
            (spec::GitReference::Tag(a), GitReference::Tag(b))
            | (spec::GitReference::Branch(a), GitReference::Branch(b))
            | (spec::GitReference::Rev(a), GitReference::Rev(b)) => a == b,
            (spec::GitReference::DefaultBranch, GitReference::DefaultBranch) => true,
            _ => false,
        },
        (spec::SourceKind::Path, SourceKind::Path)
        | (spec::SourceKind::Registry, SourceKind::Registry)
        | (spec::SourceKind::SparseRegistry, SourceKind::SparseRegistry)
        | (spec::SourceKind::LocalRegistry, SourceKind::LocalRegistry)
        | (spec::SourceKind::Directory, SourceKind::Directory) => true,
        _ => false,
    }
}

/// The source a `PackageIdSpec`'s kind and URL name.
pub fn from_spec(kind: &spec::SourceKind, url: &Url) -> anyhow::Result<SourceId> {
    let path = || url.to_file_path().map_err(|()| anyhow!("`{url}` is not a path"));
    match kind {
        spec::SourceKind::Git(reference) => SourceId::for_git(
            url,
            match reference {
                spec::GitReference::Tag(it) => GitReference::Tag(it.clone()), // clone: the interned source owns it, once per spec
                spec::GitReference::Branch(it) => GitReference::Branch(it.clone()), // clone: as above
                spec::GitReference::Rev(it) => GitReference::Rev(it.clone()), // clone: as above
                spec::GitReference::DefaultBranch => GitReference::DefaultBranch,
            },
        ),
        spec::SourceKind::Path => SourceId::for_path(&path()?),
        spec::SourceKind::Registry | spec::SourceKind::SparseRegistry => {
            SourceId::for_registry(url)
        }
        spec::SourceKind::LocalRegistry => SourceId::for_local_registry(&path()?),
        spec::SourceKind::Directory => SourceId::for_directory(&path()?),
    }
}

/// A URL as cargo's `IntoUrl` parses one, with its hint for scp-like git addresses.
pub(crate) fn into_url(s: &str) -> anyhow::Result<Url> {
    Url::parse(s).map_err(|e| {
        if s.starts_with("git@") {
            anyhow!(
                "invalid url `{s}`: {e}; try using `{}` instead",
                format_args!("ssh://{}", s.replacen(':', "/", 1))
            )
        } else {
            anyhow!("invalid url `{s}`: {e}")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::PackageId;

    fn id(source_id: SourceId) -> String {
        PackageId { name: "a".to_owned(), version: semver::Version::new(1, 2, 3), source_id }
            .to_string()
    }

    fn url(s: &str) -> Url {
        s.parse().unwrap()
    }

    /// Each kind of source as cargo's `PackageId` display names it in a message
    /// (`src/cargo/core/source_id.rs`: a registry is ``registry `name` ``).
    #[test]
    fn a_package_reads_as_cargo_names_it() {
        let crates_io = SourceId::for_alt_registry(&url(CRATES_IO_INDEX), CRATES_IO_REGISTRY).unwrap();
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

    /// A spec's kind converts to blaze-source's and back, and matches only its own kind.
    #[test]
    fn a_spec_kind_round_trips_and_matches_itself() {
        let kinds = [
            SourceKind::Git(GitReference::Tag("t".to_owned())),
            SourceKind::Git(GitReference::Branch("b".to_owned())),
            SourceKind::Git(GitReference::Rev("r".to_owned())),
            SourceKind::Git(GitReference::DefaultBranch),
            SourceKind::Path,
            SourceKind::Registry,
            SourceKind::SparseRegistry,
            SourceKind::LocalRegistry,
            SourceKind::Directory,
        ];
        for (i, kind) in kinds.iter().enumerate() {
            let converted = spec_kind(kind);
            for (j, other) in kinds.iter().enumerate() {
                assert_eq!(spec_kind_is(&converted, other), i == j, "{kind:?} against {other:?}");
            }
        }
        let git = url("https://g.example/a");
        let branch = spec::SourceKind::Git(spec::GitReference::Branch("dev".to_owned()));
        let id = from_spec(&branch, &git).unwrap();
        assert!(spec_kind_is(&branch, id.kind()));
        assert_eq!(id.url(), &git);
    }
}
