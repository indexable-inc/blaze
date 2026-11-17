//! Database used for testing `hir`: the build's [`Db`], made by [`Db::for_tests`].

use base_db::{Db, all_crates, relevant_crates};

use blaze_hash::HashMap;
use blaze_memo::testing::Ran;
use hir_def::{ModuleId, nameres::crate_def_map};
use hir_expand::EditionedFileId;
use span::FileId;
use std::alloc::Global;
use syntax::TextRange;
use test_utils::extract_annotations;

pub(crate) type TestDB = Db;

/// Sets `file`'s text in a new snapshot: one edit and its commit. A file's id and a crate's key
/// are the same across it; every other id (an item's, a [`base_db::Crate`], an interned
/// `EditionedFileId`) is of the snapshot it was taken in, taken again after.
pub(crate) fn write_file(db: &mut Db, file: FileId, text: &str) {
    let mut edit = db.edit();
    edit.set_file_text(file, text);
    _ = edit.commit();
}

/// Has every snapshot of `fx` from its next on expand builtin derives, as a build has them: an
/// input of the session arena, made each time it is, set in one commit
/// ([`hir_expand::fixture::Fixture::edit`], which takes the fixture's file ids again). Take the
/// fixture's ids after it.
pub(crate) fn expand_builtin_derives(fx: &mut hir_expand::fixture::Fixture) {
    _ = fx.edit(|edit| {
        edit.set_given(
            None,
            hir_def::nameres::builtin_derives_fact(true),
            Box::new_in(
                |_: &blaze_memo::Builder, inputs: &blaze_memo::Inputs<'_>| {
                    hir_def::nameres::set_expand_builtin_derives(inputs, true);
                },
                Global,
            ),
        );
    });
}

/// Names every file of `fx` `name` from its next snapshot on, as a build names its files (for
/// `Location::caller` and `file!()`): an input of the session arena under the rule `key`, made
/// each time that arena is, set in one commit ([`hir_expand::fixture::Fixture::edit`], which
/// takes the fixture's file ids again). Take the fixture's ids after it.
pub(crate) fn name_sources(fx: &mut hir_expand::fixture::Fixture, key: &'static str, name: &'static str) {
    _ = fx.edit(|edit| {
        edit.set_given(
            None,
            crate::mir::location::source_names_fact(key),
            Box::new_in(
                move |_: &blaze_memo::Builder, inputs: &blaze_memo::Inputs<'_>| {
                    crate::mir::location::name_sources(inputs, key, move |_, _, _| Some(name.to_owned()));
                },
                Global,
            ),
        );
    });
}

/// What `work` ran, from the database's run log (a fixture's database logs its runs): the runs
/// before it are taken off first, so none of them is counted as its.
pub(crate) fn runs_of(db: &Db, work: impl FnOnce()) -> Vec<Ran, Global> {
    let log = db.runs().expect("a fixture's database logs its runs");
    _ = log.take();
    work();
    log.take()
}

pub(crate) trait TestDBExt {
    fn module_for_file_opt(&self, file_id: impl Into<FileId>) -> Option<ModuleId>;

    fn module_for_file(&self, file_id: impl Into<FileId>) -> ModuleId {
        self.module_for_file_opt(file_id.into()).unwrap()
    }

    fn extract_annotations(&self) -> HashMap<EditionedFileId, Vec<(TextRange, String)>, Global>;
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

    fn extract_annotations(&self) -> HashMap<EditionedFileId, Vec<(TextRange, String)>, Global> {
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
