//! Where each path cargo names is (its mount's root and its path under that root), and the
//! walk that reads a directory's files through the tree, following symlinks as the disk would.

#![forbid(unsafe_code)]

use std::cell::Cell;

use blaze_ids::ContentId;
use blaze_query::Tree;
use blaze_units::query::{Mount, Origin};
use blaze_vfs::{Bytes, Mode};

use super::LowerError;

/// Symlinks followed in a row before a path is refused, as Linux's `MAXSYMLINKS`.
const MAX_LINKS: u32 = 40;

pub(crate) struct Mounts<'a, T> {
    pub(crate) mounts: &'a [Mount],
    /// The root of `Origin::View` mounts.
    pub(crate) view: ContentId,
    pub(crate) tree: &'a T,
    /// Set as a path is found in an `Origin::View` mount: what read it read the view.
    pub(crate) read_view: Cell<bool>,
}

/// A file read for the front end, by its absolute path as cargo names it.
pub(crate) struct Loaded {
    pub(crate) path: String,
    pub(crate) id: ContentId,
    pub(crate) bytes: Bytes,
}

enum Node {
    File(ContentId, Bytes),
    Dir(String),
    None,
}

impl<T: Tree> Mounts<'_, T> {
    /// The root and the path under it of `path`, absolute: by its longest mount.
    pub(crate) fn locate(&self, path: &str) -> Result<(ContentId, String), LowerError> {
        let mount = self
            .mounts
            .iter()
            .filter(|mount| rest(path, &mount.at).is_some())
            .max_by_key(|mount| mount.at.trim_end_matches('/').len());
        let Some(mount) = mount else {
            return Err(LowerError::at(
                format!(
                    "`{path}` is under no mount of the build: blaze reads a file only from the view \
                     and the sources it mounted, never from the disk"
                ),
                path,
            ));
        };
        let rest = rest(path, &mount.at).unwrap_or_default();
        let under = match (mount.under.as_str(), rest) {
            ("", rest) => rest.to_owned(),
            (under, "") => under.to_owned(),
            (under, rest) => format!("{under}/{rest}"),
        };
        let root = match mount.origin {
            Origin::View => {
                self.read_view.set(true);
                self.view
            }
            Origin::Root(root) => root,
        };
        Ok((root, under))
    }

    /// Whether `path` is under a mount.
    pub(crate) fn covers(&self, path: &str) -> bool {
        self.mounts.iter().any(|mount| rest(path, &mount.at).is_some())
    }

    /// The bytes of the file at `path`, symlinks followed; none for no file.
    pub(crate) fn read(&self, path: &str) -> Result<Option<Bytes>, LowerError> {
        Ok(match self.node(path)? {
            Node::File(_, bytes) => Some(bytes),
            Node::Dir(_) | Node::None => None,
        })
    }

    /// What `path` is, symlinks followed.
    fn node(&self, path: &str) -> Result<Node, LowerError> {
        let mut path = path.to_owned();
        for _ in 0..=MAX_LINKS {
            let (root, under) = self.locate(&path)?;
            let file = self
                .tree
                .file(root, &under)
                .map_err(|corrupt| LowerError::at(format!("`{path}` can't be read: {corrupt}"), &path))?;
            match file {
                Some((_, Mode::Symlink, target)) => {
                    let target = std::str::from_utf8(&target).map_err(|_| {
                        LowerError::at(format!("the symlink `{path}` points to a path that is not UTF-8"), &path)
                    })?;
                    path = follow(&path, target);
                }
                Some((id, _, bytes)) => return Ok(Node::File(id, bytes)),
                None => {
                    return Ok(match self.tree.read_dir(root, &under, &mut |_| {}) {
                        true => Node::Dir(path),
                        false => Node::None,
                    });
                }
            }
        }
        Err(LowerError::at(format!("`{path}`: more than {MAX_LINKS} symlinks in a row"), &path))
    }

    /// Every file under the directory `dir`, by its path under `dir` as cargo names it, but
    /// those under a path of `skipped`. A symlink is read as what it points to; one that points
    /// to nothing is no file, as on the disk.
    pub(crate) fn walk(&self, dir: &str, skipped: &[String], out: &mut Vec<Loaded>) -> Result<(), LowerError> {
        // Each directory as named, where its entries are read from, and the symlinks followed
        // to reach it.
        let mut dirs: Vec<(String, String, u32)> = vec![(dir.to_owned(), dir.to_owned(), 0)];
        while let Some((named, real, links)) = dirs.pop() {
            let (root, under) = self.locate(&real)?;
            let Some(entries) = self.tree.list(root, &under) else {
                return Err(LowerError::at(format!("the directory `{real}` is not in its mount's files"), &real));
            };
            for (name, is_dir) in entries.into_iter().rev() {
                let child = format!("{named}/{name}");
                if skipped.iter().any(|it| *it == child) {
                    continue;
                }
                let child_real = format!("{real}/{name}");
                if is_dir {
                    dirs.push((child, child_real, links));
                    continue;
                }
                match self.node(&child_real)? {
                    Node::File(id, bytes) => out.push(Loaded { path: child, id, bytes }),
                    Node::Dir(target) if links < MAX_LINKS => dirs.push((child, target, links + 1)),
                    Node::Dir(_) => {
                        return Err(LowerError::at(
                            format!("`{child}`: more than {MAX_LINKS} symlinked directories in a row"),
                            &child,
                        ));
                    }
                    Node::None => {}
                }
            }
        }
        Ok(())
    }
}

/// What is after `at` in `path` ("" for `at` itself), when `path` is under the mount `at`.
pub(crate) fn rest<'p>(path: &'p str, at: &str) -> Option<&'p str> {
    let at = at.trim_end_matches('/');
    let after = path.strip_prefix(at)?;
    match after.strip_prefix('/') {
        Some(rest) => Some(rest),
        None if after.is_empty() => Some(""),
        None => None,
    }
}

/// The directory of `path`, absolute and `/`-separated.
pub(crate) fn parent(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some(("", _)) => "/",
        Some((parent, _)) => parent,
        None => path,
    }
}

/// Where the symlink at `link` pointing to `target` leads, `.` and `..` taken out.
fn follow(link: &str, target: &str) -> String {
    let target = target.replace('\\', "/");
    let absolute = target.starts_with('/') || target.as_bytes().get(1) == Some(&b':');
    let joined = if absolute { target } else { format!("{}/{target}", parent(link)) };
    normalize(&joined)
}

pub(crate) use blaze_units::load::normal_text as normalize;
