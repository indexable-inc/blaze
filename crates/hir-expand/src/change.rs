//! Defines a unit of change that can applied to the database to get the next
//! state. Changes are transactional.
use base_db::{CrateGraphBuilder, CratesIdMap, Db, FileChange, SourceRoot};
use blaze_query::db::Durability;
use span::FileId;

use crate::proc_macro::{ProcMacros, ProcMacrosBuilder};

#[derive(Debug, Default)]
pub struct ChangeWithProcMacros {
    pub source_change: FileChange,
    pub proc_macros: Option<ProcMacrosBuilder>,
}

impl ChangeWithProcMacros {
    /// Makes the change as one write: the maintainers run once, after the crate graph and the
    /// proc macros it carries are both set, and a change setting a crate graph needs no
    /// [`ProcMacros`] input made before it.
    pub fn apply(self, db: &mut Db) -> Option<CratesIdMap> {
        db.batch(|db| {
            if self.source_change.crate_graph.is_some() && ProcMacros::try_get(db).is_none() {
                ProcMacros::init_default(db, Durability::HIGH);
            }
            let crates_id_map = self.source_change.apply(db);
            if let Some(proc_macros) = self.proc_macros {
                proc_macros.build_in(
                    db,
                    crates_id_map
                        .as_ref()
                        .expect("cannot set proc macros without setting the crate graph too"),
                );
            }
            crates_id_map
        })
    }

    pub fn change_file(&mut self, file_id: FileId, new_text: Option<String>) {
        self.source_change.change_file(file_id, new_text)
    }

    pub fn set_crate_graph(&mut self, graph: CrateGraphBuilder) {
        self.source_change.set_crate_graph(graph);
    }

    pub fn set_proc_macros(&mut self, proc_macros: ProcMacrosBuilder) {
        self.proc_macros = Some(proc_macros);
    }

    pub fn set_roots(&mut self, roots: Vec<SourceRoot>) {
        self.source_change.set_roots(roots)
    }
}
