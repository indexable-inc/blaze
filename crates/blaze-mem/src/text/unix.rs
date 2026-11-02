//! [`OsString`] and [`PathBuf`] in an allocator, on unix, where an `OsStr` is any bytes.

use std::{
    alloc::Allocator,
    borrow::Borrow,
    cmp::Ordering,
    ffi::OsStr,
    fmt,
    hash::{Hash, Hasher},
    ops::Deref,
    os::unix::ffi::OsStrExt,
    path::Path,
};

use super::String;

/// A growable `OsStr` in `A`: std's `OsString` with an allocator.
pub struct OsString<A: Allocator> {
    bytes: Vec<u8, A>,
}

impl<A: Allocator> OsString<A> {
    /// An empty one in `alloc`; nothing is allocated until it grows.
    #[inline]
    pub fn new_in(alloc: A) -> OsString<A> {
        OsString { bytes: Vec::new_in(alloc) }
    }

    #[inline]
    pub fn with_capacity_in(capacity: usize, alloc: A) -> OsString<A> {
        OsString { bytes: Vec::with_capacity_in(capacity, alloc) }
    }

    /// A copy of `text` in `alloc`.
    pub fn from_os_str_in(text: impl AsRef<OsStr>, alloc: A) -> OsString<A> {
        let text = text.as_ref().as_bytes();
        let mut bytes = Vec::with_capacity_in(text.len(), alloc);
        bytes.extend_from_slice(text);
        OsString { bytes }
    }

    /// Any bytes: on unix every byte string is an `OsStr`.
    #[inline]
    pub fn from_bytes(bytes: Vec<u8, A>) -> OsString<A> {
        OsString { bytes }
    }

    #[inline]
    pub fn as_os_str(&self) -> &OsStr {
        OsStr::from_bytes(&self.bytes)
    }

    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Appends `text`.
    #[inline]
    pub fn push(&mut self, text: impl AsRef<OsStr>) {
        self.bytes.extend_from_slice(text.as_ref().as_bytes());
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.bytes.reserve(additional);
    }

    /// Empties it, keeping its capacity.
    #[inline]
    pub fn clear(&mut self) {
        self.bytes.clear();
    }

    #[inline]
    pub fn allocator(&self) -> &A {
        self.bytes.allocator()
    }

    #[inline]
    pub fn into_bytes(self) -> Vec<u8, A> {
        self.bytes
    }

    /// The same bytes as a path.
    #[inline]
    pub fn into_path_buf(self) -> PathBuf<A> {
        PathBuf { bytes: self.bytes }
    }
}

impl<A: Allocator> From<String<A>> for OsString<A> {
    fn from(text: String<A>) -> OsString<A> {
        OsString { bytes: text.into_bytes() }
    }
}

impl<A: Allocator> Deref for OsString<A> {
    type Target = OsStr;

    #[inline]
    fn deref(&self) -> &OsStr {
        self.as_os_str()
    }
}

impl<A: Allocator> AsRef<OsStr> for OsString<A> {
    fn as_ref(&self) -> &OsStr {
        self.as_os_str()
    }
}

impl<A: Allocator> AsRef<Path> for OsString<A> {
    fn as_ref(&self) -> &Path {
        Path::new(self.as_os_str())
    }
}

impl<A: Allocator> Borrow<OsStr> for OsString<A> {
    fn borrow(&self) -> &OsStr {
        self.as_os_str()
    }
}

impl<A: Allocator> fmt::Debug for OsString<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_os_str(), f)
    }
}

impl<A: Allocator + Clone> Clone for OsString<A> {
    fn clone(&self) -> OsString<A> {
        OsString { bytes: self.bytes.clone() }
    }
}

// Equality, order and hash are `OsStr`'s, so `Borrow<OsStr>` holds for maps and sets.

impl<A: Allocator> Hash for OsString<A> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_os_str().hash(state);
    }
}

impl<A: Allocator, B: Allocator> PartialEq<OsString<B>> for OsString<A> {
    #[inline]
    fn eq(&self, other: &OsString<B>) -> bool {
        self.as_os_str() == other.as_os_str()
    }
}

impl<A: Allocator> Eq for OsString<A> {}

impl<A: Allocator> PartialOrd for OsString<A> {
    fn partial_cmp(&self, other: &OsString<A>) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<A: Allocator> Ord for OsString<A> {
    fn cmp(&self, other: &OsString<A>) -> Ordering {
        self.as_os_str().cmp(other.as_os_str())
    }
}

impl<A: Allocator> PartialEq<OsStr> for OsString<A> {
    fn eq(&self, other: &OsStr) -> bool {
        self.as_os_str() == other
    }
}

impl<A: Allocator> PartialEq<&OsStr> for OsString<A> {
    fn eq(&self, other: &&OsStr) -> bool {
        self.as_os_str() == *other
    }
}

impl<A: Allocator> PartialEq<str> for OsString<A> {
    fn eq(&self, other: &str) -> bool {
        self.as_os_str() == other
    }
}

impl<A: Allocator> PartialEq<&str> for OsString<A> {
    fn eq(&self, other: &&str) -> bool {
        self.as_os_str() == *other
    }
}

/// A growable `Path` in `A`: std's `PathBuf` with an allocator, joining as std's does on unix.
pub struct PathBuf<A: Allocator> {
    bytes: Vec<u8, A>,
}

impl<A: Allocator> PathBuf<A> {
    /// An empty path in `alloc`; nothing is allocated until it grows.
    #[inline]
    pub fn new_in(alloc: A) -> PathBuf<A> {
        PathBuf { bytes: Vec::new_in(alloc) }
    }

    #[inline]
    pub fn with_capacity_in(capacity: usize, alloc: A) -> PathBuf<A> {
        PathBuf { bytes: Vec::with_capacity_in(capacity, alloc) }
    }

    /// A copy of `path` in `alloc`.
    pub fn from_path_in(path: impl AsRef<Path>, alloc: A) -> PathBuf<A> {
        let path = path.as_ref().as_os_str().as_bytes();
        let mut bytes = Vec::with_capacity_in(path.len(), alloc);
        bytes.extend_from_slice(path);
        PathBuf { bytes }
    }

    #[inline]
    pub fn as_path(&self) -> &Path {
        Path::new(self.as_os_str())
    }

    #[inline]
    pub fn as_os_str(&self) -> &OsStr {
        OsStr::from_bytes(&self.bytes)
    }

    /// Extends it with `path` as std's `PathBuf::push` does on unix: a `path` with a root
    /// replaces it, and otherwise a `/` goes between unless it already ends in one or is empty.
    pub fn push(&mut self, path: impl AsRef<Path>) {
        let path = path.as_ref();
        if path.has_root() {
            self.bytes.clear();
        } else if self.bytes.last().is_some_and(|&last| last != b'/') {
            self.bytes.push(b'/');
        }
        self.bytes.extend_from_slice(path.as_os_str().as_bytes());
    }

    /// Cuts it to its parent, as std's `PathBuf::pop` does; false when it has none.
    pub fn pop(&mut self) -> bool {
        match self.as_path().parent().map(|parent| parent.as_os_str().len()) {
            Some(len) => {
                self.bytes.truncate(len);
                true
            }
            None => false,
        }
    }

    /// A copy with `path` pushed, in the same allocator: `Path::join` without the global heap.
    pub fn join(&self, path: impl AsRef<Path>) -> PathBuf<A>
    where
        A: Clone,
    {
        let path = path.as_ref();
        let mut out = PathBuf::with_capacity_in(
            self.bytes.len() + 1 + path.as_os_str().len(),
            self.allocator().clone(),
        );
        out.bytes.extend_from_slice(&self.bytes);
        out.push(path);
        out
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.bytes.reserve(additional);
    }

    /// Empties it, keeping its capacity.
    #[inline]
    pub fn clear(&mut self) {
        self.bytes.clear();
    }

    #[inline]
    pub fn allocator(&self) -> &A {
        self.bytes.allocator()
    }

    #[inline]
    pub fn into_os_string(self) -> OsString<A> {
        OsString { bytes: self.bytes }
    }

    #[inline]
    pub fn into_bytes(self) -> Vec<u8, A> {
        self.bytes
    }
}

impl<A: Allocator> From<String<A>> for PathBuf<A> {
    fn from(text: String<A>) -> PathBuf<A> {
        PathBuf { bytes: text.into_bytes() }
    }
}

impl<A: Allocator> From<OsString<A>> for PathBuf<A> {
    fn from(text: OsString<A>) -> PathBuf<A> {
        text.into_path_buf()
    }
}

impl<A: Allocator> Deref for PathBuf<A> {
    type Target = Path;

    #[inline]
    fn deref(&self) -> &Path {
        self.as_path()
    }
}

impl<A: Allocator> AsRef<Path> for PathBuf<A> {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl<A: Allocator> AsRef<OsStr> for PathBuf<A> {
    fn as_ref(&self) -> &OsStr {
        self.as_os_str()
    }
}

impl<A: Allocator> Borrow<Path> for PathBuf<A> {
    fn borrow(&self) -> &Path {
        self.as_path()
    }
}

impl<A: Allocator> fmt::Debug for PathBuf<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_path(), f)
    }
}

impl<A: Allocator + Clone> Clone for PathBuf<A> {
    fn clone(&self) -> PathBuf<A> {
        PathBuf { bytes: self.bytes.clone() }
    }
}

// Equality, order and hash are `Path`'s (by components, so `a//b` equals `a/b`), so
// `Borrow<Path>` holds for maps and sets.

impl<A: Allocator> Hash for PathBuf<A> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_path().hash(state);
    }
}

impl<A: Allocator, B: Allocator> PartialEq<PathBuf<B>> for PathBuf<A> {
    #[inline]
    fn eq(&self, other: &PathBuf<B>) -> bool {
        self.as_path() == other.as_path()
    }
}

impl<A: Allocator> Eq for PathBuf<A> {}

impl<A: Allocator> PartialOrd for PathBuf<A> {
    fn partial_cmp(&self, other: &PathBuf<A>) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<A: Allocator> Ord for PathBuf<A> {
    fn cmp(&self, other: &PathBuf<A>) -> Ordering {
        self.as_path().cmp(other.as_path())
    }
}

impl<A: Allocator> PartialEq<Path> for PathBuf<A> {
    fn eq(&self, other: &Path) -> bool {
        self.as_path() == other
    }
}

impl<A: Allocator> PartialEq<&Path> for PathBuf<A> {
    fn eq(&self, other: &&Path) -> bool {
        self.as_path() == *other
    }
}
