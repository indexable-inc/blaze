//! Relative paths (docs/v2/plan/design/std.md, "Paths are relative"): a [`Path`] cannot hold
//! a root or `..`, so every open is beneath the [`crate::fs::Dir`] it is joined to. There is no
//! current directory. A path is refused as it is made, with a [`PathError`] naming it and why.

use alloc::{format, borrow::ToOwned, string::String};
use core::fmt;

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
        write!(f, "path {:?} {}", self.path, self.why.says())
    }
}

impl core::error::Error for PathError {}

impl PathWhy {
    const fn says(self) -> &'static str {
        match self {
            PathWhy::Empty => "is empty",
            PathWhy::Rooted => "starts at a root: a bstd path is relative to its directory",
            PathWhy::Parent => "has a `..`: a bstd path never leaves its directory",
            PathWhy::NotCanonical => "has a `.` or an empty part: write it as one relative path",
            PathWhy::Forbidden => "has a `\\` or a NUL",
        }
    }
}

/// Why `raw` is no path, if it is not. `parent` lets `..` through, for the test that plants it.
/// A `const fn`, so [`crate::path!`] checks a literal as the crate compiles by the same rule
/// [`Path::new`] checks at run time.
const fn refused(raw: &str, parent: bool) -> Option<PathWhy> {
    let bytes = raw.as_bytes();
    if bytes.is_empty() {
        return Some(PathWhy::Empty);
    }
    if bytes[0] == b'/' || (bytes.len() > 1 && bytes[1] == b':') {
        return Some(PathWhy::Rooted);
    }
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'\\' || bytes[at] == 0 {
            return Some(PathWhy::Forbidden);
        }
        at += 1;
    }
    // Each `/`-separated part, from `start` to the separator (or the end) at `at`.
    let (mut start, mut at) = (0, 0);
    while at <= bytes.len() {
        if at == bytes.len() || bytes[at] == b'/' {
            let len = at - start;
            if len == 2 && bytes[start] == b'.' && bytes[start + 1] == b'.' {
                if !parent {
                    return Some(PathWhy::Parent);
                }
            } else if len == 0 || (len == 1 && bytes[start] == b'.') {
                return Some(PathWhy::NotCanonical);
            }
            start = at + 1;
        }
        at += 1;
    }
    None
}

/// [`Path::new`]'s check, as a compile-time panic: what [`crate::path!`] evaluates in a
/// `const`.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub const fn check_literal(raw: &str) {
    if let Some(why) = refused(raw, false) {
        panic!("{}", why.says());
    }
}

/// A [`Path`] from a string literal, checked as the crate compiles: `path!("data/log")`. A bad
/// literal (`path!("../x")`) is a compile error saying why.
#[macro_export]
macro_rules! path {
    ($raw:literal) => {{
        const _: () = $crate::path::check_literal($raw);
        $crate::path::Path::new($raw).expect("a path! literal was checked as the crate compiled")
    }};
}

impl Path {
    #[blaze::pure]
    #[blaze::nonblocking]
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

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// `self` then `more`: two paths beneath a directory make one beneath it.
    #[blaze::nonblocking]
    pub fn join(&self, more: &Path) -> Path {
        // alloc: once per path joined.
        let joined = format!("{}/{}", self.as_str(), more.as_str());
        Path(FileName::new(&joined).expect("two paths joined are a path"))
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn name(&self) -> &FileName {
        &self.0
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
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

    #[blaze::pure]
    #[blaze::nonblocking]
    fn try_from(raw: &str) -> Result<Path, PathError> {
        Path::new(raw)
    }
}
