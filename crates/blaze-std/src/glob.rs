//! Unix shell glob matching (`Pattern`, `GlobSet`) and capability-confined directory globbing
//! (`glob_dir`) over [`crate::fs::Dir`].
//!
//! Re-exports [`blaze_glob::Pattern`] (O(t * n) non-backtracking matcher — one home, rule 30) and
//! adds:
//! - [`GlobSet`]: multi-pattern include/exclude matching (`!pattern` negation rules, last match
//!   wins, like `.gitignore` / `Bun.Glob` sets).
//! - [`glob_dir`]: capability-confined, iterative (zero recursion), bounded directory walker over
//!   [`crate::fs::Dir::own_entries`] and [`crate::fs::Dir::entries`] — never leaves the granted
//!   directory and works identically under `Io`'s simulation.

use core::alloc::Allocator;
use core::fmt;

pub use blaze_glob::{Pattern, PatternError, Refusal};
use blaze_own::{AllocFailure, Vec};

use crate::{
    cx::Cx,
    fs::{Dir, EntryKind, FsError, Rights},
    path::{Path, PathError},
};

/// A set of include/exclude glob rules evaluated in order (last matching rule wins, or any
/// include matches if no exclude overrides it).
#[derive(Debug)]
pub struct GlobSet<'a, A: Allocator> {
    rules: Vec<GlobRule<'a, A>, A>,
}

#[derive(Debug)]
struct GlobRule<'a, A: Allocator> {
    pattern: Pattern<'a, A>,
    exclude: bool,
}

/// Error returned while compiling a [`GlobSet`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GlobSetError {
    Pattern { index: usize, error: PatternError },
    Alloc(AllocFailure),
}

impl fmt::Display for GlobSetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            GlobSetError::Pattern { index, error } => {
                write!(f, "bstd::glob: pattern #{index}: {error}")
            }
            GlobSetError::Alloc(err) => write!(f, "bstd::glob: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for GlobSetError {}

impl<'a, A: Allocator + Copy> GlobSet<'a, A> {
    /// Compile a slice of glob patterns. Patterns starting with `!` are exclusion rules.
    pub fn new(patterns: &[&'a str], alloc: A) -> Result<Self, GlobSetError> {
        let mut rules = Vec::new_in(alloc);
        for (index, &raw) in patterns.iter().enumerate() {
            let (exclude, body) = match raw.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, raw),
            };
            let pattern =
                Pattern::new_in(body, alloc).map_err(|error| GlobSetError::Pattern { index, error })?;
            rules.try_push(GlobRule { pattern, exclude }).map_err(|r| GlobSetError::Alloc(r.why))?;
        }
        Ok(Self { rules })
    }

    /// Return `true` if `path` matches at least one include rule and is not excluded by a later
    /// `!` rule.
    #[must_use]
    pub fn is_match(&self, path: &str) -> bool {
        let mut matched = false;
        for rule in self.rules.as_slice() {
            if rule.pattern.matches(path) {
                matched = !rule.exclude;
            }
        }
        matched
    }
}

/// Error returned by [`glob_dir`].
#[derive(Debug)]
pub enum GlobDirError {
    Fs(FsError),
    Path(PathError),
    LimitExceeded { visited: usize, max_entries: usize },
    Alloc(AllocFailure),
}

impl fmt::Display for GlobDirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GlobDirError::Fs(err) => write!(f, "bstd::glob: {err}"),
            GlobDirError::Path(err) => write!(f, "bstd::glob: {err}"),
            GlobDirError::LimitExceeded { visited, max_entries } => {
                write!(f, "bstd::glob: visited {visited} entries, exceeding limit {max_entries}")
            }
            GlobDirError::Alloc(err) => write!(f, "bstd::glob: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for GlobDirError {}

/// Walk `dir` iteratively (zero recursion, bounded by `max_entries`) using `Dir::own_entries` and
/// `Dir::entries`, returning all relative [`Path`]s matching `pattern` in sorted lexicographic order.
pub fn glob_dir<P: Rights, A: Allocator + Copy>(
    cx: &Cx,
    dir: &Dir<P>,
    pattern: &Pattern<'_, A>,
    max_entries: usize,
    alloc: A,
) -> Result<Vec<Path, A>, GlobDirError> {
    let mut matches: Vec<Path, A> = Vec::new_in(alloc);
    let mut pending_dirs: Vec<Option<Path>, A> = Vec::new_in(alloc);
    pending_dirs.try_push(None).map_err(|r| GlobDirError::Alloc(r.why))?;
    let mut visited = 0usize;

    while let Some(folder) = pending_dirs.pop() {
        let entries = match folder {
            None => dir.own_entries(cx).map_err(GlobDirError::Fs)?,
            Some(ref sub) => match dir.entries(cx, sub).map_err(GlobDirError::Fs)? {
                Some(e) => e,
                None => continue,
            },
        };

        let count = entries.len();
        let mut child_dirs: Vec<Path, A> = Vec::new_in(alloc);
        for i in 0..count {
            visited += 1;
            if visited > max_entries {
                return Err(GlobDirError::LimitExceeded { visited, max_entries });
            }
            let path = entries.path(i).map_err(GlobDirError::Path)?;
            let kind = entries.kind(i);
            if pattern.matches(path.as_str()) {
                matches.try_push(path).map_err(|r| GlobDirError::Alloc(r.why))?;
                if kind == EntryKind::Dir {
                    let sub_path = entries.path(i).map_err(GlobDirError::Path)?;
                    child_dirs.try_push(sub_path).map_err(|r| GlobDirError::Alloc(r.why))?;
                }
            } else if kind == EntryKind::Dir {
                child_dirs.try_push(path).map_err(|r| GlobDirError::Alloc(r.why))?;
            }
        }
        // Push subdirectories in reverse so lexicographically earlier folders pop first.
        while let Some(sub) = child_dirs.pop() {
            pending_dirs.try_push(Some(sub)).map_err(|r| GlobDirError::Alloc(r.why))?;
        }
    }

    matches.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(matches)
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::heap;

    use super::*;
    use crate::{
        Plant,
        fs::{Content, Durable},
        io::{Io, Limits, SimWorld},
        mem::TASK_FRAME,
    };

    #[test]
    fn glob_set_include_and_exclude() {
        let h = heap();
        let set = GlobSet::new(&["**/*.rs", "!**/target/**"], h.alloc()).expect("globset");
        assert!(set.is_match("crates/blaze-std/src/lib.rs"));
        assert!(!set.is_match("crates/blaze-std/target/debug/lib.rs"));
        assert!(!set.is_match("README.md"));
    }

    #[test]
    fn glob_dir_walks_simulated_dir_confined() {
        let h = heap();
        let io = Io::planted(SimWorld::new(7, Limits::USUAL), Plant::None);
        let cx = io
            .cx(core::time::Duration::from_secs(60), TASK_FRAME)
            .expect("cx");
        let root = io.need_fs().expect("fs").root();
        root.publish(&cx, &Path::new("src/lib.rs").expect("p"), Content::bytes(b""), Durable::No)
            .expect("write");
        root.publish(&cx, &Path::new("src/sub/mod.rs").expect("p"), Content::bytes(b""), Durable::No)
            .expect("write");
        root.publish(&cx, &Path::new("README.md").expect("p"), Content::bytes(b""), Durable::No)
            .expect("write");

        let pat = Pattern::new_in("**/*.rs", h.alloc()).expect("pat");
        let found = glob_dir(&cx, &root, &pat, 1024, h.alloc()).expect("walk");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].as_str(), "src/lib.rs");
        assert_eq!(found[1].as_str(), "src/sub/mod.rs");
    }
}
