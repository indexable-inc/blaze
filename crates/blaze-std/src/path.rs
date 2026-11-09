//! Relative paths (docs/v2/plan/design/std.md, "Paths are relative"): a [`Path`] cannot hold
//! a root or `..`, so every open is beneath the [`crate::fs::Dir`] it is joined to. There is no
//! current directory. A path is refused as it is made, with a [`PathError`] naming it and why.

use std::fmt;

use blaze_core::FileName;

/// A relative path: `/`-separated parts, each non-empty and none `.` or `..`, with no root, no
/// `\` and no NUL.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)] // clone: a refcount, no allocation
pub struct Path(FileName);

/// What [`Path::new`] refused.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct PathError {
    pub path: String,
    pub why: PathWhy,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathWhy {
    Empty,
    /// It starts at a root (`/a`) or names a drive (`C:`).
    Rooted,
    /// A part is `..`: it would leave the directory it is joined to.
    Parent,
    /// A part is `.`, or empty (`a//b`, `a/`): one path has one spelling.
    NotCanonical,
    /// A `\` (Windows' other separator) or a NUL.
    Forbidden,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let why = match self.why {
            PathWhy::Empty => "is empty",
            PathWhy::Rooted => "starts at a root: a bstd path is relative to its directory",
            PathWhy::Parent => "has a `..`: a bstd path never leaves its directory",
            PathWhy::NotCanonical => "has a `.` or an empty part: write it as one relative path",
            PathWhy::Forbidden => "has a `\\` or a NUL",
        };
        write!(f, "path {:?} {why}", self.path)
    }
}

impl std::error::Error for PathError {}

/// Why `raw` is no path, if it is not. `parent` lets `..` through, for the test that plants it.
fn refused(raw: &str, parent: bool) -> Option<PathWhy> {
    if raw.is_empty() {
        return Some(PathWhy::Empty);
    }
    if raw.starts_with('/') || raw.as_bytes().get(1) == Some(&b':') {
        return Some(PathWhy::Rooted);
    }
    if raw.contains(['\\', '\0']) {
        return Some(PathWhy::Forbidden);
    }
    for part in raw.split('/') {
        match part {
            ".." if parent => {}
            ".." => return Some(PathWhy::Parent),
            "" | "." => return Some(PathWhy::NotCanonical),
            _ => {}
        }
    }
    None
}

impl Path {
    pub fn new(raw: &str) -> Result<Path, PathError> {
        if let Some(why) = refused(raw, false) {
            // alloc: an error's message.
            return Err(PathError { path: raw.to_owned(), why });
        }
        let name = FileName::new(raw).expect("a path is a name of blaze-core's");
        Ok(Path(name))
    }

    /// `raw` with `..` let through, for the test that plants [`crate::Plant::LexicalParent`]:
    /// the join must then resolve it by text, which is the bug.
    #[cfg(test)]
    pub(crate) fn with_parents(raw: &str) -> Result<String, PathError> {
        match refused(raw, true) {
            // alloc: an error's message.
            Some(why) => Err(PathError { path: raw.to_owned(), why }),
            // alloc: a test's planted path.
            None => Ok(raw.to_owned()),
        }
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// `self` then `more`: two paths beneath a directory make one beneath it.
    pub fn join(&self, more: &Path) -> Path {
        // alloc: once per path joined.
        let joined = format!("{}/{}", self.as_str(), more.as_str());
        Path(FileName::new(&joined).expect("two paths joined are a path"))
    }

    pub(crate) fn name(&self) -> &FileName {
        &self.0
    }

    pub(crate) fn of(name: FileName) -> Path {
        Path(name)
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Path({:?})", self.as_str())
    }
}

impl TryFrom<&str> for Path {
    type Error = PathError;

    fn try_from(raw: &str) -> Result<Path, PathError> {
        Path::new(raw)
    }
}
