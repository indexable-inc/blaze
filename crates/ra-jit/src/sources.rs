//! The source files as the daemon holds them, and what tells it they changed
//! ([docs/roadmap.md](../../../docs/roadmap.md#1-the-vfs-is-the-source-of-truth)).
//!
//! - [`Sources`] is a blaze VFS of every watched source file: `head` is what the next build
//!   reads, `built` what the database was last given. Every input commits to `head`, an
//!   editor's or agent's transaction and the disk alike, so the files a build has to read are
//!   the diff of the two roots, and a file written with the text it already has is no change
//!   (an editor writes after it sent the text; the daemon reads its own writes back).
//! - [`Notices`] is the disk as one of those inputs: the operating system says which files
//!   and directories changed (FSEvents, inotify), so a build looks at those instead of taking
//!   the time of every watched file and listing every directory. When it can't say (no
//!   watcher, a queue that overflowed, `RA_JIT_POLL` set), it says everything may have, which
//!   is the look at every file it replaces.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use blaze_vfs::{Change, ChangeKind, Commit, CommitError, Hash, NodeId, Op, Vfs};
use notify::Watcher as _;
use rustc_hash::{FxHashMap, FxHashSet};

/// The workspace head every build reads.
const HEAD: &str = "workspace";

/// The commits after which nodes no root reaches are dropped.
const COLLECT_EVERY: u32 = 256;

/// What may have changed on disk since the last [`Notices::take`].
#[derive(Debug)]
pub(crate) enum Changed {
    /// Anything: every file is looked at.
    All,
    /// These files (as read on disk), and the directories with an entry added, removed or
    /// renamed.
    Some { files: FxHashSet<PathBuf>, dirs: FxHashSet<PathBuf> },
}

impl Changed {
    pub(crate) fn file(&self, on_disk: &Path) -> bool {
        match self {
            Changed::All => true,
            Changed::Some { files, .. } => files.contains(on_disk),
        }
    }

    pub(crate) fn dir(&self, on_disk: &Path) -> bool {
        match self {
            Changed::All => true,
            Changed::Some { dirs, .. } => dirs.contains(on_disk),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        matches!(self, Changed::Some { files, dirs } if files.is_empty() && dirs.is_empty())
    }

    fn add(&mut self, other: Changed) {
        match (self, other) {
            (Changed::All, _) => {}
            (this @ Changed::Some { .. }, Changed::All) => *this = Changed::All,
            (Changed::Some { files, dirs }, Changed::Some { files: more, dirs: in_more }) => {
                files.extend(more);
                dirs.extend(in_more);
            }
        }
    }
}

/// The directories the source files are in, watched by the operating system.
pub(crate) struct Notices {
    watcher: Option<notify::RecommendedWatcher>,
    events: mpsc::Receiver<notify::Result<notify::Event>>,
    /// Each watched directory as the events name it (resolved, as FSEvents reports
    /// `/private/var` for `/var`), and as the daemon reads it.
    watched: FxHashMap<PathBuf, PathBuf>,
    /// The watched directories as the daemon reads them.
    known: FxHashSet<PathBuf>,
    /// Gathered by [`Notices::wait`] and not taken yet.
    pending: Changed,
    /// Whether the watcher woke the daemon since the last [`Notices::take`].
    woke: Arc<AtomicBool>,
}

impl Notices {
    /// A watcher, unless `RA_JIT_POLL` asks for the look at every file instead or the system
    /// has none to give: then every [`Notices::take`] is [`Changed::All`]. `wake` is called
    /// when a change comes, once until the next [`Notices::take`].
    pub(crate) fn new(wake: impl Fn() + Send + 'static) -> Notices {
        let (sender, events) = mpsc::channel();
        let polled = std::env::var_os("RA_JIT_POLL").is_some_and(|it| it != "0");
        let woke = Arc::new(AtomicBool::new(false));
        let watcher = if polled {
            None
        } else {
            let woke = woke.clone();
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                // The daemon's own reads of the files are no change.
                let read =
                    event.as_ref().is_ok_and(|it| matches!(it.kind, notify::EventKind::Access(_)));
                let _ = sender.send(event);
                if !read && !woke.swap(true, Ordering::SeqCst) {
                    wake();
                }
            })
            .inspect_err(|error| {
                eprintln!("notices: no watcher ({error}); every file is looked at")
            })
            .ok()
        };
        // Nothing is known about the files before the first look.
        Notices {
            watcher,
            events,
            watched: FxHashMap::default(),
            known: FxHashSet::default(),
            pending: Changed::All,
            woke,
        }
    }

    /// Whether a change came that was not taken yet.
    pub(crate) fn pending(&mut self) -> bool {
        self.gather();
        self.watcher.is_some() && !self.pending.is_empty()
    }

    pub(crate) fn watching(&self) -> bool {
        self.watcher.is_some()
    }

    /// Watches each of `dirs` (as read on disk) not watched yet; one that can't be is looked
    /// at whole once, then not watched.
    pub(crate) fn watch(&mut self, dirs: impl IntoIterator<Item = PathBuf>) {
        let Some(watcher) = &mut self.watcher else { return };
        let new: Vec<(PathBuf, PathBuf)> = dirs
            .into_iter()
            .filter(|dir| self.known.insert(dir.clone()))
            .filter_map(|dir| Some((dunce::canonicalize(&dir).ok()?, dir)))
            .filter(|(resolved, _)| !self.watched.contains_key(resolved))
            .collect();
        if new.is_empty() {
            return;
        }
        // One commit for all: FSEvents starts its stream again for each change of the paths.
        let mut paths = watcher.paths_mut();
        let mut failed = false;
        for (resolved, _) in &new {
            failed |= paths.add(resolved, notify::RecursiveMode::NonRecursive).is_err();
        }
        failed |= paths.commit().is_err();
        if failed {
            self.pending = Changed::All;
        }
        self.watched.extend(new);
    }

    /// Files the daemon wrote or deleted itself, as read on disk: the next look has them and
    /// their directories without waiting for the system to tell.
    pub(crate) fn wrote(&mut self, files: impl IntoIterator<Item = PathBuf>) {
        let files: FxHashSet<PathBuf> = files.into_iter().collect();
        let dirs = files.iter().filter_map(|it| it.parent().map(Path::to_owned)).collect();
        self.pending.add(Changed::Some { files, dirs });
    }

    /// What changed since the last call.
    pub(crate) fn take(&mut self) -> Changed {
        self.woke.store(false, Ordering::SeqCst);
        self.gather();
        let empty = Changed::Some { files: FxHashSet::default(), dirs: FxHashSet::default() };
        let taken = std::mem::replace(&mut self.pending, empty);
        if self.watcher.is_none() { Changed::All } else { taken }
    }

    /// Waits up to `timeout` for a change: whether one came. Without a watcher it sleeps and
    /// says one may have.
    pub(crate) fn wait(&mut self, timeout: Duration) -> bool {
        self.gather();
        if self.watcher.is_none() {
            std::thread::sleep(timeout);
            return true;
        }
        if !self.pending.is_empty() {
            return true;
        }
        match self.events.recv_timeout(timeout) {
            Ok(event) => {
                self.note(event);
                self.gather();
                !self.pending.is_empty()
            }
            Err(_) => false,
        }
    }

    fn gather(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.note(event);
        }
    }

    fn note(&mut self, event: notify::Result<notify::Event>) {
        let event = match event {
            Ok(event) if !event.need_rescan() => event,
            // A queue that overflowed, or an error: what was missed is not known.
            _ => {
                self.pending = Changed::All;
                return;
            }
        };
        if matches!(event.kind, notify::EventKind::Access(_)) {
            return;
        }
        let entries = matches!(
            event.kind,
            notify::EventKind::Create(_)
                | notify::EventKind::Remove(_)
                | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
                | notify::EventKind::Any
                | notify::EventKind::Other
        );
        let mut changed = Changed::Some { files: FxHashSet::default(), dirs: FxHashSet::default() };
        let Changed::Some { files, dirs } = &mut changed else { unreachable!() };
        for path in &event.paths {
            if let Some(dir) = self.watched.get(path) {
                dirs.insert(dir.clone());
            }
            let Some(parent) = path.parent().and_then(|it| self.watched.get(it)) else { continue };
            let Some(name) = path.file_name() else { continue };
            files.insert(parent.join(name));
            if entries {
                dirs.insert(parent.clone());
            }
        }
        self.pending.add(changed);
    }
}

/// The path of `path` in the VFS: absolute, without its first `/`.
fn key(path: &Path) -> Option<&str> {
    let path = path.to_str()?.strip_prefix('/')?;
    blaze_core::FileName::new(path).map(|_| path)
}

/// Every watched source file, as built and as it is now.
pub(crate) struct Sources {
    vfs: Vfs,
    /// What the database was last given.
    built: NodeId,
    commits: u32,
}

impl Sources {
    pub(crate) fn new() -> Sources {
        let vfs = Vfs::new();
        let built = vfs.tree.empty();
        Sources { vfs, built, commits: 0 }
    }

    /// The root of what the next build reads.
    pub(crate) fn head(&self) -> Hash {
        self.vfs.tree.hash(self.vfs.head(HEAD))
    }

    /// Takes in files the database has and the VFS does not, as both built and current.
    pub(crate) fn hold<'a>(&mut self, files: impl IntoIterator<Item = (&'a Path, &'a str)>) {
        let head = self.vfs.head(HEAD);
        let ops: Vec<Op> = files
            .into_iter()
            .filter_map(|(path, text)| Some((key(path)?, text)))
            .filter(|(path, _)| self.vfs.tree.lookup(head, path).is_none())
            .map(|(path, text)| Op::Write {
                path: path.to_owned(),
                bytes: Arc::from(text.as_bytes()),
            })
            .collect();
        if ops.is_empty() {
            return;
        }
        let tree = &mut self.vfs.tree;
        match (tree.apply(head, &ops), tree.apply(self.built, &ops)) {
            (Ok(head), Ok(built)) => {
                self.vfs.set_head(HEAD, head);
                self.built = built;
            }
            (Err(error), _) | (_, Err(error)) => eprintln!("sources: not held: {error}"),
        }
    }

    /// The bytes of `path` the next build reads.
    pub(crate) fn read(&self, path: &Path) -> Option<Arc<[u8]>> {
        self.vfs.tree.read(self.vfs.head(HEAD), key(path)?).cloned()
    }

    /// Commits `ops` (paths absolute) made against `base`, or against the head when there is
    /// none.
    pub(crate) fn commit(&mut self, base: Option<Hash>, ops: &[Op]) -> Result<Commit, CommitError> {
        let base = base.unwrap_or_else(|| self.head());
        let ops: Vec<Op> = ops.iter().map(relative).collect::<Result<_, _>>()?;
        let commit = self.vfs.commit(HEAD, base, &ops)?;
        self.commits += 1;
        Ok(commit)
    }

    /// The disk says `path` holds `bytes`, or nothing: whether the VFS can hold the path (one
    /// that is not UTF-8 can't, and is read from disk).
    pub(crate) fn read_on_disk(&mut self, path: &Path, bytes: Option<Vec<u8>>) -> bool {
        let Some(at) = key(path) else { return false };
        let head = self.vfs.head(HEAD);
        let op = match bytes {
            Some(bytes) if self.vfs.tree.read(head, at).is_some_and(|it| **it == *bytes) => {
                return true;
            }
            Some(bytes) => Op::Write { path: at.to_owned(), bytes: bytes.into() },
            None if self.vfs.tree.lookup(head, at).is_none() => return true,
            None => Op::Delete { path: at.to_owned() },
        };
        let base = self.vfs.tree.hash(head);
        if let Err(error) = self.vfs.commit(HEAD, base, &[op]) {
            eprintln!("sources: {} not taken in: {error}", path.display());
        }
        self.commits += 1;
        true
    }

    /// The files the next build reads otherwise than the last, by absolute path: `None` for
    /// one gone.
    pub(crate) fn changed(&self) -> Vec<(PathBuf, Option<Arc<[u8]>>)> {
        let head = self.vfs.head(HEAD);
        self.vfs
            .tree
            .diff(self.built, head)
            .into_iter()
            .map(|Change { path, kind }| {
                let bytes = match kind {
                    ChangeKind::Removed => None,
                    ChangeKind::Added | ChangeKind::Modified => {
                        self.vfs.tree.read(head, &path).cloned()
                    }
                };
                (PathBuf::from(format!("/{path}")), bytes)
            })
            .collect()
    }

    /// The database has been given the head.
    pub(crate) fn built_head(&mut self) {
        self.built = self.vfs.head(HEAD);
        if self.commits >= COLLECT_EVERY {
            self.commits = 0;
            let mut keep = vec![self.built];
            self.vfs.collect(&mut keep);
            self.built = keep[0];
        }
    }
}

fn relative(op: &Op) -> Result<Op, CommitError> {
    let at = |path: &String| {
        key(Path::new(path))
            .map(str::to_owned)
            .ok_or_else(|| CommitError::Op(blaze_vfs::OpError::BadPath(path.clone())))
    };
    Ok(match op {
        Op::Write { path, bytes } => Op::Write { path: at(path)?, bytes: bytes.clone() },
        Op::Edit { path, at: from, len, bytes } => {
            Op::Edit { path: at(path)?, at: *from, len: *len, bytes: bytes.clone() }
        }
        Op::Delete { path } => Op::Delete { path: at(path)? },
        Op::Rename { from, to } => Op::Rename { from: at(from)?, to: at(to)? },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_of_the_same_text_is_no_change() {
        let mut sources = Sources::new();
        let lib = Path::new("/w/src/lib.rs");
        sources.hold([(lib, "fn a() {}")]);
        assert!(sources.changed().is_empty());
        sources.read_on_disk(lib, Some(b"fn a() {}".to_vec()));
        assert!(sources.changed().is_empty());

        // An editor's text, then its write of that text.
        let write = Op::Write { path: "/w/src/lib.rs".into(), bytes: Arc::from(&b"fn b() {}"[..]) };
        sources.commit(None, &[write]).unwrap();
        sources.read_on_disk(lib, Some(b"fn b() {}".to_vec()));
        assert_eq!(sources.changed(), vec![(lib.to_owned(), Some(Arc::from(&b"fn b() {}"[..])))]);
        sources.built_head();
        assert!(sources.changed().is_empty());

        sources.read_on_disk(lib, None);
        assert_eq!(sources.changed(), vec![(lib.to_owned(), None)]);
    }

    #[test]
    fn a_transaction_against_an_old_root_rebases() {
        let mut sources = Sources::new();
        sources.hold([(Path::new("/w/a.rs"), "0123456789"), (Path::new("/w/b.rs"), "b")]);
        let base = sources.head();
        sources.read_on_disk(Path::new("/w/a.rs"), Some(b"AA23456789".to_vec()));
        let edit = Op::Edit { path: "/w/a.rs".into(), at: 8, len: 2, bytes: b"zz".to_vec() };
        let commit = sources.commit(Some(base), &[edit]).unwrap();
        assert!(commit.rebased);
        assert_eq!(&*sources.read(Path::new("/w/a.rs")).unwrap(), b"AA234567zz");
    }

    #[test]
    fn notices_name_the_changed_file_and_its_directory() {
        let dir = std::env::temp_dir().join(format!("ra-jit-notices-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut notices = Notices::new(|| {});
        if !notices.watching() {
            return;
        }
        notices.watch([dir.clone()]);
        let _ = notices.take();
        std::fs::write(dir.join("a.rs"), "fn a() {}").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut changed = Changed::Some { files: FxHashSet::default(), dirs: FxHashSet::default() };
        while !changed.file(&dir.join("a.rs")) && std::time::Instant::now() < deadline {
            notices.wait(Duration::from_millis(50));
            changed.add(notices.take());
        }
        assert!(changed.file(&dir.join("a.rs")), "{changed:?}");
        assert!(changed.dir(&dir), "{changed:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
