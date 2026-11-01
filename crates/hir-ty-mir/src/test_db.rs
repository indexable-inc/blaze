//! Database used for testing `hir`: the build's [`Db`], made by [`Db::for_tests`].

use base_db::{Db, all_crates, relevant_crates};

use blaze_hash::HashMap;
use hir_def::{ModuleId, nameres::crate_def_map};
use hir_expand::EditionedFileId;
use span::FileId;
use syntax::TextRange;
use test_utils::extract_annotations;

pub(crate) type TestDB = Db;

pub(crate) trait TestDBExt {
    fn module_for_file_opt(&self, file_id: impl Into<FileId>) -> Option<ModuleId>;

    fn module_for_file(&self, file_id: impl Into<FileId>) -> ModuleId {
        self.module_for_file_opt(file_id.into()).unwrap()
    }

    fn extract_annotations(&self) -> HashMap<EditionedFileId, Vec<(TextRange, String)>>;
}

impl TestDBExt for Db {
    fn module_for_file_opt(&self, file_id: impl Into<FileId>) -> Option<ModuleId> {
        let file_id = file_id.into();
        for &krate in relevant_crates(self, file_id).iter() {
            let crate_def_map = crate_def_map(self, krate);
            for (module_id, data) in crate_def_map.modules(self) {
                if data.origin.file_id().map(|file_id| file_id.file_id(self)) == Some(file_id) {
                    return Some(module_id);
                }
            }
        }
        None
    }

    fn extract_annotations(&self) -> HashMap<EditionedFileId, Vec<(TextRange, String)>> {
        let mut files = Vec::new();
        for &krate in all_crates(self).iter() {
            let crate_def_map = crate_def_map(self, krate);
            for (module_id, _) in crate_def_map.modules(self) {
                let file_id = crate_def_map.module(self, module_id).origin.file_id();
                files.extend(file_id)
            }
        }
        files
            .into_iter()
            .filter_map(|file_id| {
                let text = self.file_text(file_id.file_id(self));
                let annotations = extract_annotations(text.text(self));
                if annotations.is_empty() {
                    return None;
                }
                Some((file_id, annotations))
            })
            .collect()
    }
}
