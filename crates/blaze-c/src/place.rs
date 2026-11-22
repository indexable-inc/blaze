//! Where a header is read from: a [`Root`] and a [`RelPath`] under it. No path of the machine is
//! ever held (rule 25): the package's own folder and the toolchain's header shelf are roots the
//! driver maps to folders, and every key, record and finding holds only the root's byte and the
//! relative path's text or digest.

use blaze_ids::{ContentId, Hasher};
use blaze_own::{AString, Alloc, String};

/// What a relative path is under.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Root {
    /// The package that holds the import: its `header` and `include` paths are under it.
    Package = 0,
    /// The toolchain's header shelf (libc's headers and the compiler's own, `stddef.h` and the
    /// rest): the same for everyone with one toolchain digest, which the key holds.
    Shelf = 1,
}

impl Root {
    pub const fn byte(self) -> u8 {
        self as u8
    }

    pub const fn from_byte(byte: u8) -> Option<Root> {
        match byte {
            0 => Some(Root::Package),
            1 => Some(Root::Shelf),
            _ => None,
        }
    }
}

/// A relative path that stays under its root: not empty, `/`-separated, no empty, `.` or `..`
/// component, no leading `/`, no `\` and no NUL. Made only by [`RelPath::new`] (rule 76), so a
/// value of the type is a path that cannot escape its directory.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RelPath<'t>(&'t str);

/// Why a text is not a [`RelPath`]: each with the byte it is at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlaceError {
    Empty,
    /// Starts with `/`: a path of the machine.
    Absolute,
    /// A `..` component at `at`: it would leave its root.
    Parent { at: u32 },
    /// A `.` or empty component at `at` (`a/./b`, `a//b`, a trailing `/`): two spellings of one
    /// path would be two keys.
    NotCanonical { at: u32 },
    /// A `\` (a second separator on some hosts) or a NUL at `at`.
    Byte { at: u32 },
    /// Longer than [`RelPath::MOST`].
    TooLong,
}

impl<'t> RelPath<'t> {
    /// The longest relative path taken (rule 18).
    pub const MOST: usize = 4096;

    pub fn new(text: &'t str) -> Result<RelPath<'t>, PlaceError> {
        if text.is_empty() {
            return Err(PlaceError::Empty);
        }
        if text.len() > RelPath::MOST {
            return Err(PlaceError::TooLong);
        }
        if text.starts_with('/') {
            return Err(PlaceError::Absolute);
        }
        let position = |at: usize| u32::try_from(at).expect("under RelPath::MOST");
        if let Some(at) = text.bytes().position(|b| b == b'\\' || b == 0) {
            return Err(PlaceError::Byte { at: position(at) });
        }
        let mut start = 0;
        for part in text.split('/') {
            match part {
                ".." => return Err(PlaceError::Parent { at: position(start) }),
                "" | "." => return Err(PlaceError::NotCanonical { at: position(start) }),
                _ => {}
            }
            start += part.len() + 1;
        }
        Ok(RelPath(text))
    }

    pub fn as_str(self) -> &'t str {
        self.0
    }

    /// The folder this path is in, `None` for a path at its root's top.
    pub fn parent(self) -> Option<RelPath<'t>> {
        self.0.rfind('/').map(|at| RelPath(&self.0[..at]))
    }
}

/// A header's place: its root and its path under it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Place<'t> {
    pub root: Root,
    pub path: RelPath<'t>,
}

/// A folder headers are searched in: a root, and a path under it (`None`: the root itself).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Dir<'t> {
    pub root: Root,
    pub path: Option<RelPath<'t>>,
}

impl<'t> Dir<'t> {
    /// The path of `name` (an `#include`'s text, itself a [`RelPath`]) in this folder, written
    /// into `alloc`. The result is a [`RelPath`] when both are: a join of two adds no `..`.
    pub fn join<'a>(self, name: RelPath<'_>, alloc: Alloc<'a>) -> AString<'a> {
        let mut out = String::new_in(alloc);
        if let Some(dir) = self.path {
            out.push_str(dir.as_str());
            out.push_str("/");
        }
        out.push_str(name.as_str());
        out
    }
}

/// The digest of a header's root and relative path: what a declaration's [`crate::model::HeaderRow`]
/// holds, so two front ends that read one header name it with one value.
pub fn path_digest(root: Root, path: RelPath<'_>) -> ContentId {
    let mut hasher = Hasher::new(crate::model::PATH_DOMAIN);
    hasher.update(&[root.byte()]);
    hasher.part(path.as_str().as_bytes());
    hasher.finish()
}
