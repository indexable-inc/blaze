//! The source files of the crates documented, each shown as a page, and the lines an item is on.

use std::sync::Arc;

use hir::{HirFileId, InFile};
use base_db::Db;
use blaze_hash::HashMap;
use syntax::{TextRange, TextSize, view as ast};

use crate::{
    model::{SourceFile, Span},
    places::Places,
};

pub(crate) struct Files {
    /// For each file: its crate's index, its index there and where its lines start.
    by_file: HashMap<base_db::FileId, (usize, usize, Arc<[u32]>)>,
    /// Each crate's files, in the order of their indices.
    pub(crate) per_crate: Vec<Vec<SourceFile>>,
    /// The absolute path of every file, for warnings.
    paths: HashMap<base_db::FileId, String>,
}

impl Files {
    pub(crate) fn new(db: &Db, places: &Places) -> Files {
        let mut by_file = HashMap::default();
        let mut per_crate = vec![];
        let mut paths = HashMap::default();
        for (krate, documented) in places.crates.iter().enumerate() {
            let root_file = documented.krate.root_file(db);
            let base = path_of(db, root_file)
                .and_then(|it| it.rsplit_once('/').map(|(dir, _)| dir.to_owned()))
                .unwrap_or_default();
            let mut files = vec![];
            let mut ids: Vec<base_db::FileId> = documented
                .krate
                .modules(db)
                .into_iter()
                .filter_map(|module| match module.definition_source_file_id(db) {
                    HirFileId::FileId(file) => Some(file.file_id(db)),
                    HirFileId::MacroFile(_) => None,
                })
                .collect();
            ids.dedup();
            for file in ids {
                if by_file.contains_key(&file) {
                    continue;
                }
                let Some(path) = path_of(db, file) else { continue };
                let text: Arc<str> = Arc::from(&**db.file_text(file).text(db));
                let starts: Arc<[u32]> = std::iter::once(0)
                    .chain(text.match_indices('\n').map(|(at, _)| at as u32 + 1))
                    .collect();
                by_file.insert(file, (krate, files.len(), starts));
                paths.insert(file, path.clone());
                files.push(SourceFile { path: relative(&base, &path), text: text.to_string() });
            }
            per_crate.push(files);
        }
        Files { by_file, per_crate, paths }
    }

    /// The lines of a node, in the file it is written in (a macro's output maps to the call).
    pub(crate) fn span(&self, db: &Db, node: InFile<ast::Node<'_>>) -> Option<Span> {
        let range = node.map(|it| it.text_range()).original_node_file_range_rooted(db);
        let file = range.file_id.file_id(db);
        let (krate, index, starts) = self.by_file.get(&file)?;
        let start = line_of(starts, range.range.start());
        let end = line_of(starts, range.range.end().max(range.range.start()));
        Some(Span { krate: *krate, file: *index, lines: (start, end) })
    }

    /// `path:line:column` of a range, for a warning; the path is as the file system has it.
    pub(crate) fn location(&self, db: &Db, at: InFile<TextRange>) -> Option<String> {
        let range = at.original_node_file_range_rooted(db);
        let file = range.file_id.file_id(db);
        let path = self.paths.get(&file).cloned().or_else(|| path_of(db, file))?;
        let text = db.file_text(file).text(db).clone();
        let offset = usize::from(range.range.start()).min(text.len());
        let before = &text[..offset];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |it| it.chars().count()) + 1;
        Some(format!("{path}:{line}:{column}"))
    }
}

fn line_of(starts: &[u32], offset: TextSize) -> u32 {
    let offset = u32::from(offset);
    match starts.binary_search(&offset) {
        Ok(at) => at as u32 + 1,
        Err(at) => at as u32,
    }
}

pub(crate) fn path_of(db: &Db, file: base_db::FileId) -> Option<String> {
    let root = db.file_source_root(file).source_root_id(db);
    let root = db.source_root(root).source_root(db);
    root.path_for_file(&file).map(|it| it.to_string())
}

/// `path` from directory `base`, with `up/` for each step out, as rustdoc names source pages.
fn relative(base: &str, path: &str) -> String {
    let base: Vec<&str> = base.split('/').filter(|it| !it.is_empty()).collect();
    let parts: Vec<&str> = path.split('/').filter(|it| !it.is_empty()).collect();
    let common = base.iter().zip(&parts).take_while(|(a, b)| a == b).count();
    let mut out: Vec<&str> = vec!["up"; base.len() - common];
    out.extend(&parts[common..]);
    out.join("/")
}

#[cfg(test)]
mod tests {
    use super::relative;

    #[test]
    fn relative_paths_step_up_with_up() {
        assert_eq!(relative("/w/src", "/w/src/lib.rs"), "lib.rs");
        assert_eq!(relative("/w/src", "/w/src/a/b.rs"), "a/b.rs");
        assert_eq!(relative("/w/src", "/w/shared/x.rs"), "up/shared/x.rs");
    }
}
