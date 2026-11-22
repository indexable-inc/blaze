//! Linear group-commit file transactions ([`Txn`]) over a [`Dir<ReadWrite>`]: stage multiple
//! publishes, renames and removals unsynced, then commit the batch with a single flush pass and
//! ONE directory sync per affected parent directory (`commit_synced` / `commit_barrier`), or
//! commit unsynced (`commit_unsynced`), or discard staged temps (`abort`).

use alloc::vec::Vec;

use blaze_core::{DirName, Done, Effect, FileName};

use super::{
    Content, Dir, FsError, FsOp, How, ReadWrite, Unsynced, Written, call, remove, rename_unsynced,
    started, sync_dir, unexpected,
};
use crate::{cx::Cx, disk::dir_of, path::Path, unfinished::Unfinished};

/// What a committed [`Txn`] applied.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Committed {
    /// Files published via temp + rename.
    pub published: usize,
    /// Existing paths renamed.
    pub renamed: usize,
    /// Paths removed.
    pub removed: usize,
    /// Unique parent directories synced (0 for `commit_unsynced`).
    pub dirs_synced: usize,
}

enum Step {
    Publish { temp: FileName, target: FileName },
    Rename { from: FileName, to: FileName },
    Remove { target: FileName },
}

/// A linear batch of file changes beneath a [`Dir<ReadWrite>`]: finished with
/// [`Txn::commit_synced`], [`Txn::commit_barrier`], [`Txn::commit_unsynced`] or [`Txn::abort`].
#[blaze::linear]
#[must_use = "a file transaction is finished with commit_synced(cx), commit_barrier(cx), commit_unsynced(cx, unsynced) or abort(cx)"]
pub struct Txn<'dir> {
    dir: &'dir Dir<ReadWrite>,
    steps: Vec<Step>,
    unfinished: Unfinished,
}

impl Dir<ReadWrite> {
    /// Begins a linear group-commit transaction beneath this directory: stage multiple publishes,
    /// renames and removals and commit them with one directory sync per affected folder.
    #[track_caller]
    pub fn txn(&self) -> Txn<'_> {
        Txn {
            dir: self,
            // alloc: none until the first staged op.
            steps: Vec::new(),
            unfinished: Unfinished::new("fs::Txn", "commit_synced(cx), commit_barrier(cx), commit_unsynced(cx, unsynced) or abort(cx)"),
        }
    }
}

impl<'dir> Txn<'dir> {
    /// Stages `path` to hold `content`: writes the temporary file unsynced now; the target path
    /// is not replaced until commit. Calling `publish` twice for the same `path` replaces the
    /// earlier staged temp.
    pub fn publish(&mut self, cx: &Cx, path: &Path, content: Content<'_>) -> Result<How, FsError> {
        let target = self.dir.beneath(path);
        let (temp, how) = self.dir.place(cx, &target, content)?;
        cx.event(how.says());
        if let Some(existing) = self.steps.iter_mut().find_map(|step| match step {
            Step::Publish { temp: old, target: t } if *t == target => Some(old),
            _ => None,
        }) {
            let replaced = core::mem::replace(existing, temp);
            let _ = remove(&self.dir.io, cx, FsOp::Remove, &replaced);
        } else {
            // alloc: one staged step in this transaction.
            self.steps.push(Step::Publish { temp, target });
        }
        Ok(how)
    }

    /// Stages renaming `from` over `to` (both beneath this directory) at commit time.
    pub fn rename_replacing(&mut self, from: &Path, to: &Path) {
        let (from, to) = (self.dir.beneath(from), self.dir.beneath(to));
        // alloc: one staged step in this transaction.
        self.steps.push(Step::Rename { from, to });
    }

    /// Stages removing `path` beneath this directory at commit time.
    pub fn remove(&mut self, path: &Path) {
        let target = self.dir.beneath(path);
        // alloc: one staged step in this transaction.
        self.steps.push(Step::Remove { target });
    }

    /// How many operations are staged in this transaction.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Whether no operations have been staged yet.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Commits all staged operations durably: flushes every staged temp (`Effect::Sync`) before
    /// renaming any target, executes all renames and removals, then syncs each unique affected
    /// parent directory ONCE (`Effect::SyncDir`).
    #[blaze::consumes]
    pub fn commit_synced(self, cx: &Cx) -> Result<Committed, FsError> {
        self.commit_durable(cx, false)
    }

    /// Commits all staged operations with an ordering barrier (`Effect::Barrier`: `F_BARRIERFSYNC`
    /// on macOS, `fdatasync` on Linux) per staged temp before renaming, then syncs each unique
    /// affected parent directory ONCE (`Effect::SyncDir`).
    #[blaze::consumes]
    pub fn commit_barrier(self, cx: &Cx) -> Result<Committed, FsError> {
        self.commit_durable(cx, true)
    }

    /// Commits all staged operations without waiting for the device.
    #[blaze::consumes]
    pub fn commit_unsynced(self, cx: &Cx, unsynced: Unsynced) -> Result<Committed, FsError> {
        let Txn { dir, steps, unfinished } = self;
        unfinished.finished();
        let mut out = Committed::default();
        for step in steps {
            match step {
                Step::Publish { temp, target } => {
                    rename_unsynced(&dir.io, cx, FsOp::Publish, &temp, &target, unsynced)?;
                    started(&dir.io, &target, unsynced);
                    out.published += 1;
                }
                Step::Rename { from, to } => {
                    rename_unsynced(&dir.io, cx, FsOp::Rename, &from, &to, unsynced)?;
                    started(&dir.io, &to, unsynced);
                    out.renamed += 1;
                }
                Step::Remove { target } => {
                    remove(&dir.io, cx, FsOp::Remove, &target)?;
                    out.removed += 1;
                }
            }
        }
        Ok(out)
    }

    /// Aborts the transaction, removing any staged temporary files without altering any target.
    #[blaze::consumes]
    pub fn abort(self, cx: &Cx) -> Result<(), FsError> {
        let Txn { dir, steps, unfinished } = self;
        unfinished.finished();
        for step in steps {
            if let Step::Publish { temp, .. } = step {
                remove(&dir.io, cx, FsOp::Remove, &temp)?;
            }
        }
        Ok(())
    }

    fn commit_durable(self, cx: &Cx, barrier: bool) -> Result<Committed, FsError> {
        let Txn { dir, steps, unfinished } = self;
        unfinished.finished();
        // 1. Flush every staged temp BEFORE renaming any target so no torn content can become visible.
        for step in &steps {
            if let Step::Publish { temp, .. } = step {
                if barrier {
                    // clone: the effect's name, a refcount.
                    let file = temp.clone();
                    match call(&dir.io, cx, FsOp::Flush, temp, |tag| Effect::Barrier { file, tag })? {
                        Done::Ordered => {}
                        done => return Err(unexpected(FsOp::Flush, temp, done)),
                    }
                } else {
                    // clone: the temp's name, a refcount.
                    let _flushed = Written::of(&dir.io, temp.clone()).flush(cx)?;
                }
            }
        }
        // 2. Apply all renames and removals unsynced and collect unique parent directories in `cx.frame()`.
        let mut dirs: Vec<DirName, &blaze_mem::Frame> = Vec::new_in(cx.frame());
        let mut track_dir = |name: &FileName| {
            let dir_str = dir_of(name);
            let parent = if dir_str.is_empty() {
                DirName::Root
            } else {
                DirName::Named(FileName::new(dir_str).expect("a file's parent directory is a valid name"))
            };
            if !dirs.contains(&parent) {
                dirs.push(parent);
            }
        };
        let mut out = Committed::default();
        for step in steps {
            match step {
                Step::Publish { temp, target } => {
                    rename_unsynced(&dir.io, cx, FsOp::Publish, &temp, &target, Unsynced::No)?;
                    track_dir(&target);
                    out.published += 1;
                }
                Step::Rename { from, to } => {
                    rename_unsynced(&dir.io, cx, FsOp::Rename, &from, &to, Unsynced::No)?;
                    track_dir(&from);
                    track_dir(&to);
                    out.renamed += 1;
                }
                Step::Remove { target } => {
                    remove(&dir.io, cx, FsOp::Remove, &target)?;
                    track_dir(&target);
                    out.removed += 1;
                }
            }
        }
        // 3. Sync each unique affected parent directory ONCE.
        for parent in &dirs {
            let dir_str = parent.name().map_or("", FileName::as_str);
            sync_dir(&dir.io, cx, FsOp::SyncNames, parent, dir_str)?;
            out.dirs_synced += 1;
        }
        Ok(out)
    }
}
