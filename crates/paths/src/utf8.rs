//! `Utf8Path` and `Utf8PathBuf`: a `Path` and a `PathBuf` known to be UTF-8, so each lends its
//! text as `&str`, in place of camino's. Each is `#[repr(transparent)]` over std's type and
//! delegates to it; the one invariant, UTF-8, is checked where a path comes in from outside
//! (`from_path`, `from_path_buf`) and kept by every operation, since std splits a path only at
//! ASCII separators and dots.

use std::{
    borrow::Borrow,
    ffi::OsStr,
    fmt,
    hash::{Hash, Hasher},
    iter::FusedIterator,
    ops,
    path::{self, Component, Path, PathBuf, StripPrefixError},
};

/// A borrowed path whose bytes are UTF-8.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Utf8Path(Path);

/// The text of part of a UTF-8 path.
fn text(part: &OsStr) -> &str {
    // SAFETY: every `OsStr` this is called on is a part of a `Utf8Path` that std cut at an ASCII
    // separator or dot, so its bytes are UTF-8 as the whole path's are.
    unsafe { str::from_utf8_unchecked(part.as_encoded_bytes()) }
}

impl Utf8Path {
    pub fn new(text: &(impl AsRef<str> + ?Sized)) -> &Utf8Path {
        Utf8Path::assume_utf8(Path::new(text.as_ref()))
    }

    /// `path`, when it is UTF-8.
    pub fn from_path(path: &Path) -> Option<&Utf8Path> {
        path.to_str().map(Utf8Path::new)
    }

    /// `path`, which the caller knows is UTF-8.
    fn assume_utf8(path: &Path) -> &Utf8Path {
        // SAFETY: `Utf8Path` is `#[repr(transparent)]` over `Path`, so the two share layout and
        // pointer metadata, and the reference keeps `path`'s lifetime; every caller passes a
        // path that is UTF-8 (text, or a part of a `Utf8Path`).
        unsafe { &*(path as *const Path as *const Utf8Path) }
    }

    pub fn as_str(&self) -> &str {
        text(self.0.as_os_str())
    }

    pub fn as_std_path(&self) -> &Path {
        &self.0
    }

    pub fn as_os_str(&self) -> &OsStr {
        self.0.as_os_str()
    }

    pub fn to_path_buf(&self) -> Utf8PathBuf {
        Utf8PathBuf(self.0.to_path_buf())
    }

    pub fn join(&self, path: impl AsRef<Utf8Path>) -> Utf8PathBuf {
        Utf8PathBuf(self.0.join(&path.as_ref().0))
    }

    pub fn parent(&self) -> Option<&Utf8Path> {
        self.0.parent().map(Utf8Path::assume_utf8)
    }

    pub fn file_name(&self) -> Option<&str> {
        self.0.file_name().map(text)
    }

    pub fn file_stem(&self) -> Option<&str> {
        self.0.file_stem().map(text)
    }

    pub fn extension(&self) -> Option<&str> {
        self.0.extension().map(text)
    }

    pub fn has_root(&self) -> bool {
        self.0.has_root()
    }

    pub fn is_absolute(&self) -> bool {
        self.0.is_absolute()
    }

    pub fn is_relative(&self) -> bool {
        self.0.is_relative()
    }

    pub fn strip_prefix(&self, base: impl AsRef<Path>) -> Result<&Utf8Path, StripPrefixError> {
        self.0.strip_prefix(base).map(Utf8Path::assume_utf8)
    }

    pub fn starts_with(&self, base: impl AsRef<Path>) -> bool {
        self.0.starts_with(base)
    }

    pub fn ends_with(&self, child: impl AsRef<Path>) -> bool {
        self.0.ends_with(child)
    }

    pub fn components(&self) -> Utf8Components<'_> {
        Utf8Components(self.0.components())
    }
}

impl Hash for Utf8Path {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl fmt::Debug for Utf8Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for Utf8Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), f)
    }
}

impl ToOwned for Utf8Path {
    type Owned = Utf8PathBuf;
    fn to_owned(&self) -> Utf8PathBuf {
        self.to_path_buf()
    }
}

impl AsRef<Utf8Path> for Utf8Path {
    fn as_ref(&self) -> &Utf8Path {
        self
    }
}

impl AsRef<Path> for Utf8Path {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<OsStr> for Utf8Path {
    fn as_ref(&self) -> &OsStr {
        self.0.as_os_str()
    }
}

impl AsRef<str> for Utf8Path {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<Utf8Path> for str {
    fn as_ref(&self) -> &Utf8Path {
        Utf8Path::new(self)
    }
}

impl AsRef<Utf8Path> for String {
    fn as_ref(&self) -> &Utf8Path {
        Utf8Path::new(self)
    }
}

/// An owned path whose bytes are UTF-8.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Utf8PathBuf(PathBuf);

impl Utf8PathBuf {
    pub fn new() -> Utf8PathBuf {
        Utf8PathBuf(PathBuf::new())
    }

    pub fn with_capacity(capacity: usize) -> Utf8PathBuf {
        Utf8PathBuf(PathBuf::with_capacity(capacity))
    }

    /// `path`, or `path` back when it is not UTF-8.
    pub fn from_path_buf(path: PathBuf) -> Result<Utf8PathBuf, PathBuf> {
        if path.to_str().is_some() { Ok(Utf8PathBuf(path)) } else { Err(path) }
    }

    pub fn as_path(&self) -> &Utf8Path {
        Utf8Path::assume_utf8(&self.0)
    }

    pub fn push(&mut self, path: impl AsRef<Utf8Path>) {
        self.0.push(&path.as_ref().0);
    }

    pub fn pop(&mut self) -> bool {
        self.0.pop()
    }

    pub fn into_std_path_buf(self) -> PathBuf {
        self.0
    }

    pub fn into_string(self) -> String {
        // SAFETY: the bytes of a `Utf8PathBuf` are UTF-8.
        unsafe { String::from_utf8_unchecked(self.0.into_os_string().into_encoded_bytes()) }
    }
}

impl ops::Deref for Utf8PathBuf {
    type Target = Utf8Path;
    fn deref(&self) -> &Utf8Path {
        self.as_path()
    }
}

impl Borrow<Utf8Path> for Utf8PathBuf {
    fn borrow(&self) -> &Utf8Path {
        self.as_path()
    }
}

impl fmt::Debug for Utf8PathBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_path(), f)
    }
}

impl fmt::Display for Utf8PathBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_path(), f)
    }
}

impl From<String> for Utf8PathBuf {
    fn from(text: String) -> Utf8PathBuf {
        Utf8PathBuf(PathBuf::from(text))
    }
}

impl From<&str> for Utf8PathBuf {
    fn from(text: &str) -> Utf8PathBuf {
        Utf8PathBuf(PathBuf::from(text))
    }
}

impl From<&Utf8Path> for Utf8PathBuf {
    fn from(path: &Utf8Path) -> Utf8PathBuf {
        path.to_path_buf()
    }
}

impl From<Utf8PathBuf> for PathBuf {
    fn from(path: Utf8PathBuf) -> PathBuf {
        path.0
    }
}

impl AsRef<Utf8Path> for Utf8PathBuf {
    fn as_ref(&self) -> &Utf8Path {
        self.as_path()
    }
}

impl AsRef<Path> for Utf8PathBuf {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<OsStr> for Utf8PathBuf {
    fn as_ref(&self) -> &OsStr {
        self.0.as_os_str()
    }
}

impl AsRef<str> for Utf8PathBuf {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// A part of a UTF-8 path: std's [`Component`] with its text as `&str`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Utf8Component<'a> {
    /// A Windows prefix (`C:`, `\\server\share`); none on the hosts blaze runs on.
    Prefix(path::PrefixComponent<'a>),
    RootDir,
    CurDir,
    ParentDir,
    Normal(&'a str),
}

impl<'a> Utf8Component<'a> {
    pub fn as_str(&self) -> &'a str {
        match self {
            Utf8Component::Prefix(prefix) => text(prefix.as_os_str()),
            Utf8Component::RootDir => path::MAIN_SEPARATOR_STR,
            Utf8Component::CurDir => ".",
            Utf8Component::ParentDir => "..",
            Utf8Component::Normal(part) => part,
        }
    }

    fn of(component: Component<'a>) -> Utf8Component<'a> {
        match component {
            Component::Prefix(prefix) => Utf8Component::Prefix(prefix),
            Component::RootDir => Utf8Component::RootDir,
            Component::CurDir => Utf8Component::CurDir,
            Component::ParentDir => Utf8Component::ParentDir,
            Component::Normal(part) => Utf8Component::Normal(text(part)),
        }
    }
}

impl AsRef<Utf8Path> for Utf8Component<'_> {
    fn as_ref(&self) -> &Utf8Path {
        Utf8Path::new(self.as_str())
    }
}

/// The parts of a UTF-8 path, from either end.
#[derive(Clone, Debug)]
pub struct Utf8Components<'a>(path::Components<'a>);

impl<'a> Iterator for Utf8Components<'a> {
    type Item = Utf8Component<'a>;
    fn next(&mut self) -> Option<Utf8Component<'a>> {
        self.0.next().map(Utf8Component::of)
    }
}

impl DoubleEndedIterator for Utf8Components<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0.next_back().map(Utf8Component::of)
    }
}

impl FusedIterator for Utf8Components<'_> {}

impl<'a> Utf8Components<'a> {
    /// What is left, as a path.
    pub fn as_path(&self) -> &'a Utf8Path {
        Utf8Path::assume_utf8(self.0.as_path())
    }
}
