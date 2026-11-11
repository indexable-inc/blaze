//! An extension's source: its manifest and every `.rs` file under its `src`, as text. What
//! the checks read, what the build compiles and what the key hashes are this one value, so
//! nothing is checked that is not built and nothing built that is not keyed.

use std::{fmt, path::Path};

use crate::manifest::{Declared, Source};

/// The source of one extension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sources {
    /// As the workspace names it.
    pub name: String,
    /// Its `Cargo.toml`.
    pub manifest: String,
    /// Its files, by path from its directory with `/` between parts, sorted.
    pub files: Vec<(String, String)>,
}

impl Sources {
    /// The text of the file at `path`.
    pub fn file(&self, path: &str) -> Option<&str> {
        self.files.iter().find(|(it, _)| it == path).map(|(_, text)| text.as_str())
    }
}

/// Why an extension's source was not found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FindError {
    /// Its source is a registry package or a git revision, which this blaze does not fetch
    /// for extensions yet.
    NotFetched { extension: String, source: String },
    /// A file or directory of it could not be read.
    Read { extension: String, path: String, why: String },
    /// It has a build script, which an extension may not: a build script is a program that
    /// runs with what its sandbox allows, not with what a `Cx` hands it.
    BuildScript { extension: String, path: String },
}

impl fmt::Display for FindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FindError::NotFetched { extension, source } => write!(
                f,
                "extension `{extension}`: its source is {source}, and this blaze builds an extension only from a `path`: \
                 fetching one pinned in blaze.lock is not built yet (docs/v2/plan/design/extensions.md, slice 2)"
            ),
            FindError::Read { extension, path, why } => {
                write!(f, "extension `{extension}`: reading `{path}`: {why}")
            }
            FindError::BuildScript { extension, path } => write!(
                f,
                "extension `{extension}`: `{path}` is a build script, which an extension may not have (rule `ext::deps`)"
            ),
        }
    }
}

impl std::error::Error for FindError {}

/// How many directories deep an extension's `src` is read.
const DEPTH: usize = 16;

/// The source of `declared`, in the workspace whose root is `workspace`.
pub fn find(workspace: &Path, declared: &Declared) -> Result<Sources, FindError> {
    let dir = match &declared.source {
        Source::Path(path) => workspace.join(path),
        Source::Registry { version } => {
            return Err(FindError::NotFetched {
                extension: declared.name.clone(), // clone: error path
                source: format!("version {version} of a registry"),
            });
        }
        Source::Git { url, rev } => {
            return Err(FindError::NotFetched {
                extension: declared.name.clone(), // clone: error path
                source: format!("revision {rev} of {url}"),
            });
        }
    };
    let failed = |path: &Path, why: std::io::Error| FindError::Read {
        extension: declared.name.clone(), // clone: error path
        path: path.display().to_string(),
        why: why.to_string(),
    };
    let manifest_path = dir.join("Cargo.toml");
    let manifest =
        std::fs::read_to_string(&manifest_path).map_err(|why| failed(&manifest_path, why))?;
    if dir.join("build.rs").exists() {
        return Err(FindError::BuildScript {
            extension: declared.name.clone(), // clone: error path
            path: "build.rs".to_owned(),      // alloc: error path
        });
    }
    let mut files = Vec::new(); // alloc: an extension's files, read once per build of it
    read_dir(&dir, "src", DEPTH, &mut files, &failed)?;
    files.sort_unstable();
    Ok(Sources { name: declared.name.clone(), manifest, files }) // clone: the name, once
}

fn read_dir(
    root: &Path,
    at: &str,
    depth: usize,
    files: &mut Vec<(String, String)>,
    failed: &impl Fn(&Path, std::io::Error) -> FindError,
) -> Result<(), FindError> {
    let dir = root.join(at);
    if depth == 0 {
        let deep = std::io::Error::other(format!("more than {DEPTH} directories deep"));
        return Err(failed(&dir, deep));
    }
    let entries = std::fs::read_dir(&dir).map_err(|why| failed(&dir, why))?;
    for entry in entries {
        let entry = entry.map_err(|why| failed(&dir, why))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = format!("{at}/{name}");
        let kind = entry.file_type().map_err(|why| failed(&entry.path(), why))?;
        if kind.is_dir() {
            read_dir(root, &path, depth - 1, files, failed)?;
        } else if name.ends_with(".rs") {
            let text =
                std::fs::read_to_string(entry.path()).map_err(|why| failed(&entry.path(), why))?;
            files.push((path, text));
        }
    }
    Ok(())
}
