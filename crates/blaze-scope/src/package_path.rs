//! A file's path in its package (D96): what a record holds for a file, never a root.
//!
//! The solve reads a crate's files in its caller's path space (blaze-world's trees `/<tree>/`,
//! the examples' `/package/<lock index>/`, the ladder's `/toolchain/<unit>/`). There a file's
//! path holds where THIS workspace placed the package: a number another workspace gives
//! differently, so a record holding it is not equal between two workspaces that use one
//! unchanged dependency, and no record is shared. A record holds instead what is the same
//! everywhere: the file's path past its package's directory (the crate's
//! [`crate::resolve::CrateInput::shown_from`]). A reader finds the file by the crate it reads
//! for and this path, joined onto that crate's directory in its own path space at read time
//! ([`PackagePath::in_package`]).
//!
//! The form, checked by [`PackagePath::new`] and made so by [`PackagePath::of`]: segments
//! separated by `/`, none empty or `.`; `..` only at the start (a workspace member's file
//! written above its own directory, `#[path = "../../shared/x.rs"]`); never a leading `/`. The
//! empty path is the package's directory itself, which names no file.

use blaze_own::{AString, AVec, Alloc, BString, Bump};

/// A path relative to its crate's package directory: the only form of a file's path a record
/// holds (see the module docs).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct PackagePath<'a>(&'a str);

/// Why a text is not a [`PackagePath`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotPackagePath {
    /// It starts with `/`: a root of some path space, which no record may hold.
    Rooted,
    /// A segment is empty (`a//b`, a trailing `/`) or `.`.
    EmptySegment,
    /// `..` after a named segment: the path is not in its shortest form.
    InnerParent,
}

impl<'a> PackagePath<'a> {
    /// `text` as a package path, checked against the form (module docs).
    pub fn new(text: &'a str) -> Result<PackagePath<'a>, NotPackagePath> {
        if text.is_empty() {
            return Ok(PackagePath(text));
        }
        if text.starts_with('/') {
            return Err(NotPackagePath::Rooted);
        }
        let mut named = false;
        for segment in text.split('/') {
            match segment {
                "" | "." => return Err(NotPackagePath::EmptySegment),
                ".." if named => return Err(NotPackagePath::InnerParent),
                ".." => {}
                _ => named = true,
            }
        }
        Ok(PackagePath(text))
    }

    /// The path of `path` past the directory `package`, both in one path space and read from
    /// its root (a leading `/` is the root; `.` and `..` are taken out by text, a `..` at the
    /// root staying there as the walk's `pop_dir` does): `..` once per segment of `package`
    /// that `path` is not under.
    pub fn of(path: &str, package: &str, alloc: Bump<'a>) -> PackagePath<'a> {
        let file = segments(path, alloc.alloc());
        let dir = segments(package, alloc.alloc());
        let common = file.iter().zip(dir.iter()).take_while(|(a, b)| a == b).count();
        let mut out = BString::new_in(alloc);
        for _ in common..dir.len() {
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str("..");
        }
        for segment in &file[common..] {
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(segment);
        }
        PackagePath(out.into_bump_str())
    }

    /// The text of the path.
    pub fn as_str(self) -> &'a str {
        self.0
    }

    /// The path in the reader's path space: joined onto `package`, the directory the reader's
    /// world placed the crate's package at. `None` when its `..`s climb above the root of that
    /// space: no file of it is meant.
    pub fn in_package<'b>(self, package: &str, alloc: Alloc<'b>) -> Option<AString<'b>> {
        let mut at = segments(package, alloc);
        for segment in self.0.split('/').filter(|s| !s.is_empty()) {
            if segment == ".." {
                at.pop()?;
            } else {
                at.push(segment);
            }
        }
        let mut out = AString::new_in(alloc);
        if package.starts_with('/') {
            out.push('/');
        }
        for (i, segment) in at.iter().enumerate() {
            if i > 0 {
                out.push('/');
            }
            out.push_str(segment);
        }
        Some(out)
    }
}

/// The segments of `path` from its root, `.` and empty ones dropped and `..` taken out by text
/// (at the root it stays there).
fn segments<'p, 'b>(path: &'p str, alloc: Alloc<'b>) -> AVec<'b, &'p str> {
    let mut out = AVec::new_in(alloc);
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            segment => out.push(segment),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    //! The form and the round trip a reader relies on: a path made by [`PackagePath::of`] from
    //! a file and its package directory, joined back onto that directory, is the file's path.

    use super::*;

    fn g() -> Alloc<'static> {
        Alloc::global()
    }

    #[test]
    fn a_file_under_its_package_is_its_path_past_the_directory() {
        let inline = blaze_own::Inline::<1024>::new();
        let erased = blaze_own::Erased::new(&inline);
        let bump = erased.bump();
        // The same file of one package placed at two trees: one record path.
        let a = PackagePath::of("/3/serde-1.0.0/src/de/mod.rs", "/3/serde-1.0.0/", bump);
        let b = PackagePath::of("/7/serde-1.0.0/src/de/mod.rs", "/7/serde-1.0.0/", bump);
        assert_eq!((a.as_str(), b.as_str()), ("src/de/mod.rs", "src/de/mod.rs"));
        assert_eq!(a.in_package("/7/serde-1.0.0/", g()).as_deref(), Some("/7/serde-1.0.0/src/de/mod.rs"));
    }

    #[test]
    fn a_file_above_its_package_keeps_leading_parents() {
        let inline = blaze_own::Inline::<1024>::new();
        let erased = blaze_own::Erased::new(&inline);
        let bump = erased.bump();
        let p = PackagePath::of("/0/crates/zones/rpc/test-utils/auth.rs", "/0/crates/zones/node/", bump);
        assert_eq!(p.as_str(), "../rpc/test-utils/auth.rs");
        assert_eq!(PackagePath::new(p.as_str()), Ok(p));
        assert_eq!(p.in_package("/0/crates/zones/node", g()).as_deref(), Some("/0/crates/zones/rpc/test-utils/auth.rs"));
        // Climbing above the reader's root names no file.
        assert_eq!(p.in_package("/", g()), None);
    }

    #[test]
    fn the_form_is_checked() {
        assert_eq!(PackagePath::new("/3/src/lib.rs"), Err(NotPackagePath::Rooted));
        assert_eq!(PackagePath::new("src//lib.rs"), Err(NotPackagePath::EmptySegment));
        assert_eq!(PackagePath::new("src/./lib.rs"), Err(NotPackagePath::EmptySegment));
        assert_eq!(PackagePath::new("src/../lib.rs"), Err(NotPackagePath::InnerParent));
        assert!(PackagePath::new("../../x.rs").is_ok());
        assert!(PackagePath::new("src/lib.rs").is_ok());
    }
}
