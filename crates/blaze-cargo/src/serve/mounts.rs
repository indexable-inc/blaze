//! Where each fetched source and the standard library are mounted (docs/v2/plan.md
//! section 3): `at -> root`, kept durable in the VFS's journal as one head per mount, so a
//! restart recovers every source fetched before and fetches none of them again. A path read
//! where it is on the disk ([`LocalRead`]) is mounted the same way but kept in memory only:
//! what is under it may change, so a restart reads it again rather than trusting a head.

#![forbid(unsafe_code)]

use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_units::query::{Mount, Origin};
use blaze_vfs::{CommitError, Mode, Op, OpError, Ticket, Vfs};
use std::alloc::Global;

use crate::serve::{
    fetch::on_disk_id,
    work::{LocalLeaf, LocalRead, Source},
};

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
    by_at: HashMap<String, Mount, Global>,
    /// Each path read where it is on the disk, by `at`: its mount, and the leaves it was read
    /// from with their stamps ([`LocalRead::leaves`]) and the instant of those looks. Never in
    /// the journal.
    locals: HashMap<String, Local, Global>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Local {
    mount: Mount,
    leaves: Option<std::sync::Arc<[LocalLeaf]>>,
    looked_ns: u64,
}

/// One local mount as a [`Sweep`] holds it: its place, its leaves (none: it is read again
/// whatever the stamps say) and the instant they were looked at.
#[derive(Debug)]
struct SweptLocal {
    at: String,
    leaves: Option<std::sync::Arc<[LocalLeaf]>>,
    looked_ns: u64,
}

/// The local mounts' leaves asked of the driver in one [`blaze_core::Effect::Stamp`]
/// ([`Mounts::sweep`]), and the answer's check ([`Sweep::stale`]).
///
/// The floor of the sweep: one `lstat` per leaf, nothing read, off the loop; it costs that
/// (ratio 1.0). On the loop it costs building the names (one copy of each leaf's path) and
/// one comparison per stamp.
#[derive(Debug)]
pub struct Sweep {
    names: std::sync::Arc<blaze_core::Names>,
    /// Sorted by place; the names are the leaves of those with leaves, in this order.
    locals: Vec<SweptLocal>,
}

/// The local mounts a [`Sweep`] found must be read again before a job reads them, sorted by
/// place ([`Sweep::stale`]): each with no leaves, and each with a leaf whose stamp no longer
/// holds. Empty (its `Default`): none is.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stale {
    /// Where each is mounted.
    pub ats: Box<[String]>,
}

/// A sweep's answer refused ([`Sweep::stale`]): the driver answered `answered` stamps for the
/// `asked` leaves named, so no stamp can be matched to its leaf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepError {
    pub asked: usize,
    pub answered: usize,
}

impl std::fmt::Display for SweepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the sweep of the paths outside the view asked {} stamps of the driver and was answered {}",
            self.asked, self.answered
        )
    }
}

impl Sweep {
    /// The effect that asks the driver for the stamps of the sweep's names, tagged `tag`; the
    /// effect and the sweep share the names (a refcount).
    pub fn effect(&self, tag: blaze_core::Tag) -> blaze_core::Effect {
        blaze_core::Effect::Stamp { files: std::sync::Arc::clone(&self.names), tag }
    }

    /// How many leaves are asked.
    pub fn leaves(&self) -> usize {
        self.names.len()
    }

    /// Where each local mount is that must be read again before a job reads it, sorted by
    /// place, by `stamps`, the driver's answer for [`Sweep::effect`]'s names in order: one with
    /// no leaves, and one any of whose leaves' stamp no longer holds
    /// ([`blaze_vfs::outside::holds`]). An answer of another length is refused, naming both
    /// lengths. A span with the counts: locals, leaves, stale.
    pub fn stale(&self, stamps: &[blaze_core::Stamp]) -> Result<Stale, SweepError> {
        let span = tracing::info_span!(
            target: "blaze",
            "sweep the stamps of the paths outside the view mounted where they are",
            locals = self.locals.len(),
            leaves = self.names.len(),
            stale = tracing::field::Empty,
        )
        .entered();
        if stamps.len() != self.names.len() {
            return Err(SweepError { asked: self.names.len(), answered: stamps.len() });
        }
        let mut stale = Vec::new_in(Global);
        let mut at = 0;
        for local in &self.locals {
            let held = match &local.leaves {
                None => false,
                Some(leaves) => {
                    let now = &stamps[at..at + leaves.len()];
                    at += leaves.len();
                    leaves.iter().zip(now).all(|(leaf, now)| blaze_vfs::outside::holds(&leaf.stamp, local.looked_ns, now))
                }
            };
            if !held {
                stale.push(local.at.clone()); // clone: once per local read again
            }
        }
        span.record("stale", stale.len());
        Ok(Stale { ats: stale.into_boxed_slice() })
    }
}

impl Mounts {
    /// The mounts a VFS recovered from its journal kept.
    pub fn recover(vfs: &Vfs) -> Mounts {
        let mut by_at = HashMap::new_in(Global);
        for (head, root) in vfs.heads() {
            if let Some((at, under)) = parse(head) {
                by_at.insert(at.clone(), Mount { at, origin: Origin::Root(root), under });
            }
        }
        Mounts { by_at, locals: HashMap::new_in(Global) }
    }

    /// Each recovered package mount under `sources` (a registry package, a git checkout) whose
    /// copy on disk is not the mount's files, forgotten, its head removed: a build script runs
    /// in its package's directory, which [`crate::serve::fetch::keep_on_disk`] wrote at its
    /// fetch with an [`ON_DISK_MARK`](crate::serve::fetch::ON_DISK_MARK) holding its
    /// [`on_disk_id`]. `mark` reads the mark of a directory (`Ok(None)` for none, or no
    /// directory). A package whose copy was deleted is fetched and written again rather than
    /// handed to a sandbox as a directory that is not there; one whose mark holds another id is
    /// fetched again too, and the write then refuses the copy by name, never reads it as the
    /// mount. Returns where each was mounted, with why.
    pub fn forget_unwritten(
        &mut self,
        vfs: &mut Vfs,
        sources: &str,
        mark: impl Fn(&str) -> Result<Option<ContentId>, String>,
    ) -> Vec<(String, String)> {
        let inside = format!("{}/", sources.trim_end_matches('/'));
        let reader = vfs.reader();
        let mut gone: Vec<(String, String)> = Vec::new(); // alloc: once per start, the packages forgotten
        for mount in self.by_at.values() {
            let Origin::Root(root) = mount.origin else { continue };
            if !mount.under.is_empty() || !mount.at.starts_with(&inside) {
                continue;
            }
            let Some(want) = tree_on_disk_id(&reader, root) else {
                gone.push((
                    mount.at.clone(),
                    format!("its mount's root {root} is not in the tree"),
                )); // clone: once per package forgotten
                continue;
            };
            let why = match mark(&mount.at) {
                Ok(Some(it)) if it == want => continue,
                Ok(Some(it)) => format!("its copy on disk holds {it:x}, its mount {want:x}"),
                Ok(None) => "it has no copy on disk".to_owned(),
                Err(why) => why,
            };
            gone.push((mount.at.clone(), why)); // clone: once per package forgotten
        }
        gone.sort_unstable();
        for (at, _) in &gone {
            if let Some(mount) = self.by_at.remove(at) {
                _ = vfs.remove_head(&head_of(&mount));
            }
        }
        gone
    }

    pub fn get(&self, at: &str) -> Option<&Mount> {
        self.by_at.get(at).or_else(|| self.locals.get(at).map(|it| &it.mount))
    }

    pub fn has(&self, at: &str) -> bool {
        self.by_at.contains_key(at) || self.locals.contains_key(at)
    }

    /// Every mount, in no order: sort by `at` where the order shows.
    pub fn all(&self) -> impl Iterator<Item = &Mount> {
        self.by_at.values().chain(self.locals.values().map(|it| &it.mount))
    }

    /// `read` put into the VFS as a root of its own and mounted where it was read, in memory
    /// only. A local mounted there before is replaced in place, so a job that opened its view
    /// before reads the old root and one after the new, and none sees the path unmounted.
    pub fn mount_local(&mut self, vfs: &mut Vfs, read: LocalRead) -> Result<Mount, String> {
        let LocalRead { at, source: Source { files, under }, leaves, looked_ns } = read;
        if self.by_at.contains_key(&at) {
            return Err(format!("`{at}` is read where it is on the disk, but a fetched source is mounted there"));
        }
        let leaves = leaves.map(std::sync::Arc::from);
        let files = files.into_iter().map(|file| (file.path, file.bytes, file.mode));
        let root = vfs
            .apply_files(vfs.empty(), files)
            .map_err(|error| format!("the files read at {at} don't make a tree: {}", op_error(&error)))?;
        let mount = Mount { at: at.clone(), origin: Origin::Root(root), under }; // clone: once per path read
        self.locals.insert(at, Local { mount: mount.clone(), leaves, looked_ns }); // clone: as above
        Ok(mount)
    }

    /// The sweep of every local mount's leaves as they are now: the names to ask the driver's
    /// [`blaze_core::Effect::Stamp`] for (one `lstat` each, off the loop, as the stampers do
    /// it), and what each answer is checked against ([`Sweep::stale`]). `None` where no path
    /// outside the view is mounted: nothing to ask. A local with no leaves (named through a
    /// link), or with a leaf the driver cannot name (the root `/`, a name with a NUL), asks
    /// nothing and is read again by every job, as one whose leaf moved.
    ///
    /// The snapshot is of the mounts as the sweep is asked: a local mounted again before the
    /// answer comes is judged by the leaves it had, so the job reads it again at most once
    /// more than it needed; one mounted first after the ask is not in the sweep, and was read
    /// after it.
    pub fn sweep(&self) -> Option<Sweep> {
        if self.locals.is_empty() {
            return None;
        }
        let mut locals: Vec<SweptLocal> = Vec::with_capacity_in(self.locals.len(), Global);
        let (mut count, mut bytes) = (0usize, 0usize);
        for (at, local) in &self.locals {
            let leaves = local.leaves.as_ref().filter(|leaves| {
                leaves.iter().all(|leaf| leaf.path.len() > 1 && leaf.path.starts_with('/') && !leaf.path.contains('\0'))
            });
            if let Some(leaves) = leaves {
                count += leaves.len();
                bytes += leaves.iter().map(|it| it.path.len()).sum::<usize>();
            }
            // clone: the place and a refcount, once per local a sweep
            locals.push(SweptLocal { at: at.clone(), leaves: leaves.cloned(), looked_ns: local.looked_ns });
        }
        locals.sort_unstable_by(|a, b| a.at.cmp(&b.at));
        let mut names = blaze_core::Names::with_capacity(count, bytes);
        for local in &mut locals {
            let Some(leaves) = &local.leaves else { continue };
            // The driver's root is `/`: a leaf's path less its leading `/` is its name there.
            let named = leaves.iter().all(|leaf| names.push_str(&leaf.path[1..]));
            if !named {
                // A path `FileName` refuses (a `.` or `..` part, an empty part): the local asks
                // nothing and is read again. Its names pushed so far are dropped with the rest.
                local.leaves = None;
            }
        }
        // Pushed again where a local was dropped midway, so the names line up with the leaves.
        #[cfg(test)]
        let planted = tests::KEEPS_DROPPED_NAMES.get();
        #[cfg(not(test))]
        let planted = false;
        if !planted && locals.iter().any(|it| it.leaves.is_none()) {
            names = blaze_core::Names::with_capacity(count, bytes);
            for leaves in locals.iter().filter_map(|it| it.leaves.as_ref()) {
                for leaf in leaves.iter() {
                    names.push_checked(&leaf.path[1..]);
                }
            }
        }
        Some(Sweep { names: std::sync::Arc::new(names), locals })
    }

    /// Where any path outside the view is mounted where it is on the disk, the first by place.
    pub fn first_local(&self) -> Option<&str> {
        self.locals.keys().map(String::as_str).min()
    }

    /// The roots the mounts hold, sorted by `at`, for a collection to keep: the collection
    /// renumbers the tree in the order it is given.
    pub fn roots(&self) -> Vec<ContentId> {
        let mut held: Vec<(&str, ContentId)> = self
            .all()
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
    pub fn mount(
        &mut self,
        vfs: &mut Vfs,
        at: &str,
        source: Source,
    ) -> Result<(Mount, Ticket), String> {
        let Source { files, under } = source;
        // Each file's ID was made where its bytes were read (or listed): none is hashed again.
        let files = files.into_iter().map(|file| (file.path, file.id, file.bytes, file.mode));
        let root = vfs.apply_known_files(vfs.empty(), files).map_err(|error| {
            format!("the files of the source for {at} don't make a tree: {}", op_error(&error))
        })?;
        let mount = Mount { at: at.to_owned(), origin: Origin::Root(root), under };
        let ticket = vfs.set_head(&head_of(&mount), root).map_err(|error| commit_error(at, &error))?;
        self.locals.remove(at);
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
        let Origin::Root(root) = mount.origin else {
            unreachable!("a source is mounted as a root of its own")
        };
        let head = format!("{LIBRARY}{fingerprint}\0{at}");
        let ticket = vfs.set_head(&head, root).map_err(|error| commit_error(at, &error))?;
        Ok((mount, ticket))
    }

    /// Each standard library read before and mounted as it was read: its fingerprint, and
    /// where it is mounted.
    pub fn libraries(&self, vfs: &Vfs) -> Vec<(String, String)> {
        let mut kept = Vec::new();
        for (head, root) in vfs.heads() {
            let Some((fingerprint, at)) =
                head.strip_prefix(LIBRARY).and_then(|it| it.split_once('\0'))
            else {
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

/// [`on_disk_id`] of the files under `root`, read from the tree without hashing their bytes
/// again (a file's content id is the tree's); `None` when the tree has no such root.
fn tree_on_disk_id(reader: &blaze_vfs::Reader, root: ContentId) -> Option<ContentId> {
    let pinned = reader.pin();
    let mut files: Vec<(String, Mode, ContentId)> = Vec::new(); // alloc: once per package checked at a start
    let mut dirs = vec![String::new()]; // alloc: as above
    while let Some(dir) = dirs.pop() {
        // alloc: as above, the names of one directory, owned past the next list
        let names: Vec<(String, bool)> =
            pinned.list(root, &dir)?.map(|(name, is_dir)| (name.to_owned(), is_dir)).collect();
        for (name, is_dir) in names {
            let path = match dir.is_empty() {
                true => name,
                false => format!("{dir}/{name}"),
            };
            match is_dir {
                true => dirs.push(path),
                false => {
                    let (id, mode, _) = pinned.held(root, &path).ok()??;
                    files.push((path, mode, id));
                }
            }
        }
    }
    Some(on_disk_id(files.iter().map(|(path, mode, id)| (path.as_str(), *mode, *id))))
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
                .map(|(path, bytes)| {
                    SourceFile::of(
                        (*path).to_owned(),
                        blaze_vfs::Bytes::copy_kept(blaze_mem::Tag::Vfs, bytes),
                        Mode::Regular,
                    )
                })
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
        let (mount, _) =
            mounts.mount(&mut vfs, at, source(&[("Cargo.toml", b"[package]\n")], "")).unwrap();
        let index =
            "/home/u/.cargo/registry/index/index.crates.io-1949cf8c6b5b557f/.cache/se/rd/serde";
        mounts.mount(&mut vfs, index, source(&[("serde", b"{}\n")], "serde")).unwrap();
        let again = Mounts::recover(&vfs);
        assert_eq!(again, mounts);
        assert_eq!(again.get(at), Some(&mount));
        assert_eq!(again.get(index).unwrap().under, "serde");
        assert_eq!(again.roots().len(), 2);
    }

    /// A recovered package mount is kept only while its copy on disk holds the mount's own
    /// files: the id the fetch writes into the mark ([`crate::serve::fetch::source_on_disk_id`],
    /// from the fetched bytes) is the id the start reads off the tree, and a copy with no mark
    /// or another id is fetched again, never read as the mount.
    #[test]
    fn a_package_mount_is_kept_only_while_its_copy_on_disk_holds_its_files() {
        let (mut vfs, _) = Vfs::recover(FileName::new("journal").unwrap(), 0, &[]);
        let mut mounts = Mounts::default();
        let mut fetched = source(
            &[
                ("Cargo.toml", b"[package]\n"),
                ("src/lib.rs", b"pub fn f() {}\n"),
                ("src-a.c", b"int a;\n"),
            ],
            "",
        );
        fetched.files[2].mode = Mode::Executable;
        let id = crate::serve::fetch::source_on_disk_id(&fetched);
        let (kept, edited, deleted) = (
            "/s/registry/src/x/a-1.0.0",
            "/s/git/checkouts/b-0123/0123abcd",
            "/s/registry/src/x/c-1.0.0",
        );
        for at in [kept, edited, deleted] {
            mounts.mount(&mut vfs, at, fetched.clone()).unwrap();
        }
        mounts.mount(&mut vfs, "/elsewhere/d", fetched.clone()).unwrap();
        let other =
            crate::serve::fetch::source_on_disk_id(&source(&[("Cargo.toml", b"[package]\n")], ""));
        let gone = mounts.forget_unwritten(&mut vfs, "/s", |at| match at {
            _ if at == kept => Ok(Some(id)),
            _ if at == edited => Ok(Some(other)),
            _ => Ok(None),
        });
        let gone: Vec<&str> = gone.iter().map(|(at, _)| at.as_str()).collect();
        assert_eq!(gone, [edited, deleted]);
        assert!(mounts.has(kept) && mounts.has("/elsewhere/d"));
        assert_eq!(Mounts::recover(&vfs), mounts, "the forgotten heads are gone from the journal");
    }

    #[test]
    fn a_library_read_is_kept_while_its_mount_is_as_it_was_read() {
        let (mut vfs, _) = Vfs::recover(FileName::new("journal").unwrap(), 0, &[]);
        let mut mounts = Mounts::default();
        let at = "/rustc/0123abcd/library";
        mounts
            .mount_library(&mut vfs, at, "f1", source(&[("core/src/lib.rs", b"#![no_core]\n")], ""))
            .unwrap();
        assert_eq!(mounts.libraries(&vfs), vec![("f1".to_owned(), at.to_owned())]);
        assert_eq!(
            Mounts::recover(&vfs).libraries(&vfs),
            mounts.libraries(&vfs),
            "kept across a restart"
        );
        mounts
            .mount(&mut vfs, at, source(&[("core/src/lib.rs", b"#![no_core] // edited\n")], ""))
            .unwrap();
        assert!(mounts.libraries(&vfs).is_empty(), "the mount is no longer what that library read");
    }

    std::thread_local! {
        /// The planted bug of the sweep's control: the names of a local dropped midway (a leaf
        /// the driver cannot name) are left in the batch, so the stamps after it are checked
        /// against the wrong leaves.
        pub(super) static KEEPS_DROPPED_NAMES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    const CHANGED: u64 = 1_000_000_007;

    fn leaf(path: &str, ino: u64) -> LocalLeaf {
        let stamp = blaze_core::Stamp { ino, size: 1, mtime_ns: CHANGED, ctime_ns: CHANGED, mode: 0o100644 };
        LocalLeaf { path: path.to_owned(), stamp }
    }

    /// Three locals: `/a` with two leaves, `/b` with a leaf the driver cannot name after one
    /// it can (so `/b` asks nothing), `/c` with one leaf, looked at long after every change.
    fn three(vfs: &mut Vfs) -> Mounts {
        let mut mounts = Mounts::default();
        let looked_ns = CHANGED + 10 * blaze_vfs::COARSE_NS;
        for (at, leaves) in [
            ("/a", vec![leaf("/a", 1), leaf("/a/lib.rs", 2)]),
            ("/b", vec![leaf("/b", 3), leaf("/b/../x", 4)]),
            ("/c", vec![leaf("/c", 5)]),
        ] {
            let read = LocalRead {
                at: at.to_owned(),
                source: source(&[("lib.rs", at.as_bytes())], ""),
                leaves: Some(leaves.into_boxed_slice()),
                looked_ns,
            };
            mounts.mount_local(vfs, read).unwrap();
        }
        mounts
    }

    /// The oracle: the sweep asks exactly the nameable locals' leaves, in place order, and
    /// names stale exactly the local with a leaf the driver cannot name and the one whose
    /// leaf's stamp moved.
    fn sweep_wrong(mounts: &Mounts) -> Result<(), String> {
        let sweep = mounts.sweep().ok_or("no sweep of three locals")?;
        let asked: Vec<&str> = (0..sweep.names.len()).map(|at| sweep.names.get(at)).collect();
        if asked != ["a", "a/lib.rs", "c"] {
            return Err(format!("the sweep asked {asked:?}"));
        }
        let mut stamps: Vec<blaze_core::Stamp> = [1, 2, 5].map(|ino| leaf("/", ino).stamp).to_vec();
        let stale = |stamps: &[blaze_core::Stamp]| sweep.stale(stamps).map_err(|it| it.to_string());
        if *stale(&stamps)?.ats != ["/b"] {
            return Err("with nothing moved, other than `/b` stale".to_owned());
        }
        stamps[2].mtime_ns += 1;
        if *stale(&stamps)?.ats != ["/b", "/c"] {
            return Err("with `/c` moved, other than `/b` and `/c` stale".to_owned());
        }
        if sweep.stale(&stamps[..2]) != Err(SweepError { asked: 3, answered: 2 }) {
            return Err("an answer of another length was taken".to_owned());
        }
        Ok(())
    }

    /// The invariant: a sweep's names line up with its locals' leaves, whatever local was
    /// dropped from it, and its check names stale exactly the locals that are.
    #[test]
    fn a_sweep_asks_each_nameable_leaf_once_and_names_the_stale_locals() {
        let (mut vfs, _) = Vfs::recover(FileName::new("journal").unwrap(), 0, &[]);
        let mounts = three(&mut vfs);
        sweep_wrong(&mounts).unwrap();
        assert_eq!(mounts.first_local(), Some("/a"));
    }

    #[test]
    fn the_oracle_catches_a_sweep_that_keeps_a_dropped_locals_names() {
        let (mut vfs, _) = Vfs::recover(FileName::new("journal").unwrap(), 0, &[]);
        let mounts = three(&mut vfs);
        KEEPS_DROPPED_NAMES.set(true);
        let caught = sweep_wrong(&mounts);
        KEEPS_DROPPED_NAMES.set(false);
        assert!(caught.is_err_and(|why| why.starts_with("the sweep asked")));
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
            let (mount, _) =
                mounts.mount(&mut vfs, at, source(&[("src/lib.rs", body.as_bytes())], "")).unwrap();
            let Origin::Root(root) = mount.origin else { panic!("a mount of a source is a root") };
            want.push((*at, root));
        }
        want.sort_by(|a, b| a.0.cmp(b.0));
        let want: Vec<ContentId> = want.into_iter().map(|(_, root)| root).collect();
        assert_eq!(mounts.roots(), want);
        assert_eq!(Mounts::recover(&vfs).roots(), want);
    }
}
