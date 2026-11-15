//! Defines a unit of change that can applied to the database to get the next
//! state. Changes are transactional.

use std::fmt;

use crate::FileId;
use blaze_hash::HashSet;
use std::alloc::Global;
use triomphe::Arc;

use crate::{CrateGraphBuilder, CratesIdMap, Db, SourceRoot, SourceRootId};

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
    /// Writes the change in one edit of `db` and commits it: the next snapshot has it. The
    /// keys of the crates of its graph, when it sets one.
    pub fn apply(self, db: &mut Db) -> Option<CratesIdMap> {
        let _p = tracing::info_span!(target: "blaze", "FileChange::apply").entered();
        let has_graph = self.crate_graph.is_some();
        let mut edit = db.edit();
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
                for file_id in root.iter() {
                    edit.set_file_source_root(file_id, root_id);
                }
                // One per source root a change sets (setup).
                edit.set_source_root(root_id, Arc::new(root));
            }
            edit.set_local_roots(local_roots);
            edit.set_library_roots(library_roots);
        }

        for (file_id, text) in self.files_changed {
            // XXX: can't actually remove the file, just reset the text
            let text = text.unwrap_or_default();
            edit.set_file_text(file_id, &text);
        }

        if let Some(crate_graph) = self.crate_graph {
            edit.set_crate_graph(crate_graph);
        }
        let committed = edit.commit();
        has_graph.then_some(committed.crates)
    }
}
