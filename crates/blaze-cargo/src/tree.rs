//! Finding source files that were not there when the workspace was loaded, and losing the
//! ones that are gone.
//!
//! A directory's timestamp changes when a file is added to it or taken out of it, so one
//! `stat` per directory tells whether any has to be listed again.

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use ide_db::{
    RootDatabase,
    base_db::{SourceDatabase, SourceRoot, SourceRootId, salsa::Durability},
};
use rustc_hash::{FxHashMap, FxHashSet};
use triomphe::Arc;
use vfs::{AbsPathBuf, FileId, Vfs, VfsPath, file_set::FileSet};

/// The directories of the source files that can change.
pub struct Tree {
    vfs: Vfs,
    directories: FxHashMap<PathBuf, Directory>,
    /// The files an `include!` named that could not be read, with their timestamp then (`None`
    /// for one that was not there, as under an `OUT_DIR` no build script wrote): each is read
    /// again once that changes, and until then is not one more to add.
    unread: FxHashMap<AbsPathBuf, Option<SystemTime>>,
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|it| it.modified()).ok()
}

struct Directory {
    /// The source root its files belong to.
    root: SourceRootId,
    /// When it was last listed, or `None` if it has not been.
    listed: Option<SystemTime>,
}

/// Files that joined or left the source roots.
#[derive(Default)]
pub struct Moved {
    pub added: Vec<(FileId, PathBuf)>,
}

impl Tree {
    pub fn new(db: &RootDatabase, vfs: Vfs, watched: &[(FileId, &Path)]) -> Tree {
        let mut tree =
            Tree { vfs, directories: FxHashMap::default(), unread: FxHashMap::default() };
        tree.watch(db, watched);
        tree
    }

    /// Looks for new files next to `watched` too.
    pub fn watch(&mut self, db: &RootDatabase, watched: &[(FileId, &Path)]) {
        for &(file, path) in watched {
            let root = db.file_source_root(file).source_root_id(db);
            if db.source_root(root).source_root(db).is_library {
                continue;
            }
            let Some(parent) = path.parent() else { continue };
            let listed = std::fs::metadata(parent).and_then(|it| it.modified()).ok();
            self.directories.entry(parent.to_owned()).or_insert(Directory { root, listed });
        }
    }

    /// Has the directories under `base` listed again: they are read from somewhere else now.
    pub fn relist(&mut self, base: &Path) {
        for (path, dir) in &mut self.directories {
            if path.starts_with(base) {
                dir.listed = None;
            }
        }
    }

    pub fn vfs(&mut self) -> &mut Vfs {
        &mut self.vfs
    }

    /// The source files that are new since the last look. Each is in its source root once
    /// this returns, with no text in it yet.
    /// Whether [`Tree::added`] has directories to list again.
    pub fn relisted(&self, db: &RootDatabase) -> bool {
        self.directories.iter().any(|(path, dir)| {
            let now = std::fs::metadata(db.on_disk(path)).and_then(|it| it.modified()).ok();
            now.is_some() && now != dir.listed
        })
    }

    pub fn added(&mut self, db: &mut RootDatabase) -> Moved {
        let mut stale: Vec<PathBuf> = self
            .directories
            .iter()
            .filter(|(path, dir)| {
                let now = std::fs::metadata(db.on_disk(path)).and_then(|it| it.modified()).ok();
                now.is_some() && now != dir.listed
            })
            .map(|(path, _)| path.clone())
            .collect();
        let mut moved = Moved::default();
        let mut joined: FxHashMap<SourceRootId, Vec<(FileId, VfsPath)>> = FxHashMap::default();
        while let Some(path) = stale.pop() {
            let Some(dir) = self.directories.get_mut(&path) else { continue };
            let root = dir.root;
            let on_disk = db.on_disk(&path);
            dir.listed = std::fs::metadata(&on_disk).and_then(|it| it.modified()).ok();
            let Ok(entries) = std::fs::read_dir(&on_disk) else { continue };
            // A package's directory holds much that is not source: only the directories
            // with source right in them are looked into there.
            let package = on_disk.join("Cargo.toml").exists();
            for entry in entries.flatten() {
                let (entry_on_disk, entry_path) = (entry.path(), path.join(entry.file_name()));
                let Ok(kind) = entry.file_type() else { continue };
                let hidden = entry.file_name().to_str().is_none_or(|it| it.starts_with('.'));
                if kind.is_dir() {
                    let source = !hidden
                        && (!package
                            || entry.file_name() != "target" && holds_source(&entry_on_disk));
                    if source && !self.directories.contains_key(&entry_path) {
                        self.directories
                            .insert(entry_path.clone(), Directory { root, listed: None });
                        stale.push(entry_path);
                    }
                    continue;
                }
                if entry_path.extension().is_none_or(|it| it != "rs") {
                    continue;
                }
                if entry_path.to_str().is_none() {
                    continue;
                }
                let vfs_path = VfsPath::from(AbsPathBuf::assert_utf8(entry_path.clone()));
                let known = self.vfs.file_id(&vfs_path).map(|it| it.0);
                let source_root = db.source_root(root).source_root(db);
                if known.is_some_and(|it| source_root.path_for_file(&it).is_some()) {
                    continue;
                }
                let file = match known {
                    Some(file) => file,
                    None => {
                        self.vfs.set_file_contents(vfs_path.clone(), Some(Vec::new()));
                        let Some((file, _)) = self.vfs.file_id(&vfs_path) else { continue };
                        db.set_file_text_with_durability(file, "", Durability::LOW);
                        file
                    }
                };
                joined.entry(root).or_default().push((file, vfs_path));
                moved.added.push((file, entry_path));
            }
        }
        for (root, files) in joined {
            for &(file, _) in &files {
                db.set_file_source_root_with_durability(file, root, Durability::LOW);
            }
            self.rewrite(db, root, files, &FxHashSet::default());
        }
        moved
    }

    /// Puts the files an `include!` or a module declaration named that were not loaded, as
    /// they are not source by their extension or are outside their package (a `#[path]`),
    /// in the source root of the file that names them, with their text.
    /// Whether [`Tree::included`] has files to add: a file that is not on disk, as that of a
    /// module still to be written, is none.
    pub fn includes_more(&self, db: &RootDatabase) -> bool {
        db.included_sources().into_iter().any(|(path, root)| {
            let source_root = db.source_root(root).source_root(db);
            let known = self.vfs.file_id(&VfsPath::from(path.clone()));
            let on_disk = db.on_disk(path.as_ref());
            !known.is_some_and(|(file, _)| source_root.path_for_file(&file).is_some())
                && on_disk.is_file()
                && self.unread.get(&path).is_none_or(|&was| was != modified(&on_disk))
        })
    }

    /// The crates that named a file [`Tree::included`] would load now, by an `include!` or a
    /// module declaration: what was compiled without it is of them, and of what depends on them,
    /// alone.
    pub fn including(&self, db: &RootDatabase) -> Vec<ide_db::base_db::Crate> {
        let loading: FxHashSet<AbsPathBuf> = db
            .included_sources()
            .into_iter()
            .filter(|(path, root)| {
                let source_root = db.source_root(*root).source_root(db);
                let known = self.vfs.file_id(&VfsPath::from(path.clone()));
                !known.is_some_and(|(file, _)| source_root.path_for_file(&file).is_some())
            })
            .map(|(path, _)| path)
            .collect();
        let crates: FxHashSet<_> = db
            .sources_included_by()
            .into_iter()
            .filter(|(_, path)| loading.contains(path))
            .map(|(krate, _)| krate)
            .collect();
        crates.into_iter().collect()
    }

    pub fn included(&mut self, db: &mut RootDatabase) -> Vec<(FileId, PathBuf)> {
        let mut added = Vec::new();
        let mut joined: FxHashMap<SourceRootId, Vec<(FileId, VfsPath)>> = FxHashMap::default();
        for (path, root) in db.included_sources() {
            let vfs_path = VfsPath::from(path.clone());
            let source_root = db.source_root(root).source_root(db);
            if let Some((file, _)) = self.vfs.file_id(&vfs_path)
                && source_root.path_for_file(&file).is_some()
            {
                continue;
            }
            let on_disk = db.on_disk(path.as_ref());
            let Ok(text) = std::fs::read_to_string(&on_disk) else {
                self.unread.insert(path, modified(&on_disk));
                continue;
            };
            self.unread.remove(&path);
            self.vfs.set_file_contents(vfs_path.clone(), Some(text.clone().into_bytes()));
            let Some((file, _)) = self.vfs.file_id(&vfs_path) else { continue };
            let durability =
                if source_root.is_library { Durability::HIGH } else { Durability::LOW };
            db.set_file_text_with_durability(file, &text, durability);
            db.set_file_source_root_with_durability(file, root, durability);
            joined.entry(root).or_default().push((file, vfs_path));
            added.push((file, path.into()));
        }
        for (root, files) in joined {
            self.rewrite(db, root, files, &FxHashSet::default());
        }
        added
    }

    /// Takes files that are gone out of their source roots, so that a path no longer leads
    /// to them.
    pub fn remove(&mut self, db: &mut RootDatabase, files: &[FileId]) {
        let mut left: FxHashMap<SourceRootId, FxHashSet<FileId>> = FxHashMap::default();
        for &file in files {
            let root = db.file_source_root(file).source_root_id(db);
            left.entry(root).or_default().insert(file);
        }
        for (root, gone) in left {
            self.rewrite(db, root, Vec::new(), &gone);
        }
    }

    fn rewrite(
        &self,
        db: &mut RootDatabase,
        root: SourceRootId,
        joined: Vec<(FileId, VfsPath)>,
        gone: &FxHashSet<FileId>,
    ) {
        let was = db.source_root(root).source_root(db);
        let mut files = FileSet::default();
        for file in was.iter().filter(|it| !gone.contains(it)) {
            if let Some(path) = was.path_for_file(&file) {
                files.insert(file, path.clone());
            }
        }
        for (file, path) in joined {
            files.insert(file, path);
        }
        let (source_root, durability) = match was.is_library {
            true => (SourceRoot::new_library(files), Durability::HIGH),
            false => (SourceRoot::new_local(files), Durability::LOW),
        };
        db.set_source_root_with_durability(root, Arc::new(source_root), durability);
    }
}

fn holds_source(directory: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(directory) else { return false };
    // Another package has a source root of its own.
    if directory.join("Cargo.toml").exists() {
        return false;
    }
    entries.flatten().any(|it| it.path().extension().is_some_and(|it| it == "rs"))
}
