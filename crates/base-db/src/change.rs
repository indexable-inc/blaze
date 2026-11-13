//! Defines a unit of change that can applied to the database to get the next
//! state. Changes are transactional.

use std::fmt;

use crate::FileId;
use blaze_hash::HashSet;
use blaze_query::db::{Durability, Setter as _};
use std::alloc::Global;
use triomphe::Arc;

use crate::{
    CrateGraphBuilder, CratesIdMap, Db, LibraryRoots, LocalRoots, SourceRoot, SourceRootId,
};

/// Encapsulate a bunch of raw `.set` calls on the database.
#[derive(Default)]
pub struct FileChange {
    pub roots: Option<Vec<SourceRoot>>,
    pub files_changed: Vec<(FileId, Option<String>)>,
    pub crate_graph: Option<CrateGraphBuilder>,
}

impl fmt::Debug for FileChange {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = fmt.debug_struct("Change");
        if let Some(roots) = &self.roots {
            d.field("roots", roots);
        }
        if !self.files_changed.is_empty() {
            d.field("files_changed", &self.files_changed.len());
        }
        if self.crate_graph.is_some() {
            d.field("crate_graph", &self.crate_graph);
        }
        d.finish()
    }
}

impl FileChange {
    pub fn set_roots(&mut self, roots: Vec<SourceRoot>) {
        self.roots = Some(roots);
    }

    pub fn change_file(&mut self, file_id: FileId, new_text: Option<String>) {
        self.files_changed.push((file_id, new_text))
    }

    pub fn set_crate_graph(&mut self, graph: CrateGraphBuilder) {
        self.crate_graph = Some(graph);
    }
    pub fn apply(self, db: &mut Db) -> Option<CratesIdMap> {
        let _p = tracing::info_span!(target: "blaze", "FileChange::apply").entered();
        if let Some(roots) = self.roots {
            let mut local_roots = HashSet::new_in(Global);
            let mut library_roots = HashSet::new_in(Global);
            for (idx, root) in roots.into_iter().enumerate() {
                let root_id = SourceRootId(idx as u32);
                if root.is_library {
                    library_roots.insert(root_id);
                } else {
                    local_roots.insert(root_id);
                }
                let durability = source_root_durability(&root);
                for file_id in root.iter() {
                    db.set_file_source_root_with_durability(file_id, root_id, durability);
                }

                // alloc: one per source root a change sets (setup).
                db.set_source_root_with_durability(root_id, Arc::new(root), durability);
            }
            LocalRoots::get(db).set_roots(db).to(local_roots);
            LibraryRoots::get(db).set_roots(db).to(library_roots);
        }

        for (file_id, text) in self.files_changed {
            let source_root_id = db.file_source_root(file_id);
            let source_root = db.source_root(source_root_id.source_root_id(db));

            let durability = file_text_durability(source_root.source_root_ref(db));
            // XXX: can't actually remove the file, just reset the text
            let text = text.unwrap_or_default();
            db.set_file_text_with_durability(file_id, &text, durability)
        }

        if let Some(crate_graph) = self.crate_graph {
            return Some(crate_graph.set_in_db(db));
        }
        None
    }
}

/// The kind of a root's file set and of its files' root map, by the root's kind
/// ([`crate::files::root_set_durability`]): `HIGH` for a library's, whose set moves only with
/// a new toolchain or a fetched package, so a read of it under a covered frame is allowed;
/// `MEDIUM` for a local root. Until 2026-11-12 every root's was `MEDIUM` here.
fn source_root_durability(root: &SourceRoot) -> Durability {
    crate::files::root_set_durability(root.is_library)
}

/// A library file's text is `HIGH`, a local one's `LOW`
/// ([`crate::files::root_content_durability`], the one rule).
fn file_text_durability(source_root: &SourceRoot) -> Durability {
    crate::files::root_content_durability(source_root.is_library)
}
