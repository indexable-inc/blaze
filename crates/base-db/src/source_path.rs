//! Where a source file is: on disk ([`AbsPathBuf`]) or virtual (a `/`-separated path no disk
//! has: blaze-wasm's packed sources, a test's database), and paths relative to a file
//! ([`AnchoredPath`]), as `#[path]`, `mod` and `include!` name them.
use std::fmt;

use paths::{AbsPath, AbsPathBuf, RelPath};

use crate::FileId;

/// A source file's path. A source root's [`FileSet`](crate::FileSet) maps these to file IDs.
#[derive(Clone, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub struct VfsPath(VfsPathRepr);

impl VfsPath {
    /// A virtual path from a `/`-separated string.
    ///
    /// # Panics
    ///
    /// Panics if `path` does not start with `'/'`.
    pub fn new_virtual_path(path: String) -> VfsPath {
        assert!(path.starts_with('/'));
        VfsPath(VfsPathRepr::VirtualPath(VirtualPath(path)))
    }

    /// A path on disk, from the text of an absolute path.
    pub fn new_real_path(path: String) -> VfsPath {
        VfsPath::from(AbsPathBuf::assert(path.into()))
    }

    /// The path on disk, if it is one.
    pub fn as_path(&self) -> Option<&AbsPath> {
        match &self.0 {
            VfsPathRepr::PathBuf(it) => Some(it.as_path()),
            VfsPathRepr::VirtualPath(_) => None,
        }
    }

    pub fn into_abs_path(self) -> Option<AbsPathBuf> {
        match self.0 {
            VfsPathRepr::PathBuf(it) => Some(it),
            VfsPathRepr::VirtualPath(_) => None,
        }
    }

    /// `path` joined to `self`, normalized; `None` for a virtual path with more leading `../`
    /// than it has components.
    pub fn join(&self, path: &str) -> Option<VfsPath> {
        match &self.0 {
            VfsPathRepr::PathBuf(it) => {
                let res = it.join(path).normalize();
                Some(VfsPath(VfsPathRepr::PathBuf(res)))
            }
            VfsPathRepr::VirtualPath(it) => {
                let res = it.join(path)?;
                Some(VfsPath(VfsPathRepr::VirtualPath(res)))
            }
        }
    }

    /// Removes the last component; `false` when there is none.
    pub fn pop(&mut self) -> bool {
        match &mut self.0 {
            VfsPathRepr::PathBuf(it) => it.pop(),
            VfsPathRepr::VirtualPath(it) => it.pop(),
        }
    }

    /// Whether `other` is a prefix of `self`, component by component. A path on disk never
    /// starts with a virtual one, nor the other way round.
    pub fn starts_with(&self, other: &VfsPath) -> bool {
        match (&self.0, &other.0) {
            (VfsPathRepr::PathBuf(lhs), VfsPathRepr::PathBuf(rhs)) => lhs.starts_with(rhs),
            (VfsPathRepr::VirtualPath(lhs), VfsPathRepr::VirtualPath(rhs)) => lhs.starts_with(rhs),
            (VfsPathRepr::PathBuf(_) | VfsPathRepr::VirtualPath(_), _) => false,
        }
    }

    pub fn strip_prefix(&self, other: &VfsPath) -> Option<&RelPath> {
        match (&self.0, &other.0) {
            (VfsPathRepr::PathBuf(lhs), VfsPathRepr::PathBuf(rhs)) => lhs.strip_prefix(rhs),
            (VfsPathRepr::VirtualPath(lhs), VfsPathRepr::VirtualPath(rhs)) => lhs.strip_prefix(rhs),
            (VfsPathRepr::PathBuf(_) | VfsPathRepr::VirtualPath(_), _) => None,
        }
    }

    /// The path without its last component; `None` for a root.
    pub fn parent(&self) -> Option<VfsPath> {
        let mut parent = self.clone(); // clone: the parent is a path of its own
        if parent.pop() { Some(parent) } else { None }
    }

    /// The last component's stem and extension.
    pub fn name_and_extension(&self) -> Option<(&str, Option<&str>)> {
        match &self.0 {
            VfsPathRepr::PathBuf(p) => p.name_and_extension(),
            VfsPathRepr::VirtualPath(p) => p.name_and_extension(),
        }
    }

    /// Appends the path's key to `buf`: `0` on disk, `1` virtual, then its text, with a
    /// Windows path's prefix and separators normalized so equal paths have equal keys. What
    /// [`FileSetConfig`](crate::FileSetConfig) matches prefixes of: keep it private to the crate.
    pub(crate) fn encode(&self, buf: &mut Vec<u8>) {
        let tag = match &self.0 {
            VfsPathRepr::PathBuf(_) => 0,
            VfsPathRepr::VirtualPath(_) => 1,
        };
        buf.push(tag);
        match &self.0 {
            VfsPathRepr::PathBuf(path) => {
                #[cfg(windows)]
                {
                    use windows_paths::Encode;
                    let components = path.components();
                    let mut add_sep = false;
                    for component in components {
                        if add_sep {
                            windows_paths::SEP.encode(buf);
                        }
                        let len_before = buf.len();
                        match component {
                            paths::Utf8Component::Prefix(prefix) => {
                                // kind() returns a normalized and comparable path prefix.
                                prefix.kind().encode(buf);
                            }
                            paths::Utf8Component::RootDir => {
                                if !add_sep {
                                    component.as_str().encode(buf);
                                }
                            }
                            _ => component.as_str().encode(buf),
                        }

                        // some components may be encoded empty
                        add_sep = len_before != buf.len();
                    }
                }
                #[cfg(not(windows))]
                {
                    buf.extend(path.as_str().as_bytes());
                }
            }
            VfsPathRepr::VirtualPath(VirtualPath(s)) => buf.extend(s.as_bytes()),
        }
    }
}

#[cfg(windows)]
mod windows_paths {
    pub(crate) trait Encode {
        fn encode(&self, buf: &mut Vec<u8>);
    }

    impl Encode for u8 {
        fn encode(&self, buf: &mut Vec<u8>) {
            buf.push(*self);
        }
    }

    impl Encode for &str {
        fn encode(&self, buf: &mut Vec<u8>) {
            buf.extend(self.as_bytes());
        }
    }

    pub(crate) const SEP: &str = "\\";
    const VERBATIM: &str = "\\\\?\\";
    const UNC: &str = "UNC";
    const DEVICE: &str = "\\\\.\\";
    const COLON: &str = ":";

    impl Encode for paths::Utf8Prefix<'_> {
        fn encode(&self, buf: &mut Vec<u8>) {
            match self {
                paths::Utf8Prefix::Verbatim(c) => {
                    VERBATIM.encode(buf);
                    c.encode(buf);
                }
                paths::Utf8Prefix::VerbatimUNC(server, share) => {
                    VERBATIM.encode(buf);
                    UNC.encode(buf);
                    SEP.encode(buf);
                    server.encode(buf);
                    SEP.encode(buf);
                    share.encode(buf);
                }
                paths::Utf8Prefix::VerbatimDisk(d) => {
                    VERBATIM.encode(buf);
                    d.encode(buf);
                    COLON.encode(buf);
                }
                paths::Utf8Prefix::DeviceNS(device) => {
                    DEVICE.encode(buf);
                    device.encode(buf);
                }
                paths::Utf8Prefix::UNC(server, share) => {
                    SEP.encode(buf);
                    SEP.encode(buf);
                    server.encode(buf);
                    SEP.encode(buf);
                    share.encode(buf);
                }
                paths::Utf8Prefix::Disk(d) => {
                    d.encode(buf);
                    COLON.encode(buf);
                }
            }
        }
    }
}

#[derive(Clone, Ord, PartialOrd, Eq, PartialEq, Hash)]
enum VfsPathRepr {
    PathBuf(AbsPathBuf),
    VirtualPath(VirtualPath),
}

impl From<AbsPathBuf> for VfsPath {
    fn from(v: AbsPathBuf) -> Self {
        VfsPath(VfsPathRepr::PathBuf(v.normalize()))
    }
}

impl fmt::Display for VfsPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            VfsPathRepr::PathBuf(it) => it.fmt(f),
            VfsPathRepr::VirtualPath(VirtualPath(it)) => it.fmt(f),
        }
    }
}

impl fmt::Debug for VfsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Debug for VfsPathRepr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self {
            VfsPathRepr::PathBuf(it) => it.fmt(f),
            VfsPathRepr::VirtualPath(VirtualPath(it)) => it.fmt(f),
        }
    }
}

impl PartialEq<AbsPath> for VfsPath {
    fn eq(&self, other: &AbsPath) -> bool {
        match &self.0 {
            VfsPathRepr::PathBuf(lhs) => lhs == other,
            VfsPathRepr::VirtualPath(_) => false,
        }
    }
}

impl PartialEq<VfsPath> for AbsPath {
    fn eq(&self, other: &VfsPath) -> bool {
        other == self
    }
}

/// A `/`-separated path of a file on no disk.
#[derive(Debug, Clone, Ord, PartialOrd, Eq, PartialEq, Hash)]
struct VirtualPath(String);

impl VirtualPath {
    fn starts_with(&self, other: &VirtualPath) -> bool {
        <_ as AsRef<paths::Utf8Path>>::as_ref(&self.0).starts_with(&other.0)
    }

    fn strip_prefix(&self, base: &VirtualPath) -> Option<&RelPath> {
        <_ as AsRef<paths::Utf8Path>>::as_ref(&self.0)
            .strip_prefix(&base.0)
            .ok()
            .map(RelPath::new_unchecked)
    }

    /// Removes everything from the last `'/'` on; `false` when there is no `'/'`.
    fn pop(&mut self) -> bool {
        let Some(pos) = self.0.rfind('/') else { return false };
        self.0.truncate(pos);
        true
    }

    /// `self/path` as strings, each leading `../` of `path` taking a component off `self`
    /// first; `None` when there are more of them than components.
    fn join(&self, mut path: &str) -> Option<VirtualPath> {
        let mut res = self.clone(); // clone: the joined path is a path of its own
        while path.starts_with("../") {
            if !res.pop() {
                return None;
            }
            path = &path["../".len()..];
        }
        path = path.trim_start_matches("./");
        res.0.reserve(1 + path.len());
        res.0.push('/');
        res.0.push_str(path);
        Some(res)
    }

    /// The last component's stem and extension: `None` for an empty one, no extension for a
    /// name with no `.` past its first byte. `"/foo/bar.baz.rs"` is `("bar.baz", Some("rs"))`.
    fn name_and_extension(&self) -> Option<(&str, Option<&str>)> {
        let file_path = self.0.strip_suffix('/').unwrap_or(&self.0);
        let file_name = match file_path.rfind('/') {
            Some(position) => &file_path[position + 1..],
            None => file_path,
        };

        if file_name.is_empty() {
            None
        } else {
            let mut file_stem_and_extension = file_name.rsplitn(2, '.');
            let extension = file_stem_and_extension.next();
            let file_stem = file_stem_and_extension.next();

            match (file_stem, extension) {
                (None, None) => None,
                (None | Some(""), Some(_)) => Some((file_name, None)),
                (Some(file_stem), extension) => Some((file_stem, extension)),
            }
        }
    }
}

/// A path relative to a file's directory, owned.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AnchoredPathBuf {
    /// The file the path is relative to.
    pub anchor: FileId,
    /// Relative to `anchor`'s directory.
    pub path: String,
}

/// A path relative to a file's directory: a `#[path]` or `include!` is read from where the file
/// that names it is, on disk or not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AnchoredPath<'a> {
    /// The file the path is relative to.
    pub anchor: FileId,
    /// Relative to `anchor`'s directory.
    pub path: &'a str,
}

#[cfg(test)]
mod tests;
