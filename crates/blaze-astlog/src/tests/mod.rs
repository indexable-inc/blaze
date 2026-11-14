//! Rules against a real front end: fixtures lowered by `hir_expand::fixture`, the view's files
//! given as blaze-intent's [`Files`].

mod query;
mod rewrite;

use base_db::{Db, FileId};
use blaze_intent::Files;
use hir_expand::fixture;

use crate::{Cell, Outcome};

/// The view: its files by path, with their text.
pub(crate) struct View {
    pub files: Vec<(FileId, String, String)>,
}

impl Files for View {
    fn file(&self, path: &str) -> Option<(FileId, &[u8])> {
        self.files.iter().find(|it| it.1 == path).map(|it| (it.0, it.2.as_bytes()))
    }

    fn path(&self, file: FileId) -> Option<&str> {
        self.files.iter().find(|it| it.0 == file).map(|it| &*it.1)
    }
}

/// A front end of `fixture`, whose files are, in order, `outside` files not in the view and
/// then the view's `paths`.
pub(crate) fn view(fixture: &str, outside: usize, paths: &[&str]) -> (Db, View) {
    let fx = fixture::db(fixture);
    let ids = fx.files().to_vec();
    let db = fx.into_db();
    assert_eq!(ids.len(), outside + paths.len(), "the fixture's files");
    let files = ids[outside..]
        .iter()
        .zip(paths)
        .map(|(file, path)| {
            let id = file.file_id(&db);
            (id, (*path).to_owned(), db.file_text(id).text(&db).to_string())
        })
        .collect();
    (db, View { files })
}

/// The rows of `relation`, each as its cells' code text (or def path, type, text, number).
#[track_caller]
pub(crate) fn texts(outcome: &Outcome, relation: &str) -> Vec<Vec<String>> {
    let Some((_, _, rows)) = outcome.relations.iter().find(|it| it.0 == relation) else {
        panic!("no relation `{relation}` in {:?}", outcome.relations.iter().map(|it| &it.0).collect::<Vec<_>>());
    };
    rows.iter()
        .map(|row| {
            row.cells
                .iter()
                .map(|cell| match cell {
                    Cell::Code { text, .. } | Cell::Expanded { text, .. } | Cell::Type { text } => {
                        text.clone()
                    }
                    Cell::Def { path } => path.clone(),
                    Cell::Text(text) => text.clone(),
                    Cell::Int(int) => int.to_string(),
                })
                .collect()
        })
        .collect()
}

/// The lines of the code cells of `relation`'s rows' first column.
#[track_caller]
pub(crate) fn lines(outcome: &Outcome, relation: &str) -> Vec<usize> {
    let (_, _, rows) = outcome.relations.iter().find(|it| it.0 == relation).expect("the relation");
    rows.iter()
        .map(|row| match &row.cells[0] {
            Cell::Code { line: Some(line), .. } => *line,
            other => panic!("not code with a line: {other:?}"),
        })
        .collect()
}
