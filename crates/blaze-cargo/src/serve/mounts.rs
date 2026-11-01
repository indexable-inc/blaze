//! Where each fetched source and the standard library are mounted (docs/v2/plan/lanes/cargo/notes.md
//! section 3): `at -> root`, kept durable in the VFS's journal as one head per mount, so a
//! restart recovers every source fetched before and fetches none of them again.

#![forbid(unsafe_code)]

use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_units::query::{Mount, Origin};
use blaze_vfs::{CommitError, Op, OpError, Ticket, Vfs};

use crate::serve::work::Source;

/// What every mount's head is named by: `mount:<at>`, a NUL, then what `at` is under the root.
/// A NUL is in no path.
const PREFIX: &str = "mount:";

fn head_of(mount: &Mount) -> String {
    format!("{PREFIX}{}\0{}", mount.at, mount.under)
}

/// What the standard library's read is kept as besides its mount: `library:<fingerprint>`, a
/// NUL, then where it is mounted, its root the mount's. While the mount there still has that
/// root, a library of that fingerprint is not read again.
const LIBRARY: &str = "library:";

fn parse(head: &str) -> Option<(String, String)> {
    let (at, under) = head.strip_prefix(PREFIX)?.split_once('\0')?;
    Some((at.to_owned(), under.to_owned()))
}

/// Every source mounted, by `at`.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct Mounts {
    by_at: HashMap<String, Mount>,
}

impl Mounts {
    /// The mounts a VFS recovered from its journal kept.
    pub fn recover(vfs: &Vfs) -> Mounts {
        let mut by_at = HashMap::default();
        for (head, root) in vfs.heads() {
            if let Some((at, under)) = parse(head) {
                by_at.insert(at.clone(), Mount { at, origin: Origin::Root(root), under });
            }
        }
        Mounts { by_at }
    }

    pub fn get(&self, at: &str) -> Option<&Mount> {
        self.by_at.get(at)
    }

    pub fn has(&self, at: &str) -> bool {
        self.by_at.contains_key(at)
    }

    /// Every mount, in no order: sort by `at` where the order shows.
    pub fn all(&self) -> impl Iterator<Item = &Mount> {
        self.by_at.values()
    }

    /// The roots the mounts hold, sorted by `at`, for a collection to keep: the collection
    /// renumbers the tree in the order it is given.
    pub fn roots(&self) -> Vec<ContentId> {
        let mut held: Vec<(&str, ContentId)> = self
            .by_at
            .values()
            .filter_map(|it| match it.origin {
                Origin::Root(root) => Some((it.at.as_str(), root)),
                Origin::View => None,
            })
            .collect();
        held.sort_unstable_by(|a, b| a.0.cmp(b.0));
        held.into_iter().map(|(_, root)| root).collect()
    }

    /// `source` put into the VFS as a root of its own and mounted at `at`, durably once the
    /// ticket is. A source mounted there before is replaced: a root is a function of its bytes,
    /// so a fetch of the same source gives the same root.
    pub fn mount(&mut self, vfs: &mut Vfs, at: &str, source: Source) -> Result<(Mount, Ticket), String> {
        let Source { files, under } = source;
        let ops: Vec<Op> =
            files.into_iter().map(|file| Op::Write { path: file.path, bytes: file.bytes, mode: file.mode }).collect();
        let root = vfs
            .apply_owned(vfs.empty(), ops)
            .map_err(|error| format!("the files of the source for {at} don't make a tree: {}", op_error(&error)))?;
        let mount = Mount { at: at.to_owned(), origin: Origin::Root(root), under };
        let ticket = vfs.set_head(&head_of(&mount), root).map_err(|error| commit_error(at, &error))?;
        if let Some(old) = self.by_at.insert(at.to_owned(), mount.clone())
            && old.under != mount.under
        {
            _ = vfs.remove_head(&head_of(&old));
        }
        Ok((mount, ticket))
    }

    /// The standard library of `fingerprint` read into `source`, mounted at `at` as
    /// [`Mounts::mount`] does and kept as that fingerprint's.
    pub fn mount_library(
        &mut self,
        vfs: &mut Vfs,
        at: &str,
        fingerprint: &str,
        source: Source,
    ) -> Result<(Mount, Ticket), String> {
        let (mount, _) = self.mount(vfs, at, source)?;
        let Origin::Root(root) = mount.origin else { unreachable!("a source is mounted as a root of its own") };
        let head = format!("{LIBRARY}{fingerprint}\0{at}");
        let ticket = vfs.set_head(&head, root).map_err(|error| commit_error(at, &error))?;
        Ok((mount, ticket))
    }

    /// Each standard library read before and mounted as it was read: its fingerprint, and
    /// where it is mounted.
    pub fn libraries(&self, vfs: &Vfs) -> Vec<(String, String)> {
        let mut kept = Vec::new();
        for (head, root) in vfs.heads() {
            let Some((fingerprint, at)) = head.strip_prefix(LIBRARY).and_then(|it| it.split_once('\0')) else {
                continue;
            };
            if self.get(at).is_some_and(|it| it.origin == Origin::Root(root)) {
                kept.push((fingerprint.to_owned(), at.to_owned()));
            }
        }
        kept
    }
}

fn op_error(error: &OpError) -> String {
    error.to_string()
}

fn commit_error(at: &str, error: &CommitError) -> String {
    format!("keeping the mount at {at}: {error}")
}

#[cfg(test)]
mod tests {
    use blaze_core::FileName;
    use blaze_vfs::Mode;

    use super::*;
    use crate::serve::work::SourceFile;

    fn source(files: &[(&str, &[u8])], under: &str) -> Source {
        Source {
            files: files
                .iter()
                .map(|(path, bytes)| SourceFile { path: (*path).to_owned(), bytes: bytes.to_vec(), mode: Mode::Regular })
                .collect(),
            under: under.to_owned(),
        }
    }

    #[test]
    fn a_mount_is_a_head_and_comes_back_from_the_heads() {
        let journal = FileName::new("journal").unwrap();
        let (mut vfs, _) = Vfs::recover(journal, 0, &[]);
        let mut mounts = Mounts::default();
        let at = "/home/u/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/serde-1.0.219";
        let (mount, _) = mounts.mount(&mut vfs, at, source(&[("Cargo.toml", b"[package]\n")], "")).unwrap();
        let index = "/home/u/.cargo/registry/index/index.crates.io-1949cf8c6b5b557f/.cache/se/rd/serde";
        mounts.mount(&mut vfs, index, source(&[("serde", b"{}\n")], "serde")).unwrap();
        let again = Mounts::recover(&vfs);
        assert_eq!(again, mounts);
        assert_eq!(again.get(at), Some(&mount));
        assert_eq!(again.get(index).unwrap().under, "serde");
        assert_eq!(again.roots().len(), 2);
    }

    #[test]
    fn a_library_read_is_kept_while_its_mount_is_as_it_was_read() {
        let (mut vfs, _) = Vfs::recover(FileName::new("journal").unwrap(), 0, &[]);
        let mut mounts = Mounts::default();
        let at = "/rustc/0123abcd/library";
        mounts.mount_library(&mut vfs, at, "f1", source(&[("core/src/lib.rs", b"#![no_core]\n")], "")).unwrap();
        assert_eq!(mounts.libraries(&vfs), vec![("f1".to_owned(), at.to_owned())]);
        assert_eq!(Mounts::recover(&vfs).libraries(&vfs), mounts.libraries(&vfs), "kept across a restart");
        mounts.mount(&mut vfs, at, source(&[("core/src/lib.rs", b"#![no_core] // edited\n")], "")).unwrap();
        assert!(mounts.libraries(&vfs).is_empty(), "the mount is no longer what that library read");
    }

    #[test]
    fn the_same_bytes_are_the_same_root() {
        let (mut vfs, _) = Vfs::recover(FileName::new("journal").unwrap(), 0, &[]);
        let mut mounts = Mounts::default();
        let files = || source(&[("src/lib.rs", b"pub fn f() {}\n")], "");
        let (one, _) = mounts.mount(&mut vfs, "/a", files()).unwrap();
        let (two, _) = mounts.mount(&mut vfs, "/b", files()).unwrap();
        assert_eq!(one.origin, two.origin);
    }

    #[test]
    fn the_roots_are_in_order_of_where_they_are_mounted() {
        let (mut vfs, _) = Vfs::recover(FileName::new("journal").unwrap(), 0, &[]);
        let mut mounts = Mounts::default();
        let ats = ["/m", "/a", "/z", "/c", "/b", "/y"];
        let mut want = Vec::new();
        for (n, at) in ats.iter().enumerate() {
            let body = format!("pub fn f{n}() {{}}\n");
            let (mount, _) = mounts.mount(&mut vfs, at, source(&[("src/lib.rs", body.as_bytes())], "")).unwrap();
            let Origin::Root(root) = mount.origin else { panic!("a mount of a source is a root") };
            want.push((*at, root));
        }
        want.sort_by(|a, b| a.0.cmp(b.0));
        let want: Vec<ContentId> = want.into_iter().map(|(_, root)| root).collect();
        assert_eq!(mounts.roots(), want);
        assert_eq!(Mounts::recover(&vfs).roots(), want);
    }
}
